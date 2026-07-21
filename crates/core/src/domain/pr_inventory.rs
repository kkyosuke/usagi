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

/// Refresh lifecycle exposed as safe, credential-free snapshot metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrRefreshState {
    #[default]
    Idle,
    Pending,
    BackingOff,
}

/// One durable inventory entry. `pinned` and `Dismissed` are user-owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrEntry {
    pub identity: PrIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub state: PrState,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub refresh: PrRefreshState,
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
                        title: None,
                        state: PrState::Open,
                        pinned: false,
                        refresh: PrRefreshState::Pending,
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

    /// Applies the safe subset returned by `gh pr view`. User-owned entries
    /// are deliberately left untouched by automatic refreshes.
    pub fn apply_refresh(
        &mut self,
        identity: &PrIdentity,
        title: Option<String>,
        state: PrState,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(identity) else {
            return false;
        };
        if entry.pinned || entry.state == PrState::Dismissed {
            return false;
        }
        if entry.title == title && entry.state == state && entry.refresh == PrRefreshState::Idle {
            return false;
        }
        entry.title = title;
        entry.state = state;
        entry.refresh = PrRefreshState::Idle;
        self.revision += 1;
        true
    }

    /// Records a retryable refresh failure without discarding the last known
    /// title or state. This is observational metadata, not an inventory revision.
    pub fn mark_refresh_backoff(&mut self, identity: &PrIdentity) {
        if let Some(entry) = self.entries.get_mut(identity)
            && !entry.pinned
            && entry.state != PrState::Dismissed
        {
            entry.refresh = PrRefreshState::BackingOff;
        }
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
            "https://github.com/o/r/issues/1",
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
    #[test]
    fn refresh_updates_once_and_never_overwrites_user_owned_entries() {
        let id = canonicalize("https://github.com/o/r/pull/9").unwrap();
        let mut inventory = PrInventory::default();
        inventory.discover([id.clone()]);
        assert!(inventory.apply_refresh(&id, Some("closed work".into()), PrState::Closed));
        assert_eq!(inventory.revision, 2);
        assert!(!inventory.apply_refresh(&id, Some("closed work".into()), PrState::Closed));
        assert!(inventory.set_user_state(&id, PrState::Dismissed, true));
        assert!(!inventory.apply_refresh(&id, Some("merged work".into()), PrState::Merged));
        assert_eq!(inventory.entries[&id].title.as_deref(), Some("closed work"));
    }
    #[test]
    fn refresh_rejects_unknown_or_pinned_entries_and_backoff_keeps_user_entries() {
        let known = canonicalize("https://github.com/o/r/pull/10").unwrap();
        let missing = canonicalize("https://github.com/o/r/pull/11").unwrap();
        let mut inventory = PrInventory::default();
        assert!(!inventory.apply_refresh(&missing, None, PrState::Open));
        inventory.discover([known.clone()]);
        assert!(inventory.set_user_state(&known, PrState::Open, true));
        assert!(!inventory.apply_refresh(&known, Some("ignored".into()), PrState::Closed));
        inventory.mark_refresh_backoff(&known);
        assert_eq!(inventory.entries[&known].refresh, PrRefreshState::Pending);
        assert!(inventory.set_user_state(&known, PrState::Dismissed, false));
        inventory.mark_refresh_backoff(&known);
        assert_eq!(inventory.entries[&known].refresh, PrRefreshState::Pending);
    }
    #[test]
    fn refresh_failure_marks_non_user_entry_without_revising_inventory() {
        let id = canonicalize("https://github.com/o/r/pull/12").unwrap();
        let mut inventory = PrInventory::default();
        inventory.discover([id.clone()]);
        let revision = inventory.revision;
        inventory.mark_refresh_backoff(&id);
        assert_eq!(inventory.entries[&id].refresh, PrRefreshState::BackingOff);
        assert_eq!(inventory.revision, revision);
    }
}
