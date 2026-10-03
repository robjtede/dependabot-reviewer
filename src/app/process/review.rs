//! Fetch a review batch and apply the GitHub Actions lockfile guard.

use std::collections::HashMap;

use console::style;
use error_stack::{Report, ResultExt as _};
use futures_buffered::BufferedStreamExt;
use futures_util::StreamExt as _;

use crate::{
    app::{approval_workflow::MergeQueueStatus, state::ReviewState, App},
    error::AppError,
    github::{CiStatus, PrInfo},
};

pub(super) struct ReviewItem {
    pub(super) repo: String,
    pub(super) owner: String,
    pub(super) repo_name: String,
    pub(super) pr: PrInfo,
    pub(super) actions_lock_check: Option<Result<bool, Report<AppError>>>,
}

impl ReviewItem {
    pub(super) fn previously_reviewed(&self, state: &ReviewState) -> bool {
        self.pr
            .dep_update
            .as_ref()
            .is_some_and(|update| state.is_previously_reviewed(update))
    }
}

pub(super) struct ReviewBatch {
    pub(super) items: Vec<ReviewItem>,
    pub(super) pending_statuses: HashMap<String, HashMap<u64, MergeQueueStatus>>,
}

impl ReviewBatch {
    pub(super) async fn fetch(app: &App, repos: &[String]) -> Result<Self, Report<AppError>> {
        println!("Fetching PR details for {} repositories", repos.len());

        let mut items = Vec::new();
        for repo in repos {
            let (owner, repo_name) = repo
                .split_once('/')
                .ok_or_else(|| Report::new(AppError::InvalidInput))
                .attach_with(|| format!("Invalid repo format: {}", repo))?;

            let prs = app.fetch_dependabot_prs_for_repo(repo).await?;
            items.extend(prs.into_iter().map(|pr| ReviewItem {
                repo: repo.clone(),
                owner: owner.to_string(),
                repo_name: repo_name.to_string(),
                pr,
                actions_lock_check: None,
            }));
        }

        items.sort_by(|a, b| {
            a.repo
                .cmp(&b.repo)
                .then_with(|| b.pr.number.cmp(&a.pr.number))
        });

        let pending_statuses = app.fetch_pending_review_statuses(&items).await?;
        let mut lockfiles = HashMap::new();

        for item in &mut items {
            item.actions_lock_check =
                Some(has_actions_lock(&app.octocrab, item, &mut lockfiles).await);
        }

        Ok(Self {
            items,
            pending_statuses,
        })
    }

    pub(super) async fn approval_items(
        self,
        app: &App,
    ) -> Result<Vec<ReviewItem>, Report<AppError>> {
        let mut lockfiles = HashMap::new();
        let mut eligible = Vec::new();

        for item in self.items {
            if has_actions_lock(&app.octocrab, &item, &mut lockfiles).await? {
                println!(
                    "  {} Skipping {}#{}: .github/workflows/actions.lock exists on {}; Dependabot cannot update this lockfile.",
                    style("⊘").yellow(), item.repo, item.pr.number, item.pr.base_ref_name,
                );
            } else {
                eligible.push(item);
            }
        }

        Ok(eligible)
    }
}

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

async fn has_actions_lock(
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

#[cfg(test)]
mod tests;
