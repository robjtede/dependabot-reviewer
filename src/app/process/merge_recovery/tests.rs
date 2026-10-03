use std::assert_matches;

use octocrab::params::pulls::MergeMethod;

use super::{super::test_support::*, *};
use crate::app::async_merge::MergeOperation;

#[derive(serde::Serialize)]
struct GraphqlRequest<T> {
    query: &'static str,
    variables: T,
}
#[tokio::test]
async fn conflict_rebase_handles_async_merge_failures() {
    let (octocrab, server) = rebase_test_client(vec![
        (202, pending_merge("merge", "direct_merge", "head", false)),
        (
            200,
            r#"{"status":"failed","details":{"message":"Pull Request has merge conflicts"}}"#
                .to_owned(),
        ),
        (200, conflicted_pr("false", "open")),
    ])
    .await;
    let error = test_merge_request(
        &octocrab,
        "example",
        "repo",
        12,
        "head",
        MergeOperation::Direct(MergeMethod::Merge),
    )
    .await
    .expect_err("merge conflicts");
    let mut prompted = false;

    let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
        prompted = true;
        Ok(false)
    })
    .await
    .expect("declined rebase");

    assert!(prompted);
    assert!(!requested);
    assert_eq!(server.await.expect("test server").len(), 3);
}

#[tokio::test]
async fn conflict_rebase_posts_only_after_confirmation() {
    let (octocrab, server) = rebase_test_client(vec![
        (405, r#"{"message":"Pull Request is not mergeable"}"#.to_owned()),
        (200, conflicted_pr("false", "open")),
        (201, r#"{"id":1,"node_id":"comment","url":"https://example.com/comment","html_url":"https://example.com/comment","user":{"login":"tester","id":1,"node_id":"user","gravatar_id":"","type":"User","site_admin":false,"avatar_url":"https://example.com","url":"https://example.com","html_url":"https://example.com","followers_url":"https://example.com","following_url":"https://example.com","gists_url":"https://example.com","starred_url":"https://example.com","subscriptions_url":"https://example.com","organizations_url":"https://example.com","repos_url":"https://example.com","events_url":"https://example.com","received_events_url":"https://example.com"},"created_at":"2026-09-17T00:00:00Z","body":"@dependabot rebase"}"#.to_owned()),
    ]).await;
    let error = merge_test_error(&octocrab).await;
    let mut prompted = false;

    let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
        prompted = true;
        Ok(true)
    })
    .await
    .expect("rebase request");

    assert!(requested, "conflicted PR should get a rebase request");
    assert!(prompted);
    let requests = server.await.expect("test server");
    assert_eq!(requests.len(), 3);
    assert!(requests
        .get(1)
        .expect("PR lookup")
        .starts_with("GET /repos/example/repo/pulls/12 "));
    let comment = requests.last().expect("comment request");
    assert!(comment.starts_with("POST /repos/example/repo/issues/12/comments "));
    assert!(comment.ends_with(r#"{"body":"@dependabot rebase"}"#));
}

#[tokio::test]
async fn conflict_rebase_does_not_comment_when_declined() {
    let (octocrab, server) = rebase_test_client(vec![
        (
            405,
            r#"{"message":"Pull Request is not mergeable"}"#.to_owned(),
        ),
        (200, conflicted_pr("false", "open")),
    ])
    .await;
    let error = merge_test_error(&octocrab).await;
    let mut prompted = false;

    let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
        prompted = true;
        Ok(false)
    })
    .await
    .expect("declined rebase");

    assert!(!requested);
    assert!(prompted);
    assert_eq!(server.await.expect("test server").len(), 2);
}

#[tokio::test]
async fn conflict_rebase_requires_current_open_conflicts() {
    for (mergeable, state) in [("true", "open"), ("null", "open"), ("false", "closed")] {
        let (octocrab, server) = rebase_test_client(vec![
            (
                405,
                r#"{"message":"Pull Request is not mergeable"}"#.to_owned(),
            ),
            (200, conflicted_pr(mergeable, state)),
        ])
        .await;
        let error = merge_test_error(&octocrab).await;
        let mut prompted = false;

        let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
            prompted = true;
            Ok(true)
        })
        .await
        .expect("ineligible PR");

        assert!(!requested);
        assert!(!prompted, "mergeable={mergeable}, state={state}");
        assert_eq!(server.await.expect("test server").len(), 2);
    }
}

#[tokio::test]
async fn conflict_rebase_ignores_unrelated_merge_failures() {
    for status in [403, 409, 429, 500] {
        let (octocrab, server) = rebase_test_client(vec![(
            status,
            r#"{"message":"Unrelated merge failure"}"#.to_owned(),
        )])
        .await;
        let error = merge_test_error(&octocrab).await;
        let mut prompted = false;

        let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
            prompted = true;
            Ok(true)
        })
        .await
        .expect("unrelated failure");

        assert!(!requested);
        assert!(!prompted, "HTTP {status}");
        assert_eq!(server.await.expect("test server").len(), 1);
    }
}

#[tokio::test]
async fn conflict_rebase_handles_graphql_merge_conflicts() {
    let (octocrab, server) = rebase_test_client(vec![
        (
            200,
            r#"{"errors":[{"message":"Pull Request has merge conflicts"}]}"#.to_owned(),
        ),
        (200, conflicted_pr("false", "open")),
    ])
    .await;
    let error = octocrab.graphql::<()>(&GraphqlRequest {
        query: "mutation { enqueuePullRequest(input: {pullRequestId: \"test\"}) { mergeQueueEntry { id } } }",
        variables: (),
    }).await.expect_err("GraphQL conflict");
    let error = Report::new(error).change_context(AppError::ApproveMerge);
    let mut prompted = false;

    let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
        prompted = true;
        Ok(false)
    })
    .await
    .expect("declined rebase");

    assert!(prompted);
    assert!(!requested);
    assert_eq!(server.await.expect("test server").len(), 2);
}

#[tokio::test]
async fn conflict_rebase_reports_comment_failures() {
    let (octocrab, server) = rebase_test_client(vec![
        (
            405,
            r#"{"message":"Pull Request is not mergeable"}"#.to_owned(),
        ),
        (200, conflicted_pr("false", "open")),
        (403, r#"{"message":"Resource not accessible"}"#.to_owned()),
    ])
    .await;
    let error = merge_test_error(&octocrab).await;

    let error = offer_conflict_rebase(&octocrab, &merge_info(), &error, || Ok(true))
        .await
        .expect_err("failed comment");

    assert_matches!(error.current_context(), AppError::Comment);
    assert!(format!("{error:?}").contains("Resource not accessible"));
    assert_eq!(server.await.expect("test server").len(), 3);
}
