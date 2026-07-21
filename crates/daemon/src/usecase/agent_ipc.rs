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

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::Utc;
use serde_json::{Value, json};
use usagi_core::{
    domain::{
        agent::{
            AgentCapability, AgentProfileId, AgentStatus, CallerRef, DispatchBinding, DispatchRun,
            InboxKind, InboxMessage, LaunchMode, LaunchRequest, LaunchScope, ModelSelector,
            ProviderCaptureProvenance, ProviderKind, ProviderResumePhase, ProviderResumeReason,
            ProviderResumeRef, ProviderResumeStatus, ProviderSessionId, RunStatus, WorkerRef,
        },
        id::{
            AgentRuntimeId, AgentRuntimeRef, ClientId, CompletionFence, ConnectionId,
            DaemonGeneration, OperationId, RequestId, SessionId, TerminalId, TerminalRef,
            WorkspaceId, WorktreeId,
        },
    },
    infrastructure::ipc::{ErrorCode, ProtocolError},
    infrastructure::runtime_model::{
        ExecutableLocator, PathExecutableLocator, WorkspaceAgentConfig,
    },
    infrastructure::store::dispatch::{
        AgentAdmissionReservation, CredentialProvenance as DispatchCredentialProvenance,
        DispatchStore,
    },
    usecase::client::{
        AgentLaunchIntent, DispatchAgentIntent, DispatchIntent, TerminalAction, TerminalRequest,
    },
};

use crate::presentation::ipc::TerminalOwner;

use super::{
    orchestration::{AdapterRegistry, OrchestrationError, Orchestrator, RuntimeAuthorization},
    runtime::{OutputJournal, PtySpawner, RuntimeCoordinator, RuntimeError},
    terminal::{Geometry, InputRequest, PtyWriter, RegistryError},
};

/// A daemon-resolved, fully fenced checkout for an available scope (a managed
/// session or the workspace root).
///
/// It is produced only by the #268 scope resolver; this crate never re-derives
/// it from a client supplied name or path.
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

/// Input port: the scope resolver owned by #268.  Consumed here to convert a
/// product-neutral launch scope into a fully fenced available checkout.  A
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
    pub terminal: TerminalRef,
    /// Present only after the daemon has observed and durably committed a
    /// successful process exit.  A replay therefore distinguishes an accepted
    /// running operation from its single final success without guessing a
    /// replacement terminal.
    pub completed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    Auto,
    Queue,
    Live,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDelivery {
    pub delivered_to: &'static str,
    pub queued: bool,
}

/// One durable Agent operation, replayed identically on resend/reconnect.
#[derive(Debug, Clone)]
struct AgentOperation {
    semantic_key: Option<String>,
    outcome: Result<AgentAdmission, ProtocolError>,
}

#[derive(Debug, Clone)]
struct McpCaller {
    runtime: AgentRuntimeRef,
    operation: OperationId,
}

/// The routing decision for a terminal request that addresses a `TerminalRef`.
pub enum TerminalOutcome {
    /// The Agent owner recognizes the terminal and produced this result.
    Handled(Result<Value, ProtocolError>),
    /// The terminal is not an Agent terminal; the caller must try the generic
    /// terminal owner instead.
    NotOwned,
}

/// Terminal-stream surface for Agent terminals, kept behind a trait so a shared
/// owner can compose it with the generic terminal owner without duplicating the
/// ownership loop.
pub trait AgentTerminalActor {
    fn handle_terminal(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        request_id: RequestId,
        action: TerminalAction,
        request: TerminalRequest,
    ) -> TerminalOutcome;
    /// Lists the Agent runtimes this actor holds in the exact requested scope.
    /// `SharedTerminalOwner` merges this with the generic terminal owner so a
    /// client's `Inventory` request sees Agent and generic terminals together.
    fn terminal_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry>;
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
    mcp_callers: BTreeMap<String, McpCaller>,
}

