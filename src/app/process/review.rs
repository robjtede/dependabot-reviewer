use std::collections::HashMap;

use console::style;
use error_stack::Report;
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
) -> String {
    let mut badges = vec![if previously_reviewed {
        style("previously reviewed").dim().to_string()
    } else {
        style("unreviewed").red().to_string()
    }];

    if let Some(status) = pending_status {
        badges.push(pending_status_badge(status));
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

    #[test]
    fn unreviewed_pr_is_not_marked_as_unmergeable_in_list() {
        let badges = review_badges(false, None);

        assert_eq!(badges, "unreviewed");
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
