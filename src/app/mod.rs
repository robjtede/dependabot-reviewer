mod fetch;
mod interactive;
mod process;

use std::io::IsTerminal as _;
use std::process::Command;

use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm};
use error_stack::{Report, ResultExt as _};
use octocrab::Octocrab;

use crate::cli::{Action, Cli};
use crate::error::AppError;

pub struct App {
    pub(crate) cli: Cli,
    pub(crate) octocrab: Octocrab,
}

impl App {
    pub fn new(cli: Cli) -> Result<Self, Report<AppError>> {
        let mut token = std::env::var("GITHUB_TOKEN").ok();

        if token.is_none() && !cli.dry_run {
            let gh_check = Command::new("gh").arg("--version").output();

            if gh_check.is_ok() && std::io::stdin().is_terminal() {
                let use_gh = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt("GITHUB_TOKEN not found. Try to use 'gh auth token'?")
                    .default(false)
                    .interact()
                    .map_err(|_| Report::new(AppError::Interactive))?;

                if use_gh {
                    let output = Command::new("gh")
                        .args(["auth", "token"])
                        .output()
                        .change_context(AppError::Initialization)
                        .attach("Failed to run 'gh auth token'")?;

                    if output.status.success() {
                        let t = String::from_utf8(output.stdout)
                            .change_context(AppError::Initialization)?
                            .trim()
                            .to_string();
                        if !t.is_empty() {
                            token = Some(t);
                        }
                    }
                }
            }
        }

        let mut builder = Octocrab::builder();
        if let Some(token) = token {
            builder = builder.personal_token(token);
        } else if !cli.dry_run {
            return Err(Report::new(AppError::Initialization).attach(
                "GITHUB_TOKEN is required. Please set it or authenticate with 'gh auth login'.",
            ));
        }

        let octocrab = builder
            .build()
            .change_context(AppError::Initialization)
            .attach("Failed to build Octocrab client")?;

        Ok(Self { cli, octocrab })
    }

    pub(crate) fn debug(&self, msg: &str) {
        if self.cli.verbose {
            eprintln!("DEBUG: {}", msg);
        }
    }

    pub async fn run(&self) -> Result<(), Report<AppError>> {
        self.debug("Starting dependabot-reviewer");

        let selected_repo = if let Some(repo) = &self.cli.repo {
            self.debug(&format!("Using specified repository: {}", repo));
            Some(repo.clone())
        } else {
            let repo_counts = self.aggregate_repos_with_counts().await?;

            if repo_counts.is_empty() {
                println!("No open Dependabot PRs found in any repository.");
                return Ok(());
            }

            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                match self.select_repository_interactive(repo_counts)? {
                    Some(repo) => Some(repo),
                    None => {
                        println!("No repository selected. Exiting.");
                        return Ok(());
                    }
                }
            } else {
                println!("Repositories with open Dependabot PRs:");
                let mut repos: Vec<_> = repo_counts.into_iter().collect();
                repos.sort_by(|(_, a), (_, b)| b.cmp(a));
                for (repo, count) in repos {
                    println!("  {} ({} PRs)", repo, count);
                }
                println!();
                println!("To process a specific repository:");
                println!("  - Run with --repo owner/repo");
                println!("  - Or run in an interactive terminal to select from a list");
                return Ok(());
            }
        };

        let mut performed_action = None;
        if let Some(repo) = selected_repo {
            performed_action = self.process_repository(&repo).await?;
        }

        println!();
        println!("{}", style("Done!").green().bold());

        if self.cli.dry_run {
            println!("Run without --dry-run to actually comment on PRs.");
        } else if let Some(action) = performed_action {
            match action {
                Action::ApproveMerge => {
                    println!("All selected PRs have been approved and merged.");
                }
                Action::Rebase => {
                    println!("Dependabot will rebase each PR automatically.");
                    println!("You can monitor the progress in the PRs.");
                }
                Action::Recreate => {
                    println!("Dependabot will recreate each PR automatically.");
                    println!("You can monitor the progress in the PRs.");
                }
            }
        }

        Ok(())
    }
}
