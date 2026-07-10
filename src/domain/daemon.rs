//! The daemon's liveness as a pure classification, and the pure view model of
//! the sessions it monitors.
//!
//! usagi records a running daemon by pid in a small file (see
//! [`crate::infrastructure::daemon_store`]); a stale record can outlive the
//! process that wrote it if the daemon was killed without deregistering. The
//! whole "is a daemon actually running?" decision reduces to combining the
//! recorded pid (if any) with whether the OS still knows that pid — kept here as
//! a pure function so every branch is unit-tested without touching the process
//! table or the filesystem.
//!
//! The daemon also keeps a snapshot of every session it monitors across the
//! registered workspaces (see [`crate::usecase::daemon::gather`]). Each
//! session's [`SessionActivity`] is derived purely from the agent lifecycle
//! [`AgentPhase`] its hooks last reported, and the aggregate is a plain
//! [`SessionSnapshot`] list — both defined here so the derivation and the on-disk
//! view model stay free of IO.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::agent_phase::AgentPhase;

/// Whether a usagi daemon is running, derived from its recorded pid and that
/// pid's liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    /// A daemon is recorded and its process is alive.
    Running { pid: u32 },
    /// A daemon is recorded but its process is gone (killed without
    /// deregistering, or the machine restarted). The record should be cleaned up.
    Stale { pid: u32 },
    /// No daemon is recorded.
    NotRunning,
}

/// Classify the daemon from its recorded pid (`None` when no record exists) and
/// whether that pid is currently alive. Pure: the caller supplies the liveness.
pub fn classify(pid: Option<u32>, alive: bool) -> DaemonState {
    match pid {
        Some(pid) if alive => DaemonState::Running { pid },
        Some(pid) => DaemonState::Stale { pid },
        None => DaemonState::NotRunning,
    }
}

/// A monitored session's current activity, derived from the agent lifecycle
/// phase its hooks last reported. This is the phase-driven half of the sidebar's
/// running / waiting / done indicator; the terminal-bell fallback the TUI also
/// uses needs the live PTY, so it is not available to the daemon yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionActivity {
    /// The agent started or resumed and is idle (`Ready`).
    Ready,
    /// The agent is working a turn (`Running`).
    Running,
    /// The agent paused mid-turn for the user's input or permission (`Waiting`).
    Waiting,
    /// The agent finished a turn or its process exited (`Ended`).
    Done,
}

impl SessionActivity {
    /// Map an agent lifecycle phase to the activity the daemon reports for it.
    pub fn from_phase(phase: AgentPhase) -> Self {
        match phase {
            AgentPhase::Ready => Self::Ready,
            AgentPhase::Running => Self::Running,
            AgentPhase::Waiting => Self::Waiting,
            AgentPhase::Ended => Self::Done,
        }
    }

    /// The lowercase word the CLI prints for this activity.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Done => "done",
        }
    }
}

/// One monitored session in the daemon's snapshot: which workspace it belongs to,
/// its session name, and its current activity (`None` when no agent phase has
/// been recorded for it — e.g. no agent has run in it yet).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Root of the workspace this session belongs to.
    pub workspace: PathBuf,
    /// The session's name.
    pub name: String,
    /// The session's current activity, or `None` when unknown.
    pub activity: Option<SessionActivity>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_and_alive_is_running() {
        assert_eq!(classify(Some(42), true), DaemonState::Running { pid: 42 });
    }

    #[test]
    fn recorded_but_dead_is_stale() {
        assert_eq!(classify(Some(42), false), DaemonState::Stale { pid: 42 });
    }

    #[test]
    fn no_record_is_not_running() {
        // The liveness argument is irrelevant with no recorded pid.
        assert_eq!(classify(None, true), DaemonState::NotRunning);
        assert_eq!(classify(None, false), DaemonState::NotRunning);
    }

    #[test]
    fn activity_maps_each_phase() {
        assert_eq!(
            SessionActivity::from_phase(AgentPhase::Ready),
            SessionActivity::Ready
        );
        assert_eq!(
            SessionActivity::from_phase(AgentPhase::Running),
            SessionActivity::Running
        );
        assert_eq!(
            SessionActivity::from_phase(AgentPhase::Waiting),
            SessionActivity::Waiting
        );
        assert_eq!(
            SessionActivity::from_phase(AgentPhase::Ended),
            SessionActivity::Done
        );
    }

    #[test]
    fn activity_as_str_words() {
        assert_eq!(SessionActivity::Ready.as_str(), "ready");
        assert_eq!(SessionActivity::Running.as_str(), "running");
        assert_eq!(SessionActivity::Waiting.as_str(), "waiting");
        assert_eq!(SessionActivity::Done.as_str(), "done");
    }
}
