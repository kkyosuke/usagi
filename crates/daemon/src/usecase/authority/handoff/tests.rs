use usagi_core::domain::id::DaemonGeneration;

use super::*;
use crate::usecase::authority::fixture::{build, operation, process};
use crate::usecase::authority::registry::{GenerationEntry, RegistryDocument};

struct Scenario {
    document: RegistryDocument,
    old: DaemonGeneration,
    next: DaemonGeneration,
}

fn entry(
    generation: DaemonGeneration,
    role: GenerationRole,
    tag: &str,
    pid: u32,
) -> GenerationEntry {
    GenerationEntry {
        generation,
        role,
        endpoint: format!("generations/{tag}/sock"),
        process: process(pid),
        expected_build: build(tag),
        verified_build: Some(build(tag)),
        revision: 1,
    }
}

fn scenario() -> Scenario {
    let old = DaemonGeneration::new();
    let next = DaemonGeneration::new();
    Scenario {
        document: RegistryDocument {
            current: Some(old),
            generations: vec![
                entry(old, GenerationRole::Active, "old", 1),
                entry(next, GenerationRole::Standby, "next", 2),
            ],
            ..RegistryDocument::default()
        },
        old,
        next,
    }
}

fn published(document: &RegistryDocument, generation: DaemonGeneration) -> LocatorObservation {
    let entry = document.entry(generation).unwrap();
    LocatorObservation::Published(PublishedLocator {
        generation,
        endpoint: entry.endpoint.clone(),
    })
}

/// Observes every recorded identity as alive.
fn all_alive(process: &ProcessIdentity) -> ProcessObservation {
    ProcessObservation::VerifiedAlive(process.clone())
}

/// Observes nothing as alive — the state a fresh process finds after a crash.
fn none_alive(_: &ProcessIdentity) -> ProcessObservation {
    ProcessObservation::Gone
}

#[test]
fn the_protocol_moves_authority_exactly_once() {
    let Scenario {
        mut document,
        old,
        next,
    } = scenario();
    let op = operation("a");

    assert_eq!(
        begin_handoff(&mut document, &op, Some(old), next),
        Ok(RolloverOutcome::Advanced)
    );
    // W1 records intent only: authority has not moved.
    assert_eq!(document.current, Some(old));
    assert_eq!(document.role(next), Some(GenerationRole::Standby));

    assert_eq!(
        commit_registry(&mut document, &op),
        Ok(RolloverOutcome::Advanced)
    );
    assert_eq!(document.current, Some(next));
    assert_eq!(document.role(old), Some(GenerationRole::Draining));
    assert_eq!(document.role(next), Some(GenerationRole::Active));
    assert_eq!(
        document.handoff.as_ref().unwrap().phase,
        HandoffPhase::Committed
    );

    assert_eq!(
        complete_handoff(&mut document, &op),
        Ok(RolloverOutcome::Advanced)
    );
    assert!(document.handoff.is_none());
    assert_eq!(document.completed_operation, Some(op));
    document.validate(2).unwrap();
}

#[test]
fn repeating_an_operation_converges_instead_of_starting_a_second_handoff() {
    let Scenario {
        mut document,
        old,
        next,
    } = scenario();
    let op = operation("a");
    let other = operation("b");

    begin_handoff(&mut document, &op, Some(old), next).unwrap();
    assert_eq!(
        begin_handoff(&mut document, &op, Some(old), next),
        Ok(RolloverOutcome::AlreadyThere)
    );
    // A concurrent restart carrying a different operation is refused rather
    // than allowed to start a competing handoff.
    assert_eq!(
        begin_handoff(&mut document, &other, Some(old), next),
        Err(RegistryError::HandoffInProgress)
    );

    commit_registry(&mut document, &op).unwrap();
    assert_eq!(
        commit_registry(&mut document, &op),
        Ok(RolloverOutcome::AlreadyThere)
    );
    complete_handoff(&mut document, &op).unwrap();

    // Every step of a finished operation converges on the same answer, which
    // is what a lost ACK replays into.
    assert_eq!(
        begin_handoff(&mut document, &op, Some(old), next),
        Ok(RolloverOutcome::AlreadyCompleted)
    );
    assert_eq!(
        commit_registry(&mut document, &op),
        Ok(RolloverOutcome::AlreadyCompleted)
    );
    assert_eq!(
        complete_handoff(&mut document, &op),
        Ok(RolloverOutcome::AlreadyCompleted)
    );
    assert_eq!(document.retained(), 2);
}

