//! Daemon-owned Agent launch IPC owner.
//!
//! This module turns a product-neutral [`AgentLaunchIntent`] into a durable
//! launch through the [`Orchestrator`] and [`RuntimeCoordinator`], resolving the
//! target checkout only through the injected #268 [`SessionScopeResolver`].  It
//! reuses the shared terminal registry/stream contract owned by the coordinator
//! rather than duplicating the generic terminal (#264) owner loop: agent
//! terminals are attached, streamed and reaped through the same
//! [`TerminalRef`]-fenced vocabulary.
//!
//! A client never supplies a path, name, argv, environment value, or secret;
//! failure, ambiguity, and stale completions surface only safe feedback and
//! never authorize a replacement spawn or a terminal guess.

#![allow(
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_arguments
)] // Injected runtime ports make these boundary signatures part of the contract.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use usagi_core::{
    domain::session_lifecycle::AgentPhase,
    domain::{
        agent::{
            AgentCapability, AgentIntegrationDiagnosis, AgentIntegrationRevision, AgentInventory,
            AgentProfileId, AgentResumableInventoryItem, AgentResumeRelation, AgentResumeTarget,
            AgentRuntimeInventoryItem, AgentRuntimeInventoryState, AgentStatus,
            AgentWorkspaceObservation, CallerRef, DaemonRestartAgent, DaemonRestartAgentPlan,
            DispatchBinding, DispatchRun, InboxKind, InboxMessage, LaunchMode, LaunchRequest,
            LaunchScope, ModelSelector, OutdatedAgentRuntime, ProviderCaptureProvenance,
            ProviderKind, ProviderResumePhase, ProviderResumeReason, ProviderResumeRef,
            ProviderResumeStatus, ProviderSessionId, RunStatus, WorkerRef, dominant_agent_status,
        },
        id::{
            AgentContinuationRef, AgentId, AgentRuntimeId, AgentRuntimeRef, CompletionFence,
            ConnectionId, DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef,
            WorkspaceId, WorktreeId,
        },
        supervisor::RunProvenance,
        terminal_launch::TerminalLaunchScope,
    },
    infrastructure::ipc::{
        AgentGoalIntent, AgentLaunchIntent, DispatchAgentIntent, DispatchIntent, ErrorCode,
        MAX_AGENT_GOAL_BYTES, ProtocolError, TerminalRequest, agent_operation_digest,
    },
    infrastructure::runtime_model::{
        ExecutableLocator, PathExecutableLocator, WorkspaceAgentConfig, supported_agent_runtimes,
    },
    infrastructure::store::dispatch::{
        AgentAdmissionReservation, CredentialProvenance as DispatchCredentialProvenance,
        DispatchStore,
    },
    usecase::agent_phase::agent_phase_aggregation_rank,
};

use crate::usecase::{
    terminal_retention_ipc::SharedTerminalRetention,
    terminal_visibility_ipc::SharedTerminalVisibility,
};
use usagi_core::domain::terminal_visibility::VisibilityOutcome;

use super::{
    orchestration::{AdapterRegistry, OrchestrationError, Orchestrator, RuntimeAuthorization},
    runtime::{OutputJournal, ProviderResumeWrite, PtySpawner, RuntimeCoordinator, RuntimeError},
    terminal::{Geometry, InputRequest, PtyWriter, RegistryError},
    terminal_owner::{
        TerminalOwner as TerminalOwnerPort, TerminalRequestContext, TerminalResponse,
    },
};

/// A daemon-resolved, fully fenced checkout for an available scope (a managed
/// session or the workspace root).
///
/// It is produced only by the injected [`SessionScopeResolver`]; this crate
/// never re-derives it from a client supplied name or path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgentScope {
    pub worktree_id: WorktreeId,
    pub working_directory: PathBuf,
}

/// Typed, safe scope-resolution failure.  Raw lifecycle detail never crosses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeResolveError {
    /// The stable (workspace, session) identity is not the current available
    /// managed session (creating/deleting/failed/stale/mismatch).
    Unavailable,
    /// Durable lifecycle state could not be read.
    Storage,
}

/// Input port that converts a product-neutral launch scope into a fully fenced
/// available checkout. A
/// `Some` session resolves that managed session's worktree; a `None` session
/// resolves the trusted workspace root. Name/path/argv re-resolution is
/// intentionally impossible at this boundary.
pub trait SessionScopeResolver {
    fn resolve_available_scope(
        &self,
        workspace: WorkspaceId,
        session: Option<SessionId>,
    ) -> Result<ResolvedAgentScope, ScopeResolveError>;
}

/// The safe admission returned for a launched or replayed Agent operation.
///
/// `terminal` is the only reference a TUI pending pane may attach to, and it is
/// fully fenced to the operation's workspace/session/worktree, daemon
/// generation, and terminal incarnation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAdmission {
    pub operation_id: String,
    pub revision: u64,
    /// Exact daemon-owned runtime admitted for this operation. Presentation
    /// layers expose only its terminal, while supervisor composition uses the
    /// full fence to bind the root task without inventing provenance.
    pub runtime: AgentRuntimeRef,
    pub terminal: TerminalRef,
    /// Stable public lineage shared by the source and replacement. It is absent
    /// only when replaying a legacy durable record which predates exact resume.
    pub continuation: Option<AgentContinuationRef>,
    /// Explicit relation for a resume replacement; ordinary launches omit it.
    pub resume_relation: Option<AgentResumeRelation>,
    /// Present only after the daemon has observed and durably committed a
    /// successful process exit.  A replay therefore distinguishes an accepted
    /// running operation from its single final success without guessing a
    /// replacement terminal.
    pub completed: bool,
    /// Digest of the canonical semantic intent this operation was admitted for
    /// (#522).  A client correlates a final — direct or replayed — to its own
    /// pending operation only when this digest matches the one it computed for
    /// its request, so a reused identity can never promote another intent's
    /// terminal.  It is absent only for a legacy durable record admitted before
    /// the semantic key was persisted; such a record replays without a digest and
    /// the client refuses the final rather than guessing.
    pub semantic_digest: Option<String>,
}

/// Outcome of an authenticated worker report.
///
/// `accepted` distinguishes the first fenced delivery from an idempotent retry.
/// `committed` is always read back from the authoritative inbox, so projection
/// retries cannot introduce a different artifact from a duplicate request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportDelivery {
    pub delivered_to: CallerRef,
    pub worker: WorkerRef,
    pub accepted: bool,
    pub committed: Option<InboxMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    Queue,
    Live,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDelivery {
    pub delivered_to: &'static str,
    pub queued: bool,
}

/// One process-local Agent operation, replayed identically on resend/reconnect.
/// Only the semantic digest is kept here: the durable runtime/dispatch stores
/// own the canonical intent under their independent byte/count retention.
#[derive(Debug, Clone)]
struct AgentOperation {
    semantic_digest: Option<String>,
    outcome: Result<AgentAdmission, ProtocolError>,
    recorded_at: DateTime<Utc>,
}

impl AgentOperation {
    fn new(
        semantic_key: Option<&str>,
        outcome: Result<AgentAdmission, ProtocolError>,
        recorded_at: DateTime<Utc>,
    ) -> Self {
        Self {
            semantic_digest: semantic_key.map(agent_operation_digest),
            outcome,
            recorded_at,
        }
    }

    fn conflicts_with(&self, semantic_key: &str) -> bool {
        self.semantic_digest
            .as_ref()
            .is_some_and(|digest| digest != &agent_operation_digest(semantic_key))
    }

    fn matches(&self, semantic_key: &str) -> bool {
        self.semantic_digest.as_deref() == Some(agent_operation_digest(semantic_key).as_str())
    }

    fn retained_bytes(&self, operation_id: &str) -> usize {
        let semantic = self.semantic_digest.as_ref().map_or(0, String::capacity);
        let outcome = match &self.outcome {
            Ok(admission) => {
                std::mem::size_of::<AgentAdmission>()
                    + admission.operation_id.capacity()
                    + admission
                        .semantic_digest
                        .as_ref()
                        .map_or(0, String::capacity)
            }
            Err(error) => {
                std::mem::size_of::<ProtocolError>()
                    + error.message.capacity()
                    + error.error_id.capacity()
                    + error
                        .details
                        .as_ref()
                        .map_or(0, |details| details.to_string().len())
            }
        };
        std::mem::size_of::<Self>() + operation_id.len() + semantic + outcome
    }
}

#[derive(Debug, Clone, Copy)]
struct AgentOperationBounds {
    operations: usize,
    bytes: usize,
    age_seconds: i64,
}

impl Default for AgentOperationBounds {
    fn default() -> Self {
        Self {
            operations: 2_048,
            bytes: 2 * 1024 * 1024,
            age_seconds: 24 * 60 * 60,
        }
    }
}

#[derive(Debug, Clone)]
struct McpCaller {
    runtime: AgentRuntimeRef,
    operation: OperationId,
    child: Option<McpChildLease>,
}

/// Exact process and connection currently holding one daemon-minted MCP
/// credential. The process identity survives a transport disconnect so the
/// same child can reconnect; only the connection association is released.
#[derive(Debug, Clone, PartialEq, Eq)]
struct McpChildLease {
    pid: u32,
    process_start_identity: String,
    connection: Option<ConnectionId>,
}

/// Dispatch authority derived from one live daemon-minted MCP credential.
/// Workspace, run, and caller identity are resolved together so a connection
/// bound to another workspace cannot combine independently valid facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedDispatchCaller {
    pub workspace_id: WorkspaceId,
    pub run_id: OperationId,
    pub caller: CallerRef,
    pub runtime: AgentRuntimeRef,
    /// Exact terminal scope owned by the authenticated Agent runtime.
    ///
    /// Read-only tools use this daemon-derived fence instead of accepting a
    /// workspace, session, or worktree selector from the MCP caller.
    pub terminal_scope: TerminalLaunchScope,
}

/// Accepts both provider-inherited and provider-isolated MCP process groups.
///
/// Codex starts each managed child as its own process-group leader. The direct
/// parent fence and the one-shot caller slot still distinguish that child from
/// an unrelated process; requiring only the provider's process group would
/// reject the production MCP child before it can receive its credential.
fn mcp_child_process_group_matches(
    provider_process_group: u32,
    child_pid: u32,
    child_process_group: u32,
) -> bool {
    child_process_group == provider_process_group || child_process_group == child_pid
}

/// The routing decision for a terminal request that addresses a `TerminalRef`.
pub enum TerminalOutcome<T = TerminalResponse> {
    /// The Agent owner recognizes the terminal and produced this result.
    Handled(Result<T, ProtocolError>),
    /// The terminal is not an Agent terminal; the caller must try the generic
    /// terminal owner instead.
    NotOwned,
}

/// Terminal-stream surface for Agent terminals, kept behind a trait so a shared
/// owner can compose it with the generic terminal owner without duplicating the
/// ownership loop.
pub trait AgentTerminalActor {
    /// Handles one typed terminal request addressed to an Agent terminal.
    fn handle(
        &mut self,
        context: TerminalRequestContext,
        request: TerminalRequest,
    ) -> TerminalOutcome;
    /// Lists the Agent runtimes this actor holds in the exact requested scope.
    /// `SharedTerminalOwner` merges this with the generic terminal owner so a
    /// client's `Inventory` request sees Agent and generic terminals together.
    fn terminal_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry>;
    /// Lists the Agent runtime tombstones this actor holds in the exact
    /// requested scope (#525). `SharedTerminalOwner` merges this with the
    /// generic owner and stamps each entry's authoritative visibility.
    fn completed_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_visibility::CompletedTerminalEntry>;
    fn disconnect(&mut self, connection: ConnectionId);
}

