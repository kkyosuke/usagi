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

    /// Returns the stable `owner/repository` label embedded in the canonical URL.
    #[must_use]
    pub fn repository(&self) -> &str {
        self.0
            .strip_prefix("https://github.com/")
            .and_then(|path| path.split_once("/pull/"))
            .map_or("unknown/unknown", |(repository, _)| repository)
    }

    /// Returns the pull-request number embedded in the canonical URL.
    #[must_use]
    pub fn number(&self) -> u64 {
        self.0
            .rsplit('/')
            .next()
            .and_then(|number| number.parse().ok())
            .unwrap_or(0)
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

/// Whether `byte` ends a candidate token.
///
/// An incremental caller carries the bytes after the last such byte into its
/// next scan, so this predicate is the shared boundary rule rather than one
/// duplicated per caller.
#[must_use]
pub fn is_candidate_terminator(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || byte.is_ascii_control()
        || matches!(byte, b'\'' | b'\"' | b'<' | b'>')
}

/// The longest candidate prefix a detection can need.
///
/// [`canonicalize`] reads only `https://github.com/<owner>/<repo>/pull/<number>`
/// and ignores every later path segment, query and fragment, and [`extract`]
/// already canonicalizes a trailing token that the buffer cut short. An
/// incremental caller therefore never needs to carry more than this many bytes
/// to join a candidate split across two buffers. GitHub bounds a login at 39 and
/// a repository name at 100 characters, so the worst prefix that can canonicalize
/// is about 185 bytes; this leaves generous headroom without making the carry
/// unbounded.
pub const CANDIDATE_PREFIX_MAX: usize = 512;

/// Extracts canonical PRs from one complete byte sequence.
///
/// A candidate is one scheme-delimited *token*: it starts at a scheme and ends
/// at the first [candidate terminator](is_candidate_terminator). A token that
/// fails [`canonicalize`] is not re-scanned for a scheme embedded inside it, so a
/// PR URL carried as another URL's query parameter is deliberately not a
/// detection.
#[must_use]
pub fn extract(bytes: &[u8]) -> Vec<PrIdentity> {
    let mut identities = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        // Both schemes are searched in one pass for whichever occurs *earliest*.
        // Scanning the whole buffer for `http://` first and only then for
        // `https://` skipped every `https://` candidate in front of a later
        // `http://`, because `"https://"` does not contain `"http://"`.
        let Some(relative) = (start..bytes.len()).position(|index| {
            bytes[index..].starts_with(b"http://") || bytes[index..].starts_with(b"https://")
        }) else {
            break;
        };
        let begin = start + relative;
        let end = bytes[begin..]
            .iter()
            .position(|byte| is_candidate_terminator(*byte))
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

/// Aggregate status of the checks reported by GitHub for a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrChecksState {
    Passing,
    Failing,
    Pending,
}

/// Review decision reported by GitHub for a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
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
    /// Whether GitHub reports this PR as a draft.
    #[serde(default)]
    pub draft: bool,
    /// Aggregate CI/check state. `None` means GitHub returned no checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<PrChecksState>,
    /// Aggregate review decision. `None` means GitHub has no decision yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<PrReviewDecision>,
    /// Whether the detector saw the URL as a standalone announcement rather
    /// than as a reference embedded in prose.
    #[serde(default)]
    pub auto_open: bool,
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
        self.discover_with_auto_open(identities.into_iter().map(|identity| (identity, true)))
    }

    /// Adds classified discoveries. A later standalone announcement may promote
    /// an existing reference, but automatic refresh never changes this bit.
    pub fn discover_with_auto_open(
        &mut self,
        identities: impl IntoIterator<Item = (PrIdentity, bool)>,
    ) -> bool {
        let mut changed = false;
        for (identity, auto_open) in identities {
            if let Some(entry) = self.entries.get_mut(&identity) {
                if auto_open && !entry.auto_open {
                    entry.auto_open = true;
                    changed = true;
                }
            } else {
                self.entries.insert(
                    identity.clone(),
                    PrEntry {
                        identity,
                        title: None,
                        state: PrState::Open,
                        pinned: false,
                        refresh: PrRefreshState::Pending,
                        draft: false,
                        checks: None,
                        review: None,
                        auto_open,
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
        draft: bool,
        checks: Option<PrChecksState>,
        review: Option<PrReviewDecision>,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(identity) else {
            return false;
        };
        if entry.pinned || entry.state == PrState::Dismissed {
            return false;
        }
        if entry.title == title
            && entry.state == state
            && entry.refresh == PrRefreshState::Idle
            && entry.draft == draft
            && entry.checks == checks
            && entry.review == review
        {
            return false;
        }
        entry.title = title;
        entry.state = state;
        entry.refresh = PrRefreshState::Idle;
        entry.draft = draft;
        entry.checks = checks;
        entry.review = review;
        self.revision += 1;
        true
    }

    /// Records a retryable refresh failure without discarding the last known
    /// title or state. Refresh metadata is part of the public snapshot, so it
    /// advances the same revision fence as every other visible field.
    pub fn mark_refresh_backoff(&mut self, identity: &PrIdentity) -> bool {
        if let Some(entry) = self.entries.get_mut(identity)
            && !entry.pinned
            && entry.state != PrState::Dismissed
            && entry.refresh != PrRefreshState::BackingOff
        {
            entry.refresh = PrRefreshState::BackingOff;
            self.revision += 1;
            return true;
        }
        false
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
    fn extracted(bytes: &[u8]) -> Vec<String> {
        extract(bytes)
            .iter()
            .map(|identity| identity.as_url().to_owned())
            .collect()
    }
    #[test]
    fn extraction_finds_either_scheme_whichever_comes_first() {
        // A later plain-http URL used to hide every https candidate in front of
        // it, because the scan looked for `http://` across the whole buffer
        // before it looked for `https://` at all.
        assert_eq!(
            extracted(b"https://github.com/o/r/pull/1 and http://example.com/x"),
            ["https://github.com/o/r/pull/1"]
        );
        assert_eq!(
            extracted(b"http://example.com/x and https://github.com/o/r/pull/1"),
            ["https://github.com/o/r/pull/1"]
        );
        assert_eq!(
            extracted(b"http://github.com/o/r/pull/2 then https://github.com/o/r/pull/1"),
            [
                "https://github.com/o/r/pull/2",
                "https://github.com/o/r/pull/1"
            ]
        );
    }
    #[test]
    fn extraction_keeps_order_and_dedupes_across_interleaved_non_github_urls() {
        assert_eq!(
            extracted(
                b"http://localhost:3000/ https://github.com/o/r/pull/7\n\
                  http://ci.example.com/job/1 https://example.com/o/r/pull/8\n\
                  https://github.com/o/r/pull/9 http://x/ https://github.com/o/r/pull/7\n"
            ),
            vec![
                "https://github.com/o/r/pull/7",
                "https://github.com/o/r/pull/9"
            ]
        );
    }
    #[test]
    fn extraction_treats_a_glued_scheme_as_one_rejected_token() {
        // One token, not two candidates: the inner URL is a component of an
        // outer non-GitHub URL, so it is not a PR the agent produced.
        assert!(extract(b"http://https://github.com/o/r/pull/1").is_empty());
        assert!(extract(b"http://x.com/r?u=https://github.com/o/r/pull/1").is_empty());
        // A trailing truncated scheme is not a candidate either.
        assert!(extract(b"tail https:/").is_empty());
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
        assert!(inventory.apply_refresh(
            &id,
            Some("closed work".into()),
            PrState::Closed,
            false,
            None,
            None
        ));
        assert_eq!(inventory.revision, 2);
        assert!(!inventory.apply_refresh(
            &id,
            Some("closed work".into()),
            PrState::Closed,
            false,
            None,
            None
        ));
        assert!(inventory.set_user_state(&id, PrState::Dismissed, true));
        assert!(!inventory.apply_refresh(
            &id,
            Some("merged work".into()),
            PrState::Merged,
            false,
            None,
            None
        ));
        assert_eq!(inventory.entries[&id].title.as_deref(), Some("closed work"));
    }
    #[test]
    fn refresh_rejects_unknown_or_pinned_entries_and_backoff_keeps_user_entries() {
        let known = canonicalize("https://github.com/o/r/pull/10").unwrap();
        let missing = canonicalize("https://github.com/o/r/pull/11").unwrap();
        let mut inventory = PrInventory::default();
        assert!(!inventory.apply_refresh(&missing, None, PrState::Open, false, None, None));
        inventory.discover([known.clone()]);
        assert!(inventory.set_user_state(&known, PrState::Open, true));
        assert!(!inventory.apply_refresh(
            &known,
            Some("ignored".into()),
            PrState::Closed,
            false,
            None,
            None
        ));
        inventory.mark_refresh_backoff(&known);
        assert_eq!(inventory.entries[&known].refresh, PrRefreshState::Pending);
        assert!(inventory.set_user_state(&known, PrState::Dismissed, false));
        inventory.mark_refresh_backoff(&known);
        assert_eq!(inventory.entries[&known].refresh, PrRefreshState::Pending);
    }
    #[test]
    fn refresh_failure_marks_non_user_entry_and_revises_the_public_snapshot() {
        let id = canonicalize("https://github.com/o/r/pull/12").unwrap();
        let mut inventory = PrInventory::default();
        inventory.discover([id.clone()]);
        let revision = inventory.revision;
        inventory.mark_refresh_backoff(&id);
        assert_eq!(inventory.entries[&id].refresh, PrRefreshState::BackingOff);
        assert_eq!(inventory.revision, revision + 1);
    }

    #[test]
    fn identity_exposes_repository_number_and_reference_can_be_promoted() {
        let id = canonicalize("https://github.com/acme/widgets/pull/42").unwrap();
        assert_eq!(id.repository(), "acme/widgets");
        assert_eq!(id.number(), 42);
        let mut inventory = PrInventory::default();
        assert!(inventory.discover_with_auto_open([(id.clone(), false)]));
        assert!(!inventory.entries[&id].auto_open);
        assert!(inventory.discover_with_auto_open([(id.clone(), true)]));
        assert!(inventory.entries[&id].auto_open);
        assert_eq!(inventory.revision, 2);
    }

    #[test]
    fn refresh_publishes_draft_checks_and_review_metadata() {
        let id = canonicalize("https://github.com/o/r/pull/13").unwrap();
        let mut inventory = PrInventory::default();
        inventory.discover([id.clone()]);
        assert!(inventory.apply_refresh(
            &id,
            Some("ready".into()),
            PrState::Open,
            true,
            Some(PrChecksState::Passing),
            Some(PrReviewDecision::Approved),
        ));
        let entry = &inventory.entries[&id];
        assert!(entry.draft);
        assert_eq!(entry.checks, Some(PrChecksState::Passing));
        assert_eq!(entry.review, Some(PrReviewDecision::Approved));
    }
}
