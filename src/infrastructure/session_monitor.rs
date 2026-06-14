//! Tracks which background terminal sessions are waiting for the user.
//!
//! Each embedded session ([`crate::infrastructure::pty::PtySession`]) keeps a
//! running count of the audible bells its shell has emitted. Interactive agents
//! ring the bell when they finish a turn and want input, so a rising count is
//! the signal that a session needs attention.
//!
//! [`SessionMonitor`] is the pure bookkeeping over those counts: it remembers a
//! per-session baseline, the set of sessions currently flagged as waiting, and
//! which session (if any) is in the foreground (attached). [`observe`] is fed
//! the latest counts each tick and returns the sessions that have *just*
//! transitioned into waiting — so the caller can fire a one-shot notification —
//! while [`waiting`] exposes the full set for rendering. All of this is free of
//! threads and IO, so the transition logic is directly testable; the live PTY
//! polling that drives it lives in [`crate::infrastructure::terminal_manager`].
//!
//! [`observe`]: SessionMonitor::observe
//! [`waiting`]: SessionMonitor::waiting

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Pure tracker of per-session "waiting for input" state, keyed by worktree path.
#[derive(Debug, Default)]
pub struct SessionMonitor {
    /// The bell count last seen for each session. A session is "newly waiting"
    /// when its current count rises above this baseline.
    baselines: HashMap<PathBuf, u64>,
    /// Sessions currently flagged as waiting for the user.
    waiting: HashSet<PathBuf>,
    /// The session the user is attached to, if any. Its bells are seen live, so
    /// it never counts as waiting.
    attached: Option<PathBuf>,
}

impl SessionMonitor {
    /// A fresh monitor with nothing tracked.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark `path` as the foreground (attached) session, or clear the foreground
    /// with `None`. The attached session is removed from the waiting set
    /// immediately — the user is looking right at it.
    pub fn set_attached(&mut self, path: Option<PathBuf>) {
        if let Some(path) = path.as_ref() {
            self.waiting.remove(path);
        }
        self.attached = path;
    }

    /// Drop all state for a session that has gone away (its shell exited), so a
    /// later session reusing the same path starts clean.
    pub fn forget(&mut self, path: &Path) {
        self.baselines.remove(path);
        self.waiting.remove(path);
        if self.attached.as_deref() == Some(path) {
            self.attached = None;
        }
    }

    /// The sessions currently waiting for the user.
    pub fn waiting(&self) -> &HashSet<PathBuf> {
        &self.waiting
    }

    /// Whether `path` is currently flagged as waiting.
    pub fn is_waiting(&self, path: &Path) -> bool {
        self.waiting.contains(path)
    }

    /// Feed the latest bell counts (one `(path, count)` per live session) and
    /// return the sessions that have *just* become waiting since the last call —
    /// each at most once, until it is cleared by attaching or forgetting.
    ///
    /// A session seen for the first time only records its baseline (so bells
    /// rung before monitoring began never fire), the attached session keeps its
    /// baseline synced without ever waiting, and any other session whose count
    /// has risen above its baseline transitions into waiting.
    pub fn observe(&mut self, readings: &[(PathBuf, u64)]) -> Vec<PathBuf> {
        let mut newly_waiting = Vec::new();
        for (path, count) in readings {
            let count = *count;
            // The foreground session's bells are seen live: keep its baseline
            // current and make sure it is never marked waiting.
            if self.attached.as_deref() == Some(path.as_path()) {
                self.waiting.remove(path);
                self.baselines.insert(path.clone(), count);
                continue;
            }
            match self.baselines.get(path) {
                // First sighting: adopt the count as the baseline, no transition.
                None => {
                    self.baselines.insert(path.clone(), count);
                }
                Some(&base) => {
                    if count > base && !self.waiting.contains(path) {
                        self.waiting.insert(path.clone());
                        newly_waiting.push(path.clone());
                    }
                    self.baselines.insert(path.clone(), count);
                }
            }
        }
        newly_waiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn new_monitor_tracks_nothing() {
        let monitor = SessionMonitor::new();
        assert!(monitor.waiting().is_empty());
        assert!(!monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn first_sighting_only_records_a_baseline() {
        let mut monitor = SessionMonitor::new();
        // Even a non-zero count on first sight does not fire: those bells
        // predate monitoring.
        let newly = monitor.observe(&[(p("/a"), 3)]);
        assert!(newly.is_empty());
        assert!(monitor.waiting().is_empty());
    }

    #[test]
    fn a_rising_count_transitions_into_waiting_once() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[(p("/a"), 0)]); // baseline
        let newly = monitor.observe(&[(p("/a"), 1)]);
        assert_eq!(newly, vec![p("/a")]);
        assert!(monitor.is_waiting(&p("/a")));

        // A further bell does not re-fire while already waiting.
        let again = monitor.observe(&[(p("/a"), 2)]);
        assert!(again.is_empty());
        assert!(monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn an_unchanged_count_does_nothing() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[(p("/a"), 5)]);
        let newly = monitor.observe(&[(p("/a"), 5)]);
        assert!(newly.is_empty());
        assert!(!monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn the_attached_session_never_waits_and_keeps_its_baseline_synced() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[(p("/a"), 0)]);
        monitor.set_attached(Some(p("/a")));

        // Bells while attached are seen live: no waiting, baseline follows.
        let newly = monitor.observe(&[(p("/a"), 4)]);
        assert!(newly.is_empty());
        assert!(!monitor.is_waiting(&p("/a")));

        // After detaching, only bells *beyond* the synced baseline fire.
        monitor.set_attached(None);
        assert!(monitor.observe(&[(p("/a"), 4)]).is_empty());
        assert_eq!(monitor.observe(&[(p("/a"), 5)]), vec![p("/a")]);
    }

    #[test]
    fn attaching_clears_an_existing_waiting_flag() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[(p("/a"), 0)]);
        monitor.observe(&[(p("/a"), 1)]);
        assert!(monitor.is_waiting(&p("/a")));
        monitor.set_attached(Some(p("/a")));
        assert!(!monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn several_sessions_are_tracked_independently() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[(p("/a"), 0), (p("/b"), 0)]);
        let newly = monitor.observe(&[(p("/a"), 1), (p("/b"), 0)]);
        assert_eq!(newly, vec![p("/a")]);
        assert!(monitor.is_waiting(&p("/a")));
        assert!(!monitor.is_waiting(&p("/b")));
    }

    #[test]
    fn forget_drops_a_dead_sessions_state() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[(p("/a"), 0)]);
        monitor.observe(&[(p("/a"), 1)]);
        monitor.set_attached(Some(p("/a")));
        monitor.forget(&p("/a"));
        assert!(!monitor.is_waiting(&p("/a")));
        assert!(monitor.attached.is_none());
        // A reused path starts clean: its first sighting is a fresh baseline.
        let newly = monitor.observe(&[(p("/a"), 9)]);
        assert!(newly.is_empty());
    }

    #[test]
    fn forget_leaves_other_sessions_attachment_intact() {
        let mut monitor = SessionMonitor::new();
        monitor.set_attached(Some(p("/a")));
        monitor.forget(&p("/b"));
        assert_eq!(monitor.attached.as_deref(), Some(p("/a").as_path()));
    }
}
