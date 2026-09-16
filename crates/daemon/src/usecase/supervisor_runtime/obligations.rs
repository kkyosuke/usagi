//! worker stop の義務と terminal handoff、artifact verification の予約・記録。

use anyhow::{Context, Result};

use super::{
    AMBIGUOUS_STOP_RESERVATION, ARTIFACT_RETRY_BASE_SECONDS, ARTIFACT_RETRY_MAX_SECONDS,
    ArtifactExpectation, ArtifactReportTrigger, ArtifactVerification, ArtifactVerificationRequest,
    ArtifactVerificationStatus, BTreeSet, DELEGATED_TASK_PREFIX, DateTime, DecisionWake,
    EscalationDecision, GOAL_REVIEW_READY_ARTIFACT_CONTRACT, HandoffContextEntry, InboxKind,
    MAX_HANDOFF_SUMMARY_BYTES, MAX_SUPERVISOR_KEY_BYTES, MAX_SUPERVISOR_TEXT_BYTES,
    MISSING_DISPATCH_ESCALATION_REASON, NO_ARTIFACT_CONTRACT, OperationId,
    PendingArtifactVerification, PendingCallerPromotion, PendingDelegatedPromotion,
    PendingGoalPromotion, PendingWorkerStop, RunProvenance, RunStatus, StructuredResult,
    SupervisorEventKind, SupervisorEventSource, SupervisorRun, SupervisorRunId, SupervisorRunQuery,
    SupervisorRunState, SupervisorRuntime, TaskId, TaskState, Utc, WorkspaceId, artifact_worktrees,
    bounded_nonempty, canonicalize, compact_handoff_text, has_unbound_root_worker,
    is_delegated_reservation, source, structured_artifact_summary,
};

impl SupervisorRuntime {
    /// Clears only the synthetic escalation produced by an older scheduler
    /// while this exact task's durable Agent promotion was still pending.
    /// Other human and policy escalations remain authoritative.
    pub(super) fn resume_pending_promotion_escalation(
        &self,
        run: SupervisorRun,
        task_id: &TaskId,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRun> {
        let Some(escalation_id) = run
            .escalation
            .as_ref()
            .filter(|escalation| {
                run.state == SupervisorRunState::Escalated
                    && escalation.blocking_task_id.as_ref() == Some(task_id)
                    && escalation.reason == MISSING_DISPATCH_ESCALATION_REASON
            })
            .map(|escalation| escalation.escalation_id)
        else {
            return Ok(run);
        };
        self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::ResolveEscalation {
                escalation_id,
                decision: EscalationDecision::Resume,
            },
        )
    }

    /// Lists Goal reservations whose root dispatch still needs provenance. The
    /// operation ID is returned from durable scheduler metadata rather than
    /// inferred from mutable Agent state.
    ///
    /// # Errors
    /// Returns an error when scheduler metadata or a referenced run is invalid.
    pub fn pending_goal_promotions(&self) -> Result<Vec<PendingGoalPromotion>> {
        let state = self.load_state()?;
        let mut pending = Vec::new();
        for (operation_id, reservation) in state.starts {
            let Some(run) = self.supervisor.load(reservation.supervisor_run_id)? else {
                continue;
            };
            let Some(workspace_id) = run.workspace_id else {
                continue;
            };
            let root = TaskId::new("root")?;
            if run.tasks.get(&root).is_some_and(|task| {
                task.required_artifact_contract == GOAL_REVIEW_READY_ARTIFACT_CONTRACT
            }) && !run.provenance.contains_key(&root)
                && !run.state.is_finished()
            {
                pending.push(PendingGoalPromotion {
                    operation_id,
                    reserved_at: run.created_at,
                    workspace_id,
                    worker_profile_id: reservation.worker_profile_id,
                    worker_semantic_digest: reservation.worker_semantic_digest,
                });
            }
        }
        Ok(pending)
    }

