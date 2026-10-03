use std::assert_matches;

use octocrab::params::pulls::MergeMethod;

use super::{super::test_support::*, *};
use crate::github::CiStatus;

fn settings() -> RepositoryMergeSettings {
    RepositoryMergeSettings {
        merge_method: MergeMethod::Squash,
        allow_auto_merge: true,
    }
}

#[tokio::test]
async fn approval_precedes_direct_merge_and_merge_uses_the_current_head() {
    let (octocrab, server) = rebase_test_client(vec![
        (200, queue_inspection(false, false, false)),
        (200, conflicted_pr("true", "open")),
        (201, approval_review()),
        (
            200,
            conflicted_pr("true", "open").replace("\"sha\":\"head\"", "\"sha\":\"new-head\""),
        ),
        (200, r#"{"status":"merged","details":{}}"#.to_owned()),
    ])
    .await;
    let app = merge_test_app(octocrab);
    let mut info = merge_info();
    info.ci_status = CiStatus::Unknown;

    let outcome = app
        .submit_approval(&info, settings(), false, None)
        .await
        .expect("approved and merged");

    assert_matches!(outcome, ApprovalOutcome::Merged);
    let requests = server.await.expect("test server");
    assert!(requests
        .get(2)
        .expect("approval")
        .starts_with("POST /repos/example/repo/pulls/12/reviews "));
    assert!(requests
        .get(2)
        .expect("approval")
        .contains(r#""event":"APPROVE""#));
    assert!(requests
        .get(4)
        .expect("merge")
        .starts_with("PUT /repos/example/repo/pulls/12/merge-async "));
    assert!(requests
        .get(4)
        .expect("merge")
        .contains(r#""sha":"new-head""#));
}

#[tokio::test]
async fn queue_submission_falls_back_to_auto_merge_for_expected_checks() {
    let (octocrab, server) = rebase_test_client(vec![
        (200, queue_inspection(true, false, false)),
        (200, conflicted_pr("true", "open")),
        (201, approval_review()),
        (400, r#"{"status":"failed","details":{"message":"Pull request required status checks are expected."}}"#.to_owned()),
        (200, r#"{"data":{"enablePullRequestAutoMerge":{"pullRequest":{"id":"pull-request-id"}}}}"#.to_owned()),
    ]).await;
    let app = merge_test_app(octocrab);

    let outcome = app
        .submit_approval(&merge_info(), settings(), false, None)
        .await
        .expect("auto-merge fallback");

    assert_matches!(outcome, ApprovalOutcome::AwaitingQueueChecks);
    let requests = server.await.expect("test server");
    assert!(requests
        .get(3)
        .expect("queue submission")
        .contains(r#""merge_action":"merge_queue""#));
    assert!(requests
        .last()
        .expect("auto-merge")
        .contains("enablePullRequestAutoMerge"));
    assert!(requests
        .last()
        .expect("auto-merge")
        .contains(r#""expectedHeadOid":"head""#));
}

#[tokio::test]
async fn already_queued_pull_requests_are_only_inspected() {
    let (octocrab, server) =
        rebase_test_client(vec![(200, queue_inspection(true, true, false))]).await;
    let app = merge_test_app(octocrab);

    let outcome = app
        .submit_approval(&merge_info(), settings(), false, None)
        .await
        .expect("existing queue entry");

    assert_matches!(outcome, ApprovalOutcome::AlreadyQueued);
    assert_eq!(server.await.expect("test server").len(), 1);
}

#[tokio::test]
async fn existing_auto_merge_refreshes_approval_only_outside_merge_queues() {
    for uses_queue in [true, false] {
        let mut responses = vec![(200, queue_inspection(uses_queue, false, true))];
        if !uses_queue {
            responses.extend([
                (200, conflicted_pr("true", "open")),
                (201, approval_review()),
            ]);
        }
        let (octocrab, server) = rebase_test_client(responses).await;
        let app = merge_test_app(octocrab);

        let outcome = app
            .submit_approval(&merge_info(), settings(), false, None)
            .await
            .expect("existing auto-merge");

        assert_matches!(outcome, ApprovalOutcome::AlreadyAutoMergeEnabled { uses_merge_queue } if uses_merge_queue == uses_queue);
        assert_eq!(
            server.await.expect("test server").len(),
            if uses_queue { 1 } else { 3 }
        );
    }
}
