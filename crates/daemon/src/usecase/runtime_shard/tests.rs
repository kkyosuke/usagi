//! The production stores against the contract, with every seam injected.
//!
//! Two "processes" are two [`OwnerRuntimeState`]s over different shard documents
//! and the *same* allocator bytes, which is exactly the pair a planned rollover
//! creates. A barrier makes the interleaving deterministic instead of hoping the
//! scheduler produces it.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use serde_json::json;
use usagi_core::domain::agent::{
    AgentProfileId, DurableLaunchSnapshot, LaunchMode, LaunchPlan, LaunchRequest, LaunchScope,
};
use usagi_core::domain::id::{
    AgentRuntimeId, AgentRuntimeRef, CompletionFence, DaemonGeneration, OperationId, SessionId,
    TerminalId, TerminalRef, WorkspaceId, WorktreeId,
};
use usagi_core::domain::terminal_launch::{
    DurableTerminalLaunchSnapshot, TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};

use super::*;
use crate::usecase::generic_terminal::DurableTerminalRecord;
use crate::usecase::resources::allocator::{ClaimState, OperationOutcome};
use crate::usecase::resources::fixture::{
    FakeClock, FakeProbe, MemoryFile, ProbeAnswer, SharedBytes, policy, probe_for,
};
use crate::usecase::resources::migration::AdoptionRefusal;
use crate::usecase::resources::shard::collectable;
use crate::usecase::runtime::{DurableOperationOutcome, DurableRuntimeRecord};
use crate::usecase::terminal::TerminalReconcileState;

/// A [`CasFile`] that loses the race a fixed number of times before behaving.
struct FlakyFile {
    bytes: SharedBytes,
    stale: AtomicUsize,
}

impl FlakyFile {
    fn new(bytes: &SharedBytes, stale: usize) -> Self {
        Self {
            bytes: bytes.clone(),
            stale: AtomicUsize::new(stale),
        }
    }
}

impl crate::usecase::resources::CasFile for FlakyFile {
    fn read(&self) -> io::Result<Option<String>> {
        Ok(self.bytes.get())
    }

    fn compare_and_write(&self, expected: Option<&str>, contents: &str) -> io::Result<bool> {
        if self
            .stale
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |stale| {
                stale.checked_sub(1)
            })
            .is_ok()
        {
            return Ok(false);
        }
        MemoryFile::new(&self.bytes).compare_and_write(expected, contents)
    }
}

/// The retained shards of one data directory, in memory.
#[derive(Default)]
struct MemorySource {
    shards: Mutex<Vec<(DaemonGeneration, SharedBytes)>>,
    fails: bool,
}

impl MemorySource {
    fn new() -> Self {
        Self::default()
    }

    fn failing() -> Self {
        Self {
            shards: Mutex::new(Vec::new()),
            fails: true,
        }
    }

    /// The bytes of one generation's shard, created on first use so `open` is the
    /// only place a shard comes into existence.
    fn bytes(&self, generation: DaemonGeneration) -> SharedBytes {
        let mut shards = self.shards.lock().unwrap();
        if let Some((_, bytes)) = shards.iter().find(|(retained, _)| *retained == generation) {
            return bytes.clone();
        }
        let bytes = SharedBytes::default();
        shards.push((generation, bytes.clone()));
        bytes
    }
}

impl ShardSource for MemorySource {
    fn generations(&self) -> io::Result<Vec<DaemonGeneration>> {
        if self.fails {
            return Err(io::Error::other("shard directory is unreadable"));
        }
        Ok(self
            .shards
            .lock()
            .unwrap()
            .iter()
            .map(|(generation, _)| *generation)
            .collect())
    }

    fn open(&self, generation: DaemonGeneration) -> io::Result<OwnerShard> {
        Ok(OwnerShard::new(
            MemoryFile::new(&self.bytes(generation)),
            generation,
        ))
    }
}

fn terminal_of(owner: DaemonGeneration) -> TerminalRef {
    TerminalRef {
        daemon_generation: owner,
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    }
}

fn fence_of(terminal: &TerminalRef, operation: OperationId) -> CompletionFence {
    CompletionFence {
        workspace_id: terminal.workspace_id,
        session_id: terminal.session_id,
        operation_id: operation,
        owner_daemon_generation: terminal.daemon_generation,
        execution_attempt: 1,
        lifecycle_attempt: 1,
        expected_revision: 1,
    }
}

fn process(pid: u32, token: &str) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_identity: token.to_owned(),
        process_group: pid,
    }
}

