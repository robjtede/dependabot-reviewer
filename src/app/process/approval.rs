//! Submit approval batches and watch queued PRs while submissions continue.

use std::collections::HashMap;

use console::style;
use error_stack::{Report, ResultExt as _};
use octocrab::params::pulls::MergeMethod;

use super::{
    approval_submission::ApprovalOutcome,
    approval_view::{finish_cancelled_merge, offer_browser_review, show_approval_outcome},
    merge_batch::{process_merge_batch, MergeSkipped},
    review::ReviewItem,
    status_rows::PrStatusRows,
    ReviewSession,
};
use crate::{
    app::{
        merge_results::{
            fetch_merge_progress, monitor_submitted_merges, MergeProgress, MergeWaitCancelled,
        },
        state::ReviewState,
        App,
    },
    error::AppError,
    github::{CiStatus, DepUpdate},
};

pub(super) struct MergeInfo {
    pub(super) repo: String,
    pub(super) owner: String,
    pub(super) repo_name: String,
    pub(super) pr_number: u64,
    pub(super) url: String,
    pub(super) base_ref_name: String,
    pub(super) ci_status: CiStatus,
    pub(super) dep_update: Option<DepUpdate>,
    pub(super) previously_reviewed: bool,
}

#[derive(Clone, Copy)]
pub(super) struct RepositoryMergeSettings {
    pub(super) merge_method: MergeMethod,
    pub(super) allow_auto_merge: bool,
}

pub(super) struct ApprovalBatch {
    items: Vec<ReviewItem>,
    contexts: HashMap<String, RepositoryMergeSettings>,
    allow_non_passing_ci: bool,
}

#[derive(Debug)]
pub(super) struct ApprovalResults {
    pub(super) performed_action: bool,
    total: usize,
    failures: Vec<(String, u64, Report<AppError>)>,
}

impl ApprovalResults {
    pub(super) fn finish(mut self) -> Result<(), Report<AppError>> {
        self.failures.retain(|(_, _, error)| {
            error.downcast_ref::<MergeWaitCancelled>().is_none()
                && !matches!(
                    error.downcast_ref::<MergeSkipped>(),
                    Some(MergeSkipped::Cancelled)
                )
        });
        if self.failures.is_empty() {
            return Ok(());
        }

        let mut report = Report::new(AppError::ApproveMerge).attach(format!(
            "{} of {} PR(s) failed or were skipped",
            self.failures.len(),
            self.total,
        ));
        for (repo, number, error) in self.failures {
            report = report.attach(format!("{repo}#{number}: {error:?}"));
        }
        Err(report)
    }
}

impl ApprovalBatch {
    pub(super) async fn prepare(
        app: &App,
        repos: &[String],
        items: Vec<ReviewItem>,
        allow_non_passing_ci: bool,
    ) -> Result<Self, Report<AppError>> {
        let mut contexts = std::collections::HashMap::new();
        for repo in repos {
            let (owner, repo_name) = repo
                .split_once('/')
                .ok_or_else(|| Report::new(AppError::InvalidInput))
                .attach_with(|| format!("Invalid repo format: {}", repo))?;
            let repo_info = app
                .octocrab
                .repos(owner, repo_name)
                .get()
                .await
                .change_context(AppError::ApproveMerge)
                .attach_with(|| format!("Failed to get repo info for {}", repo))?;
            contexts.insert(
                repo.clone(),
                RepositoryMergeSettings {
                    merge_method: preferred_merge_method(&repo_info)?,
                    allow_auto_merge: repo_info.allow_auto_merge == Some(true),
                },
            );
        }

        Ok(Self {
            items,
            contexts,
            allow_non_passing_ci,
        })
    }

    pub(super) async fn preview(
        &self,
        app: &App,
        state: &ReviewState,
    ) -> Result<(), Report<AppError>> {
        for item in &self.items {
            app.preview_approval(
                item,
                self.settings(&item.repo)?,
                item.previously_reviewed(state),
                self.allow_non_passing_ci,
            )
            .await?;
        }
        Ok(())
    }

    fn settings(&self, repo: &str) -> Result<RepositoryMergeSettings, Report<AppError>> {
        self.contexts.get(repo).copied().ok_or_else(|| {
            Report::new(AppError::ApproveMerge).attach(format!("Merge settings missing for {repo}"))
        })
    }