impl AgentRuntime {
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

impl AgentRuntime {
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
        let mut coordinator = RuntimeCoordinator::new(16, 64 * 1024, 64);
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
            mcp_callers: BTreeMap::new(),
        }
    }

    /// Constructs the runtime only after a reconciled durable snapshot has
    /// been validated and loaded. No admission path is available on failure.
    pub fn hydrate_with_dispatch_and_locator(
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
    ) -> Result<Self, super::runtime::RuntimeSnapshotError> {
        let mut coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64)?;
        coordinator.activate_generation(generation)?;
        store
            .save(coordinator.snapshot())
            .map_err(|()| super::runtime::RuntimeSnapshotError::OwnershipPersist)?;
        dispatch
            .reconcile_incomplete_admissions()
            .map_err(|_| super::runtime::RuntimeSnapshotError::DispatchReconcile)?;
        let operations = coordinator
            .snapshot()
            .records
            .into_iter()
            .map(|record| {
                let operation_id = record.operation.operation_id.to_string();
                let outcome = durable_operation_outcome(&record);
                (
                    operation_id,
                    AgentOperation {
                        semantic_key: record.semantic_key,
                        outcome,
                    },
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
            // Credentials intentionally fail closed across daemon restart.
            mcp_callers: BTreeMap::new(),
        })
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

    #[must_use]
    pub fn dispatch_store(&self) -> &DispatchStore {
        &self.dispatch
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

    /// Resolves the durable dispatch identity authenticated by an MCP child.
    /// The credential is daemon-minted process provision; no client supplied
    /// agent or session name participates in this lookup.
    #[must_use]
    pub fn mcp_dispatch_caller(&self, credential: &str) -> Option<CallerRef> {
        let run_id = self.mcp_caller(credential)?;
        let binding = self.dispatch.binding(run_id).ok()??;
        Some(CallerRef {
            session_id: binding.worker.session_id,
            agent_id: binding.worker.agent_id,
        })
    }
}

impl AgentRuntime {
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

    /// Returns the durable runtime phase projected for one session.
    #[must_use]
    pub fn session_phase(&self, session: SessionId) -> &'static str {
        self.coordinator
            .snapshot()
            .records
            .into_iter()
            .filter(|record| record.runtime.session_id == Some(session))
            .map(|record| runtime_phase(record.state))
            .max_by_key(|(priority, _)| *priority)
            .map_or("none", |(_, phase)| phase)
    }

    /// Sends to a running Agent PTY or records a durable next-launch prompt.
    pub fn prompt(
        &mut self,
        session: Option<SessionId>,
        prompt: &str,
        mode: PromptMode,
    ) -> Result<PromptDelivery, ProtocolError> {
        if prompt.trim().is_empty() {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "prompt must not be empty",
            ));
        }
        let live = self
            .coordinator
            .snapshot()
            .records
            .into_iter()
            .find(|record| {
                record.runtime.session_id == session
                    && record.state == super::runtime::RuntimeState::Running
            });
        if matches!(mode, PromptMode::Live) && live.is_none() {
            return Err(ProtocolError::new(
                ErrorCode::Unavailable,
                "target session has no live agent",
            ));
        }
        if matches!(mode, PromptMode::Queue) && live.is_some() {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "target session already has a live agent; use auto or live",
            ));
        }
        if let Some(record) = live
            && !matches!(mode, PromptMode::Queue)
        {
            let mut bytes = prompt.as_bytes().to_vec();
            bytes.push(b'\n');
            self.pty.select_terminal(&record.runtime.terminal);
            self.pty.write_all(&bytes).map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "live prompt delivery failed")
            })?;
            return Ok(PromptDelivery {
                delivered_to: "live",
                queued: false,
            });
        }
        self.dispatch
            .queue_prompt(session, prompt.to_owned(), Utc::now())
            .map_err(map_dispatch_storage_error)?;
        Ok(PromptDelivery {
            delivered_to: "queue",
            queued: true,
        })
    }

    /// Delivers a continuation only to the exact live operation that created
    /// it.  Session scope alone is insufficient here: a replacement agent in
    /// the same session must never receive a late decision answer.
    pub fn prompt_run(
        &mut self,
        operation: OperationId,
        prompt: &str,
    ) -> Result<PromptDelivery, ProtocolError> {
        if prompt.trim().is_empty() {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "prompt must not be empty",
            ));
        }
        let live = self
            .coordinator
            .snapshot()
            .records
            .into_iter()
            .find(|record| {
                record.operation.operation_id == operation
                    && record.state == super::runtime::RuntimeState::Running
            })
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::Unavailable, "target agent run is no longer live")
            })?;
        let mut bytes = prompt.as_bytes().to_vec();
        bytes.push(b'\n');
        self.pty.select_terminal(&live.runtime.terminal);
        self.pty.write_all(&bytes).map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "live prompt delivery failed")
        })?;
        Ok(PromptDelivery {
            delivered_to: "live",
            queued: false,
        })
    }

    /// Admits one Agent launch.  The same producer `operation_id` with the same
    /// intent returns the same admission (no second spawn); the same id with a
    /// different intent is a typed idempotency conflict.
    pub fn launch(
        &mut self,
        operation_id: &str,
        intent: &AgentLaunchIntent,
        scope: &dyn SessionScopeResolver,
    ) -> Result<AgentAdmission, ProtocolError> {
        let semantic_key = semantic_key(intent);
        if let Some(existing) = self.operations.get(operation_id) {
            if existing
                .semantic_key
                .as_ref()
                .is_some_and(|key| key != &semantic_key)
            {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different agent launch",
                ));
            }
            return existing.outcome.clone();
        }
        let outcome = self.admit(operation_id, intent, scope);
        self.operations.insert(
            operation_id.to_owned(),
            AgentOperation {
                semantic_key: Some(semantic_key),
                outcome: outcome.clone(),
            },
        );
        outcome
    }

    /// Starts a new daemon-owned runtime for the provider conversation retained
    /// by an interrupted/exited runtime. This never reattaches the old PTY and
    /// never falls back to provider-global "last" semantics.
    pub fn resume(
        &mut self,
        operation_id: &str,
        workspace: WorkspaceId,
        session: SessionId,
        scope: &dyn SessionScopeResolver,
    ) -> Result<AgentAdmission, ProtocolError> {
        let semantic_key = format!("resume:{workspace}:{session}");
        if let Some(existing) = self.operations.get(operation_id) {
            if existing
                .semantic_key
                .as_ref()
                .is_some_and(|key| key != &semantic_key)
            {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different agent resume",
                ));
            }
            return existing.outcome.clone();
        }
        let outcome = self.admit_resume(operation_id, workspace, session, &semantic_key, scope);
        self.operations.insert(
            operation_id.to_owned(),
            AgentOperation {
                semantic_key: Some(semantic_key),
                outcome: outcome.clone(),
            },
        );
        outcome
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
            .record_provider_resume(runtime, reference, &mut *self.store)
            .map_err(map_runtime_error)
    }

    /// Safe interrupted/resume projection for a managed session. Provider IDs
    /// are never returned; only availability and a stable reason cross IPC.
    #[must_use]
    #[allow(clippy::missing_panics_doc)] // The preceding identity iterator proves the resumable record invariant.
    pub fn session_resume_status(&self, session: SessionId) -> (bool, ProviderResumeReason) {
        let records = self.coordinator.snapshot().records;
        if records.iter().any(|record| {
            record.runtime.session_id == Some(session) && holds_live_or_unknown_agent(record.state)
        }) {
            return (false, ProviderResumeReason::LiveOrOwnershipUnknown);
        }
        let resumable = records
            .iter()
            .filter(|record| {
                record.runtime.session_id == Some(session)
                    && record.provider_resume.is_some()
                    && is_resume_source_state(record.state)
            })
            .collect::<Vec<_>>();
        let mut identities = resumable.iter().filter_map(|record| {
            record.provider_resume.as_ref().map(|reference| {
                (
                    reference.provider,
                    reference.native_session_id.clone(),
                    reference.adapter_revision,
                    reference.scope.clone(),
                )
            })
        });
        let Some(first) = identities.next() else {
            return (false, ProviderResumeReason::ProviderMetadataUnavailable);
        };
        if !identities.all(|candidate| candidate == first) {
            return (false, ProviderResumeReason::AmbiguousProviderMetadata);
        }
        let source = resumable
            .first()
            .expect("identity came from a resumable record");
        let reference = source
            .provider_resume
            .as_ref()
            .expect("resumable records retain provider metadata");
        let profile_id = &source.launch.plan.profile_id;
        let internally_compatible = resumable.iter().all(|record| {
            record.launch.plan.profile_id == *profile_id
                && record.launch.plan.profile_revision == reference.adapter_revision
                && record.launch.request.scope == reference.scope
                && record.runtime.terminal.workspace_id == reference.scope.workspace_id
                && record.runtime.terminal.session_id == reference.scope.session_id
                && record.runtime.terminal.worktree_id == reference.scope.worktree_id
        });
        let adapter_compatible = self.registry.profile(profile_id).is_ok_and(|profile| {
            profile.revision == reference.adapter_revision
                && profile.capabilities.contains(&AgentCapability::Resume)
                && provider_matches_profile(reference.provider, profile_id)
        });
        if internally_compatible && adapter_compatible {
            (true, ProviderResumeReason::ExplicitResumeAvailable)
        } else {
            (false, ProviderResumeReason::IncompatibleProviderMetadata)
        }
    }

    /// Launches a dispatch-selected worker through the same fenced Agent
    /// runtime used by ordinary Agent launch, then records its durable run and
    /// caller binding.  The caller is captured now and never accepted from a
    /// later completion request.
    pub fn dispatch(
        &mut self,
        operation_id: &str,
        intent: &DispatchIntent,
        session: SessionId,
        scope: &dyn SessionScopeResolver,
    ) -> Result<AgentAdmission, ProtocolError> {
        let operation = OperationId::parse(operation_id).map_err(|_| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "dispatch operation id must be canonical",
            )
        })?;
        if intent.prompt.is_empty() {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "dispatch prompt must not be empty",
            ));
        }
        let worker = match &intent.agent {
            DispatchAgentIntent::Existing { agent_id } => self
                .dispatch
                .agent(*agent_id)
                .map_err(map_dispatch_storage_error)?
                .ok_or_else(dispatch_agent_not_found)?,
            DispatchAgentIntent::New { runtime, model } => self
                .dispatch
                .upsert_agent_by_runtime_model(Some(session), runtime.clone(), model.clone())
                .map_err(map_dispatch_storage_error)?,
        };
        if worker.session_id != Some(session) {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "dispatch agent does not belong to session",
            ));
        }
        let launch = AgentLaunchIntent {
            workspace: intent.workspace,
            session: Some(session),
            profile: Some(worker.runtime.clone()),
        };
        let semantic = format!(
            "dispatch:{}:{}:{}",
            intent.session_name, worker.agent_id, intent.prompt
        );
        if let Some(existing) = self.operations.get(operation_id) {
            if existing
                .semantic_key
                .as_ref()
                .is_some_and(|key| key != &semantic)
            {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different dispatch",
                ));
            }
            return existing.outcome.clone();
        }
        if matches!(intent.agent, DispatchAgentIntent::New { .. }) {
            let config = WorkspaceAgentConfig::read(
                &scope
                    .resolve_available_scope(intent.workspace, Some(session))
                    .map_err(map_scope_error)?
                    .working_directory,
            );
            if !config.allows(worker.runtime.as_str(), worker.model.as_str()) {
                return Err(ProtocolError::new(
                    ErrorCode::InvalidArgument,
                    "dispatch runtime/model is not allowed by the current workspace configuration",
                ));
            }
            if !self.locator.is_available(worker.runtime.as_str()) {
                return Err(ProtocolError::new(
                    ErrorCode::Unavailable,
                    "dispatch runtime executable is unavailable",
                ));
            }
        }
        let outcome = self.admit_dispatch(
            operation,
            &launch,
            &intent.prompt,
            &worker,
            &intent.caller,
            &semantic,
            scope,
        );
        self.operations.insert(
            operation_id.to_owned(),
            AgentOperation {
                semantic_key: Some(semantic),
                outcome: outcome.clone(),
            },
        );
        outcome
    }

    #[allow(clippy::too_many_lines)] // Admission keeps its durable prepare/spawn/commit order visible.
    fn admit_dispatch(
        &mut self,
        operation: OperationId,
        launch: &AgentLaunchIntent,
        prompt: &str,
        worker: &usagi_core::domain::agent::Agent,
        caller: &CallerRef,
        semantic_key: &str,
        scope: &dyn SessionScopeResolver,
    ) -> Result<AgentAdmission, ProtocolError> {
        if let Some(existing) = self
            .dispatch
            .admission(operation)
            .map_err(map_dispatch_storage_error)?
        {
            return Err(if existing.semantic_key == semantic_key {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "agent admission is incomplete and cannot be spawned again",
                )
            } else {
                ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different dispatch",
                )
            });
        }
        if self
            .dispatch
            .run(operation)
            .map_err(map_dispatch_storage_error)?
            .is_some()
        {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "legacy agent admission is incomplete and cannot be spawned again",
            ));
        }
        let resolved = scope
            .resolve_available_scope(launch.workspace, launch.session)
            .map_err(map_scope_error)?;
        let terminal = TerminalRef {
            daemon_generation: self.active_generation()?,
            terminal_id: TerminalId::new(),
            workspace_id: launch.workspace,
            session_id: launch.session,
            worktree_id: resolved.worktree_id,
        };
        let runtime = AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), launch.session)
            .expect("terminal and runtime session are constructed from the same launch");
        let fence = CompletionFence {
            workspace_id: launch.workspace,
            session_id: launch.session,
            operation_id: operation,
            owner_daemon_generation: self.active_generation()?,
            execution_attempt: 1,
            lifecycle_attempt: 1,
            expected_revision: 0,
        };
        let request = LaunchRequest {
            profile_id: worker.runtime.clone(),
            mode: LaunchMode::Interactive,
            model: Some(worker.model.clone()),
            resume: false,
            provider_resume: None,
            initial_prompt: Some(prompt.to_owned()),
            scope: LaunchScope {
                workspace_id: launch.workspace,
                session_id: launch.session,
                worktree_id: resolved.worktree_id,
            },
            required_capabilities: [AgentCapability::McpWiring].into_iter().collect(),
        };
        let authorization = RuntimeAuthorization {
            runtime,
            operation: fence,
            mcp_allowed: true,
        };
        let credential = OperationId::new().to_string();
        let mut reserved_worker = worker.clone();
        reserved_worker.status = AgentStatus::Starting;
        reserved_worker.current_run = Some(operation);
        self.dispatch
            .reserve_admission(
                reserved_worker,
                DispatchRun {
                    run_id: operation,
                    agent_id: worker.agent_id,
                    prompt: prompt.to_owned(),
                    started_at: Utc::now(),
                    ended_at: None,
                    status: RunStatus::Preparing,
                },
                DispatchBinding {
                    run_id: operation,
                    caller: caller.clone(),
                    worker: WorkerRef {
                        session_id: worker.session_id,
                        agent_id: worker.agent_id,
                    },
                },
                AgentAdmissionReservation {
                    operation_id: operation,
                    semantic_key: semantic_key.to_owned(),
                    credential_provenance: DispatchCredentialProvenance::DaemonMintedEphemeral,
                },
            )
            .map_err(map_dispatch_storage_error)?;
        self.mcp_callers.insert(
            credential.clone(),
            McpCaller {
                runtime: authorization.runtime.clone(),
                operation,
            },
        );
        if let Err(error) = self.orchestrator.launch_with_semantic(
            &mut self.coordinator,
            &mut self.registry,
            &authorization,
            &request,
            self.geometry,
            &mut *self.store,
            &mut *self.pty,
            Some(credential.clone()),
            semantic_key.to_owned(),
        ) {
            self.mcp_callers.remove(&credential);
            let _ = self.dispatch.fail_admission(operation);
            return Err(map_orchestration_error(error));
        }
        self.commit_admission(operation, &credential, &authorization.runtime)?;
        Ok(AgentAdmission {
            operation_id: operation.to_string(),
            revision: 1,
            terminal,
            completed: false,
        })
    }

    #[allow(clippy::too_many_lines)] // Admission atomically fences launch, caller registration, and replay state.
    fn admit_resume(
        &mut self,
        operation_id: &str,
        workspace: WorkspaceId,
        session: SessionId,
        semantic_key: &str,
        scope: &dyn SessionScopeResolver,
    ) -> Result<AgentAdmission, ProtocolError> {
        let operation = OperationId::parse(operation_id).map_err(|_| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "agent resume operation id must be canonical",
            )
        })?;
        if self
            .dispatch
            .admission(operation)
            .map_err(map_dispatch_storage_error)?
            .is_some()
            || self
                .dispatch
                .run(operation)
                .map_err(map_dispatch_storage_error)?
                .is_some()
        {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "agent resume admission is incomplete and cannot be spawned again",
            ));
        }
        let resolved = scope
            .resolve_available_scope(workspace, Some(session))
            .map_err(map_scope_error)?;
        let records = self.coordinator.snapshot().records;
        if records.iter().any(|record| {
            record.runtime.session_id == Some(session) && holds_live_or_unknown_agent(record.state)
        }) {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "target session already has a live or ownership-unknown agent",
            ));
        }
        let candidates = records
            .iter()
            .filter(|record| {
                record.runtime.session_id == Some(session)
                    && record.runtime.terminal.workspace_id == workspace
                    && record.provider_resume.is_some()
                    && is_resume_source_state(record.state)
            })
            .collect::<Vec<_>>();
        let source = candidates.first().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::Unavailable,
                "provider resume metadata is unavailable for this session",
            )
        })?;
        let reference = source
            .provider_resume
            .as_ref()
            .expect("filtered provider resume metadata")
            .clone();
        let profile_id = source.launch.plan.profile_id.clone();
        let same_identity = candidates.iter().all(|candidate| {
            candidate.launch.plan.profile_id == profile_id
                && candidate.launch.plan.profile_revision == reference.adapter_revision
                && candidate.provider_resume.as_ref().is_some_and(|other| {
                    other.provider == reference.provider
                        && other.native_session_id == reference.native_session_id
                        && other.adapter_revision == reference.adapter_revision
                        && other.scope == reference.scope
                })
        });
        if !same_identity
            || !provider_matches_profile(reference.provider, &profile_id)
            || reference.adapter_revision != source.launch.plan.profile_revision
            || reference.scope.workspace_id != workspace
            || reference.scope.session_id != Some(session)
            || reference.scope.worktree_id != resolved.worktree_id
        {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "provider resume metadata is ambiguous or stale",
            ));
        }
        let profile = self.registry.profile(&profile_id).map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "agent adapter is unavailable")
        })?;
        if profile.revision != reference.adapter_revision
            || !profile.capabilities.contains(&AgentCapability::Resume)
        {
            return Err(ProtocolError::new(
                ErrorCode::Unavailable,
                "provider resume metadata is incompatible with the current adapter",
            ));
        }
        let terminal = TerminalRef {
            daemon_generation: self.active_generation()?,
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: resolved.worktree_id,
        };
        let runtime = AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), Some(session))
            .expect("terminal and runtime session are constructed from the same resume");
        let fence = CompletionFence {
            workspace_id: workspace,
            session_id: Some(session),
            operation_id: operation,
            owner_daemon_generation: self.active_generation()?,
            execution_attempt: 1,
            lifecycle_attempt: 1,
            expected_revision: 0,
        };
        let request = LaunchRequest {
            profile_id: profile_id.clone(),
            mode: LaunchMode::Interactive,
            model: source.launch.request.model.clone(),
            resume: true,
            provider_resume: Some(reference),
            initial_prompt: None,
            scope: LaunchScope {
                workspace_id: workspace,
                session_id: Some(session),
                worktree_id: resolved.worktree_id,
            },
            required_capabilities: [AgentCapability::McpWiring].into_iter().collect(),
        };
        let superseded = candidates
            .iter()
            .map(|candidate| candidate.runtime.clone())
            .collect::<Vec<_>>();
        let authorization = RuntimeAuthorization {
            runtime,
            operation: fence,
            mcp_allowed: true,
        };
        let credential = OperationId::new().to_string();
        let mut worker = self
            .dispatch
            .upsert_agent_by_runtime_model(
                Some(session),
                profile_id,
                source.launch.request.model.clone().unwrap_or_else(|| {
                    ModelSelector::new("default").expect("literal model selector is canonical")
                }),
            )
            .map_err(map_dispatch_storage_error)?;
        worker.status = AgentStatus::Starting;
        worker.current_run = Some(operation);
        let caller = CallerRef {
            session_id: worker.session_id,
            agent_id: worker.agent_id,
        };
        self.dispatch
            .reserve_admission(
                worker.clone(),
                DispatchRun {
                    run_id: operation,
                    agent_id: worker.agent_id,
                    prompt: String::new(),
                    started_at: Utc::now(),
                    ended_at: None,
                    status: RunStatus::Preparing,
                },
                DispatchBinding {
                    run_id: operation,
                    caller,
                    worker: WorkerRef {
                        session_id: worker.session_id,
                        agent_id: worker.agent_id,
                    },
                },
                AgentAdmissionReservation {
                    operation_id: operation,
                    semantic_key: semantic_key.to_owned(),
                    credential_provenance: DispatchCredentialProvenance::DaemonMintedEphemeral,
                },
            )
            .map_err(map_dispatch_storage_error)?;
        self.mcp_callers.insert(
            credential.clone(),
            McpCaller {
                runtime: authorization.runtime.clone(),
                operation,
            },
        );
        if let Err(error) = self.orchestrator.resume_with_semantic(
            &mut self.coordinator,
            &mut self.registry,
            &authorization,
            &request,
            self.geometry,
            &mut *self.store,
            &mut *self.pty,
            Some(credential.clone()),
            semantic_key.to_owned(),
            &superseded,
        ) {
            self.mcp_callers.remove(&credential);
            let _ = self.dispatch.fail_admission(operation);
            return Err(map_orchestration_error(error));
        }
        self.commit_admission(operation, &credential, &authorization.runtime)?;
        Ok(AgentAdmission {
            operation_id: operation_id.to_owned(),
            revision: 1,
            terminal,
            completed: false,
        })
    }

    #[allow(clippy::too_many_lines)] // Admission atomically fences launch, caller registration, and replay state.
    fn admit(
        &mut self,
        operation_id: &str,
        intent: &AgentLaunchIntent,
        scope: &dyn SessionScopeResolver,
    ) -> Result<AgentAdmission, ProtocolError> {
        let profile_id = intent
            .profile
            .clone()
            .unwrap_or_else(|| self.default_profile.clone());
        self.registry
            .profile(&profile_id)
            .map_err(|_| ProtocolError::new(ErrorCode::InvalidArgument, "unknown agent profile"))?;
        let operation = OperationId::parse(operation_id).map_err(|_| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "agent operation id must be a canonical operation identifier",
            )
        })?;
        let launch_semantic = semantic_key(intent);
        if let Some(existing) = self
            .dispatch
            .admission(operation)
            .map_err(map_dispatch_storage_error)?
        {
            return Err(if existing.semantic_key == launch_semantic {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "agent admission is incomplete and cannot be spawned again",
                )
            } else {
                ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different agent launch",
                )
            });
        }
        if self
            .dispatch
            .run(operation)
            .map_err(map_dispatch_storage_error)?
            .is_some()
        {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "legacy agent admission is incomplete and cannot be spawned again",
            ));
        }
        let resolved = scope
            .resolve_available_scope(intent.workspace, intent.session)
            .map_err(map_scope_error)?;
        let terminal = TerminalRef {
            daemon_generation: self.active_generation()?,
            terminal_id: TerminalId::new(),
            workspace_id: intent.workspace,
            session_id: intent.session,
            worktree_id: resolved.worktree_id,
        };
        let runtime = AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), intent.session)
            .expect("terminal and runtime session are constructed from the same intent");
        let fence = CompletionFence {
            workspace_id: intent.workspace,
            session_id: intent.session,
            operation_id: operation,
            owner_daemon_generation: self.active_generation()?,
            execution_attempt: 1,
            lifecycle_attempt: 1,
            expected_revision: 0,
        };
        let queued = self
            .dispatch
            .queued_prompt(intent.session)
            .map_err(map_dispatch_storage_error)?;
        let request = LaunchRequest {
            profile_id: profile_id.clone(),
            mode: LaunchMode::Interactive,
            model: None,
            resume: false,
            provider_resume: None,
            initial_prompt: queued.as_ref().map(|item| item.prompt.clone()),
            scope: LaunchScope {
                workspace_id: intent.workspace,
                session_id: intent.session,
                worktree_id: resolved.worktree_id,
            },
            required_capabilities: [AgentCapability::McpWiring].into_iter().collect(),
        };
        let authorization = RuntimeAuthorization {
            runtime,
            operation: fence,
            mcp_allowed: true,
        };
        let credential = OperationId::new().to_string();
        let mut worker = self
            .dispatch
            .upsert_agent_by_runtime_model(
                intent.session,
                profile_id.clone(),
                ModelSelector::new("default").expect("literal model selector is canonical"),
            )
            .map_err(map_dispatch_storage_error)?;
        worker.status = AgentStatus::Starting;
        worker.current_run = Some(operation);
        let caller = CallerRef {
            session_id: worker.session_id,
            agent_id: worker.agent_id,
        };
        self.dispatch
            .reserve_admission(
                worker.clone(),
                DispatchRun {
                    run_id: operation,
                    agent_id: worker.agent_id,
                    prompt: String::new(),
                    started_at: Utc::now(),
                    ended_at: None,
                    status: RunStatus::Preparing,
                },
                DispatchBinding {
                    run_id: operation,
                    caller,
                    worker: WorkerRef {
                        session_id: worker.session_id,
                        agent_id: worker.agent_id,
                    },
                },
                AgentAdmissionReservation {
                    operation_id: operation,
                    semantic_key: launch_semantic.clone(),
                    credential_provenance: DispatchCredentialProvenance::DaemonMintedEphemeral,
                },
            )
            .map_err(map_dispatch_storage_error)?;
        if queued.is_some() {
            self.dispatch
                .consume_prompt(intent.session)
                .map_err(map_dispatch_storage_error)?;
        }
        self.mcp_callers.insert(
            credential.clone(),
            McpCaller {
                runtime: authorization.runtime.clone(),
                operation,
            },
        );
        if let Err(error) = self.orchestrator.launch_with_semantic(
            &mut self.coordinator,
            &mut self.registry,
            &authorization,
            &request,
            self.geometry,
            &mut *self.store,
            &mut *self.pty,
            Some(credential.clone()),
            launch_semantic,
        ) {
            self.mcp_callers.remove(&credential);
            let _ = self.dispatch.fail_admission(operation);
            return Err(map_orchestration_error(error));
        }
        self.commit_admission(operation, &credential, &authorization.runtime)?;
        Ok(AgentAdmission {
            operation_id: operation_id.to_owned(),
            revision: 1,
            terminal,
            completed: false,
        })
    }

    fn commit_admission(
        &mut self,
        operation: OperationId,
        credential: &str,
        runtime: &AgentRuntimeRef,
    ) -> Result<(), ProtocolError> {
        let committed = matches!(self.dispatch.commit_admission(operation), Ok(true));
        self.finish_admission_commit(operation, credential, runtime, committed)
    }

    fn finish_admission_commit(
        &mut self,
        operation: OperationId,
        credential: &str,
        runtime: &AgentRuntimeRef,
        committed: bool,
    ) -> Result<(), ProtocolError> {
        if committed {
            return Ok(());
        }
        let compensation =
            self.coordinator
                .compensate_after_spawn(runtime, &mut *self.store, &mut *self.pty);
        self.mcp_callers.remove(credential);
        let _ = self.dispatch.fail_admission(operation);
        Err(map_runtime_error(compensation))
    }

    /// Journals daemon-owned PTY output before it becomes replayable.  A stale
    /// terminal is a safe no-op error, never a replacement.
    pub fn output(&mut self, terminal: &TerminalRef, bytes: Vec<u8>) -> Result<(), ProtocolError> {
        let runtime = self
            .coordinator
            .runtime_for_terminal(terminal)
            .ok_or_else(stale_terminal)?;
        self.coordinator
            .append_output(&runtime, bytes, &mut *self.journal)
            .map(|_| ())
            .map_err(map_runtime_error)
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
        Ok(())
    }

    fn synthesize_no_report(&mut self, runtime: &AgentRuntimeRef) -> Result<(), ProtocolError> {
        let fence = self
            .coordinator
            .record_for(runtime)
            .map_err(map_runtime_error)?
            .operation
            .clone();
        let run_id = fence.operation_id;
        let Some(binding) = self
            .dispatch
            .binding(run_id)
            .map_err(map_dispatch_storage_error)?
        else {
            return Ok(());
        };
        // A dispatch run only accepts a report for the exact runtime fence.
        // This exit is itself reached through the fenced terminal lookup above.
        let inbox = self
            .dispatch
            .inbox(&binding.caller)
            .map_err(map_dispatch_storage_error)?;
        for message in &inbox {
            if message.run_id == run_id {
                return Ok(());
            }
        }
        self.dispatch
            .append_inbox(
                &binding.caller,
                InboxMessage {
                    run_id,
                    from: binding.worker.clone(),
                    kind: InboxKind::NoReport,
                    summary: "worker exited without a completion report".into(),
                    result: None,
                    created_at: Utc::now(),
                    read: false,
                },
            )
            .map_err(map_dispatch_storage_error)?;
        self.dispatch
            .transition_run(run_id, RunStatus::NoReport, Some(Utc::now()))
            .map_err(map_dispatch_storage_error)?;
        self.dispatch
            .transition_agent(binding.worker.agent_id, AgentStatus::Exited, None)
            .map_err(map_dispatch_storage_error)?;
        Ok(())
    }

    /// Delivers a worker report only when the supplied completion fence is the
    /// exact current runtime fence.  Late, duplicate, or wrong-generation
    /// reports are safe no-ops, preserving the single inbox delivery.
    pub fn report(
        &mut self,
        runtime: &AgentRuntimeRef,
        candidate: &CompletionFence,
        kind: InboxKind,
        summary: String,
        result: Option<usagi_core::domain::agent::StructuredResult>,
    ) -> Result<(), ProtocolError> {
        if self.coordinator.require_outcome_owner(runtime).is_err() {
            return Ok(());
        }
        let record = self
            .coordinator
            .record_for(runtime)
            .map_err(map_runtime_error)?;
        if !record.operation.fences(candidate)
            || !matches!(kind, InboxKind::Completed | InboxKind::Failed)
        {
            return Ok(());
        }
        let Some(binding) = self
            .dispatch
            .binding(candidate.operation_id)
            .map_err(map_dispatch_storage_error)?
        else {
            return Ok(());
        };
        let inbox = self
            .dispatch
            .inbox(&binding.caller)
            .map_err(map_dispatch_storage_error)?;
        if inbox
            .iter()
            .any(|message| message.run_id == candidate.operation_id)
        {
            return Ok(());
        }
        self.dispatch
            .append_inbox(
                &binding.caller,
                InboxMessage {
                    run_id: candidate.operation_id,
                    from: binding.worker.clone(),
                    kind,
                    summary,
                    result,
                    created_at: Utc::now(),
                    read: false,
                },
            )
            .map_err(map_dispatch_storage_error)?;
        let status = if kind == InboxKind::Completed {
            RunStatus::Completed
        } else {
            RunStatus::Failed
        };
        self.dispatch
            .transition_run(candidate.operation_id, status, Some(Utc::now()))
            .map_err(map_dispatch_storage_error)?;
        let agent_status = if kind == InboxKind::Completed {
            AgentStatus::Idle
        } else {
            AgentStatus::Failed
        };
        self.dispatch
            .transition_agent(binding.worker.agent_id, agent_status, None)
            .map_err(map_dispatch_storage_error)?;
        Ok(())
    }

    /// Authenticates and delivers a completion report from a provisioned MCP
    /// child. An optional run ID is only an assertion about the authenticated
    /// current run; it never selects a different destination.
    pub fn report_from_mcp(
        &mut self,
        credential: &str,
        requested_run: Option<OperationId>,
        kind: InboxKind,
        summary: String,
        result: Option<usagi_core::domain::agent::StructuredResult>,
    ) -> Result<CallerRef, ProtocolError> {
        let caller = self
            .mcp_callers
            .get(credential)
            .cloned()
            .ok_or_else(unknown_caller_provenance)?;
        if requested_run.is_some_and(|run_id| run_id != caller.operation) {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "completion run does not match the authenticated worker",
            ));
        }
        let fence = self
            .coordinator
            .record_for(&caller.runtime)
            .map_err(map_runtime_error)?
            .operation
            .clone();
        let binding = self
            .dispatch
            .binding(caller.operation)
            .map_err(map_dispatch_storage_error)?
            .ok_or_else(dispatch_binding_unavailable)?;
        let delivered_to = binding.caller.clone();
        self.report(&caller.runtime, &fence, kind, summary, result)?;
        Ok(delivered_to)
    }

    fn dispatch_terminal(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        request_id: RequestId,
        action: TerminalAction,
        request: TerminalRequest,
        runtime: &AgentRuntimeRef,
    ) -> Result<Value, ProtocolError> {
        match (action, request) {
            (TerminalAction::Attach, TerminalRequest::Attach { .. }) => self
                .coordinator
                .attach(runtime, connection)
                .map(|attached| json!(attached))
                .map_err(map_runtime_error),
            (TerminalAction::Resume, TerminalRequest::Resume { after_offset, .. }) => {
                let output = self
                    .coordinator
                    .replay_from(runtime, after_offset)
                    .map_err(map_runtime_error)?;
                // Parity with the generic terminal Resume: a polling client
                // observes the hosting terminal's exit on the incremental poll,
                // not only on a resync snapshot. Without this an exited Agent's
                // pane tab is never dropped from the Closeup strip.
                let exited = self
                    .coordinator
                    .terminal_snapshot(runtime)
                    .map_err(map_runtime_error)?
                    .exited
                    .is_some();
                Ok(json!({ "output": output, "exited": exited }))
            }
            (TerminalAction::Resync, TerminalRequest::Resync { .. }) => self
                .coordinator
                .terminal_snapshot(runtime)
                .map(|snapshot| json!(snapshot))
                .map_err(map_runtime_error),
            (TerminalAction::Resize, TerminalRequest::Resize { geometry, .. }) => {
                let geometry = terminal_geometry(geometry)?;
                self.coordinator
                    .resize(runtime, geometry, &mut *self.pty)
                    .map(|snapshot| json!(snapshot))
                    .map_err(map_runtime_error)
            }
            (TerminalAction::Detach, TerminalRequest::Detach { subscription, .. }) => self
                .coordinator
                .detach(runtime, subscription, connection)
                .map(|()| json!({}))
                .map_err(map_runtime_error),
            (
                TerminalAction::Input,
                TerminalRequest::Input {
                    subscription,
                    input_seq,
                    bytes,
                    ..
                },
            ) => {
                self.pty.select_terminal(&runtime.terminal);
                self.coordinator
                    .input(
                        runtime,
                        InputRequest {
                            subscription,
                            connection,
                            client,
                            request: request_id,
                            input_seq,
                        },
                        &bytes,
                        &mut *self.pty,
                    )
                    .map(|ack| json!({ "ack": ack }))
                    .map_err(map_runtime_error)
            }
            _ => Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "terminal action does not match its payload",
            )),
        }
    }
}