    /// Generic Supervisor roots whose authenticated caller dispatch was
    /// durably reserved before the root provenance could be bound.
    ///
    /// # Errors
    /// Returns an error when a retained caller reservation is malformed.
    pub fn pending_caller_promotions(&self) -> Result<Vec<PendingCallerPromotion>> {
        let state = self.load_state()?;
        let mut pending = Vec::new();
        for (start_operation_id, reservation) in &state.starts {
            let Some(dispatch_operation_id) = reservation.caller_dispatch_run_id else {
                continue;
            };
            let Some(run) = self.supervisor.load(reservation.supervisor_run_id)? else {
                // The reservation precedes aggregate initialization. Keep it
                // for the exact start retry without blocking other recovery.
                continue;
            };
            if run.state.is_finished() {
                continue;
            }
            let root_id = TaskId::new("root")?;
            let Some(root) = run.tasks.get(&root_id) else {
                if run.state == SupervisorRunState::Planning {
                    continue;
                }
                anyhow::bail!("pending caller root task is missing");
            };
            if reservation.workspace_id != run.workspace_id {
                anyhow::bail!("pending caller root workspace fence is stale");
            }
            if run.provenance.contains_key(&root_id) {
                continue;
            }
            if root.parent_task_id.is_some()
                || root.required_artifact_contract != NO_ARTIFACT_CONTRACT
                || root.state != TaskState::Ready
                || root.generation != 1
            {
                anyhow::bail!("pending caller root reservation is malformed");
            }
            pending.push(PendingCallerPromotion {
                start_operation_id: start_operation_id.clone(),
                dispatch_operation_id: dispatch_operation_id.to_string(),
                workspace_id: run
                    .workspace_id
                    .context("pending caller root has no workspace authority")?,
                worker_session_id: reservation.worker_session_id,
                worker_agent_id: reservation
                    .worker_agent_id
                    .context("pending caller root has no Agent identity authority")?,
                worker_profile_id: reservation
                    .worker_profile_id
                    .clone()
                    .context("pending caller root has no Agent profile authority")?,
                worker_runtime_id: reservation
                    .worker_runtime_id
                    .context("pending caller root has no Agent runtime authority")?,
                worker_semantic_digest: reservation
                    .worker_semantic_digest
                    .clone()
                    .context("pending caller root has no Agent semantic authority")?,
            });
        }
        Ok(pending)
    }

    /// Pending delegated task reservations recoverable from their stable task
    /// IDs and daemon-only origin marker.
    ///
    /// # Errors
    /// Returns an error when retained supervisor state is malformed.
    pub fn pending_delegated_promotions(&self) -> Result<Vec<PendingDelegatedPromotion>> {
        let mut pending = Vec::new();
        for run in self.unfinished_runs()? {
            if run.state.is_finished() {
                continue;
            }
            for task in run.tasks.values().filter(|task| {
                task.assigned_dispatch_run.is_none()
                    && task.state == TaskState::Ready
                    && task.generation == 1
            }) {
                let Some(operation_id) = task.task_id.0.strip_prefix(DELEGATED_TASK_PREFIX) else {
                    continue;
                };
                let Ok(operation) = OperationId::parse(operation_id) else {
                    continue;
                };
                if !is_delegated_reservation(task, operation) {
                    continue;
                }
                pending.push(PendingDelegatedPromotion {
                    operation_id: operation_id.into(),
                    reserved_at: task.promotion_reserved_at.unwrap_or(run.created_at),
                    workspace_id: run
                        .workspace_id
                        .context("delegated promotion has no workspace authority")?,
                    worker_session_id: task.promotion_worker_session_id,
                    worker_agent_id: task.promotion_worker_agent_id,
                    worker_profile_id: task.promotion_worker_profile_id.clone(),
                    worker_semantic_digest: task.promotion_worker_semantic_digest.clone(),
                });
            }
        }
        Ok(pending)
    }

    /// Lists exact operation joins for Agents admitted before a terminal
    /// Supervisor task could bind provenance. A delegated task can be terminal
    /// while its parent run remains live, so retained task state is the stop
    /// authority rather than only the run's terminal state.
    ///
    /// # Errors
    /// Returns an error for malformed durable reservations, ambiguous operation
    /// ownership, or missing delegated parent provenance/promotion authority.
    pub fn pending_worker_stops(&self) -> Result<Vec<PendingWorkerStop>> {
        self.pending_worker_stops_for(None)
    }

    /// Restricts unbound stop recovery to one run for a synchronous human
    /// control response.
    ///
    /// # Errors
    /// Returns the same durable-state errors as [`Self::pending_worker_stops`].
    pub fn pending_worker_stops_for_run(
        &self,
        supervisor_run_id: SupervisorRunId,
    ) -> Result<Vec<PendingWorkerStop>> {
        self.pending_worker_stops_for(Some(supervisor_run_id))
    }

