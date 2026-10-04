use console::style;
use error_stack::Report;
use octocrab::params::pulls::MergeMethod;

use super::{App, ReviewItem};
use crate::{
    app::approval_workflow::{ApprovalMode, ApprovalWorkflow},
    cli::Action,
    error::AppError,
    github::CiStatus,
};

impl App {
    pub(super) async fn preview_action(
        &self,
        action: Action,
        item: &ReviewItem,
        previously_reviewed: bool,
        context: Option<(MergeMethod, bool)>,
        allow_non_passing_ci: bool,
    ) -> Result<(), Report<AppError>> {
        match action {
            Action::OpenUnreviewedInBrowser => {
                if previously_reviewed {
                    return Ok(());
                }
                println!(
                    "  [DRY RUN] Would open PR #{}{}: {}",
                    item.pr.number,
                    style(format!(" ({})", item.repo)).dim(),
                    item.pr.url
                );
            }
            Action::Close => {
                println!(
                    "  [DRY RUN] Would close PR #{}{}: {}",
                    item.pr.number,
                    style(format!(" ({})", item.repo)).dim(),
                    item.pr.url
                );
            }
            Action::ApproveMerge => {
                if item.pr.ci_status == CiStatus::Failing && !allow_non_passing_ci {
                    println!(
                        "  [DRY RUN] Would skip PR #{} (CI {}){}: {}",
                        item.pr.number,
                        item.pr.ci_status,
                        style(format!(" ({})", item.repo)).dim(),
                        item.pr.url
                    );
                    return Ok(());
                }

                let (merge_method, allow_auto_merge) = context.expect("approve merge context");
                let queue_status = self
                    .fetch_merge_queue_status(
                        &item.owner,
                        &item.repo_name,
                        item.pr.number,
                        &item.pr.base_ref_name,
                    )
                    .await?;
                let merge_mode = ApprovalWorkflow::plan(
                    item.pr.ci_status,
                    &queue_status,
                    allow_auto_merge,
                    allow_non_passing_ci,
                );

                let marker = if previously_reviewed {
                    " [previously reviewed]"
                } else {
                    ""
                };
                match merge_mode {
                    ApprovalMode::Direct => {
                        self.debug(&format!("PR #{} merge queue: not used", item.pr.number));
                        if allow_non_passing_ci && item.pr.ci_status != CiStatus::Passing {
                            println!(
                                "  [DRY RUN] Would attempt approve and merge PR #{}{} with {:?} (CI {}){}: {}",
                                item.pr.number,
                                marker,
                                merge_method,
                                item.pr.ci_status,
                                style(format!(" ({})", item.repo)).dim(),
                                item.pr.url
                            );
                        } else {
                            println!(
                                "  [DRY RUN] Would approve and merge PR #{}{} with {:?}{}: {}",
                                item.pr.number,
                                marker,
                                merge_method,
                                style(format!(" ({})", item.repo)).dim(),
                                item.pr.url
                            );
                        }
                    }
                    ApprovalMode::AutoMerge => {
                        self.debug(&format!(
                            "PR #{} merge queue: not used (enable regular auto-merge)",
                            item.pr.number
                        ));
                        println!(
                            "  [DRY RUN] Would approve PR #{}{} and enable auto-merge{}: {}",
                            item.pr.number,
                            marker,
                            style(format!(" ({})", item.repo)).dim(),
                            item.pr.url
                        );
                    }
                    ApprovalMode::MergeQueueEnqueue => {
                        self.debug(&format!(
                            "PR #{} merge queue: used (enqueue)",
                            item.pr.number
                        ));
                        println!(
                            "  [DRY RUN] Would approve and add PR #{}{} to the merge queue{}: {}",
                            item.pr.number,
                            marker,
                            style(format!(" ({})", item.repo)).dim(),
                            item.pr.url
                        );
                    }
                    ApprovalMode::MergeQueueAutoMerge => {
                        self.debug(&format!(
                            "PR #{} merge queue: used (auto-merge until queueable)",
                            item.pr.number
                        ));
                        if allow_non_passing_ci && item.pr.ci_status == CiStatus::Failing {
                            println!(
                                "  [DRY RUN] Would attempt approval for PR #{}{} and enable auto-merge for the merge queue despite CI failing{}: {}",
                                item.pr.number,
                                marker,
                                style(format!(" ({})", item.repo)).dim(),
                                item.pr.url
                            );
                        } else {
                            println!(
                                "  [DRY RUN] Would approve PR #{}{} and enable auto-merge for the merge queue{}: {}",
                                item.pr.number,
                                marker,
                                style(format!(" ({})", item.repo)).dim(),
                                item.pr.url
                            );
                        }
                    }
                    ApprovalMode::AlreadyQueued => {
                        self.debug(&format!(
                            "PR #{} merge queue: already queued",
                            item.pr.number
                        ));
                        println!(
                            "  [DRY RUN] Would approve PR #{}{} (already in merge queue){}: {}",
                            item.pr.number,
                            marker,
                            style(format!(" ({})", item.repo)).dim(),
                            item.pr.url
                        );
                    }
                    ApprovalMode::AlreadyAutoMergeEnabled => {
                        self.debug(&format!(
                            "PR #{} auto-merge: already enabled",
                            item.pr.number
                        ));
                        println!(
                            "  [DRY RUN] Would approve PR #{}{} (auto-merge already enabled){}: {}",
                            item.pr.number,
                            marker,
                            style(format!(" ({})", item.repo)).dim(),
                            item.pr.url
                        );
                    }
                    ApprovalMode::SkipPendingWithoutQueue => {
                        self.debug(&format!("PR #{} merge queue: not used", item.pr.number));
                        println!(
                            "  [DRY RUN] Would skip PR #{} (CI {}, no merge queue){}: {}",
                            item.pr.number,
                            item.pr.ci_status,
                            style(format!(" ({})", item.repo)).dim(),
                            item.pr.url
                        );
                    }
                }
            }
            _ => {
                println!(
                    "  [DRY RUN] Would comment on PR #{}{}: {}",
                    item.pr.number,
                    style(format!(" ({})", item.repo)).dim(),
                    item.pr.url
                );
            }
        }

        Ok(())
    }
}
