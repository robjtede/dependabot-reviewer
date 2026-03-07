use std::{collections::HashMap, io::IsTerminal as _, process::Command, time::Duration};

use clap::{Parser, ValueEnum};
use console::style;
use derive_more::Display;
use dialoguer::{theme::ColorfulTheme, Confirm, FuzzySelect, Select};
use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt;
use futures_util::{FutureExt, StreamExt as _};
use octocrab::{
    models::{pulls::ReviewAction, StatusState},
    params::{pulls::MergeMethod, repos::Reference},
    Octocrab,
};

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

    /// Action to apply to PRs. If omitted, prompts interactively.
    #[arg(short, long, value_enum)]
    action: Option<Action>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CiStatus {
    Passing,
    Failing,
    Pending,
    Unknown,
}

impl std::fmt::Display for CiStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CiStatus::Passing => write!(f, "passing"),
            CiStatus::Failing => write!(f, "failing"),
            CiStatus::Pending => write!(f, "pending"),
            CiStatus::Unknown => write!(f, "unknown"),
        }
    }
}

impl CiStatus {
    fn icon(self) -> console::StyledObject<&'static str> {
        match self {
            CiStatus::Passing => style("✓").green(),
            CiStatus::Failing => style("✗").red(),
            CiStatus::Pending => style("●").yellow(),
            CiStatus::Unknown => style("○").dim(),
        }
    }

    fn is_mergeable(self) -> bool {
        matches!(self, CiStatus::Passing | CiStatus::Unknown)
    }
}

