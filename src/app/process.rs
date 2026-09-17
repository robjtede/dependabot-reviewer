use std::{collections::HashMap, io::IsTerminal as _, process::Command, time::Duration};

use console::style;
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
    AwaitingRequiredChecks,
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

#[derive(Serialize)]
struct EnqueuePullRequestVariables<'a> {
    #[serde(rename = "pullRequestId")]
    pull_request_id: &'a str,
    #[serde(rename = "expectedHeadOid")]
    expected_head_oid: &'a str,
}

#[derive(Deserialize)]
struct MutationOnlyResponse {
    #[serde(rename = "enqueuePullRequest")]
    enqueue_pull_request: Option<EnqueuePullRequestPayload>,
    #[serde(rename = "enablePullRequestAutoMerge")]
    enable_pull_request_auto_merge: Option<EnablePullRequestAutoMergePayload>,
}

#[derive(Deserialize)]
struct EnqueuePullRequestPayload {
    #[serde(rename = "mergeQueueEntry")]
    merge_queue_entry: Option<GraphqlNode>,
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
                let review_badge = if previously_reviewed {
                    style("previously reviewed").dim()
                } else {
                    style("unreviewed").red()
                };
                let pending_badge = pending_statuses
                    .get(item.repo.as_str())
                    .and_then(|statuses| statuses.get(&item.pr.number))
                    .map(pending_status_badge);

