//! agent ipc の振る舞いを固定するテスト。

use std::collections::BTreeSet;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use super::*;
use crate::usecase::terminal::SnapshotWire;
use crate::usecase::terminal_owner::JsonTerminalOwner as TerminalOwner;
use serde_json::{Value, json};
use usagi_core::domain::{
    agent::Agent,
    id::{AgentId, AgentResumeSourceId, ClientId, RequestId},
    supervisor::{SupervisorRunId, TaskId},
};
use usagi_core::infrastructure::ipc::TerminalAction;

trait JsonAgentTerminalActor {
    fn handle_terminal(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        request_id: RequestId,
        action: TerminalAction,
        request: TerminalRequest,
        wire: SnapshotWire,
    ) -> TerminalOutcome<Value>;
}

impl<T: AgentTerminalActor> JsonAgentTerminalActor for T {
    fn handle_terminal(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        request_id: RequestId,
        action: TerminalAction,
        request: TerminalRequest,
        wire: SnapshotWire,
    ) -> TerminalOutcome<Value> {
        let matching = matches!(
            (&action, &request),
            (TerminalAction::Attach, TerminalRequest::Attach { .. })
                | (TerminalAction::Launch, TerminalRequest::Launch { .. })
                | (TerminalAction::Inventory, TerminalRequest::Inventory { .. })
                | (TerminalAction::Resume, TerminalRequest::Resume { .. })
                | (TerminalAction::Resync, TerminalRequest::Resync { .. })
                | (TerminalAction::Input, TerminalRequest::Input { .. })
                | (
                    TerminalAction::InputOutcome,
                    TerminalRequest::InputOutcome { .. }
                )
                | (TerminalAction::Resize, TerminalRequest::Resize { .. })
                | (TerminalAction::Detach, TerminalRequest::Detach { .. })
                | (
                    TerminalAction::CompletedInventory,
                    TerminalRequest::CompletedInventory { .. }
                )
                | (TerminalAction::Observe, TerminalRequest::Observe { .. })
                | (TerminalAction::Dismiss, TerminalRequest::Dismiss { .. })
        );
        if !matching {
            return TerminalOutcome::Handled(Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "terminal action does not match its payload",
            )));
        }
        match AgentTerminalActor::handle(
            self,
            TerminalRequestContext {
                connection,
                client,
                request: request_id,
            },
            request,
        ) {
            TerminalOutcome::Handled(result) => TerminalOutcome::Handled(
                result
                    .map(|response| crate::usecase::terminal_owner::response_json(response, wire)),
            ),
            TerminalOutcome::NotOwned => TerminalOutcome::NotOwned,
        }
    }
}
use crate::usecase::{
    agy::{AgyAdapter, AgyProvision, AgyProvisionFailure, AgyProvisioner},
    claude::{ClaudeAdapter, ClaudeProvision, ClaudeProvisionFailure, ClaudeProvisioner},
    codex::{CodexAdapter, CodexProvision, CodexProvisionFailure, CodexProvisioner},
    generation::ProcessIdentity,
    runtime::{
        AdapterError, AgentAdapter, ProvisionContext, ResolvedLaunch, RuntimeStore,
        RuntimeStoreSnapshot, SpawnFailure, SpawnProvision,
    },
    terminal::{Output, PtyWriteError},
};
use usagi_core::domain::agent::{AgentCapability, AgentProfile, DurableLaunchSnapshot, LaunchPlan};
use usagi_core::infrastructure::ipc::TerminalGeometry;

// ---- fakes ---------------------------------------------------------------

#[derive(Default)]
struct Store {
    saves: usize,
    fail_after: Option<usize>,
    snapshot_path: Option<PathBuf>,
}
impl RuntimeStore for Store {
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), ()> {
        self.saves += 1;
        if self.fail_after.is_some_and(|limit| self.saves > limit) {
            return Err(());
        }
        if let Some(path) = &self.snapshot_path {
            let bytes = serde_json::to_vec(&snapshot).map_err(|_| ())?;
            std::fs::write(path, bytes).map_err(|_| ())?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Journal(Vec<Output>);
impl OutputJournal for Journal {
    fn append(&mut self, output: &Output) -> Result<(), ()> {
        self.0.push(output.clone());
        Ok(())
    }
}

#[derive(Default)]
struct Pty {
    writes: Vec<u8>,
    selected: Option<TerminalRef>,
    spawn: Option<SpawnFailure>,
    resized: Vec<(TerminalRef, Geometry)>,
    released: Vec<TerminalRef>,
    resize_failure: bool,
    write_failure: bool,
    terminate_success: bool,
    terminate_calls: usize,
    terminate_fail_at: Option<usize>,
    spawn_counter: Option<Arc<AtomicU32>>,
}
impl PtySpawner for Pty {
    fn spawn(
        &mut self,
        _: &DurableLaunchSnapshot,
        _: &SpawnProvision,
        _: &TerminalRef,
    ) -> Result<ProcessIdentity, SpawnFailure> {
        if let Some(counter) = &self.spawn_counter {
            let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
            return Ok(ProcessIdentity {
                pid: count,
                start_identity: format!("fake-agent-{count}"),
                process_group: count,
            });
        }
        match self.spawn {
            Some(failure) => Err(failure),
            None => Ok(ProcessIdentity {
                pid: 4321,
                start_identity: "fake-agent".into(),
                process_group: 4321,
            }),
        }
    }

    fn terminate_reap(
        &mut self,
        _: &TerminalRef,
    ) -> Result<(), super::super::runtime::TerminateReapError> {
        self.terminate_calls += 1;
        if self.terminate_fail_at == Some(self.terminate_calls) {
            return Err(super::super::runtime::TerminateReapError);
        }
        self.terminate_success
            .then_some(())
            .ok_or(super::super::runtime::TerminateReapError)
    }
}
impl PtyWriter for Pty {
    fn select_terminal(&mut self, terminal: &TerminalRef) {
        self.selected = Some(terminal.clone());
    }
    fn resize(&mut self, terminal: &TerminalRef, geometry: Geometry) -> Result<(), PtyWriteError> {
        self.resized.push((terminal.clone(), geometry));
        if self.resize_failure {
            Err(PtyWriteError { applied_prefix: 0 })
        } else {
            Ok(())
        }
    }
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
        if self.write_failure {
            return Err(PtyWriteError { applied_prefix: 0 });
        }
        self.writes.extend_from_slice(bytes);
        Ok(())
    }
    fn release(&mut self, terminal: &TerminalRef) -> bool {
        self.released.push(terminal.clone());
        true
    }
}

/// A fake Claude provisioner keeps the test independent of a real binary.
struct FakeProvisioner;
impl ClaudeProvisioner for FakeProvisioner {
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<ClaudeProvision, ClaudeProvisionFailure> {
        Ok(ClaudeProvision {
            working_directory: PathBuf::from("/worktree"),
            environment_allowlist: BTreeSet::new(),
            spawn: SpawnProvision::new([], vec![context.inject_mcp.to_string()]),
        })
    }
}

struct FakeCodexProvisioner;
impl CodexProvisioner for FakeCodexProvisioner {
    fn provision(
        &mut self,
        _context: &ProvisionContext,
    ) -> Result<CodexProvision, CodexProvisionFailure> {
        Ok(CodexProvision {
            working_directory: PathBuf::from("/worktree"),
            environment_allowlist: BTreeSet::new(),
            spawn: SpawnProvision::new([], Vec::new()),
        })
    }
}

struct FakeAgyProvisioner;
impl AgyProvisioner for FakeAgyProvisioner {
    fn provision(
        &mut self,
        _context: &ProvisionContext,
    ) -> Result<AgyProvision, AgyProvisionFailure> {
        Ok(AgyProvision {
            working_directory: PathBuf::from("/worktree"),
            environment_allowlist: BTreeSet::new(),
            spawn: SpawnProvision::new([], Vec::new()),
            system_prompt: "scoped test contract".into(),
        })
    }
}

/// Keeps broad runtime tests focused on their existing resume scenarios by
/// modelling a pre-v5 Claude adapter. Production v5 behavior is exercised
/// through `structured_claude_runtime` and the Claude adapter's own tests.
struct LegacyClaudeAdapter {
    inner: ClaudeAdapter<FakeProvisioner>,
}

impl usagi_core::usecase::agent::AgentProfileCatalog for LegacyClaudeAdapter {
    fn find(&self, profile_id: &AgentProfileId) -> Option<AgentProfile> {
        self.inner.find(profile_id)
    }
}

impl AgentAdapter for LegacyClaudeAdapter {
    fn resolve(&mut self, request: &LaunchRequest) -> Result<ResolvedLaunch, AdapterError> {
        let mut resolved = self.inner.resolve(request)?;
        if request.mode == LaunchMode::Interactive && !request.resume {
            debug_assert!(resolved.provider_resume.is_none());
            resolved.provider_resume = Some(ProviderResumeRef {
                provider: ProviderKind::Claude,
                native_session_id: ProviderSessionId::new(OperationId::new().to_string())
                    .expect("an operation UUID is a valid provider ID"),
                adapter_revision: resolved.snapshot.plan.profile_revision,
                scope: request.scope.clone(),
                provenance: ProviderCaptureProvenance::DaemonIssued,
                last_known_status: ProviderResumeStatus::Active,
                last_known_phase: Some(ProviderResumePhase::Starting),
            });
        }
        Ok(resolved)
    }
}

struct ProfileOverrideAdapter {
    profile: AgentProfile,
    inner: CodexAdapter<FakeCodexProvisioner>,
}

impl usagi_core::usecase::agent::AgentProfileCatalog for ProfileOverrideAdapter {
    fn find(&self, profile_id: &AgentProfileId) -> Option<AgentProfile> {
        (self.profile.id == *profile_id).then(|| self.profile.clone())
    }
}

impl AgentAdapter for ProfileOverrideAdapter {
    fn resolve(&mut self, request: &LaunchRequest) -> Result<ResolvedLaunch, AdapterError> {
        self.inner.resolve(request)
    }
}

struct FakeScope(Result<ResolvedAgentScope, ScopeResolveError>);
impl SessionScopeResolver for FakeScope {
    fn resolve_available_scope(
        &self,
        _: WorkspaceId,
        _: Option<SessionId>,
    ) -> Result<ResolvedAgentScope, ScopeResolveError> {
        self.0.clone()
    }
}

struct RootAndSessionScope {
    root: ResolvedAgentScope,
    session: ResolvedAgentScope,
}
impl SessionScopeResolver for RootAndSessionScope {
    fn resolve_available_scope(
        &self,
        _: WorkspaceId,
        session: Option<SessionId>,
    ) -> Result<ResolvedAgentScope, ScopeResolveError> {
        Ok(if session.is_some() {
            self.session.clone()
        } else {
            self.root.clone()
        })
    }
}

struct FixtureLocator(PathBuf);
impl ExecutableLocator for FixtureLocator {
    fn is_available(&self, executable: &str) -> bool {
        self.0.join(executable).is_file()
    }
}

/// A minimal generic terminal owner double so the shared owner can be tested
/// without a real PTY. It records the requests it receives and returns a
/// fixed inventory so the merge path can be exercised.
#[derive(Default)]
struct FakeGeneric {
    requests: usize,
    disconnects: usize,
    inventory: Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry>,
    completed: Vec<usagi_core::domain::terminal_visibility::CompletedTerminalEntry>,
}
impl TerminalOwnerPort for FakeGeneric {
    fn handle(
        &mut self,
        _: TerminalRequestContext,
        _: TerminalRequest,
    ) -> Result<TerminalResponse, ProtocolError> {
        self.requests += 1;
        Ok(TerminalResponse::Inventory(Vec::new()))
    }
    fn inventory(
        &self,
        _: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry> {
        self.inventory.clone()
    }
    fn completed_inventory(
        &self,
        _: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_visibility::CompletedTerminalEntry> {
        self.completed.clone()
    }
    fn disconnect(&mut self, _: ConnectionId) {
        self.disconnects += 1;
    }
}

// ---- helpers -------------------------------------------------------------

fn scope() -> ResolvedAgentScope {
    ResolvedAgentScope {
        worktree_id: WorktreeId::new(),
        working_directory: PathBuf::from("/worktree"),
    }
}

fn claude_registry() -> AdapterRegistry {
    let mut registry = AdapterRegistry::new();
    let adapter = LegacyClaudeAdapter {
        inner: ClaudeAdapter::new(FakeProvisioner),
    };
    let profile = adapter.inner.profile().clone();
    registry.register(profile, Box::new(adapter)).unwrap();
    registry
}

fn structured_claude_runtime() -> AgentRuntime {
    let mut registry = AdapterRegistry::new();
    let adapter = ClaudeAdapter::new(FakeProvisioner);
    registry
        .register(adapter.profile().clone(), Box::new(adapter))
        .unwrap();
    AgentRuntime::new(
        DaemonGeneration::new(),
        registry,
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    )
}

fn runtime() -> AgentRuntime {
    AgentRuntime::new(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    )
}

fn record_dispatch_status(
    runtime: &AgentRuntime,
    workspace: WorkspaceId,
    session: Option<SessionId>,
    profile: &str,
    model: &str,
    status: AgentStatus,
) {
    let agent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            session,
            AgentProfileId::new(profile).unwrap(),
            ModelSelector::new(model).unwrap(),
        )
        .unwrap();
    let operation =
        matches!(status, AgentStatus::Starting | AgentStatus::Running).then(OperationId::new);
    runtime
        .dispatch
        .transition_agent(agent.agent_id, status, operation)
        .unwrap();
}

#[test]
fn saturated_launch_sleeps_the_oldest_completed_resumable_agent() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let generation = DaemonGeneration::new();
    let mut agent = AgentRuntime::new(
        generation,
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            terminate_success: true,
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    // A one-slot fixture exercises the shipping 16-slot policy without
    // launching sixteen identical test processes.
    let mut one_slot = RuntimeCoordinator::new(1, 64 * 1024, 64);
    one_slot.activate_generation(generation).unwrap();
    agent.coordinator = one_slot;
    let first = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: None,
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    let first_runtime = agent
        .coordinator
        .runtime_for_terminal(&first.terminal)
        .unwrap();
    agent
        .reported_phases
        .insert(first_runtime.agent_runtime_id, AgentPhase::Ended);

    let second = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: None,
            },
            &FakeScope(Ok(resolved)),
        )
        .unwrap();

    let records = agent.coordinator.snapshot().records;
    assert_eq!(agent.concurrency().in_use, 1);
    assert!(records.iter().any(|record| {
        record.runtime.terminal == first.terminal
            && record.state == super::super::runtime::RuntimeState::Sleeping
    }));
    assert!(records.iter().any(|record| {
        record.runtime.terminal == second.terminal
            && record.state == super::super::runtime::RuntimeState::Running
    }));
    assert_eq!(agent.session_phase(session), AgentPhase::Running);
    assert!(agent.inventory(workspace).runtimes.iter().any(|runtime| {
        runtime.runtime.terminal == first.terminal
            && runtime.state == AgentRuntimeInventoryState::Sleeping
    }));
}

#[test]
fn saturated_capacity_selection_compares_every_completed_resume_candidate() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let generation = DaemonGeneration::new();
    let mut agent = AgentRuntime::new(
        generation,
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            terminate_success: true,
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    let mut three_slots = RuntimeCoordinator::new(3, 64 * 1024, 64);
    three_slots.activate_generation(generation).unwrap();
    agent.coordinator = three_slots;

    for _ in 0..3 {
        let admission = agent
            .launch(
                &OperationId::new().to_string(),
                &AgentLaunchIntent {
                    workspace,
                    session: Some(session),
                    profile: None,
                },
                &FakeScope(Ok(scope())),
            )
            .unwrap();
        let runtime = agent
            .coordinator
            .runtime_for_terminal(&admission.terminal)
            .unwrap();
        agent
            .reported_phases
            .insert(runtime.agent_runtime_id, AgentPhase::Ended);
    }

    let mut operations = [OperationId::new(), OperationId::new(), OperationId::new()];
    operations.sort();
    let mut snapshot = agent.coordinator.snapshot();
    // Runtime records are keyed independently of operation age. Arrange
    // their ages so iteration first replaces the candidate, then retains it.
    snapshot.records[0].operation.operation_id = operations[2];
    snapshot.records[1].operation.operation_id = operations[0];
    snapshot.records[2].operation.operation_id = operations[1];
    let oldest = snapshot.records[1].runtime.clone();
    agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 3, 64 * 1024, 64).unwrap();

    assert_eq!(agent.sleep_one_for_capacity(), Ok(true));
    let records = agent.coordinator.snapshot().records;
    assert_eq!(agent.concurrency().in_use, 2);
    assert!(records.iter().any(|record| {
        record.runtime == oldest && record.state == super::super::runtime::RuntimeState::Sleeping
    }));
    assert_eq!(
        records
            .iter()
            .filter(|record| record.state == super::super::runtime::RuntimeState::Running)
            .count(),
        2
    );
}

#[test]
fn saturated_launch_refuses_when_no_completed_resume_source_is_safe() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let generation = DaemonGeneration::new();
    let mut agent = AgentRuntime::new(
        generation,
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            terminate_success: true,
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    let mut one_slot = RuntimeCoordinator::new(1, 64 * 1024, 64);
    one_slot.activate_generation(generation).unwrap();
    agent.coordinator = one_slot;
    let first = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: None,
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();

    assert_eq!(
        agent
            .launch(
                &OperationId::new().to_string(),
                &AgentLaunchIntent {
                    workspace,
                    session: Some(session),
                    profile: None,
                },
                &FakeScope(Ok(resolved)),
            )
            .unwrap_err()
            .code,
        ErrorCode::ResourceExhausted
    );
    assert_eq!(agent.concurrency().in_use, 1);
    assert_eq!(
        agent
            .coordinator
            .runtime_for_terminal(&first.terminal)
            .unwrap()
            .terminal,
        first.terminal
    );
}

