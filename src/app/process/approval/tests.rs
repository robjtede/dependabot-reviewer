use std::{assert_matches, time::Duration};

use super::*;
use crate::app::{
    merge_results::{fetch_merge_progress, monitor_merge_results, MergeWaitCancelled},
    process::test_support::*,
};

fn queue_snapshot(uses_queue: bool, queued: bool) -> String {
    let queue = if uses_queue {
        r#"{"id":"queue-id"}"#
    } else {
        "null"
    };
    let entry = if queued {
        r#"{"id":"entry-id"}"#
    } else {
        "null"
    };

    format!(
        r#"{{"data":{{"repository":{{"mergeQueue":{queue},"pullRequest":{{"id":"pull-request-id","headRefOid":"inspected-head","mergeQueueEntry":{entry},"autoMergeRequest":null}}}}}}}}"#
    )
}

fn approval_review() -> String {
    r#"{"id":1,"node_id":"review-id","html_url":"https://example.com/review/1"}"#.to_owned()
}

#[tokio::test]
async fn approval_completes_direct_merge_with_the_head_fetched_after_approval() {
    let (octocrab, server) = rebase_test_client(vec![
        (200, queue_snapshot(false, false)),
        (200, conflicted_pr("true", "open")),
        (200, approval_review()),
        (
            200,
            conflicted_pr("true", "open").replace(r#""sha":"head""#, r#""sha":"fresh-head""#),
        ),
        (
            200,
            r#"{"status":"merged","details":{"sha":"merged-head"}}"#.to_owned(),
        ),
    ])
    .await;
    let app = merge_test_app(octocrab);

    let outcome = app
        .approve_and_merge(&merge_info(), (MergeMethod::Squash, false), false, None)
        .await
        .expect("approved and merged");

    assert_matches!(outcome, ApprovalOutcome::Completed);

    let requests = server.await.expect("test server");
    let approval = requests.get(2).expect("approval request");
    let merge = requests.last().expect("merge request");

    assert_eq!(requests.len(), 5);
    assert!(approval.starts_with("POST /repos/example/repo/pulls/12/reviews "));
    assert!(approval.contains(r#""commit_id":"head""#));
    assert!(approval.contains(r#""event":"APPROVE""#));
    assert!(merge.starts_with("PUT /repos/example/repo/pulls/12/merge-async "));
    assert!(merge.contains(r#""sha":"fresh-head""#));
    assert!(merge.contains(r#""merge_method":"squash""#));
}

#[tokio::test]
async fn approval_watches_an_already_queued_pull_request_without_submitting_again() {
    let (octocrab, server) = rebase_test_client(vec![(200, queue_snapshot(true, true))]).await;
    let app = merge_test_app(octocrab);
    let mut info = merge_info();
    info.ci_status = crate::github::CiStatus::Pending;

    let outcome = app
        .approve_and_merge(&info, (MergeMethod::Merge, false), false, None)
        .await
        .expect("already queued");

    assert_matches!(outcome, ApprovalOutcome::WatchForMerge);
    assert_eq!(server.await.expect("test server").len(), 1);
}

#[tokio::test]
async fn pending_ci_without_auto_merge_returns_skipped_after_approval() {
    let (octocrab, server) = rebase_test_client(vec![
        (200, queue_snapshot(false, false)),
        (200, conflicted_pr("true", "open")),
        (200, approval_review()),
    ])
    .await;
    let app = merge_test_app(octocrab);
    let mut info = merge_info();
    info.ci_status = crate::github::CiStatus::Pending;

    let outcome = app
        .approve_and_merge(&info, (MergeMethod::Merge, false), false, None)
        .await
        .expect("pending merge skipped");

    assert_matches!(outcome, ApprovalOutcome::Skipped);

    let requests = server.await.expect("test server");

    assert_eq!(requests.len(), 3);
    assert!(requests
        .last()
        .expect("approval request")
        .starts_with("POST /repos/example/repo/pulls/12/reviews "));
}

#[tokio::test]
async fn direct_merge_uses_async_api_and_waits_for_completion() {
    let pending = r#"{"status":"pending","details":{"uuid":"request-id","merge_method":"squash","merge_action":"direct_merge","expected_head_sha":"head","bypass_rules":false}}"#;
    let (octocrab, server) = rebase_test_client(vec![
        (200, conflicted_pr("true", "open")),
        (202, pending.to_owned()),
        (200, pending.to_owned()),
        (
            200,
            r#"{"status":"merged","details":{"sha":"merged-head"}}"#.to_owned(),
        ),
    ])
    .await;
    let app = merge_test_app(octocrab);

    tokio::time::timeout(
        Duration::from_secs(15),
        app.direct_merge_pull_request("example", "repo", 12, MergeMethod::Squash, None),
    )
    .await
    .expect("merge completed within the test deadline")
    .expect("async merge completed");

    let requests = server.await.expect("test server");
    let submit = requests.get(1).expect("merge request");

    assert!(submit.starts_with("PUT /repos/example/repo/pulls/12/merge-async "));
    assert!(requests.iter().skip(1).all(|request| {
        let versions = request
            .lines()
            .filter(|line| {
                line.to_ascii_lowercase()
                    .starts_with("x-github-api-version:")
            })
            .collect::<Vec<_>>();

        versions == ["x-github-api-version: 2026-03-10"]
    }));
    assert!(submit.contains(r#""sha":"head""#));
    assert!(submit.contains(r#""merge_method":"squash""#));
    assert!(submit.contains(r#""merge_action":"direct_merge""#));
    assert!(submit.contains(r#""bypass_rules":false"#));
    assert!(requests.iter().skip(2).all(|request| {
        request.starts_with("GET /repos/example/repo/pulls/12/merge-async/request-id ")
    }));
}

#[tokio::test]
async fn queue_insertion_uses_async_api_without_a_merge_method() {
    let (octocrab, server) = rebase_test_client(vec![
        (202, pending_merge("merge", "merge_queue", "head", false)),
        (
            200,
            r#"{"status":"enqueued","details":{"message":"Queued"}}"#.to_owned(),
        ),
    ])
    .await;
    let app = merge_test_app(octocrab);

    let outcome = app
        .enqueue_pull_request("example", "repo", 12, "head", None)
        .await
        .expect("queue insertion");

    assert_matches!(outcome, EnqueuePullRequestOutcome::Queued);

    let requests = server.await.expect("test server");
    let submit = requests.first().expect("merge request");

    assert!(submit.starts_with("PUT /repos/example/repo/pulls/12/merge-async "));
    assert!(submit.contains(r#""merge_action":"merge_queue""#));
    assert!(submit.contains(r#""sha":"head""#));
    assert!(submit.contains(r#""bypass_rules":false"#));
    assert!(!submit.contains("merge_method"));
    assert_eq!(requests.len(), 2);
}

#[tokio::test]
async fn queue_insertion_preserves_auto_merge_fallback_for_expected_checks() {
    let (octocrab, server) = rebase_test_client(vec![
        (202, pending_merge("merge", "merge_queue", "head", false)),
        (200, r#"{"status":"failed","details":{"message":"Pull request 4 of 4 required status checks are expected."}}"#.to_owned()),
        (200, r#"{"data":{"enablePullRequestAutoMerge":{"pullRequest":{"id":"pull-request-id"}}}}"#.to_owned()),
    ])
    .await;
    let app = merge_test_app(octocrab);

    let outcome = app
        .enqueue_pull_request("example", "repo", 12, "head", None)
        .await
        .expect("required checks result");

    assert_matches!(outcome, EnqueuePullRequestOutcome::AwaitingRequiredChecks);

    app.enable_auto_merge_for_pull_request("pull-request-id", "head", MergeMethod::Merge)
        .await
        .expect("auto-merge enabled");

    let requests = server.await.expect("test server");
    let auto_merge = requests.last().expect("auto-merge request");

    assert!(auto_merge.starts_with("POST /graphql "));
    assert!(auto_merge.contains("enablePullRequestAutoMerge"));
    assert!(auto_merge.contains(r#""expectedHeadOid":"head""#));
}

#[tokio::test]
async fn queue_insertion_distinguishes_already_merged_pull_requests() {
    let (octocrab, server) = rebase_test_client(vec![(
        200,
        r#"{"status":"merged","details":{"sha":"merged-head"}}"#.to_owned(),
    )])
    .await;
    let app = merge_test_app(octocrab);

    let outcome = app
        .enqueue_pull_request("example", "repo", 12, "head", None)
        .await
        .expect("already merged");

    assert_matches!(outcome, EnqueuePullRequestOutcome::Merged);
    assert_eq!(server.await.expect("test server").len(), 1);
}

#[tokio::test]
async fn direct_merge_does_not_report_queued_as_merged() {
    let (octocrab, server) = rebase_test_client(vec![
        (200, conflicted_pr("true", "open")),
        (
            200,
            r#"{"status":"enqueued","details":{"message":"Already queued"}}"#.to_owned(),
        ),
    ])
    .await;
    let app = merge_test_app(octocrab);

    let error = app
        .direct_merge_pull_request("example", "repo", 12, MergeMethod::Merge, None)
        .await
        .expect_err("queue insertion is not a completed direct merge");

    assert!(format!("{error:?}").contains("direct merge is not complete"));
    assert_eq!(server.await.expect("test server").len(), 2);
}

#[tokio::test]
async fn async_merge_follows_a_matching_existing_request() {
    let (octocrab, server) = rebase_test_client(vec![
        (409, pending_merge("squash", "direct_merge", "head", false)),
        (
            200,
            r#"{"status":"merged","details":{"sha":"merged-head"}}"#.to_owned(),
        ),
    ])
    .await;

    let outcome = test_merge_request(
        &octocrab,
        "example",
        "repo",
        12,
        "head",
        MergeOperation::Direct(MergeMethod::Squash),
    )
    .await
    .expect("existing merge completed");

    assert_eq!(outcome, MergeOutcome::Merged);

    let requests = server.await.expect("test server");

    assert_eq!(requests.len(), 2);
    assert!(requests
        .last()
        .expect("poll request")
        .starts_with("GET /repos/example/repo/pulls/12/merge-async/request-id "));
}

#[tokio::test]
async fn submitting_an_async_merge_returns_before_polling() {
    let (octocrab, server) = rebase_test_client(vec![(
        202,
        pending_merge("merge", "direct_merge", "head", false),
    )])
    .await;

    let status = AsyncMerge::new(&octocrab)
        .start(
            "example",
            "repo",
            12,
            "head",
            MergeOperation::Direct(MergeMethod::Merge),
        )
        .await
        .expect("accepted merge request");

    assert_matches!(status, crate::app::async_merge::MergeStatus::Pending(_));
    assert_eq!(server.await.expect("test server").len(), 1);
}

#[tokio::test]
async fn temporary_poll_errors_retry_without_resubmitting_the_merge() {
    let (octocrab, server) = rebase_test_client(vec![
        (202, pending_merge("merge", "direct_merge", "head", false)),
        (503, r#"{"message":"Service unavailable"}"#.to_owned()),
        (200, r#"{"status":"merged","details":{}}"#.to_owned()),
    ])
    .await;
    let status = AsyncMerge::new(&octocrab)
        .start(
            "example",
            "repo",
            12,
            "head",
            MergeOperation::Direct(MergeMethod::Merge),
        )
        .await
        .expect("accepted request");
    let mut messages = Vec::new();

    let outcome = wait_for_async_merge(
        &octocrab,
        status,
        |message| messages.push(message.to_owned()),
        std::future::pending(),
    )
    .await
    .expect("confirmed merge");

    assert_eq!(outcome, MergeOutcome::Merged);
    assert_eq!(
        messages,
        [
            "Merging",
            "Status unavailable; retrying (Ctrl+C to stop waiting)"
        ]
    );

    let requests = server.await.expect("test server");
    assert_eq!(requests.len(), 3);
    assert!(requests
        .iter()
        .skip(1)
        .all(|request| request
            .starts_with("GET /repos/example/repo/pulls/12/merge-async/request-id ")));
}

#[tokio::test]
async fn cancelling_an_async_merge_only_stops_the_local_wait() {
    let (octocrab, server) = rebase_test_client(vec![(
        202,
        pending_merge("merge", "merge_queue", "head", false),
    )])
    .await;
    let status = AsyncMerge::new(&octocrab)
        .start("example", "repo", 12, "head", MergeOperation::Queue)
        .await
        .expect("accepted request");

    let error = wait_for_async_merge(&octocrab, status, |_| {}, async { Ok(()) })
        .await
        .expect_err("cancelled wait");

    assert!(error.downcast_ref::<MergeWaitCancelled>().is_some());
    assert_eq!(server.await.expect("test server").len(), 1);
}

#[tokio::test]
async fn results_screen_polls_ci_queue_and_the_actual_merge() {
    let snapshot = |state, ci, queue| {
        format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{"state":"{state}","mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED","mergeQueueEntry":{queue},"autoMergeRequest":{{"enabledAt":"2026-10-02T12:00:00Z"}},"commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{{"state":"{ci}"}}}}}}]}}}}}}}}}}"#
        )
    };
    let (octocrab, server) = rebase_test_client(vec![
        (200, snapshot("OPEN", "PENDING", "null")),
        (
            200,
            snapshot(
                "OPEN",
                "SUCCESS",
                r#"{"position":2,"state":"AWAITING_CHECKS"}"#,
            ),
        ),
        (200, snapshot("MERGED", "SUCCESS", "null")),
    ])
    .await;
    let info = merge_info();
    let mut messages = Vec::new();

    let results = monitor_merge_results(
        &[&info],
        async |info| {
            fetch_merge_progress(&octocrab, &info.owner, &info.repo_name, info.pr_number).await
        },
        |_, progress| messages.push(progress.message().to_owned()),
        async |_, error| error,
        std::future::pending(),
    )
    .await;

    assert!(results.failures.is_empty());
    assert!(results.cancelled.is_empty());
    assert_eq!(
        messages,
        [
            "Waiting for CI to pass",
            "In merge queue (position 2); waiting for queue CI to pass",
            "Merged"
        ]
    );

    let requests = server.await.expect("test server");
    assert_eq!(requests.len(), 3);
    assert!(requests
        .iter()
        .all(|request| request.starts_with("POST /graphql ")
            && request.contains("statusCheckRollup")
            && request.contains("mergeQueueEntry { position state }")));
}

#[tokio::test]
async fn results_screen_keeps_fetch_errors_distinct_from_merge_failures() {
    for (status, message, retryable) in [
        (403, "API rate limit exceeded", true),
        (403, "Resource not accessible", false),
        (503, "Service unavailable", true),
    ] {
        let (octocrab, server) =
            rebase_test_client(vec![(status, format!(r#"{{"message":"{message}"}}"#))]).await;

        let error = fetch_merge_progress(&octocrab, "example", "repo", 12)
            .await
            .expect_err("unavailable status");

        assert_matches!(
            error.downcast_ref::<AsyncMergeError>(),
            Some(AsyncMergeError::Unconfirmed)
        );
        assert_eq!(
            crate::app::merge_results::retryable_poll_error(&error),
            retryable
        );
        assert_eq!(server.await.expect("test server").len(), 1);
    }
}

#[tokio::test]
async fn async_merge_rejects_existing_requests_with_different_options() {
    for (method, action, sha, bypass) in [
        ("merge", "direct_merge", "head", false),
        ("squash", "merge_queue", "head", false),
        ("squash", "direct_merge", "other-head", false),
        ("squash", "direct_merge", "head", true),
    ] {
        let (octocrab, server) =
            rebase_test_client(vec![(409, pending_merge(method, action, sha, bypass))]).await;

        let error = test_merge_request(
            &octocrab,
            "example",
            "repo",
            12,
            "head",
            MergeOperation::Direct(MergeMethod::Squash),
        )
        .await
        .expect_err("different merge options");

        assert_matches!(
            error.downcast_ref::<AsyncMergeError>(),
            Some(AsyncMergeError::Unconfirmed)
        );
        assert_eq!(server.await.expect("test server").len(), 1);
    }
}

#[tokio::test]
async fn direct_merge_does_not_resubmit_after_a_poll_failure() {
    let (octocrab, server) = rebase_test_client(vec![
        (200, conflicted_pr("true", "open")),
        (202, pending_merge("merge", "direct_merge", "head", false)),
        (403, r#"{"message":"Resource not accessible"}"#.to_owned()),
    ])
    .await;
    let app = merge_test_app(octocrab);

    let error = app
        .direct_merge_pull_request("example", "repo", 12, MergeMethod::Merge, None)
        .await
        .expect_err("unconfirmed merge result");

    assert_matches!(
        error.downcast_ref::<AsyncMergeError>(),
        Some(AsyncMergeError::Unconfirmed)
    );
    assert!(format!("{error:?}").contains("Resource not accessible"));
    assert_eq!(server.await.expect("test server").len(), 3);
}

#[tokio::test]
async fn async_merge_preserves_immediate_failed_results() {
    let (octocrab, server) = rebase_test_client(vec![(
        400,
        r#"{"status":"failed","details":{"message":"Pull request is still a draft"}}"#.to_owned(),
    )])
    .await;

    let error = test_merge_request(
        &octocrab,
        "example",
        "repo",
        12,
        "head",
        MergeOperation::Queue,
    )
    .await
    .expect_err("draft pull request");

    assert_matches!(error.downcast_ref::<AsyncMergeError>(), Some(AsyncMergeError::Failed { message }) if message == "Pull request is still a draft");
    assert_eq!(server.await.expect("test server").len(), 1);
}

#[tokio::test]
async fn queue_insertion_does_not_enable_auto_merge_for_other_failures() {
    let (octocrab, server) = rebase_test_client(vec![
        (202, pending_merge("merge", "merge_queue", "head", false)),
        (
            200,
            r#"{"status":"failed","details":{"message":"Required review is missing"}}"#.to_owned(),
        ),
    ])
    .await;
    let app = merge_test_app(octocrab);

    let error = app
        .enqueue_pull_request("example", "repo", 12, "head", None)
        .await
        .expect_err("required review failure");

    assert!(format!("{error:?}").contains("Required review is missing"));
    assert_eq!(server.await.expect("test server").len(), 2);
}

#[test]
fn classifies_required_checks_expected_errors() {
    assert!(messages_are_awaiting_required_checks([
        "Pull request 4 of 4 required status checks are expected."
    ]));
}

#[test]
fn rejects_unrelated_or_empty_required_check_errors() {
    assert!(!messages_are_awaiting_required_checks([
        "Pull request is not mergeable"
    ]));
    assert!(!messages_are_awaiting_required_checks([]));
}
