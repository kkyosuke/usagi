//! Tracks which background terminal sessions are waiting for the user.
//!
//! Two signals feed this, in priority order:
//!
//! 1. **Agent lifecycle hooks** (authoritative). Agents launched by usagi report
//!    their own [`AgentPhase`] through hooks (see
//!    [`crate::domain::settings::AgentCli::launch_command`]); the watcher reads
//!    the recorded phase and passes it in. `Waiting` means the agent finished a
//!    turn (or paused for input); `Running` means it is working again.
//! 2. **The terminal bell** (fallback). When no phase is reported — an agent
//!    without hooks (e.g. Gemini), or before its first hook fires — usagi falls
//!    back to the audible-bell heuristic: interactive agents ring the bell when
//!    they want input, so a count rising above a per-session baseline marks the
//!    session as waiting.
//!
//! [`SessionMonitor`] is the pure bookkeeping over both: per-session bell
//! baselines, the last phase seen, the set currently flagged as waiting, and
//! which session (if any) is in the foreground (attached). [`observe`] is fed
//! the latest readings each tick and returns the sessions that have *just*
//! transitioned into waiting — so the caller can fire a one-shot notification —
//! while [`waiting`] exposes the full set for rendering. All of this is free of
//! threads and IO, so the transition logic is directly testable; the live PTY
//! polling and phase-file reading that drive it live in
//! [`crate::presentation::tui::home::terminal_pool`].
//!
//! [`observe`]: SessionMonitor::observe
//! [`waiting`]: SessionMonitor::waiting

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::domain::agent_phase::AgentPhase;

/// One session's reading for a tick: its worktree path, its current bell count,
/// and the phase its agent's hooks last reported (`None` when no hook-reporting
/// agent runs there, so the bell heuristic decides).
pub type Reading = (PathBuf, u64, Option<AgentPhase>);

/// Pure tracker of per-session "waiting for input" state, keyed by worktree path.
#[derive(Debug, Default)]
pub struct SessionMonitor {
    /// The bell count last seen for each session. With no reported phase, a
    /// session is "newly waiting" when its count rises above this baseline.
    baselines: HashMap<PathBuf, u64>,
    /// The last phase reported for each session, so a carried-over `Waiting`
    /// (e.g. seen again right after detaching) is not mistaken for a fresh one.
    last_phase: HashMap<PathBuf, AgentPhase>,
    /// Sessions currently flagged as waiting for the user.
    waiting: HashSet<PathBuf>,
    /// The session the user is attached to, if any. Its bells and phases are
    /// seen live, so it never counts as waiting.
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
        self.last_phase.remove(path);
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

