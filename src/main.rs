use anyhow::{anyhow, Context, Result};
use clap::Parser;
use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, FuzzySelect};
use octocrab::Octocrab;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::process::Command;
use std::sync::Arc;

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
    fn new(cli: Cli) -> Result<Self> {
        let verbose = cli.verbose;
        let mut token = std::env::var("GITHUB_TOKEN").ok();

        if token.is_none() {
            let gh_check = Command::new("gh").arg("--version").output();

            if gh_check.is_ok() {
                if std::io::stdin().is_terminal() {
                    let use_gh = Confirm::with_theme(&ColorfulTheme::default())
                        .with_prompt("GITHUB_TOKEN not found. Try to use 'gh auth token'?")
                        .default(true)
                        .interact()
                        .unwrap_or(false);

                    if use_gh {
                        let output = Command::new("gh")
                            .args(["auth", "token"])
                            .output()
                            .context("Failed to run 'gh auth token'")?;

                        if output.status.success() {
                            let t = String::from_utf8(output.stdout)?.trim().to_string();
                            if !t.is_empty() {
                                token = Some(t);
                            }
                        }
                    }
                }
            }
        }

        let mut builder = Octocrab::builder();
        if let Some(token) = token {
            builder = builder.personal_token(token);
        } else {
            return Err(anyhow!(
                "GITHUB_TOKEN is required. Please set it or authenticate with 'gh auth login'."
            ));
        }

        let octocrab = Arc::new(builder.build()?);
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

    async fn fetch_dependabot_prs_for_repo(&self, repo: &str) -> Result<Vec<PrInfo>> {
        self.debug(&format!("Fetching PRs for {}", repo));

        let (owner, repo_name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow!("Invalid repo format: {}", repo))?;

        let prs_page = self
            .octocrab
            .pulls(owner, repo_name)
            .list()
            .state(octocrab::params::State::Open)
            .send()
            .await
            .map_err(|e| anyhow!("Failed to fetch PRs for {}: {}", repo, e))?;

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

    async fn aggregate_repos_with_counts(&self) -> Result<HashMap<String, usize>> {
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
                .map_err(|e| anyhow!("Failed to search PRs in {}: {}", org, e))?;

            for issue in page.items {
                // The repository URL is usually something like https://api.github.com/repos/owner/repo
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
    ) -> Result<Option<String>> {
        // Build list: "repo (N PRs)"
        let mut items: Vec<String> = repo_counts
            .iter()
            .map(|(repo, count)| format!("{} ({} PRs)", repo, count))
            .collect();

        // Sort by count descending
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
            .context("Interactive selection failed")?;

        if selection >= items.len() {
            return Ok(None);
        }

        let selected = &items[selection];
        // Extract repo name: everything before " ("
        let repo = selected.split(" (").next().unwrap_or("").to_string();

        Ok(Some(repo))
    }

    async fn process_repository(&self, repo: &str) -> Result<()> {
        println!(
            "{} Processing {}",
            style("→").cyan(),
            style(repo).green().bold()
        );

        let prs = self.fetch_dependabot_prs_for_repo(repo).await?;

        if prs.is_empty() {
            println!("  No open Dependabot PRs found in {}", repo);
            return Ok(());
        }

        println!("  Found {} Dependabot PR(s):", prs.len());
        for pr in &prs {
            println!("    {}", pr.display());
        }
        println!();

        // Determine whether to comment on each PR, handling interactive vs non-interactive
        let should_comment = if self.cli.confirm {
            // Per-PR confirmation mode
            if !std::io::stdin().is_terminal() {
                return Err(anyhow!("Cannot use --confirm in non-interactive mode"));
            }
            let mut answers = Vec::new();

            for pr in &prs {
                let answer = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(&format!(
                        "Comment '@dependabot rebase' on PR #{}?",
                        pr.number
                    ))
                    .default(false)
                    .interact()
                    .unwrap_or(false);
                answers.push(answer);
            }

            answers
        } else {
            // No per-PR confirmation
            let all = prs.len();
            if std::io::stdin().is_terminal() {
                // Interactive: prompt once for all
                let proceed = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(&format!(
                        "Comment '@dependabot rebase' on all {} PR(s) in {}?",
                        all, repo
                    ))
                    .default(false)
                    .interact()
                    .context("Confirmation failed")?;
                vec![proceed; all]
            } else {
                // Non-interactive: auto-accept all
                vec![true; all]
            }
        };

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
                    .ok_or_else(|| anyhow!("Invalid repo format: {}", repo))?;

                self.octocrab
                    .issues(owner, repo_name)
                    .create_comment(pr.number, "@dependabot rebase")
                    .await
                    .context("Failed to comment on PR")?;

                println!(
                    "  {} Commented on PR #{}{}",
                    style("✓").green(),
                    pr.number,
                    style(format!(" ({})", repo)).dim()
                );
            }
        }

        Ok(())
    }

    async fn run(&self) -> Result<()> {
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

            // Check if we're in an interactive terminal
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                match self.select_repository_interactive(repo_counts)? {
                    Some(repo) => Some(repo),
                    None => {
                        println!("No repository selected. Exiting.");
                        return Ok(());
                    }
                }
            } else {
                // Non-interactive mode: print list and exit
                println!("Repositories with open Dependabot PRs:");
                let mut repos: Vec<_> = repo_counts.into_iter().collect();
                repos.sort_by(|(_, a), (_, b)| b.cmp(a)); // Sort by count descending
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

        if let Some(repo) = selected_repo {
            self.process_repository(&repo).await?;
        }

        println!();
        println!("{}", style("Done!").green().bold());

        if !self.cli.dry_run {
            println!("Dependabot will rebase each PR automatically.");
            println!("You can monitor the progress in the PRs.");
        } else {
            println!("Run without --dry-run to actually comment on PRs.");
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let app = App::new(cli)?;
    app.run().await?;

    Ok(())
}