    #[allow(clippy::too_many_lines)] // One inventory pass must reconcile root and recursive child stop fences consistently.
    pub(super) fn pending_worker_stops_for(
        &self,
        selected_run: Option<SupervisorRunId>,
    ) -> Result<Vec<PendingWorkerStop>> {
        let mut pending = Vec::new();
        let state = self.load_state()?;
        for (operation_id, reservation) in &state.starts {
            let Some(run) = self.supervisor.load(reservation.supervisor_run_id)? else {
                continue;
            };
            if (selected_run.is_some() && selected_run != Some(run.supervisor_run_id))
                || !matches!(
                    run.state,
                    SupervisorRunState::Cancelled | SupervisorRunState::Failed
                )
            {
                continue;
            }
            let Some(workspace_id) = run.workspace_id else {
                continue;
            };
            let root_id = TaskId::new("root")?;
            let Some(root) = run.tasks.get(&root_id) else {
                continue;
            };
            if run.provenance.contains_key(&root_id) {
                continue;
            }
            let (operation_id, requires_session) =
                if let Some(caller_dispatch) = reservation.caller_dispatch_run_id {
                    if root.required_artifact_contract != NO_ARTIFACT_CONTRACT {
                        anyhow::bail!("aborted caller root reservation is malformed");
                    }
                    (caller_dispatch, reservation.worker_session_id.is_some())
                } else {
                    if root.required_artifact_contract != GOAL_REVIEW_READY_ARTIFACT_CONTRACT {
                        continue;
                    }
                    (
                        OperationId::parse(operation_id)
                            .map_err(|_| anyhow::anyhow!("aborted Goal operation is invalid"))?,
                        false,
                    )
                };
            pending.push(PendingWorkerStop {
                operation_id,
                workspace_id,
                supervisor_run_id: run.supervisor_run_id,
                task_id: root_id,
                parent_task_id: None,
                parent_dispatch_run: None,
                generation: root.generation,
                requires_session,
                worker_session_id: reservation.worker_session_id,
                worker_agent_id: reservation.worker_agent_id,
                worker_runtime_id: reservation.worker_runtime_id,
                worker_profile_id: reservation.worker_profile_id.clone(),
                worker_semantic_digest: reservation.worker_semantic_digest.clone(),
            });
        }

        for run in self.supervisor.runs()? {
            if selected_run.is_some() && selected_run != Some(run.supervisor_run_id) {
                continue;
            }
            let Some(workspace_id) = run.workspace_id else {
                continue;
            };
            for task in run.tasks.values() {
                if task.assigned_dispatch_run.is_some()
                    || run.provenance.contains_key(&task.task_id)
                {
                    continue;
                }
                let Some(operation_id) = task.task_id.0.strip_prefix(DELEGATED_TASK_PREFIX) else {
                    continue;
                };
                let Ok(operation_id) = OperationId::parse(operation_id) else {
                    continue;
                };
                if !is_delegated_reservation(task, operation_id) {
                    continue;
                }
                if !task.state.terminal() {
                    continue;
                }
                let Some(parent_task_id) = task.parent_task_id.clone() else {
                    anyhow::bail!("terminal delegated reservation has no parent task");
                };
                if task.supervisor_run_id != run.supervisor_run_id
                    || task.generation != 1
                    || task.required_artifact_contract != NO_ARTIFACT_CONTRACT
                    || task.promotion_reserved_at.is_none()
                {
                    anyhow::bail!("terminal delegated reservation fence is stale");
                }
                let parent_dispatch_run = match task.promotion_parent_dispatch_run {
                    Some(reserved_parent) => {
                        run.tasks
                            .get(&parent_task_id)
                            .filter(|parent| parent.supervisor_run_id == run.supervisor_run_id)
                            .context("terminal delegated reservation parent task is missing")?;
                        reserved_parent
                    }
                    None => run
                        .provenance
                        .get(&parent_task_id)
                        .map(|parent| parent.dispatch_run_id)
                        .context("terminal delegated reservation parent provenance is missing")?,
                };
                pending.push(PendingWorkerStop {
                    operation_id,
                    workspace_id,
                    supervisor_run_id: run.supervisor_run_id,
                    task_id: task.task_id.clone(),
                    parent_task_id: Some(parent_task_id),
                    parent_dispatch_run: Some(parent_dispatch_run),
                    generation: task.generation,
                    requires_session: true,
                    worker_session_id: task.promotion_worker_session_id,
                    worker_agent_id: task.promotion_worker_agent_id,
                    worker_runtime_id: None,
                    worker_profile_id: task.promotion_worker_profile_id.clone(),
                    worker_semantic_digest: task.promotion_worker_semantic_digest.clone(),
                });
            }
        }

        pending.sort_by_key(PendingWorkerStop::operation_id);
        for pair in pending.windows(2) {
            if pair[0].operation_id == pair[1].operation_id && pair[0] != pair[1] {
                return Err(anyhow::Error::msg(AMBIGUOUS_STOP_RESERVATION));
            }
        }
        pending.dedup();
        Ok(pending)
    }