#[derive(Debug, Clone)]
struct PrInfo {
    number: u64,
    title: String,
    url: String,
    #[allow(dead_code)]
    head_sha: String,
    ci_status: CiStatus,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Action {
    ApproveMerge,
    Rebase,
    Recreate,
}

impl PrInfo {
    fn display(&self) -> String {
        format!(
            "{} #{}: {}\n        {}",
            self.ci_status.icon(),
            self.number,
            self.title,
            style(&self.url).dim()
        )
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

    async fn fetch_ci_status(&self, owner: &str, repo: &str, branch: &str) -> CiStatus {
        let reference = Reference::Branch(branch.to_string());

        let commits = self.octocrab.commits(owner, repo);
        let repos = self.octocrab.repos(owner, repo);

        let check_runs_fut = commits.associated_check_runs(reference.clone()).send();
        let combined_status_fut = repos.combined_status_for_ref(&reference);

        let (check_runs_result, status_result) = tokio::join!(check_runs_fut, combined_status_fut);

        let mut has_any_checks = false;
        let mut has_pending = false;
        let mut has_failure = false;

        // Evaluate check runs (GitHub Actions, etc.)
        if let Ok(list) = check_runs_result {
            for run in &list.check_runs {
                has_any_checks = true;
                match run.conclusion.as_deref() {
                    Some("success" | "neutral" | "skipped") => {}
                    Some(_) => has_failure = true,
                    None => has_pending = true, // still running
                }
            }
        }

        // Evaluate commit statuses (older CI systems)
        if let Ok(combined) = status_result {
            if combined.total_count > 0 {
                has_any_checks = true;
                match combined.state {
                    StatusState::Failure | StatusState::Error => has_failure = true,
                    StatusState::Pending => has_pending = true,
                    StatusState::Success => {}
                    _ => has_pending = true,
                }
            }
        }

        if !has_any_checks {
            CiStatus::Unknown
        } else if has_failure {
            CiStatus::Failing
        } else if has_pending {
            CiStatus::Pending
        } else {
            CiStatus::Passing
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

        let dependabot_prs: Vec<_> = prs_page
            .items
            .into_iter()
            .filter(|pr| {
                pr.user
                    .as_ref()
                    .map(|u| u.login == "dependabot[bot]")
                    .unwrap_or(false)
            })
            .collect();

        let ci_futures = dependabot_prs.into_iter().map(|pr| async move {
            let ci_status = self
                .fetch_ci_status(owner, repo_name, &pr.head.ref_field)
                .await;
            let head_sha = pr.head.sha;

            self.debug(&format!(
                "PR #{} head={} ci={}",
                pr.number,
                &head_sha[..8.min(head_sha.len())],
                ci_status,
            ));

            PrInfo {
                number: pr.number,
                title: pr.title.unwrap_or_default(),
                url: pr.html_url.map(|u| u.to_string()).unwrap_or_default(),
                head_sha,
                ci_status,
            }
        });

        let mut prs = Vec::new();
        let mut stream = futures_util::stream::iter(ci_futures).buffered_unordered(5);
        while let Some(pr) = stream.next().await {
            prs.push(pr);
        }

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

    async fn process_repository(&self, repo: &str) -> Result<Option<Action>, Report<AppError>> {
        println!("Fetching PR details for {}", repo);

        let prs = self.fetch_dependabot_prs_for_repo(repo).await?;

        if prs.is_empty() {
            println!("  No open Dependabot PRs found in {}", repo);
            return Ok(None);
        }

        println!("  Found {} Dependabot PR(s):", prs.len());
        for pr in &prs {
            println!("    {}", pr.display());
        }
        println!();

        // Use CLI-provided action or prompt interactively
        let action = if let Some(action) = self.cli.action {
            action
        } else {
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

        let mut performed_action = None;
        let mut comment_tasks = Vec::new();
        let mut merge_infos: Vec<(String, String, u64)> = Vec::new();

        for pr in &prs {
            if self.cli.dry_run {
                match action {
                    Action::ApproveMerge if !pr.ci_status.is_mergeable() => {
                        println!(
                            "  [DRY RUN] Would skip PR #{} (CI {}): {}",
                            pr.number, pr.ci_status, pr.url
                        );
                        continue;
                    }
                    Action::ApproveMerge => {
                        println!(
                            "  [DRY RUN] Would approve and merge PR #{}: {}",
                            pr.number, pr.url
                        );
                    }
                    _ => {
                        println!("  [DRY RUN] Would comment on PR #{}: {}", pr.number, pr.url);
                    }
                }
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
                        if pr.ci_status.is_mergeable() {
                            merge_infos.push((owner, repo_name, pr_number));
                        } else {
                            println!(
                                "  {} Skipping PR #{} (CI {}){}",
                                style("⊘").yellow(),
                                pr_number,
                                pr.ci_status,
                                style(format!(" ({})", repo)).dim()
                            );
                        }
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
                performed_action = Some(action);
            }
        }

        if !merge_infos.is_empty() {
            // Merges must run sequentially: each merge modifies the base branch,
            // which invalidates the head SHA of subsequent PRs. Running them in
            // parallel causes "Base branch was modified" errors.
            for (owner, repo_name, pr_number) in &merge_infos {
                let repo_info = self
                    .octocrab
                    .repos(owner, repo_name)
                    .get()
                    .await
                    .change_context(AppError::Comment)
                    .attach(format!("Failed to get repo info for PR #{}", pr_number))?;

                let merge_method =
                    match (repo_info.allow_merge_commit, repo_info.allow_squash_merge) {
                        (Some(true), Some(true)) => MergeMethod::Merge,
                        (Some(true), Some(false)) => MergeMethod::Merge,
                        (Some(false), Some(true)) => MergeMethod::Squash,
                        _ => {
                            return Err(Report::new(AppError::Comment)).attach(format!(
                                "No merge method available for PR #{}",
                                pr_number
                            ));
                        }
                    };

                const MAX_ATTEMPTS: u32 = 4;

                for attempt in 1..=MAX_ATTEMPTS {
                    let pulls = self.octocrab.pulls(owner, repo_name);

                    let pr_data = pulls
                        .get(*pr_number)
                        .await
                        .change_context(AppError::Comment)
                        .attach(format!("Failed to get PR #{}", pr_number))?;

                    let head_sha = pr_data.head.sha;

                    if attempt == 1 {
                        self.debug(&format!("Approving PR #{}", pr_number));

                        #[expect(deprecated)] // no alternative yet
                        let pr_handle = pulls.pull_number(*pr_number);

                        pr_handle
                            .reviews()
                            .create_review(head_sha.clone(), "", ReviewAction::Approve, Vec::new())
                            .await
                            .change_context(AppError::Comment)
                            .attach(format!("Failed to approve PR #{}", pr_number))?;
                    }

                    self.debug(&format!(
                        "Merging PR #{} using {:?} (head: {}, attempt {}/{})",
                        pr_number,
                        merge_method,
                        &head_sha[..8.min(head_sha.len())],
                        attempt,
                        MAX_ATTEMPTS
                    ));

                    match pulls
                        .merge(*pr_number)
                        .sha(head_sha)
                        .method(merge_method)
                        .send()
                        .await
                    {
                        Ok(_) => {
                            println!(
                                "  {} Approved and merged PR #{}{}",
                                style("✓").green(),
                                pr_number,
                                style(format!(" ({})", repo)).dim()
                            );
                            performed_action = Some(action);
                            break;
                        }
                        Err(e) if attempt < MAX_ATTEMPTS => {
                            let delay = Duration::from_secs(2u64.pow(attempt));
                            self.debug(&format!(
                                "Merge failed for PR #{}, retrying in {}s: {}",
                                pr_number,
                                delay.as_secs(),
                                e
                            ));
                            tokio::time::sleep(delay).await;
                        }
                        Err(e) => {
                            return Err(e).change_context(AppError::Comment).attach(format!(
                                "Failed to merge PR #{} after {} attempts",
                                pr_number, MAX_ATTEMPTS
                            ));
                        }
                    }
                }
            }
        }

        Ok(performed_action)
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

#[tokio::main]
async fn main() -> Result<(), Report<AppError>> {
    let cli = Cli::parse();

    let app = App::new(cli)?;
    app.run().await?;

    Ok(())
}
