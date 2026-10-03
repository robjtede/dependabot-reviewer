use std::assert_matches;

use super::*;
#[tokio::test]
async fn merge_batch_continues_after_a_conflict() {
    let mut attempted = Vec::new();
    let mut completed = Vec::new();
    let prs = [1, 2, 3, 4];

    let failures = process_merge_batch(
        &prs,
        async |pr| {
            attempted.push(*pr);

            if *pr == 2 || *pr == 4 {
                return Err(
                    Report::new(AppError::ApproveMerge).attach("Pull Request has merge conflicts")
                );
            }

            completed.push(*pr);
            Ok(())
        },
        async |_, error| error,
        std::future::pending(),
    )
    .await;

    assert_eq!(attempted, [1, 2, 3, 4]);
    assert_eq!(completed, [1, 3]);
    assert_eq!(
        failures.iter().map(|(pr, _)| **pr).collect::<Vec<_>>(),
        [2, 4]
    );
    assert!(format!("{:?}", failures.first().expect("first failure").1).contains("merge conflicts"));
}

#[tokio::test]
async fn merge_batch_returns_no_failures_when_all_prs_succeed() {
    let mut completed = Vec::new();
    let prs = [1, 2];

    let failures = process_merge_batch(
        &prs,
        async |pr| {
            completed.push(*pr);
            Ok(())
        },
        async |_, error| error,
        std::future::pending(),
    )
    .await;

    assert_eq!(completed, prs);
    assert!(failures.is_empty());
}

#[tokio::test]
async fn merge_batch_skips_remaining_prs_when_a_merge_result_is_unconfirmed() {
    let mut attempted = Vec::new();
    let prs = [1, 2, 3];

    let failures = process_merge_batch(
        &prs,
        async |pr| {
            attempted.push(*pr);

            Err(Report::new(AsyncMergeError::Unconfirmed).change_context(AppError::ApproveMerge))
        },
        async |_, error| error,
        std::future::pending(),
    )
    .await;

    assert_eq!(attempted, [1]);
    assert_eq!(failures.len(), 3);
    assert!(failures.iter().skip(1).all(|(_, error)| {
        format!("{error:?}").contains("Skipped because an earlier merge result is not confirmed")
    }));
}

#[tokio::test]
async fn cancelling_the_batch_keeps_completed_merges_and_skips_new_submissions() {
    let prs = [1, 2, 3];
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let mut cancel_tx = Some(cancel_tx);
    let mut attempted = Vec::new();

    let failures = process_merge_batch(
        &prs,
        async |pr| {
            attempted.push(*pr);
            cancel_tx
                .take()
                .expect("one submission")
                .send(())
                .expect("cancel listener");
            Ok(())
        },
        async |_, error| error,
        async {
            cancel_rx.await.expect("cancel sender");
            Ok(())
        },
    )
    .await;

    assert_eq!(attempted, [1]);
    assert_eq!(
        failures.iter().map(|(pr, _)| **pr).collect::<Vec<_>>(),
        [2, 3]
    );
    assert!(failures
        .first()
        .expect("cancelled item")
        .1
        .downcast_ref::<MergeWaitCancelled>()
        .is_some());
    assert_matches!(
        failures
            .last()
            .expect("skipped item")
            .1
            .downcast_ref::<MergeSkipped>(),
        Some(MergeSkipped::Cancelled)
    );
}

#[tokio::test]
async fn merge_batch_offers_recovery_before_submitting_the_next_pull_request() {
    use std::cell::RefCell;

    let prs = [1, 2];
    let events = RefCell::new(Vec::new());
    let failures = process_merge_batch(
        &prs,
        async |pr| {
            events.borrow_mut().push(format!("submit {pr}"));
            if *pr == 1 {
                Err(crate::app::merge_results::merge_failed(
                    "Pull request has merge conflicts",
                ))
            } else {
                Ok(())
            }
        },
        async |pr, error| {
            events.borrow_mut().push(format!("offer recovery {pr}"));
            error
        },
        std::future::pending(),
    )
    .await;

    assert_eq!(
        *events.borrow(),
        ["submit 1", "offer recovery 1", "submit 2"]
    );
    assert_eq!(failures.len(), 1);
}
