//! What the shipping stores must keep doing once their state lives in shards.

use std::collections::BTreeSet;
use std::path::PathBuf;

use usagi_core::domain::agent::{
    AgentProfileId, DurableLaunchSnapshot, LaunchMode, LaunchPlan, LaunchRequest, LaunchScope,
};
use usagi_core::domain::id::{AgentRuntimeId, AgentRuntimeRef, CompletionFence, OperationId};
use usagi_core::domain::terminal_launch::{
    DurableTerminalLaunchSnapshot, TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};

use super::*;
use crate::usecase::authority::collection::DrainObservation;
use crate::usecase::resources::allocator::{ClaimState, OperationOutcome};
use crate::usecase::resources::fixture::{
    FakeClock, FileFault, MemoryArchive, MemoryFile, ObservedChildren, SharedBytes, policy,
    terminal, verified,
};
use crate::usecase::resources::migration::AdoptionRefusal;
use crate::usecase::resources::shard::{CollectionBlocker, ResourceState};
use crate::usecase::runtime::{DurableOperationOutcome, RuntimeState};
use crate::usecase::terminal::TerminalReconcileState;

/// One data directory: the shared allocator bytes plus the shard archive.
struct World {
    allocator: SharedBytes,
    archive: MemoryArchive,
}

impl World {
    fn new() -> Self {
        Self {
            allocator: SharedBytes::default(),
            archive: MemoryArchive::new(),
        }
    }

    fn with_legacy(agents: Option<&str>, terminals: Option<&str>) -> Self {
        Self {
            allocator: SharedBytes::default(),
            archive: MemoryArchive::with_legacy(agents, terminals),
        }
    }

    /// One process's durable state, with the children it claims to have observed.
    fn state(&self, owner: DaemonGeneration, identity: ObservedChildren) -> ShardedRuntimeState {
        self.role(owner, GenerationRole::Active, identity)
    }

    fn role(
        &self,
        owner: DaemonGeneration,
        role: GenerationRole,
        identity: ObservedChildren,
    ) -> ShardedRuntimeState {
        ShardedRuntimeState::new(
            owner,
            role,
            ResourceAllocator::new(MemoryFile::new(&self.allocator), policy(2, 2)),
            Box::new(self.archive.clone()),
            Box::new(identity),
            Box::new(FakeClock::at(10)),
        )
        .unwrap()
    }

    fn allocator(&self) -> AllocatorDocument {
        self.allocator
            .get()
            .map_or_else(AllocatorDocument::default, |bytes| {
                serde_json::from_str(&bytes).unwrap()
            })
    }

    fn shard(&self, owner: DaemonGeneration) -> ShardDocument {
        self.archive.bytes(owner).get().map_or_else(
            || ShardDocument::empty(owner),
            |bytes| serde_json::from_str(&bytes).unwrap(),
        )
    }
}

fn process(pid: u32, start: &str) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_identity: start.to_owned(),
        process_group: pid,
    }
}

fn fence(resource: &TerminalRef, operation: OperationId) -> CompletionFence {
    CompletionFence {
        workspace_id: resource.workspace_id,
        session_id: resource.session_id,
        operation_id: operation,
        owner_daemon_generation: resource.daemon_generation,
        execution_attempt: 1,
        lifecycle_attempt: 1,
        expected_revision: 1,
    }
}

fn agent_record(
    resource: &TerminalRef,
    operation: OperationId,
    state: RuntimeState,
    process: Option<ProcessIdentity>,
) -> DurableRuntimeRecord {
    let profile = AgentProfileId::new("codex").unwrap();
    let request = LaunchRequest {
        profile_id: profile.clone(),
        mode: LaunchMode::Interactive,
        model: None,
        resume: false,
        provider_resume: None,
        initial_prompt: None,
        scope: LaunchScope {
            workspace_id: resource.workspace_id,
            session_id: resource.session_id,
            worktree_id: resource.worktree_id,
        },
        required_capabilities: BTreeSet::new(),
    };
    let plan = LaunchPlan::new(profile, 1, "codex", Vec::new(), [], PathBuf::from("/tmp")).unwrap();
    DurableRuntimeRecord {
        runtime: AgentRuntimeRef {
            agent_runtime_id: AgentRuntimeId::new(),
            terminal: resource.clone(),
            session_id: resource.session_id,
        },
        operation: fence(resource, operation),
        launch: DurableLaunchSnapshot::new(request, plan),
        state,
        process,
        provider_resume: None,
        continuation: None,
        resume_source: None,
        resumed_from: None,
        superseded_by: None,
        semantic_key: Some("intent".to_owned()),
        outcome: DurableOperationOutcome::Accepted,
        credential_provenance: None,
    }
}

fn terminal_record(
    resource: &TerminalRef,
    operation: OperationId,
    state: TerminalRuntimeState,
    process: Option<ProcessIdentity>,
) -> DurableTerminalRecord {
    let request = TerminalLaunchRequest {
        profile_id: TerminalProfileId::new("login-shell").unwrap(),
        scope: TerminalLaunchScope {
            workspace_id: resource.workspace_id,
            session_id: resource.session_id,
            worktree_id: resource.worktree_id,
        },
    };
    DurableTerminalRecord {
        terminal: resource.clone(),
        operation: fence(resource, operation),
        launch: DurableTerminalLaunchSnapshot::new(
            request,
            1,
            "sh",
            Vec::new(),
            PathBuf::from("/tmp"),
            [],
        )
        .unwrap(),
        state,
        process,
        launch_digest: Some("digest".to_owned()),
    }
}

fn agent_snapshot(records: Vec<DurableRuntimeRecord>) -> RuntimeStoreSnapshot {
    RuntimeStoreSnapshot {
        records,
        ..RuntimeStoreSnapshot::default()
    }
}

fn terminal_snapshot(records: Vec<DurableTerminalRecord>) -> TerminalStoreSnapshot {
    TerminalStoreSnapshot {
        records,
        ..TerminalStoreSnapshot::default()
    }
}

