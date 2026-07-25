use std::error::Error as _;

use usagi_core::domain::id::DaemonGeneration;

use super::*;
use crate::usecase::authority::fixture::{build, operation, process, registry, unknown_build};
use crate::usecase::generation::GenerationRole;

fn entry(generation: DaemonGeneration, role: GenerationRole) -> GenerationEntry {
    GenerationEntry {
        generation,
        role,
        endpoint: format!("generations/{}/sock", generation.as_str()),
        process: process(11),
        expected_build: build("next"),
        verified_build: Some(build("next")),
        revision: 1,
    }
}

fn active_document(generation: DaemonGeneration) -> RegistryDocument {
    RegistryDocument {
        current: Some(generation),
        generations: vec![entry(generation, GenerationRole::Active)],
        ..RegistryDocument::default()
    }
}

#[test]
fn an_absent_document_loads_as_an_empty_registry_and_commits_from_absence() {
    let (store, file) = registry(2);
    let snapshot = store.load().unwrap();
    assert_eq!(snapshot.document(), &RegistryDocument::default());
    assert_eq!(store.limit(), 2);
    assert!(file.contents().is_none());

    let active = DaemonGeneration::new();
    let mut next = snapshot.to_document();
    next.revision += 1;
    next.generations.push(entry(active, GenerationRole::Active));
    next.current = Some(active);
    let committed = store.commit(&snapshot, next).unwrap();
    assert_eq!(committed.document().active().unwrap().generation, active);
    assert_eq!(file.writes(), 1);
    assert_eq!(store.load().unwrap().document(), committed.document());
}

#[test]
fn unknown_schema_and_corrupt_bytes_fail_closed_without_a_write() {
    let (store, file) = registry(2);
    file.set_contents(Some("{ not json"));
    assert_eq!(
        store.load().unwrap_err().refusal(),
        Some(RegistryError::Corrupt)
    );

    let foreign = RegistryDocument {
        schema: "usagi-generation-registry-v2".into(),
        ..RegistryDocument::default()
    };
    file.set_contents(Some(&serde_json::to_string(&foreign).unwrap()));
    assert_eq!(
        store.load().unwrap_err().refusal(),
        Some(RegistryError::UnknownSchema)
    );

    // A truncated document is corrupt in the same effect-zero way.
    let active = active_document(DaemonGeneration::new());
    let serialized = serde_json::to_string(&active).unwrap();
    file.set_contents(Some(&serialized[..serialized.len() / 2]));
    assert_eq!(
        store.load().unwrap_err().refusal(),
        Some(RegistryError::Corrupt)
    );
    assert_eq!(file.writes(), 0);
}

#[test]
fn a_stale_writer_loses_the_compare_and_swap_and_changes_nothing() {
    let (store, file) = registry(2);
    let snapshot = store.load().unwrap();

    // Another process committed between this writer's read and its write.
    let winner = active_document(DaemonGeneration::new());
    file.set_contents(Some(&serde_json::to_string(&winner).unwrap()));

    let mut loser = snapshot.to_document();
    loser.revision += 1;
    assert_eq!(
        store.commit(&snapshot, loser).unwrap_err().refusal(),
        Some(RegistryError::StaleRevision)
    );
    assert_eq!(store.load().unwrap().document(), &winner);
    assert_eq!(file.writes(), 0);
}

#[test]
fn a_commit_must_advance_the_revision_by_exactly_one() {
    let (store, _) = registry(2);
    let snapshot = store.load().unwrap();
    for revision in [0, 2] {
        let mut next = snapshot.to_document();
        next.revision = revision;
        assert_eq!(
            store.commit(&snapshot, next).unwrap_err().refusal(),
            Some(RegistryError::StaleRevision)
        );
    }
}

#[test]
fn store_io_failures_are_distinguishable_from_refusals() {
    let (store, file) = registry(2);
    file.fail_read(true);
    let failure = store.load().unwrap_err();
    assert!(failure.refusal().is_none());
    assert!(
        failure
            .to_string()
            .contains("injected registry read failure")
    );
    assert!(failure.source().is_none());

    file.fail_read(false);
    let snapshot = store.load().unwrap();
    file.fail_write(true);
    let mut next = snapshot.to_document();
    next.revision += 1;
    assert!(
        store
            .commit(&snapshot, next)
            .unwrap_err()
            .refusal()
            .is_none()
    );
}

