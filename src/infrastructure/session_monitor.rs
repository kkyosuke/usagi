//! Tracks each background terminal session's agent state for the home screen.
//!
//! Two signals feed this, in priority order:
//!
//! 1. **Agent lifecycle hooks** (authoritative). Agents launched by usagi report
//!    their own [`AgentPhase`] through hooks (see
//!    [`crate::domain::settings::AgentCli::launch_command`]); the watcher reads
//!    the recorded phase and passes it in. `Ready` means it just started or
//!    resumed and is idle; `Running` means it is working a turn; `Waiting` means
//!    it finished a turn (or paused for input); `Ended` means the agent process
//!    exited (the task is done).
//! 2. **The terminal bell** (fallback). When no phase is reported — an agent
//!    without hooks (e.g. Gemini), or before its first hook fires — usagi falls
//!    back to the audible-bell heuristic: interactive agents ring the bell when
//!    they want input, so a count rising above a per-session baseline marks the
//!    session as waiting.
//!
//! [`SessionMonitor`] is the pure bookkeeping over both. It keeps each session's
//! agent state — **running**, **waiting**, or **done** ([`running`] / [`waiting`]
//! / [`done`]) — which the sidebar renders; a live session that has reported none
//! of these (e.g. just launched) shows as **ready**. It also holds the
//! per-session bell baselines and which session is in the foreground (attached).
//!
//! Display vs. notification are kept separate. The displayed state always
//! reflects the agent's true phase, **including the attached session** — so a
//! session shows the same state whether the user is looking at it or has switched
//! away (this is what keeps 切替 and 没入 consistent). Being attached only
//! suppresses two things: the bell heuristic (its bells are seen live, so they
//! never mark it waiting) and the desktop notification (the user is already
//! looking at it). [`observe`] is fed the latest readings each tick and returns
//! the sessions that have *just* transitioned into waiting or done — for the
//! background ones only — so the caller can fire a one-shot notification. All of
//! this is free of threads and IO, so the transition logic is directly testable;
//! the live PTY polling and phase-file reading that drive it live in
//! [`crate::presentation::tui::home::terminal_pool`].
//!
//! [`observe`]: SessionMonitor::observe
//! [`running`]: SessionMonitor::running
//! [`waiting`]: SessionMonitor::waiting
//! [`done`]: SessionMonitor::done

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::domain::agent_phase::AgentPhase;

/// One session's reading for a tick: its worktree path, its current bell count,
/// and the phase its agent's hooks last reported (`None` when no hook-reporting
/// agent runs there, so the bell heuristic decides).
pub type Reading = (PathBuf, u64, Option<AgentPhase>);

/// A session that has *just* transitioned into a state worth notifying about,
/// returned by [`SessionMonitor::observe`] so the watcher can fire a one-shot
/// desktop notification for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// The worktree whose session transitioned.
    pub path: PathBuf,
    /// What it transitioned into.
    pub kind: NoticeKind,
}

/// The kind of transition a [`Notice`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// The agent finished a turn (or paused for input) and now awaits the user.
    Waiting,
    /// The agent process exited: the session's work is done.
    Done,
}

/// Pure tracker of per-session agent state, keyed by worktree path.
#[derive(Debug, Default)]
pub struct SessionMonitor {
    /// The bell count last seen for each session. With no reported phase, a
    /// session is "newly waiting" when its count rises above this baseline.
    baselines: HashMap<PathBuf, u64>,
    /// Sessions whose agent is actively working a turn (reported `Running`).
    running: HashSet<PathBuf>,
    /// Sessions whose agent is waiting for the user (finished a turn / paused).
    waiting: HashSet<PathBuf>,
    /// Sessions whose agent has exited (the task is done); the bare shell it ran
    /// in may still be alive.
    done: HashSet<PathBuf>,
    /// The session the user is attached to, if any. Its bells are seen live (so
    /// the bell heuristic is skipped for it) and it never fires a notification.
    attached: Option<PathBuf>,
}

impl SessionMonitor {
    /// A fresh monitor with nothing tracked.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark `path` as the foreground (attached) session, or clear the foreground
    /// with `None`.
    ///
    /// This only changes which session is exempt from the bell heuristic and from
    /// notifications; it does **not** touch the displayed state, which keeps
    /// reflecting the agent's true phase so 切替 and 没入 agree. The next
    /// [`observe`](Self::observe) reconciles a bell-only session that was flagged
    /// waiting before it was attached.
    pub fn set_attached(&mut self, path: Option<PathBuf>) {
        self.attached = path;
    }

