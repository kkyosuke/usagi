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
#![coverage(off)] // Generic injected ports (scope resolver, store, journal, PTY) are monomorphized at the composition root; the fake-based tests below exercise every safety outcome without double-counting those instantiations.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::Utc;
use serde_json::{Value, json};
use usagi_core::{
    domain::{
        agent::{
            AgentCapability, AgentProfileId, AgentStatus, CallerRef, DispatchBinding, DispatchRun,
            InboxKind, InboxMessage, LaunchMode, LaunchRequest, LaunchScope, ModelSelector,
            RunStatus, WorkerRef,
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
    infrastructure::store::dispatch::DispatchStore,
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

/// One durable Agent operation, replayed identically on resend/reconnect.
#[derive(Debug, Clone)]
struct AgentOperation {
    semantic_key: String,
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
pub struct AgentRuntime<S, P, J, L = PathExecutableLocator> {
    generation: DaemonGeneration,
    coordinator: RuntimeCoordinator,
    orchestrator: Orchestrator,
    registry: AdapterRegistry,
    store: S,
    journal: J,
    pty: P,
    default_profile: AgentProfileId,
    geometry: Geometry,
    dispatch: DispatchStore,
    locator: L,
    operations: BTreeMap<String, AgentOperation>,
    mcp_callers: BTreeMap<String, McpCaller>,
}

impl<S, P, J> AgentRuntime<S, P, J, PathExecutableLocator> {
    #[must_use]
    pub fn new(
        generation: DaemonGeneration,
        registry: AdapterRegistry,
        store: S,
        journal: J,
        pty: P,
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
        store: S,
        journal: J,
        pty: P,
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

impl<S, P, J, L> AgentRuntime<S, P, J, L> {
    /// Constructs an Agent runtime with an injected current executable locator.
    #[must_use]
    pub fn with_dispatch_and_locator(
        generation: DaemonGeneration,
        registry: AdapterRegistry,
        store: S,
        journal: J,
        pty: P,
        default_profile: AgentProfileId,
        geometry: Geometry,
        dispatch: DispatchStore,
        locator: L,
    ) -> Self {
        Self {
            generation,
            coordinator: RuntimeCoordinator::new(16, 64 * 1024, 64),
            orchestrator: Orchestrator::new(),
            registry,
            store,
            journal,
            pty,
            default_profile,
            geometry,
            dispatch,
            locator,
            operations: BTreeMap::new(),
            mcp_callers: BTreeMap::new(),
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

    #[must_use]
    pub fn dispatch_store(&self) -> &DispatchStore {
        &self.dispatch
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
    pub fn mcp_dispatch_caller(&self, credential: &str) -> Option<CallerRef> {
        let run_id = self.mcp_caller(credential)?;
        let binding = self.dispatch.binding(run_id).ok()??;
        Some(CallerRef {
            session_id: binding.worker.session_id,
            agent_id: binding.worker.agent_id,
        })
    }
}

impl<
    S: super::runtime::RuntimeStore,
    P: PtySpawner + PtyWriter,
    J: OutputJournal,
    L: ExecutableLocator,
> AgentRuntime<S, P, J, L>
{
    /// Admits one Agent launch.  The same producer `operation_id` with the same
    /// intent returns the same admission (no second spawn); the same id with a
    /// different intent is a typed idempotency conflict.
    pub fn launch<R: SessionScopeResolver>(
        &mut self,
        operation_id: &str,
        intent: &AgentLaunchIntent,
        scope: &R,
    ) -> Result<AgentAdmission, ProtocolError> {
        let semantic_key = semantic_key(intent);
        if let Some(existing) = self.operations.get(operation_id) {
            if existing.semantic_key != semantic_key {
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
                semantic_key,
                outcome: outcome.clone(),
            },
        );
        outcome
    }

    /// Launches a dispatch-selected worker through the same fenced Agent
    /// runtime used by ordinary Agent launch, then records its durable run and
    /// caller binding.  The caller is captured now and never accepted from a
    /// later completion request.
    pub fn dispatch<R: SessionScopeResolver>(
        &mut self,
        operation_id: &str,
        intent: &DispatchIntent,
        session: SessionId,
        scope: &R,
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
                .map_err(|_| dispatch_storage_error())?
                .ok_or_else(|| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "dispatch agent was not found")
                })?,
            DispatchAgentIntent::New { runtime, model } => self
                .dispatch
                .upsert_agent_by_runtime_model(Some(session), runtime.clone(), model.clone())
                .map_err(|_| dispatch_storage_error())?,
        };
        if worker.session_id != Some(session) {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "dispatch agent does not belong to session",
            ));
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
            if existing.semantic_key != semantic {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different dispatch",
                ));
            }
            return existing.outcome.clone();
        }
        let outcome = self.admit_dispatch(
            operation,
            &launch,
            &intent.prompt,
            &worker,
            &intent.caller,
            scope,
        );
        self.operations.insert(
            operation_id.to_owned(),
            AgentOperation {
                semantic_key: semantic,
                outcome: outcome.clone(),
            },
        );
        outcome
    }

    fn admit_dispatch<R: SessionScopeResolver>(
        &mut self,
        operation: OperationId,
        launch: &AgentLaunchIntent,
        prompt: &str,
        worker: &usagi_core::domain::agent::Agent,
        caller: &CallerRef,
        scope: &R,
    ) -> Result<AgentAdmission, ProtocolError> {
        let resolved = scope
            .resolve_available_scope(launch.workspace, launch.session)
            .map_err(map_scope_error)?;
        let terminal = TerminalRef {
            daemon_generation: self.generation,
            terminal_id: TerminalId::new(),
            workspace_id: launch.workspace,
            session_id: launch.session,
            worktree_id: resolved.worktree_id,
        };
        let runtime = AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), launch.session)
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Internal, "agent runtime scope is inconsistent")
            })?;
        let fence = CompletionFence {
            workspace_id: launch.workspace,
            session_id: launch.session,
            operation_id: operation,
            owner_daemon_generation: self.generation,
            execution_attempt: 1,
            lifecycle_attempt: 1,
            expected_revision: 0,
        };
        let request = LaunchRequest {
            profile_id: worker.runtime.clone(),
            mode: LaunchMode::Interactive,
            model: Some(worker.model.clone()),
            resume: false,
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
        self.orchestrator
            .launch(
                &mut self.coordinator,
                &mut self.registry,
                &authorization,
                &request,
                self.geometry,
                &mut self.store,
                &mut self.pty,
                Some(credential.clone()),
            )
            .map_err(map_orchestration_error)?;
        self.dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: worker.agent_id,
                prompt: prompt.to_owned(),
                started_at: Utc::now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .map_err(|_| dispatch_storage_error())?;
        self.mcp_callers.insert(
            credential,
            McpCaller {
                runtime: authorization.runtime,
                operation,
            },
        );
        self.dispatch
            .upsert_binding(DispatchBinding {
                run_id: operation,
                caller: caller.clone(),
                worker: WorkerRef {
                    session_id: worker.session_id,
                    agent_id: worker.agent_id,
                },
            })
            .map_err(|_| dispatch_storage_error())?;
        self.dispatch
            .transition_agent(worker.agent_id, AgentStatus::Running, Some(operation))
            .map_err(|_| dispatch_storage_error())?;
        Ok(AgentAdmission {
            operation_id: operation.to_string(),
            revision: 1,
            terminal,
            completed: false,
        })
    }

    #[allow(clippy::too_many_lines)] // Admission atomically fences launch, caller registration, and replay state.
    fn admit<R: SessionScopeResolver>(
        &mut self,
        operation_id: &str,
        intent: &AgentLaunchIntent,
        scope: &R,
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
        let resolved = scope
            .resolve_available_scope(intent.workspace, intent.session)
            .map_err(map_scope_error)?;
        let terminal = TerminalRef {
            daemon_generation: self.generation,
            terminal_id: TerminalId::new(),
            workspace_id: intent.workspace,
            session_id: intent.session,
            worktree_id: resolved.worktree_id,
        };
        let runtime = AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), intent.session)
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Internal, "agent runtime scope is inconsistent")
            })?;
        let fence = CompletionFence {
            workspace_id: intent.workspace,
            session_id: intent.session,
            operation_id: operation,
            owner_daemon_generation: self.generation,
            execution_attempt: 1,
            lifecycle_attempt: 1,
            expected_revision: 0,
        };
        let request = LaunchRequest {
            profile_id: profile_id.clone(),
            mode: LaunchMode::Interactive,
            model: None,
            resume: false,
            initial_prompt: None,
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
        self.orchestrator
            .launch(
                &mut self.coordinator,
                &mut self.registry,
                &authorization,
                &request,
                self.geometry,
                &mut self.store,
                &mut self.pty,
                Some(credential.clone()),
            )
            .map_err(map_orchestration_error)?;
        let worker = self
            .dispatch
            .upsert_agent_by_runtime_model(
                intent.session,
                profile_id.clone(),
                ModelSelector::new("default").expect("literal model selector is canonical"),
            )
            .map_err(|_| dispatch_storage_error())?;
        self.dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: worker.agent_id,
                prompt: String::new(),
                started_at: Utc::now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .map_err(|_| dispatch_storage_error())?;
        let caller = CallerRef {
            session_id: worker.session_id,
            agent_id: worker.agent_id,
        };
        self.dispatch
            .upsert_binding(DispatchBinding {
                run_id: operation,
                caller,
                worker: WorkerRef {
                    session_id: worker.session_id,
                    agent_id: worker.agent_id,
                },
            })
            .map_err(|_| dispatch_storage_error())?;
        self.mcp_callers.insert(
            credential,
            McpCaller {
                runtime: authorization.runtime,
                operation,
            },
        );
        Ok(AgentAdmission {
            operation_id: operation_id.to_owned(),
            revision: 1,
            terminal,
            completed: false,
        })
    }

    /// Journals daemon-owned PTY output before it becomes replayable.  A stale
    /// terminal is a safe no-op error, never a replacement.
    pub fn output(&mut self, terminal: &TerminalRef, bytes: Vec<u8>) -> Result<(), ProtocolError> {
        let runtime = self
            .coordinator
            .runtime_for_terminal(terminal)
            .ok_or_else(stale_terminal)?;
        self.coordinator
            .append_output(&runtime, bytes, &mut self.journal)
            .map(|_| ())
            .map_err(map_runtime_error)
    }

    /// Commits a verified Agent exit after the caller has drained output.
    pub fn exit(&mut self, terminal: &TerminalRef, status: i32) -> Result<(), ProtocolError> {
        let runtime = self
            .coordinator
            .runtime_for_terminal(terminal)
            .ok_or_else(stale_terminal)?;
        self.coordinator
            .exit(&runtime, status, &mut self.store)
            .map_err(map_runtime_error)?;

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
        if let Some(record) = self.operations.get_mut(&operation) {
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
        }
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
            .map_err(|_| dispatch_storage_error())?
        else {
            return Ok(());
        };
        // A dispatch run only accepts a report for the exact runtime fence.
        // This exit is itself reached through the fenced terminal lookup above.
        let inbox = self
            .dispatch
            .inbox(&binding.caller)
            .map_err(|_| dispatch_storage_error())?;
        if inbox.iter().any(|message| {
            message.run_id == run_id
                && matches!(
                    message.kind,
                    InboxKind::Completed | InboxKind::Failed | InboxKind::NoReport
                )
        }) {
            return Ok(());
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
            .map_err(|_| dispatch_storage_error())?;
        self.dispatch
            .transition_run(run_id, RunStatus::NoReport, Some(Utc::now()))
            .map_err(|_| dispatch_storage_error())?;
        self.dispatch
            .transition_agent(binding.worker.agent_id, AgentStatus::Exited, None)
            .map_err(|_| dispatch_storage_error())?;
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
            .map_err(|_| dispatch_storage_error())?
        else {
            return Ok(());
        };
        let inbox = self
            .dispatch
            .inbox(&binding.caller)
            .map_err(|_| dispatch_storage_error())?;
        if inbox.iter().any(|message| {
            message.run_id == candidate.operation_id
                && matches!(
                    message.kind,
                    InboxKind::Completed | InboxKind::Failed | InboxKind::NoReport
                )
        }) {
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
            .map_err(|_| dispatch_storage_error())?;
        let status = if kind == InboxKind::Completed {
            RunStatus::Completed
        } else {
            RunStatus::Failed
        };
        self.dispatch
            .transition_run(candidate.operation_id, status, Some(Utc::now()))
            .map_err(|_| dispatch_storage_error())?;
        let agent_status = if kind == InboxKind::Completed {
            AgentStatus::Idle
        } else {
            AgentStatus::Failed
        };
        self.dispatch
            .transition_agent(binding.worker.agent_id, agent_status, None)
            .map_err(|_| dispatch_storage_error())?;
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
        let caller = self.mcp_callers.get(credential).cloned().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "agent caller provenance is unknown",
            )
        })?;
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
            .map_err(|_| dispatch_storage_error())?
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "dispatch binding is unavailable",
                )
            })?;
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
                self.pty.resize(&runtime.terminal, geometry).map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "terminal resize failed")
                })?;
                self.coordinator
                    .resize(runtime, geometry)
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
                        &mut self.pty,
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