/// The daemon's single Agent owner.  It holds the durable runtime coordinator,
/// orchestrator, adapter registry, runtime store, output journal, and PTY
/// spawner/writer, plus the producer-issued operation ledger for idempotency.
trait RuntimeStorePort: super::runtime::RuntimeStore + Send {
    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

impl<T: super::runtime::RuntimeStore + Send + 'static> RuntimeStorePort for T {
    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

trait OutputJournalPort: OutputJournal + Send {}
impl<T: OutputJournal + Send> OutputJournalPort for T {}

trait AgentPtyPort: PtySpawner + PtyWriter + Send {
    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any;
    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

impl<T: PtySpawner + PtyWriter + Send + 'static> AgentPtyPort for T {
    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

pub struct AgentRuntime {
    coordinator: RuntimeCoordinator,
    orchestrator: Orchestrator,
    registry: AdapterRegistry,
    store: Box<dyn RuntimeStorePort>,
    journal: Box<dyn OutputJournalPort>,
    pty: Box<dyn AgentPtyPort>,
    default_profile: AgentProfileId,
    geometry: Geometry,
    dispatch: DispatchStore,
    locator: Box<dyn ExecutableLocator>,
    operations: BTreeMap<String, AgentOperation>,
    operation_bounds: AgentOperationBounds,
    mcp_callers: BTreeMap<String, McpCaller>,
    /// Last phase each live runtime reported through its own lifecycle hook.
    /// Like the caller credentials, it is in-memory only: a phase report refines
    /// a live runtime's projection and must fail closed across daemon restart.
    reported_phases: BTreeMap<AgentRuntimeId, AgentPhase>,
}

/// A daemon-restart stop that failed, including the exact subset already
/// interrupted before the failure became observable.
#[derive(Debug)]
pub struct DaemonRestartInterruptionError {
    /// The failure returned to the requesting client.
    pub error: ProtocolError,
    /// Sources whose processes were already stopped and must be resumed.
    pub interrupted: DaemonRestartAgentPlan,
}

impl DaemonRestartInterruptionError {
    fn before_effect(error: ProtocolError) -> Self {
        Self {
            error,
            interrupted: DaemonRestartAgentPlan { agents: Vec::new() },
        }
    }
}

impl AgentRuntime {
    /// Session Workflow admission keeps its initial prompt inside the provider
    /// launch request, avoiding a race with a not-yet-ready interactive PTY.
    pub fn prepare_workflow_readiness(
        &self,
        operation_id: &str,
        intent: &AgentLaunchIntent,
        prompt: &str,
    ) -> Result<Option<AgentReadinessPreflight>, ProtocolError> {
        if intent.session.is_none()
            || prompt.trim().is_empty()
            || prompt.len() > 24 * 1024
            || prompt.contains('\0')
        {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "invalid session workflow launch",
            ));
        }
        let semantic = format!("workflow:{}:{prompt}", semantic_key(intent));
        if let Some(existing) = self.operations.get(operation_id) {
            if existing.conflicts_with(&semantic) {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "workflow launch identity conflicts",
                ));
            }
            return Ok(None);
        }
        OperationId::parse(operation_id).map_err(|_| dispatch_operation_id())?;
        self.readiness_ticket(
            intent
                .profile
                .clone()
                .unwrap_or_else(|| self.default_profile.clone()),
        )
        .map(Some)
    }

    /// Admit after an owner-external readiness probe and repeat its fences.
    pub fn launch_workflow_after_readiness(
        &mut self,
        operation_id: &str,
        intent: &AgentLaunchIntent,
        prompt: &str,
        scope: &dyn SessionScopeResolver,
        preflight: Option<&AgentReadinessPreflight>,
    ) -> Result<AgentAdmission, ProtocolError> {
        let current = self.prepare_workflow_readiness(operation_id, intent, prompt)?;
        self.validate_readiness(preflight, current.as_ref())?;
        if let Some(existing) = self.operations.get(operation_id) {
            return existing.outcome.clone();
        }
        if self
            .dispatch
            .agents_in_workspace(intent.workspace)
            .map_err(map_dispatch_storage_error)?
            .iter()
            .any(|worker| {
                worker.session_id == intent.session
                    && worker
                        .current_run
                        .is_some_and(|run| run.to_string() != operation_id)
                    && matches!(worker.status, AgentStatus::Starting | AgentStatus::Running)
            })
        {
            // `Busy`, not `Unavailable`: nothing was launched and resending the
            // same operation cannot succeed until the person stops that Agent.
            // `Unavailable` means "reconnect and retry the same operation",
            // which is what left a refused start wedged in the pane.
            return Err(ProtocolError::new(
                ErrorCode::Busy,
                "stop the session's existing Agent before starting a Workflow",
            ));
        }
        let semantic = format!("workflow:{}:{prompt}", semantic_key(intent));
        let outcome = self.admit(operation_id, intent, scope, Some(prompt), &semantic);
        self.remember_operation(operation_id, Some(&semantic), outcome.clone());
        outcome
    }

    fn forget_closed_runtimes(
        &mut self,
        closed: &[AgentRuntimeRef],
        owned_operations: Vec<(AgentRuntimeId, String)>,
    ) {
        let runtime_ids = closed
            .iter()
            .map(|runtime| runtime.agent_runtime_id)
            .collect::<BTreeSet<_>>();
        let operation_ids = owned_operations
            .into_iter()
            .filter(|(runtime, _)| runtime_ids.contains(runtime))
            .map(|(_, operation)| operation)
            .collect::<BTreeSet<_>>();
        self.operations
            .retain(|operation, _| !operation_ids.contains(operation));
        self.mcp_callers
            .retain(|_, caller| !runtime_ids.contains(&caller.runtime.agent_runtime_id));
        self.reported_phases
            .retain(|runtime, _| !runtime_ids.contains(runtime));
    }

    #[must_use]
    pub fn new(
        generation: DaemonGeneration,
        registry: AdapterRegistry,
        store: impl super::runtime::RuntimeStore + Send + 'static,
        journal: impl OutputJournal + Send + 'static,
        pty: impl PtySpawner + PtyWriter + Send + 'static,
        default_profile: AgentProfileId,
        geometry: Geometry,
    ) -> Self {
        Self::with_dispatch(
            generation,
            registry,
            store,
            journal,
            pty,
            default_profile,
            geometry,
            DispatchStore::new(
                std::env::temp_dir().join(format!("usagi-dispatch-{}", AgentRuntimeId::new())),
            ),
        )
    }

    #[must_use]
    pub fn with_dispatch(
        generation: DaemonGeneration,
        registry: AdapterRegistry,
        store: impl super::runtime::RuntimeStore + Send + 'static,
        journal: impl OutputJournal + Send + 'static,
        pty: impl PtySpawner + PtyWriter + Send + 'static,
        default_profile: AgentProfileId,
        geometry: Geometry,
        dispatch: DispatchStore,
    ) -> Self {
        Self::with_dispatch_and_locator(
            generation,
            registry,
            store,
            journal,
            pty,
            default_profile,
            geometry,
            dispatch,
            PathExecutableLocator,
        )
    }
}

/// How many Agent runtimes one daemon admits at a time.
///
/// It is also the Agent capacity pool's global limit: the pool is shared by every
/// retained generation and never implicitly summed with the generic terminal pool
/// ([`crate::usecase::resources::allocator::CapacityPolicy`]).
pub const AGENT_RUNTIME_LIMIT: usize = 16;

/// Immutable facts proved before an Agent readiness command runs outside the
/// owner lock. Admission compares them with current owner state after the probe;
/// the ticket is evidence of a completed check, never authority by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReadinessPreflight {
    profile: AgentProfileId,
    profile_revision: u32,
    generation: DaemonGeneration,
}

impl AgentReadinessPreflight {
    /// Provider token consumed by the shared readiness-command vocabulary.
    #[must_use]
    pub fn product(&self) -> &str {
        self.profile.as_str()
    }
}

impl AgentRuntime {
    /// Resolves the exact runtime family a Goal launch will use so Supervisor
    /// reservation can pin the same semantic fence before Agent admission.
    ///
    /// # Errors
    /// Returns an error when the Goal or selected profile is invalid.
    pub fn goal_worker_profile(
        &self,
        intent: &AgentGoalIntent,
    ) -> Result<AgentProfileId, ProtocolError> {
        validate_goal(intent)?;
        let profile = intent
            .profile
            .clone()
            .unwrap_or_else(|| self.default_profile.clone());
        self.readiness_ticket(profile).map(|ticket| ticket.profile)
    }

    /// Captures readiness facts for a dispatch-selected worker. The dispatch
    /// method still re-resolves the worker, configuration, executable, scope,
    /// and durable operation after the owner-external probe.
    pub fn prepare_dispatch_readiness(
        &self,
        operation_id: &str,
        intent: &DispatchIntent,
    ) -> Result<Option<AgentReadinessPreflight>, ProtocolError> {
        if self.operations.contains_key(operation_id) {
            return Ok(None);
        }
        OperationId::parse(operation_id).map_err(|_| dispatch_operation_id())?;
        let profile = match &intent.agent {
            DispatchAgentIntent::New { runtime, .. } => runtime.clone(),
            DispatchAgentIntent::Existing { agent_id } => {
                self.dispatch
                    .agent_in_workspace(intent.workspace, *agent_id)
                    .map_err(map_dispatch_storage_error)?
                    .ok_or_else(dispatch_agent_not_found)?
                    .runtime
            }
        };
        self.readiness_ticket(profile).map(Some)
    }

    /// Dispatch counterpart of [`Self::launch_after_readiness`].
    pub fn dispatch_after_readiness(
        &mut self,
        operation_id: &str,
        intent: &DispatchIntent,
        session: SessionId,
        scope: &dyn SessionScopeResolver,
        preflight: Option<&AgentReadinessPreflight>,
    ) -> Result<AgentAdmission, ProtocolError> {
        let current = self.prepare_dispatch_readiness(operation_id, intent)?;
        self.validate_readiness(preflight, current.as_ref())?;
        self.dispatch(operation_id, intent, session, scope)
    }

    /// Dispatches with the exact read-only worker plan already fenced by a
    /// Supervisor reservation. The worker is persisted atomically with Agent
    /// admission, after readiness and idempotency checks have passed.
    pub fn dispatch_planned_after_readiness(
        &mut self,
        operation_id: &str,
        intent: &DispatchIntent,
        session: SessionId,
        scope: &dyn SessionScopeResolver,
        preflight: Option<&AgentReadinessPreflight>,
        planned_worker: &usagi_core::domain::agent::Agent,
    ) -> Result<AgentAdmission, ProtocolError> {
        let current = self.prepare_dispatch_readiness(operation_id, intent)?;
        self.validate_readiness(preflight, current.as_ref())?;
        self.dispatch_with_planned_worker(
            operation_id,
            intent,
            session,
            scope,
            Some(planned_worker),
        )
    }

    fn readiness_ticket(
        &self,
        profile_id: AgentProfileId,
    ) -> Result<AgentReadinessPreflight, ProtocolError> {
        let profile = self
            .registry
            .profile(&profile_id)
            .map_err(|_| ProtocolError::new(ErrorCode::InvalidArgument, "unknown agent profile"))?;
        Ok(AgentReadinessPreflight {
            profile: profile.id,
            profile_revision: profile.revision,
            generation: self.active_generation()?,
        })
    }

    fn validate_readiness(
        &self,
        supplied: Option<&AgentReadinessPreflight>,
        current: Option<&AgentReadinessPreflight>,
    ) -> Result<(), ProtocolError> {
        // A concurrent identical admission turns the second caller into a
        // replay; it is safe without consuming its now-redundant ticket.
        if current.is_none() {
            return Ok(());
        }
        if supplied != current {
            return Err(ProtocolError::new(
                ErrorCode::RevisionConflict,
                "agent readiness preflight became stale",
            ));
        }
        let current = current.expect("checked above");
        if !self
            .locator
            .is_available(runtime_executable(current.profile.as_str()))
        {
            return Err(ProtocolError::new(
                ErrorCode::Unavailable,
                "agent CLI is unavailable or not authenticated; install it and sign in, then retry",
            ));
        }
        Ok(())
    }

