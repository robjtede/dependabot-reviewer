use clap::Parser as _;
use error_stack::Report;

use super::{approval::MergeInfo, review::ReviewItem};
use crate::{
    app::{
        async_merge::{AsyncMerge, MergeOperation, MergeOutcome},
        merge_results::wait_for_async_merge,
        App,
    },
    error::AppError,
    github::{CiStatus, PrInfo},
};

pub(super) fn conflicted_pr(mergeable: &str, state: &str) -> String {
    format!(
        r#"{{"id":1,"number":12,"url":"https://example.com/pr/12","head":{{"ref":"dependabot/test","sha":"head"}},"base":{{"ref":"main","sha":"base"}},"mergeable":{mergeable},"state":"{state}","merged":false}}"#
    )
}

pub(super) fn merge_info() -> MergeInfo {
    MergeInfo {
        repo: "example/repo".to_owned(),
        owner: "example".to_owned(),
        repo_name: "repo".to_owned(),
        pr_number: 12,
        url: "https://example.com/pr/12".to_owned(),
        base_ref_name: "main".to_owned(),
        ci_status: CiStatus::Passing,
        dep_update: None,
        previously_reviewed: false,
    }
}

pub(super) async fn rebase_test_client(
    responses: Vec<(u16, String)>,
) -> (octocrab::Octocrab, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let address = listener.local_addr().expect("test address");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();

        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = Vec::new();
            let mut buffer = [0; 1024];

            loop {
                let count = socket.read(&mut buffer).await.expect("read request");
                assert_ne!(count, 0, "request ended early");
                request.extend_from_slice(buffer.get(..count).expect("received bytes"));

                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers =
                        String::from_utf8_lossy(request.get(..end).expect("request headers"));
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().expect("body length"))
                        })
                        .unwrap_or(0);

                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }

            requests.push(String::from_utf8(request).expect("UTF-8 request"));
            let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        }

        requests
    });
    let octocrab = octocrab::Octocrab::builder()
        .base_uri(format!("http://{address}"))
        .expect("test URI")
        .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
        .build()
        .expect("test client");

    (octocrab, server)
}

pub(super) async fn merge_test_error(octocrab: &octocrab::Octocrab) -> Report<AppError> {
    let response = octocrab._get("/merge-error").await.expect("error response");
    let error = octocrab::map_github_error(response)
        .await
        .expect_err("merge failure");

    Report::new(error).change_context(AppError::ApproveMerge)
}

pub(super) fn merge_test_app(octocrab: octocrab::Octocrab) -> App {
    App {
        cli: crate::cli::Cli::try_parse_from(["dependabot-reviewer", "--repo", "example/repo"])
            .expect("test CLI"),
        octocrab,
    }
}

pub(super) async fn test_merge_request(
    octocrab: &octocrab::Octocrab,
    owner: &str,
    repo: &str,
    number: u64,
    sha: &str,
    operation: MergeOperation,
) -> Result<MergeOutcome, Report<AppError>> {
    let status = AsyncMerge::new(octocrab)
        .start(owner, repo, number, sha, operation)
        .await?;

    wait_for_async_merge(octocrab, status, |_| {}, std::future::pending()).await
}

pub(super) fn pending_merge(method: &str, action: &str, sha: &str, bypass_rules: bool) -> String {
    format!(
        r#"{{"status":"pending","details":{{"uuid":"request-id","merge_method":"{method}","merge_action":"{action}","expected_head_sha":"{sha}","bypass_rules":{bypass_rules}}}}}"#
    )
}

pub(super) fn queue_inspection(uses_queue: bool, queued: bool, auto_merge: bool) -> String {
    let queue = if uses_queue {
        r#"{"id":"queue"}"#
    } else {
        "null"
    };
    let entry = if queued { r#"{"id":"entry"}"# } else { "null" };
    let auto_merge = if auto_merge {
        r#"{"enabledAt":"2026-10-02T12:00:00Z"}"#
    } else {
        "null"
    };

    format!(
        r#"{{"data":{{"repository":{{"mergeQueue":{queue},"pullRequest":{{"id":"pull-request-id","headRefOid":"head","mergeQueueEntry":{entry},"autoMergeRequest":{auto_merge}}}}}}}}}"#
    )
}

pub(super) fn approval_review() -> String {
    r#"{"id":1,"node_id":"review","html_url":"https://example.com/pr/12#review"}"#.to_owned()
}

pub(super) fn actions_review_item() -> ReviewItem {
    let mut item = review_item(
        12,
        "Bump the actions group with 3 updates",
        CiStatus::Passing,
    );
    item.pr.head_ref_name = "dependabot/github_actions/actions-group".to_owned();
    item.pr.base_ref_name = "release/1.x".to_owned();
    item
}

pub(super) fn review_item(number: u64, title: &str, ci_status: CiStatus) -> ReviewItem {
    ReviewItem {
        repo: "example/repo".to_string(),
        owner: "example".to_string(),
        repo_name: "repo".to_string(),
        pr: PrInfo {
            number,
            title: title.to_string(),
            url: format!("https://github.com/example/repo/pull/{}", number),
            api_url: format!("https://api.github.com/repos/example/repo/pulls/{number}"),
            base_ref_name: "main".to_string(),
            head_ref_name: "dependabot/cargo/tokio-1.1.0".to_string(),
            ci_status,
            dep_update: None,
        },
        actions_lock_check: None,
    }
}
