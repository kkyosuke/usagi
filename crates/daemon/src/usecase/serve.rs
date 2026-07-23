//! The `usagi daemon serve` usecase: run the daemon in the foreground.
//!
//! `serve` is the daemon process itself (a hidden subcommand; `usagi daemon`
//! with no subcommand runs it too). It owns its record's lifecycle:
//!
//! 1. **single-instance guard** — acquire the [`InstanceLock`]; if another
//!    daemon holds it, refuse rather than start a second one;
//! 2. **prepare** — arrange shutdown delivery before any worker is spawned;
//! 3. **recover** — snapshot the previous lifecycle record, retire its stale
//!    endpoint, and prove that the record was not concurrently replaced;
//! 4. **register** — replace the unchanged stale record with this process's pid
//!    in `daemon.json`;
//! 5. **publish** — expose its endpoint only after the lock and record prove it
//!    is the active daemon;
//! 6. **run** — block until asked to shut down;
//! 7. **retire** — stop and join endpoint admission, generation-conditionally
//!    unlink the endpoint, then conditionally clear this exact lifecycle record.
//!    The lock is released by the OS when the process exits.
//!
//! The lock is the authoritative guard: because it waits briefly for a departing
//! holder, a `restart` hands off cleanly, and because the OS drops it on death it
//! also excludes a crashed daemon's leftovers. The record is only how clients
//! discover the pid, so `serve` reads it (without probing) to name the holder
//! when refused.
//!
//! The store's file seam, the shutdown signal, and the lock are injected, so
//! this stays pure and fully testable; the synthesis root binds the real
//! filesystem, signal wait, and file lock.

use std::io::{self, Write};

use usagi_core::domain::AppInfo;
use usagi_core::domain::daemon::DaemonRecord;
use usagi_core::infrastructure::daemon::{
    DaemonReady, DaemonRecordStore, InstanceLock, RecordFile, ShutdownSignal,
};

/// Type-erased durable record port used by the production composition and
/// failpoint tests, so both exercise the same serve state machine symbol.
pub trait DaemonRecordPort {
    /// Loads the current record, or reports a durable store failure.
    ///
    /// # Errors
    ///
    /// Returns the underlying durable store error.
    fn load(&self) -> io::Result<Option<DaemonRecord>>;
    /// Saves the active daemon record.
    ///
    /// # Errors
    ///
    /// Returns the underlying durable store error.
    fn save(&self, record: &DaemonRecord) -> io::Result<()>;
    /// Clears the active daemon record only if it still equals `expected`.
    ///
    /// # Errors
    ///
    /// Returns the underlying durable store error.
    fn clear_if(&self, expected: &DaemonRecord) -> io::Result<bool>;
}

impl<F: RecordFile> DaemonRecordPort for DaemonRecordStore<F> {
    fn load(&self) -> io::Result<Option<DaemonRecord>> {
        DaemonRecordStore::load(self)
    }

    fn save(&self, record: &DaemonRecord) -> io::Result<()> {
        DaemonRecordStore::save(self, record)
    }

    fn clear_if(&self, expected: &DaemonRecord) -> io::Result<bool> {
        DaemonRecordStore::clear_if(self, expected)
    }
}

