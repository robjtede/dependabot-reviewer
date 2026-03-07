use std::collections::HashMap;

use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt as _;
use futures_util::StreamExt as _;
use octocrab::{models::StatusState, params::repos::Reference};

use super::App;
use crate::{
    error::AppError,
    github::{CiStatus, PrInfo},
};

impl App {
    pub(crate) async fn fetch_ci_status(&self, owner: &str, repo: &str, branch: &str) -> CiStatus {
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

    pub(crate) async fn fetch_dependabot_prs_for_repo(
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

    pub(crate) async fn aggregate_repos_with_counts(
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
}
