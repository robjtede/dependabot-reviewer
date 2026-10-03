//! Coordinate review refresh, action selection, and review-state persistence.

mod actions;
mod approval;
mod approval_submission;
mod approval_view;
mod merge_batch;
mod merge_recovery;
mod pull_requests;
mod review;
mod review_view;
mod status_rows;

#[cfg(test)]
mod test_support;

use std::collections::HashSet;

use camino::Utf8PathBuf;
use error_stack::Report;

use self::{
    approval::ApprovalBatch,
    review::ReviewBatch,
    review_view::{failing_ci_agent_prompt, PromptChoice},
};
use super::{state::ReviewState, App};
use crate::{cli::Action, error::AppError};

struct ReviewSession {
    state_path: Utf8PathBuf,
    review_state: ReviewState,
    performed_action: Option<Action>,
    opened_in_browser: bool,
}

impl ReviewSession {
    fn load(app: &App) -> Result<Self, Report<AppError>> {
        let state_path = ReviewState::default_path()?;
        app.debug(&format!("Reading state from {}", state_path));
        let review_state = ReviewState::load_from_path(&state_path)?;

        Ok(Self {
            state_path,
            review_state,
            performed_action: None,
            opened_in_browser: false,
        })
    }

    async fn run(
        &mut self,
        app: &App,
        repos: &[String],
        processed_pr_urls: &mut HashSet<String>,
    ) -> Result<Option<Action>, Report<AppError>> {
        loop {
            let batch = ReviewBatch::fetch(app, repos).await?;
            if batch.items.is_empty() {
                println!("  No open Dependabot PRs found in the selected repositories.");
                return Ok(self.performed_action);
            }

            batch.display(&self.review_state, &self.state_path);

            let (action, allow_non_passing_ci) = match batch.prompt(app.cli.action)? {
                PromptChoice::Refresh => {
                    println!();
                    continue;
                }
                PromptChoice::PrintFailingCiPrompt => {
                    match failing_ci_agent_prompt(&batch.items) {
                        Some(prompt) => println!("{}", prompt),
                        None => println!("  No Dependabot PRs have failing CI."),
                    }
                    return Ok(self.performed_action);
                }
                PromptChoice::ApproveMergeIncludingNonPassingCi => (Action::ApproveMerge, true),
                PromptChoice::Action(action) => (action, app.cli.allow_non_passing_ci),
            };

            if matches!(action, Action::ApproveMerge) {
                let items = batch.approval_items(app).await?;
                if items.is_empty() {
                    return Ok(self.performed_action);
                }

                let approvals =
                    ApprovalBatch::prepare(app, repos, items, allow_non_passing_ci).await?;
                if app.cli.dry_run {
                    approvals.preview(app, &self.review_state).await?;
                } else {
                    let results = approvals.run(app, self).await?;
                    if results.performed_action {
                        self.performed_action = Some(action);
                    }
                    processed_pr_urls.extend(approvals.pr_urls().map(str::to_owned));
                    results.finish()?;
                }
            } else {
                let result = app
                    .process_actions(action, &batch.items, &self.review_state, processed_pr_urls)
                    .await?;
                if result.performed_action {
                    self.performed_action = Some(action);
                }
                self.opened_in_browser |= result.opened_in_browser;
            }

            if matches!(action, Action::OpenUnreviewedInBrowser)
                && !app.cli.dry_run
                && !self.opened_in_browser
            {
                println!("  No unreviewed PRs to open.");
            }

            if app.cli.action.is_some() || !matches!(action, Action::OpenUnreviewedInBrowser) {
                return Ok(self.performed_action);
            }

            println!();
        }
    }
}

impl App {
    pub(crate) async fn process_repositories(
        &self,
        repos: &[String],
        processed_pr_urls: &mut HashSet<String>,
    ) -> Result<Option<Action>, Report<AppError>> {
        ReviewSession::load(self)?
            .run(self, repos, processed_pr_urls)
            .await
    }
}
