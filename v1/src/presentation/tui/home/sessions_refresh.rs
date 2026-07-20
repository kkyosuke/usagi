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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::domain::workspace_state::SessionRecord;

/// A monotonically increasing identifier for a git sync dispatched for a
/// workspace root. The event loop applies only the latest generation for that
/// root, so a slow old sync cannot overwrite a newer reading.
pub type GitSyncGeneration = u64;

/// The current freshness/error state of a workspace root's background git sync.
#[derive(Debug, Clone)]
pub struct GitSyncState {
    pub generation: GitSyncGeneration,
    pub started_at: Option<Instant>,
    pub finished_at: Option<Instant>,
    pub status: GitSyncStatus,
    pub error: Option<String>,
}

impl GitSyncState {
    pub fn syncing(generation: GitSyncGeneration, started_at: Instant) -> Self {
        Self {
            generation,
            started_at: Some(started_at),
            finished_at: None,
            status: GitSyncStatus::Syncing,
            error: None,
        }
    }

    pub fn fresh(generation: GitSyncGeneration, finished_at: Instant) -> Self {
        Self {
            generation,
            started_at: None,
            finished_at: Some(finished_at),
            status: GitSyncStatus::Fresh,
            error: None,
        }
    }

    pub fn stale(
        generation: GitSyncGeneration,
        started_at: Option<Instant>,
        finished_at: Instant,
        error: impl Into<String>,
    ) -> Self {
        Self {
            generation,
            started_at,
            finished_at: Some(finished_at),
            status: GitSyncStatus::Stale,
            error: Some(error.into()),
        }
    }
}

/// Whether the visible git status is fresh, still being refreshed, or stale
/// because the latest refresh failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitSyncStatus {
    Fresh,
    Syncing,
    Stale,
}

/// A background git sync completion, or an external recorded-state refresh that
/// should not affect git freshness.
#[derive(Debug, Clone)]
pub enum SessionsRefresh {
    GitSync(GitSyncOutcome),
    Recorded {
        root: PathBuf,
        sessions: Vec<SessionRecord>,
    },
}

impl SessionsRefresh {
    pub fn root(&self) -> &PathBuf {
        match self {
            SessionsRefresh::GitSync(outcome) => &outcome.root,
            SessionsRefresh::Recorded { root, .. } => root,
        }
    }
}

/// Result of one generation of background git sync for a workspace root.
#[derive(Debug, Clone)]
pub struct GitSyncOutcome {
    pub root: PathBuf,
    pub generation: GitSyncGeneration,
    pub started_at: Instant,
    pub finished_at: Instant,
    pub result: Result<Vec<SessionRecord>, String>,
}

/// A cloneable handle onto the per-workspace session lists background work
/// produced, keyed by each workspace's root path.
///
/// Cloning shares the same underlying map, so a background thread's
/// [`set`](Self::set) is visible to the event loop that [`take_all`](Self::take_all)s
/// it. A fresh handle (the default) holds nothing — what the loop sees until a
/// refresh lands.
#[derive(Clone, Default)]
pub struct SessionsRefreshHandle {
    shared: Arc<Mutex<HashMap<PathBuf, SessionsRefresh>>>,
    next_generation: Arc<AtomicU64>,
}

impl SessionsRefreshHandle {
    /// A handle holding no pending refresh.
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next git-sync generation for `root` and return the state the
    /// UI should show immediately while the worker runs.
    pub fn begin_git_sync(
        &self,
        root: impl Into<PathBuf>,
    ) -> (PathBuf, GitSyncGeneration, GitSyncState) {
        let root = root.into();
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let started_at = Instant::now();
        (
            root,
            generation,
            GitSyncState::syncing(generation, started_at),
        )
    }

    /// Record a freshly-produced session list for the workspace rooted at `root`
    /// (called from a background thread). A later refresh for the same root that
    /// lands before the loop has taken the previous one simply replaces it — the
    /// newest reading is the one that matters — while a refresh for a *different*
    /// root is kept alongside it.
    pub fn set_recorded(&self, root: impl Into<PathBuf>, sessions: Vec<SessionRecord>) {
        let root = root.into();
        self.lock()
            .insert(root.clone(), SessionsRefresh::Recorded { root, sessions });
    }

