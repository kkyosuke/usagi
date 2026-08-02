use std::error::Error as _;

use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::DaemonGeneration as WireGeneration;

use super::*;
use crate::usecase::authority::fixture::{
    MemoryLocator, ProbeReply, RecordingProbe, build, hello, process, registry, unknown_build,
};
use crate::usecase::authority::registry::RegistryError;
use crate::usecase::generation::GenerationRole;

const ENDPOINT: &str = "generations/standby/sock";

#[test]
fn readiness_admits_only_the_exact_admitted_artifact_from_the_registered_peer() {
    let generation = DaemonGeneration::new();
    let expected = build("next");
    verify_readiness(generation, &expected, &hello(generation, &expected)).unwrap();

    let mut foreign = hello(generation, &expected);
    foreign.daemon_generation = WireGeneration(DaemonGeneration::new().as_str());
    assert_eq!(
        verify_readiness(generation, &expected, &foreign),
        Err(ReadinessRefusal::GenerationMismatch)
    );

    // Same version and target, different source tree.
    assert_eq!(
        verify_readiness(generation, &expected, &hello(generation, &build("other"))),
        Err(ReadinessRefusal::IdentityMismatch)
    );
    assert_eq!(
        verify_readiness(generation, &expected, &hello(generation, &unknown_build())),
        Err(ReadinessRefusal::IdentityUnknown)
    );
    assert_eq!(
        verify_readiness(generation, &unknown_build(), &hello(generation, &expected)),
        Err(ReadinessRefusal::IdentityUnknown)
    );

    for missing in standby_readiness_required_capabilities() {
        let mut older = hello(generation, &expected);
        older
            .capabilities
            .retain(|advertised| advertised != missing.wire_name());
        assert_eq!(
            verify_readiness(generation, &expected, &older),
            Err(ReadinessRefusal::UnsupportedCapability)
        );
    }
}

#[test]
fn preparing_a_standby_verifies_it_without_touching_the_current_locator() {
    let (store, file) = registry(2);
    let generation = DaemonGeneration::new();
    let expected = build("next");
    let probe = RecordingProbe::new(ProbeReply::Hello(Box::new(hello(generation, &expected))));
    let locator = MemoryLocator::default();

    let snapshot = prepare_standby(
        &store,
        &probe,
        generation,
        ENDPOINT,
        &process(22),
        &expected,
    )
    .unwrap();

    let entry = snapshot.document().entry(generation).unwrap();
    assert_eq!(entry.role, GenerationRole::Standby);
    assert!(entry.is_build_verified());
    assert_eq!(snapshot.document().current, None);
    assert_eq!(probe.calls(), vec![ENDPOINT.to_owned()]);
    // Exactly two registry writes — admit and verify — and nothing else.
    assert_eq!(file.writes(), 2);
    assert!(locator.publishes().is_empty());
    assert_eq!(locator.retires(), 0);

    // Re-running readiness is idempotent: no probe, no further write.
    let repeated = prepare_standby(
        &store,
        &probe,
        generation,
        ENDPOINT,
        &process(22),
        &expected,
    )
    .unwrap();
    assert!(
        repeated
            .document()
            .entry(generation)
            .unwrap()
            .is_build_verified()
    );
    assert_eq!(probe.calls().len(), 1);
    assert_eq!(file.writes(), 2);
}

#[test]
fn a_mismatched_or_unreachable_standby_keeps_the_old_authority() {
    let active = DaemonGeneration::new();
    let generation = DaemonGeneration::new();
    let expected = build("next");

    let cases: [(ProbeReply, &str); 3] = [
        (
            ProbeReply::Hello(Box::new(hello(generation, &build("other")))),
            "does not match",
        ),
        (
            ProbeReply::Hello(Box::new(hello(generation, &unknown_build()))),
            "unknown",
        ),
        (ProbeReply::Failure("connection refused"), "probe failed"),
    ];

    for (reply, message) in cases {
        let (store, _) = registry(2);
        store
            .update(|document| {
                document
                    .generations
                    .push(crate::usecase::authority::registry::GenerationEntry {
                        generation: active,
                        role: GenerationRole::Active,
                        endpoint: "generations/active/sock".into(),
                        process: process(11),
                        expected_build: build("old"),
                        verified_build: None,
                        revision: 1,
                    });
                document.current = Some(active);
                Ok(())
            })
            .unwrap();
        let probe = RecordingProbe::new(reply);
        let locator = MemoryLocator::default();

        let failure = prepare_standby(
            &store,
            &probe,
            generation,
            ENDPOINT,
            &process(22),
            &expected,
        )
        .unwrap_err();
        assert!(failure.to_string().contains(message), "{failure}");
        assert!(failure.source().is_none());

        let document = store.load().unwrap();
        // The candidate stays a standby and the active generation is untouched.
        assert_eq!(
            document.document().role(generation),
            Some(GenerationRole::Standby)
        );
        assert_eq!(document.document().current, Some(active));
        assert!(locator.publishes().is_empty());
    }
}

