use std::{
    collections::HashMap,
    future::Future,
    io::{self, IsTerminal as _},
    process::Command,
    time::Duration,
};

use console::style;
use derive_more::Display;
use dialoguer::{theme::ColorfulTheme, Confirm, Select};
use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt;
use futures_util::{FutureExt as _, StreamExt as _};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use octocrab::{
    models::pulls::ReviewAction,
    params::pulls::{MergeMethod, State as PullRequestState},
};
use serde::{Deserialize, Serialize};

use super::{
    approval_workflow::{ApprovalMode, ApprovalWorkflow, MergeQueueStatus},
    async_merge::{AsyncMerge, AsyncMergeError, MergeOperation, MergeOutcome},
    merge_results::{
        fetch_merge_progress, monitor_submitted_merges, wait_for_async_merge, MergeProgress,
        MergeWaitCancelled,
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

struct PrStatusRows {
    _multi_progress: MultiProgress,
    bars: HashMap<(String, u64), ProgressBar>,
}

impl PrStatusRows {
    fn new(prs: impl IntoIterator<Item = (String, u64)>) -> Option<Self> {
        if !std::io::stdout().is_terminal() {
            return None;
        }

        Some(Self::with_draw_target(prs, ProgressDrawTarget::stdout()))
    }

    fn with_draw_target(
        prs: impl IntoIterator<Item = (String, u64)>,
        draw_target: ProgressDrawTarget,
    ) -> Self {
        let multi_progress = MultiProgress::with_draw_target(draw_target);
        multi_progress.set_move_cursor(true);
        let style = ProgressStyle::with_template("  {spinner:.dim} {msg}")
            .expect("progress status template is valid");
        let mut bars = HashMap::new();

        for (repo, pr_number) in prs {
            let bar = multi_progress.add(ProgressBar::new_spinner());
            bar.set_style(style.clone());
            bar.enable_steady_tick(Duration::from_millis(120));
            bar.set_message(Self::message(&repo, pr_number, "Waiting to process"));
            bars.insert((repo, pr_number), bar);
        }

        Self {
            _multi_progress: multi_progress,
            bars,
        }
    }

    fn update(&self, repo: &str, pr_number: u64, status: &str) {
        if let Some(bar) = self.bars.get(&(repo.to_owned(), pr_number)) {
            bar.set_message(Self::message(repo, pr_number, status));
        }
    }

    fn finish_success(&self, repo: &str, pr_number: u64, status: &str) {
        self.complete(repo, pr_number, &format!("✓ {status}"));
    }

    fn finish_skipped(&self, repo: &str, pr_number: u64, status: &str) {
        self.complete(repo, pr_number, &format!("⊘ {status}"));
    }

    fn complete(&self, repo: &str, pr_number: u64, status: &str) {
        if let Some(bar) = self.bars.get(&(repo.to_owned(), pr_number)) {
            bar.disable_steady_tick();
            bar.set_style(
                ProgressStyle::with_template("  {msg}")
                    .expect("completed status template is valid"),
            );
            bar.set_message(Self::message(repo, pr_number, status));
        }
    }

    fn finish(&self) {
        for bar in self.bars.values() {
            bar.finish();
        }
        println!();
    }

    fn suspend<T>(&self, callback: impl FnOnce() -> T) -> T {
        self._multi_progress.suspend(callback)
    }

    fn message(repo: &str, pr_number: u64, status: &str) -> String {
        format!("{repo}#{pr_number}: {status}")
    }
}

impl Drop for PrStatusRows {
    fn drop(&mut self) {
        for bar in self.bars.values() {
            bar.disable_steady_tick();
            if !bar.is_finished() {
                bar.abandon();
            }
        }
    }
}

#[derive(Debug)]
enum EnqueuePullRequestOutcome {
    Queued,
    Merged,
    AwaitingRequiredChecks,
}

#[derive(Debug, Display)]
enum MergeSkipped {
    #[display("Skipped because an earlier merge result is not confirmed. Check the pull request before trying again.")]
    Unconfirmed,
    #[display("Skipped because waiting for merge results was cancelled")]
    Cancelled,
}

#[derive(Clone, Copy)]
enum PromptChoice {
    Refresh,
    PrintFailingCiPrompt,
    ApproveMergeIncludingNonPassingCi,
    Action(Action),
}

fn should_render_status_rows(dry_run: bool, verbose: bool) -> bool {
    !dry_run && !verbose
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
    pub(crate) async fn process_repositories(
        &self,
        repos: &[String],
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
                    match action {
                        Action::OpenUnreviewedInBrowser => {
                            if previously_reviewed {
                                continue;
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
                                continue;
                            }

                            let (merge_method, allow_auto_merge) = approve_merge_context
                                .as_ref()
                                .and_then(|contexts| contexts.get(&item.repo))
                                .copied()
                                .expect("approve merge context");
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
                                    self.debug(&format!(
                                        "PR #{} merge queue: not used",
                                        item.pr.number
                                    ));
                                    if allow_non_passing_ci
                                        && item.pr.ci_status != CiStatus::Passing
                                    {
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
                                    if allow_non_passing_ci
                                        && item.pr.ci_status == CiStatus::Failing
                                    {
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
                                    self.debug(&format!(
                                        "PR #{} merge queue: not used",
                                        item.pr.number
                                    ));
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
                            let mut watch_for_merge = false;
                            if let Some(statuses) = &pr_statuses {
                                statuses.update(&info.repo, info.pr_number, "Inspecting merge strategy");
                            }
                            let (merge_method, allow_auto_merge) = approve_merge_context
                                .as_ref()
                                .and_then(|contexts| contexts.get(&info.repo))
                                .copied()
                                .expect("approve merge context");
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
                                if let Some(statuses) = &pr_statuses {
                                    statuses.update(&info.repo, info.pr_number, "Approving pull request");
                                }
                                self.approve_pull_request(&info.owner, &info.repo_name, info.pr_number)
                                    .await?;
                            }

                            match merge_mode {
                                ApprovalMode::Direct => {
                                    self.debug(&format!("PR #{} merge queue: not used", info.pr_number));
                                    if let Some(statuses) = &pr_statuses {
                                        statuses.update(&info.repo, info.pr_number, "Merging");
                                    }
                                    self.direct_merge_pull_request(
                                        &info.owner,
                                        &info.repo_name,
                                        info.pr_number,
                                        merge_method,
                                        pr_statuses.as_ref(),
                                    )
                                    .await?;
                                    if let Some(statuses) = &pr_statuses {
                                        statuses.finish_success(
                                            &info.repo,
                                            info.pr_number,
                                            "Approved and merged",
                                        );
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
                                    if let Some(statuses) = &pr_statuses {
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
                                            pr_statuses.as_ref(),
                                        )
                                        .await?
                                    {
                                        EnqueuePullRequestOutcome::Queued => {
                                            watch_for_merge = true;
                                            if let Some(statuses) = &pr_statuses {
                                                statuses.update(
                                                    &info.repo,
                                                    info.pr_number,
                                                    "In merge queue",
                                                );
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
                                            if let Some(statuses) = &pr_statuses {
                                                statuses.finish_success(
                                                    &info.repo,
                                                    info.pr_number,
                                                    "Already merged",
                                                );
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
                                            if let Some(statuses) = &pr_statuses {
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
                                    if let Some(statuses) = &pr_statuses {
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
                                    if let Some(statuses) = &pr_statuses {
                                        statuses.update(
                                            &info.repo,
                                            info.pr_number,
                                            "In merge queue",
                                        );
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
                                        if let Some(statuses) = &pr_statuses {
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
                                        if let Some(statuses) = &pr_statuses {
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
                                    if let Some(statuses) = &pr_statuses {
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
                                    return Ok(());
                                }
                            }

                            if watch_for_merge {
                                pending_tx.send(info)
                                    .map_err(|_closed| Report::new(AppError::ApproveMerge).attach(MergeWaitCancelled))?;
                            }

                            if let Some(dep_update) = &info.dep_update {
                                review_state.record_approved(dep_update);
                                state_changed = true;
                            }

                            performed_action = Some(action);

                            Ok(())
                        },
                        async |info, error| {
                            self.handle_merge_failure(info, error, pr_statuses.as_ref()).await
                        },
                        tokio::signal::ctrl_c(),
                    ).await;

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

    async fn fetch_pending_review_statuses(
        &self,
        review_items: &[ReviewItem],
    ) -> Result<HashMap<String, HashMap<u64, MergeQueueStatus>>, Report<AppError>> {
        let status_tasks = review_items
            .iter()
            .filter(|item| item.pr.ci_status == CiStatus::Pending)
            .map(|item| {
                let repo = item.repo.clone();
                let owner = item.owner.clone();
                let repo_name = item.repo_name.clone();
                let pr_number = item.pr.number;
                let base_ref_name = item.pr.base_ref_name.clone();

                async move {
                    let status = self
                        .fetch_merge_queue_status(&owner, &repo_name, pr_number, &base_ref_name)
                        .await?;

                    Ok::<_, Report<_>>((repo, pr_number, status))
                }
            });

        let mut statuses = HashMap::<String, HashMap<u64, MergeQueueStatus>>::new();
        let mut stream = futures_util::stream::iter(status_tasks).buffered_unordered(5);
        while let Some(result) = stream.next().await {
            let (repo, pr_number, status) = result?;
            statuses.entry(repo).or_default().insert(pr_number, status);
        }

        Ok(statuses)
    }

    async fn fetch_merge_queue_status(
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

    async fn handle_merge_failure(
        &self,
        info: &MergeInfo,
        error: Report<AppError>,
        statuses: Option<&PrStatusRows>,
    ) -> Report<AppError> {
        if error.downcast_ref::<MergeWaitCancelled>().is_some() {
            finish_cancelled_merge(info, statuses);
            return error;
        }

        if let Some(skipped) = error.downcast_ref::<MergeSkipped>() {
            let message = skipped.to_string();
            if let Some(statuses) = statuses {
                statuses.finish_skipped(&info.repo, info.pr_number, &message);
            } else {
                println!("  {}#{}: {message}", info.repo, info.pr_number);
            }

            return error;
        }

        let message = if matches!(
            error.downcast_ref::<AsyncMergeError>(),
            Some(AsyncMergeError::Unconfirmed)
        ) {
            "Merge result unconfirmed; check the pull request".to_owned()
        } else if let Some(AsyncMergeError::Failed { message }) =
            error.downcast_ref::<AsyncMergeError>()
        {
            format!("Merge failed: {message}")
        } else {
            "Approval or merge failed; check the pull request".to_owned()
        };

        if let Some(statuses) = statuses {
            statuses.complete(&info.repo, info.pr_number, &format!("✗ {message}"));
        } else {
            println!("  {}#{}: {message}", info.repo, info.pr_number);
        }

        let rebase_result = offer_conflict_rebase(&self.octocrab, info, &error, || {
            let prompt = || {
                if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                    println!("  {}#{} has merge conflicts. Comment `@dependabot rebase` on {} to request a rebase.", info.repo, info.pr_number, info.url);
                    return Ok(false);
                }

                Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(format!("{}#{} has merge conflicts. Post `@dependabot rebase`?", info.repo, info.pr_number))
                    .default(false)
                    .interact()
                    .change_context(AppError::Interactive)
                    .attach("Rebase confirmation failed")
            };

            match statuses {
                Some(statuses) => statuses.suspend(prompt),
                None => prompt(),
            }
        }).await;

        match rebase_result {
            Ok(true) => {
                if let Some(statuses) = statuses {
                    statuses.complete(
                        &info.repo,
                        info.pr_number,
                        "Rebase requested; run again after CI completes",
                    );
                } else {
                    println!("  Rebase requested for {}#{}. Run the tool again after Dependabot updates the PR and CI completes.", info.repo, info.pr_number);
                }

                error.attach("Dependabot rebase requested; PR is not merged")
            }
            Ok(false) => error,
            Err(rebase_error) => error.attach(format!(
                "Could not request a Dependabot rebase: {rebase_error:?}"
            )),
        }
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

async fn has_actions_lock(
    octocrab: &octocrab::Octocrab,
    item: &ReviewItem,
    cache: &mut HashMap<(String, String), bool>,
) -> Result<bool, Report<AppError>> {
    // Grouped update titles do not always parse as a single dependency update.
    if !item
        .pr
        .head_ref_name
        .starts_with("dependabot/github_actions/")
    {
        return Ok(false);
    }

    let key = (item.repo.clone(), item.pr.base_ref_name.clone());
    if let Some(&exists) = cache.get(&key) {
        return Ok(exists);
    }

    let query = serde_urlencoded::to_string([("ref", &item.pr.base_ref_name)])
        .change_context(AppError::GitHubApi)?;
    let route = format!(
        "/repos/{}/contents/.github/workflows/actions.lock?{query}",
        item.repo,
    );
    let response = octocrab
        ._get(route)
        .await
        .change_context(AppError::GitHubApi)?;

    let exists = if response.status().as_u16() == 404 {
        false
    } else {
        octocrab::map_github_error(response)
            .await
            .change_context(AppError::GitHubApi)
            .attach_with(|| {
                format!(
                    "Cannot check actions.lock for {} on {}; approval and merge stopped",
                    item.repo, item.pr.base_ref_name,
                )
            })?;
        true
    };

    cache.insert(key, exists);
    Ok(exists)
}

async fn offer_conflict_rebase(
    octocrab: &octocrab::Octocrab,
    info: &MergeInfo,
    error: &Report<AppError>,
    confirm: impl FnOnce() -> Result<bool, Report<AppError>>,
) -> Result<bool, Report<AppError>> {
    let async_merge_conflicts = matches!(
        error.downcast_ref::<AsyncMergeError>(),
        Some(AsyncMergeError::Failed { message })
            if message.to_ascii_lowercase().contains("merge conflict")
                || message.to_ascii_lowercase().contains("not mergeable")
    );
    let may_have_conflicts = async_merge_conflicts
        || match error.downcast_ref::<octocrab::Error>() {
            Some(octocrab::Error::GitHub { source, .. }) => source.status_code.as_u16() == 405,
            Some(octocrab::Error::Graphql { source, .. }) => source.0.iter().any(|error| {
                error
                    .message
                    .to_ascii_lowercase()
                    .contains("merge conflict")
            }),
            _ => false,
        };

    if !may_have_conflicts {
        return Ok(false);
    }

    // A rejected merge can also mean branch protection or an outdated head SHA.
    // Confirm that the PR still has conflicts before offering a rebase.
    let pr = octocrab
        .pulls(&info.owner, &info.repo_name)
        .get(info.pr_number)
        .await
        .change_context(AppError::GitHubApi)
        .attach("Failed to check current merge conflicts")?;

    if pr.state != Some(octocrab::models::IssueState::Open)
        || pr.merged == Some(true)
        || pr.mergeable != Some(false)
        || !confirm()?
    {
        return Ok(false);
    }

    octocrab
        .issues(&info.owner, &info.repo_name)
        .create_comment(info.pr_number, "@dependabot rebase")
        .await
        .change_context(AppError::Comment)
        .attach("Failed to post the Dependabot rebase request")?;

    Ok(true)
}

async fn process_merge_batch<'a, T>(
    items: &'a [T],
    mut process: impl AsyncFnMut(&'a T) -> Result<(), Report<AppError>>,
    mut on_failure: impl AsyncFnMut(&'a T, Report<AppError>) -> Report<AppError>,
    cancel: impl Future<Output = io::Result<()>>,
) -> Vec<(&'a T, Report<AppError>)> {
    let mut failures = Vec::new();
    let mut stopped = None;
    tokio::pin!(cancel);

    for item in items {
        if let Some(cancelled) = stopped {
            let reason = if cancelled {
                MergeSkipped::Cancelled
            } else {
                MergeSkipped::Unconfirmed
            };
            let error = Report::new(AppError::ApproveMerge).attach(reason);
            failures.push((item, on_failure(item, error).await));

            continue;
        }

        let result = tokio::select! {
            biased;
            result = &mut cancel => Err(super::merge_results::cancelled(result)),
            result = process(item) => result,
        };

        if let Err(error) = result {
            if error.downcast_ref::<MergeWaitCancelled>().is_some() {
                stopped = Some(true);
            } else if matches!(
                error.downcast_ref::<AsyncMergeError>(),
                Some(AsyncMergeError::Unconfirmed)
            ) {
                stopped = Some(false);
            }

            failures.push((item, on_failure(item, error).await));
        }
    }

    failures
}

fn update_merge_status(
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

fn finish_cancelled_merge(info: &MergeInfo, statuses: Option<&PrStatusRows>) {
    let message = "Stopped waiting; GitHub can still process the request";
    if let Some(statuses) = statuses {
        statuses.complete(&info.repo, info.pr_number, message);
    } else {
        println!("  {}#{}: {message}", info.repo, info.pr_number);
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

fn pending_status_badge(status: &MergeQueueStatus) -> String {
    if status.already_queued {
        format!("{}", style("queued").green())
    } else if status.auto_merge_enabled {
        format!("{}", style("auto-merge enabled").green())
    } else if status.uses_merge_queue {
        format!("{}", style("not queued").yellow())
    } else {
        format!("{}", style("not auto-merge enabled").yellow())
    }
}

fn review_badges(
    previously_reviewed: bool,
    pending_status: Option<&MergeQueueStatus>,
    actions_lock_check: &Result<bool, Report<AppError>>,
) -> String {
    let mut badges = vec![if previously_reviewed {
        style("previously reviewed").dim().to_string()
    } else {
        style("unreviewed").red().to_string()
    }];

    if let Some(status) = pending_status {
        badges.push(pending_status_badge(status));
    }

    match actions_lock_check {
        Ok(true) => badges.push(style("will not merge: actions.lock").yellow().to_string()),
        Err(_) => badges.push(
            style("merge status unknown: actions.lock check failed")
                .yellow()
                .to_string(),
        ),
        Ok(false) => {}
    }

    badges.join(", ")
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

fn graphql_merge_method(merge_method: MergeMethod) -> &'static str {
    match merge_method {
        MergeMethod::Merge => "MERGE",
        MergeMethod::Squash => "SQUASH",
        MergeMethod::Rebase => "REBASE",
        _ => "MERGE",
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

fn failing_ci_agent_prompt(review_items: &[ReviewItem]) -> Option<String> {
    let failing_items = review_items
        .iter()
        .filter(|item| item.pr.ci_status == CiStatus::Failing)
        .collect::<Vec<_>>();

    if failing_items.is_empty() {
        return None;
    }

    let mut prompt = String::from("Triage the failing CI for these Dependabot pull requests:\n\n");

    for item in failing_items {
        prompt.push_str(&format!(
            "- {}#{}: {}\n  {}\n",
            item.repo, item.pr.number, item.pr.title, item.pr.url
        ));
    }

    prompt.push_str(
        "\nFor each pull request, inspect the failing checks and scan the relevant logs briefly. \
Categorize the failure as a dependency regression, repository issue, CI or infrastructure \
issue, flaky failure, unrelated existing failure, or unclear. Summarize the evidence for the \
category. Suggest a fix when one is obvious from the logs or surrounding context. Group pull \
requests that share the same failure when useful. Do not make changes. State when a failure \
needs deeper investigation.",
    );

    Some(prompt)
}

#[cfg(test)]
mod tests {
    use std::{
        assert_matches, io,
        sync::{Arc, Mutex},
    };

    use clap::Parser as _;
    use indicatif::TermLike;

    use super::{super::merge_results::monitor_merge_results, *};

    #[derive(Clone, Debug, Default)]
    struct RecordingTerm {
        clears: Arc<Mutex<usize>>,
        writes: Arc<Mutex<usize>>,
    }

    impl RecordingTerm {
        fn clear_count(&self) -> usize {
            *self.clears.lock().expect("recording term lock")
        }

        fn reset(&self) {
            *self.clears.lock().expect("recording term lock") = 0;
            *self.writes.lock().expect("recording term lock") = 0;
        }

        fn write_count(&self) -> usize {
            *self.writes.lock().expect("recording term lock")
        }
    }

    impl TermLike for RecordingTerm {
        fn width(&self) -> u16 {
            120
        }

        fn move_cursor_up(&self, _: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_down(&self, _: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_right(&self, _: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_left(&self, _: usize) -> io::Result<()> {
            Ok(())
        }

        fn write_line(&self, _: &str) -> io::Result<()> {
            let mut writes = self
                .writes
                .lock()
                .map_err(|err| io::Error::other(err.to_string()))?;
            *writes += 1;
            Ok(())
        }

        fn write_str(&self, _: &str) -> io::Result<()> {
            let mut writes = self
                .writes
                .lock()
                .map_err(|err| io::Error::other(err.to_string()))?;
            *writes += 1;
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            let mut clears = self
                .clears
                .lock()
                .map_err(|err| io::Error::other(err.to_string()))?;
            *clears += 1;
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    fn conflicted_pr(mergeable: &str, state: &str) -> String {
        format!(
            r#"{{"id":1,"number":12,"url":"https://example.com/pr/12","head":{{"ref":"dependabot/test","sha":"head"}},"base":{{"ref":"main","sha":"base"}},"mergeable":{mergeable},"state":"{state}","merged":false}}"#
        )
    }

    fn merge_info() -> MergeInfo {
        MergeInfo {
            repo: "example/repo".to_owned(),
            owner: "example".to_owned(),
            repo_name: "repo".to_owned(),
            pr_number: 12,
            url: "https://example.com/pr/12".to_owned(),
            base_ref_name: "main".to_owned(),
            ci_status: CiStatus::Passing,
            dep_update: None,
            previously_reviewed: false,
        }
    }

    async fn rebase_test_client(
        responses: Vec<(u16, String)>,
    ) -> (octocrab::Octocrab, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::{
            io::{AsyncReadExt as _, AsyncWriteExt as _},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test address");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();

            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut request = Vec::new();
                let mut buffer = [0; 1024];

                loop {
                    let count = socket.read(&mut buffer).await.expect("read request");
                    assert_ne!(count, 0, "request ended early");
                    request.extend_from_slice(buffer.get(..count).expect("received bytes"));

                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers =
                            String::from_utf8_lossy(request.get(..end).expect("request headers"));
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().expect("body length"))
                            })
                            .unwrap_or(0);

                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }

                requests.push(String::from_utf8(request).expect("UTF-8 request"));
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }

            requests
        });
        let octocrab = octocrab::Octocrab::builder()
            .base_uri(format!("http://{address}"))
            .expect("test URI")
            .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
            .build()
            .expect("test client");

        (octocrab, server)
    }

    async fn merge_test_error(octocrab: &octocrab::Octocrab) -> Report<AppError> {
        let response = octocrab._get("/merge-error").await.expect("error response");
        let error = octocrab::map_github_error(response)
            .await
            .expect_err("merge failure");

        Report::new(error).change_context(AppError::ApproveMerge)
    }

    fn merge_test_app(octocrab: octocrab::Octocrab) -> App {
        App {
            cli: crate::cli::Cli::try_parse_from(["dependabot-reviewer", "--repo", "example/repo"])
                .expect("test CLI"),
            octocrab,
        }
    }

    async fn test_merge_request(
        octocrab: &octocrab::Octocrab,
        owner: &str,
        repo: &str,
        number: u64,
        sha: &str,
        operation: MergeOperation,
    ) -> Result<MergeOutcome, Report<AppError>> {
        let status = AsyncMerge::new(octocrab)
            .start(owner, repo, number, sha, operation)
            .await?;

        wait_for_async_merge(octocrab, status, |_| {}, std::future::pending()).await
    }

    fn pending_merge(method: &str, action: &str, sha: &str, bypass_rules: bool) -> String {
        format!(
            r#"{{"status":"pending","details":{{"uuid":"request-id","merge_method":"{method}","merge_action":"{action}","expected_head_sha":"{sha}","bypass_rules":{bypass_rules}}}}}"#
        )
    }

    #[tokio::test]
    async fn direct_merge_uses_async_api_and_waits_for_completion() {
        let pending = r#"{"status":"pending","details":{"uuid":"request-id","merge_method":"squash","merge_action":"direct_merge","expected_head_sha":"head","bypass_rules":false}}"#;
        let (octocrab, server) = rebase_test_client(vec![
            (200, conflicted_pr("true", "open")),
            (202, pending.to_owned()),
            (200, pending.to_owned()),
            (
                200,
                r#"{"status":"merged","details":{"sha":"merged-head"}}"#.to_owned(),
            ),
        ])
        .await;
        let app = merge_test_app(octocrab);

        tokio::time::timeout(
            Duration::from_secs(15),
            app.direct_merge_pull_request("example", "repo", 12, MergeMethod::Squash, None),
        )
        .await
        .expect("merge completed within the test deadline")
        .expect("async merge completed");

        let requests = server.await.expect("test server");
        let submit = requests.get(1).expect("merge request");

        assert!(submit.starts_with("PUT /repos/example/repo/pulls/12/merge-async "));
        assert!(requests.iter().skip(1).all(|request| {
            let versions = request
                .lines()
                .filter(|line| {
                    line.to_ascii_lowercase()
                        .starts_with("x-github-api-version:")
                })
                .collect::<Vec<_>>();

            versions == ["x-github-api-version: 2026-03-10"]
        }));
        assert!(submit.contains(r#""sha":"head""#));
        assert!(submit.contains(r#""merge_method":"squash""#));
        assert!(submit.contains(r#""merge_action":"direct_merge""#));
        assert!(submit.contains(r#""bypass_rules":false"#));
        assert!(requests.iter().skip(2).all(|request| {
            request.starts_with("GET /repos/example/repo/pulls/12/merge-async/request-id ")
        }));
    }

    #[tokio::test]
    async fn queue_insertion_uses_async_api_without_a_merge_method() {
        let (octocrab, server) = rebase_test_client(vec![
            (202, pending_merge("merge", "merge_queue", "head", false)),
            (
                200,
                r#"{"status":"enqueued","details":{"message":"Queued"}}"#.to_owned(),
            ),
        ])
        .await;
        let app = merge_test_app(octocrab);

        let outcome = app
            .enqueue_pull_request("example", "repo", 12, "head", None)
            .await
            .expect("queue insertion");

        assert_matches!(outcome, EnqueuePullRequestOutcome::Queued);

        let requests = server.await.expect("test server");
        let submit = requests.first().expect("merge request");

        assert!(submit.starts_with("PUT /repos/example/repo/pulls/12/merge-async "));
        assert!(submit.contains(r#""merge_action":"merge_queue""#));
        assert!(submit.contains(r#""sha":"head""#));
        assert!(submit.contains(r#""bypass_rules":false"#));
        assert!(!submit.contains("merge_method"));
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn queue_insertion_preserves_auto_merge_fallback_for_expected_checks() {
        let (octocrab, server) = rebase_test_client(vec![
            (202, pending_merge("merge", "merge_queue", "head", false)),
            (200, r#"{"status":"failed","details":{"message":"Pull request 4 of 4 required status checks are expected."}}"#.to_owned()),
            (200, r#"{"data":{"enablePullRequestAutoMerge":{"pullRequest":{"id":"pull-request-id"}}}}"#.to_owned()),
        ])
        .await;
        let app = merge_test_app(octocrab);

        let outcome = app
            .enqueue_pull_request("example", "repo", 12, "head", None)
            .await
            .expect("required checks result");

        assert_matches!(outcome, EnqueuePullRequestOutcome::AwaitingRequiredChecks);

        app.enable_auto_merge_for_pull_request("pull-request-id", "head", MergeMethod::Merge)
            .await
            .expect("auto-merge enabled");

        let requests = server.await.expect("test server");
        let auto_merge = requests.last().expect("auto-merge request");

        assert!(auto_merge.starts_with("POST /graphql "));
        assert!(auto_merge.contains("enablePullRequestAutoMerge"));
        assert!(auto_merge.contains(r#""expectedHeadOid":"head""#));
    }

    #[tokio::test]
    async fn queue_insertion_distinguishes_already_merged_pull_requests() {
        let (octocrab, server) = rebase_test_client(vec![(
            200,
            r#"{"status":"merged","details":{"sha":"merged-head"}}"#.to_owned(),
        )])
        .await;
        let app = merge_test_app(octocrab);

        let outcome = app
            .enqueue_pull_request("example", "repo", 12, "head", None)
            .await
            .expect("already merged");

        assert_matches!(outcome, EnqueuePullRequestOutcome::Merged);
        assert_eq!(server.await.expect("test server").len(), 1);
    }

    #[tokio::test]
    async fn direct_merge_does_not_report_queued_as_merged() {
        let (octocrab, server) = rebase_test_client(vec![
            (200, conflicted_pr("true", "open")),
            (
                200,
                r#"{"status":"enqueued","details":{"message":"Already queued"}}"#.to_owned(),
            ),
        ])
        .await;
        let app = merge_test_app(octocrab);

        let error = app
            .direct_merge_pull_request("example", "repo", 12, MergeMethod::Merge, None)
            .await
            .expect_err("queue insertion is not a completed direct merge");

        assert!(format!("{error:?}").contains("direct merge is not complete"));
        assert_eq!(server.await.expect("test server").len(), 2);
    }

    #[tokio::test]
    async fn async_merge_follows_a_matching_existing_request() {
        let (octocrab, server) = rebase_test_client(vec![
            (409, pending_merge("squash", "direct_merge", "head", false)),
            (
                200,
                r#"{"status":"merged","details":{"sha":"merged-head"}}"#.to_owned(),
            ),
        ])
        .await;

        let outcome = test_merge_request(
            &octocrab,
            "example",
            "repo",
            12,
            "head",
            MergeOperation::Direct(MergeMethod::Squash),
        )
        .await
        .expect("existing merge completed");

        assert_eq!(outcome, MergeOutcome::Merged);

        let requests = server.await.expect("test server");

        assert_eq!(requests.len(), 2);
        assert!(requests
            .last()
            .expect("poll request")
            .starts_with("GET /repos/example/repo/pulls/12/merge-async/request-id "));
    }

    #[tokio::test]
    async fn submitting_an_async_merge_returns_before_polling() {
        let (octocrab, server) = rebase_test_client(vec![(
            202,
            pending_merge("merge", "direct_merge", "head", false),
        )])
        .await;

        let status = AsyncMerge::new(&octocrab)
            .start(
                "example",
                "repo",
                12,
                "head",
                MergeOperation::Direct(MergeMethod::Merge),
            )
            .await
            .expect("accepted merge request");

        assert_matches!(status, super::super::async_merge::MergeStatus::Pending(_));
        assert_eq!(server.await.expect("test server").len(), 1);
    }

    #[tokio::test]
    async fn temporary_poll_errors_retry_without_resubmitting_the_merge() {
        let (octocrab, server) = rebase_test_client(vec![
            (202, pending_merge("merge", "direct_merge", "head", false)),
            (503, r#"{"message":"Service unavailable"}"#.to_owned()),
            (200, r#"{"status":"merged","details":{}}"#.to_owned()),
        ])
        .await;
        let status = AsyncMerge::new(&octocrab)
            .start(
                "example",
                "repo",
                12,
                "head",
                MergeOperation::Direct(MergeMethod::Merge),
            )
            .await
            .expect("accepted request");
        let mut messages = Vec::new();

        let outcome = wait_for_async_merge(
            &octocrab,
            status,
            |message| messages.push(message.to_owned()),
            std::future::pending(),
        )
        .await
        .expect("confirmed merge");

        assert_eq!(outcome, MergeOutcome::Merged);
        assert_eq!(
            messages,
            [
                "Merging",
                "Status unavailable; retrying (Ctrl+C to stop waiting)"
            ]
        );

        let requests = server.await.expect("test server");
        assert_eq!(requests.len(), 3);
        assert!(requests.iter().skip(1).all(|request| request
            .starts_with("GET /repos/example/repo/pulls/12/merge-async/request-id ")));
    }

    #[tokio::test]
    async fn cancelling_an_async_merge_only_stops_the_local_wait() {
        let (octocrab, server) = rebase_test_client(vec![(
            202,
            pending_merge("merge", "merge_queue", "head", false),
        )])
        .await;
        let status = AsyncMerge::new(&octocrab)
            .start("example", "repo", 12, "head", MergeOperation::Queue)
            .await
            .expect("accepted request");

        let error = wait_for_async_merge(&octocrab, status, |_| {}, async { Ok(()) })
            .await
            .expect_err("cancelled wait");

        assert!(error.downcast_ref::<MergeWaitCancelled>().is_some());
        assert_eq!(server.await.expect("test server").len(), 1);
    }

    #[tokio::test]
    async fn results_screen_polls_ci_queue_and_the_actual_merge() {
        let snapshot = |state, ci, queue| {
            format!(
                r#"{{"data":{{"repository":{{"pullRequest":{{"state":"{state}","mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED","mergeQueueEntry":{queue},"autoMergeRequest":{{"enabledAt":"2026-10-02T12:00:00Z"}},"commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{{"state":"{ci}"}}}}}}]}}}}}}}}}}"#
            )
        };
        let (octocrab, server) = rebase_test_client(vec![
            (200, snapshot("OPEN", "PENDING", "null")),
            (
                200,
                snapshot(
                    "OPEN",
                    "SUCCESS",
                    r#"{"position":2,"state":"AWAITING_CHECKS"}"#,
                ),
            ),
            (200, snapshot("MERGED", "SUCCESS", "null")),
        ])
        .await;
        let info = merge_info();
        let mut messages = Vec::new();

        let results = monitor_merge_results(
            &[&info],
            async |info| {
                fetch_merge_progress(&octocrab, &info.owner, &info.repo_name, info.pr_number).await
            },
            |_, progress| messages.push(progress.message().to_owned()),
            async |_, error| error,
            std::future::pending(),
        )
        .await;

        assert!(results.failures.is_empty());
        assert!(results.cancelled.is_empty());
        assert_eq!(
            messages,
            [
                "Waiting for CI to pass",
                "In merge queue (position 2); waiting for queue CI to pass",
                "Merged"
            ]
        );

        let requests = server.await.expect("test server");
        assert_eq!(requests.len(), 3);
        assert!(requests
            .iter()
            .all(|request| request.starts_with("POST /graphql ")
                && request.contains("statusCheckRollup")
                && request.contains("mergeQueueEntry { position state }")));
    }

    #[tokio::test]
    async fn results_screen_keeps_fetch_errors_distinct_from_merge_failures() {
        for (status, message, retryable) in [
            (403, "API rate limit exceeded", true),
            (403, "Resource not accessible", false),
            (503, "Service unavailable", true),
        ] {
            let (octocrab, server) =
                rebase_test_client(vec![(status, format!(r#"{{"message":"{message}"}}"#))]).await;

            let error = fetch_merge_progress(&octocrab, "example", "repo", 12)
                .await
                .expect_err("unavailable status");

            assert_matches!(
                error.downcast_ref::<AsyncMergeError>(),
                Some(AsyncMergeError::Unconfirmed)
            );
            assert_eq!(
                super::super::merge_results::retryable_poll_error(&error),
                retryable
            );
            assert_eq!(server.await.expect("test server").len(), 1);
        }
    }

    #[tokio::test]
    async fn async_merge_rejects_existing_requests_with_different_options() {
        for (method, action, sha, bypass) in [
            ("merge", "direct_merge", "head", false),
            ("squash", "merge_queue", "head", false),
            ("squash", "direct_merge", "other-head", false),
            ("squash", "direct_merge", "head", true),
        ] {
            let (octocrab, server) =
                rebase_test_client(vec![(409, pending_merge(method, action, sha, bypass))]).await;

            let error = test_merge_request(
                &octocrab,
                "example",
                "repo",
                12,
                "head",
                MergeOperation::Direct(MergeMethod::Squash),
            )
            .await
            .expect_err("different merge options");

            assert_matches!(
                error.downcast_ref::<AsyncMergeError>(),
                Some(AsyncMergeError::Unconfirmed)
            );
            assert_eq!(server.await.expect("test server").len(), 1);
        }
    }

    #[tokio::test]
    async fn direct_merge_does_not_resubmit_after_a_poll_failure() {
        let (octocrab, server) = rebase_test_client(vec![
            (200, conflicted_pr("true", "open")),
            (202, pending_merge("merge", "direct_merge", "head", false)),
            (403, r#"{"message":"Resource not accessible"}"#.to_owned()),
        ])
        .await;
        let app = merge_test_app(octocrab);

        let error = app
            .direct_merge_pull_request("example", "repo", 12, MergeMethod::Merge, None)
            .await
            .expect_err("unconfirmed merge result");

        assert_matches!(
            error.downcast_ref::<AsyncMergeError>(),
            Some(AsyncMergeError::Unconfirmed)
        );
        assert!(format!("{error:?}").contains("Resource not accessible"));
        assert_eq!(server.await.expect("test server").len(), 3);
    }

    #[tokio::test]
    async fn async_merge_preserves_immediate_failed_results() {
        let (octocrab, server) = rebase_test_client(vec![(
            400,
            r#"{"status":"failed","details":{"message":"Pull request is still a draft"}}"#
                .to_owned(),
        )])
        .await;

        let error = test_merge_request(
            &octocrab,
            "example",
            "repo",
            12,
            "head",
            MergeOperation::Queue,
        )
        .await
        .expect_err("draft pull request");

        assert_matches!(error.downcast_ref::<AsyncMergeError>(), Some(AsyncMergeError::Failed { message }) if message == "Pull request is still a draft");
        assert_eq!(server.await.expect("test server").len(), 1);
    }

    #[tokio::test]
    async fn queue_insertion_does_not_enable_auto_merge_for_other_failures() {
        let (octocrab, server) = rebase_test_client(vec![
            (202, pending_merge("merge", "merge_queue", "head", false)),
            (
                200,
                r#"{"status":"failed","details":{"message":"Required review is missing"}}"#
                    .to_owned(),
            ),
        ])
        .await;
        let app = merge_test_app(octocrab);

        let error = app
            .enqueue_pull_request("example", "repo", 12, "head", None)
            .await
            .expect_err("required review failure");

        assert!(format!("{error:?}").contains("Required review is missing"));
        assert_eq!(server.await.expect("test server").len(), 2);
    }

    #[tokio::test]
    async fn conflict_rebase_handles_async_merge_failures() {
        let (octocrab, server) = rebase_test_client(vec![
            (202, pending_merge("merge", "direct_merge", "head", false)),
            (
                200,
                r#"{"status":"failed","details":{"message":"Pull Request has merge conflicts"}}"#
                    .to_owned(),
            ),
            (200, conflicted_pr("false", "open")),
        ])
        .await;
        let error = test_merge_request(
            &octocrab,
            "example",
            "repo",
            12,
            "head",
            MergeOperation::Direct(MergeMethod::Merge),
        )
        .await
        .expect_err("merge conflicts");
        let mut prompted = false;

        let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
            prompted = true;
            Ok(false)
        })
        .await
        .expect("declined rebase");

        assert!(prompted);
        assert!(!requested);
        assert_eq!(server.await.expect("test server").len(), 3);
    }

    fn actions_review_item() -> ReviewItem {
        let mut item = review_item(
            12,
            "Bump the actions group with 3 updates",
            CiStatus::Passing,
        );
        item.pr.head_ref_name = "dependabot/github_actions/actions-group".to_owned();
        item.pr.base_ref_name = "release/1.x".to_owned();
        item
    }

    #[test]
    fn actions_lock_badge_marks_pr_as_unmergeable_in_list() {
        let badges = review_badges(false, None, &Ok(true));

        assert!(badges.contains("unreviewed"));
        assert!(badges.contains("will not merge: actions.lock"));
    }

    #[test]
    fn actions_lock_badge_distinguishes_missing_and_failed_checks() {
        let allowed = review_badges(false, None, &Ok(false));
        let unknown = review_badges(false, None, &Err(Report::new(AppError::GitHubApi)));

        assert_eq!(allowed, "unreviewed");
        assert!(unknown.contains("merge status unknown: actions.lock check failed"));
    }

    #[tokio::test]
    async fn actions_lock_blocks_grouped_updates_and_caches_by_repo_and_base() {
        let (octocrab, server) = rebase_test_client(vec![(200, "{}".to_owned()); 3]).await;
        let mut cache = HashMap::new();
        let mut item = actions_review_item();

        assert!(has_actions_lock(&octocrab, &item, &mut cache)
            .await
            .expect("lock check"));
        assert!(has_actions_lock(&octocrab, &item, &mut cache)
            .await
            .expect("cached check"));

        item.pr.base_ref_name = "main".to_owned();
        assert!(has_actions_lock(&octocrab, &item, &mut cache)
            .await
            .expect("other base"));

        item.repo = "example/other".to_owned();
        item.repo_name = "other".to_owned();
        assert!(has_actions_lock(&octocrab, &item, &mut cache)
            .await
            .expect("other repo"));

        let requests = server.await.expect("test server");
        assert_eq!(requests.len(), 3);
        assert!(requests.first().expect("request").starts_with(
            "GET /repos/example/repo/contents/.github/workflows/actions.lock?ref=release%2F1.x "
        ));
    }

    #[tokio::test]
    async fn actions_lock_allows_missing_lockfile() {
        let (octocrab, server) =
            rebase_test_client(vec![(404, r#"{"message":"Not Found"}"#.to_owned())]).await;

        assert!(
            !has_actions_lock(&octocrab, &actions_review_item(), &mut HashMap::new())
                .await
                .expect("missing lockfile")
        );
        server.await.expect("test server");
    }

    #[tokio::test]
    async fn actions_lock_lookup_errors_prevent_approval() {
        for status in [403, 429, 500] {
            let (octocrab, server) =
                rebase_test_client(vec![(status, r#"{"message":"lookup failed"}"#.to_owned())])
                    .await;

            let error = has_actions_lock(&octocrab, &actions_review_item(), &mut HashMap::new())
                .await
                .expect_err("failed checks must stop approval");
            assert!(format!("{error:?}").contains("approval and merge stopped"));
            server.await.expect("test server");
        }
    }

    #[tokio::test]
    async fn actions_lock_does_not_block_other_ecosystems() {
        let (octocrab, server) = rebase_test_client(vec![]).await;
        let item = review_item(12, "Bump tokio from 1 to 2", CiStatus::Passing);
        let mut cache = HashMap::from([((item.repo.clone(), item.pr.base_ref_name.clone()), true)]);

        assert!(!has_actions_lock(&octocrab, &item, &mut cache)
            .await
            .expect("cargo update"));
        assert!(server.await.expect("test server").is_empty());
    }

    #[tokio::test]
    async fn conflict_rebase_posts_only_after_confirmation() {
        let (octocrab, server) = rebase_test_client(vec![
            (405, r#"{"message":"Pull Request is not mergeable"}"#.to_owned()),
            (200, conflicted_pr("false", "open")),
            (201, r#"{"id":1,"node_id":"comment","url":"https://example.com/comment","html_url":"https://example.com/comment","user":{"login":"tester","id":1,"node_id":"user","gravatar_id":"","type":"User","site_admin":false,"avatar_url":"https://example.com","url":"https://example.com","html_url":"https://example.com","followers_url":"https://example.com","following_url":"https://example.com","gists_url":"https://example.com","starred_url":"https://example.com","subscriptions_url":"https://example.com","organizations_url":"https://example.com","repos_url":"https://example.com","events_url":"https://example.com","received_events_url":"https://example.com"},"created_at":"2026-09-17T00:00:00Z","body":"@dependabot rebase"}"#.to_owned()),
        ]).await;
        let error = merge_test_error(&octocrab).await;
        let mut prompted = false;

        let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
            prompted = true;
            Ok(true)
        })
        .await
        .expect("rebase request");

        assert!(requested, "conflicted PR should get a rebase request");
        assert!(prompted);
        let requests = server.await.expect("test server");
        assert_eq!(requests.len(), 3);
        assert!(requests
            .get(1)
            .expect("PR lookup")
            .starts_with("GET /repos/example/repo/pulls/12 "));
        let comment = requests.last().expect("comment request");
        assert!(comment.starts_with("POST /repos/example/repo/issues/12/comments "));
        assert!(comment.ends_with(r#"{"body":"@dependabot rebase"}"#));
    }

    #[tokio::test]
    async fn conflict_rebase_does_not_comment_when_declined() {
        let (octocrab, server) = rebase_test_client(vec![
            (
                405,
                r#"{"message":"Pull Request is not mergeable"}"#.to_owned(),
            ),
            (200, conflicted_pr("false", "open")),
        ])
        .await;
        let error = merge_test_error(&octocrab).await;
        let mut prompted = false;

        let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
            prompted = true;
            Ok(false)
        })
        .await
        .expect("declined rebase");

        assert!(!requested);
        assert!(prompted);
        assert_eq!(server.await.expect("test server").len(), 2);
    }

    #[tokio::test]
    async fn conflict_rebase_requires_current_open_conflicts() {
        for (mergeable, state) in [("true", "open"), ("null", "open"), ("false", "closed")] {
            let (octocrab, server) = rebase_test_client(vec![
                (
                    405,
                    r#"{"message":"Pull Request is not mergeable"}"#.to_owned(),
                ),
                (200, conflicted_pr(mergeable, state)),
            ])
            .await;
            let error = merge_test_error(&octocrab).await;
            let mut prompted = false;

            let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
                prompted = true;
                Ok(true)
            })
            .await
            .expect("ineligible PR");

            assert!(!requested);
            assert!(!prompted, "mergeable={mergeable}, state={state}");
            assert_eq!(server.await.expect("test server").len(), 2);
        }
    }

    #[tokio::test]
    async fn conflict_rebase_ignores_unrelated_merge_failures() {
        for status in [403, 409, 429, 500] {
            let (octocrab, server) = rebase_test_client(vec![(
                status,
                r#"{"message":"Unrelated merge failure"}"#.to_owned(),
            )])
            .await;
            let error = merge_test_error(&octocrab).await;
            let mut prompted = false;

            let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
                prompted = true;
                Ok(true)
            })
            .await
            .expect("unrelated failure");

            assert!(!requested);
            assert!(!prompted, "HTTP {status}");
            assert_eq!(server.await.expect("test server").len(), 1);
        }
    }

    #[tokio::test]
    async fn conflict_rebase_handles_graphql_merge_conflicts() {
        let (octocrab, server) = rebase_test_client(vec![
            (
                200,
                r#"{"errors":[{"message":"Pull Request has merge conflicts"}]}"#.to_owned(),
            ),
            (200, conflicted_pr("false", "open")),
        ])
        .await;
        let error = octocrab.graphql::<()>(&GraphqlRequest {
            query: "mutation { enqueuePullRequest(input: {pullRequestId: \"test\"}) { mergeQueueEntry { id } } }",
            variables: (),
        }).await.expect_err("GraphQL conflict");
        let error = Report::new(error).change_context(AppError::ApproveMerge);
        let mut prompted = false;

        let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
            prompted = true;
            Ok(false)
        })
        .await
        .expect("declined rebase");

        assert!(prompted);
        assert!(!requested);
        assert_eq!(server.await.expect("test server").len(), 2);
    }

    #[tokio::test]
    async fn conflict_rebase_reports_comment_failures() {
        let (octocrab, server) = rebase_test_client(vec![
            (
                405,
                r#"{"message":"Pull Request is not mergeable"}"#.to_owned(),
            ),
            (200, conflicted_pr("false", "open")),
            (403, r#"{"message":"Resource not accessible"}"#.to_owned()),
        ])
        .await;
        let error = merge_test_error(&octocrab).await;

        let error = offer_conflict_rebase(&octocrab, &merge_info(), &error, || Ok(true))
            .await
            .expect_err("failed comment");

        assert_matches!(error.current_context(), AppError::Comment);
        assert!(format!("{error:?}").contains("Resource not accessible"));
        assert_eq!(server.await.expect("test server").len(), 3);
    }

    #[tokio::test]
    async fn merge_batch_continues_after_a_conflict() {
        let mut attempted = Vec::new();
        let mut completed = Vec::new();
        let prs = [1, 2, 3, 4];

        let failures = process_merge_batch(
            &prs,
            async |pr| {
                attempted.push(*pr);

                if *pr == 2 || *pr == 4 {
                    return Err(Report::new(AppError::ApproveMerge)
                        .attach("Pull Request has merge conflicts"));
                }

                completed.push(*pr);
                Ok(())
            },
            async |_, error| error,
            std::future::pending(),
        )
        .await;

        assert_eq!(attempted, [1, 2, 3, 4]);
        assert_eq!(completed, [1, 3]);
        assert_eq!(
            failures.iter().map(|(pr, _)| **pr).collect::<Vec<_>>(),
            [2, 4]
        );
        assert!(
            format!("{:?}", failures.first().expect("first failure").1).contains("merge conflicts")
        );
    }

    #[tokio::test]
    async fn merge_batch_returns_no_failures_when_all_prs_succeed() {
        let mut completed = Vec::new();
        let prs = [1, 2];

        let failures = process_merge_batch(
            &prs,
            async |pr| {
                completed.push(*pr);
                Ok(())
            },
            async |_, error| error,
            std::future::pending(),
        )
        .await;

        assert_eq!(completed, prs);
        assert!(failures.is_empty());
    }

    #[tokio::test]
    async fn merge_batch_skips_remaining_prs_when_a_merge_result_is_unconfirmed() {
        let mut attempted = Vec::new();
        let prs = [1, 2, 3];

        let failures = process_merge_batch(
            &prs,
            async |pr| {
                attempted.push(*pr);

                Err(Report::new(AsyncMergeError::Unconfirmed)
                    .change_context(AppError::ApproveMerge))
            },
            async |_, error| error,
            std::future::pending(),
        )
        .await;

        assert_eq!(attempted, [1]);
        assert_eq!(failures.len(), 3);
        assert!(failures.iter().skip(1).all(|(_, error)| {
            format!("{error:?}")
                .contains("Skipped because an earlier merge result is not confirmed")
        }));
    }

    #[test]
    fn status_rows_reuse_their_terminal_lines_after_a_pr_finishes() {
        let term = RecordingTerm::default();
        let statuses = PrStatusRows::with_draw_target(
            [
                ("example/repo".to_string(), 1),
                ("example/repo".to_string(), 2),
            ],
            ProgressDrawTarget::term_like(Box::new(term.clone())),
        );

        term.reset();
        statuses.finish_success("example/repo", 1, "Approved and merged");
        statuses.update("example/repo", 2, "Approving pull request");

        assert!(
            !statuses
                .bars
                .get(&("example/repo".to_string(), 1))
                .expect("first status bar")
                .is_finished(),
            "completed rows should remain in the status board"
        );
        assert_eq!(term.clear_count(), 0, "status rows should not be appended");
    }

    #[tokio::test]
    async fn cancelling_the_batch_keeps_completed_merges_and_skips_new_submissions() {
        let prs = [1, 2, 3];
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let mut cancel_tx = Some(cancel_tx);
        let mut attempted = Vec::new();

        let failures = process_merge_batch(
            &prs,
            async |pr| {
                attempted.push(*pr);
                cancel_tx
                    .take()
                    .expect("one submission")
                    .send(())
                    .expect("cancel listener");
                Ok(())
            },
            async |_, error| error,
            async {
                cancel_rx.await.expect("cancel sender");
                Ok(())
            },
        )
        .await;

        assert_eq!(attempted, [1]);
        assert_eq!(
            failures.iter().map(|(pr, _)| **pr).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(failures
            .first()
            .expect("cancelled item")
            .1
            .downcast_ref::<MergeWaitCancelled>()
            .is_some());
        assert_matches!(
            failures
                .last()
                .expect("skipped item")
                .1
                .downcast_ref::<MergeSkipped>(),
            Some(MergeSkipped::Cancelled)
        );
    }

    #[tokio::test]
    async fn merge_batch_offers_recovery_before_submitting_the_next_pull_request() {
        use std::cell::RefCell;

        let prs = [1, 2];
        let events = RefCell::new(Vec::new());
        let failures = process_merge_batch(
            &prs,
            async |pr| {
                events.borrow_mut().push(format!("submit {pr}"));
                if *pr == 1 {
                    Err(super::super::merge_results::merge_failed(
                        "Pull request has merge conflicts",
                    ))
                } else {
                    Ok(())
                }
            },
            async |pr, error| {
                events.borrow_mut().push(format!("offer recovery {pr}"));
                error
            },
            std::future::pending(),
        )
        .await;

        assert_eq!(
            *events.borrow(),
            ["submit 1", "offer recovery 1", "submit 2"]
        );
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn status_rows_do_not_redraw_after_processing_finishes() {
        let term = RecordingTerm::default();
        let statuses = PrStatusRows::with_draw_target(
            [
                ("example/repo".to_string(), 1),
                ("example/repo".to_string(), 2),
            ],
            ProgressDrawTarget::term_like(Box::new(term.clone())),
        );

        statuses.finish_success("example/repo", 1, "Approved and merged");
        statuses.finish_success("example/repo", 2, "Approved and merged");
        term.reset();
        statuses.finish();
        let writes_before_drop = term.write_count();
        drop(statuses);

        assert_eq!(
            term.write_count(),
            writes_before_drop,
            "dropping status rows must not redraw over the final application output"
        );
    }

    #[test]
    fn status_rows_stop_when_processing_returns_an_error() {
        let statuses = PrStatusRows::with_draw_target(
            [("example/repo".to_string(), 1)],
            ProgressDrawTarget::hidden(),
        );
        let bar = statuses.bars.values().next().expect("status bar").clone();
        let process = || -> Result<(), &'static str> {
            let _statuses = statuses;
            Err("Pull Request has merge conflicts")?;
            Ok(())
        };

        assert!(process().is_err());
        assert!(bar.is_finished(), "error returns must stop status rows");
    }

    #[test]
    fn status_rows_are_disabled_for_verbose_runs() {
        assert!(!should_render_status_rows(false, true));
        assert!(!should_render_status_rows(true, false));
        assert!(should_render_status_rows(false, false));
    }

    #[test]
    fn classifies_required_checks_expected_errors() {
        assert!(messages_are_awaiting_required_checks([
            "Pull request 4 of 4 required status checks are expected."
        ]));
    }

    #[test]
    fn rejects_unrelated_or_empty_required_check_errors() {
        assert!(!messages_are_awaiting_required_checks([
            "Pull request is not mergeable"
        ]));
        assert!(!messages_are_awaiting_required_checks([]));
    }

    #[test]
    fn agent_prompt_lists_only_prs_with_failing_ci() {
        let review_items = [
            review_item(12, "Bump serde from 1.0.0 to 1.0.1", CiStatus::Failing),
            review_item(11, "Bump tokio from 1.0.0 to 1.1.0", CiStatus::Passing),
        ];

        let prompt = failing_ci_agent_prompt(&review_items).expect("expected agent prompt");

        assert!(prompt.contains("example/repo#12"));
        assert!(prompt.contains("Bump serde from 1.0.0 to 1.0.1"));
        assert!(prompt.contains("https://github.com/example/repo/pull/12"));
        assert!(!prompt.contains("example/repo#11"));
        assert!(prompt.contains("Categorize the failure"));
        assert!(prompt.contains("Suggest a fix when one is obvious"));
    }

    #[test]
    fn agent_prompt_is_absent_without_failing_ci() {
        let review_items = [review_item(
            11,
            "Bump tokio from 1.0.0 to 1.1.0",
            CiStatus::Passing,
        )];

        assert!(failing_ci_agent_prompt(&review_items).is_none());
    }

    fn review_item(number: u64, title: &str, ci_status: CiStatus) -> ReviewItem {
        ReviewItem {
            repo: "example/repo".to_string(),
            owner: "example".to_string(),
            repo_name: "repo".to_string(),
            pr: PrInfo {
                number,
                title: title.to_string(),
                url: format!("https://github.com/example/repo/pull/{}", number),
                base_ref_name: "main".to_string(),
                head_ref_name: "dependabot/cargo/tokio-1.1.0".to_string(),
                ci_status,
                dep_update: None,
            },
            actions_lock_check: None,
        }
    }
}
