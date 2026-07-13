//! The `usagi daemon start` usecase: launch the daemon in the background.
//!
//! Unlike [`serve`](crate::usecase::serve), which *is* the daemon and runs in
//! the foreground, `start` spawns a detached `serve` and returns once it has
//! registered:
//!
//! 1. **single-instance guard** — if a live daemon already holds the record,
//!    refuse rather than launch a second one;
//! 2. **launch** — spawn a detached `serve` process;
//! 3. **confirm** — poll `daemon.json` until the launched process registers a
//!    live record, then report its pid; time out if it never does.
//!
//! The spawned `serve` writes its own pid, so `start` learns the pid by reading
//! the record. The store, probe, launcher, and sleeper are injected, so this
//! stays pure and fully testable; the synthesis root binds the real spawn and
//! sleep.

use std::io;

use usagi_core::domain::AppInfo;
use usagi_core::domain::daemon::{DaemonState, classify};
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonRecordStore, LivenessProbe, RecordFile, Sleeper,
};

/// How many times to poll for the launched daemon's record before giving up.
/// At the synthesis root's ~50ms sleep this is a ~2s window.
pub(crate) const MAX_POLLS: usize = 40;

/// Launch a background daemon and report the outcome.
///
/// # Errors
///
/// Returns the store's load error, the launcher's spawn error, or a timeout
/// error when the launched daemon does not register within [`MAX_POLLS`] polls.
///
/// # Panics
///
/// Never in practice: the guard unwraps the record only after `classify`
/// reports `Alive`, which happens only when a record is present.
#[coverage(off)]
pub fn start<F: RecordFile, P: LivenessProbe, L: DaemonLauncher, K: Sleeper>(
    store: &DaemonRecordStore<F>,
    probe: &P,
    launcher: &L,
    sleeper: &K,
    info: &AppInfo,
) -> io::Result<String> {
    let existing = store.load()?;
    let alive = existing
        .as_ref()
        .is_some_and(|record| probe.is_alive(record.pid));
    let describe = info.describe();

    if matches!(classify(existing.as_ref(), alive), DaemonState::Alive) {
        let running = existing
            .expect("classify reports Alive only for a present record")
            .pid;
        return Ok(format!(
            "{describe}: daemon already running (pid {running})"
        ));
    }

    let pid = launch_and_confirm(store, probe, launcher, sleeper)?;
    Ok(format!("{describe}: daemon started (pid {pid})"))
}

/// Spawn a detached daemon and poll `daemon.json` until it registers a live
/// record, returning its pid. Shared by [`start`] and
/// [`restart`](crate::usecase::restart::restart), which differ only in the
/// guard and reporting around it.
///
/// # Errors
///
/// Returns the launcher's spawn error, the store's load error, or a timeout
/// error when the launched daemon does not register within [`MAX_POLLS`] polls.
#[coverage(off)]
pub(crate) fn launch_and_confirm<F: RecordFile, P: LivenessProbe, L: DaemonLauncher, K: Sleeper>(
    store: &DaemonRecordStore<F>,
    probe: &P,
    launcher: &L,
    sleeper: &K,
) -> io::Result<u32> {
    launcher.launch()?;

    for _ in 0..MAX_POLLS {
        if let Some(record) = store.load()?
            && probe.is_alive(record.pid)
        {
            return Ok(record.pid);
        }
        sleeper.sleep();
    }

    Err(io::Error::other(
        "daemon did not register within the startup window",
    ))
}

#[cfg(test)]
mod tests {
    use super::start;
    use crate::test_support::{FixedProbe, InMemoryRecordFile, NoopSleeper, TestLauncher};
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
    fn launches_and_reports_the_registered_pid() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        // The launcher mimics the spawned serve registering pid 5555.
        let launcher = TestLauncher::registering(&store, 5555);
        assert_eq!(
            start(&store, &FixedProbe(true), &launcher, &NoopSleeper, &info()).unwrap(),
            "usagi v0.1.0: daemon started (pid 5555)"
        );
    }

    #[test]
    fn refuses_when_a_live_daemon_already_runs() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let existing = DaemonRecord::new(1111);
        store.save(&existing).unwrap();
        // A launcher that would register 5555 if wrongly called.
        let launcher = TestLauncher::registering(&store, 5555);
        assert_eq!(
            start(&store, &FixedProbe(true), &launcher, &NoopSleeper, &info()).unwrap(),
            "usagi v0.1.0: daemon already running (pid 1111)"
        );
        // The launcher was not invoked — the record is untouched.
        assert_eq!(store.load().unwrap(), Some(existing));
    }

    #[test]
    fn times_out_when_the_daemon_never_registers() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        // An idle launcher spawns nothing, so no record ever appears.
        let launcher = TestLauncher::idle(&store);
        assert!(start(&store, &FixedProbe(true), &launcher, &NoopSleeper, &info()).is_err());
    }

    #[test]
    fn propagates_load_error() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
        let launcher = TestLauncher::idle(&store);
        assert!(start(&store, &FixedProbe(true), &launcher, &NoopSleeper, &info()).is_err());
    }
}
