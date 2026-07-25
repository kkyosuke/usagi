//! Custody supervision: decide whether a serving daemon is still the authority
//! for its data directory.
//!
//! A daemon is started detached (its own process group) so a foreground hangup
//! never takes its PTYs down with it. The consequence is that nothing reaps it
//! when its launcher dies abnormally: a `$USAGI_HOME` temporary directory can be
//! deleted underneath it and the process keeps listening forever.
//!
//! The terminating condition is **loss of custody**, not idleness: a legitimate
//! daemon owns live PTYs and a supervisor schedule even with zero clients, so
//! "no client" is never evidence that it should exit. Custody is the conjunction
//! of two invariants, each observed through the injected [`CustodyProbe`]:
//!
//! 1. **lock custody** — the pathname of the single-instance lock still names
//!    the inode this process locked. An absent or replaced pathname means this
//!    process is no longer the singleton for that data directory.
//! 2. **record custody** — `daemon.json` still records this process as owner
//!    (the same pid and OS process-start identity). An absent or foreign record
//!    means the authority was retired or handed to another incarnation.
//!
//! [`evaluate`] is a pure decision over those observations, so the whole policy
//! is unit-testable with fakes; the synthesis root binds the real `stat` and
//! record read and turns a loss into the ordinary graceful shutdown request.

use std::io;

use usagi_core::domain::daemon::DaemonRecord;

/// Filesystem identity of a node: the pair that survives renames and detects
/// replacement, which a pathname alone cannot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeIdentity {
    /// Device the node lives on.
    pub dev: u64,
    /// Inode number within that device.
    pub ino: u64,
}

/// Observations a serving daemon needs to decide whether it is still the
/// authority for its data directory.
pub trait CustodyProbe {
    /// Identity of the lock inode this process holds, observed when the
    /// single-instance lock was acquired.
    ///
    /// # Errors
    ///
    /// Returns an error when the held identity was never observed, which leaves
    /// custody undecidable rather than lost.
    fn locked_inode(&self) -> io::Result<NodeIdentity>;

    /// Identity of the node the lock pathname currently names, or `None` when
    /// the pathname is absent.
    ///
    /// # Errors
    ///
    /// Returns the underlying `stat` error for anything other than absence.
    fn lock_pathname(&self) -> io::Result<Option<NodeIdentity>>;

    /// The durable owner record, or `None` when it is absent.
    ///
    /// # Errors
    ///
    /// Returns the underlying read or parse error.
    fn owner_record(&self) -> io::Result<Option<DaemonRecord>>;
}

/// Why a serving daemon is no longer the authority for its data directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CustodyLoss {
    /// The single-instance lock pathname no longer exists.
    LockPathAbsent,
    /// The lock pathname names a different inode than the locked one.
    LockInodeReplaced,
    /// `daemon.json` no longer exists.
    RecordAbsent,
    /// `daemon.json` records another owner incarnation.
    RecordReplaced,
}

impl CustodyLoss {
    /// A stable, log-friendly reason for this loss.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::LockPathAbsent => "instance lock path is gone",
            Self::LockInodeReplaced => "instance lock path names another inode",
            Self::RecordAbsent => "owner record is gone",
            Self::RecordReplaced => "owner record names another incarnation",
        }
    }
}

/// Whether a serving daemon still holds custody of its data directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Custody {
    /// Both invariants hold: this process is still the authority.
    Held,
    /// An invariant broke; the daemon must shut down gracefully.
    Lost(CustodyLoss),
}

/// Decide custody for the daemon whose registered record is `owner`.
///
/// The lock is examined before the record so a deleted data directory is
/// reported without reading (and thereby re-creating anything under) the tree
/// this process has already lost.
///
/// # Errors
///
/// Returns the probe's error. A failed observation is deliberately *not* a loss:
/// the caller keeps serving and re-evaluates on the next tick, so a transient
/// filesystem error can never terminate a legitimate daemon.
pub fn evaluate(probe: &dyn CustodyProbe, owner: &DaemonRecord) -> io::Result<Custody> {
    let locked = probe.locked_inode()?;
    match probe.lock_pathname()? {
        None => return Ok(Custody::Lost(CustodyLoss::LockPathAbsent)),
        Some(pathname) if pathname != locked => {
            return Ok(Custody::Lost(CustodyLoss::LockInodeReplaced));
        }
        Some(_) => {}
    }
    match probe.owner_record()? {
        None => Ok(Custody::Lost(CustodyLoss::RecordAbsent)),
        Some(record) if !is_same_incarnation(&record, owner) => {
            Ok(Custody::Lost(CustodyLoss::RecordReplaced))
        }
        Some(_) => Ok(Custody::Held),
    }
}

/// Whether `record` still names the `owner` incarnation.
///
/// Identity is the pid together with the OS process-start identity — the same
/// pair every other owner observation uses. `started_at` is deliberately
/// excluded: a record rewritten with this process's exact identity still names
/// this process, and treating a re-stamped timestamp as a foreign owner would
/// terminate a daemon that never lost anything.
fn is_same_incarnation(record: &DaemonRecord, owner: &DaemonRecord) -> bool {
    record.pid == owner.pid && record.process_start_identity == owner.process_start_identity
}

