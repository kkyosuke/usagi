//! agent ipc の振る舞いを固定するテスト。

mod admission;
mod dispatch;
mod restart;
mod resume;
mod terminal;

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
struct FakeAgentProvisioner;
impl ClaudeProvisioner for FakeAgentProvisioner {
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

struct FakeAgentCodexProvisioner;
impl CodexProvisioner for FakeAgentCodexProvisioner {
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

struct FakeAgentAgyProvisioner;
impl AgyProvisioner for FakeAgentAgyProvisioner {
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
    inner: ClaudeAdapter<FakeAgentProvisioner>,
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
    inner: CodexAdapter<FakeAgentCodexProvisioner>,
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
        inner: ClaudeAdapter::new(FakeAgentProvisioner),
    };
    let profile = adapter.inner.profile().clone();
    registry.register(profile, Box::new(adapter)).unwrap();
    registry
}

fn structured_claude_runtime() -> AgentRuntime {
    let mut registry = AdapterRegistry::new();
    let adapter = ClaudeAdapter::new(FakeAgentProvisioner);
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
    let adapter = CodexAdapter::new(FakeAgentCodexProvisioner);
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
    let adapter = AgyAdapter::new(FakeAgentAgyProvisioner);
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
#[allow(clippy::too_many_lines)] // The fixture fixes root/session and mixed-provider ordering in one inventory.
fn exact_inventory_separates_root_sessions_and_same_scope_histories() {
    let mut registry = claude_registry();
    let codex = CodexAdapter::new(FakeAgentCodexProvisioner);
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
fn used_helpers_stay_referenced() {
    // Keep the fake adapter machinery exercised so the imports the E2E relies
    // on cannot silently rot.
    let mut adapter = ClaudeAdapter::new(FakeAgentProvisioner);
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
    let inner = CodexAdapter::new(FakeAgentCodexProvisioner);
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

fn handled(outcome: TerminalOutcome<Value>) -> Value {
    handled_result(outcome).unwrap()
}

fn handled_result(outcome: TerminalOutcome<Value>) -> Result<Value, ProtocolError> {
    match outcome {
        TerminalOutcome::Handled(result) => result,
        TerminalOutcome::NotOwned => Err(stale_terminal()),
    }
}