    /// Drop all state for a session that has gone away (its shell exited), so a
    /// later session reusing the same path starts clean.
    pub fn forget(&mut self, path: &Path) {
        self.baselines.remove(path);
        self.running.remove(path);
        self.waiting.remove(path);
        self.done.remove(path);
        if self.attached.as_deref() == Some(path) {
            self.attached = None;
        }
    }

    /// The sessions whose agent is actively working a turn.
    pub fn running(&self) -> &HashSet<PathBuf> {
        &self.running
    }

    /// Whether `path`'s agent is actively working a turn.
    pub fn is_running(&self, path: &Path) -> bool {
        self.running.contains(path)
    }

    /// The sessions whose agent is waiting for the user.
    pub fn waiting(&self) -> &HashSet<PathBuf> {
        &self.waiting
    }

    /// Whether `path`'s agent is currently waiting for input.
    pub fn is_waiting(&self, path: &Path) -> bool {
        self.waiting.contains(path)
    }

    /// The sessions whose agent has finished (exited).
    pub fn done(&self) -> &HashSet<PathBuf> {
        &self.done
    }

    /// Whether `path`'s agent has finished (exited).
    pub fn is_done(&self, path: &Path) -> bool {
        self.done.contains(path)
    }

    /// Feed the latest [`Reading`]s (one per live session) and return the
    /// background sessions that have *just* transitioned into waiting or done
    /// since the last call — each at most once, until the state changes again.
    ///
    /// The displayed state always tracks the agent's true phase, **whether or not
    /// the session is attached**. Being attached only suppresses the bell
    /// heuristic (its bells are seen live) and the returned notices (the user is
    /// already looking). For each session:
    ///
    /// - A reported phase is authoritative. `Ready` clears running, waiting and
    ///   done (the session is idle, awaiting input — no notice); `Running` marks
    ///   it running and clears waiting and done; `Waiting` marks it waiting
    ///   (notifying once, unless it was already waiting); `Ended` marks it done
    ///   (notifying once, unless already done).
    /// - With no phase, the bell heuristic decides: a first sighting only records
    ///   a baseline (so bells rung before monitoring began never fire), and a
    ///   later count above that baseline transitions the session into waiting —
    ///   except while attached, where the bell is ignored and any prior bell-based
    ///   waiting is cleared.
    pub fn observe(&mut self, readings: &[Reading]) -> Vec<Notice> {
        let mut notices = Vec::new();
        for (path, count, phase) in readings {
            let count = *count;
            let attached = self.attached.as_deref() == Some(path.as_path());
            match phase {
                // The agent just started or resumed and is idle: it is neither
                // running, waiting, nor done, and there is nothing to notify. Sync
                // the bell baseline so a stale bell never fires later.
                Some(AgentPhase::Ready) => {
                    self.running.remove(path);
                    self.waiting.remove(path);
                    self.done.remove(path);
                    self.baselines.insert(path.clone(), count);
                }
                // A working agent is neither waiting nor done; sync the bell
                // baseline so a stale bell rung while it worked never fires later.
                Some(AgentPhase::Running) => {
                    self.waiting.remove(path);
                    self.done.remove(path);
                    self.running.insert(path.clone());
                    self.baselines.insert(path.clone(), count);
                }
                // The agent finished a turn (or paused for input): it waits. Fire
                // a one-shot only on a genuine transition, and never for the
                // attached session (the user is looking right at it).
                Some(AgentPhase::Waiting) => {
                    self.running.remove(path);
                    self.done.remove(path);
                    self.baselines.insert(path.clone(), count);
                    let newly = self.waiting.insert(path.clone());
                    if newly && !attached {
                        notices.push(Notice {
                            path: path.clone(),
                            kind: NoticeKind::Waiting,
                        });
                    }
                }
                // The agent exited: the task is done. Same one-shot rule.
                Some(AgentPhase::Ended) => {
                    self.running.remove(path);
                    self.waiting.remove(path);
                    self.baselines.insert(path.clone(), count);
                    let newly = self.done.insert(path.clone());
                    if newly && !attached {
                        notices.push(Notice {
                            path: path.clone(),
                            kind: NoticeKind::Done,
                        });
                    }
                }
                // No reported phase: we cannot assert the agent is working, so it
                // is not running from our side; the bell heuristic decides waiting.
                None => {
                    self.running.remove(path);
                    match self.baselines.get(path) {
                        // First sighting: adopt the count as the baseline.
                        None => {
                            self.baselines.insert(path.clone(), count);
                        }
                        Some(&base) => {
                            if attached {
                                // Seen live: the bell is not trusted, and any
                                // earlier bell-based waiting is cleared.
                                self.waiting.remove(path);
                            } else if count > base && !self.waiting.contains(path) {
                                self.done.remove(path);
                                self.waiting.insert(path.clone());
                                notices.push(Notice {
                                    path: path.clone(),
                                    kind: NoticeKind::Waiting,
                                });
                            }
                            self.baselines.insert(path.clone(), count);
                        }
                    }
                }
            }
        }
        notices
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

    fn waiting(s: &str) -> Notice {
        Notice {
            path: p(s),
            kind: NoticeKind::Waiting,
        }
    }

    fn done(s: &str) -> Notice {
        Notice {
            path: p(s),
            kind: NoticeKind::Done,
        }
    }

    #[test]
    fn new_monitor_tracks_nothing() {
        let monitor = SessionMonitor::new();
        assert!(monitor.running().is_empty());
        assert!(monitor.waiting().is_empty());
        assert!(monitor.done().is_empty());
        assert!(!monitor.is_running(&p("/a")));
        assert!(!monitor.is_waiting(&p("/a")));
        assert!(!monitor.is_done(&p("/a")));
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
        assert_eq!(newly, vec![waiting("/a")]);
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
    fn the_attached_sessions_bell_is_ignored_and_its_baseline_synced() {
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
        assert_eq!(monitor.observe(&[bell("/a", 5)]), vec![waiting("/a")]);
    }

    #[test]
    fn attaching_reconciles_a_bell_based_waiting_on_the_next_tick() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[bell("/a", 0)]);
        monitor.observe(&[bell("/a", 1)]);
        assert!(monitor.is_waiting(&p("/a")));
        // Attaching alone does not rewrite state; the next reading (its bells now
        // seen live) clears the bell-based waiting flag.
        monitor.set_attached(Some(p("/a")));
        monitor.observe(&[bell("/a", 2)]);
        assert!(!monitor.is_waiting(&p("/a")));
    }