fn agents(records: Vec<DurableRuntimeRecord>) -> String {
    serde_json::to_string(&agent_snapshot(records)).unwrap()
}

fn terminals(records: Vec<DurableTerminalRecord>) -> String {
    serde_json::to_string(&terminal_snapshot(records)).unwrap()
}

#[test]
fn a_projection_is_live_only_when_this_process_observed_the_child() {
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let operation = OperationId::new();
    let observed = ObservedChildren::new().with(7, "start-7");
    let blind = ObservedChildren::new();

    let running = agent_record(
        &resource,
        operation,
        RuntimeState::Running,
        Some(process(7, "start-7")),
    );
    assert_eq!(
        project_agent(&running, &observed).state,
        ProjectedState::Running(verified(7, "start-7"))
    );
    // The same durable record, read by a process that never observed the child:
    // the fixed-token case a legacy store cannot distinguish from a real one.
    assert_eq!(
        project_agent(&running, &blind).state,
        ProjectedState::Unproven
    );
    assert_eq!(
        project_agent(&running, &UnprovenChildren).state,
        ProjectedState::Unproven
    );

    for (state, expected) in [
        (RuntimeState::Reserved, ProjectedState::Reserved),
        (RuntimeState::Exited, ProjectedState::Exited),
        (RuntimeState::Reclaimed, ProjectedState::Exited),
        (RuntimeState::SpawnFailed, ProjectedState::SpawnFailed),
        (
            RuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown),
            ProjectedState::Unproven,
        ),
    ] {
        let record = agent_record(&resource, operation, state, None);
        assert_eq!(project_agent(&record, &observed).state, expected);
    }

    // Both pools key one ledger, so the digest says which pool it belongs to.
    let generic = terminal_record(&resource, operation, TerminalRuntimeState::Reserved, None);
    assert_eq!(project_agent(&running, &blind).digest, "agent:intent");
    assert_eq!(project_terminal(&generic, &blind).digest, "terminal:digest");
    assert!(ProjectedState::Reserved.holds_capacity());
    assert!(!ProjectedState::Exited.holds_capacity());
    assert!(!ProjectedState::SpawnFailed.holds_capacity());
    assert!(ProjectedState::Unproven.holds_capacity());
}

#[test]
fn a_reservation_claims_capacity_before_the_shard_records_it() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let operation = OperationId::new();
    let mut store = ShardedAgentStore::new(world.state(owner, ObservedChildren::new()));

    let reserved = agent_record(&resource, operation, RuntimeState::Reserved, None);
    store.save(agent_snapshot(vec![reserved.clone()])).unwrap();

    let allocator = world.allocator();
    let claim = allocator.claim(&resource).unwrap();
    assert_eq!(claim.state, ClaimState::Reserved);
    assert_eq!(claim.owner, owner);
    assert_eq!(
        allocator.operation(&operation).unwrap().outcome,
        OperationOutcome::Reserved
    );
    let shard = world.shard(owner);
    let entry = shard.resource(&resource).unwrap();
    assert_eq!(entry.state, ResourceState::Reserved);
    assert_eq!(entry.kind, ResourceKind::Agent);
    // The record travels with the state: one document, one compare-and-swap.
    assert_eq!(
        serde_json::from_str::<DurableRuntimeRecord>(entry.payload.as_deref().unwrap()).unwrap(),
        reserved
    );
}

#[test]
fn a_spawned_child_becomes_a_live_claim_and_a_durable_final() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let operation = OperationId::new();
    let mut store =
        ShardedAgentStore::new(world.state(owner, ObservedChildren::new().with(9, "start-9")));

    let snapshot = agent_snapshot(vec![agent_record(
        &resource,
        operation,
        RuntimeState::Running,
        Some(process(9, "start-9")),
    )]);
    store.save(snapshot.clone()).unwrap();
    // Saving the same truth again converges: the final is not resealed.
    store.save(snapshot).unwrap();

    let allocator = world.allocator();
    assert_eq!(allocator.claim(&resource).unwrap().state, ClaimState::Live);
    assert_eq!(
        allocator.operation(&operation).unwrap().outcome,
        OperationOutcome::Spawned
    );
    assert_eq!(
        world.shard(owner).resource(&resource).unwrap().state,
        ResourceState::Running
    );
    assert_eq!(allocator.pool_used(ResourceKind::Agent), 1);
}

#[test]
fn an_exit_releases_its_capacity_exactly_once_and_keeps_the_record() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let operation = OperationId::new();
    let mut store =
        ShardedAgentStore::new(world.state(owner, ObservedChildren::new().with(9, "start-9")));

    store
        .save(agent_snapshot(vec![agent_record(
            &resource,
            operation,
            RuntimeState::Running,
            Some(process(9, "start-9")),
        )]))
        .unwrap();
    let exited = agent_snapshot(vec![agent_record(
        &resource,
        operation,
        RuntimeState::Exited,
        Some(process(9, "start-9")),
    )]);
    store.save(exited.clone()).unwrap();
    let before = world.allocator().claim(&resource).unwrap().revision;
    store.save(exited).unwrap();

    let allocator = world.allocator();
    assert_eq!(
        allocator.claim(&resource).unwrap().state,
        ClaimState::Released
    );
    // Released exactly once: the second pass finds nothing left to apply.
    assert_eq!(allocator.claim(&resource).unwrap().revision, before);
    assert_eq!(allocator.pool_used(ResourceKind::Agent), 0);
    let shard = world.shard(owner);
    // The exit is swept out of the outbox, and the record stays as history.
    assert_eq!(shard.unacked_outbox(), 0);
    assert_eq!(
        shard.resource(&resource).unwrap().state,
        ResourceState::Exited { status: None }
    );
}

