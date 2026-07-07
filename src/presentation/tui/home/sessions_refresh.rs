//! A shared handle the home event loop reads to pick up session lists that a
//! background thread re-synced from git or re-read after an external write.
//!
//! Two producers write here. Leaving an embedded pane (detaching with `Ctrl-O`,
//! or every pane exiting) re-reads each worktree's git status so the sidebar
//! reflects any commit / push / merge made inside it. That sync shells out to
//! `git status` for every worktree and waits on the cross-process state lock, so
//! it is slow precisely when several sessions are running agents — running it
//! inline would freeze the detach. The other producer is the `state.json` watcher
//! ([`run`](super::run)), which polls every workspace's file and republishes the
//! recorded sessions when an agent's MCP `session_create` / `session_delegate_issue`,
//! another usagi window, or the CLI writes it out of band.
//!
//! Both write here keyed by the **workspace root** they refreshed, so the event
//! loop can route each list to the right sidebar group — the primary workspace or
//! one of the 統合(unite) groups. Keying by root also means a refresh for one
//! workspace never clobbers a pending refresh for another: the slot holds at most
//! one list per root, newest wins. The loop takes them on the next frame and
//! applies each without yanking the cursor. Until they land, the just-left
//! statuses stay on screen.
//!
//! Mirrors [`UpdateHandle`](super::update::UpdateHandle): a cloneable
//! `Arc<Mutex<_>>` slot a background thread writes and the loop reads.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::domain::workspace_state::SessionRecord;

/// A cloneable handle onto the per-workspace session lists background work
/// produced, keyed by each workspace's root path.
///
/// Cloning shares the same underlying map, so a background thread's
/// [`set`](Self::set) is visible to the event loop that [`take_all`](Self::take_all)s
/// it. A fresh handle (the default) holds nothing — what the loop sees until a
/// refresh lands.
#[derive(Clone, Default)]
pub struct SessionsRefreshHandle {
    shared: Arc<Mutex<HashMap<PathBuf, Vec<SessionRecord>>>>,
}

impl SessionsRefreshHandle {
    /// A handle holding no pending refresh.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a freshly-produced session list for the workspace rooted at `root`
    /// (called from a background thread). A later refresh for the same root that
    /// lands before the loop has taken the previous one simply replaces it — the
    /// newest reading is the one that matters — while a refresh for a *different*
    /// root is kept alongside it.
    pub fn set(&self, root: impl Into<PathBuf>, sessions: Vec<SessionRecord>) {
        self.lock().insert(root.into(), sessions);
    }

    /// Take every pending refresh, emptying the slot, so the event loop applies
    /// each list exactly once. Empty while no refresh has landed. Each entry is a
    /// `(workspace root, sessions)` pair the loop routes to the matching sidebar
    /// group.
    pub fn take_all(&self) -> Vec<(PathBuf, Vec<SessionRecord>)> {
        self.lock().drain().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Vec<SessionRecord>>> {
        // Recover a poisoned lock rather than propagating the panic. This handle
        // is read by the TUI event loop every frame while the terminal is in raw
        // / alternate-screen mode; escalating a poison here would crash the UI
        // with the terminal left in a broken state. The same never-crash-on-
        // poison policy the terminal pool documents and relies on; the slot only
        // guards an `insert` / `drain`, so a stale reading is the worst outcome.
        self.shared.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions(name: &str) -> Vec<SessionRecord> {
        vec![SessionRecord {
            name: name.to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: std::path::PathBuf::from(format!("/repo/.usagi/sessions/{name}")),
            worktrees: Vec::new(),
            created_at: chrono::Utc::now(),
            last_active: None,
        }]
    }

    #[test]
    fn lock_recovers_from_a_poisoned_mutex_instead_of_crashing() {
        // A thread that panics while holding the lock poisons the mutex. The
        // handle must still hand back a usable guard (recovering the inner value)
        // rather than propagating the poison and crashing the TUI event loop.
        let handle = SessionsRefreshHandle::new();
        handle.set("/repo", sessions("before"));
        let clone = handle.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.shared.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        // The slot is still readable and holds the last value written.
        let taken = handle.take_all();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].1[0].name, "before");
    }

    #[test]
    fn a_fresh_handle_holds_no_refresh() {
        assert!(SessionsRefreshHandle::new().take_all().is_empty());
    }

    #[test]
    fn set_is_visible_through_a_clone_and_taken_once() {
        let handle = SessionsRefreshHandle::new();
        let writer = handle.clone();
        writer.set("/repo", sessions("main"));
        // The reader sees the writer's list, takes it once, then the slot empties.
        let taken = handle.take_all();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0, PathBuf::from("/repo"));
        assert_eq!(taken[0].1[0].name, "main");
        assert!(handle.take_all().is_empty());
    }

    #[test]
    fn a_later_set_for_the_same_root_replaces_an_untaken_one() {
        let handle = SessionsRefreshHandle::new();
        handle.set("/repo", sessions("stale"));
        handle.set("/repo", sessions("fresh"));
        let taken = handle.take_all();
        assert_eq!(taken.len(), 1, "same root keeps only the newest list");
        assert_eq!(taken[0].1[0].name, "fresh");
    }

    #[test]
    fn refreshes_for_different_roots_are_kept_side_by_side() {
        // A unite workspace's refresh must not clobber the primary's: each root
        // gets its own slot, so both are handed to the loop together.
        let handle = SessionsRefreshHandle::new();
        handle.set("/primary", sessions("a"));
        handle.set("/extra", sessions("b"));
        let mut taken = handle.take_all();
        taken.sort_by(|l, r| l.0.cmp(&r.0));
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].0, PathBuf::from("/extra"));
        assert_eq!(taken[1].0, PathBuf::from("/primary"));
    }
}
