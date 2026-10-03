//! Execute the approval plan for one pull request and return its outcome.

use error_stack::{Report, ResultExt as _};

use super::{
    approval::{MergeInfo, RepositoryMergeSettings},
    pull_requests::EnqueuePullRequestOutcome,
    status_rows::PrStatusRows,
};
use crate::{
    app::{
        approval_workflow::{ApprovalMode, ApprovalWorkflow},
        App,
    },
    error::AppError,
};

#[derive(Clone, Copy, Debug)]
pub(super) enum ApprovalOutcome {
    Merged,
    AutoMerge,
    Queued,
    AlreadyMerged,
    AwaitingQueueChecks,
    QueueAutoMerge,
    AlreadyQueued,
    AlreadyAutoMergeEnabled { uses_merge_queue: bool },
    Skipped,
}

impl ApprovalOutcome {
    pub(super) fn needs_monitoring(self) -> bool {
        !matches!(self, Self::Merged | Self::AlreadyMerged | Self::Skipped)
    }
}

impl App {
    pub(super) async fn submit_approval(
        &self,
        info: &MergeInfo,
        settings: RepositoryMergeSettings,
        allow_non_passing_ci: bool,
        statuses: Option<&PrStatusRows>,
    ) -> Result<ApprovalOutcome, Report<AppError>> {
        if let Some(statuses) = statuses {
            statuses.update(&info.repo, info.pr_number, "Inspecting merge strategy");
        }
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
        let mode = ApprovalWorkflow::plan(
            info.ci_status,
            &queue_status,
            settings.allow_auto_merge,
            allow_non_passing_ci,
        );

        if !(matches!(mode, ApprovalMode::AlreadyQueued)
            || matches!(mode, ApprovalMode::AlreadyAutoMergeEnabled)
                && queue_status.uses_merge_queue)
        {
            if let Some(statuses) = statuses {
                statuses.update(&info.repo, info.pr_number, "Approving pull request");
            }
            self.approve_pull_request(&info.owner, &info.repo_name, info.pr_number)
                .await?;
        }

        let outcome = match mode {
            ApprovalMode::Direct => {
                self.debug(&format!("PR #{} merge queue: not used", info.pr_number));
                if let Some(statuses) = statuses {
                    statuses.update(&info.repo, info.pr_number, "Merging");
                }
                self.direct_merge_pull_request(
                    &info.owner,
                    &info.repo_name,
                    info.pr_number,
                    settings.merge_method,
                    statuses,
                )
                .await?;
                ApprovalOutcome::Merged
            }
            ApprovalMode::AutoMerge => {
                self.debug(&format!(
                    "PR #{} merge queue: not used (enable regular auto-merge)",
                    info.pr_number
                ));
                self.enable_auto_merge_for_pull_request(
                    &queue_status.pull_request_id,
                    &queue_status.head_oid,
                    settings.merge_method,
                )
                .await?;
                ApprovalOutcome::AutoMerge
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
                    EnqueuePullRequestOutcome::Queued => ApprovalOutcome::Queued,
                    EnqueuePullRequestOutcome::Merged => ApprovalOutcome::AlreadyMerged,
                    EnqueuePullRequestOutcome::AwaitingRequiredChecks => {
                        self.debug(&format!(
                            "PR #{} cannot enter the merge queue yet; enabling auto-merge",
                            info.pr_number
                        ));
                        self.enable_auto_merge_for_pull_request(
                            &queue_status.pull_request_id,
                            &queue_status.head_oid,
                            settings.merge_method,
                        )
                        .await?;
                        ApprovalOutcome::AwaitingQueueChecks
                    }
                }
            }
            ApprovalMode::MergeQueueAutoMerge => {
                self.debug(&format!(
                    "PR #{} merge queue: used (auto-merge until queueable)",
                    info.pr_number
                ));
                self.enable_auto_merge_for_pull_request(
                    &queue_status.pull_request_id,
                    &queue_status.head_oid,
                    settings.merge_method,
                )
                .await?;
                ApprovalOutcome::QueueAutoMerge
            }
            ApprovalMode::AlreadyQueued => {
                self.debug(&format!(
                    "PR #{} merge queue: already queued",
                    info.pr_number
                ));
                ApprovalOutcome::AlreadyQueued
            }
            ApprovalMode::AlreadyAutoMergeEnabled => {
                let detail = if queue_status.uses_merge_queue {
                    "already enabled for merge queue"
                } else {
                    "already enabled (approval refreshed)"
                };
                self.debug(&format!("PR #{} auto-merge: {detail}", info.pr_number));
                ApprovalOutcome::AlreadyAutoMergeEnabled {
                    uses_merge_queue: queue_status.uses_merge_queue,
                }
            }
            ApprovalMode::SkipPendingWithoutQueue => {
                self.debug(&format!("PR #{} merge queue: not used", info.pr_number));
                ApprovalOutcome::Skipped
            }
        };
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests;