impl AgentTerminalActor for AgentRuntime {
    fn handle_terminal(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        request_id: RequestId,
        action: TerminalAction,
        request: TerminalRequest,
    ) -> TerminalOutcome {
        let Some(terminal) = terminal_of(&request) else {
            return TerminalOutcome::NotOwned;
        };
        let Some(runtime) = self.coordinator.runtime_for_terminal(terminal) else {
            return TerminalOutcome::NotOwned;
        };
        TerminalOutcome::Handled(
            self.dispatch_terminal(connection, client, request_id, action, request, &runtime),
        )
    }

    fn terminal_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry> {
        self.coordinator.inventory(scope)
    }

    fn disconnect(&mut self, connection: ConnectionId) {
        self.coordinator.disconnect(connection);
    }
}

/// The daemon's sole terminal owner.  Terminal requests are routed to the Agent
/// owner when they address an Agent terminal, and otherwise to the generic
/// terminal owner (#264), so both share one ownership loop and vocabulary.
pub struct SharedTerminalOwner<G, A> {
    agent: A,
    generic: G,
}

impl<G, A> SharedTerminalOwner<G, A> {
    pub fn new(agent: A, generic: G) -> Self {
        Self { agent, generic }
    }
}

impl<G: TerminalOwner, A: AgentTerminalActor> TerminalOwner for SharedTerminalOwner<G, A> {
    fn request(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        request_id: RequestId,
        action: TerminalAction,
        payload: Value,
    ) -> Result<Value, ProtocolError> {
        // Inventory addresses no single terminal, so it is not routed by
        // `handle_terminal`. Merge both owners' in-scope runtimes here so a
        // restoring client discovers Agent and generic terminals together.
        if matches!(action, TerminalAction::Inventory) {
            let Ok(TerminalRequest::Inventory { scope }) =
                serde_json::from_value::<TerminalRequest>(payload.clone())
            else {
                return Err(ProtocolError::new(
                    ErrorCode::InvalidArgument,
                    "invalid terminal inventory scope",
                ));
            };
            let mut entries = self.generic.inventory(&scope);
            entries.extend(self.agent.terminal_inventory(&scope));
            return Ok(json!({ "terminals": entries }));
        }
        let routed = match serde_json::from_value::<TerminalRequest>(payload.clone()) {
            Ok(request) => self
                .agent
                .handle_terminal(connection, client, request_id, action, request),
            Err(_) => TerminalOutcome::NotOwned,
        };
        match routed {
            TerminalOutcome::Handled(result) => result,
            TerminalOutcome::NotOwned => self
                .generic
                .request(connection, client, request_id, action, payload),
        }
    }