    /// Constructs an Agent runtime with an injected current executable locator.
    ///
    /// # Panics
    ///
    /// Panics only if a newly allocated generation coordinator rejects its
    /// first production generation, which indicates an internal invariant bug.
    #[must_use]
    pub fn with_dispatch_and_locator(
        generation: DaemonGeneration,
        registry: AdapterRegistry,
        store: impl super::runtime::RuntimeStore + Send + 'static,
        journal: impl OutputJournal + Send + 'static,
        pty: impl PtySpawner + PtyWriter + Send + 'static,
        default_profile: AgentProfileId,
        geometry: Geometry,
        dispatch: DispatchStore,
        locator: impl ExecutableLocator + 'static,
    ) -> Self {
        let mut coordinator = RuntimeCoordinator::new(AGENT_RUNTIME_LIMIT, 64 * 1024, 64);
        coordinator
            .activate_generation(generation)
            .expect("a fresh Agent coordinator accepts its production generation");
        Self {
            coordinator,
            orchestrator: Orchestrator::new(),
            registry,
            store: Box::new(store),
            journal: Box::new(journal),
            pty: Box::new(pty),
            default_profile,
            geometry,
            dispatch,
            locator: Box::new(locator),
            operations: BTreeMap::new(),
            operation_bounds: AgentOperationBounds::default(),
            mcp_callers: BTreeMap::new(),
            reported_phases: BTreeMap::new(),
        }
    }

    /// Constructs the runtime only after a reconciled durable snapshot has
    /// been validated and loaded. No admission path is available on failure.
    pub fn hydrate_with_dispatch_and_locator(
        generation: DaemonGeneration,
        registry: AdapterRegistry,
        store: impl super::runtime::RuntimeStore + Send + 'static,
        journal: impl OutputJournal + Send + 'static,
        pty: impl PtySpawner + PtyWriter + Send + 'static,
        default_profile: AgentProfileId,
        geometry: Geometry,
        dispatch: DispatchStore,
        locator: impl ExecutableLocator + 'static,
        snapshot: super::runtime::RuntimeStoreSnapshot,
    ) -> Result<Self, super::runtime::RuntimeSnapshotError> {
        Self::hydrate_with_retention(
            generation,
            registry,
            store,
            journal,
            pty,
            default_profile,
            geometry,
            dispatch,
            locator,
            snapshot,
            SharedTerminalRetention::new(),
        )
    }

    /// Hydrates the owner bound to the daemon-wide retention authority, so
    /// Agent runtimes and generic terminals share one aggregate budget (#526).
    #[allow(clippy::too_many_arguments)]
    pub fn hydrate_with_retention(
        generation: DaemonGeneration,
        registry: AdapterRegistry,
        mut store: impl super::runtime::RuntimeStore + Send + 'static,
        journal: impl OutputJournal + Send + 'static,
        pty: impl PtySpawner + PtyWriter + Send + 'static,
        default_profile: AgentProfileId,
        geometry: Geometry,
        dispatch: DispatchStore,
        locator: impl ExecutableLocator + 'static,
        snapshot: super::runtime::RuntimeStoreSnapshot,
        retention: SharedTerminalRetention,
    ) -> Result<Self, super::runtime::RuntimeSnapshotError> {
        let mut coordinator = RuntimeCoordinator::hydrate_with_retention(
            snapshot,
            AGENT_RUNTIME_LIMIT,
            64 * 1024,
            64,
            retention,
        )?;
        coordinator.activate_generation(generation)?;
        store
            .save(coordinator.snapshot())
            .map_err(|()| super::runtime::RuntimeSnapshotError::OwnershipPersist)?;
        dispatch
            .reconcile_incomplete_admissions()
            .map_err(|_| super::runtime::RuntimeSnapshotError::DispatchReconcile)?;
        let recorded_at = Utc::now();
        let operations = coordinator
            .snapshot()
            .records
            .into_iter()
            .map(|record| {
                let operation_id = record.operation.operation_id.to_string();
                let outcome = durable_operation_outcome(&record);
                (
                    operation_id,
                    AgentOperation::new(record.semantic_key.as_deref(), outcome, recorded_at),
                )
            })
            .collect();
        Ok(Self {
            coordinator,
            orchestrator: Orchestrator::new(),
            registry,
            store: Box::new(store),
            journal: Box::new(journal),
            pty: Box::new(pty),
            default_profile,
            geometry,
            dispatch,
            locator: Box::new(locator),
            operations,
            operation_bounds: AgentOperationBounds::default(),
            // Credentials and reported phases intentionally fail closed across
            // daemon restart.
            mcp_callers: BTreeMap::new(),
            reported_phases: BTreeMap::new(),
        })
    }

    fn remember_operation(
        &mut self,
        operation_id: &str,
        semantic_key: Option<&str>,
        outcome: Result<AgentAdmission, ProtocolError>,
    ) {
        let now = Utc::now();
        self.operations.insert(
            operation_id.to_owned(),
            AgentOperation::new(semantic_key, outcome, now),
        );
        self.prune_operations(now);
    }

    fn prune_operations(&mut self, now: DateTime<Utc>) {
        let protected = self
            .coordinator
            .snapshot()
            .records
            .into_iter()
            .map(|record| record.operation.operation_id.to_string())
            .collect::<BTreeSet<_>>();
        let max_age_seconds = self.operation_bounds.age_seconds;
        self.operations.retain(|operation_id, operation| {
            protected.contains(operation_id)
                || now
                    .signed_duration_since(operation.recorded_at)
                    .num_seconds()
                    <= max_age_seconds
        });

        let mut retained_bytes = self
            .operations
            .iter()
            .map(|(operation_id, operation)| operation.retained_bytes(operation_id))
            .sum::<usize>();
        while self.operations.len() > self.operation_bounds.operations
            || retained_bytes > self.operation_bounds.bytes
        {
            let Some(oldest) = self
                .operations
                .iter()
                .filter(|(operation_id, _)| !protected.contains(*operation_id))
                .min_by(|(left_id, left), (right_id, right)| {
                    left.recorded_at
                        .cmp(&right.recorded_at)
                        .then_with(|| left_id.cmp(right_id))
                })
                .map(|(operation_id, _)| operation_id.clone())
            else {
                break;
            };
            if let Some(removed) = self.operations.remove(&oldest) {
                retained_bytes = retained_bytes.saturating_sub(removed.retained_bytes(&oldest));
            }
        }
    }

    /// Returns the durable outcome of a previously admitted operation, so a
    /// reconnecting client can replay the same accepted/final result.
    #[must_use]
    pub fn operation_outcome(
        &self,
        operation_id: &str,
    ) -> Option<Result<AgentAdmission, ProtocolError>> {
        self.operations
            .get(operation_id)
            .map(|operation| operation.outcome.clone())
    }

    /// Resolves the exact retained Agent runtime admitted by one durable
    /// operation. This internal join remains available after the process-local
    /// replay cache ages out. Durable hydration rejects duplicate ownership.
    #[must_use]
    pub fn runtime_for_operation(&self, operation_id: OperationId) -> Option<AgentRuntimeRef> {
        self.coordinator.runtime_for_operation(operation_id)
    }

    /// Observe one workflow participant through only the exact admitted runtime
    /// and its explicit resume chain, never a new launch that reused an Agent ID.
    #[must_use]
    pub fn workflow_live_operation(&self, operation: OperationId) -> Option<OperationId> {
        let operation = *self.workflow_operation_lineage(operation).last()?;
        self.coordinator
            .snapshot()
            .records
            .iter()
            .find(|record| {
                record.operation.operation_id == operation
                    && record.superseded_by.is_none()
                    && record.state == super::runtime::RuntimeState::Running
            })
            .map(|record| record.operation.operation_id)
    }

    /// Exact admitted operations, including exited ancestors and descendants.
    /// A new launch sharing an Agent ID is never part of this provenance.
    #[must_use]
    pub fn workflow_operation_lineage(&self, operation: OperationId) -> Vec<OperationId> {
        Self::workflow_lineage(&self.coordinator.snapshot(), operation)
    }

    fn workflow_lineage(
        snapshot: &super::runtime::RuntimeStoreSnapshot,
        operation: OperationId,
    ) -> Vec<OperationId> {
        let Some(mut record) = snapshot
            .records
            .iter()
            .find(|record| record.operation.operation_id == operation)
        else {
            return Vec::new();
        };
        let mut lineage = Vec::new();
        for _ in 0..=snapshot.records.len() {
            if lineage.contains(&record.operation.operation_id) {
                break;
            }
            lineage.push(record.operation.operation_id);
            if let Some(replacement) = record.superseded_by {
                let Some(next) = snapshot
                    .records
                    .iter()
                    .find(|candidate| candidate.runtime.agent_runtime_id == replacement)
                else {
                    break;
                };
                record = next;
            } else {
                break;
            }
        }
        lineage
    }

    #[must_use]
    pub fn dispatch_store(&self) -> &DispatchStore {
        &self.dispatch
    }

    /// Runtime reservations whose matching dispatch admission has already
    /// failed. They cannot be live tabs, but older failure paths may have left
    /// their pre-spawn durable record behind.
    pub fn failed_reservation_ids(&self) -> Result<Vec<AgentRuntimeId>, ProtocolError> {
        let failed = self.failed_dispatch_ids()?;
        Ok(self
            .coordinator
            .snapshot()
            .records
            .into_iter()
            .filter(|record| Self::is_failed_reservation(record, &failed))
            .map(|record| record.runtime.agent_runtime_id)
            .collect())
    }

    fn failed_dispatch_ids(&self) -> Result<BTreeSet<OperationId>, ProtocolError> {
        Ok(self
            .dispatch
            .runs()
            .map_err(map_dispatch_storage_error)?
            .into_iter()
            .filter(|run| run.status == RunStatus::Failed)
            .map(|run| run.run_id)
            .collect())
    }

    fn is_failed_reservation(
        record: &super::runtime::DurableRuntimeRecord,
        failed: &BTreeSet<OperationId>,
    ) -> bool {
        matches!(
            record.state,
            super::runtime::RuntimeState::Reserved
                | super::runtime::RuntimeState::ReconcileRequired(
                    super::runtime::ReconcileState::IdentityUnknown
                )
        ) && record.process.is_none()
            && failed.contains(&record.operation.operation_id)
    }

    /// Explicitly repairs every failed pre-spawn reservation selected by
    /// [`Self::failed_reservation_ids`].
    pub fn clean_failed_reservations(&mut self) -> Result<usize, ProtocolError> {
        let ids = self.failed_reservation_ids()?;
        let records = self.coordinator.snapshot().records;
        let mut cleaned = 0;
        for runtime in records
            .into_iter()
            .filter(|record| ids.contains(&record.runtime.agent_runtime_id))
            .map(|record| record.runtime)
        {
            cleaned += usize::from(
                self.coordinator
                    .clean_failed_launch(&runtime, &mut *self.store)
                    .map_err(map_runtime_error)?,
            );
        }
        Ok(cleaned)
    }

