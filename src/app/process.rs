use std::{io::IsTerminal as _, process::Command, time::Duration};

use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, Select};
use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt;
use futures_util::{FutureExt as _, StreamExt as _};
use octocrab::{models::pulls::ReviewAction, params::pulls::MergeMethod};
use serde::{Deserialize, Serialize};

use super::{state::ReviewState, App};
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

struct MergeQueueStatus {
    pull_request_id: String,
    head_oid: String,
    uses_merge_queue: bool,
    already_queued: bool,
    auto_merge_enabled: bool,
}

#[derive(Debug)]
enum ApproveMergeMode {
    Direct,
    MergeQueueEnqueue,
    MergeQueueAutoMerge,
    AlreadyQueued,
    AlreadyAutoMergeEnabled,
    SkipPendingWithoutQueue,
}

enum PromptChoice {
    Refresh,
    Action(Action),
}

#[derive(Serialize)]
struct GraphqlRequest<'a, T> {
    query: &'a str,
    variables: T,
}

#[derive(Deserialize)]
struct GraphqlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Deserialize)]
struct GraphqlError {
    message: String,
}

#[derive(Serialize)]
struct MergeQueueStatusVariables<'a> {
    owner: &'a str,
    repo: &'a str,
    number: i64,
    #[serde(rename = "baseBranch")]
    base_branch: &'a str,
}

#[derive(Deserialize)]
struct MergeQueueStatusData {
    repository: Option<MergeQueueStatusRepository>,
}

#[derive(Deserialize)]
struct MergeQueueStatusRepository {
    #[serde(rename = "mergeQueue")]
    merge_queue: Option<GraphqlNode>,
    #[serde(rename = "pullRequest")]
    pull_request: Option<MergeQueueStatusPullRequest>,
}

#[derive(Deserialize)]
struct MergeQueueStatusPullRequest {
    id: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "mergeQueueEntry")]
    merge_queue_entry: Option<GraphqlNode>,
    #[serde(rename = "autoMergeRequest")]
    auto_merge_request: Option<AutoMergeRequest>,
}

#[derive(Deserialize)]
struct GraphqlNode {
    id: String,
}

#[derive(Deserialize)]
struct AutoMergeRequest {
    #[serde(rename = "enabledAt")]
    enabled_at: String,
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

                println!(
                    "    {} #{}: {} [{}]\n        {}",
                    item.pr.ci_status.icon(),
                    item.pr.number,
                    item.pr.title,
                    review_badge,
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
                let items = vec![
                    "Approve + Merge",
                    "Open Unreviewed In Browser",
                    "Rebase",
                    "Recreate",
                    "Refresh PR State",
                ];
                let selection = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Choose action to apply to these PRs")
                    .items(&items)
                    .default(0)
                    .interact()
                    .change_context(AppError::ActionSelection)
                    .attach("Action selection failed")?;
                match selection {
                    0 => PromptChoice::Action(Action::ApproveMerge),
                    1 => PromptChoice::Action(Action::OpenUnreviewedInBrowser),
                    2 => PromptChoice::Action(Action::Rebase),
                    3 => PromptChoice::Action(Action::Recreate),
                    4 => PromptChoice::Refresh,
                    _ => unreachable!(),
                }
            };

