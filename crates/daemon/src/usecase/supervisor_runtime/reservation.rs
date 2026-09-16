//! 予約した dispatch / goal / handoff の確保と、その bind・失敗処理。

use anyhow::{Context, Result};

use super::{
    AgentId, AgentProfileId, AgentRuntimeRef, ArtifactContract, BTreeSet, DateTime, DecisionWake,
    DecisionWaker, DelegatedDispatchReservation, GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
    GitHubRepository, GoalSpecification, InboxKind, MAX_SUPERVISOR_TEXT_BYTES,
    MAX_WAKE_RESERVATIONS, NO_ARTIFACT_CONTRACT, OperationId, RunProvenance, RunStatus,
    RuntimeState, SessionId, SupervisorEventKind, SupervisorEventSource, SupervisorRunId,
    SupervisorRunQuery, SupervisorRunState, SupervisorRuntime, SupervisorStartRequest, TaskId,
    TaskState, Utc, WakeReservation, WorkspaceId, bounded_nonempty, child_dispatch_policy_denial,
    delegated_handoff_prompt, delegated_task_digest, delegated_task_id, delegated_task_suffix,
    delegated_worker_matches_reservation, delegated_worker_semantic_digest,
    has_caller_root_reservation, is_delegated_reservation, live_task_dispatch_authority, task_node,
    validate_provenance_chain,
};

impl SupervisorRuntime {
    /// Durably reserves the Goal run before the Agent process is spawned. This
    /// is the first phase of Goal admission and makes the run ID available even
    /// if binding must be reconciled after a daemon restart.
    ///
    /// # Errors
    /// Returns an error when the Goal contract or durable start cannot be
    /// admitted.
    pub fn reserve_goal_for_workspace(
        &self,
        caller: &str,
        workspace: WorkspaceId,
        operation_id: &str,
        goal: GoalSpecification,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.start_scoped(SupervisorStartRequest {
            caller,
            workspace: Some(workspace),
            operation_id,
            root_task: goal.instruction,
            root_artifact_contract: GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            artifact_repository: Some(goal.artifact_repository),
            worker_profile_id: None,
            worker_semantic_digest: None,
            caller_dispatch: None,
            initial_tasks: Vec::new(),
            policy_selector,
            now,
        })
    }

    // 注入された port をそのまま受け取る composition 境界で、束ねると呼び手が構造体を組むだけになる。
    #[allow(clippy::too_many_arguments)]
    /// Goal reservation variant which also pins the selected Agent runtime
    /// family before any process can be spawned.
    ///
    /// # Errors
    /// Returns an error when the reservation conflicts with an existing
    /// operation or cannot be persisted.
    pub fn reserve_goal_for_workspace_with_profile(
        &self,
        caller: &str,
        workspace: WorkspaceId,
        operation_id: &str,
        goal: GoalSpecification,
        worker_profile_id: AgentProfileId,
        worker_semantic_digest: String,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.start_scoped(SupervisorStartRequest {
            caller,
            workspace: Some(workspace),
            operation_id,
            root_task: goal.instruction,
            root_artifact_contract: GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            artifact_repository: Some(goal.artifact_repository),
            worker_profile_id: Some(worker_profile_id),
            worker_semantic_digest: Some(worker_semantic_digest),
            caller_dispatch: None,
            initial_tasks: Vec::new(),
            policy_selector,
            now,
        })
    }

    /// Binds the exact Agent fence to a previously reserved Goal root. The
    /// operation ID is the durable join key; caller text and Goal content are
    /// never reconstructed by the recovery path.
    ///
    /// # Errors
    /// Returns an error when the reservation, worker scope, dispatch run, or
    /// existing provenance conflicts.
    pub fn bind_reserved_workspace_root_dispatch(
        &self,
        operation_id: &str,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        let dispatch_run_id = OperationId::parse(operation_id)
            .map_err(|_| anyhow::anyhow!("supervisor root dispatch operation is invalid"))?;
        if self.dispatch.run(dispatch_run_id)?.is_some() {
            let state = self.load_state()?;
            if let Some(reservation) = state.starts.get(operation_id)
                && self
                    .load_started_run(reservation.supervisor_run_id)?
                    .artifact_repository
                    .is_none()
            {
                anyhow::bail!("reserved supervisor run is not a Goal run");
            }
        }
        self.bind_reserved_root_task(
            operation_id,
            dispatch_run_id,
            worker,
            GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            true,
            now,
        )
    }