#[test]
fn a_full_pool_refuses_the_save_that_precedes_the_spawn() {
    let world = World::new();
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let mut draining = ShardedTerminalStore::new(world.state(old, ObservedChildren::new()));
    let mut active = ShardedTerminalStore::new(world.state(new, ObservedChildren::new()));

    let records = (0..2)
        .map(|_| {
            terminal_record(
                &terminal(old),
                OperationId::new(),
                TerminalRuntimeState::Reserved,
                None,
            )
        })
        .collect();
    draining.save(terminal_snapshot(records)).unwrap();

    let wanted = terminal(new);
    // Two generations, one pool: the limit holds across both of them.
    assert!(
        active
            .save(terminal_snapshot(vec![terminal_record(
                &wanted,
                OperationId::new(),
                TerminalRuntimeState::Reserved,
                None,
            )]))
            .is_err()
    );
    assert!(world.allocator().claim(&wanted).is_none());
    assert!(world.shard(new).resource(&wanted).is_none());
    assert_eq!(world.allocator().pool_used(ResourceKind::Terminal), 2);
}

#[test]
fn an_old_exit_and_a_new_reservation_both_survive_the_same_allocator() {
    let world = World::new();
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let old_resource = terminal(old);
    let new_resource = terminal(new);
    let old_operation = OperationId::new();
    let mut draining =
        ShardedTerminalStore::new(world.state(old, ObservedChildren::new().with(4, "start-4")));
    let mut active = ShardedTerminalStore::new(world.state(new, ObservedChildren::new()));

    draining
        .save(terminal_snapshot(vec![terminal_record(
            &old_resource,
            old_operation,
            TerminalRuntimeState::Running,
            Some(process(4, "start-4")),
        )]))
        .unwrap();

    // The old owner commits its exit while the new owner reserves: two writers,
    // two documents, and one allocator that keeps both transitions.
    draining
        .save(terminal_snapshot(vec![terminal_record(
            &old_resource,
            old_operation,
            TerminalRuntimeState::Exited,
            Some(process(4, "start-4")),
        )]))
        .unwrap();
    active
        .save(terminal_snapshot(vec![terminal_record(
            &new_resource,
            OperationId::new(),
            TerminalRuntimeState::Reserved,
            None,
        )]))
        .unwrap();

    let allocator = world.allocator();
    assert_eq!(
        allocator.claim(&old_resource).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(
        allocator.claim(&new_resource).unwrap().state,
        ClaimState::Reserved
    );
    assert_eq!(
        world.shard(old).resource(&old_resource).unwrap().state,
        ResourceState::Exited { status: None }
    );
    assert_eq!(
        world.shard(new).resource(&new_resource).unwrap().state,
        ResourceState::Reserved
    );
    // Neither process wrote the other's shard.
    assert!(world.shard(old).resource(&new_resource).is_none());
    assert!(world.shard(new).resource(&old_resource).is_none());
}

#[test]
fn a_foreign_record_is_never_written_and_is_released_only_when_terminated() {
    let world = World::new();
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let old_resource = terminal(old);
    let operation = OperationId::new();
    let mut draining =
        ShardedTerminalStore::new(world.state(old, ObservedChildren::new().with(4, "start-4")));

    draining
        .save(terminal_snapshot(vec![terminal_record(
            &old_resource,
            operation,
            TerminalRuntimeState::Running,
            Some(process(4, "start-4")),
        )]))
        .unwrap();

    let active = world.state(new, ObservedChildren::new());
    let unproven = RuntimeProjection {
        resource: old_resource.clone(),
        kind: ResourceKind::Terminal,
        operation,
        digest: "terminal:digest".to_owned(),
        state: ProjectedState::Unproven,
        payload: String::new(),
    };
    let report = active
        .commit(ResourceKind::Terminal, std::slice::from_ref(&unproven))
        .unwrap();
    assert_eq!((report.owned, report.foreign, report.released), (0, 1, 0));
    // An unprovable foreign record is not terminated, so nothing is released.
    assert_eq!(
        world.allocator().claim(&old_resource).unwrap().state,
        ClaimState::Live
    );
    assert!(world.shard(new).resources.is_empty());
    assert_eq!(world.shard(old).resources.len(), 1);

    let terminated = RuntimeProjection {
        state: ProjectedState::Exited,
        ..unproven
    };
    let report = active
        .commit(ResourceKind::Terminal, std::slice::from_ref(&terminated))
        .unwrap();
    assert_eq!(report.released, 1);
    let released = world.allocator().claim(&old_resource).unwrap().clone();
    assert_eq!(released.state, ClaimState::Released);
    // A repeated pass releases nothing a second time.
    active
        .commit(ResourceKind::Terminal, &[terminated])
        .unwrap();
    assert_eq!(
        world.allocator().claim(&old_resource).unwrap().revision,
        released.revision
    );
}

#[test]
fn a_record_the_owner_stopped_retaining_leaves_the_shard() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let kept = terminal(owner);
    let dropped = terminal(owner);
    let mut store = ShardedTerminalStore::new(world.state(owner, ObservedChildren::new()));

    let records = vec![
        terminal_record(
            &kept,
            OperationId::new(),
            TerminalRuntimeState::Exited,
            None,
        ),
        terminal_record(
            &dropped,
            OperationId::new(),
            TerminalRuntimeState::Exited,
            None,
        ),
    ];
    store.save(terminal_snapshot(records.clone())).unwrap();
    assert_eq!(world.shard(owner).resources.len(), 2);

    store
        .save(terminal_snapshot(records[..1].to_vec()))
        .unwrap();
    let shard = world.shard(owner);
    assert!(shard.resource(&kept).is_some());
    assert!(shard.resource(&dropped).is_none());
}

