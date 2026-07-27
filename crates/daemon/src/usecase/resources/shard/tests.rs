//! One writer per shard: reservation, child identity, outbox, and collection.

use usagi_core::domain::id::{DaemonGeneration, OperationId, TerminalRef};

use super::{
    CollectionBlocker, OwnerEvent, ResourceState, SHARD_SCHEMA, ShardDocument, collectable,
    hydrate, open_writer, retired_collectable,
};
use crate::usecase::resources::allocator::{AllocatorDocument, ClaimState, ResourceKind};
use crate::usecase::resources::fixture::{
    SharedBytes, policy, shard as bind_shard, terminal, verified,
};
use crate::usecase::resources::identity::ChildIdentity;
use crate::usecase::resources::{CasDocument, ResourceError};

fn reserved(owner: DaemonGeneration) -> (ShardDocument, OperationId, TerminalRef) {
    let mut document = ShardDocument::empty(owner);
    let operation = OperationId::new();
    let resource = terminal(owner);
    document
        .reserve(&operation, "digest", ResourceKind::Terminal, &resource)
        .unwrap();
    (document, operation, resource)
}

fn running(owner: DaemonGeneration) -> (ShardDocument, OperationId, TerminalRef, ChildIdentity) {
    let (mut document, operation, resource) = reserved(owner);
    let identity = verified(41, "os:41");
    document.record_spawn(&resource, &identity).unwrap();
    (document, operation, resource, identity)
}

#[test]
fn a_reservation_is_idempotent_and_a_contradicting_one_is_refused() {
    let owner = DaemonGeneration::new();
    let (mut document, operation, resource) = reserved(owner);
    assert_eq!(document.live_resources(), 1);
    assert!(ResourceState::Reserved.is_live());
    assert!(ResourceState::Running.is_live());
    assert!(!ResourceState::OwnershipUnknown.is_live());
    assert!(!ResourceState::Exited { status: Some(0) }.is_live());

    document
        .reserve(&operation, "digest", ResourceKind::Terminal, &resource)
        .unwrap();
    assert_eq!(document.resources.len(), 1);

    assert_eq!(
        document.reserve(&operation, "other", ResourceKind::Terminal, &resource),
        Err(ResourceError::DuplicateResource)
    );
    assert_eq!(
        document.reserve(
            &OperationId::new(),
            "digest",
            ResourceKind::Terminal,
            &terminal(DaemonGeneration::new()),
        ),
        Err(ResourceError::ForeignOwner),
        "a shard only ever writes its own generation's resources"
    );
    document.validate().unwrap();
}

#[test]
fn a_child_is_recorded_only_with_an_identity_that_can_be_re_observed() {
    let owner = DaemonGeneration::new();
    let (mut document, _, resource) = reserved(owner);
    let identity = verified(51, "os:51");

    assert_eq!(
        document.record_spawn(&resource, &ChildIdentity::unverifiable(51, "start")),
        Err(ResourceError::IdentityUnverifiable)
    );
    assert_eq!(
        document.resource(&resource).unwrap().state,
        ResourceState::Reserved
    );

    document.record_spawn(&resource, &identity).unwrap();
    assert_eq!(
        document.resource(&resource).unwrap().state,
        ResourceState::Running
    );
    let revision = document.resource(&resource).unwrap().revision;
    document.record_spawn(&resource, &identity).unwrap();
    assert_eq!(document.resource(&resource).unwrap().revision, revision);

    assert_eq!(
        document.record_spawn(&resource, &verified(52, "os:52")),
        Err(ResourceError::WrongState),
        "a record never silently adopts a second child"
    );
    assert_eq!(
        document.record_spawn(&terminal(owner), &identity),
        Err(ResourceError::UnknownResource)
    );
    document.validate().unwrap();
}

#[test]
fn an_unprovable_record_stays_visible_and_holds_no_child() {
    let owner = DaemonGeneration::new();
    let (mut document, _, resource, _) = running(owner);
    document.mark_ownership_unknown(&resource).unwrap();
    let revision = document.resource(&resource).unwrap().revision;
    document.mark_ownership_unknown(&resource).unwrap();
    assert_eq!(document.resource(&resource).unwrap().revision, revision);
    assert_eq!(document.live_resources(), 0);
    assert_eq!(
        document.mark_ownership_unknown(&terminal(owner)),
        Err(ResourceError::UnknownResource)
    );
    assert_eq!(
        document.commit_exit(&resource, Some(0)),
        Err(ResourceError::WrongState),
        "an unprovable record never produces an exit"
    );
}

