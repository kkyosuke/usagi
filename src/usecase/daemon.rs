//! Daemon lifecycle orchestration: start / stop / status, and the daemon's own
//! self-registration.
//!
//! usagi's daemon is a single per-machine process that (in later work) will own
//! the agent PTYs and session monitoring so agents keep running after the TUI
//! closes. This module owns the *control plane* around that process: deciding
//! whether one is already running, asking a running one to stop, and letting a
//! freshly launched daemon claim the single-instance slot. The process table and
//! the actual process spawn are injected (`alive` / `spawn`) so every decision
//! here is unit-tested without a real daemon.
//!
//! Concurrency: the record read-modify-writes take the same [`StoreLock`] on the
//! daemon directory that the daemon's own [`register`] takes, so a `start`'s
//! liveness check, a `stop`, and a daemon claiming the slot never interleave.
//! [`start`] releases the lock *before* spawning so the child can register.
//!
//! [`StoreLock`]: crate::infrastructure::store_lock::StoreLock

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::domain::agent_phase::AgentPhase;
use crate::domain::daemon::{classify, DaemonState, SessionActivity, SessionSnapshot};
use crate::infrastructure::daemon_sessions_store;
use crate::infrastructure::daemon_store::{self, DaemonRecord};
use crate::infrastructure::store_lock::StoreLock;

/// Outcome of [`start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// No live daemon was recorded, so a new one was spawned.
    Started,
    /// A live daemon is already recorded; nothing was spawned.
    AlreadyRunning { pid: u32 },
}

/// Outcome of [`stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// A live daemon was asked to stop.
    Stopping { pid: u32 },
    /// Only a stale record existed; it was removed and no process was signalled.
    RemovedStale { pid: u32 },
    /// No daemon was recorded.
    NotRunning,
}

/// Outcome of [`register`], the daemon's own startup claim on the single-instance
/// slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// This process now owns the record and should run.
    Registered,
    /// Another live daemon already owns the slot; this process should exit.
    AlreadyRunning { pid: u32 },
}

/// Report whether a daemon is running, combining the recorded pid with `alive`.
pub fn status(dir: &Path, alive: &dyn Fn(u32) -> bool) -> Result<DaemonState> {
    let pid = daemon_store::read(dir)?.map(|r| r.pid);
    let is_alive = pid.is_some_and(alive);
    Ok(classify(pid, is_alive))
}

/// Start a daemon unless a live one is already recorded. The liveness check runs
/// under the daemon lock; the lock is released before `spawn` runs so the spawned
/// daemon can take the lock in [`register`]. `spawn` launches the detached daemon
/// process (`usagi daemon serve`).
pub fn start(
    dir: &Path,
    alive: &dyn Fn(u32) -> bool,
    spawn: &dyn Fn() -> Result<()>,
) -> Result<StartOutcome> {
    {
        let _lock = StoreLock::acquire(dir)?;
        if let Some(record) = daemon_store::read(dir)? {
            if alive(record.pid) {
                return Ok(StartOutcome::AlreadyRunning { pid: record.pid });
            }
        }
        // Lock drops here so the child's register() can acquire it.
    }
    spawn()?;
    Ok(StartOutcome::Started)
}

/// Ask a running daemon to stop, or clean up a stale record. Taken under the lock
/// so it cannot race a daemon registering.
pub fn stop(dir: &Path, alive: &dyn Fn(u32) -> bool) -> Result<StopOutcome> {
    let _lock = StoreLock::acquire(dir)?;
    match daemon_store::read(dir)? {
        Some(record) if alive(record.pid) => {
            daemon_store::request_stop(dir)?;
            Ok(StopOutcome::Stopping { pid: record.pid })
        }
        Some(record) => {
            daemon_store::clear(dir)?;
            Ok(StopOutcome::RemovedStale { pid: record.pid })
        }
        None => Ok(StopOutcome::NotRunning),
    }
}

