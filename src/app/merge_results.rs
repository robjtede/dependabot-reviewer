use std::{future::Future, io, time::Duration};

use derive_more::Display;
use error_stack::{Report, ResultExt as _};
use futures_util::StreamExt as _;
use octocrab::Octocrab;
use serde::Deserialize;

use super::async_merge::{AsyncMerge, AsyncMergeError, MergeOutcome, MergeStatus};
use crate::error::AppError;

pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Display)]
#[display("Stopped waiting for merge results. GitHub can still process submitted requests.")]
pub(crate) struct MergeWaitCancelled;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MergeProgress {
    Merged,
    Waiting(String),
    Inactive,
}

pub(crate) struct MonitoredResults<'a, T> {
    pub(crate) failures: Vec<(&'a T, Report<AppError>)>,
    pub(crate) cancelled: Vec<&'a T>,
}

#[cfg(test)]
pub(crate) async fn monitor_merge_results<'a, T>(
    items: &[&'a T],
    poll: impl AsyncFn(&T) -> Result<MergeProgress, Report<AppError>>,
    update: impl FnMut(&T, &MergeProgress),
    on_failure: impl AsyncFnMut(&T, Report<AppError>) -> Report<AppError>,
    cancel: impl Future<Output = io::Result<()>>,
) -> MonitoredResults<'a, T> {
    monitor_submitted_merges(
        futures_util::stream::iter(items.iter().copied()),
        poll,
        update,
        on_failure,
        cancel,
    )
    .await
}

pub(crate) async fn monitor_submitted_merges<'a, T: 'a>(
    submissions: impl futures_util::Stream<Item = &'a T>,
    poll: impl AsyncFn(&T) -> Result<MergeProgress, Report<AppError>>,
    mut update: impl FnMut(&T, &MergeProgress),
    mut on_failure: impl AsyncFnMut(&T, Report<AppError>) -> Report<AppError>,
    cancel: impl Future<Output = io::Result<()>>,
) -> MonitoredResults<'a, T> {
    let mut results = MonitoredResults {
        failures: Vec::new(),
        cancelled: Vec::new(),
    };
    let mut outstanding = std::collections::BTreeMap::new();
    let permits = tokio::sync::Semaphore::new(5);
    let mut polls = futures_util::stream::FuturesUnordered::new();
    let mut submissions_open = true;
    let mut next_index = 0;
    tokio::pin!(submissions, cancel);

    while submissions_open || !outstanding.is_empty() {
        tokio::select! {
            result = &mut cancel => {
                for item in outstanding.into_values() {
                    match &result {
                        Ok(()) => results.cancelled.push(item),
                        Err(error) => results.failures.push((item, cancelled(Err(io::Error::new(error.kind(), error.to_string()))))),
                    }
                }

                return results;
            }
            item = submissions.next(), if submissions_open => {
                if let Some(item) = item {
                    let target = WatchedMerge { index: next_index, item, inactive_polls: 0, last_message: String::new() };
                    outstanding.insert(next_index, item);
                    next_index += 1;
                    polls.push(poll_target(target, &poll, &permits, false));
                } else {
                    submissions_open = false;
                }
            }
            next = polls.next(), if !polls.is_empty() => {
                let Some((mut target, progress)) = next else { continue; };
                let progress = match progress {
                    Ok(MergeProgress::Inactive) if target.inactive_polls >= 2 => {
                        Err(merge_failed("Pull request left the merge queue or auto-merge was disabled. Check the PR before trying again."))
                    }
                    Ok(MergeProgress::Inactive) => {
                        target.inactive_polls += 1;
                        Ok(MergeProgress::Inactive)
                    }
                    Err(error) if retryable_poll_error(&error) => {
                        Ok(MergeProgress::Waiting("Status unavailable; retrying (Ctrl+C to stop waiting)".to_owned()))
                    }
                    progress => {
                        target.inactive_polls = 0;
                        progress
                    }
                };

                match progress {
                    Ok(MergeProgress::Merged) => {
                        update(target.item, &MergeProgress::Merged);
                        outstanding.remove(&target.index);
                    }
                    Ok(progress) => {
                        if progress.message() != target.last_message {
                            update(target.item, &progress);
                            target.last_message = progress.message().to_owned();
                        }

                        polls.push(poll_target(target, &poll, &permits, true));
                    }
                    Err(error) => {
                        let error = on_failure(target.item, error).await;
                        results.failures.push((target.item, error));
                        outstanding.remove(&target.index);
                    }
                }
            }
        }
    }

    results
}

