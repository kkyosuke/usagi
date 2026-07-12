//! The `usagi daemon serve` usecase: run the daemon in the foreground.
//!
//! `serve` is the daemon process itself (a hidden subcommand; `usagi daemon`
//! with no subcommand runs it too). It owns its record's lifecycle:
//!
//! 1. **single-instance guard** — if a live daemon already holds the record,
//!    refuse rather than start a second one;
//! 2. **register** — otherwise reclaim any stale record and write this
//!    process's pid to `daemon.json`;
//! 3. **run** — block until asked to shut down;
//! 4. **deregister** — clear the record on the way out.
//!
//! The store's file seam, the probe, and the shutdown signal are injected, so
//! this stays pure and fully testable; the synthesis root binds the real
//! filesystem, process probe, and signal wait. The daemon does not yet own PTYs
//! or watch sessions — that arrives once the control plane is in place.

use std::io::{self, Write};

use usagi_core::domain::AppInfo;
use usagi_core::domain::daemon::{DaemonRecord, DaemonState, classify};
use usagi_core::infrastructure::daemon::{
    DaemonRecordStore, LivenessProbe, RecordFile, ShutdownSignal,
};

/// Run the daemon in the foreground under process id `pid`, writing progress
/// lines to `out`.
///
/// # Errors
///
/// Returns the store's load / save / clear error, the shutdown signal's wait
/// error, or an `out` write error.
///
/// # Panics
///
/// Never in practice: the guard unwraps the record only after `classify`
/// reports `Alive`, which happens only when a record is present.
pub fn serve<W: Write, F: RecordFile, P: LivenessProbe, S: ShutdownSignal>(
    out: &mut W,
    store: &DaemonRecordStore<F>,
    probe: &P,
    shutdown: &S,
    pid: u32,
    info: &AppInfo,
) -> io::Result<()> {
    let existing = store.load()?;
    let alive = existing
        .as_ref()
        .is_some_and(|record| probe.is_alive(record.pid));
    let describe = info.describe();

    if matches!(classify(existing.as_ref(), alive), DaemonState::Alive) {
        let running = existing
            .expect("classify reports Alive only for a present record")
            .pid;
        return writeln!(out, "{describe}: daemon already running (pid {running})");
    }

    // Reclaim any stale record and register this process.
    store.save(&DaemonRecord::new(pid))?;
    writeln!(out, "{describe}: daemon serving (pid {pid})")?;

    shutdown.wait()?;

    store.clear()?;
    writeln!(out, "{describe}: daemon stopped (pid {pid})")
}

#[cfg(test)]
mod tests {
    use super::serve;
    use crate::test_support::{FailingShutdown, FixedProbe, ImmediateShutdown, InMemoryRecordFile};
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::DaemonRecord;
    use usagi_core::infrastructure::daemon::DaemonRecordStore;

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    fn serve_lines(
        store: &DaemonRecordStore<InMemoryRecordFile>,
        probe: &FixedProbe,
        pid: u32,
    ) -> String {
        let mut buf = Vec::new();
        serve(&mut buf, store, probe, &ImmediateShutdown, pid, &info()).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn refuses_when_a_live_daemon_already_holds_the_record() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let existing = DaemonRecord::new(1111);
        store.save(&existing).unwrap();
        assert_eq!(
            serve_lines(&store, &FixedProbe(true), 2222),
            "usagi v0.1.0: daemon already running (pid 1111)\n"
        );
        // The existing record is left untouched — we did not register or clear.
        assert_eq!(store.load().unwrap(), Some(existing));
    }

    #[test]
    fn registers_serves_and_clears_when_no_daemon_runs() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert_eq!(
            serve_lines(&store, &FixedProbe(false), 2222),
            "usagi v0.1.0: daemon serving (pid 2222)\nusagi v0.1.0: daemon stopped (pid 2222)\n"
        );
        // The record is cleared on the way out.
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn reclaims_a_stale_record_before_serving() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(9999)).unwrap();
        // The recorded pid is dead (probe false), so serve reclaims it and runs.
        assert_eq!(
            serve_lines(&store, &FixedProbe(false), 2222),
            "usagi v0.1.0: daemon serving (pid 2222)\nusagi v0.1.0: daemon stopped (pid 2222)\n"
        );
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn propagates_wait_error_and_keeps_the_record() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let mut buf = Vec::new();
        assert!(
            serve(
                &mut buf,
                &store,
                &FixedProbe(false),
                &FailingShutdown,
                2222,
                &info()
            )
            .is_err()
        );
        // Registered before the failing wait, so the record survives for status/stop.
        assert_eq!(store.load().unwrap().map(|record| record.pid), Some(2222));
    }

    #[test]
    fn propagates_load_error() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
        let mut buf = Vec::new();
        assert!(
            serve(
                &mut buf,
                &store,
                &FixedProbe(true),
                &ImmediateShutdown,
                2222,
                &info()
            )
            .is_err()
        );
    }
}