    /// Backward-compatible name for recorded-state refreshes. Git-sync workers
    /// use [`complete_git_sync`](Self::complete_git_sync) so freshness is tracked.
    pub fn set(&self, root: impl Into<PathBuf>, sessions: Vec<SessionRecord>) {
        self.set_recorded(root, sessions);
    }

    /// Record a completed git sync. Later completions for the same root replace
    /// earlier untaken ones in the slot; stale generations are still rejected by
    /// `HomeState` when the event loop applies them.
    pub fn complete_git_sync(&self, outcome: GitSyncOutcome) {
        self.lock()
            .insert(outcome.root.clone(), SessionsRefresh::GitSync(outcome));
    }

    /// Take every pending refresh, emptying the slot, so the event loop applies
    /// each list exactly once. Empty while no refresh has landed. Each entry is a
    /// `(workspace root, sessions)` pair the loop routes to the matching sidebar
    /// group.
    pub fn take_all(&self) -> Vec<SessionsRefresh> {
        self.lock().drain().map(|(_, refresh)| refresh).collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, SessionsRefresh>> {
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
            todos: Vec::new(),
            decisions: Vec::new(),
            name: name.to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: std::path::PathBuf::from(format!("/repo/.usagi/sessions/{name}")),
            worktrees: Vec::new(),
            worktree_provenance: Vec::new(),
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
        handle.set_recorded("/repo", sessions("before"));
        let clone = handle.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.shared.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        // The slot is still readable and holds the last value written.
        let taken = handle.take_all();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].root(), &PathBuf::from("/repo"));
    }

    #[test]
    fn a_fresh_handle_holds_no_refresh() {
        assert!(SessionsRefreshHandle::new().take_all().is_empty());
    }

    #[test]
    fn set_is_visible_through_a_clone_and_taken_once() {
        let handle = SessionsRefreshHandle::new();
        let writer = handle.clone();
        writer.set_recorded("/repo", sessions("main"));
        // The reader sees the writer's list, takes it once, then the slot empties.
        let taken = handle.take_all();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].root(), &PathBuf::from("/repo"));
        assert!(handle.take_all().is_empty());
    }

    #[test]
    fn a_later_set_for_the_same_root_replaces_an_untaken_one() {
        let handle = SessionsRefreshHandle::new();
        handle.set_recorded("/repo", sessions("stale"));
        handle.set_recorded("/repo", sessions("fresh"));
        let taken = handle.take_all();
        assert_eq!(taken.len(), 1, "same root keeps only the newest list");
        assert_eq!(taken[0].root(), &PathBuf::from("/repo"));
    }

    #[test]
    fn refreshes_for_different_roots_are_kept_side_by_side() {
        // A unite workspace's refresh must not clobber the primary's: each root
        // gets its own slot, so both are handed to the loop together.
        let handle = SessionsRefreshHandle::new();
        handle.set_recorded("/primary", sessions("a"));
        handle.set_recorded("/extra", sessions("b"));
        let mut taken = handle.take_all();
        taken.sort_by(|l, r| l.root().cmp(r.root()));
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].root(), &PathBuf::from("/extra"));
        assert_eq!(taken[1].root(), &PathBuf::from("/primary"));
    }

    #[test]
    fn complete_git_sync_publishes_a_git_outcome() {
        let handle = SessionsRefreshHandle::new();
        let (root, generation, _) = handle.begin_git_sync("/repo");
        let started_at = std::time::Instant::now();
        handle.complete_git_sync(GitSyncOutcome {
            root,
            generation,
            started_at,
            finished_at: started_at,
            result: Ok(sessions("fresh")),
        });

        let taken = handle.take_all();
        assert_eq!(taken.len(), 1);
        assert!(matches!(taken[0], SessionsRefresh::GitSync(_)));
        assert_eq!(taken[0].root(), &PathBuf::from("/repo"));
    }

    #[test]
    fn begin_git_sync_allocates_increasing_generations() {
        let handle = SessionsRefreshHandle::new();
        let (_, first, first_state) = handle.begin_git_sync("/repo");
        let (_, second, second_state) = handle.begin_git_sync("/repo");
        assert!(first < second);
        assert_eq!(first_state.status, GitSyncStatus::Syncing);
        assert_eq!(second_state.generation, second);
    }
}