#[test]
fn a_live_record_that_left_the_owners_truth_is_fenced_instead_of_dropped() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let mut store =
        ShardedTerminalStore::new(world.state(owner, ObservedChildren::new().with(5, "start-5")));

    store
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            OperationId::new(),
            TerminalRuntimeState::Running,
            Some(process(5, "start-5")),
        )]))
        .unwrap();
    store.save(TerminalStoreSnapshot::default()).unwrap();

    // Forgetting it would hide a child nothing would reap, so it keeps its
    // capacity and becomes unprovable instead.
    assert_eq!(
        world.shard(owner).resource(&resource).unwrap().state,
        ResourceState::OwnershipUnknown
    );
    assert_eq!(
        world.allocator().claim(&resource).unwrap().state,
        ClaimState::Live
    );
}

#[test]
fn one_kind_never_forgets_the_other_kinds_records() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let agent = terminal(owner);
    let generic = terminal(owner);
    let mut agent_store = ShardedAgentStore::new(world.state(owner, ObservedChildren::new()));
    let mut terminal_store = ShardedTerminalStore::new(world.state(owner, ObservedChildren::new()));

    agent_store
        .save(agent_snapshot(vec![agent_record(
            &agent,
            OperationId::new(),
            RuntimeState::Reserved,
            None,
        )]))
        .unwrap();
    terminal_store
        .save(terminal_snapshot(vec![terminal_record(
            &generic,
            OperationId::new(),
            TerminalRuntimeState::Reserved,
            None,
        )]))
        .unwrap();

    let shard = world.shard(owner);
    assert_eq!(shard.resource(&agent).unwrap().kind, ResourceKind::Agent);
    assert_eq!(
        shard.resource(&generic).unwrap().kind,
        ResourceKind::Terminal
    );
}

#[test]
fn a_terminated_record_whose_ledger_entry_was_collected_still_saves() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let state = world.state(owner, ObservedChildren::new());

    // No claim and no operation record exist: the ledger already collected this
    // launch, and history is not re-admitted.
    let report = state
        .commit(
            ResourceKind::Agent,
            &[RuntimeProjection {
                resource: resource.clone(),
                kind: ResourceKind::Agent,
                operation: OperationId::new(),
                digest: "agent:intent".to_owned(),
                state: ProjectedState::Exited,
                payload: "{}".to_owned(),
            }],
        )
        .unwrap();
    assert_eq!(report.owned, 1);
    assert!(world.allocator().operations.is_empty());
    assert_eq!(
        world.shard(owner).resource(&resource).unwrap().state,
        ResourceState::Exited { status: None }
    );
}

#[test]
fn an_unprovable_record_that_is_later_terminated_keeps_converging() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let operation = OperationId::new();
    let mut store = ShardedAgentStore::new(world.state(owner, ObservedChildren::new()));

    store
        .save(agent_snapshot(vec![agent_record(
            &resource,
            operation,
            RuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown),
            Some(process(3, "fixed-token")),
        )]))
        .unwrap();
    assert_eq!(
        world.allocator().operation(&operation).unwrap().outcome,
        OperationOutcome::Ambiguous
    );
    assert_eq!(
        world.shard(owner).resource(&resource).unwrap().state,
        ResourceState::OwnershipUnknown
    );

    // The operator supersedes it: the record becomes reclaimed history, and the
    // shard's unprovable state cannot "exit" a second time.
    store
        .save(agent_snapshot(vec![agent_record(
            &resource,
            operation,
            RuntimeState::Reclaimed,
            None,
        )]))
        .unwrap();
    assert_eq!(
        world.shard(owner).resource(&resource).unwrap().state,
        ResourceState::OwnershipUnknown
    );
    // An ambiguous final never releases capacity: a child may exist.
    assert_eq!(
        world.allocator().claim(&resource).unwrap().state,
        ClaimState::Reserved
    );
    assert_eq!(world.allocator().pool_used(ResourceKind::Agent), 1);
}

#[test]
fn a_failed_spawn_releases_its_capacity_as_a_definite_failure() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let operation = OperationId::new();
    let mut store = ShardedTerminalStore::new(world.state(owner, ObservedChildren::new()));

    // The coordinator persists its reservation before it spawns, so the claim the
    // failure releases is the one that reservation took.
    store
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Reserved,
            None,
        )]))
        .unwrap();
    store
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::SpawnFailed,
            None,
        )]))
        .unwrap();

    let allocator = world.allocator();
    assert_eq!(
        allocator.operation(&operation).unwrap().outcome,
        OperationOutcome::Failed(LaunchFailure::Spawn)
    );
    assert_eq!(
        allocator.claim(&resource).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(allocator.pool_used(ResourceKind::Terminal), 0);
}

#[test]
fn a_draining_owner_publishes_without_consuming_its_own_events() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let operation = OperationId::new();
    let mut active =
        ShardedTerminalStore::new(world.state(owner, ObservedChildren::new().with(6, "start-6")));
    active
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Running,
            Some(process(6, "start-6")),
        )]))
        .unwrap();

    let mut draining = ShardedTerminalStore::new(world.role(
        owner,
        GenerationRole::Draining,
        ObservedChildren::new().with(6, "start-6"),
    ));
    draining
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Exited,
            Some(process(6, "start-6")),
        )]))
        .unwrap();

    // The exit is published and waits for the active consumer; a draining owner
    // never writes the allocator's consumed revision itself.
    assert_eq!(world.shard(owner).unacked_outbox(), 1);
    assert_eq!(
        world.allocator().claim(&resource).unwrap().state,
        ClaimState::Live
    );
}

