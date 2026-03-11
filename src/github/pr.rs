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
    pub base_ref_name: String,
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
    let to_version_part = &versions_part[to_index + " to ".len()..];
    let to_version_part_lower = &versions_part_lower[to_index + " to ".len()..];
    let to_version = strip_dependabot_location_suffix(to_version_part, to_version_part_lower)
        .trim()
        .to_string();

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

fn strip_dependabot_location_suffix<'a>(to_version: &'a str, to_version_lower: &str) -> &'a str {
    if let Some(path_index) = to_version_lower.find(" in /") {
        &to_version[..path_index]
    } else {
        to_version
    }
}

#[cfg(test)]
mod tests {
    use super::parse_dep_update;

    #[test]
    fn parses_standard_cargo_title() {
        let parsed = parse_dep_update(
            "build(deps): bump tokio from 1.49.0 to 1.50.0",
            "dependabot/cargo/tokio-1.50.0",
        )
        .expect("expected dependency update to parse");

        assert_eq!(parsed.dep_type, "cargo");
        assert_eq!(parsed.dep_name, "tokio");
        assert_eq!(parsed.to_version, "1.50.0");
    }

    #[test]
    fn strips_workspace_suffix_from_to_version() {
        let parsed = parse_dep_update(
            "chore(deps): bump quinn-proto from 0.11.9 to 0.11.14 in /examples",
            "dependabot/cargo/quinn-proto-0.11.14",
        )
        .expect("expected dependency update to parse");

        assert_eq!(parsed.dep_type, "cargo");
        assert_eq!(parsed.dep_name, "quinn-proto");
        assert_eq!(parsed.to_version, "0.11.14");
    }

    #[test]
    fn maps_github_actions_to_actions_type() {
        let parsed = parse_dep_update(
            "Bump actions/setup-node from 4 to 5",
            "dependabot/github_actions/actions/setup-node-5",
        )
        .expect("expected dependency update to parse");

        assert_eq!(parsed.dep_type, "actions");
        assert_eq!(parsed.dep_name, "actions/setup-node");
        assert_eq!(parsed.to_version, "5");
    }

    #[test]
    fn maps_npm_and_yarn_to_npm_type() {
        let parsed = parse_dep_update(
            "build(deps): bump vite from 5.4.19 to 5.4.20",
            "dependabot/npm_and_yarn/vite-5.4.20",
        )
        .expect("expected dependency update to parse");

        assert_eq!(parsed.dep_type, "npm");
        assert_eq!(parsed.dep_name, "vite");
        assert_eq!(parsed.to_version, "5.4.20");
    }

    #[test]
    fn returns_none_for_non_bump_titles() {
        let parsed = parse_dep_update(
            "docs: update changelog for release",
            "dependabot/cargo/tokio-1.50.0",
        );

        assert!(parsed.is_none());
    }
}
