use console::style;
use error_stack::{Report, ResultExt as _};
use octocrab::{models::pulls::ReviewAction, params::pulls::MergeMethod};
use serde::{Deserialize, Serialize};

use super::{
    status_rows::{update_merge_status, PrStatusRows},
    App, MergeInfo,
};
use crate::{
    app::{
        approval_workflow::{ApprovalMode, ApprovalWorkflow, MergeQueueStatus},
        async_merge::{AsyncMerge, AsyncMergeError, MergeOperation, MergeOutcome},
        merge_results::wait_for_async_merge,
    },
    error::AppError,
};

#[derive(Debug)]
pub(super) enum ApprovalOutcome {
    Completed,
    WatchForMerge,
    Skipped,
}

#[derive(Debug)]
enum EnqueuePullRequestOutcome {
    Queued,
    Merged,
    AwaitingRequiredChecks,
}

#[derive(Serialize)]
struct GraphqlRequest<'a, T> {
    query: &'a str,
    variables: T,
}

#[derive(Deserialize)]
struct GraphqlNode {
    id: String,
}

#[derive(Serialize)]
struct EnableAutoMergeVariables<'a> {
    #[serde(rename = "pullRequestId")]
    pull_request_id: &'a str,
    #[serde(rename = "expectedHeadOid")]
    expected_head_oid: &'a str,
    #[serde(rename = "mergeMethod")]
    merge_method: &'a str,
}

#[derive(Deserialize)]
struct MutationOnlyResponse {
    #[serde(rename = "enablePullRequestAutoMerge")]
    enable_pull_request_auto_merge: Option<EnablePullRequestAutoMergePayload>,
}

#[derive(Deserialize)]
struct EnablePullRequestAutoMergePayload {
    #[serde(rename = "pullRequest")]
    pull_request: Option<GraphqlNode>,
}

