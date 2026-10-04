mod approval;
mod dry_run;
mod merge_batch;
mod review;
mod status_rows;

#[cfg(test)]
mod test_support;

use std::{
    collections::{HashMap, HashSet},
    io::IsTerminal as _,
    process::Command,
};

use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, Select};
use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt;
use futures_util::{FutureExt as _, StreamExt as _};
use octocrab::params::pulls::State as PullRequestState;

use self::{
    approval::{preferred_merge_method, ApprovalOutcome},
    merge_batch::{finish_cancelled_merge, process_merge_batch, MergeSkipped},
    review::{failing_ci_agent_prompt, has_actions_lock, review_badges},
    status_rows::{should_render_status_rows, PrStatusRows},
};
use super::{
    merge_results::{
        fetch_merge_progress, monitor_submitted_merges, MergeProgress, MergeWaitCancelled,
    },
    state::ReviewState,
    App,
};
use crate::{
    cli::Action,
    error::AppError,
    github::{CiStatus, DepUpdate, PrInfo},
};

struct ReviewItem {
    repo: String,
    owner: String,
    repo_name: String,
    pr: PrInfo,
    actions_lock_check: Option<Result<bool, Report<AppError>>>,
}

struct MergeInfo {
    repo: String,
    owner: String,
    repo_name: String,
    pr_number: u64,
    url: String,
    base_ref_name: String,
    ci_status: CiStatus,
    dep_update: Option<DepUpdate>,
    previously_reviewed: bool,
}

#[derive(Clone, Copy)]
enum PromptChoice {
    Refresh,
    PrintFailingCiPrompt,
    ApproveMergeIncludingNonPassingCi,
    Action(Action),
}

