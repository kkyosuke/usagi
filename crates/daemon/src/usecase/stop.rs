//! The `usagi daemon stop` usecase: terminate a running daemon and reclaim its
//! record.
//!
//! Mirrors [`status`](crate::usecase::status), but acts on the state instead of
//! only reporting it. It loads the record, classifies it with the liveness
//! probe, and then:
//!
//! - **running**: asks the process to terminate, then clears `daemon.json`;
//! - **stale**: leaves no live process, so it just clears the leftover record;
//! - **not running**: reports there is nothing to stop.
//!
//! The store's file seam, the probe, and the terminator are injected, so this
//! stays pure and fully testable; the synthesis root binds the real filesystem,
//! process probe, and signal.
//!
//! A graceful stop marker for a long-running `serve` loop is a later concern;
//! with no daemon loop yet, terminating the recorded pid is the effective stop.

use std::io;

use usagi_core::domain::AppInfo;
use usagi_core::domain::daemon::{DaemonState, classify};
use usagi_core::infrastructure::daemon::{
    DaemonRecordStore, LivenessProbe, RecordFile, Terminator,
};

/// Stop the recorded daemon and report the outcome.
///
/// # Errors
///
/// Returns the store's load error, the terminator's error when a running daemon
/// cannot be signalled, or the store's clear error when the record cannot be
/// removed.
///
/// # Panics
///
/// Never in practice: the `Alive` arm unwraps the record, and `classify` reports
/// `Alive` only when a record is present.
#[coverage(off)]
pub fn stop<F: RecordFile, P: LivenessProbe, T: Terminator>(
    store: &DaemonRecordStore<F>,
    probe: &P,
    terminator: &T,
    info: &AppInfo,
) -> io::Result<String> {
    let record = store.load()?;
    let alive = record
        .as_ref()
        .is_some_and(|record| probe.is_alive(record.pid));
    let describe = info.describe();
    match classify(record.as_ref(), alive) {
        DaemonState::Alive => {
            let pid = record
                .expect("classify reports Alive only for a present record")
                .pid;
            terminator.terminate(pid)?;
            store.clear()?;
            Ok(format!("{describe}: daemon stopped (pid {pid})"))
        }
        DaemonState::Stale => {
            store.clear()?;
            Ok(format!("{describe}: cleared stale daemon record"))
        }
        DaemonState::Absent => Ok(format!("{describe}: daemon not running")),
    }
}

#[cfg(test)]
mod tests {
    use super::stop;
    use crate::test_support::{FixedProbe, InMemoryRecordFile, RecordingTerminator};
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
        let terminator = RecordingTerminator::default();
        assert_eq!(
            stop(&store, &FixedProbe(false), &terminator, &info()).unwrap(),
            "usagi v0.1.0: daemon not running"
        );
        assert!(terminator.terminated().is_empty());
    }

    #[test]
    fn terminates_and_clears_when_running() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        let terminator = RecordingTerminator::default();
        assert_eq!(
            stop(&store, &FixedProbe(true), &terminator, &info()).unwrap(),
            "usagi v0.1.0: daemon stopped (pid 4321)"
        );
        assert_eq!(terminator.terminated(), vec![4321]);
        // The record is reclaimed after a successful stop.
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn clears_stale_record_without_terminating() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        let terminator = RecordingTerminator::default();
        assert_eq!(
            stop(&store, &FixedProbe(false), &terminator, &info()).unwrap(),
            "usagi v0.1.0: cleared stale daemon record"
        );
        assert!(terminator.terminated().is_empty());
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn propagates_terminate_error_and_keeps_record() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let record = DaemonRecord::new(4321);
        store.save(&record).unwrap();
        let terminator = RecordingTerminator::failing();
        assert!(stop(&store, &FixedProbe(true), &terminator, &info()).is_err());
        // The stop failed before clearing, so the record survives for a retry.
        assert_eq!(store.load().unwrap(), Some(record));
    }

    #[test]
    fn propagates_load_error() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
        let terminator = RecordingTerminator::default();
        assert!(stop(&store, &FixedProbe(true), &terminator, &info()).is_err());
    }
}