#[test]
fn commands_are_tracked_until_they_complete_and_publish_once() {
    let owner = DaemonGeneration::new();
    let (mut document, _, resource, _) = running(owner);
    let command = OperationId::new();

    assert_eq!(
        document.accept_command(&terminal(owner), &command),
        Err(ResourceError::UnknownResource)
    );
    document.accept_command(&resource, &command).unwrap();
    document.accept_command(&resource, &command).unwrap();
    assert_eq!(document.in_flight.len(), 1);
    document
        .validate()
        .expect("an accepted command for an owned resource is well formed");

    document
        .commit_command_completion(&resource, &command)
        .unwrap();
    assert!(document.in_flight.is_empty());
    assert_eq!(document.unacked_outbox(), 1);
    // The completion is already published, so repeating it converges instead of
    // publishing a second event.
    document
        .commit_command_completion(&resource, &command)
        .unwrap();
    assert_eq!(document.unacked_outbox(), 1);

    assert_eq!(
        document.commit_command_completion(&resource, &OperationId::new()),
        Err(ResourceError::UnknownOperation)
    );
    assert_eq!(
        document.commit_command_completion(&terminal(owner), &command),
        Err(ResourceError::UnknownResource)
    );

    let (mut reserved_only, _, pending) = reserved(owner);
    assert_eq!(
        reserved_only.accept_command(&pending, &command),
        Err(ResourceError::WrongState),
        "a command needs a running child"
    );
    document.validate().unwrap();
}

#[test]
fn output_progress_publishes_one_event_per_offset() {
    let owner = DaemonGeneration::new();
    let (mut document, _, resource, _) = running(owner);
    document.commit_output(&resource, 10).unwrap();
    document.commit_output(&resource, 10).unwrap();
    assert_eq!(document.unacked_outbox(), 1);
    document.commit_output(&resource, 20).unwrap();
    assert_eq!(document.unacked_outbox(), 2);
    assert_eq!(
        document.commit_output(&terminal(owner), 1),
        Err(ResourceError::UnknownResource)
    );
    assert!(!OwnerEvent::Output { offset: 10 }.is_terminal());
    assert!(
        OwnerEvent::Exit { status: Some(0) }.is_terminal(),
        "only an exit releases capacity"
    );
    assert!(
        !OwnerEvent::CommandCompleted {
            command: OperationId::new()
        }
        .is_terminal()
    );
    document.validate().unwrap();
}

#[test]
fn an_exit_records_and_publishes_in_one_transition() {
    let owner = DaemonGeneration::new();
    let (mut document, _, resource, _) = running(owner);
    document
        .accept_command(&resource, &OperationId::new())
        .unwrap();

    document.commit_exit(&resource, Some(3)).unwrap();
    assert_eq!(
        document.resource(&resource).unwrap().state,
        ResourceState::Exited { status: Some(3) }
    );
    assert_eq!(document.unacked_outbox(), 1);
    assert!(
        document.in_flight.is_empty(),
        "an exit ends this owner's in-flight commands"
    );

    document.commit_exit(&resource, Some(3)).unwrap();
    assert_eq!(document.unacked_outbox(), 1, "an exit publishes once");
    assert_eq!(
        document.commit_exit(&resource, Some(9)),
        Err(ResourceError::WrongState)
    );
    assert_eq!(
        document.commit_exit(&terminal(owner), Some(0)),
        Err(ResourceError::UnknownResource)
    );
    document.validate().unwrap();
}

#[test]
fn the_owner_reclaims_only_what_the_consumer_has_applied() {
    let owner = DaemonGeneration::new();
    let (mut document, operation, resource, _) = running(owner);
    document.commit_output(&resource, 5).unwrap();
    document.commit_exit(&resource, Some(0)).unwrap();
    assert_eq!(document.unacked_outbox(), 2);

    let mut allocator = AllocatorDocument::default();
    allocator
        .reserve(
            &operation,
            "digest",
            ResourceKind::Terminal,
            owner,
            &resource,
            policy(4, 4),
        )
        .unwrap();
    allocator.mark_spawned(&operation, 1).unwrap();

    assert_eq!(document.reclaim(&allocator), 0, "nothing applied yet");
    allocator.consume_progress(owner, &resource, 1).unwrap();
    assert_eq!(document.reclaim(&allocator), 1);
    assert_eq!(document.unacked_outbox(), 1);
    assert!(
        document.resource(&resource).is_some(),
        "the exited record stays while its exit is unapplied"
    );

    allocator.consume_exit(owner, &resource, 2).unwrap();
    assert_eq!(document.reclaim(&allocator), 1);
    assert_eq!(document.unacked_outbox(), 0);
    assert!(document.resource(&resource).is_none());
    document.validate().unwrap();
}

