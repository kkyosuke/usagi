//! The draining owner publishes; the active consumer applies; nobody writes the
//! other's document.

use usagi_core::domain::id::{DaemonGeneration, OperationId, TerminalRef};

use super::{ActiveConsumer, ConsumeReport, publish_exit, reclaim_outbox};
use crate::usecase::resources::allocator::{ClaimState, ResourceAllocator, ResourceKind};
use crate::usecase::resources::fixture::{
    FileFault, MemoryFile, SharedBytes, allocator, policy, shard as bind_shard, terminal, verified,
};
use crate::usecase::resources::shard::{OwnerShard, ShardDocument};
use crate::usecase::resources::{CasFile, ResourceError};

struct Drained {
    allocator_bytes: SharedBytes,
    shard_bytes: SharedBytes,
    owner: DaemonGeneration,
    resource: TerminalRef,
}

impl Drained {
    /// A draining owner with one live terminal that has already been claimed and
    /// spawned in the allocator.
    fn new() -> Self {
        let owner = DaemonGeneration::new();
        let world = Self {
            allocator_bytes: SharedBytes::default(),
            shard_bytes: SharedBytes::default(),
            owner,
            resource: terminal(owner),
        };
        let operation = OperationId::new();
        let policy = policy(2, 2);
        world
            .allocator()
            .update(|document| {
                document.reserve(
                    &operation,
                    "digest",
                    ResourceKind::Terminal,
                    owner,
                    &world.resource,
                    policy,
                )?;
                document.mark_spawned(&operation, 1)
            })
            .unwrap();
        world
            .shard()
            .update(|document| {
                document.reserve(
                    &operation,
                    "digest",
                    ResourceKind::Terminal,
                    &world.resource,
                )?;
                document.record_spawn(&world.resource, &verified(91, "os:91"))
            })
            .unwrap();
        world
    }

    fn allocator(&self) -> ResourceAllocator {
        allocator(&self.allocator_bytes, policy(2, 2))
    }

    fn shard(&self) -> OwnerShard {
        bind_shard(&self.shard_bytes, self.owner)
    }

    fn owner_shard(&self) -> ShardDocument {
        self.shard().load().unwrap().to_document()
    }
}

#[test]
fn one_exit_releases_capacity_once_across_redelivery_and_consumer_restarts() {
    let world = Drained::new();
    publish_exit(&world.shard(), &world.resource, 7).unwrap();
    let published = world.owner_shard();
    assert_eq!(published.unacked_outbox(), 1);

    let allocator = world.allocator();
    let consumer = ActiveConsumer::new(&allocator);
    let first = consumer.consume(&published).unwrap();
    assert_eq!(
        first,
        ConsumeReport {
            applied: 1,
            duplicates: 0,
            refused: 0
        }
    );
    let ledger = allocator.load().unwrap().to_document();
    assert_eq!(
        ledger.claim(&world.resource).unwrap().state,
        ClaimState::Released
    );
    let released_revision = ledger.claim(&world.resource).unwrap().revision;

    // A consumer that lost its acknowledgement, restarted, or re-read the same
    // shard applies nothing a second time.
    for _ in 0..2 {
        let repeat = ActiveConsumer::new(&allocator).consume(&published).unwrap();
        assert_eq!(repeat.applied, 0);
        assert_eq!(repeat.duplicates, 1);
    }
    let ledger = allocator.load().unwrap().to_document();
    assert_eq!(
        ledger.claim(&world.resource).unwrap().revision,
        released_revision
    );
}

#[test]
fn events_are_applied_in_owner_order_however_the_outbox_is_read() {
    let world = Drained::new();
    world
        .shard()
        .update(|document| {
            document.commit_output(&world.resource, 4)?;
            document.commit_exit(&world.resource, 0)
        })
        .unwrap();
    let mut reordered = world.owner_shard();
    reordered.outbox.reverse();

    let allocator = world.allocator();
    let report = ActiveConsumer::new(&allocator).consume(&reordered).unwrap();
    assert_eq!(report.applied, 2);
    let ledger = allocator.load().unwrap().to_document();
    assert_eq!(ledger.consumed_revision(&world.resource), Some(2));
    assert_eq!(
        ledger.claim(&world.resource).unwrap().state,
        ClaimState::Released
    );
}

#[test]
fn the_active_generation_never_writes_the_old_shard() {
    let world = Drained::new();
    publish_exit(&world.shard(), &world.resource, 0).unwrap();
    let published = world.owner_shard();
    let before = world.shard_bytes.get().unwrap();

    let allocator = world.allocator();
    ActiveConsumer::new(&allocator).consume(&published).unwrap();
    assert_eq!(
        world.shard_bytes.get().unwrap(),
        before,
        "an acknowledgement is the allocator's consumed revision, not a write"
    );

    // Only the owner reclaims, and only what the allocator records as consumed.
    let reclaimed = reclaim_outbox(&world.shard(), &allocator).unwrap();
    assert_eq!(reclaimed, 1);
    assert_eq!(world.owner_shard().unacked_outbox(), 0);
    assert_ne!(world.shard_bytes.get().unwrap(), before);
    assert_eq!(reclaim_outbox(&world.shard(), &allocator).unwrap(), 0);
}

#[test]
fn an_event_about_another_owners_resource_is_refused_and_changes_nothing() {
    let world = Drained::new();
    publish_exit(&world.shard(), &world.resource, 0).unwrap();
    let mut forged = world.owner_shard();
    forged.owner = DaemonGeneration::new();

    let allocator = world.allocator();
    let before = world.allocator_bytes.get().unwrap();
    let report = ActiveConsumer::new(&allocator).consume(&forged).unwrap();
    assert_eq!(
        report,
        ConsumeReport {
            applied: 0,
            duplicates: 0,
            refused: 1
        }
    );
    assert_eq!(world.allocator_bytes.get().unwrap(), before);

    // An event for a resource with no claim at all is refused the same way.
    let mut unknown = world.owner_shard();
    unknown.outbox[0].resource = terminal(world.owner);
    let report = ActiveConsumer::new(&allocator).consume(&unknown).unwrap();
    assert_eq!(report.refused, 1);
    assert_eq!(
        allocator
            .load()
            .unwrap()
            .document()
            .claim(&world.resource)
            .unwrap()
            .state,
        ClaimState::Live,
        "another resource's event never touches this claim"
    );
}

#[test]
fn a_store_failure_stops_the_pass_without_a_partial_acknowledgement() {
    let world = Drained::new();
    publish_exit(&world.shard(), &world.resource, 0).unwrap();
    let published = world.owner_shard();

    let broken = ResourceAllocator::new(
        MemoryFile::faulty(&world.allocator_bytes, FileFault::WriteFails),
        policy(2, 2),
    );
    let failure = ActiveConsumer::new(&broken)
        .consume(&published)
        .unwrap_err();
    assert!(failure.refusal().is_none());

    let unreadable = ResourceAllocator::new(
        MemoryFile::faulty(&world.allocator_bytes, FileFault::ReadFails),
        policy(2, 2),
    );
    assert!(
        reclaim_outbox(&world.shard(), &unreadable)
            .unwrap_err()
            .refusal()
            .is_none()
    );

    assert_eq!(
        publish_exit(&world.shard(), &terminal(world.owner), 0)
            .unwrap_err()
            .refusal(),
        Some(ResourceError::UnknownResource)
    );
    assert!(
        MemoryFile::new(&world.shard_bytes)
            .read()
            .unwrap()
            .is_some()
    );
}
