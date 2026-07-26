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
use usagi_core::infrastructure::daemon::{DaemonLauncher, LivenessProbe, Sleeper};

use crate::usecase::serve::DaemonRecordPort;

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
pub fn start(
    store: &dyn DaemonRecordPort,
    probe: &dyn LivenessProbe,
    launcher: &dyn DaemonLauncher,
    sleeper: &dyn Sleeper,
    info: &AppInfo,
) -> io::Result<String> {
    let existing = store.load()?;
    let observation = existing.as_ref().map_or(
        usagi_core::domain::daemon::DaemonProcessObservation::Unknown,
        |record| probe.observe(record),
    );
    let describe = info.describe();

    match classify(existing.as_ref(), observation) {
        DaemonState::Alive => {
            let running = existing
                .expect("classify reports Alive only for a present record")
                .pid;
            return Ok(format!(
                "{describe}: daemon already running (pid {running})"
            ));
        }
        DaemonState::Unverified => {
            return Err(io::Error::other(
                "daemon owner identity is unverified; refusing to start a replacement",
            ));
        }
        // Both stale reasons prove the recorded owner is gone, so a replacement
        // is safe: the launched `serve` still has to win the singleton lock and
        // reclaim the leftover endpoint before it registers.
        DaemonState::Stale(_) | DaemonState::Absent => {}
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
pub(crate) fn launch_and_confirm(
    store: &dyn DaemonRecordPort,
    probe: &dyn LivenessProbe,
    launcher: &dyn DaemonLauncher,
    sleeper: &dyn Sleeper,
) -> io::Result<u32> {
    launcher.launch()?;

    for _ in 0..MAX_POLLS {
        if let Some(record) = store.load()?
            && probe.observe(&record) == usagi_core::domain::daemon::DaemonProcessObservation::Exact
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
    use crate::test_support::{
        FixedProbe, InMemoryRecordFile, NoopSleeper, ObservedAs, TestLauncher,
    };
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
    use usagi_core::infrastructure::daemon::{DaemonRecordStore, LivenessProbe};

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    /// Reports the seeded pid as reused by an unrelated process and every other
    /// pid as its exact owner, so `start` first sees a reclaimable record and
    /// then confirms the replacement it launched.
    struct ReusedPidProbe(u32);

    impl LivenessProbe for ReusedPidProbe {
        fn observe(&self, record: &DaemonRecord) -> DaemonProcessObservation {
            if record.pid == self.0 {
                DaemonProcessObservation::IdentityMismatch
            } else {
                DaemonProcessObservation::Exact
            }
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
    fn refuses_to_replace_an_unverified_record() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let existing = DaemonRecord::new(1111);
        store.save(&existing).unwrap();
        let launcher = TestLauncher::registering(&store, 5555);
        let error = start(
            &store,
            &ObservedAs(DaemonProcessObservation::Unknown),
            &launcher,
            &NoopSleeper,
            &info(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("identity is unverified"));
        assert_eq!(launcher.launches(), 0);
        assert_eq!(store.load().unwrap(), Some(existing));
    }

    #[test]
    fn starts_one_replacement_for_a_record_whose_pid_was_reused() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let stale = DaemonRecord::identified(1111, "old-incarnation");
        store.save(&stale).unwrap();
        let launcher = TestLauncher::registering(&store, 5555);

        assert_eq!(
            start(
                &store,
                &ReusedPidProbe(stale.pid),
                &launcher,
                &NoopSleeper,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon started (pid 5555)"
        );
        // Exactly one detached daemon, and the confirmed record is a different
        // incarnation than the reclaimed one.
        assert_eq!(launcher.launches(), 1);
        let registered = store.load().unwrap().unwrap();
        assert_eq!(registered.pid, 5555);
        assert_ne!(
            registered.process_start_identity,
            stale.process_start_identity
        );
    }

    #[test]
    fn propagates_load_error() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
        let launcher = TestLauncher::idle(&store);
        assert!(start(&store, &FixedProbe(true), &launcher, &NoopSleeper, &info()).is_err());
    }
}
