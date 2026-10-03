//! Execute browser, close, rebase, and recreate actions for a review batch.

use std::{collections::HashSet, process::Command};

use console::style;
use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt;
use futures_util::{FutureExt as _, StreamExt as _};

use super::{
    review::ReviewItem,
    status_rows::{should_render_status_rows, PrStatusRows},
};
use crate::{
    app::{state::ReviewState, App},
    cli::Action,
    error::AppError,
};

#[derive(Default)]
pub(super) struct ActionResults {
    pub(super) performed_action: bool,
    pub(super) opened_in_browser: bool,
}

impl App {
    pub(super) async fn process_actions(
        &self,
        action: Action,
        review_items: &[ReviewItem],
        review_state: &ReviewState,
        processed_pr_urls: &mut HashSet<String>,
    ) -> Result<ActionResults, Report<AppError>> {
        let mut result = ActionResults::default();
        let mut action_tasks = Vec::new();
        let pr_statuses = should_render_status_rows(self.cli.dry_run, self.cli.verbose)
            .then(|| {
                PrStatusRows::new(
                    review_items
                        .iter()
                        .map(|item| (item.repo.clone(), item.pr.number)),
                )
            })
            .flatten();

        for item in review_items {
            let previously_reviewed = item.previously_reviewed(review_state);
            if self.cli.dry_run {
                if matches!(action, Action::OpenUnreviewedInBrowser) && previously_reviewed {
                    continue;
                }
                let operation = match action {
                    Action::OpenUnreviewedInBrowser => "open",
                    Action::Close => "close",
                    _ => "comment on",
                };
                println!(
                    "  [DRY RUN] Would {} PR #{}{}: {}",
                    operation,
                    item.pr.number,
                    style(format!(" ({})", item.repo)).dim(),
                    item.pr.url,
                );
                continue;
            }

            let pr_number = item.pr.number;
            let octocrab = self.octocrab.clone();

            match action {
                Action::OpenUnreviewedInBrowser => {
                    if previously_reviewed {
                        if let Some(statuses) = &pr_statuses {
                            statuses.finish_skipped(&item.repo, pr_number, "Already reviewed");
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
                    result.performed_action = true;
                    result.opened_in_browser = true;
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
                Action::Rebase | Action::Recreate => {
                    let (status, comment) = if matches!(action, Action::Rebase) {
                        ("Requesting Dependabot rebase", "@dependabot rebase")
                    } else {
                        ("Requesting Dependabot recreation", "@dependabot recreate")
                    };
                    if let Some(statuses) = &pr_statuses {
                        statuses.update(&item.repo, pr_number, status);
                    }
                    let owner = item.owner.clone();
                    let repo_name = item.repo_name.clone();
                    let repo = item.repo.clone();
                    action_tasks.push(
                        async move {
                            self.debug(&format!("Commenting on PR #{}", pr_number));

                            octocrab
                                .issues(owner, repo_name)
                                .create_comment(pr_number, comment)
                                .await
                                .change_context(AppError::Comment)
                                .attach(format!("Failed to comment on PR #{}", pr_number))?;

                            Ok::<_, Report<_>>((pr_number, repo))
                        }
                        .boxed(),
                    );
                }
                Action::ApproveMerge => {
                    return Err(Report::new(AppError::InvalidInput)
                        .attach("Approval requires an approval batch"));
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

            while let Some(completed) = stream.next().await {
                let (pr_number, repo) = completed?;
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
                result.performed_action = true;
            }
        }

        if let Some(statuses) = pr_statuses {
            statuses.finish();
        }
        if !self.cli.dry_run && !matches!(action, Action::OpenUnreviewedInBrowser) {
            processed_pr_urls.extend(review_items.iter().map(|item| item.pr.api_url.clone()));
        }

        Ok(result)
    }
}

pub(super) fn open_in_browser(url: &str) -> Result<(), Report<AppError>> {
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

#[cfg(test)]
mod tests;