#[test]
fn registering_a_standby_requires_a_known_artifact_and_a_free_slot() {
    let (store, _) = registry(2);
    let active = DaemonGeneration::new();
    let standby = DaemonGeneration::new();
    store
        .update(|document| {
            document
                .generations
                .push(entry(active, GenerationRole::Active));
            document.current = Some(active);
            Ok(())
        })
        .unwrap();

    assert_eq!(
        store
            .update(|document| document.register_standby(
                2,
                standby,
                "generations/standby/sock",
                process(22),
                unknown_build(),
            ))
            .unwrap_err()
            .refusal(),
        Some(RegistryError::BuildIdentityUnknown)
    );

    let register = |generation, endpoint: &'static str, artifact: BuildIdentity| {
        store.update(move |document| {
            document.register_standby(2, generation, endpoint, process(22), artifact)
        })
    };
    register(standby, "generations/standby/sock", build("next")).unwrap();
    // The identical registration is the retry of a lost ACK: idempotent.
    register(standby, "generations/standby/sock", build("next")).unwrap();
    assert_eq!(store.load().unwrap().document().retained(), 2);

    assert_eq!(
        register(standby, "generations/other/sock", build("next"))
            .unwrap_err()
            .refusal(),
        Some(RegistryError::DuplicateGeneration)
    );
    assert_eq!(
        register(
            DaemonGeneration::new(),
            "generations/third/sock",
            build("next")
        )
        .unwrap_err()
        .refusal(),
        Some(RegistryError::GenerationLimit)
    );
}

