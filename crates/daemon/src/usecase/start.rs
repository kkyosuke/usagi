//! The `usagi daemon start` usecase: launch the daemon in the background.
//!
//! Unlike [`serve`](crate::usecase::serve), which *is* the daemon and runs in
//! the foreground, `start` spawns a detached `serve` and returns once it has
//! registered:
//!
//! 1. **single-instance guard** — if a live daemon already holds the record,
//!    refuse rather than launch a second one; stale or unverified records enter
//!    signal-free, singleton-lock-fenced recovery;
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
use crate::usecase::stop::{StaleCleanup, StaleDaemonCleanup};

/// How many times to poll for the launched daemon's record before giving up.
/// At the synthesis root's ~50ms sleep this is a ~30s window.
///
/// A cold start does more than bind a socket: it recovers the generation
/// registry, hydrates runtime state, and adopts the workspaces it is asked to
/// serve. The window was ~2s, which a loaded host exceeds routinely — one
/// observed start registered ~11s after `start` had already reported a failure.
/// That outcome is the worst of both: the operator is told the daemon did not
/// start while a healthy daemon is coming up behind the message, so the obvious
/// next step (start it again) then refuses because one is already running.
/// The window is sized for the slow-but-healthy case; a daemon that actually
/// failed still reports its own reason through [`startup_timeout_message`].
pub(crate) const MAX_POLLS: usize = 600;

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
    stale_cleanup: &dyn StaleDaemonCleanup,
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
        // Process identity is signal authority, not reclaim authority.  Stale
        // and unverified records both enter the same signal-free transaction:
        // production acquires daemon.lock, rechecks the complete record, and
        // retires its endpoint before clearing it. A live owner keeps the lock,
        // so an undecidable PID never authorizes either a signal or cleanup.
        DaemonState::Stale(_) | DaemonState::Unverified => {
            let record = existing
                .as_ref()
                .expect("classify reports a present non-absent state only for a record");
            match stale_cleanup.cleanup_if(store, record)? {
                StaleCleanup::Cleared => {}
                StaleCleanup::Superseded => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "daemon ownership changed during startup recovery",
                    ));
                }
            }
        }
        DaemonState::Absent => {}
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
    // Read before launching: an entry already present belongs to an earlier
    // failure, and reporting it as this launch's cause would send the operator
    // after the wrong thing.
    let before = launcher.recorded_failure();
    launcher.launch()?;

    let outcome = confirm_launch(store, probe, launcher, sleeper, before.as_deref());
    match outcome {
        Ok(pid) => Ok(pid),
        Err(error) => {
            let message = error.to_string();
            launcher.abort_launch().map_err(|abort| {
                io::Error::other(format!(
                    "{message}; the launched daemon could not be stopped: {abort}"
                ))
            })?;
            Err(io::Error::other(format!(
                "{message}; the launched daemon was stopped"
            )))
        }
    }
}

fn confirm_launch(
    store: &dyn DaemonRecordPort,
    probe: &dyn LivenessProbe,
    launcher: &dyn DaemonLauncher,
    sleeper: &dyn Sleeper,
    before: Option<&str>,
) -> io::Result<u32> {
    for _ in 0..MAX_POLLS {
        if let Some(record) = store.load()?
            && probe.observe(&record) == usagi_core::domain::daemon::DaemonProcessObservation::Exact
        {
            return Ok(record.pid);
        }
        if let Some(status) = launcher.launched_exit()? {
            return Err(io::Error::other(format!(
                "daemon exited before registering ({status})"
            )));
        }
        sleeper.sleep();
    }

    // One last observation closes the gap after the final sleep. If the child
    // is still not authoritative, stop and reap exactly the child this launch
    // created before reporting failure; otherwise it could register after the
    // operator has already been told startup failed.
    if let Some(record) = store.load()?
        && probe.observe(&record) == usagi_core::domain::daemon::DaemonProcessObservation::Exact
    {
        return Ok(record.pid);
    }
    Err(io::Error::other(startup_timeout_message(
        before,
        launcher.recorded_failure().as_deref(),
        launcher.failure_log_hint().as_deref(),
    )))
}

