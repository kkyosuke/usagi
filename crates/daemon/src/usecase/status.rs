//! The `usagi daemon status` usecase: report the daemon's lifecycle state.
//!
//! Composes the daemon record store (loading `daemon.json`), the process identity probe
//! (does the recorded PID still have the exact process-start identity?), and the domain
//! [`classify`](usagi_core::domain::daemon::classify) decision into a single
//! human-readable line. Both the store's file seam and the probe are injected,
//! so this stays pure and fully testable; the synthesis root binds the real
//! filesystem and process probe.

use std::io;

use usagi_core::domain::AppInfo;
use usagi_core::domain::daemon::{DaemonState, StaleReason, classify};
use usagi_core::infrastructure::daemon::LivenessProbe;

use crate::usecase::serve::DaemonRecordPort;

/// Build the `status` report line: load the record, probe whether its process is
/// alive, and classify the two into running / stale / unverified / not-running.
///
/// Both stale reasons are reported as reclaimable, but they are named apart: an
/// owner that simply vanished and an owner whose PID has been handed to an
/// unrelated process are different events, and only the second explains why an
/// unrelated live process holds the recorded PID.
///
/// # Errors
///
/// Returns the store's load error — a read failure or a malformed `daemon.json`.
///
/// # Panics
///
/// Never in practice: the arms that name a pid read it from the loaded record,
/// and `classify` reports those states only when a record is present.
pub fn report(
    store: &dyn DaemonRecordPort,
    probe: &dyn LivenessProbe,
    info: &AppInfo,
) -> io::Result<String> {
    let record = store.load()?;
    let observation = record.as_ref().map_or(
        usagi_core::domain::daemon::DaemonProcessObservation::Unknown,
        |record| probe.observe(record),
    );
    let describe = info.describe();
    let recorded_pid = record.as_ref().map(|record| record.pid);
    let pid = || recorded_pid.expect("classify names a pid only for a present record");
    Ok(match classify(record.as_ref(), observation) {
        DaemonState::Alive => format!("{describe}: daemon running (pid {})", pid()),
        DaemonState::Stale(StaleReason::OwnerGone) => format!(
            "{describe}: daemon not running (stale record, pid {} is gone; reclaimable)",
            pid()
        ),
        DaemonState::Stale(StaleReason::PidReused) => format!(
            "{describe}: daemon not running (stale record, pid {} was reused by another process; reclaimable)",
            pid()
        ),
        DaemonState::Unverified => {
            format!("{describe}: daemon state unverified (record retained)")
        }
        DaemonState::Absent => format!("{describe}: daemon not running"),
    })
}

#[cfg(test)]
mod tests {
    use super::report;
    use crate::test_support::{FixedProbe, InMemoryRecordFile, ObservedAs};
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
    use usagi_core::infrastructure::daemon::DaemonRecordStore;

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    #[test]
    fn reports_not_running_when_no_record() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert_eq!(
            report(&store, &FixedProbe(false), &info()).unwrap(),
            "usagi v0.1.0: daemon not running"
        );
    }

    #[test]
    fn reports_running_with_pid_when_record_and_process_alive() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        assert_eq!(
            report(&store, &FixedProbe(true), &info()).unwrap(),
            "usagi v0.1.0: daemon running (pid 4321)"
        );
    }

    #[test]
    fn reports_stale_when_record_but_process_gone() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        assert_eq!(
            report(&store, &FixedProbe(false), &info()).unwrap(),
            "usagi v0.1.0: daemon not running (stale record, pid 4321 is gone; reclaimable)"
        );
    }

    #[test]
    fn names_a_reused_pid_apart_from_a_vanished_owner_and_keeps_both_reclaimable() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store
            .save(&DaemonRecord::identified(4321, "old-incarnation"))
            .unwrap();
        // Both lines say "reclaimable", because both observations prove the
        // recorded owner is gone. Only this one explains why an unrelated live
        // process answers for pid 4321.
        assert_eq!(
            report(
                &store,
                &ObservedAs(DaemonProcessObservation::IdentityMismatch),
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon not running (stale record, pid 4321 was reused by another process; reclaimable)"
        );
    }

    #[test]
    fn reports_unverified_and_retains_record_when_identity_is_unknown() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let record = DaemonRecord::new(4321);
        store.save(&record).unwrap();
        assert_eq!(
            report(
                &store,
                &ObservedAs(DaemonProcessObservation::Unknown),
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon state unverified (record retained)"
        );
        assert_eq!(store.load().unwrap(), Some(record));
    }

    #[test]
    fn reports_not_running_after_record_cleared() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        let record = store.load().unwrap().unwrap();
        assert!(store.clear_if(&record).unwrap());
        assert_eq!(
            report(&store, &FixedProbe(true), &info()).unwrap(),
            "usagi v0.1.0: daemon not running"
        );
    }

    #[test]
    fn propagates_malformed_record_as_error() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
        assert!(report(&store, &FixedProbe(true), &info()).is_err());
    }
}
