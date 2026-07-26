use std::cell::Cell;
use std::io;

use usagi_core::domain::AppInfo;
use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::daemon::DaemonRecordStore;
use usagi_core::infrastructure::ipc::{BuildIdentity, OperationId, build_identity};

use super::{
    LiveResources, ReplacementPlan, ResourceCensus, SeamlessRefusal, StopPlan, TransitionMode,
    manual_operation_id, plan_replacement, plan_stop, replace_daemon, seamless_refusal,
    stop_daemon,
};
use crate::test_support::{
    FixedProbe, InMemoryRecordFile, NoopReady, NoopSleeper, RecordingTerminator, TestLauncher,
};
use crate::usecase::authority::registry::{GenerationEntry, RegistryDocument};
use crate::usecase::generation::{GenerationRole, ProcessIdentity};

fn info() -> AppInfo {
    AppInfo {
        name: "usagi",
        version: "0.1.0",
    }
}

fn artifact() -> BuildIdentity {
    build_identity("2.6.0", "fixture", "test-target", "debug", &"a".repeat(64))
}

/// A census answering with fixed counts, and counting how often it was asked.
struct FixedCensus {
    live: LiveResources,
    calls: Cell<usize>,
}

impl FixedCensus {
    const fn of(agents: usize, terminals: usize) -> Self {
        Self {
            live: LiveResources { agents, terminals },
            calls: Cell::new(0),
        }
    }
}

impl ResourceCensus for FixedCensus {
    fn live(&self) -> io::Result<LiveResources> {
        self.calls.set(self.calls.get() + 1);
        Ok(self.live)
    }
}

/// The owner clears its own record once signalled; this emulates that on the
/// first wait so a stop completes without a real daemon.
struct ClearingSleeper<'a> {
    store: &'a DaemonRecordStore<InMemoryRecordFile>,
    expected: &'a DaemonRecord,
}

impl usagi_core::infrastructure::daemon::Sleeper for ClearingSleeper<'_> {
    fn sleep(&self) {
        assert!(self.store.clear_if(self.expected).unwrap());
    }
}

/// Reports one pid as taken over by an unrelated process and every other pid as
/// its exact owner, so a stale record is reclaimed and the replacement is
/// confirmed.
struct ReusedPidProbe(u32);

impl usagi_core::infrastructure::daemon::LivenessProbe for ReusedPidProbe {
    fn observe(&self, record: &DaemonRecord) -> DaemonProcessObservation {
        if record.pid == self.0 {
            DaemonProcessObservation::IdentityMismatch
        } else {
            DaemonProcessObservation::Exact
        }
    }
}

/// A census that cannot read the durable stores at all.
struct FailingCensus;

impl ResourceCensus for FailingCensus {
    fn live(&self) -> io::Result<LiveResources> {
        Err(io::Error::other("runtime store unreadable"))
    }
}

fn entry(role: GenerationRole, verified: bool) -> GenerationEntry {
    let expected_build = artifact();
    GenerationEntry {
        generation: DaemonGeneration::new(),
        role,
        endpoint: "/fixture/endpoint".to_owned(),
        process: ProcessIdentity {
            pid: 4242,
            start_identity: "start-4242".to_owned(),
            process_group: 4242,
        },
        verified_build: verified.then(|| expected_build.clone()),
        expected_build,
        revision: 1,
    }
}

fn document(entries: Vec<GenerationEntry>) -> RegistryDocument {
    RegistryDocument {
        generations: entries,
        ..RegistryDocument::default()
    }
}

#[test]
fn live_resources_sum_both_kinds_and_render_them() {
    let live = LiveResources {
        agents: 2,
        terminals: 3,
    };
    assert_eq!(live.total(), 5);
    assert!(!live.is_empty());
    assert!(LiveResources::default().is_empty());
    assert_eq!(
        live.to_string(),
        "2 Agent runtime(s) and 3 generic terminal(s)"
    );
}

#[test]
fn only_states_that_still_hold_a_pty_master_are_counted_as_live() {
    use crate::usecase::terminal::{TerminalReconcileState, TerminalRuntimeState};

    for owned in [
        TerminalRuntimeState::Reserved,
        TerminalRuntimeState::Running,
    ] {
        assert!(super::owns_pty(owned), "{owned:?} holds the PTY master");
    }
    // An exited, reclaimed, failed, or crash-orphaned record has no owner left
    // to take anything from.
    for released in [
        TerminalRuntimeState::Exited,
        TerminalRuntimeState::Reclaimed,
        TerminalRuntimeState::SpawnFailed,
        TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown),
        TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::OrphanRunning),
    ] {
        assert!(
            !super::owns_pty(released),
            "{released:?} owns no PTY master"
        );
    }

    assert_eq!(
        super::census_of(
            &[
                TerminalRuntimeState::Running,
                TerminalRuntimeState::Exited,
                TerminalRuntimeState::Reserved,
            ],
            &[TerminalRuntimeState::Running],
        ),
        LiveResources {
            agents: 2,
            terminals: 1,
        }
    );
    // Two daemons that have never launched anything are equally idle.
    assert_eq!(super::census_of(&[], &[]), LiveResources::default());
}

