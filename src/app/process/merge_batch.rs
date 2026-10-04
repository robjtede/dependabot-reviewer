use std::{
    future::Future,
    io::{self, IsTerminal as _},
};

use derive_more::Display;
use dialoguer::{theme::ColorfulTheme, Confirm};
use error_stack::{Report, ResultExt as _};

use super::{status_rows::PrStatusRows, App, MergeInfo};
use crate::{
    app::{async_merge::AsyncMergeError, merge_results::MergeWaitCancelled},
    error::AppError,
};

#[derive(Debug, Display)]
pub(super) enum MergeSkipped {
    #[display("Skipped because an earlier merge result is not confirmed. Check the pull request before trying again.")]
    Unconfirmed,
    #[display("Skipped because waiting for merge results was cancelled")]
    Cancelled,
}

impl App {
    pub(super) async fn handle_merge_failure(
        &self,
        info: &MergeInfo,
        error: Report<AppError>,
        statuses: Option<&PrStatusRows>,
    ) -> Report<AppError> {
        if error.downcast_ref::<MergeWaitCancelled>().is_some() {
            finish_cancelled_merge(info, statuses);
            return error;
        }

        if let Some(skipped) = error.downcast_ref::<MergeSkipped>() {
            let message = skipped.to_string();
            if let Some(statuses) = statuses {
                statuses.finish_skipped(&info.repo, info.pr_number, &message);
            } else {
                println!("  {}#{}: {message}", info.repo, info.pr_number);
            }

            return error;
        }

        let message = if matches!(
            error.downcast_ref::<AsyncMergeError>(),
            Some(AsyncMergeError::Unconfirmed)
        ) {
            "Merge result unconfirmed; check the pull request".to_owned()
        } else if let Some(AsyncMergeError::Failed { message }) =
            error.downcast_ref::<AsyncMergeError>()
        {
            format!("Merge failed: {message}")
        } else {
            "Approval or merge failed; check the pull request".to_owned()
        };

        if let Some(statuses) = statuses {
            statuses.complete(&info.repo, info.pr_number, &format!("✗ {message}"));
        } else {
            println!("  {}#{}: {message}", info.repo, info.pr_number);
        }

        let rebase_result = offer_conflict_rebase(&self.octocrab, info, &error, || {
            let prompt = || {
                if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                    println!("  {}#{} has merge conflicts. Comment `@dependabot rebase` on {} to request a rebase.", info.repo, info.pr_number, info.url);
                    return Ok(false);
                }

                Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(format!("{}#{} has merge conflicts. Post `@dependabot rebase`?", info.repo, info.pr_number))
                    .default(false)
                    .interact()
                    .change_context(AppError::Interactive)
                    .attach("Rebase confirmation failed")
            };

            match statuses {
                Some(statuses) => statuses.suspend(prompt),
                None => prompt(),
            }
        }).await;

        match rebase_result {
            Ok(true) => {
                if let Some(statuses) = statuses {
                    statuses.complete(
                        &info.repo,
                        info.pr_number,
                        "Rebase requested; run again after CI completes",
                    );
                } else {
                    println!("  Rebase requested for {}#{}. Run the tool again after Dependabot updates the PR and CI completes.", info.repo, info.pr_number);
                }

                error.attach("Dependabot rebase requested; PR is not merged")
            }
            Ok(false) => error,
            Err(rebase_error) => error.attach(format!(
                "Could not request a Dependabot rebase: {rebase_error:?}"
            )),
        }
    }
}

async fn offer_conflict_rebase(
    octocrab: &octocrab::Octocrab,
    info: &MergeInfo,
    error: &Report<AppError>,
    confirm: impl FnOnce() -> Result<bool, Report<AppError>>,
) -> Result<bool, Report<AppError>> {
    let async_merge_conflicts = matches!(
        error.downcast_ref::<AsyncMergeError>(),
        Some(AsyncMergeError::Failed { message })
            if message.to_ascii_lowercase().contains("merge conflict")
                || message.to_ascii_lowercase().contains("not mergeable")
    );
    let may_have_conflicts = async_merge_conflicts
        || match error.downcast_ref::<octocrab::Error>() {
            Some(octocrab::Error::GitHub { source, .. }) => source.status_code.as_u16() == 405,
            Some(octocrab::Error::Graphql { source, .. }) => source.0.iter().any(|error| {
                error
                    .message
                    .to_ascii_lowercase()
                    .contains("merge conflict")
            }),
            _ => false,
        };

    if !may_have_conflicts {
        return Ok(false);
    }

    // A rejected merge can also mean branch protection or an outdated head SHA.
    // Confirm that the PR still has conflicts before offering a rebase.
    let pr = octocrab
        .pulls(&info.owner, &info.repo_name)
        .get(info.pr_number)
        .await
        .change_context(AppError::GitHubApi)
        .attach("Failed to check current merge conflicts")?;

    if pr.state != Some(octocrab::models::IssueState::Open)
        || pr.merged == Some(true)
        || pr.mergeable != Some(false)
        || !confirm()?
    {
        return Ok(false);
    }

    octocrab
        .issues(&info.owner, &info.repo_name)
        .create_comment(info.pr_number, "@dependabot rebase")
        .await
        .change_context(AppError::Comment)
        .attach("Failed to post the Dependabot rebase request")?;

    Ok(true)
}