#[test]
fn hydrate_returns_the_records_every_retained_shard_holds() {
    let world = World::new();
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let agent = terminal(old);
    let generic = terminal(old);
    let mut agent_store =
        ShardedAgentStore::new(world.state(old, ObservedChildren::new().with(1, "start-1")));
    let mut terminal_store =
        ShardedTerminalStore::new(world.state(old, ObservedChildren::new().with(2, "start-2")));
    agent_store
        .save(agent_snapshot(vec![agent_record(
            &agent,
            OperationId::new(),
            RuntimeState::Running,
            Some(process(1, "start-1")),
        )]))
        .unwrap();
    terminal_store
        .save(terminal_snapshot(vec![terminal_record(
            &generic,
            OperationId::new(),
            TerminalRuntimeState::Running,
            Some(process(2, "start-2")),
        )]))
        .unwrap();

    // A third generation's shard, so hydration reads more than one document and
    // has to order them.
    let other = DaemonGeneration::new();
    let elsewhere = terminal(other);
    ShardedTerminalStore::new(world.state(other, ObservedChildren::new()))
        .save(terminal_snapshot(vec![terminal_record(
            &elsewhere,
            OperationId::new(),
            TerminalRuntimeState::Reserved,
            None,
        )]))
        .unwrap();

    // A fresh process owns a new generation, so every record it finds belongs to
    // somebody else and comes back fenced.
    let hydrated = world.state(new, ObservedChildren::new()).hydrate().unwrap();
    assert_eq!(hydrated.interrupted, 3);
    assert_eq!(hydrated.agents.records.len(), 1);
    assert_eq!(hydrated.terminals.records.len(), 2);
    assert_eq!(
        hydrated.agents.records[0].state,
        RuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown)
    );
    assert!(hydrated.terminals.records.iter().all(|record| record.state
        == TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown)));
    assert!(hydrated.migration.is_none());
}

#[test]
fn hydrate_reclaims_a_retired_child_the_platform_proves_gone() {
    let world = World::new();
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let resource = terminal(old);
    let generic = terminal(old);
    let operation = OperationId::new();
    let mut old_store =
        ShardedAgentStore::new(world.state(old, ObservedChildren::new().with(31, "start-31")));
    old_store
        .save(agent_snapshot(vec![agent_record(
            &resource,
            operation,
            RuntimeState::Running,
            Some(process(31, "start-31")),
        )]))
        .unwrap();
    ShardedAgentStore::new(world.state(old, ObservedChildren::new()))
        .save(agent_snapshot(vec![agent_record(
            &resource,
            operation,
            RuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown),
            Some(process(31, "start-31")),
        )]))
        .unwrap();
    assert_eq!(
        world.shard(old).resources[0].state,
        ResourceState::OwnershipUnknown
    );
    let terminal_operation = OperationId::new();
    ShardedTerminalStore::new(world.state(old, ObservedChildren::new().with(32, "start-32")))
        .save(terminal_snapshot(vec![terminal_record(
            &generic,
            terminal_operation,
            TerminalRuntimeState::Running,
            Some(process(32, "start-32")),
        )]))
        .unwrap();
    ShardedTerminalStore::new(world.state(old, ObservedChildren::new()))
        .save(terminal_snapshot(vec![terminal_record(
            &generic,
            terminal_operation,
            TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown),
            Some(process(32, "start-32")),
        )]))
        .unwrap();

    let hydrated = world
        .state(new, ObservedChildren::new().with_gone(31).with_gone(32))
        .hydrate()
        .unwrap();

    assert_eq!(hydrated.interrupted, 0);
    assert_eq!(hydrated.agents.records[0].state, RuntimeState::Interrupted);
    assert_eq!(
        hydrated.terminals.records[0].state,
        TerminalRuntimeState::Interrupted
    );
    assert_eq!(
        world.allocator().claim(&resource).unwrap().state,
        ClaimState::Released,
        "definite OS absence frees the retired generation's capacity"
    );
    assert_eq!(
        world.allocator().claim(&generic).unwrap().state,
        ClaimState::Released
    );
}

#[test]
fn hydrate_fails_closed_on_state_this_build_must_not_act_on() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);

    // A payload that is not a record at all.
    let mut document = ShardDocument::empty(owner);
    document
        .reserve(
            &OperationId::new(),
            "agent:x",
            ResourceKind::Agent,
            &resource,
        )
        .unwrap();
    document.set_payload(&resource, "not-a-record").unwrap();
    world
        .archive
        .bytes(owner)
        .set(&serde_json::to_string(&document).unwrap());
    assert_eq!(
        world
            .state(owner, ObservedChildren::new())
            .hydrate()
            .unwrap_err()
            .refusal(),
        Some(ResourceError::Corrupt)
    );

    // The same, for a generic terminal record.
    let generic = terminal(owner);
    let mut document = ShardDocument::empty(owner);
    document
        .reserve(
            &OperationId::new(),
            "terminal:digest",
            ResourceKind::Terminal,
            &generic,
        )
        .unwrap();
    document.set_payload(&generic, "not-a-record").unwrap();
    world
        .archive
        .bytes(owner)
        .set(&serde_json::to_string(&document).unwrap());
    assert_eq!(
        world
            .state(owner, ObservedChildren::new())
            .hydrate()
            .unwrap_err()
            .refusal(),
        Some(ResourceError::Corrupt)
    );

    // Bytes that are not a shard document.
    world.archive.bytes(owner).set("not-json");
    assert_eq!(
        world
            .state(owner, ObservedChildren::new())
            .hydrate()
            .unwrap_err()
            .refusal(),
        Some(ResourceError::Corrupt)
    );
}

#[test]
fn hydrate_refuses_a_shard_whose_own_invariants_are_broken() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let other = DaemonGeneration::new();
    let mut document = ShardDocument::empty(owner);
    // A hand-written document claiming a resource of another generation.
    document.resources.push(ShardResource {
        resource: terminal(other),
        kind: ResourceKind::Agent,
        operation: OperationId::new(),
        digest: String::new(),
        process: None,
        state: ResourceState::Reserved,
        payload: None,
        revision: 1,
    });
    world
        .archive
        .bytes(owner)
        .set(&serde_json::to_string(&document).unwrap());
    assert_eq!(
        world
            .state(owner, ObservedChildren::new())
            .hydrate()
            .unwrap_err()
            .refusal(),
        Some(ResourceError::Corrupt)
    );
}