#[test]
fn an_unobservable_intent_can_be_aborted_but_a_commit_cannot() {
    let Scenario {
        mut document,
        old,
        next,
    } = scenario();
    let op = operation("a");
    assert_eq!(
        abort_handoff(&mut document, &op),
        Err(RegistryError::UnknownOperation)
    );

    begin_handoff(&mut document, &op, Some(old), next).unwrap();
    assert_eq!(
        abort_handoff(&mut document, &operation("b")),
        Err(RegistryError::UnknownOperation)
    );
    assert_eq!(
        abort_handoff(&mut document, &op),
        Ok(RolloverOutcome::Advanced)
    );
    assert_eq!(document.current, Some(old));
    assert_eq!(document.completed_operation, Some(op));

    let Scenario {
        mut document,
        old,
        next,
    } = scenario();
    let op = operation("c");
    begin_handoff(&mut document, &op, Some(old), next).unwrap();
    commit_registry(&mut document, &op).unwrap();
    assert_eq!(
        abort_handoff(&mut document, &op),
        Err(RegistryError::WrongPhase)
    );
}

#[test]
fn steps_refuse_an_operation_that_is_not_the_one_in_flight() {
    let Scenario {
        mut document,
        old,
        next,
    } = scenario();
    let op = operation("a");
    let other = operation("b");
    assert_eq!(
        commit_registry(&mut document, &op),
        Err(RegistryError::UnknownOperation)
    );
    assert_eq!(
        complete_handoff(&mut document, &op),
        Err(RegistryError::UnknownOperation)
    );

    begin_handoff(&mut document, &op, Some(old), next).unwrap();
    assert_eq!(
        commit_registry(&mut document, &other),
        Err(RegistryError::UnknownOperation)
    );
    assert_eq!(
        complete_handoff(&mut document, &other),
        Err(RegistryError::UnknownOperation)
    );
    assert_eq!(
        complete_handoff(&mut document, &op),
        Err(RegistryError::WrongPhase)
    );
}

#[test]
fn only_a_verified_standby_and_the_exact_current_owner_are_eligible() {
    let Scenario {
        document,
        old,
        next,
    } = scenario();
    let op = operation("a");

    let mut unknown = document.clone();
    assert_eq!(
        begin_handoff(&mut unknown, &op, Some(old), DaemonGeneration::new()),
        Err(RegistryError::UnknownGeneration)
    );

    let mut not_standby = document.clone();
    assert_eq!(
        begin_handoff(&mut not_standby, &op, Some(next), old),
        Err(RegistryError::InvalidTransition)
    );

    let mut unverified = document.clone();
    unverified
        .generations
        .iter_mut()
        .find(|entry| entry.generation == next)
        .unwrap()
        .verified_build = None;
    assert_eq!(
        begin_handoff(&mut unverified, &op, Some(old), next),
        Err(RegistryError::BuildMismatch)
    );

    let mut unknown_build = document.clone();
    unknown_build
        .generations
        .iter_mut()
        .find(|entry| entry.generation == next)
        .unwrap()
        .expected_build = crate::usecase::authority::fixture::unknown_build();
    assert_eq!(
        begin_handoff(&mut unknown_build, &op, Some(old), next),
        Err(RegistryError::BuildIdentityUnknown)
    );

    let mut wrong_predecessor = document.clone();
    assert_eq!(
        begin_handoff(&mut wrong_predecessor, &op, None, next),
        Err(RegistryError::MultipleActive)
    );

    // Defensive: a document whose `current` names a non-active generation is
    // rejected by validation, so construct it directly to cover the guard.
    let mut inconsistent = document;
    inconsistent
        .generations
        .iter_mut()
        .find(|entry| entry.generation == old)
        .unwrap()
        .role = GenerationRole::Draining;
    assert_eq!(
        begin_handoff(&mut inconsistent, &op, Some(old), next),
        Err(RegistryError::InvalidTransition)
    );
}

