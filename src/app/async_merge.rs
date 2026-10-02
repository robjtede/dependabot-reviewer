use derive_more::Display;
use error_stack::{Report, ResultExt as _};
use http::{HeaderValue, Method, Request, StatusCode};
use octocrab::{params::pulls::MergeMethod, FromResponse as _, OctoBody, Octocrab};
use serde::{Deserialize, Serialize};

use crate::error::AppError;

#[derive(Debug, Display)]
pub(crate) enum AsyncMergeError {
    #[display("{message}")]
    Failed { message: String },

    #[display("Merge result is not confirmed; GitHub may still process the request")]
    Unconfirmed,
}

impl_more::impl_leaf_error!(AsyncMergeError);

#[derive(Clone, Copy)]
pub(crate) enum MergeOperation {
    Direct(MergeMethod),
    Queue,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MergeOutcome {
    Merged,
    Enqueued,
}

#[derive(Debug)]
pub(crate) enum MergeStatus {
    Pending(PendingMergeRequest),
    Complete(MergeOutcome),
}

#[derive(Debug)]
pub(crate) struct PendingMergeRequest {
    route: String,
    uuid: String,
    options: MergeRequest,
}

impl PendingMergeRequest {
    pub(crate) fn progress_message(&self) -> &'static str {
        match self.options.merge_action {
            "merge_queue" => "Joining merge queue",
            _ => "Merging",
        }
    }
}

pub(crate) struct AsyncMerge<'a> {
    octocrab: &'a Octocrab,
}

impl<'a> AsyncMerge<'a> {
    pub(crate) fn new(octocrab: &'a Octocrab) -> Self {
        Self { octocrab }
    }

    pub(crate) async fn start(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        sha: &str,
        operation: MergeOperation,
    ) -> Result<MergeStatus, Report<AppError>> {
        let route = format!("/repos/{owner}/{repo}/pulls/{pr_number}/merge-async");
        let (merge_action, merge_method) = match operation {
            MergeOperation::Direct(method) => ("direct_merge", Some(rest_merge_method(method))),
            MergeOperation::Queue => ("merge_queue", None),
        };
        let options = MergeRequest {
            sha: sha.to_owned(),
            merge_action,
            merge_method,
            bypass_rules: false,
        };
        let request = self.build_request(Method::PUT, &route, Some(&options))?;
        let response = self
            .octocrab
            .execute(request)
            .await
            .change_context(AppError::ApproveMerge)
            .attach(AsyncMergeError::Unconfirmed)
            .attach("Could not confirm whether GitHub accepted the merge request")?;

        let status = response.status();
        if !matches!(
            status,
            StatusCode::OK | StatusCode::ACCEPTED | StatusCode::CONFLICT | StatusCode::BAD_REQUEST
        ) {
            let error = match octocrab::map_github_error(response).await {
                Err(error) => Report::new(error).change_context(AppError::ApproveMerge),
                Ok(_) => Report::new(AppError::ApproveMerge)
                    .attach(format!("Unexpected merge response status: {status}")),
            };

            return Err(if !status.is_client_error() {
                error.attach(AsyncMergeError::Unconfirmed)
            } else {
                error
            });
        }

        let result = MergeResponse::from_response(response)
            .await
            .change_context(AppError::ApproveMerge)
            .attach(AsyncMergeError::Unconfirmed)?;

        match result {
            MergeResponse::Pending(details) => {
                validate_pending(&details, &options, None)?;

                Ok(MergeStatus::Pending(PendingMergeRequest {
                    route,
                    uuid: details.uuid,
                    options,
                }))
            }
            result => completed_result(result).map(MergeStatus::Complete),
        }
    }

    pub(crate) async fn poll(
        &self,
        pending: &PendingMergeRequest,
    ) -> Result<Option<MergeOutcome>, Report<AppError>> {
        let route = format!("{}/{}", pending.route, pending.uuid);
        let request = self
            .build_request(Method::GET, &route, None)
            .attach(AsyncMergeError::Unconfirmed)?;
        let response = self
            .octocrab
            .execute(request)
            .await
            .change_context(AppError::ApproveMerge)
            .attach(AsyncMergeError::Unconfirmed)?;
        let response = octocrab::map_github_error(response)
            .await
            .change_context(AppError::ApproveMerge)
            .attach(AsyncMergeError::Unconfirmed)
            .attach(format!("Could not confirm the merge result at {route}"))?;
        let result = MergeResponse::from_response(response)
            .await
            .change_context(AppError::ApproveMerge)
            .attach(AsyncMergeError::Unconfirmed)?;

        match result {
            MergeResponse::Pending(details) => {
                validate_pending(&details, &pending.options, Some(&pending.uuid))?;

                Ok(None)
            }
            result => completed_result(result).map(Some),
        }
    }

    fn build_request(
        &self,
        method: Method,
        route: &str,
        payload: Option<&MergeRequest>,
    ) -> Result<Request<OctoBody>, Report<AppError>> {
        let mut request = self
            .octocrab
            .build_request(Request::builder().method(method).uri(route), payload)
            .change_context(AppError::ApproveMerge)?;

        // Replace the API version that Octocrab adds when it builds the request.
        request.headers_mut().insert(
            "x-github-api-version",
            HeaderValue::from_static("2026-03-10"),
        );

        Ok(request)
    }
}

#[derive(Debug, Serialize)]
struct MergeRequest {
    sha: String,
    merge_action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_method: Option<&'static str>,
    bypass_rules: bool,
}

#[derive(Deserialize)]
#[serde(tag = "status", content = "details", rename_all = "snake_case")]
enum MergeResponse {
    Pending(PendingMerge),
    Merged {},
    Enqueued {},
    Failed { message: String },
}

#[derive(Deserialize)]
struct PendingMerge {
    uuid: String,
    merge_method: Option<String>,
    merge_action: String,
    expected_head_sha: String,
    bypass_rules: bool,
}

fn validate_pending(
    details: &PendingMerge,
    options: &MergeRequest,
    request_id: Option<&str>,
) -> Result<(), Report<AppError>> {
    if details.uuid.is_empty()
        || details.expected_head_sha != options.sha
        || details.merge_action != options.merge_action
        || details.bypass_rules
        || options
            .merge_method
            .is_some_and(|method| details.merge_method.as_deref() != Some(method))
        || request_id.is_some_and(|id| id != details.uuid)
    {
        return Err(Report::new(AsyncMergeError::Unconfirmed)
            .change_context(AppError::ApproveMerge)
            .attach("A pending merge request has different options. Check the pull request before trying again."));
    }

    Ok(())
}

fn completed_result(result: MergeResponse) -> Result<MergeOutcome, Report<AppError>> {
    match result {
        MergeResponse::Merged {} => Ok(MergeOutcome::Merged),
        MergeResponse::Enqueued {} => Ok(MergeOutcome::Enqueued),
        MergeResponse::Failed { message } => {
            Err(Report::new(AsyncMergeError::Failed { message })
                .change_context(AppError::ApproveMerge))
        }
        MergeResponse::Pending(_) => {
            Err(Report::new(AsyncMergeError::Unconfirmed).change_context(AppError::ApproveMerge))
        }
    }
}

fn rest_merge_method(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "merge",
        MergeMethod::Squash => "squash",
        MergeMethod::Rebase => "rebase",
        _ => "merge",
    }
}