#[test]
fn a_legacy_store_is_adopted_once_and_only_where_it_can_prove_itself() {
    let old = DaemonGeneration::new();
    let proven = terminal(old);
    let fixed = terminal(old);
    let world = World::with_legacy(
        Some(&agents(vec![agent_record(
            &proven,
            OperationId::new(),
            RuntimeState::Running,
            // A legacy record's token is never OS-observed by this process, so
            // even a well-formed one cannot become live.
            Some(process(11, "daemon-owned-agent-pty")),
        )])),
        Some(&terminals(vec![terminal_record(
            &fixed,
            OperationId::new(),
            TerminalRuntimeState::Running,
            Some(process(12, "daemon-owned-pty")),
        )])),
    );

    let new = DaemonGeneration::new();
    let hydrated = world.state(new, ObservedChildren::new()).hydrate().unwrap();
    let migration = hydrated.migration.unwrap();
    assert_eq!(migration.marker.schema, MIGRATION_SCHEMA);
    assert_eq!(migration.marker.adopted, 2);
    assert_eq!(migration.marker.unknown, 2);
    assert_eq!(migration.marker.generations, vec![old.as_str()]);
    assert!(
        migration
            .unknown
            .iter()
            .all(|record| record.refusal == AdoptionRefusal::UnverifiableIdentity)
    );
    // Non-spawnable safe failures: recorded, visible, and holding no capacity.
    let shard = world.shard(old);
    assert_eq!(shard.live_resources(), 0);
    assert!(
        shard
            .resources
            .iter()
            .all(|entry| entry.state == ResourceState::OwnershipUnknown)
    );
    assert!(world.allocator().claims.is_empty());
    assert_eq!(hydrated.agents.records.len(), 1);
    assert_eq!(hydrated.terminals.records.len(), 1);
    assert!(world.archive.marker().is_some());

    // The stores are out of service, so a second hydrate migrates nothing.
    assert!(
        world
            .state(new, ObservedChildren::new())
            .hydrate()
            .unwrap()
            .migration
            .is_none()
    );
}

#[test]
fn a_legacy_store_this_build_cannot_read_is_not_migrated_at_all() {
    for (agents_bytes, terminals_bytes, expected) in [
        (Some("not-json"), None, ResourceError::Corrupt),
        (
            Some(r#"{"schema_version":99,"records":[]}"#),
            None,
            ResourceError::UnknownSchema,
        ),
        (None, Some("not-json"), ResourceError::Corrupt),
        (
            None,
            Some(r#"{"schema_version":9,"records":[]}"#),
            ResourceError::UnknownSchema,
        ),
    ] {
        let world = World::with_legacy(agents_bytes, terminals_bytes);
        assert_eq!(
            world
                .state(DaemonGeneration::new(), ObservedChildren::new())
                .hydrate()
                .unwrap_err()
                .refusal(),
            Some(expected)
        );
        // Nothing was sealed, so the bytes stay available for a fix or a rollback.
        assert!(world.archive.marker().is_none());
    }
}

#[test]
fn a_migration_that_crashed_before_sealing_converges_on_the_next_pass() {
    let old = DaemonGeneration::new();
    let resource = terminal(old);
    let legacy = agents(vec![agent_record(
        &resource,
        OperationId::new(),
        RuntimeState::Running,
        Some(process(31, "daemon-owned-agent-pty")),
    )]);
    let world = World::with_legacy(Some(&legacy), None);
    let new = DaemonGeneration::new();

    world
        .state(new, ObservedChildren::new())
        .hydrate()
        .unwrap()
        .migration
        .unwrap();
    let after_first = world.shard(old);

    // The same legacy bytes are still readable (the seal never landed, or an
    // older build wrote them again): adoption is deterministic and does not touch
    // the shard it already built.
    let replayed = World {
        allocator: world.allocator.clone(),
        archive: MemoryArchive::with_legacy(Some(&legacy), None),
    };
    replayed
        .archive
        .bytes(old)
        .set(&world.archive.bytes(old).get().unwrap());
    let report = replayed
        .state(new, ObservedChildren::new())
        .hydrate()
        .unwrap()
        .migration
        .unwrap();
    assert_eq!(report.marker.adopted, 0);
    assert_eq!(replayed.shard(old), after_first);
}

#[test]
fn a_retired_shard_is_collected_only_once_nothing_retains_it() {
    let world = World::new();
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let resource = terminal(old);
    let operation = OperationId::new();
    let mut draining =
        ShardedTerminalStore::new(world.state(old, ObservedChildren::new().with(41, "start-41")));
    draining
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Running,
            Some(process(41, "start-41")),
        )]))
        .unwrap();

    let active = world.state(new, ObservedChildren::new());
    let retained: BTreeSet<String> = std::iter::once(resource.terminal_id.as_str()).collect();
    // A live claim keeps it, and so does the active generation still retaining
    // the record.
    assert_eq!(active.collect_retired(&retained).unwrap(), 0);

    draining
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Exited,
            Some(process(41, "start-41")),
        )]))
        .unwrap();
    assert_eq!(active.collect_retired(&retained).unwrap(), 0);
    assert_eq!(active.collect_retired(&BTreeSet::new()).unwrap(), 1);
    assert_eq!(world.archive.collected(), vec![old.as_str()]);
    // Its own shard is never a collection candidate.
    assert_eq!(active.collect_retired(&BTreeSet::new()).unwrap(), 0);
}

#[test]
fn a_draining_owner_observes_its_own_shard_and_global_claim_together() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let operation = OperationId::new();
    let empty = world.role(owner, GenerationRole::Draining, ObservedChildren::new());
    assert_eq!(DrainObservation::blocker(&empty).unwrap(), None);

    let mut store = ShardedTerminalStore::new(world.role(
        owner,
        GenerationRole::Draining,
        ObservedChildren::new().with(71, "start-71"),
    ));
    store
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Running,
            Some(process(71, "start-71")),
        )]))
        .unwrap();
    assert_eq!(
        empty.self_collectable().unwrap(),
        Some(CollectionBlocker::LiveResource)
    );
}

