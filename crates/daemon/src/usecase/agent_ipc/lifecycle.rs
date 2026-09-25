//! Agent の起動・再開・daemon 再起動をまたぐ復帰。

use anyhow::Result;

use super::{
    AgentAdmission, AgentCapability, AgentGoalIntent, AgentId, AgentIntegrationRevision,
    AgentLaunchIntent, AgentPhase, AgentReadinessPreflight, AgentResumeTarget, AgentRuntime,
    AgentRuntimeRef, BTreeSet, DaemonRestartAgent, DaemonRestartAgentPlan,
    DaemonRestartInterruptionError, ErrorCode, OperationId, ProtocolError,
    ProviderCaptureProvenance, ProviderKind, ProviderResumeReason, SessionId, SessionScopeResolver,
    autonomous_goal_prompt, expected_integration_revisions, goal_semantic_key,
    holds_live_or_unknown_agent, is_resume_source_state, map_dispatch_storage_error,
    map_runtime_error, provider_matches_profile, repair_resume_semantic_key, resume_semantic_key,
    resume_target, semantic_key, validate_goal,
};

impl AgentRuntime {
    /// Captures the current immutable launch facts needed for an owner-external
    /// readiness probe. Replays need no new process and therefore return none.
    pub fn prepare_launch_readiness(
        &self,
        operation_id: &str,
        intent: &AgentLaunchIntent,
    ) -> Result<Option<AgentReadinessPreflight>, ProtocolError> {
        let semantic = semantic_key(intent);
        if let Some(existing) = self.operations.get(operation_id) {
            if existing.conflicts_with(&semantic) {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different agent launch",
                ));
            }
            return Ok(None);
        }
        OperationId::parse(operation_id).map_err(|_| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "agent operation id must be a canonical operation identifier",
            )
        })?;
        let profile = intent
            .profile
            .clone()
            .unwrap_or_else(|| self.default_profile.clone());
        self.readiness_ticket(profile).map(Some)
    }

    /// Goal-driven counterpart whose idempotency meaning includes the exact
    /// objective while reusing the ordinary profile readiness proof.
    pub fn prepare_goal_launch_readiness(
        &self,
        operation_id: &str,
        intent: &AgentGoalIntent,
    ) -> Result<Option<AgentReadinessPreflight>, ProtocolError> {
        validate_goal(intent)?;
        let semantic = goal_semantic_key(intent);
        if let Some(existing) = self.operations.get(operation_id) {
            if existing.conflicts_with(&semantic) {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different goal launch",
                ));
            }
            return Ok(None);
        }
        OperationId::parse(operation_id).map_err(|_| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "agent operation id must be a canonical operation identifier",
            )
        })?;
        let profile = intent
            .profile
            .clone()
            .unwrap_or_else(|| self.default_profile.clone());
        self.readiness_ticket(profile).map(Some)
    }

    /// Captures the provider and owner generation for one exact resume without
    /// invoking an adapter or mutating durable state.
    pub fn prepare_resume_readiness(
        &self,
        operation_id: &str,
        target: &AgentResumeTarget,
    ) -> Result<Option<AgentReadinessPreflight>, ProtocolError> {
        let semantic = resume_semantic_key(target);
        self.prepare_resume_readiness_for(operation_id, target, &semantic)
    }

    /// Readiness preflight for the repair-only revision migration semantic.
    pub fn prepare_current_integration_resume_readiness(
        &self,
        operation_id: &str,
        target: &AgentResumeTarget,
        expected_revision: u32,
    ) -> Result<Option<AgentReadinessPreflight>, ProtocolError> {
        let semantic = repair_resume_semantic_key(target, expected_revision);
        self.prepare_resume_readiness_for(operation_id, target, &semantic)
    }

    pub(super) fn prepare_resume_readiness_for(
        &self,
        operation_id: &str,
        target: &AgentResumeTarget,
        semantic: &str,
    ) -> Result<Option<AgentReadinessPreflight>, ProtocolError> {
        if let Some(existing) = self.operations.get(operation_id) {
            if existing.conflicts_with(semantic) {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different agent resume",
                ));
            }
            return Ok(None);
        }
        OperationId::parse(operation_id).map_err(|_| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "agent resume operation id must be canonical",
            )
        })?;
        let record = self
            .coordinator
            .snapshot()
            .records
            .into_iter()
            .find(|record| record.runtime.agent_runtime_id == target.runtime_id)
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::StaleTarget, "agent resume target is stale")
            })?;
        self.readiness_ticket(record.launch.plan.profile_id)
            .map(Some)
    }

    /// Admits only if the facts observed before readiness are still current.
    /// All ordinary generation, scope, concurrency, executable, and idempotency
    /// checks still run after this comparison and before reservation/spawn.
    pub fn launch_after_readiness(
        &mut self,
        operation_id: &str,
        intent: &AgentLaunchIntent,
        scope: &dyn SessionScopeResolver,
        preflight: Option<&AgentReadinessPreflight>,
    ) -> Result<AgentAdmission, ProtocolError> {
        let current = self.prepare_launch_readiness(operation_id, intent)?;
        self.validate_readiness(preflight, current.as_ref())?;
        self.launch(operation_id, intent, scope)
    }

    /// Admit an opt-in goal launch after the same owner-external readiness
    /// check used by classic launches.
    pub fn launch_goal_after_readiness(
        &mut self,
        operation_id: &str,
        intent: &AgentGoalIntent,
        scope: &dyn SessionScopeResolver,
        preflight: Option<&AgentReadinessPreflight>,
    ) -> Result<AgentAdmission, ProtocolError> {
        let current = self.prepare_goal_launch_readiness(operation_id, intent)?;
        self.validate_readiness(preflight, current.as_ref())?;
        self.launch_goal(operation_id, intent, scope)
    }

    /// Exact-resume counterpart of [`Self::launch_after_readiness`].
    pub fn resume_exact_after_readiness(
        &mut self,
        operation_id: &str,
        target: &AgentResumeTarget,
        scope: &dyn SessionScopeResolver,
        preflight: Option<&AgentReadinessPreflight>,
    ) -> Result<AgentAdmission, ProtocolError> {
        let current = self.prepare_resume_readiness(operation_id, target)?;
        self.validate_readiness(preflight, current.as_ref())?;
        self.resume_exact(operation_id, target, scope)
    }

    /// Repair-only resume counterpart. The readiness ticket is taken from the
    /// current adapter while `target` continues to fence the old durable source.
    pub fn resume_with_current_integration_after_readiness(
        &mut self,
        operation_id: &str,
        target: &AgentResumeTarget,
        expected_revision: u32,
        scope: &dyn SessionScopeResolver,
        preflight: Option<&AgentReadinessPreflight>,
    ) -> Result<AgentAdmission, ProtocolError> {
        let current = self.prepare_current_integration_resume_readiness(
            operation_id,
            target,
            expected_revision,
        )?;
        self.validate_readiness(preflight, current.as_ref())?;
        self.resume_with_current_integration(operation_id, target, expected_revision, scope)
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
            if existing.conflicts_with(&semantic_key) {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different agent launch",
                ));
            }
            return existing.outcome.clone();
        }
        let outcome = self.admit(operation_id, intent, scope, None, &semantic_key);
        self.remember_operation(operation_id, Some(&semantic_key), outcome.clone());
        outcome
    }

    /// Launch one workspace-root Director with the autonomous work contract as
    /// its initial prompt. This is a separate entry point so classic launch
    /// cannot accidentally inherit goal semantics.
    pub fn launch_goal(
        &mut self,
        operation_id: &str,
        intent: &AgentGoalIntent,
        scope: &dyn SessionScopeResolver,
    ) -> Result<AgentAdmission, ProtocolError> {
        validate_goal(intent)?;
        let semantic_key = goal_semantic_key(intent);
        if let Some(existing) = self.operations.get(operation_id) {
            if existing.conflicts_with(&semantic_key) {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different goal launch",
                ));
            }
            return existing.outcome.clone();
        }
        let launch = AgentLaunchIntent {
            workspace: intent.workspace,
            session: None,
            profile: intent.profile.clone(),
        };
        let runtime = intent
            .profile
            .as_ref()
            .unwrap_or(&self.default_profile)
            .as_str();
        let prompt = autonomous_goal_prompt(&intent.goal, runtime);
        let outcome = self.admit(operation_id, &launch, scope, Some(&prompt), &semantic_key);
        self.remember_operation(operation_id, Some(&semantic_key), outcome.clone());
        outcome
    }

    /// Plans an exact, machine-wide stop/resume set for daemon replacement.
    ///
    /// Every process that can still be an Agent owner must be represented by a
    /// resumable target. The plan is effect-free; the same selection is checked
    /// again inside the rollover active-control barrier before any process is
    /// interrupted.
    pub fn plan_daemon_restart_agents(
        &self,
        expected: &[AgentIntegrationRevision],
        force: bool,
    ) -> Result<DaemonRestartAgentPlan, ProtocolError> {
        let expected = expected_integration_revisions(expected)?;
        let records = self.coordinator.snapshot().records;
        let failed = self.failed_dispatch_ids().unwrap_or_default();
        if records.iter().any(|record| {
            holds_live_or_unknown_agent(record.state)
                && !matches!(
                    record.state,
                    crate::usecase::runtime::RuntimeState::Reserved
                        | crate::usecase::runtime::RuntimeState::Running
                )
        }) {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "an Agent process has ownership-unknown state; no Agent was stopped",
            ));
        }
        let mut agents = records
            .iter()
            .filter(|record| {
                matches!(
                    record.state,
                    crate::usecase::runtime::RuntimeState::Reserved
                        | crate::usecase::runtime::RuntimeState::Running
                ) && record.superseded_by.is_none()
            })
            .map(|record| {
                let expected_revision = expected
                    .get(record.launch.plan.profile_id.as_str())
                    .copied()
                    .ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "a live Agent profile has no current integration revision",
                        )
                    })?;
                let (available, _) = Self::repair_source_availability(record, &records);
                let target = resume_target(record).filter(|_| available).ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::Busy,
                        "a live Agent has no exact provider resume metadata; no Agent was stopped",
                    )
                })?;
                let phase = self.record_phase(record, &failed).1;
                if !force && phase == AgentPhase::Running {
                    return Err(ProtocolError::new(
                        ErrorCode::Busy,
                        "an Agent is running a prompt or tool; retry with --force to interrupt it",
                    ));
                }
                Ok(DaemonRestartAgent {
                    runtime: record.runtime.clone(),
                    target,
                    profile_id: record.launch.plan.profile_id.clone(),
                    expected_revision,
                    phase,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        agents.sort_by_key(|item| item.runtime.agent_runtime_id.as_str());
        let workspaces = agents
            .iter()
            .map(|item| item.runtime.terminal.workspace_id)
            .collect::<BTreeSet<_>>();
        if workspaces.len() > 1 {
            return Err(ProtocolError::new(
                ErrorCode::Busy,
                "live Agents span multiple workspaces; no Agent was stopped",
            ));
        }
        let selected = agents
            .iter()
            .map(|item| item.runtime.agent_runtime_id)
            .collect::<BTreeSet<_>>();
        if self
            .mcp_callers
            .values()
            .any(|caller| !selected.contains(&caller.runtime.agent_runtime_id))
        {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "daemon-provisioned MCP authority cannot be matched to a live Agent; no Agent was stopped",
            ));
        }
        Ok(DaemonRestartAgentPlan { agents })
    }

    /// Interrupts the exact plan revalidated inside the rollover barrier.
    pub fn interrupt_agents_for_daemon_restart(
        &mut self,
        expected: &[AgentIntegrationRevision],
        selected: &[AgentRuntimeRef],
        force: bool,
    ) -> Result<DaemonRestartAgentPlan, Box<DaemonRestartInterruptionError>> {
        let plan = self
            .plan_daemon_restart_agents(expected, force)
            .map_err(DaemonRestartInterruptionError::before_effect)
            .map_err(Box::new)?;
        let mut planned = plan
            .agents
            .iter()
            .map(|item| item.runtime.clone())
            .collect::<Vec<_>>();
        let mut requested = selected.to_vec();
        planned.sort_by_key(|runtime| runtime.agent_runtime_id.as_str());
        requested.sort_by_key(|runtime| runtime.agent_runtime_id.as_str());
        if requested.len() != selected.len() || requested != planned {
            return Err(Box::new(DaemonRestartInterruptionError::before_effect(
                ProtocolError::new(
                    ErrorCode::StaleTarget,
                    "live Agent selection changed before daemon handoff; no Agent was stopped",
                ),
            )));
        }
        let runtime_ids = plan
            .agents
            .iter()
            .map(|item| item.runtime.agent_runtime_id.as_str())
            .collect();
        if let Err(error) =
            self.coordinator
                .interrupt_agents(&runtime_ids, &mut *self.store, &mut *self.pty)
        {
            let stopped = self
                .coordinator
                .snapshot()
                .records
                .into_iter()
                .filter(|record| {
                    runtime_ids.contains(&record.runtime.agent_runtime_id.as_str())
                        && record.state == crate::usecase::runtime::RuntimeState::Exited
                })
                .map(|record| record.runtime.agent_runtime_id.as_str())
                .collect::<BTreeSet<_>>();
            self.clear_daemon_restart_authority(&stopped);
            return Err(Box::new(DaemonRestartInterruptionError {
                error: map_runtime_error(error),
                interrupted: DaemonRestartAgentPlan {
                    agents: plan
                        .agents
                        .into_iter()
                        .filter(|agent| stopped.contains(&agent.runtime.agent_runtime_id.as_str()))
                        .collect(),
                },
            }));
        }
        self.clear_daemon_restart_authority(&runtime_ids);
        Ok(plan)
    }

    /// Whether rollback/recovery must resume one source from a durable daemon
    /// restart transaction.
    ///
    /// A transaction is persisted before interruption, so a pre-effect crash
    /// can leave entries whose exact original runtime is still live. Those
    /// entries are already recovered and must not be sent through the non-live
    /// exact-resume path. Every other state is left to that path's full fences.
    pub fn daemon_restart_restore_needed(
        &self,
        runtime: &AgentRuntimeRef,
    ) -> Result<bool, ProtocolError> {
        self.coordinator
            .record_for(runtime)
            .map(|record| {
                !matches!(
                    record.state,
                    crate::usecase::runtime::RuntimeState::Reserved
                        | crate::usecase::runtime::RuntimeState::Running
                )
            })
            .map_err(map_runtime_error)
    }

    pub(super) fn clear_daemon_restart_authority(&mut self, runtime_ids: &BTreeSet<String>) {
        self.mcp_callers
            .retain(|_, caller| !runtime_ids.contains(&caller.runtime.agent_runtime_id.as_str()));
        self.reported_phases
            .retain(|runtime, _| !runtime_ids.contains(&runtime.as_str()));
    }

    /// Starts a new daemon-owned runtime for one exact interrupted source. This
    /// never reattaches the old PTY and never falls back to provider-global
    /// "last" semantics.
    pub fn resume_exact(
        &mut self,
        operation_id: &str,
        target: &AgentResumeTarget,
        scope: &dyn SessionScopeResolver,
    ) -> Result<AgentAdmission, ProtocolError> {
        let semantic_key = resume_semantic_key(target);
        if let Some(existing) = self.operations.get(operation_id) {
            if existing.conflicts_with(&semantic_key) {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different agent resume",
                ));
            }
            return existing.outcome.clone();
        }
        let outcome = self.admit_resume_exact(operation_id, target, &semantic_key, scope, None);
        self.remember_operation(operation_id, Some(&semantic_key), outcome.clone());
        outcome
    }

    /// Resumes one interrupted source while intentionally migrating only its
    /// adapter integration revision. Provider, scope, lineage, and source
    /// incarnation remain exact; provider-global "last" semantics are never
    /// used.
    pub fn resume_with_current_integration(
        &mut self,
        operation_id: &str,
        target: &AgentResumeTarget,
        expected_revision: u32,
        scope: &dyn SessionScopeResolver,
    ) -> Result<AgentAdmission, ProtocolError> {
        let semantic_key = repair_resume_semantic_key(target, expected_revision);
        if let Some(existing) = self.operations.get(operation_id) {
            if !existing.matches(&semantic_key) {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "operation id was reused with a different agent repair resume",
                ));
            }
            return existing.outcome.clone();
        }
        let outcome = self.admit_resume_exact(
            operation_id,
            target,
            &semantic_key,
            scope,
            Some(expected_revision),
        );
        self.remember_operation(operation_id, Some(&semantic_key), outcome.clone());
        outcome
    }

    /// Safe interrupted/resume projection for a managed session. Provider IDs
    /// are never returned; only availability and a stable reason cross IPC.
    #[must_use]
    pub fn session_resume_status(&self, session: SessionId) -> (bool, ProviderResumeReason) {
        let records = self.coordinator.snapshot().records;
        let mut resumable = Vec::new();
        for record in &records {
            if record.runtime.session_id != Some(session) {
                continue;
            }
            if holds_live_or_unknown_agent(record.state) {
                return (false, ProviderResumeReason::LiveOrOwnershipUnknown);
            }
            if is_resume_source_state(record.state) {
                resumable.push(record);
            }
        }
        if resumable.is_empty() {
            return (false, ProviderResumeReason::ProviderMetadataUnavailable);
        }
        let mut available = 0;
        let mut reason = ProviderResumeReason::ProviderMetadataUnavailable;
        for record in resumable {
            let (candidate_available, candidate_reason) =
                self.resume_source_availability(record, &records);
            available += usize::from(candidate_available);
            if reason == ProviderResumeReason::ProviderMetadataUnavailable
                && candidate_reason != ProviderResumeReason::ProviderMetadataUnavailable
            {
                reason = candidate_reason;
            }
        }
        if available == 1 {
            return (true, ProviderResumeReason::ExplicitResumeAvailable);
        }
        if available > 1 {
            return (false, ProviderResumeReason::AmbiguousProviderMetadata);
        }
        (false, reason)
    }

    pub(super) fn resume_source_availability(
        &self,
        record: &crate::usecase::runtime::DurableRuntimeRecord,
        records: &[crate::usecase::runtime::DurableRuntimeRecord],
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
            && record.runtime.terminal.worktree_id == reference.scope.worktree_id;
        let adapter_compatible = self
            .registry
            .profile(&record.launch.plan.profile_id)
            .is_ok_and(|profile| {
                profile.revision == reference.adapter_revision
                    && profile.capabilities.contains(&AgentCapability::Resume)
                    && provider_matches_profile(reference.provider, &record.launch.plan.profile_id)
            });
        if internally_compatible && adapter_compatible {
            (true, ProviderResumeReason::ExplicitResumeAvailable)
        } else {
            (false, ProviderResumeReason::IncompatibleProviderMetadata)
        }
    }

    pub(super) fn repair_resume_source_availability(
        &self,
        record: &crate::usecase::runtime::DurableRuntimeRecord,
        records: &[crate::usecase::runtime::DurableRuntimeRecord],
        expected_revision: u32,
    ) -> (bool, ProviderResumeReason) {
        let (source_compatible, reason) = Self::repair_source_availability(record, records);
        if !source_compatible {
            return (false, reason);
        }
        let reference = record
            .provider_resume
            .as_ref()
            .expect("repair-compatible source has provider metadata");
        let current_compatible = self
            .registry
            .profile(&record.launch.plan.profile_id)
            .is_ok_and(|profile| {
                profile.revision == expected_revision
                    && profile.capabilities.contains(&AgentCapability::Resume)
                    && provider_matches_profile(reference.provider, &record.launch.plan.profile_id)
            });
        if current_compatible {
            (true, ProviderResumeReason::ExplicitResumeAvailable)
        } else {
            (false, ProviderResumeReason::IncompatibleProviderMetadata)
        }
    }

    pub(super) fn require_peer_stopped(&self, agent_id: AgentId) -> Result<(), ProtocolError> {
        self.require_peer_stopped_except(agent_id, None)
    }

    pub(super) fn require_peer_stopped_except(
        &self,
        agent_id: AgentId,
        source: Option<OperationId>,
    ) -> Result<(), ProtocolError> {
        if !self.peer_is_stopped_except(agent_id, source)? {
            return Err(ProtocolError::new(
                ErrorCode::Unavailable,
                "peer runtime already exists; use agent_message for a live peer",
            ));
        }
        Ok(())
    }

    pub(super) fn peer_is_stopped_except(
        &self,
        agent_id: AgentId,
        source: Option<OperationId>,
    ) -> Result<bool, ProtocolError> {
        let runs: BTreeSet<_> = self
            .dispatch
            .runs()
            .map_err(map_dispatch_storage_error)?
            .into_iter()
            .filter(|run| run.agent_id == agent_id && Some(run.run_id) != source)
            .map(|run| run.run_id)
            .collect();
        Ok(!self.coordinator.snapshot().records.iter().any(|record| {
            runs.contains(&record.operation.operation_id)
                && !matches!(
                    record.state,
                    crate::usecase::runtime::RuntimeState::Exited
                        | crate::usecase::runtime::RuntimeState::Reclaimed
                        | crate::usecase::runtime::RuntimeState::SpawnFailed
                )
        }))
    }
}
