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

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The smallest PID a lifecycle record may name.
///
/// `0` and `1` never identify a daemon this build started, and both are
/// dangerous to carry in a durable record: POSIX `kill(0, …)` addresses the
/// *caller's* whole process group, and `1` is the OS init process. Rejecting
/// them at the record boundary means no corrupted, hand-edited, or forged
/// `daemon.json` can present such a value to a signal path.
pub const MIN_RECORD_PID: u32 = 2;

/// The largest PID a lifecycle record may name.
///
/// `pid_t` is a signed 32-bit integer on every platform usagi supports, so a
/// larger unsigned value has no `pid_t` spelling: it is the wire form of a
/// negative PID, which addresses a process *group* rather than a process.
pub const MAX_RECORD_PID: u32 = 0x7fff_ffff;

/// Whether `pid` may appear in a daemon lifecycle record.
///
/// This is the numeric half of ownership: it says the value can name one
/// process, not that the process is this daemon. Exact ownership is still
/// [`process_start_identity`](DaemonRecord::process_start_identity).
#[must_use]
pub fn is_record_pid(pid: u32) -> bool {
    (MIN_RECORD_PID..=MAX_RECORD_PID).contains(&pid)
}

/// A PID that cannot appear in a daemon lifecycle record.
///
/// Returned by the record's deserialization and by registration, so a rejected
/// value is reported the same way whichever boundary it arrives through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidRecordPid(pub u32);

impl fmt::Display for InvalidRecordPid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "daemon record pid {} cannot name a process (expected {MIN_RECORD_PID}..={MAX_RECORD_PID})",
            self.0
        )
    }
}

impl std::error::Error for InvalidRecordPid {}

/// The lifecycle record a running `usagi daemon` persists to
/// `<data-dir>/daemon/daemon.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireDaemonRecord")]
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

/// The stored / transmitted shape of a [`DaemonRecord`], validated on the way
/// in.
///
/// `DaemonRecord` deserializes through this type so every reader — the record
/// store loading `daemon.json` and the IPC handshake carrying the daemon's
/// self-report — rejects an unusable PID at the same place, before any caller
/// can act on it.
#[derive(Deserialize)]
struct WireDaemonRecord {
    pid: u32,
    #[serde(default)]
    process_start_identity: Option<String>,
    started_at: DateTime<Utc>,
}

impl TryFrom<WireDaemonRecord> for DaemonRecord {
    type Error = InvalidRecordPid;

    fn try_from(wire: WireDaemonRecord) -> Result<Self, Self::Error> {
        if !is_record_pid(wire.pid) {
            return Err(InvalidRecordPid(wire.pid));
        }
        Ok(Self {
            pid: wire.pid,
            process_start_identity: wire.process_start_identity,
            started_at: wire.started_at,
        })
    }
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

/// How the OS proved that the recorded owner incarnation no longer exists.
///
/// Both variants are equally reclaimable — the record names a process that is
/// gone either way — but they are different events, so `status` names them
/// separately instead of telling an operator only that something is stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// No process occupies the recorded PID any more.
    OwnerGone,
    /// The recorded PID is occupied by an unrelated process incarnation, so the
    /// owner exited and the OS handed its PID to someone else.
    PidReused,
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
    /// A record exists and the OS has proven its recorded owner incarnation is
    /// gone, in the way [`StaleReason`] names.
    Stale(StaleReason),
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
///
/// [`Gone`](DaemonProcessObservation::Gone) and
/// [`IdentityMismatch`](DaemonProcessObservation::IdentityMismatch) are both
/// *positive* evidence that the recorded owner incarnation is gone, so both are
/// reclaimable. Only [`Unknown`](DaemonProcessObservation::Unknown) — a legacy
/// record, a failed observation, an unsupported platform — leaves ownership
/// undecided, and only that stays [`Unverified`](DaemonState::Unverified).
#[must_use]
pub fn classify(
    record: Option<&DaemonRecord>,
    observation: DaemonProcessObservation,
) -> DaemonState {
    match (record, observation) {
        (None, _) => DaemonState::Absent,
        (Some(_), DaemonProcessObservation::Exact) => DaemonState::Alive,
        (Some(_), DaemonProcessObservation::Gone) => DaemonState::Stale(StaleReason::OwnerGone),
        (Some(_), DaemonProcessObservation::IdentityMismatch) => {
            DaemonState::Stale(StaleReason::PidReused)
        }
        (Some(_), DaemonProcessObservation::Unknown) => DaemonState::Unverified,
    }
}

#[cfg(test)]
mod tests;