#[cfg(test)]
mod tests {
    use super::{Custody, CustodyLoss, CustodyProbe, NodeIdentity, evaluate};
    use std::io;
    use usagi_core::domain::daemon::DaemonRecord;

    fn identity(ino: u64) -> NodeIdentity {
        NodeIdentity { dev: 7, ino }
    }

    fn owner() -> DaemonRecord {
        DaemonRecord::identified(4321, "test:4321")
    }

    /// A probe whose three observations are configured independently, so every
    /// invariant can be broken in isolation.
    struct FakeProbe {
        locked: io::Result<NodeIdentity>,
        pathname: io::Result<Option<NodeIdentity>>,
        record: io::Result<Option<DaemonRecord>>,
    }

    impl Default for FakeProbe {
        fn default() -> Self {
            Self {
                locked: Ok(identity(11)),
                pathname: Ok(Some(identity(11))),
                record: Ok(Some(owner())),
            }
        }
    }

    fn clone_result<T: Clone>(result: &io::Result<T>) -> io::Result<T> {
        match result {
            Ok(value) => Ok(value.clone()),
            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
        }
    }

    impl CustodyProbe for FakeProbe {
        fn locked_inode(&self) -> io::Result<NodeIdentity> {
            clone_result(&self.locked)
        }

        fn lock_pathname(&self) -> io::Result<Option<NodeIdentity>> {
            clone_result(&self.pathname)
        }

        fn owner_record(&self) -> io::Result<Option<DaemonRecord>> {
            clone_result(&self.record)
        }
    }

    #[test]
    fn holds_custody_while_the_locked_inode_and_owner_record_are_intact() {
        assert_eq!(
            evaluate(&FakeProbe::default(), &owner()).unwrap(),
            Custody::Held
        );
    }

    #[test]
    fn loses_custody_when_the_lock_path_disappears_or_is_replaced() {
        let absent = FakeProbe {
            pathname: Ok(None),
            ..FakeProbe::default()
        };
        assert_eq!(
            evaluate(&absent, &owner()).unwrap(),
            Custody::Lost(CustodyLoss::LockPathAbsent)
        );

        let replaced = FakeProbe {
            pathname: Ok(Some(identity(12))),
            ..FakeProbe::default()
        };
        assert_eq!(
            evaluate(&replaced, &owner()).unwrap(),
            Custody::Lost(CustodyLoss::LockInodeReplaced)
        );
    }

    #[test]
    fn a_lost_lock_is_reported_without_reading_the_record() {
        let probe = FakeProbe {
            pathname: Ok(None),
            record: Err(io::Error::other("the record must not be read")),
            ..FakeProbe::default()
        };
        assert_eq!(
            evaluate(&probe, &owner()).unwrap(),
            Custody::Lost(CustodyLoss::LockPathAbsent)
        );
    }

    #[test]
    fn loses_custody_when_the_record_disappears_or_names_another_incarnation() {
        let absent = FakeProbe {
            record: Ok(None),
            ..FakeProbe::default()
        };
        assert_eq!(
            evaluate(&absent, &owner()).unwrap(),
            Custody::Lost(CustodyLoss::RecordAbsent)
        );

        for foreign in [
            DaemonRecord::identified(9999, "test:4321"),
            DaemonRecord::identified(4321, "test:9999"),
            DaemonRecord::new(4321),
        ] {
            let replaced = FakeProbe {
                record: Ok(Some(foreign)),
                ..FakeProbe::default()
            };
            assert_eq!(
                evaluate(&replaced, &owner()).unwrap(),
                Custody::Lost(CustodyLoss::RecordReplaced)
            );
        }
    }

    #[test]
    fn a_re_stamped_record_with_this_exact_identity_keeps_custody() {
        let mut restamped = owner();
        restamped.started_at += chrono::Duration::seconds(5);
        let probe = FakeProbe {
            record: Ok(Some(restamped)),
            ..FakeProbe::default()
        };
        assert_eq!(evaluate(&probe, &owner()).unwrap(), Custody::Held);
    }

    #[test]
    fn an_undecidable_observation_is_an_error_and_never_a_loss() {
        for probe in [
            FakeProbe {
                locked: Err(io::Error::other("held identity unobserved")),
                ..FakeProbe::default()
            },
            FakeProbe {
                pathname: Err(io::Error::other("stat failed")),
                ..FakeProbe::default()
            },
            FakeProbe {
                record: Err(io::Error::other("read failed")),
                ..FakeProbe::default()
            },
        ] {
            assert!(evaluate(&probe, &owner()).is_err());
        }
    }

    #[test]
    fn every_loss_carries_a_distinct_reason() {
        let reasons = [
            CustodyLoss::LockPathAbsent,
            CustodyLoss::LockInodeReplaced,
            CustodyLoss::RecordAbsent,
            CustodyLoss::RecordReplaced,
        ]
        .map(CustodyLoss::reason);
        let unique: std::collections::BTreeSet<_> = reasons.iter().collect();
        assert_eq!(unique.len(), reasons.len());
    }
}
