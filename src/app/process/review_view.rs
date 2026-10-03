//! Display the review batch and select the next session action.

use camino::Utf8Path;
use console::style;
use dialoguer::{theme::ColorfulTheme, Select};
use error_stack::{Report, ResultExt as _};

use super::review::{ReviewBatch, ReviewItem};
use crate::{
    app::{approval_workflow::MergeQueueStatus, state::ReviewState},
    cli::Action,
    error::AppError,
    github::CiStatus,
};

#[derive(Clone, Copy)]
pub(super) enum PromptChoice {
    Refresh,
    PrintFailingCiPrompt,
    ApproveMergeIncludingNonPassingCi,
    Action(Action),
}

impl ReviewBatch {
    pub(super) fn display(&self, review_state: &ReviewState, state_path: &Utf8Path) {
        let review_items = &self.items;
        let pending_statuses = &self.pending_statuses;
        println!("  Found {} Dependabot PR(s):", review_items.len());
        println!("  Review state: {}", style(state_path.as_str()).dim());
        let mut current_repo: Option<&str> = None;
        for item in review_items {
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
    }

    pub(super) fn prompt(&self, action: Option<Action>) -> Result<PromptChoice, Report<AppError>> {
        if let Some(action) = action {
            return Ok(PromptChoice::Action(action));
        }

        let review_items = &self.items;
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
            })
    }
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

pub(super) fn failing_ci_agent_prompt(review_items: &[ReviewItem]) -> Option<String> {
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
mod tests;
