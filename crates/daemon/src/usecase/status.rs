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
use usagi_core::domain::daemon::{DaemonState, classify};
use usagi_core::infrastructure::daemon::LivenessProbe;

use crate::usecase::serve::DaemonRecordPort;

/// Build the `status` report line: load the record, probe whether its process is
/// alive, and classify the two into running / stale / not-running.
///
/// # Errors
///
/// Returns the store's load error — a read failure or a malformed `daemon.json`.
///
/// # Panics
///
/// Never in practice: the `Alive` arm unwraps the record, and `classify` reports
/// `Alive` only when a record is present.
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
    Ok(match classify(record.as_ref(), observation) {
        DaemonState::Alive => {
            let pid = record
                .expect("classify reports Alive only for a present record")
                .pid;
            format!("{describe}: daemon running (pid {pid})")
        }
        DaemonState::Stale => format!("{describe}: daemon not running (stale record, reclaimable)"),
        DaemonState::Unverified => {
            format!("{describe}: daemon state unverified (record retained)")
        }
        DaemonState::Absent => format!("{describe}: daemon not running"),
    })
}

#[cfg(test)]
mod tests {
    use super::report;
    use crate::test_support::{FixedProbe, InMemoryRecordFile};
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::DaemonRecord;
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
            "usagi v0.1.0: daemon not running (stale record, reclaimable)"
        );
    }

    struct UnknownProbe;

    impl usagi_core::infrastructure::daemon::LivenessProbe for UnknownProbe {
        fn observe(
            &self,
            _record: &DaemonRecord,
        ) -> usagi_core::domain::daemon::DaemonProcessObservation {
            usagi_core::domain::daemon::DaemonProcessObservation::Unknown
        }
    }

    #[test]
    fn reports_unverified_and_retains_record_when_identity_is_unknown() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let record = DaemonRecord::new(4321);
        store.save(&record).unwrap();
        assert_eq!(
            report(&store, &UnknownProbe, &info()).unwrap(),
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
