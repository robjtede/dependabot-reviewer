use std::time::Duration;

use console::style;
use dialoguer::{Select, theme::ColorfulTheme};
use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt;
use futures_util::{FutureExt as _, StreamExt as _};
use octocrab::{models::pulls::ReviewAction, params::pulls::MergeMethod};

use super::App;
use crate::{cli::Action, error::AppError};

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

        println!("  Found {} Dependabot PR(s):", prs.len());
        for pr in &prs {
            println!("    {}", pr.display());
        }
        println!();

        // Use CLI-provided action or prompt interactively
        let action = if let Some(action) = self.cli.action {
            action
        } else {
            let items = vec!["Approve + Merge", "Rebase", "Recreate"];
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Choose action to apply to these PRs")
                .items(&items)
                .default(0)
                .interact()
                .change_context(AppError::ActionSelection)
                .attach("Action selection failed")?;
            match selection {
                0 => Action::ApproveMerge,
                1 => Action::Rebase,
                2 => Action::Recreate,
                _ => unreachable!(),
            }
        };

        let mut performed_action = None;
        let mut comment_tasks = Vec::new();
        let mut merge_infos: Vec<(String, String, u64)> = Vec::new();

        for pr in &prs {
            if self.cli.dry_run {
                match action {
                    Action::ApproveMerge if !pr.ci_status.is_mergeable() => {
                        println!(
                            "  [DRY RUN] Would skip PR #{} (CI {}): {}",
                            pr.number, pr.ci_status, pr.url
                        );
                        continue;
                    }
                    Action::ApproveMerge => {
                        println!(
                            "  [DRY RUN] Would approve and merge PR #{}: {}",
                            pr.number, pr.url
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
                    Action::Rebase => {
                        comment_tasks.push(
                            async move {
                                self.debug(&format!("Commenting on PR #{}", pr_number));

                                octocrab
                                    .issues(owner, repo_name)
                                    .create_comment(pr_number, "@dependabot rebase")
                                    .await
                                    .change_context(AppError::Comment)
                                    .attach(format!("Failed to comment on PR #{}", pr_number))?;

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
                                    .attach(format!("Failed to comment on PR #{}", pr_number))?;

                                Ok::<_, Report<_>>(pr_number)
                            }
                            .boxed(),
                        );
                    }
                    Action::ApproveMerge => {
                        if pr.ci_status.is_mergeable() {
                            merge_infos.push((owner, repo_name, pr_number));
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
            // Merges must run sequentially: each merge modifies the base branch,
            // which invalidates the head SHA of subsequent PRs. Running them in
            // parallel causes "Base branch was modified" errors.
            for (owner, repo_name, pr_number) in &merge_infos {
                let repo_info = self
                    .octocrab
                    .repos(owner, repo_name)
                    .get()
                    .await
                    .change_context(AppError::Comment)
                    .attach(format!("Failed to get repo info for PR #{}", pr_number))?;

                let merge_method =
                    match (repo_info.allow_merge_commit, repo_info.allow_squash_merge) {
                        (Some(true), Some(true)) => MergeMethod::Merge,
                        (Some(true), Some(false)) => MergeMethod::Merge,
                        (Some(false), Some(true)) => MergeMethod::Squash,
                        _ => {
                            return Err(Report::new(AppError::Comment)).attach(format!(
                                "No merge method available for PR #{}",
                                pr_number
                            ));
                        }
                    };

                const MAX_ATTEMPTS: u32 = 4;

                for attempt in 1..=MAX_ATTEMPTS {
                    let pulls = self.octocrab.pulls(owner, repo_name);

                    let pr_data = pulls
                        .get(*pr_number)
                        .await
                        .change_context(AppError::Comment)
                        .attach(format!("Failed to get PR #{}", pr_number))?;

                    let head_sha = pr_data.head.sha;

                    if attempt == 1 {
                        self.debug(&format!("Approving PR #{}", pr_number));

                        #[expect(deprecated)] // no alternative yet
                        let pr_handle = pulls.pull_number(*pr_number);

                        pr_handle
                            .reviews()
                            .create_review(head_sha.clone(), "", ReviewAction::Approve, Vec::new())
                            .await
                            .change_context(AppError::Comment)
                            .attach(format!("Failed to approve PR #{}", pr_number))?;
                    }

                    self.debug(&format!(
                        "Merging PR #{} using {:?} (head: {}, attempt {}/{})",
                        pr_number,
                        merge_method,
                        &head_sha[..8.min(head_sha.len())],
                        attempt,
                        MAX_ATTEMPTS
                    ));

                    match pulls
                        .merge(*pr_number)
                        .sha(head_sha)
                        .method(merge_method)
                        .send()
                        .await
                    {
                        Ok(_) => {
                            println!(
                                "  {} Approved and merged PR #{}{}",
                                style("✓").green(),
                                pr_number,
                                style(format!(" ({})", repo)).dim()
                            );
                            performed_action = Some(action);
                            break;
                        }
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
            }
        }

        Ok(performed_action)
    }
}
