//! Every crash boundary of claim → reserve → spawn → final, and the replay that
//! keeps the spawn count at one.

use usagi_core::domain::id::{DaemonGeneration, OperationId, TerminalRef};

use super::{LaunchIntent, LaunchStep, LeakReason, SpawnRefusal, execute_launch, plan_launch};
use crate::usecase::resources::allocator::{
    Admission, AllocatorDocument, ClaimState, LaunchFailure, OperationOutcome, ResourceAllocator,
    ResourceKind,
};
use crate::usecase::resources::fixture::{
    FakeClock, FakeProbe, FakeSpawner, MemoryFile, ProbeAnswer, SharedBytes, SpawnPlan, allocator,
    intent, policy, probe_for, shard as bind_shard, terminal, verified, wide_limits,
};
use crate::usecase::resources::identity::{ChildIdentity, ChildObservation};
use crate::usecase::resources::shard::{OwnerShard, ResourceState, ShardDocument};
use crate::usecase::resources::{ResourceError, ResourceFailure};

struct World {
    allocator_bytes: SharedBytes,
    shard_bytes: SharedBytes,
    owner: DaemonGeneration,
    operation: OperationId,
    resource: TerminalRef,
}

impl World {
    fn new() -> Self {
        let owner = DaemonGeneration::new();
        Self {
            allocator_bytes: SharedBytes::default(),
            shard_bytes: SharedBytes::default(),
            owner,
            operation: OperationId::new(),
            resource: terminal(owner),
        }
    }

    fn allocator(&self) -> ResourceAllocator<MemoryFile> {
        allocator(&self.allocator_bytes, policy(2, 2))
    }

    fn shard(&self) -> OwnerShard<MemoryFile> {
        bind_shard(&self.shard_bytes, self.owner)
    }

    fn intent(&self) -> LaunchIntent {
        intent(&self.operation, "digest", &self.resource)
    }

    fn ledger(&self) -> AllocatorDocument {
        self.allocator().load().unwrap().to_document()
    }

    fn owner_shard(&self) -> ShardDocument {
        self.shard().load().unwrap().to_document()
    }

    fn launch(
        &self,
        spawner: &mut FakeSpawner,
        probe: &FakeProbe,
        clock: &FakeClock,
    ) -> Result<super::LaunchAccepted, ResourceFailure> {
        execute_launch(
            &self.allocator(),
            &self.shard(),
            &self.intent(),
            spawner,
            probe,
            clock,
            &wide_limits(),
        )
    }
}

fn spawner(pid: u32, start: &str) -> FakeSpawner {
    FakeSpawner::new(SpawnPlan::Child {
        pid,
        start: start.to_owned(),
    })
}

#[test]
fn a_launch_spawns_once_and_every_later_delivery_replays_the_same_final() {
    let world = World::new();
    let clock = FakeClock::at(5);
    let probe = probe_for(81, "os:81");
    let mut spawn = spawner(81, "os:81");

    let accepted = world.launch(&mut spawn, &probe, &clock).unwrap();
    assert_eq!(accepted.operation, world.operation);
    assert_eq!(accepted.resource, world.resource);
    assert_eq!(accepted.outcome, OperationOutcome::Spawned);
    assert!(accepted.spawned);
    assert_eq!(spawn.spawns, 1);

    // Response loss, reconnect, a same-process duplicate, and a restart hydrate
    // are all the same thing to the ledger: the identical operation.
    for _ in 0..3 {
        let replay = world.launch(&mut spawn, &probe, &clock).unwrap();
        assert_eq!(replay.resource, accepted.resource);
        assert_eq!(replay.outcome, accepted.outcome);
        assert_eq!(replay.revision, accepted.revision);
        assert!(!replay.spawned);
    }
    assert_eq!(spawn.spawns, 1, "the child is spawned at most once");

    let ledger = world.ledger();
    assert_eq!(
        ledger.claim(&world.resource).unwrap().state,
        ClaimState::Live
    );
    assert_eq!(ledger.pool_used(ResourceKind::Terminal), 1);
    assert_eq!(
        world.owner_shard().resource(&world.resource).unwrap().state,
        ResourceState::Running
    );
}

