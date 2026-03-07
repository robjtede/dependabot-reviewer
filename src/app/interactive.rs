use std::collections::HashMap;

use dialoguer::{theme::ColorfulTheme, FuzzySelect};
use error_stack::{Report, ResultExt as _};

use crate::error::AppError;

use super::App;

impl App {
    pub(crate) fn select_repository_interactive(
        &self,
        repo_counts: HashMap<String, usize>,
    ) -> Result<Option<String>, Report<AppError>> {
        let mut items: Vec<String> = repo_counts
            .iter()
            .map(|(repo, count)| format!("{} ({} PRs)", repo, count))
            .collect();

        items.sort();

        if items.is_empty() {
            return Ok(None);
        }

        println!("Repositories with open Dependabot PRs:");
        println!();

        let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Choose a repository")
            .items(&items)
            .interact()
            .change_context(AppError::Interactive)
            .attach("Interactive selection failed")?;

        if selection >= items.len() {
            return Ok(None);
        }

        let selected = &items[selection];
        let repo = selected.split(" (").next().unwrap_or("").to_string();

        Ok(Some(repo))
    }
}
