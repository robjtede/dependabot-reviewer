use std::{collections::HashMap, io::IsTerminal as _, process::Command};

use clap::Parser;
use console::style;
use derive_more::Display;
use dialoguer::{theme::ColorfulTheme, Confirm, FuzzySelect, Select};
use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt;
use futures_util::{FutureExt, StreamExt as _};
use octocrab::{models::pulls::ReviewAction, params::pulls::MergeMethod, Octocrab};

#[derive(Debug, Display)]
pub enum AppError {
    #[display("Failed to initialize application")]
    Initialization,

    #[display("GitHub API error")]
    GitHubApi,

    #[display("Failed to search for PRs")]
    Search,

    #[display("Failed to comment on PR")]
    Comment,

    #[display("Interactive prompt failed")]
    Interactive,

    #[display("Invalid input provided")]
    InvalidInput,

    #[display("Action selection failed")]
    ActionSelection,
}

impl_more::impl_leaf_error!(AppError);

#[derive(Parser, Debug)]
#[command(name = "dependabot-reviewer")]
#[command(about = "Mass rebase or recreate Dependabot PRs across repositories", long_about = None)]
struct Cli {
    /// GitHub organizations to search (can be used multiple times).
    #[arg(short, long, default_values = ["actix", "robjtede", "x52dev"])]
    org: Vec<String>,

    /// Specific repository to process (owner/repo).
    #[arg(short, long)]
    repo: Option<String>,

    /// Require confirmation before commenting on each PR.
    #[arg(short, long)]
    confirm: bool,

    /// Dry run - show what would be done without actually commenting.
    #[arg(short, long)]
    dry_run: bool,

    /// Enable verbose debug logging.
    #[arg(short, long)]
    verbose: bool,

    /// Allow selecting actions via dialog
    #[arg(skip)]
    action: Option<Action>,

    /// Recreate PRs instead of rebasing them.
    #[arg(long)]
    recreate: bool,
}

#[derive(Debug, Clone)]
struct PrInfo {
    number: u64,
    title: String,
    url: String,
}

#[derive(Debug, Clone, Copy)]
enum Action {
    ApproveMerge,
    Rebase,
    Recreate,
}

impl PrInfo {
    fn display(&self) -> String {
        format!("#{}: {}", self.number, self.title)
    }
}

struct App {
    cli: Cli,
    octocrab: Octocrab,
}