    fn active_generation(&self) -> Result<DaemonGeneration, ProtocolError> {
        self.coordinator.active_generation().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "agent generation ownership is unavailable",
            )
        })
    }

    /// Resolves an opaque MCP credential only while its exact runtime is live.
    #[must_use]
    pub fn mcp_caller(&self, credential: &str) -> Option<OperationId> {
        let caller = self.mcp_callers.get(credential)?;
        self.coordinator
            .record_for(&caller.runtime)
            .ok()
            .filter(|record| record.state == super::runtime::RuntimeState::Running)
            .map(|_| caller.operation)
    }

    /// Claims the one MCP child slot whose OS parent is the live Agent process
    /// and whose process group is inherited or self-led. The bearer crosses
    /// only this authenticated IPC response and is thereafter fenced to the
    /// claiming process-start identity and its renewable connection lease.
    pub fn claim_mcp_child(
        &mut self,
        child_pid: u32,
        process_start_identity: &str,
        parent_pid: u32,
        process_group: u32,
        connection: ConnectionId,
        existing_process_is_live: &dyn Fn(u32, &str) -> bool,
    ) -> Result<String, ProtocolError> {
        let mut matches = self.mcp_callers.iter_mut().filter(|(_, caller)| {
            caller.child.as_ref().is_none_or(|child| {
                (child.pid == child_pid && child.process_start_identity == process_start_identity)
                    || !existing_process_is_live(child.pid, &child.process_start_identity)
            }) && self
                .coordinator
                .record_for(&caller.runtime)
                .is_ok_and(|record| {
                    record.state == super::runtime::RuntimeState::Running
                        && record.process.as_ref().is_some_and(|process| {
                            process.pid == parent_pid
                                && mcp_child_process_group_matches(
                                    process.process_group,
                                    child_pid,
                                    process_group,
                                )
                        })
                })
        });
        let Some((credential, caller)) = matches.next() else {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "MCP child process does not belong to a live Agent runtime",
            ));
        };
        if matches.next().is_some() {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "MCP child process ownership is ambiguous",
            ));
        }
        caller.child = Some(McpChildLease {
            pid: child_pid,
            process_start_identity: process_start_identity.to_owned(),
            connection: Some(connection),
        });
        Ok(credential.clone())
    }

    #[must_use]
    pub fn authenticate_mcp_child_connection(
        &mut self,
        credential: &str,
        child_pid: u32,
        process_start_identity: &str,
        connection: ConnectionId,
    ) -> bool {
        if self.mcp_caller(credential).is_none() {
            return false;
        }
        let Some(child) = self
            .mcp_callers
            .get_mut(credential)
            .and_then(|caller| caller.child.as_mut())
        else {
            return false;
        };
        if child.pid != child_pid || child.process_start_identity != process_start_identity {
            return false;
        }
        child.connection = Some(connection);
        true
    }

    /// Releases only the active transport lease owned by this connection.
    ///
    /// The exact process claim remains so a policy client may reconnect with
    /// the same credential. Cleanup from an older connection cannot clear a
    /// newer connection lease, and a reused PID cannot acquire the claim.
    pub fn release_mcp_connection(&mut self, connection: ConnectionId) {
        for caller in self.mcp_callers.values_mut() {
            if caller
                .child
                .as_ref()
                .is_some_and(|child| child.connection == Some(connection))
                && let Some(child) = caller.child.as_mut()
            {
                child.connection = None;
            }
        }
    }

    /// Releases stale transport leases against the daemon's bounded live
    /// connection census while retaining each exact process claim.
    pub fn retain_live_mcp_connections(&mut self, live: &BTreeSet<ConnectionId>) {
        for caller in self.mcp_callers.values_mut() {
            if let Some(child) = caller.child.as_mut()
                && child
                    .connection
                    .is_some_and(|connection| !live.contains(&connection))
            {
                child.connection = None;
            }
        }
    }

    /// Coalesces terminal attachment and input-epoch cleanup against the
    /// daemon's bounded live-connection census.
    pub fn retain_live_connections(&mut self, live: &BTreeSet<ConnectionId>) {
        self.coordinator
            .retain_live_connections(live, &mut *self.pty);
    }

    /// Enforces Director Work's transitive provider/runtime invariant before
    /// session creation or Agent admission performs any side effect.
    pub fn require_same_dispatch_runtime(
        &self,
        workspace: WorkspaceId,
        caller: &CallerRef,
        selected: &DispatchAgentIntent,
    ) -> Result<(), ProtocolError> {
        let caller_runtime = self
            .dispatch
            .agent_in_workspace(workspace, caller.agent_id)
            .map_err(map_dispatch_storage_error)?
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "delegating Agent runtime is unavailable",
                )
            })?
            .runtime;
        let selected_runtime = match selected {
            DispatchAgentIntent::New { runtime, .. } => runtime.clone(),
            DispatchAgentIntent::Existing { agent_id } => {
                self.dispatch
                    .agent_in_workspace(workspace, *agent_id)
                    .map_err(map_dispatch_storage_error)?
                    .ok_or_else(dispatch_agent_not_found)?
                    .runtime
            }
        };
        if selected_runtime != caller_runtime {
            return Err(ProtocolError::new(
                ErrorCode::PermissionDenied,
                "delegated Agent runtime must match the authenticated caller runtime",
            ));
        }
        Ok(())
    }

    /// Resolves a short-lived provider hook from authenticated OS process identity.
    ///
    /// Claude uses exec-form hooks, so the hook is a direct child of the live
    /// provider and may either inherit its process group or lead its own.
    /// The inherited-group case remains accepted for providers/configurations
    /// which still use a shell-form hook. Hooks receive no bearer and cannot
    /// acquire the MCP child's dispatch scope.
    #[must_use]
    pub fn hook_credential(
        &self,
        hook_pid: u32,
        parent_pid: u32,
        process_group: u32,
    ) -> Option<&str> {
        let mut matches = self.mcp_callers.iter().filter(|(_, caller)| {
            self.coordinator
                .record_for(&caller.runtime)
                .is_ok_and(|record| {
                    record.state == super::runtime::RuntimeState::Running
                        && record.process.as_ref().is_some_and(|process| {
                            process.process_group == process_group
                                || (process.pid == parent_pid
                                    && mcp_child_process_group_matches(
                                        process.process_group,
                                        hook_pid,
                                        process_group,
                                    ))
                        })
                })
        });
        let (credential, _) = matches.next()?;
        matches.next().is_none().then_some(credential.as_str())
    }

    /// Resolves the durable dispatch identity authenticated by an MCP child.
    /// The credential is daemon-minted after an OS-authenticated child claim; no client supplied
    /// agent or session name participates in this lookup.
    #[must_use]
    pub fn mcp_dispatch_caller(&self, credential: &str) -> Option<CallerRef> {
        self.mcp_dispatch_context(credential)
            .map(|context| context.caller)
    }

    /// Resolves all dispatch provenance from the same live credential and
    /// verifies the durable Agent ownership sidecar before returning it.
    #[must_use]
    pub fn mcp_dispatch_context(&self, credential: &str) -> Option<AuthenticatedDispatchCaller> {
        let mcp = self.mcp_callers.get(credential)?;
        let record = self
            .coordinator
            .record_for(&mcp.runtime)
            .ok()
            .filter(|record| record.state == super::runtime::RuntimeState::Running)?;
        let workspace_id = record.runtime.terminal.workspace_id;
        let run_id = mcp.operation;
        let binding = self.dispatch.binding(run_id).ok()??;
        let caller = CallerRef {
            session_id: binding.worker.session_id,
            agent_id: binding.worker.agent_id,
        };
        let terminal_scope = TerminalLaunchScope {
            workspace_id,
            session_id: record.runtime.session_id,
            worktree_id: record.runtime.terminal.worktree_id,
        };
        (self.dispatch.workspace_for_agent(caller.agent_id).ok()?? == workspace_id).then_some(
            AuthenticatedDispatchCaller {
                workspace_id,
                run_id,
                caller,
                runtime: mcp.runtime.clone(),
                terminal_scope,
            },
        )
    }
}

impl AgentRuntime {
    /// Sleeps every quiescent, exactly resumable Agent in one managed session.
    /// The session and worktree are never removed. If any live runtime in the
    /// session is running work or lacks resume metadata, the whole request is
    /// refused before signalling a process.
    pub fn sleep_session(&mut self, session: SessionId) -> Result<usize, ProtocolError> {
        let records = self.coordinator.snapshot().records;
        let live = records
            .iter()
            .filter(|record| {
                record.runtime.session_id == Some(session)
                    && record.state == super::runtime::RuntimeState::Running
            })
            .collect::<Vec<_>>();
        if live.is_empty() {
            return Err(ProtocolError::new(
                ErrorCode::Unavailable,
                "session has no live Agent to sleep",
            ));
        }
        if live.iter().any(|record| {
            !matches!(
                self.reported_phases.get(&record.runtime.agent_runtime_id),
                Some(AgentPhase::Ready | AgentPhase::Ended)
            ) || !self.resume_source_availability(record, &records).0
        }) {
            return Err(ProtocolError::new(
                ErrorCode::Busy,
                "session Agent is busy or has no exact provider resume metadata",
            ));
        }
        let runtime_ids = live
            .iter()
            .map(|record| record.runtime.agent_runtime_id.as_str())
            .collect::<BTreeSet<_>>();
        self.sleep_runtime_ids(&runtime_ids)
    }

    fn sleep_runtime_ids(
        &mut self,
        runtime_ids: &BTreeSet<String>,
    ) -> Result<usize, ProtocolError> {
        let slept = self
            .coordinator
            .sleep_agents(runtime_ids, &mut *self.store, &mut *self.pty)
            .map_err(map_runtime_error)?;
        self.mcp_callers
            .retain(|_, caller| !runtime_ids.contains(&caller.runtime.agent_runtime_id.as_str()));
        self.reported_phases
            .retain(|runtime, _| !runtime_ids.contains(&runtime.as_str()));
        Ok(slept)
    }

    /// Resolves an authenticated MCP child to its owning managed session.
    #[must_use]
    pub fn caller_session(&self, credential: &str) -> Option<SessionId> {
        let caller = self.mcp_callers.get(credential)?;
        self.coordinator
            .record_for(&caller.runtime)
            .ok()
            .filter(|record| record.state == super::runtime::RuntimeState::Running)
            .and_then(|record| record.runtime.session_id)
    }

    /// Number of process-local MCP caller credentials this daemon has minted.
    ///
    /// An unclaimed credential still belongs to a live Agent and would become
    /// unusable after handing control authority to another process.
    #[must_use]
    pub fn provisioned_mcp_callers(&self) -> usize {
        self.mcp_callers.len()
    }

    /// Returns the runtime phase projected for one session.
    ///
    /// The daemon-observed [`RuntimeState`](super::runtime::RuntimeState) is the
    /// authority: a phase an agent reported through its own lifecycle hook
    /// refines the projection only while that exact runtime is still `Running`.
    /// A report therefore never makes a reserved, interrupted, or exited runtime
    /// look alive, and never hides an interruption.
    #[must_use]
    pub fn session_phase(&self, session: SessionId) -> AgentPhase {
        let failed = self.failed_dispatch_ids().unwrap_or_default();
        self.coordinator
            .snapshot()
            .records
            .into_iter()
            .filter(|record| record.runtime.session_id == Some(session))
            .map(|record| self.record_phase(&record, &failed))
            .max_by_key(|(priority, _)| *priority)
            .map_or(AgentPhase::Absent, |(_, phase)| phase)
    }

    fn record_phase(
        &self,
        record: &super::runtime::DurableRuntimeRecord,
        failed: &BTreeSet<OperationId>,
    ) -> (u8, AgentPhase) {
        if Self::is_failed_reservation(record, failed) {
            return runtime_phase(super::runtime::RuntimeState::SpawnFailed);
        }
        if record.state == super::runtime::RuntimeState::Running
            && let Some(phase) = self.reported_phases.get(&record.runtime.agent_runtime_id)
        {
            return reported_phase(*phase);
        }
        runtime_phase(record.state)
    }

    /// Whether this workspace has an Agent runtime that is live or whose
    /// process ownership has not been proved safe to release.
    ///
    /// A retirement asks this before giving the workspace back: its PTY children
    /// belong to that workspace's scopes, and releasing the workspace while one
    /// is alive would hand its worktrees to a second owner.
    #[must_use]
    pub fn has_running_agent(&self, workspace: WorkspaceId) -> bool {
        self.retirement_blocker_count(workspace) != 0
    }

    /// Number of Agent records which may still own a PTY process.
    #[must_use]
    pub fn retirement_blocker_count(&self, workspace: WorkspaceId) -> usize {
        self.coordinator
            .snapshot()
            .records
            .iter()
            .filter(|record| {
                record.runtime.terminal.workspace_id == workspace
                    && matches!(
                        record.state,
                        super::runtime::RuntimeState::Running
                            | super::runtime::RuntimeState::ReconcileRequired(
                                super::runtime::ReconcileState::OrphanRunning
                                    | super::runtime::ReconcileState::SpawnAmbiguous
                                    | super::runtime::ReconcileState::PersistAfterSpawn
                            )
                    )
            })
            .count()
    }