/// Claim the single-instance slot for a freshly launched daemon (pid `self_pid`).
/// Refuses if another *live* daemon already holds it; otherwise takes over (also
/// replacing a stale record) and clears any leftover stop marker so the fresh
/// daemon does not exit on a previous run's request. Taken under the lock.
pub fn register(dir: &Path, self_pid: u32, alive: &dyn Fn(u32) -> bool) -> Result<RegisterOutcome> {
    let _lock = StoreLock::acquire(dir)?;
    if let Some(record) = daemon_store::read(dir)? {
        if record.pid != self_pid && alive(record.pid) {
            return Ok(RegisterOutcome::AlreadyRunning { pid: record.pid });
        }
    }
    daemon_store::write(dir, &DaemonRecord { pid: self_pid })?;
    daemon_store::clear_stop_request(dir)?;
    Ok(RegisterOutcome::Registered)
}

/// Release the single-instance slot as the daemon exits. Taken under the lock.
pub fn deregister(dir: &Path) -> Result<()> {
    let _lock = StoreLock::acquire(dir)?;
    daemon_store::clear(dir)
}

/// A session's name paired with its worktree paths, as loaded from a workspace's
/// state. The unit [`gather`] iterates over.
pub type SessionWorktrees = (String, Vec<PathBuf>);

/// Build the daemon's view of every monitored session across the registered
/// workspaces. The three readings are injected so the aggregation is tested with
/// fakes and the real store IO stays at the composition root:
///
/// - `list_roots` — the roots of the registered workspaces.
/// - `load_sessions` — for a workspace root, each session's name and its
///   worktree paths.
/// - `read_phase` — the agent lifecycle phase recorded for a worktree, if any.
///
/// A session's activity is taken from the first of its worktrees that has a
/// recorded phase (a usagi session runs its agent in one worktree), or `None`
/// when none does.
pub fn gather(
    list_roots: &dyn Fn() -> Vec<PathBuf>,
    load_sessions: &dyn Fn(&Path) -> Vec<SessionWorktrees>,
    read_phase: &dyn Fn(&Path) -> Option<AgentPhase>,
) -> Vec<SessionSnapshot> {
    let mut snapshots = Vec::new();
    for root in list_roots() {
        for (name, worktrees) in load_sessions(&root) {
            let activity = worktrees
                .iter()
                .find_map(|worktree| read_phase(worktree))
                .map(SessionActivity::from_phase);
            let worktree = worktrees.first().cloned();
            snapshots.push(SessionSnapshot {
                workspace: root.clone(),
                name,
                worktree,
                activity,
            });
        }
    }
    snapshots
}

