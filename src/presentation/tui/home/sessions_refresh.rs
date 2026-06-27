//! A shared handle the home event loop reads to pick up a session list that a
//! background thread re-synced from git.
//!
//! Leaving an embedded pane (detaching with `Ctrl-O`, or every pane exiting)
//! re-reads each worktree's git status so the sidebar reflects any commit / push
//! / merge made inside it. That sync shells out to `git status` for every
//! worktree and waits on the cross-process state lock, so it is slow precisely
//! when several sessions are running agents — running it inline would freeze the
//! detach. Instead it runs on its own thread and writes the refreshed list here;
//! the event loop takes it on the next frame and applies it without yanking the
//! cursor. Until it lands, the just-left statuses stay on screen.
//!
//! Mirrors [`UpdateHandle`](super::update::UpdateHandle): a cloneable
//! `Arc<Mutex<_>>` slot a background thread writes once and the loop reads.

use std::sync::{Arc, Mutex};

use crate::domain::workspace_state::SessionRecord;

/// A cloneable handle onto the session list a background sync produced.
///
/// Cloning shares the same underlying slot, so the background thread's
/// [`set`](Self::set) is visible to the event loop that [`take`](Self::take)s it.
/// A fresh handle (the default) holds nothing — what the loop sees until a sync
/// completes.
#[derive(Clone, Default)]
pub struct SessionsRefreshHandle {
    shared: Arc<Mutex<Option<Vec<SessionRecord>>>>,
}

impl SessionsRefreshHandle {
    /// A handle holding no pending refresh.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a freshly-synced session list (called from the background thread).
    /// A later sync that lands before the loop has taken the previous one simply
    /// replaces it — the newest reading is the one that matters.
    pub fn set(&self, sessions: Vec<SessionRecord>) {
        *self.lock() = Some(sessions);
    }

    /// Take the pending refresh, leaving the slot empty, so the event loop
    /// applies each synced list exactly once. `None` while no sync has landed.
    pub fn take(&self) -> Option<Vec<SessionRecord>> {
        self.lock().take()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Vec<SessionRecord>>> {
        // Recover a poisoned lock rather than propagating the panic. This handle
        // is read by the TUI event loop every frame while the terminal is in raw
        // / alternate-screen mode; escalating a poison here would crash the UI
        // with the terminal left in a broken state. The same never-crash-on-
        // poison policy the terminal pool documents and relies on; the slot only
        // guards a `replace` / `take`, so a stale reading is the worst outcome.
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
        handle.set(sessions("before"));
        let clone = handle.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.shared.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        // The slot is still readable and holds the last value written.
        let taken = handle.take();
        assert_eq!(taken.as_ref().map(|s| s[0].name.as_str()), Some("before"));
    }

    #[test]
    fn a_fresh_handle_holds_no_refresh() {
        assert!(SessionsRefreshHandle::new().take().is_none());
    }

    #[test]
    fn set_is_visible_through_a_clone_and_taken_once() {
        let handle = SessionsRefreshHandle::new();
        let writer = handle.clone();
        writer.set(sessions("main"));
        // The reader sees the writer's list, takes it once, then the slot empties.
        let taken = handle.take().expect("a refresh was set");
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].name, "main");
        assert!(handle.take().is_none());
    }

    #[test]
    fn a_later_set_replaces_an_untaken_one() {
        let handle = SessionsRefreshHandle::new();
        handle.set(sessions("stale"));
        handle.set(sessions("fresh"));
        let taken = handle.take().expect("a refresh was set");
        assert_eq!(taken[0].name, "fresh");
    }
}