    /// Binds a generic `supervisor_start` root to the exact authenticated Agent
    /// dispatch which invoked the tool. This makes a public run observable from
    /// its first turn instead of manufacturing an unowned Ready task.
    ///
    /// # Errors
    /// Returns an error when the reservation or exact caller dispatch fence is
    /// missing, conflicting, or cannot be persisted.
    pub fn bind_reserved_caller_dispatch(
        &self,
        start_operation_id: &str,
        dispatch_run_id: OperationId,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.bind_reserved_root_task(
            start_operation_id,
            dispatch_run_id,
            worker,
            NO_ARTIFACT_CONTRACT,
            false,
            now,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Every persisted root fence is validated together before binding.
    pub(super) fn bind_reserved_root_task(
        &self,
        start_operation_id: &str,
        dispatch_run_id: OperationId,
        worker: &AgentRuntimeRef,
        expected_contract: ArtifactContract,
        require_workspace_root: bool,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        let dispatch = self
            .dispatch
            .run(dispatch_run_id)?
            .context("supervisor root dispatch does not exist")?;
        let state = self.load_state()?;
        let reservation = state
            .starts
            .get(start_operation_id)
            .context("supervisor root reservation does not exist")?;
        if matches!(reservation.workspace_id, Some(expected) if expected != worker.terminal.workspace_id)
        {
            anyhow::bail!("supervisor root worker is outside its reserved workspace");
        }
        if matches!(reservation.caller_dispatch_run_id, Some(expected) if expected != dispatch_run_id)
        {
            anyhow::bail!("supervisor root caller dispatch conflicts with its reservation");
        }
        let dispatch_agent = self
            .dispatch
            .agent_in_workspace(worker.terminal.workspace_id, dispatch.agent_id)?
            .context("supervisor root dispatch Agent does not exist")?;
        if matches!(reservation.worker_session_id, Some(expected) if Some(expected) != worker.session_id)
            || matches!(reservation.worker_agent_id, Some(expected) if expected != dispatch.agent_id)
            || matches!(reservation.worker_runtime_id, Some(expected) if expected != worker.agent_runtime_id)
            || dispatch_agent.session_id != worker.session_id
        {
            anyhow::bail!("supervisor root worker is outside its reserved Agent scope");
        }
        if let Some(expected_profile) = reservation.worker_profile_id.as_ref()
            && &dispatch_agent.runtime != expected_profile
        {
            anyhow::bail!("supervisor root worker is outside its reserved Agent scope");
        }
        if let Some(expected_digest) = reservation.worker_semantic_digest.as_ref() {
            let admission = self
                .dispatch
                .admission(dispatch_run_id)?
                .context("supervisor root admission does not exist")?;
            if usagi_core::infrastructure::ipc::agent_operation_digest(&admission.semantic_key)
                != *expected_digest
            {
                anyhow::bail!("supervisor root Agent admission has another semantic intent");
            }
        }
        let mut run = self.load_started_run(reservation.supervisor_run_id)?;
        if run.workspace_id != Some(worker.terminal.workspace_id)
            || (require_workspace_root && worker.session_id.is_some())
        {
            anyhow::bail!("supervisor root worker is outside the workspace root scope");
        }
        let root_id = TaskId::new("root")?;
        let root = run
            .tasks
            .get(&root_id)
            .context("supervisor root task is missing")?;
        if root.required_artifact_contract != expected_contract {
            anyhow::bail!("supervisor root reservation has another artifact contract");
        }
        let root_generation = root.generation;
        if run.state == SupervisorRunState::Planning {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::Admission,
                SupervisorEventKind::SetRunState {
                    state: SupervisorRunState::Running,
                    terminal_reason: None,
                },
            )?;
        }
        self.ensure_supervisor_start_dispatch_available(start_operation_id, dispatch_run_id)?;
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: root_id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id,
            worker_session_id: worker.session_id,
            worker_agent_id: worker.agent_runtime_id,
            worker_worktree_id: worker.terminal.worktree_id,
            generation: root_generation,
        };
        if let Some(existing) = run.provenance.get(&root_id) {
            if existing == &provenance {
                return Ok(run.query());
            }
            anyhow::bail!("supervisor root dispatch provenance conflicts with the existing run");
        }
        run = self.resume_pending_promotion_escalation(run, &root_id, now)?;
        run = self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::Dispatch {
                task_id: root_id,
                generation: root_generation,
                provenance,
            },
        )?;
        Ok(run.query())
    }

    /// Returns the repository pinned by an existing Goal reservation so an
    /// idempotent admission replay never consults worker-mutable Git config.
    ///
    /// # Errors
    /// Returns an error when scheduler metadata or the reserved run is invalid.
    pub fn reserved_goal_repository(&self, operation_id: &str) -> Result<Option<GitHubRepository>> {
        let state = self.load_state()?;
        let Some(reservation) = state.starts.get(operation_id) else {
            return Ok(None);
        };
        if let Some(repository) = reservation.artifact_repository.clone() {
            if let Some(run) = self.supervisor.load(reservation.supervisor_run_id)?
                && run.artifact_repository.as_ref() != Some(&repository)
            {
                anyhow::bail!("Goal reservation repository conflicts with its durable run");
            }
            return Ok(Some(repository));
        }
        Ok(self
            .supervisor
            .load(reservation.supervisor_run_id)?
            .and_then(|run| run.artifact_repository))
    }

    /// Marks a pre-spawn Goal reservation failed after Agent admission proves a
    /// definite failure. Ambiguous/post-spawn failures must remain pending for
    /// reconciliation instead.
    ///
    /// # Errors
    /// Returns an error when the durable reservation cannot be loaded or
    /// transitioned.
    pub fn fail_reserved_goal(
        &self,
        operation_id: &str,
        reason: String,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        let state = self.load_state()?;
        let reservation = state
            .starts
            .get(operation_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor goal reservation does not exist"))?;
        let mut run = self.load_started_run(reservation.supervisor_run_id)?;
        let root_id = TaskId::new("root")?;
        let root = run
            .tasks
            .get(&root_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor root task is missing"))?;
        if root.required_artifact_contract != GOAL_REVIEW_READY_ARTIFACT_CONTRACT {
            anyhow::bail!("supervisor reservation is not a Goal run");
        }
        if run.state.is_finished() {
            return Ok(run.query());
        }
        run = self.resume_pending_promotion_escalation(run, &root_id, now)?;
        Ok(self
            .apply(
                &run,
                now,
                SupervisorEventSource::DispatchFailure,
                SupervisorEventKind::SetRunState {
                    state: SupervisorRunState::Failed,
                    terminal_reason: Some(reason),
                },
            )?
            .query())
    }

    /// Closes a generic caller-root reservation whose exact authenticated
    /// dispatch can no longer be bound. The retained start fence remains until
    /// worker reconciliation proves the caller operation stopped or absent.
    ///
    /// # Errors
    /// Returns an error when the start is not a generic caller reservation or
    /// its durable run cannot be transitioned.
    pub fn fail_reserved_caller_dispatch(
        &self,
        start_operation_id: &str,
        reason: String,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        let state = self.load_state()?;
        let reservation = state
            .starts
            .get(start_operation_id)
            .filter(|reservation| reservation.caller_dispatch_run_id.is_some())
            .context("supervisor caller reservation does not exist")?;
        let mut run = self.load_started_run(reservation.supervisor_run_id)?;
        let root_id = TaskId::new("root")?;
        let root = run
            .tasks
            .get(&root_id)
            .context("supervisor caller root task is missing")?;
        if root.required_artifact_contract != NO_ARTIFACT_CONTRACT {
            anyhow::bail!("supervisor reservation is not a caller-root run");
        }
        if run.state.is_finished() {
            return Ok(run.query());
        }
        run = self.resume_pending_promotion_escalation(run, &root_id, now)?;
        Ok(self
            .apply(
                &run,
                now,
                SupervisorEventSource::DispatchFailure,
                SupervisorEventKind::SetRunState {
                    state: SupervisorRunState::Failed,
                    terminal_reason: Some(reason),
                },
            )?
            .query())
    }

    #[allow(clippy::too_many_arguments)] // The commit boundary compares each independent Agent identity fence explicitly.
    pub(super) fn ensure_pending_operation_matches_reservation(
        &self,
        operation: OperationId,
        workspace_id: WorkspaceId,
        requires_session: bool,
        worker_session_id: Option<SessionId>,
        worker_profile_id: Option<&AgentProfileId>,
        worker_agent_id: Option<AgentId>,
        worker_semantic_digest: Option<&str>,
    ) -> Result<()> {
        let Some(dispatch) = self.dispatch.run(operation)? else {
            return Ok(());
        };
        if !matches!(dispatch.status, RunStatus::Preparing | RunStatus::Running) {
            anyhow::bail!("pending Supervisor operation has closed supervisor ownership");
        }
        let agent = self
            .dispatch
            .agent_in_workspace(workspace_id, dispatch.agent_id)?
            .context("pending Supervisor operation has foreign Agent ownership")?;
        let session_matches = if requires_session {
            match worker_session_id {
                Some(expected) => agent.session_id == Some(expected),
                None => agent.session_id.is_some(),
            }
        } else {
            agent.session_id.is_none()
        };
        if !session_matches
            || matches!(worker_profile_id, Some(expected) if &agent.runtime != expected)
            || matches!(worker_agent_id, Some(expected) if dispatch.agent_id != expected)
        {
            anyhow::bail!("pending Supervisor operation conflicts with its Agent ownership");
        }
        if let Some(expected) = worker_semantic_digest {
            let admission = self
                .dispatch
                .admission(operation)?
                .context("pending Supervisor operation has no semantic authority")?;
            if usagi_core::infrastructure::ipc::agent_operation_digest(&admission.semantic_key)
                != expected
            {
                anyhow::bail!("pending Supervisor operation conflicts with its Agent semantics");
            }
        }
        Ok(())
    }

    pub(super) fn retained_dispatch_owners(
        &self,
        state: &RuntimeState,
        dispatch_run: OperationId,
    ) -> Result<Vec<(SupervisorRunId, TaskId)>> {
        let mut pending_roots = state
            .starts
            .values()
            .filter_map(|reservation| {
                if reservation.caller_dispatch_run_id == Some(dispatch_run) {
                    Some((reservation.supervisor_run_id, NO_ARTIFACT_CONTRACT))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if let Some(reservation) = state.starts.get(&dispatch_run.to_string())
            && reservation.caller_dispatch_run_id.is_none()
            && self
                .supervisor
                .load(reservation.supervisor_run_id)?
                .is_none()
        {
            // Before aggregate initialization a Goal and a legacy generic
            // start cannot be distinguished. Conservatively retain the exact
            // operation as a conceptual owner so it cannot become classic.
            pending_roots.push((
                reservation.supervisor_run_id,
                GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            ));
        }
        let pending_delegated = delegated_task_id(dispatch_run)?;
        let mut owners = Vec::new();
        let mut found_pending_roots = BTreeSet::new();
        for run in self.supervisor.runs()? {
            let mut run_pending_roots = pending_roots
                .iter()
                .filter(|(run_id, _)| *run_id == run.supervisor_run_id)
                .copied()
                .collect::<Vec<_>>();
            if run.artifact_repository.is_some()
                && state
                    .starts
                    .get(&dispatch_run.to_string())
                    .is_some_and(|reservation| {
                        reservation.caller_dispatch_run_id.is_none()
                            && reservation.supervisor_run_id == run.supervisor_run_id
                    })
            {
                run_pending_roots
                    .push((run.supervisor_run_id, GOAL_REVIEW_READY_ARTIFACT_CONTRACT));
            }
            for (task_id, _) in run
                .provenance
                .iter()
                .filter(|(_, provenance)| provenance.dispatch_run_id == dispatch_run)
            {
                let owner = (run.supervisor_run_id, task_id.clone());
                if !owners.contains(&owner) {
                    owners.push(owner);
                }
            }
            for (_, expected_contract) in &run_pending_roots {
                found_pending_roots.insert(run.supervisor_run_id);
                let root = TaskId::new("root")?;
                if let Some(task) = run.tasks.get(&root) {
                    if !(task.supervisor_run_id == run.supervisor_run_id
                        && task.parent_task_id.is_none()
                        && task.required_artifact_contract == *expected_contract
                        && (expected_contract == &NO_ARTIFACT_CONTRACT
                            || run.artifact_repository.is_some()))
                    {
                        anyhow::bail!("retained supervisor root reservation is malformed");
                    }
                } else if run.state != SupervisorRunState::Planning {
                    anyhow::bail!("retained supervisor root reservation is malformed");
                }
                let owner = (run.supervisor_run_id, root);
                if !owners.contains(&owner) {
                    owners.push(owner);
                }
            }
            if run
                .tasks
                .get(&pending_delegated)
                .is_some_and(|task| is_delegated_reservation(task, dispatch_run))
            {
                let owner = (run.supervisor_run_id, pending_delegated.clone());
                if !owners.contains(&owner) {
                    owners.push(owner);
                }
            }
        }
        for (run_id, _) in pending_roots {
            if !found_pending_roots.contains(&run_id) {
                let owner = (run_id, TaskId::new("root")?);
                if !owners.contains(&owner) {
                    owners.push(owner);
                }
            }
        }
        Ok(owners)
    }

    /// Persists a delegated task before its Agent spawn. `None` means the
    /// parent dispatch is not supervised and classic delegation is unchanged.
    ///
    /// # Errors
    /// Returns an error for a conflicting child operation or durable reducer
    /// failure.
    #[allow(clippy::too_many_arguments)] // Reservation records every exact child identity before the Agent effect.
    pub fn reserve_delegated_dispatch_for_session(
        &self,
        parent_dispatch_run: OperationId,
        child_operation_id: &str,
        instruction: impl AsRef<str>,
        worker_session_id: SessionId,
        reserved_worker: &usagi_core::domain::agent::Agent,
        session_name: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<DelegatedDispatchReservation>> {
        self.reserve_delegated_dispatch_inner(
            parent_dispatch_run,
            child_operation_id,
            instruction,
            Some(worker_session_id),
            Some(reserved_worker),
            Some(session_name),
            false,
            false,
            now,
        )
    }

    /// Reserves an exact peer in the supervised caller's managed session.
    /// Unlike child-session delegation, an explicit peer handoff may select a
    /// different runtime; that runtime remains part of the durable fence.
    ///
    /// # Errors
    /// Returns an error for self/cross-session handoff, conflicting replay, or
    /// Supervisor policy and persistence failures.
    pub fn reserve_peer_handoff(
        &self,
        parent_dispatch_run: OperationId,
        child_operation_id: &str,
        instruction: impl AsRef<str>,
        reserved_worker: &usagi_core::domain::agent::Agent,
        session_name: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<DelegatedDispatchReservation>> {
        self.reserve_delegated_dispatch_inner(
            parent_dispatch_run,
            child_operation_id,
            instruction,
            reserved_worker.session_id,
            Some(reserved_worker),
            Some(session_name),
            false,
            true,
            now,
        )
    }

    #[cfg(test)]
    pub(super) fn reserve_delegated_dispatch(
        &self,
        parent_dispatch_run: OperationId,
        child_operation_id: &str,
        instruction: impl AsRef<str>,
        now: DateTime<Utc>,
    ) -> Result<Option<DelegatedDispatchReservation>> {
        self.reserve_delegated_dispatch_inner(
            parent_dispatch_run,
            child_operation_id,
            instruction,
            None,
            None,
            None,
            false,
            false,
            now,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Validation and durable reservation form one atomic Supervisor boundary.
    pub(super) fn reserve_delegated_dispatch_inner(
        &self,
        parent_dispatch_run: OperationId,
        child_operation_id: &str,
        instruction: impl AsRef<str>,
        worker_session_id: Option<SessionId>,
        reserved_worker: Option<&usagi_core::domain::agent::Agent>,
        session_name: Option<&str>,
        allow_existing_agent_operation: bool,
        peer_handoff: bool,
        now: DateTime<Utc>,
    ) -> Result<Option<DelegatedDispatchReservation>> {
        let instruction = instruction.as_ref();
        bounded_nonempty(
            "delegated supervisor instruction",
            instruction,
            MAX_SUPERVISOR_TEXT_BYTES,
        )?;
        let child_dispatch_run = OperationId::parse(child_operation_id)
            .context("delegated dispatch operation is invalid")?;
        let Some((mut run, parent_task_id)) = self.supervised_parent(parent_dispatch_run)? else {
            return Ok(None);
        };
        let worker_profile_id = if peer_handoff {
            let parent = self
                .dispatch
                .run(parent_dispatch_run)?
                .context("handoff parent dispatch is unavailable")?;
            let parent_agent = self
                .dispatch
                .agent(parent.agent_id)?
                .context("handoff parent Agent is unavailable")?;
            let worker = reserved_worker.context("handoff requires an exact peer Agent")?;
            if parent_agent.session_id.is_none()
                || worker.session_id != parent_agent.session_id
                || worker.agent_id == parent.agent_id
            {
                anyhow::bail!("handoff requires a distinct Agent in the caller's managed session");
            }
            Some(worker.runtime.clone())
        } else {
            worker_session_id
                .map(|_| self.dispatch_profile(parent_dispatch_run))
                .transpose()?
        };
        if let Some(worker) = reserved_worker
            && (worker.session_id != worker_session_id
                || worker_profile_id.as_ref() != Some(&worker.runtime))
        {
            anyhow::bail!("delegated Agent reservation is outside its Supervisor scope");
        }
        let worker_agent_id = reserved_worker.map(|worker| worker.agent_id);
        if child_dispatch_run == parent_dispatch_run {
            anyhow::bail!("delegated dispatch operation must differ from its parent");
        }
        let task_id = delegated_task_id(child_dispatch_run)?;
        if let Some(existing) = run.tasks.get(&task_id) {
            if existing.state.terminal()
                && existing.assigned_dispatch_run.is_none()
                && !run.provenance.contains_key(&task_id)
            {
                // A terminal unbound reservation may represent a refusal that
                // never reached Agent durable admission. Burning its operation
                // identity prevents a retry from spawning an unbindable worker.
                anyhow::bail!("delegated task conflicts with its existing supervisor task");
            }
            let authority_operation = if existing.state.terminal() {
                let provenance = run
                    .provenance
                    .get(&task_id)
                    .context("terminal delegated task has no dispatch provenance")?;
                validate_provenance_chain(&run, &task_id, provenance)?;
                provenance.dispatch_run_id
            } else {
                live_task_dispatch_authority(
                    &self.load_state()?,
                    &run,
                    &task_id,
                    &mut BTreeSet::new(),
                )?
                .context("delegated task has no current promotion authority")?
                .operation_id
            };
            let suffix = delegated_task_suffix(child_dispatch_run, instruction);
            let worker_semantic_digest = delegated_worker_semantic_digest(
                worker_agent_id,
                session_name,
                &existing.instruction_body,
            );
            let existing_parent_dispatch = match existing.promotion_parent_dispatch_run {
                Some(parent) => Some(parent),
                None => match run.provenance.get(&task_id) {
                    Some(provenance) => provenance.parent_dispatch_run,
                    None => None,
                },
            };
            if authority_operation != child_dispatch_run
                || !is_delegated_reservation(existing, child_dispatch_run)
                || existing.parent_task_id.as_ref() != Some(&parent_task_id)
                || matches!(existing_parent_dispatch, Some(parent) if parent != parent_dispatch_run)
                || matches!(existing.promotion_worker_session_id, Some(session) if Some(session) != worker_session_id)
                || matches!(&existing.promotion_worker_profile_id, Some(profile) if Some(profile) != worker_profile_id.as_ref())
                || matches!(existing.promotion_worker_agent_id, Some(agent) if Some(agent) != worker_agent_id)
                || matches!(&existing.promotion_worker_semantic_digest, Some(digest) if Some(digest) != worker_semantic_digest.as_ref())
                || (existing.instruction_body != instruction
                    && !existing.instruction_body.ends_with(&suffix))
                || existing.required_artifact_contract != NO_ARTIFACT_CONTRACT
            {
                anyhow::bail!("delegated task conflicts with its existing supervisor task");
            }
            return Ok(Some(DelegatedDispatchReservation {
                run: run.query(),
                prompt: existing.instruction_body.clone(),
            }));
        }
        self.ensure_new_delegated_operation_is_unused(
            child_dispatch_run,
            &task_id,
            allow_existing_agent_operation,
        )?;
        let mut policy_snapshot = run.clone();
        if let Some(parent) = policy_snapshot.tasks.get_mut(&parent_task_id)
            && parent.parent_task_id.is_none()
            && parent.assigned_dispatch_run.is_none()
            && parent.promotion_reserved_at.is_none()
            && has_caller_root_reservation(&self.load_state()?, run.supervisor_run_id)
        {
            // A pre-upgrade generic start can gain its durable caller join on
            // exact replay without a promotion timestamp in the older task
            // snapshot. Count that reserved root for policy admission too.
            parent.promotion_reserved_at = Some(now);
        }
        if let Some(reason) = child_dispatch_policy_denial(&policy_snapshot, &parent_task_id)? {
            if run.escalation.is_none() {
                let event = SupervisorEventKind::Escalate {
                    task_id: Some(parent_task_id.clone()),
                    reason: reason.clone(),
                    safe_evidence: "policy limits were evaluated before the delegated Agent effect"
                        .into(),
                    choices: vec!["resume".into(), "cancel".into()],
                };
                self.apply(&run, now, SupervisorEventSource::Admission, event)?;
            }
            anyhow::bail!("supervisor policy denied delegated dispatch: {reason}");
        }
        let prompt = delegated_handoff_prompt(&run, child_dispatch_run, instruction);
        let worker_semantic_digest =
            delegated_worker_semantic_digest(worker_agent_id, session_name, &prompt);
        let mut task = task_node(
            &run,
            task_id,
            Some(parent_task_id),
            BTreeSet::new(),
            prompt.clone(),
            NO_ARTIFACT_CONTRACT,
        );
        task.instruction_digest = delegated_task_digest(child_dispatch_run);
        task.promotion_reserved_at = Some(now);
        task.promotion_parent_dispatch_run = Some(parent_dispatch_run);
        task.promotion_worker_session_id = worker_session_id;
        task.promotion_worker_profile_id = worker_profile_id;
        task.promotion_worker_agent_id = worker_agent_id;
        task.promotion_worker_semantic_digest = worker_semantic_digest;
        run = self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::AddTask { task },
        )?;
        Ok(Some(DelegatedDispatchReservation {
            run: run.query(),
            prompt,
        }))
    }

    /// Marks a reserved delegated task failed after a definite spawn failure.
    ///
    /// # Errors
    /// Returns an error when no exact reservation exists or reducer state is
    /// inconsistent.
    pub fn fail_reserved_delegated_dispatch(
        &self,
        child_operation_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<SupervisorRunQuery>> {
        let operation = OperationId::parse(child_operation_id)
            .map_err(|_| anyhow::anyhow!("delegated dispatch operation is invalid"))?;
        let task_id = delegated_task_id(operation)?;
        let mut matches = self.unfinished_runs()?.into_iter().filter_map(|run| {
            let task = run.tasks.get(&task_id)?.clone();
            is_delegated_reservation(&task, operation).then_some((run, task))
        });
        let Some((mut run, task)) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            anyhow::bail!("delegated dispatch belongs to multiple supervisor runs");
        }
        if task.state.terminal() {
            return Ok(Some(run.query()));
        }
        run = self.resume_pending_promotion_escalation(run, &task_id, now)?;
        let run = self.apply(
            &run,
            now,
            SupervisorEventSource::DispatchFailure,
            SupervisorEventKind::Cancel {
                task_id: Some(task_id),
                reason: "delegated Agent admission failed before dispatch".into(),
            },
        )?;
        Ok(Some(run.query()))
    }

    /// Binds one admitted child Agent using only its exact durable reservation.
    /// Parent provenance and instruction remain single-sourced in the DAG.
    ///
    /// # Errors
    /// Returns an error for missing dispatch state, conflicting provenance,
    /// cross-workspace ownership, or reducer persistence failure.
    #[allow(clippy::too_many_lines)] // Binding validates the complete immutable reservation before provenance is committed.
    pub fn bind_reserved_delegated_dispatch(
        &self,
        child_operation_id: &str,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<Option<SupervisorRunQuery>> {
        let child_dispatch_run = OperationId::parse(child_operation_id)
            .context("delegated dispatch operation is invalid")?;
        let child_dispatch = self
            .dispatch
            .run(child_dispatch_run)?
            .context("delegated dispatch does not exist")?;
        let task_id = delegated_task_id(child_dispatch_run)?;
        let mut matches = self.unfinished_runs()?.into_iter().filter_map(|run| {
            let task = run.tasks.get(&task_id)?.clone();
            is_delegated_reservation(&task, child_dispatch_run).then_some((run, task))
        });
        let Some((mut run, task)) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            anyhow::bail!("delegated dispatch belongs to multiple supervisor runs");
        }
        let child_agent = if task.promotion_worker_session_id.is_some()
            || task.promotion_worker_profile_id.is_some()
            || task.promotion_worker_agent_id.is_some()
            || task.promotion_worker_semantic_digest.is_some()
        {
            Some(
                self.dispatch
                    .agent_in_workspace(worker.terminal.workspace_id, child_dispatch.agent_id)?
                    .context("delegated dispatch Agent does not exist")?,
            )
        } else {
            None
        };
        let child_semantic_digest = if task.promotion_worker_semantic_digest.is_some() {
            Some(usagi_core::infrastructure::ipc::agent_operation_digest(
                &self
                    .dispatch
                    .admission(child_dispatch_run)?
                    .context("delegated dispatch admission does not exist")?
                    .semantic_key,
            ))
        } else {
            None
        };
        if !delegated_worker_matches_reservation(
            run.workspace_id,
            worker,
            child_agent.as_ref(),
            &task,
            &child_dispatch,
            child_semantic_digest.as_ref(),
        ) {
            anyhow::bail!("delegated worker is outside its reserved supervisor scope");
        }
        let parent_task_id = task
            .parent_task_id
            .clone()
            .context("delegated supervisor task has no parent")?;
        let parent_dispatch_run = if let Some(reserved) = task.promotion_parent_dispatch_run {
            run.tasks
                .get(&parent_task_id)
                .filter(|parent| parent.supervisor_run_id == run.supervisor_run_id)
                .context("delegated parent task is missing")?;
            reserved
        } else {
            let parent = run
                .provenance
                .get(&parent_task_id)
                .context("delegated parent provenance is missing")?;
            validate_provenance_chain(&run, &parent_task_id, parent)?;
            parent.dispatch_run_id
        };
        let state = self.load_state()?;
        let authority = live_task_dispatch_authority(&state, &run, &task_id, &mut BTreeSet::new())?;
        let authority = authority.context("delegated dispatch promotion authority is missing")?;
        if authority.operation_id != child_dispatch_run {
            anyhow::bail!("delegated dispatch promotion fence is stale");
        }
        run = self.resume_pending_promotion_escalation(run, &task_id, now)?;
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: task_id.clone(),
            parent_task_id: Some(parent_task_id),
            parent_dispatch_run: Some(parent_dispatch_run),
            dispatch_run_id: child_dispatch_run,
            worker_session_id: worker.session_id,
            worker_agent_id: worker.agent_runtime_id,
            worker_worktree_id: worker.terminal.worktree_id,
            generation: task.generation,
        };
        if let Some(existing) = run.provenance.get(&task_id) {
            if existing == &provenance {
                return Ok(Some(run.query()));
            }
            anyhow::bail!("delegated dispatch provenance conflicts with the existing task");
        }
        let generation = task.generation;
        run = self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::Dispatch {
                task_id,
                generation,
                provenance,
            },
        )?;
        Ok(Some(run.query()))
    }

    pub(super) fn reserve_parent_wake(
        &self,
        run: &mut usagi_core::domain::supervisor::SupervisorRun,
        parent_id: &TaskId,
        child_run: OperationId,
        kind: InboxKind,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let parent = run.tasks.get(parent_id).cloned().expect("parent exists");
        let key = format!("{}:{}:{}", child_run, parent_id.0, parent.generation);
        let mut state = self.load_state()?;
        if state.wakes.contains_key(&key) || state.expired_wakes.contains(&key) {
            return Ok(());
        }
        if parent.state == TaskState::Running {
            let event = SupervisorEventKind::SetTaskState {
                task_id: parent_id.clone(),
                generation: parent.generation,
                state: TaskState::AwaitingDecision,
            };
            *run = self.apply(run, now, SupervisorEventSource::DispatchCompletion, event)?;
        }
        let parent = run.tasks.get(parent_id).expect("parent retained");
        if parent.state != TaskState::AwaitingDecision {
            return Ok(());
        }
        let Some(parent_provenance) = run.provenance.get(parent_id).cloned() else {
            return Ok(());
        };
        let outcome = self.outcome(child_run, kind)?;
        state.compact_delivered_wakes();
        if state.wakes.len() >= MAX_WAKE_RESERVATIONS {
            anyhow::bail!("supervisor wake reservation capacity is exhausted");
        }
        state.wakes.insert(
            key,
            WakeReservation {
                wake: DecisionWake {
                    supervisor_run_id: run.supervisor_run_id,
                    parent_task_id: parent_id.clone(),
                    parent_generation: parent.generation,
                    parent: parent_provenance,
                    child_run_id: child_run,
                    outcome,
                    dag: run
                        .tasks
                        .iter()
                        .map(|(id, task)| (id.clone(), task.state))
                        .collect(),
                    remaining_budget_summary: "policy has not configured a budget".into(),
                },
                delivered: false,
            },
        );
        self.save_state(&state)
    }

    pub(super) fn deliver_reserved(
        &self,
        now: DateTime<Utc>,
        waker: &mut dyn DecisionWaker,
    ) -> Result<()> {
        let mut state = self.load_state()?;
        let mut changed = false;
        let mut first_failure = None;
        let pending = state
            .wakes
            .iter()
            .filter(|(_, item)| !item.delivered)
            .map(|(key, item)| (key.clone(), item.wake.clone()))
            .collect::<Vec<_>>();
        for (key, wake) in pending {
            let delivered = waker
                .wake(&wake)
                .and_then(|()| self.resume_parent_after_wake(&wake, now));
            match delivered {
                Ok(()) => {
                    if let Some(reservation) = state.wakes.get_mut(&key) {
                        reservation.delivered = true;
                        changed = true;
                    }
                }
                Err(error) => {
                    first_failure.get_or_insert(error);
                }
            }
        }
        if changed {
            state.compact_delivered_wakes();
            self.save_state(&state)?;
        }
        first_failure.map_or(Ok(()), Err)
    }
}