#[test]
fn a_record_the_owner_stops_retaining_is_dropped_unless_it_holds_a_child() {
    let owner = DaemonGeneration::new();
    let (mut document, _, resource, _) = running(owner);
    let absent = terminal(owner);

    // Nothing to forget converges silently, so a repeated pass writes nothing.
    document.forget(&absent).unwrap();
    // A live record is never forgotten: that would hide a child nothing reaps.
    assert_eq!(document.forget(&resource), Err(ResourceError::WrongState));

    document.commit_exit(&resource, None).unwrap();
    assert_eq!(document.unacked_outbox(), 1);
    document.forget(&resource).unwrap();
    assert!(document.resource(&resource).is_none());
    // Its published events go with it: nothing can apply an event for a record
    // that no longer exists.
    assert_eq!(document.unacked_outbox(), 0);
    document.validate().unwrap();
}

#[test]
fn a_retired_owners_outbox_is_measured_by_what_the_allocator_applied() {
    let owner = DaemonGeneration::new();
    let (mut document, operation, resource, _) = running(owner);
    document.commit_exit(&resource, Some(0)).unwrap();
    let mut allocator = AllocatorDocument::default();

    // No claim at all: the event has no addressee, so it cannot keep this
    // generation alive forever.
    assert_eq!(document.unconsumed_outbox(&allocator), 0);
    assert_eq!(retired_collectable(&document, &allocator), Ok(()));

    allocator
        .reserve(
            &operation,
            "digest",
            ResourceKind::Terminal,
            owner,
            &resource,
            policy(4, 4),
        )
        .unwrap();
    allocator.mark_spawned(&operation, 1).unwrap();
    assert_eq!(document.unconsumed_outbox(&allocator), 1);
    assert_eq!(
        retired_collectable(&document, &allocator),
        Err(CollectionBlocker::UnackedOutbox)
    );

    allocator.consume_exit(owner, &resource, 1).unwrap();
    assert_eq!(document.unconsumed_outbox(&allocator), 0);
    // The dead owner never swept its own outbox, and that no longer blocks it.
    assert!(document.unacked_outbox() > 0);
    assert_eq!(retired_collectable(&document, &allocator), Ok(()));
}

#[test]
fn a_generation_is_collectable_only_when_every_count_is_zero() {
    let owner = DaemonGeneration::new();
    let (mut document, operation, resource, _) = running(owner);
    let mut allocator = AllocatorDocument::default();
    allocator
        .reserve(
            &operation,
            "digest",
            ResourceKind::Terminal,
            owner,
            &resource,
            policy(4, 4),
        )
        .unwrap();
    allocator.mark_spawned(&operation, 1).unwrap();

    assert_eq!(
        collectable(&document, &allocator),
        Err(CollectionBlocker::LiveResource)
    );

    // A command accepted while the child was running, whose record then became
    // unprovable, is still this owner's responsibility.
    let command = OperationId::new();
    document.accept_command(&resource, &command).unwrap();
    document.mark_ownership_unknown(&resource).unwrap();
    assert_eq!(
        collectable(&document, &allocator),
        Err(CollectionBlocker::InFlightCommand)
    );

    document
        .commit_command_completion(&resource, &command)
        .unwrap();
    assert_eq!(
        collectable(&document, &allocator),
        Err(CollectionBlocker::UnackedOutbox)
    );

    allocator.consume_progress(owner, &resource, 1).unwrap();
    document.reclaim(&allocator);
    assert_eq!(document.unacked_outbox(), 0);
    assert_eq!(
        collectable(&document, &allocator),
        Err(CollectionBlocker::CapacityClaim),
        "an unprovable record keeps its capacity: it is never guessed away"
    );

    let mut released = allocator.clone();
    released
        .claims
        .iter_mut()
        .for_each(|claim| claim.state = ClaimState::Released);
    let mut drained = document.clone();
    drained.resources.clear();
    assert_eq!(collectable(&drained, &released), Ok(()));
}