#[test]
fn an_absent_registry_has_no_successor_to_hand_authority_to() {
    assert_eq!(
        seamless_refusal(None),
        SeamlessRefusal::NoGenerationRegistry
    );
}

#[test]
fn a_foreign_registry_schema_is_refused_before_it_is_interpreted() {
    let foreign = RegistryDocument {
        schema: "usagi-generations-v99".to_owned(),
        // A standby that would otherwise be admitted must not be read out of a
        // document this build does not understand.
        generations: vec![entry(GenerationRole::Standby, true)],
        ..RegistryDocument::default()
    };
    assert_eq!(
        seamless_refusal(Some(&foreign)),
        SeamlessRefusal::RegistrySchemaUnsupported
    );
}

#[test]
fn only_a_verified_standby_counts_as_a_successor() {
    // Nothing registered, an unverified standby, and a live active generation
    // are all the same answer: there is no successor.
    for entries in [
        vec![],
        vec![entry(GenerationRole::Standby, false)],
        vec![entry(GenerationRole::Draining, true)],
    ] {
        assert_eq!(
            seamless_refusal(Some(&document(entries))),
            SeamlessRefusal::NoVerifiedStandby
        );
    }
    assert_eq!(
        seamless_refusal(Some(&document(vec![entry(GenerationRole::Standby, true)]))),
        SeamlessRefusal::StandbyNotAdmitted
    );
}

#[test]
fn every_refusal_names_the_prerequisite_it_is_missing() {
    for (refusal, expected) in [
        (
            SeamlessRefusal::NoGenerationRegistry,
            "no generation registry exists",
        ),
        (
            SeamlessRefusal::RegistrySchemaUnsupported,
            "not the schema this build writes",
        ),
        (
            SeamlessRefusal::RegistryUnreadable("corrupt".into()),
            "cannot be trusted: corrupt",
        ),
        (
            SeamlessRefusal::NoVerifiedStandby,
            "no verified standby generation",
        ),
        (
            SeamlessRefusal::StandbyNotAdmitted,
            "cannot admit a standby generation",
        ),
    ] {
        assert!(
            refusal.to_string().contains(expected),
            "{refusal:?} does not mention {expected}"
        );
    }
}

#[test]
fn an_idle_daemon_is_replaced_and_stopped_without_being_forced() {
    let idle = LiveResources::default();
    assert_eq!(
        plan_replacement(
            TransitionMode::Planned,
            &SeamlessRefusal::NoGenerationRegistry,
            idle
        ),
        ReplacementPlan::ColdTransition
    );
    assert_eq!(
        plan_stop(TransitionMode::Planned, idle),
        StopPlan::Terminate
    );
}

#[test]
fn live_runtime_refuses_a_planned_transition_and_reports_what_it_saved() {
    let live = LiveResources {
        agents: 1,
        terminals: 2,
    };
    assert_eq!(
        plan_replacement(
            TransitionMode::Planned,
            &SeamlessRefusal::NoVerifiedStandby,
            live
        ),
        ReplacementPlan::Refused {
            seamless: SeamlessRefusal::NoVerifiedStandby,
            live,
        }
    );
    assert_eq!(
        plan_stop(TransitionMode::Planned, live),
        StopPlan::Refused(live)
    );
}

#[test]
fn an_explicit_cold_transition_gives_the_live_runtime_up() {
    let live = LiveResources {
        agents: 1,
        terminals: 0,
    };
    assert_eq!(
        plan_replacement(
            TransitionMode::Cold,
            &SeamlessRefusal::NoVerifiedStandby,
            live
        ),
        ReplacementPlan::ColdTransition
    );
    assert_eq!(plan_stop(TransitionMode::Cold, live), StopPlan::Terminate);
}

