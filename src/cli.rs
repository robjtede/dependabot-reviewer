use clap::{Parser, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "dependabot-reviewer")]
#[command(about = "Mass rebase or recreate Dependabot PRs across repositories", long_about = None)]
pub struct Cli {
    /// GitHub organizations to search (can be used multiple times).
    #[arg(short, long, default_values = ["actix", "robjtede", "x52dev"])]
    pub org: Vec<String>,

    /// Specific repository to process (owner/repo).
    #[arg(short, long)]
    pub repo: Option<String>,

    /// Require confirmation before commenting on each PR.
    #[arg(short, long)]
    pub confirm: bool,

    /// Dry run - show what would be done without actually commenting.
    #[arg(short, long)]
    pub dry_run: bool,

    /// Enable verbose debug logging.
    #[arg(short, long)]
    pub verbose: bool,

    /// Action to apply to PRs. If omitted, prompts interactively.
    #[arg(short, long, value_enum)]
    pub action: Option<Action>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Action {
    OpenInBrowser,
    ApproveMerge,
    Rebase,
    Recreate,
}
