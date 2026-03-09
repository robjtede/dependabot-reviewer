use std::{io::IsTerminal as _, process::Command, time::Duration};

use console::style;
use dialoguer::{Confirm, Select, theme::ColorfulTheme};
use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt;
use futures_util::{FutureExt as _, StreamExt as _};
use octocrab::{models::pulls::ReviewAction, params::pulls::MergeMethod};

use super::{App, state::ReviewState};
use crate::{cli::Action, error::AppError, github::DepUpdate};

struct MergeInfo {
    owner: String,
    repo_name: String,
    pr_number: u64,
    url: String,
    dep_update: Option<DepUpdate>,
    previously_reviewed: bool,
}

impl App {
    pub(crate) async fn process_repository(
        &self,
        repo: &str,
    ) -> Result<Option<Action>, Report<AppError>> {
        println!("Fetching PR details for {}", repo);

        let prs = self.fetch_dependabot_prs_for_repo(repo).await?;

        if prs.is_empty() {
            println!("  No open Dependabot PRs found in {}", repo);
            return Ok(None);
        }

        let (mut review_state, state_path) = ReviewState::load_from_default_path()?;
        println!("  Found {} Dependabot PR(s):", prs.len());
        println!("  Review state: {}", style(state_path.display()).dim());
        for pr in &prs {
            let previously_reviewed = pr
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
                pr.ci_status.icon(),
                pr.number,
                pr.title,
                review_badge,
                style(&pr.url).dim()
            );
            if pr.dep_update.is_none() {
                println!(
                    "      {}",
                    style("No dependency/version metadata parsed from PR title").dim()
                );
            }
            println!();
        }
        println!();