    /// Releases root-operation replay metadata only after Agent reconciliation
    /// stopped an aborted, unbound worker. Mere absence is not proof because
    /// Goal admission may still be waiting behind the Agent owner lock. Delegated
    /// operation identity remains encoded in its retained task and needs no
    /// separate scheduler reservation.
    ///
    /// # Errors
    /// Returns an error when a root candidate no longer matches the exact
    /// terminal run reservation or the durable state cannot be saved.
    pub fn acknowledge_pending_worker_stops(&self, stops: &[PendingWorkerStop]) -> Result<()> {
        let mut state = self.load_state()?;
        let mut changed = false;
        for stop in stops {
            if stop.parent_task_id.is_some() {
                continue;
            }
            let operation_id = stop.operation_id.to_string();
            let matching = state
                .starts
                .iter()
                .filter(|(start_operation, reservation)| {
                    reservation.supervisor_run_id == stop.supervisor_run_id
                        && (start_operation.as_str() == operation_id
                            || reservation.caller_dispatch_run_id == Some(stop.operation_id))
                })
                .map(|(start_operation, reservation)| {
                    (start_operation.clone(), reservation.clone())
                })
                .collect::<Vec<_>>();
            if matching.is_empty() {
                if state.expired_starts.contains(&operation_id) {
                    continue;
                }
                if state.starts.iter().any(|(start_operation, reservation)| {
                    start_operation == &operation_id
                        || reservation.caller_dispatch_run_id == Some(stop.operation_id)
                }) {
                    anyhow::bail!("aborted root operation changed run ownership");
                }
                anyhow::bail!("aborted root operation reservation disappeared");
            }
            if matching.len() != 1 {
                anyhow::bail!("aborted root operation has ambiguous reservations");
            }
            let (start_operation_id, reservation) = &matching[0];
            let Some(run) = self.supervisor.load(stop.supervisor_run_id)? else {
                anyhow::bail!("aborted root run disappeared");
            };
            if reservation.supervisor_run_id != stop.supervisor_run_id
                || !has_unbound_root_worker(&run, Some(reservation))
            {
                anyhow::bail!("aborted root worker stop acknowledgement is stale");
            }
            state.starts.remove(start_operation_id);
            state.expired_starts.insert(start_operation_id);
            state.expired_starts.insert(&operation_id);
            changed = true;
        }
        if changed {
            self.save_state(&state)?;
        }
        Ok(())
    }

