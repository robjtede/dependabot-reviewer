use std::{collections::HashMap, time::SystemTime};

use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt as _;
use futures_util::StreamExt as _;
use octocrab::{models::StatusState, params::repos::Reference};

use super::App;
use crate::{
    error::AppError,
    github::{parse_dep_update, CiStatus, PrInfo},
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

        let prs_page = discovery_get::<octocrab::Page<octocrab::models::pulls::PullRequest>>(
            &self.octocrab,
            &format!("/repos/{owner}/{repo_name}/pulls?state=open"),
        )
        .await
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
            let head_ref = pr.head.ref_field.clone();
            let ci_status = self.fetch_ci_status(owner, repo_name, &head_ref).await;
            let head_sha = pr.head.sha;

            self.debug(&format!(
                "PR #{} head={} ci={}",
                pr.number,
                head_sha.get(..8).unwrap_or(&head_sha),
                ci_status,
            ));

            let title = pr.title.unwrap_or_default();
            let dep_update = parse_dep_update(&title, &head_ref);

            PrInfo {
                number: pr.number,
                title,
                url: pr.html_url.map(|u| u.to_string()).unwrap_or_default(),
                base_ref_name: pr.base.ref_field,
                head_ref_name: head_ref,
                ci_status,
                dep_update,
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
                let parameters =
                    serde_urlencoded::to_string([("q", query)]).change_context(AppError::Search)?;
                let page = discovery_get::<octocrab::Page<octocrab::models::issues::Issue>>(
                    &octocrab,
                    &format!("/search/issues?{parameters}"),
                )
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

async fn discovery_get<R: octocrab::FromResponse>(
    octocrab: &octocrab::Octocrab,
    route: &str,
) -> Result<R, Report<AppError>> {
    let result = async {
        let response = octocrab._get(route).await.change_context(AppError::GitHubApi)?;
        let status = response.status().as_u16();
        let header_number = |name| {
            response.headers().get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
        };
        let retry_after = header_number("retry-after");
        let remaining = header_number("x-ratelimit-remaining");
        let reset = header_number("x-ratelimit-reset");

        // Octocrab drops response headers when it converts a GitHub error.
        let response = octocrab::map_github_error(response).await.map_err(|error| {
            let rate_limit_message = match &error {
                octocrab::Error::GitHub { source, .. } => {
                    source.message.to_ascii_lowercase().contains("rate limit")
                }
                _ => false,
            };
            let rate_limited = status == 429
                || (status == 403
                    && (remaining == Some(0) || retry_after.is_some() || rate_limit_message));
            let mut report = Report::new(error)
                .change_context(AppError::GitHubApi)
                .attach(format!("GitHub returned HTTP {status}"));

            if rate_limited {
                let delay = retry_after.or_else(|| {
                    if remaining != Some(0) {
                        return None;
                    }

                    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).ok()?;
                    reset.map(|reset| reset.saturating_sub(now.as_secs()).saturating_add(1))
                });
                let guidance = match delay {
                    Some(1) => "GitHub rate limit reached. Retry after at least 1 second.".to_owned(),
                    Some(seconds) => format!("GitHub rate limit reached. Retry after at least {seconds} seconds."),
                    None => "GitHub rate limit reached. No retry time was provided. Wait at least 60 seconds before retrying; increase the wait if the limit persists.".to_owned(),
                };

                report = report.attach(guidance);
            } else if matches!(status, 401 | 403) {
                report = report.attach("Check that your GitHub token is valid and has access to the requested repositories and organizations.");
            }

            report
        })?;

        R::from_response(response).await.change_context(AppError::GitHubApi)
    }
    .await;

    result.attach("PR discovery failed. This does not mean there are no open Dependabot PRs.")
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    use super::*;

    async fn discover(
        status: u16,
        headers: &str,
        body: &str,
    ) -> Result<octocrab::Page<String>, Report<AppError>> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test address");
        let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = Vec::new();
            let mut buffer = [0; 1024];

            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).await.expect("read request");
                assert_ne!(count, 0, "request ended before headers");
                request.extend_from_slice(&buffer[..count]);
            }

            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        let octocrab = octocrab::Octocrab::builder()
            .base_uri(format!("http://{address}"))
            .expect("test URI")
            .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
            .build()
            .expect("test client");
        let result = discovery_get(&octocrab, "/search/issues?q=test").await;
        server.await.expect("test server");
        result
    }

    #[tokio::test]
    async fn discovery_reports_rate_limit_retry_delay() {
        let error = discover(
            429,
            "retry-after: 120\r\n",
            r#"{"message":"You have exceeded a secondary rate limit."}"#,
        )
        .await
        .expect_err("rate limit must fail discovery");
        let message = format!("{error:?}");

        assert!(
            message.contains("Retry after at least 120 seconds"),
            "{message}"
        );
        assert!(message.contains("This does not mean there are no open"));
    }

    #[tokio::test]
    async fn discovery_uses_primary_rate_limit_reset() {
        let error = discover(
            403,
            "x-ratelimit-remaining: 0\r\nx-ratelimit-reset: 0\r\n",
            r#"{"message":"API rate limit exceeded"}"#,
        )
        .await
        .expect_err("rate limit must fail discovery");
        let message = format!("{error:?}");

        assert!(
            message.contains("Retry after at least 1 second."),
            "{message}"
        );
    }

    #[tokio::test]
    async fn discovery_prefers_retry_after_over_reset() {
        let error = discover(
            403,
            "retry-after: 120\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset: 0\r\n",
            r#"{"message":"API rate limit exceeded"}"#,
        )
        .await
        .expect_err("rate limit must fail discovery");

        assert!(format!("{error:?}").contains("Retry after at least 120 seconds"));
    }

    #[tokio::test]
    async fn discovery_reports_secondary_rate_limit_without_valid_headers() {
        let error = discover(
            403,
            "retry-after: invalid\r\nx-ratelimit-remaining: 10\r\nx-ratelimit-reset: 0\r\n",
            r#"{"message":"You have exceeded a secondary rate limit."}"#,
        )
        .await
        .expect_err("secondary rate limit must fail discovery");

        assert!(format!("{error:?}").contains("Wait at least 60 seconds"));
    }

    #[tokio::test]
    async fn discovery_reports_rate_limits_without_timing_headers() {
        for status in [403, 429] {
            let error = discover(status, "", r#"{"message":"API rate limit exceeded"}"#)
                .await
                .expect_err("rate limit must fail discovery");

            assert!(format!("{error:?}").contains("Wait at least 60 seconds"));
        }
    }

    #[tokio::test]
    async fn discovery_does_not_label_permission_errors_as_rate_limits() {
        let error = discover(
            403,
            "x-ratelimit-remaining: 10\r\nx-ratelimit-reset: 0\r\n",
            r#"{"message":"Resource not accessible by personal access token"}"#,
        )
        .await
        .expect_err("permission error must fail discovery");
        let message = format!("{error:?}");

        assert!(message.contains("GitHub token is valid"));
        assert!(message.contains("Resource not accessible"));
        assert!(!message.contains("GitHub rate limit reached"));
        assert!(message.contains("PR discovery failed"));
    }

    #[tokio::test]
    async fn discovery_keeps_server_errors_distinct_from_empty_results() {
        let error = discover(500, "", r#"{"message":"Internal Server Error"}"#)
            .await
            .expect_err("server error must fail discovery");
        let message = format!("{error:?}");

        assert!(message.contains("HTTP 500"));
        assert!(message.contains("Internal Server Error"));
        assert!(message.contains("PR discovery failed"));
        assert!(!message.contains("GitHub rate limit reached"));
    }

    #[tokio::test]
    async fn discovery_preserves_repository_results() {
        let page = discover(200, "", r#"["first", "second"]"#)
            .await
            .expect("successful repository response");

        assert_eq!(page.items, ["first", "second"]);
    }

    #[tokio::test]
    async fn discovery_preserves_successful_empty_results() {
        let page = discover(
            200,
            "",
            r#"{"items":[],"total_count":0,"incomplete_results":false}"#,
        )
        .await
        .expect("successful empty search");

        assert!(page.items.is_empty());
    }
}
