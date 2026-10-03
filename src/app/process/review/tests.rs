use super::{super::test_support::*, *};
#[tokio::test]
async fn actions_lock_blocks_grouped_updates_and_caches_by_repo_and_base() {
    let (octocrab, server) = rebase_test_client(vec![(200, "{}".to_owned()); 3]).await;
    let mut cache = HashMap::new();
    let mut item = actions_review_item();

    assert!(has_actions_lock(&octocrab, &item, &mut cache)
        .await
        .expect("lock check"));
    assert!(has_actions_lock(&octocrab, &item, &mut cache)
        .await
        .expect("cached check"));

    item.pr.base_ref_name = "main".to_owned();
    assert!(has_actions_lock(&octocrab, &item, &mut cache)
        .await
        .expect("other base"));

    item.repo = "example/other".to_owned();
    item.repo_name = "other".to_owned();
    assert!(has_actions_lock(&octocrab, &item, &mut cache)
        .await
        .expect("other repo"));

    let requests = server.await.expect("test server");
    assert_eq!(requests.len(), 3);
    assert!(requests.first().expect("request").starts_with(
        "GET /repos/example/repo/contents/.github/workflows/actions.lock?ref=release%2F1.x "
    ));
}

#[tokio::test]
async fn actions_lock_allows_missing_lockfile() {
    let (octocrab, server) =
        rebase_test_client(vec![(404, r#"{"message":"Not Found"}"#.to_owned())]).await;

    assert!(
        !has_actions_lock(&octocrab, &actions_review_item(), &mut HashMap::new())
            .await
            .expect("missing lockfile")
    );
    server.await.expect("test server");
}

#[tokio::test]
async fn actions_lock_lookup_errors_prevent_approval() {
    for status in [403, 429, 500] {
        let (octocrab, server) =
            rebase_test_client(vec![(status, r#"{"message":"lookup failed"}"#.to_owned())]).await;

        let error = has_actions_lock(&octocrab, &actions_review_item(), &mut HashMap::new())
            .await
            .expect_err("failed checks must stop approval");
        assert!(format!("{error:?}").contains("approval and merge stopped"));
        server.await.expect("test server");
    }
}

#[tokio::test]
async fn actions_lock_does_not_block_other_ecosystems() {
    let (octocrab, server) = rebase_test_client(vec![]).await;
    let item = review_item(12, "Bump tokio from 1 to 2", CiStatus::Passing);
    let mut cache = HashMap::from([((item.repo.clone(), item.pr.base_ref_name.clone()), true)]);

    assert!(!has_actions_lock(&octocrab, &item, &mut cache)
        .await
        .expect("cargo update"));
    assert!(server.await.expect("test server").is_empty());
}