fn agent_record(
    terminal: &TerminalRef,
    operation: OperationId,
    state: TerminalRuntimeState,
    identity: Option<ProcessIdentity>,
) -> DurableRuntimeRecord {
    let scope = LaunchScope {
        workspace_id: terminal.workspace_id,
        session_id: terminal.session_id,
        worktree_id: terminal.worktree_id,
    };
    let profile = AgentProfileId::new("codex").unwrap();
    let request = LaunchRequest {
        profile_id: profile.clone(),
        mode: LaunchMode::Interactive,
        model: None,
        resume: false,
        provider_resume: None,
        initial_prompt: None,
        scope,
        required_capabilities: std::collections::BTreeSet::new(),
    };
    let plan = LaunchPlan::new(
        profile,
        1,
        "codex",
        vec!["codex".to_owned()],
        Vec::new(),
        std::path::PathBuf::from("/tmp"),
    )
    .unwrap();
    DurableRuntimeRecord {
        runtime: AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), terminal.session_id)
            .unwrap(),
        operation: fence_of(terminal, operation),
        launch: DurableLaunchSnapshot::new(request, plan),
        state,
        process: identity,
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
    terminal: &TerminalRef,
    operation: OperationId,
    state: TerminalRuntimeState,
    identity: Option<ProcessIdentity>,
) -> DurableTerminalRecord {
    let request = TerminalLaunchRequest {
        profile_id: TerminalProfileId::new("login-shell").unwrap(),
        scope: TerminalLaunchScope {
            workspace_id: terminal.workspace_id,
            session_id: terminal.session_id,
            worktree_id: terminal.worktree_id,
        },
    };
    DurableTerminalRecord {
        terminal: terminal.clone(),
        operation: fence_of(terminal, operation),
        launch: DurableTerminalLaunchSnapshot::new(
            request,
            1,
            "/bin/sh",
            Vec::new(),
            std::path::PathBuf::from("/tmp"),
            Vec::new(),
        )
        .unwrap(),
        state,
        process: identity,
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

/// One owner's writer over `shard`/`ledger`, seeing `probe`.
fn writer(
    kind: ResourceKind,
    owner: DaemonGeneration,
    shard: &SharedBytes,
    ledger: &SharedBytes,
    probe: FakeProbe,
    limits: (usize, usize),
) -> OwnerRuntimeState {
    OwnerRuntimeState::new(
        kind,
        OwnerShard::new(MemoryFile::new(shard), owner),
        ResourceAllocator::new(MemoryFile::new(ledger), policy(limits.0, limits.1)),
        Box::new(probe),
        Box::new(FakeClock::at(7)),
    )
}

fn ledger_of(bytes: &SharedBytes) -> crate::usecase::resources::allocator::AllocatorDocument {
    ResourceAllocator::new(MemoryFile::new(bytes), policy(1, 1))
        .load()
        .unwrap()
        .to_document()
}

fn shard_of(bytes: &SharedBytes, owner: DaemonGeneration) -> ShardDocument {
    OwnerShard::new(MemoryFile::new(bytes), owner)
        .load()
        .unwrap()
        .to_document()
}

#[test]
fn every_durable_state_projects_to_exactly_one_contract_meaning() {
    let owner = DaemonGeneration::new();
    let foreign = terminal_of(DaemonGeneration::new());
    let running = terminal_of(owner);
    let identity = process(11, "token");
    let cases = [
        (
            TerminalRuntimeState::Reserved,
            None,
            ProjectedState::Reserved,
        ),
        (
            TerminalRuntimeState::Running,
            Some(identity.clone()),
            ProjectedState::Running(identity.clone()),
        ),
        (TerminalRuntimeState::Running, None, ProjectedState::Unknown),
        (
            TerminalRuntimeState::SpawnFailed,
            None,
            ProjectedState::Failed,
        ),
        (
            TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown),
            None,
            ProjectedState::Unknown,
        ),
        (TerminalRuntimeState::Exited, None, ProjectedState::Ended),
        (TerminalRuntimeState::Reclaimed, None, ProjectedState::Ended),
    ];
    for (state, identity, expected) in cases {
        let operation = OperationId::new();
        let agents = agent_snapshot(vec![
            agent_record(&running, operation, state, identity.clone()),
            agent_record(&foreign, OperationId::new(), state, identity.clone()),
        ]);
        let projected = project_agents(&agents, owner);
        assert_eq!(projected.len(), 1, "a foreign owner's record is not ours");
        assert_eq!(projected[0].state, expected);
        assert_eq!(projected[0].kind, ResourceKind::Agent);
        assert_eq!(projected[0].operation, operation);
        assert_eq!(projected[0].digest, "intent");

        let terminals = terminal_snapshot(vec![
            terminal_record(&running, operation, state, identity.clone()),
            terminal_record(&foreign, OperationId::new(), state, identity.clone()),
        ]);
        let projected = project_terminals(&terminals, owner);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].state, expected);
        assert_eq!(projected[0].kind, ResourceKind::Terminal);
        assert_eq!(projected[0].digest, "digest");
    }
}

#[test]
fn a_record_without_a_producer_intent_projects_an_empty_digest() {
    let owner = DaemonGeneration::new();
    let resource = terminal_of(owner);
    let operation = OperationId::new();
    let mut agent = agent_record(&resource, operation, TerminalRuntimeState::Reserved, None);
    agent.semantic_key = None;
    assert_eq!(
        project_agents(&agent_snapshot(vec![agent]), owner)[0].digest,
        ""
    );
    let mut terminal = terminal_record(&resource, operation, TerminalRuntimeState::Reserved, None);
    terminal.launch_digest = None;
    assert_eq!(
        project_terminals(&terminal_snapshot(vec![terminal]), owner)[0].digest,
        ""
    );
}

#[test]
fn only_the_exact_process_the_os_reports_is_a_verifiable_child() {
    let recorded = process(21, "token");
    assert_eq!(
        verify_process(&probe_for(21, "token"), &recorded).map(|identity| identity.is_verifiable()),
        Some(true)
    );
    assert!(
        verify_process(&FakeProbe::new(), &recorded).is_none(),
        "gone"
    );
    assert!(
        verify_process(&probe_for(21, "another"), &recorded).is_none(),
        "the pid was reused by another process"
    );
    assert!(
        verify_process(&FakeProbe::new().with(21, ProbeAnswer::Denied), &recorded).is_none(),
        "an unreadable platform proves nothing"
    );
}

#[test]
fn a_reserved_save_claims_capacity_and_commits_the_payload_in_one_swap() {
    let owner = DaemonGeneration::new();
    let (shard, ledger) = (SharedBytes::default(), SharedBytes::default());
    let resource = terminal_of(owner);
    let operation = OperationId::new();
    let mut store = TerminalShardStore::new(writer(
        ResourceKind::Terminal,
        owner,
        &shard,
        &ledger,
        FakeProbe::new(),
        (2, 2),
    ));
    let snapshot = terminal_snapshot(vec![terminal_record(
        &resource,
        operation,
        TerminalRuntimeState::Reserved,
        None,
    )]);

    store.save(snapshot.clone()).unwrap();

    let document = shard_of(&shard, owner);
    assert_eq!(
        document.resource(&resource).unwrap().state,
        ResourceState::Reserved
    );
    assert_eq!(
        serde_json::from_value::<TerminalStoreSnapshot>(
            document.payload(ResourceKind::Terminal).unwrap().clone()
        )
        .unwrap(),
        snapshot
    );
    let claim = ledger_of(&ledger).claim(&resource).unwrap().clone();
    assert_eq!(claim.state, ClaimState::Reserved);
    assert_eq!(claim.owner, owner);
    // A repeated save of the same state is a converged repeat, not a second claim.
    store.save(snapshot).unwrap();
    assert_eq!(ledger_of(&ledger).claims.len(), 1);
}