impl App {
    fn new(cli: Cli) -> Result<Self, Report<AppError>> {
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

    fn debug(&self, msg: &str) {
        if self.cli.verbose {
            eprintln!("DEBUG: {}", msg);
        }
    }

    async fn fetch_dependabot_prs_for_repo(
        &self,
        repo: &str,
    ) -> Result<Vec<PrInfo>, Report<AppError>> {
        self.debug(&format!("Fetching PRs for {}", repo));

        let (owner, repo_name) = repo
            .split_once('/')
            .ok_or_else(|| Report::new(AppError::InvalidInput))
            .attach_with(|| format!("Invalid repo format: {}", repo))?;

        let prs_page = self
            .octocrab
            .pulls(owner, repo_name)
            .list()
            .state(octocrab::params::State::Open)
            .send()
            .await
            .change_context(AppError::GitHubApi)
            .attach_with(|| format!("Failed to fetch PRs for {}", repo))?;

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

    async fn aggregate_repos_with_counts(
        &self,
    ) -> Result<HashMap<String, usize>, Report<AppError>> {
        println!("Finding dependabot PRs for {} orgs", self.cli.org.len());

        let mut repo_counts = HashMap::new();
        let mut search_tasks = Vec::new();

        for org in &self.cli.org {
            let org = org.clone();
            let octocrab = self.octocrab.clone();
            let verbose = self.cli.verbose;

            search_tasks.push(async move {
                if verbose {
                    eprintln!("DEBUG: Searching organization: {}", org);
                }

                let query = format!("org:{} author:dependabot[bot] is:pr is:open", org);
                let page = octocrab
                    .search()
                    .issues_and_pull_requests(&query)
                    .send()
                    .await
                    .change_context(AppError::Search)
                    .attach_with(|| format!("Failed to search PRs in {}", org))?;

                Ok::<_, Report<AppError>>(page.items)
            });
        }

        let mut stream = futures_util::stream::iter(search_tasks).buffered_unordered(5);

        while let Some(result) = stream.next().await {
            let items = result?;
            for issue in items {
                let repo_url = &issue.repository_url;
                let path = repo_url.path();
                if let Some(name_with_owner) = path.strip_prefix("/repos/") {
                    let count = repo_counts.entry(name_with_owner.to_string()).or_insert(0);
                    *count += 1;
                }
            }
        }

        Ok(repo_counts)
    }

    fn select_repository_interactive(
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

    async fn process_repository(&self, repo: &str) -> Result<bool, Report<AppError>> {
        println!("Fetching PR details for {}", repo);

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

        // Show action dialog after listing PRs
        let action = {
            let items = vec!["Approve + Merge", "Rebase", "Recreate"];
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Choose action to apply to these PRs")
                .items(&items)
                .default(0)
                .interact()
                .change_context(AppError::ActionSelection)
                .attach("Action selection failed")?;
            match selection {
                0 => Action::ApproveMerge,
                1 => Action::Rebase,
                2 => Action::Recreate,
                _ => unreachable!(),
            }
        };

        let mut commented = false;
        let mut comment_tasks = Vec::new();
        let mut merge_tasks = Vec::new();

        for pr in &prs {
            if self.cli.dry_run {
                println!("  [DRY RUN] Would comment on PR #{}: {}", pr.number, pr.url);
            } else {
                let (owner, repo_name) = repo
                    .split_once('/')
                    .ok_or_else(|| Report::new(AppError::InvalidInput))
                    .attach_with(|| format!("Invalid repo format: {}", repo))?;

                let owner = owner.to_string();
                let repo_name = repo_name.to_string();
                let pr_number = pr.number;
                let octocrab = self.octocrab.clone();

                match action {
                    Action::Rebase => {
                        comment_tasks.push(
                            async move {
                                self.debug(&format!("Commenting on PR #{}", pr.number));

                                octocrab
                                    .issues(owner, repo_name)
                                    .create_comment(pr_number, "@dependabot rebase")
                                    .await
                                    .change_context(AppError::Comment)
                                    .attach(format!("Failed to comment on PR #{}", pr_number))?;

                                Ok::<_, Report<_>>(pr_number)
                            }
                            .boxed(),
                        );
                    }
                    Action::Recreate => {
                        comment_tasks.push(
                            async move {
                                self.debug(&format!("Commenting on PR #{}", pr.number));

                                octocrab
                                    .issues(owner, repo_name)
                                    .create_comment(pr_number, "@dependabot recreate")
                                    .await
                                    .change_context(AppError::Comment)
                                    .attach(format!("Failed to comment on PR #{}", pr_number))?;

                                Ok::<_, Report<_>>(pr_number)
                            }
                            .boxed(),
                        );
                    }
                    Action::ApproveMerge => {
                        merge_tasks.push(async move {
                            let repo = octocrab
                                .repos(&owner, &repo_name)
                                .get()
                                .await
                                .change_context(AppError::Comment)
                                .attach(format!("Failed to approve PR #{}", pr_number))?;

                            let merge_method =
                                match (repo.allow_merge_commit, repo.allow_squash_merge) {
                                    (Some(true), Some(true)) => MergeMethod::Merge,
                                    (Some(true), Some(false)) => MergeMethod::Merge,
                                    (Some(false), Some(true)) => MergeMethod::Squash,
                                    _ => {
                                        return Err(AppError::Comment).attach(format!(
                                            "No merge method available for PR #{}",
                                            pr_number
                                        ))
                                    }
                                };

                            self.debug(&format!("Approving PR #{}", pr.number));

                            let pulls = octocrab.pulls(owner, repo_name);

                            let pr = pulls
                                .get(pr_number)
                                .await
                                .change_context(AppError::Comment)
                                .attach(format!("Failed to approve PR #{}", pr_number))?;

                            let head_sha = pr.head.sha;

                            self.debug(&format!("Head commit: {head_sha}"));

                            #[expect(deprecated)] // no alternative yet
                            let pr = pulls.pull_number(pr_number);

                            pr.reviews()
                                .create_review(
                                    head_sha.clone(),
                                    "",
                                    ReviewAction::Approve,
                                    Vec::new(),
                                )
                                .await
                                .change_context(AppError::Comment)
                                .attach(format!("Failed to approve PR #{}", pr_number))?;

                            self.debug(&format!(
                                "Merging PR #{} using {:?}",
                                pr_number, merge_method
                            ));

                            pulls
                                .merge(pr_number)
                                .sha(head_sha)
                                .method(merge_method)
                                .send()
                                .await
                                .change_context(AppError::Comment)
                                .attach(format!("Failed to merge PR #{}", pr_number))?;

                            Ok::<_, Report<_>>(pr_number)
                        });
                    }
                }
            }
        }

        if !comment_tasks.is_empty() {
            let mut stream = futures_util::stream::iter(comment_tasks).buffered_unordered(5);

            while let Some(result) = stream.next().await {
                let pr_number = result?;
                println!(
                    "  {} Commented on PR #{}{}",
                    style("✓").green(),
                    pr_number,
                    style(format!(" ({})", repo)).dim()
                );
                commented = true;
            }
        }

        if !merge_tasks.is_empty() {
            let mut stream = futures_util::stream::iter(merge_tasks).buffered_unordered(5);

            while let Some(result) = stream.next().await {
                let pr_number = result?;
                println!(
                    "  {} Approved and merged PR #{}{}",
                    style("✓").green(),
                    pr_number,
                    style(format!(" ({})", repo)).dim()
                );
                commented = true;
            }
        }

        Ok(commented)
    }

    async fn run(&self) -> Result<(), Report<AppError>> {
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
            println!(
                "Dependabot will {} each PR automatically.",
                if self.cli.recreate {
                    "recreate"
                } else {
                    "rebase"
                }
            );
            println!("You can monitor the progress in the PRs.");
        }

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Report<AppError>> {
    let cli = Cli::parse();

    let app = App::new(cli)?;
    app.run().await?;

    Ok(())
}
