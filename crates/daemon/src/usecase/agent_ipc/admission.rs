//! Agent の受け入れ判定（admission）と容量確保、commit。

use anyhow::Result;

use super::{
    AgentAdmission, AgentAdmissionReservation, AgentCapability, AgentId, AgentLaunchIntent,
    AgentPhase, AgentResumeRelation, AgentResumeTarget, AgentRuntime, AgentRuntimeId,
    AgentRuntimeRef, AgentStatus, BTreeSet, CallerRef, CompletionFence, DispatchBinding,
    DispatchCredentialProvenance, DispatchRun, ErrorCode, LaunchMode, LaunchRequest, LaunchScope,
    McpCaller, ModelSelector, OperationId, ProtocolError, ProviderResumeReason, RunStatus,
    RuntimeAuthorization, SessionScopeResolver, TerminalId, TerminalRef, Utc, WorkerRef,
    agent_operation_digest, dispatch_admission_incomplete, dispatch_agent_not_found,
    dispatch_binding_unavailable, durable_operation_outcome, is_resume_source_state,
    map_dispatch_storage_error, map_orchestration_error, map_runtime_error, map_scope_error,
};

impl AgentRuntime {
    /// Frees one slot at saturation by sleeping the oldest completed turn that
    /// can be resumed exactly. Running, waiting, and merely ready Agents are
    /// never selected automatically.
    pub(super) fn sleep_one_for_capacity(&mut self) -> Result<bool, ProtocolError> {
        if !self.coordinator.concurrency().is_saturated() {
            return Ok(false);
        }
        let records = self.coordinator.snapshot().records;
        let mut candidate = None;
        for record in &records {
            let eligible = record.state == crate::usecase::runtime::RuntimeState::Running
                && self
                    .reported_phases
                    .get(&record.runtime.agent_runtime_id)
                    .is_some_and(|phase| *phase == AgentPhase::Ended)
                && self.resume_source_availability(record, &records).0;
            if !eligible {
                continue;
            }
            match candidate {
                None => candidate = Some(record),
                Some(old) if record.operation.operation_id < old.operation.operation_id => {
                    candidate = Some(record);
                }
                Some(_) => {}
            }
        }
        let Some(candidate) = candidate else {
            return Ok(false);
        };
        let runtime_ids = [candidate.runtime.agent_runtime_id.as_str()]
            .into_iter()
            .collect::<BTreeSet<_>>();
        self.sleep_runtime_ids(&runtime_ids).map(|count| count == 1)
    }

    #[allow(clippy::too_many_lines)] // Admission keeps its durable prepare/spawn/commit order visible.
    pub(super) fn admit_dispatch(
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
                dispatch_admission_incomplete()
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
            required_capabilities: [AgentCapability::McpWiring, AgentCapability::SystemPrompt]
                .into_iter()
                .collect(),
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
        self.sleep_one_for_capacity()?;
        self.dispatch
            .reserve_admission_for_workspace(
                launch.workspace,
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
                child: None,
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
            let _ = self
                .coordinator
                .fail_reserved_launch(&authorization.runtime, &mut *self.store);
            let _ = self.dispatch.fail_admission(operation);
            return Err(map_orchestration_error(error));
        }
        self.commit_admission(operation, &credential, &authorization.runtime)?;
        Ok(AgentAdmission {
            operation_id: operation.to_string(),
            revision: 1,
            runtime: authorization.runtime.clone(),
            terminal,
            continuation: self
                .coordinator
                .record_for(&authorization.runtime)
                .ok()
                .and_then(|record| record.continuation),
            resume_relation: None,
            completed: false,
            semantic_digest: Some(agent_operation_digest(semantic_key)),
        })
    }

