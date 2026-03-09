use std::{
    cmp::Ordering,
    fs,
    io::{ErrorKind, Write as _},
    path::{Path, PathBuf},
};

use error_stack::{Report, ResultExt as _};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{error::AppError, github::DepUpdate};

const STATE_FILE_NAME: &str = "state.toml";
const APP_CONFIG_DIR: &str = "dependabot-reviewer";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ReviewState {
    #[serde(default)]
    entries: Vec<ReviewEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReviewEntry {
    dep_type: String,
    dep_name: String,
    highest_approved_version: String,
}

impl ReviewState {
    pub(crate) fn load_from_default_path() -> Result<(Self, PathBuf), Report<AppError>> {
        let config_root = dirs::config_dir()
            .ok_or_else(|| Report::new(AppError::Initialization))
            .attach("Unable to resolve user config directory")?;
        let path = config_root.join(APP_CONFIG_DIR).join(STATE_FILE_NAME);

        let state = Self::load_from_path(&path)?;
        Ok((state, path))
    }

    fn load_from_path(path: &Path) -> Result<Self, Report<AppError>> {
        match fs::read_to_string(path) {
            Ok(content) => toml::from_str::<Self>(&content)
                .change_context(AppError::Initialization)
                .attach_with(|| format!("Invalid TOML in {}", path.display())),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(Report::new(AppError::Initialization))
                .attach_with(|| format!("Failed to read {}", path.display()))
                .attach(err.to_string()),
        }
    }

    pub(crate) fn save_to_path(&self, path: &Path) -> Result<(), Report<AppError>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .change_context(AppError::Initialization)
                .attach_with(|| format!("Failed to create {}", parent.display()))?;
        }

        let payload = toml::to_string_pretty(self)
            .change_context(AppError::Initialization)
            .attach("Failed to serialize reviewer state")?;

        let temp_path = path.with_extension("toml.tmp");
        let mut temp_file = fs::File::create(&temp_path)
            .change_context(AppError::Initialization)
            .attach_with(|| format!("Failed to create {}", temp_path.display()))?;
        temp_file
            .write_all(payload.as_bytes())
            .change_context(AppError::Initialization)
            .attach_with(|| format!("Failed to write {}", temp_path.display()))?;

        fs::rename(&temp_path, path)
            .change_context(AppError::Initialization)
            .attach_with(|| {
                format!(
                    "Failed to replace {} with {}",
                    path.display(),
                    temp_path.display()
                )
            })?;

        Ok(())
    }

    pub(crate) fn highest_approved_for(&self, dep_type: &str, dep_name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.dep_type == dep_type && entry.dep_name == dep_name)
            .map(|entry| entry.highest_approved_version.as_str())
    }

    pub(crate) fn is_previously_reviewed(&self, dep_update: &DepUpdate) -> bool {
        let Some(highest) = self.highest_approved_for(&dep_update.dep_type, &dep_update.dep_name)
        else {
            return false;
        };

        is_reviewed_or_older(&dep_update.to_version, highest)
    }

    pub(crate) fn record_approved(&mut self, dep_update: &DepUpdate) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.dep_type == dep_update.dep_type && entry.dep_name == dep_update.dep_name
        }) {
            if compare_versions(&dep_update.to_version, &entry.highest_approved_version)
                != Some(Ordering::Less)
            {
                entry.highest_approved_version = dep_update.to_version.clone();
            }
            return;
        }

        self.entries.push(ReviewEntry {
            dep_type: dep_update.dep_type.clone(),
            dep_name: dep_update.dep_name.clone(),
            highest_approved_version: dep_update.to_version.clone(),
        });
    }
}

fn is_reviewed_or_older(candidate: &str, approved_highest: &str) -> bool {
    match compare_versions(candidate, approved_highest) {
        Some(Ordering::Less) | Some(Ordering::Equal) => true,
        Some(Ordering::Greater) => false,
        None => normalize_version(candidate) == normalize_version(approved_highest),
    }
}

fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let a = parse_version(a)?;
    let b = parse_version(b)?;
    Some(a.cmp(&b))
}

fn parse_version(raw: &str) -> Option<Version> {
    let normalized = normalize_version(raw);
    Version::parse(&normalized).ok()
}

fn normalize_version(version: &str) -> String {
    version.trim().trim_start_matches('v').to_string()
}