#[test]
fn a_crashed_owners_published_exit_is_still_consumed_once() {
    let world = World::new();
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let resource = terminal(old);
    let operation = OperationId::new();
    let mut draining = ShardedTerminalStore::new(world.role(
        old,
        GenerationRole::Draining,
        ObservedChildren::new().with(51, "start-51"),
    ));
    draining
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Running,
            Some(process(51, "start-51")),
        )]))
        .unwrap();
    draining
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Exited,
            Some(process(51, "start-51")),
        )]))
        .unwrap();
    assert_eq!(world.shard(old).unacked_outbox(), 1);

    // The owner is gone. The new active generation applies what it published and
    // can then collect the shard, which the dead owner could never sweep itself.
    world.state(new, ObservedChildren::new()).hydrate().unwrap();
    assert_eq!(
        world.allocator().claim(&resource).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(
        world
            .state(new, ObservedChildren::new())
            .collect_retired(&BTreeSet::new())
            .unwrap(),
        1
    );
}

#[test]
fn a_standby_hydrates_without_consuming_anything() {
    let world = World::new();
    let old = DaemonGeneration::new();
    let resource = terminal(old);
    let operation = OperationId::new();
    let mut owner = ShardedTerminalStore::new(world.role(
        old,
        GenerationRole::Draining,
        ObservedChildren::new().with(61, "start-61"),
    ));
    owner
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Running,
            Some(process(61, "start-61")),
        )]))
        .unwrap();
    owner
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Exited,
            Some(process(61, "start-61")),
        )]))
        .unwrap();

    let standby = world.role(
        DaemonGeneration::new(),
        GenerationRole::Standby,
        ObservedChildren::new(),
    );
    standby.hydrate().unwrap();
    // Readiness is read only: the exit is still waiting for the active consumer.
    assert_eq!(
        world.allocator().claim(&resource).unwrap().state,
        ClaimState::Live
    );
    assert_eq!(world.shard(old).unacked_outbox(), 1);
}

#[test]
fn a_store_failure_is_reported_as_a_refused_save() {
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let unreadable = || {
        ShardedRuntimeState::new(
            owner,
            GenerationRole::Active,
            ResourceAllocator::new(
                MemoryFile::faulty(&SharedBytes::default(), FileFault::ReadFails),
                policy(1, 1),
            ),
            Box::new(MemoryArchive::new()),
            Box::new(ObservedChildren::new()),
            Box::new(FakeClock::at(1)),
        )
        .unwrap()
    };

    let mut agent_store = ShardedAgentStore::new(unreadable());
    assert_eq!(agent_store.state().owner(), owner);
    assert!(
        agent_store
            .save(agent_snapshot(vec![agent_record(
                &resource,
                OperationId::new(),
                RuntimeState::Reserved,
                None,
            )]))
            .is_err()
    );

    let mut terminal_store = ShardedTerminalStore::new(unreadable());
    assert_eq!(terminal_store.state().owner(), owner);
    assert!(
        terminal_store
            .save(terminal_snapshot(vec![terminal_record(
                &resource,
                OperationId::new(),
                TerminalRuntimeState::Reserved,
                None,
            )]))
            .is_err()
    );
    assert!(unreadable().collect_retired(&BTreeSet::new()).is_err());
    assert!(unreadable().self_collectable().is_err());
}