                println!(
                    "    {} #{}: {} [{}{}{}]\n        {}",
                    item.pr.ci_status.icon(),
                    item.pr.number,
                    item.pr.title,
                    review_badge,
                    if pending_badge.is_some() { ", " } else { "" },
                    pending_badge.unwrap_or_default(),
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

                if !self.cli.verbose && pr_statuses.is_none() {
                    pr_statuses = PrStatusRows::new(
                        merge_infos
                            .iter()
                            .map(|info| (info.repo.clone(), info.pr_number)),
                    );
                }

                // Merges must run sequentially: each merge modifies the base branch,
                // which invalidates the head SHA of subsequent PRs. Running them in
                // parallel causes "Base branch was modified" errors.
                merge_failures = process_merge_batch(&merge_infos, async |info| {
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
                                statuses.finish_success(
                                    &info.repo,
                                    info.pr_number,
                                    "Approved; auto-merge enabled",
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
                                    &queue_status.pull_request_id,
                                    &queue_status.head_oid,
                                )
                                .await?
                            {
                                EnqueuePullRequestOutcome::Queued => {
                                    if let Some(statuses) = &pr_statuses {
                                        statuses.finish_success(
                                            &info.repo,
                                            info.pr_number,
                                            "Approved; added to merge queue",
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
                                EnqueuePullRequestOutcome::AwaitingRequiredChecks => {
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
                                        statuses.finish_success(
                                            &info.repo,
                                            info.pr_number,
                                            "Approved; auto-merge enabled while checks complete",
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
                                statuses.finish_success(
                                    &info.repo,
                                    info.pr_number,
                                    "Approved; merge-queue auto-merge enabled",
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
                            self.debug(&format!(
                                "PR #{} merge queue: already queued",
                                info.pr_number
                            ));
                            if let Some(statuses) = &pr_statuses {
                                statuses.finish_success(
                                    &info.repo,
                                    info.pr_number,
                                    "Already in merge queue",
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
                            if queue_status.uses_merge_queue {
                                self.debug(&format!(
                                    "PR #{} auto-merge: already enabled for merge queue",
                                    info.pr_number
                                ));
                                if let Some(statuses) = &pr_statuses {
                                    statuses.finish_success(
                                        &info.repo,
                                        info.pr_number,
                                        "Auto-merge already enabled",
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
                                    statuses.finish_success(
                                        &info.repo,
                                        info.pr_number,
                                        "Auto-merge already enabled; approval refreshed",
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

                    if let Some(dep_update) = &info.dep_update {
                        review_state.record_approved(dep_update);
                        state_changed = true;
                    }

                    performed_action = Some(action);

                    Ok(())
                })
                .await;

                for (info, _) in &merge_failures {
                    if let Some(statuses) = &pr_statuses {
                        statuses.complete(&info.repo, info.pr_number, "✗ Approval or merge failed");
                    }
                }
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

            if !merge_failures.is_empty() {
                let mut report = Report::new(AppError::ApproveMerge).attach(format!(
                    "{} of {} PR(s) failed; all remaining PRs were processed",
                    merge_failures.len(),
                    merge_infos.len(),
                ));

                for (info, error) in merge_failures {
                    let rebase_result = offer_conflict_rebase(&self.octocrab, info, &error, || {
                        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                            println!(
                                "  {}#{} has merge conflicts. Comment `@dependabot rebase` on {} to request a rebase.",
                                info.repo, info.pr_number, info.url
                            );
                            return Ok(false);
                        }

                        Confirm::with_theme(&ColorfulTheme::default())
                            .with_prompt(format!(
                                "{}#{} has merge conflicts. Post `@dependabot rebase`?",
                                info.repo, info.pr_number
                            ))
                            .default(false)
                            .interact()
                            .change_context(AppError::Interactive)
                            .attach("Rebase confirmation failed")
                    })
                    .await;

                    let error = match rebase_result {
                        Ok(true) => {
                            println!(
                                "  {} Rebase requested for {}#{}. Run the tool again after Dependabot updates the PR and CI completes.",
                                style("✓").green(), info.repo, info.pr_number
                            );
                            error.attach("Dependabot rebase requested; PR is not merged")
                        }
                        Ok(false) => error,
                        Err(rebase_error) => error.attach(format!(
                            "Could not request a Dependabot rebase: {rebase_error:?}"
                        )),
                    };

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

    async fn direct_merge_pull_request(
        &self,
        owner: &str,
        repo_name: &str,
        pr_number: u64,
        merge_method: MergeMethod,
    ) -> Result<(), Report<AppError>> {
        const MAX_ATTEMPTS: u32 = 4;

        for attempt in 1..=MAX_ATTEMPTS {
            let pulls = self.octocrab.pulls(owner, repo_name);

            let pr_data = pulls
                .get(pr_number)
                .await
                .change_context(AppError::ApproveMerge)
                .attach(format!("Failed to get PR #{}", pr_number))?;

            let head_sha = pr_data.head.sha;

            self.debug(&format!(
                "Merging PR #{} using {:?} (head: {}, attempt {}/{})",
                pr_number,
                merge_method,
                head_sha.get(..8).unwrap_or(&head_sha),
                attempt,
                MAX_ATTEMPTS
            ));

            match pulls
                .merge(pr_number)
                .sha(head_sha)
                .method(merge_method)
                .send()
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) if attempt < MAX_ATTEMPTS => {
                    let delay = Duration::from_secs(2u64.pow(attempt));
                    self.debug(&format!(
                        "Merge failed for PR #{}, retrying in {}s: {}",
                        pr_number,
                        delay.as_secs(),
                        e
                    ));
                    tokio::time::sleep(delay).await;
                }
                Err(e) => {
                    return Err(e)
                        .change_context(AppError::ApproveMerge)
                        .attach(format!(
                            "Failed to merge PR #{} after {} attempts",
                            pr_number, MAX_ATTEMPTS
                        ));
                }
            }
        }

        Err(Report::new(AppError::ApproveMerge).attach(format!(
            "Merge retry loop ended without merging PR #{} after {} attempts",
            pr_number, MAX_ATTEMPTS
        )))
    }

    async fn enqueue_pull_request(
        &self,
        pull_request_id: &str,
        expected_head_oid: &str,
    ) -> Result<EnqueuePullRequestOutcome, Report<AppError>> {
        const MUTATION: &str = r#"
            mutation EnqueuePullRequest($pullRequestId: ID!, $expectedHeadOid: GitObjectID!) {
              enqueuePullRequest(
                input: {
                  pullRequestId: $pullRequestId
                  expectedHeadOid: $expectedHeadOid
                }
              ) {
                mergeQueueEntry { id }
              }
            }
        "#;

        let payload = GraphqlRequest {
            query: MUTATION,
            variables: EnqueuePullRequestVariables {
                pull_request_id,
                expected_head_oid,
            },
        };
        let data: MutationOnlyResponse = match self.octocrab.graphql(&payload).await {
            Ok(data) => data,
            Err(error) if graphql_error_is_awaiting_required_checks(&error) => {
                return Ok(EnqueuePullRequestOutcome::AwaitingRequiredChecks);
            }
            Err(error) => {
                return Err(error)
                    .change_context(AppError::ApproveMerge)
                    .attach("Failed to enqueue pull request");
            }
        };

        enqueue_pull_request_outcome(data)
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

async fn offer_conflict_rebase(
    octocrab: &octocrab::Octocrab,
    info: &MergeInfo,
    error: &Report<AppError>,
    confirm: impl FnOnce() -> Result<bool, Report<AppError>>,
) -> Result<bool, Report<AppError>> {
    let may_have_conflicts = match error.downcast_ref::<octocrab::Error>() {
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

async fn process_merge_batch<T>(
    items: &[T],
    mut process: impl AsyncFnMut(&T) -> Result<(), Report<AppError>>,
) -> Vec<(&T, Report<AppError>)> {
    let mut failures = Vec::new();

    for item in items {
        if let Err(error) = process(item).await {
            failures.push((item, error));
        }
    }

    failures
}

fn graphql_error_is_awaiting_required_checks(error: &octocrab::Error) -> bool {
    let octocrab::Error::Graphql { source, .. } = error else {
        return false;
    };

    messages_are_awaiting_required_checks(source.0.iter().map(|error| error.message.as_str()))
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

fn enqueue_pull_request_outcome(
    data: MutationOnlyResponse,
) -> Result<EnqueuePullRequestOutcome, Report<AppError>> {
    let _merge_queue_entry_id = data
        .enqueue_pull_request
        .and_then(|payload| payload.merge_queue_entry)
        .map(|entry| entry.id)
        .ok_or_else(|| Report::new(AppError::ApproveMerge))
        .attach("enqueuePullRequest did not return a merge queue entry")?;

    Ok(EnqueuePullRequestOutcome::Queued)
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

    use indicatif::TermLike;

    use super::*;

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
                    request.extend_from_slice(&buffer[..count]);

                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
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

        let failures = process_merge_batch(&prs, async |pr| {
            attempted.push(*pr);

            if *pr == 2 || *pr == 4 {
                return Err(
                    Report::new(AppError::ApproveMerge).attach("Pull Request has merge conflicts")
                );
            }

            completed.push(*pr);
            Ok(())
        })
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

        let failures = process_merge_batch(&prs, async |pr| {
            completed.push(*pr);
            Ok(())
        })
        .await;

        assert_eq!(completed, prs);
        assert!(failures.is_empty());
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
    fn returns_queued_when_enqueue_mutation_succeeds() {
        let data = MutationOnlyResponse {
            enqueue_pull_request: Some(EnqueuePullRequestPayload {
                merge_queue_entry: Some(GraphqlNode {
                    id: "queue-entry-id".to_string(),
                }),
            }),
            enable_pull_request_auto_merge: None,
        };

        let outcome = enqueue_pull_request_outcome(data).expect("expected queued outcome");

        assert_matches!(outcome, EnqueuePullRequestOutcome::Queued);
    }

    #[test]
    fn rejects_unrelated_or_empty_graphql_errors() {
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
                ci_status,
                dep_update: None,
            },
        }
    }
}
