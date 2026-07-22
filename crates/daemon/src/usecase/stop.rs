//! The `usagi daemon stop` usecase: terminate a running daemon and reclaim its
//! record.
//!
//! Mirrors [`status`](crate::usecase::status), but acts on the state instead of
//! only reporting it. It loads the record, classifies it with the liveness
//! probe, and then:
//!
//! - **running**: asks the process to terminate, then waits until the owner has
//!   retired its endpoint and cleared that exact record;
//! - **stale**: leaves no live process, so it conditionally clears that exact
//!   leftover record;
//! - **not running**: reports there is nothing to stop.
//!
//! The store's file seam, the probe, and the terminator are injected, so this
//! stays pure and fully testable; the synthesis root binds the real filesystem,
//! process probe, and signal.

use std::io;

use usagi_core::domain::AppInfo;
use usagi_core::domain::daemon::{DaemonState, classify};
use usagi_core::infrastructure::daemon::{
    DaemonRecordStore, LivenessProbe, RecordFile, Sleeper, Terminator,
};

const MAX_CLEANUP_POLLS: usize = 40;

/// Stop the recorded daemon and report the outcome.
///
/// # Errors
///
/// Returns the store's load error, the terminator's error when a running daemon
/// cannot be signalled, or the store's clear error when the record cannot be
/// removed, or a timeout / incomplete-cleanup error after shutdown was
/// requested. A concurrently installed replacement record is preserved.
///
/// # Panics
///
/// Never in practice: the `Alive` arm unwraps the record, and `classify` reports
/// `Alive` only when a record is present.
pub fn stop<F: RecordFile, P: LivenessProbe, T: Terminator, K: Sleeper>(
    store: &DaemonRecordStore<F>,
    probe: &P,
    terminator: &T,
    sleeper: &K,
    info: &AppInfo,
) -> io::Result<String> {
    let record = store.load()?;
    let alive = record
        .as_ref()
        .is_some_and(|record| probe.is_alive(record.pid));
    let describe = info.describe();
    match classify(record.as_ref(), alive) {
        DaemonState::Alive => {
            let record = record
                .as_ref()
                .expect("classify reports Alive only for a present record");
            let pid = record.pid;
            terminator.terminate(pid)?;
            wait_for_owner_cleanup(store, probe, sleeper, record)?;
            Ok(format!("{describe}: daemon stopped (pid {pid})"))
        }
        DaemonState::Stale => {
            let record = record
                .as_ref()
                .expect("classify reports Stale only for a present record");
            store.clear_if(record)?;
            Ok(format!("{describe}: cleared stale daemon record"))
        }
        DaemonState::Absent => Ok(format!("{describe}: daemon not running")),
    }
}