impl App {
    pub(crate) async fn process_repositories(
        &self,
        repos: &[String],
        processed_pr_urls: &mut HashSet<String>,
    ) -> Result<Option<Action>, Report<AppError>> {
        let state_path = ReviewState::default_path()?;
        self.debug(&format!("Reading state from {}", state_path));
        let mut review_state = ReviewState::load_from_path(&state_path)?;

        let mut performed_action = None;
        let mut opened_in_browser_in_session = false;
        loop {
            println!("Fetching PR details for {} repositories", repos.len());

            let mut review_items = Vec::new();
            for repo in repos {
                let (owner, repo_name) = repo
                    .split_once('/')
                    .ok_or_else(|| Report::new(AppError::InvalidInput))
                    .attach_with(|| format!("Invalid repo format: {}", repo))?;

                let prs = self.fetch_dependabot_prs_for_repo(repo).await?;
                review_items.extend(prs.into_iter().map(|pr| ReviewItem {
                    repo: repo.clone(),
                    owner: owner.to_string(),
                    repo_name: repo_name.to_string(),
                    pr,
                    actions_lock_check: None,
                }));
            }

            review_items.sort_by(|a, b| {
                a.repo
                    .cmp(&b.repo)
                    .then_with(|| b.pr.number.cmp(&a.pr.number))
            });

            if review_items.is_empty() {
                println!("  No open Dependabot PRs found in the selected repositories.");
                return Ok(performed_action);
            }

            let pending_statuses = self.fetch_pending_review_statuses(&review_items).await?;

            let mut lockfiles = HashMap::new();
            for item in &mut review_items {
                item.actions_lock_check =
                    Some(has_actions_lock(&self.octocrab, item, &mut lockfiles).await);
            }

            println!("  Found {} Dependabot PR(s):", review_items.len());
            println!("  Review state: {}", style(state_path.as_str()).dim());
            let mut current_repo: Option<&str> = None;
            for item in &review_items {
                if current_repo != Some(item.repo.as_str()) {
                    if current_repo.is_some() {
                        println!();
                    }
                    println!("  {}", style(&item.repo).bold());
                    current_repo = Some(item.repo.as_str());
                }

                let previously_reviewed = item
                    .pr
                    .dep_update
                    .as_ref()
                    .map(|dep_update| review_state.is_previously_reviewed(dep_update))
                    .unwrap_or(false);
                let pending_status = pending_statuses
                    .get(item.repo.as_str())
                    .and_then(|statuses| statuses.get(&item.pr.number));
                let badges = review_badges(
                    previously_reviewed,
                    pending_status,
                    item.actions_lock_check
                        .as_ref()
                        .expect("lockfile check ran"),
                );

                println!(
                    "    {} #{}: {} [{}]\n        {}",
                    item.pr.ci_status.icon(),
                    item.pr.number,
                    item.pr.title,
                    badges,
                    style(&item.pr.url).dim()
                );
                if item.pr.dep_update.is_none() {
                    println!(
                        "      {}",
                        style("No dependency/version metadata parsed from PR title").dim()
                    );
                }
                println!();
            }
            println!();

            let prompt_choice = if let Some(action) = self.cli.action {
                PromptChoice::Action(action)
            } else {
                let mut choices = vec![(
                    "Approve + Merge",
                    PromptChoice::Action(Action::ApproveMerge),
                )];

                if review_items
                    .iter()
                    .any(|item| matches!(item.pr.ci_status, CiStatus::Failing | CiStatus::Pending))
                {
                    choices.push((
                        "Approve + Merge (including failing and pending CI)",
                        PromptChoice::ApproveMergeIncludingNonPassingCi,
                    ));
                }

                choices.extend([
                    (
                        "Open Unreviewed In Browser",
                        PromptChoice::Action(Action::OpenUnreviewedInBrowser),
                    ),
                    ("Rebase", PromptChoice::Action(Action::Rebase)),
                    ("Recreate", PromptChoice::Action(Action::Recreate)),
                    ("Close", PromptChoice::Action(Action::Close)),
                    (
                        "Print Agent Prompt for Failing CI",
                        PromptChoice::PrintFailingCiPrompt,
                    ),
                    ("Refresh PR State", PromptChoice::Refresh),
                ]);

                let items: Vec<_> = choices.iter().map(|(label, _)| *label).collect();
                let selection = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Choose action to apply to these PRs")
                    .items(&items)
                    .default(0)
                    .interact()
                    .change_context(AppError::ActionSelection)
                    .attach("Action selection failed")?;
                choices
                    .get(selection)
                    .map(|(_, choice)| *choice)
                    .ok_or_else(|| {
                        Report::new(AppError::ActionSelection).attach(format!(
                            "Action selection {selection} is outside the available options"
                        ))
                    })?
            };

            let (action, allow_non_passing_ci) = match prompt_choice {
                PromptChoice::Refresh => {
                    println!();
                    continue;
                }
                PromptChoice::PrintFailingCiPrompt => {
                    match failing_ci_agent_prompt(&review_items) {
                        Some(prompt) => println!("{}", prompt),
                        None => println!("  No Dependabot PRs have failing CI."),
                    }
                    return Ok(performed_action);
                }
                PromptChoice::ApproveMergeIncludingNonPassingCi => (Action::ApproveMerge, true),
                PromptChoice::Action(action) => (action, self.cli.allow_non_passing_ci),
            };

            if matches!(action, Action::ApproveMerge) {
                let mut lockfiles = HashMap::new();
                let mut eligible_items = Vec::new();

                for item in review_items {
                    if has_actions_lock(&self.octocrab, &item, &mut lockfiles).await? {
                        println!(
                            "  {} Skipping {}#{}: .github/workflows/actions.lock exists on {}; Dependabot cannot update this lockfile.",
                            style("⊘").yellow(), item.repo, item.pr.number, item.pr.base_ref_name,
                        );
                    } else {
                        eligible_items.push(item);
                    }
                }

                review_items = eligible_items;

                if review_items.is_empty() {
                    return Ok(performed_action);
                }
            }

            let approve_merge_context = if matches!(action, Action::ApproveMerge) {
                let mut contexts = std::collections::HashMap::new();
                for repo in repos {
                    let (owner, repo_name) = repo
                        .split_once('/')
                        .ok_or_else(|| Report::new(AppError::InvalidInput))
                        .attach_with(|| format!("Invalid repo format: {}", repo))?;
                    let repo_info = self
                        .octocrab
                        .repos(owner, repo_name)
                        .get()
                        .await
                        .change_context(AppError::ApproveMerge)
                        .attach_with(|| format!("Failed to get repo info for {}", repo))?;
                    contexts.insert(
                        repo.clone(),
                        (
                            preferred_merge_method(&repo_info)?,
                            repo_info.allow_auto_merge == Some(true),
                        ),
                    );
                }
                Some(contexts)
            } else {
                None
            };

            let mut action_tasks = Vec::new();
            let mut merge_infos: Vec<MergeInfo> = Vec::new();
            let mut state_changed = false;
            let mut merge_failures = Vec::new();
            let mut pr_statuses = (should_render_status_rows(self.cli.dry_run, self.cli.verbose)
                && !matches!(action, Action::ApproveMerge))
            .then(|| {
                PrStatusRows::new(
                    review_items
                        .iter()
                        .map(|item| (item.repo.clone(), item.pr.number)),
                )
            })
            .flatten();

            for item in &review_items {
                let previously_reviewed = item
                    .pr
                    .dep_update
                    .as_ref()
                    .map(|dep_update| review_state.is_previously_reviewed(dep_update))
                    .unwrap_or(false);

                if self.cli.dry_run {
                    self.preview_action(
                        action,
                        item,
                        previously_reviewed,
                        approve_merge_context
                            .as_ref()
                            .and_then(|contexts| contexts.get(&item.repo))
                            .copied(),
                        allow_non_passing_ci,
                    )
                    .await?;
                } else {
                    let pr_number = item.pr.number;
                    let octocrab = self.octocrab.clone();

                    match action {
                        Action::OpenUnreviewedInBrowser => {
                            if previously_reviewed {
                                if let Some(statuses) = &pr_statuses {
                                    statuses.finish_skipped(
                                        &item.repo,
                                        pr_number,
                                        "Already reviewed",
                                    );
                                }
                                continue;
                            }
                            if let Some(statuses) = &pr_statuses {
                                statuses.update(&item.repo, pr_number, "Opening in browser");
                            } else {
                                println!(
                                    "  {} Running `open {}`",
                                    style("•").dim(),
                                    style(&item.pr.url).dim()
                                );
                            }
                            open_in_browser(&item.pr.url)?;
                            processed_pr_urls.insert(item.pr.api_url.clone());
                            if let Some(statuses) = &pr_statuses {
                                statuses.finish_success(&item.repo, pr_number, "Opened in browser");
                            } else {
                                println!(
                                    "  {} Opened PR #{}{}",
                                    style("✓").green(),
                                    pr_number,
                                    style(format!(" ({})", item.repo)).dim()
                                );
                            }
                            performed_action = Some(action);
                            opened_in_browser_in_session = true;
                        }
                        Action::Close => {
                            if let Some(statuses) = &pr_statuses {
                                statuses.update(&item.repo, pr_number, "Closing pull request");
                            }
                            let owner = item.owner.clone();
                            let repo_name = item.repo_name.clone();
                            let repo = item.repo.clone();
                            action_tasks.push(
                                async move {
                                    self.close_pull_request(&owner, &repo_name, pr_number)
                                        .await?;

                                    Ok::<_, Report<_>>((pr_number, repo))
                                }
                                .boxed(),
                            );
                        }
                        Action::Rebase => {
                            if let Some(statuses) = &pr_statuses {
                                statuses.update(
                                    &item.repo,
                                    pr_number,
                                    "Requesting Dependabot rebase",
                                );
                            }
                            let owner = item.owner.clone();
                            let repo_name = item.repo_name.clone();
                            let repo = item.repo.clone();
                            action_tasks.push(
                                async move {
                                    self.debug(&format!("Commenting on PR #{}", pr_number));

                                    octocrab
                                        .issues(owner, repo_name)
                                        .create_comment(pr_number, "@dependabot rebase")
                                        .await
                                        .change_context(AppError::Comment)
                                        .attach(format!(
                                            "Failed to comment on PR #{}",
                                            pr_number
                                        ))?;

                                    Ok::<_, Report<_>>((pr_number, repo))
                                }
                                .boxed(),
                            );
                        }
                        Action::Recreate => {
                            if let Some(statuses) = &pr_statuses {
                                statuses.update(
                                    &item.repo,
                                    pr_number,
                                    "Requesting Dependabot recreation",
                                );
                            }
                            let owner = item.owner.clone();
                            let repo_name = item.repo_name.clone();
                            let repo = item.repo.clone();
                            action_tasks.push(
                                async move {
                                    self.debug(&format!("Commenting on PR #{}", pr_number));

                                    octocrab
                                        .issues(owner, repo_name)
                                        .create_comment(pr_number, "@dependabot recreate")
                                        .await
                                        .change_context(AppError::Comment)
                                        .attach(format!(
                                            "Failed to comment on PR #{}",
                                            pr_number
                                        ))?;

                                    Ok::<_, Report<_>>((pr_number, repo))
                                }
                                .boxed(),
                            );
                        }
                        Action::ApproveMerge => {
                            if item.pr.ci_status != CiStatus::Failing || allow_non_passing_ci {
                                merge_infos.push(MergeInfo {
                                    repo: item.repo.clone(),
                                    owner: item.owner.clone(),
                                    repo_name: item.repo_name.clone(),
                                    pr_number,
                                    url: item.pr.url.clone(),
                                    base_ref_name: item.pr.base_ref_name.clone(),
                                    ci_status: item.pr.ci_status,
                                    dep_update: item.pr.dep_update.clone(),
                                    previously_reviewed,
                                });
                            } else {
                                if let Some(statuses) = &pr_statuses {
                                    statuses.finish_skipped(
                                        &item.repo,
                                        pr_number,
                                        &format!("Skipped: CI {}", item.pr.ci_status),
                                    );
                                } else {
                                    println!(
                                        "  {} Skipping PR #{} (CI {}){}",
                                        style("⊘").yellow(),
                                        pr_number,
                                        item.pr.ci_status,
                                        style(format!(" ({})", item.repo)).dim()
                                    );
                                }
                            }
                        }
                    }
                }
            }

            if !action_tasks.is_empty() {
                let (status, output) = if matches!(action, Action::Close) {
                    ("Closed pull request", "Closed")
                } else {
                    ("Dependabot request sent", "Commented on")
                };
                let mut stream = futures_util::stream::iter(action_tasks).buffered_unordered(5);

                while let Some(result) = stream.next().await {
                    let (pr_number, repo) = result?;
                    if let Some(statuses) = &pr_statuses {
                        statuses.finish_success(&repo, pr_number, status);
                    } else {
                        println!(
                            "  {} {} PR #{}{}",
                            style("✓").green(),
                            output,
                            pr_number,
                            style(format!(" ({})", repo)).dim()
                        );
                    }
                    performed_action = Some(action);
                }
            }

            if !merge_infos.is_empty() {
                let non_previously_reviewed: Vec<_> = merge_infos
                    .iter()
                    .filter(|info| !info.previously_reviewed)
                    .collect();

                if !non_previously_reviewed.is_empty()
                    && !opened_in_browser_in_session
                    && std::io::stdin().is_terminal()
                    && std::io::stdout().is_terminal()
                {
                    let open_urls = Confirm::with_theme(&ColorfulTheme::default())
                        .with_prompt(format!(
                            "Open {} non-previously-reviewed PR(s) in browser before approve+merge?",
                            non_previously_reviewed.len()
                        ))
                        .default(true)
                        .interact()
                        .change_context(AppError::Interactive)
                        .attach("Browser-open confirmation failed")?;

                    if open_urls {
                        for info in &non_previously_reviewed {
                            if let Some(statuses) = &pr_statuses {
                                statuses.update(
                                    &info.repo,
                                    info.pr_number,
                                    "Opening in browser before approval",
                                );
                            } else {
                                println!(
                                    "  {} Running `open {}`",
                                    style("•").dim(),
                                    style(&info.url).dim()
                                );
                            }
                            open_in_browser(&info.url)?;
                            if let Some(statuses) = &pr_statuses {
                                statuses.update(&info.repo, info.pr_number, "Waiting for approval");
                            }
                        }
                    }
                }

                let show_wait_hint = || {
                    println!("Waiting for merge results. Press Ctrl+C to stop waiting. GitHub can still process submitted requests.")
                };
                if let Some(statuses) = &pr_statuses {
                    statuses.suspend(show_wait_hint);
                } else {
                    show_wait_hint();
                }

                if !self.cli.verbose && pr_statuses.is_none() {
                    pr_statuses = PrStatusRows::new(
                        merge_infos
                            .iter()
                            .map(|info| (info.repo.clone(), info.pr_number)),
                    );
                }

                // Submit direct merges in order because each merge changes the base
                // branch. Watch queued PRs while the remaining submissions run.
                let (pending_tx, pending_rx) = tokio::sync::mpsc::unbounded_channel();

                let submit_merges = async {
                    let failures = process_merge_batch(
                        &merge_infos,
                        async |info| {
                            let context = approve_merge_context
                                .as_ref()
                                .and_then(|contexts| contexts.get(&info.repo))
                                .copied()
                                .expect("approve merge context");
                            let outcome = self
                                .approve_and_merge(
                                    info,
                                    context,
                                    allow_non_passing_ci,
                                    pr_statuses.as_ref(),
                                )
                                .await?;

                            let watch_for_merge = match outcome {
                                ApprovalOutcome::Completed => false,
                                ApprovalOutcome::WatchForMerge => true,
                                ApprovalOutcome::Skipped => return Ok(()),
                            };

                            if watch_for_merge {
                                pending_tx.send(info).map_err(|_closed| {
                                    Report::new(AppError::ApproveMerge).attach(MergeWaitCancelled)
                                })?;
                            }

                            if let Some(dep_update) = &info.dep_update {
                                review_state.record_approved(dep_update);
                                state_changed = true;
                            }

                            performed_action = Some(action);

                            Ok(())
                        },
                        async |info, error| {
                            self.handle_merge_failure(info, error, pr_statuses.as_ref())
                                .await
                        },
                        tokio::signal::ctrl_c(),
                    )
                    .await;

                    // Save approvals while queued PRs continue to be monitored.
                    if state_changed {
                        review_state.save_to_path(&state_path)?;
                        state_changed = false;

                        let show_saved_state = || {
                            println!(
                                "  {} Updated review state at {}",
                                style("✓").green(),
                                style(state_path.as_str()).dim()
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
                    let submissions =
                        futures_util::stream::unfold(pending_rx, async |mut receiver| {
                            receiver.recv().await.map(|info| (info, receiver))
                        });
                    let results = monitor_submitted_merges(
                        submissions,
                        async |info| {
                            fetch_merge_progress(
                                &self.octocrab,
                                &info.owner,
                                &info.repo_name,
                                info.pr_number,
                            )
                            .await
                        },
                        |info, progress| {
                            if let Some(statuses) = &pr_statuses {
                                if *progress == MergeProgress::Merged {
                                    statuses.finish_success(
                                        &info.repo,
                                        info.pr_number,
                                        progress.message(),
                                    );
                                } else {
                                    statuses.update(&info.repo, info.pr_number, progress.message());
                                }
                            } else {
                                println!(
                                    "  {}#{}: {}",
                                    info.repo,
                                    info.pr_number,
                                    progress.message()
                                );
                            }
                        },
                        async |info, error| {
                            self.handle_merge_failure(info, error, pr_statuses.as_ref())
                                .await
                        },
                        tokio::signal::ctrl_c(),
                    )
                    .await;

                    Ok::<_, Report<AppError>>(results)
                };
                let (failures, results) = tokio::try_join!(submit_merges, watch_merges)?;
                merge_failures = failures;

                for info in results.cancelled {
                    finish_cancelled_merge(info, pr_statuses.as_ref());
                }

                merge_failures.extend(results.failures);
            }

            if let Some(statuses) = pr_statuses.take() {
                statuses.finish();
            }

            if state_changed {
                review_state.save_to_path(&state_path)?;
                println!(
                    "  {} Updated review state at {}",
                    style("✓").green(),
                    style(state_path.as_str()).dim()
                );
            }

            if !self.cli.dry_run && !matches!(action, Action::OpenUnreviewedInBrowser) {
                processed_pr_urls.extend(review_items.into_iter().map(|item| item.pr.api_url));
            }

            merge_failures.retain(|(_, error)| {
                error.downcast_ref::<MergeWaitCancelled>().is_none()
                    && !matches!(
                        error.downcast_ref::<MergeSkipped>(),
                        Some(MergeSkipped::Cancelled)
                    )
            });

            if !merge_failures.is_empty() {
                let mut report = Report::new(AppError::ApproveMerge).attach(format!(
                    "{} of {} PR(s) failed or were skipped",
                    merge_failures.len(),
                    merge_infos.len(),
                ));

                for (info, error) in merge_failures {
                    report = report.attach(format!("{}#{}: {error:?}", info.repo, info.pr_number));
                }

                return Err(report);
            }

            if matches!(action, Action::OpenUnreviewedInBrowser)
                && !self.cli.dry_run
                && !opened_in_browser_in_session
            {
                println!("  No unreviewed PRs to open.");
            }

            if self.cli.action.is_some() || !matches!(action, Action::OpenUnreviewedInBrowser) {
                return Ok(performed_action);
            }

            println!();
        }
    }

    async fn close_pull_request(
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
}

fn open_in_browser(url: &str) -> Result<(), Report<AppError>> {
    let status = Command::new("open")
        .arg(url)
        .status()
        .change_context(AppError::Interactive)
        .attach_with(|| format!("Failed to run open for {}", url))?;
    if !status.success() {
        return Err(Report::new(AppError::Interactive))
            .attach_with(|| format!("open failed for {}", url));
    }
    Ok(())
}
