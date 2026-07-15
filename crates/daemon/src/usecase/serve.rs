//! The `usagi daemon serve` usecase: run the daemon in the foreground.
//!
//! `serve` is the daemon process itself (a hidden subcommand; `usagi daemon`
//! with no subcommand runs it too). It owns its record's lifecycle:
//!
//! 1. **single-instance guard** — acquire the [`InstanceLock`]; if another
//!    daemon holds it, refuse rather than start a second one;
//! 2. **register** — otherwise overwrite any stale record with this process's
//!    pid in `daemon.json`;
//! 3. **publish** — expose its endpoint only after the lock and record prove it
//!    is the active daemon;
//! 4. **run** — block until asked to shut down;
//! 5. **deregister** — clear the record on the way out. The lock is released by
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

/// Run the daemon in the foreground under process id `pid`, writing progress
/// lines to `out`.
///
/// # Errors
///
/// Returns the lock's acquire error, the store's load / save / clear error, the
/// ready publication / shutdown signal error, or an `out` write error.
#[coverage(off)]
pub fn serve<W: Write, F: RecordFile, R: DaemonReady, S: ShutdownSignal, M: InstanceLock>(
    out: &mut W,
    store: &DaemonRecordStore<F>,
    ready: &R,
    shutdown: &S,
    lock: &M,
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
    writeln!(out, "{describe}: daemon serving (pid {pid})")?;

    shutdown.wait()?;

    store.clear()?;
    writeln!(out, "{describe}: daemon stopped (pid {pid})")
}

#[cfg(test)]
mod tests {
    use super::serve;
    use crate::test_support::{
        FailingShutdown, FakeLock, ImmediateShutdown, InMemoryRecordFile, NoopReady,
    };
    use std::cell::Cell;
    use std::io;
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::DaemonRecord;
    use usagi_core::infrastructure::daemon::{DaemonReady, DaemonRecordStore};

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
    }

    struct FailingReady;
    impl DaemonReady for FailingReady {
        fn publish(&self) -> io::Result<()> {
            Err(io::Error::other("publish failed"))
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
}