    /// Lists completed contracted dispatches whose verification can be safely
    /// replayed after a daemon restart. The dispatch ID comes from persisted
    /// provenance; worker output is never used to select the task.
    ///
    /// # Errors
    /// Returns an error when retained supervisor or dispatch state is invalid.
    pub fn pending_artifact_verifications(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<PendingArtifactVerification>> {
        let mut pending = Vec::new();
        let completed = self
            .dispatch_runs()?
            .into_iter()
            .filter(|dispatch| dispatch.status == RunStatus::Completed)
            .map(|dispatch| dispatch.run_id)
            .collect::<BTreeSet<_>>();
        // This recovery lane is periodic. Hydrate one active aggregate at a
        // time so the 256-run admission bound cannot become a snapshot-sized
        // peak-memory multiplier.
        for id in self.supervisor.unfinished_run_ids()? {
            let run = self.load_indexed_run(id)?;
            if run.state != SupervisorRunState::Running {
                continue;
            }
            for task in run.tasks.values().filter(|task| {
                task.required_artifact_contract == GOAL_REVIEW_READY_ARTIFACT_CONTRACT
                    && task
                        .verification_retry_at
                        .is_none_or(|retry_at| retry_at <= now)
                    && matches!(
                        task.state,
                        TaskState::Dispatched | TaskState::Running | TaskState::Verifying
                    )
            }) {
                let provenance = run.provenance.get(&task.task_id).ok_or_else(|| {
                    anyhow::anyhow!("contracted supervisor task provenance is missing")
                })?;
                if provenance.generation != task.generation
                    || task.assigned_dispatch_run != Some(provenance.dispatch_run_id)
                {
                    anyhow::bail!("contracted supervisor task provenance fence is stale");
                }
                if completed.contains(&provenance.dispatch_run_id) {
                    pending.push(PendingArtifactVerification {
                        dispatch_run_id: provenance.dispatch_run_id,
                    });
                }
            }
        }
        pending.sort_by_key(|item| item.dispatch_run_id.to_string());
        pending.dedup();
        Ok(pending)
    }

    /// Moves a completed contracted dispatch to `Verifying` and captures its
    /// committed structured result under an exact task generation fence.
    ///
    /// # Errors
    /// Returns an error when dispatch/supervisor durable state is inconsistent.
    pub fn prepare_artifact_verification(
        &self,
        dispatch_run_id: OperationId,
        now: DateTime<Utc>,
    ) -> Result<Option<ArtifactVerificationRequest>> {
        self.prepare_artifact_verification_with_trigger(
            dispatch_run_id,
            now,
            ArtifactReportTrigger::Recovery,
        )
    }

    /// Prepares verification after the exact worker explicitly reported again.
    /// This is the only path which can leave artifact-rework waiting state.
    ///
    /// # Errors
    /// Returns an error when dispatch, candidate, or supervisor provenance is
    /// inconsistent or cannot be persisted.
    pub fn prepare_artifact_verification_after_report(
        &self,
        dispatch_run_id: OperationId,
        result: Option<StructuredResult>,
        now: DateTime<Utc>,
    ) -> Result<Option<ArtifactVerificationRequest>> {
        self.prepare_artifact_verification_with_trigger(
            dispatch_run_id,
            now,
            ArtifactReportTrigger::Fresh(result),
        )
    }

    pub(super) fn prepare_artifact_verification_with_trigger(
        &self,
        dispatch_run_id: OperationId,
        now: DateTime<Utc>,
        trigger: ArtifactReportTrigger,
    ) -> Result<Option<ArtifactVerificationRequest>> {
        let fresh_report = matches!(&trigger, ArtifactReportTrigger::Fresh(_));
        let mut found = self.unfinished_runs()?.into_iter().filter_map(|run| {
            let task = run
                .provenance
                .iter()
                .find(|(_, provenance)| provenance.dispatch_run_id == dispatch_run_id)
                .map(|(task, _)| task.clone());
            task.map(|task| (run, task))
        });
        let Some((mut run, task_id)) = found.next() else {
            return Ok(None);
        };
        if found.next().is_some() {
            anyhow::bail!("dispatch belongs to multiple supervisor runs");
        }
        let task = run
            .tasks
            .get(&task_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor task is missing"))?;
        if task.required_artifact_contract != GOAL_REVIEW_READY_ARTIFACT_CONTRACT
            || run.state != SupervisorRunState::Running
        {
            return Ok(None);
        }
        let task_state = task.state;
        let generation = task.generation;
        let verification_attempt = task.verification_attempt;
        let previous_verification_digest = task.verification_digest.clone();
        let expectation = task.verification_expectation.clone();
        let contract = task.required_artifact_contract;
        let workspace_id = run
            .workspace_id
            .ok_or_else(|| anyhow::anyhow!("artifact run workspace is missing"))?;
        if !fresh_report
            && task
                .verification_retry_at
                .is_some_and(|retry_at| retry_at > now)
        {
            return Ok(None);
        }
        let dispatch = self
            .dispatch
            .run(dispatch_run_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact dispatch is missing"))?;
        if dispatch.status != RunStatus::Completed {
            return Ok(None);
        }
        if task.state == TaskState::Dispatched {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::DispatchCompletion,
                SupervisorEventKind::Running {
                    task_id: task_id.clone(),
                    generation,
                },
            )?;
        }
        if matches!(task_state, TaskState::Dispatched | TaskState::Running)
            || (fresh_report && task_state == TaskState::AwaitingDecision)
        {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::DispatchCompletion,
                SupervisorEventKind::SetTaskState {
                    task_id: task_id.clone(),
                    generation,
                    state: TaskState::Succeeded,
                },
            )?;
        } else if task_state != TaskState::Verifying {
            return Ok(None);
        }
        let Some(repository) = run.artifact_repository.clone() else {
            self.reject_missing_artifact_repository(&run, task_id, generation, now)?;
            return Ok(None);
        };
        let (run, result) = self.prepare_artifact_candidate(
            run,
            &task_id,
            generation,
            dispatch_run_id,
            trigger,
            now,
        )?;
        let worktrees = artifact_worktrees(&run);
        Ok(Some(ArtifactVerificationRequest {
            supervisor_run_id: run.supervisor_run_id,
            task_id,
            generation,
            verification_attempt,
            previous_verification_digest,
            workspace_id,
            contract,
            repository,
            result,
            expectation,
            worktrees,
        }))
    }

    pub(super) fn prepare_artifact_candidate(
        &self,
        mut run: SupervisorRun,
        task_id: &TaskId,
        generation: u64,
        dispatch_run_id: OperationId,
        trigger: ArtifactReportTrigger,
        now: DateTime<Utc>,
    ) -> Result<(SupervisorRun, Option<StructuredResult>)> {
        let (fresh, reported_result) = match trigger {
            ArtifactReportTrigger::Fresh(result) => (true, result),
            ArtifactReportTrigger::Recovery => {
                let result = match self.dispatch.binding(dispatch_run_id)? {
                    Some(binding) => self
                        .dispatch
                        .inbox(&binding.caller)?
                        .into_iter()
                        .find(|message| message.run_id == dispatch_run_id)
                        .and_then(|message| message.result),
                    None => None,
                };
                (false, result)
            }
        };
        let candidate_pr = reported_result
            .as_ref()
            .and_then(|result| result.pr.as_deref())
            .and_then(canonicalize)
            .map(|identity| identity.as_url().to_owned());
        if fresh || !run.verification_candidates.contains_key(task_id) {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::Verification,
                SupervisorEventKind::VerificationCandidateRecorded {
                    task_id: task_id.clone(),
                    generation,
                    candidate_pr,
                },
            )?;
        }
        let mut result = reported_result.unwrap_or_default();
        result.pr = run.verification_candidates.get(task_id).cloned().flatten();
        Ok((run, Some(result)))
    }