fn wait_for_owner_cleanup<F: RecordFile, P: LivenessProbe, K: Sleeper>(
    store: &DaemonRecordStore<F>,
    probe: &P,
    sleeper: &K,
    expected: &usagi_core::domain::daemon::DaemonRecord,
) -> io::Result<()> {
    for poll in 0..=MAX_CLEANUP_POLLS {
        match store.load()? {
            Some(current) if current == *expected => {
                if !probe.is_alive(expected.pid) {
                    // The owner may retire, clear, and exit between our record
                    // read and liveness probe. Recheck the completion fence so
                    // a successful cleanup in that window is not reported as
                    // an incomplete shutdown.
                    return match store.load()? {
                        Some(current) if current == *expected => Err(io::Error::other(format!(
                            "daemon {} exited before endpoint cleanup completed",
                            expected.pid
                        ))),
                        Some(_) | None => Ok(()),
                    };
                }
            }
            Some(_) | None => return Ok(()),
        }
        if poll < MAX_CLEANUP_POLLS {
            sleeper.sleep();
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "daemon {} did not complete endpoint cleanup within the shutdown window",
            expected.pid
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::stop;
    use crate::test_support::{FixedProbe, InMemoryRecordFile, NoopSleeper, RecordingTerminator};
    use std::cell::Cell;
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::DaemonRecord;
    use usagi_core::infrastructure::daemon::{
        DaemonRecordStore, LivenessProbe, Sleeper, Terminator,
    };

    fn replacement_of(record: &DaemonRecord) -> DaemonRecord {
        DaemonRecord {
            pid: record.pid,
            started_at: record.started_at + chrono::Duration::nanoseconds(1),
        }
    }

    struct ReplacingProbe<'a> {
        store: &'a DaemonRecordStore<InMemoryRecordFile>,
        replacement: DaemonRecord,
    }

    impl LivenessProbe for ReplacingProbe<'_> {
        fn is_alive(&self, _pid: u32) -> bool {
            self.store.save(&self.replacement).unwrap();
            false
        }
    }

    struct ReplacingTerminator<'a> {
        store: &'a DaemonRecordStore<InMemoryRecordFile>,
        replacement: DaemonRecord,
    }

    impl Terminator for ReplacingTerminator<'_> {
        fn terminate(&self, _pid: u32) -> std::io::Result<()> {
            self.store.save(&self.replacement)
        }
    }

    struct OwnerCleanupSleeper<'a> {
        store: &'a DaemonRecordStore<InMemoryRecordFile>,
        expected: &'a DaemonRecord,
        calls: Cell<u8>,
    }

    impl Sleeper for OwnerCleanupSleeper<'_> {
        fn sleep(&self) {
            assert_eq!(self.store.load().unwrap().as_ref(), Some(self.expected));
            assert!(self.store.clear_if(self.expected).unwrap());
            self.calls.set(self.calls.get() + 1);
        }
    }

    struct AliveThenGoneProbe {
        calls: Cell<u8>,
    }

    impl LivenessProbe for AliveThenGoneProbe {
        fn is_alive(&self, _pid: u32) -> bool {
            let alive = self.calls.get() == 0;
            self.calls.set(self.calls.get() + 1);
            alive
        }
    }

    struct CleanupWhileBecomingGoneProbe<'a> {
        store: &'a DaemonRecordStore<InMemoryRecordFile>,
        expected: &'a DaemonRecord,
        calls: Cell<u8>,
    }

    impl LivenessProbe for CleanupWhileBecomingGoneProbe<'_> {
        fn is_alive(&self, _pid: u32) -> bool {
            let calls = self.calls.get();
            self.calls.set(calls + 1);
            if calls == 0 {
                true
            } else {
                assert!(self.store.clear_if(self.expected).unwrap());
                false
            }
        }
    }

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
            stop(
                &store,
                &FixedProbe(false),
                &terminator,
                &NoopSleeper,
                &info(),
            )
            .unwrap(),
            "usagi v0.1.0: daemon not running"
        );
        assert!(terminator.terminated().is_empty());
    }

    #[test]
    fn running_stop_keeps_the_record_until_owner_cleanup_completes() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let record = DaemonRecord::new(4321);
        store.save(&record).unwrap();
        let terminator = RecordingTerminator::default();
        let cleanup = OwnerCleanupSleeper {
            store: &store,
            expected: &record,
            calls: Cell::new(0),
        };
        assert_eq!(
            stop(&store, &FixedProbe(true), &terminator, &cleanup, &info(),).unwrap(),
            "usagi v0.1.0: daemon stopped (pid 4321)"
        );
        assert_eq!(terminator.terminated(), vec![4321]);
        assert_eq!(cleanup.calls.get(), 1);
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn clears_stale_record_without_terminating() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        let terminator = RecordingTerminator::default();
        assert_eq!(
            stop(
                &store,
                &FixedProbe(false),
                &terminator,
                &NoopSleeper,
                &info(),
            )
            .unwrap(),
            "usagi v0.1.0: cleared stale daemon record"
        );
        assert!(terminator.terminated().is_empty());
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn running_stop_resuming_after_replacement_save_preserves_the_replacement() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let old = DaemonRecord::new(4321);
        let replacement = replacement_of(&old);
        store.save(&old).unwrap();

        assert_eq!(
            stop(
                &store,
                &FixedProbe(true),
                &ReplacingTerminator {
                    store: &store,
                    replacement: replacement.clone(),
                },
                &NoopSleeper,
                &info(),
            )
            .unwrap(),
            "usagi v0.1.0: daemon stopped (pid 4321)"
        );
        assert_eq!(store.load().unwrap(), Some(replacement));
    }

    #[test]
    fn stale_stop_racing_a_replacement_save_preserves_the_replacement() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let old = DaemonRecord::new(4321);
        let replacement = replacement_of(&old);
        store.save(&old).unwrap();

        assert_eq!(
            stop(
                &store,
                &ReplacingProbe {
                    store: &store,
                    replacement: replacement.clone(),
                },
                &RecordingTerminator::default(),
                &NoopSleeper,
                &info(),
            )
            .unwrap(),
            "usagi v0.1.0: cleared stale daemon record"
        );
        assert_eq!(store.load().unwrap(), Some(replacement));
    }

    #[test]
    fn propagates_terminate_error_and_keeps_record() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let record = DaemonRecord::new(4321);
        store.save(&record).unwrap();
        let terminator = RecordingTerminator::failing();
        assert!(
            stop(
                &store,
                &FixedProbe(true),
                &terminator,
                &NoopSleeper,
                &info(),
            )
            .is_err()
        );
        // The stop failed before clearing, so the record survives for a retry.
        assert_eq!(store.load().unwrap(), Some(record));
    }

    #[test]
    fn propagates_load_error() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
        let terminator = RecordingTerminator::default();
        assert!(
            stop(
                &store,
                &FixedProbe(true),
                &terminator,
                &NoopSleeper,
                &info(),
            )
            .is_err()
        );
    }

    #[test]
    fn owner_exit_before_cleanup_is_an_error_and_keeps_the_record() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let record = DaemonRecord::new(4321);
        store.save(&record).unwrap();
        let terminator = RecordingTerminator::default();
        let error = stop(
            &store,
            &AliveThenGoneProbe {
                calls: Cell::new(0),
            },
            &terminator,
            &NoopSleeper,
            &info(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("before endpoint cleanup completed")
        );
        assert_eq!(store.load().unwrap(), Some(record));
    }

    #[test]
    fn owner_cleanup_between_record_and_liveness_checks_is_successful() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let record = DaemonRecord::new(4321);
        store.save(&record).unwrap();
        let probe = CleanupWhileBecomingGoneProbe {
            store: &store,
            expected: &record,
            calls: Cell::new(0),
        };

        assert_eq!(
            stop(
                &store,
                &probe,
                &RecordingTerminator::default(),
                &NoopSleeper,
                &info(),
            )
            .unwrap(),
            "usagi v0.1.0: daemon stopped (pid 4321)"
        );
        assert_eq!(probe.calls.get(), 2);
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn times_out_while_a_signalled_owner_keeps_its_record() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let record = DaemonRecord::new(4321);
        store.save(&record).unwrap();
        let error = stop(
            &store,
            &FixedProbe(true),
            &RecordingTerminator::default(),
            &NoopSleeper,
            &info(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(store.load().unwrap(), Some(record));
    }
}
