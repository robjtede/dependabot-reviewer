use std::time::Duration;

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

pub(crate) struct AsyncMerge<'a> {
    octocrab: &'a Octocrab,
    poll_interval: Duration,
    wait_timeout: Duration,
}

impl<'a> AsyncMerge<'a> {
    pub(crate) fn new(octocrab: &'a Octocrab) -> Self {
        Self {
            octocrab,
            poll_interval: Duration::from_secs(2),
            wait_timeout: Duration::from_secs(120),
        }
    }

    pub(crate) async fn merge(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        sha: &str,
        operation: MergeOperation,
    ) -> Result<MergeOutcome, Report<AppError>> {
        let route = format!("/repos/{owner}/{repo}/pulls/{pr_number}/merge-async");
        let (merge_action, merge_method) = match operation {
            MergeOperation::Direct(method) => ("direct_merge", Some(rest_merge_method(method))),
            MergeOperation::Queue => ("merge_queue", None),
        };
        let payload = MergeRequest {
            sha,
            merge_action,
            merge_method,
            bypass_rules: false,
        };

        tokio::time::timeout(self.wait_timeout, self.submit_and_wait(&route, &payload))
            .await
            .unwrap_or_else(|_| {
                Err(Report::new(AsyncMergeError::Unconfirmed)
                    .change_context(AppError::ApproveMerge)
                    .attach(format!(
                        "Merge result for PR #{pr_number} was not confirmed within {} seconds. Check the pull request before trying again.",
                        self.wait_timeout.as_secs(),
                    )))
            })
    }

    async fn submit_and_wait(
        &self,
        route: &str,
        payload: &MergeRequest<'_>,
    ) -> Result<MergeOutcome, Report<AppError>> {
        let request = self.build_request(Method::PUT, route, Some(payload))?;
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

        let mut result = MergeResponse::from_response(response)
            .await
            .change_context(AppError::ApproveMerge)
            .attach(AsyncMergeError::Unconfirmed)?;
        let mut request_id = None;

        loop {
            match result {
                MergeResponse::Merged {} => return Ok(MergeOutcome::Merged),
                MergeResponse::Enqueued {} => return Ok(MergeOutcome::Enqueued),
                MergeResponse::Failed { message } => {
                    return Err(Report::new(AsyncMergeError::Failed { message })
                        .change_context(AppError::ApproveMerge));
                }
                MergeResponse::Pending(details) => {
                    if details.uuid.is_empty()
                        || details.expected_head_sha != payload.sha
                        || details.merge_action != payload.merge_action
                        || details.bypass_rules
                        || payload
                            .merge_method
                            .is_some_and(|method| details.merge_method.as_deref() != Some(method))
                        || request_id.as_ref().is_some_and(|id| id != &details.uuid)
                    {
                        return Err(Report::new(AsyncMergeError::Unconfirmed)
                            .change_context(AppError::ApproveMerge)
                            .attach("A pending merge request has different options. Check the pull request before trying again."));
                    }

                    let poll_route = format!("{route}/{}", details.uuid);
                    request_id = Some(details.uuid);

                    tokio::time::sleep(self.poll_interval).await;

                    let request = self
                        .build_request(Method::GET, &poll_route, None)
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
                        .attach(format!(
                            "Could not confirm the merge result at {poll_route}"
                        ))?;

                    result = MergeResponse::from_response(response)
                        .await
                        .change_context(AppError::ApproveMerge)
                        .attach(AsyncMergeError::Unconfirmed)?;
                }
            }
        }
    }

    fn build_request(
        &self,
        method: Method,
        route: &str,
        payload: Option<&MergeRequest<'_>>,
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

#[derive(Serialize)]
struct MergeRequest<'a> {
    sha: &'a str,
    merge_action: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_method: Option<&'a str>,
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

fn rest_merge_method(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "merge",
        MergeMethod::Squash => "squash",
        MergeMethod::Rebase => "rebase",
        _ => "merge",
    }
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    use super::*;

    #[tokio::test]
    async fn timeout_leaves_the_merge_result_unconfirmed() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener");
        let address = listener.local_addr().expect("test address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("merge request");
            let mut buffer = [0; 4096];

            assert_ne!(
                socket.read(&mut buffer).await.expect("read merge request"),
                0,
            );

            let body = r#"{"status":"pending","details":{"uuid":"request-id","merge_method":"merge","merge_action":"direct_merge","expected_head_sha":"head","bypass_rules":false}}"#;
            let response = format!("HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());

            socket
                .write_all(response.as_bytes())
                .await
                .expect("pending response");
        });
        let octocrab = Octocrab::builder()
            .base_uri(format!("http://{address}"))
            .expect("test URI")
            .build()
            .expect("test client");
        let merge = AsyncMerge {
            octocrab: &octocrab,
            poll_interval: Duration::from_secs(2),
            wait_timeout: Duration::from_millis(100),
        };

        let error = merge
            .merge(
                "example",
                "repo",
                12,
                "head",
                MergeOperation::Direct(MergeMethod::Merge),
            )
            .await
            .expect_err("merge deadline");

        assert!(matches!(
            error.downcast_ref::<AsyncMergeError>(),
            Some(AsyncMergeError::Unconfirmed)
        ));
        assert!(format!("{error:?}").contains("was not confirmed within"));
        server.await.expect("test server");
    }
}