struct WatchedMerge<'a, T> {
    index: usize,
    item: &'a T,
    inactive_polls: u8,
    last_message: String,
}

async fn poll_target<'a, T>(
    target: WatchedMerge<'a, T>,
    poll: &impl AsyncFn(&T) -> Result<MergeProgress, Report<AppError>>,
    permits: &tokio::sync::Semaphore,
    delayed: bool,
) -> (WatchedMerge<'a, T>, Result<MergeProgress, Report<AppError>>) {
    if delayed {
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let progress = match permits.acquire().await {
        Ok(_permit) => poll(target.item).await,
        Err(error) => Err(Report::new(error).change_context(AppError::GitHubApi)),
    };

    (target, progress)
}

impl MergeProgress {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Merged => "Merged",
            Self::Waiting(message) => message,
            Self::Inactive => "Waiting for GitHub to update merge status",
        }
    }
}

pub(crate) async fn wait_for_async_merge(
    octocrab: &Octocrab,
    status: MergeStatus,
    mut update: impl FnMut(&str),
    cancel: impl Future<Output = io::Result<()>>,
) -> Result<MergeOutcome, Report<AppError>> {
    let pending = match status {
        MergeStatus::Complete(outcome) => return Ok(outcome),
        MergeStatus::Pending(pending) => pending,
    };
    tokio::pin!(cancel);
    let mut last_message = pending.progress_message();
    update(last_message);

    loop {
        let poll = async {
            tokio::time::sleep(POLL_INTERVAL).await;
            AsyncMerge::new(octocrab).poll(&pending).await
        };

        let result = tokio::select! {
            result = &mut cancel => return Err(cancelled(result)),
            result = poll => result,
        };

        let message = match result {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => pending.progress_message(),
            Err(error) if retryable_poll_error(&error) => {
                "Status unavailable; retrying (Ctrl+C to stop waiting)"
            }
            Err(error) => return Err(error),
        };

        if message != last_message {
            update(message);
            last_message = message;
        }
    }
}

pub(crate) fn cancelled(result: io::Result<()>) -> Report<AppError> {
    match result {
        Ok(()) => Report::new(AppError::ApproveMerge).attach(MergeWaitCancelled),
        Err(error) => Report::new(error)
            .change_context(AppError::ApproveMerge)
            .attach("Could not listen for Ctrl+C")
            .attach(AsyncMergeError::Unconfirmed),
    }
}

pub(crate) fn retryable_poll_error(error: &Report<AppError>) -> bool {
    match error.downcast_ref::<octocrab::Error>() {
        Some(octocrab::Error::GitHub { source, .. }) => {
            source.status_code.is_server_error()
                || source.status_code.as_u16() == 429
                || (source.status_code.as_u16() == 403
                    && source.message.to_ascii_lowercase().contains("rate limit"))
        }
        Some(octocrab::Error::Service { .. } | octocrab::Error::Hyper { .. }) => true,
        _ => false,
    }
}

pub(crate) async fn fetch_merge_progress(
    octocrab: &Octocrab,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<MergeProgress, Report<AppError>> {
    let number = i64::try_from(number).change_context(AppError::GitHubApi)?;
    let payload = progress_request(owner, repo, number);
    let data: ProgressData = octocrab
        .graphql(&payload)
        .await
        .change_context(AppError::GitHubApi)
        .attach(AsyncMergeError::Unconfirmed)?;
    let pr = data
        .repository
        .and_then(|repository| repository.pull_request)
        .ok_or_else(|| {
            Report::new(AppError::GitHubApi)
                .attach(AsyncMergeError::Unconfirmed)
                .attach("Pull request missing in merge status response")
        })?;

    pr.progress()
}

fn progress_request<'a>(owner: &'a str, repo: &'a str, number: i64) -> impl serde::Serialize + 'a {
    #[derive(serde::Serialize)]
    struct Request<'a> {
        query: &'static str,
        variables: Variables<'a>,
    }

    #[derive(serde::Serialize)]
    struct Variables<'a> {
        owner: &'a str,
        repo: &'a str,
        number: i64,
    }

    Request {
        query: r#"
            query MergeProgress($owner: String!, $repo: String!, $number: Int!) {
              repository(owner: $owner, name: $repo) {
                pullRequest(number: $number) {
                  state
                  mergeable
                  mergeStateStatus
                  mergeQueueEntry { position state }
                  autoMergeRequest { enabledAt }
                  commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
                }
              }
            }
        "#,
        variables: Variables {
            owner,
            repo,
            number,
        },
    }
}

