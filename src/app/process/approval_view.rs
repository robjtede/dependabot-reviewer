//! Render approval previews, submission outcomes, and merge-wait status.

use std::io::IsTerminal as _;

use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm};
use error_stack::{Report, ResultExt as _};

use super::{
    actions::open_in_browser,
    approval::{MergeInfo, RepositoryMergeSettings},
    approval_submission::ApprovalOutcome,
    review::ReviewItem,
    status_rows::PrStatusRows,
};

pub(super) fn offer_browser_review(
    infos: &[MergeInfo],
    opened_in_browser: bool,
) -> Result<(), Report<AppError>> {
    let unreviewed = infos
        .iter()
        .filter(|info| !info.previously_reviewed)
        .collect::<Vec<_>>();
    if unreviewed.is_empty()
        || opened_in_browser
        || !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
    {
        return Ok(());
    }

    let open_urls = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "Open {} non-previously-reviewed PR(s) in browser before approve+merge?",
            unreviewed.len()
        ))
        .default(true)
        .interact()
        .change_context(AppError::Interactive)
        .attach("Browser-open confirmation failed")?;

    if open_urls {
        for info in unreviewed {
            println!(
                "  {} Running `open {}`",
                style("•").dim(),
                style(&info.url).dim()
            );
            open_in_browser(&info.url)?;
        }
    }

    Ok(())
}
use crate::{
    app::{
        approval_workflow::{ApprovalMode, ApprovalWorkflow},
        App,
    },
    error::AppError,
    github::CiStatus,
};

impl App {
    pub(super) async fn preview_approval(
        &self,
        item: &ReviewItem,
        settings: RepositoryMergeSettings,
        previously_reviewed: bool,
        allow_non_passing_ci: bool,
    ) -> Result<(), Report<AppError>> {
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

        let RepositoryMergeSettings {
            merge_method,
            allow_auto_merge,
        } = settings;
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

        Ok(())
    }
}

pub(super) fn show_approval_outcome(
    info: &MergeInfo,
    outcome: ApprovalOutcome,
    statuses: Option<&PrStatusRows>,
) {
    let (message, output, complete) = match outcome {
        ApprovalOutcome::Merged => (
            "Approved and merged".to_owned(),
            format!("Approved and merged PR #{}", info.pr_number),
            true,
        ),
        ApprovalOutcome::AutoMerge => (
            "Waiting for CI to pass (auto-merge enabled)".to_owned(),
            format!("Approved PR #{} and enabled auto-merge", info.pr_number),
            false,
        ),
        ApprovalOutcome::Queued => (
            "In merge queue".to_owned(),
            format!(
                "Approved PR #{} and added it to the merge queue",
                info.pr_number
            ),
            false,
        ),
        ApprovalOutcome::AlreadyMerged => (
            "Already merged".to_owned(),
            format!("PR #{} is already merged", info.pr_number),
            true,
        ),
        ApprovalOutcome::AwaitingQueueChecks => (
            "Waiting for CI to pass before joining merge queue".to_owned(),
            format!(
                "Approved PR #{} and enabled auto-merge while required checks complete",
                info.pr_number
            ),
            false,
        ),
        ApprovalOutcome::QueueAutoMerge => (
            "Waiting for CI to pass before joining merge queue".to_owned(),
            format!(
                "Approved PR #{} and enabled auto-merge for the merge queue",
                info.pr_number
            ),
            false,
        ),
        ApprovalOutcome::AlreadyQueued => (
            "In merge queue".to_owned(),
            format!("Approved PR #{} (already in merge queue)", info.pr_number),
            false,
        ),
        ApprovalOutcome::AlreadyAutoMergeEnabled { uses_merge_queue } => {
            let detail = if uses_merge_queue {
                "auto-merge already enabled"
            } else {
                "auto-merge already enabled; approval refreshed"
            };
            (
                "Waiting for merge (auto-merge enabled)".to_owned(),
                format!("Approved PR #{} ({detail})", info.pr_number),
                false,
            )
        }
        ApprovalOutcome::Skipped => {
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
            return;
        }
    };

    if let Some(statuses) = statuses {
        if complete {
            statuses.finish_success(&info.repo, info.pr_number, &message);
        } else {
            statuses.update(&info.repo, info.pr_number, &message);
        }
    } else {
        println!(
            "  {} {}{}",
            style("✓").green(),
            output,
            style(format!(" ({})", info.repo)).dim()
        );
    }
}

pub(super) fn update_merge_status(
    statuses: Option<&PrStatusRows>,
    owner: &str,
    repo: &str,
    number: u64,
    message: &str,
) {
    if let Some(statuses) = statuses {
        statuses.update(&format!("{owner}/{repo}"), number, message);
    } else {
        println!("  {owner}/{repo}#{number}: {message}");
    }
}

pub(super) fn finish_cancelled_merge(info: &MergeInfo, statuses: Option<&PrStatusRows>) {
    let message = "Stopped waiting; GitHub can still process the request";
    if let Some(statuses) = statuses {
        statuses.complete(&info.repo, info.pr_number, message);
    } else {
        println!("  {}#{}: {message}", info.repo, info.pr_number);
    }
}