/// Run the daemon in the foreground under process id `pid`, writing progress
/// lines to `out`.
///
/// # Errors
///
/// Returns the lock's acquire error, the store's load / save / clear error, the
/// shutdown preparation / wait error, the endpoint publish / quiesce / retire
/// error, or an `out` write error.
pub fn serve(
    out: &mut dyn Write,
    store: &dyn DaemonRecordPort,
    ready: &dyn DaemonReady,
    shutdown: &dyn ShutdownSignal,
    lock: &dyn InstanceLock,
    pid: u32,
    info: &AppInfo,
) -> io::Result<()> {
    let describe = info.describe();

    if !lock.acquire()? {
        // Another daemon holds the lock. Name it from its record if we can; a
        // live holder always has one, but tolerate a missing/racing record.
        return match store.load()?.map(|record| record.pid) {
            Some(running) => writeln!(out, "{describe}: daemon already running (pid {running})"),
            None => writeln!(out, "{describe}: daemon already running"),
        };
    }

    // Prepare signal delivery before registration makes this process visible or
    // endpoint publication spawns workers. A stop arriving after registration
    // can therefore only take the owner cleanup path below.
    shutdown.prepare()?;

    // The instance lock proves that the previous process is inactive, but its
    // endpoint and lifecycle record may remain after an abnormal exit. Retire
    // the endpoint before replacing the record so cleanup remains attributable
    // to the previous incarnation. An exact recheck fences a concurrent
    // stop/replacement from being overwritten after recovery.
    let previous = store.load()?;
    ready.recover_stale_endpoint()?;
    if store.load()? != previous {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "daemon record changed during stale endpoint recovery",
        ));
    }

    // Recovery has proved that no inactive endpoint remains and the snapshot is
    // still current. Register this process before publishing its new endpoint.
    let record = DaemonRecord::new(pid);
    store.save(&record)?;
    if let Err(error) = ready.publish() {
        // Binding may already have published a locator before a later startup
        // step failed. Clear the lifecycle fence only after retryable endpoint
        // ownership proves every artifact was retired.
        clear_after_retire(ready, store, &record);
        return Err(error);
    }
    if let Err(error) = writeln!(out, "{describe}: daemon serving (pid {pid})") {
        retire_and_clear_after_failure(ready, store, &record);
        return Err(error);
    }

    if let Err(error) = shutdown.wait() {
        // Preserve the primary wait error while best-effort cleanup removes only
        // this owner's metadata. A concurrently saved replacement survives.
        retire_and_clear_after_failure(ready, store, &record);
        return Err(error);
    }

    if let Err(error) = ready.quiesce() {
        // `retire` is idempotent and may still complete cleanup when the first
        // join attempt reported an error (or its worker unwound).
        clear_after_retire(ready, store, &record);
        return Err(error);
    }
    // Keep the exact record as a completion fence until the generation endpoint
    // is gone. A stop waiter can treat record disappearance as proof that join
    // and generation-fenced retirement succeeded, while a retirement failure
    // remains fail-closed and diagnosable through the retained record.
    ready.retire()?;
    store.clear_if(&record)?;
    writeln!(out, "{describe}: daemon stopped (pid {pid})")
}

fn retire_and_clear_after_failure(
    ready: &dyn DaemonReady,
    store: &dyn DaemonRecordPort,
    record: &DaemonRecord,
) {
    let _ = ready.quiesce();
    clear_after_retire(ready, store, record);
}

fn clear_after_retire(
    ready: &dyn DaemonReady,
    store: &dyn DaemonRecordPort,
    record: &DaemonRecord,
) {
    if ready.retire().is_ok() {
        let _ = store.clear_if(record);
    }
}

#[cfg(test)]
mod tests {
    use super::serve;
    use crate::test_support::{
        FailingShutdown, FakeLock, ImmediateShutdown, InMemoryRecordFile, NoopReady,
    };
    use std::cell::{Cell, RefCell};
    use std::io;
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::DaemonRecord;
    use usagi_core::infrastructure::daemon::{DaemonReady, DaemonRecordStore};

    struct FailingRecordFile {
        write: bool,
        remove: bool,
    }
    impl usagi_core::infrastructure::daemon::RecordFile for FailingRecordFile {
        fn read(&self) -> io::Result<Option<String>> {
            Ok(None)
        }
        fn write(&self, _: &str) -> io::Result<()> {
            if self.write {
                Err(io::Error::other("write"))
            } else {
                Ok(())
            }
        }
        fn remove_if(&self, _: &str) -> io::Result<bool> {
            if self.remove {
                Err(io::Error::other("remove"))
            } else {
                Ok(false)
            }
        }
    }

    struct BrokenWriter;
    impl io::Write for BrokenWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("output"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    /// Serve with the lock acquired and an immediate shutdown, returning output.
    fn serve_lines(store: &DaemonRecordStore<InMemoryRecordFile>, pid: u32) -> String {
        let mut buf = Vec::new();
        serve(
            &mut buf,
            store,
            &NoopReady,
            &ImmediateShutdown,
            &FakeLock::Acquired,
            pid,
            &info(),
        )
        .unwrap();
        String::from_utf8(buf).unwrap()
    }

    struct RecordingReady<'a> {
        store: &'a DaemonRecordStore<InMemoryRecordFile>,
        published: Cell<u8>,
    }
    impl DaemonReady for RecordingReady<'_> {
        fn recover_stale_endpoint(&self) -> io::Result<()> {
            Ok(())
        }

        fn publish(&self) -> io::Result<()> {
            assert_eq!(
                self.store.load().unwrap().map(|record| record.pid),
                Some(2222)
            );
            self.published.set(self.published.get() + 1);
            Ok(())
        }