#[test]
fn a_manual_restart_is_keyed_like_a_forced_replacement_of_the_running_artifact() {
    let build = artifact();
    let first = manual_operation_id(&build, "local").expect("a known artifact has a key");
    // Both verbs against the same build and channel attribute their transition
    // to one operation; a different channel is a different operation.
    assert_eq!(Some(first.clone()), manual_operation_id(&build, "local"));
    assert_ne!(
        Some(first.clone()),
        manual_operation_id(&build, "production")
    );
    assert!(first.0.starts_with("build-rollover-v1-"));

    // An unknown artifact has no safe key at all.
    let mut unknown = build;
    unknown.artifact.clear();
    assert_eq!(manual_operation_id(&unknown, "local"), None);
    assert_eq!(manual_operation_id(&artifact(), ""), None);
}

/// A daemon that is not running owns nothing, whatever its leftover snapshots
/// say — a crashed owner's records must not make `stop` refuse to stop nothing.
#[test]
fn no_live_owner_is_never_censused_and_never_blocks_a_transition() {
    let store = DaemonRecordStore::new(InMemoryRecordFile::default());
    let terminator = RecordingTerminator::default();
    let census = FixedCensus::of(3, 3);

    assert_eq!(
        stop_daemon(
            &store,
            &FixedProbe(true),
            &terminator,
            &NoopSleeper,
            &NoopReady,
            &census,
            TransitionMode::Planned,
            &info(),
        )
        .unwrap(),
        "usagi v0.1.0: daemon not running"
    );
    assert_eq!(census.calls.get(), 0);

    // The same holds for a record whose owner cannot be proved alive.
    store.save(&DaemonRecord::new(1111)).unwrap();
    let launcher = TestLauncher::registering(&store, 5555);
    assert!(
        replace_daemon(
            &store,
            &ReusedPidProbe(1111),
            &terminator,
            &launcher,
            &NoopSleeper,
            &NoopReady,
            &census,
            &SeamlessRefusal::NoGenerationRegistry,
            TransitionMode::Planned,
            None,
            &info(),
        )
        .is_ok()
    );
    assert_eq!(census.calls.get(), 0);
}

/// A live owner is asked exactly once, and its answer decides the transition.
#[test]
fn a_live_owner_is_censused_once_before_it_is_signalled() {
    let store = DaemonRecordStore::new(InMemoryRecordFile::default());
    let running = DaemonRecord::new(1111);
    store.save(&running).unwrap();
    let terminator = RecordingTerminator::default();
    let census = FixedCensus::of(0, 0);

    assert_eq!(
        stop_daemon(
            &store,
            &FixedProbe(true),
            &terminator,
            &ClearingSleeper {
                store: &store,
                expected: &running,
            },
            &NoopReady,
            &census,
            TransitionMode::Planned,
            &info(),
        )
        .unwrap(),
        "usagi v0.1.0: daemon stopped (pid 1111)"
    );
    assert_eq!(census.calls.get(), 1);
    assert_eq!(terminator.terminated(), vec![1111]);
}

#[test]
fn stopping_a_busy_daemon_is_refused_with_nothing_signalled() {
    let store = DaemonRecordStore::new(InMemoryRecordFile::default());
    let existing = DaemonRecord::new(1111);
    store.save(&existing).unwrap();
    let terminator = RecordingTerminator::default();

    let error = stop_daemon(
        &store,
        &FixedProbe(true),
        &terminator,
        &NoopSleeper,
        &NoopReady,
        &FixedCensus::of(1, 1),
        TransitionMode::Planned,
        &info(),
    )
    .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert!(error.to_string().contains("1 Agent runtime(s)"));
    assert!(error.to_string().contains("--force"));
    // A stop has no seamless alternative, so it never claims one was missing.
    assert!(!error.to_string().contains("registry"));
    assert!(terminator.terminated().is_empty());
    assert_eq!(store.load().unwrap(), Some(existing));
}

#[test]
fn an_explicit_cold_stop_terminates_the_busy_daemon() {
    let store = DaemonRecordStore::new(InMemoryRecordFile::default());
    let running = DaemonRecord::new(1111);
    store.save(&running).unwrap();
    let terminator = RecordingTerminator::default();
    assert_eq!(
        stop_daemon(
            &store,
            &FixedProbe(true),
            &terminator,
            &ClearingSleeper {
                store: &store,
                expected: &running,
            },
            &NoopReady,
            &FixedCensus::of(2, 0),
            TransitionMode::Cold,
            &info(),
        )
        .unwrap(),
        "usagi v0.1.0: daemon stopped (pid 1111)"
    );
    assert_eq!(terminator.terminated(), vec![1111]);
}

