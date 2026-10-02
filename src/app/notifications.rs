use std::{collections::HashSet, io::IsTerminal as _};

use dialoguer::{theme::ColorfulTheme, Confirm};
use error_stack::Report;
use octocrab::{models::NotificationId, Octocrab, Page};
use serde::Deserialize;

use super::App;
use crate::error::AppError;

#[derive(Deserialize)]
struct Notification {
    id: NotificationId,
    subject: NotificationSubject,
}

#[derive(Deserialize)]
struct NotificationSubject {
    #[serde(rename = "type")]
    kind: String,
    url: Option<String>,
}

#[derive(Default)]
struct NotificationCleanup {
    removed: usize,
    errors: Vec<Report<AppError>>,
}

impl App {
    pub(crate) async fn offer_notification_cleanup(&self, pr_urls: &HashSet<String>) {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return;
        }

        self.cleanup_notifications(pr_urls, || {
            Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!(
                    "Remove GitHub notifications for {} PR(s) from this run (mark as done)?",
                    pr_urls.len()
                ))
                .default(false)
                .interact()
        })
        .await;
    }

    async fn cleanup_notifications(
        &self,
        pr_urls: &HashSet<String>,
        confirm: impl FnOnce() -> dialoguer::Result<bool>,
    ) {
        if self.cli.dry_run || pr_urls.is_empty() {
            return;
        }

        match confirm() {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                eprintln!("Could not confirm notification cleanup: {error}");
                return;
            }
        }

        let cleanup = clear_pr_notifications(&self.octocrab, pr_urls).await;

        println!("Marked {} GitHub notification(s) as done.", cleanup.removed);

        for error in cleanup.errors {
            eprintln!("Could not remove all PR notifications: {error:?}");
        }
    }
}

