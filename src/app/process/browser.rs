use std::collections::{hash_map::Entry, HashMap, HashSet};

use semver::Version;

use crate::github::DepUpdate;

fn version(raw: &str) -> Option<Version> {
    let raw = raw.trim_start_matches('v');
    if let Ok(version) = Version::parse(raw) {
        return Some(version);
    }

    let (major, minor) = raw.split_once('.').unwrap_or((raw, "0"));
    Version::parse(&format!("{major}.{minor}.0")).ok()
}

/// For a common target, the lowest starting version spans the largest update.
pub(super) fn duplicates<'a>(
    updates: impl Iterator<Item = Option<&'a DepUpdate>>,
) -> HashSet<usize> {
    let mut representatives = HashMap::new();
    let mut skipped = HashSet::new();

    for (index, update) in updates.enumerate() {
        let Some(update) = update else { continue };
        let key = (&update.dep_type, &update.dep_name, &update.to_version);
        let start = version(&update.from_version);

        match representatives.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert((index, start));
            }
            Entry::Occupied(mut entry) => {
                let (previous, lowest) = entry.get();
                if start.as_ref().is_some_and(|v| {
                    lowest
                        .as_ref()
                        .is_none_or(|old| v.cmp_precedence(old).is_lt())
                }) {
                    skipped.insert(*previous);
                    entry.insert((index, start));
                } else {
                    skipped.insert(index);
                }
            }
        }
    }

    skipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::parse_dep_update;

    #[test]
    fn opens_the_widest_update_for_each_dependency() {
        let updates = [
            "Bump foo from 0.1.1 to 0.1.2",
            "Bump bar from 0.2.0 to 0.2.1",
            "Bump foo from 0.1.0 to 0.1.2",
        ]
        .map(|title| parse_dep_update(title, "dependabot/cargo/pkg"));

        assert_eq!(
            duplicates(updates.iter().map(Option::as_ref)),
            HashSet::from([0])
        );
    }
}