#[test]
fn recovery_of_a_steady_state_repairs_only_the_locator() {
    let Scenario { document, old, .. } = scenario();
    let mut document = document;
    document.generations.retain(|entry| entry.generation == old);

    let matching = published(&document, old);
    assert_eq!(
        plan_recovery(&document, &matching, &mut all_alive),
        RecoveryPlan {
            outcome: RecoveryOutcome::Consistent,
            retire_locator: false,
            publish: None,
            document: None,
        }
    );

    for observation in [
        LocatorObservation::Absent,
        LocatorObservation::Unreadable,
        LocatorObservation::Published(PublishedLocator {
            generation: DaemonGeneration::new(),
            endpoint: "generations/ghost/sock".into(),
        }),
    ] {
        let plan = plan_recovery(&document, &observation, &mut all_alive);
        assert_eq!(plan.outcome, RecoveryOutcome::RepairedCurrent);
        assert_eq!(
            plan.publish,
            Some(PublishedLocator {
                generation: old,
                endpoint: document.entry(old).unwrap().endpoint.clone(),
            })
        );
        assert!(plan.document.is_none());
        assert!(!plan.retire_locator);
    }
}

#[test]
fn an_empty_registry_only_tolerates_an_absent_locator() {
    let empty = RegistryDocument::default();
    assert_eq!(
        plan_recovery(&empty, &LocatorObservation::Absent, &mut all_alive).outcome,
        RecoveryOutcome::Consistent
    );
    for (observation, reason) in [
        (
            LocatorObservation::Unreadable,
            RecoveryRefusal::UnreadableLocator,
        ),
        (
            LocatorObservation::Published(PublishedLocator {
                generation: DaemonGeneration::new(),
                endpoint: "generations/ghost/sock".into(),
            }),
            RecoveryRefusal::StaleCurrent,
        ),
    ] {
        let plan = plan_recovery(&empty, &observation, &mut all_alive);
        assert_eq!(plan.outcome, RecoveryOutcome::FailedClosed(reason));
        assert!(plan.retire_locator);
        assert_eq!(plan.document.unwrap().revision, 1);
    }
}

#[test]
fn a_dead_active_generation_is_retired_and_never_republished() {
    let Scenario { document, old, .. } = scenario();
    let locator = published(&document, old);
    let plan = plan_recovery(&document, &locator, &mut none_alive);
    assert_eq!(
        plan.outcome,
        RecoveryOutcome::FailedClosed(RecoveryRefusal::ActiveGone)
    );
    assert!(plan.retire_locator);
    assert!(plan.publish.is_none());
    let repaired = plan.document.unwrap();
    assert_eq!(repaired.current, None);
    assert_eq!(repaired.retained(), 0);
    assert_eq!(repaired.revision, document.revision + 1);
    repaired.validate(2).unwrap();
}

#[test]
fn pid_reuse_is_not_proof_that_the_recorded_owner_is_alive() {
    let Scenario { document, old, .. } = scenario();
    let locator = published(&document, old);
    let plan = plan_recovery(&document, &locator, &mut |process| {
        ProcessObservation::VerifiedAlive(ProcessIdentity {
            start_identity: "reused".into(),
            ..process.clone()
        })
    });
    assert_eq!(
        plan.outcome,
        RecoveryOutcome::FailedClosed(RecoveryRefusal::ActiveGone)
    );

    let unknown = plan_recovery(&document, &locator, &mut |_| ProcessObservation::Unknown);
    assert_eq!(
        unknown.outcome,
        RecoveryOutcome::FailedClosed(RecoveryRefusal::ActiveGone)
    );
}

#[test]
fn a_crash_between_the_intent_and_the_commit_keeps_the_old_authority() {
    let Scenario {
        mut document,
        old,
        next,
    } = scenario();
    let op = operation("a");
    begin_handoff(&mut document, &op, Some(old), next).unwrap();
    let locator = published(&document, old);

    let plan = plan_recovery(&document, &locator, &mut all_alive);
    assert_eq!(plan.outcome, RecoveryOutcome::AbortedIntent(op.clone()));
    assert!(plan.publish.is_none());
    assert!(!plan.retire_locator);
    let repaired = plan.document.unwrap();
    assert_eq!(repaired.current, Some(old));
    assert_eq!(repaired.role(next), Some(GenerationRole::Standby));
    assert_eq!(repaired.completed_operation, Some(op));
    assert_eq!(repaired.revision, document.revision + 1);
    repaired.validate(2).unwrap();
}

