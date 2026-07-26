//! Per-pool capacity, producer-operation identity, and the cross-process swap.

use std::sync::{Arc, Barrier};

use usagi_core::domain::id::{DaemonGeneration, OperationId, TerminalId};

use super::{
    ALLOCATOR_SCHEMA, Admission, AllocatorDocument, ClaimState, ConsumeOutcome, ExpiryClass,
    LaunchFailure, OperationOutcome, OperationTombstone, ResourceKind, precedes_or_equals,
};
use crate::usecase::resources::fixture::{SharedBytes, allocator, policy, terminal};
use crate::usecase::resources::{CasDocument, ResourceError};

fn reserved(
    kind: ResourceKind,
    limit: usize,
) -> (AllocatorDocument, OperationId, DaemonGeneration) {
    let owner = DaemonGeneration::new();
    let operation = OperationId::new();
    let mut document = AllocatorDocument::default();
    document
        .reserve(
            &operation,
            "digest",
            kind,
            owner,
            &terminal(owner),
            policy(limit, limit),
        )
        .unwrap();
    (document, operation, owner)
}

#[test]
fn each_pool_holds_its_own_limit_and_the_two_are_never_summed() {
    let owner = DaemonGeneration::new();
    let mut document = AllocatorDocument::default();
    let policy = policy(1, 2);
    assert_eq!(policy.limit(ResourceKind::Agent), 1);
    assert_eq!(policy.limit(ResourceKind::Terminal), 2);
    assert_eq!(ResourceKind::Agent.pool(), "agent");
    assert_eq!(ResourceKind::Terminal.pool(), "terminal");

    for kind in [
        ResourceKind::Agent,
        ResourceKind::Terminal,
        ResourceKind::Terminal,
    ] {
        document
            .reserve(
                &OperationId::new(),
                "digest",
                kind,
                owner,
                &terminal(owner),
                policy,
            )
            .unwrap();
    }
    assert_eq!(document.pool_used(ResourceKind::Agent), 1);
    assert_eq!(document.pool_used(ResourceKind::Terminal), 2);
    // The Agent pool being full does not consume terminal capacity, and neither
    // pool borrows from the other's headroom.
    for kind in [ResourceKind::Agent, ResourceKind::Terminal] {
        assert_eq!(
            document.reserve(
                &OperationId::new(),
                "digest",
                kind,
                owner,
                &terminal(owner),
                policy,
            ),
            Err(ResourceError::CapacityExhausted)
        );
    }
    assert_eq!(document.owner_claims(owner), 3);
    assert_eq!(document.owner_claims(DaemonGeneration::new()), 0);
    document.validate().unwrap();
}

#[test]
fn the_same_operation_replays_and_a_different_intent_conflicts() {
    let (mut document, operation, owner) = reserved(ResourceKind::Terminal, 4);
    let claimed = document.operation(&operation).unwrap().resource.clone();

    let replay = document
        .admit(&operation, "digest", ResourceKind::Terminal, policy(4, 4))
        .unwrap();
    assert_eq!(
        replay,
        Admission::Replay {
            resource: claimed.clone(),
            outcome: OperationOutcome::Reserved,
            revision: 1,
        }
    );

    assert_eq!(
        document.admit(&operation, "other", ResourceKind::Terminal, policy(4, 4)),
        Err(ResourceError::OperationConflict)
    );
    // A conflict changes nothing: the pool and the claimed resource stand.
    assert_eq!(document.pool_used(ResourceKind::Terminal), 1);
    assert_eq!(
        document.claim(&claimed).unwrap().state,
        ClaimState::Reserved
    );

    // Re-reserving the same operation is idempotent, not a second claim.
    let again = document
        .reserve(
            &operation,
            "digest",
            ResourceKind::Terminal,
            owner,
            &terminal(owner),
            policy(4, 4),
        )
        .unwrap();
    assert!(matches!(again, Admission::Replay { .. }));
    assert_eq!(document.claims.len(), 1);
}

#[test]
fn a_resource_id_or_owner_that_contradicts_the_claim_is_refused() {
    let owner = DaemonGeneration::new();
    let other = DaemonGeneration::new();
    let mut document = AllocatorDocument::default();
    let resource = terminal(owner);
    document
        .reserve(
            &OperationId::new(),
            "digest",
            ResourceKind::Terminal,
            owner,
            &resource,
            policy(4, 4),
        )
        .unwrap();
    assert_eq!(
        document.reserve(
            &OperationId::new(),
            "digest",
            ResourceKind::Terminal,
            owner,
            &resource,
            policy(4, 4),
        ),
        Err(ResourceError::DuplicateResource)
    );
    assert_eq!(
        document.reserve(
            &OperationId::new(),
            "digest",
            ResourceKind::Terminal,
            other,
            &terminal(owner),
            policy(4, 4),
        ),
        Err(ResourceError::DuplicateResource),
        "a resource must name the generation that claims it"
    );
}

