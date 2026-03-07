use clap::Parser;
use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, FuzzySelect};
use error_stack::{report, Result, ResultExt as _};
use octocrab::Octocrab;

use std::collections::HashMap;
use std::fmt;
use std::io::IsTerminal as _;
use std::process::Command;
use std::sync::Arc;

#[derive(Debug)]
pub enum AppError {
    Initialization,
    GitHubApi,
    Search,
    Comment,
    Interactive,
    InvalidInput,
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Initialization => write!(f, "Failed to initialize application"),
            AppError::GitHubApi => write!(f, "GitHub API error"),
            AppError::Search => write!(f, "Failed to search for PRs"),
            AppError::Comment => write!(f, "Failed to comment on PR"),
            AppError::Interactive => write!(f, "Interactive prompt failed"),
            AppError::InvalidInput => write!(f, "Invalid input provided"),
        }
    }
}

impl error_stack::Context for AppError {}

#[derive(Parser, Debug)]
#[command(name = "dependabot-reviewer")]
#[command(about = "Mass rebase Dependabot PRs across repositories", long_about = None)]
struct Cli {
    /// GitHub organizations to search (can be used multiple times)
    #[arg(short, long, default_value = "x52dev")]
    org: Vec<String>,

    /// Specific repository to process (owner/repo)
    #[arg(short, long)]
    repo: Option<String>,

    /// Require confirmation before commenting on each PR
    #[arg(short, long)]
    confirm: bool,

    /// Dry run - show what would be done without actually commenting
    #[arg(short, long)]
    dry_run: bool,

    /// Enable verbose debug logging
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug, Clone)]
struct PrInfo {
    number: u64,
    title: String,
    url: String,
}

impl PrInfo {
    fn display(&self) -> String {
        format!("#{}: {}", self.number, self.title)
    }
}

struct App {
    cli: Cli,
    verbose: bool,
    octocrab: Arc<Octocrab>,
}