    /// Diagnoses launch-time hook/MCP integration revisions without exposing
    /// rendered configuration or provider-native identities.
    pub fn diagnose_integrations(
        &self,
        workspace: WorkspaceId,
        expected: &[AgentIntegrationRevision],
    ) -> Result<AgentIntegrationDiagnosis, ProtocolError> {
        let expected = expected_integration_revisions(expected)?;
        let records = self.coordinator.snapshot().records;
        let failed = self.failed_dispatch_ids().unwrap_or_default();
        let mut outdated = records
            .iter()
            .filter(|record| record.runtime.terminal.workspace_id == workspace)
            .filter(|record| {
                integration_diagnosable_state(record.state) && record.superseded_by.is_none()
            })
            .filter_map(|record| {
                let expected_revision = expected.get(record.launch.plan.profile_id.as_str())?;
                (record.launch.plan.profile_revision < *expected_revision).then(|| {
                    let resume_available = Self::repair_source_availability(record, &records).0;
                    OutdatedAgentRuntime {
                        runtime: record.runtime.clone(),
                        continuation: record.continuation,
                        profile_id: record.launch.plan.profile_id.clone(),
                        actual_revision: record.launch.plan.profile_revision,
                        expected_revision: *expected_revision,
                        state: runtime_inventory_state(record.state),
                        phase: if matches!(
                            record.state,
                            super::runtime::RuntimeState::Reserved
                                | super::runtime::RuntimeState::Running
                        ) {
                            self.record_phase(record, &failed).1
                        } else {
                            runtime_phase(record.state).1
                        },
                        resume_available,
                    }
                })
            })
            .collect::<Vec<_>>();
        outdated.sort_by_key(|item| item.runtime.agent_runtime_id.as_str());
        let outdated_mcp_children = self
            .mcp_callers
            .values()
            .filter(|caller| caller.child.is_some())
            .filter(|caller| {
                outdated
                    .iter()
                    .any(|item| item.runtime.agent_runtime_id == caller.runtime.agent_runtime_id)
            })
            .count();
        Ok(AgentIntegrationDiagnosis {
            workspace_id: workspace,
            outdated,
            outdated_mcp_children,
            provisioned_mcp_callers: Some(self.mcp_callers.len()),
        })
    }

    /// Stops only outdated integrations selected by a diagnosis against the
    /// invoking binary. `Running` is fail-closed because it may be inside a
    /// provider tool call; `--force` is the explicit authority to discard it.
    pub fn interrupt_outdated_agents(
        &mut self,
        workspace: WorkspaceId,
        expected: &[AgentIntegrationRevision],
        selected: &[AgentRuntimeRef],
        force: bool,
    ) -> Result<(usize, AgentIntegrationDiagnosis), ProtocolError> {
        let mut diagnosis = self.diagnose_integrations(workspace, expected)?;
        let selected_ids = selected
            .iter()
            .map(|runtime| runtime.agent_runtime_id)
            .collect::<BTreeSet<_>>();
        if selected_ids.len() != selected.len()
            || selected.iter().any(|runtime| {
                !diagnosis
                    .outdated
                    .iter()
                    .any(|candidate| candidate.runtime == *runtime)
            })
        {
            return Err(ProtocolError::new(
                ErrorCode::StaleTarget,
                "outdated Agent selection changed after diagnosis; no Agent was stopped",
            ));
        }
        diagnosis
            .outdated
            .retain(|runtime| selected_ids.contains(&runtime.runtime.agent_runtime_id));
        diagnosis.outdated_mcp_children = self
            .mcp_callers
            .values()
            .filter(|caller| caller.child.is_some())
            .filter(|caller| selected_ids.contains(&caller.runtime.agent_runtime_id))
            .count();
        if diagnosis
            .outdated
            .iter()
            .any(|runtime| !runtime.resume_available)
        {
            return Err(ProtocolError::new(
                ErrorCode::Busy,
                "an outdated Agent has no exact provider resume metadata; no Agent was stopped",
            ));
        }
        if !force
            && diagnosis
                .outdated
                .iter()
                .any(|runtime| runtime.phase == AgentPhase::Running)
        {
            return Err(ProtocolError::new(
                ErrorCode::Busy,
                "an outdated Agent is running a prompt or tool; retry with --force to discard it",
            ));
        }
        let runtime_ids = diagnosis
            .outdated
            .iter()
            .map(|item| item.runtime.agent_runtime_id.as_str())
            .collect();
        let result = self
            .coordinator
            .interrupt_agents(&runtime_ids, &mut *self.store, &mut *self.pty)
            .map_err(map_runtime_error)?;
        self.mcp_callers
            .retain(|_, caller| !runtime_ids.contains(&caller.runtime.agent_runtime_id.as_str()));
        self.reported_phases
            .retain(|runtime, _| !runtime_ids.contains(&runtime.as_str()));
        Ok((result, diagnosis))
    }

    /// Stops only the Agent runtimes fenced by durable Supervisor provenance.
    ///
    /// Every selected runtime is validated before any PTY is signalled. A
    /// recycled or corrupt Agent identity therefore fails the whole pass
    /// closed instead of falling back to another runtime in the workspace.
    /// Missing or already-exited records are converged no-ops, which makes the
    /// operation safe for daemon-startup and periodic recovery.
    pub fn interrupt_supervisor_workers(
        &mut self,
        workspace: WorkspaceId,
        provenance: &[RunProvenance],
    ) -> Result<usize, ProtocolError> {
        let mut expected = BTreeMap::new();
        for worker in provenance {
            let scope = (worker.worker_session_id, worker.worker_worktree_id);
            if expected
                .insert(worker.worker_agent_id, scope)
                .is_some_and(|existing| existing != scope)
            {
                return Err(ProtocolError::new(
                    ErrorCode::StaleTarget,
                    "supervisor worker provenance conflicts for one Agent runtime",
                ));
            }
        }

        let records = self.coordinator.snapshot().records;
        let mut runtime_ids = BTreeSet::new();
        for (runtime_id, (session_id, worktree_id)) in expected {
            let Some(record) = records
                .iter()
                .find(|record| record.runtime.agent_runtime_id == runtime_id)
            else {
                continue;
            };
            if record.runtime.terminal.workspace_id != workspace
                || record.runtime.session_id != session_id
                || record.runtime.terminal.session_id != session_id
                || record.runtime.terminal.worktree_id != worktree_id
            {
                return Err(ProtocolError::new(
                    ErrorCode::StaleTarget,
                    "supervisor worker provenance no longer fences its Agent runtime",
                ));
            }
            match record.state {
                super::runtime::RuntimeState::Reserved
                | super::runtime::RuntimeState::Running
                | super::runtime::RuntimeState::ReconcileRequired(
                    super::runtime::ReconcileState::SpawnAmbiguous
                    | super::runtime::ReconcileState::PersistAfterSpawn
                    | super::runtime::ReconcileState::OrphanRunning,
                ) => {
                    runtime_ids.insert(runtime_id.as_str().clone());
                }
                super::runtime::RuntimeState::Interrupted
                | super::runtime::RuntimeState::Sleeping
                | super::runtime::RuntimeState::Exited
                | super::runtime::RuntimeState::SpawnFailed
                | super::runtime::RuntimeState::Reclaimed => {}
                super::runtime::RuntimeState::ReconcileRequired(_) => {
                    return Err(ProtocolError::new(
                        ErrorCode::OwnershipUnknown,
                        "supervisor worker process ownership requires reconciliation",
                    ));
                }
            }
        }

        if runtime_ids.is_empty() {
            return Ok(0);
        }
        let interrupted = self
            .coordinator
            .interrupt_agents(&runtime_ids, &mut *self.store, &mut *self.pty)
            .map_err(map_runtime_error)?;
        self.mcp_callers
            .retain(|_, caller| !runtime_ids.contains(&caller.runtime.agent_runtime_id.as_str()));
        self.reported_phases
            .retain(|runtime, _| !runtime_ids.contains(&runtime.as_str()));
        Ok(interrupted)
    }

    /// Returns one deterministic, secret-free inventory for workspace-root and
    /// managed-session Agent runtimes.
    #[must_use]
    pub fn inventory(&self, workspace: WorkspaceId) -> AgentInventory {
        let failed = self.failed_dispatch_ids().unwrap_or_default();
        let mut records = self
            .coordinator
            .snapshot()
            .records
            .into_iter()
            .filter(|record| record.runtime.terminal.workspace_id == workspace)
            .collect::<Vec<_>>();
        records.sort_by_key(|record| {
            (
                record.operation.operation_id.to_string(),
                record.runtime.agent_runtime_id.as_str(),
            )
        });
        let runtimes = records
            .iter()
            .filter_map(|record| {
                record
                    .continuation
                    .map(|continuation| AgentRuntimeInventoryItem {
                        runtime: record.runtime.clone(),
                        continuation,
                        state: if Self::is_failed_reservation(record, &failed) {
                            AgentRuntimeInventoryState::Unavailable
                        } else {
                            runtime_inventory_state(record.state)
                        },
                        resumed_from: record.resumed_from,
                    })
            })
            .collect();
        let resumable = records
            .iter()
            .filter(|record| is_resume_source_state(record.state))
            .map(|record| {
                let target = resume_target(record);
                let (available, reason) = self.resume_source_availability(record, &records);
                // Only the closed provider/phase vocabulary is projected. The
                // provider-native ID stays inside the durable record.
                AgentResumableInventoryItem {
                    runtime_id: record.runtime.agent_runtime_id,
                    target,
                    available,
                    reason,
                    provider: record
                        .provider_resume
                        .as_ref()
                        .map(|reference| reference.provider),
                    last_known_phase: record
                        .provider_resume
                        .as_ref()
                        .and_then(|reference| reference.last_known_phase),
                }
            })
            .collect();
        AgentInventory {
            workspace_id: workspace,
            runtimes,
            resumable,
        }
    }

    /// Returns one cross-project observation with both runtime detail and the
    /// dispatch terminal state used by `session list`. Multiple dispatch Agents
    /// in one session use the same deterministic status precedence as that list.
    pub fn workspace_observation(
        &self,
        workspace: WorkspaceId,
    ) -> Result<AgentWorkspaceObservation, ProtocolError> {
        let mut selected = BTreeMap::new();
        for agent in self
            .dispatch
            .agents_in_workspace(workspace)
            .map_err(map_dispatch_storage_error)?
        {
            let Some(session) = agent.session_id else {
                continue;
            };
            selected
                .entry(session)
                .and_modify(|current: &mut AgentStatus| {
                    *current = dominant_agent_status(*current, agent.status);
                })
                .or_insert(agent.status);
        }
        Ok(AgentWorkspaceObservation {
            inventory: self.inventory(workspace),
            session_statuses: selected,
        })
    }