#[derive(Deserialize)]
struct ProgressData {
    repository: Option<ProgressRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProgressRepository {
    pull_request: Option<PullRequestProgress>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestProgress {
    state: String,
    mergeable: String,
    merge_state_status: String,
    merge_queue_entry: Option<QueueEntry>,
    auto_merge_request: Option<AutoMergeRequest>,
    commits: CommitConnection,
}

impl PullRequestProgress {
    fn progress(self) -> Result<MergeProgress, Report<AppError>> {
        if self.state == "MERGED" {
            return Ok(MergeProgress::Merged);
        }

        if self.state == "CLOSED" {
            return Err(merge_failed("Pull request was closed without merging"));
        }

        if self.mergeable == "CONFLICTING" || self.merge_state_status == "DIRTY" {
            return Err(merge_failed("Pull request has merge conflicts"));
        }

        if let Some(entry) = self.merge_queue_entry {
            let state = match entry.state.as_str() {
                "AWAITING_CHECKS" => "waiting for queue CI to pass",
                "LOCKED" => "locked",
                "MERGEABLE" => "ready to merge",
                "UNMERGEABLE" => "blocked; check queue requirements",
                _ => "queued",
            };

            return Ok(MergeProgress::Waiting(format!(
                "In merge queue (position {}); {state}",
                entry.position
            )));
        }

        if self.auto_merge_request.is_none() {
            return Ok(MergeProgress::Inactive);
        }

        let ci = self
            .commits
            .nodes
            .into_iter()
            .flatten()
            .find_map(|node| node.commit.status_check_rollup);
        let message = match ci.as_ref().map(|ci| ci.state.as_str()) {
            Some("ERROR" | "FAILURE") => "CI failed; fix or rerun checks to continue",
            Some("PENDING" | "EXPECTED") => "Waiting for CI to pass",
            _ => match self.merge_state_status.as_str() {
                "BEHIND" => "Behind base branch; rebase or update the branch to continue",
                "BLOCKED" => "Waiting for merge requirements (reviews or branch rules)",
                "DRAFT" => "Waiting for pull request to be marked ready for review",
                "CLEAN" | "HAS_HOOKS" => "Waiting for GitHub to merge",
                "UNSTABLE" => "Waiting for CI to pass",
                _ => "Waiting for GitHub to determine merge status",
            },
        };

        Ok(MergeProgress::Waiting(message.to_owned()))
    }
}

pub(crate) fn merge_failed(message: &str) -> Report<AppError> {
    Report::new(AsyncMergeError::Failed {
        message: message.to_owned(),
    })
    .change_context(AppError::ApproveMerge)
}

#[derive(Deserialize)]
struct QueueEntry {
    position: u64,
    state: String,
}

#[derive(Deserialize)]
struct AutoMergeRequest {
    #[serde(rename = "enabledAt")]
    _enabled_at: Option<String>,
}

#[derive(Deserialize)]
struct CommitConnection {
    nodes: Vec<Option<CommitNode>>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: CommitProgress,
}

#[derive(Deserialize)]
struct CommitProgress {
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<CheckRollup>,
}

#[derive(Deserialize)]
struct CheckRollup {
    state: String,
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        future::pending,
    };

    use super::*;

    fn pull_request(ci: Option<&str>, merge_state: &str) -> PullRequestProgress {
        PullRequestProgress {
            state: "OPEN".to_owned(),
            mergeable: "MERGEABLE".to_owned(),
            merge_state_status: merge_state.to_owned(),
            merge_queue_entry: None,
            auto_merge_request: Some(AutoMergeRequest {
                _enabled_at: Some("now".to_owned()),
            }),
            commits: CommitConnection {
                nodes: vec![Some(CommitNode {
                    commit: CommitProgress {
                        status_check_rollup: ci.map(|state| CheckRollup {
                            state: state.to_owned(),
                        }),
                    },
                })],
            },
        }
    }

    #[test]
    fn reports_ci_and_merge_blockers_without_claiming_success() {
        for (ci, merge_state, expected) in [
            (Some("PENDING"), "BLOCKED", "Waiting for CI to pass"),
            (Some("EXPECTED"), "BLOCKED", "Waiting for CI to pass"),
            (
                Some("FAILURE"),
                "BLOCKED",
                "CI failed; fix or rerun checks to continue",
            ),
            (
                Some("ERROR"),
                "UNSTABLE",
                "CI failed; fix or rerun checks to continue",
            ),
            (
                Some("SUCCESS"),
                "BEHIND",
                "Behind base branch; rebase or update the branch to continue",
            ),
            (
                Some("SUCCESS"),
                "BLOCKED",
                "Waiting for merge requirements (reviews or branch rules)",
            ),
            (
                None,
                "UNKNOWN",
                "Waiting for GitHub to determine merge status",
            ),
            (Some("SUCCESS"), "CLEAN", "Waiting for GitHub to merge"),
        ] {
            let progress = pull_request(ci, merge_state)
                .progress()
                .expect("pending progress");

            assert_eq!(progress, MergeProgress::Waiting(expected.to_owned()));
        }
    }

    #[test]
    fn queue_status_uses_queue_checks_instead_of_pull_request_ci() {
        let mut pr = pull_request(Some("SUCCESS"), "CLEAN");
        pr.auto_merge_request = None;
        pr.merge_queue_entry = Some(QueueEntry {
            position: 3,
            state: "AWAITING_CHECKS".to_owned(),
        });

        assert_eq!(
            pr.progress().expect("queued progress").message(),
            "In merge queue (position 3); waiting for queue CI to pass"
        );
    }

    #[test]
    fn recognizes_merged_closed_and_conflicting_pull_requests() {
        let mut merged = pull_request(Some("SUCCESS"), "CLEAN");
        merged.state = "MERGED".to_owned();
        merged.auto_merge_request = None;

        assert_eq!(
            merged.progress().expect("merged progress"),
            MergeProgress::Merged
        );

        let mut closed = pull_request(Some("SUCCESS"), "CLEAN");
        closed.state = "CLOSED".to_owned();
        assert!(format!("{:?}", closed.progress().expect_err("closed PR"))
            .contains("closed without merging"));

        let mut conflict = pull_request(None, "DIRTY");
        conflict.mergeable = "CONFLICTING".to_owned();
        assert!(
            format!("{:?}", conflict.progress().expect_err("conflicting PR"))
                .contains("merge conflicts")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn keeps_polling_past_two_minutes_and_reports_changes_until_merged() {
        let pr = 1;
        let rounds = Cell::new(0);
        let mut messages = Vec::new();
        let start = tokio::time::Instant::now();

        let results = monitor_merge_results(
            &[&pr],
            async |_| {
                rounds.set(rounds.get() + 1);
                Ok(match rounds.get() {
                    1..=66 => MergeProgress::Waiting("Waiting for CI to pass".to_owned()),
                    67 => MergeProgress::Waiting("In merge queue (position 2); queued".to_owned()),
                    68 => MergeProgress::Waiting(
                        "In merge queue (position 1); waiting for queue CI to pass".to_owned(),
                    ),
                    _ => MergeProgress::Merged,
                })
            },
            |_, progress| messages.push(progress.message().to_owned()),
            async |_, error| error,
            pending(),
        )
        .await;

        assert!(results.failures.is_empty());
        assert!(results.cancelled.is_empty());
        assert!(start.elapsed() > Duration::from_secs(120));
        assert_eq!(
            messages,
            [
                "Waiting for CI to pass",
                "In merge queue (position 2); queued",
                "In merge queue (position 1); waiting for queue CI to pass",
                "Merged"
            ]
        );
    }

    #[tokio::test]
    async fn handles_failures_without_waiting_for_another_status_request() {
        let pending_pr = 1;
        let conflicting_pr = 2;
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let mut cancel_tx = Some(cancel_tx);
        let mut offered = Vec::new();

        let results = monitor_merge_results(
            &[&pending_pr, &conflicting_pr],
            async |pr| {
                if *pr == 1 {
                    pending().await
                } else {
                    Err(merge_failed("Pull request has merge conflicts"))
                }
            },
            |_, _| {},
            async |pr, error| {
                offered.push(*pr);
                cancel_tx
                    .take()
                    .expect("one failure")
                    .send(())
                    .expect("cancel listener");
                error
            },
            async {
                cancel_rx.await.expect("cancel sender");
                Ok(())
            },
        )
        .await;

        assert_eq!(offered, [2]);
        assert_eq!(
            results
                .failures
                .iter()
                .map(|(pr, _)| **pr)
                .collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(
            results.cancelled.into_iter().copied().collect::<Vec<_>>(),
            [1]
        );
    }

    #[tokio::test]
    async fn cancellation_keeps_completed_rows_and_only_stops_pending_rows() {
        let pending_pr = 1;
        let merged_pr = 2;
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let mut cancel_tx = Some(cancel_tx);
        let mut merged = Vec::new();

        let results = monitor_merge_results(
            &[&pending_pr, &merged_pr],
            async |pr| {
                if *pr == 1 {
                    pending().await
                } else {
                    Ok(MergeProgress::Merged)
                }
            },
            |pr, progress| {
                assert_eq!(*progress, MergeProgress::Merged);
                merged.push(*pr);
                cancel_tx
                    .take()
                    .expect("one merge")
                    .send(())
                    .expect("cancel listener");
            },
            async |_, error| error,
            async {
                cancel_rx.await.expect("cancel sender");
                Ok(())
            },
        )
        .await;

        assert_eq!(merged, [2]);
        assert!(results.failures.is_empty());
        assert_eq!(
            results.cancelled.into_iter().copied().collect::<Vec<_>>(),
            [1]
        );
    }

    #[tokio::test]
    async fn watches_new_submissions_while_an_earlier_status_request_is_pending() {
        let first_pr = 1;
        let second_pr = 2;
        let (submissions_tx, submissions_rx) = tokio::sync::mpsc::unbounded_channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = RefCell::new(Some(started_tx));
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let mut cancel_tx = Some(cancel_tx);
        let mut merged = Vec::new();
        let submissions = futures_util::stream::unfold(submissions_rx, async |mut receiver| {
            receiver.recv().await.map(|item| (item, receiver))
        });
        let monitor = monitor_submitted_merges(
            submissions,
            async |pr| {
                if *pr == 1 {
                    started_tx
                        .borrow_mut()
                        .take()
                        .expect("first status request")
                        .send(())
                        .expect("submission listener");
                    pending().await
                } else {
                    Ok(MergeProgress::Merged)
                }
            },
            |pr, _| {
                merged.push(*pr);
                cancel_tx
                    .take()
                    .expect("one merge")
                    .send(())
                    .expect("cancel listener");
            },
            async |_, error| error,
            async {
                cancel_rx.await.expect("cancel sender");
                Ok(())
            },
        );
        let submit = async {
            submissions_tx.send(&first_pr).expect("results listener");
            started_rx.await.expect("status watcher");
            submissions_tx.send(&second_pr).expect("results listener");
            drop(submissions_tx);
        };

        let (_, results) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(submit, monitor)
        })
        .await
        .expect("new submissions must be watched while an earlier poll is pending");

        assert_eq!(merged, [2]);
        assert!(results.failures.is_empty());
        assert_eq!(
            results.cancelled.into_iter().copied().collect::<Vec<_>>(),
            [1]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn tolerates_status_propagation_then_reports_disabled_auto_merge() {
        let pr = 1;
        let rounds = Cell::new(0);
        let offered = RefCell::new(Vec::new());

        let results = monitor_merge_results(
            &[&pr],
            async |_| {
                rounds.set(rounds.get() + 1);
                Ok(match rounds.get() {
                    1 | 3.. => MergeProgress::Inactive,
                    _ => MergeProgress::Waiting("Waiting for CI to pass".to_owned()),
                })
            },
            |_, _| {},
            async |pr, error| {
                offered.borrow_mut().push(*pr);
                error
            },
            pending(),
        )
        .await;

        assert_eq!(rounds.get(), 5);
        assert_eq!(*offered.borrow(), [1]);
        assert_eq!(results.failures.len(), 1);
        assert!(
            format!("{:?}", results.failures.first().expect("failure").1)
                .contains("auto-merge was disabled")
        );
    }
}
