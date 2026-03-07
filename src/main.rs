extern crate serde_json;
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, FuzzySelect};
use serde_json::Value;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::process::Command;

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
}

impl App {
    fn new(cli: Cli) -> Result<Self> {
        let verbose = cli.verbose;
        Ok(Self { cli, verbose })
    }

    fn debug(&self, msg: &str) {
        if self.verbose {
            eprintln!("DEBUG: {}", msg);
        }
    }

    fn run_gh_json(&self, args: &[&str]) -> Result<Value> {
        self.debug(&format!("Running: gh {}", args.join(" ")));

        let output = Command::new("gh")
            .args(args)
            .output()
            .context("Failed to execute gh command. Is it installed and authenticated?")?;

        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            let cmd = format!("gh {}", args.join(" "));
            self.debug(&format!("Command failed: {}", cmd));
            self.debug(&format!("Error: {}", error));
            return Err(anyhow!("gh command failed: {}", error));
        }

        let stdout = String::from_utf8(output.stdout).context("gh output is not valid UTF-8")?;

        if stdout.trim().is_empty() {
            return Ok(Value::Array(vec![]));
        }

        serde_json::from_str(&stdout).context("Failed to parse gh JSON output")
    }

    fn fetch_dependabot_prs_for_repo(&self, repo: &str) -> Result<Vec<PrInfo>> {
        self.debug(&format!("Fetching PRs for {}", repo));

        let json = self
            .run_gh_json(&[
                "pr",
                "list",
                "--repo",
                repo,
                "--author",
                "dependabot[bot]",
                "--state",
                "open",
                "--json",
                "number,title,url",
            ])
            .map_err(|e| anyhow!("Failed to fetch PRs for {}: {}", repo, e))?;

        let mut prs = Vec::new();
        if let Value::Array(items) = json {
            for item in items {
                if let (Some(number), Some(title), Some(url)) = (
                    item.get("number").and_then(|v| v.as_u64()),
                    item.get("title").and_then(|v| v.as_str()),
                    item.get("url").and_then(|v| v.as_str()),
                ) {
                    prs.push(PrInfo {
                        number,
                        title: title.to_string(),
                        url: url.to_string(),
                    });
                }
            }
        }

        Ok(prs)
    }

    fn aggregate_repos_with_counts(&self) -> Result<HashMap<String, usize>> {
        self.debug("Aggregating repos with PR counts");

        let mut repo_counts: HashMap<String, usize> = HashMap::new();

        for org in &self.cli.org {
            self.debug(&format!("Searching organization: {}", org));

            let json = self
                .run_gh_json(&[
                    "search",
                    "prs",
                    "--owner",
                    org,
                    "--author",
                    "dependabot[bot]",
                    "--state",
                    "open",
                    "--json",
                    "repository",
                ])
                .map_err(|e| anyhow!("Failed to search PRs in {}: {}", org, e))?;

            if let Value::Array(items) = json {
                for item in items {
                    if let Some(repo_obj) = item.get("repository") {
                        if let Some(name_with_owner) =
                            repo_obj.get("nameWithOwner").and_then(|v| v.as_str())
                        {
                            *repo_counts.entry(name_with_owner.to_string()).or_insert(0) += 1;
                        }
                    }
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

    fn process_repository(&self, repo: &str) -> Result<()> {
        println!(
            "{} Processing {}",
            style("→").cyan(),
            style(repo).green().bold()
        );

        let prs = self.fetch_dependabot_prs_for_repo(repo)?;

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
                let output = Command::new("gh")
                    .args([
                        "pr",
                        "comment",
                        "--repo",
                        repo,
                        &pr.number.to_string(),
                        "--body",
                        "@dependabot rebase",
                    ])
                    .output()
                    .context("Failed to execute gh pr comment")?;

                if !output.status.success() {
                    let error = String::from_utf8_lossy(&output.stderr);
                    return Err(anyhow!("Failed to comment on PR #{}: {}", pr.number, error));
                }

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

    fn run(&self) -> Result<()> {
        self.debug("Starting dependabot-reviewer");

        let selected_repo = if let Some(repo) = &self.cli.repo {
            self.debug(&format!("Using specified repository: {}", repo));
            Some(repo.clone())
        } else {
            let repo_counts = self.aggregate_repos_with_counts()?;

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
            self.process_repository(&repo)?;
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    let app = App::new(cli)?;
    app.run()?;

    Ok(())
}
