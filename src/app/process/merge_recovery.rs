//! Report merge failures and offer a Dependabot rebase for current conflicts.

use std::io::IsTerminal as _;

use dialoguer::{theme::ColorfulTheme, Confirm};
use error_stack::{Report, ResultExt as _};

use super::{
    approval::MergeInfo, approval_view::finish_cancelled_merge, merge_batch::MergeSkipped,
    status_rows::PrStatusRows,
};
use crate::{
    app::{async_merge::AsyncMergeError, merge_results::MergeWaitCancelled, App},
    error::AppError,
};

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

pub(super) async fn offer_conflict_rebase(
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

#[cfg(test)]
mod tests;