    /// Feed the latest [`Reading`]s (one per live session) and return the
    /// sessions that have *just* become waiting since the last call — each at
    /// most once, until it is cleared by resuming, attaching, or forgetting.
    ///
    /// The foreground (attached) session never waits — the user is looking right
    /// at it — but its baseline and last phase are kept synced so it does not
    /// fire the instant it is detached. For every other session:
    ///
    /// - A reported phase is authoritative. `Running` clears any waiting flag;
    ///   `Waiting` sets it (and fires once, unless the previous phase was already
    ///   `Waiting`, i.e. it merely carried over from being attached); `Ended`
    ///   clears it, dropping back to the bare shell.
    /// - With no phase, the bell heuristic decides: a first sighting only records
    ///   a baseline (so bells rung before monitoring began never fire), and a
    ///   later count above that baseline transitions the session into waiting.
    pub fn observe(&mut self, readings: &[Reading]) -> Vec<PathBuf> {
        let mut newly_waiting = Vec::new();
        for (path, count, phase) in readings {
            let count = *count;
            // The foreground session's signals are seen live: keep its baseline
            // and phase current and make sure it is never marked waiting.
            if self.attached.as_deref() == Some(path.as_path()) {
                self.waiting.remove(path);
                self.baselines.insert(path.clone(), count);
                if let Some(phase) = phase {
                    self.last_phase.insert(path.clone(), *phase);
                }
                continue;
            }
            match phase {
                // A working agent is not waiting; sync the bell baseline so a
                // stale bell rung while it worked never fires later.
                Some(AgentPhase::Running) => {
                    self.waiting.remove(path);
                    self.last_phase.insert(path.clone(), AgentPhase::Running);
                    self.baselines.insert(path.clone(), count);
                }
                // The agent finished a turn (or paused for input): it waits. Fire
                // a one-shot only on a genuine transition — not when the same
                // `Waiting` simply reappears after the session was detached.
                Some(AgentPhase::Waiting) => {
                    let was_waiting = self.waiting.contains(path);
                    let carried_over = self.last_phase.insert(path.clone(), AgentPhase::Waiting)
                        == Some(AgentPhase::Waiting);
                    self.waiting.insert(path.clone());
                    self.baselines.insert(path.clone(), count);
                    if !was_waiting && !carried_over {
                        newly_waiting.push(path.clone());
                    }
                }
                // The agent exited: drop any waiting flag and fall back to the
                // bare shell's bell from here on.
                Some(AgentPhase::Ended) => {
                    self.waiting.remove(path);
                    self.last_phase.insert(path.clone(), AgentPhase::Ended);
                    self.baselines.insert(path.clone(), count);
                }
                // No reported phase: the bell heuristic decides.
                None => match self.baselines.get(path) {
                    // First sighting: adopt the count as the baseline.
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
                },
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

    /// A bell-only reading (no reported phase), exercising the fallback path.
    fn bell(s: &str, count: u64) -> Reading {
        (p(s), count, None)
    }

    /// A reading with a reported agent phase (the authoritative path).
    fn phased(s: &str, count: u64, phase: AgentPhase) -> Reading {
        (p(s), count, Some(phase))
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
        let newly = monitor.observe(&[bell("/a", 3)]);
        assert!(newly.is_empty());
        assert!(monitor.waiting().is_empty());
    }

    #[test]
    fn a_rising_count_transitions_into_waiting_once() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[bell("/a", 0)]); // baseline
        let newly = monitor.observe(&[bell("/a", 1)]);
        assert_eq!(newly, vec![p("/a")]);
        assert!(monitor.is_waiting(&p("/a")));

        // A further bell does not re-fire while already waiting.
        let again = monitor.observe(&[bell("/a", 2)]);
        assert!(again.is_empty());
        assert!(monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn an_unchanged_count_does_nothing() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[bell("/a", 5)]);
        let newly = monitor.observe(&[bell("/a", 5)]);
        assert!(newly.is_empty());
        assert!(!monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn the_attached_session_never_waits_and_keeps_its_baseline_synced() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[bell("/a", 0)]);
        monitor.set_attached(Some(p("/a")));

        // Bells while attached are seen live: no waiting, baseline follows.
        let newly = monitor.observe(&[bell("/a", 4)]);
        assert!(newly.is_empty());
        assert!(!monitor.is_waiting(&p("/a")));

        // After detaching, only bells *beyond* the synced baseline fire.
        monitor.set_attached(None);
        assert!(monitor.observe(&[bell("/a", 4)]).is_empty());
        assert_eq!(monitor.observe(&[bell("/a", 5)]), vec![p("/a")]);
    }

    #[test]
    fn attaching_clears_an_existing_waiting_flag() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[bell("/a", 0)]);
        monitor.observe(&[bell("/a", 1)]);
        assert!(monitor.is_waiting(&p("/a")));
        monitor.set_attached(Some(p("/a")));
        assert!(!monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn several_sessions_are_tracked_independently() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[bell("/a", 0), bell("/b", 0)]);
        let newly = monitor.observe(&[bell("/a", 1), bell("/b", 0)]);
        assert_eq!(newly, vec![p("/a")]);
        assert!(monitor.is_waiting(&p("/a")));
        assert!(!monitor.is_waiting(&p("/b")));
    }

    #[test]
    fn forget_drops_a_dead_sessions_state() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[bell("/a", 0)]);
        monitor.observe(&[bell("/a", 1)]);
        monitor.set_attached(Some(p("/a")));
        monitor.forget(&p("/a"));
        assert!(!monitor.is_waiting(&p("/a")));
        assert!(monitor.attached.is_none());
        // A reused path starts clean: its first sighting is a fresh baseline.
        let newly = monitor.observe(&[bell("/a", 9)]);
        assert!(newly.is_empty());
    }

    #[test]
    fn forget_leaves_other_sessions_attachment_intact() {
        let mut monitor = SessionMonitor::new();
        monitor.set_attached(Some(p("/a")));
        monitor.forget(&p("/b"));
        assert_eq!(monitor.attached.as_deref(), Some(p("/a").as_path()));
    }

    // --- agent-phase (authoritative) path -----------------------------------

    #[test]
    fn a_waiting_phase_marks_the_session_and_fires_once() {
        let mut monitor = SessionMonitor::new();
        // The agent reports it is working, then that it stopped: that stop is a
        // genuine transition into waiting and fires exactly once.
        monitor.observe(&[phased("/a", 0, AgentPhase::Running)]);
        let newly = monitor.observe(&[phased("/a", 0, AgentPhase::Waiting)]);
        assert_eq!(newly, vec![p("/a")]);
        assert!(monitor.is_waiting(&p("/a")));
        // Still waiting next tick, but no re-fire.
        let again = monitor.observe(&[phased("/a", 0, AgentPhase::Waiting)]);
        assert!(again.is_empty());
        assert!(monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn a_running_phase_clears_a_prior_waiting() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[phased("/a", 0, AgentPhase::Waiting)]);
        assert!(monitor.is_waiting(&p("/a")));
        // The user replied: the agent is working again, so it no longer waits.
        let newly = monitor.observe(&[phased("/a", 0, AgentPhase::Running)]);
        assert!(newly.is_empty());
        assert!(!monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn a_phase_overrides_the_bell_and_resyncs_its_baseline() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[bell("/a", 0)]); // bell baseline 0
                                           // The agent reports it is working even though the bell count has risen:
                                           // the phase wins, so the session does not wait, and the baseline is
                                           // resynced so the elevated count never fires once the phase drops away.
        let newly = monitor.observe(&[phased("/a", 5, AgentPhase::Running)]);
        assert!(newly.is_empty());
        assert!(!monitor.is_waiting(&p("/a")));
        assert!(monitor.observe(&[bell("/a", 5)]).is_empty());
        assert!(!monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn an_ended_phase_clears_waiting_and_falls_back_to_the_bell() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[phased("/a", 7, AgentPhase::Waiting)]);
        assert!(monitor.is_waiting(&p("/a")));
        // The agent exits: it no longer waits, and the bare shell's baseline is
        // synced to the current count.
        monitor.observe(&[phased("/a", 7, AgentPhase::Ended)]);
        assert!(!monitor.is_waiting(&p("/a")));
        // From here the bell heuristic governs the bare shell again.
        assert_eq!(monitor.observe(&[bell("/a", 8)]), vec![p("/a")]);
        assert!(monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn a_waiting_phase_does_not_re_fire_just_because_a_session_was_detached() {
        let mut monitor = SessionMonitor::new();
        // While attached, the agent stops: the user sees it live, so no waiting
        // flag — but its phase is synced.
        monitor.set_attached(Some(p("/a")));
        let newly = monitor.observe(&[phased("/a", 0, AgentPhase::Waiting)]);
        assert!(newly.is_empty());
        assert!(!monitor.is_waiting(&p("/a")));
        // After detaching, the same still-waiting state marks the session for
        // rendering but does not fire a fresh notification.
        monitor.set_attached(None);
        let after = monitor.observe(&[phased("/a", 0, AgentPhase::Waiting)]);
        assert!(after.is_empty());
        assert!(monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn a_first_seen_waiting_phase_fires() {
        let mut monitor = SessionMonitor::new();
        // A detached session whose very first reading is already waiting is a
        // real transition worth surfacing.
        let newly = monitor.observe(&[phased("/a", 0, AgentPhase::Waiting)]);
        assert_eq!(newly, vec![p("/a")]);
    }
}