    /// Resolves one daemon-issued process credential to its exact live Codex
    /// runtime, then forwards the documented `SessionStart` session ID to the
    /// product-neutral structured capture boundary.
    pub fn capture_codex_session(
        &mut self,
        credential: &str,
        native_session_id: ProviderSessionId,
    ) -> Result<(), ProtocolError> {
        let caller = self.mcp_callers.get(credential).cloned().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "Codex runtime credential is unknown",
            )
        })?;
        if self.mcp_caller(credential).is_none() {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "Codex runtime credential is not live",
            ));
        }
        self.capture_structured_provider_session(
            &caller.runtime,
            ProviderKind::Codex,
            native_session_id,
        )
    }

    /// Accepts only a provider session ID delivered by a documented structured
    /// adapter channel. No filesystem or transcript discovery exists at this
    /// boundary; absence of such a call leaves Codex resume unavailable.
    pub fn capture_structured_provider_session(
        &mut self,
        runtime: &AgentRuntimeRef,
        provider: ProviderKind,
        native_session_id: ProviderSessionId,
    ) -> Result<(), ProtocolError> {
        let record = self
            .coordinator
            .record_for(runtime)
            .map_err(map_runtime_error)?;
        if !provider_matches_profile(provider, &record.launch.plan.profile_id) {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "provider session metadata does not match the runtime profile",
            ));
        }
        // An already-running Codex can still have the pre-v5 dedicated capture
        // hook while the `usagi` executable has been updated in place. The new
        // combined SessionStart hook and that legacy hook may therefore report
        // the same ID in either order. Treat the second report as an idempotent
        // compatibility call instead of letting its older Running projection
        // conflict with the combined hook's Starting projection.
        if record.provider_resume.as_ref().is_some_and(|existing| {
            existing.provider == provider
                && existing.native_session_id == native_session_id
                && existing.adapter_revision == record.launch.plan.profile_revision
                && existing.scope == record.launch.request.scope
                && existing.provenance == ProviderCaptureProvenance::ProviderStructured
        }) {
            return Ok(());
        }
        let reference = ProviderResumeRef {
            provider,
            native_session_id,
            adapter_revision: record.launch.plan.profile_revision,
            scope: record.launch.request.scope.clone(),
            provenance: ProviderCaptureProvenance::ProviderStructured,
            last_known_status: ProviderResumeStatus::Active,
            last_known_phase: Some(ProviderResumePhase::Running),
        };
        self.coordinator
            .write_provider_resume(
                runtime,
                reference,
                ProviderResumeWrite::Attach,
                &mut *self.store,
            )
            .map_err(map_runtime_error)
    }

    /// Refreshes the current interactive conversation from the exact runtime's
    /// documented structured starting hook. Headless runs still report phase
    /// but never become resumable conversations.
    fn capture_provider_session_start(
        &mut self,
        runtime: &AgentRuntimeRef,
        native_session_id: ProviderSessionId,
        phase: ProviderResumePhase,
    ) -> Result<bool, ProtocolError> {
        let record = self
            .coordinator
            .record_for(runtime)
            .map_err(map_runtime_error)?;
        if record.launch.request.mode != LaunchMode::Interactive {
            return Ok(false);
        }
        let provider = provider_for_profile(&record.launch.plan.profile_id).ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "runtime profile does not support provider session capture",
            )
        })?;
        let reference = ProviderResumeRef {
            provider,
            native_session_id,
            adapter_revision: record.launch.plan.profile_revision,
            scope: record.launch.request.scope.clone(),
            provenance: ProviderCaptureProvenance::ProviderStructured,
            last_known_status: ProviderResumeStatus::Active,
            last_known_phase: Some(phase),
        };
        self.coordinator
            .write_provider_resume(
                runtime,
                reference,
                ProviderResumeWrite::Replace,
                &mut *self.store,
            )
            .map_err(map_runtime_error)?;
        Ok(true)
    }

    fn repair_source_availability(
        record: &super::runtime::DurableRuntimeRecord,
        records: &[super::runtime::DurableRuntimeRecord],
    ) -> (bool, ProviderResumeReason) {
        let (Some(continuation), Some(_source), Some(reference)) = (
            record.continuation,
            record.resume_source,
            record.provider_resume.as_ref(),
        ) else {
            return (false, ProviderResumeReason::ProviderMetadataUnavailable);
        };
        if record.superseded_by.is_some() {
            return (false, ProviderResumeReason::SourceAlreadySuperseded);
        }
        if records.iter().any(|candidate| {
            candidate.runtime.agent_runtime_id != record.runtime.agent_runtime_id
                && candidate.continuation == Some(continuation)
                && holds_live_or_unknown_agent(candidate.state)
        }) {
            return (false, ProviderResumeReason::LiveOrOwnershipUnknown);
        }
        let capture_compatible = matches!(
            (reference.provider, reference.provenance),
            (
                ProviderKind::Claude,
                ProviderCaptureProvenance::DaemonIssued
                    | ProviderCaptureProvenance::ProviderStructured
            ) | (
                ProviderKind::Codex | ProviderKind::Agy,
                ProviderCaptureProvenance::ProviderStructured
            )
        );
        let internally_compatible = capture_compatible
            && record.launch.plan.profile_revision == reference.adapter_revision
            && record.launch.request.scope == reference.scope
            && record.runtime.terminal.workspace_id == reference.scope.workspace_id
            && record.runtime.terminal.session_id == reference.scope.session_id
            && record.runtime.terminal.worktree_id == reference.scope.worktree_id
            && provider_matches_profile(reference.provider, &record.launch.plan.profile_id);
        if internally_compatible {
            (true, ProviderResumeReason::ExplicitResumeAvailable)
        } else {
            (false, ProviderResumeReason::IncompatibleProviderMetadata)
        }
    }

    /// Journals daemon-owned PTY output before it becomes replayable.  A stale
    /// terminal is a safe no-op error, never a replacement.
    pub fn output(&mut self, terminal: &TerminalRef, bytes: Vec<u8>) -> Result<(), ProtocolError> {
        let runtime = self
            .coordinator
            .runtime_for_terminal(terminal)
            .ok_or_else(stale_terminal)?;
        let (_, replies) = self
            .coordinator
            .append_output_with_replies(&runtime, bytes, &mut *self.journal)
            .map_err(map_runtime_error)?;
        if !replies.is_empty() {
            self.pty.select_terminal(terminal);
            // Output has already committed. A lost terminal reply must not
            // recast that accepted output as a failed observer event.
            let _ = self.pty.write_all(&replies);
        }
        Ok(())
    }

    /// Commits a verified Agent exit after the caller has drained output.
    ///
    /// # Panics
    ///
    /// Panics only if the internal admission ledger invariant is broken: every
    /// launched runtime must retain its operation record until exit.
    pub fn exit(&mut self, terminal: &TerminalRef, status: i32) -> Result<(), ProtocolError> {
        let runtime = self
            .coordinator
            .runtime_for_terminal(terminal)
            .ok_or_else(stale_terminal)?;
        let result = self.coordinator.exit(&runtime, status, &mut *self.store);
        if matches!(
            result,
            Ok(())
                | Err(RuntimeError::ReconcileRequired(
                    super::runtime::ReconcileState::PersistAfterExit
                ))
        ) {
            self.pty.release(terminal);
        }
        result.map_err(map_runtime_error)?;

        // The operation ledger is the only authority for replay.  Update it
        // after the terminal registry and durable runtime record have accepted
        // the exit, so duplicate observer notifications cannot create a second
        // completion.  Non-zero exits deliberately replay a safe failure;
        // neither status text nor private CLI output crosses this boundary.
        let operation = self
            .coordinator
            .record_for(&runtime)
            .map_err(map_runtime_error)?
            .operation
            .operation_id
            .as_str()
            .clone();
        let record = self
            .operations
            .get_mut(&operation)
            .expect("runtime exits retain their admitted operation ledger");
        record.outcome = match &record.outcome {
            Ok(admission) if status == 0 => {
                let mut final_admission = admission.clone();
                final_admission.completed = true;
                Ok(final_admission)
            }
            Ok(_) => Err(ProtocolError::new(
                ErrorCode::Unavailable,
                "agent process ended unsuccessfully; inspect the attached terminal output",
            )),
            Err(error) => Err(error.clone()),
        };
        self.synthesize_no_report(&runtime)?;
        self.mcp_callers
            .retain(|_, caller| caller.runtime.agent_runtime_id != runtime.agent_runtime_id);
        self.reported_phases.remove(&runtime.agent_runtime_id);
        self.prune_operations(Utc::now());
        Ok(())
    }
}

impl AgentRuntime {
    /// Runs one bounded retention collection pass and applies its decisions to
    /// this owner's records and journals. The composition root drives it
    /// periodically so an idle daemon still ages its finals out of the budget.
    pub fn collect_retention_garbage(&mut self) -> usize {
        self.coordinator.retention().collect();
        let collected = self.coordinator.collect_garbage(&mut *self.store);
        self.prune_operations(Utc::now());
        collected
    }

    /// Closes every Agent runtime belonging to a managed session.
    ///
    /// This is daemon-internal teardown, not a client-selected terminal kill:
    /// the session lifecycle supplies the stable [`SessionId`], and each live
    /// process is terminated only through its daemon-owned fenced terminal.
    pub fn close_session(&mut self, session: SessionId) -> Result<usize, ProtocolError> {
        let owned_operations = self
            .coordinator
            .snapshot()
            .records
            .into_iter()
            .filter(|record| record.runtime.session_id == Some(session))
            .map(|record| {
                (
                    record.runtime.agent_runtime_id,
                    record.operation.operation_id.to_string(),
                )
            })
            .collect::<Vec<_>>();
        let closed = self
            .coordinator
            .close_session(session, &mut *self.store, &mut *self.pty)
            .map_err(map_runtime_error)?;
        self.forget_closed_runtimes(&closed, owned_operations);
        Ok(closed.len())
    }

    /// Closes every Agent runtime belonging to one retiring workspace.
    pub fn close_workspace(&mut self, workspace: WorkspaceId) -> Result<usize, ProtocolError> {
        let owned_operations = self
            .coordinator
            .snapshot()
            .records
            .into_iter()
            .filter(|record| record.runtime.terminal.workspace_id == workspace)
            .map(|record| {
                (
                    record.runtime.agent_runtime_id,
                    record.operation.operation_id.to_string(),
                )
            })
            .collect::<Vec<_>>();
        let closed = self
            .coordinator
            .close_workspace(workspace, &mut *self.store, &mut *self.pty)
            .map_err(map_runtime_error)?;
        self.forget_closed_runtimes(&closed, owned_operations);
        Ok(closed.len())
    }

    /// Managed-session identities currently retained by the Agent owner.
    #[must_use]
    pub fn managed_session_ids(&self) -> std::collections::BTreeSet<SessionId> {
        self.coordinator
            .snapshot()
            .records
            .into_iter()
            .filter_map(|record| record.runtime.session_id)
            .collect()
    }

    /// The resource ids this owner still answers for.
    ///
    /// Durable state of a generation that is gone may only be collected once
    /// nothing retains its records any more, and this is the live half of that
    /// question ([`crate::usecase::resources::durable::ShardedRuntimeState::collect`]).
    #[must_use]
    pub fn retained_resources(&self) -> std::collections::BTreeSet<String> {
        self.coordinator
            .snapshot()
            .records
            .iter()
            .map(|record| record.runtime.terminal.terminal_id.as_str())
            .collect()
    }

    /// Publishes this owner's Agent concurrency level into `gauge`.
    ///
    /// The composition root binds the gauge the metrics broker reads, so a
    /// display-only observer never takes this runtime's lock to learn how much of
    /// the [`AGENT_RUNTIME_LIMIT`] pool is in use.
    pub fn bind_concurrency_gauge(
        &mut self,
        gauge: crate::usecase::metrics::AgentConcurrencyGauge,
    ) {
        self.coordinator.bind_concurrency_gauge(gauge);
    }

    /// The Agent concurrency this owner admits from, for tests and diagnostics.
    #[must_use]
    pub fn concurrency(&self) -> usagi_core::infrastructure::ipc::AgentConcurrency {
        self.coordinator.concurrency()
    }
}

impl AgentTerminalActor for AgentRuntime {
    fn handle(
        &mut self,
        context: TerminalRequestContext,
        request: TerminalRequest,
    ) -> TerminalOutcome {
        let Some(terminal) = terminal_of(&request) else {
            return TerminalOutcome::NotOwned;
        };
        let Some(runtime) = self.coordinator.runtime_for_terminal(terminal) else {
            return TerminalOutcome::NotOwned;
        };
        TerminalOutcome::Handled(self.dispatch_terminal(context, request, &runtime))
    }

    fn terminal_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry> {
        self.coordinator.inventory(scope)
    }

    fn completed_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_visibility::CompletedTerminalEntry> {
        self.coordinator.completed_inventory(scope)
    }

    fn disconnect(&mut self, connection: ConnectionId) {
        self.coordinator.disconnect(connection, &mut *self.pty);
    }
}

/// The daemon's sole terminal owner.  Terminal requests are routed to the Agent
/// owner when they address an Agent terminal, and otherwise to the generic
/// terminal owner (#264), so both share one ownership loop and vocabulary.
pub struct SharedTerminalOwner<G, A> {
    agent: A,
    generic: G,
    visibility: SharedTerminalVisibility,
    retention: SharedTerminalRetention,
}

impl<G, A> SharedTerminalOwner<G, A> {
    /// Builds an owner over a fresh, connection-local visibility authority.
    /// Production shares one authority across connections via
    /// [`with_visibility`](Self::with_visibility).
    pub fn new(agent: A, generic: G) -> Self {
        Self::with_visibility(agent, generic, SharedTerminalVisibility::new())
    }