#[test]
fn finals_are_recorded_once_and_a_contradicting_final_is_refused() {
    let (mut document, operation, _) = reserved(ResourceKind::Terminal, 4);
    let resource = document.operation(&operation).unwrap().resource.clone();

    document.mark_spawned(&operation, 10).unwrap();
    assert_eq!(document.claim(&resource).unwrap().state, ClaimState::Live);
    let record = document.operation(&operation).unwrap();
    assert_eq!(record.outcome, OperationOutcome::Spawned);
    assert_eq!(record.sealed_at, Some(10));
    let revision = record.revision;

    document.mark_spawned(&operation, 99).unwrap();
    assert_eq!(document.operation(&operation).unwrap().revision, revision);
    assert_eq!(document.operation(&operation).unwrap().sealed_at, Some(10));

    assert_eq!(
        document.mark_ambiguous(&operation, 11),
        Err(ResourceError::WrongState)
    );
    assert_eq!(
        document.mark_failed(&operation, LaunchFailure::Spawn, 11),
        Err(ResourceError::WrongState)
    );

    let unknown = OperationId::new();
    assert_eq!(
        document.mark_spawned(&unknown, 1),
        Err(ResourceError::UnknownOperation)
    );
    assert_eq!(
        document.mark_failed(&unknown, LaunchFailure::Reservation, 1),
        Err(ResourceError::UnknownOperation)
    );
    document.validate().unwrap();
}