    fn disconnect(&mut self, connection: ConnectionId) {
        self.agent.disconnect(connection);
        self.generic.disconnect(connection);
    }
}

fn terminal_of(request: &TerminalRequest) -> Option<&TerminalRef> {
    match request {
        TerminalRequest::Attach { terminal }
        | TerminalRequest::Resume { terminal, .. }
        | TerminalRequest::Resync { terminal }
        | TerminalRequest::Input { terminal, .. }
        | TerminalRequest::Resize { terminal, .. }
        | TerminalRequest::Detach { terminal, .. } => Some(terminal),
        TerminalRequest::Launch { .. } | TerminalRequest::Inventory { .. } => None,
    }
}

fn semantic_key(intent: &AgentLaunchIntent) -> String {
    format!(
        "{}:{}:{}",
        intent.workspace.as_str(),
        intent
            .session
            .map_or_else(|| "workspace-root".to_owned(), |session| session.as_str()),
        intent
            .profile
            .as_ref()
            .map_or_else(|| "<default>".to_owned(), ToString::to_string),
    )
}

fn provider_matches_profile(provider: ProviderKind, profile: &AgentProfileId) -> bool {
    matches!(
        (provider, profile.as_str()),
        (ProviderKind::Claude, "claude") | (ProviderKind::Codex, "codex")
    )
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
            | super::runtime::RuntimeState::ReconcileRequired(
                super::runtime::ReconcileState::IdentityUnknown
            )
    )
}

