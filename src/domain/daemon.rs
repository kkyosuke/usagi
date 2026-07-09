//! The daemon's liveness as a pure classification.
//!
//! usagi records a running daemon by pid in a small file (see
//! [`crate::infrastructure::daemon_store`]); a stale record can outlive the
//! process that wrote it if the daemon was killed without deregistering. The
//! whole "is a daemon actually running?" decision reduces to combining the
//! recorded pid (if any) with whether the OS still knows that pid — kept here as
//! a pure function so every branch is unit-tested without touching the process
//! table or the filesystem.

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
}
