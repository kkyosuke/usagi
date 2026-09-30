//! The `usagi daemon status` usecase: report the daemon's lifecycle state.
//!
//! Composes the daemon record store (loading `daemon.json`), the process identity probe
//! (does the recorded PID still have the exact process-start identity?), the
//! [`EndpointObservation`] of whether that process still answers, and the domain
//! [`classify`](usagi_core::domain::daemon::classify) decision into a single
//! human-readable line. Every seam is injected, so this stays pure and fully
//! testable; the synthesis root binds the real filesystem, process probe, and
//! endpoint probe.
//!
//! The endpoint is a separate question from the process, and this report is the
//! one place an operator goes to ask it: `usagi update` sends them here by name
//! when its own synchronization cannot reach the daemon.

use std::io;

use usagi_core::domain::AppInfo;
use usagi_core::domain::daemon::{DaemonState, StaleReason, classify};
use usagi_core::infrastructure::daemon::LivenessProbe;

use crate::usecase::build_report::{self, BuildObservation};
use crate::usecase::endpoint::EndpointObservation;
use crate::usecase::serve::DaemonRecordPort;

/// What an operator must run to replace a daemon that answers nothing.
///
/// A planned replacement refuses it
/// ([`SeamlessRefusal::ActiveUnreachable`](crate::usecase::replacement::SeamlessRefusal::ActiveUnreachable)),
/// so the report and the refusal name the same command.
const UNREACHABLE_REMEDY: &str = "it cannot serve requests; replace it with `usagi daemon restart --force`, which gives up whatever runtime it still holds";

/// Build the `status` report line: load the record, probe whether its process is
/// alive, and classify the two into running / stale / unverified / not-running.
///
/// Both stale reasons are reported as reclaimable, but they are named apart: an
/// owner that simply vanished and an owner whose PID has been handed to an
/// unrelated process are different events, and only the second explains why an
/// unrelated live process holds the recorded PID.
///
/// A live owner is reported as running only while `endpoint` does not prove the
/// opposite. "Running" is a claim about serving, and a process that answers
/// nothing is not serving — reporting it as running once sent an operator
/// looking for the fault everywhere except at the daemon. An unprobed endpoint
/// makes no claim, so the line is the one this report always gave.
///
/// # Errors
///
/// Returns the store's load error — a read failure or a malformed `daemon.json`.
///
/// # Panics
///
/// Never in practice: the arms that name a pid read it from the loaded record,
/// and `classify` reports those states only when a record is present.
pub fn report(
    store: &dyn DaemonRecordPort,
    probe: &dyn LivenessProbe,
    endpoint: EndpointObservation,
    info: &AppInfo,
) -> io::Result<String> {
    report_observed(store, probe, endpoint, None, info)
}

/// [`report`], naming the build the daemon's handshake advertised.
///
/// The prefix names the binary that ran this report, so without this clause a
/// freshly updated client reports its own new version beside a daemon that is
/// still the old one. The clause is added only to a daemon reported as running:
/// a build observed from an owner that has since gone stale describes nothing
/// that is still there.
///
/// # Errors
///
/// Returns the store's load error, as [`report`] does.
///
/// # Panics
///
/// As [`report`].
pub fn report_observed(
    store: &dyn DaemonRecordPort,
    probe: &dyn LivenessProbe,
    endpoint: EndpointObservation,
    build: Option<&BuildObservation>,
    info: &AppInfo,
) -> io::Result<String> {
    let record = store.load()?;
    let observation = record.as_ref().map_or(
        usagi_core::domain::daemon::DaemonProcessObservation::Unknown,
        |record| probe.observe(record),
    );
    let describe = info.describe();
    let recorded_pid = record.as_ref().map(|record| record.pid);
    let pid = || recorded_pid.expect("classify names a pid only for a present record");
    Ok(match classify(record.as_ref(), observation) {
        DaemonState::Alive if endpoint.is_silent() => format!(
            "{describe}: daemon running but not answering (pid {}); {UNREACHABLE_REMEDY}",
            pid()
        ),
        DaemonState::Alive => format!(
            "{describe}: daemon running (pid {}){}",
            pid(),
            build_report::clause(build)
        ),
        DaemonState::Stale(StaleReason::OwnerGone) => format!(
            "{describe}: daemon not running (stale record, pid {} is gone; reclaimable)",
            pid()
        ),
        DaemonState::Stale(StaleReason::PidReused) => format!(
            "{describe}: daemon not running (stale record, pid {} was reused by another process; reclaimable)",
            pid()
        ),
        DaemonState::Unverified => {
            format!("{describe}: daemon state unverified (record retained)")
        }
        DaemonState::Absent => format!("{describe}: daemon not running"),
    })
}

#[cfg(test)]
mod tests {
    use super::{BuildObservation, EndpointObservation, report, report_observed};
    use crate::test_support::{FixedProbe, InMemoryRecordFile, ObservedAs};
    use usagi_core::domain::AppInfo;
    use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
    use usagi_core::infrastructure::daemon::DaemonRecordStore;
    use usagi_core::infrastructure::ipc::BuildIdentity;

    fn info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    #[test]
    fn reports_not_running_when_no_record() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert_eq!(
            report(
                &store,
                &FixedProbe(false),
                EndpointObservation::NotObserved,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon not running"
        );
    }