#[test]
fn a_self_contradicting_shard_is_refused_rather_than_repaired() {
    let owner = DaemonGeneration::new();
    let (valid, _, resource, identity) = running(owner);

    let mut wrong_schema = valid.clone();
    wrong_schema.schema = "other".to_owned();
    assert_eq!(wrong_schema.validate(), Err(ResourceError::UnknownSchema));
    assert_eq!(SHARD_SCHEMA, "usagi-owner-shard-v1");

    let mut duplicate = valid.clone();
    duplicate
        .resources
        .push(valid.resource(&resource).unwrap().clone());
    assert_eq!(duplicate.validate(), Err(ResourceError::Corrupt));

    let mut foreign = valid.clone();
    foreign.owner = DaemonGeneration::new();
    assert_eq!(foreign.validate(), Err(ResourceError::Corrupt));

    let mut unverifiable_running = valid.clone();
    unverifiable_running.resources[0].process =
        Some(ChildIdentity::unverifiable(identity.pid, "start"));
    assert_eq!(unverifiable_running.validate(), Err(ResourceError::Corrupt));

    let mut reserved_with_child = valid.clone();
    reserved_with_child.resources[0].state = ResourceState::Reserved;
    assert_eq!(reserved_with_child.validate(), Err(ResourceError::Corrupt));

    let mut outbox_ahead = valid.clone();
    outbox_ahead.outbox.push(super::OutboxEvent {
        event_revision: 9,
        resource: resource.clone(),
        event: OwnerEvent::Exit { status: Some(0) },
    });
    assert_eq!(outbox_ahead.validate(), Err(ResourceError::Corrupt));

    let mut duplicate_revision = valid.clone();
    duplicate_revision.event_sequence = 1;
    for _ in 0..2 {
        duplicate_revision.outbox.push(super::OutboxEvent {
            event_revision: 1,
            resource: resource.clone(),
            event: OwnerEvent::Exit { status: Some(0) },
        });
    }
    assert_eq!(duplicate_revision.validate(), Err(ResourceError::Corrupt));

    let mut unknown_outbox_resource = valid.clone();
    unknown_outbox_resource.event_sequence = 1;
    unknown_outbox_resource.outbox.push(super::OutboxEvent {
        event_revision: 1,
        resource: terminal(owner),
        event: OwnerEvent::Exit { status: Some(0) },
    });
    assert_eq!(
        unknown_outbox_resource.validate(),
        Err(ResourceError::Corrupt)
    );

    let mut unknown_command_resource = valid.clone();
    unknown_command_resource
        .in_flight
        .push(super::InFlightCommand {
            resource: terminal(owner),
            command: OperationId::new(),
        });
    assert_eq!(
        unknown_command_resource.validate(),
        Err(ResourceError::Corrupt)
    );

    valid.validate().unwrap();
}

#[test]
fn a_standby_hydrates_read_only_and_only_activation_opens_a_writer() {
    let owner = DaemonGeneration::new();
    let shard_bytes = SharedBytes::default();
    let allocator_bytes = SharedBytes::default();
    let shard = bind_shard(&shard_bytes, owner);
    let allocator = crate::usecase::resources::fixture::allocator(&allocator_bytes, policy(2, 2));

    let operation = OperationId::new();
    let resource = terminal(owner);
    shard
        .update(|document| {
            document.reserve(&operation, "digest", ResourceKind::Terminal, &resource)
        })
        .unwrap();
    let stored = shard_bytes.get().unwrap();
    let ledger = allocator.load().unwrap().to_document();

    let sealed = hydrate(&shard, &ledger).unwrap();
    assert_eq!(sealed.owner(), owner);
    assert_eq!(sealed.shard_revision(), 1);
    assert_eq!(sealed.allocator_revision(), 0);
    assert_eq!(sealed.resources(), 1);
    assert_eq!(
        shard_bytes.get().unwrap(),
        stored,
        "hydrate and readiness write nothing at all"
    );

    let lease = open_writer(&shard, &ledger, &sealed).unwrap();
    assert_eq!(lease.owner(), owner);
    assert_eq!(lease.shard_revision(), 1);
    assert_eq!(lease.allocator_revision(), 0);

    // Either object moving under the seal invalidates it.
    shard
        .update(|document| document.record_spawn(&resource, &verified(71, "os:71")))
        .unwrap();
    assert_eq!(
        open_writer(&shard, &ledger, &sealed).unwrap_err().refusal(),
        Some(ResourceError::SealedElsewhere)
    );
    let moved_ledger = AllocatorDocument {
        revision: 5,
        ..ledger.clone()
    };
    let fresh = hydrate(&shard, &ledger).unwrap();
    assert_eq!(
        open_writer(&shard, &moved_ledger, &fresh)
            .unwrap_err()
            .refusal(),
        Some(ResourceError::SealedElsewhere)
    );
}

#[test]
fn a_shard_document_belonging_to_another_generation_is_never_written() {
    let owner = DaemonGeneration::new();
    let bytes = SharedBytes::default();
    let foreign = ShardDocument::empty(DaemonGeneration::new());
    bytes.set(&serde_json::to_string(&foreign).unwrap());

    let shard = bind_shard(&bytes, owner);
    assert_eq!(
        shard
            .update(|document| document.reserve(
                &OperationId::new(),
                "digest",
                ResourceKind::Terminal,
                &terminal(owner),
            ))
            .unwrap_err()
            .refusal(),
        Some(ResourceError::ForeignOwner)
    );
    assert_eq!(
        hydrate(&shard, &AllocatorDocument::default())
            .unwrap_err()
            .refusal(),
        Some(ResourceError::ForeignOwner)
    );
    assert_eq!(
        bytes.get().unwrap(),
        serde_json::to_string(&foreign).unwrap()
    );
    assert_eq!(shard.owner(), owner);
}