#[test]
fn a_census_counts_live_runtime_without_touching_anything() {
    let old = DaemonGeneration::new();
    let unmigrated = terminal(old);
    let world = World::with_legacy(
        Some(&agents(vec![agent_record(
            &unmigrated,
            OperationId::new(),
            RuntimeState::Running,
            Some(process(71, "daemon-owned-agent-pty")),
        )])),
        Some(&terminals(vec![terminal_record(
            &terminal(old),
            OperationId::new(),
            // A record waiting to be reconciled owns no PTY any more.
            TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown),
            None,
        )])),
    );
    let live = terminal(old);
    let observed = ObservedChildren::new()
        .with(72, "start-72")
        .with(73, "start-73");
    let mut store = ShardedTerminalStore::new(world.state(old, observed));
    store
        .save(terminal_snapshot(vec![terminal_record(
            &live,
            OperationId::new(),
            TerminalRuntimeState::Running,
            Some(process(72, "start-72")),
        )]))
        .unwrap();
    let live_agent = terminal(old);
    ShardedAgentStore::new(world.state(old, ObservedChildren::new().with(73, "start-73")))
        .save(agent_snapshot(vec![agent_record(
            &live_agent,
            OperationId::new(),
            RuntimeState::Running,
            Some(process(73, "start-73")),
        )]))
        .unwrap();

    // The legacy store is still readable, so the PTYs it describes are counted
    // even though nothing has migrated them yet.
    assert_eq!(
        census(&world.archive).unwrap(),
        LiveCensus {
            agents: 2,
            terminals: 1
        }
    );
    assert!(world.archive.marker().is_none());

    for (agents_bytes, terminals_bytes) in [
        (Some("not-json"), None),
        (Some(r#"{"schema_version":99,"records":[]}"#), None),
        (None, Some("not-json")),
        (None, Some(r#"{"schema_version":9,"records":[]}"#)),
    ] {
        let broken = World::with_legacy(agents_bytes, terminals_bytes);
        // A census that cannot be taken is never reported as "nothing is live".
        assert!(census(&broken.archive).is_err());
    }
    let corrupt = World::new();
    corrupt.archive.bytes(old).set("not-json");
    assert!(census(&corrupt.archive).is_err());
}

#[test]
fn a_shard_resource_without_a_record_is_skipped_rather_than_guessed() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let mut document = ShardDocument::empty(owner);
    document
        .reserve(
            &OperationId::new(),
            "terminal:digest",
            ResourceKind::Terminal,
            &resource,
        )
        .unwrap();
    world
        .archive
        .bytes(owner)
        .set(&serde_json::to_string(&document).unwrap());

    // A reservation whose record is not there yet holds capacity in the shard, and
    // nothing about it is invented for the snapshot the coordinators hydrate.
    let hydrated = world
        .state(owner, ObservedChildren::new())
        .hydrate()
        .unwrap();
    assert!(hydrated.terminals.records.is_empty());
    assert!(hydrated.agents.records.is_empty());
}

#[test]
fn a_shard_transition_that_contradicts_itself_refuses_the_save() {
    let world = World::new();
    let owner = DaemonGeneration::new();
    let resource = terminal(owner);
    let state = world.state(owner, ObservedChildren::new());
    let reserved = RuntimeProjection {
        resource: resource.clone(),
        kind: ResourceKind::Terminal,
        operation: OperationId::new(),
        digest: "terminal:digest".to_owned(),
        state: ProjectedState::Reserved,
        payload: "{}".to_owned(),
    };

    // The same resource id under a second producer operation cannot be reserved:
    // one resource has one owner operation, and a contradiction is not merged.
    let conflicting = RuntimeProjection {
        operation: OperationId::new(),
        ..reserved.clone()
    };
    assert_eq!(
        state
            .commit(ResourceKind::Terminal, &[reserved.clone(), conflicting])
            .unwrap_err()
            .refusal(),
        Some(ResourceError::DuplicateResource)
    );
    assert!(world.shard(owner).resources.is_empty());

    // A resource id that two records claim is a contradiction the shard refuses:
    // the terminated one takes no claim, so the refusal comes from the shard
    // itself rather than from the ledger.
    let terminated = RuntimeProjection {
        operation: OperationId::new(),
        state: ProjectedState::Exited,
        ..reserved.clone()
    };
    assert_eq!(
        state
            .commit(ResourceKind::Terminal, &[terminated, reserved.clone()])
            .unwrap_err()
            .refusal(),
        Some(ResourceError::DuplicateResource)
    );
    assert!(world.shard(owner).resources.is_empty());

    // A running record cannot be handed a different child either.
    let running = RuntimeProjection {
        state: ProjectedState::Running(verified(81, "start-81")),
        ..reserved.clone()
    };
    state
        .commit(ResourceKind::Terminal, std::slice::from_ref(&running))
        .unwrap();
    let replaced = RuntimeProjection {
        state: ProjectedState::Running(verified(82, "start-82")),
        ..reserved
    };
    assert_eq!(
        state
            .commit(ResourceKind::Terminal, &[replaced])
            .unwrap_err()
            .refusal(),
        Some(ResourceError::WrongState)
    );
    assert_eq!(
        world.shard(owner).resource(&resource).unwrap().process,
        Some(verified(81, "start-81"))
    );
}

#[test]
fn a_terminated_legacy_record_is_adopted_without_being_called_live() {
    let old = DaemonGeneration::new();
    let resource = terminal(old);
    let world = World::with_legacy(
        None,
        Some(&terminals(vec![terminal_record(
            &resource,
            OperationId::new(),
            TerminalRuntimeState::Exited,
            None,
        )])),
    );

    let migration = world
        .state(DaemonGeneration::new(), ObservedChildren::new())
        .hydrate()
        .unwrap()
        .migration
        .unwrap();

    assert_eq!(migration.marker.adopted, 1);
    assert_eq!(world.shard(old).live_resources(), 0);
    assert!(world.allocator().claims.is_empty());
}

#[test]
fn a_collection_pass_bounds_the_ledger_and_removes_drained_shards() {
    let world = World::new();
    let old = DaemonGeneration::new();
    let new = DaemonGeneration::new();
    let resource = terminal(old);
    let operation = OperationId::new();
    let mut draining =
        ShardedTerminalStore::new(world.state(old, ObservedChildren::new().with(91, "start-91")));
    draining
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Running,
            Some(process(91, "start-91")),
        )]))
        .unwrap();
    draining
        .save(terminal_snapshot(vec![terminal_record(
            &resource,
            operation,
            TerminalRuntimeState::Exited,
            Some(process(91, "start-91")),
        )]))
        .unwrap();

    // The shipping bounds keep the answer for its minimum window, so a pass this
    // early collects the drained shard without evicting the operation.
    let limits = shipping_retention_limits();
    assert!(limits.min_window > 0);
    let active = world.state(new, ObservedChildren::new());
    let (ledger, shards) = active.collect(&BTreeSet::new(), &limits).unwrap();
    assert_eq!(ledger.evicted, 0);
    assert!(!ledger.backpressure);
    assert_eq!(shards, 1);
    assert!(world.allocator().operation(&operation).is_some());

    // Far past the window, the full outcome becomes a compact tombstone and the
    // same producer id can never be admitted again.
    let aged = ShardedRuntimeState::new(
        new,
        GenerationRole::Active,
        ResourceAllocator::new(MemoryFile::new(&world.allocator), policy(2, 2)),
        Box::new(world.archive.clone()),
        Box::new(ObservedChildren::new()),
        Box::new(FakeClock::at(limits.max_age * 4)),
    )
    .unwrap();
    let (ledger, _) = aged.collect(&BTreeSet::new(), &limits).unwrap();
    assert_eq!(ledger.evicted, 1);
    assert!(world.allocator().is_expired(&operation));
}

#[test]
fn the_marker_round_trips_through_its_durable_form() {
    let marker = MigrationMarker {
        schema: MIGRATION_SCHEMA.to_owned(),
        generations: vec!["g1".to_owned()],
        adopted: 2,
        unknown: 1,
    };
    let encoded: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&marker).unwrap()).unwrap();
    assert_eq!(encoded["schema"], MIGRATION_SCHEMA);
    assert_eq!(encoded["generations"][0], "g1");
    assert_eq!(encoded["adopted"], 2);
    assert_eq!(encoded["unknown"], 1);
    assert!(LegacySnapshots::default().is_empty());
}