    #[test]
    fn reports_running_with_pid_when_record_and_process_alive() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        assert_eq!(
            report(
                &store,
                &FixedProbe(true),
                EndpointObservation::NotObserved,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon running (pid 4321)"
        );
    }

    fn observation(daemon: &str, client: &str) -> BuildObservation {
        let build = |version: &str| BuildIdentity {
            version: version.to_owned(),
            commit: format!("{version}commit"),
            target: "test".to_owned(),
            artifact: String::new(),
        };
        BuildObservation {
            daemon: build(daemon),
            client: build(client),
        }
    }

    /// The prefix is this client's version. After an update, a daemon that is
    /// still the old build must not read as updated.
    #[test]
    fn names_the_running_daemon_build_and_a_mismatch_with_this_client() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        assert_eq!(
            report_observed(
                &store,
                &FixedProbe(true),
                EndpointObservation::Answering,
                Some(&observation("4.8.5", "4.8.8")),
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon running (pid 4321); daemon build v4.8.5 (4.8.5co) differs from this client v4.8.8 (4.8.8co)"
        );
    }

    /// A build observed from an owner that is no longer running describes
    /// nothing that is still there, so only a running daemon carries it.
    #[test]
    fn a_stale_record_does_not_carry_an_observed_build() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        let line = report_observed(
            &store,
            &FixedProbe(false),
            EndpointObservation::NotObserved,
            Some(&observation("4.8.8", "4.8.8")),
            &info(),
        )
        .unwrap();
        assert!(!line.contains("daemon build"), "{line}");
    }

    /// `usagi update` names this command when its own synchronization cannot
    /// reach the daemon. Reporting a silent owner as plainly "running" answered
    /// that question wrongly, and left the operator with nothing to act on.
    #[test]
    fn names_a_live_owner_that_answers_nothing_apart_from_a_serving_one() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        let line = report(
            &store,
            &FixedProbe(true),
            EndpointObservation::Silent,
            &info(),
        )
        .unwrap();
        assert!(
            line.contains("daemon running but not answering (pid 4321)"),
            "{line}"
        );
        assert!(line.contains("usagi daemon restart --force"), "{line}");
        // An endpoint that answered is the ordinary running report, and so is one
        // nobody probed.
        assert_eq!(
            report(
                &store,
                &FixedProbe(true),
                EndpointObservation::Answering,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon running (pid 4321)"
        );
        assert_eq!(
            report(
                &store,
                &FixedProbe(true),
                EndpointObservation::NotObserved,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon running (pid 4321)"
        );
    }

    /// Silence is only ever read against a live owner: a record whose process is
    /// gone is stale, and saying "not answering" there would hide the reclaim.
    #[test]
    fn a_silent_endpoint_does_not_disturb_a_record_whose_owner_is_gone() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        assert_eq!(
            report(
                &store,
                &FixedProbe(false),
                EndpointObservation::Silent,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon not running (stale record, pid 4321 is gone; reclaimable)"
        );
        let empty = DaemonRecordStore::new(InMemoryRecordFile::default());
        assert_eq!(
            report(
                &empty,
                &FixedProbe(true),
                EndpointObservation::Silent,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon not running"
        );
    }

    #[test]
    fn reports_stale_when_record_but_process_gone() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        assert_eq!(
            report(
                &store,
                &FixedProbe(false),
                EndpointObservation::NotObserved,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon not running (stale record, pid 4321 is gone; reclaimable)"
        );
    }

    #[test]
    fn names_a_reused_pid_apart_from_a_vanished_owner_and_keeps_both_reclaimable() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store
            .save(&DaemonRecord::identified(4321, "old-incarnation"))
            .unwrap();
        // Both lines say "reclaimable", because both observations prove the
        // recorded owner is gone. Only this one explains why an unrelated live
        // process answers for pid 4321.
        assert_eq!(
            report(
                &store,
                &ObservedAs(DaemonProcessObservation::IdentityMismatch),
                EndpointObservation::NotObserved,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon not running (stale record, pid 4321 was reused by another process; reclaimable)"
        );
    }

    #[test]
    fn reports_unverified_and_retains_record_when_identity_is_unknown() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        let record = DaemonRecord::new(4321);
        store.save(&record).unwrap();
        assert_eq!(
            report(
                &store,
                &ObservedAs(DaemonProcessObservation::Unknown),
                EndpointObservation::NotObserved,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon state unverified (record retained)"
        );
        assert_eq!(store.load().unwrap(), Some(record));
    }

    #[test]
    fn reports_not_running_after_record_cleared() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::default());
        store.save(&DaemonRecord::new(4321)).unwrap();
        let record = store.load().unwrap().unwrap();
        assert!(store.clear_if(&record).unwrap());
        assert_eq!(
            report(
                &store,
                &FixedProbe(true),
                EndpointObservation::NotObserved,
                &info()
            )
            .unwrap(),
            "usagi v0.1.0: daemon not running"
        );
    }

    #[test]
    fn propagates_malformed_record_as_error() {
        let store = DaemonRecordStore::new(InMemoryRecordFile::with("not json"));
        assert!(
            report(
                &store,
                &FixedProbe(true),
                EndpointObservation::NotObserved,
                &info()
            )
            .is_err()
        );
    }
}