pub(super) async fn process_merge_batch<'a, T>(
    items: &'a [T],
    mut process: impl AsyncFnMut(&'a T) -> Result<(), Report<AppError>>,
    mut on_failure: impl AsyncFnMut(&'a T, Report<AppError>) -> Report<AppError>,
    cancel: impl Future<Output = io::Result<()>>,
) -> Vec<(&'a T, Report<AppError>)> {
    let mut failures = Vec::new();
    let mut stopped = None;
    tokio::pin!(cancel);

    for item in items {
        if let Some(cancelled) = stopped {
            let reason = if cancelled {
                MergeSkipped::Cancelled
            } else {
                MergeSkipped::Unconfirmed
            };
            let error = Report::new(AppError::ApproveMerge).attach(reason);
            failures.push((item, on_failure(item, error).await));

            continue;
        }

        let result = tokio::select! {
            biased;
            result = &mut cancel => Err(crate::app::merge_results::cancelled(result)),
            result = process(item) => result,
        };

        if let Err(error) = result {
            if error.downcast_ref::<MergeWaitCancelled>().is_some() {
                stopped = Some(true);
            } else if matches!(
                error.downcast_ref::<AsyncMergeError>(),
                Some(AsyncMergeError::Unconfirmed)
            ) {
                stopped = Some(false);
            }

            failures.push((item, on_failure(item, error).await));
        }
    }

    failures
}

pub(super) fn finish_cancelled_merge(info: &MergeInfo, statuses: Option<&PrStatusRows>) {
    let message = "Stopped waiting; GitHub can still process the request";
    if let Some(statuses) = statuses {
        statuses.complete(&info.repo, info.pr_number, message);
    } else {
        println!("  {}#{}: {message}", info.repo, info.pr_number);
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use octocrab::params::pulls::MergeMethod;

    use super::*;
    use crate::app::{async_merge::MergeOperation, process::test_support::*};

    #[tokio::test]
    async fn conflict_rebase_handles_async_merge_failures() {
        let (octocrab, server) = rebase_test_client(vec![
            (202, pending_merge("merge", "direct_merge", "head", false)),
            (
                200,
                r#"{"status":"failed","details":{"message":"Pull Request has merge conflicts"}}"#
                    .to_owned(),
            ),
            (200, conflicted_pr("false", "open")),
        ])
        .await;
        let error = test_merge_request(
            &octocrab,
            "example",
            "repo",
            12,
            "head",
            MergeOperation::Direct(MergeMethod::Merge),
        )
        .await
        .expect_err("merge conflicts");
        let mut prompted = false;

        let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
            prompted = true;
            Ok(false)
        })
        .await
        .expect("declined rebase");

        assert!(prompted);
        assert!(!requested);
        assert_eq!(server.await.expect("test server").len(), 3);
    }

    #[tokio::test]
    async fn conflict_rebase_posts_only_after_confirmation() {
        let (octocrab, server) = rebase_test_client(vec![
            (405, r#"{"message":"Pull Request is not mergeable"}"#.to_owned()),
            (200, conflicted_pr("false", "open")),
            (201, r#"{"id":1,"node_id":"comment","url":"https://example.com/comment","html_url":"https://example.com/comment","user":{"login":"tester","id":1,"node_id":"user","gravatar_id":"","type":"User","site_admin":false,"avatar_url":"https://example.com","url":"https://example.com","html_url":"https://example.com","followers_url":"https://example.com","following_url":"https://example.com","gists_url":"https://example.com","starred_url":"https://example.com","subscriptions_url":"https://example.com","organizations_url":"https://example.com","repos_url":"https://example.com","events_url":"https://example.com","received_events_url":"https://example.com"},"created_at":"2026-09-17T00:00:00Z","body":"@dependabot rebase"}"#.to_owned()),
        ]).await;
        let error = merge_test_error(&octocrab).await;
        let mut prompted = false;

        let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
            prompted = true;
            Ok(true)
        })
        .await
        .expect("rebase request");

        assert!(requested, "conflicted PR should get a rebase request");
        assert!(prompted);
        let requests = server.await.expect("test server");
        assert_eq!(requests.len(), 3);
        assert!(requests
            .get(1)
            .expect("PR lookup")
            .starts_with("GET /repos/example/repo/pulls/12 "));
        let comment = requests.last().expect("comment request");
        assert!(comment.starts_with("POST /repos/example/repo/issues/12/comments "));
        assert!(comment.ends_with(r#"{"body":"@dependabot rebase"}"#));
    }

    #[tokio::test]
    async fn conflict_rebase_does_not_comment_when_declined() {
        let (octocrab, server) = rebase_test_client(vec![
            (
                405,
                r#"{"message":"Pull Request is not mergeable"}"#.to_owned(),
            ),
            (200, conflicted_pr("false", "open")),
        ])
        .await;
        let error = merge_test_error(&octocrab).await;
        let mut prompted = false;

        let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
            prompted = true;
            Ok(false)
        })
        .await
        .expect("declined rebase");

        assert!(!requested);
        assert!(prompted);
        assert_eq!(server.await.expect("test server").len(), 2);
    }

    #[tokio::test]
    async fn conflict_rebase_requires_current_open_conflicts() {
        for (mergeable, state) in [("true", "open"), ("null", "open"), ("false", "closed")] {
            let (octocrab, server) = rebase_test_client(vec![
                (
                    405,
                    r#"{"message":"Pull Request is not mergeable"}"#.to_owned(),
                ),
                (200, conflicted_pr(mergeable, state)),
            ])
            .await;
            let error = merge_test_error(&octocrab).await;
            let mut prompted = false;

            let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
                prompted = true;
                Ok(true)
            })
            .await
            .expect("ineligible PR");

            assert!(!requested);
            assert!(!prompted, "mergeable={mergeable}, state={state}");
            assert_eq!(server.await.expect("test server").len(), 2);
        }
    }

    #[tokio::test]
    async fn conflict_rebase_ignores_unrelated_merge_failures() {
        for status in [403, 409, 429, 500] {
            let (octocrab, server) = rebase_test_client(vec![(
                status,
                r#"{"message":"Unrelated merge failure"}"#.to_owned(),
            )])
            .await;
            let error = merge_test_error(&octocrab).await;
            let mut prompted = false;

            let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
                prompted = true;
                Ok(true)
            })
            .await
            .expect("unrelated failure");

            assert!(!requested);
            assert!(!prompted, "HTTP {status}");
            assert_eq!(server.await.expect("test server").len(), 1);
        }
    }

    #[tokio::test]
    async fn conflict_rebase_handles_graphql_merge_conflicts() {
        #[derive(serde::Serialize)]
        struct GraphqlRequest<'a> {
            query: &'a str,
            variables: (),
        }

        let (octocrab, server) = rebase_test_client(vec![
            (
                200,
                r#"{"errors":[{"message":"Pull Request has merge conflicts"}]}"#.to_owned(),
            ),
            (200, conflicted_pr("false", "open")),
        ])
        .await;
        let error = octocrab.graphql::<()>(&GraphqlRequest {
            query: "mutation { enqueuePullRequest(input: {pullRequestId: \"test\"}) { mergeQueueEntry { id } } }",
            variables: (),
        }).await.expect_err("GraphQL conflict");
        let error = Report::new(error).change_context(AppError::ApproveMerge);
        let mut prompted = false;

        let requested = offer_conflict_rebase(&octocrab, &merge_info(), &error, || {
            prompted = true;
            Ok(false)
        })
        .await
        .expect("declined rebase");

        assert!(prompted);
        assert!(!requested);
        assert_eq!(server.await.expect("test server").len(), 2);
    }

    #[tokio::test]
    async fn conflict_rebase_reports_comment_failures() {
        let (octocrab, server) = rebase_test_client(vec![
            (
                405,
                r#"{"message":"Pull Request is not mergeable"}"#.to_owned(),
            ),
            (200, conflicted_pr("false", "open")),
            (403, r#"{"message":"Resource not accessible"}"#.to_owned()),
        ])
        .await;
        let error = merge_test_error(&octocrab).await;

        let error = offer_conflict_rebase(&octocrab, &merge_info(), &error, || Ok(true))
            .await
            .expect_err("failed comment");

        assert_matches!(error.current_context(), AppError::Comment);
        assert!(format!("{error:?}").contains("Resource not accessible"));
        assert_eq!(server.await.expect("test server").len(), 3);
    }

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
                    return Err(Report::new(AppError::ApproveMerge)
                        .attach("Pull Request has merge conflicts"));
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
        assert!(
            format!("{:?}", failures.first().expect("first failure").1).contains("merge conflicts")
        );
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

                Err(Report::new(AsyncMergeError::Unconfirmed)
                    .change_context(AppError::ApproveMerge))
            },
            async |_, error| error,
            std::future::pending(),
        )
        .await;

        assert_eq!(attempted, [1]);
        assert_eq!(failures.len(), 3);
        assert!(failures.iter().skip(1).all(|(_, error)| {
            format!("{error:?}")
                .contains("Skipped because an earlier merge result is not confirmed")
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
}