    pub(super) fn pr_urls(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|item| item.pr.api_url.as_str())
    }

    fn merge_infos(&self, state: &ReviewState) -> Vec<MergeInfo> {
        let mut infos = Vec::new();
        for item in &self.items {
            if item.pr.ci_status == CiStatus::Failing && !self.allow_non_passing_ci {
                println!(
                    "  {} Skipping PR #{} (CI {}){}",
                    style("⊘").yellow(),
                    item.pr.number,
                    item.pr.ci_status,
                    style(format!(" ({})", item.repo)).dim(),
                );
                continue;
            }
            infos.push(MergeInfo {
                repo: item.repo.clone(),
                owner: item.owner.clone(),
                repo_name: item.repo_name.clone(),
                pr_number: item.pr.number,
                url: item.pr.url.clone(),
                base_ref_name: item.pr.base_ref_name.clone(),
                ci_status: item.pr.ci_status,
                dep_update: item.pr.dep_update.clone(),
                previously_reviewed: item.previously_reviewed(state),
            });
        }
        infos
    }

    pub(super) async fn run(
        &self,
        app: &App,
        session: &mut ReviewSession,
    ) -> Result<ApprovalResults, Report<AppError>> {
        let merge_infos = self.merge_infos(&session.review_state);
        let mut performed_action = false;
        if merge_infos.is_empty() {
            return Ok(ApprovalResults {
                performed_action,
                total: 0,
                failures: Vec::new(),
            });
        }

        offer_browser_review(&merge_infos, session.opened_in_browser)?;
        println!("Waiting for merge results. Press Ctrl+C to stop waiting. GitHub can still process submitted requests.");
        let pr_statuses = (!app.cli.verbose)
            .then(|| {
                PrStatusRows::new(
                    merge_infos
                        .iter()
                        .map(|info| (info.repo.clone(), info.pr_number)),
                )
            })
            .flatten();

        // Each direct merge changes the base branch. Keep submissions in order.
        let (pending_tx, pending_rx) = tokio::sync::mpsc::unbounded_channel();
        let review_state = &mut session.review_state;
        let state_path = &session.state_path;
        let mut state_changed = false;

        let submit_merges = async {
            let failures = process_merge_batch(
                &merge_infos,
                async |info| {
                    let outcome = app
                        .submit_approval(
                            info,
                            self.settings(&info.repo)?,
                            self.allow_non_passing_ci,
                            pr_statuses.as_ref(),
                        )
                        .await?;
                    show_approval_outcome(info, outcome, pr_statuses.as_ref());
                    if matches!(outcome, ApprovalOutcome::Skipped) {
                        return Ok(());
                    }
                    if outcome.needs_monitoring() {
                        pending_tx.send(info).map_err(|_closed| {
                            Report::new(AppError::ApproveMerge).attach(MergeWaitCancelled)
                        })?;
                    }
                    if let Some(update) = &info.dep_update {
                        review_state.record_approved(update);
                        state_changed = true;
                    }
                    performed_action = true;
                    Ok(())
                },
                async |info, error| {
                    app.handle_merge_failure(info, error, pr_statuses.as_ref())
                        .await
                },
                tokio::signal::ctrl_c(),
            )
            .await;

            // Save approvals while queued PRs continue to be monitored.
            if state_changed {
                review_state.save_to_path(state_path)?;
                let show_saved_state = || {
                    println!(
                        "  {} Updated review state at {}",
                        style("✓").green(),
                        style(state_path.as_str()).dim(),
                    )
                };
                if let Some(statuses) = &pr_statuses {
                    statuses.suspend(show_saved_state);
                } else {
                    show_saved_state();
                }
            }
            drop(pending_tx);
            Ok::<_, Report<AppError>>(failures)
        };
        let watch_merges = async {
            let submissions = futures_util::stream::unfold(pending_rx, async |mut receiver| {
                receiver.recv().await.map(|info| (info, receiver))
            });
            let results = monitor_submitted_merges(
                submissions,
                async |info| {
                    fetch_merge_progress(
                        &app.octocrab,
                        &info.owner,
                        &info.repo_name,
                        info.pr_number,
                    )
                    .await
                },
                |info, progress| {
                    if let Some(statuses) = &pr_statuses {
                        if *progress == MergeProgress::Merged {
                            statuses.finish_success(&info.repo, info.pr_number, progress.message());
                        } else {
                            statuses.update(&info.repo, info.pr_number, progress.message());
                        }
                    } else {
                        println!("  {}#{}: {}", info.repo, info.pr_number, progress.message());
                    }
                },
                async |info, error| {
                    app.handle_merge_failure(info, error, pr_statuses.as_ref())
                        .await
                },
                tokio::signal::ctrl_c(),
            )
            .await;

            Ok::<_, Report<AppError>>(results)
        };

        let (mut failures, results) = tokio::try_join!(submit_merges, watch_merges)?;
        for info in results.cancelled {
            finish_cancelled_merge(info, pr_statuses.as_ref());
        }
        failures.extend(results.failures);
        if let Some(statuses) = pr_statuses {
            statuses.finish();
        }

        Ok(ApprovalResults {
            performed_action,
            total: merge_infos.len(),
            failures: failures
                .into_iter()
                .map(|(info, error)| (info.repo.clone(), info.pr_number, error))
                .collect(),
        })
    }
}

fn preferred_merge_method(
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

#[cfg(test)]
mod tests;