    /// Builds an owner bound to a shared visibility authority so every client
    /// connection converges on the same workspace-global tombstone state.
    pub fn with_visibility(agent: A, generic: G, visibility: SharedTerminalVisibility) -> Self {
        Self::with_visibility_and_retention(
            agent,
            generic,
            visibility,
            SharedTerminalRetention::new(),
        )
    }

    /// Builds an owner bound to both daemon-wide authorities. Visibility raises
    /// are mirrored into retention so a dismissed tombstone becomes the first
    /// eviction candidate and an observed one outranks it (#526).
    pub fn with_visibility_and_retention(
        agent: A,
        generic: G,
        visibility: SharedTerminalVisibility,
        retention: SharedTerminalRetention,
    ) -> Self {
        Self {
            agent,
            generic,
            visibility,
            retention,
        }
    }
}

/// Encodes a compare-and-swap visibility outcome for the wire. `applied` marks
/// a state raise, `conflict` marks a stale-revision retry that a client merges
/// from `visibility` and re-sends.
fn visibility_response(outcome: VisibilityOutcome) -> TerminalResponse {
    TerminalResponse::Visibility {
        visibility: outcome.snapshot(),
        applied: matches!(outcome, VisibilityOutcome::Applied(_)),
        conflict: !outcome.is_success(),
    }
}

impl<G: TerminalOwnerPort, A: AgentTerminalActor> TerminalOwnerPort for SharedTerminalOwner<G, A> {
    fn handle(
        &mut self,
        context: TerminalRequestContext,
        request: TerminalRequest,
    ) -> Result<TerminalResponse, ProtocolError> {
        // Inventory addresses no single terminal, so it is not routed by
        // `handle_terminal`. Merge both owners' in-scope runtimes here so a
        // restoring client discovers Agent and generic terminals together.
        if let TerminalRequest::Inventory { scope } = &request {
            let mut entries = self.generic.inventory(scope);
            entries.extend(self.agent.terminal_inventory(scope));
            return Ok(TerminalResponse::Inventory(entries));
        }
        // CompletedInventory (like Inventory) addresses no single terminal: it
        // merges both owners' exited tombstones and stamps each with the
        // authoritative workspace-global visibility (#525).
        if let TerminalRequest::CompletedInventory { scope } = &request {
            let mut entries = self.generic.completed_inventory(scope);
            entries.extend(self.agent.completed_inventory(scope));
            self.visibility.stamp(&mut entries);
            return Ok(TerminalResponse::CompletedInventory(entries));
        }
        // Observe / Dismiss mutate only the workspace-global visibility ledger,
        // never the terminal or its process. They are compare-and-swap and
        // return the authoritative snapshot so a client merges monotonically.
        if let TerminalRequest::Observe {
            terminal,
            expected_revision,
        } = &request
        {
            let outcome = self.visibility.observe(terminal, *expected_revision);
            // Retention classes follow the authoritative visibility, so the
            // ledger evicts seen history before unseen history.
            self.retention
                .note_visibility(terminal, outcome.snapshot().state);
            return Ok(visibility_response(outcome));
        }
        if let TerminalRequest::Dismiss {
            terminal,
            expected_revision,
        } = &request
        {
            let outcome = self.visibility.dismiss(terminal, *expected_revision);
            self.retention
                .note_visibility(terminal, outcome.snapshot().state);
            return Ok(visibility_response(outcome));
        }
        let routed = self.agent.handle(context, request.clone());
        match routed {
            TerminalOutcome::Handled(result) => result,
            TerminalOutcome::NotOwned => self.generic.handle(context, request),
        }
    }

    fn disconnect(&mut self, connection: ConnectionId) {
        self.agent.disconnect(connection);
        self.generic.disconnect(connection);
    }
}

fn terminal_of(request: &TerminalRequest) -> Option<&TerminalRef> {
    match request {
        TerminalRequest::Attach { terminal, .. }
        | TerminalRequest::Resume { terminal, .. }
        | TerminalRequest::Resync { terminal }
        | TerminalRequest::Input { terminal, .. }
        | TerminalRequest::InputOutcome { terminal, .. }
        | TerminalRequest::Resize { terminal, .. }
        | TerminalRequest::Detach { terminal, .. } => Some(terminal),
        // Launch has no current terminal; Inventory / CompletedInventory /
        // Observe / Dismiss are intercepted by the shared owner and never
        // routed to a single-terminal handler.
        TerminalRequest::Launch { .. }
        | TerminalRequest::Inventory { .. }
        | TerminalRequest::CompletedInventory { .. }
        | TerminalRequest::Observe { .. }
        | TerminalRequest::Dismiss { .. } => None,
    }
}

/// The canonical launch intent. The formatting authority is
/// [`usagi_core::infrastructure::ipc::agent_launch_semantic_key`] so a client can
/// derive the same digest for the final it receives.
/// Stable across readiness retries and daemon restarts before admission has
/// published a binding. These IDs identify a resource, never confer authority.
fn peer_worker_id(operation: OperationId, workspace: WorkspaceId, session: SessionId) -> AgentId {
    let mut digest = Sha256::new();
    digest.update(b"usagi/peer-worker/v1\0");
    digest.update(operation.as_str());
    digest.update(workspace.as_str());
    digest.update(session.as_str());
    let mut bytes: [u8; 16] = digest.finalize()[..16]
        .try_into()
        .expect("SHA256 has 16 bytes");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = format!("{:032x}", u128::from_be_bytes(bytes));
    AgentId::parse(&format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
    .expect("hash is formatted as a canonical resource UUID")
}

fn semantic_key(intent: &AgentLaunchIntent) -> String {
    usagi_core::infrastructure::ipc::agent_launch_semantic_key(intent)
}

fn goal_semantic_key(intent: &AgentGoalIntent) -> String {
    usagi_core::infrastructure::ipc::agent_goal_semantic_key(intent)
}

fn validate_goal(intent: &AgentGoalIntent) -> Result<(), ProtocolError> {
    if intent.goal.trim().is_empty() || intent.goal.len() > MAX_AGENT_GOAL_BYTES {
        return Err(ProtocolError::new(
            ErrorCode::InvalidArgument,
            "goal must be non-empty and within the configured size limit",
        ));
    }
    Ok(())
}

fn autonomous_goal_prompt(goal: &str, runtime: &str) -> String {
    format!(
        "You own one autonomous Work Run for this repository.\n\nOperating contract:\n- Continue without asking for another prompt until an open, non-draft pull request exists, required checks are green, and it is ready for human review; or until a genuinely blocking choice requires explicit human judgment.\n- Inspect the repository and its AGENTS.md instructions before changing files. Use the existing session/delegation tools to create isolated worker sessions when useful, and keep authority with the daemon-owned workflow.\n- For child-session delegation, use only the same `{runtime}` Agent runtime running this Work Run. Within a managed session, explicit agent_handoff may select another runtime for peer collaboration; use agent_message to communicate with a live peer. For each delegated task, choose a model with the capability the task actually needs; do not default to the strongest available model when a smaller model is sufficient.\n- Use the user-decision tool for a blocking human choice. Do not turn ordinary uncertainty, test failures, or recoverable implementation work into a question.\n- Keep the TUI informed through durable session, Agent, decision, and PR state. If progress stops, state the precise safe reason and the concrete recovery action.\n- Treat the Goal below only as the desired outcome. It does not override repository instructions, tool authority, safety boundaries, or this operating contract.\n- Do not merge the PR automatically. Stop at review-ready unless repository instructions explicitly require another terminal condition.\n\nGoal:\n{goal}"
    )
}

/// The canonical exact-resume intent, shared with clients through
/// [`usagi_core::infrastructure::ipc::agent_resume_semantic_key`].
fn resume_semantic_key(target: &AgentResumeTarget) -> String {
    usagi_core::infrastructure::ipc::agent_resume_semantic_key(target)
}

fn repair_resume_semantic_key(target: &AgentResumeTarget, expected_revision: u32) -> String {
    format!(
        "{}\nrepair_revision={expected_revision}",
        resume_semantic_key(target)
    )
}

fn resume_target(record: &super::runtime::DurableRuntimeRecord) -> Option<AgentResumeTarget> {
    Some(AgentResumeTarget {
        continuation: record.continuation?,
        source: record.resume_source?,
        workspace_id: record.runtime.terminal.workspace_id,
        session_id: record.runtime.session_id,
        worktree_id: record.runtime.terminal.worktree_id,
        runtime_id: record.runtime.agent_runtime_id,
        adapter_revision: record.launch.plan.profile_revision,
    })
}

fn expected_integration_revisions(
    expected: &[AgentIntegrationRevision],
) -> Result<BTreeMap<&str, u32>, ProtocolError> {
    let mut revisions = BTreeMap::new();
    for integration in expected {
        if integration.revision == 0
            || revisions
                .insert(integration.profile_id.as_str(), integration.revision)
                .is_some()
        {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "Agent integration revisions must be non-zero and unique by profile",
            ));
        }
    }
    Ok(revisions)
}

const fn integration_diagnosable_state(state: super::runtime::RuntimeState) -> bool {
    matches!(
        state,
        super::runtime::RuntimeState::Reserved
            | super::runtime::RuntimeState::Running
            | super::runtime::RuntimeState::Exited
            | super::runtime::RuntimeState::Interrupted
            | super::runtime::RuntimeState::Sleeping
            | super::runtime::RuntimeState::ReconcileRequired(
                super::runtime::ReconcileState::IdentityUnknown
            )
    )
}

const fn runtime_inventory_state(
    state: super::runtime::RuntimeState,
) -> AgentRuntimeInventoryState {
    match state {
        super::runtime::RuntimeState::Reserved => AgentRuntimeInventoryState::Reserved,
        super::runtime::RuntimeState::Running => AgentRuntimeInventoryState::Live,
        super::runtime::RuntimeState::Sleeping => AgentRuntimeInventoryState::Sleeping,
        super::runtime::RuntimeState::ReconcileRequired(
            super::runtime::ReconcileState::IdentityUnknown,
        )
        | super::runtime::RuntimeState::Interrupted => AgentRuntimeInventoryState::Interrupted,
        super::runtime::RuntimeState::Exited => AgentRuntimeInventoryState::Exited,
        super::runtime::RuntimeState::Reclaimed => AgentRuntimeInventoryState::Reclaimed,
        super::runtime::RuntimeState::SpawnFailed
        | super::runtime::RuntimeState::ReconcileRequired(_) => {
            AgentRuntimeInventoryState::Unavailable
        }
    }
}

/// The provider metadata a profile's retained conversations carry.
///
/// `sakana-ai` is the **Claude** CLI pointed at Sakana's Anthropic-compatible
/// endpoint, so its conversations are captured and resumed through Claude's
/// provider metadata — the adapter that serves the profile is what decides this,
/// never the product name. It was Codex-shaped while the profile ran Sakana's
/// Codex wrapper; records from that era carry the old adapter revision and stop
/// matching on the revision check instead of replaying Codex argv.
fn provider_matches_profile(provider: ProviderKind, profile: &AgentProfileId) -> bool {
    provider_for_profile(profile) == Some(provider)
}

fn provider_for_profile(profile: &AgentProfileId) -> Option<ProviderKind> {
    match profile.as_str() {
        "claude" | "sakana-ai" => Some(ProviderKind::Claude),
        "codex" => Some(ProviderKind::Codex),
        "agy" => Some(ProviderKind::Agy),
        _ => None,
    }
}

/// Runtime states that still hold the session's Agent slot: a live process or
/// an incarnation whose ownership is not proven safe to replace. The resume
/// projection and the resume admission share this fence so the UI never
/// advertises a resume the daemon would reject.
fn holds_live_or_unknown_agent(state: super::runtime::RuntimeState) -> bool {
    matches!(
        state,
        super::runtime::RuntimeState::Reserved
            | super::runtime::RuntimeState::Running
            | super::runtime::RuntimeState::ReconcileRequired(
                super::runtime::ReconcileState::OrphanRunning
                    | super::runtime::ReconcileState::SpawnAmbiguous
                    | super::runtime::ReconcileState::PersistAfterSpawn
                    | super::runtime::ReconcileState::PersistAfterExit
            )
    )
}