    #[test]
    fn several_sessions_are_tracked_independently() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[bell("/a", 0), bell("/b", 0)]);
        let newly = monitor.observe(&[bell("/a", 1), bell("/b", 0)]);
        assert_eq!(newly, vec![waiting("/a")]);
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
    fn forget_drops_a_done_session_and_leaves_others_attachment_intact() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[phased("/b", 0, AgentPhase::Ended)]);
        assert!(monitor.is_done(&p("/b")));
        monitor.set_attached(Some(p("/a")));
        monitor.forget(&p("/b"));
        assert!(!monitor.is_done(&p("/b")));
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
        assert_eq!(newly, vec![waiting("/a")]);
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
        // The user replied: the agent is working again, so it no longer waits but
        // is marked running, and the transition fires no notice.
        let newly = monitor.observe(&[phased("/a", 0, AgentPhase::Running)]);
        assert!(newly.is_empty());
        assert!(!monitor.is_waiting(&p("/a")));
        assert!(monitor.is_running(&p("/a")));
    }

    #[test]
    fn a_ready_phase_marks_nothing_and_fires_no_notice() {
        let mut monitor = SessionMonitor::new();
        // A freshly started/resumed session is idle: not running, waiting, or
        // done, and silent (the user launched it themselves).
        let newly = monitor.observe(&[phased("/a", 0, AgentPhase::Ready)]);
        assert!(newly.is_empty());
        assert!(!monitor.is_running(&p("/a")));
        assert!(!monitor.is_waiting(&p("/a")));
        assert!(!monitor.is_done(&p("/a")));
    }

    #[test]
    fn a_ready_phase_clears_a_prior_running_waiting_and_done() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[phased("/a", 0, AgentPhase::Running)]);
        assert!(monitor.is_running(&p("/a")));
        // Resuming (SessionStart → ready) drops it back to idle.
        monitor.observe(&[phased("/a", 0, AgentPhase::Ready)]);
        assert!(!monitor.is_running(&p("/a")));
        assert!(!monitor.is_waiting(&p("/a")));
        assert!(!monitor.is_done(&p("/a")));
    }

    #[test]
    fn a_waiting_then_an_ended_phase_clears_running() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[phased("/a", 0, AgentPhase::Running)]);
        assert!(monitor.is_running(&p("/a")));
        // Finishing the turn clears running and marks waiting.
        monitor.observe(&[phased("/a", 0, AgentPhase::Waiting)]);
        assert!(!monitor.is_running(&p("/a")));
        assert!(monitor.is_waiting(&p("/a")));
        // Exiting clears running again (already clear) and marks done.
        monitor.observe(&[phased("/a", 0, AgentPhase::Ended)]);
        assert!(!monitor.is_running(&p("/a")));
        assert!(monitor.is_done(&p("/a")));
    }

    #[test]
    fn dropping_the_phase_clears_a_stale_running() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[phased("/a", 0, AgentPhase::Running)]);
        assert!(monitor.is_running(&p("/a")));
        // The phase file goes away (e.g. a hookless restart): with no phase we no
        // longer assert it is working, so running clears and the bell governs.
        monitor.observe(&[bell("/a", 0)]);
        assert!(!monitor.is_running(&p("/a")));
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
    fn an_ended_phase_marks_done_and_fires_once() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[phased("/a", 0, AgentPhase::Waiting)]);
        assert!(monitor.is_waiting(&p("/a")));
        // The agent exits: it is no longer waiting but done, firing exactly once.
        let newly = monitor.observe(&[phased("/a", 0, AgentPhase::Ended)]);
        assert_eq!(newly, vec![done("/a")]);
        assert!(!monitor.is_waiting(&p("/a")));
        assert!(monitor.is_done(&p("/a")));
        // Still done next tick, but no re-fire.
        let again = monitor.observe(&[phased("/a", 0, AgentPhase::Ended)]);
        assert!(again.is_empty());
        assert!(monitor.is_done(&p("/a")));
    }

    #[test]
    fn an_ended_phase_then_a_bell_falls_back_to_the_bell() {
        let mut monitor = SessionMonitor::new();
        monitor.observe(&[phased("/a", 7, AgentPhase::Ended)]);
        assert!(monitor.is_done(&p("/a")));
        // If the phase later drops away (the bare shell lives on), the bell
        // heuristic governs again: a count beyond the synced baseline waits.
        assert_eq!(monitor.observe(&[bell("/a", 8)]), vec![waiting("/a")]);
        assert!(monitor.is_waiting(&p("/a")));
        assert!(!monitor.is_done(&p("/a")));
    }

    #[test]
    fn the_attached_session_still_shows_its_phase_but_never_notifies() {
        let mut monitor = SessionMonitor::new();
        monitor.set_attached(Some(p("/a")));
        // Attached: the waiting phase is shown (so 切替 and 没入 agree) but no
        // notice fires.
        let newly = monitor.observe(&[phased("/a", 0, AgentPhase::Waiting)]);
        assert!(newly.is_empty());
        assert!(monitor.is_waiting(&p("/a")));
        // Likewise on done while attached: shown, not notified.
        let after = monitor.observe(&[phased("/a", 0, AgentPhase::Ended)]);
        assert!(after.is_empty());
        assert!(monitor.is_done(&p("/a")));
    }

    #[test]
    fn a_waiting_phase_does_not_re_fire_just_because_a_session_was_detached() {
        let mut monitor = SessionMonitor::new();
        // While attached, the agent stops: shown waiting, but no notice.
        monitor.set_attached(Some(p("/a")));
        assert!(monitor
            .observe(&[phased("/a", 0, AgentPhase::Waiting)])
            .is_empty());
        assert!(monitor.is_waiting(&p("/a")));
        // After detaching, the same still-waiting state does not fire a fresh
        // notice — it was already waiting.
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
        assert_eq!(newly, vec![waiting("/a")]);
    }

    #[test]
    fn a_first_seen_ended_phase_fires() {
        let mut monitor = SessionMonitor::new();
        let newly = monitor.observe(&[phased("/a", 0, AgentPhase::Ended)]);
        assert_eq!(newly, vec![done("/a")]);
        assert!(monitor.is_done(&p("/a")));
    }
}