#[test]
fn an_exhausted_pool_refuses_the_save_and_leaves_both_documents_untouched() {
    let owner = DaemonGeneration::new();
    let (shard, ledger) = (SharedBytes::default(), SharedBytes::default());
    let first = terminal_of(owner);
    let second = terminal_of(owner);
    let state = writer(
        ResourceKind::Terminal,
        owner,
        &shard,
        &ledger,
        FakeProbe::new(),
        (1, 1),
    );
    let accepted = terminal_snapshot(vec![terminal_record(
        &first,
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    )]);
    TerminalShardStore::new(state)
        .save(accepted.clone())
        .unwrap();
    let (shard_bytes, ledger_bytes) = (shard.get(), ledger.get());

    let mut overflowing = accepted.clone();
    overflowing.records.push(terminal_record(
        &second,
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    ));
    let state = writer(
        ResourceKind::Terminal,
        owner,
        &shard,
        &ledger,
        FakeProbe::new(),
        (1, 1),
    );
    assert_eq!(
        state
            .commit(&json!({}), &project_terminals(&overflowing, owner))
            .unwrap_err()
            .refusal(),
        Some(ResourceError::CapacityExhausted)
    );
    assert_eq!(shard.get(), shard_bytes, "the refusal wrote no shard");
    assert_eq!(ledger.get(), ledger_bytes, "the refusal took no capacity");
    assert!(shard_of(&shard, owner).resource(&second).is_none());
    // The port erases the reason, but not the fact: nothing was spawned.
    assert!(
        TerminalShardStore::new(writer(
            ResourceKind::Terminal,
            owner,
            &shard,
            &ledger,
            FakeProbe::new(),
            (1, 1),
        ))
        .save(overflowing)
        .is_err()
    );
}

#[test]
fn a_running_record_is_owned_only_while_the_os_confirms_its_child() {
    let owner = DaemonGeneration::new();
    let (shard, ledger) = (SharedBytes::default(), SharedBytes::default());
    let resource = terminal_of(owner);
    let operation = OperationId::new();
    let identity = process(31, "token");
    let running = terminal_snapshot(vec![terminal_record(
        &resource,
        operation,
        TerminalRuntimeState::Running,
        Some(identity.clone()),
    )]);
    let state = writer(
        ResourceKind::Terminal,
        owner,
        &shard,
        &ledger,
        probe_for(31, "token"),
        (2, 2),
    );

    let report = state
        .commit(&json!({}), &project_terminals(&running, owner))
        .unwrap();

    assert_eq!(report.running, 1);
    let entry = shard_of(&shard, owner).resource(&resource).unwrap().clone();
    assert_eq!(entry.state, ResourceState::Running);
    assert!(entry.process.unwrap().is_verifiable());
    let ledger_document = ledger_of(&ledger);
    assert_eq!(
        ledger_document.claim(&resource).unwrap().state,
        ClaimState::Live
    );
    assert_eq!(
        ledger_document.operation(&operation).unwrap().outcome,
        OperationOutcome::Spawned
    );
}

#[test]
fn a_running_record_the_os_cannot_confirm_never_becomes_owned() {
    let owner = DaemonGeneration::new();
    let (shard, ledger) = (SharedBytes::default(), SharedBytes::default());
    let resource = terminal_of(owner);
    let operation = OperationId::new();
    let running = terminal_snapshot(vec![terminal_record(
        &resource,
        operation,
        TerminalRuntimeState::Running,
        Some(process(41, "token")),
    )]);
    let records = project_terminals(&running, owner);

    // The pid answers with somebody else's token: nothing about this child is
    // proved, so its capacity is kept and its record is not owned.
    let state = writer(
        ResourceKind::Terminal,
        owner,
        &shard,
        &ledger,
        probe_for(41, "reused"),
        (2, 2),
    );
    let report = state.commit(&json!({}), &records).unwrap();
    assert_eq!(report.unknown, 1);
    assert_eq!(
        shard_of(&shard, owner).resource(&resource).unwrap().state,
        ResourceState::OwnershipUnknown
    );
    let ledger_document = ledger_of(&ledger);
    assert_eq!(
        ledger_document.claim(&resource).unwrap().state,
        ClaimState::Reserved,
        "a child may exist, so the capacity stays held"
    );
    assert_eq!(
        ledger_document.operation(&operation).unwrap().outcome,
        OperationOutcome::Reserved
    );
    assert_eq!(shard_of(&shard, owner).live_resources(), 0);

    // Later agreement does not restore ownership: the window in which the OS
    // could have answered has passed.
    let state = writer(
        ResourceKind::Terminal,
        owner,
        &shard,
        &ledger,
        probe_for(41, "token"),
        (2, 2),
    );
    state.commit(&json!({}), &records).unwrap();
    assert_eq!(
        shard_of(&shard, owner).resource(&resource).unwrap().state,
        ResourceState::OwnershipUnknown
    );
}

