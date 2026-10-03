use std::time::Duration;

use super::{super::test_support::*, *};
use crate::github::DepUpdate;

fn batch(item: ReviewItem) -> ApprovalBatch {
    ApprovalBatch {
        contexts: HashMap::from([(
            item.repo.clone(),
            RepositoryMergeSettings {
                merge_method: MergeMethod::Merge,
                allow_auto_merge: true,
            },
        )]),
        items: vec![item],
        allow_non_passing_ci: false,
    }
}

#[tokio::test]
async fn saves_review_state_while_a_queued_pull_request_is_still_waiting() {
    let pending = r#"{"data":{"repository":{"pullRequest":{"state":"OPEN","mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED","mergeQueueEntry":{"position":1,"state":"AWAITING_CHECKS"},"autoMergeRequest":null,"commits":{"nodes":[]}}}}}"#;
    let (octocrab, server) = rebase_test_client(vec![
        (200, queue_inspection(true, true, false)),
        (200, pending.to_owned()),
    ])
    .await;
    let app = merge_test_app(octocrab);
    let directory = tempfile::tempdir().expect("state directory");
    let state_path = camino::Utf8PathBuf::from_path_buf(directory.path().join("state.toml"))
        .expect("UTF-8 state path");
    let update = DepUpdate {
        dep_type: "cargo".to_owned(),
        dep_name: "serde".to_owned(),
        to_version: "1.0.1".to_owned(),
    };
    let mut item = review_item(12, "Bump serde", CiStatus::Passing);
    item.pr.dep_update = Some(update.clone());
    let batch = batch(item);
    let mut session = ReviewSession {
        state_path: state_path.clone(),
        review_state: ReviewState::default(),
        performed_action: None,
        opened_in_browser: true,
    };
    let running = batch.run(&app, &mut session);
    tokio::pin!(running);

    tokio::select! {
        result = &mut running => panic!("queued PR must still be waiting: {result:?}"),
        requests = tokio::time::timeout(Duration::from_secs(5), server) => {
            assert_eq!(requests.expect("status request deadline").expect("test server").len(), 2);
        }
    }

    let saved =
        ReviewState::load_from_path(&state_path).expect("state saved before monitoring finishes");
    assert!(saved.is_previously_reviewed(&update));
}

#[tokio::test]
async fn dry_run_inspects_the_plan_without_sending_approval_or_merge_requests() {
    let (octocrab, server) =
        rebase_test_client(vec![(200, queue_inspection(true, false, false))]).await;
    let app = merge_test_app(octocrab);
    let batch = batch(review_item(12, "Bump serde", CiStatus::Passing));

    batch
        .preview(&app, &ReviewState::default())
        .await
        .expect("approval preview");

    let requests = server.await.expect("test server");
    assert_eq!(requests.len(), 1);
    assert!(requests
        .first()
        .expect("inspection")
        .contains("query MergeQueueStatus"));
}