/// Refresh the monitored-sessions snapshot under `dir` from `gather_fn`, writing
/// it only when it differs from the persisted one so an unchanged tick does no
/// IO. Returns whether the snapshot changed. `gather_fn` is injected (see
/// [`gather`]) so the tick is tested without the real stores.
pub fn monitor_tick(dir: &Path, gather_fn: &dyn Fn() -> Vec<SessionSnapshot>) -> Result<bool> {
    let current = gather_fn();
    if current == daemon_sessions_store::read(dir)? {
        return Ok(false);
    }
    daemon_sessions_store::write(dir, &current)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn dead(_: u32) -> bool {
        false
    }
    fn live(_: u32) -> bool {
        true
    }
    /// A spawn that succeeds without side effects, shared by the start tests
    /// where `spawn` is either exercised (Started) or deliberately not reached
    /// (AlreadyRunning) — reusing one function keeps both cases covered.
    fn noop_spawn() -> Result<()> {
        Ok(())
    }

    #[test]
    fn status_reports_not_running_without_a_record() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(status(tmp.path(), &live).unwrap(), DaemonState::NotRunning);
    }

    #[test]
    fn status_reports_running_for_a_live_recorded_pid() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 99 }).unwrap();
        assert_eq!(
            status(tmp.path(), &live).unwrap(),
            DaemonState::Running { pid: 99 }
        );
    }

    #[test]
    fn status_reports_stale_for_a_dead_recorded_pid() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 99 }).unwrap();
        assert_eq!(
            status(tmp.path(), &dead).unwrap(),
            DaemonState::Stale { pid: 99 }
        );
    }

    #[test]
    fn start_spawns_when_nothing_is_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let spawned = Cell::new(false);
        let outcome = start(tmp.path(), &dead, &|| {
            spawned.set(true);
            Ok(())
        })
        .unwrap();
        assert_eq!(outcome, StartOutcome::Started);
        assert!(spawned.get());
    }

    #[test]
    fn start_spawns_when_the_record_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 5 }).unwrap();
        let outcome = start(tmp.path(), &dead, &noop_spawn).unwrap();
        // Started is only returned after spawn ran, so it also proves the stale
        // record did not block the launch.
        assert_eq!(outcome, StartOutcome::Started);
    }

    #[test]
    fn start_refuses_when_a_live_daemon_is_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 5 }).unwrap();
        // AlreadyRunning is returned before spawn is reached, so no launch happens.
        let outcome = start(tmp.path(), &live, &noop_spawn).unwrap();
        assert_eq!(outcome, StartOutcome::AlreadyRunning { pid: 5 });
    }

    #[test]
    fn start_propagates_a_spawn_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let err = start(tmp.path(), &dead, &|| Err(anyhow::anyhow!("boom"))).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn stop_signals_a_live_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 8 }).unwrap();
        assert_eq!(
            stop(tmp.path(), &live).unwrap(),
            StopOutcome::Stopping { pid: 8 }
        );
        // The stop marker is left for the daemon to pick up.
        assert!(daemon_store::take_stop_request(tmp.path()).unwrap());
    }

    #[test]
    fn stop_removes_a_stale_record_without_signalling() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 8 }).unwrap();
        assert_eq!(
            stop(tmp.path(), &dead).unwrap(),
            StopOutcome::RemovedStale { pid: 8 }
        );
        assert_eq!(daemon_store::read(tmp.path()).unwrap(), None);
        assert!(!daemon_store::take_stop_request(tmp.path()).unwrap());
    }

    #[test]
    fn stop_reports_not_running_without_a_record() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(stop(tmp.path(), &live).unwrap(), StopOutcome::NotRunning);
    }

    #[test]
    fn register_claims_an_empty_slot_and_clears_a_stale_stop_marker() {
        let tmp = tempfile::tempdir().unwrap();
        // A stop marker left by a previous run must not make the fresh daemon exit.
        daemon_store::request_stop(tmp.path()).unwrap();
        assert_eq!(
            register(tmp.path(), 100, &dead).unwrap(),
            RegisterOutcome::Registered
        );
        assert_eq!(
            daemon_store::read(tmp.path()).unwrap(),
            Some(DaemonRecord { pid: 100 })
        );
        assert!(!daemon_store::take_stop_request(tmp.path()).unwrap());
    }

    #[test]
    fn register_takes_over_a_stale_record() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 1 }).unwrap();
        assert_eq!(
            register(tmp.path(), 2, &dead).unwrap(),
            RegisterOutcome::Registered
        );
        assert_eq!(
            daemon_store::read(tmp.path()).unwrap(),
            Some(DaemonRecord { pid: 2 })
        );
    }

    #[test]
    fn register_refuses_when_another_live_daemon_holds_the_slot() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 1 }).unwrap();
        assert_eq!(
            register(tmp.path(), 2, &live).unwrap(),
            RegisterOutcome::AlreadyRunning { pid: 1 }
        );
        // The incumbent's record is left intact.
        assert_eq!(
            daemon_store::read(tmp.path()).unwrap(),
            Some(DaemonRecord { pid: 1 })
        );
    }

    #[test]
    fn register_re_registers_its_own_pid() {
        // A daemon re-registering under its own pid (even reported dead by a racy
        // liveness check) simply refreshes the record rather than refusing itself.
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 42 }).unwrap();
        assert_eq!(
            register(tmp.path(), 42, &live).unwrap(),
            RegisterOutcome::Registered
        );
    }

    #[test]
    fn deregister_clears_the_record() {
        let tmp = tempfile::tempdir().unwrap();
        daemon_store::write(tmp.path(), &DaemonRecord { pid: 3 }).unwrap();
        deregister(tmp.path()).unwrap();
        assert_eq!(daemon_store::read(tmp.path()).unwrap(), None);
    }

    #[test]
    fn gather_derives_each_session_activity_from_its_worktree_phase() {
        // Two workspaces: /a has one session whose worktree reports Waiting; /b
        // has a session with no phase (None) and one whose second worktree reports
        // Running (the first has none, proving find_map scans past it).
        let list_roots = || vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let load_sessions = |root: &Path| match root.to_str().unwrap() {
            "/a" => vec![("solo".to_string(), vec![PathBuf::from("/a/wt")])],
            _ => vec![
                ("idle".to_string(), vec![PathBuf::from("/b/idle")]),
                (
                    "busy".to_string(),
                    vec![PathBuf::from("/b/busy0"), PathBuf::from("/b/busy1")],
                ),
            ],
        };
        let read_phase = |wt: &Path| match wt.to_str().unwrap() {
            "/a/wt" => Some(AgentPhase::Waiting),
            "/b/busy1" => Some(AgentPhase::Running),
            _ => None,
        };

        let snapshots = gather(&list_roots, &load_sessions, &read_phase);

        assert_eq!(
            snapshots,
            vec![
                SessionSnapshot {
                    workspace: PathBuf::from("/a"),
                    name: "solo".to_string(),
                    worktree: Some(PathBuf::from("/a/wt")),
                    activity: Some(SessionActivity::Waiting),
                },
                SessionSnapshot {
                    workspace: PathBuf::from("/b"),
                    name: "idle".to_string(),
                    worktree: Some(PathBuf::from("/b/idle")),
                    activity: None,
                },
                SessionSnapshot {
                    workspace: PathBuf::from("/b"),
                    name: "busy".to_string(),
                    worktree: Some(PathBuf::from("/b/busy0")),
                    activity: Some(SessionActivity::Running),
                },
            ]
        );
    }

    #[test]
    fn monitor_tick_writes_on_change_and_skips_when_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let snapshot = || {
            vec![SessionSnapshot {
                workspace: PathBuf::from("/repo"),
                name: "s".to_string(),
                worktree: None,
                activity: Some(SessionActivity::Running),
            }]
        };
        // First tick persists the fresh snapshot and reports a change.
        assert!(monitor_tick(dir, &snapshot).unwrap());
        assert_eq!(daemon_sessions_store::read(dir).unwrap(), snapshot());
        // A second identical tick writes nothing and reports no change.
        assert!(!monitor_tick(dir, &snapshot).unwrap());
    }

    #[test]
    fn monitor_tick_rewrites_when_the_snapshot_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        monitor_tick(dir, &|| {
            vec![SessionSnapshot {
                workspace: PathBuf::from("/repo"),
                name: "s".to_string(),
                worktree: None,
                activity: Some(SessionActivity::Running),
            }]
        })
        .unwrap();
        // The session moved to Waiting: the tick detects the difference and writes.
        let next = || {
            vec![SessionSnapshot {
                workspace: PathBuf::from("/repo"),
                name: "s".to_string(),
                worktree: None,
                activity: Some(SessionActivity::Waiting),
            }]
        };
        assert!(monitor_tick(dir, &next).unwrap());
        assert_eq!(daemon_sessions_store::read(dir).unwrap(), next());
    }
}