#[test]
fn a_census_failure_stops_a_transition_before_it_signals_anything() {
    let store = DaemonRecordStore::new(InMemoryRecordFile::default());
    let running = DaemonRecord::new(1111);
    store.save(&running).unwrap();
    let terminator = RecordingTerminator::default();
    let launcher = TestLauncher::registering(&store, 5555);

    let stop_error = stop_daemon(
        &store,
        &FixedProbe(true),
        &terminator,
        &NoopSleeper,
        &NoopReady,
        &FailingCensus,
        TransitionMode::Cold,
        &info(),
    )
    .unwrap_err();
    assert_eq!(stop_error.to_string(), "runtime store unreadable");

    let replace_error = replace_daemon(
        &store,
        &FixedProbe(true),
        &terminator,
        &launcher,
        &NoopSleeper,
        &NoopReady,
        &FailingCensus,
        &SeamlessRefusal::NoGenerationRegistry,
        TransitionMode::Cold,
        None,
        &info(),
    )
    .unwrap_err();
    assert_eq!(replace_error.to_string(), "runtime store unreadable");

    assert!(terminator.terminated().is_empty());
    assert_eq!(launcher.launches(), 0);
    assert_eq!(store.load().unwrap(), Some(running));
}

#[test]
fn replacing_an_idle_daemon_performs_the_cold_transition_under_its_operation() {
    let store = DaemonRecordStore::new(InMemoryRecordFile::default());
    let terminator = RecordingTerminator::default();
    let launcher = TestLauncher::registering(&store, 5555);
    let operation = OperationId("build-rollover-v1-abc".into());

    assert_eq!(
        replace_daemon(
            &store,
            &FixedProbe(true),
            &terminator,
            &launcher,
            &NoopSleeper,
            &NoopReady,
            &FixedCensus::of(0, 0),
            &SeamlessRefusal::NoGenerationRegistry,
            TransitionMode::Planned,
            Some(&operation),
            &info(),
        )
        .unwrap(),
        "usagi v0.1.0: daemon restarted (pid 5555) (operation build-rollover-v1-abc)"
    );
    assert_eq!(launcher.launches(), 1);
}

#[test]
fn a_replacement_without_a_safe_operation_key_still_reports_the_transition() {
    let store = DaemonRecordStore::new(InMemoryRecordFile::default());
    let terminator = RecordingTerminator::default();
    let launcher = TestLauncher::registering(&store, 5555);

    assert_eq!(
        replace_daemon(
            &store,
            &FixedProbe(true),
            &terminator,
            &launcher,
            &NoopSleeper,
            &NoopReady,
            &FixedCensus::of(0, 0),
            &SeamlessRefusal::NoGenerationRegistry,
            TransitionMode::Planned,
            None,
            &info(),
        )
        .unwrap(),
        "usagi v0.1.0: daemon restarted (pid 5555)"
    );
}

/// A cold transition that cannot confirm its successor fails as itself: the
/// operation key is a label on a completed transition, never on a failed one.
#[test]
fn a_failed_cold_transition_propagates_its_own_error() {
    let store = DaemonRecordStore::new(InMemoryRecordFile::default());
    let terminator = RecordingTerminator::default();
    // An idle launcher registers nothing, so the start phase times out.
    let launcher = TestLauncher::idle(&store);
    let operation = OperationId("build-rollover-v1-abc".into());

    let error = replace_daemon(
        &store,
        &FixedProbe(true),
        &terminator,
        &launcher,
        &NoopSleeper,
        &NoopReady,
        &FixedCensus::of(0, 0),
        &SeamlessRefusal::NoGenerationRegistry,
        TransitionMode::Planned,
        Some(&operation),
        &info(),
    )
    .unwrap_err();

    assert!(!error.to_string().contains("operation"));
    assert_eq!(store.load().unwrap(), None);
}

#[test]
fn replacing_a_busy_daemon_is_refused_and_names_the_missing_prerequisite() {
    let store = DaemonRecordStore::new(InMemoryRecordFile::default());
    let running = DaemonRecord::new(1111);
    store.save(&running).unwrap();
    let terminator = RecordingTerminator::default();
    let launcher = TestLauncher::registering(&store, 5555);

    let error = replace_daemon(
        &store,
        &FixedProbe(true),
        &terminator,
        &launcher,
        &NoopSleeper,
        &NoopReady,
        &FixedCensus::of(0, 3),
        &SeamlessRefusal::NoGenerationRegistry,
        TransitionMode::Planned,
        None,
        &info(),
    )
    .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert!(error.to_string().contains("3 generic terminal(s)"));
    assert!(error.to_string().contains("no generation registry exists"));
    // Effect zero: the old daemon is untouched and no successor was launched.
    assert!(terminator.terminated().is_empty());
    assert_eq!(launcher.launches(), 0);
    assert_eq!(store.load().unwrap(), Some(running));
}