#[test]
fn a_definite_spawn_failure_releases_its_capacity_exactly_once() {
    let owner = DaemonGeneration::new();
    let (shard, ledger) = (SharedBytes::default(), SharedBytes::default());
    let resource = terminal_of(owner);
    let operation = OperationId::new();
    let reserved = terminal_snapshot(vec![terminal_record(
        &resource,
        operation,
        TerminalRuntimeState::Reserved,
        None,
    )]);
    let failed = terminal_snapshot(vec![terminal_record(
        &resource,
        operation,
        TerminalRuntimeState::SpawnFailed,
        None,
    )]);
    let state = writer(
        ResourceKind::Terminal,
        owner,
        &shard,
        &ledger,
        FakeProbe::new(),
        (2, 2),
    );
    state
        .commit(&json!({}), &project_terminals(&reserved, owner))
        .unwrap();

    let report = state
        .commit(&json!({}), &project_terminals(&failed, owner))
        .unwrap();

    assert_eq!(report.failed, 1);
    let ledger_document = ledger_of(&ledger);
    assert_eq!(
        ledger_document.claim(&resource).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(ledger_document.pool_used(ResourceKind::Terminal), 0);
    assert_eq!(
        shard_of(&shard, owner).resource(&resource).unwrap().state,
        ResourceState::OwnershipUnknown
    );
    // Repeating it neither releases twice nor rewrites the sealed answer.
    let revision = ledger_document.claim(&resource).unwrap().revision;
    state
        .commit(&json!({}), &project_terminals(&failed, owner))
        .unwrap();
    assert_eq!(
        ledger_of(&ledger).claim(&resource).unwrap().revision,
        revision
    );
}

#[test]
fn an_exit_releases_capacity_once_and_a_reclaimed_record_is_not_resurrected() {
    let owner = DaemonGeneration::new();
    let (shard, ledger) = (SharedBytes::default(), SharedBytes::default());
    let resource = terminal_of(owner);
    let operation = OperationId::new();
    let running = terminal_snapshot(vec![terminal_record(
        &resource,
        operation,
        TerminalRuntimeState::Running,
        Some(process(51, "token")),
    )]);
    let state = writer(
        ResourceKind::Terminal,
        owner,
        &shard,
        &ledger,
        probe_for(51, "token"),
        (2, 2),
    );
    state
        .commit(&json!({}), &project_terminals(&running, owner))
        .unwrap();

    state.publish_exit(&resource, 0).unwrap();

    let document = shard_of(&shard, owner);
    assert_eq!(
        document.unacked_outbox(),
        0,
        "the owner reclaimed its outbox"
    );
    assert!(
        document.resource(&resource).is_none(),
        "a fully consumed exit is forgotten"
    );
    let ledger_document = ledger_of(&ledger);
    assert_eq!(
        ledger_document.claim(&resource).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(collectable(&document, &ledger_document), Ok(()));

    // The store keeps saving the exited record afterwards. It must not take a
    // second claim for a resource whose capacity was already released.
    let exited = terminal_snapshot(vec![terminal_record(
        &resource,
        operation,
        TerminalRuntimeState::Exited,
        Some(process(51, "token")),
    )]);
    let report = state
        .commit(&json!({}), &project_terminals(&exited, owner))
        .unwrap();
    assert_eq!(report.ended, 1);
    assert!(shard_of(&shard, owner).resource(&resource).is_none());
    assert_eq!(ledger_of(&ledger).pool_used(ResourceKind::Terminal), 0);

    // A second publication has nothing left to publish, and refuses instead of
    // releasing anything twice.
    assert_eq!(
        state.publish_exit(&resource, 0).unwrap_err().refusal(),
        Some(ResourceError::UnknownResource)
    );
}

#[test]
fn a_draining_exit_and_a_new_spawn_both_survive_the_same_allocator() {
    let ledger = SharedBytes::default();
    let (draining_bytes, active_bytes) = (SharedBytes::default(), SharedBytes::default());
    let draining = DaemonGeneration::new();
    let active = DaemonGeneration::new();
    let old_resource = terminal_of(draining);
    let new_resource = terminal_of(active);
    let old_operation = OperationId::new();
    let new_operation = OperationId::new();

    // The draining owner already holds a running child of its own.
    let old_running = terminal_snapshot(vec![terminal_record(
        &old_resource,
        old_operation,
        TerminalRuntimeState::Running,
        Some(process(61, "old")),
    )]);
    writer(
        ResourceKind::Terminal,
        draining,
        &draining_bytes,
        &ledger,
        probe_for(61, "old"),
        (4, 4),
    )
    .commit(&json!({}), &project_terminals(&old_running, draining))
    .unwrap();

    let new_running = terminal_snapshot(vec![terminal_record(
        &new_resource,
        new_operation,
        TerminalRuntimeState::Running,
        Some(process(62, "new")),
    )]);
    let barrier = Arc::new(Barrier::new(2));
    let exit = {
        let (barrier, ledger, shard) =
            (Arc::clone(&barrier), ledger.clone(), draining_bytes.clone());
        let resource = old_resource.clone();
        std::thread::spawn(move || {
            let state = writer(
                ResourceKind::Terminal,
                draining,
                &shard,
                &ledger,
                FakeProbe::new(),
                (4, 4),
            );
            barrier.wait();
            state.publish_exit(&resource, 3).unwrap();
        })
    };
    let spawn = {
        let (barrier, ledger, shard) = (barrier, ledger.clone(), active_bytes.clone());
        std::thread::spawn(move || {
            let state = writer(
                ResourceKind::Terminal,
                active,
                &shard,
                &ledger,
                probe_for(62, "new"),
                (4, 4),
            );
            barrier.wait();
            state
                .commit(&json!({}), &project_terminals(&new_running, active))
                .unwrap();
        })
    };
    exit.join().unwrap();
    spawn.join().unwrap();

    // Both transitions are durable. A whole-snapshot store would have kept one.
    let ledger_document = ledger_of(&ledger);
    assert_eq!(
        ledger_document.operation(&new_operation).unwrap().outcome,
        OperationOutcome::Spawned,
        "the active generation's spawn survived"
    );
    assert_eq!(
        ledger_document.claim(&old_resource).unwrap().state,
        ClaimState::Released,
        "the draining generation's exit survived"
    );
    assert_eq!(ledger_document.pool_used(ResourceKind::Terminal), 1);
    assert_eq!(
        shard_of(&active_bytes, active)
            .resource(&new_resource)
            .unwrap()
            .state,
        ResourceState::Running
    );
    assert!(
        shard_of(&draining_bytes, draining)
            .resource(&old_resource)
            .is_none()
    );
}

#[test]
fn a_lost_race_is_retried_and_a_writer_that_never_wins_fails_closed() {
    let owner = DaemonGeneration::new();
    let (shard, ledger) = (SharedBytes::default(), SharedBytes::default());
    let resource = terminal_of(owner);
    let records = project_terminals(
        &terminal_snapshot(vec![terminal_record(
            &resource,
            OperationId::new(),
            TerminalRuntimeState::Reserved,
            None,
        )]),
        owner,
    );

    // Losing the first comparison is a race, not an answer: the retry converges.
    let flaky = OwnerRuntimeState::new(
        ResourceKind::Terminal,
        OwnerShard::new(MemoryFile::new(&shard), owner),
        ResourceAllocator::new(FlakyFile::new(&ledger, 1), policy(2, 2)),
        Box::new(FakeProbe::new()),
        Box::new(FakeClock::at(1)),
    );
    flaky.commit(&json!({}), &records).unwrap();
    assert!(ledger_of(&ledger).claim(&resource).is_some());

    // A writer that never wins gives up with the typed refusal instead of
    // erasing the document that keeps beating it.
    let hopeless = OwnerRuntimeState::new(
        ResourceKind::Terminal,
        OwnerShard::new(MemoryFile::new(&shard), owner),
        ResourceAllocator::new(FlakyFile::new(&ledger, usize::MAX), policy(2, 2)),
        Box::new(FakeProbe::new()),
        Box::new(FakeClock::at(1)),
    );
    let other = terminal_of(owner);
    let more = project_terminals(
        &terminal_snapshot(vec![terminal_record(
            &other,
            OperationId::new(),
            TerminalRuntimeState::Reserved,
            None,
        )]),
        owner,
    );
    assert_eq!(
        hopeless.commit(&json!({}), &more).unwrap_err().refusal(),
        Some(ResourceError::StaleRevision)
    );
    assert!(ledger_of(&ledger).claim(&other).is_none());
}

#[test]
fn a_store_that_cannot_serialize_or_write_refuses_the_save() {
    let owner = DaemonGeneration::new();
    let ledger = SharedBytes::default();
    let unwritable = SharedBytes::default();
    unwritable.set("{not a shard");
    let resource = terminal_of(owner);
    let mut terminals = TerminalShardStore::new(writer(
        ResourceKind::Terminal,
        owner,
        &unwritable,
        &ledger,
        FakeProbe::new(),
        (2, 2),
    ));
    assert!(
        terminals
            .save(terminal_snapshot(vec![terminal_record(
                &resource,
                OperationId::new(),
                TerminalRuntimeState::Reserved,
                None,
            )]))
            .is_err()
    );
    let mut agents = AgentShardStore::new(writer(
        ResourceKind::Agent,
        owner,
        &unwritable,
        &ledger,
        FakeProbe::new(),
        (2, 2),
    ));
    assert!(
        agents
            .save(agent_snapshot(vec![agent_record(
                &resource,
                OperationId::new(),
                TerminalRuntimeState::Reserved,
                None,
            )]))
            .is_err()
    );
}

#[test]
fn an_agent_save_writes_only_its_own_generations_records() {
    let owner = DaemonGeneration::new();
    let (shard, ledger) = (SharedBytes::default(), SharedBytes::default());
    let mine = terminal_of(owner);
    let theirs = terminal_of(DaemonGeneration::new());
    let mut store = AgentShardStore::new(writer(
        ResourceKind::Agent,
        owner,
        &shard,
        &ledger,
        FakeProbe::new(),
        (2, 2),
    ));

    store
        .save(agent_snapshot(vec![
            agent_record(
                &mine,
                OperationId::new(),
                TerminalRuntimeState::Reserved,
                None,
            ),
            agent_record(
                &theirs,
                OperationId::new(),
                TerminalRuntimeState::Running,
                Some(process(71, "token")),
            ),
        ]))
        .unwrap();

    let document = shard_of(&shard, owner);
    assert_eq!(document.resources.len(), 1);
    let payload: RuntimeStoreSnapshot =
        serde_json::from_value(document.payload(ResourceKind::Agent).unwrap().clone()).unwrap();
    assert_eq!(payload.records.len(), 1);
    assert_eq!(payload.records[0].runtime.terminal, mine);
    assert!(ledger_of(&ledger).claim(&theirs).is_none());
}

#[test]
fn legacy_records_are_adopted_into_the_shards_of_the_generations_they_name() {
    let source = MemorySource::new();
    let ledger = SharedBytes::default();
    let allocator = ResourceAllocator::new(MemoryFile::new(&ledger), policy(4, 4));
    let first = DaemonGeneration::new();
    let second = DaemonGeneration::new();
    let confirmed = terminal_of(first);
    let fixed = terminal_of(first);
    let elsewhere = terminal_of(second);
    let legacy = terminal_snapshot(vec![
        terminal_record(
            &confirmed,
            OperationId::new(),
            TerminalRuntimeState::Running,
            Some(process(81, "real")),
        ),
        terminal_record(
            &fixed,
            OperationId::new(),
            TerminalRuntimeState::Running,
            Some(process(82, "daemon-owned-pty")),
        ),
        terminal_record(
            &elsewhere,
            OperationId::new(),
            TerminalRuntimeState::Reserved,
            None,
        ),
    ]);

    let summary = migrate_terminals(
        &source,
        &allocator,
        &probe_for(81, "real"),
        &FakeClock::at(5),
        &legacy,
    )
    .unwrap();

    assert_eq!(summary.owners, 2);
    assert_eq!(summary.adopted, 1);
    // The fixed token proves nothing, and a record with no child cannot be owned.
    assert_eq!(
        summary
            .unknown
            .iter()
            .map(|unknown| (unknown.resource.clone(), unknown.refusal))
            .collect::<Vec<_>>(),
        vec![
            (fixed.clone(), AdoptionRefusal::UnverifiableIdentity),
            (elsewhere.clone(), AdoptionRefusal::UnverifiableIdentity),
        ]
    );
    let adopted = shard_of(&source.bytes(first), first);
    assert_eq!(
        adopted.resource(&confirmed).unwrap().state,
        ResourceState::Running
    );
    assert_eq!(
        adopted.resource(&fixed).unwrap().state,
        ResourceState::OwnershipUnknown
    );
    assert_eq!(adopted.live_resources(), 1);
    let payload: TerminalStoreSnapshot =
        serde_json::from_value(adopted.payload(ResourceKind::Terminal).unwrap().clone()).unwrap();
    assert_eq!(payload.records.len(), 2, "only this owner's records");
    // A confirmed child holds capacity; a record that proves nothing holds none.
    let ledger_document = ledger_of(&ledger);
    assert_eq!(
        ledger_document.claim(&confirmed).unwrap().state,
        ClaimState::Live
    );
    assert!(ledger_document.claim(&fixed).is_none());
    assert_eq!(ledger_document.pool_used(ResourceKind::Terminal), 1);
    assert_eq!(
        shard_of(&source.bytes(second), second)
            .resource(&elsewhere)
            .unwrap()
            .state,
        ResourceState::OwnershipUnknown
    );

    // Re-running the migration after a crash adopts the same records once.
    let repeated = migrate_terminals(
        &source,
        &allocator,
        &probe_for(81, "real"),
        &FakeClock::at(5),
        &legacy,
    )
    .unwrap();
    assert_eq!(repeated, summary);
    assert_eq!(shard_of(&source.bytes(first), first).resources.len(), 2);
    assert_eq!(ledger_of(&ledger).claims.len(), 1);
}

#[test]
fn legacy_agent_records_are_adopted_with_their_producer_operation() {
    let source = MemorySource::new();
    let ledger = SharedBytes::default();
    let allocator = ResourceAllocator::new(MemoryFile::new(&ledger), policy(4, 4));
    let owner = DaemonGeneration::new();
    let resource = terminal_of(owner);
    let operation = OperationId::new();
    let legacy = agent_snapshot(vec![agent_record(
        &resource,
        operation,
        TerminalRuntimeState::Running,
        Some(process(91, "real")),
    )]);

    let summary = migrate_agents(
        &source,
        &allocator,
        &probe_for(91, "real"),
        &FakeClock::at(5),
        &legacy,
    )
    .unwrap();

    assert_eq!((summary.owners, summary.adopted), (1, 1));
    assert!(summary.unknown.is_empty());
    let document = shard_of(&source.bytes(owner), owner);
    assert_eq!(
        document.resource(&resource).unwrap().kind,
        ResourceKind::Agent
    );
    assert_eq!(document.resource(&resource).unwrap().operation, operation);
    assert_eq!(
        ledger_of(&ledger).operation(&operation).unwrap().outcome,
        OperationOutcome::Spawned
    );
}

#[test]
fn a_legacy_store_this_build_cannot_trust_is_never_adopted() {
    assert_eq!(
        read_legacy_agents("{not json").unwrap_err(),
        ResourceError::Corrupt
    );
    assert_eq!(
        read_legacy_agents(r#"{"schema_version":999,"records":[]}"#).unwrap_err(),
        ResourceError::UnknownSchema
    );
    assert_eq!(
        read_legacy_terminals("{not json").unwrap_err(),
        ResourceError::Corrupt
    );
    assert_eq!(
        read_legacy_terminals(r#"{"schema_version":999,"records":[]}"#).unwrap_err(),
        ResourceError::UnknownSchema
    );

    // A well-formed store is adopted, and one whose generation binding
    // contradicts its own records is not.
    let owner = DaemonGeneration::new();
    let resource = terminal_of(owner);
    let record = agent_record(
        &resource,
        OperationId::new(),
        TerminalRuntimeState::Running,
        Some(process(101, "daemon-owned-agent-pty")),
    );
    let (reconciled, _) = agent_snapshot(vec![record.clone()]).reconcile_after_daemon_restart();
    let bytes = serde_json::to_string(&reconciled).unwrap();
    assert_eq!(read_legacy_agents(&bytes).unwrap().records.len(), 1);
    let mut contradictory = reconciled;
    contradictory.generation.terminals.clear();
    assert_eq!(
        read_legacy_agents(&serde_json::to_string(&contradictory).unwrap()).unwrap_err(),
        ResourceError::Corrupt
    );
    assert_eq!(
        read_legacy_terminals(&serde_json::to_string(&terminal_snapshot(vec![])).unwrap())
            .unwrap()
            .records
            .len(),
        0
    );
}

#[test]
fn a_shard_that_cannot_be_reached_stops_the_migration_without_adopting() {
    struct Unbindable;
    impl ShardSource for Unbindable {
        fn generations(&self) -> io::Result<Vec<DaemonGeneration>> {
            Ok(Vec::new())
        }
        fn open(&self, _: DaemonGeneration) -> io::Result<OwnerShard> {
            Err(io::Error::other("the shard directory is unwritable"))
        }
    }

    let ledger = SharedBytes::default();
    let allocator = ResourceAllocator::new(MemoryFile::new(&ledger), policy(4, 4));
    let owner = DaemonGeneration::new();
    let resource = terminal_of(owner);
    let legacy = terminal_snapshot(vec![terminal_record(
        &resource,
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    )]);
    assert!(
        migrate_terminals(
            &Unbindable,
            &allocator,
            &FakeProbe::new(),
            &FakeClock::at(1),
            &legacy,
        )
        .unwrap_err()
        .refusal()
        .is_none()
    );
}

#[test]
fn hydrating_merges_every_retained_shard_and_fences_records_it_cannot_own() {
    let source = MemorySource::new();
    let ledger = SharedBytes::default();
    let first = DaemonGeneration::new();
    let second = DaemonGeneration::new();
    let running = terminal_of(first);
    let other = terminal_of(second);
    for (owner, resource, pid) in [(first, running.clone(), 111), (second, other.clone(), 112)] {
        let bytes = source.bytes(owner);
        let state = writer(
            ResourceKind::Terminal,
            owner,
            &bytes,
            &ledger,
            probe_for(pid, "token"),
            (4, 4),
        );
        let snapshot = terminal_snapshot(vec![terminal_record(
            &resource,
            OperationId::new(),
            TerminalRuntimeState::Running,
            Some(process(pid, "token")),
        )]);
        TerminalShardStore::new(state).save(snapshot).unwrap();
    }

    let (hydrated, interrupted) = hydrate_terminals(&source).unwrap();

    assert_eq!(hydrated.records.len(), 2);
    assert_eq!(interrupted, 2, "no restarted process owns another's PTY");
    assert!(hydrated.records.iter().all(|record| record.state
        == TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown)));
    // A shard with no payload of this kind contributes nothing rather than failing.
    let empty = DaemonGeneration::new();
    source.open(empty).unwrap().update(|_| Ok(())).unwrap();
    assert_eq!(hydrate_terminals(&source).unwrap().0.records.len(), 2);
    assert_eq!(hydrate_agents(&source).unwrap().0.records.len(), 0);
}

#[test]
fn hydrating_the_agent_snapshot_merges_and_reconciles_every_shard() {
    let source = MemorySource::new();
    let ledger = SharedBytes::default();
    let owner = DaemonGeneration::new();
    let resource = terminal_of(owner);
    let bytes = source.bytes(owner);
    AgentShardStore::new(writer(
        ResourceKind::Agent,
        owner,
        &bytes,
        &ledger,
        probe_for(121, "token"),
        (4, 4),
    ))
    .save(agent_snapshot(vec![agent_record(
        &resource,
        OperationId::new(),
        TerminalRuntimeState::Running,
        Some(process(121, "token")),
    )]))
    .unwrap();

    let (hydrated, interrupted) = hydrate_agents(&source).unwrap();

    assert_eq!(interrupted, 1);
    assert_eq!(
        hydrated.records[0].state,
        TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown)
    );
    assert_eq!(
        hydrated.records[0].outcome,
        DurableOperationOutcome::OwnershipUnknown
    );
    hydrated.validate_ownership().unwrap();
}

#[test]
fn a_payload_this_build_cannot_trust_fails_the_whole_hydrate() {
    let owner = DaemonGeneration::new();
    let foreign = DaemonGeneration::new();
    for payload in [
        json!({"schema_version": 1, "records": "not a list"}),
        // A record naming another generation cannot be in this owner's document.
        serde_json::to_value(terminal_snapshot(vec![terminal_record(
            &terminal_of(foreign),
            OperationId::new(),
            TerminalRuntimeState::Reserved,
            None,
        )]))
        .unwrap(),
    ] {
        let source = MemorySource::new();
        let bytes = source.bytes(owner);
        OwnerShard::new(MemoryFile::new(&bytes), owner)
            .update(|document| {
                document.set_payload(ResourceKind::Terminal, payload.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            hydrate_terminals(&source).unwrap_err().refusal(),
            Some(ResourceError::Corrupt)
        );
    }
    for payload in [
        json!({"schema_version": 4, "records": "not a list"}),
        serde_json::to_value(agent_snapshot(vec![agent_record(
            &terminal_of(foreign),
            OperationId::new(),
            TerminalRuntimeState::Reserved,
            None,
        )]))
        .unwrap(),
    ] {
        let source = MemorySource::new();
        let bytes = source.bytes(owner);
        OwnerShard::new(MemoryFile::new(&bytes), owner)
            .update(|document| {
                document.set_payload(ResourceKind::Agent, payload.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            hydrate_agents(&source).unwrap_err().refusal(),
            Some(ResourceError::Corrupt)
        );
    }
}

#[test]
fn a_terminal_payload_the_snapshot_itself_refuses_fails_closed() {
    let source = MemorySource::new();
    let owner = DaemonGeneration::new();
    let mut record = terminal_record(
        &terminal_of(owner),
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    );
    record.launch.schema_version += 1;
    let bytes = source.bytes(owner);
    OwnerShard::new(MemoryFile::new(&bytes), owner)
        .update(|document| {
            document.set_payload(
                ResourceKind::Terminal,
                serde_json::to_value(terminal_snapshot(vec![record.clone()])).unwrap(),
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(
        hydrate_terminals(&source).unwrap_err().refusal(),
        Some(ResourceError::Corrupt)
    );
}

#[test]
fn an_unreadable_shard_directory_is_never_read_as_empty() {
    assert!(hydrate_agents(&MemorySource::failing()).is_err());
    assert!(hydrate_terminals(&MemorySource::failing()).is_err());
    assert!(live_census(&MemorySource::failing()).is_err());
}

#[test]
fn the_census_counts_the_live_runtime_of_every_retained_generation_per_kind() {
    let source = MemorySource::new();
    let ledger = SharedBytes::default();
    let first = DaemonGeneration::new();
    let second = DaemonGeneration::new();
    let agent = terminal_of(first);
    let terminal = terminal_of(second);
    AgentShardStore::new(writer(
        ResourceKind::Agent,
        first,
        &source.bytes(first),
        &ledger,
        FakeProbe::new(),
        (4, 4),
    ))
    .save(agent_snapshot(vec![agent_record(
        &agent,
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    )]))
    .unwrap();
    TerminalShardStore::new(writer(
        ResourceKind::Terminal,
        second,
        &source.bytes(second),
        &ledger,
        FakeProbe::new(),
        (4, 4),
    ))
    .save(terminal_snapshot(vec![terminal_record(
        &terminal,
        OperationId::new(),
        TerminalRuntimeState::Reserved,
        None,
    )]))
    .unwrap();

    assert_eq!(live_census(&source).unwrap(), (1, 1));
}

/// One dead owner's shard, holding a resource in `state` with `process`.
fn dead_owner(
    state: TerminalRuntimeState,
    process_identity: Option<ProcessIdentity>,
    probe: &FakeProbe,
) -> (
    DaemonGeneration,
    TerminalRef,
    SharedBytes,
    SharedBytes,
    CollectionReport,
) {
    let owner = DaemonGeneration::new();
    let (shard, ledger) = (SharedBytes::default(), SharedBytes::default());
    let resource = terminal_of(owner);
    let snapshot = terminal_snapshot(vec![terminal_record(
        &resource,
        OperationId::new(),
        state,
        process_identity,
    )]);
    // The dead owner wrote its own state while it was alive: its probe agreed.
    writer(
        ResourceKind::Terminal,
        owner,
        &shard,
        &ledger,
        probe_for(131, "token"),
        (4, 4),
    )
    .commit(&json!({}), &project_terminals(&snapshot, owner))
    .unwrap();
    let allocator = ResourceAllocator::new(MemoryFile::new(&ledger), policy(4, 4));
    let report = collect_dead_owner(
        &OwnerShard::new(MemoryFile::new(&shard), owner),
        &allocator,
        probe,
        &FakeClock::at(9),
    )
    .unwrap();
    (owner, resource, shard, ledger, report)
}

#[test]
fn a_dead_owners_child_that_is_proved_gone_gives_its_capacity_back() {
    let (owner, resource, shard, ledger, report) = dead_owner(
        TerminalRuntimeState::Running,
        Some(process(131, "token")),
        &FakeProbe::new(),
    );

    assert_eq!(
        report,
        CollectionReport {
            consumed: 0,
            unknown: 1,
            released: 1,
            retained: 0,
            reclaimed: 0,
        }
    );
    let document = shard_of(&shard, owner);
    assert_eq!(
        document.resource(&resource).unwrap().state,
        ResourceState::OwnershipUnknown
    );
    let ledger_document = ledger_of(&ledger);
    assert_eq!(
        ledger_document.claim(&resource).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(ledger_document.pool_used(ResourceKind::Terminal), 0);
    assert_eq!(collectable(&document, &ledger_document), Ok(()));
}

#[test]
fn a_dead_owners_orphan_that_is_still_running_keeps_its_capacity() {
    for probe in [
        probe_for(131, "token"),
        FakeProbe::new().with(131, ProbeAnswer::Denied),
    ] {
        let (owner, resource, shard, ledger, report) = dead_owner(
            TerminalRuntimeState::Running,
            Some(process(131, "token")),
            &probe,
        );

        assert_eq!(
            (report.unknown, report.released, report.retained),
            (1, 0, 1)
        );
        // The PTY master died with its daemon, so the record is never adopted.
        assert_eq!(
            shard_of(&shard, owner).resource(&resource).unwrap().state,
            ResourceState::OwnershipUnknown
        );
        let ledger_document = ledger_of(&ledger);
        assert_eq!(
            ledger_document.claim(&resource).unwrap().state,
            ClaimState::Live,
            "something is still running, so the capacity is still spent"
        );
        assert_eq!(ledger_document.pool_used(ResourceKind::Terminal), 1);
    }
}

#[test]
fn a_dead_owners_bare_reservation_ends_ambiguous_and_keeps_its_capacity() {
    let (owner, resource, shard, ledger, report) =
        dead_owner(TerminalRuntimeState::Reserved, None, &FakeProbe::new());

    assert_eq!(
        (report.unknown, report.released, report.retained),
        (1, 0, 1)
    );
    assert_eq!(
        shard_of(&shard, owner).resource(&resource).unwrap().state,
        ResourceState::OwnershipUnknown
    );
    let ledger_document = ledger_of(&ledger);
    assert_eq!(
        ledger_document
            .operation(&ledger_document.claims[0].operation)
            .unwrap()
            .outcome,
        OperationOutcome::Ambiguous,
        "a child may have been spawned this owner never recorded"
    );
    assert_eq!(
        ledger_document.claim(&resource).unwrap().state,
        ClaimState::Reserved
    );
}

#[test]
fn a_dead_owners_published_exit_is_applied_and_its_outbox_reclaimed() {
    let owner = DaemonGeneration::new();
    let (shard, ledger) = (SharedBytes::default(), SharedBytes::default());
    let resource = terminal_of(owner);
    let snapshot = terminal_snapshot(vec![terminal_record(
        &resource,
        OperationId::new(),
        TerminalRuntimeState::Running,
        Some(process(141, "token")),
    )]);
    let owner_shard = OwnerShard::new(MemoryFile::new(&shard), owner);
    writer(
        ResourceKind::Terminal,
        owner,
        &shard,
        &ledger,
        probe_for(141, "token"),
        (4, 4),
    )
    .commit(&json!({}), &project_terminals(&snapshot, owner))
    .unwrap();
    // The owner published its child's exit and died before anything applied it.
    owner_shard
        .update(|document| document.commit_exit(&resource, 0))
        .unwrap();

    let allocator = ResourceAllocator::new(MemoryFile::new(&ledger), policy(4, 4));
    let report = collect_dead_owner(
        &owner_shard,
        &allocator,
        &FakeProbe::new(),
        &FakeClock::at(9),
    )
    .unwrap();

    assert_eq!(
        report,
        CollectionReport {
            consumed: 1,
            unknown: 0,
            released: 0,
            retained: 0,
            reclaimed: 1,
        }
    );
    let document = shard_of(&shard, owner);
    assert!(document.resource(&resource).is_none());
    let ledger_document = ledger_of(&ledger);
    assert_eq!(
        ledger_document.claim(&resource).unwrap().state,
        ClaimState::Released
    );
    assert_eq!(collectable(&document, &ledger_document), Ok(()));
}

#[test]
fn collecting_a_generation_with_nothing_to_collect_changes_nothing() {
    let owner = DaemonGeneration::new();
    let (shard, ledger) = (SharedBytes::default(), SharedBytes::default());
    let allocator = ResourceAllocator::new(MemoryFile::new(&ledger), policy(1, 1));
    let report = collect_dead_owner(
        &OwnerShard::new(MemoryFile::new(&shard), owner),
        &allocator,
        &FakeProbe::new(),
        &FakeClock::at(9),
    )
    .unwrap();
    assert_eq!(report, CollectionReport::default());
    assert_eq!(
        shard.get(),
        None,
        "an absent shard is not created to say so"
    );
}

#[test]
fn the_retired_legacy_suffix_is_stable() {
    assert_eq!(RETIRED_LEGACY_SUFFIX, ".migrated");
}