/// What a start that never registered tells the operator.
///
/// The deadline itself explains nothing: every distinct cause — a socket path
/// over the platform limit, a data directory with the wrong mode, a workspace
/// another daemon owns — produces the same silence at this end. When the daemon
/// recorded a reason of its own during this launch, that reason is the message;
/// otherwise the operator is at least pointed at the log.
fn startup_timeout_message(
    before: Option<&str>,
    after: Option<&str>,
    log_hint: Option<&str>,
) -> String {
    let deadline = "daemon did not register within the startup window";
    match (after, log_hint) {
        (Some(reported), _) if after != before => {
            format!("{deadline}; the daemon reported: {reported}")
        }
        (_, Some(hint)) => format!("{deadline}; no reason was recorded, see {hint}"),
        (_, None) => deadline.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_POLLS, launch_and_confirm, start, startup_timeout_message};
    use std::cell::Cell;

    use crate::test_support::{
        FixedProbe, InMemoryRecordFile, NoopReady, NoopSleeper, TestLauncher,
    };
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
    use usagi_core::infrastructure::daemon::{
        DaemonLauncher, DaemonRecordStore, LivenessProbe, Sleeper,
    };

    use crate::usecase::serve::DaemonRecordPort;
    use crate::usecase::stop::{StaleCleanup, StaleDaemonCleanup};

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    /// Registers the daemon after a configured number of confirmation sleeps,
    /// matching a cold start that has not written `daemon.json` yet.
    struct DelayedRegistration<'a> {
        store: &'a dyn DaemonRecordPort,
        sleeps: Cell<usize>,
        register_after: usize,
        pid: u32,
    }

    impl Sleeper for DelayedRegistration<'_> {
        fn sleep(&self) {
            let sleeps = self.sleeps.get() + 1;
            self.sleeps.set(sleeps);
            if sleeps == self.register_after {
                self.store.save(&DaemonRecord::new(self.pid)).unwrap();
            }
        }
    }

    struct CountingSleeper(Cell<usize>);

    impl Sleeper for CountingSleeper {
        fn sleep(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// A cold start slower than the old ~2s window is confirmed, not reported as
    /// a timeout.
    ///
    /// Reporting it was the worst outcome available: the operator was told the
    /// daemon had not started while a healthy one came up behind the message,
    /// and the obvious retry then refused because one was already running.
    #[test]
    fn a_slow_cold_start_is_confirmed_rather_than_reported_as_a_timeout() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let launcher = TestLauncher::idle(&store);
        // Past the old limit of 40 polls, and past the ~11s that was actually
        // observed at the production sleep of ~50ms.
        let sleeper = DelayedRegistration {
            store: &store,
            sleeps: Cell::new(0),
            register_after: 300,
            pid: 4242,
        };

        assert_eq!(
            launch_and_confirm(&store, &FixedProbe(true), &launcher, &sleeper).unwrap(),
            4242
        );
        assert!(
            sleeper.sleeps.get() > 40,
            "the old window would have given up"
        );
    }

    #[test]
    fn a_daemon_that_never_registers_still_times_out() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let launcher = TestLauncher::idle(&store);
        let sleeper = CountingSleeper(Cell::new(0));

        let error = launch_and_confirm(&store, &FixedProbe(true), &launcher, &sleeper)
            .expect_err("a daemon without a record must time out");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(sleeper.0.get(), MAX_POLLS);
        assert_eq!(launcher.aborts(), 1);
        assert!(error.to_string().contains("launched daemon was stopped"));
    }

    #[test]
    fn registration_on_the_final_observation_is_still_confirmed() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let launcher = TestLauncher::idle(&store);
        let sleeper = DelayedRegistration {
            store: &store,
            sleeps: Cell::new(0),
            register_after: MAX_POLLS,
            pid: 4242,
        };

        assert_eq!(
            launch_and_confirm(&store, &FixedProbe(true), &launcher, &sleeper).unwrap(),
            4242
        );
        assert_eq!(sleeper.sleeps.get(), MAX_POLLS);
        assert_eq!(launcher.aborts(), 0);
    }

    struct AbortErrorLauncher;

    impl DaemonLauncher for AbortErrorLauncher {
        fn launch(&self) -> std::io::Result<()> {
            Ok(())
        }

        fn abort_launch(&self) -> std::io::Result<()> {
            Err(std::io::Error::other("kill failed"))
        }
    }

    #[test]
    fn an_abort_failure_preserves_both_startup_and_cleanup_context() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());

        let error =
            launch_and_confirm(&store, &FixedProbe(true), &AbortErrorLauncher, &NoopSleeper)
                .expect_err("a failed cleanup must not hide the startup timeout");

        assert_eq!(
            error.to_string(),
            "daemon did not register within the startup window; the launched daemon could not be stopped: kill failed"
        );
    }

    struct ExitedLauncher;

    impl DaemonLauncher for ExitedLauncher {
        fn launch(&self) -> std::io::Result<()> {
            Ok(())
        }

        fn launched_exit(&self) -> std::io::Result<Option<String>> {
            Ok(Some("exit status: 78".into()))
        }

        fn abort_launch(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_daemon_that_exits_before_registration_reports_its_status_immediately() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let sleeper = CountingSleeper(Cell::new(0));

        let error = launch_and_confirm(&store, &FixedProbe(true), &ExitedLauncher, &sleeper)
            .expect_err("an exited daemon cannot register later");

        assert_eq!(sleeper.0.get(), 0);
        assert_eq!(
            error.to_string(),
            "daemon exited before registering (exit status: 78); the launched daemon was stopped"
        );
    }

    struct StatusErrorLauncher(Cell<usize>);

    impl DaemonLauncher for StatusErrorLauncher {
        fn launch(&self) -> std::io::Result<()> {
            Ok(())
        }

        fn launched_exit(&self) -> std::io::Result<Option<String>> {
            Err(std::io::Error::other("child status unavailable"))
        }

        fn abort_launch(&self) -> std::io::Result<()> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }
    }

    #[test]
    fn a_confirmation_error_still_aborts_the_launched_daemon() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let launcher = StatusErrorLauncher(Cell::new(0));

        let error = launch_and_confirm(&store, &FixedProbe(true), &launcher, &NoopSleeper)
            .expect_err("an unobservable launch cannot be left running");

        assert_eq!(launcher.0.get(), 1);
        assert!(error.to_string().contains("child status unavailable"));
        assert!(error.to_string().contains("launched daemon was stopped"));
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

    /// Treats only the pre-migration record as ownership-unknown. The daemon
    /// registered by the launcher represents a current identified owner.
    struct LegacyPidProbe(u32);

    impl LivenessProbe for LegacyPidProbe {
        fn observe(&self, record: &DaemonRecord) -> DaemonProcessObservation {
            if record.pid == self.0 {
                DaemonProcessObservation::Unknown
            } else {
                DaemonProcessObservation::Exact
            }
        }
    }

    struct BusyCleanup;

    impl StaleDaemonCleanup for BusyCleanup {
        fn cleanup_if(
            &self,
            _store: &dyn DaemonRecordPort,
            _expected: &DaemonRecord,
        ) -> std::io::Result<StaleCleanup> {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "daemon singleton lock is held",
            ))
        }
    }

    struct SupersededCleanup;

    impl StaleDaemonCleanup for SupersededCleanup {
        fn cleanup_if(
            &self,
            _store: &dyn DaemonRecordPort,
            _expected: &DaemonRecord,
        ) -> std::io::Result<StaleCleanup> {
            Ok(StaleCleanup::Superseded)
        }
    }

    #[test]
    fn launches_and_reports_the_registered_pid() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        // The launcher mimics the spawned serve registering pid 5555.
        let launcher = TestLauncher::registering(&store, 5555);
        assert_eq!(
            start(
                &store,
                &FixedProbe(true),
                &launcher,
                &NoopSleeper,
                &NoopReady,
                &info(),
            )
            .unwrap(),
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
            start(
                &store,
                &FixedProbe(true),
                &launcher,
                &NoopSleeper,
                &NoopReady,
                &info(),
            )
            .unwrap(),
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
        assert!(
            start(
                &store,
                &FixedProbe(true),
                &launcher,
                &NoopSleeper,
                &NoopReady,
                &info(),
            )
            .is_err()
        );
    }

    #[test]
    fn reclaims_an_unverified_record_without_signalling_then_starts() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let existing = DaemonRecord::new(1111);
        store.save(&existing).unwrap();
        let launcher = TestLauncher::registering(&store, 5555);
        assert_eq!(
            start(
                &store,
                &LegacyPidProbe(existing.pid),
                &launcher,
                &NoopSleeper,
                &NoopReady,
                &info(),
            )
            .unwrap(),
            "usagi v0.1.0: daemon started (pid 5555)"
        );
        assert_eq!(launcher.launches(), 1);
        assert_ne!(store.load().unwrap(), Some(existing));
    }

    #[test]
    fn unverified_recovery_refusal_preserves_record_and_never_launches() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let existing = DaemonRecord::new(1111);
        store.save(&existing).unwrap();
        let launcher = TestLauncher::registering(&store, 5555);

        let error = start(
            &store,
            &LegacyPidProbe(existing.pid),
            &launcher,
            &NoopSleeper,
            &BusyCleanup,
            &info(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(launcher.launches(), 0);
        assert_eq!(store.load().unwrap(), Some(existing));
    }

    #[test]
    fn ownership_change_during_recovery_is_busy_and_never_launches() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let existing = DaemonRecord::new(1111);
        store.save(&existing).unwrap();
        let launcher = TestLauncher::registering(&store, 5555);

        let error = start(
            &store,
            &LegacyPidProbe(existing.pid),
            &launcher,
            &NoopSleeper,
            &SupersededCleanup,
            &info(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("ownership changed"));
        assert_eq!(launcher.launches(), 0);
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
                &NoopReady,
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
        assert!(
            start(
                &store,
                &FixedProbe(true),
                &launcher,
                &NoopSleeper,
                &NoopReady,
                &info(),
            )
            .is_err()
        );
    }

    /// The deadline alone explains nothing: every distinct cause looks the same
    /// from this end, because the launched daemon's stderr goes to `/dev/null`.
    #[test]
    fn a_startup_timeout_reports_what_the_daemon_recorded_for_itself() {
        let reported = startup_timeout_message(
            None,
            Some("path must be shorter than SUN_LEN"),
            Some("/home/u/.usagi/logs"),
        );
        assert!(reported.contains("did not register within the startup window"));
        assert!(
            reported.contains("path must be shorter than SUN_LEN"),
            "{reported}"
        );
    }

    /// An entry that was already there belongs to an earlier failure. Reporting
    /// it would send the operator after the wrong cause, so an unchanged log is
    /// treated as "nothing recorded" and only points at the log itself.
    #[test]
    fn a_startup_timeout_does_not_blame_a_failure_that_predates_the_launch() {
        let stale = Some("a failure from yesterday");
        let reported = startup_timeout_message(stale, stale, Some("/home/u/.usagi/logs"));
        assert!(!reported.contains("yesterday"), "{reported}");
        assert!(reported.contains("no reason was recorded"), "{reported}");
        assert!(reported.contains("/home/u/.usagi/logs"), "{reported}");
    }

    /// With no log to point at — every test launcher, and any environment
    /// without a data directory — the message stays exactly what it was.
    #[test]
    fn a_startup_timeout_without_a_log_reports_only_the_deadline() {
        assert_eq!(
            startup_timeout_message(None, None, None),
            "daemon did not register within the startup window"
        );
    }
}