#[test]
fn the_same_operation_with_another_intent_conflicts_without_touching_anything() {
    let world = World::new();
    let clock = FakeClock::at(1);
    let probe = probe_for(82, "os:82");
    let mut spawn = spawner(82, "os:82");
    world.launch(&mut spawn, &probe, &clock).unwrap();
    let before = (world.allocator_bytes.get(), world.shard_bytes.get());

    let conflicting = LaunchIntent {
        digest: "other-geometry".to_owned(),
        ..world.intent()
    };
    let failure = execute_launch(
        &world.allocator(),
        &world.shard(),
        &conflicting,
        &mut spawn,
        &probe,
        &clock,
        &wide_limits(),
    )
    .unwrap_err();
    assert_eq!(failure.refusal(), Some(ResourceError::OperationConflict));
    assert_eq!(spawn.spawns, 1);
    assert_eq!(
        (world.allocator_bytes.get(), world.shard_bytes.get()),
        before,
        "a conflict is effect zero"
    );
}

#[test]
fn a_crash_after_the_claim_or_after_the_reservation_still_spawns_once() {
    // Crash between L1 and L2: only the claim is durable.
    let world = World::new();
    let clock = FakeClock::at(2);
    let policy = policy(2, 2);
    world
        .allocator()
        .update(|document| {
            document.reserve(
                &world.operation,
                "digest",
                ResourceKind::Terminal,
                world.owner,
                &world.resource,
                policy,
            )
        })
        .unwrap();
    assert!(world.shard_bytes.get().is_none());

    let probe = probe_for(83, "os:83");
    let mut spawn = spawner(83, "os:83");
    let resumed = world.launch(&mut spawn, &probe, &clock).unwrap();
    assert_eq!(resumed.outcome, OperationOutcome::Spawned);
    assert_eq!(resumed.resource, world.resource);
    assert_eq!(spawn.spawns, 1);

    // Crash between L2 and L3: both sides are durable, still nothing spawned.
    let second = World::new();
    second
        .allocator()
        .update(|document| {
            document.reserve(
                &second.operation,
                "digest",
                ResourceKind::Terminal,
                second.owner,
                &second.resource,
                policy,
            )
        })
        .unwrap();
    second
        .shard()
        .update(|document| {
            document.reserve(
                &second.operation,
                "digest",
                ResourceKind::Terminal,
                &second.resource,
            )
        })
        .unwrap();
    let mut spawn = spawner(84, "os:84");
    let probe = probe_for(84, "os:84");
    let accepted = second.launch(&mut spawn, &probe, &clock).unwrap();
    assert_eq!(accepted.outcome, OperationOutcome::Spawned);
    assert_eq!(spawn.spawns, 1);
}

#[test]
fn a_crash_after_the_spawn_commits_the_final_without_spawning_again() {
    let world = World::new();
    let clock = FakeClock::at(3);
    let probe = probe_for(85, "os:85");
    let policy = policy(2, 2);
    world
        .allocator()
        .update(|document| {
            document.reserve(
                &world.operation,
                "digest",
                ResourceKind::Terminal,
                world.owner,
                &world.resource,
                policy,
            )
        })
        .unwrap();
    world
        .shard()
        .update(|document| {
            document.reserve(
                &world.operation,
                "digest",
                ResourceKind::Terminal,
                &world.resource,
            )?;
            document.record_spawn(&world.resource, &verified(85, "os:85"))
        })
        .unwrap();

    let mut spawn = FakeSpawner::new(SpawnPlan::Definite);
    let accepted = world.launch(&mut spawn, &probe, &clock).unwrap();
    assert_eq!(accepted.outcome, OperationOutcome::Spawned);
    assert!(!accepted.spawned);
    assert_eq!(
        spawn.spawns, 0,
        "the recorded child is adopted, not replaced"
    );
    assert_eq!(
        world.ledger().claim(&world.resource).unwrap().state,
        ClaimState::Live
    );
}

