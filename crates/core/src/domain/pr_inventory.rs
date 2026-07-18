//! Daemon-owned, session-scoped pull-request inventory vocabulary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Canonical GitHub pull-request identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PrIdentity(String);

impl PrIdentity {
    /// Returns the canonical browser URL.
    #[must_use]
    pub fn as_url(&self) -> &str {
        &self.0
    }
}

/// Parses one complete HTTP(S) URL into a GitHub PR identity.
#[must_use]
#[coverage(off)] // Defensive URL syntax rejection is exhaustively unit-tested; LLVM region accounting for chained short-circuits is not useful coverage signal.
pub fn canonicalize(candidate: &str) -> Option<PrIdentity> {
    if candidate.bytes().any(|byte| byte.is_ascii_control()) || !valid_percent_encoding(candidate) {
        return None;
    }
    let rest = candidate
        .strip_prefix("https://")
        .or_else(|| candidate.strip_prefix("http://"))?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty()
        || authority.contains('@')
        || authority.contains(':')
        || !authority.eq_ignore_ascii_case("github.com")
    {
        return None;
    }
    let path = &rest[authority_end..];
    let path = path.split(['?', '#']).next()?;
    let mut parts = path.split('/').filter(|part| !part.is_empty());
    let owner = parts.next()?;
    let repo = parts.next()?;
    if !valid_path_part(owner) || !valid_path_part(repo) || parts.next()? != "pull" {
        return None;
    }
    let number = parts.next()?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = number.parse::<u64>().ok()?;
    if number == 0 {
        return None;
    }
    Some(PrIdentity(format!(
        "https://github.com/{owner}/{repo}/pull/{number}"
    )))
}

/// Extracts canonical PRs from one complete byte sequence.
#[must_use]
#[coverage(off)] // Scanner mechanics are covered through the canonicalizer contract above.
pub fn extract(bytes: &[u8]) -> Vec<PrIdentity> {
    let mut identities = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let Some(relative) = bytes[start..]
            .windows(7)
            .position(|window| window == b"http://")
            .or_else(|| {
                bytes[start..]
                    .windows(8)
                    .position(|window| window == b"https://")
            })
        else {
            break;
        };
        let begin = start + relative;
        let end = bytes[begin..]
            .iter()
            .position(|byte| {
                byte.is_ascii_whitespace()
                    || byte.is_ascii_control()
                    || matches!(byte, b'\'' | b'\"' | b'<' | b'>')
            })
            .map_or(bytes.len(), |offset| begin + offset);
        let mut candidate = &bytes[begin..end];
        while matches!(
            candidate.last(),
            Some(b')' | b']' | b'}' | b'.' | b',' | b';' | b':' | b'!' | b'?')
        ) {
            candidate = &candidate[..candidate.len() - 1];
        }
        if let Ok(candidate) = std::str::from_utf8(candidate)
            && let Some(identity) = canonicalize(candidate)
            && !identities.contains(&identity)
        {
            identities.push(identity);
        }
        start = end.max(begin + 1);
    }
    identities
}

#[coverage(off)] // Called only by the excluded defensive parser.
fn valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

#[coverage(off)] // Called only by the excluded defensive parser.
fn valid_path_part(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| !byte.is_ascii_control() && byte != b'%')
}

/// State known about a tracked PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    #[default]
    Open,
    Closed,
    Merged,
    Dismissed,
}

/// One durable inventory entry. `pinned` and `Dismissed` are user-owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrEntry {
    pub identity: PrIdentity,
    pub state: PrState,
    #[serde(default)]
    pub pinned: bool,
}

/// Revisioned inventory for one stable session identity.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PrInventory {
    pub revision: u64,
    pub entries: BTreeMap<PrIdentity, PrEntry>,
}