#[test]
fn manual_sleep_requires_an_idle_exact_resume_source_and_retains_the_session() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let mut agent = AgentRuntime::new(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            terminate_success: true,
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    assert_eq!(
        agent.sleep_session(session).unwrap_err().code,
        ErrorCode::Unavailable
    );

    let admission = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: None,
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let runtime = agent
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap();
    assert_eq!(
        agent.sleep_session(session).unwrap_err().code,
        ErrorCode::Busy
    );

    agent
        .reported_phases
        .insert(runtime.agent_runtime_id, AgentPhase::Ready);
    let mut snapshot = agent.coordinator.snapshot();
    let resume = snapshot.records[0].provider_resume.take().unwrap();
    agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    assert_eq!(
        agent.sleep_session(session).unwrap_err().code,
        ErrorCode::Busy
    );
    agent
        .coordinator
        .write_provider_resume(
            &runtime,
            resume,
            ProviderResumeWrite::Attach,
            &mut *agent.store,
        )
        .unwrap();

    assert_eq!(agent.sleep_session(session), Ok(1));
    assert_eq!(agent.session_phase(session), AgentPhase::Sleeping);
    assert_eq!(agent.concurrency().in_use, 0);
    assert!(agent.mcp_callers.is_empty());
    assert!(
        !agent
            .reported_phases
            .contains_key(&runtime.agent_runtime_id)
    );
    assert_eq!(
        agent.sleep_session(session).unwrap_err().code,
        ErrorCode::Unavailable
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture exercises every ownership fence and orphan retry outcome.
fn supervisor_stop_validates_every_fence_and_retries_an_orphaned_process() {
    let workspace = WorkspaceId::new();
    let resolved = scope();
    let mut agent = AgentRuntime::new(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    let admission = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: None,
            },
            &FakeScope(Ok(resolved)),
        )
        .unwrap();
    let runtime = agent
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap();
    let provenance = RunProvenance {
        supervisor_run_id: SupervisorRunId::new(),
        task_id: TaskId::new("root").unwrap(),
        parent_task_id: None,
        parent_dispatch_run: None,
        dispatch_run_id: OperationId::new(),
        worker_session_id: runtime.session_id,
        worker_agent_id: runtime.agent_runtime_id,
        worker_worktree_id: runtime.terminal.worktree_id,
        generation: 1,
    };

    let mut conflicting = provenance.clone();
    conflicting.worker_session_id = Some(SessionId::new());
    assert_eq!(
        agent
            .interrupt_supervisor_workers(workspace, &[provenance.clone(), conflicting],)
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );

    let mut absent = provenance.clone();
    absent.worker_agent_id = AgentRuntimeId::new();
    assert_eq!(
        agent.interrupt_supervisor_workers(workspace, &[absent]),
        Ok(0)
    );

    let mut stale = provenance.clone();
    stale.worker_worktree_id = WorktreeId::new();
    assert_eq!(
        agent
            .interrupt_supervisor_workers(workspace, &[stale])
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );
    assert_eq!(
        agent.coordinator.snapshot().records[0].state,
        super::super::runtime::RuntimeState::Running
    );

    assert_eq!(
        agent
            .interrupt_supervisor_workers(workspace, std::slice::from_ref(&provenance))
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        agent.coordinator.snapshot().records[0].state,
        super::super::runtime::RuntimeState::ReconcileRequired(
            super::super::runtime::ReconcileState::OrphanRunning
        )
    );

    agent
        .pty
        .as_any_mut()
        .downcast_mut::<Pty>()
        .unwrap()
        .terminate_success = true;
    agent
        .reported_phases
        .insert(runtime.agent_runtime_id, AgentPhase::Running);
    assert_eq!(
        agent.interrupt_supervisor_workers(workspace, std::slice::from_ref(&provenance)),
        Ok(1)
    );
    assert!(
        !agent
            .reported_phases
            .contains_key(&runtime.agent_runtime_id)
    );
    assert_eq!(
        agent.coordinator.snapshot().records[0].state,
        super::super::runtime::RuntimeState::Exited
    );
    assert_eq!(
        agent.interrupt_supervisor_workers(workspace, std::slice::from_ref(&provenance)),
        Ok(0)
    );

    let mut ownership_unknown = agent.coordinator.snapshot();
    ownership_unknown.records[0].state = super::super::runtime::RuntimeState::ReconcileRequired(
        super::super::runtime::ReconcileState::IdentityUnknown,
    );
    ownership_unknown.records[0].process = None;
    ownership_unknown.generation.terminals[0].process = None;
    ownership_unknown.generation.terminals[0].state =
        super::super::generation::TerminalState::IdentityUnknown;
    agent.coordinator = RuntimeCoordinator::hydrate(ownership_unknown, 16, 64 * 1024, 64).unwrap();
    assert_eq!(
        agent
            .interrupt_supervisor_workers(workspace, std::slice::from_ref(&provenance))
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
fn runtime_operation_join_is_exact_and_durable_ownership_is_unique() {
    let mut agent = runtime();
    let first_operation = OperationId::new();
    let first = agent
        .launch(
            &first_operation.to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    assert_eq!(
        agent.runtime_for_operation(first_operation),
        agent.coordinator.runtime_for_terminal(&first.terminal)
    );
    assert_eq!(agent.runtime_for_operation(OperationId::new()), None);

    let second_operation = OperationId::new();
    agent
        .launch(
            &second_operation.to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let mut duplicate = agent.coordinator.snapshot();
    for record in &mut duplicate.records {
        record.operation.operation_id = first_operation;
    }
    assert_eq!(
        RuntimeCoordinator::hydrate(duplicate, 16, 64 * 1024, 64).unwrap_err(),
        super::super::runtime::RuntimeSnapshotError::DuplicateOperation
    );
}

fn restart_runtime() -> AgentRuntime {
    AgentRuntime::with_dispatch_and_locator(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(tempfile::tempdir().unwrap().keep()),
        PathExecutableLocator,
    )
}

#[test]
#[allow(clippy::too_many_lines)] // One end-to-end usecase scenario proves stop, lost response, migration, and replay together.
fn doctor_restarts_only_outdated_idle_integration_and_migrates_exact_resume() {
    let workspace = WorkspaceId::new();
    let resolved = scope();
    let mut agent = AgentRuntime::new(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            terminate_success: true,
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    let admission = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    let runtime_id = agent
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap()
        .agent_runtime_id;
    let mcp_caller = agent
        .mcp_callers
        .values_mut()
        .next()
        .expect("launch registers one MCP caller");
    mcp_caller.child = Some(McpChildLease {
        pid: 9001,
        process_start_identity: "process-9001".into(),
        connection: Some(ConnectionId::new()),
    });
    let mut snapshot = agent.coordinator.snapshot();
    snapshot.records[0].launch.plan.profile_revision = 1;
    snapshot.records[0]
        .provider_resume
        .as_mut()
        .unwrap()
        .adapter_revision = 1;
    agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    agent
        .reported_phases
        .insert(runtime_id, AgentPhase::Waiting);
    let current_revision = crate::usecase::claude::PROFILE_REVISION;
    let expected = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: current_revision,
    }];

    let diagnosis = agent.diagnose_integrations(workspace, &expected).unwrap();
    assert_eq!(diagnosis.outdated.len(), 1);
    assert_eq!(diagnosis.outdated[0].actual_revision, 1);
    assert_eq!(diagnosis.outdated[0].expected_revision, current_revision);
    assert_eq!(diagnosis.outdated[0].phase, AgentPhase::Waiting);
    assert!(diagnosis.outdated[0].resume_available);
    assert_eq!(diagnosis.outdated_mcp_children, 1);
    assert_eq!(diagnosis.provisioned_mcp_callers, Some(1));
    let newcomer = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    let newcomer_runtime = agent
        .coordinator
        .runtime_for_terminal(&newcomer.terminal)
        .unwrap()
        .clone();
    let mut snapshot = agent.coordinator.snapshot();
    let newcomer_record = snapshot
        .records
        .iter_mut()
        .find(|record| record.runtime == newcomer_runtime)
        .unwrap();
    newcomer_record.launch.plan.profile_revision = 1;
    newcomer_record
        .provider_resume
        .as_mut()
        .unwrap()
        .adapter_revision = 1;
    agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    agent
        .reported_phases
        .insert(newcomer_runtime.agent_runtime_id, AgentPhase::Waiting);
    let (interrupted, stopped) = agent
        .interrupt_outdated_agents(
            workspace,
            &expected,
            &diagnosis
                .outdated
                .iter()
                .map(|item| item.runtime.clone())
                .collect::<Vec<_>>(),
            false,
        )
        .unwrap();
    assert_eq!(interrupted, 1);
    assert_eq!(stopped.outdated, diagnosis.outdated);
    assert_eq!(stopped.outdated_mcp_children, 1);
    assert_eq!(
        stopped.provisioned_mcp_callers,
        Some(2),
        "the diagnosis taken at interruption includes a newly minted, unclaimed credential"
    );
    assert_eq!(
        agent
            .coordinator
            .runtime_for_terminal(&newcomer.terminal)
            .unwrap(),
        newcomer_runtime.clone(),
        "an Agent that became outdated after diagnosis is not part of the confirmed selection"
    );
    assert_eq!(agent.mcp_callers.len(), 1);
    assert_eq!(
        agent
            .diagnose_integrations(workspace, &expected)
            .unwrap()
            .outdated
            .len(),
        2,
        "the stopped source and the unselected newcomer both remain diagnosable"
    );
    assert_eq!(
        agent
            .interrupt_outdated_agents(
                workspace,
                &expected,
                &diagnosis
                    .outdated
                    .iter()
                    .map(|item| item.runtime.clone())
                    .collect::<Vec<_>>(),
                false,
            )
            .unwrap()
            .0,
        0
    );
    let target = agent.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    assert_eq!(
        agent
            .resume_with_current_integration(
                &OperationId::new().to_string(),
                &target,
                current_revision + 1,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    let repair_operation = OperationId::new().to_string();
    let replacement = agent
        .resume_with_current_integration(
            &repair_operation,
            &target,
            current_revision,
            &FakeScope(Ok(resolved)),
        )
        .unwrap();
    assert!(
        agent
            .prepare_current_integration_resume_readiness(
                &repair_operation,
                &target,
                current_revision,
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        agent
            .resume_with_current_integration(
                &repair_operation,
                &target,
                current_revision,
                &FakeScope(Ok(scope())),
            )
            .unwrap(),
        replacement
    );
    assert_ne!(replacement.terminal, admission.terminal);
    let after = agent.diagnose_integrations(workspace, &expected).unwrap();
    assert_eq!(after.outdated.len(), 1);
    assert_eq!(after.outdated[0].runtime, newcomer_runtime);
    assert_eq!(
        agent
            .resume_with_current_integration(
                &repair_operation,
                &target,
                current_revision + 1,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
}

#[test]
fn doctor_refuses_invalid_revision_catalog_and_running_agent_without_force() {
    let workspace = WorkspaceId::new();
    let mut agent = runtime();
    let invalid = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: 0,
    }];
    assert_eq!(
        agent
            .diagnose_integrations(workspace, &invalid)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );

    let admission = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let mut snapshot = agent.coordinator.snapshot();
    snapshot.records[0].launch.plan.profile_revision = 1;
    snapshot.records[0]
        .provider_resume
        .as_mut()
        .unwrap()
        .adapter_revision = 1;
    agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    let expected = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: 2,
    }];
    let selected = agent
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap()
        .clone();
    let mut stale = selected.clone();
    stale.agent_runtime_id = AgentRuntimeId::new();
    assert_eq!(
        agent
            .interrupt_outdated_agents(workspace, &expected, &[stale], false)
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );
    assert_eq!(
        agent
            .interrupt_outdated_agents(
                workspace,
                &expected,
                std::slice::from_ref(&selected),
                false,
            )
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );
    assert_eq!(
        agent
            .coordinator
            .runtime_for_terminal(&admission.terminal)
            .unwrap()
            .terminal,
        admission.terminal
    );

    let mut snapshot = agent.coordinator.snapshot();
    snapshot.records[0].provider_resume = None;
    agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    let diagnosis = agent.diagnose_integrations(workspace, &expected).unwrap();
    assert!(!diagnosis.outdated[0].resume_available);
    assert_eq!(
        agent
            .interrupt_outdated_agents(workspace, &expected, std::slice::from_ref(&selected), true,)
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );
    assert_eq!(
        agent
            .coordinator
            .runtime_for_terminal(&admission.terminal)
            .unwrap()
            .terminal,
        admission.terminal,
        "missing exact provider metadata must be refused before termination"
    );
}

#[test]
fn daemon_restart_plan_revalidates_every_live_agent_before_interrupting() {
    let workspace = WorkspaceId::new();
    let mut agent = runtime();
    agent.pty = Box::new(Pty {
        terminate_success: true,
        ..Pty::default()
    });
    let admission = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let expected = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: crate::usecase::claude::PROFILE_REVISION,
    }];
    assert_eq!(
        agent
            .plan_daemon_restart_agents(&expected, false)
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );
    let credential = agent.mcp_callers.keys().next().cloned().unwrap();
    agent
        .report_agent_phase(&credential, AgentPhase::Waiting)
        .unwrap();
    let plan = agent.plan_daemon_restart_agents(&expected, false).unwrap();
    assert_eq!(plan.agents.len(), 1);
    assert_eq!(plan.agents[0].runtime.terminal, admission.terminal);
    assert_eq!(plan.agents[0].phase, AgentPhase::Waiting);
    assert!(
        !agent
            .daemon_restart_restore_needed(&plan.agents[0].runtime)
            .unwrap()
    );

    let mut stale = plan.agents[0].runtime.clone();
    stale.agent_runtime_id = AgentRuntimeId::new();
    assert_eq!(
        agent
            .interrupt_agents_for_daemon_restart(&expected, &[stale], true,)
            .unwrap_err()
            .error
            .code,
        ErrorCode::StaleTarget
    );
    assert_eq!(agent.provisioned_mcp_callers(), 1);

    let current = agent.plan_daemon_restart_agents(&expected, true).unwrap();
    let stopped = agent
        .interrupt_agents_for_daemon_restart(
            &expected,
            &current
                .agents
                .iter()
                .map(|item| item.runtime.clone())
                .collect::<Vec<_>>(),
            true,
        )
        .unwrap();
    assert_eq!(stopped, current);
    assert!(
        agent
            .daemon_restart_restore_needed(&stopped.agents[0].runtime)
            .unwrap()
    );
    assert_eq!(agent.provisioned_mcp_callers(), 0);
    assert!(
        agent
            .inventory(workspace)
            .runtimes
            .iter()
            .all(|item| item.state == AgentRuntimeInventoryState::Exited)
    );
}

#[test]
fn daemon_restart_plan_refuses_every_unprovable_agent_authority() {
    let expected = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: crate::usecase::claude::PROFILE_REVISION,
    }];
    let workspace = WorkspaceId::new();

    let mut incomplete = runtime();
    incomplete
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    assert_eq!(
        incomplete
            .plan_daemon_restart_agents(&[], true)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let mut snapshot = incomplete.coordinator.snapshot();
    snapshot.records[0].provider_resume = None;
    incomplete.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    assert_eq!(
        incomplete
            .plan_daemon_restart_agents(&expected, true)
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );

    let mut unmatched = runtime();
    unmatched
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let credential = unmatched.mcp_callers.keys().next().cloned().unwrap();
    unmatched
        .mcp_callers
        .get_mut(&credential)
        .unwrap()
        .runtime
        .agent_runtime_id = AgentRuntimeId::new();
    assert_eq!(
        unmatched
            .plan_daemon_restart_agents(&expected, true)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );

    let mut historical = runtime();
    let exited = historical
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    historical.exit(&exited.terminal, 0).unwrap();
    assert!(
        historical
            .plan_daemon_restart_agents(&expected, true)
            .unwrap()
            .agents
            .is_empty()
    );
}

#[test]
fn daemon_restart_plan_refuses_agents_from_multiple_workspaces_before_stopping_any() {
    let first_workspace = WorkspaceId::new();
    let second_workspace = WorkspaceId::new();
    let mut agent = runtime();
    for workspace in [first_workspace, second_workspace] {
        agent
            .launch(
                &OperationId::new().to_string(),
                &AgentLaunchIntent {
                    workspace,
                    session: None,
                    profile: Some(AgentProfileId::new("claude").unwrap()),
                },
                &FakeScope(Ok(scope())),
            )
            .unwrap();
    }
    for credential in agent.mcp_callers.keys().cloned().collect::<Vec<_>>() {
        agent
            .report_agent_phase(&credential, AgentPhase::Waiting)
            .unwrap();
    }
    let expected = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: crate::usecase::claude::PROFILE_REVISION,
    }];

    let refusal = agent
        .plan_daemon_restart_agents(&expected, false)
        .unwrap_err();

    assert_eq!(refusal.code, ErrorCode::Busy);
    assert!(refusal.message.contains("multiple workspaces"));
    assert_eq!(agent.provisioned_mcp_callers(), 2);
    assert!(
        agent
            .coordinator
            .snapshot()
            .records
            .iter()
            .all(|record| { record.state == crate::usecase::runtime::RuntimeState::Running })
    );
}

#[test]
fn daemon_restart_interruption_reports_only_the_partial_stop_for_rollback() {
    let workspace = WorkspaceId::new();
    let mut agent = runtime();
    agent.pty = Box::new(Pty {
        terminate_success: true,
        terminate_fail_at: Some(2),
        spawn_counter: Some(Arc::new(AtomicU32::new(100))),
        ..Pty::default()
    });
    for workspace in [workspace, workspace] {
        agent
            .launch(
                &OperationId::new().to_string(),
                &AgentLaunchIntent {
                    workspace,
                    session: None,
                    profile: Some(AgentProfileId::new("claude").unwrap()),
                },
                &FakeScope(Ok(scope())),
            )
            .unwrap();
    }
    for credential in agent.mcp_callers.keys().cloned().collect::<Vec<_>>() {
        agent
            .report_agent_phase(&credential, AgentPhase::Waiting)
            .unwrap();
    }
    let expected = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: crate::usecase::claude::PROFILE_REVISION,
    }];
    let plan = agent.plan_daemon_restart_agents(&expected, false).unwrap();
    let failure = agent
        .interrupt_agents_for_daemon_restart(
            &expected,
            &plan
                .agents
                .iter()
                .map(|item| item.runtime.clone())
                .collect::<Vec<_>>(),
            false,
        )
        .unwrap_err();

    assert_eq!(failure.error.code, ErrorCode::OwnershipUnknown);
    assert_eq!(failure.interrupted.agents.len(), 1);
    assert_eq!(agent.provisioned_mcp_callers(), 1);
    let snapshot = agent.coordinator.snapshot();
    assert_eq!(
        snapshot
            .records
            .iter()
            .filter(|record| record.state == super::super::runtime::RuntimeState::Exited)
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .records
            .iter()
            .filter(|record| {
                record.state
                    == super::super::runtime::RuntimeState::ReconcileRequired(
                        super::super::runtime::ReconcileState::OrphanRunning,
                    )
            })
            .count(),
        1
    );
    assert_eq!(
        agent
            .plan_daemon_restart_agents(&expected, true)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
fn integration_diagnosis_admits_only_resumable_or_ownership_unknown_states() {
    use super::super::runtime::{ReconcileState, RuntimeState};

    for state in [
        RuntimeState::Reserved,
        RuntimeState::Running,
        RuntimeState::Exited,
        RuntimeState::Interrupted,
        RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown),
    ] {
        assert!(integration_diagnosable_state(state));
    }
    for state in [
        RuntimeState::Reclaimed,
        RuntimeState::SpawnFailed,
        RuntimeState::ReconcileRequired(ReconcileState::SpawnAmbiguous),
        RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning),
    ] {
        assert!(!integration_diagnosable_state(state));
    }
}

#[test]
fn repair_source_diagnosis_covers_every_fail_closed_relation() {
    let workspace = WorkspaceId::new();
    let mut agent = runtime();
    agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let records = agent.coordinator.snapshot().records;
    assert!(AgentRuntime::repair_source_availability(&records[0], &records).0);
    let mut missing = records[0].clone();
    missing.provider_resume = None;
    assert_eq!(
        agent
            .repair_resume_source_availability(&missing, &records, 2)
            .1,
        ProviderResumeReason::ProviderMetadataUnavailable
    );

    let mut superseded = records[0].clone();
    superseded.superseded_by = Some(AgentRuntimeId::new());
    assert_eq!(
        AgentRuntime::repair_source_availability(&superseded, &records).1,
        ProviderResumeReason::SourceAlreadySuperseded
    );

    let mut duplicate = records[0].clone();
    duplicate.runtime.agent_runtime_id = AgentRuntimeId::new();
    assert_eq!(
        AgentRuntime::repair_source_availability(&records[0], &[records[0].clone(), duplicate],).1,
        ProviderResumeReason::LiveOrOwnershipUnknown
    );

    let mut incompatible = records[0].clone();
    incompatible
        .provider_resume
        .as_mut()
        .unwrap()
        .scope
        .worktree_id = WorktreeId::new();
    assert_eq!(
        AgentRuntime::repair_source_availability(&incompatible, &records).1,
        ProviderResumeReason::IncompatibleProviderMetadata
    );
}

fn hydrate_restart_runtime(snapshot: RuntimeStoreSnapshot) -> AgentRuntime {
    AgentRuntime::hydrate_with_dispatch_and_locator(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(tempfile::tempdir().unwrap().keep()),
        PathExecutableLocator,
        snapshot,
    )
    .unwrap()
}

fn runtime_with_fixture(locator: FixtureLocator) -> AgentRuntime {
    AgentRuntime::with_dispatch_and_locator(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(tempfile::tempdir().unwrap().keep()),
        locator,
    )
}

fn codex_runtime() -> AgentRuntime {
    let mut registry = AdapterRegistry::new();
    let adapter = CodexAdapter::new(FakeCodexProvisioner);
    registry
        .register(adapter.profile().clone(), Box::new(adapter))
        .unwrap();
    AgentRuntime::with_dispatch_and_locator(
        DaemonGeneration::new(),
        registry,
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("codex").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(tempfile::tempdir().unwrap().keep()),
        PathExecutableLocator,
    )
}

fn agy_runtime() -> AgentRuntime {
    let mut registry = AdapterRegistry::new();
    let adapter = AgyAdapter::new(FakeAgyProvisioner);
    registry
        .register(adapter.profile().clone(), Box::new(adapter))
        .unwrap();
    AgentRuntime::with_dispatch_and_locator(
        DaemonGeneration::new(),
        registry,
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("agy").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(tempfile::tempdir().unwrap().keep()),
        PathExecutableLocator,
    )
}

fn durable_phase(runtime: &AgentRuntime) -> Option<ProviderResumePhase> {
    runtime.coordinator.snapshot().records[0]
        .provider_resume
        .as_ref()
        .and_then(|reference| reference.last_known_phase)
}

fn store_mut(runtime: &mut AgentRuntime) -> &mut Store {
    runtime.store.as_any_mut().downcast_mut::<Store>().unwrap()
}

fn pty(runtime: &AgentRuntime) -> &Pty {
    runtime.pty.as_any().downcast_ref::<Pty>().unwrap()
}

fn pty_mut(runtime: &mut AgentRuntime) -> &mut Pty {
    runtime.pty.as_any_mut().downcast_mut::<Pty>().unwrap()
}

#[test]
fn agent_output_answers_cursor_position_queries_through_the_owned_pty() {
    let mut agent = runtime();
    let admission = agent
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();

    agent
        .output(&admission.terminal, b"warning\r\n> \x1b[6n".to_vec())
        .unwrap();

    assert_eq!(pty(&agent).writes, b"\x1b[2;3R");
    assert_eq!(pty(&agent).selected, Some(admission.terminal));
}

fn configured_scope(workspace: &std::path::Path) -> ResolvedAgentScope {
    std::fs::create_dir_all(workspace.join(".usagi")).unwrap();
    std::fs::write(
        workspace.join(".usagi/config.toml"),
        "[agents.claude]\nmodels = [\"test\"]\n",
    )
    .unwrap();
    ResolvedAgentScope {
        worktree_id: WorktreeId::new(),
        working_directory: workspace.to_path_buf(),
    }
}

fn intent(profile: Option<&str>) -> AgentLaunchIntent {
    AgentLaunchIntent {
        workspace: WorkspaceId::new(),
        session: Some(SessionId::new()),
        profile: optional_profile(profile),
    }
}

fn root_intent(profile: Option<&str>) -> AgentLaunchIntent {
    AgentLaunchIntent {
        workspace: WorkspaceId::new(),
        session: None,
        profile: optional_profile(profile),
    }
}

fn optional_profile(profile: Option<&str>) -> Option<AgentProfileId> {
    profile.map(|name| AgentProfileId::new(name).unwrap())
}

// ---- tests ---------------------------------------------------------------

#[test]
fn workspace_observation_aggregates_session_status_independent_of_store_order() {
    let runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let terminal_session = SessionId::new();
    for (scope, profile, model, status) in [
        (Some(session), "claude", "idle", AgentStatus::Idle),
        (Some(session), "codex", "running", AgentStatus::Running),
        (
            Some(session),
            "claude",
            "starting-after-running",
            AgentStatus::Starting,
        ),
        (
            Some(terminal_session),
            "claude",
            "failed-before-idle",
            AgentStatus::Failed,
        ),
        (
            Some(terminal_session),
            "codex",
            "idle-after-failed",
            AgentStatus::Idle,
        ),
        (None, "codex", "root", AgentStatus::Starting),
    ] {
        record_dispatch_status(&runtime, workspace, scope, profile, model, status);
    }

    let observation = runtime.workspace_observation(workspace).unwrap();
    assert_eq!(observation.inventory.workspace_id, workspace);
    assert_eq!(
        observation.session_statuses,
        BTreeMap::from([
            (session, AgentStatus::Running),
            (terminal_session, AgentStatus::Failed),
        ])
    );
    assert!(
        runtime
            .workspace_observation(WorkspaceId::new())
            .unwrap()
            .session_statuses
            .is_empty()
    );

    std::fs::write(runtime.dispatch.registry_path(), "broken").unwrap();
    assert_eq!(
        runtime.workspace_observation(workspace).unwrap_err().code,
        ErrorCode::Unavailable
    );
}

#[test]
fn daemon_dispatch_store_requires_ownership_without_reparenting() {
    let directory = tempfile::tempdir().unwrap();
    let store = DispatchStore::new(directory.path());
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let worker = Agent {
        agent_id: AgentId::new(),
        session_id: Some(session),
        runtime: AgentProfileId::new("claude").unwrap(),
        model: ModelSelector::new("test").unwrap(),
        status: AgentStatus::Starting,
        current_run: None,
    };
    let admission = |operation, parent| {
        (
            worker.clone(),
            DispatchRun {
                run_id: operation,
                agent_id: worker.agent_id,
                prompt: "dispatch ownership".into(),
                started_at: Utc::now(),
                ended_at: None,
                status: RunStatus::Preparing,
            },
            DispatchBinding {
                run_id: operation,
                caller: CallerRef {
                    session_id: Some(parent),
                    agent_id: AgentId::new(),
                },
                worker: WorkerRef {
                    session_id: Some(session),
                    agent_id: worker.agent_id,
                },
            },
            AgentAdmissionReservation {
                operation_id: operation,
                semantic_key: "dispatch-ownership".into(),
                credential_provenance: DispatchCredentialProvenance::DaemonMintedEphemeral,
            },
        )
    };

    let missing = OperationId::new();
    let (agent, run, binding, reservation) = admission(missing, SessionId::new());
    assert!(
        store
            .reserve_admission(agent, run, binding, reservation)
            .is_err()
    );
    assert!(store.run(missing).unwrap().is_none());

    store.upsert_agent(workspace, worker.clone()).unwrap();
    let initial_parent = SessionId::new();
    store
        .record_session_parent(workspace, session, Some(initial_parent))
        .unwrap();
    let admitted = OperationId::new();
    let (agent, run, binding, reservation) = admission(admitted, SessionId::new());
    store
        .reserve_admission(agent, run, binding, reservation)
        .unwrap();

    let conflicting = OperationId::new();
    let (agent, run, binding, reservation) = admission(conflicting, SessionId::new());
    store
        .reserve_admission(agent, run, binding, reservation)
        .unwrap();
    assert!(store.run(conflicting).unwrap().is_some());
    assert!(store.admission(conflicting).unwrap().is_some());
    assert_eq!(
        store.session_parent(workspace, session).unwrap(),
        Some(initial_parent)
    );
}

/// The Agent runtime is the authority a metrics observer reads through: the
/// level it publishes is `AGENT_RUNTIME_LIMIT` wide and moves with the
/// admissions it grants, so nobody has to count runtimes or restate the limit.
#[test]
fn a_bound_gauge_reports_this_owners_agent_concurrency() {
    use crate::usecase::metrics::AgentConcurrencyGauge;

    let mut runtime = runtime();
    let gauge = AgentConcurrencyGauge::default();
    runtime.bind_concurrency_gauge(gauge.clone());
    assert_eq!(
        gauge.observe(),
        Some(usagi_core::infrastructure::ipc::AgentConcurrency {
            in_use: 0,
            limit: u32::try_from(AGENT_RUNTIME_LIMIT).unwrap(),
        })
    );

    runtime
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();

    assert_eq!(gauge.observe(), Some(runtime.concurrency()));
    assert_eq!(gauge.observe().unwrap().in_use, 1);
    assert!(!gauge.observe().unwrap().is_saturated());
}

#[test]
fn readiness_ticket_is_revalidated_before_launch_effects() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let intent = intent(Some("claude"));
    let operation = OperationId::new().to_string();
    let ticket = runtime
        .prepare_launch_readiness(&operation, &intent)
        .unwrap()
        .unwrap();

    let stale = AgentReadinessPreflight {
        profile: ticket.profile.clone(),
        profile_revision: ticket.profile_revision + 1,
        generation: ticket.generation,
    };
    let error = runtime
        .launch_after_readiness(
            &operation,
            &intent,
            &FakeScope(Ok(ResolvedAgentScope {
                worktree_id: WorktreeId::new(),
                working_directory: PathBuf::from("/worktree"),
            })),
            Some(&stale),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::RevisionConflict);
    assert!(runtime.coordinator.snapshot().records.is_empty());

    std::fs::remove_file(fixture.path().join("claude")).unwrap();
    let error = runtime
        .launch_after_readiness(
            &operation,
            &intent,
            &FakeScope(Ok(ResolvedAgentScope {
                worktree_id: WorktreeId::new(),
                working_directory: PathBuf::from("/worktree"),
            })),
            Some(&ticket),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(runtime.coordinator.snapshot().records.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)] // One table-like sweep covers every secret-free preflight refusal/replay branch.
