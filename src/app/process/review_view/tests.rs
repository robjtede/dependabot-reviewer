use super::{super::test_support::review_item, *};
#[test]
fn actions_lock_badge_marks_pr_as_unmergeable_in_list() {
    let badges = review_badges(false, None, &Ok(true));

    assert!(badges.contains("unreviewed"));
    assert!(badges.contains("will not merge: actions.lock"));
}

#[test]
fn actions_lock_badge_distinguishes_missing_and_failed_checks() {
    let allowed = review_badges(false, None, &Ok(false));
    let unknown = review_badges(false, None, &Err(Report::new(AppError::GitHubApi)));

    assert_eq!(allowed, "unreviewed");
    assert!(unknown.contains("merge status unknown: actions.lock check failed"));
}

#[test]
fn agent_prompt_lists_only_prs_with_failing_ci() {
    let review_items = [
        review_item(12, "Bump serde from 1.0.0 to 1.0.1", CiStatus::Failing),
        review_item(11, "Bump tokio from 1.0.0 to 1.1.0", CiStatus::Passing),
    ];

    let prompt = failing_ci_agent_prompt(&review_items).expect("expected agent prompt");

    assert!(prompt.contains("example/repo#12"));
    assert!(prompt.contains("Bump serde from 1.0.0 to 1.0.1"));
    assert!(prompt.contains("https://github.com/example/repo/pull/12"));
    assert!(!prompt.contains("example/repo#11"));
    assert!(prompt.contains("Categorize the failure"));
    assert!(prompt.contains("Suggest a fix when one is obvious"));
}

#[test]
fn agent_prompt_is_absent_without_failing_ci() {
    let review_items = [review_item(
        11,
        "Bump tokio from 1.0.0 to 1.1.0",
        CiStatus::Passing,
    )];

    assert!(failing_ci_agent_prompt(&review_items).is_none());
}