        fn quiesce(&self) -> io::Result<()> {
            Ok(())
        }

        fn retire(&self) -> io::Result<()> {
            Ok(())
        }
    }

    struct OrderedShutdown<'a> {
        events: &'a RefCell<Vec<&'static str>>,
        store: &'a DaemonRecordStore<InMemoryRecordFile>,
    }
    impl usagi_core::infrastructure::daemon::ShutdownSignal for OrderedShutdown<'_> {
        fn prepare(&self) -> io::Result<()> {
            assert_eq!(self.store.load().unwrap(), None);
            self.events.borrow_mut().push("prepare");
            Ok(())
        }

        fn wait(&self) -> io::Result<()> {
            self.events.borrow_mut().push("wait");
            Ok(())
        }
    }

    struct OrderedReady<'a> {
        events: &'a RefCell<Vec<&'static str>>,
        store: &'a DaemonRecordStore<InMemoryRecordFile>,
    }

    struct ReplacingReady<'a> {
        store: &'a DaemonRecordStore<InMemoryRecordFile>,
        replacement: RefCell<Option<DaemonRecord>>,
    }
    impl DaemonReady for ReplacingReady<'_> {
        fn recover_stale_endpoint(&self) -> io::Result<()> {
            Ok(())
        }

        fn publish(&self) -> io::Result<()> {
            Ok(())
        }

        fn quiesce(&self) -> io::Result<()> {
            let old = self.store.load()?.expect("serve registered its record");
            let replacement = DaemonRecord {
                pid: old.pid,
                started_at: old.started_at + chrono::Duration::nanoseconds(1),
            };
            self.store.save(&replacement)?;
            *self.replacement.borrow_mut() = Some(replacement);
            Ok(())
        }

        fn retire(&self) -> io::Result<()> {
            assert_eq!(self.store.load()?, *self.replacement.borrow());
            Ok(())
        }
    }
    impl DaemonReady for OrderedReady<'_> {
        fn recover_stale_endpoint(&self) -> io::Result<()> {
            assert_eq!(self.store.load().unwrap(), None);
            self.events.borrow_mut().push("recover");
            Ok(())
        }

        fn publish(&self) -> io::Result<()> {
            assert!(self.store.load().unwrap().is_some());
            self.events.borrow_mut().push("publish");
            Ok(())
        }

        fn quiesce(&self) -> io::Result<()> {
            assert!(self.store.load().unwrap().is_some());
            self.events.borrow_mut().push("quiesce");
            Ok(())
        }

        fn retire(&self) -> io::Result<()> {
            assert!(self.store.load().unwrap().is_some());
            self.events.borrow_mut().push("retire");
            Ok(())
        }
    }

    struct ConfigurableShutdown {
        fail_prepare: bool,
    }
    impl usagi_core::infrastructure::daemon::ShutdownSignal for ConfigurableShutdown {
        fn prepare(&self) -> io::Result<()> {
            if self.fail_prepare {
                Err(io::Error::other("prepare failed"))
            } else {
                Ok(())
            }
        }

        fn wait(&self) -> io::Result<()> {
            Ok(())
        }
    }

    struct CleanupReady {
        fail_publish: bool,
        fail_quiesce: bool,
        fail_retire: bool,
        quiesces: Cell<u8>,
        retires: Cell<u8>,
    }
    impl DaemonReady for CleanupReady {
        fn recover_stale_endpoint(&self) -> io::Result<()> {
            Ok(())
        }

        fn publish(&self) -> io::Result<()> {
            if self.fail_publish {
                Err(io::Error::other("publish failed"))
            } else {
                Ok(())
            }
        }

        fn quiesce(&self) -> io::Result<()> {
            self.quiesces.set(self.quiesces.get() + 1);
            if self.fail_quiesce {
                Err(io::Error::other("quiesce failed"))
            } else {
                Ok(())
            }
        }

        fn retire(&self) -> io::Result<()> {
            self.retires.set(self.retires.get() + 1);
            if self.fail_retire {
                Err(io::Error::other("retire failed"))
            } else {
                Ok(())
            }
        }
    }

    struct CountingRecoveryReady<'a> {
        recoveries: Cell<u8>,
        publishes: Cell<u8>,
        replacement: Option<(&'a DaemonRecordStore<InMemoryRecordFile>, DaemonRecord)>,
    }
    impl DaemonReady for CountingRecoveryReady<'_> {
        fn recover_stale_endpoint(&self) -> io::Result<()> {
            self.recoveries.set(self.recoveries.get() + 1);
            if let Some((store, replacement)) = &self.replacement {
                store.save(replacement)?;
            }
            Ok(())
        }

        fn publish(&self) -> io::Result<()> {
            self.publishes.set(self.publishes.get() + 1);
            Ok(())
        }

        fn quiesce(&self) -> io::Result<()> {
            Ok(())
        }

        fn retire(&self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailOnceRecoveryReady {
        recoveries: Cell<u8>,
        publishes: Cell<u8>,
    }
    impl DaemonReady for FailOnceRecoveryReady {
        fn recover_stale_endpoint(&self) -> io::Result<()> {
            let attempt = self.recoveries.get() + 1;
            self.recoveries.set(attempt);
            if attempt == 1 {
                Err(io::Error::other("recovery failed"))
            } else {
                Ok(())
            }
        }

        fn publish(&self) -> io::Result<()> {
            self.publishes.set(self.publishes.get() + 1);
            Ok(())
        }

        fn quiesce(&self) -> io::Result<()> {
            Ok(())
        }

        fn retire(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn registers_serves_and_clears_when_it_holds_the_lock() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert_eq!(
            serve_lines(&store, 2222),
            "usagi v0.1.0: daemon serving (pid 2222)\nusagi v0.1.0: daemon stopped (pid 2222)\n"
        );
        // The record is cleared on the way out.
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn prepares_before_publication_then_quiesces_and_retires_before_exit() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let events = RefCell::new(Vec::new());
        let ready = OrderedReady {
            events: &events,
            store: &store,
        };
        let shutdown = OrderedShutdown {
            events: &events,
            store: &store,
        };
        serve(
            &mut Vec::new(),
            &store,
            &ready,
            &shutdown,
            &FakeLock::Acquired,
            2222,
            &info(),
        )
        .unwrap();
        assert_eq!(
            events.into_inner(),
            ["prepare", "recover", "publish", "wait", "quiesce", "retire"]
        );
    }

    #[test]
    fn recovery_failure_preserves_the_previous_record_and_retry_can_start() {
        let previous = DaemonRecord::new(1111);
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&previous).unwrap();
        let ready = FailOnceRecoveryReady {
            recoveries: Cell::new(0),
            publishes: Cell::new(0),
        };

        assert!(
            serve(
                &mut Vec::new(),
                &store,
                &ready,
                &ImmediateShutdown,
                &FakeLock::Acquired,
                2222,
                &info(),
            )
            .is_err()
        );
        assert_eq!(store.load().unwrap(), Some(previous));
        assert_eq!(ready.recoveries.get(), 1);
        assert_eq!(ready.publishes.get(), 0);

        serve(
            &mut Vec::new(),
            &store,
            &ready,
            &ImmediateShutdown,
            &FakeLock::Acquired,
            2222,
            &info(),
        )
        .unwrap();
        assert_eq!(store.load().unwrap(), None);
        assert_eq!(ready.recoveries.get(), 2);
        assert_eq!(ready.publishes.get(), 1);
    }

    #[test]
    fn exact_recheck_preserves_a_replacement_and_never_publishes() {
        let previous = DaemonRecord::new(1111);
        let replacement = DaemonRecord::new(3333);
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&previous).unwrap();
        let ready = CountingRecoveryReady {
            recoveries: Cell::new(0),
            publishes: Cell::new(0),
            replacement: Some((&store, replacement.clone())),
        };

        let error = serve(
            &mut Vec::new(),
            &store,
            &ready,
            &ImmediateShutdown,
            &FakeLock::Acquired,
            2222,
            &info(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(store.load().unwrap(), Some(replacement));
        assert_eq!(ready.recoveries.get(), 1);
        assert_eq!(ready.publishes.get(), 0);
    }

    #[test]
    fn recheck_read_failure_preserves_the_previous_record_and_never_publishes() {
        let previous = DaemonRecord::new(1111);
        let contents = serde_json::to_string(&previous).unwrap();
        let store = DaemonRecordStore::new(InMemoryRecordFile::failing_read_on(&contents, 1));
        let ready = CountingRecoveryReady {
            recoveries: Cell::new(0),
            publishes: Cell::new(0),
            replacement: None,
        };

        assert!(
            serve(
                &mut Vec::new(),
                &store,
                &ready,
                &ImmediateShutdown,
                &FakeLock::Acquired,
                2222,
                &info(),
            )
            .is_err()
        );
        assert_eq!(store.load().unwrap(), Some(previous));
        assert_eq!(ready.recoveries.get(), 1);
        assert_eq!(ready.publishes.get(), 0);
    }

    #[test]
    fn recovers_before_registration_even_when_the_previous_record_is_absent() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let ready = CountingRecoveryReady {
            recoveries: Cell::new(0),
            publishes: Cell::new(0),
            replacement: None,
        };

        serve(
            &mut Vec::new(),
            &store,
            &ready,
            &ImmediateShutdown,
            &FakeLock::Acquired,
            2222,
            &info(),
        )
        .unwrap();

        assert_eq!(ready.recoveries.get(), 1);
        assert_eq!(ready.publishes.get(), 1);
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn preparation_failure_never_registers_or_publishes() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert!(
            serve(
                &mut Vec::new(),
                &store,
                &NoopReady,
                &ConfigurableShutdown { fail_prepare: true },
                &FakeLock::Acquired,
                2222,
                &info(),
            )
            .is_err()
        );
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn cleanup_failures_are_reported_without_skipping_retirement() {
        let quiesce_failure = CleanupReady {
            fail_publish: false,
            fail_quiesce: true,
            fail_retire: false,
            quiesces: Cell::new(0),
            retires: Cell::new(0),
        };
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert!(
            serve(
                &mut Vec::new(),
                &store,
                &quiesce_failure,
                &ImmediateShutdown,
                &FakeLock::Acquired,
                2222,
                &info(),
            )
            .is_err()
        );
        assert_eq!(quiesce_failure.quiesces.get(), 1);
        assert_eq!(quiesce_failure.retires.get(), 1);
        assert_eq!(store.load().unwrap(), None);

        let retire_failure = CleanupReady {
            fail_publish: false,
            fail_quiesce: false,
            fail_retire: true,
            quiesces: Cell::new(0),
            retires: Cell::new(0),
        };
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert!(
            serve(
                &mut Vec::new(),
                &store,
                &retire_failure,
                &ImmediateShutdown,
                &FakeLock::Acquired,
                2222,
                &info(),
            )
            .is_err()
        );
        assert_eq!(retire_failure.quiesces.get(), 1);
        assert_eq!(retire_failure.retires.get(), 1);
        assert!(store.load().unwrap().is_some());
    }

    #[test]
    fn publishes_after_registration_and_never_when_the_lock_is_held() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let ready = RecordingReady {
            store: &store,
            published: Cell::new(0),
        };
        serve(
            &mut Vec::new(),
            &store,
            &ready,
            &ConfigurableShutdown {
                fail_prepare: false,
            },
            &FakeLock::Acquired,
            2222,
            &info(),
        )
        .unwrap();
        assert_eq!(ready.published.get(), 1);

        let refused = DaemonRecordStore::new(InMemoryRecordFile::default());
        let refused_ready = RecordingReady {
            store: &refused,
            published: Cell::new(0),
        };
        serve(
            &mut Vec::new(),
            &refused,
            &refused_ready,
            &ImmediateShutdown,
            &FakeLock::Held,
            2222,
            &info(),
        )
        .unwrap();
        assert_eq!(refused_ready.published.get(), 0);
    }

    #[test]
    fn late_owner_cleanup_preserves_a_replacement_incarnation() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let ready = ReplacingReady {
            store: &store,
            replacement: RefCell::new(None),
        };

        serve(
            &mut Vec::new(),
            &store,
            &ready,
            &ImmediateShutdown,
            &FakeLock::Acquired,
            2222,
            &info(),
        )
        .unwrap();

        assert_eq!(store.load().unwrap(), ready.replacement.into_inner());
    }

    #[test]
    fn clears_the_record_when_endpoint_publication_fails() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let ready = CleanupReady {
            fail_publish: true,
            fail_quiesce: false,
            fail_retire: false,
            quiesces: Cell::new(0),
            retires: Cell::new(0),
        };
        assert!(
            serve(
                &mut Vec::new(),
                &store,
                &ready,
                &ImmediateShutdown,
                &FakeLock::Acquired,
                2222,
                &info(),
            )
            .is_err()
        );
        assert_eq!(store.load().unwrap(), None);
        assert_eq!(ready.quiesces.get(), 0);
        assert_eq!(ready.retires.get(), 1);
    }

    #[test]
    fn publication_cleanup_failure_retains_the_record_for_stale_recovery() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let ready = CleanupReady {
            fail_publish: true,
            fail_quiesce: false,
            fail_retire: true,
            quiesces: Cell::new(0),
            retires: Cell::new(0),
        };

        assert!(
            serve(
                &mut Vec::new(),
                &store,
                &ready,
                &ImmediateShutdown,
                &FakeLock::Acquired,
                2222,
                &info(),
            )
            .is_err()
        );
        assert_eq!(ready.retires.get(), 1);
        assert!(store.load().unwrap().is_some());
    }

    #[test]
    fn overwrites_a_stale_record_when_it_holds_the_lock() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(9999)).unwrap();
        // Holding the lock means no live daemon; serve overwrites the leftover.
        assert_eq!(
            serve_lines(&store, 2222),
            "usagi v0.1.0: daemon serving (pid 2222)\nusagi v0.1.0: daemon stopped (pid 2222)\n"
        );
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn refuses_and_names_the_holder_when_the_lock_is_held() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let existing = DaemonRecord::new(1111);
        store.save(&existing).unwrap();
        let mut buf = Vec::new();
        serve(
            &mut buf,
            &store,
            &NoopReady,
            &ImmediateShutdown,
            &FakeLock::Held,
            2222,
            &info(),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "usagi v0.1.0: daemon already running (pid 1111)\n"
        );
        // The existing record is left untouched — we did not register or clear.
        assert_eq!(store.load().unwrap(), Some(existing));
    }

    #[test]
    fn refuses_without_a_pid_when_the_lock_is_held_and_no_record_exists() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let mut buf = Vec::new();
        serve(
            &mut buf,
            &store,
            &NoopReady,
            &ImmediateShutdown,
            &FakeLock::Held,
            2222,
            &info(),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "usagi v0.1.0: daemon already running\n"
        );
    }

    #[test]
    fn propagates_lock_error() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let mut buf = Vec::new();
        assert!(
            serve(
                &mut buf,
                &store,
                &NoopReady,
                &ImmediateShutdown,
                &FakeLock::Failing,
                2222,
                &info()
            )
            .is_err()
        );
    }

    #[test]
    fn wait_error_retires_then_clears_the_record() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let mut buf = Vec::new();
        assert!(
            serve(
                &mut buf,
                &store,
                &NoopReady,
                &FailingShutdown,
                &FakeLock::Acquired,
                2222,
                &info()
            )
            .is_err()
        );
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn propagates_load_error_when_refused() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
        let mut buf = Vec::new();
        assert!(
            serve(
                &mut buf,
                &store,
                &NoopReady,
                &ImmediateShutdown,
                &FakeLock::Held,
                2222,
                &info()
            )
            .is_err()
        );
    }

    #[test]
    fn propagates_registration_output_and_final_clear_failures() {
        let healthy = FailingRecordFile {
            write: false,
            remove: false,
        };
        assert!(
            usagi_core::infrastructure::daemon::RecordFile::read(&healthy)
                .unwrap()
                .is_none()
        );
        usagi_core::infrastructure::daemon::RecordFile::write(&healthy, "record").unwrap();
        usagi_core::infrastructure::daemon::RecordFile::remove_if(&healthy, "record").unwrap();
        io::Write::flush(&mut BrokenWriter).unwrap();
        for (file, mut output) in [
            (
                FailingRecordFile {
                    write: true,
                    remove: false,
                },
                Box::new(Vec::new()) as Box<dyn io::Write>,
            ),
            (
                FailingRecordFile {
                    write: false,
                    remove: true,
                },
                Box::new(Vec::new()) as Box<dyn io::Write>,
            ),
        ] {
            assert!(
                serve(
                    &mut output,
                    &DaemonRecordStore::new(file),
                    &NoopReady,
                    &ImmediateShutdown,
                    &FakeLock::Acquired,
                    2222,
                    &info(),
                )
                .is_err()
            );
        }
        assert!(
            serve(
                &mut BrokenWriter,
                &DaemonRecordStore::new(InMemoryRecordFile::default()),
                &NoopReady,
                &ImmediateShutdown,
                &FakeLock::Acquired,
                2222,
                &info(),
            )
            .is_err()
        );
    }
}
