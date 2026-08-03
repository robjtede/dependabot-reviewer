use crate::github::CiStatus;

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

pub(crate) struct ApprovalWorkflow;

impl ApprovalWorkflow {
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