fn durable_operation_outcome(
    record: &super::runtime::DurableRuntimeRecord,
) -> Result<AgentAdmission, ProtocolError> {
    use super::runtime::DurableOperationOutcome;
    match record.outcome {
        DurableOperationOutcome::Accepted => Ok(AgentAdmission {
            operation_id: record.operation.operation_id.to_string(),
            revision: 1,
            terminal: record.runtime.terminal.clone(),
            completed: false,
        }),
        DurableOperationOutcome::Completed => Ok(AgentAdmission {
            operation_id: record.operation.operation_id.to_string(),
            revision: 1,
            terminal: record.runtime.terminal.clone(),
            completed: true,
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

fn terminal_geometry(
    geometry: usagi_core::usecase::client::TerminalGeometry,
) -> Result<Geometry, ProtocolError> {
    (geometry.cols > 0 && geometry.rows > 0)
        .then_some(Geometry {
            cols: geometry.cols,
            rows: geometry.rows,
        })
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "terminal geometry must be non-zero",
            )
        })
}

fn stale_terminal() -> ProtocolError {
    ProtocolError::new(ErrorCode::StaleTarget, "agent terminal reference is stale")
}

fn map_dispatch_storage_error(_: anyhow::Error) -> ProtocolError {
    ProtocolError::new(
        ErrorCode::Unavailable,
        "daemon could not persist dispatch state",
    )
}

fn dispatch_agent_not_found() -> ProtocolError {
    ProtocolError::new(ErrorCode::InvalidArgument, "dispatch agent was not found")
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

const fn runtime_phase(state: super::runtime::RuntimeState) -> (u8, &'static str) {
    use super::runtime::RuntimeState;
    match state {
        RuntimeState::Running => (4, "running"),
        RuntimeState::Reserved => (3, "ready"),
        RuntimeState::ReconcileRequired(super::runtime::ReconcileState::IdentityUnknown) => {
            (3, "interrupted")
        }
        RuntimeState::SpawnFailed | RuntimeState::ReconcileRequired(_) => (2, "exited"),
        RuntimeState::Exited | RuntimeState::Reclaimed => (1, "ended"),
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
        RuntimeError::Terminal(RegistryError::ResyncRequired) => (
            ErrorCode::ResyncRequired,
            "agent terminal output requires resynchronization",
        ),
        RuntimeError::Terminal(RegistryError::PtyResizeFailed) => {
            (ErrorCode::Unavailable, "terminal resize failed")
        }
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };

    use super::*;
    use crate::usecase::{
        claude::{ClaudeAdapter, ClaudeProvision, ClaudeProvisionFailure, ClaudeProvisioner},
        codex::{CodexAdapter, CodexProvision, CodexProvisionFailure, CodexProvisioner},
        generation::ProcessIdentity,
        runtime::{
            AdapterError, AgentAdapter, ProvisionContext, ResolvedLaunch, RuntimeStore,
            RuntimeStoreSnapshot, SpawnFailure, SpawnProvision,
        },
        terminal::{Output, PtyWriteError},
    };
    use usagi_core::domain::agent::{
        AgentCapability, AgentProfile, DurableLaunchSnapshot, LaunchPlan,
    };
    use usagi_core::usecase::client::TerminalGeometry;

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
            self.terminate_success
                .then_some(())
                .ok_or(super::super::runtime::TerminateReapError)
        }
    }
    impl PtyWriter for Pty {
        fn select_terminal(&mut self, terminal: &TerminalRef) {
            self.selected = Some(terminal.clone());
        }
        fn resize(
            &mut self,
            terminal: &TerminalRef,
            geometry: Geometry,
        ) -> Result<(), PtyWriteError> {
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
    }
    impl TerminalOwner for FakeGeneric {
        fn request(
            &mut self,
            _: ConnectionId,
            _: ClientId,
            _: RequestId,
            _: TerminalAction,
            _: Value,
        ) -> Result<Value, ProtocolError> {
            self.requests += 1;
            Ok(json!({ "generic": true }))
        }
        fn inventory(
            &self,
            _: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
        ) -> Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry> {
            self.inventory.clone()
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
        let adapter = ClaudeAdapter::new(FakeProvisioner);
        registry
            .register(adapter.profile().clone(), Box::new(adapter))
            .unwrap();
        registry
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
        let native_id = ProviderSessionId::new("structured-codex-session").unwrap();
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
            .capture_structured_provider_session(
                &first_runtime,
                ProviderKind::Codex,
                native_id.clone(),
            )
            .unwrap();
        assert_eq!(
            runtime
                .capture_structured_provider_session(
                    &first_runtime,
                    ProviderKind::Codex,
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
        assert_eq!(
            runtime
                .admit_resume(
                    &initial_operation.to_string(),
                    workspace,
                    session,
                    &format!("resume:{workspace}:{session}"),
                    &FakeScope(Ok(resolved.clone())),
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
                .resume(
                    &OperationId::new().to_string(),
                    workspace,
                    session,
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
                .resume(
                    &OperationId::new().to_string(),
                    workspace,
                    session,
                    &FakeScope(Ok(resolved.clone())),
                )
                .unwrap_err()
                .code,
            ErrorCode::Unavailable
        );
        runtime.registry = original_registry;

        pty_mut(&mut runtime).spawn = Some(SpawnFailure::Definite);
        assert_eq!(
            runtime
                .resume(
                    &OperationId::new().to_string(),
                    workspace,
                    session,
                    &FakeScope(Ok(resolved.clone())),
                )
                .unwrap_err()
                .code,
            ErrorCode::Unavailable
        );
        pty_mut(&mut runtime).spawn = None;
        assert_eq!(
            runtime
                .resume(
                    "not-an-operation-id",
                    workspace,
                    session,
                    &FakeScope(Ok(resolved.clone()))
                )
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            runtime
                .resume(
                    &OperationId::new().to_string(),
                    workspace,
                    session,
                    &FakeScope(Ok(scope())),
                )
                .unwrap_err()
                .code,
            ErrorCode::OwnershipUnknown
        );
        let operation = OperationId::new().to_string();
        let resumed = runtime
            .resume(
                &operation,
                workspace,
                session,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap();
        assert_ne!(resumed.terminal, first.terminal);
        assert_eq!(
            runtime
                .resume(
                    &operation,
                    workspace,
                    session,
                    &FakeScope(Ok(resolved.clone())),
                )
                .unwrap()
                .terminal,
            resumed.terminal
        );
        assert_eq!(
            runtime
                .resume(
                    &operation,
                    WorkspaceId::new(),
                    session,
                    &FakeScope(Ok(resolved.clone())),
                )
                .unwrap_err()
                .code,
            ErrorCode::IdempotencyConflict
        );
        assert_eq!(
            runtime
                .resume(
                    &OperationId::new().to_string(),
                    workspace,
                    session,
                    &FakeScope(Ok(resolved)),
                )
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(runtime.coordinator.snapshot().records.len(), 3);
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
        assert_eq!(
            runtime
                .resume(
                    &OperationId::new().to_string(),
                    workspace,
                    session,
                    &FakeScope(Ok(resolved)),
                )
                .unwrap_err()
                .code,
            ErrorCode::Unavailable
        );
    }

    #[test]
    fn restart_resume_supersedes_the_interrupted_runtime_without_leaking_capacity() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let resolved = scope();
        let make_runtime = || {
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
        };
        let mut first = make_runtime();
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
        let (reconciled, interrupted) = first
            .coordinator
            .snapshot()
            .reconcile_after_daemon_restart();
        assert_eq!(interrupted, 1);

        let mut second = AgentRuntime::hydrate_with_dispatch_and_locator(
            DaemonGeneration::new(),
            claude_registry(),
            Store::default(),
            Journal::default(),
            Pty::default(),
            AgentProfileId::new("claude").unwrap(),
            Geometry { cols: 80, rows: 24 },
            DispatchStore::new(tempfile::tempdir().unwrap().keep()),
            PathExecutableLocator,
            reconciled,
        )
        .unwrap();
        assert_eq!(second.session_phase(session), "interrupted");
        assert_eq!(second.coordinator.occupied_slots(), 1);

        let resumed = second
            .resume(
                &OperationId::new().to_string(),
                workspace,
                session,
                &FakeScope(Ok(resolved)),
            )
            .unwrap();
        assert_ne!(resumed.terminal, initial.terminal);
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

        second.exit(&resumed.terminal, 0).unwrap();
        assert_eq!(second.coordinator.occupied_slots(), 0);
        assert_eq!(second.session_phase(session), "ended");
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
                DispatchStore::new(dispatch_dir.clone()),
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
            DispatchStore::new(dispatch_dir),
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
        let inventory = second.coordinator.inventory(
            &usagi_core::domain::terminal_launch::TerminalLaunchScope {
                workspace_id: workspace,
                session_id: Some(session),
                worktree_id: resolved.worktree_id,
            },
        );
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
                .filter(|record| {
                    record.role == super::super::generation::GenerationRole::Active
                })
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
                .filter(|record| {
                    record.role == super::super::generation::GenerationRole::Active
                })
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
    fn queued_prompt_is_consumed_by_launch_and_auto_then_delivers_live() {
        let mut runtime = runtime();
        let launch_intent = intent(None);
        let session = launch_intent.session.unwrap();
        assert_eq!(runtime.session_phase(session), "none");
        assert_eq!(
            runtime
                .prompt(Some(session), "  ", PromptMode::Auto)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            runtime
                .prompt(Some(session), "now", PromptMode::Live)
                .unwrap_err()
                .code,
            ErrorCode::Unavailable
        );
        let queued = runtime
            .prompt(Some(session), "queued work", PromptMode::Auto)
            .unwrap();
        assert_eq!(queued.delivered_to, "queue");
        assert!(
            runtime
                .dispatch
                .queued_prompt(Some(session))
                .unwrap()
                .is_some()
        );

        let operation = OperationId::new();
        runtime
            .launch(
                &operation.to_string(),
                &launch_intent,
                &FakeScope(Ok(scope())),
            )
            .unwrap();
        assert_eq!(runtime.session_phase(session), "running");
        assert!(
            runtime
                .dispatch
                .queued_prompt(Some(session))
                .unwrap()
                .is_none()
        );
        let credential = runtime.mcp_callers.keys().next().unwrap().clone();
        assert_eq!(runtime.caller_session(&credential), Some(session));

        let live = runtime
            .prompt(Some(session), "follow up", PromptMode::Auto)
            .unwrap();
        assert_eq!(live.delivered_to, "live");
        assert_eq!(pty(&runtime).writes, b"follow up\n");
        let fenced = runtime.prompt_run(operation, "decision answer").unwrap();
        assert_eq!(fenced.delivered_to, "live");
        assert_eq!(pty(&runtime).writes, b"follow up\ndecision answer\n");
        assert!(runtime.prompt_run(OperationId::new(), "late").is_err());
        assert_eq!(
            runtime.prompt_run(operation, "  ").unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert!(
            runtime
                .prompt(Some(session), "later", PromptMode::Queue)
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
                .prompt(Some(session), "fails", PromptMode::Live)
                .unwrap_err()
                .code,
            ErrorCode::Unavailable
        );
        assert!(runtime.prompt(None, "now", PromptMode::Live).is_err());
        assert!(runtime.prompt(None, "  ", PromptMode::Auto).is_err());
    }

    #[test]
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
            },
        ));
        assert_eq!(attached["snapshot"]["replay"], json!(b"ready\n".to_vec()));
        let subscription = attached["subscription"].as_u64().unwrap();

        handled(runtime.handle_terminal(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Resize,
            TerminalRequest::Resize {
                terminal: terminal.clone(),
                geometry: TerminalGeometry { cols: 43, rows: 17 },
            },
        ));
        assert_eq!(
            pty(&runtime).resized,
            vec![(terminal.clone(), Geometry { cols: 43, rows: 17 })]
        );

        let ack = handled(runtime.handle_terminal(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Input,
            TerminalRequest::Input {
                terminal: terminal.clone(),
                subscription,
                input_seq: 0,
                bytes: b"go\n".to_vec(),
            },
        ));
        assert_eq!(ack["ack"], "Written");

        handled(runtime.handle_terminal(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Detach,
            TerminalRequest::Detach {
                terminal: terminal.clone(),
                subscription,
            },
        ));
        // A disconnect drops only subscriptions; the process/PTY stay alive.
        runtime.disconnect(connection);

        let reattached = handled(runtime.handle_terminal(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Attach,
            TerminalRequest::Attach {
                terminal: terminal.clone(),
            },
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
            },
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
        let inbox = runtime.dispatch_store().inbox(&caller).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].kind, InboxKind::NoReport);
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
            commits: vec!["abc".into()],
            ..Default::default()
        };
        assert_eq!(
            runtime
                .report_from_mcp(
                    &credential,
                    None,
                    InboxKind::Completed,
                    "done".into(),
                    Some(result.clone()),
                )
                .unwrap(),
            caller
        );
        runtime
            .report_from_mcp(
                &credential,
                None,
                InboxKind::Completed,
                "duplicate".into(),
                None,
            )
            .unwrap();
        runtime.exit(&admission.terminal, 0).unwrap();
        let inbox = runtime.dispatch_store().inbox(&caller).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].kind, InboxKind::Completed);
        assert_eq!(inbox[0].result, Some(result));

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
        let worktree = tempfile::tempdir().unwrap();
        let scope = configured_scope(worktree.path());
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
                &FakeScope(Ok(scope.clone())),
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
                &FakeScope(Ok(scope.clone())),
            )
            .unwrap_err();
        assert_eq!(unavailable.code, ErrorCode::Unavailable);
        assert_eq!(runtime.coordinator.occupied_slots(), 1);

        std::fs::write(&executable, "fixture").unwrap();
        std::fs::write(
            worktree.path().join(".usagi/config.toml"),
            "[agents.claude]\nmodels = [\"other\"]\n",
        )
        .unwrap();
        let rejected = runtime
            .dispatch(
                &OperationId::new().to_string(),
                &dispatch("test"),
                session,
                &FakeScope(Ok(scope)),
            )
            .unwrap_err();
        assert_eq!(rejected.code, ErrorCode::InvalidArgument);
        assert_eq!(runtime.coordinator.occupied_slots(), 1);
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
                    terminal: foreign.clone()
                },
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
                })
                .unwrap(),
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
                    intent: usagi_core::usecase::client::TerminalLaunchIntent {
                        request: usagi_core::domain::terminal_launch::TerminalLaunchRequest {
                            profile_id:
                                usagi_core::domain::terminal_launch::TerminalProfileId::new(
                                    "login-shell",
                                )
                                .unwrap(),
                            scope: usagi_core::domain::terminal_launch::TerminalLaunchScope {
                                workspace_id: WorkspaceId::new(),
                                session_id: Some(SessionId::new()),
                                worktree_id: WorktreeId::new(),
                            },
                        },
                        geometry: usagi_core::usecase::client::TerminalGeometry {
                            cols: 80,
                            rows: 24,
                        },
                    },
                })
                .unwrap(),
            )
            .unwrap();
        assert_eq!(generic["generic"], true);

        // Unparseable payload → generic owner.
        owner
            .request(
                connection,
                client,
                RequestId::new(),
                TerminalAction::Attach,
                json!({ "operation": "bogus" }),
            )
            .unwrap();

        owner.disconnect(connection);
        assert_eq!(owner.generic.requests, 2);
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
            entry.kind == TerminalKind::Agent
                && entry.terminal.fences(&agent_terminal)
                && entry.live
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
            (super::super::runtime::RuntimeState::Running, "running"),
            (super::super::runtime::RuntimeState::Reserved, "ready"),
            (super::super::runtime::RuntimeState::SpawnFailed, "exited"),
            (
                super::super::runtime::RuntimeState::ReconcileRequired(
                    ReconcileState::IdentityUnknown,
                ),
                "interrupted",
            ),
            (
                super::super::runtime::RuntimeState::ReconcileRequired(
                    ReconcileState::OrphanRunning,
                ),
                "exited",
            ),
            (super::super::runtime::RuntimeState::Exited, "ended"),
            (super::super::runtime::RuntimeState::Reclaimed, "ended"),
        ] {
            assert_eq!(runtime_phase(state).1, expected);
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
        let durable = DispatchStore::new(&dispatch_dir);
        let worker = durable
            .upsert_agent_by_runtime_model(
                Some(session),
                AgentProfileId::new("claude").unwrap(),
                ModelSelector::new("test").unwrap(),
            )
            .unwrap();
        let intent = DispatchIntent {
            workspace: WorkspaceId::new(),
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

    fn handled(outcome: TerminalOutcome) -> Value {
        handled_result(outcome).unwrap()
    }

    fn handled_result(outcome: TerminalOutcome) -> Result<Value, ProtocolError> {
        match outcome {
            TerminalOutcome::Handled(result) => result,
            TerminalOutcome::NotOwned => Err(stale_terminal()),
        }
    }
}
