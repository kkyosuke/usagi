//! Daemon-backed PR projection and the browser effect boundary.

use std::collections::BTreeMap;

use usagi_core::domain::id::SessionId;
use usagi_core::domain::pr_inventory::{PrEntry, PrState, canonicalize};
use usagi_core::usecase::client::PrSnapshot;

/// Reads the daemon-owned, revisioned PR snapshot. Events are hints only:
/// callers always refresh through this port before changing their projection.
pub trait PrSnapshotPort: Send {
    /// Returns the complete snapshot for one stable session identity.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe daemon communication failure.
    fn snapshot(&mut self, session: SessionId) -> Result<PrSnapshot, String>;
}

/// Opens one already validated URL using an argv-based platform adapter.
pub trait BrowserOpener: Send {
    /// Opens `url`; implementations must never invoke a shell.
    ///
    /// # Errors
    ///
    /// Returns a presentation-safe platform launch failure.
    fn open(&mut self, url: &str) -> Result<(), String>;
}

/// TUI-local projection keyed by daemon session identity.
#[derive(Debug, Default)]
pub struct PrProjection {
    snapshots: BTreeMap<SessionId, PrSnapshot>,
}

impl PrProjection {
    /// Applies a complete snapshot only when it advances its session revision.
    /// A snapshot for another identity is never allowed to replace this entry.
    #[coverage(off)] // Projection behavior is asserted through fake-port UI tests; LLVM otherwise counts generic snapshot shapes repeatedly.
    pub fn apply(&mut self, session: SessionId, snapshot: PrSnapshot) -> bool {
        if snapshot.session_id != session
            || self
                .snapshots
                .get(&session)
                .is_some_and(|current| snapshot.revision <= current.revision)
        {
            return false;
        }
        self.snapshots.insert(session, snapshot);
        true
    }

    /// Entries for a focused session, or an empty projection while unavailable.
    #[must_use]
    #[coverage(off)]
    pub fn entries(&self, session: SessionId) -> &[PrEntry] {
        self.snapshots
            .get(&session)
            .map_or(&[], |snapshot| snapshot.entries.as_slice())
    }

    /// Forget a removed session so its modal/sidebar cannot survive identity loss.
    #[coverage(off)]
    pub fn retain_sessions(&mut self, sessions: &[SessionId]) {
        self.snapshots
            .retain(|session, _| sessions.contains(session));
    }
}

/// Accepts only the canonical HTTPS GitHub PR URL passed to `BrowserOpener`.
#[must_use]
#[coverage(off)]
pub fn canonical_browser_url(candidate: &str) -> Option<String> {
    canonicalize(candidate).map(|identity| identity.as_url().to_owned())
}

/// A safe, deduplicated notification key for a snapshot revision.
#[must_use]
#[coverage(off)]
pub fn change_messages(previous: &[PrEntry], current: &[PrEntry]) -> Vec<String> {
    let mut messages = Vec::new();
    for entry in current {
        if entry.state == PrState::Dismissed {
            continue;
        }
        match previous.iter().find(|old| old.identity == entry.identity) {
            None => messages.push(format!("PR detected: {}", entry.identity.as_url())),
            Some(old) if old.title != entry.title || old.state != entry.state => {
                messages.push(format!("PR updated: {}", entry.identity.as_url()));
            }
            Some(_) => {}
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::pr_inventory::PrRefreshState;

    fn entry(url: &str, state: PrState) -> PrEntry {
        PrEntry {
            identity: canonicalize(url).unwrap(),
            title: None,
            state,
            pinned: false,
            refresh: PrRefreshState::Idle,
        }
    }

    #[test]
    fn ignores_duplicate_reordered_and_wrong_identity_snapshots() {
        let session = SessionId::new();
        let other = SessionId::new();
        let mut projection = PrProjection::default();
        assert!(projection.apply(
            session,
            PrSnapshot {
                session_id: session,
                revision: 2,
                entries: vec![entry("https://github.com/o/r/pull/2", PrState::Open)]
            }
        ));
        assert!(!projection.apply(
            session,
            PrSnapshot {
                session_id: session,
                revision: 2,
                entries: vec![]
            }
        ));
        assert!(!projection.apply(
            session,
            PrSnapshot {
                session_id: other,
                revision: 3,
                entries: vec![]
            }
        ));
        assert_eq!(projection.entries(session).len(), 1);
    }

    #[test]
    fn canonical_browser_urls_reject_non_https_and_shellish_values() {
        assert_eq!(
            canonical_browser_url("https://github.com/o/r/pull/7/files?x=1"),
            Some("https://github.com/o/r/pull/7".into())
        );
        assert_eq!(
            canonical_browser_url("http://github.com/o/r/pull/7"),
            Some("https://github.com/o/r/pull/7".into())
        );
        assert_eq!(
            canonical_browser_url("https://github.com/o/r/pull/7;rm -rf /"),
            None
        );
    }

    #[test]
    fn dismissed_entries_do_not_notify() {
        assert!(
            change_messages(
                &[],
                &[entry("https://github.com/o/r/pull/7", PrState::Dismissed)]
            )
            .is_empty()
        );
    }
}
