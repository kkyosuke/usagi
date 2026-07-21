//! The `usagi daemon serve` usecase: run the daemon in the foreground.
//!
//! `serve` is the daemon process itself (a hidden subcommand; `usagi daemon`
//! with no subcommand runs it too). It owns its record's lifecycle:
//!
//! 1. **single-instance guard** — acquire the [`InstanceLock`]; if another
//!    daemon holds it, refuse rather than start a second one;
//! 2. **prepare** — arrange shutdown delivery before any worker is spawned;
//! 3. **register** — overwrite any stale record with this process's pid in
//!    `daemon.json`;
//! 4. **publish** — expose its endpoint only after the lock and record prove it
//!    is the active daemon;
//! 5. **run** — block until asked to shut down;
//! 6. **retire** — stop and join endpoint admission, clear the lifecycle record,
//!    then generation-conditionally unlink the endpoint. The lock is released by
//!    the OS when the process exits.
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
    /// Clears the active daemon record.
    ///
    /// # Errors
    ///
    /// Returns the underlying durable store error.
    fn clear(&self) -> io::Result<()>;
}

impl<F: RecordFile> DaemonRecordPort for DaemonRecordStore<F> {
    fn load(&self) -> io::Result<Option<DaemonRecord>> {
        DaemonRecordStore::load(self)
    }

    fn save(&self, record: &DaemonRecord) -> io::Result<()> {
        DaemonRecordStore::save(self, record)
    }

    fn clear(&self) -> io::Result<()> {
        DaemonRecordStore::clear(self)
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

    // We hold the lock. Overwrite any stale record and register this process.
    store.save(&DaemonRecord::new(pid))?;
    if let Err(error) = ready.publish() {
        // A failed endpoint was never usable, so leave no live-looking record
        // for a process that has not begun serving. Preserve the publish error:
        // a cleanup failure only leaves a stale record, which status/stop can
        // safely reclaim after this process exits.
        let _ = store.clear();
        return Err(error);
    }
    if let Err(error) = writeln!(out, "{describe}: daemon serving (pid {pid})") {
        retire_after_failure(ready);
        let _ = store.clear();
        return Err(error);
    }

    if let Err(error) = shutdown.wait() {
        // The record deliberately survives an abnormal wait failure so status /
        // stop can report and reclaim it after this process exits. The endpoint,
        // however, must never outlive its owner.
        retire_after_failure(ready);
        return Err(error);
    }

    if let Err(error) = ready.quiesce() {
        // `retire` is idempotent and may still complete cleanup when the first
        // join attempt reported an error (or its worker unwound).
        let _ = ready.retire();
        return Err(error);
    }
    // Once admission is joined, remove discovery metadata in the order clients
    // need for a clean handoff: the old PID record disappears before the locator
    // becomes NotFound, while the instance lock still excludes a replacement.
    let clear = store.clear();
    let retire = ready.retire();
    clear?;
    retire?;
    writeln!(out, "{describe}: daemon stopped (pid {pid})")
}

fn retire_after_failure(ready: &dyn DaemonReady) {
    let _ = ready.quiesce();
    let _ = ready.retire();
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
        fn remove(&self) -> io::Result<()> {
            if self.remove {
                Err(io::Error::other("remove"))
            } else {
                Ok(())
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

    struct FailingReady;
    impl DaemonReady for FailingReady {
        fn publish(&self) -> io::Result<()> {
            Err(io::Error::other("publish failed"))
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
    impl DaemonReady for OrderedReady<'_> {
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
            assert_eq!(self.store.load().unwrap(), None);
            self.events.borrow_mut().push("retire");
            Ok(())
        }
    }

    struct PrepareFailure;
    impl usagi_core::infrastructure::daemon::ShutdownSignal for PrepareFailure {
        fn prepare(&self) -> io::Result<()> {
            Err(io::Error::other("prepare failed"))
        }

        fn wait(&self) -> io::Result<()> {
            panic!("wait must not run after prepare fails")
        }
    }

    struct CleanupReady {
        fail_quiesce: bool,
        fail_retire: bool,
        quiesces: Cell<u8>,
        retires: Cell<u8>,
    }
    impl DaemonReady for CleanupReady {
        fn publish(&self) -> io::Result<()> {
            Ok(())
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
            ["prepare", "publish", "wait", "quiesce", "retire"]
        );
    }

    #[test]
    fn preparation_failure_never_registers_or_publishes() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert!(
            serve(
                &mut Vec::new(),
                &store,
                &NoopReady,
                &PrepareFailure,
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
        assert!(store.load().unwrap().is_some());

        let retire_failure = CleanupReady {
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
        assert_eq!(store.load().unwrap(), None);
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
            &ImmediateShutdown,
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
    fn clears_the_record_when_endpoint_publication_fails() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert!(
            serve(
                &mut Vec::new(),
                &store,
                &FailingReady,
                &ImmediateShutdown,
                &FakeLock::Acquired,
                2222,
                &info(),
            )
            .is_err()
        );
        assert_eq!(store.load().unwrap(), None);
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
    fn propagates_wait_error_and_keeps_the_record() {
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
        // Registered before the failing wait, so the record survives for status/stop.
        assert_eq!(store.load().unwrap().map(|record| record.pid), Some(2222));
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
        usagi_core::infrastructure::daemon::RecordFile::remove(&healthy).unwrap();
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