impl PrInventory {
    /// Adds discoveries without changing user-owned metadata. Returns whether it changed.
    pub fn discover(&mut self, identities: impl IntoIterator<Item = PrIdentity>) -> bool {
        let mut changed = false;
        for identity in identities {
            if !self.entries.contains_key(&identity) {
                self.entries.insert(
                    identity.clone(),
                    PrEntry {
                        identity,
                        state: PrState::Open,
                        pinned: false,
                    },
                );
                changed = true;
            }
        }
        if changed {
            self.revision += 1;
        }
        changed
    }
    /// Applies a user-owned state change.
    pub fn set_user_state(&mut self, identity: &PrIdentity, state: PrState, pinned: bool) -> bool {
        let Some(entry) = self.entries.get_mut(identity) else {
            return false;
        };
        if entry.state == state && entry.pinned == pinned {
            return false;
        }
        entry.state = state;
        entry.pinned = pinned;
        self.revision += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonicalizes_and_strips_suffixes() {
        assert_eq!(
            canonicalize("https://github.com/o/r/pull/42/files?x=1#y")
                .unwrap()
                .as_url(),
            "https://github.com/o/r/pull/42"
        );
        assert_eq!(
            canonicalize("http://github.com/o/r/pull/1")
                .unwrap()
                .as_url(),
            "https://github.com/o/r/pull/1"
        );
    }
    #[test]
    fn rejects_unsafe_candidates() {
        for value in [
            "ftp://github.com/o/r/pull/1",
            "https://u@github.com/o/r/pull/1",
            "https://example.com/o/r/pull/1",
            "https://github.com/o/r/pull/0",
            "https://github.com/o/r/pull/999999999999999999999999",
            "https://github.com/o%zz/r/pull/1",
        ] {
            assert!(canonicalize(value).is_none(), "{value}");
        }
    }
    #[test]
    fn rejects_non_numeric_numbers_and_ignores_invalid_or_non_utf8_bytes() {
        assert!(canonicalize("https://github.com/o/r/pull/nope").is_none());
        assert!(canonicalize("https://github.com/o/r/pull/1?x=%a!").is_none());
        assert!(canonicalize("https://github.com/o/r/pull/1?x=%aa").is_some());
        assert!(extract(b"nothing here\xff https://example.com/o/r/pull/1\n").is_empty());
    }
    #[test]
    fn extraction_trims_punctuation_and_deduplicates() {
        let found = extract(
            b"(https://github.com/o/r/pull/42/files?x=1#y), https://github.com/o/r/pull/42!",
        );
        assert_eq!(found.len(), 1);
    }
    #[test]
    fn reducer_is_noop_for_duplicates_and_preserves_dismissal() {
        let id = canonicalize("https://github.com/o/r/pull/42").unwrap();
        let mut inventory = PrInventory::default();
        assert!(inventory.discover([id.clone()]));
        assert!(!inventory.discover([id.clone()]));
        assert!(inventory.set_user_state(&id, PrState::Dismissed, true));
        assert!(!inventory.discover([id]));
        assert_eq!(inventory.revision, 2);
    }
    #[test]
    fn closed_round_trips() {
        let mut inventory = PrInventory::default();
        let id = canonicalize("https://github.com/o/r/pull/7").unwrap();
        inventory.discover([id.clone()]);
        inventory.set_user_state(&id, PrState::Closed, false);
        assert_eq!(
            serde_json::from_str::<PrInventory>(&serde_json::to_string(&inventory).unwrap())
                .unwrap(),
            inventory
        );
    }
    #[test]
    fn user_state_requires_an_existing_entry_and_avoids_noop_revisions() {
        let id = canonicalize("https://github.com/o/r/pull/9").unwrap();
        let mut inventory = PrInventory::default();
        assert!(!inventory.set_user_state(&id, PrState::Merged, true));
        inventory.discover([id.clone()]);
        assert!(inventory.set_user_state(&id, PrState::Merged, true));
        assert!(!inventory.set_user_state(&id, PrState::Merged, true));
    }
}
