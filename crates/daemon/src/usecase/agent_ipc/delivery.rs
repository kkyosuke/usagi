//! prompt の投函と report の受理・照合。

use anyhow::Result;

use super::{
    AgentId, AgentPhase, AgentRuntime, AgentRuntimeRef, AgentStatus, CompletionFence,
    DispatchBinding, ErrorCode, InboxKind, InboxMessage, OperationId, PromptDelivery, PromptMode,
    ProtocolError, ProviderKind, ProviderResumePhase, ProviderSessionId, ReportDelivery, RunStatus,
    SessionId, TerminalRef, Utc, WorkspaceId, dispatch_agent_not_found,
    dispatch_binding_unavailable, durable_provider_phase, map_dispatch_storage_error,
    map_runtime_error, provider_for_profile, unknown_caller_provenance,
};

impl AgentRuntime {
    /// Accepts one agent lifecycle phase report bound to a live runtime by the
    /// daemon-owned runtime identity resolved from the reporting process group.
    ///
    /// The caller names neither a runtime, session, worktree, nor path: the
    /// credential is the only selector, and an unknown or no longer live
    /// credential is refused without recording anything.  The reported phase
    /// refines the runtime projection, and additionally the durable safe phase
    /// of provider resume metadata when that mapping cannot claim process death
    /// (see [`durable_provider_phase`]).
    pub fn report_agent_phase(
        &mut self,
        credential: &str,
        phase: AgentPhase,
    ) -> Result<(), ProtocolError> {
        self.report_agent_phase_with_session(credential, phase, None)
    }

