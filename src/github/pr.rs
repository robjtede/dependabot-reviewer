use crate::github::CiStatus;

#[derive(Debug, Clone)]
pub struct DepUpdate {
    pub dep_type: String,
    pub dep_name: String,
    pub to_version: String,
}

#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub url: String,
    #[allow(dead_code)]
    pub head_sha: String,
    pub ci_status: CiStatus,
    pub dep_update: Option<DepUpdate>,
}

pub fn parse_dep_update(title: &str, head_ref: &str) -> Option<DepUpdate> {
    // Handles common Dependabot titles such as:
    // "build(deps): bump tokio from 1.0.0 to 1.44.1"
    // "Bump actions/setup-node from 3 to 4"
    let lower = title.to_ascii_lowercase();
    let bump_index = lower.find("bump ")?;
    let bump_part = &title[bump_index + "bump ".len()..];
    let bump_part_lower = &lower[bump_index + "bump ".len()..];

    let from_index = bump_part_lower.find(" from ")?;
    let dep_name = bump_part[..from_index].trim().to_string();
    let versions_part = &bump_part[from_index + " from ".len()..];
    let versions_part_lower = &bump_part_lower[from_index + " from ".len()..];

    let to_index = versions_part_lower.find(" to ")?;
    let from_version = versions_part[..to_index].trim();
    let to_version = versions_part[to_index + " to ".len()..].trim().to_string();

    if dep_name.is_empty() || from_version.is_empty() || to_version.is_empty() {
        return None;
    }

    let dep_type = infer_dep_type(head_ref).to_string();

    Some(DepUpdate {
        dep_type,
        dep_name,
        to_version,
    })
}

fn infer_dep_type(head_ref: &str) -> &str {
    // Dependabot branches usually look like:
    // dependabot/<ecosystem>/<dependency>-<version>
    let ecosystem = head_ref
        .strip_prefix("dependabot/")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default();

    match ecosystem {
        "cargo" => "cargo",
        "github_actions" => "actions",
        "npm_and_yarn" | "npm" | "yarn" | "pnpm" => "npm",
        _ => "unknown",
    }
}