#[test]
fn aborting_an_intent_still_repairs_a_locator_that_drifted() {
    let Scenario {
        mut document,
        old,
        next,
    } = scenario();
    let op = operation("a");
    begin_handoff(&mut document, &op, Some(old), next).unwrap();

    let plan = plan_recovery(&document, &LocatorObservation::Absent, &mut all_alive);
    assert_eq!(plan.outcome, RecoveryOutcome::AbortedIntent(op));
    assert_eq!(
        plan.publish,
        Some(PublishedLocator {
            generation: old,
            endpoint: document.entry(old).unwrap().endpoint.clone(),
        })
    );
    assert!(plan.document.unwrap().handoff.is_none());
}

#[test]
fn an_intent_whose_owner_died_fails_closed_rather_than_being_aborted() {
    let Scenario {
        mut document,
        old,
        next,
    } = scenario();
    let op = operation("a");
    begin_handoff(&mut document, &op, Some(old), next).unwrap();

    let locator = published(&document, old);
    let plan = plan_recovery(&document, &locator, &mut none_alive);
    assert_eq!(
        plan.outcome,
        RecoveryOutcome::FailedClosed(RecoveryRefusal::ActiveGone)
    );
    let repaired = plan.document.unwrap();
    assert_eq!(repaired.retained(), 0);
    assert_eq!(repaired.completed_operation, Some(op));
    assert_eq!(repaired.revision, document.revision + 1);
}

#[test]
fn a_committed_handoff_rolls_forward_from_either_side_of_the_locator_write() {
    let Scenario {
        mut document,
        old,
        next,
    } = scenario();
    let op = operation("a");
    begin_handoff(&mut document, &op, Some(old), next).unwrap();
    commit_registry(&mut document, &op).unwrap();
    let target = PublishedLocator {
        generation: next,
        endpoint: document.entry(next).unwrap().endpoint.clone(),
    };

    // Crashed before the locator write: the old endpoint is still published,
    // and recovery moves forward to the committed authority.
    let before = plan_recovery(&document, &published(&document, old), &mut all_alive);
    assert_eq!(before.outcome, RecoveryOutcome::RolledForward(op.clone()));
    assert_eq!(before.publish, Some(target.clone()));
    let repaired = before.document.unwrap();
    assert_eq!(repaired.current, Some(next));
    assert_eq!(repaired.role(old), Some(GenerationRole::Draining));
    assert!(repaired.handoff.is_none());
    assert_eq!(repaired.completed_operation, Some(op.clone()));

    // Crashed after the locator write: only the bookkeeping is missing.
    let after = plan_recovery(
        &document,
        &LocatorObservation::Published(target),
        &mut all_alive,
    );
    assert_eq!(after.outcome, RecoveryOutcome::RolledForward(op));
    assert!(after.publish.is_none());
    assert_eq!(after.document.unwrap().current, Some(next));
}

#[test]
fn a_committed_handoff_whose_successor_died_never_restores_the_old_authority() {
    let Scenario {
        mut document,
        old,
        next,
    } = scenario();
    let op = operation("a");
    begin_handoff(&mut document, &op, Some(old), next).unwrap();
    commit_registry(&mut document, &op).unwrap();

    // The old owner is still observable, but authority already moved: reviving
    // it would roll back a commit clients could have seen.
    let plan = plan_recovery(&document, &published(&document, old), &mut |process| {
        if process.pid == 1 {
            ProcessObservation::VerifiedAlive(process.clone())
        } else {
            ProcessObservation::Gone
        }
    });
    assert_eq!(
        plan.outcome,
        RecoveryOutcome::FailedClosed(RecoveryRefusal::SuccessorGone)
    );
    assert!(plan.retire_locator);
    let repaired = plan.document.unwrap();
    assert_eq!(repaired.current, None);
    assert_eq!(repaired.role(old), Some(GenerationRole::Retired));
    assert_eq!(repaired.role(next), Some(GenerationRole::Retired));
    assert_eq!(repaired.completed_operation, Some(op));
    repaired.validate(2).unwrap();
}

#[test]
fn a_committed_handoff_with_a_missing_successor_entry_fails_closed() {
    let Scenario {
        mut document,
        old,
        next,
    } = scenario();
    let op = operation("a");
    begin_handoff(&mut document, &op, Some(old), next).unwrap();
    commit_registry(&mut document, &op).unwrap();
    document
        .generations
        .retain(|entry| entry.generation != next);

    let plan = plan_recovery(&document, &LocatorObservation::Absent, &mut all_alive);
    assert_eq!(
        plan.outcome,
        RecoveryOutcome::FailedClosed(RecoveryRefusal::SuccessorGone)
    );
}