    pub(super) fn reject_missing_artifact_repository(
        &self,
        run: &SupervisorRun,
        task_id: TaskId,
        generation: u64,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.apply(
            run,
            now,
            SupervisorEventSource::Verification,
            SupervisorEventKind::VerificationResult {
                task_id,
                generation,
                passed: false,
                result_digest: "missing-pre-spawn-repository".into(),
                safe_summary: "artifact repository was not recorded before Goal worker spawn"
                    .into(),
            },
        )
        .map(drop)
    }

    /// Durably pins trusted Git facts before the provider is queried. Replays
    /// with the same expectation are idempotent; a changed expectation is a
    /// provenance violation.
    ///
    /// # Errors
    /// Returns an error when the request fence is stale or persistence fails.
    pub fn record_artifact_expectation(
        &self,
        request: &ArtifactVerificationRequest,
        expectation: &ArtifactExpectation,
        now: DateTime<Utc>,
    ) -> Result<ArtifactVerificationRequest> {
        let run = self.load_started_run(request.supervisor_run_id)?;
        let task = run
            .tasks
            .get(&request.task_id)
            .ok_or_else(|| anyhow::anyhow!("artifact verification task is missing"))?;
        if task.generation != request.generation
            || task.required_artifact_contract != request.contract
            || task.state != TaskState::Verifying
            || task.verification_attempt != request.verification_attempt
            || run.artifact_repository.as_ref() != Some(&request.repository)
            || expectation.repository() != &request.repository
        {
            anyhow::bail!("artifact expectation fence is stale");
        }
        let run = if task.verification_expectation.as_ref() == Some(expectation) {
            run
        } else {
            self.apply(
                &run,
                now,
                SupervisorEventSource::Verification,
                SupervisorEventKind::VerificationExpectationRecorded {
                    task_id: request.task_id.clone(),
                    generation: request.generation,
                    expectation: expectation.clone(),
                },
            )?
        };
        let mut pinned = request.clone();
        pinned
            .expectation
            .clone_from(&run.tasks[&request.task_id].verification_expectation);
        Ok(pinned)
    }

