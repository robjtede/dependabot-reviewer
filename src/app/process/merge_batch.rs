//! Submit merges in order and stop submissions when a result is not confirmed.

use std::{future::Future, io};

use derive_more::Display;
use error_stack::Report;

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

#[cfg(test)]
mod tests;