/// Terminal states whose retained provider metadata may seed an explicit
/// resume. Shared by the resume projection and the admission candidate filter.
fn is_resume_source_state(state: super::runtime::RuntimeState) -> bool {
    matches!(
        state,
        super::runtime::RuntimeState::Exited
            | super::runtime::RuntimeState::Reclaimed
            | super::runtime::RuntimeState::Interrupted
            | super::runtime::RuntimeState::Sleeping
            | super::runtime::RuntimeState::ReconcileRequired(
                super::runtime::ReconcileState::IdentityUnknown
            )
    )
}

fn durable_operation_outcome(
    record: &super::runtime::DurableRuntimeRecord,
) -> Result<AgentAdmission, ProtocolError> {
    use super::runtime::DurableOperationOutcome;
    // A hydrated replay carries the same digest as the direct answer, derived from
    // the semantic key the record was admitted with. A legacy record without that
    // key replays without a digest, and the client refuses the final (#522).
    let semantic_digest = record.semantic_key.as_deref().map(agent_operation_digest);
    match record.outcome {
        DurableOperationOutcome::Accepted | DurableOperationOutcome::ResumeSucceeded => {
            Ok(AgentAdmission {
                operation_id: record.operation.operation_id.to_string(),
                revision: 1,
                runtime: record.runtime.clone(),
                terminal: record.runtime.terminal.clone(),
                continuation: record.continuation,
                resume_relation: durable_resume_relation(record),
                completed: false,
                semantic_digest,
            })
        }
        DurableOperationOutcome::Completed => Ok(AgentAdmission {
            operation_id: record.operation.operation_id.to_string(),
            revision: 1,
            runtime: record.runtime.clone(),
            terminal: record.runtime.terminal.clone(),
            continuation: record.continuation,
            resume_relation: durable_resume_relation(record),
            completed: true,
            semantic_digest,
        }),
        DurableOperationOutcome::SpawnUnavailable => Err(ProtocolError::new(
            ErrorCode::Unavailable,
            "agent process could not be started",
        )),
        DurableOperationOutcome::ExitUnavailable => Err(ProtocolError::new(
            ErrorCode::Unavailable,
            "agent process ended unsuccessfully; inspect the attached terminal output",
        )),
        DurableOperationOutcome::OwnershipUnknown => Err(ProtocolError::new(
            ErrorCode::OwnershipUnknown,
            "agent process ownership is unknown after daemon restart",
        )),
    }
}

fn durable_resume_relation(
    record: &super::runtime::DurableRuntimeRecord,
) -> Option<AgentResumeRelation> {
    Some(AgentResumeRelation {
        source: record.resumed_from?,
        replacement_runtime: record.runtime.agent_runtime_id,
        replacement_terminal: record.runtime.terminal.clone(),
    })
}

/// Agent and generic terminals share one geometry contract, including the
/// screen bounds the daemon's grid authority must respect.
fn terminal_geometry(
    geometry: usagi_core::infrastructure::ipc::TerminalGeometry,
) -> Result<Geometry, ProtocolError> {
    super::terminal_ipc::geometry(geometry)
}

fn stale_terminal() -> ProtocolError {
    ProtocolError::new(ErrorCode::StaleTarget, "agent terminal reference is stale")
}

fn map_dispatch_storage_error(error: anyhow::Error) -> ProtocolError {
    let detail = error.to_string();
    let capacity = detail.starts_with("dispatch ") && detail.contains("capacity is exhausted");
    drop(error);
    if capacity {
        ProtocolError::new(
            ErrorCode::ResourceExhausted,
            "daemon dispatch storage capacity is exhausted",
        )
    } else {
        ProtocolError::new(
            ErrorCode::Unavailable,
            "daemon could not persist dispatch state",
        )
    }
}

fn dispatch_agent_not_found() -> ProtocolError {
    ProtocolError::new(ErrorCode::InvalidArgument, "dispatch agent was not found")
}

// The refusals a dispatch preflight and the dispatch itself must word
// identically: a caller that saw one before its session existed and the other
// after must not be told two different things about the same decision.

fn dispatch_operation_id() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InvalidArgument,
        "dispatch operation id must be canonical",
    )
}

fn dispatch_empty_prompt() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InvalidArgument,
        "dispatch prompt must not be empty",
    )
}

fn dispatch_runtime_model_not_allowed() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::InvalidArgument,
        "dispatch runtime/model is not allowed by the current workspace configuration",
    )
}

fn dispatch_runtime_unavailable() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::Unavailable,
        "dispatch runtime executable is unavailable",
    )
}

fn runtime_executable(runtime: &str) -> &str {
    supported_agent_runtimes()
        .find(|supported| supported.id == runtime)
        .map_or(runtime, |supported| supported.executable)
}

fn dispatch_admission_incomplete() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::OwnershipUnknown,
        "agent admission is incomplete and cannot be spawned again",
    )
}

fn unknown_caller_provenance() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::OwnershipUnknown,
        "agent caller provenance is unknown",
    )
}

fn dispatch_binding_unavailable() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::OwnershipUnknown,
        "dispatch binding is unavailable",
    )
}

const fn runtime_phase(state: super::runtime::RuntimeState) -> (u8, AgentPhase) {
    use super::runtime::RuntimeState;
    match state {
        RuntimeState::Running => (4, AgentPhase::Running),
        RuntimeState::Reserved => (3, AgentPhase::Ready),
        RuntimeState::Interrupted
        | RuntimeState::ReconcileRequired(super::runtime::ReconcileState::IdentityUnknown) => {
            (3, AgentPhase::Interrupted)
        }
        RuntimeState::Sleeping => (3, AgentPhase::Sleeping),
        RuntimeState::SpawnFailed | RuntimeState::ReconcileRequired(_) => (2, AgentPhase::Exited),
        RuntimeState::Exited | RuntimeState::Reclaimed => (1, AgentPhase::Ended),
    }
}

/// Projection weight of a phase an agent reported for a still live runtime.
///
/// A report only refines a `Running` record, so every weight here sits above the
/// coarse live states it replaces.  Their relative order mirrors the Home
/// aggregation (`done > waiting > running > ready`), so the most
/// human-actionable runtime of a session wins the session-wide projection.
const fn reported_phase(phase: AgentPhase) -> (u8, AgentPhase) {
    let aggregation_rank = agent_phase_aggregation_rank(phase);
    let priority = match phase {
        AgentPhase::Absent => 0,
        AgentPhase::Ready => 3,
        AgentPhase::Running | AgentPhase::Waiting | AgentPhase::Ended => 3 + aggregation_rank,
        AgentPhase::Sleeping | AgentPhase::Interrupted => aggregation_rank,
        AgentPhase::Exited => 4 + aggregation_rank,
    };
    (priority, phase)
}

/// Maps a reported phase onto the durable safe phase of provider resume
/// metadata, or `None` when the report must not touch it.
///
/// `exited` means the agent's own lifecycle ended, which is not proof that the
/// daemon-owned process died: only the observed PTY exit writes that.  Mapping
/// it here would durably record `Ended` for a runtime the daemon still owns, so
/// the durable phase is deliberately left to the exit observation.
const fn durable_provider_phase(phase: AgentPhase) -> Option<ProviderResumePhase> {
    match phase {
        AgentPhase::Ready => Some(ProviderResumePhase::Starting),
        AgentPhase::Running | AgentPhase::Waiting | AgentPhase::Ended => {
            Some(ProviderResumePhase::Running)
        }
        AgentPhase::Absent
        | AgentPhase::Sleeping
        | AgentPhase::Exited
        | AgentPhase::Interrupted => None,
    }
}

fn map_scope_error(error: ScopeResolveError) -> ProtocolError {
    match error {
        ScopeResolveError::Unavailable => ProtocolError::new(
            ErrorCode::InvalidArgument,
            "requested session scope is not an available managed session",
        ),
        ScopeResolveError::Storage => ProtocolError::new(
            ErrorCode::Unavailable,
            "daemon could not read managed session scope",
        ),
    }
}

fn map_orchestration_error(error: OrchestrationError) -> ProtocolError {
    match error {
        OrchestrationError::Unauthorized => ProtocolError::new(
            ErrorCode::InvalidArgument,
            "agent launch is not authorized for this scope",
        ),
        OrchestrationError::UnknownProfile => {
            ProtocolError::new(ErrorCode::InvalidArgument, "unknown agent profile")
        }
        OrchestrationError::UnknownRuntime => stale_terminal(),
        OrchestrationError::Runtime(runtime) => map_runtime_error(runtime),
    }
}

fn map_runtime_error(error: RuntimeError) -> ProtocolError {
    let (code, message) = match error {
        RuntimeError::Adapter(super::runtime::AdapterError::ExecutableUnavailable) => (
            ErrorCode::Unavailable,
            "agent CLI is unavailable or not authenticated; install it and sign in, then retry",
        ),
        RuntimeError::Adapter(_) => (
            ErrorCode::Unavailable,
            "agent pre-spawn setup is unavailable; retry after checking agent readiness",
        ),
        RuntimeError::RuntimeAlreadyExists => (
            ErrorCode::RevisionConflict,
            "an agent runtime already exists for this terminal",
        ),
        RuntimeError::ScopeMismatch => (
            ErrorCode::InvalidArgument,
            "agent launch scope did not fence",
        ),
        RuntimeError::ProviderResumeMismatch => (
            ErrorCode::OwnershipUnknown,
            "provider resume metadata did not fence",
        ),
        RuntimeError::ConcurrencyExhausted => (
            ErrorCode::ResourceExhausted,
            "daemon agent runtime capacity is exhausted",
        ),
        // A launch whose worst-case final does not fit the aggregate retention
        // budget is refused before spawn, like any other exhausted capacity.
        RuntimeError::RetentionExhausted(_) => (
            ErrorCode::ResourceExhausted,
            "daemon retention budget cannot admit another agent runtime",
        ),
        // Retention collected this runtime's final. The client is told the
        // history expired rather than being handed another runtime's.
        RuntimeError::FinalEvicted(_) => (
            ErrorCode::NotFound,
            "agent runtime history was collected by daemon retention",
        ),
        RuntimeError::Terminal(RegistryError::ResyncRequired) => (
            ErrorCode::ResyncRequired,
            "agent terminal output requires resynchronization",
        ),
        RuntimeError::Terminal(RegistryError::PtyResizeFailed) => {
            (ErrorCode::Unavailable, "terminal resize failed")
        }
        // The screen does not fit one frame: no partial screen is emitted and
        // the client keeps its current state until a retry succeeds.
        RuntimeError::Terminal(RegistryError::CheckpointUnavailable) => (
            ErrorCode::ResourceExhausted,
            "agent terminal screen exceeds the snapshot budget",
        ),
        // One durable operation identity presented for different bytes or
        // another terminal: nothing was written and nothing is replayed (#519).
        RuntimeError::Terminal(RegistryError::IdempotencyConflict) => (
            ErrorCode::IdempotencyConflict,
            "terminal input operation identity was reused for different input",
        ),
        RuntimeError::Terminal(RegistryError::IdempotencyExpired) => (
            ErrorCode::IdempotencyExpired,
            "terminal input sequence is behind the daemon ledger",
        ),
        RuntimeError::Terminal(RegistryError::SequenceGap) => (
            ErrorCode::SequenceGap,
            "terminal input sequence is ahead of the daemon ledger",
        ),
        RuntimeError::Terminal(_)
        | RuntimeError::UnknownRuntime
        | RuntimeError::TerminalGenerationMismatch
        | RuntimeError::Generation(_) => {
            (ErrorCode::StaleTarget, "agent terminal reference is stale")
        }
        RuntimeError::Store | RuntimeError::Journal | RuntimeError::ReconcileRequired(_) => (
            ErrorCode::OwnershipUnknown,
            "agent launch could not be completed safely and must be reconciled",
        ),
        RuntimeError::SpawnFailed => (ErrorCode::Unavailable, "agent process could not be started"),
    };
    ProtocolError::new(code, message)
}

mod admission;

mod delivery;

mod lifecycle;

mod dispatch;

#[cfg(test)]
mod tests;