async fn clear_pr_notifications(
    octocrab: &Octocrab,
    pr_urls: &HashSet<String>,
) -> NotificationCleanup {
    let mut cleanup = NotificationCleanup::default();

    if pr_urls.is_empty() {
        return cleanup;
    }

    let mut thread_ids = HashSet::new();

    let mut page = match octocrab
        .get::<Page<Notification>, _, _>(
            "/notifications",
            Some(&[("all", "true"), ("per_page", "100")]),
        )
        .await
    {
        Ok(page) => page,
        Err(error) => {
            cleanup.errors.push(
                Report::new(error)
                    .change_context(AppError::GitHubApi)
                    .attach("Failed to list GitHub notifications. Notification access requires a supported token with the notifications or repo scope."),
            );
            return cleanup;
        }
    };

    // Removing threads during pagination can shift later pages and skip matches.
    loop {
        thread_ids.extend(
            page.items
                .into_iter()
                .filter(|notification| {
                    notification.subject.kind == "PullRequest"
                        && notification
                            .subject
                            .url
                            .as_deref()
                            .is_some_and(|url| pr_urls.contains(url))
                })
                .map(|notification| notification.id),
        );

        match octocrab.get_page::<Notification>(&page.next).await {
            Ok(Some(next_page)) => page = next_page,
            Ok(None) => break,
            Err(error) => {
                cleanup.errors.push(
                    Report::new(error)
                        .change_context(AppError::GitHubApi)
                        .attach("Failed to fetch the next notification page"),
                );
                break;
            }
        }
    }

    for thread_id in thread_ids {
        let route = format!("/notifications/threads/{thread_id}");
        let result = async {
            let response = octocrab._delete(route.as_str(), None::<&()>).await?;
            octocrab::map_github_error(response).await.map(drop)
        }
        .await;

        match result {
            Ok(()) => cleanup.removed += 1,
            Err(error) => cleanup.errors.push(
                Report::new(error)
                    .change_context(AppError::GitHubApi)
                    .attach(format!("Failed to mark notification {thread_id} as done")),
            ),
        }
    }

    cleanup
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    use super::*;

    fn test_app(octocrab: Octocrab) -> App {
        use clap::Parser as _;

        App {
            cli: crate::cli::Cli::try_parse_from(["dependabot-reviewer", "--repo", "example/repo"])
                .expect("test CLI"),
            octocrab,
        }
    }

    async fn notification_test_client(
        responses: Vec<(u16, String, String)>,
    ) -> (Octocrab, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test address");

        let server = tokio::spawn(async move {
            let mut requests = Vec::new();

            if responses.is_empty() {
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), listener.accept())
                        .await
                        .is_err(),
                    "cleanup must not send a request"
                );
            }

            for (status, headers, body) in responses {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut request = Vec::new();
                let mut buffer = [0; 1024];

                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).await.expect("read request");
                    assert_ne!(count, 0, "request ended before headers");
                    request.extend_from_slice(buffer.get(..count).expect("received bytes"));
                }

                requests.push(String::from_utf8(request).expect("UTF-8 request"));

                let headers = headers.replace("{address}", &address.to_string());
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }

            requests
        });

        let octocrab = Octocrab::builder()
            .base_uri(format!("http://{address}"))
            .expect("test URI")
            .add_retry_config(octocrab::service::middleware::retry::RetryConfig::None)
            .build()
            .expect("test client");

        (octocrab, server)
    }

    fn pr_urls() -> HashSet<String> {
        ["https://api.github.com/repos/example/repo/pulls/12".to_owned()]
            .into_iter()
            .collect()
    }

    fn notification(id: u64, kind: &str, url: &str) -> String {
        format!(r#"{{"id":"{id}","unread":false,"subject":{{"type":"{kind}","url":"{url}"}}}}"#)
    }

    #[tokio::test]
    async fn removes_only_matching_pr_threads_after_fetching_all_pages() {
        let selected = notification(
            1,
            "PullRequest",
            "https://api.github.com/repos/example/repo/pulls/12",
        );
        let other_repo = notification(
            2,
            "PullRequest",
            "https://api.github.com/repos/example/other/pulls/12",
        );
        let issue = notification(
            3,
            "Issue",
            "https://api.github.com/repos/example/repo/pulls/12",
        );
        let second_match = notification(
            4,
            "PullRequest",
            "https://api.github.com/repos/example/repo/pulls/12",
        );
        let missing_url = r#"{"id":"5","subject":{"type":"PullRequest","url":null}}"#;
        let (octocrab, server) = notification_test_client(vec![
            (
                200,
                "Link: <http://{address}/notifications?all=true&per_page=100&page=2>; rel=\"next\"\r\n".to_owned(),
                format!("[{selected},{other_repo},{issue},{missing_url}]"),
            ),
            (200, String::new(), format!("[{selected},{second_match}]")),
            (204, String::new(), String::new()),
            (204, String::new(), String::new()),
        ])
        .await;

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            clear_pr_notifications(&octocrab, &pr_urls()),
        )
        .await
        .expect("cleanup deadline");

        assert_eq!(result.removed, 2);
        assert!(result.errors.is_empty());

        let requests = server.await.expect("test server");
        assert!(requests
            .first()
            .expect("list request")
            .starts_with("GET /notifications?all=true&per_page=100 HTTP/1.1"));
        assert!(requests
            .get(1)
            .expect("next page")
            .starts_with("GET /notifications?all=true&per_page=100&page=2 HTTP/1.1"));

        let deletions = requests
            .iter()
            .skip(2)
            .map(|request| request.lines().next().expect("request line"))
            .collect::<HashSet<_>>();
        assert_eq!(
            deletions,
            HashSet::from([
                "DELETE /notifications/threads/1 HTTP/1.1",
                "DELETE /notifications/threads/4 HTTP/1.1",
            ])
        );
    }

    #[tokio::test]
    async fn continues_removing_threads_after_a_delete_failure() {
        let first = notification(
            1,
            "PullRequest",
            "https://api.github.com/repos/example/repo/pulls/12",
        );
        let second = notification(
            2,
            "PullRequest",
            "https://api.github.com/repos/example/repo/pulls/12",
        );
        let (octocrab, server) = notification_test_client(vec![
            (200, String::new(), format!("[{first},{second}]")),
            (403, String::new(), r#"{"message":"Forbidden"}"#.to_owned()),
            (204, String::new(), String::new()),
        ])
        .await;

        let result = clear_pr_notifications(&octocrab, &pr_urls()).await;

        assert_eq!(result.removed, 1);
        assert_eq!(result.errors.len(), 1);
        assert!(format!("{:?}", result.errors).contains("Forbidden"));
        assert_eq!(server.await.expect("test server").len(), 3);
    }

    #[tokio::test]
    async fn removes_known_matches_when_a_later_page_fails() {
        let first = notification(
            1,
            "PullRequest",
            "https://api.github.com/repos/example/repo/pulls/12",
        );
        let (octocrab, server) = notification_test_client(vec![
            (
                200,
                "Link: <http://{address}/notifications?page=2>; rel=\"next\"\r\n".to_owned(),
                format!("[{first}]"),
            ),
            (
                500,
                String::new(),
                r#"{"message":"Page failed"}"#.to_owned(),
            ),
            (204, String::new(), String::new()),
        ])
        .await;

        let result = clear_pr_notifications(&octocrab, &pr_urls()).await;

        assert_eq!(result.removed, 1);
        assert_eq!(result.errors.len(), 1);
        assert!(format!("{:?}", result.errors).contains("Page failed"));
        assert!(server
            .await
            .expect("test server")
            .last()
            .expect("delete request")
            .starts_with("DELETE /notifications/threads/1 HTTP/1.1"));
    }

    #[tokio::test]
    async fn reports_an_unsupported_token_without_removing_threads() {
        let (octocrab, server) = notification_test_client(vec![(
            403,
            String::new(),
            r#"{"message":"Resource not accessible by personal access token"}"#.to_owned(),
        )])
        .await;

        let result = clear_pr_notifications(&octocrab, &pr_urls()).await;

        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.removed, 0);
        assert_eq!(server.await.expect("test server").len(), 1);
    }

    #[tokio::test]
    async fn declining_cleanup_does_not_fetch_or_remove_notifications() {
        let (octocrab, server) = notification_test_client(Vec::new()).await;
        let app = test_app(octocrab);

        app.cleanup_notifications(&pr_urls(), || Ok(false)).await;

        assert!(server.await.expect("test server").is_empty());
    }

    #[tokio::test]
    async fn dry_runs_do_not_prompt_or_remove_notifications() {
        let (octocrab, server) = notification_test_client(Vec::new()).await;
        let mut app = test_app(octocrab);
        app.cli.dry_run = true;

        app.cleanup_notifications(&pr_urls(), || {
            panic!("dry runs must not prompt for notification cleanup")
        })
        .await;

        assert!(server.await.expect("test server").is_empty());
    }

    #[tokio::test]
    async fn runs_without_processed_prs_do_not_prompt_or_remove_notifications() {
        let (octocrab, server) = notification_test_client(Vec::new()).await;
        let app = test_app(octocrab);

        app.cleanup_notifications(&HashSet::new(), || {
            panic!("runs without processed PRs must not prompt for notification cleanup")
        })
        .await;

        assert!(server.await.expect("test server").is_empty());
    }

    #[tokio::test]
    async fn accepting_cleanup_marks_the_matching_thread_as_done() {
        let selected = notification(
            1,
            "PullRequest",
            "https://api.github.com/repos/example/repo/pulls/12",
        );
        let (octocrab, server) = notification_test_client(vec![
            (200, String::new(), format!("[{selected}]")),
            (204, String::new(), String::new()),
        ])
        .await;
        let app = test_app(octocrab);

        app.cleanup_notifications(&pr_urls(), || Ok(true)).await;

        assert!(server
            .await
            .expect("test server")
            .last()
            .expect("delete request")
            .starts_with("DELETE /notifications/threads/1 HTTP/1.1"));
    }

    #[tokio::test]
    async fn missing_notifications_do_not_send_delete_requests() {
        let (octocrab, server) =
            notification_test_client(vec![(200, String::new(), "[]".to_owned())]).await;
        let app = test_app(octocrab);

        app.cleanup_notifications(&pr_urls(), || Ok(true)).await;

        assert_eq!(server.await.expect("test server").len(), 1);
    }
}