    /// Commits one independently obtained verification result and finalizes the
    /// run only when every tracked task has succeeded.
    ///
    /// # Errors
    /// Returns an error when the request fence is stale or durable state cannot
    /// be updated.
    pub fn record_artifact_verification(
        &self,
        request: &ArtifactVerificationRequest,
        verification: ArtifactVerification,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        bounded_nonempty(
            "artifact verification digest",
            &verification.result_digest,
            MAX_SUPERVISOR_KEY_BYTES,
        )?;
        if verification.safe_summary.len() > MAX_SUPERVISOR_TEXT_BYTES {
            anyhow::bail!(
                "invalid artifact verification summary: maximum is {MAX_SUPERVISOR_TEXT_BYTES} UTF-8 bytes"
            );
        }
        let run = self.load_started_run(request.supervisor_run_id)?;
        let task = run
            .tasks
            .get(&request.task_id)
            .ok_or_else(|| anyhow::anyhow!("artifact verification task is missing"))?;
        if task.generation == request.generation
            && task.required_artifact_contract == request.contract
            && (run.state != SupervisorRunState::Running || task.state.terminal())
        {
            return Ok(run.query());
        }
        if task.generation != request.generation
            || task.required_artifact_contract != request.contract
            || run.artifact_repository.as_ref() != Some(&request.repository)
            || task.state != TaskState::Verifying
        {
            anyhow::bail!("artifact verification fence is stale");
        }
        if task.verification_attempt != request.verification_attempt {
            return if task.verification_attempt > request.verification_attempt {
                Ok(run.query())
            } else {
                anyhow::bail!("artifact verification attempt fence is stale")
            };
        }
        if task.verification_expectation != request.expectation {
            anyhow::bail!("artifact verification expectation fence is stale");
        }
        if verification.status == ArtifactVerificationStatus::Verified
            && request.expectation.is_none()
        {
            anyhow::bail!("verified artifact expectation is missing");
        }
        let kind = match verification.status {
            ArtifactVerificationStatus::Verified => SupervisorEventKind::VerificationResult {
                task_id: request.task_id.clone(),
                generation: request.generation,
                passed: true,
                result_digest: verification.result_digest,
                safe_summary: verification.safe_summary,
            },
            ArtifactVerificationStatus::Rejected => SupervisorEventKind::VerificationResult {
                task_id: request.task_id.clone(),
                generation: request.generation,
                passed: false,
                result_digest: verification.result_digest,
                safe_summary: verification.safe_summary,
            },
            ArtifactVerificationStatus::Retryable => {
                let exponent = request.verification_attempt.min(30);
                let delay = ARTIFACT_RETRY_BASE_SECONDS
                    .saturating_mul(1_i64 << exponent)
                    .min(ARTIFACT_RETRY_MAX_SECONDS);
                SupervisorEventKind::VerificationDeferred {
                    task_id: request.task_id.clone(),
                    generation: request.generation,
                    result_digest: verification.result_digest,
                    safe_summary: verification.safe_summary,
                    retry_at: now + chrono::Duration::seconds(delay),
                }
            }
        };
        let run = self.apply(&run, now, SupervisorEventSource::Verification, kind)?;
        Ok(self.finalize_terminal_tasks(run, now)?.query())
    }

    /// Returns exact worker provenance still owned by aborted runs. Agent
    /// records are the termination authority; this list is the durable join
    /// from a terminal Supervisor fact to those exact runtime identities.
    ///
    /// # Errors
    /// Returns an error when durable state is corrupt or provenance no longer
    /// fences the task generation and dispatch recorded by its run.
    pub fn worker_stop_obligations(&self) -> Result<Vec<(WorkspaceId, RunProvenance)>> {
        self.worker_stop_obligations_for(None)
    }

    /// Restricts bound stop recovery to one run for a synchronous control.
    ///
    /// # Errors
    /// Returns the same durable-state errors as [`Self::worker_stop_obligations`].
    pub fn worker_stop_obligations_for_run(
        &self,
        supervisor_run_id: SupervisorRunId,
    ) -> Result<Vec<(WorkspaceId, RunProvenance)>> {
        self.worker_stop_obligations_for(Some(supervisor_run_id))
    }

    pub(super) fn worker_stop_obligations_for(
        &self,
        selected_run: Option<SupervisorRunId>,
    ) -> Result<Vec<(WorkspaceId, RunProvenance)>> {
        let mut obligations = Vec::new();
        for run in self.aborted_runs()? {
            if (selected_run.is_some() && selected_run != Some(run.supervisor_run_id))
                || !matches!(
                    run.state,
                    SupervisorRunState::Cancelled | SupervisorRunState::Failed
                )
            {
                continue;
            }
            let Some(workspace) = run.workspace_id else {
                // Legacy unscoped history cannot authorize a process signal.
                // It is intentionally invisible to the workspace control plane.
                continue;
            };
            for (task_id, provenance) in &run.provenance {
                let Some(task) = run.tasks.get(task_id) else {
                    anyhow::bail!("aborted supervisor provenance task is missing");
                };
                if task.assigned_dispatch_run.is_none() && provenance.generation < task.generation {
                    // A completed earlier attempt is historical provenance,
                    // not a worker still owned by the cancelled generation.
                    continue;
                }
                if provenance.supervisor_run_id != run.supervisor_run_id
                    || provenance.task_id != *task_id
                    || provenance.generation != task.generation
                    || task.assigned_dispatch_run != Some(provenance.dispatch_run_id)
                {
                    anyhow::bail!("aborted supervisor worker provenance fence is stale");
                }
                obligations.push((workspace, provenance.clone()));
            }
        }
        obligations.sort_by_key(|(workspace, provenance)| {
            (workspace.to_string(), provenance.worker_agent_id.as_str())
        });
        obligations.dedup();
        Ok(obligations)
    }