#[test]
fn a_definite_spawn_failure_releases_the_claim_and_an_ambiguous_one_keeps_it() {
    let world = World::new();
    let clock = FakeClock::at(4);
    let probe = FakeProbe::new();
    let mut definite = FakeSpawner::new(SpawnPlan::Definite);
    let failed = world.launch(&mut definite, &probe, &clock).unwrap();
    assert_eq!(
        failed.outcome,
        OperationOutcome::Failed(LaunchFailure::Spawn)
    );
    assert!(!failed.spawned);
    assert_eq!(
        world.ledger().claim(&world.resource).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(world.ledger().pool_used(ResourceKind::Terminal), 0);
    // The definite failure is a durable final too, so a retry replays it.
    let replay = world.launch(&mut definite, &probe, &clock).unwrap();
    assert_eq!(replay.outcome, failed.outcome);
    assert_eq!(replay.revision, failed.revision);
    assert_eq!(definite.spawns, 1);

    let ambiguous_world = World::new();
    let mut ambiguous = FakeSpawner::new(SpawnPlan::Ambiguous);
    let accepted = ambiguous_world
        .launch(&mut ambiguous, &probe, &clock)
        .unwrap();
    assert_eq!(accepted.outcome, OperationOutcome::Ambiguous);
    assert_eq!(
        ambiguous_world
            .ledger()
            .claim(&ambiguous_world.resource)
            .unwrap()
            .state,
        ClaimState::Reserved,
        "a process may exist, so the capacity is not released"
    );
    assert_eq!(
        ambiguous_world
            .owner_shard()
            .resource(&ambiguous_world.resource)
            .unwrap()
            .state,
        ResourceState::OwnershipUnknown
    );
    let replay = ambiguous_world
        .launch(&mut ambiguous, &probe, &clock)
        .unwrap();
    assert_eq!(replay.outcome, OperationOutcome::Ambiguous);
    assert_eq!(ambiguous.spawns, 1);
}

#[test]
fn an_unprovable_record_reaches_its_ambiguous_final_without_a_second_spawn() {
    // Both sides are durable and the shard already holds a record whose ownership
    // cannot be proved — the state a crash between spawn and record leaves.
    let world = World::new();
    let clock = FakeClock::at(9);
    let policy = policy(2, 2);
    world
        .allocator()
        .update(|document| {
            document.reserve(
                &world.operation,
                "digest",
                ResourceKind::Terminal,
                world.owner,
                &world.resource,
                policy,
            )
        })
        .unwrap();
    world
        .shard()
        .update(|document| {
            document.reserve(
                &world.operation,
                "digest",
                ResourceKind::Terminal,
                &world.resource,
            )?;
            document.record_spawn(&world.resource, &verified(95, "os:95"))?;
            document.mark_ownership_unknown(&world.resource)
        })
        .unwrap();

    let probe = FakeProbe::new();
    let mut spawn = spawner(95, "os:95");
    let accepted = world.launch(&mut spawn, &probe, &clock).unwrap();
    assert_eq!(accepted.outcome, OperationOutcome::Ambiguous);
    assert!(!accepted.spawned);
    assert_eq!(spawn.spawns, 0, "a child may exist, so none is started");
    assert_eq!(
        world.ledger().claim(&world.resource).unwrap().state,
        ClaimState::Reserved,
        "the capacity of a possibly-live child is never released"
    );
    let replay = world.launch(&mut spawn, &probe, &clock).unwrap();
    assert_eq!(replay.revision, accepted.revision);
    assert_eq!(spawn.spawns, 0);
}

#[test]
fn a_reservation_without_a_claim_fails_closed_instead_of_spawning() {
    let world = World::new();
    let clock = FakeClock::at(1);
    let probe = FakeProbe::new();
    world
        .shard()
        .update(|document| {
            document.reserve(
                &world.operation,
                "digest",
                ResourceKind::Terminal,
                &world.resource,
            )
        })
        .unwrap();

    let mut spawn = spawner(86, "os:86");
    let failure = world.launch(&mut spawn, &probe, &clock).unwrap_err();
    assert_eq!(failure.refusal(), Some(ResourceError::OwnershipUnknown));
    assert_eq!(spawn.spawns, 0);
    assert!(world.allocator_bytes.get().is_none());
}

#[test]
fn a_full_pool_and_a_full_ledger_both_refuse_before_anything_is_spawned() {
    let world = World::new();
    let clock = FakeClock::at(1);
    let probe = FakeProbe::new();
    let policy = policy(2, 1);
    let allocator = ResourceAllocator::new(MemoryFile::new(&world.allocator_bytes), policy);
    allocator
        .update(|document| {
            document.reserve(
                &OperationId::new(),
                "digest",
                ResourceKind::Terminal,
                world.owner,
                &terminal(world.owner),
                policy,
            )
        })
        .unwrap();

    let mut spawn = spawner(87, "os:87");
    let failure = execute_launch(
        &allocator,
        &world.shard(),
        &world.intent(),
        &mut spawn,
        &probe,
        &clock,
        &wide_limits(),
    )
    .unwrap_err();
    assert_eq!(failure.refusal(), Some(ResourceError::CapacityExhausted));
    assert_eq!(spawn.spawns, 0);

    let tight = crate::usecase::resources::retention::RetentionLimits::new(1, 1 << 20, 100, 50, 50);
    let failure = execute_launch(
        &allocator,
        &world.shard(),
        &world.intent(),
        &mut spawn,
        &probe,
        &clock,
        &tight,
    )
    .unwrap_err();
    assert_eq!(
        failure.refusal(),
        Some(ResourceError::RetentionBackpressure),
        "a full ledger refuses fresh admission rather than evicting a record"
    );
    assert_eq!(spawn.spawns, 0);
}

#[test]
fn a_resumed_launch_keeps_the_resource_its_claim_already_names() {
    let world = World::new();
    let clock = FakeClock::at(1);
    let probe = probe_for(88, "os:88");
    let policy = policy(2, 2);
    world
        .allocator()
        .update(|document| {
            document.reserve(
                &world.operation,
                "digest",
                ResourceKind::Terminal,
                world.owner,
                &world.resource,
                policy,
            )
        })
        .unwrap();

    // The retry proposes a freshly minted resource id; the claim's identity wins.
    let retried = LaunchIntent {
        resource: terminal(world.owner),
        ..world.intent()
    };
    let mut spawn = spawner(88, "os:88");
    let accepted = execute_launch(
        &world.allocator(),
        &world.shard(),
        &retried,
        &mut spawn,
        &probe,
        &clock,
        &wide_limits(),
    )
    .unwrap();
    assert_eq!(accepted.resource, world.resource);
    assert_eq!(spawn.spawns, 1);
    assert_eq!(world.owner_shard().resources.len(), 1);
}

#[test]
#[allow(clippy::too_many_lines)] // The planner's matrix is one table; splitting it hides the shape.
fn the_planner_answers_every_durable_shape_from_evidence_alone() {
    let owner = DaemonGeneration::new();
    let operation = OperationId::new();
    let resource = terminal(owner);
    let plan = intent(&operation, "digest", &resource);
    let policy = policy(2, 2);
    let mut exact = |_: &ChildIdentity| ChildObservation::Exact;

    let empty_allocator = AllocatorDocument::default();
    let empty_shard = ShardDocument::empty(owner);
    assert_eq!(
        plan_launch(&empty_allocator, &empty_shard, &plan, &mut exact).unwrap(),
        LaunchStep::Reserve
    );

    let mut ledger = AllocatorDocument::default();
    ledger
        .reserve(
            &operation,
            "digest",
            ResourceKind::Terminal,
            owner,
            &resource,
            policy,
        )
        .unwrap();
    assert_eq!(
        plan_launch(&ledger, &empty_shard, &plan, &mut exact).unwrap(),
        LaunchStep::Reserve,
        "a claim without a reservation is completed, not abandoned"
    );

    let mut shard = ShardDocument::empty(owner);
    shard
        .reserve(&operation, "digest", ResourceKind::Terminal, &resource)
        .unwrap();
    assert_eq!(
        plan_launch(&ledger, &shard, &plan, &mut exact).unwrap(),
        LaunchStep::Spawn {
            resource: resource.clone()
        }
    );

    let mut foreign_owner = ledger.clone();
    foreign_owner.operations[0].owner = DaemonGeneration::new();
    assert_eq!(
        plan_launch(&foreign_owner, &shard, &plan, &mut exact).unwrap(),
        LaunchStep::Leaked(LeakReason::OwnerMismatch)
    );

    let mut other_intent = shard.clone();
    other_intent.resources[0].digest = "other".to_owned();
    assert_eq!(
        plan_launch(&ledger, &other_intent, &plan, &mut exact).unwrap(),
        LaunchStep::Leaked(LeakReason::IntentMismatch)
    );

    let mut running = shard.clone();
    running
        .record_spawn(&resource, &verified(89, "os:89"))
        .unwrap();
    assert_eq!(
        plan_launch(&ledger, &running, &plan, &mut exact).unwrap(),
        LaunchStep::CommitFinal {
            resource: resource.clone()
        }
    );
    for observation in [ChildObservation::Gone, ChildObservation::Reused] {
        let mut gone = |_: &ChildIdentity| observation;
        assert_eq!(
            plan_launch(&ledger, &running, &plan, &mut gone).unwrap(),
            LaunchStep::CommitFinal {
                resource: resource.clone()
            },
            "a child that is proved gone still proves it was spawned"
        );
    }
    let mut unknown = |_: &ChildIdentity| ChildObservation::Unknown;
    assert_eq!(
        plan_launch(&ledger, &running, &plan, &mut unknown).unwrap(),
        LaunchStep::CommitAmbiguous {
            resource: resource.clone()
        }
    );

    let mut exited = running.clone();
    exited.commit_exit(&resource, 0).unwrap();
    assert_eq!(
        plan_launch(&ledger, &exited, &plan, &mut exact).unwrap(),
        LaunchStep::CommitFinal {
            resource: resource.clone()
        }
    );

    let mut unprovable = running.clone();
    unprovable.mark_ownership_unknown(&resource).unwrap();
    assert_eq!(
        plan_launch(&ledger, &unprovable, &plan, &mut exact).unwrap(),
        LaunchStep::CommitAmbiguous {
            resource: resource.clone()
        }
    );

    let mut final_ledger = ledger.clone();
    final_ledger.mark_spawned(&operation, 1).unwrap();
    let revision = final_ledger.operation(&operation).unwrap().revision;
    assert_eq!(
        plan_launch(&final_ledger, &running, &plan, &mut exact).unwrap(),
        LaunchStep::Replay {
            resource: resource.clone(),
            outcome: OperationOutcome::Spawned,
            revision,
        }
    );

    let expired = AllocatorDocument {
        watermark: Some(operation),
        ..AllocatorDocument::default()
    };
    assert_eq!(
        plan_launch(&expired, &empty_shard, &plan, &mut exact),
        Err(ResourceError::OperationExpired)
    );

    let conflicting = intent(&operation, "other", &resource);
    assert_eq!(
        plan_launch(&ledger, &shard, &conflicting, &mut exact),
        Err(ResourceError::OperationConflict)
    );

    let mut without_claim = ShardDocument::empty(owner);
    without_claim
        .reserve(&operation, "digest", ResourceKind::Terminal, &resource)
        .unwrap();
    assert_eq!(
        plan_launch(&empty_allocator, &without_claim, &plan, &mut exact).unwrap(),
        LaunchStep::Leaked(LeakReason::ReservationWithoutClaim)
    );
}

#[test]
fn a_store_failure_is_reported_as_unavailable_not_as_a_refusal() {
    let world = World::new();
    let clock = FakeClock::at(1);
    let probe = FakeProbe::new().with(
        90,
        ProbeAnswer::Alive {
            start: "os:90".to_owned(),
            group: 90,
        },
    );
    let broken = ResourceAllocator::new(
        crate::usecase::resources::fixture::MemoryFile::faulty(
            &world.allocator_bytes,
            crate::usecase::resources::fixture::FileFault::ReadFails,
        ),
        policy(2, 2),
    );
    let mut spawn = spawner(90, "os:90");
    let failure = execute_launch(
        &broken,
        &world.shard(),
        &world.intent(),
        &mut spawn,
        &probe,
        &clock,
        &wide_limits(),
    )
    .unwrap_err();
    assert!(failure.refusal().is_none());
    assert_eq!(spawn.spawns, 0);
    assert!(matches!(
        SpawnRefusal::Definite,
        SpawnRefusal::Definite | SpawnRefusal::Ambiguous
    ));
    assert_eq!(
        Admission::Fresh,
        Admission::Fresh,
        "the fresh admission marker is part of the reserve contract"
    );
}