#[test]
fn an_unknown_expected_artifact_is_refused_before_a_slot_is_taken() {
    let (store, file) = registry(2);
    let generation = DaemonGeneration::new();
    let probe = RecordingProbe::new(ProbeReply::Failure("never reached"));

    let failure = prepare_standby(
        &store,
        &probe,
        generation,
        ENDPOINT,
        &process(22),
        &unknown_build(),
    )
    .unwrap_err();
    assert!(matches!(&failure, StandbyFailure::Registry(registry)
            if registry.refusal() == Some(RegistryError::BuildIdentityUnknown)));
    assert!(probe.calls().is_empty());
    assert_eq!(file.writes(), 0);
    assert_eq!(store.load().unwrap().document().retained(), 0);
}

#[test]
fn every_readiness_refusal_reads_as_a_safety_outcome() {
    for refusal in [
        ReadinessRefusal::GenerationMismatch,
        ReadinessRefusal::IdentityUnknown,
        ReadinessRefusal::IdentityMismatch,
        ReadinessRefusal::UnsupportedCapability,
    ] {
        assert!(!refusal.to_string().is_empty());
        assert!(refusal.source().is_none());
        let failure: StandbyFailure = refusal.into();
        assert_eq!(failure.to_string(), refusal.to_string());
    }
    let registry: StandbyFailure = RegistryError::StaleRevision.into();
    assert_eq!(
        registry.to_string(),
        RegistryError::StaleRevision.to_string()
    );
}

/// A registry holding one live active generation, as a running daemon leaves it.
fn active_registry(generation: DaemonGeneration, owner: &ProcessIdentity) -> RegistryDocument {
    let mut document = RegistryDocument::default();
    document
        .activate_first(2, generation, ENDPOINT, owner.clone(), build("next"))
        .unwrap();
    document
}

/// The lifecycle record the same daemon writes for that process.
fn owner_record(owner: &ProcessIdentity) -> DaemonRecord {
    DaemonRecord::identified(owner.pid, owner.start_identity.clone())
}

#[test]
fn a_standby_is_admitted_only_next_to_a_registered_live_active_owner() {
    let active = DaemonGeneration::new();
    let owner = process(31);
    let document = active_registry(active, &owner);
    let record = owner_record(&owner);

    assert_eq!(
        admissible_active(
            Some(&document),
            &ActiveOwner {
                record: Some(&record),
                observation: DaemonProcessObservation::Exact,
            },
        ),
        Ok(active)
    );
}

#[test]
fn a_data_directory_without_a_trusted_registry_admits_no_standby() {
    let owner = process(31);
    let record = owner_record(&owner);
    let live = ActiveOwner {
        record: Some(&record),
        observation: DaemonProcessObservation::Exact,
    };

    assert_eq!(
        admissible_active(None, &live),
        Err(StandbyStartRefusal::NoGenerationRegistry)
    );

    let mut foreign = active_registry(DaemonGeneration::new(), &owner);
    foreign.schema = "usagi-generation-registry-v2".to_owned();
    assert_eq!(
        admissible_active(Some(&foreign), &live),
        Err(StandbyStartRefusal::RegistrySchemaUnsupported)
    );
}

/// A standby is not a way to start serving: without a live owner the first
/// daemon activates instead, and admitting a standby here would produce a
/// successor with nothing to succeed.
#[test]
fn no_live_owner_means_no_standby() {
    let active = DaemonGeneration::new();
    let owner = process(31);
    let document = active_registry(active, &owner);
    let record = owner_record(&owner);

    assert_eq!(
        admissible_active(
            Some(&document),
            &ActiveOwner {
                record: None,
                observation: DaemonProcessObservation::Gone,
            },
        ),
        Err(StandbyStartRefusal::NoLiveOwner)
    );
    for uncertain in [
        DaemonProcessObservation::Gone,
        DaemonProcessObservation::IdentityMismatch,
        DaemonProcessObservation::Unknown,
    ] {
        assert_eq!(
            admissible_active(
                Some(&document),
                &ActiveOwner {
                    record: Some(&record),
                    observation: uncertain,
                },
            ),
            Err(StandbyStartRefusal::NoLiveOwner)
        );
    }
}