    pub(super) fn finalize_terminal_tasks(
        &self,
        run: SupervisorRun,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRun> {
        if run.state != SupervisorRunState::Running
            || run.tasks.is_empty()
            || !run.tasks.values().all(|task| task.state.terminal())
        {
            return Ok(run);
        }
        let succeeded = run
            .tasks
            .values()
            .all(|task| task.state == TaskState::Succeeded);
        let (source, terminal_reason) = if succeeded {
            (SupervisorEventSource::DispatchCompletion, None)
        } else if run
            .tasks
            .values()
            .any(|task| task.state == TaskState::Failed)
        {
            (
                SupervisorEventSource::DispatchFailure,
                Some("one or more supervisor tasks failed".into()),
            )
        } else if run
            .tasks
            .values()
            .any(|task| task.state == TaskState::Blocked)
        {
            (
                SupervisorEventSource::DispatchFailure,
                Some("one or more supervisor tasks were blocked".into()),
            )
        } else {
            (
                SupervisorEventSource::Cancel,
                Some("one or more supervisor tasks were cancelled".into()),
            )
        };
        self.apply(
            &run,
            now,
            source,
            SupervisorEventKind::SetRunState {
                state: if succeeded {
                    SupervisorRunState::Succeeded
                } else {
                    SupervisorRunState::Failed
                },
                terminal_reason,
            },
        )
    }

    pub(super) fn handoff_entry(
        &self,
        task_id: &TaskId,
        generation: u64,
        dispatch_run_id: OperationId,
        fallback: InboxKind,
        recorded_at: DateTime<Utc>,
    ) -> Result<HandoffContextEntry> {
        let message = self.dispatch.binding(dispatch_run_id)?.and_then(|binding| {
            self.dispatch
                .inbox(&binding.caller)
                .ok()
                .and_then(|messages| {
                    messages
                        .into_iter()
                        .find(|message| message.run_id == dispatch_run_id)
                })
        });
        let message = message.filter(|message| message.kind == fallback);
        let summary = message.as_ref().map_or_else(
            || "worker terminal state committed without an inbox report".to_owned(),
            |message| compact_handoff_text(&message.summary, MAX_HANDOFF_SUMMARY_BYTES),
        );
        let artifacts = message
            .as_ref()
            .and_then(|message| message.result.as_ref())
            .and_then(structured_artifact_summary);
        Ok(HandoffContextEntry {
            task_id: task_id.clone(),
            generation,
            dispatch_run_id,
            outcome: fallback,
            summary,
            artifacts,
            recorded_at,
        })
    }

    pub(super) fn record_terminal_handoff(
        &self,
        run: SupervisorRun,
        task_id: &TaskId,
        provenance: &RunProvenance,
        kind: InboxKind,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRun> {
        let current = run.tasks.get(task_id).expect("task retained");
        let captures_handoff = current.state == TaskState::Succeeded
            || current.state == TaskState::Failed
            || current.state == TaskState::Verifying;
        if !captures_handoff
            || current.generation != provenance.generation
            || current.assigned_dispatch_run != Some(provenance.dispatch_run_id)
            || run
                .handoff_context
                .iter()
                .any(|entry| entry.dispatch_run_id == provenance.dispatch_run_id)
        {
            return Ok(run);
        }
        self.handoff_entry(
            task_id,
            current.generation,
            provenance.dispatch_run_id,
            kind,
            now,
        )
        .and_then(|entry| {
            self.apply(
                &run,
                now,
                source(kind),
                SupervisorEventKind::RecordHandoff { entry },
            )
        })
    }

    pub(super) fn resume_parent_after_wake(
        &self,
        wake: &DecisionWake,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let Some(run) = self.supervisor.load(wake.supervisor_run_id)? else {
            anyhow::bail!("supervisor parent wake run is unavailable");
        };
        let task = run
            .tasks
            .get(&wake.parent_task_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor parent wake task is unavailable"))?;
        if task.generation != wake.parent_generation
            || run.provenance.get(&wake.parent_task_id) != Some(&wake.parent)
        {
            anyhow::bail!("supervisor parent wake fence is stale");
        }
        if task.state == TaskState::AwaitingDecision {
            self.apply(
                &run,
                now,
                SupervisorEventSource::Admission,
                SupervisorEventKind::SetTaskState {
                    task_id: wake.parent_task_id.clone(),
                    generation: wake.parent_generation,
                    state: TaskState::Running,
                },
            )?;
        } else if task.state != TaskState::Running && !task.state.terminal() {
            anyhow::bail!("supervisor parent wake task is not resumable");
        }
        Ok(())
    }
}