impl App {
    pub(super) async fn approve_and_merge(
        &self,
        info: &MergeInfo,
        context: (MergeMethod, bool),
        allow_non_passing_ci: bool,
        statuses: Option<&PrStatusRows>,
    ) -> Result<ApprovalOutcome, Report<AppError>> {
        let mut watch_for_merge = false;
        if let Some(statuses) = statuses {
            statuses.update(&info.repo, info.pr_number, "Inspecting merge strategy");
        }
        let (merge_method, allow_auto_merge) = context;
        let queue_status = self
            .fetch_merge_queue_status(
                &info.owner,
                &info.repo_name,
                info.pr_number,
                &info.base_ref_name,
            )
            .await
            .change_context(AppError::ApproveMerge)
            .attach(format!(
                "Failed to inspect merge strategy for PR #{}",
                info.pr_number
            ))?;

        let merge_mode = ApprovalWorkflow::plan(
            info.ci_status,
            &queue_status,
            allow_auto_merge,
            allow_non_passing_ci,
        );

        if !(matches!(merge_mode, ApprovalMode::AlreadyQueued)
            || matches!(merge_mode, ApprovalMode::AlreadyAutoMergeEnabled)
                && queue_status.uses_merge_queue)
        {
            if let Some(statuses) = statuses {
                statuses.update(&info.repo, info.pr_number, "Approving pull request");
            }
            self.approve_pull_request(&info.owner, &info.repo_name, info.pr_number)
                .await?;
        }

        match merge_mode {
            ApprovalMode::Direct => {
                self.debug(&format!("PR #{} merge queue: not used", info.pr_number));
                if let Some(statuses) = statuses {
                    statuses.update(&info.repo, info.pr_number, "Merging");
                }
                self.direct_merge_pull_request(
                    &info.owner,
                    &info.repo_name,
                    info.pr_number,
                    merge_method,
                    statuses,
                )
                .await?;
                if let Some(statuses) = statuses {
                    statuses.finish_success(&info.repo, info.pr_number, "Approved and merged");
                } else {
                    println!(
                        "  {} Approved and merged PR #{}{}",
                        style("✓").green(),
                        info.pr_number,
                        style(format!(" ({})", info.repo)).dim()
                    );
                }
            }
            ApprovalMode::AutoMerge => {
                watch_for_merge = true;
                self.debug(&format!(
                    "PR #{} merge queue: not used (enable regular auto-merge)",
                    info.pr_number
                ));
                self.enable_auto_merge_for_pull_request(
                    &queue_status.pull_request_id,
                    &queue_status.head_oid,
                    merge_method,
                )
                .await?;
                if let Some(statuses) = statuses {
                    statuses.update(
                        &info.repo,
                        info.pr_number,
                        "Waiting for CI to pass (auto-merge enabled)",
                    );
                } else {
                    println!(
                        "  {} Approved PR #{} and enabled auto-merge{}",
                        style("✓").green(),
                        info.pr_number,
                        style(format!(" ({})", info.repo)).dim()
                    );
                }
            }
            ApprovalMode::MergeQueueEnqueue => {
                self.debug(&format!(
                    "PR #{} merge queue: used (enqueue)",
                    info.pr_number
                ));
                match self
                    .enqueue_pull_request(
                        &info.owner,
                        &info.repo_name,
                        info.pr_number,
                        &queue_status.head_oid,
                        statuses,
                    )
                    .await?
                {
                    EnqueuePullRequestOutcome::Queued => {
                        watch_for_merge = true;
                        if let Some(statuses) = statuses {
                            statuses.update(&info.repo, info.pr_number, "In merge queue");
                        } else {
                            println!(
                                "  {} Approved PR #{} and added it to the merge queue{}",
                                style("✓").green(),
                                info.pr_number,
                                style(format!(" ({})", info.repo)).dim()
                            );
                        }
                    }
                    EnqueuePullRequestOutcome::Merged => {
                        if let Some(statuses) = statuses {
                            statuses.finish_success(&info.repo, info.pr_number, "Already merged");
                        } else {
                            println!(
                                "  {} PR #{} is already merged{}",
                                style("✓").green(),
                                info.pr_number,
                                style(format!(" ({})", info.repo)).dim()
                            );
                        }
                    }
                    EnqueuePullRequestOutcome::AwaitingRequiredChecks => {
                        watch_for_merge = true;
                        self.debug(&format!(
                            "PR #{} cannot enter the merge queue yet; enabling auto-merge",
                            info.pr_number
                        ));
                        self.enable_auto_merge_for_pull_request(
                            &queue_status.pull_request_id,
                            &queue_status.head_oid,
                            merge_method,
                        )
                        .await?;
                        if let Some(statuses) = statuses {
                            statuses.update(
                                &info.repo,
                                info.pr_number,
                                "Waiting for CI to pass before joining merge queue",
                            );
                        } else {
                            println!(
                                "  {} Approved PR #{} and enabled auto-merge while required checks complete{}",
                                style("✓").green(),
                                info.pr_number,
                                style(format!(" ({})", info.repo)).dim()
                            );
                        }
                    }
                }
            }
            ApprovalMode::MergeQueueAutoMerge => {
                watch_for_merge = true;
                self.debug(&format!(
                    "PR #{} merge queue: used (auto-merge until queueable)",
                    info.pr_number
                ));
                self.enable_auto_merge_for_pull_request(
                    &queue_status.pull_request_id,
                    &queue_status.head_oid,
                    merge_method,
                )
                .await?;
                if let Some(statuses) = statuses {
                    statuses.update(
                        &info.repo,
                        info.pr_number,
                        "Waiting for CI to pass before joining merge queue",
                    );
                } else {
                    println!(
                        "  {} Approved PR #{} and enabled auto-merge for the merge queue{}",
                        style("✓").green(),
                        info.pr_number,
                        style(format!(" ({})", info.repo)).dim()
                    );
                }
            }
            ApprovalMode::AlreadyQueued => {
                watch_for_merge = true;
                self.debug(&format!(
                    "PR #{} merge queue: already queued",
                    info.pr_number
                ));
                if let Some(statuses) = statuses {
                    statuses.update(&info.repo, info.pr_number, "In merge queue");
                } else {
                    println!(
                        "  {} Approved PR #{} (already in merge queue){}",
                        style("✓").green(),
                        info.pr_number,
                        style(format!(" ({})", info.repo)).dim()
                    );
                }
            }
            ApprovalMode::AlreadyAutoMergeEnabled => {
                watch_for_merge = true;
                if queue_status.uses_merge_queue {
                    self.debug(&format!(
                        "PR #{} auto-merge: already enabled for merge queue",
                        info.pr_number
                    ));
                    if let Some(statuses) = statuses {
                        statuses.update(
                            &info.repo,
                            info.pr_number,
                            "Waiting for merge (auto-merge enabled)",
                        );
                    } else {
                        println!(
                            "  {} Approved PR #{} (auto-merge already enabled){}",
                            style("✓").green(),
                            info.pr_number,
                            style(format!(" ({})", info.repo)).dim()
                        );
                    }
                } else {
                    self.debug(&format!(
                        "PR #{} auto-merge: already enabled (approval refreshed)",
                        info.pr_number
                    ));
                    if let Some(statuses) = statuses {
                        statuses.update(
                            &info.repo,
                            info.pr_number,
                            "Waiting for merge (auto-merge enabled)",
                        );
                    } else {
                        println!(
                            "  {} Approved PR #{} (auto-merge already enabled; approval refreshed){}",
                            style("✓").green(),
                            info.pr_number,
                            style(format!(" ({})", info.repo)).dim()
                        );
                    }
                }
            }
            ApprovalMode::SkipPendingWithoutQueue => {
                self.debug(&format!("PR #{} merge queue: not used", info.pr_number));
                if let Some(statuses) = statuses {
                    statuses.finish_skipped(
                        &info.repo,
                        info.pr_number,
                        &format!("Skipped: CI {}, no merge queue", info.ci_status),
                    );
                } else {
                    println!(
                        "  {} Skipping PR #{} (CI {}, no merge queue){}",
                        style("⊘").yellow(),
                        info.pr_number,
                        info.ci_status,
                        style(format!(" ({})", info.repo)).dim()
                    );
                }
                return Ok(ApprovalOutcome::Skipped);
            }
        }

        Ok(if watch_for_merge {
            ApprovalOutcome::WatchForMerge
        } else {
            ApprovalOutcome::Completed
        })
    }

