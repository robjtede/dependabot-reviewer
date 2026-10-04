use std::collections::HashMap;

use console::style;
use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt;
use futures_util::StreamExt as _;

use super::{App, ReviewItem};
use crate::{app::approval_workflow::MergeQueueStatus, error::AppError, github::CiStatus};

impl App {
    pub(super) async fn fetch_pending_review_statuses(
        &self,
        review_items: &[ReviewItem],
    ) -> Result<HashMap<String, HashMap<u64, MergeQueueStatus>>, Report<AppError>> {
        let status_tasks = review_items
            .iter()
            .filter(|item| item.pr.ci_status == CiStatus::Pending)
            .map(|item| {
                let repo = item.repo.clone();
                let owner = item.owner.clone();
                let repo_name = item.repo_name.clone();
                let pr_number = item.pr.number;
                let base_ref_name = item.pr.base_ref_name.clone();

                async move {
                    let status = self
                        .fetch_merge_queue_status(&owner, &repo_name, pr_number, &base_ref_name)
                        .await?;

                    Ok::<_, Report<_>>((repo, pr_number, status))
                }
            });

        let mut statuses = HashMap::<String, HashMap<u64, MergeQueueStatus>>::new();
        let mut stream = futures_util::stream::iter(status_tasks).buffered_unordered(5);
        while let Some(result) = stream.next().await {
            let (repo, pr_number, status) = result?;
            statuses.entry(repo).or_default().insert(pr_number, status);
        }

        Ok(statuses)
    }
}

pub(super) async fn has_actions_lock(
    octocrab: &octocrab::Octocrab,
    item: &ReviewItem,
    cache: &mut HashMap<(String, String), bool>,
) -> Result<bool, Report<AppError>> {
    // Grouped update titles do not always parse as a single dependency update.
    if !item
        .pr
        .head_ref_name
        .starts_with("dependabot/github_actions/")
    {
        return Ok(false);
    }

    let key = (item.repo.clone(), item.pr.base_ref_name.clone());
    if let Some(&exists) = cache.get(&key) {
        return Ok(exists);
    }

    let query = serde_urlencoded::to_string([("ref", &item.pr.base_ref_name)])
        .change_context(AppError::GitHubApi)?;
    let route = format!(
        "/repos/{}/contents/.github/workflows/actions.lock?{query}",
        item.repo,
    );
    let response = octocrab
        ._get(route)
        .await
        .change_context(AppError::GitHubApi)?;

    let exists = if response.status().as_u16() == 404 {
        false
    } else {
        octocrab::map_github_error(response)
            .await
            .change_context(AppError::GitHubApi)
            .attach_with(|| {
                format!(
                    "Cannot check actions.lock for {} on {}; approval and merge stopped",
                    item.repo, item.pr.base_ref_name,
                )
            })?;
        true
    };

    cache.insert(key, exists);
    Ok(exists)
}

fn pending_status_badge(status: &MergeQueueStatus) -> String {
    if status.already_queued {
        format!("{}", style("queued").green())
    } else if status.auto_merge_enabled {
        format!("{}", style("auto-merge enabled").green())
    } else if status.uses_merge_queue {
        format!("{}", style("not queued").yellow())
    } else {
        format!("{}", style("not auto-merge enabled").yellow())
    }
}

pub(super) fn review_badges(
    previously_reviewed: bool,
    pending_status: Option<&MergeQueueStatus>,
    actions_lock_check: &Result<bool, Report<AppError>>,
) -> String {
    let mut badges = vec![if previously_reviewed {
        style("previously reviewed").dim().to_string()
    } else {
        style("unreviewed").red().to_string()
    }];

    if let Some(status) = pending_status {
        badges.push(pending_status_badge(status));
    }

    match actions_lock_check {
        Ok(true) => badges.push(style("will not merge: actions.lock").yellow().to_string()),
        Err(_) => badges.push(
            style("merge status unknown: actions.lock check failed")
                .yellow()
                .to_string(),
        ),
        Ok(false) => {}
    }

    badges.join(", ")
}

pub(super) fn failing_ci_agent_prompt(review_items: &[ReviewItem]) -> Option<String> {
    let failing_items = review_items
        .iter()
        .filter(|item| item.pr.ci_status == CiStatus::Failing)
        .collect::<Vec<_>>();

    if failing_items.is_empty() {
        return None;
    }

    let mut prompt = String::from("Triage the failing CI for these Dependabot pull requests:\n\n");

    for item in failing_items {
        prompt.push_str(&format!(
            "- {}#{}: {}\n  {}\n",
            item.repo, item.pr.number, item.pr.title, item.pr.url
        ));
    }

    prompt.push_str(
        "\nFor each pull request, inspect the failing checks and scan the relevant logs briefly. \
Categorize the failure as a dependency regression, repository issue, CI or infrastructure \
issue, flaky failure, unrelated existing failure, or unclear. Summarize the evidence for the \
category. Suggest a fix when one is obvious from the logs or surrounding context. Group pull \
requests that share the same failure when useful. Do not make changes. State when a failure \
needs deeper investigation.",
    );

    Some(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::process::test_support::*;

    fn actions_review_item() -> ReviewItem {
        let mut item = review_item(
            12,
            "Bump the actions group with 3 updates",
            CiStatus::Passing,
        );
        item.pr.head_ref_name = "dependabot/github_actions/actions-group".to_owned();
        item.pr.base_ref_name = "release/1.x".to_owned();
        item
    }

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
                rebase_test_client(vec![(status, r#"{"message":"lookup failed"}"#.to_owned())])
                    .await;

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
}