#[test]
fn standby_verification_accepts_only_the_exact_admitted_artifact() {
    let (store, _) = registry(2);
    let standby = DaemonGeneration::new();
    store
        .update(|document| {
            document.register_standby(
                2,
                standby,
                "generations/standby/sock",
                process(22),
                build("next"),
            )
        })
        .unwrap();

    assert_eq!(
        store
            .update(
                |document| document.verify_standby_build(DaemonGeneration::new(), &build("next"))
            )
            .unwrap_err()
            .refusal(),
        Some(RegistryError::UnknownGeneration)
    );
    assert_eq!(
        store
            .update(|document| document.verify_standby_build(standby, &build("other")))
            .unwrap_err()
            .refusal(),
        Some(RegistryError::BuildMismatch)
    );
    assert_eq!(
        store
            .update(|document| document.verify_standby_build(standby, &unknown_build()))
            .unwrap_err()
            .refusal(),
        Some(RegistryError::BuildIdentityUnknown)
    );
    assert!(
        !store
            .load()
            .unwrap()
            .document()
            .entry(standby)
            .unwrap()
            .is_build_verified()
    );

    store
        .update(|document| document.verify_standby_build(standby, &build("next")))
        .unwrap();
    let verified = store.load().unwrap();
    let verified = verified.document().entry(standby).unwrap();
    assert!(verified.is_build_verified());
    assert_eq!(verified.revision, 2);

    // Only a standby is verifiable; an active generation is past that gate.
    store
        .update(|document| {
            document.transition(standby, GenerationRole::Active)?;
            document.current = Some(standby);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        store
            .update(|document| document.verify_standby_build(standby, &build("next")))
            .unwrap_err()
            .refusal(),
        Some(RegistryError::InvalidTransition)
    );
}

#[test]
fn the_role_transition_table_only_moves_toward_retirement() {
    use GenerationRole::{Active, Draining, Retired, Standby};
    let allowed = [
        (Standby, Active),
        (Standby, Retired),
        (Active, Draining),
        (Active, Retired),
        (Draining, Retired),
    ];
    for from in [Standby, Active, Draining, Retired] {
        for to in [Standby, Active, Draining, Retired] {
            assert_eq!(
                transition_allowed(from, to),
                allowed.contains(&(from, to)),
                "{from:?} -> {to:?}"
            );
        }
    }
}

#[test]
fn transition_clears_current_and_refuses_unknown_or_illegal_moves() {
    let active = DaemonGeneration::new();
    let mut document = active_document(active);
    assert_eq!(
        document.transition(DaemonGeneration::new(), GenerationRole::Retired),
        Err(RegistryError::UnknownGeneration)
    );
    assert_eq!(
        document.transition(active, GenerationRole::Standby),
        Err(RegistryError::InvalidTransition)
    );
    document
        .transition(active, GenerationRole::Draining)
        .unwrap();
    assert_eq!(document.current, None);
    assert_eq!(document.role(active), Some(GenerationRole::Draining));
    assert_eq!(document.entry(active).unwrap().revision, 2);
    assert!(document.active().is_none());
    document
        .transition(active, GenerationRole::Retired)
        .unwrap();
    assert_eq!(document.retained(), 0);
    assert_eq!(document.role(DaemonGeneration::new()), None);
}

#[test]
fn validation_refuses_every_shape_that_could_produce_two_authorities() {
    let active = DaemonGeneration::new();
    let second = DaemonGeneration::new();
    let base = active_document(active);
    base.validate(2).unwrap();

    let mut duplicate = base.clone();
    duplicate
        .generations
        .push(entry(active, GenerationRole::Standby));
    assert_eq!(duplicate.validate(2), Err(RegistryError::Corrupt));

    let mut two_active = base.clone();
    two_active
        .generations
        .push(entry(second, GenerationRole::Active));
    assert_eq!(two_active.validate(2), Err(RegistryError::MultipleActive));

    let mut headless = base.clone();
    headless.current = None;
    assert_eq!(headless.validate(2), Err(RegistryError::MultipleActive));

    let mut wrong_current = base.clone();
    wrong_current.current = Some(second);
    assert_eq!(
        wrong_current.validate(2),
        Err(RegistryError::MultipleActive)
    );

    let mut over_limit = base.clone();
    over_limit
        .generations
        .push(entry(second, GenerationRole::Standby));
    assert_eq!(over_limit.validate(1), Err(RegistryError::GenerationLimit));
    over_limit.validate(2).unwrap();

    let mut dangling_to = base.clone();
    dangling_to.handoff = Some(HandoffRecord {
        operation: operation("a"),
        from: Some(active),
        to: second,
        endpoint: "generations/second/sock".into(),
        phase: HandoffPhase::Preparing,
    });
    assert_eq!(dangling_to.validate(2), Err(RegistryError::Corrupt));

    let mut dangling_from = base;
    dangling_from
        .generations
        .push(entry(second, GenerationRole::Standby));
    dangling_from.handoff = Some(HandoffRecord {
        operation: operation("a"),
        from: Some(DaemonGeneration::new()),
        to: second,
        endpoint: "generations/second/sock".into(),
        phase: HandoffPhase::Preparing,
    });
    assert_eq!(dangling_from.validate(2), Err(RegistryError::Corrupt));

    let mut empty = RegistryDocument::default();
    empty.validate(2).unwrap();
    empty.schema = "other".into();
    assert_eq!(empty.validate(2), Err(RegistryError::UnknownSchema));
}

#[test]
fn a_commit_that_would_break_an_invariant_is_refused_before_it_is_written() {
    let (store, file) = registry(2);
    let snapshot = store.load().unwrap();
    let mut broken = snapshot.to_document();
    broken.revision += 1;
    broken.current = Some(DaemonGeneration::new());
    assert_eq!(
        store.commit(&snapshot, broken).unwrap_err().refusal(),
        Some(RegistryError::MultipleActive)
    );
    assert_eq!(file.writes(), 0);
}

#[test]
fn verified_build_only_counts_when_it_is_the_expected_artifact() {
    let generation = DaemonGeneration::new();
    let mut candidate = entry(generation, GenerationRole::Standby);
    assert!(candidate.is_build_verified());
    candidate.verified_build = Some(build("other"));
    assert!(!candidate.is_build_verified());
    candidate.verified_build = None;
    assert!(!candidate.is_build_verified());
}

#[test]
fn every_refusal_reads_as_a_safety_outcome() {
    for error in [
        RegistryError::UnknownSchema,
        RegistryError::Corrupt,
        RegistryError::StaleRevision,
        RegistryError::DuplicateGeneration,
        RegistryError::UnknownGeneration,
        RegistryError::GenerationLimit,
        RegistryError::InvalidTransition,
        RegistryError::MultipleActive,
        RegistryError::BuildIdentityUnknown,
        RegistryError::BuildMismatch,
        RegistryError::HandoffInProgress,
        RegistryError::UnknownOperation,
        RegistryError::WrongPhase,
    ] {
        assert!(!error.to_string().is_empty());
        assert!(error.source().is_none());
        let failure: RegistryFailure = error.into();
        assert_eq!(failure.refusal(), Some(error));
        assert_eq!(failure.to_string(), error.to_string());
        assert!(failure.source().is_none());
    }
    let io: RegistryFailure = std::io::Error::other("disk").into();
    assert!(io.to_string().contains("disk"));
    assert_eq!(format!("{:?}", HandoffPhase::Committed), "Committed");
}