    pub(super) async fn fetch_merge_queue_status(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        base_ref_name: &str,
    ) -> Result<MergeQueueStatus, Report<AppError>> {
        ApprovalWorkflow::new(&self.octocrab)
            .inspect(owner, repo, pr_number, base_ref_name)
            .await
    }

    async fn approve_pull_request(
        &self,
        owner: &str,
        repo_name: &str,
        pr_number: u64,
    ) -> Result<(), Report<AppError>> {
        let pulls = self.octocrab.pulls(owner, repo_name);
        let pr_data = pulls
            .get(pr_number)
            .await
            .change_context(AppError::ApproveMerge)
            .attach(format!("Failed to get PR #{}", pr_number))?;
        let head_sha = pr_data.head.sha;

        self.debug(&format!("Approving PR #{}", pr_number));

        #[expect(
            deprecated,
            reason = "octocrab has no supported alternative for creating an approval review"
        )]
        let pr_handle = pulls.pull_number(pr_number);

        pr_handle
            .reviews()
            .create_review(head_sha, "", ReviewAction::Approve, Vec::new())
            .await
            .change_context(AppError::ApproveMerge)
            .attach(format!("Failed to approve PR #{}", pr_number))?;

        Ok(())
    }

    async fn direct_merge_pull_request(
        &self,
        owner: &str,
        repo_name: &str,
        pr_number: u64,
        merge_method: MergeMethod,
        statuses: Option<&PrStatusRows>,
    ) -> Result<(), Report<AppError>> {
        let pr_data = self
            .octocrab
            .pulls(owner, repo_name)
            .get(pr_number)
            .await
            .change_context(AppError::ApproveMerge)
            .attach(format!("Failed to get PR #{pr_number}"))?;
        let status = AsyncMerge::new(&self.octocrab)
            .start(
                owner,
                repo_name,
                pr_number,
                &pr_data.head.sha,
                MergeOperation::Direct(merge_method),
            )
            .await?;
        let outcome = wait_for_async_merge(
            &self.octocrab,
            status,
            |message| update_merge_status(statuses, owner, repo_name, pr_number, message),
            tokio::signal::ctrl_c(),
        )
        .await?;

        match outcome {
            MergeOutcome::Merged => Ok(()),
            MergeOutcome::Enqueued => Err(Report::new(AppError::ApproveMerge)
                .attach("Pull request is in the merge queue; direct merge is not complete")),
        }
    }

    async fn enqueue_pull_request(
        &self,
        owner: &str,
        repo_name: &str,
        pr_number: u64,
        expected_head_oid: &str,
        statuses: Option<&PrStatusRows>,
    ) -> Result<EnqueuePullRequestOutcome, Report<AppError>> {
        let result = async {
            let status = AsyncMerge::new(&self.octocrab)
                .start(
                    owner,
                    repo_name,
                    pr_number,
                    expected_head_oid,
                    MergeOperation::Queue,
                )
                .await?;

            wait_for_async_merge(
                &self.octocrab,
                status,
                |message| update_merge_status(statuses, owner, repo_name, pr_number, message),
                tokio::signal::ctrl_c(),
            )
            .await
        }
        .await;

        match result {
            Ok(MergeOutcome::Enqueued) => Ok(EnqueuePullRequestOutcome::Queued),
            Ok(MergeOutcome::Merged) => Ok(EnqueuePullRequestOutcome::Merged),
            Err(error)
                if error.downcast_ref::<AsyncMergeError>().is_some_and(|error| {
                    matches!(error, AsyncMergeError::Failed { message } if messages_are_awaiting_required_checks([message.as_str()]))
                }) =>
            {
                Ok(EnqueuePullRequestOutcome::AwaitingRequiredChecks)
            }
            Err(error) => Err(error.attach("Failed to enqueue pull request")),
        }
    }

    async fn enable_auto_merge_for_pull_request(
        &self,
        pull_request_id: &str,
        expected_head_oid: &str,
        merge_method: MergeMethod,
    ) -> Result<(), Report<AppError>> {
        const MUTATION: &str = r#"
            mutation EnablePullRequestAutoMerge(
              $pullRequestId: ID!
              $expectedHeadOid: GitObjectID!
              $mergeMethod: PullRequestMergeMethod!
            ) {
              enablePullRequestAutoMerge(
                input: {
                  pullRequestId: $pullRequestId
                  expectedHeadOid: $expectedHeadOid
                  mergeMethod: $mergeMethod
                }
              ) {
                pullRequest { id }
              }
            }
        "#;

        let payload = GraphqlRequest {
            query: MUTATION,
            variables: EnableAutoMergeVariables {
                pull_request_id,
                expected_head_oid,
                merge_method: graphql_merge_method(merge_method),
            },
        };
        let data: MutationOnlyResponse = self
            .octocrab
            .graphql(&payload)
            .await
            .change_context(AppError::ApproveMerge)
            .attach("Failed to enable pull request auto-merge")?;
        let _pull_request_id = data
            .enable_pull_request_auto_merge
            .and_then(|payload| payload.pull_request)
            .map(|pull_request| pull_request.id)
            .ok_or_else(|| Report::new(AppError::ApproveMerge))
            .attach("enablePullRequestAutoMerge did not return a pull request")?;
        Ok(())
    }
}

