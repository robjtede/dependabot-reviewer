use super::{super::test_support::*, *};
use crate::github::CiStatus;

#[tokio::test]
async fn successful_actions_send_the_expected_request_and_record_processed_prs() {
    for (action, expected) in [
        (Action::Close, "PATCH /repos/example/repo/pulls/12 "),
        (
            Action::Rebase,
            "POST /repos/example/repo/issues/12/comments ",
        ),
        (
            Action::Recreate,
            "POST /repos/example/repo/issues/12/comments ",
        ),
    ] {
        let body = if matches!(action, Action::Close) {
            conflicted_pr("true", "closed")
        } else {
            r#"{"id":1,"node_id":"comment","url":"https://example.com/comment","html_url":"https://example.com/comment","user":{"login":"tester","id":1,"node_id":"user","gravatar_id":"","type":"User","site_admin":false,"avatar_url":"https://example.com","url":"https://example.com","html_url":"https://example.com","followers_url":"https://example.com","following_url":"https://example.com","gists_url":"https://example.com","starred_url":"https://example.com","subscriptions_url":"https://example.com","organizations_url":"https://example.com","repos_url":"https://example.com","events_url":"https://example.com","received_events_url":"https://example.com"},"created_at":"2026-09-17T00:00:00Z","body":"@dependabot rebase"}"#.to_owned()
        };
        let (octocrab, server) = rebase_test_client(vec![(200, body)]).await;
        let app = merge_test_app(octocrab);
        let item = review_item(12, "Bump serde", CiStatus::Passing);
        let api_url = item.pr.api_url.clone();
        let mut processed = HashSet::new();

        let results = app
            .process_actions(action, &[item], &ReviewState::default(), &mut processed)
            .await
            .expect("action completed");

        assert!(results.performed_action);
        assert!(!results.opened_in_browser);
        assert_eq!(processed, HashSet::from([api_url]));
        let requests = server.await.expect("test server");
        let request = requests.first().expect("action request");
        assert!(request.starts_with(expected));
        if matches!(action, Action::Close) {
            assert!(request.contains(r#""state":"closed""#));
        } else {
            let comment = if matches!(action, Action::Rebase) {
                "@dependabot rebase"
            } else {
                "@dependabot recreate"
            };
            assert!(request.contains(comment));
        }
    }
}

#[tokio::test]
async fn dry_run_does_not_send_actions_or_record_processed_prs() {
    let (octocrab, server) = rebase_test_client(Vec::new()).await;
    let mut app = merge_test_app(octocrab);
    app.cli.dry_run = true;
    let items = [review_item(12, "Bump serde", CiStatus::Passing)];
    let mut processed = HashSet::new();

    for action in [
        Action::Close,
        Action::Rebase,
        Action::Recreate,
        Action::OpenUnreviewedInBrowser,
    ] {
        let results = app
            .process_actions(action, &items, &ReviewState::default(), &mut processed)
            .await
            .expect("action preview");
        assert!(!results.performed_action);
        assert!(!results.opened_in_browser);
    }

    assert!(processed.is_empty());
    assert!(server.await.expect("test server").is_empty());
}