        let mut performed_action = None;
        let mut opened_in_browser_in_session = false;
        loop {
            let action = if let Some(action) = self.cli.action {
                action
            } else {
                let items = vec!["Open In Browser", "Approve + Merge", "Rebase", "Recreate"];
                let selection = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("Choose action to apply to these PRs")
                    .items(&items)
                    .default(0)
                    .interact()
                    .change_context(AppError::ActionSelection)
                    .attach("Action selection failed")?;
                match selection {
                    0 => Action::OpenInBrowser,
                    1 => Action::ApproveMerge,
                    2 => Action::Rebase,
                    3 => Action::Recreate,
                    _ => unreachable!(),
                }
            };

            let mut comment_tasks = Vec::new();
            let mut merge_infos: Vec<MergeInfo> = Vec::new();
            let mut state_changed = false;

            for pr in &prs {
                let previously_reviewed = pr
                    .dep_update
                    .as_ref()
                    .map(|dep_update| review_state.is_previously_reviewed(dep_update))
                    .unwrap_or(false);

                if self.cli.dry_run {
                    match action {
                        Action::OpenInBrowser => {
                            println!("  [DRY RUN] Would open PR #{}: {}", pr.number, pr.url);
                        }
                        Action::ApproveMerge if !pr.ci_status.is_mergeable() => {
                            println!(
                                "  [DRY RUN] Would skip PR #{} (CI {}): {}",
                                pr.number, pr.ci_status, pr.url
                            );
                            continue;
                        }
                        Action::ApproveMerge => {
                            let marker = if previously_reviewed {
                                " [previously reviewed]"
                            } else {
                                ""
                            };
                            println!(
                                "  [DRY RUN] Would approve and merge PR #{}{}: {}",
                                pr.number, marker, pr.url
                            );
                        }
                        _ => {
                            println!("  [DRY RUN] Would comment on PR #{}: {}", pr.number, pr.url);
                        }
                    }
                } else {
                    let (owner, repo_name) = repo
                        .split_once('/')
                        .ok_or_else(|| Report::new(AppError::InvalidInput))
                        .attach_with(|| format!("Invalid repo format: {}", repo))?;

                    let owner = owner.to_string();
                    let repo_name = repo_name.to_string();
                    let pr_number = pr.number;
                    let octocrab = self.octocrab.clone();

                    match action {
                        Action::OpenInBrowser => {
                            println!(
                                "  {} Running `open {}`",
                                style("•").dim(),
                                style(&pr.url).dim()
                            );
                            open_in_browser(&pr.url)?;
                            println!(
                                "  {} Opened PR #{}{}",
                                style("✓").green(),
                                pr_number,
                                style(format!(" ({})", repo)).dim()
                            );
                            performed_action = Some(action);
                            opened_in_browser_in_session = true;
                        }
                        Action::Rebase => {
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

                                    Ok::<_, Report<_>>(pr_number)
                                }
                                .boxed(),
                            );
                        }
                        Action::Recreate => {
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

                                    Ok::<_, Report<_>>(pr_number)
                                }
                                .boxed(),
                            );
                        }
                        Action::ApproveMerge => {
                            if pr.ci_status.is_mergeable() {
                                merge_infos.push(MergeInfo {
                                    owner,
                                    repo_name,
                                    pr_number,
                                    url: pr.url.clone(),
                                    dep_update: pr.dep_update.clone(),
                                    previously_reviewed,
                                });
                            } else {
                                println!(
                                    "  {} Skipping PR #{} (CI {}){}",
                                    style("⊘").yellow(),
                                    pr_number,
                                    pr.ci_status,
                                    style(format!(" ({})", repo)).dim()
                                );
                            }
                        }
                    }
                }
            }

            if !comment_tasks.is_empty() {
                let mut stream = futures_util::stream::iter(comment_tasks).buffered_unordered(5);

                while let Some(result) = stream.next().await {
                    let pr_number = result?;
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
                    let repo_info = self
                        .octocrab
                        .repos(&info.owner, &info.repo_name)
                        .get()
                        .await
                        .change_context(AppError::Comment)
                        .attach(format!(
                            "Failed to get repo info for PR #{}",
                            info.pr_number
                        ))?;

                    let merge_method =
                        match (repo_info.allow_merge_commit, repo_info.allow_squash_merge) {
                            (Some(true), Some(true)) => MergeMethod::Merge,
                            (Some(true), Some(false)) => MergeMethod::Merge,
                            (Some(false), Some(true)) => MergeMethod::Squash,
                            _ => {
                                return Err(Report::new(AppError::Comment)).attach(format!(
                                    "No merge method available for PR #{}",
                                    info.pr_number
                                ));
                            }
                        };

                    const MAX_ATTEMPTS: u32 = 4;

                    for attempt in 1..=MAX_ATTEMPTS {
                        let pulls = self.octocrab.pulls(&info.owner, &info.repo_name);

                        let pr_data = pulls
                            .get(info.pr_number)
                            .await
                            .change_context(AppError::Comment)
                            .attach(format!("Failed to get PR #{}", info.pr_number))?;

                        let head_sha = pr_data.head.sha;

                        if attempt == 1 {
                            self.debug(&format!("Approving PR #{}", info.pr_number));

                            #[expect(deprecated)] // no alternative yet
                            let pr_handle = pulls.pull_number(info.pr_number);

                            pr_handle
                                .reviews()
                                .create_review(
                                    head_sha.clone(),
                                    "",
                                    ReviewAction::Approve,
                                    Vec::new(),
                                )
                                .await
                                .change_context(AppError::Comment)
                                .attach(format!("Failed to approve PR #{}", info.pr_number))?;
                        }

                        self.debug(&format!(
                            "Merging PR #{} using {:?} (head: {}, attempt {}/{})",
                            info.pr_number,
                            merge_method,
                            &head_sha[..8.min(head_sha.len())],
                            attempt,
                            MAX_ATTEMPTS
                        ));

                        match pulls
                            .merge(info.pr_number)
                            .sha(head_sha)
                            .method(merge_method)
                            .send()
                            .await
                        {
                            Ok(_) => {
                                println!(
                                    "  {} Approved and merged PR #{}{}",
                                    style("✓").green(),
                                    info.pr_number,
                                    style(format!(" ({})", repo)).dim()
                                );
                                if let Some(dep_update) = &info.dep_update {
                                    review_state.record_approved(dep_update);
                                    state_changed = true;
                                }
                                performed_action = Some(action);
                                break;
                            }
                            Err(e) if attempt < MAX_ATTEMPTS => {
                                let delay = Duration::from_secs(2u64.pow(attempt));
                                self.debug(&format!(
                                    "Merge failed for PR #{}, retrying in {}s: {}",
                                    info.pr_number,
                                    delay.as_secs(),
                                    e
                                ));
                                tokio::time::sleep(delay).await;
                            }
                            Err(e) => {
                                return Err(e).change_context(AppError::Comment).attach(format!(
                                    "Failed to merge PR #{} after {} attempts",
                                    info.pr_number, MAX_ATTEMPTS
                                ));
                            }
                        }
                    }
                }
            }

            if state_changed {
                review_state.save_to_path(&state_path)?;
                println!(
                    "  {} Updated review state at {}",
                    style("✓").green(),
                    style(state_path.display()).dim()
                );
            }

            if self.cli.action.is_some() || !matches!(action, Action::OpenInBrowser) {
                return Ok(performed_action);
            }

            println!();
        }
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
