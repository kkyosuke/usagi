//! The daemon lifecycle record persisted to `<data-dir>/daemon/daemon.json`.
//!
//! `DaemonRecord` is the value object a running `usagi daemon` writes on
//! startup. It is a plain value object carrying only its [`DaemonRecord::new`]
//! constructor, which stamps `started_at`. It derives `serde` so the daemon
//! record store (an infrastructure concern) can persist it as JSON without the
//! domain knowing where or how it is stored.
//!
//! Other processes read the record to locate a running daemon — the TUI / CLI
//! clients that autospawn or connect, and a second daemon guarding
//! single-instance startup. A record whose `pid` is no longer alive is treated
//! as stale and reclaimed.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The lifecycle record a running `usagi daemon` persists to
/// `<data-dir>/daemon/daemon.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecord {
    /// Process id of the running daemon, used for liveness and single-instance checks.
    pub pid: u32,
    /// When the daemon registered this record.
    pub started_at: DateTime<Utc>,
}

impl DaemonRecord {
    /// Build a record for the daemon process `pid`, stamping `started_at` with
    /// the current time.
    #[must_use]
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            started_at: Utc::now(),
        }
    }
}

/// The lifecycle state derived from a daemon record and the liveness of its
/// process. It is what clients act on: connect to an [`Alive`](DaemonState::Alive)
/// daemon, reclaim a [`Stale`](DaemonState::Stale) record before spawning a fresh
/// one, and spawn directly when [`Absent`](DaemonState::Absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    /// A record exists and its process is alive — a running daemon to connect to.
    Alive,
    /// A record exists but its process is gone — a leftover to reclaim.
    Stale,
    /// No record exists — no daemon has registered.
    Absent,
}

/// Classify the daemon lifecycle state from an optional record and whether its
/// process is alive.
///
/// The record's presence and the liveness probe are supplied by the caller:
/// reading `daemon.json` and probing the pid are infrastructure concerns (real
/// IO), so this stays a pure decision. When `record` is `None` the result is
/// [`Absent`](DaemonState::Absent) and `alive` is irrelevant; otherwise `alive`
/// selects [`Alive`](DaemonState::Alive) or [`Stale`](DaemonState::Stale).
#[must_use]
pub fn classify(record: Option<&DaemonRecord>, alive: bool) -> DaemonState {
    match record {
        None => DaemonState::Absent,
        Some(_) if alive => DaemonState::Alive,
        Some(_) => DaemonState::Stale,
    }
}

#[cfg(test)]
mod tests;