/// The mixed-build case: an older `serve` owns the data directory without ever
/// registering, so the registry cannot name the authority a handoff would take
/// from. Fail closed rather than admit a standby beside it.
#[test]
fn a_live_owner_the_registry_does_not_name_fails_closed() {
    let owner = process(31);
    let record = owner_record(&owner);
    let live = ActiveOwner {
        record: Some(&record),
        observation: DaemonProcessObservation::Exact,
    };

    // A registry this build wrote, but with every generation already retired:
    // the live owner is not in it.
    let mut retired = active_registry(DaemonGeneration::new(), &owner);
    retired.retire_self(retired.current.unwrap()).unwrap();
    assert_eq!(
        admissible_active(Some(&retired), &live),
        Err(StandbyStartRefusal::OwnerUnregistered)
    );

    // A registry naming an active generation that is *another* process: the
    // record and the registry disagree about who owns the directory.
    let other = active_registry(DaemonGeneration::new(), &process(99));
    assert_eq!(
        admissible_active(Some(&other), &live),
        Err(StandbyStartRefusal::OwnerUnregistered)
    );

    // Same pid, different process incarnation.
    let reused = active_registry(
        DaemonGeneration::new(),
        &ProcessIdentity {
            pid: owner.pid,
            start_identity: "start-elsewhere".to_owned(),
            process_group: owner.process_group,
        },
    );
    assert_eq!(
        admissible_active(Some(&reused), &live),
        Err(StandbyStartRefusal::OwnerUnregistered)
    );
}

#[test]
fn a_handoff_in_flight_admits_no_third_process() {
    let active = DaemonGeneration::new();
    let owner = process(31);
    let mut document = active_registry(active, &owner);
    let successor = DaemonGeneration::new();
    document
        .register_standby(2, successor, ENDPOINT, process(32), build("next"))
        .unwrap();
    document
        .verify_standby_build(successor, &build("next"))
        .unwrap();
    crate::usecase::authority::handoff::begin_handoff(
        &mut document,
        &crate::usecase::authority::fixture::operation("mid"),
        Some(active),
        successor,
    )
    .unwrap();
    let record = owner_record(&owner);

    assert_eq!(
        admissible_active(
            Some(&document),
            &ActiveOwner {
                record: Some(&record),
                observation: DaemonProcessObservation::Exact,
            },
        ),
        Err(StandbyStartRefusal::HandoffInFlight)
    );
}

#[test]
fn every_start_refusal_reads_as_a_safety_outcome() {
    let refusals = [
        StandbyStartRefusal::NoGenerationRegistry,
        StandbyStartRefusal::RegistrySchemaUnsupported,
        StandbyStartRefusal::NoLiveOwner,
        StandbyStartRefusal::OwnerUnregistered,
        StandbyStartRefusal::HandoffInFlight,
    ];
    for refusal in refusals {
        assert!(!refusal.to_string().is_empty());
        assert!(refusal.source().is_none());
        assert_eq!(
            std::io::Error::from(refusal).to_string(),
            refusal.to_string()
        );
    }
    let messages: std::collections::BTreeSet<_> =
        refusals.iter().map(ToString::to_string).collect();
    assert_eq!(messages.len(), refusals.len());
}

/// Every observation this suite makes: alive exactly when the pid is listed.
fn alive(pids: Vec<u32>) -> impl FnMut(&ProcessIdentity) -> ProcessObservation {
    move |observed| {
        if pids.contains(&observed.pid) {
            ProcessObservation::VerifiedAlive(observed.clone())
        } else {
            ProcessObservation::Gone
        }
    }
}