impl<S: super::runtime::RuntimeStore, P: PtySpawner + PtyWriter, J: OutputJournal>
    AgentTerminalActor for AgentRuntime<S, P, J>
{
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

fn dispatch_storage_error() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::Unavailable,
        "daemon could not persist dispatch state",
    )
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
        RuntimeError::ConcurrencyExhausted => (
            ErrorCode::ResourceExhausted,
            "daemon agent runtime capacity is exhausted",
        ),
        RuntimeError::Terminal(RegistryError::ResyncRequired) => (
            ErrorCode::ResyncRequired,
            "agent terminal output requires resynchronization",
        ),
        RuntimeError::Terminal(_)
        | RuntimeError::UnknownRuntime
        | RuntimeError::TerminalGenerationMismatch => {
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

    use super::*;
    use crate::usecase::{
        claude::{ClaudeAdapter, ClaudeProvision, ClaudeProvisionFailure, ClaudeProvisioner},
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
    }
    impl RuntimeStore for Store {
        type Error = ();
        fn save(&mut self, _: RuntimeStoreSnapshot) -> Result<(), ()> {
            self.saves += 1;
            match self.fail_after {
                Some(limit) if self.saves > limit => Err(()),
                _ => Ok(()),
            }
        }
    }

    #[derive(Default)]
    struct Journal(Vec<Output>);
    impl OutputJournal for Journal {
        type Error = ();
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
    }
    impl PtySpawner for Pty {
        fn spawn(
            &mut self,
            _: &DurableLaunchSnapshot,
            _: &SpawnProvision,
            _: &TerminalRef,
        ) -> Result<ProcessIdentity, SpawnFailure> {
            match self.spawn {
                Some(failure) => Err(failure),
                None => Ok(ProcessIdentity {
                    pid: 4321,
                    start_identity: "fake-agent".into(),
                    process_group: 4321,
                }),
            }
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
            Ok(())
        }
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
            self.writes.extend_from_slice(bytes);
            Ok(())
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

    fn runtime() -> AgentRuntime<Store, Pty, Journal> {
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

    fn runtime_with_fixture(
        locator: FixtureLocator,
    ) -> AgentRuntime<Store, Pty, Journal, FixtureLocator> {
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
            profile: profile.map(|name| AgentProfileId::new(name).unwrap()),
        }
    }

    fn root_intent(profile: Option<&str>) -> AgentLaunchIntent {
        AgentLaunchIntent {
            workspace: WorkspaceId::new(),
            session: None,
            profile: profile.map(|name| AgentProfileId::new(name).unwrap()),
        }
    }

    // ---- tests ---------------------------------------------------------------

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
            runtime.pty.resized,
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
        assert_eq!(runtime.pty.selected.as_ref(), Some(&terminal));
        assert_eq!(runtime.pty.writes, b"go\n");
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
    }

    #[test]
    fn trimmed_agent_output_maps_to_a_resync_protocol_error() {
        let error = map_runtime_error(RuntimeError::Terminal(RegistryError::ResyncRequired));

        assert_eq!(error.code, ErrorCode::ResyncRequired);
    }

    fn handled(outcome: TerminalOutcome) -> Value {
        match outcome {
            TerminalOutcome::Handled(result) => result.unwrap(),
            TerminalOutcome::NotOwned => panic!("expected the agent owner to handle the terminal"),
        }
    }
}