fn readiness_preparation_covers_replay_conflict_and_safe_refusals() {
    let mut runtime = runtime();
    let launch = intent(None);
    assert_eq!(
        runtime
            .prepare_launch_readiness(&OperationId::new().to_string(), &launch)
            .unwrap()
            .unwrap()
            .product(),
        "claude"
    );
    assert_eq!(
        runtime
            .prepare_launch_readiness("invalid", &launch)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let unknown = intent(Some("unknown"));
    assert_eq!(
        runtime
            .prepare_launch_readiness(&OperationId::new().to_string(), &unknown)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let launch_operation = OperationId::new().to_string();
    runtime.operations.insert(
        launch_operation.clone(),
        AgentOperation::new(
            Some(&semantic_key(&launch)),
            Err(ProtocolError::new(ErrorCode::Unavailable, "fixture")),
            Utc::now(),
        ),
    );
    assert!(
        runtime
            .prepare_launch_readiness(&launch_operation, &launch)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        runtime
            .prepare_launch_readiness(&launch_operation, &intent(Some("claude")))
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );

    let stale_target = AgentResumeTarget {
        continuation: AgentContinuationRef::new(),
        source: AgentResumeSourceId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
        runtime_id: AgentRuntimeId::new(),
        adapter_revision: 1,
    };
    assert_eq!(
        runtime
            .prepare_resume_readiness("invalid", &stale_target)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .prepare_resume_readiness(&OperationId::new().to_string(), &stale_target)
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );
    let resume_operation = OperationId::new().to_string();
    runtime.operations.insert(
        resume_operation.clone(),
        AgentOperation::new(
            Some(&resume_semantic_key(&stale_target)),
            Err(ProtocolError::new(ErrorCode::Unavailable, "fixture")),
            Utc::now(),
        ),
    );
    assert!(
        runtime
            .prepare_resume_readiness(&resume_operation, &stale_target)
            .unwrap()
            .is_none()
    );
    let mut conflicting = stale_target.clone();
    conflicting.source = AgentResumeSourceId::new();
    assert_eq!(
        runtime
            .prepare_resume_readiness(&resume_operation, &conflicting)
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );

    let dispatch_operation = OperationId::new().to_string();
    let dispatch = DispatchIntent {
        workspace: WorkspaceId::new(),
        session_name: "worker".into(),
        caller: CallerRef {
            session_id: None,
            agent_id: AgentId::new(),
        },
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: ModelSelector::new("test").unwrap(),
        },
        prompt: "work".into(),
    };
    assert_eq!(
        runtime
            .prepare_dispatch_readiness("invalid", &dispatch)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert!(
        runtime
            .prepare_dispatch_readiness(&dispatch_operation, &dispatch)
            .unwrap()
            .is_some()
    );
    let existing = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            dispatch.workspace,
            None,
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("test").unwrap(),
        )
        .unwrap();
    let existing_dispatch = DispatchIntent {
        agent: DispatchAgentIntent::Existing {
            agent_id: existing.agent_id,
        },
        ..dispatch.clone()
    };
    assert!(
        runtime
            .prepare_dispatch_readiness(&OperationId::new().to_string(), &existing_dispatch,)
            .unwrap()
            .is_some()
    );
    runtime.operations.insert(
        dispatch_operation.clone(),
        AgentOperation::new(
            None,
            Err(ProtocolError::new(ErrorCode::Unavailable, "fixture")),
            Utc::now(),
        ),
    );
    assert!(
        runtime
            .prepare_dispatch_readiness(&dispatch_operation, &dispatch)
            .unwrap()
            .is_none()
    );
    let missing = DispatchIntent {
        agent: DispatchAgentIntent::Existing {
            agent_id: AgentId::new(),
        },
        ..dispatch
    };
    assert_eq!(
        runtime
            .prepare_dispatch_readiness(&OperationId::new().to_string(), &missing)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn readiness_admission_wrappers_cover_launch_exact_and_dispatch() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let launch = AgentLaunchIntent {
        workspace,
        session: Some(session),
        profile: Some(AgentProfileId::new("claude").unwrap()),
    };
    let operation = OperationId::new().to_string();
    let ticket = runtime
        .prepare_launch_readiness(&operation, &launch)
        .unwrap()
        .unwrap();
    assert_eq!(ticket.product(), "claude");
    let admitted = runtime
        .launch_after_readiness(
            &operation,
            &launch,
            &FakeScope(Ok(resolved.clone())),
            Some(&ticket),
        )
        .unwrap();
    assert!(
        runtime
            .launch_after_readiness(&operation, &launch, &FakeScope(Ok(resolved.clone())), None,)
            .is_ok(),
        "a concurrent completed admission replays without a ticket"
    );
    runtime.exit(&admitted.terminal, 0).unwrap();
    let target = runtime.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    let resume_operation = OperationId::new().to_string();
    let resume_ticket = runtime
        .prepare_resume_readiness(&resume_operation, &target)
        .unwrap()
        .unwrap();
    let resumed = runtime
        .resume_exact_after_readiness(
            &resume_operation,
            &target,
            &FakeScope(Ok(resolved.clone())),
            Some(&resume_ticket),
        )
        .unwrap();
    runtime.exit(&resumed.terminal, 0).unwrap();
    let repair_target = runtime.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    let repair_operation = OperationId::new().to_string();
    let repair_ticket = runtime
        .prepare_current_integration_resume_readiness(&repair_operation, &repair_target, 2)
        .unwrap()
        .unwrap();
    runtime
        .resume_with_current_integration_after_readiness(
            &repair_operation,
            &repair_target,
            2,
            &FakeScope(Ok(resolved.clone())),
            Some(&repair_ticket),
        )
        .unwrap();
    assert_eq!(
        runtime
            .resume_with_current_integration_after_readiness(
                "invalid",
                &repair_target,
                2,
                &FakeScope(Ok(resolved.clone())),
                None,
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let worktree = tempfile::tempdir().unwrap();
    let mut dispatch_runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let dispatch = DispatchIntent {
        workspace,
        session_name: "worker".into(),
        caller: CallerRef {
            session_id: None,
            agent_id: AgentId::new(),
        },
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: ModelSelector::new("test").unwrap(),
        },
        prompt: "work".into(),
    };
    let dispatch_operation = OperationId::new().to_string();
    let dispatch_ticket = dispatch_runtime
        .prepare_dispatch_readiness(&dispatch_operation, &dispatch)
        .unwrap()
        .unwrap();
    dispatch_runtime
        .dispatch_after_readiness(
            &dispatch_operation,
            &dispatch,
            session,
            &FakeScope(Ok(configured_scope(worktree.path()))),
            Some(&dispatch_ticket),
        )
        .unwrap();

    let planned_session = SessionId::new();
    let planned_operation = OperationId::new().to_string();
    let planned_ticket = dispatch_runtime
        .prepare_dispatch_readiness(&planned_operation, &dispatch)
        .unwrap()
        .unwrap();
    let planned = dispatch_runtime
        .plan_dispatch_worker(workspace, planned_session, &dispatch.agent)
        .unwrap();
    dispatch_runtime
        .dispatch_planned_after_readiness(
            &planned_operation,
            &dispatch,
            planned_session,
            &FakeScope(Ok(configured_scope(worktree.path()))),
            Some(&planned_ticket),
            &planned,
        )
        .unwrap();

    let mut wrong_session = planned.clone();
    wrong_session.session_id = Some(SessionId::new());
    let wrong_session_operation = OperationId::new().to_string();
    let wrong_session_ticket = dispatch_runtime
        .prepare_dispatch_readiness(&wrong_session_operation, &dispatch)
        .unwrap()
        .unwrap();
    assert_eq!(
        dispatch_runtime
            .dispatch_planned_after_readiness(
                &wrong_session_operation,
                &dispatch,
                planned_session,
                &FakeScope(Ok(configured_scope(worktree.path()))),
                Some(&wrong_session_ticket),
                &wrong_session,
            )
            .unwrap_err()
            .code,
        ErrorCode::RevisionConflict
    );
    let existing_selection = DispatchIntent {
        agent: DispatchAgentIntent::Existing {
            agent_id: planned.agent_id,
        },
        ..dispatch
    };
    let mut wrong_agent = planned.clone();
    wrong_agent.agent_id = AgentId::new();
    let wrong_agent_operation = OperationId::new().to_string();
    let wrong_agent_ticket = dispatch_runtime
        .prepare_dispatch_readiness(&wrong_agent_operation, &existing_selection)
        .unwrap()
        .unwrap();
    assert_eq!(
        dispatch_runtime
            .dispatch_planned_after_readiness(
                &wrong_agent_operation,
                &existing_selection,
                planned_session,
                &FakeScope(Ok(configured_scope(worktree.path()))),
                Some(&wrong_agent_ticket),
                &wrong_agent,
            )
            .unwrap_err()
            .code,
        ErrorCode::RevisionConflict
    );
}

#[test]
fn retained_resources_lists_live_agent_terminals() {
    let mut runtime = runtime();
    let admission = runtime
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();

    assert_eq!(
        runtime.retained_resources(),
        std::iter::once(admission.terminal.terminal_id.as_str()).collect()
    );
}

#[test]
fn workspace_runtime_count_and_close_share_the_same_workspace_selector() {
    let mut runtime = AgentRuntime::new(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            terminate_success: true,
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    let launch_intent = intent(None);
    let workspace = launch_intent.workspace;
    let admission = runtime
        .launch(
            &OperationId::new().to_string(),
            &launch_intent,
            &FakeScope(Ok(scope())),
        )
        .unwrap();

    assert_eq!(runtime.retirement_blocker_count(workspace), 1);
    let running_snapshot = runtime.coordinator.snapshot();
    for (state, ownership_state, expected) in [
        (
            super::super::runtime::RuntimeState::ReconcileRequired(
                super::super::runtime::ReconcileState::OrphanRunning,
            ),
            super::super::generation::TerminalState::OrphanRunning,
            1,
        ),
        (
            super::super::runtime::RuntimeState::ReconcileRequired(
                super::super::runtime::ReconcileState::SpawnAmbiguous,
            ),
            super::super::generation::TerminalState::IdentityUnknown,
            1,
        ),
        (
            super::super::runtime::RuntimeState::ReconcileRequired(
                super::super::runtime::ReconcileState::PersistAfterSpawn,
            ),
            super::super::generation::TerminalState::IdentityUnknown,
            1,
        ),
        (
            super::super::runtime::RuntimeState::ReconcileRequired(
                super::super::runtime::ReconcileState::IdentityUnknown,
            ),
            super::super::generation::TerminalState::IdentityUnknown,
            0,
        ),
    ] {
        let mut snapshot = running_snapshot.clone();
        snapshot.records[0].state = state;
        snapshot.generation.terminals[0].state = ownership_state;
        runtime.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
        assert_eq!(runtime.retirement_blocker_count(workspace), expected);
    }
    runtime.coordinator = RuntimeCoordinator::hydrate(running_snapshot, 16, 64 * 1024, 64).unwrap();
    runtime.reported_phases.insert(
        runtime.coordinator.snapshot().records[0]
            .runtime
            .agent_runtime_id,
        AgentPhase::Running,
    );
    assert_eq!(runtime.close_workspace(workspace).unwrap(), 1);
    assert_eq!(runtime.retirement_blocker_count(workspace), 0);
    assert!(runtime.reported_phases.is_empty());
    assert!(
        !runtime
            .retained_resources()
            .contains(&admission.terminal.terminal_id.as_str())
    );
}

/// #522: every admission and every final — direct, replayed, or hydrated after
/// a restart — states the operation it belongs to and the digest of the intent
/// it was admitted for, so a client can refuse an answer that means something
/// else instead of promoting its terminal.
#[test]
fn agent_admissions_and_finals_carry_their_operation_and_semantic_digest() {
    let mut runtime = runtime();
    let fake_scope = FakeScope(Ok(scope()));
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let expected_digest = agent_operation_digest(
        &usagi_core::infrastructure::ipc::agent_launch_semantic_key(&launch_intent),
    );

    let admitted = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap();
    assert_eq!(admitted.operation_id, operation);
    assert_eq!(admitted.semantic_digest.as_deref(), Some(&*expected_digest));
    assert!(!admitted.completed);

    // A resend of the same operation replays the identical identity/digest.
    let replay = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap();
    assert_eq!(replay, admitted);

    // The single durable final keeps them and only adds `completed`.
    runtime.exit(&admitted.terminal, 0).unwrap();
    let completed = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap();
    assert!(completed.completed);
    assert_eq!(completed.operation_id, operation);
    assert_eq!(completed.semantic_digest, admitted.semantic_digest);
    assert_eq!(completed.terminal, admitted.terminal);
    assert_eq!(
        runtime.operation_outcome(&operation),
        Some(Ok(completed.clone())),
        "a reconnecting client reads exactly the same final"
    );

    // Another intent is another digest, so one identity cannot answer for both.
    let other = OperationId::new().to_string();
    let other_admission = runtime.launch(&other, &intent(None), &fake_scope).unwrap();
    assert_ne!(other_admission.semantic_digest, admitted.semantic_digest);
    assert_eq!(
        runtime
            .launch(&operation, &intent(None), &fake_scope)
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict,
        "the same identity with another intent stays a conflict"
    );
}

#[test]
fn process_local_operation_replay_is_bounded_without_retaining_intent_text() {
    let mut runtime = runtime();
    runtime.operation_bounds = AgentOperationBounds {
        operations: 2,
        bytes: usize::MAX,
        age_seconds: 60,
    };
    let launch = intent(None);
    let base = Utc::now();
    let operations = [
        OperationId::new().to_string(),
        OperationId::new().to_string(),
        OperationId::new().to_string(),
    ];
    for (index, operation) in operations.iter().enumerate() {
        assert!(
            runtime
                .launch(
                    operation,
                    &launch,
                    &FakeScope(Err(ScopeResolveError::Unavailable)),
                )
                .is_err()
        );
        if let Some(retained) = runtime.operations.get_mut(operation) {
            retained.recorded_at =
                base + chrono::Duration::seconds(i64::try_from(index).expect("three fixtures fit"));
        }
    }
    assert_eq!(runtime.operations.len(), 2);
    assert!(runtime.operation_outcome(&operations[0]).is_none());
    assert!(runtime.operation_outcome(&operations[1]).is_some());
    assert!(runtime.operation_outcome(&operations[2]).is_some());

    let long_intent = "private prompt ".repeat(1_000);
    let digested = OperationId::new().to_string();
    runtime.operation_bounds.operations = 3;
    let mut detailed_error = ProtocolError::new(ErrorCode::Unavailable, "fixture");
    detailed_error.details = Some(serde_json::json!({ "reason": "fixture" }));
    runtime.remember_operation(&digested, Some(&long_intent), Err(detailed_error));
    let retained = runtime.operations.get(&digested).unwrap();
    assert_eq!(retained.semantic_digest.as_ref().unwrap().len(), 64);
    assert_ne!(
        retained.semantic_digest.as_deref(),
        Some(long_intent.as_str())
    );

    runtime.operation_bounds.bytes = 1;
    runtime.prune_operations(Utc::now());
    assert!(runtime.operations.is_empty());
}

#[test]
fn operation_replay_age_expires_only_unprotected_history() {
    let mut runtime = runtime();
    runtime.operation_bounds = AgentOperationBounds {
        operations: 0,
        bytes: 0,
        age_seconds: 1,
    };
    let expired = OperationId::new().to_string();
    runtime.remember_operation(
        &expired,
        Some("expired"),
        Err(ProtocolError::new(ErrorCode::Unavailable, "fixture")),
    );
    assert!(
        runtime.operations.is_empty(),
        "byte/count pressure is immediate"
    );

    runtime.operation_bounds = AgentOperationBounds {
        operations: 8,
        bytes: usize::MAX,
        age_seconds: 1,
    };
    runtime.remember_operation(
        &expired,
        Some("expired"),
        Err(ProtocolError::new(ErrorCode::Unavailable, "fixture")),
    );
    runtime.operations.get_mut(&expired).unwrap().recorded_at =
        Utc::now() - chrono::Duration::seconds(2);
    runtime.prune_operations(Utc::now());
    assert!(!runtime.operations.contains_key(&expired));

    runtime.operation_bounds = AgentOperationBounds {
        operations: 0,
        bytes: 0,
        age_seconds: 0,
    };
    let protected = OperationId::new().to_string();
    runtime
        .launch(&protected, &intent(None), &FakeScope(Ok(scope())))
        .unwrap();
    assert!(runtime.operations.contains_key(&protected));
}

#[test]
fn operation_replay_eviction_breaks_timestamp_ties_by_identity() {
    let mut runtime = runtime();
    runtime.operation_bounds = AgentOperationBounds {
        operations: 1,
        bytes: usize::MAX,
        age_seconds: 60,
    };
    let recorded_at = Utc::now();
    let mut operations = [
        OperationId::new().to_string(),
        OperationId::new().to_string(),
    ];
    operations.sort();
    for operation in &operations {
        runtime.operations.insert(
            operation.clone(),
            AgentOperation::new(
                Some("same intent"),
                Err(ProtocolError::new(ErrorCode::Unavailable, "fixture")),
                recorded_at,
            ),
        );
    }

    runtime.prune_operations(recorded_at);

    assert!(!runtime.operations.contains_key(&operations[0]));
    assert!(runtime.operations.contains_key(&operations[1]));
}

#[test]
fn normal_exit_releases_ephemeral_caller_and_phase_state() {
    let mut runtime = runtime();
    let operation = OperationId::new().to_string();
    let admission = runtime
        .launch(&operation, &intent(None), &FakeScope(Ok(scope())))
        .unwrap();
    let runtime_id = runtime
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap()
        .agent_runtime_id;
    runtime
        .reported_phases
        .insert(runtime_id, AgentPhase::Running);
    assert_eq!(runtime.mcp_callers.len(), 1);

    runtime.exit(&admission.terminal, 0).unwrap();

    assert!(runtime.mcp_callers.is_empty());
    assert!(!runtime.reported_phases.contains_key(&runtime_id));
    assert!(runtime.operations.contains_key(&operation));
}

#[test]
fn session_close_removes_the_agent_from_runtime_inventory_and_replay() {
    let mut runtime = runtime();
    pty_mut(&mut runtime).terminate_success = true;
    let session = SessionId::new();
    let launch_intent = AgentLaunchIntent {
        workspace: WorkspaceId::new(),
        session: Some(session),
        profile: None,
    };
    let operation = OperationId::new().to_string();
    runtime
        .launch(&operation, &launch_intent, &FakeScope(Ok(scope())))
        .unwrap();
    let runtime_id = runtime.coordinator.snapshot().records[0]
        .runtime
        .agent_runtime_id;
    runtime
        .reported_phases
        .insert(runtime_id, AgentPhase::Running);

    assert_eq!(
        runtime.managed_session_ids(),
        [session].into_iter().collect()
    );
    assert_eq!(runtime.close_session(session).unwrap(), 1);
    assert!(
        runtime
            .inventory(launch_intent.workspace)
            .runtimes
            .is_empty()
    );
    assert!(runtime.managed_session_ids().is_empty());
    assert_eq!(runtime.operation_outcome(&operation), None);
    assert!(runtime.mcp_callers.is_empty());
    assert!(runtime.reported_phases.is_empty());
}

#[test]
fn reported_phase_refines_a_live_projection_but_never_outranks_observation() {
    let mut runtime = structured_claude_runtime();
    let session = SessionId::new();
    let launch_intent = AgentLaunchIntent {
        workspace: WorkspaceId::new(),
        session: Some(session),
        profile: None,
    };
    let launched = runtime
        .launch(
            &OperationId::new().to_string(),
            &launch_intent,
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();
    assert_eq!(runtime.session_phase(session), AgentPhase::Running);
    assert!(
        runtime.coordinator.snapshot().records[0]
            .provider_resume
            .is_none(),
        "fresh Claude metadata comes only from SessionStart"
    );

    runtime
        .report_agent_phase_with_session(
            &credential,
            AgentPhase::Ready,
            Some(ProviderSessionId::new("claude-session").unwrap()),
        )
        .unwrap();
    let captured = runtime.coordinator.snapshot();
    let reference = captured.records[0].provider_resume.as_ref().unwrap();
    assert_eq!(reference.provider, ProviderKind::Claude);
    assert_eq!(
        reference.provenance,
        ProviderCaptureProvenance::ProviderStructured
    );
    assert_eq!(runtime.session_phase(session), AgentPhase::Ready);

    // Only the daemon-minted credential selects the reporting runtime.
    assert_eq!(
        runtime
            .report_agent_phase("unknown-credential", AgentPhase::Waiting)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(runtime.session_phase(session), AgentPhase::Ready);

    for phase in [
        AgentPhase::Ready,
        AgentPhase::Running,
        AgentPhase::Waiting,
        AgentPhase::Ended,
        AgentPhase::Exited,
    ] {
        runtime.report_agent_phase(&credential, phase).unwrap();
        assert_eq!(runtime.session_phase(session), phase);
    }
    for phase in [AgentPhase::Absent, AgentPhase::Interrupted] {
        assert_eq!(
            runtime
                .report_agent_phase(&credential, phase)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }

    // `exited` proves no process death, so the durable safe phase keeps the
    // last value a live report could justify.
    assert_eq!(
        durable_phase(&runtime),
        Some(ProviderResumePhase::Running),
        "reported exit must not write a durable end"
    );

    // An unchanged durable phase does not rewrite the snapshot; a changed one does.
    let saves = store_mut(&mut runtime).saves;
    runtime
        .report_agent_phase(&credential, AgentPhase::Running)
        .unwrap();
    assert_eq!(store_mut(&mut runtime).saves, saves);
    runtime
        .report_agent_phase(&credential, AgentPhase::Ready)
        .unwrap();
    assert_eq!(durable_phase(&runtime), Some(ProviderResumePhase::Starting));
    assert_eq!(store_mut(&mut runtime).saves, saves + 1);

    // The observed exit outranks the last report, and the credential dies
    // with the runtime it was minted for.
    runtime
        .report_agent_phase(&credential, AgentPhase::Running)
        .unwrap();
    runtime.exit(&launched.terminal, 0).unwrap();
    assert_eq!(runtime.session_phase(session), AgentPhase::Ended);
    assert_eq!(
        runtime
            .report_agent_phase(&credential, AgentPhase::Running)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
fn session_start_capture_replaces_the_current_conversation_for_both_providers() {
    for (mut runtime, profile, provider) in [
        (structured_claude_runtime(), None, ProviderKind::Claude),
        (codex_runtime(), Some("codex"), ProviderKind::Codex),
    ] {
        let session = SessionId::new();
        runtime
            .launch(
                &OperationId::new().to_string(),
                &AgentLaunchIntent {
                    workspace: WorkspaceId::new(),
                    session: Some(session),
                    profile: profile.map(|value| AgentProfileId::new(value).unwrap()),
                },
                &FakeScope(Ok(scope())),
            )
            .unwrap();
        let credential = runtime.mcp_callers.keys().next().cloned().unwrap();

        runtime
            .report_agent_phase_with_session(
                &credential,
                AgentPhase::Ready,
                Some(ProviderSessionId::new("first-session").unwrap()),
            )
            .unwrap();
        runtime
            .report_agent_phase_with_session(
                &credential,
                AgentPhase::Ready,
                Some(ProviderSessionId::new("after-clear").unwrap()),
            )
            .unwrap();
        if provider == ProviderKind::Codex {
            // A v4 dedicated capture hook can overlap a v5 common phase
            // hook while an already-running process picks up a new binary.
            runtime
                .capture_codex_session(&credential, ProviderSessionId::new("after-clear").unwrap())
                .unwrap();
        }

        let snapshot = runtime.coordinator.snapshot();
        let reference = snapshot.records[0].provider_resume.as_ref().unwrap();
        assert_eq!(reference.provider, provider);
        assert_eq!(
            reference.native_session_id.expose_sensitive(),
            "after-clear"
        );
        assert_eq!(
            reference.provenance,
            ProviderCaptureProvenance::ProviderStructured
        );
        assert_eq!(reference.last_known_status, ProviderResumeStatus::Active);
        assert_eq!(
            reference.last_known_phase,
            Some(ProviderResumePhase::Starting)
        );
        assert_eq!(runtime.session_phase(session), AgentPhase::Ready);

        assert_eq!(
            runtime
                .report_agent_phase_with_session(
                    &credential,
                    AgentPhase::Running,
                    Some(ProviderSessionId::new("forged-transition").unwrap()),
                )
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            runtime.coordinator.snapshot().records[0]
                .provider_resume
                .as_ref()
                .unwrap()
                .native_session_id
                .expose_sensitive(),
            "after-clear"
        );
    }
}

#[test]
fn antigravity_pre_invocation_captures_conversation_and_running_phase() {
    let mut runtime = agy_runtime();
    let session = SessionId::new();
    runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace: WorkspaceId::new(),
                session: Some(session),
                profile: Some(AgentProfileId::new("agy").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();

    runtime
        .report_agent_phase_with_session(
            &credential,
            AgentPhase::Running,
            Some(ProviderSessionId::new("agy-conversation").unwrap()),
        )
        .unwrap();

    let snapshot = runtime.coordinator.snapshot();
    let reference = snapshot.records[0].provider_resume.as_ref().unwrap();
    assert_eq!(reference.provider, ProviderKind::Agy);
    assert_eq!(
        reference.native_session_id.expose_sensitive(),
        "agy-conversation"
    );
    assert_eq!(
        reference.last_known_phase,
        Some(ProviderResumePhase::Running)
    );
    assert_eq!(runtime.session_phase(session), AgentPhase::Running);
}

#[test]
fn session_start_capture_ignores_headless_and_rejects_unsupported_profiles() {
    let mut runtime = structured_claude_runtime();
    runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace: WorkspaceId::new(),
                session: Some(SessionId::new()),
                profile: None,
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();

    let mut snapshot = runtime.coordinator.snapshot();
    snapshot.records[0].launch.request.mode = LaunchMode::Headless;
    runtime.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    runtime
        .report_agent_phase_with_session(
            &credential,
            AgentPhase::Ready,
            Some(ProviderSessionId::new("ignored-headless-session").unwrap()),
        )
        .unwrap();
    assert!(
        runtime.coordinator.snapshot().records[0]
            .provider_resume
            .is_none()
    );

    let mut snapshot = runtime.coordinator.snapshot();
    snapshot.records[0].launch.request.mode = LaunchMode::Interactive;
    snapshot.records[0].launch.request.profile_id = AgentProfileId::new("shell").unwrap();
    snapshot.records[0].launch.plan.profile_id = AgentProfileId::new("shell").unwrap();
    runtime.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    assert_eq!(
        runtime
            .report_agent_phase_with_session(
                &credential,
                AgentPhase::Ready,
                Some(ProviderSessionId::new("unsupported-session").unwrap()),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn stale_process_local_credentials_fail_closed_at_structured_boundaries() {
    let mut runtime = codex_runtime();
    let admission = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace: WorkspaceId::new(),
                session: Some(SessionId::new()),
                profile: Some(AgentProfileId::new("codex").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let runtime_ref = runtime
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();
    runtime
        .coordinator
        .exit(&runtime_ref, 0, &mut *runtime.store)
        .unwrap();
    assert!(runtime.mcp_callers.contains_key(&credential));

    assert_eq!(
        runtime
            .report_agent_phase(&credential, AgentPhase::Running)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        runtime
            .capture_codex_session(&credential, ProviderSessionId::new("late-session").unwrap(),)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
fn phase_report_without_provider_metadata_refines_only_the_projection() {
    let mut runtime = codex_runtime();
    let session = SessionId::new();
    let launch_intent = AgentLaunchIntent {
        workspace: WorkspaceId::new(),
        session: Some(session),
        profile: Some(AgentProfileId::new("codex").unwrap()),
    };
    runtime
        .launch(
            &OperationId::new().to_string(),
            &launch_intent,
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();
    // Codex has no provider metadata before its structured capture, so the
    // report has nothing durable to refine and still must not fail.
    assert!(
        runtime.coordinator.snapshot().records[0]
            .provider_resume
            .is_none()
    );
    let saves = store_mut(&mut runtime).saves;
    runtime
        .report_agent_phase(&credential, AgentPhase::Waiting)
        .unwrap();
    assert_eq!(runtime.session_phase(session), AgentPhase::Waiting);
    assert_eq!(store_mut(&mut runtime).saves, saves);
}

#[test]
#[allow(clippy::too_many_lines)] // One end-to-end test keeps capture, exit, resume, replay, and live rejection visibly ordered.
fn structured_codex_identity_enables_one_explicit_new_runtime_resume() {
    let mut runtime = codex_runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let launch_intent = AgentLaunchIntent {
        workspace,
        session: Some(session),
        profile: Some(AgentProfileId::new("codex").unwrap()),
    };
    let initial_operation = OperationId::new();
    let first = runtime
        .launch(
            &initial_operation.to_string(),
            &launch_intent,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    assert_eq!(
        runtime.session_resume_status(session),
        (false, ProviderResumeReason::LiveOrOwnershipUnknown)
    );
    let first_runtime = runtime
        .coordinator
        .runtime_for_terminal(&first.terminal)
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();
    let native_id = ProviderSessionId::new("structured-codex-session").unwrap();
    assert_eq!(
        runtime
            .capture_codex_session(
                "unknown-credential",
                ProviderSessionId::new("ignored-session").unwrap(),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        runtime
            .capture_structured_provider_session(
                &first_runtime,
                ProviderKind::Claude,
                native_id.clone(),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    runtime
        .capture_codex_session(&credential, native_id.clone())
        .unwrap();
    assert_eq!(
        runtime
            .capture_codex_session(
                &credential,
                ProviderSessionId::new("different-session").unwrap(),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    let captured = runtime.coordinator.snapshot();
    assert_eq!(
        captured.records[0]
            .provider_resume
            .as_ref()
            .unwrap()
            .provenance,
        ProviderCaptureProvenance::ProviderStructured
    );
    assert!(
        !serde_json::to_string(&captured.records[0].launch)
            .unwrap()
            .contains(native_id.expose_sensitive())
    );

    runtime.exit(&first.terminal, 0).unwrap();
    let target = runtime
        .inventory(workspace)
        .resumable
        .into_iter()
        .find_map(|item| item.target)
        .unwrap();
    let mut wrong_capture_policy = runtime.coordinator.snapshot().records[0].clone();
    wrong_capture_policy
        .provider_resume
        .as_mut()
        .unwrap()
        .provenance = ProviderCaptureProvenance::DaemonIssued;
    assert_eq!(
        runtime.resume_source_availability(
            &wrong_capture_policy,
            std::slice::from_ref(&wrong_capture_policy),
        ),
        (false, ProviderResumeReason::IncompatibleProviderMetadata)
    );
    assert_eq!(
        runtime
            .capture_codex_session(&credential, ProviderSessionId::new("late-session").unwrap(),)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        runtime
            .resume_exact(
                &initial_operation.to_string(),
                &target,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    assert_eq!(
        runtime
            .resume_exact(
                "not-an-operation-id",
                &target,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .admit_resume_exact(
                &initial_operation.to_string(),
                &target,
                &resume_semantic_key(&target),
                &FakeScope(Ok(resolved.clone())),
                None,
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        runtime.session_resume_status(session),
        (true, ProviderResumeReason::ExplicitResumeAvailable)
    );
    let mut ambiguous_snapshot = runtime.coordinator.snapshot();
    let mut ambiguous_record = ambiguous_snapshot.records[0].clone();
    let mut ambiguous_ownership = ambiguous_snapshot
        .generation
        .terminals
        .iter()
        .find(|ownership| {
            ownership
                .terminal
                .fences(&ambiguous_record.runtime.terminal)
        })
        .unwrap()
        .clone();
    ambiguous_record.runtime.agent_runtime_id = AgentRuntimeId::new();
    ambiguous_record.continuation = Some(AgentContinuationRef::new());
    ambiguous_record.resume_source = Some(usagi_core::domain::id::AgentResumeSourceId::new());
    let ambiguous_terminal_id = TerminalId::new();
    ambiguous_record.runtime.terminal.terminal_id = ambiguous_terminal_id;
    ambiguous_ownership.terminal.terminal_id = ambiguous_terminal_id;
    ambiguous_record.operation.operation_id = OperationId::new();
    ambiguous_record.semantic_key = Some("ambiguous-resume-source".into());
    ambiguous_record
        .provider_resume
        .as_mut()
        .unwrap()
        .native_session_id = ProviderSessionId::new("other-codex-session").unwrap();
    ambiguous_snapshot.records.push(ambiguous_record);
    ambiguous_snapshot
        .generation
        .terminals
        .push(ambiguous_ownership);
    let original_coordinator = std::mem::replace(
        &mut runtime.coordinator,
        RuntimeCoordinator::hydrate(ambiguous_snapshot, 16, 64 * 1024, 64).unwrap(),
    );
    assert_eq!(
        runtime.session_resume_status(session),
        (false, ProviderResumeReason::AmbiguousProviderMetadata)
    );
    runtime.coordinator = original_coordinator;

    let original_registry = std::mem::replace(&mut runtime.registry, AdapterRegistry::new());
    assert_eq!(
        runtime.session_resume_status(session),
        (false, ProviderResumeReason::IncompatibleProviderMetadata)
    );
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    runtime.registry = original_registry;

    let inner = CodexAdapter::new(FakeCodexProvisioner);
    let mut profile = inner.profile().clone();
    profile.capabilities.remove(&AgentCapability::Resume);
    let mut incompatible_registry = AdapterRegistry::new();
    incompatible_registry
        .register(
            profile.clone(),
            Box::new(ProfileOverrideAdapter { profile, inner }),
        )
        .unwrap();
    let original_registry = std::mem::replace(&mut runtime.registry, incompatible_registry);
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    runtime.registry = original_registry;

    assert_eq!(
        runtime
            .resume_exact(
                "not-an-operation-id",
                &target,
                &FakeScope(Ok(resolved.clone()))
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );
    let operation = OperationId::new().to_string();
    let resumed = runtime
        .resume_exact(&operation, &target, &FakeScope(Ok(resolved.clone())))
        .unwrap();
    assert_ne!(resumed.terminal, first.terminal);
    assert_eq!(resumed.continuation, Some(target.continuation));
    assert_eq!(
        resumed.resume_relation.as_ref().unwrap().source,
        target.source
    );
    assert_eq!(
        runtime
            .resume_exact(&operation, &target, &FakeScope(Ok(resolved.clone())))
            .unwrap()
            .terminal,
        resumed.terminal
    );
    let mut conflicting = target.clone();
    conflicting.runtime_id = AgentRuntimeId::new();
    assert_eq!(
        runtime
            .resume_exact(&operation, &conflicting, &FakeScope(Ok(resolved.clone())))
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    let double_click = runtime
        .resume_exact(
            &OperationId::new().to_string(),
            &target,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    assert_eq!(double_click.terminal, resumed.terminal);
    assert_eq!(double_click.continuation, resumed.continuation);
    assert_eq!(double_click.resume_relation, resumed.resume_relation);
    // The workspace-wide question a retirement asks: this workspace has a
    // running Agent, another workspace does not.
    assert!(runtime.has_running_agent(workspace));
    assert!(!runtime.has_running_agent(WorkspaceId::new()));
    let inventory = runtime.inventory(workspace);
    assert_eq!(inventory.runtimes.len(), 2);
    assert!(
        inventory
            .runtimes
            .iter()
            .all(|item| item.continuation == target.continuation)
    );
    assert_eq!(
        inventory.resumable[0].reason,
        ProviderResumeReason::SourceAlreadySuperseded
    );
    assert_eq!(runtime.coordinator.snapshot().records.len(), 2);

    let mut live_replacement = runtime.coordinator.snapshot();
    for record in &mut live_replacement.records {
        if record.runtime.agent_runtime_id == target.runtime_id {
            record.superseded_by = None;
        }
        if record.resumed_from == Some(target.source) {
            record.resumed_from = None;
        }
    }
    runtime.coordinator = RuntimeCoordinator::hydrate(live_replacement, 16, 64 * 1024, 64).unwrap();
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(resolved)),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
fn codex_without_structured_identity_fails_closed_for_resume() {
    let mut runtime = codex_runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let first = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("codex").unwrap()),
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    runtime.exit(&first.terminal, 0).unwrap();
    assert_eq!(
        runtime.session_resume_status(session),
        (false, ProviderResumeReason::ProviderMetadataUnavailable)
    );
    let item = &runtime.inventory(workspace).resumable[0];
    assert!(item.target.is_some());
    assert!(!item.available);
    assert_eq!(
        item.reason,
        ProviderResumeReason::ProviderMetadataUnavailable
    );
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                item.target.as_ref().unwrap(),
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
}

#[test]
#[allow(clippy::too_many_lines)] // The fixture fixes root/session and mixed-provider ordering in one inventory.
fn exact_inventory_separates_root_sessions_and_same_scope_histories() {
    let mut registry = claude_registry();
    let codex = CodexAdapter::new(FakeCodexProvisioner);
    registry
        .register(codex.profile().clone(), Box::new(codex))
        .unwrap();
    let mut runtime = AgentRuntime::with_dispatch_and_locator(
        DaemonGeneration::new(),
        registry,
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(tempfile::tempdir().unwrap().keep()),
        PathExecutableLocator,
    );
    let workspace = WorkspaceId::new();
    let session_a = SessionId::new();
    let session_b = SessionId::new();
    let root_scope = scope();
    let ambiguous_scope = scope();
    let captured_codex_scope = scope();

    let root = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(root_scope.clone())),
        )
        .unwrap();
    runtime.exit(&root.terminal, 0).unwrap();

    let first_a = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session_a),
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(ambiguous_scope.clone())),
        )
        .unwrap();
    runtime.exit(&first_a.terminal, 0).unwrap();
    let second_a = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session_a),
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(ambiguous_scope.clone())),
        )
        .unwrap();
    runtime.exit(&second_a.terminal, 0).unwrap();

    let codex_b = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session_b),
                profile: Some(AgentProfileId::new("codex").unwrap()),
            },
            &FakeScope(Ok(captured_codex_scope)),
        )
        .unwrap();
    let codex_b_runtime = runtime
        .coordinator
        .runtime_for_terminal(&codex_b.terminal)
        .unwrap();
    runtime
        .capture_structured_provider_session(
            &codex_b_runtime,
            ProviderKind::Codex,
            ProviderSessionId::new("inventory-codex-session").unwrap(),
        )
        .unwrap();
    runtime.exit(&codex_b.terminal, 0).unwrap();

    let codex_without_capture = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session_a),
                profile: Some(AgentProfileId::new("codex").unwrap()),
            },
            &FakeScope(Ok(ambiguous_scope.clone())),
        )
        .unwrap();
    runtime.exit(&codex_without_capture.terminal, 0).unwrap();

    let inventory = runtime.inventory(workspace);
    assert_eq!(inventory, runtime.inventory(workspace));
    assert_eq!(inventory.runtimes.len(), 5);
    assert_eq!(inventory.resumable.len(), 5);
    assert_eq!(
        inventory
            .resumable
            .iter()
            .filter(|item| item.available)
            .count(),
        4
    );
    assert!(inventory.resumable.iter().any(|item| {
        item.target
            .as_ref()
            .is_some_and(|target| target.session_id.is_none())
    }));
    assert_eq!(
        inventory
            .resumable
            .iter()
            .filter(|item| {
                item.target
                    .as_ref()
                    .is_some_and(|target| target.session_id == Some(session_a))
            })
            .count(),
        3
    );
    // The safe provider vocabulary lets a client label an interrupted tab
    // per provider; it is absent exactly when no metadata was retained, and
    // it never travels with the provider-native ID.
    assert_eq!(
        inventory
            .resumable
            .iter()
            .filter(|item| item.provider == Some(ProviderKind::Claude))
            .count(),
        3
    );
    assert_eq!(
        inventory
            .resumable
            .iter()
            .filter(|item| item.provider == Some(ProviderKind::Codex))
            .count(),
        1
    );
    assert_eq!(
        inventory
            .resumable
            .iter()
            .filter(|item| item.provider.is_none())
            .count(),
        1
    );
    assert!(
        inventory
            .resumable
            .iter()
            .all(|item| item.provider.is_some() == item.available)
    );
    let encoded = serde_json::to_string(&inventory).unwrap();
    assert!(!encoded.contains("inventory-codex-session"));
    let selected = inventory
        .resumable
        .iter()
        .find_map(|item| {
            item.target.as_ref().filter(|target| {
                target.runtime_id
                    == runtime
                        .coordinator
                        .runtime_for_terminal(&first_a.terminal)
                        .unwrap()
                        .agent_runtime_id
            })
        })
        .unwrap()
        .clone();
    let untouched_runtime = runtime
        .coordinator
        .runtime_for_terminal(&second_a.terminal)
        .unwrap();
    let resumed = runtime
        .resume_exact(
            &OperationId::new().to_string(),
            &selected,
            &FakeScope(Ok(ambiguous_scope)),
        )
        .unwrap();
    assert_eq!(resumed.continuation, Some(selected.continuation));
    assert!(
        runtime
            .coordinator
            .record_for(&untouched_runtime)
            .unwrap()
            .superseded_by
            .is_none()
    );

    let root_target = inventory
        .resumable
        .iter()
        .filter(|item| item.available)
        .find_map(|item| {
            item.target
                .as_ref()
                .filter(|target| target.session_id.is_none())
        })
        .unwrap();
    let root_resumed = runtime
        .resume_exact(
            &OperationId::new().to_string(),
            root_target,
            &FakeScope(Ok(root_scope)),
        )
        .unwrap();
    assert!(root_resumed.terminal.session_id.is_none());
}

#[test]
fn exact_resume_rejects_every_public_fence_before_spawn() {
    let mut runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let launched = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    let live_runtime = runtime
        .coordinator
        .runtime_for_terminal(&launched.terminal)
        .unwrap();
    let live_target =
        resume_target(runtime.coordinator.record_for(&live_runtime).unwrap()).unwrap();
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &live_target,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    runtime.exit(&launched.terminal, 0).unwrap();
    let target = runtime.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();

    let mut stale_targets = Vec::new();
    let mut stale = target.clone();
    stale.continuation = AgentContinuationRef::new();
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.source = usagi_core::domain::id::AgentResumeSourceId::new();
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.workspace_id = WorkspaceId::new();
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.session_id = None;
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.worktree_id = WorktreeId::new();
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.runtime_id = AgentRuntimeId::new();
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.adapter_revision += 1;
    stale_targets.push(stale);
    for stale in stale_targets {
        assert_eq!(
            runtime
                .resume_exact(
                    &OperationId::new().to_string(),
                    &stale,
                    &FakeScope(Ok(resolved.clone())),
                )
                .unwrap_err()
                .code,
            ErrorCode::StaleTarget
        );
    }
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Err(ScopeResolveError::Unavailable)),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );
    assert_eq!(runtime.coordinator.snapshot().records.len(), 1);
}

#[test]
fn exact_resume_spawn_failure_removes_only_its_ephemeral_credential() {
    let mut runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let launched = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    runtime.exit(&launched.terminal, 0).unwrap();
    let target = runtime.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    let callers_before = runtime.mcp_callers.len();
    pty_mut(&mut runtime).spawn = Some(SpawnFailure::Definite);
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(resolved)),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(runtime.mcp_callers.len(), callers_before);
}

#[test]
fn a_source_tombstone_refuses_replay_after_replacement_history_is_collected() {
    let mut runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let launched = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    runtime.exit(&launched.terminal, 0).unwrap();
    let target = runtime.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    let mut snapshot = runtime.coordinator.snapshot();
    snapshot.records[0].superseded_by = Some(AgentRuntimeId::new());
    runtime.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();

    let error = runtime
        .resume_exact(
            &OperationId::new().to_string(),
            &target,
            &FakeScope(Ok(resolved)),
        )
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::StaleTarget);
    assert_eq!(
        error.message,
        "agent resume replacement history was collected"
    );
}

#[test]
fn runtime_inventory_states_and_live_fences_cover_every_durable_variant() {
    use super::super::runtime::{ReconcileState, RuntimeState};

    for (state, expected) in [
        (RuntimeState::Reserved, AgentRuntimeInventoryState::Reserved),
        (RuntimeState::Running, AgentRuntimeInventoryState::Live),
        (
            RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown),
            AgentRuntimeInventoryState::Interrupted,
        ),
        (
            RuntimeState::Interrupted,
            AgentRuntimeInventoryState::Interrupted,
        ),
        (RuntimeState::Exited, AgentRuntimeInventoryState::Exited),
        (
            RuntimeState::Reclaimed,
            AgentRuntimeInventoryState::Reclaimed,
        ),
        (
            RuntimeState::SpawnFailed,
            AgentRuntimeInventoryState::Unavailable,
        ),
        (
            RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning),
            AgentRuntimeInventoryState::Unavailable,
        ),
    ] {
        assert_eq!(runtime_inventory_state(state), expected);
    }
    for state in [
        RuntimeState::Reserved,
        RuntimeState::Running,
        RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning),
        RuntimeState::ReconcileRequired(ReconcileState::SpawnAmbiguous),
        RuntimeState::ReconcileRequired(ReconcileState::PersistAfterSpawn),
        RuntimeState::ReconcileRequired(ReconcileState::PersistAfterExit),
    ] {
        assert!(holds_live_or_unknown_agent(state));
    }
    assert!(!holds_live_or_unknown_agent(RuntimeState::Exited));
    for state in [
        RuntimeState::Exited,
        RuntimeState::Reclaimed,
        RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown),
    ] {
        assert!(is_resume_source_state(state));
    }
    assert!(!is_resume_source_state(RuntimeState::Running));
}

#[test]
fn schema_v3_runtime_without_public_lineage_loads_as_resume_unavailable() {
    let mut runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let launched = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    runtime.exit(&launched.terminal, 0).unwrap();
    let mut legacy = runtime.coordinator.snapshot();
    legacy.schema_version = 3;
    let mut partial_lineage = legacy.records[0].clone();
    partial_lineage.continuation = Some(AgentContinuationRef::new());
    partial_lineage.resume_source = None;
    assert!(resume_target(&partial_lineage).is_none());
    legacy.records[0].continuation = None;
    legacy.records[0].resume_source = None;
    runtime.coordinator = RuntimeCoordinator::hydrate(legacy, 16, 64 * 1024, 64).unwrap();

    let inventory = runtime.inventory(workspace);
    assert!(inventory.runtimes.is_empty());
    assert_eq!(inventory.resumable.len(), 1);
    assert!(inventory.resumable[0].target.is_none());
    assert!(!inventory.resumable[0].available);
    assert_eq!(
        inventory.resumable[0].reason,
        ProviderResumeReason::ProviderMetadataUnavailable
    );
}

#[test]
fn restart_resume_supersedes_the_interrupted_runtime_without_leaking_capacity() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let mut first = restart_runtime();
    let initial = first
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    let initial_runtime = first
        .coordinator
        .runtime_for_terminal(&initial.terminal)
        .unwrap();
    let continuation = initial.continuation.unwrap();
    let (reconciled, interrupted) = first
        .coordinator
        .snapshot()
        .reconcile_after_daemon_restart();
    assert_eq!(interrupted, 1);

    let mut second = hydrate_restart_runtime(reconciled);
    // Restart preserves the dispatch journal as well as runtime state;
    // exact resume must not guess an Agent from its provider/model tuple.
    second.dispatch = first.dispatch.clone();
    assert_eq!(second.session_phase(session), AgentPhase::Interrupted);
    assert_eq!(second.coordinator.occupied_slots(), 1);

    let target = second.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    assert_eq!(target.continuation, continuation);
    let resume_operation = OperationId::new().to_string();
    let resumed = second
        .resume_exact(&resume_operation, &target, &FakeScope(Ok(resolved.clone())))
        .unwrap();
    assert_ne!(resumed.terminal, initial.terminal);
    assert_eq!(resumed.continuation, Some(continuation));
    assert_eq!(
        resumed.resume_relation.as_ref().unwrap().source,
        target.source
    );
    assert_eq!(second.coordinator.occupied_slots(), 1);
    let superseded = second.coordinator.record_for(&initial_runtime).unwrap();
    assert_eq!(
        superseded.state,
        super::super::runtime::RuntimeState::Reclaimed
    );
    assert_eq!(
        superseded
            .provider_resume
            .as_ref()
            .unwrap()
            .last_known_status,
        ProviderResumeStatus::Exited
    );
    assert_eq!(
        superseded.superseded_by,
        resumed
            .resume_relation
            .as_ref()
            .map(|relation| relation.replacement_runtime)
    );
    assert!(matches!(
        second.coordinator.retention().lookup(&initial.terminal),
        usagi_core::domain::terminal_retention::FinalLookup::Retained(_)
    ));
    second.coordinator.snapshot().validate_ownership().unwrap();

    let (reconciled_again, interrupted_again) = second
        .coordinator
        .snapshot()
        .reconcile_after_daemon_restart();
    assert_eq!(interrupted_again, 1);
    let mut third = hydrate_restart_runtime(reconciled_again);
    third.dispatch = second.dispatch.clone();
    let replay = third
        .resume_exact(&resume_operation, &target, &FakeScope(Ok(resolved.clone())))
        .unwrap();
    assert_eq!(replay.terminal, resumed.terminal);
    assert_eq!(replay.continuation, Some(continuation));
    assert_eq!(replay.resume_relation, resumed.resume_relation);
    let double_click = third
        .resume_exact(
            &OperationId::new().to_string(),
            &target,
            &FakeScope(Ok(resolved)),
        )
        .unwrap();
    assert_eq!(double_click.terminal, resumed.terminal);
    assert_eq!(double_click.resume_relation, resumed.resume_relation);
    assert_eq!(third.coordinator.snapshot().records.len(), 2);

    assert_eq!(third.session_phase(session), AgentPhase::Interrupted);
}

#[test]
#[allow(clippy::too_many_lines)] // One restart scenario keeps the two runtime instances and shared file visibly ordered.
fn restart_hydrates_file_snapshot_before_dispatch_admission_and_preserves_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let snapshot_path = dir.path().join("agents.json");
    let dispatch_dir = dir.path().join("dispatch");
    let executable_dir = tempfile::tempdir().unwrap();
    std::fs::write(executable_dir.path().join("claude"), "fixture").unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let resolved = configured_scope(worktree.path());
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let caller = CallerRef {
        session_id: Some(SessionId::new()),
        agent_id: usagi_core::domain::id::AgentId::new(),
    };
    let dispatch_intent = |prompt: &str| DispatchIntent {
        workspace,
        session_name: "worker".into(),
        caller: caller.clone(),
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: usagi_core::domain::agent::ModelSelector::new("test").unwrap(),
        },
        prompt: prompt.into(),
    };
    let spawns = Arc::new(AtomicU32::new(0));
    let make_fresh = || {
        AgentRuntime::with_dispatch_and_locator(
            DaemonGeneration::new(),
            claude_registry(),
            Store {
                snapshot_path: Some(snapshot_path.clone()),
                ..Store::default()
            },
            Journal::default(),
            Pty {
                spawn_counter: Some(Arc::clone(&spawns)),
                ..Pty::default()
            },
            AgentProfileId::new("claude").unwrap(),
            Geometry { cols: 80, rows: 24 },
            DispatchStore::new(&dispatch_dir),
            FixtureLocator(executable_dir.path().to_path_buf()),
        )
    };
    let mut first = make_fresh();
    let successful = OperationId::new().to_string();
    let success_terminal = first
        .dispatch(
            &successful,
            &dispatch_intent("success"),
            session,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap()
        .terminal;
    first.exit(&success_terminal, 0).unwrap();
    let unsuccessful = OperationId::new().to_string();
    let failed_terminal = first
        .dispatch(
            &unsuccessful,
            &dispatch_intent("failure"),
            session,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap()
        .terminal;
    first.exit(&failed_terminal, 17).unwrap();
    let interrupted = OperationId::new().to_string();
    first
        .dispatch(
            &interrupted,
            &dispatch_intent("pending"),
            session,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    let old_credential = first.mcp_callers.keys().next().unwrap().clone();
    assert_eq!(spawns.load(Ordering::SeqCst), 3);
    drop(first);

    let loaded: RuntimeStoreSnapshot =
        serde_json::from_slice(&std::fs::read(&snapshot_path).unwrap()).unwrap();
    loaded.validate_schema().unwrap();
    loaded.validate_ownership().unwrap();
    let interrupted_record = loaded
        .records
        .iter()
        .find(|record| record.operation.operation_id.to_string() == interrupted)
        .unwrap()
        .clone();
    let (reconciled, count) = loaded.reconcile_after_daemon_restart();
    assert_eq!(count, 1);
    let reconciled_interrupted = reconciled
        .records
        .iter()
        .find(|record| record.operation.operation_id.to_string() == interrupted)
        .unwrap();
    assert_eq!(
        reconciled_interrupted
            .provider_resume
            .as_ref()
            .unwrap()
            .last_known_status,
        ProviderResumeStatus::Interrupted
    );
    assert_eq!(
        reconciled_interrupted
            .provider_resume
            .as_ref()
            .unwrap()
            .last_known_phase,
        Some(ProviderResumePhase::Interrupted)
    );
    assert!(reconciled.generation.current.is_none());
    assert!(
        reconciled
            .generation
            .records
            .iter()
            .all(|record| { record.role == super::super::generation::GenerationRole::Retired })
    );
    Store {
        snapshot_path: Some(snapshot_path.clone()),
        ..Store::default()
    }
    .save(reconciled.clone())
    .unwrap();
    let mut second = AgentRuntime::hydrate_with_dispatch_and_locator(
        DaemonGeneration::new(),
        claude_registry(),
        Store {
            snapshot_path: Some(snapshot_path.clone()),
            ..Store::default()
        },
        Journal::default(),
        Pty {
            spawn_counter: Some(Arc::clone(&spawns)),
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(&dispatch_dir),
        FixtureLocator(executable_dir.path().to_path_buf()),
        reconciled,
    )
    .unwrap();

    // Replay is resolved before current admission checks; the executable
    // disappearing after restart cannot turn a durable final into a new
    // launch failure (or authorize a replacement spawn).
    std::fs::remove_file(executable_dir.path().join("claude")).unwrap();
    let replay = second
        .dispatch(
            &successful,
            &dispatch_intent("success"),
            session,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    assert!(replay.completed);
    assert_eq!(replay.terminal, success_terminal);
    assert_eq!(
        second
            .dispatch(
                &unsuccessful,
                &dispatch_intent("failure"),
                session,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(
        second
            .dispatch(
                &interrupted,
                &dispatch_intent("pending"),
                session,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        second
            .dispatch(
                &successful,
                &dispatch_intent("different"),
                session,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 3);
    assert!(second.mcp_caller(&old_credential).is_none());
    assert_eq!(
        second
            .output(&interrupted_record.runtime.terminal, b"late".to_vec())
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        second
            .exit(&interrupted_record.runtime.terminal, 0)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    let inbox_before = second.dispatch.inbox(&caller).unwrap();
    second
        .report(
            &interrupted_record.runtime,
            &interrupted_record.operation,
            InboxKind::Completed,
            "late completion".into(),
            None,
        )
        .unwrap();
    assert_eq!(second.dispatch.inbox(&caller).unwrap(), inbox_before);
    let inventory =
        second
            .coordinator
            .inventory(&usagi_core::domain::terminal_launch::TerminalLaunchScope {
                workspace_id: workspace,
                session_id: Some(session),
                worktree_id: resolved.worktree_id,
            });
    assert!(inventory.iter().all(|entry| !entry.live));

    second
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(resolved)),
        )
        .unwrap();
    assert_eq!(spawns.load(Ordering::SeqCst), 4);
    let saved: RuntimeStoreSnapshot =
        serde_json::from_slice(&std::fs::read(snapshot_path).unwrap()).unwrap();
    saved.validate_ownership().unwrap();
    assert_eq!(saved.records.len(), 4);
    assert!(saved.generation.current.is_some());
    assert_eq!(
        saved
            .generation
            .records
            .iter()
            .filter(|record| { record.role == super::super::generation::GenerationRole::Active })
            .count(),
        1
    );
    assert!(saved.records.iter().any(|record| {
        record.operation.operation_id.to_string() == successful
            && record.outcome == super::super::runtime::DurableOperationOutcome::Completed
    }));
}

#[test]
fn concurrent_production_admission_uses_one_generation_transition_and_spawn() {
    use std::sync::{Barrier, Mutex};

    let spawns = Arc::new(AtomicU32::new(0));
    let runtime = Arc::new(Mutex::new(AgentRuntime::with_dispatch(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            spawn_counter: Some(Arc::clone(&spawns)),
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(tempfile::tempdir().unwrap().keep()),
    )));
    let operation = OperationId::new().to_string();
    let launch = intent(None);
    let resolved = scope();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let runtime = Arc::clone(&runtime);
            let operation = operation.clone();
            let launch = launch.clone();
            let resolved = resolved.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                runtime
                    .lock()
                    .unwrap()
                    .launch(&operation, &launch, &FakeScope(Ok(resolved)))
                    .unwrap()
            })
        })
        .collect();
    let admissions: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();

    assert!(admissions[0].terminal.fences(&admissions[1].terminal));
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
    let snapshot = runtime.lock().unwrap().coordinator.snapshot();
    assert_eq!(snapshot.generation.terminals.len(), 1);
    assert_eq!(
        snapshot
            .generation
            .records
            .iter()
            .filter(|record| { record.role == super::super::generation::GenerationRole::Active })
            .count(),
        1
    );
}

#[test]
fn restart_reconciles_prepared_admission_without_spawning_a_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let dispatch_dir = dir.path().join("dispatch");
    let spawns = Arc::new(AtomicU32::new(0));
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let mut first = AgentRuntime::with_dispatch(
        DaemonGeneration::new(),
        claude_registry(),
        Store {
            saves: 0,
            fail_after: Some(0),
            ..Store::default()
        },
        Journal::default(),
        Pty {
            spawn_counter: Some(Arc::clone(&spawns)),
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(&dispatch_dir),
    );
    assert_eq!(
        first
            .launch(&operation, &launch_intent, &FakeScope(Ok(scope())),)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 0);
    drop(first);

    let mut second = AgentRuntime::hydrate_with_dispatch_and_locator(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            spawn_counter: Some(Arc::clone(&spawns)),
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(&dispatch_dir),
        PathExecutableLocator,
        RuntimeStoreSnapshot::default(),
    )
    .unwrap();
    assert_eq!(
        second
            .launch(&operation, &launch_intent, &FakeScope(Ok(scope())),)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    let mut conflict = launch_intent;
    conflict.workspace = WorkspaceId::new();
    let mut third = AgentRuntime::with_dispatch(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(&dispatch_dir),
    );
    assert_eq!(
        third
            .launch(&operation, &conflict, &FakeScope(Ok(scope())))
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 0);
    assert_eq!(
        second
            .dispatch
            .run(OperationId::parse(&operation).unwrap())
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Failed
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn queued_prompt_is_explicitly_consumed_by_launch_and_live_only_delivers_live() {
    let mut runtime = runtime();
    let launch_intent = intent(None);
    let workspace = launch_intent.workspace;
    let session = launch_intent.session.unwrap();
    assert_eq!(runtime.session_phase(session), AgentPhase::Absent);
    assert_eq!(
        runtime
            .prompt(workspace, Some(session), "  ", PromptMode::Live)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .queue_prompt_for_next_launch(workspace, Some(session), "  ")
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .prompt(workspace, Some(session), "now", PromptMode::Live)
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(
        runtime
            .prompt(workspace, Some(session), "start me", PromptMode::Live)
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert!(
        runtime
            .dispatch
            .queued_prompt(workspace, Some(session))
            .unwrap()
            .is_none()
    );
    let queued = runtime
        .prompt(workspace, Some(session), "queued work", PromptMode::Queue)
        .unwrap();
    assert_eq!(queued.delivered_to, "queue");
    assert!(
        runtime
            .dispatch
            .queued_prompt(workspace, Some(session))
            .unwrap()
            .is_some()
    );
    let parent_session = SessionId::new();
    let parent_agent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            Some(parent_session),
            AgentProfileId::new("codex").unwrap(),
            ModelSelector::new("parent").unwrap(),
        )
        .unwrap();
    let parent = CallerRef {
        session_id: Some(parent_session),
        agent_id: parent_agent.agent_id,
    };
    runtime
        .dispatch
        .queue_delegated_prompt(
            workspace,
            Some(session),
            "delegated work".into(),
            Utc::now(),
            parent.clone(),
            OperationId::new(),
        )
        .unwrap();

    let operation = OperationId::new();
    runtime
        .launch(
            &operation.to_string(),
            &launch_intent,
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    assert_eq!(runtime.session_phase(session), AgentPhase::Running);
    assert_eq!(
        runtime.dispatch.binding(operation).unwrap().unwrap().caller,
        parent
    );
    assert!(
        runtime
            .dispatch
            .queued_prompt(workspace, Some(session))
            .unwrap()
            .is_none()
    );
    let credential = runtime.mcp_callers.keys().next().unwrap().clone();
    assert_eq!(runtime.caller_session(&credential), Some(session));

    let live = runtime
        .prompt(workspace, Some(session), "follow up", PromptMode::Live)
        .unwrap();
    assert_eq!(live.delivered_to, "live");
    assert_eq!(pty(&runtime).writes, b"follow up\r");
    let fenced = runtime.prompt_run(operation, "decision\nanswer\n").unwrap();
    assert_eq!(fenced.delivered_to, "live");
    assert_eq!(pty(&runtime).writes, b"follow up\rdecision\nanswer\n\r");
    assert!(runtime.prompt_run(OperationId::new(), "late").is_err());
    assert_eq!(
        runtime.prompt_run(operation, "  ").unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    assert!(
        runtime
            .prompt(workspace, Some(session), "later", PromptMode::Queue)
            .is_err()
    );
    pty_mut(&mut runtime).write_failure = true;
    assert_eq!(
        runtime
            .prompt_run(operation, "failed decision")
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(
        runtime
            .prompt(workspace, Some(session), "fails", PromptMode::Live)
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert!(
        runtime
            .prompt(workspace, None, "now", PromptMode::Live)
            .is_err()
    );
    assert!(
        runtime
            .prompt(workspace, None, "  ", PromptMode::Live)
            .is_err()
    );
}

#[test]
fn session_workflow_launch_rechecks_readiness_and_embeds_exact_prompt() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let intent = intent(None);
    let operation = OperationId::new().to_string();
    let prompt = "workflow task";
    let scope = FakeScope(Ok(scope()));
    for invalid in ["", "\0", " "] {
        assert!(
            runtime
                .prepare_workflow_readiness(&operation, &intent, invalid)
                .is_err()
        );
    }
    assert!(
        runtime
            .prepare_workflow_readiness("invalid", &intent, prompt)
            .is_err()
    );
    let preflight = runtime
        .prepare_workflow_readiness(&operation, &intent, prompt)
        .unwrap();
    assert!(
        runtime
            .launch_workflow_after_readiness(&operation, &intent, prompt, &scope, None)
            .is_err()
    );
    let first = runtime
        .launch_workflow_after_readiness(&operation, &intent, prompt, &scope, preflight.as_ref())
        .unwrap();
    let replay = runtime
        .launch_workflow_after_readiness(&operation, &intent, prompt, &scope, None)
        .unwrap();
    assert_eq!(first, replay);
    assert!(
        runtime
            .prepare_workflow_readiness(&operation, &intent, "changed")
            .is_err()
    );
    assert_eq!(
        runtime.coordinator.snapshot().records[0]
            .launch
            .request
            .initial_prompt
            .as_deref(),
        Some(prompt)
    );
    let other = OperationId::new().to_string();
    let preflight = runtime
        .prepare_workflow_readiness(&other, &intent, prompt)
        .unwrap();
    assert!(
        runtime
            .launch_workflow_after_readiness(&other, &intent, prompt, &scope, preflight.as_ref())
            .is_err()
    );
}

#[test]
fn workflow_restart_replays_interrupted_admission_without_spawning_a_replacement() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut first = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let intent = intent(None);
    let operation = OperationId::new().to_string();
    let prompt = "workflow immutable goal";
    let ticket = first
        .prepare_workflow_readiness(&operation, &intent, prompt)
        .unwrap();
    first
        .launch_workflow_after_readiness(
            &operation,
            &intent,
            prompt,
            &FakeScope(Ok(scope())),
            ticket.as_ref(),
        )
        .unwrap();
    let dispatch = first.dispatch.clone();
    let (snapshot, count) = first
        .coordinator
        .snapshot()
        .reconcile_after_daemon_restart();
    assert_eq!(count, 1);
    let spawns = Arc::new(AtomicU32::new(0));
    let mut restored = AgentRuntime::hydrate_with_dispatch_and_locator(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            spawn_counter: Some(Arc::clone(&spawns)),
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        dispatch,
        FixtureLocator(fixture.path().to_path_buf()),
        snapshot,
    )
    .unwrap();
    assert!(
        restored
            .prepare_workflow_readiness(&operation, &intent, prompt)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        restored
            .launch_workflow_after_readiness(
                &operation,
                &intent,
                prompt,
                &FakeScope(Ok(scope())),
                None
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        restored
            .prepare_workflow_readiness(&operation, &intent, "different goal")
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 0);
}

#[test]
fn goal_launch_is_root_scoped_idempotent_and_carries_the_work_contract() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let workspace = WorkspaceId::new();
    let operation = OperationId::new().to_string();
    let mut intent = AgentGoalIntent {
        workspace,
        profile: Some(AgentProfileId::new("claude").unwrap()),
        goal: "update the docs and open a PR".into(),
    };

    let mut invalid = intent.clone();
    invalid.goal = "  ".into();
    assert_eq!(
        runtime
            .prepare_goal_launch_readiness(&OperationId::new().to_string(), &invalid)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    invalid.goal = "x".repeat(MAX_AGENT_GOAL_BYTES + 1);
    assert_eq!(
        runtime
            .launch_goal(
                &OperationId::new().to_string(),
                &invalid,
                &FakeScope(Ok(scope()))
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .prepare_goal_launch_readiness("invalid", &intent)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );

    let readiness = runtime
        .prepare_goal_launch_readiness(&operation, &intent)
        .unwrap()
        .unwrap();

    let first = runtime
        .launch_goal_after_readiness(
            &operation,
            &intent,
            &FakeScope(Ok(scope())),
            Some(&readiness),
        )
        .unwrap();
    assert!(
        runtime
            .prepare_goal_launch_readiness(&operation, &intent)
            .unwrap()
            .is_none()
    );
    let replay = runtime
        .launch_goal(&operation, &intent, &FakeScope(Ok(scope())))
        .unwrap();
    assert!(first.terminal.fences(&replay.terminal));
    assert_eq!(first.terminal.session_id, None);

    let record = &runtime.coordinator.snapshot().records[0];
    let prompt = record.launch.request.initial_prompt.as_deref().unwrap();
    assert!(prompt.contains("update the docs and open a PR"));
    assert!(prompt.contains("open, non-draft pull request"));
    assert!(prompt.contains("same `claude` Agent runtime"));
    assert!(prompt.contains("capability the task actually needs"));
    assert!(prompt.contains("do not default to the strongest available model"));
    assert!(prompt.contains("user-decision tool"));
    assert!(prompt.contains("Do not merge"));

    let mut changed = intent.clone();
    changed.goal = "a different goal".into();
    assert_eq!(
        runtime
            .launch_goal(&operation, &changed, &FakeScope(Ok(scope())))
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );

    let mut queued = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    queued
        .prompt(workspace, None, "already queued", PromptMode::Queue)
        .unwrap();
    intent.goal = "do not replace the queue".into();
    assert_eq!(
        queued
            .launch_goal(
                &OperationId::new().to_string(),
                &intent,
                &FakeScope(Ok(scope()))
            )
            .unwrap_err()
            .message,
        "workspace root already has a queued prompt"
    );
}

#[test]
fn goal_readiness_defaults_profile_and_rejects_semantic_conflict() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let operation = OperationId::new().to_string();
    let mut intent = AgentGoalIntent {
        workspace: WorkspaceId::new(),
        profile: None,
        goal: "use the default profile".into(),
    };
    assert_eq!(
        runtime.goal_worker_profile(&intent).unwrap().as_str(),
        "claude"
    );
    let mut explicit = intent.clone();
    explicit.profile = Some(AgentProfileId::new("claude").unwrap());
    assert_eq!(
        runtime.goal_worker_profile(&explicit).unwrap().as_str(),
        "claude"
    );
    let mut invalid = intent.clone();
    invalid.goal = " ".into();
    assert_eq!(
        runtime.goal_worker_profile(&invalid).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    let readiness = runtime
        .prepare_goal_launch_readiness(&operation, &intent)
        .unwrap()
        .unwrap();
    assert_eq!(readiness.product(), "claude");
    runtime
        .launch_goal_after_readiness(
            &operation,
            &intent,
            &FakeScope(Ok(scope())),
            Some(&readiness),
        )
        .unwrap();

    intent.goal = "a different goal".into();
    assert_eq!(
        runtime
            .prepare_goal_launch_readiness(&operation, &intent)
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
}

#[test]
fn root_prompt_selects_the_agent_in_its_exact_workspace() {
    let mut runtime = runtime();
    let first = root_intent(None);
    let second = root_intent(None);
    runtime
        .launch(
            &OperationId::new().to_string(),
            &first,
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    runtime
        .launch(
            &OperationId::new().to_string(),
            &second,
            &FakeScope(Ok(scope())),
        )
        .unwrap();

    runtime
        .prompt(second.workspace, None, "workspace two", PromptMode::Live)
        .unwrap();
    assert_eq!(
        pty(&runtime).selected.as_ref().unwrap().workspace_id,
        second.workspace
    );
    assert_eq!(pty(&runtime).writes, b"workspace two\r");
}

#[test]
#[allow(clippy::too_many_lines)] // One ordered scenario keeps launch, both snapshot revisions, input, detach, reattach and exit visibly sequential.
fn end_to_end_launch_output_attach_input_detach_reattach_and_exit() {
    let mut runtime = runtime();
    let fake_scope = FakeScope(Ok(scope()));
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let admission = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap();
    assert_eq!(admission.operation_id, operation);
    assert_eq!(admission.revision, 1);
    assert_eq!(admission.terminal.session_id, launch_intent.session);
    let terminal = admission.terminal.clone();

    // Daemon-owned PTY output is journaled before it is replayable.
    runtime.output(&terminal, b"ready\n".to_vec()).unwrap();

    let connection = ConnectionId::new();
    let client = ClientId::new();
    let attached = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Attach,
        TerminalRequest::Attach {
            terminal: terminal.clone(),
            geometry: None,
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(attached["snapshot"]["replay"], json!(b"ready\n".to_vec()));
    let subscription = attached["subscription"].as_u64().unwrap();

    // The same Agent terminal serves a revision 2 connection its semantic
    // screen instead of the raw tail (#534): Agent and generic terminals
    // share one snapshot contract.
    let checkpointed = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Attach,
        TerminalRequest::Attach {
            terminal: terminal.clone(),
            geometry: None,
        },
        SnapshotWire::ScreenCheckpoint,
    ));
    let screen = &checkpointed["snapshot"]["screen"];
    assert!(checkpointed["snapshot"]["replay"].is_null());
    assert_eq!(
        checkpointed["snapshot"]["base_offset"],
        checkpointed["snapshot"]["output_offset"]
    );
    assert_eq!(
        screen["schema_version"].as_u64(),
        Some(u64::from(usagi_core::usecase::vt_screen::SCHEMA_VERSION))
    );
    // The screen is the authority for what the PTY printed.
    let restored = usagi_core::usecase::vt_screen::VtScreen::from_checkpoint(
        &serde_json::from_value(screen.clone()).unwrap(),
    )
    .unwrap();
    assert_eq!(restored.cells()[0].trim_end(), "ready");

    handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resize,
        TerminalRequest::Resize {
            terminal: terminal.clone(),
            geometry: TerminalGeometry { cols: 43, rows: 17 },
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(
        pty(&runtime).resized,
        vec![(terminal.clone(), Geometry { cols: 43, rows: 17 })]
    );

    let input_operation = OperationId::new();
    let ack = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Input,
        TerminalRequest::Input {
            terminal: terminal.clone(),
            subscription,
            input_seq: 0,
            input_operation: Some(input_operation),
            bytes: b"go\n".to_vec(),
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(ack["ack"], "Written");

    // The Agent owner shares the durable input operation ledger, so a client
    // whose acknowledgement was lost resolves the same final here too (#519),
    // and an identity it never issued is a typed unknown.
    let resolve = |runtime: &mut AgentRuntime, operation| {
        handled(runtime.handle_terminal(
            connection,
            client,
            RequestId::new(),
            TerminalAction::InputOutcome,
            TerminalRequest::InputOutcome {
                terminal: terminal.clone(),
                input_operation: operation,
            },
            SnapshotWire::RawTail,
        ))
    };
    let resolved = resolve(&mut runtime, input_operation);
    assert_eq!(resolved["outcome"], "final");
    assert_eq!(resolved["ack"], "Written");
    assert_eq!(
        resolve(&mut runtime, OperationId::new())["outcome"],
        "unknown"
    );
    // Reusing that identity for different bytes conflicts without writing.
    let conflict = handled_result(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Input,
        TerminalRequest::Input {
            terminal: terminal.clone(),
            subscription,
            input_seq: 1,
            input_operation: Some(input_operation),
            bytes: b"rm -rf\n".to_vec(),
        },
        SnapshotWire::RawTail,
    ))
    .unwrap_err();
    assert_eq!(conflict.code, ErrorCode::IdempotencyConflict);

    handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Detach,
        TerminalRequest::Detach {
            terminal: terminal.clone(),
            subscription,
        },
        SnapshotWire::RawTail,
    ));
    // A coalesced live-set sweep drops only subscriptions; the process/PTY
    // stay alive.
    runtime.retain_live_connections(&BTreeSet::new());

    let reattached = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Attach,
        TerminalRequest::Attach {
            terminal: terminal.clone(),
            geometry: None,
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(reattached["snapshot"]["output_offset"], 6);

    runtime.exit(&terminal, 0).unwrap();
    let final_replay = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap();
    assert_eq!(final_replay.terminal, terminal);
    assert!(final_replay.completed);
    let resync = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resync,
        TerminalRequest::Resync {
            terminal: terminal.clone(),
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(resync["exited"], 0);
    assert_eq!(pty(&runtime).selected.as_ref(), Some(&terminal));
    assert_eq!(pty(&runtime).writes, b"go\n");
}

#[test]
fn agent_resume_reports_exit_for_parity_with_the_generic_terminal() {
    // Regression: an Agent's `Resume` must carry the hosting terminal's
    // `exited` flag (like the generic terminal Resume), so a TUI client's
    // per-frame poll observes the exit and drops the pane tab instead of
    // leaving it stranded until an incidental resync.
    let mut runtime = runtime();
    let fake_scope = FakeScope(Ok(scope()));
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let terminal = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap()
        .terminal;
    runtime.output(&terminal, b"working\n".to_vec()).unwrap();

    let connection = ConnectionId::new();
    let client = ClientId::new();
    let live = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resume,
        TerminalRequest::Resume {
            terminal: terminal.clone(),
            after_offset: 0,
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(live["exited"], false);

    runtime.exit(&terminal, 0).unwrap();
    assert!(runtime.exit(&terminal, 0).is_err());
    assert_eq!(
        pty(&runtime).released.as_slice(),
        std::slice::from_ref(&terminal)
    );
    let late_resize = runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resize,
        TerminalRequest::Resize {
            terminal: terminal.clone(),
            geometry: TerminalGeometry { cols: 80, rows: 24 },
        },
        SnapshotWire::RawTail,
    );
    assert!(matches!(
        late_resize,
        TerminalOutcome::Handled(Err(ProtocolError {
            code: ErrorCode::StaleTarget,
            ..
        }))
    ));
    let exited = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resume,
        TerminalRequest::Resume {
            terminal: terminal.clone(),
            after_offset: 8,
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(exited["exited"], true);
}

#[test]
fn observed_exit_releases_transport_when_the_final_store_write_fails() {
    let mut runtime = runtime();
    let terminal = runtime
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap()
        .terminal;
    let saves = store_mut(&mut runtime).saves;
    store_mut(&mut runtime).fail_after = Some(saves);

    let error = runtime.exit(&terminal, 0).unwrap_err();
    assert_eq!(error.code, ErrorCode::OwnershipUnknown);
    assert_eq!(pty(&runtime).released, [terminal]);
}

#[test]
fn workspace_root_agent_launches_and_attaches_without_a_session() {
    let mut runtime = runtime();
    let fake_scope = FakeScope(Ok(scope()));
    let operation = OperationId::new().to_string();
    let launch_intent = root_intent(None);
    let admission = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap();
    // The admitted terminal is a workspace-root terminal (no session), and
    // its live IO is attachable exactly like a session agent's.
    assert_eq!(admission.terminal.session_id, None);
    let terminal = admission.terminal.clone();
    runtime.output(&terminal, b"root-agent\n".to_vec()).unwrap();
    let attached = handled(runtime.handle_terminal(
        ConnectionId::new(),
        ClientId::new(),
        RequestId::new(),
        TerminalAction::Attach,
        TerminalRequest::Attach {
            terminal: terminal.clone(),
            geometry: None,
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(
        attached["snapshot"]["replay"],
        json!(b"root-agent\n".to_vec())
    );
}

#[test]
fn unsuccessful_exit_replays_one_safe_final_failure() {
    let mut runtime = runtime();
    let fake_scope = FakeScope(Ok(scope()));
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let terminal = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap()
        .terminal;
    let runtime_ref = runtime.coordinator.runtime_for_terminal(&terminal).unwrap();
    let fence = runtime
        .coordinator
        .record_for(&runtime_ref)
        .unwrap()
        .operation
        .clone();
    runtime
        .report(
            &runtime_ref,
            &fence,
            InboxKind::Completed,
            "no dispatch binding".into(),
            None,
        )
        .unwrap();

    runtime.exit(&terminal, 23).unwrap();
    let failure = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap_err();
    assert_eq!(failure.code, ErrorCode::Unavailable);
    assert_eq!(
        failure.message,
        "agent process ended unsuccessfully; inspect the attached terminal output"
    );
    assert!(
        runtime
            .launch(&operation, &launch_intent, &fake_scope)
            .is_err()
    );

    let operation = OperationId::new().to_string();
    let terminal = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap()
        .terminal;
    runtime.operations.get_mut(&operation).unwrap().outcome = Err(stale_terminal());
    runtime.exit(&terminal, 0).unwrap();
    assert_eq!(
        runtime
            .launch(&operation, &launch_intent, &fake_scope)
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );
}

#[test]
fn missing_dispatch_binding_is_a_safe_noop_for_report_and_observer_exit() {
    let mut runtime = runtime();
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let terminal = runtime
        .launch(&operation, &launch_intent, &FakeScope(Ok(scope())))
        .unwrap()
        .terminal;
    let runtime_ref = runtime.coordinator.runtime_for_terminal(&terminal).unwrap();
    let fence = runtime
        .coordinator
        .record_for(&runtime_ref)
        .unwrap()
        .operation
        .clone();
    runtime.dispatch = DispatchStore::new(tempfile::tempdir().unwrap().keep());
    runtime.mcp_callers.insert(
        "missing-binding".into(),
        McpCaller {
            runtime: runtime_ref.clone(),
            operation: fence.operation_id,
            child: None,
        },
    );
    assert_eq!(
        runtime
            .report_from_mcp(
                "missing-binding",
                None,
                InboxKind::Completed,
                "missing binding".into(),
                None,
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    runtime
        .report(
            &runtime_ref,
            &fence,
            InboxKind::Completed,
            "missing binding".into(),
            None,
        )
        .unwrap();
    runtime.exit(&terminal, 0).unwrap();
}

#[test]
fn resend_replays_and_conflicting_intent_is_rejected_without_second_spawn() {
    let mut runtime = runtime();
    let fake_scope = FakeScope(Ok(scope()));
    let operation = OperationId::new().to_string();
    let launch_intent = intent(Some("claude"));
    let first = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap();
    let second = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(
        runtime.operation_outcome(&operation).unwrap().unwrap(),
        first
    );

    let mut conflict = launch_intent.clone();
    conflict.profile = Some(AgentProfileId::new("codex").unwrap());
    assert_eq!(
        runtime
            .launch(&operation, &conflict, &fake_scope)
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    // Only one runtime was ever reserved.
    assert_eq!(runtime.coordinator.occupied_slots(), 1);
}

#[test]
fn unavailable_scope_and_unknown_profile_are_safe_and_never_spawn() {
    let mut unavailable = runtime();
    assert_eq!(
        unavailable
            .launch(
                &OperationId::new().to_string(),
                &intent(None),
                &FakeScope(Err(ScopeResolveError::Unavailable)),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(unavailable.coordinator.occupied_slots(), 0);

    let mut storage = runtime();
    assert_eq!(
        storage
            .launch(
                &OperationId::new().to_string(),
                &intent(None),
                &FakeScope(Err(ScopeResolveError::Storage)),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );

    let mut unknown = runtime();
    assert_eq!(
        unknown
            .launch(
                &OperationId::new().to_string(),
                &intent(Some("codex")),
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );

    let mut bad_operation = runtime();
    assert_eq!(
        bad_operation
            .launch("not-a-uuid", &intent(None), &FakeScope(Ok(scope())))
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn legacy_run_without_admission_metadata_is_not_spawned() {
    let mut runtime = runtime();
    let operation = OperationId::new();
    runtime
        .dispatch
        .upsert_run(DispatchRun {
            run_id: operation,
            agent_id: usagi_core::domain::id::AgentId::new(),
            prompt: String::new(),
            started_at: Utc::now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();

    assert_eq!(
        runtime
            .launch(
                &operation.to_string(),
                &intent(None),
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(runtime.coordinator.occupied_slots(), 0);
}

#[test]
fn provider_metadata_matches_only_its_compatible_profiles() {
    // `sakana-ai` runs the Claude CLI, so the Claude adapter is what
    // captures and resumes its conversations: its retained metadata is
    // `ProviderKind::Claude`. Codex metadata — including a record written
    // while this profile still ran Sakana's Codex wrapper — must never
    // authorize it, or the daemon would hand Codex resume data to a Claude
    // adapter.
    for (provider, profile, expected) in [
        (ProviderKind::Claude, "claude", true),
        (ProviderKind::Codex, "codex", true),
        (ProviderKind::Claude, "sakana-ai", true),
        (ProviderKind::Agy, "agy", true),
        (ProviderKind::Codex, "sakana-ai", false),
        (ProviderKind::Claude, "codex", false),
        (ProviderKind::Codex, "claude", false),
        (ProviderKind::Agy, "codex", false),
        (ProviderKind::Codex, "agy", false),
    ] {
        assert_eq!(
            provider_matches_profile(provider, &AgentProfileId::new(profile).unwrap()),
            expected,
            "{provider:?} {profile}"
        );
    }
}

#[test]
fn mcp_child_accepts_inherited_or_isolated_process_groups_only() {
    for (provider_group, child_pid, child_group, expected) in [
        (4321, 9001, 4321, true),
        (4321, 9001, 9001, true),
        (4321, 9001, 9999, false),
    ] {
        assert_eq!(
            mcp_child_process_group_matches(provider_group, child_pid, child_group),
            expected
        );
    }
}

#[test]
fn delegated_dispatch_requires_the_authenticated_callers_runtime() {
    let runtime = runtime();
    let workspace = WorkspaceId::new();
    let caller_agent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            Some(SessionId::new()),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("manager").unwrap(),
        )
        .unwrap();
    let same_runtime_agent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            Some(SessionId::new()),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("worker").unwrap(),
        )
        .unwrap();
    let other_runtime_agent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            Some(SessionId::new()),
            AgentProfileId::new("codex").unwrap(),
            ModelSelector::new("worker").unwrap(),
        )
        .unwrap();
    let caller = CallerRef {
        session_id: caller_agent.session_id,
        agent_id: caller_agent.agent_id,
    };

    for selected in [
        DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: ModelSelector::new("different-model-is-allowed").unwrap(),
        },
        DispatchAgentIntent::Existing {
            agent_id: same_runtime_agent.agent_id,
        },
    ] {
        runtime
            .require_same_dispatch_runtime(workspace, &caller, &selected)
            .unwrap();
    }
    for selected in [
        DispatchAgentIntent::New {
            runtime: AgentProfileId::new("codex").unwrap(),
            model: ModelSelector::new("worker").unwrap(),
        },
        DispatchAgentIntent::Existing {
            agent_id: other_runtime_agent.agent_id,
        },
    ] {
        assert_eq!(
            runtime
                .require_same_dispatch_runtime(workspace, &caller, &selected)
                .unwrap_err()
                .code,
            ErrorCode::PermissionDenied
        );
    }
    assert_eq!(
        runtime
            .require_same_dispatch_runtime(
                workspace,
                &CallerRef {
                    session_id: None,
                    agent_id: usagi_core::domain::id::AgentId::new(),
                },
                &DispatchAgentIntent::New {
                    runtime: AgentProfileId::new("claude").unwrap(),
                    model: ModelSelector::new("worker").unwrap(),
                },
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
fn hook_identity_accepts_exec_form_direct_children_and_legacy_inherited_groups() {
    let mut runtime = runtime();
    runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace: WorkspaceId::new(),
                session: Some(SessionId::new()),
                profile: None,
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().unwrap().as_str();

    // Shell-form hooks inherit the provider's process group.
    assert_eq!(runtime.hook_credential(9000, 8999, 4321), Some(credential));
    // Exec-form hooks are direct children and may be their own group leader.
    assert_eq!(runtime.hook_credential(9001, 4321, 9001), Some(credential));
    // Merely being self-led is insufficient without the direct-parent fence.
    assert_eq!(runtime.hook_credential(9002, 8999, 9002), None);
    assert_eq!(runtime.hook_credential(9003, 4321, 9999), None);
}

#[test]
#[allow(clippy::too_many_lines)] // Preserve the full launch/resume/exit identity scenario.
fn same_model_launches_and_exact_resume_preserve_each_peer_identity() {
    let mut runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = FakeScope(Ok(scope()));
    let launch = AgentLaunchIntent {
        workspace,
        session: Some(session),
        profile: Some(AgentProfileId::new("claude").unwrap()),
    };
    let first_operation = OperationId::new();
    let first = runtime
        .launch(&first_operation.to_string(), &launch, &resolved)
        .unwrap();
    let first_agent = runtime
        .dispatch
        .binding(first_operation)
        .unwrap()
        .unwrap()
        .worker
        .agent_id;
    let second_operation = OperationId::new();
    let second = runtime
        .launch(&second_operation.to_string(), &launch, &resolved)
        .unwrap();
    let second_agent = runtime
        .dispatch
        .binding(second_operation)
        .unwrap()
        .unwrap()
        .worker
        .agent_id;
    assert_ne!(first_agent, second_agent);
    assert_eq!(
        runtime
            .dispatch
            .agent(first_agent)
            .unwrap()
            .unwrap()
            .current_run,
        Some(first_operation)
    );
    runtime.exit(&second.terminal, 0).unwrap();
    let target = runtime
        .inventory(workspace)
        .resumable
        .into_iter()
        .find_map(|item| {
            item.target
                .filter(|target| target.runtime_id == second.runtime.agent_runtime_id)
        })
        .unwrap();
    let resumed_operation = OperationId::new();
    let resumed = runtime
        .resume_exact(&resumed_operation.to_string(), &target, &resolved)
        .unwrap();
    assert_eq!(
        runtime
            .dispatch
            .binding(resumed_operation)
            .unwrap()
            .unwrap()
            .worker
            .agent_id,
        second_agent
    );
    assert_eq!(
        runtime
            .dispatch
            .agent(first_agent)
            .unwrap()
            .unwrap()
            .current_run,
        Some(first_operation)
    );
    runtime
        .notify_peer(workspace, session, first_agent)
        .unwrap();
    assert_eq!(pty(&runtime).selected.as_ref(), Some(&first.terminal));
    runtime
        .notify_peer(workspace, session, second_agent)
        .unwrap();
    assert_eq!(pty(&runtime).selected.as_ref(), Some(&resumed.terminal));
    assert_eq!(
        runtime.workflow_operation_lineage(second_operation),
        vec![second_operation, resumed_operation]
    );
    assert_eq!(
        runtime.workflow_live_operation(second_operation),
        Some(resumed_operation)
    );
    assert_eq!(
        runtime.workflow_operation_lineage(first_operation),
        vec![first_operation]
    );
    assert!(
        runtime
            .workflow_operation_lineage(OperationId::new())
            .is_empty()
    );
    runtime.exit(&resumed.terminal, 0).unwrap();
    assert_eq!(
        runtime.workflow_operation_lineage(second_operation),
        vec![second_operation, resumed_operation]
    );
    assert_eq!(runtime.workflow_live_operation(second_operation), None);
    assert_eq!(runtime.workflow_live_operation(OperationId::new()), None);
    let snapshot = runtime.coordinator.snapshot();
    for replacement in [AgentRuntimeId::new(), resumed.runtime.agent_runtime_id] {
        let mut broken = snapshot.clone();
        broken
            .records
            .iter_mut()
            .find(|record| record.operation.operation_id == resumed_operation)
            .unwrap()
            .superseded_by = Some(replacement);
        // Missing/cyclic replacement data cannot invent another admitted
        // operation or loop forever, even before hydration rejects it.
        assert_eq!(
            AgentRuntime::workflow_lineage(&broken, second_operation),
            vec![second_operation, resumed_operation]
        );
    }
}

#[test]
fn peer_plan_is_stable_before_admission_and_across_runtime_restart() {
    let runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let caller = CallerRef {
        session_id: Some(session),
        agent_id: AgentId::new(),
    };
    let operation = OperationId::new();
    let selected = DispatchAgentIntent::New {
        runtime: AgentProfileId::new("claude").unwrap(),
        model: ModelSelector::new("default").unwrap(),
    };
    let worker = runtime
        .plan_peer_worker(&operation.to_string(), workspace, &caller, &selected)
        .unwrap();
    assert_eq!(
        runtime
            .plan_peer_worker(&operation.to_string(), workspace, &caller, &selected)
            .unwrap()
            .agent_id,
        worker.agent_id
    );
    let restarted = self::runtime();
    assert_eq!(
        restarted
            .plan_peer_worker(&operation.to_string(), workspace, &caller, &selected)
            .unwrap()
            .agent_id,
        worker.agent_id
    );
    assert_ne!(
        peer_worker_id(OperationId::new(), workspace, session),
        worker.agent_id
    );
    assert_ne!(
        peer_worker_id(operation, WorkspaceId::new(), session),
        worker.agent_id
    );
    assert_ne!(
        peer_worker_id(operation, workspace, SessionId::new()),
        worker.agent_id
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One peer lifetime covers planning, admission, notification, report and stopped reuse.
fn peer_handoff_preserves_identity_and_targets_only_the_named_runtime() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let caller_agent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            Some(session),
            AgentProfileId::new("codex").unwrap(),
            ModelSelector::new("test").unwrap(),
        )
        .unwrap();
    let caller = CallerRef {
        session_id: Some(session),
        agent_id: caller_agent.agent_id,
    };
    let operation = OperationId::new().to_string();
    let selected = DispatchAgentIntent::New {
        runtime: AgentProfileId::new("claude").unwrap(),
        model: ModelSelector::new("test").unwrap(),
    };
    let worker = runtime
        .plan_peer_worker(&operation, workspace, &caller, &selected)
        .unwrap();
    assert_ne!(worker.agent_id, caller.agent_id);
    assert_eq!(worker.session_id, Some(session));
    assert!(
        runtime
            .plan_peer_worker("bad", workspace, &caller, &selected)
            .is_err()
    );
    assert!(
        runtime
            .plan_peer_worker(
                &operation,
                workspace,
                &CallerRef {
                    session_id: None,
                    ..caller.clone()
                },
                &selected
            )
            .is_err()
    );
    assert!(
        runtime
            .plan_peer_worker(
                &operation,
                workspace,
                &caller,
                &DispatchAgentIntent::Existing {
                    agent_id: caller.agent_id
                }
            )
            .is_err()
    );
    let dispatch = DispatchIntent {
        workspace,
        session_name: "current".into(),
        caller: caller.clone(),
        agent: selected.clone(),
        prompt: "review without editing".into(),
    };
    let scope = FakeScope(Ok(configured_scope(worktree.path())));
    let admission = runtime
        .dispatch_with_planned_worker(&operation, &dispatch, session, &scope, Some(&worker))
        .unwrap();
    let mut self_handoff = dispatch.clone();
    self_handoff.caller.agent_id = worker.agent_id;
    assert_eq!(
        runtime
            .dispatch_with_planned_worker(
                &OperationId::new().to_string(),
                &self_handoff,
                session,
                &scope,
                Some(&worker)
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let mut other_caller = dispatch.clone();
    other_caller.caller.agent_id = AgentId::new();
    assert_eq!(
        runtime
            .dispatch_with_planned_worker(&operation, &other_caller, session, &scope, Some(&worker))
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    // A second selector can have been planned while the first request was
    // still doing readiness IO. Stable identity does not authorize changing
    // the model on a replay that reaches admission after the first request.
    let mut conflicting_worker = worker.clone();
    conflicting_worker.model = ModelSelector::new("changed").unwrap();
    let mut conflicting_dispatch = dispatch.clone();
    conflicting_dispatch.agent = DispatchAgentIntent::New {
        runtime: conflicting_worker.runtime.clone(),
        model: conflicting_worker.model.clone(),
    };
    assert_eq!(
        runtime
            .dispatch_with_planned_worker(
                &operation,
                &conflicting_dispatch,
                session,
                &scope,
                Some(&conflicting_worker)
            )
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    assert_eq!(
        runtime
            .plan_peer_worker(&operation, workspace, &caller, &selected)
            .unwrap()
            .agent_id,
        worker.agent_id
    );
    assert!(
        runtime
            .plan_peer_worker(
                &operation,
                workspace,
                &caller,
                &DispatchAgentIntent::Existing {
                    agent_id: caller.agent_id
                }
            )
            .is_err()
    );
    assert!(
        runtime
            .plan_peer_worker(
                &operation,
                workspace,
                &CallerRef {
                    agent_id: AgentId::new(),
                    ..caller.clone()
                },
                &selected
            )
            .is_err()
    );
    assert!(
        runtime
            .plan_peer_worker(
                &OperationId::new().to_string(),
                workspace,
                &caller,
                &DispatchAgentIntent::Existing {
                    agent_id: worker.agent_id
                }
            )
            .is_err()
    );
    assert!(
        runtime
            .dispatch_with_planned_worker(
                &OperationId::new().to_string(),
                &dispatch,
                session,
                &scope,
                Some(&worker)
            )
            .is_err()
    );
    assert_eq!(
        runtime
            .dispatch_with_planned_worker(&operation, &dispatch, session, &scope, Some(&worker))
            .unwrap()
            .terminal,
        admission.terminal
    );
    runtime
        .notify_peer(workspace, session, worker.agent_id)
        .unwrap();
    assert_eq!(pty(&runtime).selected.as_ref(), Some(&admission.terminal));
    assert!(
        runtime
            .notify_peer(workspace, SessionId::new(), worker.agent_id)
            .is_err()
    );
    assert!(
        runtime
            .notify_peer(workspace, session, caller.agent_id)
            .is_err()
    );
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();
    runtime
        .report_from_mcp(
            &credential,
            None,
            InboxKind::Completed,
            "review finished".into(),
            None,
        )
        .unwrap();
    assert_eq!(runtime.dispatch.inbox(&caller).unwrap().len(), 1);
    // Reporting does not stop the PTY: completed live reviewers still
    // receive messages and cannot be re-launched under the same identity.
    assert!(
        runtime
            .plan_peer_worker(
                &OperationId::new().to_string(),
                workspace,
                &caller,
                &DispatchAgentIntent::Existing {
                    agent_id: worker.agent_id
                }
            )
            .is_err()
    );
    runtime.exit(&admission.terminal, 0).unwrap();
    assert!(
        runtime
            .notify_peer(workspace, session, worker.agent_id)
            .is_err()
    );
    assert_eq!(
        runtime
            .plan_peer_worker(
                &OperationId::new().to_string(),
                workspace,
                &caller,
                &DispatchAgentIntent::Existing {
                    agent_id: worker.agent_id
                }
            )
            .unwrap()
            .agent_id,
        worker.agent_id
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One dispatch lifetime exercises claim, reconnect, PID reuse, replay, and exit invalidation.
fn dispatch_launches_once_persists_binding_and_synthesizes_no_report_on_exit() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let caller = CallerRef {
        session_id: Some(SessionId::new()),
        agent_id: usagi_core::domain::id::AgentId::new(),
    };
    let operation = OperationId::new().to_string();
    let dispatch = DispatchIntent {
        workspace,
        session_name: "worker".into(),
        caller: caller.clone(),
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: usagi_core::domain::agent::ModelSelector::new("test").unwrap(),
        },
        prompt: "finish the task".into(),
    };
    let admission = runtime
        .dispatch(
            &operation,
            &dispatch,
            session,
            &FakeScope(Ok(configured_scope(worktree.path()))),
        )
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();
    let durable_snapshot = serde_json::to_string(&runtime.coordinator.snapshot()).unwrap();
    assert!(durable_snapshot.contains("daemon_minted_ephemeral"));
    assert!(!durable_snapshot.contains(&credential));
    assert_eq!(
        runtime.mcp_caller(&credential),
        Some(OperationId::parse(&operation).unwrap())
    );
    let first_connection = ConnectionId::new();
    let second_connection = ConnectionId::new();
    assert!(!runtime.authenticate_mcp_child_connection(
        &credential,
        9001,
        "start-a",
        first_connection
    ));
    assert!(
        runtime
            .claim_mcp_child(9001, "start-a", 9998, 4321, first_connection, &|_, _| true)
            .is_err()
    );
    let ambiguous = runtime.mcp_callers[&credential].clone();
    runtime
        .mcp_callers
        .insert("ambiguous-runtime".into(), ambiguous);
    assert_eq!(
        runtime
            .claim_mcp_child(9001, "start-a", 4321, 4321, first_connection, &|_, _| true)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    runtime.mcp_callers.remove("ambiguous-runtime");
    assert_eq!(
        runtime
            .claim_mcp_child(9001, "start-a", 4321, 4321, first_connection, &|_, _| true)
            .unwrap(),
        credential
    );
    assert!(runtime.authenticate_mcp_child_connection(
        &credential,
        9001,
        "start-a",
        second_connection
    ));
    assert!(!runtime.authenticate_mcp_child_connection(
        &credential,
        9002,
        "start-a",
        second_connection
    ));
    assert!(!runtime.authenticate_mcp_child_connection(
        &credential,
        9001,
        "reused-pid",
        second_connection
    ));
    assert!(
        runtime
            .claim_mcp_child(9002, "start-b", 4321, 4321, second_connection, &|_, _| true)
            .is_err()
    );
    assert!(
        runtime
            .claim_mcp_child(9001, "start-a", 4321, 9999, second_connection, &|_, _| true)
            .is_err()
    );
    runtime.release_mcp_connection(first_connection);
    assert_eq!(
        runtime.mcp_callers[&credential]
            .child
            .as_ref()
            .and_then(|child| child.connection),
        Some(second_connection)
    );
    runtime.retain_live_mcp_connections(&BTreeSet::from([second_connection]));
    assert_eq!(
        runtime.mcp_callers[&credential]
            .child
            .as_ref()
            .and_then(|child| child.connection),
        Some(second_connection)
    );
    runtime.retain_live_mcp_connections(&BTreeSet::new());
    assert_eq!(
        runtime.mcp_callers[&credential]
            .child
            .as_ref()
            .and_then(|child| child.connection),
        None
    );
    let reconnect = ConnectionId::new();
    assert!(runtime.authenticate_mcp_child_connection(&credential, 9001, "start-a", reconnect));
    runtime.release_mcp_connection(reconnect);
    assert!(
        runtime
            .claim_mcp_child(9003, "start-c", 4321, 4321, reconnect, &|_, _| true)
            .is_err(),
        "a live exact-process claim must not be reassigned after disconnect"
    );
    assert!(!runtime.authenticate_mcp_child_connection(&credential, 9003, "start-c", reconnect));
    assert_eq!(
        runtime
            .claim_mcp_child(9003, "start-c", 4321, 4321, reconnect, &|pid, identity| {
                assert_eq!((pid, identity), (9001, "start-a"));
                false
            })
            .unwrap(),
        credential,
        "a replacement MCP process may claim only after exact death proof"
    );
    assert!(runtime.authenticate_mcp_child_connection(&credential, 9003, "start-c", reconnect));
    assert_eq!(runtime.mcp_caller("forged"), None);
    let run_id = OperationId::parse(&operation).unwrap();
    assert_eq!(
        runtime
            .dispatch_store()
            .binding(run_id)
            .unwrap()
            .unwrap()
            .caller,
        caller
    );
    assert_eq!(runtime.dispatch_store().inbox(&caller).unwrap(), Vec::new());
    assert_eq!(
        runtime
            .dispatch(
                &operation,
                &dispatch,
                session,
                &FakeScope(Ok(configured_scope(worktree.path())))
            )
            .unwrap(),
        admission
    );
    runtime.exit(&admission.terminal, 0).unwrap();
    assert_eq!(runtime.mcp_caller(&credential), None);
    assert!(!runtime.authenticate_mcp_child_connection(&credential, 9003, "start-c", reconnect));
    let inbox = runtime.dispatch_store().inbox(&caller).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, InboxKind::NoReport);
}

#[test]
fn completion_wake_queues_after_live_prompt_write_failure() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let workspace = WorkspaceId::new();
    let parent_session = SessionId::new();
    let parent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            Some(parent_session),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("manager").unwrap(),
        )
        .unwrap();
    let caller = CallerRef {
        session_id: Some(parent_session),
        agent_id: parent.agent_id,
    };
    let operation = OperationId::new();
    let worker_session = SessionId::new();
    runtime
        .dispatch(
            &operation.to_string(),
            &DispatchIntent {
                workspace,
                session_name: "worker".into(),
                caller,
                agent: DispatchAgentIntent::New {
                    runtime: AgentProfileId::new("claude").unwrap(),
                    model: ModelSelector::new("test").unwrap(),
                },
                prompt: "finish".into(),
            },
            worker_session,
            &FakeScope(Ok(configured_scope(worktree.path()))),
        )
        .unwrap();
    let credential = runtime
        .mcp_callers
        .iter()
        .find(|(_, caller)| caller.operation == operation)
        .map(|(credential, _)| credential.clone())
        .unwrap();
    runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(parent_session),
                profile: None,
            },
            &FakeScope(Ok(configured_scope(worktree.path()))),
        )
        .unwrap();
    assert!(
        runtime
            .prompt(
                workspace,
                Some(parent_session),
                "public queue",
                PromptMode::Queue,
            )
            .is_err(),
        "public queue mode must still reject a live target"
    );

    pty_mut(&mut runtime).write_failure = true;
    runtime
        .report_from_mcp(&credential, None, InboxKind::Completed, "done".into(), None)
        .unwrap();

    let wake = runtime
        .dispatch_store()
        .queued_prompt(workspace, Some(parent_session))
        .unwrap()
        .expect("failed live wake must remain durable for the next launch");
    assert!(wake.prompt.contains("A child report is ready"));
    assert!(wake.prompt.contains("done"));
}

#[test]
#[allow(clippy::too_many_lines)] // Related fence and completion branches share one admitted fixture.
fn completed_dispatch_does_not_receive_no_report_and_wrong_fence_is_noop() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let parent_session = SessionId::new();
    let parent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            Some(parent_session),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("manager").unwrap(),
        )
        .unwrap();
    let caller = CallerRef {
        session_id: Some(parent_session),
        agent_id: parent.agent_id,
    };
    let operation = OperationId::new().to_string();
    let dispatch = DispatchIntent {
        workspace,
        session_name: "worker".into(),
        caller: caller.clone(),
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: usagi_core::domain::agent::ModelSelector::new("test").unwrap(),
        },
        prompt: "finish".into(),
    };
    let admission = runtime
        .dispatch(
            &operation,
            &dispatch,
            session,
            &FakeScope(Ok(configured_scope(worktree.path()))),
        )
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();
    assert_eq!(
        runtime
            .report_from_mcp("forged", None, InboxKind::Completed, "ignored".into(), None)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        runtime.mcp_dispatch_caller(&credential).unwrap().session_id,
        Some(session)
    );
    let authenticated = runtime.mcp_dispatch_context(&credential).unwrap();
    assert_eq!(authenticated.workspace_id, workspace);
    assert_eq!(authenticated.run_id.to_string(), operation);
    assert_eq!(authenticated.caller.session_id, Some(session));
    assert_eq!(
        authenticated.terminal_scope,
        TerminalLaunchScope {
            workspace_id: admission.terminal.workspace_id,
            session_id: admission.terminal.session_id,
            worktree_id: admission.terminal.worktree_id,
        }
    );
    assert!(runtime.mcp_dispatch_caller("forged").is_none());
    let runtime_ref = runtime
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap();
    let fence = runtime
        .coordinator
        .record_for(&runtime_ref)
        .unwrap()
        .operation
        .clone();
    let mut wrong = fence.clone();
    wrong.owner_daemon_generation = DaemonGeneration::new();
    runtime
        .report(
            &runtime_ref,
            &wrong,
            InboxKind::Completed,
            "wrong".into(),
            None,
        )
        .unwrap();
    assert!(runtime.dispatch_store().inbox(&caller).unwrap().is_empty());
    assert_eq!(
        runtime
            .report_from_mcp(
                &credential,
                Some(OperationId::new()),
                InboxKind::Completed,
                "wrong run".into(),
                None,
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    let result = usagi_core::domain::agent::StructuredResult {
        pr: Some("https://github.com/o/r/pull/1".into()),
        commits: vec!["abc".into()],
        ..Default::default()
    };
    let delivery = runtime
        .report_from_mcp(
            &credential,
            None,
            InboxKind::Completed,
            "done".into(),
            Some(result.clone()),
        )
        .unwrap();
    assert_eq!(delivery.delivered_to, caller);
    assert_eq!(delivery.worker.session_id, Some(session));
    assert!(delivery.accepted);
    let wake = runtime
        .dispatch_store()
        .queued_prompt(workspace, Some(parent_session))
        .unwrap()
        .expect("a stopped manager must receive a durable wake prompt");
    assert!(wake.prompt.contains("A child report is ready"));
    assert!(wake.prompt.contains("done"));
    assert_eq!(
        delivery
            .committed
            .as_ref()
            .and_then(|message| message.result.as_ref()),
        Some(&result)
    );
    let completed_run = OperationId::parse(&operation).unwrap();
    let completed_binding = runtime
        .dispatch_store()
        .binding(completed_run)
        .unwrap()
        .unwrap();
    let completed_at = runtime
        .dispatch_store()
        .run(completed_run)
        .unwrap()
        .unwrap()
        .ended_at;
    runtime
        .reconcile_report_status(&completed_binding, InboxKind::Completed)
        .unwrap();
    runtime
        .reconcile_report_status(&completed_binding, InboxKind::NoReport)
        .unwrap();
    assert_eq!(
        runtime
            .dispatch_store()
            .run(completed_run)
            .unwrap()
            .unwrap()
            .ended_at,
        completed_at,
        "an already converged retry must preserve its completion time"
    );
    let replacement = usagi_core::domain::agent::StructuredResult {
        pr: Some("https://github.com/o/r/pull/2".into()),
        ..Default::default()
    };
    // Model a crash or storage failure after the inbox append committed but
    // before either registry transition became durable.
    runtime
        .dispatch_store()
        .transition_run(completed_run, RunStatus::Running, None)
        .unwrap();
    runtime
        .dispatch_store()
        .transition_agent(
            completed_binding.worker.agent_id,
            AgentStatus::Running,
            Some(completed_run),
        )
        .unwrap();
    let duplicate = runtime
        .report_from_mcp(
            &credential,
            None,
            InboxKind::Failed,
            "conflicting retry".into(),
            Some(replacement),
        )
        .unwrap();
    assert!(!duplicate.accepted);
    assert_eq!(
        duplicate
            .committed
            .as_ref()
            .and_then(|message| message.result.as_ref()),
        Some(&result),
        "a retry must expose only the first committed artifact"
    );
    assert_eq!(
        runtime
            .dispatch_store()
            .run(completed_run)
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Completed,
        "the committed outcome repairs a partially persisted run"
    );
    assert_eq!(
        runtime
            .dispatch_store()
            .agent(completed_binding.worker.agent_id)
            .unwrap()
            .unwrap()
            .status,
        AgentStatus::Idle,
        "the retry payload cannot reverse the committed outcome"
    );

    let successor_operation = OperationId::new();
    assert_eq!(
        runtime
            .dispatch(
                &successor_operation.to_string(),
                &dispatch,
                session,
                &FakeScope(Ok(configured_scope(worktree.path()))),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable,
        "public dispatch cannot replace a still-live Agent"
    );
    // Retain the late-completion regression for overlaps persisted by old
    // daemons, which allowed a successor before the predecessor PTY exited.
    let worker = runtime
        .dispatch
        .agent(completed_binding.worker.agent_id)
        .unwrap()
        .unwrap();
    let successor = runtime
        .admit_dispatch(
            successor_operation,
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(worker.runtime.clone()),
            },
            &dispatch.prompt,
            &worker,
            &caller,
            &usagi_core::infrastructure::ipc::agent_dispatch_semantic_key(
                &dispatch.session_name,
                worker.agent_id,
                &dispatch.prompt,
            ),
            &FakeScope(Ok(configured_scope(worktree.path()))),
        )
        .unwrap();
    runtime.remember_operation(
        &successor_operation.to_string(),
        None,
        Ok(successor.clone()),
    );
    let successor_binding = runtime
        .dispatch_store()
        .binding(successor_operation)
        .unwrap()
        .unwrap();
    assert_eq!(
        successor_binding.worker.agent_id, completed_binding.worker.agent_id,
        "the runtime/model selector reuses the same stable Agent"
    );
    let late_duplicate = runtime
        .report_from_mcp(
            &credential,
            None,
            InboxKind::Completed,
            "late duplicate".into(),
            None,
        )
        .unwrap();
    assert!(!late_duplicate.accepted);
    let preserved = runtime
        .dispatch_store()
        .agent(successor_binding.worker.agent_id)
        .unwrap()
        .unwrap();
    assert_eq!(preserved.status, AgentStatus::Running);
    assert_eq!(preserved.current_run, Some(successor_operation));

    runtime.exit(&admission.terminal, 0).unwrap();
    let inbox = runtime.dispatch_store().inbox(&caller).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, InboxKind::Completed);
    assert_eq!(inbox[0].result, Some(result));
    runtime.exit(&successor.terminal, 0).unwrap();

    let failed_operation = OperationId::new();
    let failed = runtime
        .dispatch(
            &failed_operation.to_string(),
            &dispatch,
            session,
            &FakeScope(Ok(configured_scope(worktree.path()))),
        )
        .unwrap();
    let failed_credential = runtime
        .mcp_callers
        .iter()
        .find(|(_, provenance)| provenance.operation == failed_operation)
        .map(|(credential, _)| credential.clone())
        .unwrap();
    runtime
        .report_from_mcp(
            &failed_credential,
            None,
            InboxKind::Failed,
            "failed".into(),
            None,
        )
        .unwrap();
    let binding = runtime
        .dispatch_store()
        .binding(failed_operation)
        .unwrap()
        .unwrap();
    runtime
        .dispatch_store()
        .transition_run(failed_operation, RunStatus::Running, None)
        .unwrap();
    runtime
        .dispatch_store()
        .transition_agent(
            binding.worker.agent_id,
            AgentStatus::Running,
            Some(failed_operation),
        )
        .unwrap();
    let duplicate = runtime
        .report_from_mcp(
            &failed_credential,
            None,
            InboxKind::Completed,
            "conflicting retry".into(),
            None,
        )
        .unwrap();
    assert!(!duplicate.accepted);
    assert_eq!(duplicate.committed.unwrap().kind, InboxKind::Failed);
    assert_eq!(
        runtime
            .dispatch_store()
            .run(failed_operation)
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Failed
    );
    assert_eq!(
        runtime
            .dispatch_store()
            .agent(binding.worker.agent_id)
            .unwrap()
            .unwrap()
            .status,
        AgentStatus::Failed
    );
    runtime.exit(&failed.terminal, 1).unwrap();
}

#[test]
fn dispatch_revalidates_current_allowlist_and_fixture_executable_before_spawn() {
    let fixture = tempfile::tempdir().unwrap();
    let executable = fixture.path().join("claude");
    std::fs::write(&executable, "fixture").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let session_worktree = tempfile::tempdir().unwrap();
    let root_scope = configured_scope(workspace.path());
    let session_scope = configured_scope(session_worktree.path());
    std::fs::write(
        session_worktree.path().join(".usagi/config.toml"),
        "[agents.claude]\nmodels = [\"session-only\"]\n",
    )
    .unwrap();
    let scope = RootAndSessionScope {
        root: root_scope,
        session: session_scope,
    };
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let session = SessionId::new();
    let dispatch = |model: &str| DispatchIntent {
        workspace: WorkspaceId::new(),
        session_name: "worker".into(),
        caller: CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: usagi_core::domain::id::AgentId::new(),
        },
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: usagi_core::domain::agent::ModelSelector::new(model).unwrap(),
        },
        prompt: "finish".into(),
    };
    let accepted = runtime
        .dispatch(
            &OperationId::new().to_string(),
            &dispatch("test"),
            session,
            &scope,
        )
        .unwrap();
    assert_eq!(accepted.terminal.session_id, Some(session));
    assert_eq!(runtime.coordinator.occupied_slots(), 1);

    std::fs::remove_file(&executable).unwrap();
    let unavailable = runtime
        .dispatch(
            &OperationId::new().to_string(),
            &dispatch("test"),
            session,
            &scope,
        )
        .unwrap_err();
    assert_eq!(unavailable.code, ErrorCode::Unavailable);
    assert_eq!(runtime.coordinator.occupied_slots(), 1);

    std::fs::write(&executable, "fixture").unwrap();
    std::fs::write(
        workspace.path().join(".usagi/config.toml"),
        "[agents.claude]\nmodels = [\"other\"]\n",
    )
    .unwrap();
    let rejected = runtime
        .dispatch(
            &OperationId::new().to_string(),
            &dispatch("test"),
            session,
            &scope,
        )
        .unwrap_err();
    assert_eq!(rejected.code, ErrorCode::InvalidArgument);
    assert_eq!(runtime.coordinator.occupied_slots(), 1);
}

/// A delegation has to build a worktree before it can dispatch into it, so
/// every refusal that needs no side effect belongs before the create. The
/// preflight raises the same refusals `dispatch` does, without touching the
/// dispatch store or the coordinator (#611).
#[test]
fn the_dispatch_preflight_refuses_before_anything_is_created() {
    let fixture = tempfile::tempdir().unwrap();
    let executable = fixture.path().join("claude");
    std::fs::write(&executable, "fixture").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let scope = configured_scope(workspace.path());
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let claude = AgentProfileId::new("claude").unwrap();
    let allowed = ModelSelector::new("test").unwrap();
    let operation = OperationId::new().to_string();
    let preflight = |runtime: &AgentRuntime, operation: &str, prompt: &str, model: &str| {
        runtime.preflight_dispatch(
            operation,
            prompt,
            &claude,
            &ModelSelector::new(model).unwrap(),
            workspace.path(),
        )
    };

    preflight(&runtime, &operation, "finish", "test").unwrap();
    assert_eq!(
        preflight(&runtime, "not-canonical", "finish", "test")
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        preflight(&runtime, &operation, "", "test")
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    // An unknown model is refused with the same message `dispatch` uses.
    let rejected = preflight(&runtime, &operation, "finish", "other").unwrap_err();
    assert_eq!(rejected.code, ErrorCode::InvalidArgument);
    assert!(rejected.message.contains("not allowed"));
    // Nothing above reserved anything: no agent, no run, no occupied slot.
    assert!(runtime.dispatch_store().agents().unwrap().is_empty());
    assert!(runtime.dispatch_store().runs().unwrap().is_empty());
    assert_eq!(runtime.coordinator.occupied_slots(), 0);

    std::fs::remove_file(&executable).unwrap();
    assert_eq!(
        preflight(&runtime, &operation, "finish", "test")
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    std::fs::write(&executable, "fixture").unwrap();

    // An operation that already owns a durable admission must not create a
    // second session for it: the spawn's outcome is not this daemon's to
    // decide again.
    let session = SessionId::new();
    let admitted = OperationId::new();
    runtime
        .dispatch(
            &admitted.to_string(),
            &DispatchIntent {
                workspace: WorkspaceId::new(),
                session_name: "worker".into(),
                caller: CallerRef {
                    session_id: Some(SessionId::new()),
                    agent_id: usagi_core::domain::id::AgentId::new(),
                },
                agent: DispatchAgentIntent::New {
                    runtime: claude.clone(),
                    model: allowed.clone(),
                },
                prompt: "finish".into(),
            },
            session,
            &FakeScope(Ok(scope)),
        )
        .unwrap();
    // This daemon already answered that operation, so a retry replays through
    // `dispatch` and the preflight deliberately admits it.
    preflight(&runtime, &admitted.to_string(), "finish", "test").unwrap();
    // A restart loses the in-memory outcome, and only the durable run is
    // left: that is the reservation the preflight must refuse to redo.
    runtime.operations.clear();
    let incomplete = preflight(&runtime, &admitted.to_string(), "finish", "test").unwrap_err();
    assert_eq!(incomplete.code, ErrorCode::OwnershipUnknown);
    assert!(incomplete.message.contains("cannot be spawned again"));
}

#[test]
fn sakana_dispatch_preflight_checks_the_executable_it_launches() {
    let fixture = tempfile::tempdir().unwrap();
    // Fugu runs the Claude CLI, so that is the executable whose absence
    // makes this runtime unavailable.
    let executable = fixture.path().join("claude");
    std::fs::write(&executable, "fixture").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join(".usagi")).unwrap();
    std::fs::write(
        workspace.path().join(".usagi/config.toml"),
        "[agents.sakana-ai]\nmodels = [\"fixture\"]\n",
    )
    .unwrap();
    let runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let operation = OperationId::new().to_string();
    let sakana = AgentProfileId::new("sakana-ai").unwrap();
    let model = ModelSelector::new("fixture").unwrap();

    runtime
        .preflight_dispatch(
            &operation,
            "inspect argv",
            &sakana,
            &model,
            workspace.path(),
        )
        .unwrap();
    std::fs::remove_file(executable).unwrap();
    assert_eq!(
        runtime
            .preflight_dispatch(
                &operation,
                "inspect argv",
                &sakana,
                &model,
                workspace.path()
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(runtime_executable("unknown-runtime"), "unknown-runtime");
}

#[test]
fn spawn_failure_is_a_fenced_safe_failure_that_replays_identically() {
    let mut runtime = AgentRuntime::new(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            spawn: Some(SpawnFailure::Definite),
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let error = runtime
        .launch(&operation, &launch_intent, &FakeScope(Ok(scope())))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    // The failure is durable: a resend returns the same safe failure.
    assert_eq!(
        runtime
            .launch(&operation, &launch_intent, &FakeScope(Ok(scope())))
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
}

#[test]
fn terminal_requests_for_unknown_refs_are_not_owned_and_output_is_stale_safe() {
    let mut runtime = runtime();
    let foreign = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    assert!(matches!(
        runtime.handle_terminal(
            ConnectionId::new(),
            ClientId::new(),
            RequestId::new(),
            TerminalAction::Attach,
            TerminalRequest::Attach {
                terminal: foreign.clone(),
                geometry: None,
            },
            SnapshotWire::RawTail,
        ),
        TerminalOutcome::NotOwned
    ));
    // Launch/Inventory never address an agent terminal.
    assert!(matches!(
        runtime.handle_terminal(
            ConnectionId::new(),
            ClientId::new(),
            RequestId::new(),
            TerminalAction::Inventory,
            TerminalRequest::Inventory {
                scope: usagi_core::domain::terminal_launch::TerminalLaunchScope {
                    workspace_id: WorkspaceId::new(),
                    session_id: None,
                    worktree_id: WorktreeId::new(),
                },
            },
            SnapshotWire::RawTail,
        ),
        TerminalOutcome::NotOwned
    ));
    assert_eq!(
        runtime.output(&foreign, b"x".to_vec()).unwrap_err().code,
        ErrorCode::StaleTarget
    );
    assert_eq!(
        runtime.exit(&foreign, 0).unwrap_err().code,
        ErrorCode::StaleTarget
    );
}

#[test]
fn agent_resize_rejects_each_forged_terminal_ref_field_before_pty_effect() {
    let mut runtime = runtime();
    let terminal = runtime
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap()
        .terminal;
    let mut forged = Vec::new();
    let mut reference = terminal.clone();
    reference.daemon_generation = DaemonGeneration::new();
    forged.push(reference);
    let mut reference = terminal.clone();
    reference.terminal_id = TerminalId::new();
    forged.push(reference);
    let mut reference = terminal.clone();
    reference.workspace_id = WorkspaceId::new();
    forged.push(reference);
    let mut reference = terminal.clone();
    reference.session_id = Some(SessionId::new());
    forged.push(reference);
    let mut reference = terminal;
    reference.worktree_id = WorktreeId::new();
    forged.push(reference);

    for terminal in forged {
        assert!(matches!(
            runtime.handle_terminal(
                ConnectionId::new(),
                ClientId::new(),
                RequestId::new(),
                TerminalAction::Resize,
                TerminalRequest::Resize {
                    terminal,
                    geometry: TerminalGeometry {
                        cols: 100,
                        rows: 40
                    },
                },
                SnapshotWire::RawTail,
            ),
            TerminalOutcome::NotOwned
        ));
    }
    assert!(pty(&runtime).resized.is_empty());
}

#[test]
fn agent_resize_failure_does_not_commit_geometry() {
    let mut runtime = runtime();
    let terminal = runtime
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap()
        .terminal;
    pty_mut(&mut runtime).resize_failure = true;

    let outcome = runtime.handle_terminal(
        ConnectionId::new(),
        ClientId::new(),
        RequestId::new(),
        TerminalAction::Resize,
        TerminalRequest::Resize {
            terminal: terminal.clone(),
            geometry: TerminalGeometry {
                cols: 100,
                rows: 40,
            },
        },
        SnapshotWire::RawTail,
    );
    let error = handled_result(outcome).unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);

    let snapshot = handled(runtime.handle_terminal(
        ConnectionId::new(),
        ClientId::new(),
        RequestId::new(),
        TerminalAction::Resync,
        TerminalRequest::Resync {
            terminal: terminal.clone(),
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(snapshot["geometry"], json!({"cols":80,"rows":24}));
    assert_eq!(
        handled_result(runtime.handle_terminal(
            ConnectionId::new(),
            ClientId::new(),
            RequestId::new(),
            TerminalAction::Attach,
            TerminalRequest::Resync {
                terminal: terminal.clone(),
            },
            SnapshotWire::RawTail,
        ))
        .unwrap_err()
        .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(pty(&runtime).resized.len(), 1);
}

#[test]
fn shared_owner_routes_agent_terminals_to_agent_and_others_to_generic() {
    let mut agent = runtime();
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let admission = agent
        .launch(&operation, &launch_intent, &FakeScope(Ok(scope())))
        .unwrap();
    let terminal = admission.terminal.clone();
    agent.output(&terminal, b"hi\n".to_vec()).unwrap();

    let mut owner = SharedTerminalOwner::new(agent, FakeGeneric::default());
    let connection = ConnectionId::new();
    let client = ClientId::new();
    // Agent terminal → agent owner.
    let attached = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Attach,
            serde_json::to_value(TerminalRequest::Attach {
                terminal: terminal.clone(),
                geometry: None,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap();
    assert_eq!(attached["snapshot"]["replay"], json!(b"hi\n".to_vec()));

    // A generic Launch (no agent terminal) → generic owner.
    let generic = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Launch,
            serde_json::to_value(TerminalRequest::Launch {
                intent: usagi_core::infrastructure::ipc::TerminalLaunchIntent {
                    request: usagi_core::domain::terminal_launch::TerminalLaunchRequest {
                        profile_id: usagi_core::domain::terminal_launch::TerminalProfileId::new(
                            "login-shell",
                        )
                        .unwrap(),
                        scope: usagi_core::domain::terminal_launch::TerminalLaunchScope {
                            workspace_id: WorkspaceId::new(),
                            session_id: Some(SessionId::new()),
                            worktree_id: WorktreeId::new(),
                        },
                    },
                    geometry: usagi_core::infrastructure::ipc::TerminalGeometry {
                        cols: 80,
                        rows: 24,
                    },
                    launch_operation: None,
                },
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap();
    assert_eq!(generic["terminals"], json!([]));

    // Malformed payload is rejected before either usecase owner runs.
    owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Attach,
            json!({ "operation": "bogus" }),
            SnapshotWire::RawTail,
        )
        .unwrap_err();

    TerminalOwner::disconnect(&mut owner, connection);
    assert_eq!(owner.generic.requests, 1);
    assert_eq!(owner.generic.disconnects, 1);
}

#[test]
fn shared_owner_inventory_merges_agent_and_generic_and_rejects_invalid_scope() {
    use usagi_core::domain::terminal_launch::{
        TerminalInventoryEntry, TerminalKind, TerminalLaunchScope,
    };

    let mut agent = runtime();
    let operation = OperationId::new().to_string();
    let admission = agent
        .launch(&operation, &intent(None), &FakeScope(Ok(scope())))
        .unwrap();
    let agent_terminal = admission.terminal.clone();
    // Query with the launched Agent's exact scope so it is in scope.
    let inventory_scope = TerminalLaunchScope {
        workspace_id: agent_terminal.workspace_id,
        session_id: agent_terminal.session_id,
        worktree_id: agent_terminal.worktree_id,
    };
    // A generic terminal the generic owner reports for the same scope.
    let generic_terminal = TerminalRef {
        daemon_generation: agent_terminal.daemon_generation,
        terminal_id: TerminalId::new(),
        workspace_id: agent_terminal.workspace_id,
        session_id: agent_terminal.session_id,
        worktree_id: agent_terminal.worktree_id,
    };
    let generic = FakeGeneric {
        inventory: vec![TerminalInventoryEntry {
            terminal: generic_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        }],
        ..FakeGeneric::default()
    };
    let mut owner = SharedTerminalOwner::new(agent, generic);
    let connection = ConnectionId::new();
    let client = ClientId::new();
    // The shared owner handles Inventory through `request`; when used as a
    // nested generic owner its trait-level default inventory is empty.
    assert!(TerminalOwner::inventory(&owner, &inventory_scope).is_empty());

    let reply = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Inventory,
            serde_json::to_value(TerminalRequest::Inventory {
                scope: inventory_scope,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap();
    let entries: Vec<TerminalInventoryEntry> =
        serde_json::from_value(reply["terminals"].clone()).unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|entry| {
        entry.kind == TerminalKind::Terminal
            && entry.terminal.fences(&generic_terminal)
            && entry.live
    }));
    assert!(entries.iter().any(|entry| {
        entry.kind == TerminalKind::Agent && entry.terminal.fences(&agent_terminal) && entry.live
    }));

    // A payload that is not a valid inventory request is a safe rejection,
    // never a generic-owner fallback.
    let error = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Inventory,
            json!({ "operation": "bogus" }),
            SnapshotWire::RawTail,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[test]
fn an_agent_runtime_refuses_a_snapshot_schema_it_cannot_read() {
    let refused = AgentRuntime::hydrate_with_retention(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(tempfile::tempdir().unwrap().keep()),
        PathExecutableLocator,
        RuntimeStoreSnapshot {
            schema_version: 99,
            ..RuntimeStoreSnapshot::default()
        },
        SharedTerminalRetention::new(),
    )
    .err();

    // Startup fails closed: no generation is activated and no admission opens.
    assert_eq!(
        refused,
        Some(super::super::runtime::RuntimeSnapshotError::UnknownSchema(
            99
        ))
    );
}

#[test]
fn dismissing_a_tombstone_makes_it_the_first_eviction_candidate() {
    use crate::usecase::terminal_retention_ipc::tests::manual_retention;
    use usagi_core::domain::terminal_visibility::TerminalVisibilityState;

    let (retention, clock) = manual_retention();
    let mut agent = AgentRuntime::hydrate_with_retention(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(tempfile::tempdir().unwrap().keep()),
        PathExecutableLocator,
        RuntimeStoreSnapshot::default(),
        retention.clone(),
    )
    .unwrap();
    let admission = agent
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let terminal = admission.terminal.clone();
    agent.exit(&terminal, 0).unwrap();
    assert_eq!(
        retention
            .lookup(&terminal)
            .retained()
            .map(|record| record.visibility),
        Some(TerminalVisibilityState::Unobserved)
    );

    let mut owner = SharedTerminalOwner::with_visibility_and_retention(
        agent,
        FakeGeneric::default(),
        SharedTerminalVisibility::new(),
        retention.clone(),
    );
    let connection = ConnectionId::new();
    let client = ClientId::new();
    for (action, request, expected) in [
        (
            TerminalAction::Observe,
            TerminalRequest::Observe {
                terminal: terminal.clone(),
                expected_revision: 0,
            },
            TerminalVisibilityState::Observed,
        ),
        (
            TerminalAction::Dismiss,
            TerminalRequest::Dismiss {
                terminal: terminal.clone(),
                expected_revision: 1,
            },
            TerminalVisibilityState::Dismissed,
        ),
    ] {
        let reply = owner
            .request(
                connection,
                client,
                RequestId::new(),
                action,
                serde_json::to_value(request).unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap();
        assert_eq!(reply["applied"], json!(true));
        // The retention class follows the authoritative visibility.
        assert_eq!(
            retention
                .lookup(&terminal)
                .retained()
                .map(|record| record.visibility),
            Some(expected)
        );
    }

    // The periodic collector then ages the dismissed final out.
    clock.advance(1000);
    assert_eq!(owner.agent.collect_retention_garbage(), 1);
    assert!(retention.lookup(&terminal).marker().is_some());
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers merge, stamping, and CAS.
fn shared_owner_completed_inventory_merges_and_stamps_visibility() {
    use usagi_core::domain::terminal_launch::{TerminalKind, TerminalLaunchScope};
    use usagi_core::domain::terminal_visibility::{
        CompletedTerminalEntry, TerminalVisibility, TerminalVisibilityState,
    };

    let mut agent = runtime();
    let operation = OperationId::new().to_string();
    let admission = agent
        .launch(&operation, &intent(None), &FakeScope(Ok(scope())))
        .unwrap();
    let agent_terminal = admission.terminal.clone();
    // Exit the Agent so it becomes an exited tombstone, not a live runtime.
    agent.exit(&agent_terminal, 0).unwrap();
    let query_scope = TerminalLaunchScope {
        workspace_id: agent_terminal.workspace_id,
        session_id: agent_terminal.session_id,
        worktree_id: agent_terminal.worktree_id,
    };
    let generic_terminal = TerminalRef {
        daemon_generation: agent_terminal.daemon_generation,
        terminal_id: TerminalId::new(),
        workspace_id: agent_terminal.workspace_id,
        session_id: agent_terminal.session_id,
        worktree_id: agent_terminal.worktree_id,
    };
    let generic = FakeGeneric {
        completed: vec![CompletedTerminalEntry {
            terminal: generic_terminal.clone(),
            kind: TerminalKind::Terminal,
            exit_status: 3,
            base_offset: 0,
            final_output_offset: 12,
            visibility: TerminalVisibility::unobserved(),
        }],
        ..FakeGeneric::default()
    };
    let mut owner = SharedTerminalOwner::new(agent, generic);
    let connection = ConnectionId::new();
    let client = ClientId::new();
    let query = |owner: &mut SharedTerminalOwner<_, _>| -> Vec<CompletedTerminalEntry> {
        let reply = owner
            .request(
                connection,
                client,
                RequestId::new(),
                TerminalAction::CompletedInventory,
                serde_json::to_value(TerminalRequest::CompletedInventory {
                    scope: query_scope.clone(),
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap();
        serde_json::from_value(reply["entries"].clone()).unwrap()
    };

    let entries = query(&mut owner);
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| { entry.visibility.state == TerminalVisibilityState::Unobserved })
    );
    let agent_entry = entries
        .iter()
        .find(|entry| entry.kind == TerminalKind::Agent)
        .unwrap();
    assert!(agent_entry.terminal.fences(&agent_terminal));

    // Observe the generic tombstone and re-query: only that exact ref rises.
    let observed = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Observe,
            serde_json::to_value(TerminalRequest::Observe {
                terminal: generic_terminal.clone(),
                expected_revision: 0,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap();
    assert_eq!(observed["applied"], serde_json::json!(true));
    assert_eq!(observed["conflict"], serde_json::json!(false));

    let entries = query(&mut owner);
    let generic_entry = entries
        .iter()
        .find(|entry| entry.terminal.fences(&generic_terminal))
        .unwrap();
    assert_eq!(
        generic_entry.visibility.state,
        TerminalVisibilityState::Observed
    );
    assert_eq!(generic_entry.exit_status, 3);
    // The Agent tombstone's independent visibility is untouched.
    let agent_entry = entries
        .iter()
        .find(|entry| entry.kind == TerminalKind::Agent)
        .unwrap();
    assert_eq!(
        agent_entry.visibility.state,
        TerminalVisibilityState::Unobserved
    );

    // An invalid completed-inventory payload is a safe rejection.
    let error = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::CompletedInventory,
            json!({ "operation": "bogus" }),
            SnapshotWire::RawTail,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers observe/dismiss CAS, conflict, and rejection.
fn shared_owner_observe_and_dismiss_are_cas_and_do_not_touch_the_process() {
    use usagi_core::domain::terminal_visibility::{TerminalVisibility, TerminalVisibilityState};

    let mut owner = SharedTerminalOwner::new(runtime(), FakeGeneric::default());
    let connection = ConnectionId::new();
    let client = ClientId::new();
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    let visibility = |value: &Value| -> TerminalVisibility {
        serde_json::from_value(value["visibility"].clone()).unwrap()
    };
    let send = |owner: &mut SharedTerminalOwner<_, _>,
                action: TerminalAction,
                request: TerminalRequest|
     -> Value {
        owner
            .request(
                connection,
                client,
                RequestId::new(),
                action,
                serde_json::to_value(request).unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap()
    };

    let observed = send(
        &mut owner,
        TerminalAction::Observe,
        TerminalRequest::Observe {
            terminal: terminal.clone(),
            expected_revision: 0,
        },
    );
    assert_eq!(
        visibility(&observed).state,
        TerminalVisibilityState::Observed
    );
    assert_eq!(visibility(&observed).revision, 1);

    // A stale dismiss conflicts and returns the authoritative snapshot.
    let conflict = send(
        &mut owner,
        TerminalAction::Dismiss,
        TerminalRequest::Dismiss {
            terminal: terminal.clone(),
            expected_revision: 0,
        },
    );
    assert_eq!(conflict["applied"], serde_json::json!(false));
    assert_eq!(conflict["conflict"], serde_json::json!(true));
    assert_eq!(
        visibility(&conflict).state,
        TerminalVisibilityState::Observed
    );

    // Merging to the authoritative revision succeeds.
    let dismissed = send(
        &mut owner,
        TerminalAction::Dismiss,
        TerminalRequest::Dismiss {
            terminal: terminal.clone(),
            expected_revision: 1,
        },
    );
    assert_eq!(dismissed["applied"], serde_json::json!(true));
    assert_eq!(
        visibility(&dismissed).state,
        TerminalVisibilityState::Dismissed
    );

    // A stale observe never lowers the dismissed state (idempotent no-op).
    let idempotent = send(
        &mut owner,
        TerminalAction::Observe,
        TerminalRequest::Observe {
            terminal,
            expected_revision: 0,
        },
    );
    assert_eq!(idempotent["applied"], serde_json::json!(false));
    assert_eq!(idempotent["conflict"], serde_json::json!(false));
    assert_eq!(
        visibility(&idempotent).state,
        TerminalVisibilityState::Dismissed
    );

    // A well-formed but non-visibility payload under a visibility action is
    // a safe rejection, never routed to a terminal handler.
    let mismatch = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Observe,
            serde_json::to_value(TerminalRequest::Attach {
                terminal: TerminalRef {
                    daemon_generation: DaemonGeneration::new(),
                    terminal_id: TerminalId::new(),
                    workspace_id: WorkspaceId::new(),
                    session_id: None,
                    worktree_id: WorktreeId::new(),
                },
                geometry: None,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap_err();
    assert_eq!(mismatch.code, ErrorCode::InvalidArgument);

    // A malformed visibility payload is a safe rejection.
    let error = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Observe,
            json!({ "operation": "bogus" }),
            SnapshotWire::RawTail,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[test]
fn used_helpers_stay_referenced() {
    // Keep the fake adapter machinery exercised so the imports the E2E relies
    // on cannot silently rot.
    let mut adapter = ClaudeAdapter::new(FakeProvisioner);
    let request = LaunchRequest {
        profile_id: AgentProfileId::new("claude").unwrap(),
        mode: LaunchMode::Interactive,
        model: None,
        resume: false,
        provider_resume: None,
        initial_prompt: None,
        scope: LaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        },
        required_capabilities: BTreeSet::new(),
    };
    let resolved: ResolvedLaunch = adapter.resolve(&request).unwrap();
    assert_eq!(resolved.snapshot.plan.program, "claude");
    let inner = CodexAdapter::new(FakeCodexProvisioner);
    let profile = inner.profile().clone();
    let mut override_adapter = ProfileOverrideAdapter { profile, inner };
    let codex_request = LaunchRequest {
        profile_id: AgentProfileId::new("codex").unwrap(),
        ..request
    };
    assert_eq!(
        override_adapter
            .resolve(&codex_request)
            .unwrap()
            .snapshot
            .plan
            .program,
        "codex"
    );
    let _ = (
        AdapterError::ProvisionFailed,
        AgentCapability::Resume,
        AgentProfile::new(
            AgentProfileId::new("claude").unwrap(),
            "Claude",
            1,
            [],
            [LaunchMode::Interactive],
        ),
        LaunchPlan::new(
            AgentProfileId::new("claude").unwrap(),
            1,
            "claude",
            vec![],
            [],
            PathBuf::from("."),
        )
        .unwrap(),
    );

    let mut runtime = runtime();
    let (restart_snapshot, _) = runtime
        .coordinator
        .snapshot()
        .reconcile_after_daemon_restart();
    runtime.coordinator = RuntimeCoordinator::hydrate(restart_snapshot, 16, 64 * 1024, 64)
        .expect("a reconciled empty snapshot is valid");
    assert_eq!(
        runtime.active_generation().unwrap_err().code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
fn trimmed_agent_output_maps_to_a_resync_protocol_error() {
    let error = map_runtime_error(RuntimeError::Terminal(RegistryError::ResyncRequired));

    assert_eq!(error.code, ErrorCode::ResyncRequired);
}

#[test]
#[allow(clippy::too_many_lines)] // Table-style coverage of all helper error and replay outcomes.
fn helper_error_routes_and_durable_replay_outcomes_are_total() {
    use super::super::runtime::{DurableOperationOutcome, ReconcileState};

    for (state, expected) in [
        (
            super::super::runtime::RuntimeState::Running,
            AgentPhase::Running,
        ),
        (
            super::super::runtime::RuntimeState::Reserved,
            AgentPhase::Ready,
        ),
        (
            super::super::runtime::RuntimeState::SpawnFailed,
            AgentPhase::Exited,
        ),
        (
            super::super::runtime::RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown),
            AgentPhase::Interrupted,
        ),
        (
            super::super::runtime::RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning),
            AgentPhase::Exited,
        ),
        (
            super::super::runtime::RuntimeState::Exited,
            AgentPhase::Ended,
        ),
        (
            super::super::runtime::RuntimeState::Reclaimed,
            AgentPhase::Ended,
        ),
    ] {
        assert_eq!(runtime_phase(state).1, expected);
    }
    for (phase, priority) in [
        (AgentPhase::Absent, 0),
        (AgentPhase::Ready, 3),
        (AgentPhase::Running, 5),
        (AgentPhase::Waiting, 6),
        (AgentPhase::Ended, 7),
        (AgentPhase::Exited, 8),
        (AgentPhase::Interrupted, 4),
    ] {
        assert_eq!(reported_phase(phase), (priority, phase));
    }
    assert!(is_resume_source_state(
        super::super::runtime::RuntimeState::Exited
    ));
    assert!(is_resume_source_state(
        super::super::runtime::RuntimeState::Reclaimed
    ));
    assert!(is_resume_source_state(
        super::super::runtime::RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown)
    ));
    assert!(!is_resume_source_state(
        super::super::runtime::RuntimeState::Running
    ));
    let run_id = OperationId::new();
    for kind in [InboxKind::Completed, InboxKind::Failed, InboxKind::NoReport] {
        let message = InboxMessage {
            run_id,
            from: WorkerRef {
                session_id: None,
                agent_id: usagi_core::domain::id::AgentId::new(),
            },
            kind,
            summary: String::new(),
            result: None,
            created_at: Utc::now(),
            read: false,
        };
        assert_eq!(message.run_id, run_id);
        assert_ne!(message.run_id, OperationId::new());
    }

    let mut orphan_runtime = runtime();
    let admission = orphan_runtime
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let mut record = orphan_runtime.coordinator.snapshot().records.remove(0);
    for (outcome, expected_code, completed) in [
        (DurableOperationOutcome::Accepted, None, false),
        (DurableOperationOutcome::Completed, None, true),
        (
            DurableOperationOutcome::SpawnUnavailable,
            Some(ErrorCode::Unavailable),
            false,
        ),
        (
            DurableOperationOutcome::ExitUnavailable,
            Some(ErrorCode::Unavailable),
            false,
        ),
        (
            DurableOperationOutcome::OwnershipUnknown,
            Some(ErrorCode::OwnershipUnknown),
            false,
        ),
    ] {
        record.outcome = outcome;
        let projection = durable_operation_outcome(&record);
        if let Some(code) = expected_code {
            assert_eq!(projection.unwrap_err().code, code);
        } else {
            assert_eq!(projection.unwrap().completed, completed);
        }
    }
    assert_eq!(record.runtime.terminal, admission.terminal);
    assert_eq!(
        handled_result(TerminalOutcome::NotOwned).unwrap_err().code,
        ErrorCode::StaleTarget
    );

    assert_eq!(
        terminal_geometry(TerminalGeometry { cols: 0, rows: 1 })
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        map_scope_error(ScopeResolveError::Unavailable).code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        map_scope_error(ScopeResolveError::Storage).code,
        ErrorCode::Unavailable
    );
    for (error, code) in [
        (OrchestrationError::Unauthorized, ErrorCode::InvalidArgument),
        (
            OrchestrationError::UnknownProfile,
            ErrorCode::InvalidArgument,
        ),
        (OrchestrationError::UnknownRuntime, ErrorCode::StaleTarget),
    ] {
        assert_eq!(map_orchestration_error(error).code, code);
    }
    for (error, code) in [
        (
            RuntimeError::Adapter(AdapterError::ExecutableUnavailable),
            ErrorCode::Unavailable,
        ),
        (
            RuntimeError::Adapter(AdapterError::ProvisionFailed),
            ErrorCode::Unavailable,
        ),
        (
            RuntimeError::RuntimeAlreadyExists,
            ErrorCode::RevisionConflict,
        ),
        (RuntimeError::ScopeMismatch, ErrorCode::InvalidArgument),
        (
            RuntimeError::ConcurrencyExhausted,
            ErrorCode::ResourceExhausted,
        ),
        (
            RuntimeError::Terminal(RegistryError::PtyResizeFailed),
            ErrorCode::Unavailable,
        ),
        (
            RuntimeError::Terminal(RegistryError::CheckpointUnavailable),
            ErrorCode::ResourceExhausted,
        ),
        (
            RuntimeError::Terminal(RegistryError::IdempotencyExpired),
            ErrorCode::IdempotencyExpired,
        ),
        (
            RuntimeError::Terminal(RegistryError::SequenceGap),
            ErrorCode::SequenceGap,
        ),
        (
            RuntimeError::Terminal(RegistryError::StaleTarget),
            ErrorCode::StaleTarget,
        ),
        (RuntimeError::UnknownRuntime, ErrorCode::StaleTarget),
        (
            RuntimeError::TerminalGenerationMismatch,
            ErrorCode::StaleTarget,
        ),
        (RuntimeError::Store, ErrorCode::OwnershipUnknown),
        (RuntimeError::Journal, ErrorCode::OwnershipUnknown),
        (
            RuntimeError::ReconcileRequired(ReconcileState::IdentityUnknown),
            ErrorCode::OwnershipUnknown,
        ),
        (RuntimeError::SpawnFailed, ErrorCode::Unavailable),
        // An unreservable admission is exhausted capacity; a collected
        // final is expired history, not a stale runtime reference.
        (
            RuntimeError::RetentionExhausted(
                usagi_core::domain::terminal_retention::AdmissionRejection {
                    scope: usagi_core::domain::terminal_retention::RetentionScope::Workspace,
                    dimension: usagi_core::domain::terminal_retention::RetentionDimension::Bytes,
                },
            ),
            ErrorCode::ResourceExhausted,
        ),
        (
            RuntimeError::FinalEvicted(
                usagi_core::domain::terminal_retention::EvictionReason::Emergency,
            ),
            ErrorCode::NotFound,
        ),
    ] {
        assert_eq!(map_runtime_error(error).code, code);
    }

    let root = AgentLaunchIntent {
        workspace: WorkspaceId::new(),
        session: None,
        profile: None,
    };
    assert!(semantic_key(&root).contains("workspace-root:<default>"));
    let counter = Arc::new(AtomicU32::new(0));
    let mut pty = Pty {
        spawn_counter: Some(counter),
        ..Pty::default()
    };
    pty.select_terminal(&admission.terminal);
    pty.resize(&admission.terminal, Geometry { cols: 1, rows: 1 })
        .unwrap();
    pty.write_all(b"x").unwrap();
    assert_eq!(
        map_dispatch_storage_error(anyhow::anyhow!("store failpoint")).code,
        ErrorCode::Unavailable
    );
    assert_eq!(
        map_dispatch_storage_error(anyhow::anyhow!(
            "dispatch inbox capacity is exhausted by unacknowledged messages"
        ))
        .code,
        ErrorCode::ResourceExhausted
    );
    assert_eq!(
        map_dispatch_storage_error(anyhow::anyhow!(
            "dispatch registry capacity is exhausted by protected records"
        ))
        .code,
        ErrorCode::ResourceExhausted
    );
}

#[test]
fn dispatch_rejects_invalid_unknown_and_foreign_requests_before_spawn() {
    let mut runtime = runtime();
    let session = SessionId::new();
    let caller = CallerRef {
        session_id: Some(SessionId::new()),
        agent_id: usagi_core::domain::id::AgentId::new(),
    };
    let unknown = DispatchIntent {
        workspace: WorkspaceId::new(),
        session_name: "worker".into(),
        caller: caller.clone(),
        agent: DispatchAgentIntent::Existing {
            agent_id: usagi_core::domain::id::AgentId::new(),
        },
        prompt: "work".into(),
    };
    assert_eq!(
        runtime
            .dispatch("invalid", &unknown, session, &FakeScope(Ok(scope())))
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let mut empty = unknown.clone();
    empty.prompt.clear();
    assert_eq!(
        runtime
            .dispatch(
                &OperationId::new().to_string(),
                &empty,
                session,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .dispatch(
                &OperationId::new().to_string(),
                &unknown,
                session,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );

    let foreign_session = SessionId::new();
    let foreign = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            unknown.workspace,
            Some(foreign_session),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("test").unwrap(),
        )
        .unwrap();
    let foreign_intent = DispatchIntent {
        agent: DispatchAgentIntent::Existing {
            agent_id: foreign.agent_id,
        },
        ..unknown
    };
    assert_eq!(
        runtime
            .dispatch(
                &OperationId::new().to_string(),
                &foreign_intent,
                session,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert!(runtime.coordinator.snapshot().records.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)] // The durable admission states intentionally share one replay setup.
fn dispatch_replays_prepared_conflicting_and_legacy_admissions_without_respawn() {
    let temp = tempfile::tempdir().unwrap();
    let dispatch_dir = temp.path().join("dispatch");
    let session = SessionId::new();
    let workspace = WorkspaceId::new();
    let durable = DispatchStore::new(&dispatch_dir);
    let worker = durable
        .upsert_agent_by_runtime_model(
            workspace,
            Some(session),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("test").unwrap(),
        )
        .unwrap();
    let intent = DispatchIntent {
        workspace,
        session_name: "worker".into(),
        caller: CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: usagi_core::domain::id::AgentId::new(),
        },
        agent: DispatchAgentIntent::Existing {
            agent_id: worker.agent_id,
        },
        prompt: "work".into(),
    };
    let operation = OperationId::new();
    let make_runtime = |store| {
        AgentRuntime::with_dispatch(
            DaemonGeneration::new(),
            claude_registry(),
            store,
            Journal::default(),
            Pty::default(),
            AgentProfileId::new("claude").unwrap(),
            Geometry { cols: 80, rows: 24 },
            DispatchStore::new(&dispatch_dir),
        )
    };
    let mut first = make_runtime(Store {
        saves: 0,
        fail_after: Some(0),
        ..Store::default()
    });
    assert_eq!(
        first
            .dispatch(
                &operation.to_string(),
                &intent,
                session,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    for (candidate, code) in [
        (intent.clone(), ErrorCode::OwnershipUnknown),
        (
            DispatchIntent {
                prompt: "different".into(),
                ..intent.clone()
            },
            ErrorCode::IdempotencyConflict,
        ),
    ] {
        assert_eq!(
            make_runtime(Store::default())
                .dispatch(
                    &operation.to_string(),
                    &candidate,
                    session,
                    &FakeScope(Ok(scope())),
                )
                .unwrap_err()
                .code,
            code
        );
    }

    let legacy_dir = temp.path().join("legacy");
    let legacy = DispatchStore::new(&legacy_dir);
    let worker = legacy
        .upsert_agent_by_runtime_model(
            workspace,
            Some(session),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("test").unwrap(),
        )
        .unwrap();
    let legacy_operation = OperationId::new();
    legacy
        .upsert_run(DispatchRun {
            run_id: legacy_operation,
            agent_id: worker.agent_id,
            prompt: "legacy".into(),
            started_at: Utc::now(),
            ended_at: None,
            status: RunStatus::Preparing,
        })
        .unwrap();
    let mut runtime = AgentRuntime::with_dispatch(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        legacy,
    );
    assert_eq!(
        runtime
            .dispatch(
                &legacy_operation.to_string(),
                &DispatchIntent {
                    agent: DispatchAgentIntent::Existing {
                        agent_id: worker.agent_id,
                    },
                    ..intent
                },
                session,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
fn admission_commit_failpoint_compensates_partial_effects() {
    let operation = OperationId::new();
    let mut orphan_runtime = runtime();
    let admission = orphan_runtime
        .launch(
            &operation.to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let runtime_ref = orphan_runtime
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap();
    assert_eq!(
        orphan_runtime
            .finish_admission_commit(operation, "missing", &runtime_ref, false)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert!(matches!(
        orphan_runtime
            .coordinator
            .record_for(&runtime_ref)
            .unwrap()
            .state,
        super::super::runtime::RuntimeState::ReconcileRequired(
            super::super::runtime::ReconcileState::OrphanRunning
        )
    ));

    let operation = OperationId::new();
    let mut runtime = runtime();
    let admission = runtime
        .launch(
            &operation.to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let runtime_ref = runtime
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap();
    pty_mut(&mut runtime).terminate_success = true;
    let saves = store_mut(&mut runtime).saves;
    store_mut(&mut runtime).fail_after = Some(saves);
    assert_eq!(
        runtime
            .finish_admission_commit(operation, "missing", &runtime_ref, false)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert!(matches!(
        runtime.coordinator.record_for(&runtime_ref).unwrap().state,
        super::super::runtime::RuntimeState::SpawnFailed
    ));
}

#[test]
fn clean_never_selects_a_live_runtime_even_if_its_dispatch_run_is_failed() {
    let operation = OperationId::new();
    let mut runtime = runtime();
    let admission = runtime
        .launch(
            &operation.to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    runtime.dispatch.fail_admission(operation).unwrap();

    assert!(runtime.failed_reservation_ids().unwrap().is_empty());
    assert_eq!(runtime.clean_failed_reservations().unwrap(), 0);
    assert_eq!(
        runtime
            .coordinator
            .record_for(
                &runtime
                    .coordinator
                    .runtime_for_terminal(&admission.terminal)
                    .unwrap()
            )
            .unwrap()
            .state,
        super::super::runtime::RuntimeState::Running
    );
}

#[test]
fn clean_repairs_a_failed_dispatch_reservation_and_hides_ghost_ready() {
    let operation = OperationId::new();
    let mut runtime = runtime();
    let admission = runtime
        .launch(
            &operation.to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let session = admission.terminal.session_id.unwrap();
    runtime.dispatch.fail_admission(operation).unwrap();
    let mut snapshot = runtime.coordinator.snapshot();
    snapshot.records[0].state = super::super::runtime::RuntimeState::ReconcileRequired(
        super::super::runtime::ReconcileState::IdentityUnknown,
    );
    snapshot.records[0].process = None;
    snapshot.generation.terminals[0].process = None;
    snapshot.generation.terminals[0].state =
        super::super::generation::TerminalState::IdentityUnknown;
    runtime.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 1024, 2).unwrap();

    assert_eq!(runtime.failed_reservation_ids().unwrap().len(), 1);
    assert_eq!(runtime.session_phase(session), AgentPhase::Exited);
    assert_eq!(
        runtime.inventory(admission.terminal.workspace_id).runtimes[0].state,
        AgentRuntimeInventoryState::Unavailable
    );
    assert_eq!(runtime.clean_failed_reservations().unwrap(), 1);
    assert!(runtime.failed_reservation_ids().unwrap().is_empty());
    assert_eq!(
        runtime.coordinator.snapshot().records[0].state,
        super::super::runtime::RuntimeState::SpawnFailed
    );
    assert_eq!(runtime.close_session(session).unwrap(), 1);
    assert!(runtime.coordinator.snapshot().records.is_empty());
}

fn handled(outcome: TerminalOutcome<Value>) -> Value {
    handled_result(outcome).unwrap()
}

fn handled_result(outcome: TerminalOutcome<Value>) -> Result<Value, ProtocolError> {
    match outcome {
        TerminalOutcome::Handled(result) => result,
        TerminalOutcome::NotOwned => Err(stale_terminal()),
    }
}

#[test]
fn agent_dispatch_refuses_non_terminal_typed_requests() {
    let mut runtime = runtime();
    let admission = runtime
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let runtime_ref = runtime
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap();
    let error = runtime
        .dispatch_terminal(
            TerminalRequestContext {
                connection: ConnectionId::new(),
                client: ClientId::new(),
                request: RequestId::new(),
            },
            TerminalRequest::Inventory {
                scope: usagi_core::domain::terminal_launch::TerminalLaunchScope {
                    workspace_id: WorkspaceId::new(),
                    session_id: None,
                    worktree_id: WorktreeId::new(),
                },
            },
            &runtime_ref,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}