/// A standby holds no lock and no lifecycle record, so its registry entry is its
/// custody. Retirement — by recovery that failed an abandoned authority closed,
/// or by collection — is what tells it to exit.
#[test]
fn a_standby_holds_custody_until_its_entry_is_retired_or_replaced() {
    let active = DaemonGeneration::new();
    let owner = process(31);
    let mut document = active_registry(active, &owner);
    let generation = DaemonGeneration::new();
    let mine = process(32);
    document
        .register_standby(2, generation, ENDPOINT, mine.clone(), build("next"))
        .unwrap();

    assert_eq!(
        evaluate_custody(&document, generation, &mine, &mut alive(vec![31])),
        StandbyCustody::Held
    );

    // A promotion is not a loss: only a retired generation admits nothing. It
    // also ends the incumbent check — this generation *is* the authority now,
    // which is why nothing in this registry needs to be observably alive.
    let mut promoted = document.clone();
    promoted
        .transition(active, GenerationRole::Draining)
        .unwrap();
    promoted
        .transition(generation, GenerationRole::Active)
        .unwrap();
    promoted.current = Some(generation);
    assert_eq!(
        evaluate_custody(&promoted, generation, &mine, &mut alive(Vec::new())),
        StandbyCustody::Held
    );

    let mut retired = document.clone();
    retired.retire_self(generation).unwrap();
    assert_eq!(
        evaluate_custody(&retired, generation, &mine, &mut alive(vec![31])),
        StandbyCustody::Lost(StandbyCustodyLoss::EntryRetired)
    );

    assert_eq!(
        evaluate_custody(
            &document,
            DaemonGeneration::new(),
            &mine,
            &mut alive(vec![31])
        ),
        StandbyCustody::Lost(StandbyCustodyLoss::EntryAbsent)
    );

    assert_eq!(
        evaluate_custody(&document, generation, &process(77), &mut alive(vec![31])),
        StandbyCustody::Lost(StandbyCustodyLoss::EntryReplaced)
    );
}

/// A standby that outlives its incumbent is not idle — it is a *retained*
/// generation, and activation refuses a registry that retains one. Without this
/// invariant, `daemon start` after a clean `daemon stop` fails with
/// `authority_retained` forever, because a clean stop retires the active's entry
/// and leaves the standby's untouched.
#[test]
fn a_standby_loses_custody_when_its_incumbent_goes_away() {
    let active = DaemonGeneration::new();
    let owner = process(31);
    let mut document = active_registry(active, &owner);
    let generation = DaemonGeneration::new();
    let mine = process(32);
    document
        .register_standby(2, generation, ENDPOINT, mine.clone(), build("next"))
        .unwrap();

    // A clean `daemon stop`: the active gave its own entry up and nothing else
    // changed. This is the case that used to be missed.
    let mut stopped = document.clone();
    stopped.retire_self(active).unwrap();
    assert_eq!(
        evaluate_custody(&stopped, generation, &mine, &mut alive(vec![31])),
        StandbyCustody::Lost(StandbyCustodyLoss::IncumbentGone)
    );

    // Or the active died without giving anything up, which the OS reports.
    assert_eq!(
        evaluate_custody(&document, generation, &mine, &mut alive(Vec::new())),
        StandbyCustody::Lost(StandbyCustodyLoss::IncumbentGone)
    );

    // A PID that is live but is not the recorded incarnation proves nothing.
    assert_eq!(
        evaluate_custody(&document, generation, &mine, &mut |_| {
            ProcessObservation::Unknown
        }),
        StandbyCustody::Lost(StandbyCustodyLoss::IncumbentGone)
    );
}

/// A handoff is the incumbent being replaced on purpose, so a momentarily
/// absent active inside one is the protocol working — not the authority
/// disappearing. Its outcome decides instead.
#[test]
fn an_in_flight_handoff_suspends_the_incumbent_check() {
    let active = DaemonGeneration::new();
    let owner = process(31);
    let mut document = active_registry(active, &owner);
    let generation = DaemonGeneration::new();
    let mine = process(32);
    document
        .register_standby(2, generation, ENDPOINT, mine.clone(), build("next"))
        .unwrap();
    document.handoff = Some(crate::usecase::authority::registry::HandoffRecord {
        operation: usagi_core::infrastructure::ipc::OperationId("rollover".into()),
        from: Some(active),
        to: generation,
        endpoint: ENDPOINT.to_owned(),
        phase: crate::usecase::authority::registry::HandoffPhase::Preparing,
    });

    assert_eq!(
        evaluate_custody(&document, generation, &mine, &mut alive(Vec::new())),
        StandbyCustody::Held
    );
}

#[test]
fn every_custody_loss_carries_a_distinct_reason() {
    let reasons = [
        StandbyCustodyLoss::EntryAbsent,
        StandbyCustodyLoss::EntryRetired,
        StandbyCustodyLoss::EntryReplaced,
        StandbyCustodyLoss::IncumbentGone,
    ]
    .map(StandbyCustodyLoss::reason);
    let unique: std::collections::BTreeSet<_> = reasons.iter().collect();
    assert_eq!(unique.len(), reasons.len());
}