            let action = match prompt_choice {
                PromptChoice::Refresh => {
                    println!();
                    continue;
                }
                PromptChoice::Action(action) => action,
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
                        .change_context(AppError::Comment)
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

            let mut comment_tasks = Vec::new();
            let mut merge_infos: Vec<MergeInfo> = Vec::new();
            let mut state_changed = false;

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
                        Action::ApproveMerge => {
                            if item.pr.ci_status == CiStatus::Failing {
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
                            let merge_mode = self.choose_approve_merge_mode(
                                item.pr.number,
                                item.pr.ci_status,
                                &queue_status,
                                allow_auto_merge,
                            );

                            let marker = if previously_reviewed {
                                " [previously reviewed]"
                            } else {
                                ""
                            };
                            match merge_mode {
                                ApproveMergeMode::Direct => {
                                    self.debug(&format!(
                                        "PR #{} merge queue: not used",
                                        item.pr.number
                                    ));
                                    println!(
                                        "  [DRY RUN] Would approve and merge PR #{}{} with {:?}{}: {}",
                                        item.pr.number,
                                        marker,
                                        merge_method,
                                        style(format!(" ({})", item.repo)).dim(),
                                        item.pr.url
                                    );
                                }
                                ApproveMergeMode::MergeQueueEnqueue => {
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
                                ApproveMergeMode::MergeQueueAutoMerge => {
                                    self.debug(&format!(
                                        "PR #{} merge queue: used (auto-merge until queueable)",
                                        item.pr.number
                                    ));
                                    println!(
                                        "  [DRY RUN] Would approve PR #{}{} and enable auto-merge for the merge queue{}: {}",
                                        item.pr.number,
                                        marker,
                                        style(format!(" ({})", item.repo)).dim(),
                                        item.pr.url
                                    );
                                }
                                ApproveMergeMode::AlreadyQueued => {
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
                                ApproveMergeMode::AlreadyAutoMergeEnabled => {
                                    self.debug(&format!(
                                        "PR #{} merge queue: already using auto-merge",
                                        item.pr.number
                                    ));
                                    println!(
                                        "  [DRY RUN] Would approve PR #{}{} (auto-merge already enabled for merge queue){}: {}",
                                        item.pr.number,
                                        marker,
                                        style(format!(" ({})", item.repo)).dim(),
                                        item.pr.url
                                    );
                                }
                                ApproveMergeMode::SkipPendingWithoutQueue => {
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
                                continue;
                            }
                            println!(
                                "  {} Running `open {}`",
                                style("•").dim(),
                                style(&item.pr.url).dim()
                            );
                            open_in_browser(&item.pr.url)?;
                            println!(
                                "  {} Opened PR #{}{}",
                                style("✓").green(),
                                pr_number,
                                style(format!(" ({})", item.repo)).dim()
                            );
                            performed_action = Some(action);
                            opened_in_browser_in_session = true;
                        }
                        Action::Rebase => {
                            let owner = item.owner.clone();
                            let repo_name = item.repo_name.clone();
                            let repo = item.repo.clone();
                            comment_tasks.push(
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
                            let owner = item.owner.clone();
                            let repo_name = item.repo_name.clone();
                            let repo = item.repo.clone();
                            comment_tasks.push(
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
                            if item.pr.ci_status != CiStatus::Failing {
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

            if !comment_tasks.is_empty() {
                let mut stream = futures_util::stream::iter(comment_tasks).buffered_unordered(5);

                while let Some(result) = stream.next().await {
                    let (pr_number, repo) = result?;
                    println!(
                        "  {} Commented on PR #{}{}",
                        style("✓").green(),
                        pr_number,
                        style(format!(" ({})", repo)).dim()
                    );
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
                            println!(
                                "  {} Running `open {}`",
                                style("•").dim(),
                                style(&info.url).dim()
                            );
                            open_in_browser(&info.url)?;
                        }
                    }
                }

                // Merges must run sequentially: each merge modifies the base branch,
                // which invalidates the head SHA of subsequent PRs. Running them in
                // parallel causes "Base branch was modified" errors.
                for info in &merge_infos {
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
                        .change_context(AppError::Comment)
                        .attach(format!(
                            "Failed to inspect merge strategy for PR #{}",
                            info.pr_number
                        ))?;

                    let merge_mode = self.choose_approve_merge_mode(
                        info.pr_number,
                        info.ci_status,
                        &queue_status,
                        allow_auto_merge,
                    );

                    self.approve_pull_request(&info.owner, &info.repo_name, info.pr_number)
                        .await?;

                    match merge_mode {
                        ApproveMergeMode::Direct => {
                            self.debug(&format!("PR #{} merge queue: not used", info.pr_number));
                            self.direct_merge_pull_request(
                                &info.owner,
                                &info.repo_name,
                                info.pr_number,
                                merge_method,
                            )
                            .await?;
                            println!(
                                "  {} Approved and merged PR #{}{}",
                                style("✓").green(),
                                info.pr_number,
                                style(format!(" ({})", info.repo)).dim()
                            );
                        }
                        ApproveMergeMode::MergeQueueEnqueue => {
                            self.debug(&format!(
                                "PR #{} merge queue: used (enqueue)",
                                info.pr_number
                            ));
                            self.enqueue_pull_request(
                                &queue_status.pull_request_id,
                                &queue_status.head_oid,
                            )
                            .await?;
                            println!(
                                "  {} Approved PR #{} and added it to the merge queue{}",
                                style("✓").green(),
                                info.pr_number,
                                style(format!(" ({})", info.repo)).dim()
                            );
                        }
                        ApproveMergeMode::MergeQueueAutoMerge => {
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
                            println!(
                                "  {} Approved PR #{} and enabled auto-merge for the merge queue{}",
                                style("✓").green(),
                                info.pr_number,
                                style(format!(" ({})", info.repo)).dim()
                            );
                        }
                        ApproveMergeMode::AlreadyQueued => {
                            self.debug(&format!(
                                "PR #{} merge queue: already queued",
                                info.pr_number
                            ));
                            println!(
                                "  {} Approved PR #{} (already in merge queue){}",
                                style("✓").green(),
                                info.pr_number,
                                style(format!(" ({})", info.repo)).dim()
                            );
                        }
                        ApproveMergeMode::AlreadyAutoMergeEnabled => {
                            self.debug(&format!(
                                "PR #{} merge queue: already using auto-merge",
                                info.pr_number
                            ));
                            println!(
                                "  {} Approved PR #{} (auto-merge already enabled){}",
                                style("✓").green(),
                                info.pr_number,
                                style(format!(" ({})", info.repo)).dim()
                            );
                        }
                        ApproveMergeMode::SkipPendingWithoutQueue => {
                            self.debug(&format!("PR #{} merge queue: not used", info.pr_number));
                            println!(
                                "  {} Skipping PR #{} (CI {}, no merge queue){}",
                                style("⊘").yellow(),
                                info.pr_number,
                                info.ci_status,
                                style(format!(" ({})", info.repo)).dim()
                            );
                            continue;
                        }
                    }

                    if let Some(dep_update) = &info.dep_update {
                        review_state.record_approved(dep_update);
                        state_changed = true;
                    }

                    performed_action = Some(action);
                }
            }

            if state_changed {
                review_state.save_to_path(&state_path)?;
                println!(
                    "  {} Updated review state at {}",
                    style("✓").green(),
                    style(state_path.as_str()).dim()
                );
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

    async fn fetch_merge_queue_status(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        base_ref_name: &str,
    ) -> Result<MergeQueueStatus, Report<AppError>> {
        const QUERY: &str = r#"
            query MergeQueueStatus($owner: String!, $repo: String!, $number: Int!, $baseBranch: String!) {
              repository(owner: $owner, name: $repo) {
                mergeQueue(branch: $baseBranch) { id }
                pullRequest(number: $number) {
                  id
                  headRefOid
                  mergeQueueEntry { id }
                  autoMergeRequest { enabledAt }
                }
              }
            }
        "#;

        let number = i64::try_from(pr_number)
            .change_context(AppError::Comment)
            .attach_with(|| format!("PR #{} number is too large for GraphQL", pr_number))?;
        let payload = GraphqlRequest {
            query: QUERY,
            variables: MergeQueueStatusVariables {
                owner,
                repo,
                number,
                base_branch: base_ref_name,
            },
        };
        let response: GraphqlResponse<MergeQueueStatusData> = self
            .octocrab
            .graphql(&payload)
            .await
            .change_context(AppError::Comment)
            .attach(format!(
                "Failed to query merge queue status for PR #{}",
                pr_number
            ))?;
        let data = graphql_data(response)
            .change_context(AppError::Comment)
            .attach(format!(
                "Invalid merge queue status response for PR #{}",
                pr_number
            ))?;
        let repository = data
            .repository
            .ok_or_else(|| Report::new(AppError::Comment))
            .attach(format!(
                "Repository missing in GraphQL response for PR #{}",
                pr_number
            ))?;
        let pull_request = repository
            .pull_request
            .ok_or_else(|| Report::new(AppError::Comment))
            .attach(format!(
                "Pull request missing in GraphQL response for PR #{}",
                pr_number
            ))?;

        let _ = repository.merge_queue.as_ref().map(|node| node.id.as_str());
        let _ = pull_request
            .merge_queue_entry
            .as_ref()
            .map(|node| node.id.as_str());
        let _ = pull_request
            .auto_merge_request
            .as_ref()
            .map(|request| request.enabled_at.as_str());

        Ok(MergeQueueStatus {
            pull_request_id: pull_request.id,
            head_oid: pull_request.head_ref_oid,
            uses_merge_queue: repository.merge_queue.is_some(),
            already_queued: pull_request.merge_queue_entry.is_some(),
            auto_merge_enabled: pull_request.auto_merge_request.is_some(),
        })
    }

    fn choose_approve_merge_mode(
        &self,
        pr_number: u64,
        ci_status: CiStatus,
        queue_status: &MergeQueueStatus,
        allow_auto_merge: bool,
    ) -> ApproveMergeMode {
        if queue_status.uses_merge_queue {
            if queue_status.already_queued {
                return ApproveMergeMode::AlreadyQueued;
            }
            if queue_status.auto_merge_enabled {
                return ApproveMergeMode::AlreadyAutoMergeEnabled;
            }

            return match ci_status {
                CiStatus::Passing | CiStatus::Unknown => ApproveMergeMode::MergeQueueEnqueue,
                CiStatus::Pending => ApproveMergeMode::MergeQueueAutoMerge,
                CiStatus::Failing => unreachable!("failing PRs are skipped before planning"),
            };
        }

        match ci_status {
            CiStatus::Passing | CiStatus::Unknown => ApproveMergeMode::Direct,
            CiStatus::Pending if allow_auto_merge => {
                self.debug(&format!(
                    "PR #{} merge queue: not used (auto-merge available but disabled for this flow)",
                    pr_number
                ));
                ApproveMergeMode::SkipPendingWithoutQueue
            }
            CiStatus::Pending => ApproveMergeMode::SkipPendingWithoutQueue,
            CiStatus::Failing => unreachable!("failing PRs are skipped before planning"),
        }
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
            .change_context(AppError::Comment)
            .attach(format!("Failed to get PR #{}", pr_number))?;
        let head_sha = pr_data.head.sha;

        self.debug(&format!("Approving PR #{}", pr_number));

        #[expect(deprecated)] // no alternative yet
        let pr_handle = pulls.pull_number(pr_number);

        pr_handle
            .reviews()
            .create_review(head_sha, "", ReviewAction::Approve, Vec::new())
            .await
            .change_context(AppError::Comment)
            .attach(format!("Failed to approve PR #{}", pr_number))?;

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
                .change_context(AppError::Comment)
                .attach(format!("Failed to get PR #{}", pr_number))?;

            let head_sha = pr_data.head.sha;

            self.debug(&format!(
                "Merging PR #{} using {:?} (head: {}, attempt {}/{})",
                pr_number,
                merge_method,
                &head_sha[..8.min(head_sha.len())],
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
                    return Err(e).change_context(AppError::Comment).attach(format!(
                        "Failed to merge PR #{} after {} attempts",
                        pr_number, MAX_ATTEMPTS
                    ));
                }
            }
        }

        unreachable!()
    }

    async fn enqueue_pull_request(
        &self,
        pull_request_id: &str,
        expected_head_oid: &str,
    ) -> Result<(), Report<AppError>> {
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
        let response: GraphqlResponse<MutationOnlyResponse> = self
            .octocrab
            .graphql(&payload)
            .await
            .change_context(AppError::Comment)
            .attach("Failed to enqueue pull request")?;
        let data = graphql_data(response)
            .change_context(AppError::Comment)
            .attach("Invalid enqueuePullRequest response")?;
        let _merge_queue_entry = data
            .enqueue_pull_request
            .and_then(|payload| payload.merge_queue_entry)
            .ok_or_else(|| Report::new(AppError::Comment))
            .attach("enqueuePullRequest did not return a merge queue entry")?;
        Ok(())
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
        let response: GraphqlResponse<MutationOnlyResponse> = self
            .octocrab
            .graphql(&payload)
            .await
            .change_context(AppError::Comment)
            .attach("Failed to enable pull request auto-merge")?;
        let data = graphql_data(response)
            .change_context(AppError::Comment)
            .attach("Invalid enablePullRequestAutoMerge response")?;
        let _pull_request = data
            .enable_pull_request_auto_merge
            .and_then(|payload| payload.pull_request)
            .ok_or_else(|| Report::new(AppError::Comment))
            .attach("enablePullRequestAutoMerge did not return a pull request")?;
        Ok(())
    }
}

fn graphql_data<T>(response: GraphqlResponse<T>) -> Result<T, Report<AppError>> {
    if !response.errors.is_empty() {
        let messages = response
            .errors
            .into_iter()
            .map(|error| error.message)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(Report::new(AppError::Comment)).attach(messages);
    }

    response
        .data
        .ok_or_else(|| Report::new(AppError::Comment))
        .attach("GraphQL response did not include data")
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
        _ => Err(Report::new(AppError::Comment)).attach("No merge method available"),
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
