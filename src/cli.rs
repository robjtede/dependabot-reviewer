use clap::{Parser, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "dependabot-reviewer")]
#[command(about = "Review and manage Dependabot PRs across repositories", long_about = None)]
pub struct Cli {
    /// GitHub organizations to search (can be used multiple times).
    #[arg(short, long)]
    pub org: Vec<String>,

    /// Persist the provided --org values as the default GitHub organizations.
    #[arg(long)]
    pub save_default_orgs: bool,

    /// Specific repository to process (owner/repo).
    #[arg(short, long)]
    pub repo: Option<String>,

    /// Require confirmation before commenting on each PR.
    #[arg(short, long)]
    pub confirm: bool,

    /// Dry run - show what would be done without performing the selected action.
    #[arg(short, long)]
    pub dry_run: bool,

    /// Attempt approve+merge even when CI is pending or failing.
    #[arg(long)]
    pub allow_non_passing_ci: bool,

    /// Get the GitHub token from `gh auth token`.
    #[arg(long)]
    pub use_gh_auth_token: bool,

    /// Enable verbose debug logging.
    #[arg(short, long)]
    pub verbose: bool,

    /// Action to apply to PRs. If omitted, prompts interactively.
    #[arg(short, long, value_enum)]
    pub action: Option<Action>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Action {
    OpenUnreviewedInBrowser,
    ApproveMerge,
    Close,
    Rebase,
    Recreate,
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{Action, Cli};

    #[test]
    fn parses_use_gh_auth_token() {
        let cli = Cli::try_parse_from(["dependabot-reviewer", "--use-gh-auth-token"])
            .expect("flag should parse");

        assert!(cli.use_gh_auth_token);
    }

    #[test]
    fn parses_close_action() {
        let cli = Cli::try_parse_from(["dependabot-reviewer", "--action", "close"])
            .expect("close action should parse");

        assert!(matches!(cli.action, Some(Action::Close)));
    }
}
