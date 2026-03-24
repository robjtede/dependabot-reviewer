use std::collections::HashMap;

use dialoguer::{theme::ColorfulTheme, MultiSelect};
use error_stack::{Report, ResultExt as _};

use super::App;
use crate::error::AppError;

impl App {
    pub(crate) fn select_repository_interactive(
        &self,
        repo_counts: HashMap<String, usize>,
    ) -> Result<Vec<String>, Report<AppError>> {
        let mut items: Vec<String> = repo_counts
            .iter()
            .map(|(repo, count)| format!("{} ({} PRs)", repo, count))
            .collect();

        items.sort();

        if items.is_empty() {
            return Ok(Vec::new());
        }

        println!("Repositories with open Dependabot PRs:");
        println!();

        let selections = MultiSelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Choose repositories")
            .items(&items)
            .interact()
            .change_context(AppError::Interactive)
            .attach("Interactive selection failed")?;

        Ok(selections
            .into_iter()
            .filter_map(|selection| items.get(selection))
            .filter_map(|selected| selected.split(" (").next())
            .map(ToOwned::to_owned)
            .collect())
    }
}
