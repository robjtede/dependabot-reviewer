use std::collections::BTreeSet;

use dialoguer::{theme::ColorfulTheme, MultiSelect, Select};
use error_stack::{Report, ResultExt as _};

use super::ReviewItem;
use crate::error::AppError;

pub(super) fn select_rebase_items(
    review_items: Vec<ReviewItem>,
) -> Result<Vec<ReviewItem>, Report<AppError>> {
    let theme = ColorfulTheme::default();
    let scope = Select::with_theme(&theme)
        .with_prompt("Choose PRs to rebase")
        .items(["Rebase All", "Rebase Some Repos", "Rebase Some PRs"])
        .default(0)
        .interact()
        .change_context(AppError::ActionSelection)
        .attach("Rebase scope selection failed")?;

    let selection = match scope {
        0 => RebaseSelection::All,
        1 => {
            let repos: Vec<_> = review_items
                .iter()
                .map(|item| item.repo.as_str())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let selections = MultiSelect::with_theme(&theme)
                .with_prompt(
                    "Choose repositories to rebase (Space to select or deselect, Enter to confirm)",
                )
                .items(&repos)
                .interact()
                .change_context(AppError::ActionSelection)
                .attach("Rebase repository selection failed")?;

            RebaseSelection::Repositories(
                selections
                    .into_iter()
                    .filter_map(|index| repos.get(index))
                    .map(|repo| (*repo).to_owned())
                    .collect(),
            )
        }
        2 => {
            let labels: Vec<_> = review_items
                .iter()
                .map(|item| format!("{}: {}", item.repo, item.pr.title))
                .collect();
            let selections = MultiSelect::with_theme(&theme)
                .with_prompt("Choose PRs to rebase (Space to select or deselect, Enter to confirm)")
                .items(&labels)
                .interact()
                .change_context(AppError::ActionSelection)
                .attach("Rebase PR selection failed")?;

            RebaseSelection::PullRequests(selections)
        }
        _ => {
            return Err(Report::new(AppError::ActionSelection))
                .attach(format!("Invalid rebase scope selection: {scope}"));
        }
    };

    Ok(selection.apply(review_items))
}

enum RebaseSelection {
    All,
    Repositories(Vec<String>),
    PullRequests(Vec<usize>),
}

impl RebaseSelection {
    fn apply(self, review_items: Vec<ReviewItem>) -> Vec<ReviewItem> {
        review_items
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let selected = match &self {
                    Self::All => true,
                    Self::Repositories(repos) => repos.contains(&item.repo),
                    Self::PullRequests(indices) => indices.contains(&index),
                };

                selected.then_some(item)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::process::test_support::review_item, github::CiStatus};

    fn review_items() -> Vec<ReviewItem> {
        let first = review_item(12, "Bump serde from 1.0.0 to 1.0.1", CiStatus::Passing);
        let second = review_item(11, "Bump tokio from 1.0.0 to 1.1.0", CiStatus::Pending);
        let mut third = review_item(12, "Bump serde from 1.0.0 to 1.0.1", CiStatus::Failing);
        third.repo = "example/other".to_owned();
        third.repo_name = "other".to_owned();

        vec![first, second, third]
    }

    fn selected_prs(selection: RebaseSelection) -> Vec<(String, u64)> {
        selection
            .apply(review_items())
            .into_iter()
            .map(|item| (item.repo, item.pr.number))
            .collect()
    }

    #[test]
    fn rebase_all_includes_every_pr() {
        assert_eq!(
            selected_prs(RebaseSelection::All),
            [
                ("example/repo".to_owned(), 12),
                ("example/repo".to_owned(), 11),
                ("example/other".to_owned(), 12),
            ]
        );
    }

    #[test]
    fn rebase_repositories_includes_all_prs_only_in_selected_repositories() {
        assert_eq!(
            selected_prs(RebaseSelection::Repositories(vec![
                "example/repo".to_owned()
            ])),
            [
                ("example/repo".to_owned(), 12),
                ("example/repo".to_owned(), 11),
            ]
        );
    }

    #[test]
    fn rebase_prs_keeps_only_selected_rows_with_duplicate_numbers_and_titles() {
        assert_eq!(
            selected_prs(RebaseSelection::PullRequests(vec![1, 2])),
            [
                ("example/repo".to_owned(), 11),
                ("example/other".to_owned(), 12),
            ]
        );
    }

    #[test]
    fn empty_rebase_selection_includes_no_prs() {
        assert!(selected_prs(RebaseSelection::Repositories(Vec::new())).is_empty());
        assert!(selected_prs(RebaseSelection::PullRequests(Vec::new())).is_empty());
    }
}