    #[allow(clippy::too_many_lines)] // Admission atomically fences launch, caller registration, and replay state.
    pub(super) fn admit_resume_exact(
        &mut self,
        operation_id: &str,
        target: &AgentResumeTarget,
        semantic_key: &str,
        scope: &dyn SessionScopeResolver,
        repair_revision: Option<u32>,
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
        let records = self.coordinator.snapshot().records;
        let source = records
            .iter()
            .find(|record| record.runtime.agent_runtime_id == target.runtime_id)
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::StaleTarget, "agent resume target is stale")
            })?;
        if source.continuation != Some(target.continuation)
            || source.resume_source != Some(target.source)
            || source.runtime.terminal.workspace_id != target.workspace_id
            || source.runtime.session_id != target.session_id
            || source.runtime.terminal.session_id != target.session_id
            || source.runtime.terminal.worktree_id != target.worktree_id
            || source.launch.request.scope.workspace_id != target.workspace_id
            || source.launch.request.scope.session_id != target.session_id
            || source.launch.request.scope.worktree_id != target.worktree_id
            || source.launch.plan.profile_revision != target.adapter_revision
        {
            return Err(ProtocolError::new(
                ErrorCode::StaleTarget,
                "agent resume target fences do not match durable state",
            ));
        }
        if !is_resume_source_state(source.state) {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "agent resume source is still live or is not interrupted",
            ));
        }
        if let Some(replacement_id) = source.superseded_by {
            let Some(replacement) = records
                .iter()
                .find(|record| record.runtime.agent_runtime_id == replacement_id)
            else {
                return Err(ProtocolError::new(
                    ErrorCode::StaleTarget,
                    "agent resume replacement history was collected",
                ));
            };
            debug_assert_eq!(replacement.resumed_from, Some(target.source));
            debug_assert_eq!(replacement.continuation, Some(target.continuation));
            return durable_operation_outcome(replacement);
        }
        let (available, reason) = match repair_revision {
            Some(revision) => self.repair_resume_source_availability(source, &records, revision),
            None => self.resume_source_availability(source, &records),
        };
        if !available {
            let (code, message) = match reason {
                ProviderResumeReason::LiveOrOwnershipUnknown => (
                    ErrorCode::OwnershipUnknown,
                    "agent resume replacement is live or ownership is unknown",
                ),
                _ => (ErrorCode::Unavailable, "agent resume source is unavailable"),
            };
            return Err(ProtocolError::new(code, message));
        }
        let resolved = scope
            .resolve_available_scope(target.workspace_id, target.session_id)
            .map_err(map_scope_error)?;
        if resolved.worktree_id != target.worktree_id {
            return Err(ProtocolError::new(
                ErrorCode::StaleTarget,
                "agent resume worktree incarnation is stale",
            ));
        }
        let mut reference = source
            .provider_resume
            .as_ref()
            .expect("available exact source has provider resume metadata")
            .clone();
        if let Some(revision) = repair_revision {
            reference.adapter_revision = revision;
        }
        let profile_id = source.launch.plan.profile_id.clone();
        let terminal = TerminalRef {
            daemon_generation: self.active_generation()?,
            terminal_id: TerminalId::new(),
            workspace_id: target.workspace_id,
            session_id: target.session_id,
            worktree_id: resolved.worktree_id,
        };
        let runtime =
            AgentRuntimeRef::new(AgentRuntimeId::new(), terminal.clone(), target.session_id)
                .expect("terminal and runtime session are constructed from the same resume");
        let fence = CompletionFence {
            workspace_id: target.workspace_id,
            session_id: target.session_id,
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
                workspace_id: target.workspace_id,
                session_id: target.session_id,
                worktree_id: resolved.worktree_id,
            },
            required_capabilities: [AgentCapability::McpWiring, AgentCapability::SystemPrompt]
                .into_iter()
                .collect(),
        };
        let superseded = [source.runtime.clone()];
        let authorization = RuntimeAuthorization {
            runtime,
            operation: fence,
            mcp_allowed: true,
        };
        let credential = OperationId::new().to_string();
        // A tuple no longer identifies an Agent: same-session peers may use
        // the same provider/model. Preserve the exact source's mailbox identity.
        let source_binding = self
            .dispatch
            .binding(source.operation.operation_id)
            .map_err(map_dispatch_storage_error)?
            .ok_or_else(dispatch_binding_unavailable)?;
        let mut worker = self
            .dispatch
            .agent_in_workspace(target.workspace_id, source_binding.worker.agent_id)
            .map_err(map_dispatch_storage_error)?
            .filter(|worker| worker.session_id == target.session_id && worker.runtime == profile_id)
            .ok_or_else(dispatch_agent_not_found)?;
        self.require_peer_stopped_except(worker.agent_id, Some(source.operation.operation_id))?;
        worker.status = AgentStatus::Starting;
        worker.current_run = Some(operation);
        let caller = CallerRef {
            session_id: worker.session_id,
            agent_id: worker.agent_id,
        };
        self.sleep_one_for_capacity()?;
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
                child: None,
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
            let _ = self
                .coordinator
                .fail_reserved_launch(&authorization.runtime, &mut *self.store);
            let _ = self.dispatch.fail_admission(operation);
            return Err(map_orchestration_error(error));
        }
        self.commit_admission(operation, &credential, &authorization.runtime)?;
        Ok(AgentAdmission {
            operation_id: operation_id.to_owned(),
            revision: 1,
            runtime: authorization.runtime.clone(),
            terminal,
            continuation: Some(target.continuation),
            resume_relation: Some(AgentResumeRelation {
                source: target.source,
                replacement_runtime: authorization.runtime.agent_runtime_id,
                replacement_terminal: authorization.runtime.terminal.clone(),
            }),
            completed: false,
            semantic_digest: Some(agent_operation_digest(semantic_key)),
        })
    }

    #[allow(clippy::too_many_lines)] // Admission atomically fences launch, caller registration, and replay state.
    pub(super) fn admit(
        &mut self,
        operation_id: &str,
        intent: &AgentLaunchIntent,
        scope: &dyn SessionScopeResolver,
        initial_prompt: Option<&str>,
        launch_semantic: &str,
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
        let semantic_digest = agent_operation_digest(launch_semantic);
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
            .queued_prompt(intent.workspace, intent.session)
            .map_err(map_dispatch_storage_error)?;
        if initial_prompt.is_some() && queued.is_some() {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "workspace root already has a queued prompt",
            ));
        }
        let request = LaunchRequest {
            profile_id: profile_id.clone(),
            mode: LaunchMode::Interactive,
            model: None,
            resume: false,
            provider_resume: None,
            initial_prompt: initial_prompt
                .map(str::to_owned)
                .or_else(|| queued.as_ref().map(|item| item.prompt.clone())),
            scope: LaunchScope {
                workspace_id: intent.workspace,
                session_id: intent.session,
                worktree_id: resolved.worktree_id,
            },
            required_capabilities: [AgentCapability::McpWiring, AgentCapability::SystemPrompt]
                .into_iter()
                .collect(),
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
                intent.workspace,
                intent.session,
                profile_id.clone(),
                ModelSelector::new("default").expect("literal model selector is canonical"),
            )
            .map_err(map_dispatch_storage_error)?;
        if !self.peer_is_stopped_except(worker.agent_id, None)? {
            // Ordinary launches must not steal a live peer's identity either.
            worker.agent_id = AgentId::new();
        }
        worker.status = AgentStatus::Starting;
        worker.current_run = Some(operation);
        // A delayed delegation carries the authenticated parent in its durable
        // prompt slot. Ordinary interactive launches retain the historical
        // self-binding used for a top-level conversation.
        let caller = queued
            .as_ref()
            .and_then(|item| item.caller.clone())
            .unwrap_or(CallerRef {
                session_id: worker.session_id,
                agent_id: worker.agent_id,
            });
        self.sleep_one_for_capacity()?;
        self.dispatch
            .reserve_admission_for_workspace(
                intent.workspace,
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
                    semantic_key: launch_semantic.to_owned(),
                    credential_provenance: DispatchCredentialProvenance::DaemonMintedEphemeral,
                },
            )
            .map_err(map_dispatch_storage_error)?;
        if queued.is_some() {
            self.dispatch
                .consume_prompt(intent.workspace, intent.session)
                .map_err(map_dispatch_storage_error)?;
        }
        self.mcp_callers.insert(
            credential.clone(),
            McpCaller {
                runtime: authorization.runtime.clone(),
                operation,
                child: None,
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
            launch_semantic.to_owned(),
        ) {
            self.mcp_callers.remove(&credential);
            let _ = self
                .coordinator
                .fail_reserved_launch(&authorization.runtime, &mut *self.store);
            let _ = self.dispatch.fail_admission(operation);
            return Err(map_orchestration_error(error));
        }
        self.commit_admission(operation, &credential, &authorization.runtime)?;
        Ok(AgentAdmission {
            operation_id: operation_id.to_owned(),
            revision: 1,
            runtime: authorization.runtime.clone(),
            terminal,
            continuation: self
                .coordinator
                .record_for(&authorization.runtime)
                .ok()
                .and_then(|record| record.continuation),
            resume_relation: None,
            completed: false,
            semantic_digest: Some(semantic_digest),
        })
    }

    pub(super) fn commit_admission(
        &mut self,
        operation: OperationId,
        credential: &str,
        runtime: &AgentRuntimeRef,
    ) -> Result<(), ProtocolError> {
        let committed = matches!(self.dispatch.commit_admission(operation), Ok(true));
        self.finish_admission_commit(operation, credential, runtime, committed)
    }

    pub(super) fn finish_admission_commit(
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
}
