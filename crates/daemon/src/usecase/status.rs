//! The `usagi daemon status` usecase: report the daemon's lifecycle state.
//!
//! Composes the daemon record store (loading `daemon.json`), the liveness probe
//! (is the recorded pid alive?), and the domain
//! [`classify`](usagi_core::domain::daemon::classify) decision into a single
//! human-readable line. Both the store's file seam and the probe are injected,
//! so this stays pure and fully testable; the synthesis root binds the real
//! filesystem and process probe.

use std::io;

use usagi_core::domain::AppInfo;
use usagi_core::domain::daemon::{DaemonState, classify};
use usagi_core::infrastructure::daemon::{DaemonRecordStore, LivenessProbe, RecordFile};

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
pub fn report<F: RecordFile, P: LivenessProbe>(
    store: &DaemonRecordStore<F>,
    probe: &P,
    info: &AppInfo,
) -> io::Result<String> {
    let record = store.load()?;
    let alive = record
        .as_ref()
        .is_some_and(|record| probe.is_alive(record.pid));
    let describe = info.describe();
    Ok(match classify(record.as_ref(), alive) {
        DaemonState::Alive => {
            let pid = record
                .expect("classify reports Alive only for a present record")
                .pid;
            format!("{describe}: daemon running (pid {pid})")
        }
        DaemonState::Stale => format!("{describe}: daemon not running (stale record, reclaimable)"),
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