impl App {
    fn new(cli: Cli) -> Result<Self, AppError> {
        let verbose = cli.verbose;
        let mut token = std::env::var("GITHUB_TOKEN").ok();

        if token.is_none() && !cli.dry_run {
            let gh_check = Command::new("gh").arg("--version").output();

            if gh_check.is_ok() && std::io::stdin().is_terminal() {
                let use_gh = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt("GITHUB_TOKEN not found. Try to use 'gh auth token'?")
                    .default(true)
                    .interact()
                    .map_err(|_| report!(AppError::Interactive))?;

                if use_gh {
                    let output = Command::new("gh")
                        .args(["auth", "token"])
                        .output()
                        .change_context(AppError::Initialization)
                        .attach_printable("Failed to run 'gh auth token'")?;

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
            return Err(report!(AppError::Initialization)).attach_printable(
                "GITHUB_TOKEN is required. Please set it or authenticate with 'gh auth login'.",
            );
        }

        let octocrab = Arc::new(
            builder
                .build()
                .change_context(AppError::Initialization)
                .attach_printable("Failed to build Octocrab client")?,
        );

        Ok(Self {
            cli,
            verbose,
            octocrab,
        })
    }

    fn debug(&self, msg: &str) {
        if self.verbose {
            eprintln!("DEBUG: {}", msg);
        }
    }

    async fn fetch_dependabot_prs_for_repo(&self, repo: &str) -> Result<Vec<PrInfo>, AppError> {
        self.debug(&format!("Fetching PRs for {}", repo));

        let (owner, repo_name) = repo
            .split_once('/')
            .ok_or_else(|| report!(AppError::InvalidInput))
            .attach_printable_lazy(|| format!("Invalid repo format: {}", repo))?;

        let prs_page = self
            .octocrab
            .pulls(owner, repo_name)
            .list()
            .state(octocrab::params::State::Open)
            .send()
            .await
            .change_context(AppError::GitHubApi)
            .attach_printable_lazy(|| format!("Failed to fetch PRs for {}", repo))?;

        let prs = prs_page
            .items
            .into_iter()
            .filter(|pr| {
                pr.user
                    .as_ref()
                    .map(|u| u.login == "dependabot[bot]")
                    .unwrap_or(false)
            })
            .map(|pr| PrInfo {
                number: pr.number,
                title: pr.title.unwrap_or_default(),
                url: pr.html_url.map(|u| u.to_string()).unwrap_or_default(),
            })
            .collect();

        Ok(prs)
    }

    async fn aggregate_repos_with_counts(&self) -> Result<HashMap<String, usize>, AppError> {
        self.debug("Aggregating repos with PR counts");

        let mut repo_counts: HashMap<String, usize> = HashMap::new();

        for org in &self.cli.org {
            self.debug(&format!("Searching organization: {}", org));

            let query = format!("org:{} author:dependabot[bot] is:pr is:open", org);
            let page = self
                .octocrab
                .search()
                .issues_and_pull_requests(&query)
                .send()
                .await
                .change_context(AppError::Search)
                .attach_printable_lazy(|| format!("Failed to search PRs in {}", org))?;

            for issue in page.items {
                let repo_url = &issue.repository_url;
                let path = repo_url.path();
                if let Some(name_with_owner) = path.strip_prefix("/repos/") {
                    *repo_counts.entry(name_with_owner.to_string()).or_insert(0) += 1;
                }
            }
        }

        Ok(repo_counts)
    }

    fn select_repository_interactive(
        &self,
        repo_counts: HashMap<String, usize>,
    ) -> Result<Option<String>, AppError> {
        let mut items: Vec<String> = repo_counts
            .iter()
            .map(|(repo, count)| format!("{} ({} PRs)", repo, count))
            .collect();

        items.sort_by(|a, b| {
            let count_a = a
                .rsplit(" (")
                .next()
                .and_then(|s| s.strip_suffix(" PRs)"))
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            let count_b = b
                .rsplit(" (")
                .next()
                .and_then(|s| s.strip_suffix(" PRs)"))
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            count_b.cmp(&count_a)
        });

        if items.is_empty() {
            return Ok(None);
        }

        println!("Repositories with open Dependabot PRs:");
        println!();

        let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
            .with_prompt("Select a repository")
            .items(&items)
            .interact()
            .change_context(AppError::Interactive)
            .attach_printable("Interactive selection failed")?;

        if selection >= items.len() {
            return Ok(None);
        }

        let selected = &items[selection];
        let repo = selected.split(" (").next().unwrap_or("").to_string();

        Ok(Some(repo))
    }

    async fn process_repository(&self, repo: &str) -> Result<bool, AppError> {
        println!(
            "{} Processing {}",
            style("→").cyan(),
            style(repo).green().bold()
        );

        let prs = self.fetch_dependabot_prs_for_repo(repo).await?;

        if prs.is_empty() {
            println!("  No open Dependabot PRs found in {}", repo);
            return Ok(false);
        }

        println!("  Found {} Dependabot PR(s):", prs.len());
        for pr in &prs {
            println!("    {}", pr.display());
        }
        println!();

        let should_comment = if self.cli.confirm {
            if !std::io::stdin().is_terminal() {
                return Err(report!(AppError::InvalidInput))
                    .attach_printable("Cannot use --confirm in non-interactive mode");
            }
            let mut answers = Vec::new();

            for pr in &prs {
                let answer = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(format!(
                        "Comment '@dependabot rebase' on PR #{}?",
                        pr.number
                    ))
                    .default(false)
                    .interact()
                    .map_err(|_| report!(AppError::Interactive))?;
                answers.push(answer);
            }

            answers
        } else {
            let all = prs.len();
            if std::io::stdin().is_terminal() {
                let proceed = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(format!(
                        "Comment '@dependabot rebase' on all {} PR(s) in {}?",
                        all, repo
                    ))
                    .default(false)
                    .interact()
                    .change_context(AppError::Interactive)
                    .attach_printable("Confirmation failed")?;
                vec![proceed; all]
            } else {
                vec![true; all]
            }
        };

        let mut commented = false;
        for (pr, should) in prs.iter().zip(should_comment.iter()) {
            if !should {
                self.debug(&format!("Skipping PR #{}", pr.number));
                continue;
            }

            if self.cli.dry_run {
                println!("  [DRY RUN] Would comment on PR #{}: {}", pr.number, pr.url);
            } else {
                self.debug(&format!("Commenting on PR #{}", pr.number));

                let (owner, repo_name) = repo
                    .split_once('/')
                    .ok_or_else(|| report!(AppError::InvalidInput))
                    .attach_printable_lazy(|| format!("Invalid repo format: {}", repo))?;

                self.octocrab
                    .issues(owner, repo_name)
                    .create_comment(pr.number, "@dependabot rebase")
                    .await
                    .change_context(AppError::Comment)
                    .attach_printable_lazy(|| format!("Failed to comment on PR #{}", pr.number))?;

                println!(
                    "  {} Commented on PR #{}{}",
                    style("✓").green(),
                    pr.number,
                    style(format!(" ({})", repo)).dim()
                );
                commented = true;
            }
        }

        Ok(commented)
    }

    async fn run(&self) -> Result<(), AppError> {
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

        let mut commented = false;
        if let Some(repo) = selected_repo {
            commented = self.process_repository(&repo).await?;
        }

        println!();
        println!("{}", style("Done!").green().bold());

        if self.cli.dry_run {
            println!("Run without --dry-run to actually comment on PRs.");
        } else if commented {
            println!("Dependabot will rebase each PR automatically.");
            println!("You can monitor the progress in the PRs.");
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    let cli = Cli::parse();

    let app = App::new(cli)?;
    app.run().await?;

    Ok(())
}