#[test]
fn a_definite_failure_releases_capacity_and_an_ambiguous_one_keeps_it() {
    let (mut document, failed, owner) = reserved(ResourceKind::Terminal, 4);
    let failed_resource = document.operation(&failed).unwrap().resource.clone();
    document
        .mark_failed(&failed, LaunchFailure::Spawn, 5)
        .unwrap();
    assert_eq!(
        document.claim(&failed_resource).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(document.pool_used(ResourceKind::Terminal), 0);

    let ambiguous = OperationId::new();
    document
        .reserve(
            &ambiguous,
            "digest",
            ResourceKind::Terminal,
            owner,
            &terminal(owner),
            policy(4, 4),
        )
        .unwrap();
    let held = document.operation(&ambiguous).unwrap().resource.clone();
    document.mark_ambiguous(&ambiguous, 6).unwrap();
    assert_eq!(
        document.claim(&held).unwrap().state,
        ClaimState::Reserved,
        "a child may exist, so its capacity is never guessed away"
    );
    assert_eq!(document.pool_used(ResourceKind::Terminal), 1);
    assert!(!OperationOutcome::Ambiguous.is_collectable());
    assert!(OperationOutcome::Ambiguous.is_final());
    assert!(!OperationOutcome::Reserved.is_final());
    assert!(OperationOutcome::Failed(LaunchFailure::Reservation).is_collectable());
}

#[test]
fn an_owner_event_releases_capacity_exactly_once_however_it_is_redelivered() {
    let (mut document, operation, owner) = reserved(ResourceKind::Terminal, 4);
    let resource = document.operation(&operation).unwrap().resource.clone();
    document.mark_spawned(&operation, 1).unwrap();

    assert_eq!(
        document.consume_progress(owner, &resource, 1).unwrap(),
        ConsumeOutcome::Applied
    );
    assert_eq!(document.consumed_revision(&resource), Some(1));
    assert_eq!(
        document.claim(&resource).unwrap().state,
        ClaimState::Live,
        "progress never releases capacity"
    );

    assert_eq!(
        document.consume_exit(owner, &resource, 2).unwrap(),
        ConsumeOutcome::Applied
    );
    assert_eq!(
        document.claim(&resource).unwrap().state,
        ClaimState::Released
    );
    let released_revision = document.claim(&resource).unwrap().revision;

    for revision in [2, 1, 2] {
        assert_eq!(
            document.consume_exit(owner, &resource, revision).unwrap(),
            ConsumeOutcome::AlreadyConsumed,
            "duplicate, late, and reordered redelivery converge"
        );
    }
    assert_eq!(
        document.claim(&resource).unwrap().revision,
        released_revision
    );
    assert_eq!(document.consumed_revision(&resource), Some(2));
    document.validate().unwrap();
}

#[test]
fn an_event_from_the_wrong_owner_or_for_an_unknown_resource_changes_nothing() {
    let (mut document, operation, owner) = reserved(ResourceKind::Terminal, 4);
    let resource = document.operation(&operation).unwrap().resource.clone();
    document.mark_spawned(&operation, 1).unwrap();
    let before = document.clone();

    assert_eq!(
        document.consume_exit(DaemonGeneration::new(), &resource, 1),
        Err(ResourceError::WrongOwner)
    );
    assert_eq!(
        document.consume_exit(owner, &terminal(owner), 1),
        Err(ResourceError::UnknownResource)
    );
    assert_eq!(document, before);
}

#[test]
fn a_compacted_or_below_watermark_operation_can_never_be_admitted_again() {
    let mut document = AllocatorDocument::default();
    let old = OperationId::parse("018f0000-0000-7000-8000-000000000001").unwrap();
    let newer = OperationId::parse("018f0000-0000-7000-8000-000000000009").unwrap();
    assert!(precedes_or_equals(&old, &newer));
    assert!(precedes_or_equals(&old, &old));
    assert!(!precedes_or_equals(&newer, &old));

    document.tombstones.push(OperationTombstone {
        operation: old,
        digest: "digest".to_owned(),
        class: ExpiryClass::Spawned,
        cutoff: 1,
    });
    assert!(document.is_expired(&old));
    assert_eq!(
        document.admit(&old, "digest", ResourceKind::Terminal, policy(4, 4)),
        Err(ResourceError::OperationExpired)
    );

    document.watermark = Some(newer);
    document.tombstones.clear();
    assert!(document.is_expired(&old), "the watermark alone is enough");
    assert!(document.is_expired(&newer));
    assert_eq!(
        document.reserve(
            &newer,
            "digest",
            ResourceKind::Terminal,
            DaemonGeneration::new(),
            &terminal(DaemonGeneration::new()),
            policy(4, 4),
        ),
        Err(ResourceError::OperationExpired)
    );
    assert!(document.claims.is_empty());
    assert_eq!(document.tombstone(&old), None);
}

#[test]
fn a_self_contradicting_document_is_refused_rather_than_repaired() {
    let owner = DaemonGeneration::new();
    let (valid, operation, _) = reserved(ResourceKind::Terminal, 4);
    let resource = valid.operation(&operation).unwrap().resource.clone();

    let mut wrong_schema = valid.clone();
    wrong_schema.schema = "other".to_owned();
    assert_eq!(
        wrong_schema.validate(),
        Err(ResourceError::UnknownSchema),
        "{ALLOCATOR_SCHEMA} is the only schema this build acts on"
    );

    let mut duplicate_claim = valid.clone();
    duplicate_claim
        .claims
        .push(valid.claim(&resource).unwrap().clone());
    assert_eq!(duplicate_claim.validate(), Err(ResourceError::Corrupt));

    let mut orphan_claim = valid.clone();
    orphan_claim.operations.clear();
    assert_eq!(orphan_claim.validate(), Err(ResourceError::Corrupt));

    let mut foreign_claim = valid.clone();
    foreign_claim.claims[0].owner = DaemonGeneration::new();
    assert_eq!(foreign_claim.validate(), Err(ResourceError::Corrupt));

    let mut duplicate_operation = valid.clone();
    duplicate_operation
        .operations
        .push(valid.operation(&operation).unwrap().clone());
    assert_eq!(duplicate_operation.validate(), Err(ResourceError::Corrupt));

    let mut foreign_operation = valid.clone();
    foreign_operation.operations[0].owner = DaemonGeneration::new();
    assert_eq!(foreign_operation.validate(), Err(ResourceError::Corrupt));

    let mut unsealed_final = valid.clone();
    unsealed_final.operations[0].outcome = OperationOutcome::Spawned;
    assert_eq!(unsealed_final.validate(), Err(ResourceError::Corrupt));

    let mut below_watermark = valid.clone();
    below_watermark.watermark = Some(operation);
    assert_eq!(below_watermark.validate(), Err(ResourceError::Corrupt));

    let mut tombstoned_twice = valid.clone();
    tombstoned_twice.tombstones.push(OperationTombstone {
        operation,
        digest: "digest".to_owned(),
        class: ExpiryClass::Spawned,
        cutoff: 0,
    });
    assert_eq!(tombstoned_twice.validate(), Err(ResourceError::Corrupt));

    let mut orphan_consumed = valid.clone();
    orphan_consumed.consumed.push(super::ConsumedEvent {
        resource: terminal(owner),
        owner,
        event_revision: 1,
    });
    assert_eq!(orphan_consumed.validate(), Err(ResourceError::Corrupt));

    valid.validate().unwrap();
}

#[test]
fn a_bound_allocator_exposes_its_policy_and_swaps_through_its_store() {
    let bytes = SharedBytes::default();
    let allocator = allocator(&bytes, policy(1, 2));
    assert_eq!(allocator.policy().limit(ResourceKind::Terminal), 2);
    let owner = DaemonGeneration::new();
    let operation = OperationId::new();
    let resource = terminal(owner);
    let policy = allocator.policy();
    let (admission, snapshot) = allocator
        .update(|document| {
            document.reserve(
                &operation,
                "digest",
                ResourceKind::Terminal,
                owner,
                &resource,
                policy,
            )
        })
        .unwrap();
    assert_eq!(admission, Admission::Fresh);
    assert_eq!(snapshot.document().revision, 1);
    assert_eq!(
        allocator
            .load()
            .unwrap()
            .document()
            .pool_used(ResourceKind::Terminal),
        1
    );
    assert_eq!(
        allocator
            .store()
            .load(AllocatorDocument::default)
            .unwrap()
            .document()
            .claims
            .len(),
        1
    );
}

/// The regression this whole module exists for: a draining owner's exit and a new
/// active owner's spawn hit the allocator at the same time. Both transitions must
/// survive — with a whole-snapshot store the later rename erased one of them.
#[test]
fn a_concurrent_exit_and_spawn_both_survive_the_shared_document() {
    let bytes = SharedBytes::default();
    let policy = policy(2, 2);
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let exiting = OperationId::new();
    let exiting_resource = terminal(old);

    // The draining generation already owns a live terminal.
    let setup = allocator(&bytes, policy);
    setup
        .update(|document| {
            document.reserve(
                &exiting,
                "digest",
                ResourceKind::Terminal,
                old,
                &exiting_resource,
                policy,
            )?;
            document.mark_spawned(&exiting, 1)
        })
        .unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for writer in 0..2u8 {
        let bytes = bytes.clone();
        let barrier = Arc::clone(&barrier);
        let exiting = exiting_resource.clone();
        let spawning = terminal(new);
        let spawn_operation = OperationId::new();
        handles.push(std::thread::spawn(move || {
            let allocator = allocator(&bytes, policy);
            barrier.wait();
            // Both writers retry until their own transition is durable, which is
            // what a compare-and-swap makes possible and a whole-save does not.
            for _ in 0..64 {
                let result = if writer == 0 {
                    allocator
                        .update(|document| document.consume_exit(old, &exiting, 7).map(|_| ()))
                        .map(|_| ())
                } else {
                    allocator
                        .update(|document| {
                            document
                                .reserve(
                                    &spawn_operation,
                                    "digest",
                                    ResourceKind::Terminal,
                                    new,
                                    &spawning,
                                    policy,
                                )
                                .map(|_| ())
                        })
                        .map(|_| ())
                };
                if result.is_ok() {
                    return;
                }
            }
            panic!("a compare-and-swap writer never converged");
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let document = allocator(&bytes, policy).load().unwrap().to_document();
    assert_eq!(
        document.claim(&exiting_resource).unwrap().state,
        ClaimState::Released,
        "the draining owner's exit was not lost"
    );
    assert_eq!(
        document
            .claims
            .iter()
            .filter(|claim| claim.owner == new && claim.state == ClaimState::Reserved)
            .count(),
        1,
        "the new owner's spawn reservation was not lost"
    );
    assert_eq!(document.pool_used(ResourceKind::Terminal), 1);
    document.validate().unwrap();
    assert_eq!(
        document.claim(&terminal(new)).map(|claim| claim.revision),
        None
    );
    assert!(TerminalId::new().as_str().len() > 8);
}

#[test]
fn a_dead_owners_gone_child_gives_its_capacity_back_exactly_once() {
    let (mut document, operation, owner) = reserved(ResourceKind::Terminal, 2);
    let resource = document.claims[0].resource.clone();
    document.mark_spawned(&operation, 1).unwrap();
    assert_eq!(document.pool_used(ResourceKind::Terminal), 1);

    // Another generation may not release a claim it does not own, and a resource
    // nothing holds capacity for is refused rather than invented.
    assert_eq!(
        document.release_gone(DaemonGeneration::new(), &resource),
        Err(ResourceError::WrongOwner)
    );
    assert_eq!(
        document.release_gone(owner, &terminal(owner)),
        Err(ResourceError::UnknownResource)
    );
    assert_eq!(document.pool_used(ResourceKind::Terminal), 1);

    document.release_gone(owner, &resource).unwrap();
    let revision = document.claim(&resource).unwrap().revision;
    assert_eq!(
        document.claim(&resource).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(document.pool_used(ResourceKind::Terminal), 0);

    // Repeating it releases nothing a second time.
    document.release_gone(owner, &resource).unwrap();
    assert_eq!(document.claim(&resource).unwrap().revision, revision);
    document.validate().unwrap();
}
