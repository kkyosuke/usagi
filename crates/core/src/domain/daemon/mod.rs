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
//! single-instance startup. Process ownership is proven by the OS process-start
//! identity recorded alongside the PID; PID liveness alone is never authority
//! to signal or reclaim a daemon.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The lifecycle record a running `usagi daemon` persists to
/// `<data-dir>/daemon/daemon.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecord {
    /// Process id of the running daemon.
    pub pid: u32,
    /// OS-observed process-start identity for `pid`.
    ///
    /// `None` is accepted only to read legacy records. It is ownership unknown,
    /// not evidence that the current occupant of `pid` is this daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start_identity: Option<String>,
    /// When the daemon registered this record.
    pub started_at: DateTime<Utc>,
}

impl DaemonRecord {
    /// Build a legacy/fixture record without OS process-start evidence.
    ///
    /// Production daemon registration uses [`Self::identified`]. Keeping this
    /// constructor permits conservative migration tests for records written
    /// before process identity became mandatory.
    #[must_use]
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            process_start_identity: None,
            started_at: Utc::now(),
        }
    }

    /// Build a record with OS-observed process-start identity.
    #[must_use]
    pub fn identified(pid: u32, process_start_identity: impl Into<String>) -> Self {
        Self {
            pid,
            process_start_identity: Some(process_start_identity.into()),
            started_at: Utc::now(),
        }
    }

    /// Whether the record contains non-empty process-start evidence.
    #[must_use]
    pub fn has_process_identity(&self) -> bool {
        self.process_start_identity
            .as_deref()
            .is_some_and(|identity| !identity.is_empty())
    }
}

/// OS observation of the exact process recorded as daemon owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonProcessObservation {
    /// PID and process-start identity both match the record.
    Exact,
    /// No process currently occupies the recorded PID.
    Gone,
    /// The PID exists but belongs to another process incarnation.
    IdentityMismatch,
    /// Ownership cannot be established (legacy identity, unsupported or failed
    /// OS observation).
    Unknown,
}

/// The lifecycle state derived from a daemon record and exact owner
/// observation. It is what clients act on: connect only to an
/// [`Alive`](DaemonState::Alive) daemon, reclaim only a proven
/// [`Stale`](DaemonState::Stale) record, refuse an
/// [`Unverified`](DaemonState::Unverified) record, and spawn directly when
/// [`Absent`](DaemonState::Absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    /// A record exists and its exact process owner is alive.
    Alive,
    /// A record exists and its recorded process is proven gone.
    Stale,
    /// A record exists but exact ownership cannot be established.
    Unverified,
    /// No record exists — no daemon has registered.
    Absent,
}

/// Classify the daemon lifecycle state from an optional record and exact process
/// observation.
///
/// The record's presence and process observation are supplied by the caller:
/// reading `daemon.json` and observing process identity are infrastructure
/// concerns (real IO), so this stays a pure decision. When `record` is `None`
/// the result is [`Absent`](DaemonState::Absent) and `observation` is
/// irrelevant.
#[must_use]
pub fn classify(
    record: Option<&DaemonRecord>,
    observation: DaemonProcessObservation,
) -> DaemonState {
    match record {
        None => DaemonState::Absent,
        Some(_) if observation == DaemonProcessObservation::Exact => DaemonState::Alive,
        Some(_) if observation == DaemonProcessObservation::Gone => DaemonState::Stale,
        Some(_) => DaemonState::Unverified,
    }
}

#[cfg(test)]
mod tests;
