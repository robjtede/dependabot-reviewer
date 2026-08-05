use error_stack::{Report, ResultExt as _};
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};

use crate::{error::AppError, github::CiStatus};

pub(crate) struct MergeQueueStatus {
    pub(crate) pull_request_id: String,
    pub(crate) head_oid: String,
    pub(crate) uses_merge_queue: bool,
    pub(crate) already_queued: bool,
    pub(crate) auto_merge_enabled: bool,
}

#[derive(Debug)]
pub(crate) enum ApprovalMode {
    Direct,
    AutoMerge,
    MergeQueueEnqueue,
    MergeQueueAutoMerge,
    AlreadyQueued,
    AlreadyAutoMergeEnabled,
    SkipPendingWithoutQueue,
}

pub(crate) struct ApprovalWorkflow {
    octocrab: Octocrab,
}

impl ApprovalWorkflow {
    pub(crate) fn new(octocrab: &Octocrab) -> Self {
        Self {
            octocrab: octocrab.clone(),
        }
    }

    pub(crate) async fn inspect(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        base_ref_name: &str,
    ) -> Result<MergeQueueStatus, Report<AppError>> {
        const QUERY: &str = r#"
            query MergeQueueStatus($owner: String!, $repo: String!, $number: Int!, $baseBranch: String!) {
              repository(owner: $owner, name: $repo) {
                mergeQueue(branch: $baseBranch) { id }
                pullRequest(number: $number) {
                  id
                  headRefOid
                  mergeQueueEntry { id }
                  autoMergeRequest { enabledAt }
                }
              }
            }
        "#;

        let number = i64::try_from(pr_number)
            .change_context(AppError::GitHubApi)
            .attach_with(|| format!("PR #{pr_number} number is too large for GraphQL"))?;
        let payload = InspectionRequest {
            query: QUERY,
            variables: InspectionVariables {
                owner,
                repo,
                number,
                base_branch: base_ref_name,
            },
        };
        let data: InspectionData = self
            .octocrab
            .graphql(&payload)
            .await
            .change_context(AppError::GitHubApi)
            .attach(format!(
                "Failed to query merge queue status for PR #{pr_number}"
            ))?;
        let repository = data
            .repository
            .ok_or_else(|| Report::new(AppError::GitHubApi))
            .attach(format!(
                "Repository missing in GraphQL response for PR #{pr_number}"
            ))?;
        let pull_request = repository
            .pull_request
            .ok_or_else(|| Report::new(AppError::GitHubApi))
            .attach(format!(
                "Pull request missing in GraphQL response for PR #{pr_number}"
            ))?;

        let _ = repository.merge_queue.as_ref().map(|node| node.id.as_str());
        let _ = pull_request
            .merge_queue_entry
            .as_ref()
            .map(|node| node.id.as_str());
        let _ = pull_request
            .auto_merge_request
            .as_ref()
            .map(|request| request.enabled_at.as_str());

        Ok(MergeQueueStatus {
            pull_request_id: pull_request.id,
            head_oid: pull_request.head_ref_oid,
            uses_merge_queue: repository.merge_queue.is_some(),
            already_queued: pull_request.merge_queue_entry.is_some(),
            auto_merge_enabled: pull_request.auto_merge_request.is_some(),
        })
    }

    pub(crate) fn plan(
        ci_status: CiStatus,
        queue_status: &MergeQueueStatus,
        allow_auto_merge: bool,
        allow_non_passing_ci: bool,
    ) -> ApprovalMode {
        if queue_status.auto_merge_enabled {
            return ApprovalMode::AlreadyAutoMergeEnabled;
        }

        if queue_status.uses_merge_queue {
            if queue_status.already_queued {
                return ApprovalMode::AlreadyQueued;
            }

            return match ci_status {
                CiStatus::Passing | CiStatus::Unknown => ApprovalMode::MergeQueueEnqueue,
                CiStatus::Pending | CiStatus::Failing if allow_non_passing_ci => {
                    ApprovalMode::MergeQueueAutoMerge
                }
                CiStatus::Pending | CiStatus::Failing => ApprovalMode::SkipPendingWithoutQueue,
            };
        }

        match ci_status {
            CiStatus::Passing | CiStatus::Unknown => ApprovalMode::Direct,
            CiStatus::Pending | CiStatus::Failing if allow_non_passing_ci => ApprovalMode::Direct,
            CiStatus::Pending if allow_auto_merge => ApprovalMode::AutoMerge,
            CiStatus::Pending | CiStatus::Failing => ApprovalMode::SkipPendingWithoutQueue,
        }
    }
}

#[derive(Serialize)]
struct InspectionRequest<'a, T> {
    query: &'a str,
    variables: T,
}

#[derive(Serialize)]
struct InspectionVariables<'a> {
    owner: &'a str,
    repo: &'a str,
    number: i64,
    #[serde(rename = "baseBranch")]
    base_branch: &'a str,
}

#[derive(Deserialize)]
struct InspectionData {
    repository: Option<InspectionRepository>,
}

#[derive(Deserialize)]
struct InspectionRepository {
    #[serde(rename = "mergeQueue")]
    merge_queue: Option<InspectionNode>,
    #[serde(rename = "pullRequest")]
    pull_request: Option<InspectionPullRequest>,
}

#[derive(Deserialize)]
struct InspectionPullRequest {
    id: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "mergeQueueEntry")]
    merge_queue_entry: Option<InspectionNode>,
    #[serde(rename = "autoMergeRequest")]
    auto_merge_request: Option<InspectionAutoMergeRequest>,
}

#[derive(Deserialize)]
struct InspectionNode {
    id: String,
}

#[derive(Deserialize)]
struct InspectionAutoMergeRequest {
    #[serde(rename = "enabledAt")]
    enabled_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_status(uses_merge_queue: bool) -> MergeQueueStatus {
        MergeQueueStatus {
            pull_request_id: String::new(),
            head_oid: String::new(),
            uses_merge_queue,
            already_queued: false,
            auto_merge_enabled: false,
        }
    }

    #[test]
    fn queues_passing_ci_when_a_merge_queue_exists() {
        assert!(matches!(
            ApprovalWorkflow::plan(CiStatus::Passing, &queue_status(true), false, false),
            ApprovalMode::MergeQueueEnqueue
        ));
    }

    #[test]
    fn enables_auto_merge_for_pending_ci_without_a_queue() {
        assert!(matches!(
            ApprovalWorkflow::plan(CiStatus::Pending, &queue_status(false), true, false),
            ApprovalMode::AutoMerge
        ));
    }

    #[test]
    fn preserves_unknown_ci_as_mergeable() {
        assert!(matches!(
            ApprovalWorkflow::plan(CiStatus::Unknown, &queue_status(false), false, false),
            ApprovalMode::Direct
        ));
    }
}