fn messages_are_awaiting_required_checks<'a>(messages: impl IntoIterator<Item = &'a str>) -> bool {
    let mut messages = messages.into_iter();
    let Some(first) = messages.next() else {
        return false;
    };

    let is_expected_checks_error =
        |message: &str| message.contains("required status check") && message.contains(" expected");

    is_expected_checks_error(first) && messages.all(is_expected_checks_error)
}

pub(super) fn preferred_merge_method(
    repo_info: &octocrab::models::Repository,
) -> Result<MergeMethod, Report<AppError>> {
    match (
        repo_info.allow_merge_commit,
        repo_info.allow_squash_merge,
        repo_info.allow_rebase_merge,
    ) {
        (Some(true), _, _) => Ok(MergeMethod::Merge),
        (Some(false), Some(true), _) => Ok(MergeMethod::Squash),
        (Some(false), Some(false), Some(true)) => Ok(MergeMethod::Rebase),
        _ => Err(Report::new(AppError::ApproveMerge)).attach("No merge method available"),
    }
}

fn graphql_merge_method(merge_method: MergeMethod) -> &'static str {
    match merge_method {
        MergeMethod::Merge => "MERGE",
        MergeMethod::Squash => "SQUASH",
        MergeMethod::Rebase => "REBASE",
        _ => "MERGE",
    }
}

#[cfg(test)]
mod tests;
