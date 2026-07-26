//! The `usagi daemon restart` usecase: stop any running daemon, then start a
//! fresh one.
//!
//! It composes the two existing control-plane usecases: [`stop`](crate::usecase::stop)
//! asks a running daemon to stop and waits for its endpoint retirement / exact
//! record clear (or lock-fenced endpoint and record recovery when initially stale), then
//! [`launch_and_confirm`](crate::usecase::start::launch_and_confirm) spawns
//! a detached `serve` and waits for it to register. The store, probe, terminator,
//! launcher, and sleeper are injected, so this stays pure and fully testable.
//!
//! Record cleanup is incarnation-conditional, so a delayed stop or old owner
//! cannot remove the freshly registered restart record even when the OS reuses
//! the same pid.

use std::io;

use usagi_core::domain::AppInfo;
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonRecordStore, LivenessProbe, RecordFile, Sleeper, Terminator,
};

use crate::usecase::stop::StaleDaemonCleanup;
use crate::usecase::{start, stop};

/// Stop any running or stale daemon, then launch and confirm a fresh one.
///
/// # Errors
///
/// Returns the stop phase's error (load / terminate / clear) or the start
/// phase's error (spawn / load / registration timeout).
pub fn restart<F: RecordFile, P: LivenessProbe, T: Terminator, L: DaemonLauncher, K: Sleeper>(
    store: &DaemonRecordStore<F>,
    probe: &P,
    terminator: &T,
    launcher: &L,
    sleeper: &K,
    stale_cleanup: &dyn StaleDaemonCleanup,
    info: &AppInfo,
) -> io::Result<String> {
    // Bring down whatever is there (running → signal + owner cleanup wait,
    // stale → endpoint cleanup + exact-record clear, absent → nothing); its report line is not
    // surfaced by restart.
    stop::stop(store, probe, terminator, sleeper, stale_cleanup, info)?;
    let pid = start::launch_and_confirm(store, probe, launcher, sleeper)?;
    Ok(format!("{}: daemon restarted (pid {pid})", info.describe()))
}

#[cfg(test)]
mod tests {
    use super::restart;
    use crate::test_support::{
        FixedProbe, InMemoryRecordFile, NoopReady, NoopSleeper, ObservedAs, RecordingTerminator,
        TestLauncher,
    };
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
    use usagi_core::infrastructure::daemon::{DaemonRecordStore, LivenessProbe, Sleeper};

    struct OwnerCleanupSleeper<'a> {
        store: &'a DaemonRecordStore<InMemoryRecordFile>,
        expected: &'a DaemonRecord,
    }

    impl Sleeper for OwnerCleanupSleeper<'_> {
        fn sleep(&self) {
            assert!(self.store.clear_if(self.expected).unwrap());
        }
    }

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    #[test]
    fn stops_the_running_daemon_then_starts_a_fresh_one() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let old = DaemonRecord::new(1111);
        store.save(&old).unwrap();
        let terminator = RecordingTerminator::default();
        let launcher = TestLauncher::registering(&store, 5555);
        let sleeper = OwnerCleanupSleeper {
            store: &store,
            expected: &old,
        };
        assert_eq!(
            restart(
                &store,
                &FixedProbe(true),
                &terminator,
                &launcher,
                &sleeper,
                &NoopReady,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon restarted (pid 5555)"
        );
        // The old daemon was signalled and the new one is now recorded.
        assert_eq!(terminator.terminated(), vec![1111]);
        assert_eq!(store.load().unwrap().map(|record| record.pid), Some(5555));
    }

    #[test]
    fn starts_a_daemon_when_none_was_running() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let terminator = RecordingTerminator::default();
        let launcher = TestLauncher::registering(&store, 5555);
        assert_eq!(
            restart(
                &store,
                &FixedProbe(true),
                &terminator,
                &launcher,
                &NoopSleeper,
                &NoopReady,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon restarted (pid 5555)"
        );
        // Nothing was running, so no termination was attempted.
        assert!(terminator.terminated().is_empty());
    }

    /// Reports the seeded pid as reused by an unrelated process and every other
    /// pid as its exact owner: the stop phase must reclaim without signalling,
    /// and the start phase must then confirm the replacement.
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
    fn reclaims_a_reused_pid_record_then_starts_one_replacement() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let stale = DaemonRecord::identified(1111, "old-incarnation");
        store.save(&stale).unwrap();
        let terminator = RecordingTerminator::default();
        let launcher = TestLauncher::registering(&store, 5555);

        assert_eq!(
            restart(
                &store,
                &ReusedPidProbe(stale.pid),
                &terminator,
                &launcher,
                &NoopSleeper,
                &NoopReady,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon restarted (pid 5555)"
        );
        // The process occupying the reused pid was never signalled, and exactly
        // one replacement was launched.
        assert!(terminator.terminated().is_empty());
        assert_eq!(launcher.launches(), 1);
        assert_eq!(store.load().unwrap().map(|record| record.pid), Some(5555));
    }

    #[test]
    fn refuses_an_unverified_owner_before_launching_anything() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let existing = DaemonRecord::new(1111);
        store.save(&existing).unwrap();
        let terminator = RecordingTerminator::default();
        let launcher = TestLauncher::registering(&store, 5555);

        let error = restart(
            &store,
            &ObservedAs(DaemonProcessObservation::Unknown),
            &terminator,
            &launcher,
            &NoopSleeper,
            &NoopReady,
            &info(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("identity is unverified"));
        assert!(terminator.terminated().is_empty());
        assert_eq!(launcher.launches(), 0);
        assert_eq!(store.load().unwrap(), Some(existing));
    }

    #[test]
    fn propagates_stop_error() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(1111)).unwrap();
        let terminator = RecordingTerminator::failing();
        let launcher = TestLauncher::registering(&store, 5555);
        assert!(
            restart(
                &store,
                &FixedProbe(true),
                &terminator,
                &launcher,
                &NoopSleeper,
                &NoopReady,
                &info()
            )
            .is_err()
        );
    }

    #[test]
    fn propagates_start_timeout() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let terminator = RecordingTerminator::default();
        // An idle launcher registers nothing, so the start phase times out.
        let launcher = TestLauncher::idle(&store);
        assert!(
            restart(
                &store,
                &FixedProbe(true),
                &terminator,
                &launcher,
                &NoopSleeper,
                &NoopReady,
                &info()
            )
            .is_err()
        );
    }
}
