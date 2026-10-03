//! Perform the GitHub mutations used by review actions.

use error_stack::{Report, ResultExt as _};
use octocrab::{
    models::pulls::ReviewAction,
    params::pulls::{MergeMethod, State as PullRequestState},
};
use serde::{Deserialize, Serialize};

use super::{approval_view::update_merge_status, status_rows::PrStatusRows};
use crate::{
    app::{
        approval_workflow::{ApprovalWorkflow, MergeQueueStatus},
        async_merge::{AsyncMerge, AsyncMergeError, MergeOperation, MergeOutcome},
        merge_results::wait_for_async_merge,
        App,
    },
    error::AppError,
};

#[derive(Debug)]
pub(super) enum EnqueuePullRequestOutcome {
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

    pub(super) async fn approve_pull_request(
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

    pub(super) async fn close_pull_request(
        &self,
        owner: &str,
        repo_name: &str,
        pr_number: u64,
    ) -> Result<(), Report<AppError>> {
        self.debug(&format!("Closing PR #{}", pr_number));

        self.octocrab
            .pulls(owner, repo_name)
            .update(pr_number)
            .state(PullRequestState::Closed)
            .send()
            .await
            .change_context(AppError::Close)
            .attach(format!("Failed to close PR #{}", pr_number))?;

        Ok(())
    }

    pub(super) async fn direct_merge_pull_request(
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

    pub(super) async fn enqueue_pull_request(
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

    pub(super) async fn enable_auto_merge_for_pull_request(
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