    /// Reports a lifecycle phase and atomically refreshes the provider-owned
    /// conversation identity carried by a provider's first structured hook.
    pub fn report_agent_phase_with_session(
        &mut self,
        credential: &str,
        phase: AgentPhase,
        native_session_id: Option<ProviderSessionId>,
    ) -> Result<(), ProtocolError> {
        if !phase.is_reportable() {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "agent phase is not reportable",
            ));
        }
        let caller = self.mcp_callers.get(credential).cloned().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "agent runtime credential is unknown",
            )
        })?;
        if self.mcp_caller(credential).is_none() {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "agent runtime credential is not live",
            ));
        }
        if native_session_id.is_some() && phase != AgentPhase::Ready {
            let record = self
                .coordinator
                .record_for(&caller.runtime)
                .map_err(map_runtime_error)?;
            let agy_start = phase == AgentPhase::Running
                && provider_for_profile(&record.launch.plan.profile_id) == Some(ProviderKind::Agy);
            if !agy_start {
                return Err(ProtocolError::new(
                    ErrorCode::InvalidArgument,
                    "provider session ID is only valid for a starting hook",
                ));
            }
        }
        let durable = durable_provider_phase(phase);
        let captured = if let Some(native_session_id) = native_session_id {
            let capture_phase = if phase == AgentPhase::Ready {
                ProviderResumePhase::Starting
            } else {
                // A session-bearing non-ready report was admitted above only
                // for AGY's Running `PreInvocation` hook.
                ProviderResumePhase::Running
            };
            self.capture_provider_session_start(&caller.runtime, native_session_id, capture_phase)?
        } else {
            false
        };
        if !captured && let Some(durable) = durable {
            self.coordinator
                .record_provider_phase(&caller.runtime, durable, &mut *self.store)
                .map_err(map_runtime_error)?;
        }
        self.reported_phases
            .insert(caller.runtime.agent_runtime_id, phase);
        Ok(())
    }

    pub(super) fn prompt_agent(
        &mut self,
        workspace: WorkspaceId,
        session: SessionId,
        agent_id: AgentId,
        prompt: &str,
    ) -> Result<(), ProtocolError> {
        let worker = self
            .dispatch
            .agent_in_workspace(workspace, agent_id)
            .map_err(map_dispatch_storage_error)?
            .filter(|agent| agent.session_id == Some(session))
            .ok_or_else(dispatch_agent_not_found)?;
        let run = worker.current_run.ok_or_else(dispatch_agent_not_found)?;
        self.prompt_run(run, prompt).map(|_| ())
    }

    /// Sends to a running Agent PTY or records a durable next-launch prompt.
    pub fn prompt(
        &mut self,
        workspace: WorkspaceId,
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
                record.runtime.terminal.workspace_id == workspace
                    && record.runtime.session_id == session
                    && record.state == crate::usecase::runtime::RuntimeState::Running
            });
        if matches!(mode, PromptMode::Live) && live.is_none() {
            return Err(ProtocolError::new(
                ErrorCode::Unavailable,
                "target session has no live agent; use session_dispatch to start it or mode=queue for intentional deferred delivery",
            ));
        }
        if matches!(mode, PromptMode::Queue) && live.is_some() {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "target session already has a live agent; use mode=live",
            ));
        }
        if let Some(record) = live {
            self.submit_live_prompt(&record.runtime.terminal, prompt)?;
            return Ok(PromptDelivery {
                delivered_to: "live",
                queued: false,
            });
        }
        self.queue_prompt_for_next_launch(workspace, session, prompt)
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
                    && record.state == crate::usecase::runtime::RuntimeState::Running
            })
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::Unavailable, "target agent run is no longer live")
            })?;
        self.submit_live_prompt(&live.runtime.terminal, prompt)?;
        Ok(PromptDelivery {
            delivered_to: "live",
            queued: false,
        })
    }

    /// Writes one complete prompt submission to a live Agent. All prompt paths
    /// use this boundary so none can leave text in the provider's input editor
    /// without the same carriage return emitted by the TUI Enter key.
    pub(super) fn submit_live_prompt(
        &mut self,
        terminal: &TerminalRef,
        prompt: &str,
    ) -> Result<(), ProtocolError> {
        let mut bytes = prompt.as_bytes().to_vec();
        bytes.push(b'\r');
        self.pty.select_terminal(terminal);
        self.pty
            .write_all(&bytes)
            .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "live prompt delivery failed"))
    }

    /// Persists a prompt for the next launch even while a runtime is still
    /// recorded as live. This is reserved for internal wake delivery after a
    /// live PTY write failed; public queue mode keeps rejecting live targets.
    pub fn queue_prompt_for_next_launch(
        &mut self,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        prompt: &str,
    ) -> Result<PromptDelivery, ProtocolError> {
        if prompt.trim().is_empty() {
            return Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "prompt must not be empty",
            ));
        }
        self.dispatch
            .queue_prompt(workspace, session, prompt.to_owned(), Utc::now())
            .map_err(map_dispatch_storage_error)?;
        Ok(PromptDelivery {
            delivered_to: "queue",
            queued: true,
        })
    }

    pub(super) fn synthesize_no_report(
        &mut self,
        runtime: &AgentRuntimeRef,
    ) -> Result<(), ProtocolError> {
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
    ) -> Result<bool, ProtocolError> {
        if self.coordinator.require_outcome_owner(runtime).is_err() {
            return Ok(false);
        }
        let record = self
            .coordinator
            .record_for(runtime)
            .map_err(map_runtime_error)?;
        if !record.operation.fences(candidate)
            || !matches!(kind, InboxKind::Completed | InboxKind::Failed)
        {
            return Ok(false);
        }
        let Some(binding) = self
            .dispatch
            .binding(candidate.operation_id)
            .map_err(map_dispatch_storage_error)?
        else {
            return Ok(false);
        };
        let inbox = self
            .dispatch
            .inbox(&binding.caller)
            .map_err(map_dispatch_storage_error)?;
        if let Some(committed) = inbox
            .iter()
            .find(|message| message.run_id == candidate.operation_id)
        {
            self.reconcile_report_status(&binding, committed.kind)?;
            return Ok(false);
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
        self.reconcile_report_status(&binding, kind)?;
        Ok(true)
    }

    /// Converges the registry half of a report after the inbox half committed.
    ///
    /// A retry must use the committed message kind, not the new request: the
    /// inbox write can succeed before either registry transition does. Replaying
    /// both idempotent transitions repairs that partial state without changing
    /// the first report's outcome or appending a second message.
    pub(super) fn reconcile_report_status(
        &self,
        binding: &DispatchBinding,
        kind: InboxKind,
    ) -> Result<(), ProtocolError> {
        let (run_status, agent_status) = match kind {
            InboxKind::Completed => (RunStatus::Completed, AgentStatus::Idle),
            InboxKind::Failed => (RunStatus::Failed, AgentStatus::Failed),
            InboxKind::NoReport => return Ok(()),
        };
        self.dispatch
            .reconcile_report_outcome(
                binding.run_id,
                binding.worker.agent_id,
                run_status,
                agent_status,
                Utc::now(),
            )
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
    ) -> Result<ReportDelivery, ProtocolError> {
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
        let worker = binding.worker;
        let accepted = self.report(&caller.runtime, &fence, kind, summary, result)?;
        let committed = self
            .dispatch
            .inbox(&delivered_to)
            .map_err(map_dispatch_storage_error)?
            .into_iter()
            .find(|message| message.run_id == caller.operation);
        // Inbox commit is the durable fact. The wake is only a notification:
        // a live waiting Manager receives it immediately, while a stopped one
        // gets a durable next-launch prompt. Failure here never rolls back or
        // redirects the already committed report.
        if accepted
            && delivered_to.agent_id != worker.agent_id
            && let Some(message) = committed.as_ref()
            && let Some(workspace) = self
                .dispatch
                .workspace_for_agent(delivered_to.agent_id)
                .ok()
                .flatten()
        {
            let notice = format!(
                "A child report is ready (run {}). Read your session inbox, verify the result, aggregate all required children, then report only to your caller. Summary: {}",
                message.run_id, message.summary
            );
            if let Some(session) = delivered_to
                .session_id
                .filter(|session| worker.session_id == Some(*session))
            {
                // A session-scoped queue could wake a sibling (or the sender).
                // The inbox remains durable when this exact caller is stopped.
                let _ = self.prompt_agent(workspace, session, delivered_to.agent_id, &notice);
            } else if self
                .prompt(
                    workspace,
                    delivered_to.session_id,
                    &notice,
                    PromptMode::Live,
                )
                .is_err()
            {
                let _ =
                    self.queue_prompt_for_next_launch(workspace, delivered_to.session_id, &notice);
            }
        }
        Ok(ReportDelivery {
            delivered_to,
            worker,
            accepted,
            committed,
        })
    }
}
