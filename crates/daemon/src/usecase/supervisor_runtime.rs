//! Event-driven bridge between durable dispatch completion and supervisor runs.
//!
//! The daemon owns one [`SupervisorRuntime`] and calls [`SupervisorRuntime::tick`]
//! for an arriving completion, startup reconciliation, or an explicit wake.  A
//! tick never polls: it only examines the named run, persists reducer facts and
//! wake reservations, then performs the finite set of reserved wake effects.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use usagi_core::{
    domain::{
        agent::{InboxKind, RunStatus, StructuredResult},
        id::{AgentRuntimeRef, OperationId, WorkspaceId},
        supervisor::{
            ArtifactContract, EscalationDecision, GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            MAX_INITIAL_TASKS, MAX_SUPERVISOR_KEY_BYTES, MAX_SUPERVISOR_REASON_BYTES,
            MAX_SUPERVISOR_TEXT_BYTES, MAX_TASK_DEPENDENCIES, NO_ARTIFACT_CONTRACT, RunProvenance,
            SupervisorEvent, SupervisorEventKind, SupervisorEventSource, SupervisorRun,
            SupervisorRunId, SupervisorRunQuery, SupervisorRunState, TaskId, TaskNode, TaskState,
            presentation_text_is_safe,
        },
    },
    infrastructure::{
        persistence::json_file,
        store::{
            dispatch::DispatchStore,
            supervisor::{
                EventCursor, EventQuery, RUN_LIST_RESPONSE_MAX_BYTES, SupervisorRunPage,
                SupervisorStore,
            },
        },
    },
};

/// Redaction-safe input delivered to the parent-agent wake adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionWake {
    pub supervisor_run_id: SupervisorRunId,
    pub parent_task_id: TaskId,
    pub parent_generation: u64,
    pub parent: RunProvenance,
    pub child_run_id: OperationId,
    pub outcome: WakeOutcome,
    pub dag: Vec<(TaskId, TaskState)>,
    pub remaining_budget_summary: String,
}

/// The safe terminal fact passed to a decision maker; worker terminal output is
/// deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeOutcome {
    pub kind: InboxKind,
    pub summary: String,
}

/// Redaction-safe result produced outside the supervisor lock by an independent
/// artifact verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactVerification {
    pub passed: bool,
    pub result_digest: String,
    pub safe_summary: String,
}

/// Provider boundary used after a worker completion has moved a contracted
/// task into `Verifying`. Worker-controlled output is input, never authority.
pub trait ArtifactVerifier {
    fn verify(
        &mut self,
        contract: ArtifactContract,
        result: Option<&StructuredResult>,
    ) -> ArtifactVerification;
}

/// Exact task fence and worker-reported candidate prepared under the supervisor
/// lock, then independently verified without holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactVerificationRequest {
    pub supervisor_run_id: SupervisorRunId,
    pub task_id: TaskId,
    pub generation: u64,
    pub contract: ArtifactContract,
    pub result: Option<StructuredResult>,
}

/// Durable Goal operation whose reserved root still needs exact provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGoalPromotion {
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDelegatedPromotion {
    pub operation_id: String,
}

/// Completed contracted dispatch whose independent artifact verification has
/// not reached a terminal supervisor state yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingArtifactVerification {
    pub dispatch_run_id: OperationId,
}

/// Composition-root adapter. Implementations use the persisted parent
/// provenance to resolve/restart the parent session and send the request.
pub trait DecisionWaker {
    /// # Errors
    ///
    /// Returns an error when the parent session cannot safely receive the wake.
    fn wake(&mut self, wake: &DecisionWake) -> Result<()>;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RuntimeState {
    wakes: BTreeMap<String, WakeReservation>,
    starts: BTreeMap<String, StartReservation>,
    #[serde(default)]
    expired_wakes: KeyTombstones,
    #[serde(default)]
    expired_starts: KeyTombstones,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WakeReservation {
    wake: DecisionWake,
    delivered: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StartReservation {
    semantic_key: String,
    supervisor_run_id: SupervisorRunId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct KeyTombstones {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    words: Vec<u64>,
}

const TOMBSTONE_WORDS: usize = 512;
const TOMBSTONE_HASHES: u64 = 4;
const DELEGATED_TASK_PREFIX: &str = "delegated-";
const DELEGATED_TASK_DIGEST_PREFIX: &str = "delegated-operation:";
#[cfg(not(test))]
const MAX_START_RESERVATIONS: usize = 256;
#[cfg(test)]
const MAX_START_RESERVATIONS: usize = 8;
#[cfg(not(test))]
const MAX_WAKE_RESERVATIONS: usize = 512;
#[cfg(test)]
const MAX_WAKE_RESERVATIONS: usize = 8;
#[cfg(not(test))]
const RETAIN_DELIVERED_WAKES: usize = 128;
#[cfg(test)]
const RETAIN_DELIVERED_WAKES: usize = 4;

/// Applies the same serialized budget to every read-only supervisor query, not
/// only list pages. The caller maps this capacity refusal to `resource_exhausted`.
///
/// # Errors
/// Returns an error when serialization fails or the response exceeds the budget.
pub fn bounded_supervisor_query(value: serde_json::Value) -> Result<serde_json::Value> {
    if serde_json::to_vec(&value)?.len() > RUN_LIST_RESPONSE_MAX_BYTES {
        anyhow::bail!("supervisor query response capacity is exhausted");
    }
    Ok(value)
}

impl KeyTombstones {
    fn bit(key: &str, seed: u64) -> usize {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        for byte in key.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        usize::try_from(hash % (TOMBSTONE_WORDS as u64 * 64)).expect("bit index fits")
    }

    fn contains(&self, key: &str) -> bool {
        self.words.len() == TOMBSTONE_WORDS
            && (0..TOMBSTONE_HASHES).all(|seed| {
                let bit = Self::bit(key, seed);
                self.words[bit / 64] & (1_u64 << (bit % 64)) != 0
            })
    }

    fn insert(&mut self, key: &str) {
        self.words.resize(TOMBSTONE_WORDS, 0);
        self.words.truncate(TOMBSTONE_WORDS);
        for seed in 0..TOMBSTONE_HASHES {
            let bit = Self::bit(key, seed);
            self.words[bit / 64] |= 1_u64 << (bit % 64);
        }
    }
}

impl RuntimeState {
    fn validate_limits(&self) -> Result<()> {
        let tombstones_are_valid = |tombstones: &KeyTombstones| {
            tombstones.words.is_empty() || tombstones.words.len() == TOMBSTONE_WORDS
        };
        if self.starts.len() > MAX_START_RESERVATIONS
            || self.wakes.len() > MAX_WAKE_RESERVATIONS
            || !tombstones_are_valid(&self.expired_starts)
            || !tombstones_are_valid(&self.expired_wakes)
        {
            anyhow::bail!("supervisor runtime metadata exceeds or violates its hard limit");
        }
        Ok(())
    }

    fn compact_delivered_wakes(&mut self) {
        let undelivered = self
            .wakes
            .values()
            .filter(|reservation| !reservation.delivered)
            .count();
        let keep_delivered = RETAIN_DELIVERED_WAKES.min(
            MAX_WAKE_RESERVATIONS
                .saturating_sub(undelivered)
                .min(self.wakes.len()),
        );
        let remove = self
            .wakes
            .values()
            .filter(|reservation| reservation.delivered)
            .count()
            .saturating_sub(keep_delivered);
        let keys = self
            .wakes
            .iter()
            .filter(|(_, reservation)| reservation.delivered)
            .take(remove)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            self.wakes.remove(&key);
            self.expired_wakes.insert(&key);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialTask {
    pub task_id: String,
    /// Durable manager task. Omitted children belong to the root Director task.
    #[serde(default)]
    pub parent_task_id: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub instruction: String,
    #[serde(default)]
    pub required_artifact_contract: ArtifactContract,
}

fn bounded_nonempty(name: &str, value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max {
        anyhow::bail!("invalid {name}: expected 1..={max} UTF-8 bytes");
    }
    Ok(())
}

fn bounded_safe_label(name: &str, value: &str, max: usize) -> Result<()> {
    bounded_nonempty(name, value, max)?;
    if !presentation_text_is_safe(value) {
        anyhow::bail!(
            "invalid {name}: control and bidirectional formatting characters are forbidden"
        );
    }
    Ok(())
}

fn validate_start_input(
    operation_id: &str,
    root_task: &str,
    initial_tasks: &[InitialTask],
    policy_selector: Option<&str>,
) -> Result<()> {
    bounded_nonempty(
        "supervisor idempotency key",
        operation_id,
        MAX_SUPERVISOR_KEY_BYTES,
    )?;
    bounded_nonempty("supervisor root task", root_task, MAX_SUPERVISOR_TEXT_BYTES)?;
    if initial_tasks.len() > MAX_INITIAL_TASKS {
        anyhow::bail!("invalid initial task count: maximum is {MAX_INITIAL_TASKS}");
    }
    if let Some(policy_selector) = policy_selector {
        bounded_nonempty(
            "supervisor policy selector",
            policy_selector,
            MAX_SUPERVISOR_KEY_BYTES,
        )?;
    }
    for task in initial_tasks {
        TaskId::new(&task.task_id).map_err(anyhow::Error::msg)?;
        if let Some(parent) = &task.parent_task_id {
            TaskId::new(parent).map_err(anyhow::Error::msg)?;
        }
        if task.dependencies.len() > MAX_TASK_DEPENDENCIES {
            anyhow::bail!("invalid task dependency count: maximum is {MAX_TASK_DEPENDENCIES}");
        }
        for dependency in &task.dependencies {
            TaskId::new(dependency).map_err(anyhow::Error::msg)?;
        }
        bounded_nonempty(
            "supervisor task instruction",
            &task.instruction,
            MAX_SUPERVISOR_TEXT_BYTES,
        )?;
    }
    Ok(())
}

fn push_semantic_component(key: &mut String, value: &str) {
    key.push_str(&value.len().to_string());
    key.push(':');
    key.push_str(value);
}

fn delegated_task_id(operation: OperationId) -> Result<TaskId> {
    TaskId::new(format!("{DELEGATED_TASK_PREFIX}{operation}")).map_err(anyhow::Error::msg)
}

fn delegated_task_digest(operation: OperationId) -> String {
    format!("{DELEGATED_TASK_DIGEST_PREFIX}{operation}")
}

fn is_delegated_reservation(task: &TaskNode, operation: OperationId) -> bool {
    task.task_id.0 == format!("{DELEGATED_TASK_PREFIX}{operation}")
        && task.instruction_digest == delegated_task_digest(operation)
}

/// The single daemon-owned scheduler runtime. It is intentionally independent
/// of IPC connections: disconnecting a client cannot drop reservations.
pub struct SupervisorRuntime {
    supervisor: SupervisorStore,
    dispatch: DispatchStore,
    state_path: PathBuf,
    apply_fail_at: Cell<Option<usize>>,
    apply_calls: Cell<usize>,
    #[cfg(test)]
    dispatch_registry_reads: Cell<usize>,
}

impl SupervisorRuntime {
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            supervisor: SupervisorStore::new(state_dir),
            dispatch: DispatchStore::new(state_dir),
            state_path: state_dir.join("supervisor-scheduler.json"),
            apply_fail_at: Cell::new(None),
            apply_calls: Cell::new(0),
            #[cfg(test)]
            dispatch_registry_reads: Cell::new(0),
        }
    }

    #[cfg(test)]
    fn fail_apply_at(&self, call: usize) {
        self.apply_fail_at.set(Some(call));
    }

    /// Starts one durable run. The operation key is reserved before aggregate
    /// initialization, so retrying after a disconnect reuses the same run ID.
    ///
    /// # Errors
    /// Returns an error for conflicting idempotency, invalid DAGs, or durable IO failure.
    ///
    #[allow(clippy::too_many_lines)]
    pub fn start(
        &self,
        caller: &str,
        operation_id: &str,
        root_task: String,
        initial_tasks: Vec<InitialTask>,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.start_scoped(
            caller,
            None,
            operation_id,
            root_task,
            NO_ARTIFACT_CONTRACT,
            initial_tasks,
            policy_selector,
            now,
        )
    }

    /// Starts a run owned by one daemon-admitted workspace. This is the
    /// production entry point; the unscoped wrapper remains for legacy callers
    /// and deterministic domain fixtures.
    ///
    /// # Errors
    ///
    /// Returns an error when admission input is invalid or the durable run
    /// cannot be initialized.
    #[allow(clippy::too_many_arguments)]
    pub fn start_for_workspace(
        &self,
        caller: &str,
        workspace: WorkspaceId,
        operation_id: &str,
        root_task: String,
        initial_tasks: Vec<InitialTask>,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.start_scoped(
            caller,
            Some(workspace),
            operation_id,
            root_task,
            NO_ARTIFACT_CONTRACT,
            initial_tasks,
            policy_selector,
            now,
        )
    }

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
        root_task: String,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.start_scoped(
            caller,
            Some(workspace),
            operation_id,
            root_task,
            GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            Vec::new(),
            policy_selector,
            now,
        )
    }

    /// Starts a Goal run and binds its root task to an already admitted
    /// workspace-root Agent dispatch. Production reserves before spawn and
    /// calls [`Self::bind_reserved_workspace_root_dispatch`] afterwards; this
    /// composed entry point remains useful to exact-retry callers and tests.
    ///
    /// Retrying after any durable partial write is safe: `start_for_workspace`
    /// reuses the run and an exact existing root provenance is returned as-is.
    ///
    /// # Errors
    ///
    /// Returns an error when the worker is not rooted in the requested
    /// workspace, the dispatch identity is absent, or durable state cannot be
    /// initialized and bound consistently.
    #[allow(clippy::too_many_arguments)]
    pub fn start_for_workspace_root_dispatch(
        &self,
        caller: &str,
        workspace: WorkspaceId,
        operation_id: &str,
        root_task: String,
        policy_selector: Option<String>,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.reserve_goal_for_workspace(
            caller,
            workspace,
            operation_id,
            root_task,
            policy_selector,
            now,
        )?;
        self.bind_reserved_workspace_root_dispatch(operation_id, worker, now)
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
        if self.dispatch.run(dispatch_run_id)?.is_none() {
            anyhow::bail!("supervisor root dispatch does not exist");
        }
        let state = self.load_state()?;
        let reservation = state
            .starts
            .get(operation_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor root reservation does not exist"))?;
        let mut run = self.load_started_run(reservation.supervisor_run_id)?;
        if run.workspace_id != Some(worker.terminal.workspace_id) || worker.session_id.is_some() {
            anyhow::bail!("supervisor root worker is outside the workspace root scope");
        }
        let root_id = TaskId::new("root")?;
        let root = run
            .tasks
            .get(&root_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor root task is missing"))?;
        if root.required_artifact_contract != GOAL_REVIEW_READY_ARTIFACT_CONTRACT {
            anyhow::bail!("supervisor root reservation is not a Goal run");
        }
        let root_generation = root.generation;
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
        if run.state == SupervisorRunState::Escalated
            && let Some(escalation) = run.escalation.as_ref()
            && escalation.blocking_task_id.as_ref() == Some(&root_id)
            && escalation.reason == "no worker dispatch reservation was produced for a ready task"
        {
            let escalation_id = escalation.escalation_id;
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::Admission,
                SupervisorEventKind::ResolveEscalation {
                    escalation_id,
                    decision: EscalationDecision::Resume,
                },
            )?;
        }
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

    fn load_started_run(&self, id: SupervisorRunId) -> Result<SupervisorRun> {
        self.supervisor
            .load(id)?
            .ok_or_else(|| anyhow::anyhow!("supervisor run disappeared during root binding"))
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
            if run.workspace_id.is_none() {
                continue;
            }
            let root = TaskId::new("root")?;
            if run.tasks.get(&root).is_some_and(|task| {
                task.required_artifact_contract == GOAL_REVIEW_READY_ARTIFACT_CONTRACT
            }) && !run.provenance.contains_key(&root)
                && !run.state.is_finished()
            {
                pending.push(PendingGoalPromotion { operation_id });
            }
        }
        Ok(pending)
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
        let run = self.load_started_run(reservation.supervisor_run_id)?;
        let root = run
            .tasks
            .get(&TaskId::new("root")?)
            .ok_or_else(|| anyhow::anyhow!("supervisor root task is missing"))?;
        if root.required_artifact_contract != GOAL_REVIEW_READY_ARTIFACT_CONTRACT {
            anyhow::bail!("supervisor reservation is not a Goal run");
        }
        if run.state.is_finished() {
            return Ok(run.query());
        }
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

    /// Persists a delegated task before its Agent spawn. `None` means the
    /// parent dispatch is not supervised and classic delegation is unchanged.
    ///
    /// # Errors
    /// Returns an error for a conflicting child operation or durable reducer
    /// failure.
    pub fn reserve_delegated_dispatch(
        &self,
        parent_dispatch_run: OperationId,
        child_operation_id: &str,
        instruction: String,
        now: DateTime<Utc>,
    ) -> Result<Option<SupervisorRunQuery>> {
        bounded_nonempty(
            "delegated supervisor instruction",
            &instruction,
            MAX_SUPERVISOR_TEXT_BYTES,
        )?;
        let child_dispatch_run = OperationId::parse(child_operation_id)
            .map_err(|_| anyhow::anyhow!("delegated dispatch operation is invalid"))?;
        let mut matches = self.supervisor.runs()?.into_iter().filter_map(|run| {
            let task = run
                .provenance
                .iter()
                .find(|(_, provenance)| provenance.dispatch_run_id == parent_dispatch_run)
                .map(|(task, _)| task.clone());
            task.map(|task| (run, task))
        });
        let Some((mut run, parent_task_id)) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            anyhow::bail!("parent dispatch belongs to multiple supervisor runs");
        }
        let task_id = delegated_task_id(child_dispatch_run)?;
        if let Some(existing) = run.tasks.get(&task_id) {
            if !is_delegated_reservation(existing, child_dispatch_run)
                || existing.parent_task_id.as_ref() != Some(&parent_task_id)
                || existing.instruction_body != instruction
                || existing.required_artifact_contract != NO_ARTIFACT_CONTRACT
            {
                anyhow::bail!("delegated task conflicts with its existing supervisor task");
            }
            return Ok(Some(run.query()));
        }
        let mut task = task_node(
            &run,
            task_id,
            Some(parent_task_id),
            BTreeSet::new(),
            instruction,
            NO_ARTIFACT_CONTRACT,
        );
        task.instruction_digest = delegated_task_digest(child_dispatch_run);
        run = self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::AddTask { task },
        )?;
        Ok(Some(run.query()))
    }

    /// Pending delegated task reservations recoverable from their stable task
    /// IDs and daemon-only origin marker.
    ///
    /// # Errors
    /// Returns an error when retained supervisor state is malformed.
    pub fn pending_delegated_promotions(&self) -> Result<Vec<PendingDelegatedPromotion>> {
        let mut pending = Vec::new();
        for run in self.supervisor.runs()? {
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
                });
            }
        }
        Ok(pending)
    }

    /// Lists completed contracted dispatches whose verification can be safely
    /// replayed after a daemon restart. The dispatch ID comes from persisted
    /// provenance; worker output is never used to select the task.
    ///
    /// # Errors
    /// Returns an error when retained supervisor or dispatch state is invalid.
    pub fn pending_artifact_verifications(&self) -> Result<Vec<PendingArtifactVerification>> {
        let mut pending = Vec::new();
        for run in self.supervisor.runs()? {
            if run.state != SupervisorRunState::Running {
                continue;
            }
            for task in run.tasks.values().filter(|task| {
                task.required_artifact_contract == GOAL_REVIEW_READY_ARTIFACT_CONTRACT
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
                if self
                    .dispatch
                    .run(provenance.dispatch_run_id)?
                    .is_some_and(|dispatch| dispatch.status == RunStatus::Completed)
                {
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
        let mut matches = self.supervisor.runs()?.into_iter().filter_map(|run| {
            let task = run.tasks.get(&task_id)?.clone();
            is_delegated_reservation(&task, operation).then_some((run, task))
        });
        let Some((run, task)) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            anyhow::bail!("delegated dispatch belongs to multiple supervisor runs");
        }
        if task.state.terminal() {
            return Ok(Some(run.query()));
        }
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

    /// Adds and binds a child dispatch beneath the exact supervised parent
    /// dispatch. A caller outside a Supervisor Run is a no-op, preserving
    /// classic delegation semantics.
    ///
    /// # Errors
    /// Returns an error for conflicting replay, cross-workspace provenance, or
    /// durable reducer failure.
    pub fn attach_delegated_dispatch(
        &self,
        parent_dispatch_run: OperationId,
        child_operation_id: &str,
        instruction: String,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<Option<SupervisorRunQuery>> {
        if self
            .reserve_delegated_dispatch(parent_dispatch_run, child_operation_id, instruction, now)?
            .is_none()
        {
            return Ok(None);
        }
        self.bind_reserved_delegated_dispatch(child_operation_id, worker, now)
    }

    /// Binds one admitted child Agent using only its exact durable reservation.
    /// Parent provenance and instruction remain single-sourced in the DAG.
    ///
    /// # Errors
    /// Returns an error for missing dispatch state, conflicting provenance,
    /// cross-workspace ownership, or reducer persistence failure.
    pub fn bind_reserved_delegated_dispatch(
        &self,
        child_operation_id: &str,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<Option<SupervisorRunQuery>> {
        let child_dispatch_run = OperationId::parse(child_operation_id)
            .map_err(|_| anyhow::anyhow!("delegated dispatch operation is invalid"))?;
        if self.dispatch.run(child_dispatch_run)?.is_none() {
            anyhow::bail!("delegated dispatch does not exist");
        }
        let task_id = delegated_task_id(child_dispatch_run)?;
        let mut matches = self.supervisor.runs()?.into_iter().filter_map(|run| {
            let task = run.tasks.get(&task_id)?.clone();
            is_delegated_reservation(&task, child_dispatch_run).then_some((run, task))
        });
        let Some((mut run, task)) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            anyhow::bail!("delegated dispatch belongs to multiple supervisor runs");
        }
        if run.workspace_id != Some(worker.terminal.workspace_id) || worker.session_id.is_none() {
            anyhow::bail!("delegated worker is outside the supervisor workspace");
        }
        let parent_task_id = task
            .parent_task_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("delegated supervisor task has no parent"))?;
        let parent_dispatch_run = run
            .provenance
            .get(&parent_task_id)
            .ok_or_else(|| anyhow::anyhow!("delegated parent provenance is missing"))?
            .dispatch_run_id;
        if run.state == SupervisorRunState::Escalated
            && let Some(escalation) = run.escalation.as_ref()
            && escalation.blocking_task_id.as_ref() == Some(&task_id)
            && escalation.reason == "no worker dispatch reservation was produced for a ready task"
        {
            let escalation_id = escalation.escalation_id;
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::Admission,
                SupervisorEventKind::ResolveEscalation {
                    escalation_id,
                    decision: EscalationDecision::Resume,
                },
            )?;
        }
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
        let mut found = self.supervisor.runs()?.into_iter().filter_map(|run| {
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
        let contract = task.required_artifact_contract;
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
        if matches!(task_state, TaskState::Dispatched | TaskState::Running) {
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
        let result = match self.dispatch.binding(dispatch_run_id)? {
            Some(binding) => self
                .dispatch
                .inbox(&binding.caller)?
                .into_iter()
                .find(|message| message.run_id == dispatch_run_id)
                .and_then(|message| message.result),
            None => None,
        };
        Ok(Some(ArtifactVerificationRequest {
            supervisor_run_id: run.supervisor_run_id,
            task_id,
            generation,
            contract,
            result,
        }))
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
            || task.state != TaskState::Verifying
        {
            anyhow::bail!("artifact verification fence is stale");
        }
        let run = self.apply(
            &run,
            now,
            SupervisorEventSource::Verification,
            SupervisorEventKind::VerificationResult {
                task_id: request.task_id.clone(),
                generation: request.generation,
                passed: verification.passed,
                result_digest: verification.result_digest,
                safe_summary: verification.safe_summary,
            },
        )?;
        Ok(self.finalize_terminal_tasks(run, now)?.query())
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn start_scoped(
        &self,
        caller: &str,
        workspace: Option<WorkspaceId>,
        operation_id: &str,
        root_task: String,
        root_artifact_contract: ArtifactContract,
        initial_tasks: Vec<InitialTask>,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        validate_start_input(
            operation_id,
            &root_task,
            &initial_tasks,
            policy_selector.as_deref(),
        )?;
        let mut semantic_key = String::new();
        push_semantic_component(&mut semantic_key, caller);
        push_semantic_component(&mut semantic_key, &root_task);
        push_semantic_component(&mut semantic_key, root_artifact_contract.as_str());
        push_semantic_component(&mut semantic_key, &initial_tasks.len().to_string());
        for task in &initial_tasks {
            push_semantic_component(&mut semantic_key, &task.task_id);
            push_semantic_component(
                &mut semantic_key,
                task.parent_task_id.as_deref().unwrap_or("root"),
            );
            push_semantic_component(&mut semantic_key, &task.dependencies.len().to_string());
            for dependency in &task.dependencies {
                push_semantic_component(&mut semantic_key, dependency);
            }
            push_semantic_component(&mut semantic_key, &task.instruction);
            push_semantic_component(&mut semantic_key, task.required_artifact_contract.as_str());
        }
        push_semantic_component(
            &mut semantic_key,
            policy_selector.as_deref().unwrap_or("default"),
        );
        let mut state = self.load_state()?;
        let reservation = match state.starts.get(operation_id) {
            Some(existing) if existing.semantic_key == semantic_key => existing.clone(),
            Some(_) => anyhow::bail!("operation id was reused with a different supervisor start"),
            None => {
                if state.expired_starts.contains(operation_id) {
                    anyhow::bail!("supervisor start idempotency window expired");
                }
                self.ensure_start_capacity(&mut state)?;
                let reservation = StartReservation {
                    semantic_key,
                    supervisor_run_id: SupervisorRunId::new(),
                };
                state
                    .starts
                    .insert(operation_id.to_owned(), reservation.clone());
                self.save_state(&state)?;
                reservation
            }
        };
        let policy_revision = policy_selector.unwrap_or_else(|| "default".into());
        let mut run = if let Some(existing) = self.supervisor.load(reservation.supervisor_run_id)? {
            if existing.root_caller_ref != caller
                || existing.workspace_id != workspace
                || existing.policy_revision != policy_revision
            {
                anyhow::bail!("supervisor start reservation does not match its durable run");
            }
            if existing.state != SupervisorRunState::Planning {
                return Ok(existing.query());
            }
            existing
        } else {
            let mut run = SupervisorRun::new_with_id(
                reservation.supervisor_run_id,
                caller.to_owned(),
                operation_id.to_owned(),
                operation_id.to_owned(),
                policy_revision,
                now,
            );
            run.workspace_id = workspace;
            self.supervisor.initialize(&run)?;
            run
        };
        let root_id = TaskId::new("root")?;
        if let Some(root) = run.tasks.get(&root_id) {
            if root.parent_task_id.is_some()
                || !root.dependencies.is_empty()
                || root.instruction_body != root_task
                || root.required_artifact_contract != root_artifact_contract
            {
                anyhow::bail!("supervisor root task conflicts with its start reservation");
            }
        } else {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::Admission,
                SupervisorEventKind::AddTask {
                    task: task_node(
                        &run,
                        root_id,
                        None,
                        BTreeSet::new(),
                        root_task,
                        root_artifact_contract,
                    ),
                },
            )?;
        }
        let mut pending = initial_tasks;
        while !pending.is_empty() {
            let before = pending.len();
            let mut remaining = Vec::new();
            for task in pending {
                let dependencies = task
                    .dependencies
                    .iter()
                    .map(|value| TaskId::new(value.clone()))
                    .collect::<Result<BTreeSet<_>, _>>()?;
                let parent =
                    TaskId::new(task.parent_task_id.clone().unwrap_or_else(|| "root".into()))?;
                let task_id = TaskId::new(task.task_id.clone())?;
                if let Some(existing) = run.tasks.get(&task_id) {
                    if existing.parent_task_id.as_ref() != Some(&parent)
                        || existing.dependencies != dependencies
                        || existing.instruction_body != task.instruction
                        || existing.required_artifact_contract != task.required_artifact_contract
                    {
                        anyhow::bail!(
                            "supervisor initial task conflicts with its start reservation"
                        );
                    }
                } else if dependencies.iter().all(|id| run.tasks.contains_key(id))
                    && run.tasks.contains_key(&parent)
                {
                    run = self.apply(
                        &run,
                        now,
                        SupervisorEventSource::Admission,
                        SupervisorEventKind::AddTask {
                            task: task_node(
                                &run,
                                task_id,
                                Some(parent),
                                dependencies,
                                task.instruction,
                                task.required_artifact_contract,
                            ),
                        },
                    )?;
                } else {
                    remaining.push(task);
                }
            }
            if remaining.len() == before {
                anyhow::bail!("initial task DAG has a missing dependency or cycle");
            }
            pending = remaining;
        }
        run = self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::SetRunState {
                state: SupervisorRunState::Running,
                terminal_reason: None,
            },
        )?;
        Ok(run.query())
    }

    /// Lists the bounded retained runs that explicitly belong to one
    /// workspace. Legacy unscoped snapshots are excluded fail-closed.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable supervisor index or a selected run
    /// snapshot cannot be read consistently.
    pub fn list_workspace(&self, workspace: WorkspaceId) -> Result<Vec<SupervisorRunQuery>> {
        const MAX_TUI_WORK_RUNS: usize = 16;
        self.supervisor.workspace_runs(workspace, MAX_TUI_WORK_RUNS)
    }

    /// Reads one caller-owned durable run.
    ///
    /// # Errors
    /// Returns an error when durable state cannot be read.
    pub fn get(&self, caller: &str, id: SupervisorRunId) -> Result<Option<SupervisorRunQuery>> {
        match self.owned_run(caller, id)? {
            Some(run) => Ok(Some(run.query())),
            None => Ok(None),
        }
    }

    /// Lists caller-owned durable runs.
    ///
    /// # Errors
    /// Returns an error when durable state cannot be listed or replayed.
    pub fn list(
        &self,
        caller: &str,
        state: Option<SupervisorRunState>,
    ) -> Result<Vec<SupervisorRunQuery>> {
        Ok(self
            .supervisor
            .runs()?
            .into_iter()
            .filter(|run| {
                run.root_caller_ref == caller && state.is_none_or(|value| run.state == value)
            })
            .map(|run| run.query())
            .collect())
    }

    /// Lists one bounded caller-owned page using the durable run index.
    ///
    /// # Errors
    /// Returns an error when the cursor, durable state, or response budget is invalid.
    pub fn list_page(
        &self,
        caller: &str,
        state: Option<SupervisorRunState>,
        cursor: usize,
        limit: usize,
    ) -> Result<SupervisorRunPage> {
        self.supervisor.runs_page(caller, state, cursor, limit)
    }

    /// Commits a fenced cancellation.
    ///
    /// # Errors
    /// Returns an error for an unknown owner, invalid transition, or durable IO failure.
    pub fn cancel(
        &self,
        caller: &str,
        id: SupervisorRunId,
        reason: String,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        bounded_safe_label(
            "supervisor cancellation reason",
            &reason,
            MAX_SUPERVISOR_REASON_BYTES,
        )?;
        let run = self
            .owned_run(caller, id)?
            .ok_or_else(|| anyhow::anyhow!("supervisor run does not exist for this caller"))?;
        self.apply(
            &run,
            now,
            SupervisorEventSource::Cancel,
            SupervisorEventKind::Cancel {
                task_id: None,
                reason,
            },
        )
        .map(|run| run.query())
    }

    /// Commits an authorized escalation decision.
    ///
    /// # Errors
    /// Returns an error for an invalid owner/fence/transition or durable IO failure.
    pub fn resolve_escalation(
        &self,
        caller: &str,
        id: SupervisorRunId,
        escalation_id: OperationId,
        decision: EscalationDecision,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        let run = self
            .owned_run(caller, id)?
            .ok_or_else(|| anyhow::anyhow!("supervisor run does not exist for this caller"))?;
        self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::ResolveEscalation {
                escalation_id,
                decision,
            },
        )
        .map(|run| run.query())
    }

    /// Returns redaction-safe event metadata for one caller-owned run.
    ///
    /// # Errors
    /// Returns an error for an unknown owner or durable IO failure.
    pub fn events(
        &self,
        caller: &str,
        id: SupervisorRunId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<(Vec<EventQuery>, EventCursor)> {
        self.owned_run(caller, id)?
            .ok_or_else(|| anyhow::anyhow!("supervisor run does not exist for this caller"))?;
        self.supervisor.events(
            id,
            EventCursor {
                next_sequence: after_sequence.saturating_add(1),
            },
            limit,
        )
    }

    /// Reconciles every durable run after startup or a completion wake.
    ///
    /// # Errors
    /// Returns the first durable reconciliation or wake delivery failure.
    pub fn tick_all(&self, now: DateTime<Utc>, waker: &mut dyn DecisionWaker) -> Result<()> {
        for run in self.supervisor.runs()? {
            self.tick(run.supervisor_run_id, now, waker)?;
        }
        Ok(())
    }

    fn owned_run(&self, caller: &str, id: SupervisorRunId) -> Result<Option<SupervisorRun>> {
        Ok(self
            .supervisor
            .load(id)?
            .filter(|run| run.root_caller_ref == caller))
    }

    /// Reconciles one run and delivers each durably reserved wake at least once.
    /// A repeat/restart is safe because reducer event IDs and wake reservation
    /// keys are stable (`child dispatch run` + `parent decision generation`).
    ///
    /// # Errors
    ///
    /// Returns an error when durable state cannot be read or committed, or the
    /// waker cannot deliver a reserved request.
    ///
    /// # Panics
    ///
    /// Panics only if an already-corrupt supervisor snapshot contains
    /// provenance for a missing task or parent.
    pub fn tick(
        &self,
        id: SupervisorRunId,
        now: DateTime<Utc>,
        waker: &mut dyn DecisionWaker,
    ) -> Result<()> {
        let Some(mut run) = self.supervisor.load(id)? else {
            return Ok(());
        };
        let dispatch_runs = self.dispatch_runs()?;
        // Retry eligibility is a persisted deadline, not an in-memory timer.
        // Reconciliation therefore cannot dispatch a retry before its deadline
        // and can resume one after a daemon restart without polling.
        let mut due_retries = Vec::new();
        for (id, task) in &run.tasks {
            if task.state == TaskState::Retrying
                && matches!(task.retry_at, Some(retry_at) if retry_at <= now)
            {
                due_retries.push((id.clone(), task.generation));
            }
        }
        for (task_id, generation) in due_retries {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::Timer,
                SupervisorEventKind::RetryReady {
                    task_id,
                    generation,
                },
            )?;
        }
        for (task_id, provenance) in run.provenance.clone() {
            let Some(dispatch_run) = dispatch_runs
                .iter()
                .find(|run| run.run_id == provenance.dispatch_run_id)
            else {
                continue;
            };
            let Some((terminal, kind)) = terminal(dispatch_run.status) else {
                continue;
            };
            let task = run
                .tasks
                .get(&task_id)
                .cloned()
                .expect("provenance task exists");
            if task.state == TaskState::Dispatched {
                let event = SupervisorEventKind::Running {
                    task_id: task_id.clone(),
                    generation: task.generation,
                };
                run = self.apply(&run, now, SupervisorEventSource::DispatchCompletion, event)?;
            }
            let current = run.tasks.get(&task_id).expect("task retained");
            if matches!(current.state, TaskState::Dispatched | TaskState::Running) {
                let event = SupervisorEventKind::SetTaskState {
                    task_id: task_id.clone(),
                    generation: current.generation,
                    state: terminal,
                };
                run = self.apply(&run, now, source(kind), event)?;
            } else if !current.state.terminal() {
                continue;
            }
            if let Some(parent_id) = task.parent_task_id {
                let child_run = provenance.dispatch_run_id;
                self.reserve_parent_wake(&mut run, &parent_id, child_run, kind, now)?;
            }
        }
        run = self.finalize_terminal_tasks(run, now)?;
        // A ready task without a dispatch reservation is not progress. The
        // previous scheduler left such runs in `running` forever when selector
        // resolution or dispatch admission produced no worker. Persist a typed
        // escalation so callers can distinguish an actionable stop from a live
        // scheduler and inspect the exact blocking task.
        if run.state == SupervisorRunState::Running
            && let Some((task_id, _)) = run.tasks.iter().find(|(_, task)| {
                task.state == TaskState::Ready && task.assigned_dispatch_run.is_none()
            })
        {
            let _ = self.apply(
                &run,
                now,
                SupervisorEventSource::DispatchFailure,
                SupervisorEventKind::Escalate {
                    task_id: Some(task_id.clone()),
                    reason: "no worker dispatch reservation was produced for a ready task".into(),
                    safe_evidence:
                        "runtime/model selection or dispatch admission did not assign a run".into(),
                    choices: vec!["resume".into(), "cancel".into()],
                },
            )?;
        }
        self.deliver_reserved(waker)
    }

    fn finalize_terminal_tasks(
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

    fn dispatch_runs(&self) -> Result<Vec<usagi_core::domain::agent::DispatchRun>> {
        #[cfg(test)]
        self.dispatch_registry_reads
            .set(self.dispatch_registry_reads.get() + 1);
        self.dispatch.runs()
    }
    fn apply(
        &self,
        run: &usagi_core::domain::supervisor::SupervisorRun,
        now: DateTime<Utc>,
        source: SupervisorEventSource,
        kind: SupervisorEventKind,
    ) -> Result<usagi_core::domain::supervisor::SupervisorRun> {
        let call = self.apply_calls.get();
        self.apply_calls.set(call + 1);
        if self.apply_fail_at.get() == Some(call) {
            anyhow::bail!("injected supervisor apply failure");
        }
        let event = SupervisorEvent {
            sequence: run.state_revision + 1,
            event_id: OperationId::new(),
            causation_id: None,
            correlation_id: None,
            observed_at: now,
            payload_digest: "scheduler".into(),
            source,
            kind,
        };
        self.supervisor
            .apply(run.supervisor_run_id, run.state_revision, &event)
    }
    fn reserve_parent_wake(
        &self,
        run: &mut usagi_core::domain::supervisor::SupervisorRun,
        parent_id: &TaskId,
        child_run: OperationId,
        kind: InboxKind,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let parent = run.tasks.get(parent_id).cloned().expect("parent exists");
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
        let key = format!("{}:{}:{}", child_run, parent_id.0, parent.generation);
        let mut state = self.load_state()?;
        if state.wakes.contains_key(&key) || state.expired_wakes.contains(&key) {
            return Ok(());
        }
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
    fn outcome(&self, child: OperationId, fallback: InboxKind) -> Result<WakeOutcome> {
        let message = self.dispatch.binding(child)?.and_then(|binding| {
            self.dispatch
                .inbox(&binding.caller)
                .ok()
                .and_then(|messages| messages.into_iter().find(|message| message.run_id == child))
        });
        Ok(message.map_or(
            WakeOutcome {
                kind: fallback,
                summary: "worker terminal state committed without an inbox report".into(),
            },
            |message| WakeOutcome {
                kind: message.kind,
                summary: message.summary,
            },
        ))
    }
    fn deliver_reserved(&self, waker: &mut dyn DecisionWaker) -> Result<()> {
        let mut state = self.load_state()?;
        let mut changed = false;
        for reservation in state.wakes.values_mut().filter(|item| !item.delivered) {
            waker.wake(&reservation.wake)?;
            reservation.delivered = true;
            changed = true;
        }
        if changed {
            state.compact_delivered_wakes();
            self.save_state(&state)?;
        }
        Ok(())
    }

    fn ensure_start_capacity(&self, state: &mut RuntimeState) -> Result<()> {
        if state.starts.len() < MAX_START_RESERVATIONS {
            return Ok(());
        }
        let mut recyclable = Vec::new();
        for (key, reservation) in &state.starts {
            match self.supervisor.load(reservation.supervisor_run_id)? {
                None => recyclable.push((None, key.clone())),
                Some(run) if run.state.is_finished() => {
                    recyclable.push((run.terminal_at.or(Some(run.updated_at)), key.clone()));
                }
                Some(_) => {}
            }
        }
        recyclable.sort_by_key(|(terminal_at, key)| (*terminal_at, key.clone()));
        for (_, key) in recyclable {
            if state.starts.len() < MAX_START_RESERVATIONS {
                break;
            }
            state.starts.remove(&key);
            state.expired_starts.insert(&key);
        }
        if state.starts.len() >= MAX_START_RESERVATIONS {
            anyhow::bail!("supervisor start reservation capacity is exhausted");
        }
        Ok(())
    }

    fn load_state(&self) -> Result<RuntimeState> {
        let state: RuntimeState = json_file::read(&self.state_path)?.unwrap_or_default();
        state.validate_limits()?;
        Ok(state)
    }
    fn save_state(&self, state: &RuntimeState) -> Result<()> {
        state.validate_limits()?;
        json_file::write_atomic(
            self.state_path.parent().expect("state path has parent"),
            &self.state_path,
            state,
        )
    }
}

fn task_node(
    run: &SupervisorRun,
    task_id: TaskId,
    parent_task_id: Option<TaskId>,
    dependencies: BTreeSet<TaskId>,
    instruction: String,
    required_artifact_contract: ArtifactContract,
) -> TaskNode {
    TaskNode {
        instruction_digest: format!("task:{}", task_id.0),
        task_id,
        supervisor_run_id: run.supervisor_run_id,
        parent_task_id,
        dependencies,
        instruction_body: instruction,
        required_artifact_contract,
        attempt: 1,
        generation: 1,
        assigned_dispatch_run: None,
        retry_at: None,
        verification_digest: None,
        state: TaskState::Pending,
    }
}

fn terminal(status: RunStatus) -> Option<(TaskState, InboxKind)> {
    match status {
        RunStatus::Preparing | RunStatus::Running => None,
        RunStatus::Completed => Some((TaskState::Succeeded, InboxKind::Completed)),
        RunStatus::Failed => Some((TaskState::Failed, InboxKind::Failed)),
        RunStatus::NoReport => Some((TaskState::Failed, InboxKind::NoReport)),
    }
}
fn source(kind: InboxKind) -> SupervisorEventSource {
    match kind {
        InboxKind::Completed => SupervisorEventSource::DispatchCompletion,
        InboxKind::Failed => SupervisorEventSource::DispatchFailure,
        InboxKind::NoReport => SupervisorEventSource::NoReport,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::{BTreeMap, BTreeSet};
    use usagi_core::domain::{
        agent::{CallerRef, DispatchBinding, DispatchRun, InboxMessage, WorkerRef},
        id::{
            AgentId, AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, SessionId, TerminalId,
            TerminalRef, WorktreeId,
        },
        supervisor::{SupervisorRun, TaskNode},
    };

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap()
    }
    fn root_worker(workspace: WorkspaceId) -> AgentRuntimeRef {
        AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: workspace,
                session_id: None,
                worktree_id: WorktreeId::new(),
            },
            None,
        )
        .unwrap()
    }
    fn delegated_worker(workspace: WorkspaceId) -> AgentRuntimeRef {
        let session = SessionId::new();
        AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: workspace,
                session_id: Some(session),
                worktree_id: WorktreeId::new(),
            },
            Some(session),
        )
        .unwrap()
    }
    fn task(run: SupervisorRunId, id: &str, parent: Option<&str>) -> TaskNode {
        TaskNode {
            task_id: TaskId::new(id).unwrap(),
            supervisor_run_id: run,
            parent_task_id: parent.map(|id| TaskId::new(id).unwrap()),
            dependencies: BTreeSet::new(),
            instruction_digest: id.into(),
            instruction_body: id.into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
            attempt: 1,
            generation: 1,
            assigned_dispatch_run: None,
            retry_at: None,
            verification_digest: None,
            state: TaskState::Pending,
        }
    }
    fn event(run: &SupervisorRun, kind: SupervisorEventKind) -> SupervisorEvent {
        SupervisorEvent {
            sequence: run.state_revision + 1,
            event_id: OperationId::new(),
            causation_id: None,
            correlation_id: None,
            observed_at: now(),
            payload_digest: "test".into(),
            source: SupervisorEventSource::Admission,
            kind,
        }
    }
    fn provenance(
        run: SupervisorRunId,
        task: &TaskId,
        parent: Option<(&TaskId, OperationId)>,
        dispatch: OperationId,
    ) -> RunProvenance {
        RunProvenance {
            supervisor_run_id: run,
            task_id: task.clone(),
            parent_task_id: parent.as_ref().map(|(id, _)| (*id).clone()),
            parent_dispatch_run: parent.map(|(_, id)| id),
            dispatch_run_id: dispatch,
            worker_session_id: Some(SessionId::new()),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        }
    }
    #[derive(Default)]
    struct Waker {
        wakes: Vec<DecisionWake>,
    }
    impl DecisionWaker for Waker {
        fn wake(&mut self, wake: &DecisionWake) -> Result<()> {
            self.wakes.push(wake.clone());
            Ok(())
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One lifecycle fixture covers verification, escalation, resume, and late-result fencing.
    fn workspace_root_dispatch_is_bound_idempotently_and_completes_the_run() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        let worker = root_worker(workspace);
        let unbound = scheduler
            .reserve_goal_for_workspace(
                "goal-composer",
                workspace,
                &operation.to_string(),
                "finish the requested work".into(),
                Some("standard".into()),
                now(),
            )
            .unwrap();
        let mut waker = Waker::default();
        scheduler
            .tick(unbound.supervisor_run_id, now(), &mut waker)
            .unwrap();
        assert_eq!(
            scheduler
                .get("goal-composer", unbound.supervisor_run_id)
                .unwrap()
                .unwrap()
                .state,
            SupervisorRunState::Escalated
        );
        let first = scheduler
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &operation.to_string(),
                "finish the requested work".into(),
                Some("standard".into()),
                &worker,
                now(),
            )
            .unwrap();
        let replay = scheduler
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &operation.to_string(),
                "finish the requested work".into(),
                Some("standard".into()),
                &worker,
                now(),
            )
            .unwrap();
        assert_eq!(replay, first);
        assert_eq!(first.tasks[0].state, TaskState::Dispatched);
        assert_eq!(first.tasks[0].assigned_dispatch_run, Some(operation));
        assert_eq!(first.provenance[0].worker_session_id, None);

        scheduler
            .tick(first.supervisor_run_id, now(), &mut waker)
            .unwrap();
        let active = scheduler
            .get("goal-composer", first.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(active.state, SupervisorRunState::Running);
        assert!(active.escalation.is_none());

        dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        scheduler
            .tick(first.supervisor_run_id, now(), &mut waker)
            .unwrap();
        let completed = scheduler
            .get("goal-composer", first.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(completed.tasks[0].state, TaskState::Verifying);
        assert_eq!(completed.state, SupervisorRunState::Running);
        let request = scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .unwrap();
        for invalid in [
            ArtifactVerification {
                passed: true,
                result_digest: String::new(),
                safe_summary: "verified".into(),
            },
            ArtifactVerification {
                passed: true,
                result_digest: "verified".into(),
                safe_summary: "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
            },
        ] {
            assert!(
                scheduler
                    .record_artifact_verification(&request, invalid, now())
                    .is_err()
            );
        }
        assert_eq!(
            scheduler
                .get("goal-composer", first.supervisor_run_id)
                .unwrap()
                .unwrap()
                .tasks[0]
                .state,
            TaskState::Verifying
        );
        let rejected = scheduler
            .record_artifact_verification(
                &request,
                ArtifactVerification {
                    passed: false,
                    result_digest: "provider-unavailable".into(),
                    safe_summary: "pull request verification provider is unavailable".into(),
                },
                now(),
            )
            .unwrap();
        assert_eq!(rejected.state, SupervisorRunState::Escalated);
        assert_eq!(
            rejected.escalation.as_ref().unwrap().safe_evidence,
            "pull request verification provider is unavailable"
        );
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap()
                .is_none()
        );
        scheduler
            .resolve_escalation(
                "goal-composer",
                first.supervisor_run_id,
                rejected.escalation.unwrap().escalation_id,
                EscalationDecision::Resume,
                now(),
            )
            .unwrap();
        let retry = scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .unwrap();
        let completed = scheduler
            .record_artifact_verification(
                &retry,
                ArtifactVerification {
                    passed: true,
                    result_digest: "verified".into(),
                    safe_summary: "verified".into(),
                },
                now(),
            )
            .unwrap();
        assert_eq!(completed.tasks[0].state, TaskState::Succeeded);
        assert_eq!(completed.state, SupervisorRunState::Succeeded);
        assert!(
            scheduler
                .pending_artifact_verifications()
                .unwrap()
                .is_empty()
        );
        let late = scheduler
            .record_artifact_verification(
                &retry,
                ArtifactVerification {
                    passed: false,
                    result_digest: "late-provider-result".into(),
                    safe_summary: "late provider result".into(),
                },
                now(),
            )
            .unwrap();
        assert_eq!(late, completed);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One artifact fence fixture covers candidate capture and every stale request dimension.
    fn artifact_verification_preparation_captures_only_the_exact_completed_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: "goal".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        let caller = CallerRef {
            session_id: None,
            agent_id: AgentId::new(),
        };
        let structured = StructuredResult {
            pr: Some("https://github.com/acme/repo/pull/1".into()),
            commits: vec!["abc".into()],
            changed_files: vec!["src/lib.rs".into()],
            verification: Some("candidate only".into()),
        };
        scheduler
            .dispatch
            .upsert_binding(DispatchBinding {
                run_id: operation,
                caller: caller.clone(),
                worker: WorkerRef {
                    session_id: None,
                    agent_id: AgentId::new(),
                },
            })
            .unwrap();
        scheduler
            .dispatch
            .append_inbox(
                &caller,
                InboxMessage {
                    run_id: operation,
                    from: WorkerRef {
                        session_id: None,
                        agent_id: AgentId::new(),
                    },
                    kind: InboxKind::Completed,
                    summary: "worker says complete".into(),
                    result: Some(structured.clone()),
                    created_at: now(),
                    read: false,
                },
            )
            .unwrap();
        let run = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &operation.to_string(),
                "finish".into(),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        assert_eq!(run.tasks[0].state, TaskState::Dispatched);
        let second_operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: second_operation,
                agent_id: AgentId::new(),
                prompt: "another goal".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        scheduler
            .start_for_workspace_root_dispatch(
                "another-goal",
                workspace,
                &second_operation.to_string(),
                "another finish".into(),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let mut expected_pending = vec![operation, second_operation];
        expected_pending.sort_by_key(ToString::to_string);
        assert_eq!(
            scheduler.pending_artifact_verifications().unwrap(),
            expected_pending
                .into_iter()
                .map(|dispatch_run_id| PendingArtifactVerification { dispatch_run_id })
                .collect::<Vec<_>>()
        );

        let request = scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .unwrap();
        assert_eq!(request.result, Some(structured));
        assert_eq!(request.task_id, TaskId::new("root").unwrap());
        assert_eq!(
            scheduler
                .get("goal", run.supervisor_run_id)
                .unwrap()
                .unwrap()
                .tasks[0]
                .state,
            TaskState::Verifying
        );
        assert!(
            scheduler
                .prepare_artifact_verification(OperationId::new(), now())
                .unwrap()
                .is_none()
        );

        let missing_task = ArtifactVerificationRequest {
            task_id: TaskId::new("missing").unwrap(),
            ..request.clone()
        };
        assert!(
            scheduler
                .record_artifact_verification(
                    &missing_task,
                    ArtifactVerification {
                        passed: true,
                        result_digest: "verified".into(),
                        safe_summary: "verified".into(),
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("task is missing")
        );
        for stale in [
            ArtifactVerificationRequest {
                generation: request.generation + 1,
                ..request.clone()
            },
            ArtifactVerificationRequest {
                contract: NO_ARTIFACT_CONTRACT,
                ..request.clone()
            },
        ] {
            assert!(
                scheduler
                    .record_artifact_verification(
                        &stale,
                        ArtifactVerification {
                            passed: true,
                            result_digest: "verified".into(),
                            safe_summary: "verified".into(),
                        },
                        now(),
                    )
                    .unwrap_err()
                    .to_string()
                    .contains("fence is stale")
            );
        }

        let mut unexpectedly_running = scheduler
            .supervisor
            .load(run.supervisor_run_id)
            .unwrap()
            .unwrap();
        unexpectedly_running.state = SupervisorRunState::Running;
        unexpectedly_running
            .tasks
            .get_mut(&request.task_id)
            .unwrap()
            .state = TaskState::Running;
        scheduler
            .supervisor
            .initialize(&unexpectedly_running)
            .unwrap();
        assert!(
            scheduler
                .record_artifact_verification(
                    &request,
                    ArtifactVerification {
                        passed: true,
                        result_digest: "verified".into(),
                        safe_summary: "verified".into(),
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("fence is stale")
        );

        let mut duplicate = unexpectedly_running;
        duplicate.supervisor_run_id = SupervisorRunId::new();
        for task in duplicate.tasks.values_mut() {
            task.supervisor_run_id = duplicate.supervisor_run_id;
        }
        for provenance in duplicate.provenance.values_mut() {
            provenance.supervisor_run_id = duplicate.supervisor_run_id;
        }
        scheduler.supervisor.initialize(&duplicate).unwrap();
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap_err()
                .to_string()
                .contains("multiple supervisor runs")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One corruption fixture keeps terminal status, contracts, and provenance fences visibly related.
    fn artifact_preparation_rejects_nonterminal_wrong_contract_and_corrupt_membership() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: "goal".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        let started = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &operation.to_string(),
                "finish".into(),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap()
                .is_none()
        );
        assert!(
            scheduler
                .pending_artifact_verifications()
                .unwrap()
                .is_empty()
        );

        let mut unsupported_state = scheduler
            .supervisor
            .load(started.supervisor_run_id)
            .unwrap()
            .unwrap();
        unsupported_state
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .state = TaskState::AwaitingDecision;
        scheduler.supervisor.initialize(&unsupported_state).unwrap();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: "goal".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap()
                .is_none()
        );
        assert!(
            scheduler
                .pending_artifact_verifications()
                .unwrap()
                .is_empty()
        );

        let mut wrong_contract = scheduler
            .supervisor
            .load(started.supervisor_run_id)
            .unwrap()
            .unwrap();
        wrong_contract
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .required_artifact_contract = NO_ARTIFACT_CONTRACT;
        scheduler.supervisor.initialize(&wrong_contract).unwrap();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: "goal".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap()
                .is_none()
        );

        let missing_dispatch = OperationId::new();
        let mut corrupt = wrong_contract;
        let corrupt_root = corrupt
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap();
        corrupt_root.required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
        corrupt_root.state = TaskState::Dispatched;
        corrupt
            .provenance
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .dispatch_run_id = missing_dispatch;
        scheduler.supervisor.initialize(&corrupt).unwrap();
        assert!(
            scheduler
                .pending_artifact_verifications()
                .unwrap_err()
                .to_string()
                .contains("provenance fence is stale")
        );
        assert!(
            scheduler
                .prepare_artifact_verification(missing_dispatch, now())
                .unwrap_err()
                .to_string()
                .contains("artifact dispatch is missing")
        );

        let mut missing_task = corrupt.clone();
        let provenance = missing_task
            .provenance
            .remove(&TaskId::new("root").unwrap())
            .unwrap();
        missing_task
            .provenance
            .insert(TaskId::new("missing").unwrap(), provenance);
        scheduler.supervisor.initialize(&missing_task).unwrap();
        assert!(
            scheduler
                .pending_artifact_verifications()
                .unwrap_err()
                .to_string()
                .contains("provenance is missing")
        );
        assert!(
            scheduler
                .prepare_artifact_verification(missing_dispatch, now())
                .unwrap_err()
                .to_string()
                .contains("supervisor task is missing")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One refusal matrix covers every fail-closed root provenance boundary.
    fn workspace_root_dispatch_refuses_invalid_missing_and_conflicting_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let worker = root_worker(workspace);
        assert!(
            scheduler
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    "not-an-operation",
                    "root".into(),
                    None,
                    &worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("operation is invalid")
        );
        let missing = OperationId::new();
        assert!(
            scheduler
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    &missing.to_string(),
                    "root".into(),
                    None,
                    &worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("dispatch does not exist")
        );
        let operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    &operation.to_string(),
                    String::new(),
                    None,
                    &worker,
                    now(),
                )
                .is_err()
        );
        assert!(
            scheduler
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    &operation.to_string(),
                    "root".into(),
                    None,
                    &root_worker(WorkspaceId::new()),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("outside the workspace root scope")
        );
        scheduler
            .start_for_workspace_root_dispatch(
                "caller",
                workspace,
                &operation.to_string(),
                "root".into(),
                None,
                &worker,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    &operation.to_string(),
                    "root".into(),
                    None,
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("provenance conflicts")
        );
        assert!(
            scheduler
                .load_started_run(SupervisorRunId::new())
                .unwrap_err()
                .to_string()
                .contains("disappeared")
        );

        let missing_root_temp = tempfile::tempdir().unwrap();
        let missing_root = SupervisorRuntime::new(missing_root_temp.path());
        let missing_root_dispatch = DispatchStore::new(missing_root_temp.path());
        let missing_root_operation = OperationId::new();
        missing_root_dispatch
            .upsert_run(DispatchRun {
                run_id: missing_root_operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        missing_root.fail_apply_at(0);
        assert!(
            missing_root
                .reserve_goal_for_workspace(
                    "caller",
                    workspace,
                    &missing_root_operation.to_string(),
                    "root".into(),
                    None,
                    now(),
                )
                .is_err()
        );
        let id = missing_root.load_state().unwrap().starts[&missing_root_operation.to_string()]
            .supervisor_run_id;
        let mut incomplete = missing_root.supervisor.load(id).unwrap().unwrap();
        incomplete.state = SupervisorRunState::Running;
        missing_root.supervisor.initialize(&incomplete).unwrap();
        assert!(
            missing_root
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    &missing_root_operation.to_string(),
                    "root".into(),
                    None,
                    &worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("root task is missing")
        );
    }

    #[test]
    fn workspace_root_dispatch_propagates_each_binding_write_failure() {
        for (escalate_before_binding, failed_apply) in [(false, 2), (true, 3), (true, 4)] {
            let temp = tempfile::tempdir().unwrap();
            let scheduler = SupervisorRuntime::new(temp.path());
            let dispatch = DispatchStore::new(temp.path());
            let workspace = WorkspaceId::new();
            let operation = OperationId::new();
            dispatch
                .upsert_run(DispatchRun {
                    run_id: operation,
                    agent_id: AgentId::new(),
                    prompt: String::new(),
                    started_at: now(),
                    ended_at: None,
                    status: RunStatus::Running,
                })
                .unwrap();
            let started = scheduler
                .reserve_goal_for_workspace(
                    "caller",
                    workspace,
                    &operation.to_string(),
                    "root".into(),
                    None,
                    now(),
                )
                .unwrap();
            if escalate_before_binding {
                scheduler
                    .tick(started.supervisor_run_id, now(), &mut Waker::default())
                    .unwrap();
            }
            scheduler.fail_apply_at(failed_apply);

            assert!(
                scheduler
                    .start_for_workspace_root_dispatch(
                        "caller",
                        workspace,
                        &operation.to_string(),
                        "root".into(),
                        None,
                        &root_worker(workspace),
                        now(),
                    )
                    .unwrap_err()
                    .to_string()
                    .contains("injected supervisor apply failure")
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One recovery inventory fixture covers every reservation class and terminal replay.
    fn pending_goal_inventory_and_definite_failure_are_exact() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();

        let unscoped_operation = OperationId::new().to_string();
        scheduler
            .start(
                "caller",
                &unscoped_operation,
                "unscoped".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        let classic_operation = OperationId::new();
        scheduler
            .start_for_workspace(
                "caller",
                workspace,
                &classic_operation.to_string(),
                "classic".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        dispatch
            .upsert_run(DispatchRun {
                run_id: classic_operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_workspace_root_dispatch(
                    &classic_operation.to_string(),
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("not a Goal run")
        );
        let classic_before = scheduler
            .get(
                "caller",
                scheduler.load_state().unwrap().starts[&classic_operation.to_string()]
                    .supervisor_run_id,
            )
            .unwrap()
            .unwrap();
        assert!(
            scheduler
                .fail_reserved_goal(
                    &classic_operation.to_string(),
                    "must not cross contracts".into(),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("not a Goal run")
        );
        assert_eq!(
            scheduler
                .get("caller", classic_before.supervisor_run_id)
                .unwrap()
                .unwrap(),
            classic_before
        );

        let goal_operation = OperationId::new();
        let goal = scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &goal_operation.to_string(),
                "goal".into(),
                None,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .pending_artifact_verifications()
                .unwrap()
                .is_empty()
        );
        let mut state = scheduler.load_state().unwrap();
        state.starts.insert(
            OperationId::new().to_string(),
            StartReservation {
                semantic_key: "orphan".into(),
                supervisor_run_id: SupervisorRunId::new(),
            },
        );
        scheduler.save_state(&state).unwrap();
        assert_eq!(
            scheduler.pending_goal_promotions().unwrap(),
            vec![PendingGoalPromotion {
                operation_id: goal_operation.to_string()
            }]
        );

        assert!(
            scheduler
                .fail_reserved_goal("missing", "failed".into(), now())
                .unwrap_err()
                .to_string()
                .contains("reservation does not exist")
        );
        let failed = scheduler
            .fail_reserved_goal(
                &goal_operation.to_string(),
                "definite failure".into(),
                now(),
            )
            .unwrap();
        assert_eq!(failed.state, SupervisorRunState::Failed);
        assert_eq!(failed.terminal_reason.as_deref(), Some("definite failure"));
        assert_eq!(
            scheduler
                .fail_reserved_goal(&goal_operation.to_string(), "ignored replay".into(), now())
                .unwrap(),
            failed
        );
        assert!(scheduler.pending_goal_promotions().unwrap().is_empty());
        assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
        assert_eq!(
            scheduler
                .get("goal", goal.supervisor_run_id)
                .unwrap()
                .unwrap(),
            failed
        );

        let reservation_without_dispatch = OperationId::new();
        scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &reservation_without_dispatch.to_string(),
                "goal".into(),
                None,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_workspace_root_dispatch(
                    &reservation_without_dispatch.to_string(),
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("dispatch does not exist")
        );
        let dispatch_without_reservation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: dispatch_without_reservation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_workspace_root_dispatch(
                    &dispatch_without_reservation.to_string(),
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("reservation does not exist")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One recovery fixture keeps reservation, collision, escalation, and exact replay assertions together.
    fn delegated_dispatch_is_reserved_before_spawn_and_reconciled_by_exact_operation() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &root_operation.to_string(),
                "root work".into(),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let before_oversized = scheduler
            .get("goal-composer", root.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &OperationId::new().to_string(),
                    "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
                    now(),
                )
                .is_err()
        );
        assert_eq!(
            scheduler
                .get("goal-composer", root.supervisor_run_id)
                .unwrap()
                .unwrap(),
            before_oversized
        );
        let child_operation = OperationId::new();
        let reserved = scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work".into(),
                now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(reserved.tasks.len(), 2);
        assert_eq!(reserved.provenance.len(), 1);
        assert_eq!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work".into(),
                    now(),
                )
                .unwrap()
                .unwrap(),
            reserved
        );
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "different child work".into(),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("conflicts")
        );
        let pending = scheduler.pending_delegated_promotions().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].operation_id, child_operation.to_string());

        scheduler
            .tick(root.supervisor_run_id, now(), &mut Waker::default())
            .unwrap();
        assert_eq!(
            scheduler
                .get("goal-composer", root.supervisor_run_id)
                .unwrap()
                .unwrap()
                .state,
            SupervisorRunState::Escalated
        );
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_operation,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .attach_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work".into(),
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("outside the supervisor workspace")
        );
        let worker = delegated_worker(workspace);
        let attached = scheduler
            .attach_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work".into(),
                &worker,
                now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(attached.state, SupervisorRunState::Running);
        assert!(attached.escalation.is_none());
        assert_eq!(attached.tasks.len(), 2);
        assert_eq!(attached.provenance.len(), 2);
        assert!(
            attached
                .provenance
                .iter()
                .any(|item| item.parent_dispatch_run == Some(root_operation))
        );
        assert_eq!(
            scheduler
                .attach_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work".into(),
                    &worker,
                    now(),
                )
                .unwrap()
                .unwrap(),
            attached
        );
        assert!(
            scheduler
                .attach_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work".into(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("provenance conflicts")
        );

        // A user-defined task whose ID happens to look similar is not a
        // daemon reservation because it does not carry the origin marker.
        let fake_operation = OperationId::new();
        let run = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        scheduler
            .apply(
                &run,
                now(),
                SupervisorEventSource::Admission,
                SupervisorEventKind::AddTask {
                    task: task_node(
                        &run,
                        delegated_task_id(fake_operation).unwrap(),
                        Some(TaskId::new("root").unwrap()),
                        BTreeSet::new(),
                        "ordinary task".into(),
                        NO_ARTIFACT_CONTRACT,
                    ),
                },
            )
            .unwrap();
        assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One fail-closed matrix exercises the durable child join boundary.
    fn delegated_dispatch_refuses_missing_malformed_and_ambiguous_reservations() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &root_operation.to_string(),
                "root".into(),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();

        assert!(
            scheduler
                .reserve_delegated_dispatch(root_operation, "invalid", "child".into(), now())
                .unwrap_err()
                .to_string()
                .contains("operation is invalid")
        );
        let outside = OperationId::new();
        assert_eq!(
            scheduler
                .reserve_delegated_dispatch(
                    OperationId::new(),
                    &outside.to_string(),
                    "child".into(),
                    now(),
                )
                .unwrap(),
            None
        );
        assert_eq!(
            scheduler
                .attach_delegated_dispatch(
                    OperationId::new(),
                    &outside.to_string(),
                    "child".into(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap(),
            None
        );
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch("invalid", &delegated_worker(workspace), now())
                .unwrap_err()
                .to_string()
                .contains("operation is invalid")
        );
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &outside.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("dispatch does not exist")
        );
        dispatch
            .upsert_run(DispatchRun {
                run_id: outside,
                agent_id: AgentId::new(),
                prompt: "outside".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert_eq!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &outside.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap(),
            None
        );
        assert!(
            scheduler
                .fail_reserved_delegated_dispatch("invalid", now())
                .unwrap_err()
                .to_string()
                .contains("operation is invalid")
        );
        assert_eq!(
            scheduler
                .fail_reserved_delegated_dispatch(&outside.to_string(), now())
                .unwrap(),
            None
        );

        let mut run = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        for (id, digest) in [
            ("ordinary-task".to_owned(), "ordinary".to_owned()),
            (
                "delegated-not-an-operation".to_owned(),
                "delegated-operation:not-an-operation".to_owned(),
            ),
            (
                format!("delegated-{outside}"),
                "not-the-daemon-origin-marker".to_owned(),
            ),
        ] {
            let id = TaskId::new(id).unwrap();
            let mut child = task_node(
                &run,
                id.clone(),
                Some(TaskId::new("root").unwrap()),
                BTreeSet::new(),
                "ordinary".into(),
                NO_ARTIFACT_CONTRACT,
            );
            child.instruction_digest = digest;
            child.state = TaskState::Ready;
            run.tasks.insert(id, child);
        }
        scheduler.supervisor.initialize(&run).unwrap();
        assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());

        let child_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child".into(),
                now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(scheduler.pending_delegated_promotions().unwrap().len(), 1);
        let child_id = delegated_task_id(child_operation).unwrap();
        let mut retrying = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        retrying.tasks.get_mut(&child_id).unwrap().state = TaskState::Retrying;
        retrying.tasks.get_mut(&child_id).unwrap().generation = 2;
        scheduler.supervisor.initialize(&retrying).unwrap();
        assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
        retrying.tasks.get_mut(&child_id).unwrap().state = TaskState::Ready;
        retrying.tasks.get_mut(&child_id).unwrap().generation = 1;
        scheduler.supervisor.initialize(&retrying).unwrap();
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_operation,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        let mut malformed = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        malformed.tasks.get_mut(&child_id).unwrap().parent_task_id = None;
        scheduler.supervisor.initialize(&malformed).unwrap();
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &child_operation.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("has no parent")
        );
        malformed.tasks.get_mut(&child_id).unwrap().parent_task_id =
            Some(TaskId::new("root").unwrap());
        malformed.provenance.clear();
        scheduler.supervisor.initialize(&malformed).unwrap();
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &child_operation.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("parent provenance is missing")
        );
    }

    #[test]
    fn duplicate_supervisor_membership_refuses_delegated_dispatch_ambiguity() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &root_operation.to_string(),
                "root".into(),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child".into(),
                now(),
            )
            .unwrap();

        let mut duplicate = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        duplicate.supervisor_run_id = SupervisorRunId::new();
        for task in duplicate.tasks.values_mut() {
            task.supervisor_run_id = duplicate.supervisor_run_id;
        }
        for provenance in duplicate.provenance.values_mut() {
            provenance.supervisor_run_id = duplicate.supervisor_run_id;
        }
        scheduler.supervisor.initialize(&duplicate).unwrap();

        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &OperationId::new().to_string(),
                    "another child".into(),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("multiple supervisor runs")
        );
        assert!(
            scheduler
                .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
                .unwrap_err()
                .to_string()
                .contains("multiple supervisor runs")
        );
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_operation,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &child_operation.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("multiple supervisor runs")
        );
    }

    #[test]
    fn definite_delegated_spawn_failure_closes_the_pending_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        scheduler
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &root_operation.to_string(),
                "root work".into(),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work".into(),
                now(),
            )
            .unwrap()
            .unwrap();

        let failed = scheduler
            .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
            .unwrap()
            .unwrap();
        let child = failed
            .tasks
            .iter()
            .find(|task| task.task_id.0 == format!("delegated-{child_operation}"))
            .unwrap();
        assert_eq!(child.state, TaskState::Cancelled);
        assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
        assert_eq!(
            scheduler
                .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
                .unwrap()
                .unwrap(),
            failed
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Each injected commit point proves that a reservation never reports an uncommitted transition.
    fn goal_and_delegated_reservation_commit_failures_are_reported() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let goal_operation = OperationId::new();
        let goal = scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &goal_operation.to_string(),
                "goal".into(),
                None,
                now(),
            )
            .unwrap();
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .fail_reserved_goal(&goal_operation.to_string(), "failed".into(), now())
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        let mut missing_root = scheduler
            .supervisor
            .load(goal.supervisor_run_id)
            .unwrap()
            .unwrap();
        missing_root.tasks.clear();
        scheduler.supervisor.initialize(&missing_root).unwrap();
        assert!(
            scheduler
                .fail_reserved_goal(&goal_operation.to_string(), "failed".into(), now())
                .unwrap_err()
                .to_string()
                .contains("root task is missing")
        );

        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let root_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &root_operation.to_string(),
                "root".into(),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child".into(),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child".into(),
                now(),
            )
            .unwrap();
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_operation,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        scheduler
            .tick(root.supervisor_run_id, now(), &mut Waker::default())
            .unwrap();
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &child_operation.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        scheduler.fail_apply_at(scheduler.apply_calls.get() + 1);
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &child_operation.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
    }

    #[test]
    fn artifact_transition_commit_failures_remain_retryable() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: "goal".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &operation.to_string(),
                "finish".into(),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();

        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        scheduler.fail_apply_at(scheduler.apply_calls.get() + 1);
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        let request = scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .unwrap();
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .record_artifact_verification(
                    &request,
                    ArtifactVerification {
                        passed: true,
                        result_digest: "verified".into(),
                        safe_summary: "verified".into(),
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
    }

    #[test]
    fn terminal_task_kinds_choose_a_safe_terminal_run_reason() {
        for (task_state, expected_reason) in [
            (TaskState::Failed, "one or more supervisor tasks failed"),
            (
                TaskState::Blocked,
                "one or more supervisor tasks were blocked",
            ),
            (
                TaskState::Cancelled,
                "one or more supervisor tasks were cancelled",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let scheduler = SupervisorRuntime::new(temp.path());
            let mut run = SupervisorRun::new(
                "caller".into(),
                "root".into(),
                "input".into(),
                "policy".into(),
                now(),
            );
            run.state = SupervisorRunState::Running;
            let mut root = task(run.supervisor_run_id, "root", None);
            root.state = task_state;
            run.tasks.insert(TaskId::new("root").unwrap(), root);
            scheduler.supervisor.initialize(&run).unwrap();
            let finalized = scheduler.finalize_terminal_tasks(run, now()).unwrap();
            assert_eq!(finalized.state, SupervisorRunState::Failed);
            assert_eq!(finalized.terminal_reason.as_deref(), Some(expected_reason));
        }
    }

    fn wake_reservation(index: usize, delivered: bool) -> WakeReservation {
        let run = SupervisorRunId::new();
        let parent = TaskId::new(format!("parent-{index}")).unwrap();
        let child = OperationId::new();
        WakeReservation {
            wake: DecisionWake {
                supervisor_run_id: run,
                parent_task_id: parent.clone(),
                parent_generation: 1,
                parent: provenance(run, &parent, None, OperationId::new()),
                child_run_id: child,
                outcome: WakeOutcome {
                    kind: InboxKind::Completed,
                    summary: "done".into(),
                },
                dag: Vec::new(),
                remaining_budget_summary: "none".into(),
            },
            delivered,
        }
    }

    #[test]
    fn terminal_statuses_and_sources_preserve_the_safe_completion_vocabulary() {
        assert_eq!(terminal(RunStatus::Running), None);
        assert_eq!(
            terminal(RunStatus::Completed),
            Some((TaskState::Succeeded, InboxKind::Completed))
        );
        assert_eq!(
            terminal(RunStatus::Failed),
            Some((TaskState::Failed, InboxKind::Failed))
        );
        assert_eq!(
            terminal(RunStatus::NoReport),
            Some((TaskState::Failed, InboxKind::NoReport))
        );
        assert_eq!(
            source(InboxKind::Completed),
            SupervisorEventSource::DispatchCompletion
        );
        assert_eq!(
            source(InboxKind::Failed),
            SupervisorEventSource::DispatchFailure
        );
        assert_eq!(source(InboxKind::NoReport), SupervisorEventSource::NoReport);
    }

    #[test]
    fn read_only_query_responses_have_an_aggregate_serialized_budget() {
        assert_eq!(
            bounded_supervisor_query(serde_json::json!({"runs": []})).unwrap(),
            serde_json::json!({"runs": []})
        );
        assert!(
            bounded_supervisor_query(serde_json::json!({
                "value": "x".repeat(RUN_LIST_RESPONSE_MAX_BYTES)
            }))
            .unwrap_err()
            .to_string()
            .contains("capacity is exhausted")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Every field/count boundary belongs to one admission matrix.
    fn start_input_limits_are_utf8_byte_bounds_before_any_durable_effect() {
        let exact_root = format!("{}a", "う".repeat((MAX_SUPERVISOR_TEXT_BYTES - 1) / 3));
        let exact_id = format!(
            "{}aa",
            "う".repeat((usagi_core::domain::supervisor::MAX_TASK_ID_BYTES - 2) / 3)
        );
        let task = InitialTask {
            task_id: exact_id,
            parent_task_id: None,
            dependencies: Vec::new(),
            instruction: "work".into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
        };
        assert_eq!(exact_root.len(), MAX_SUPERVISOR_TEXT_BYTES);
        validate_start_input(
            "operation",
            &exact_root,
            std::slice::from_ref(&task),
            Some("policy"),
        )
        .unwrap();
        assert!(
            validate_start_input("operation", &(exact_root + "x"), &[task], None,)
                .unwrap_err()
                .to_string()
                .contains("root task")
        );
        assert!(
            validate_start_input(&"x".repeat(MAX_SUPERVISOR_KEY_BYTES + 1), "root", &[], None,)
                .is_err()
        );
        assert!(
            validate_start_input(
                "operation",
                "root",
                &vec![
                    InitialTask {
                        task_id: "task".into(),
                        parent_task_id: None,
                        dependencies: Vec::new(),
                        instruction: "work".into(),
                        required_artifact_contract: NO_ARTIFACT_CONTRACT,
                    };
                    MAX_INITIAL_TASKS + 1
                ],
                None,
            )
            .is_err()
        );
        for invalid in [
            InitialTask {
                task_id: "x".repeat(usagi_core::domain::supervisor::MAX_TASK_ID_BYTES + 1),
                parent_task_id: None,
                dependencies: Vec::new(),
                instruction: "work".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: Some(
                    "x".repeat(usagi_core::domain::supervisor::MAX_TASK_ID_BYTES + 1),
                ),
                dependencies: Vec::new(),
                instruction: "work".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: None,
                dependencies: vec!["dependency".into(); MAX_TASK_DEPENDENCIES + 1],
                instruction: "work".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: None,
                dependencies: vec![
                    "x".repeat(usagi_core::domain::supervisor::MAX_TASK_ID_BYTES + 1),
                ],
                instruction: "work".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: None,
                dependencies: Vec::new(),
                instruction: "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            },
        ] {
            assert!(validate_start_input("operation", "root", &[invalid], None).is_err());
        }
        assert!(validate_start_input("operation", "root", &[], Some("")).is_err());
        assert!(
            serde_json::from_value::<InitialTask>(serde_json::json!({
                "task_id": "task",
                "instruction": "work",
                "required_artifact_contract": "unsupported"
            }))
            .is_err()
        );

        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        assert!(
            scheduler
                .start(
                    "caller",
                    "operation",
                    "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
                    Vec::new(),
                    None,
                    now(),
                )
                .is_err()
        );
        assert!(!scheduler.state_path.exists());
        assert!(!temp.path().join("supervisor-runs").exists());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Start and wake retention share one durable metadata contract.
    fn runtime_metadata_compacts_safe_history_and_backpressures_live_state() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let mut state = RuntimeState::default();

        for index in 0..MAX_START_RESERVATIONS {
            let run = SupervisorRun::new(
                "caller".into(),
                format!("task-{index}"),
                "input".into(),
                "policy".into(),
                now(),
            );
            scheduler.supervisor.initialize(&run).unwrap();
            state.starts.insert(
                format!("start-{index}"),
                StartReservation {
                    semantic_key: format!("semantic-{index}"),
                    supervisor_run_id: run.supervisor_run_id,
                },
            );
        }
        assert!(
            scheduler
                .ensure_start_capacity(&mut state)
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );

        let first_id = state.starts["start-0"].supervisor_run_id;
        let mut escalated = scheduler.supervisor.load(first_id).unwrap().unwrap();
        escalated.state = SupervisorRunState::Escalated;
        escalated.terminal_at = Some(now());
        json_file::write_atomic(
            scheduler
                .supervisor
                .snapshot_path(first_id)
                .parent()
                .unwrap(),
            &scheduler.supervisor.snapshot_path(first_id),
            &escalated,
        )
        .unwrap();
        assert!(
            scheduler
                .ensure_start_capacity(&mut state)
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );

        let mut finished = scheduler.supervisor.load(first_id).unwrap().unwrap();
        finished.state = SupervisorRunState::Succeeded;
        finished.terminal_at = Some(now());
        json_file::write_atomic(
            scheduler
                .supervisor
                .snapshot_path(first_id)
                .parent()
                .unwrap(),
            &scheduler.supervisor.snapshot_path(first_id),
            &finished,
        )
        .unwrap();
        scheduler.ensure_start_capacity(&mut state).unwrap();
        assert_eq!(state.starts.len(), MAX_START_RESERVATIONS - 1);
        assert!(state.expired_starts.contains("start-0"));

        let mut missing = RuntimeState::default();
        for index in 0..=MAX_START_RESERVATIONS {
            missing.starts.insert(
                format!("missing-{index}"),
                StartReservation {
                    semantic_key: format!("missing-semantic-{index}"),
                    supervisor_run_id: SupervisorRunId::new(),
                },
            );
        }
        scheduler.ensure_start_capacity(&mut missing).unwrap();
        assert_eq!(missing.starts.len(), MAX_START_RESERVATIONS - 1);
        assert!(missing.expired_starts.contains("missing-0"));
        assert!(missing.expired_starts.contains("missing-1"));

        scheduler.save_state(&state).unwrap();
        assert!(
            scheduler
                .start("caller", "start-0", "root".into(), Vec::new(), None, now(),)
                .unwrap_err()
                .to_string()
                .contains("idempotency window expired")
        );

        for index in 0..MAX_WAKE_RESERVATIONS {
            state
                .wakes
                .insert(format!("wake-{index:02}"), wake_reservation(index, true));
        }
        state.compact_delivered_wakes();
        assert_eq!(state.wakes.len(), RETAIN_DELIVERED_WAKES);
        assert!(state.expired_wakes.contains("wake-00"));

        state.wakes.clear();
        for index in 0..=MAX_WAKE_RESERVATIONS {
            state.wakes.insert(
                format!("pending-{index:02}"),
                wake_reservation(index, false),
            );
        }
        state.compact_delivered_wakes();
        assert_eq!(state.wakes.len(), MAX_WAKE_RESERVATIONS + 1);
        assert!(
            scheduler
                .save_state(&state)
                .unwrap_err()
                .to_string()
                .contains("hard limit")
        );

        let run_id = SupervisorRunId::new();
        let parent_id = TaskId::new("parent-capacity").unwrap();
        let child = OperationId::new();
        let mut run = SupervisorRun::new_with_id(
            run_id,
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let mut parent = task(run_id, "parent-capacity", None);
        parent.state = TaskState::AwaitingDecision;
        run.tasks.insert(parent_id.clone(), parent);
        run.provenance.insert(
            parent_id.clone(),
            provenance(run_id, &parent_id, None, OperationId::new()),
        );
        state.wakes.clear();
        state.wakes = (0..MAX_WAKE_RESERVATIONS)
            .map(|index| (format!("full-{index}"), wake_reservation(index, false)))
            .collect();
        scheduler.save_state(&state).unwrap();
        assert!(
            scheduler
                .reserve_parent_wake(&mut run, &parent_id, child, InboxKind::Completed, now(),)
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );
        state.wakes.clear();
        let wake_key = format!("{}:{}:{}", child, parent_id.0, 1);
        state.expired_wakes.insert(&wake_key);
        scheduler.save_state(&state).unwrap();
        scheduler
            .reserve_parent_wake(&mut run, &parent_id, child, InboxKind::Completed, now())
            .unwrap();
        assert!(scheduler.load_state().unwrap().wakes.is_empty());
    }

    #[test]
    fn oversized_or_malformed_runtime_metadata_fails_closed_on_load() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let mut state = RuntimeState {
            wakes: (0..=MAX_WAKE_RESERVATIONS)
                .map(|index| (format!("pending-{index}"), wake_reservation(index, false)))
                .collect(),
            ..RuntimeState::default()
        };
        json_file::write_atomic(temp.path(), &scheduler.state_path, &state).unwrap();
        assert!(
            scheduler
                .load_state()
                .unwrap_err()
                .to_string()
                .contains("hard limit")
        );

        state.wakes.clear();
        state.expired_starts.words.push(1);
        json_file::write_atomic(temp.path(), &scheduler.state_path, &state).unwrap();
        assert!(
            scheduler
                .load_state()
                .unwrap_err()
                .to_string()
                .contains("hard limit")
        );
    }

    #[test]
    fn a_missing_run_is_a_noop_and_does_not_call_the_waker() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let initial = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let mut waker = Waker::default();
        scheduler
            .tick(initial.supervisor_run_id, now(), &mut waker)
            .unwrap();
        assert!(waker.wakes.is_empty());
    }

    #[test]
    fn a_ready_task_without_a_dispatch_reservation_escalates_instead_of_stalling() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let started = scheduler
            .start(
                "caller",
                "operation",
                "root work".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        let mut waker = Waker::default();

        scheduler
            .tick(started.supervisor_run_id, now(), &mut waker)
            .unwrap();

        let stopped = scheduler
            .get("caller", started.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(stopped.state, SupervisorRunState::Escalated);
        let escalation = stopped.escalation.unwrap();
        assert_eq!(escalation.blocking_task_id.unwrap().0, "root");
        assert_eq!(
            escalation.reason,
            "no worker dispatch reservation was produced for a ready task"
        );
        assert!(escalation.safe_evidence.contains("runtime/model selection"));
        assert!(waker.wakes.is_empty());
    }

    #[test]
    fn a_dispatch_escalation_persistence_failure_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let started = scheduler
            .start(
                "caller",
                "operation",
                "root work".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        scheduler.fail_apply_at(2);

        let error = scheduler
            .tick(started.supervisor_run_id, now(), &mut Waker::default())
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("injected supervisor apply failure")
        );
    }

    #[test]
    fn tick_reconciles_only_retries_whose_durable_deadline_is_due() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let store = SupervisorStore::new(temp.path());
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let due_id = TaskId::new("due").unwrap();
        let future_id = TaskId::new("future").unwrap();
        let mut due = task(run.supervisor_run_id, "due", None);
        due.state = TaskState::Retrying;
        due.retry_at = Some(now());
        let mut future = task(run.supervisor_run_id, "future", None);
        future.state = TaskState::Retrying;
        future.retry_at = Some(now() + chrono::Duration::seconds(1));
        run.tasks = BTreeMap::from([(due_id.clone(), due), (future_id.clone(), future)]);
        store.initialize(&run).unwrap();

        scheduler.fail_apply_at(0);
        assert!(
            scheduler
                .tick(run.supervisor_run_id, now(), &mut Waker::default())
                .unwrap_err()
                .to_string()
                .contains("injected")
        );
        let scheduler = SupervisorRuntime::new(temp.path());
        scheduler
            .tick(run.supervisor_run_id, now(), &mut Waker::default())
            .unwrap();

        let saved = store.load(run.supervisor_run_id).unwrap().unwrap();
        assert_eq!(saved.tasks[&due_id].state, TaskState::Ready);
        assert_eq!(saved.tasks[&future_id].state, TaskState::Retrying);
    }

    #[test]
    fn start_propagates_each_injected_partial_apply_failure() {
        for fail_at in 0..=2 {
            let temp = tempfile::tempdir().unwrap();
            let scheduler = SupervisorRuntime::new(temp.path());
            scheduler.fail_apply_at(fail_at);
            let operation = format!("operation-{fail_at}");
            let initial_tasks = vec![InitialTask {
                task_id: "child".into(),
                parent_task_id: None,
                dependencies: vec!["root".into()],
                instruction: "child".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            }];
            assert!(
                scheduler
                    .start(
                        "caller",
                        &operation,
                        "root".into(),
                        initial_tasks.clone(),
                        None,
                        now(),
                    )
                    .unwrap_err()
                    .to_string()
                    .contains("injected")
            );
            let recovered = scheduler
                .start(
                    "caller",
                    &operation,
                    "root".into(),
                    initial_tasks,
                    None,
                    now(),
                )
                .unwrap();
            assert_eq!(recovered.state, SupervisorRunState::Running);
            assert_eq!(recovered.tasks.len(), 2);
        }
    }

    #[test]
    fn start_recovery_refuses_inconsistent_partial_snapshots() {
        for (fail_at, expected) in [
            (0, "reservation does not match"),
            (1, "root task conflicts"),
            (2, "initial task conflicts"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let scheduler = SupervisorRuntime::new(temp.path());
            let operation = format!("inconsistent-{fail_at}");
            let initial_tasks = vec![InitialTask {
                task_id: "child".into(),
                parent_task_id: None,
                dependencies: vec!["root".into()],
                instruction: "child".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            }];
            scheduler.fail_apply_at(fail_at);
            assert!(
                scheduler
                    .start(
                        "caller",
                        &operation,
                        "root".into(),
                        initial_tasks.clone(),
                        None,
                        now(),
                    )
                    .is_err()
            );
            let id = scheduler.load_state().unwrap().starts[&operation].supervisor_run_id;
            let mut run = scheduler.supervisor.load(id).unwrap().unwrap();
            if fail_at == 0 {
                run.root_caller_ref = "other-caller".into();
            } else if fail_at == 1 {
                run.tasks
                    .get_mut(&TaskId::new("root").unwrap())
                    .unwrap()
                    .instruction_body = "other root".into();
            } else {
                run.tasks
                    .get_mut(&TaskId::new("child").unwrap())
                    .unwrap()
                    .instruction_body = "other child".into();
            }
            scheduler.supervisor.initialize(&run).unwrap();
            assert!(
                scheduler
                    .start(
                        "caller",
                        &operation,
                        "root".into(),
                        initial_tasks,
                        None,
                        now(),
                    )
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn structured_inbox_report_is_used_for_the_wake_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let run_id = OperationId::new();
        let caller = CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: AgentId::new(),
        };
        dispatch
            .upsert_binding(DispatchBinding {
                run_id,
                caller: caller.clone(),
                worker: WorkerRef {
                    session_id: Some(SessionId::new()),
                    agent_id: AgentId::new(),
                },
            })
            .unwrap();
        dispatch
            .append_inbox(
                &caller,
                InboxMessage {
                    run_id,
                    from: WorkerRef {
                        session_id: Some(SessionId::new()),
                        agent_id: AgentId::new(),
                    },
                    kind: InboxKind::Failed,
                    summary: "safe failure".into(),
                    result: None,
                    created_at: now(),
                    read: false,
                },
            )
            .unwrap();
        assert_eq!(
            scheduler.outcome(run_id, InboxKind::Completed).unwrap(),
            WakeOutcome {
                kind: InboxKind::Failed,
                summary: "safe failure".into(),
            }
        );
    }

    #[test]
    fn incomplete_parent_provenance_is_fail_closed_after_child_completion() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let store = SupervisorStore::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let parent = TaskId::new("parent").unwrap();
        let child = TaskId::new("child").unwrap();
        let child_run = OperationId::new();
        let mut parent_task = task(run.supervisor_run_id, "parent", None);
        parent_task.state = TaskState::AwaitingDecision;
        let mut child_task = task(run.supervisor_run_id, "child", Some("parent"));
        child_task.state = TaskState::Dispatched;
        run.tasks = BTreeMap::from([(parent.clone(), parent_task), (child.clone(), child_task)]);
        run.provenance.insert(
            child.clone(),
            provenance(
                run.supervisor_run_id,
                &child,
                Some((&parent, OperationId::new())),
                child_run,
            ),
        );
        store.initialize(&run).unwrap();
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_run,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::NoReport,
            })
            .unwrap();
        let mut waker = Waker::default();
        scheduler
            .tick(run.supervisor_run_id, now(), &mut waker)
            .unwrap();
        assert_eq!(
            store.load(run.supervisor_run_id).unwrap().unwrap().tasks[&child].state,
            TaskState::Failed
        );
        assert!(waker.wakes.is_empty());
    }

    #[test]
    fn tick_retries_a_partial_parent_wake_and_ignores_nonterminal_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let store = SupervisorStore::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let parent = TaskId::new("parent").unwrap();
        let waiting = TaskId::new("waiting").unwrap();
        let child = TaskId::new("child").unwrap();
        let waiting_run = OperationId::new();
        let child_run = OperationId::new();
        let mut parent_task = task(run.supervisor_run_id, "parent", None);
        parent_task.state = TaskState::Running;
        let mut waiting_task = task(run.supervisor_run_id, "waiting", None);
        waiting_task.state = TaskState::Dispatched;
        let mut child_task = task(run.supervisor_run_id, "child", Some("parent"));
        child_task.state = TaskState::Dispatched;
        run.tasks = BTreeMap::from([
            (parent.clone(), parent_task),
            (waiting.clone(), waiting_task),
            (child.clone(), child_task),
        ]);
        run.provenance.insert(
            waiting.clone(),
            provenance(run.supervisor_run_id, &waiting, None, waiting_run),
        );
        run.provenance.insert(
            child.clone(),
            provenance(
                run.supervisor_run_id,
                &child,
                Some((&parent, OperationId::new())),
                child_run,
            ),
        );
        store.initialize(&run).unwrap();
        for (run_id, status) in [
            (waiting_run, RunStatus::Running),
            (child_run, RunStatus::Completed),
        ] {
            dispatch
                .upsert_run(DispatchRun {
                    run_id,
                    agent_id: AgentId::new(),
                    prompt: "child".into(),
                    started_at: now(),
                    ended_at: None,
                    status,
                })
                .unwrap();
        }

        scheduler.fail_apply_at(1);
        assert!(
            scheduler
                .tick(run.supervisor_run_id, now(), &mut Waker::default())
                .unwrap_err()
                .to_string()
                .contains("injected")
        );
        let scheduler = SupervisorRuntime::new(temp.path());
        scheduler.fail_apply_at(1);
        assert!(
            scheduler
                .tick(run.supervisor_run_id, now(), &mut Waker::default())
                .unwrap_err()
                .to_string()
                .contains("injected")
        );
        let scheduler = SupervisorRuntime::new(temp.path());
        let mut waker = Waker::default();
        scheduler
            .tick(run.supervisor_run_id, now(), &mut waker)
            .unwrap();
        assert_eq!(scheduler.dispatch_registry_reads.get(), 1);
        let saved = store.load(run.supervisor_run_id).unwrap().unwrap();
        assert_eq!(saved.tasks[&waiting].state, TaskState::Dispatched);
        assert_eq!(saved.tasks[&child].state, TaskState::Succeeded);
        assert_eq!(saved.tasks[&parent].state, TaskState::AwaitingDecision);
        assert!(waker.wakes.is_empty());
    }

    #[test]
    fn tick_skips_blocked_provenance_and_accepts_terminal_work_without_a_parent() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let store = SupervisorStore::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let blocked = TaskId::new("blocked").unwrap();
        let standalone = TaskId::new("standalone").unwrap();
        let terminal_parent = TaskId::new("terminal-parent").unwrap();
        let blocked_run = OperationId::new();
        let standalone_run = OperationId::new();
        let mut blocked_task = task(run.supervisor_run_id, "blocked", None);
        blocked_task.state = TaskState::AwaitingDecision;
        let mut standalone_task = task(run.supervisor_run_id, "standalone", None);
        standalone_task.state = TaskState::Dispatched;
        let mut terminal_parent_task = task(run.supervisor_run_id, "terminal-parent", None);
        terminal_parent_task.state = TaskState::Succeeded;
        run.tasks = BTreeMap::from([
            (blocked.clone(), blocked_task),
            (standalone.clone(), standalone_task),
            (terminal_parent.clone(), terminal_parent_task),
        ]);
        run.provenance.insert(
            blocked.clone(),
            provenance(run.supervisor_run_id, &blocked, None, blocked_run),
        );
        run.provenance.insert(
            standalone.clone(),
            provenance(run.supervisor_run_id, &standalone, None, standalone_run),
        );
        scheduler
            .reserve_parent_wake(
                &mut run,
                &terminal_parent,
                OperationId::new(),
                InboxKind::Completed,
                now(),
            )
            .unwrap();
        store.initialize(&run).unwrap();
        for run_id in [blocked_run, standalone_run] {
            dispatch
                .upsert_run(DispatchRun {
                    run_id,
                    agent_id: AgentId::new(),
                    prompt: "work".into(),
                    started_at: now(),
                    ended_at: Some(now()),
                    status: RunStatus::Completed,
                })
                .unwrap();
        }

        scheduler
            .tick(run.supervisor_run_id, now(), &mut Waker::default())
            .unwrap();
        let saved = store.load(run.supervisor_run_id).unwrap().unwrap();
        assert_eq!(saved.tasks[&blocked].state, TaskState::AwaitingDecision);
        assert_eq!(saved.tasks[&standalone].state, TaskState::Succeeded);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // The fixture is a complete durable history.
    fn completion_is_reconciled_once_and_restart_does_not_duplicate_the_parent_wake() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let store = SupervisorStore::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let initial = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let id = initial.supervisor_run_id;
        store.initialize(&initial).unwrap();
        let parent_id = TaskId::new("parent").unwrap();
        let child_id = TaskId::new("child").unwrap();
        let parent_run = OperationId::new();
        let child_run = OperationId::new();
        let mut run = store.load(id).unwrap().unwrap();
        run = store
            .apply(
                id,
                run.state_revision,
                &event(
                    &run,
                    SupervisorEventKind::AddTask {
                        task: task(id, "parent", None),
                    },
                ),
            )
            .unwrap();
        run = store
            .apply(
                id,
                run.state_revision,
                &event(
                    &run,
                    SupervisorEventKind::Dispatch {
                        task_id: parent_id.clone(),
                        generation: 1,
                        provenance: provenance(id, &parent_id, None, parent_run),
                    },
                ),
            )
            .unwrap();
        run = store
            .apply(
                id,
                run.state_revision,
                &event(
                    &run,
                    SupervisorEventKind::Running {
                        task_id: parent_id.clone(),
                        generation: 1,
                    },
                ),
            )
            .unwrap();
        run = store
            .apply(
                id,
                run.state_revision,
                &event(
                    &run,
                    SupervisorEventKind::AddTask {
                        task: task(id, "child", Some("parent")),
                    },
                ),
            )
            .unwrap();
        let _ = store
            .apply(
                id,
                run.state_revision,
                &event(
                    &run,
                    SupervisorEventKind::Dispatch {
                        task_id: child_id.clone(),
                        generation: 1,
                        provenance: provenance(
                            id,
                            &child_id,
                            Some((&parent_id, parent_run)),
                            child_run,
                        ),
                    },
                ),
            )
            .unwrap();
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_run,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        let mut waker = Waker::default();
        scheduler.tick(id, now(), &mut waker).unwrap();
        let saved = store.load(id).unwrap().unwrap();
        assert_eq!(saved.tasks[&child_id].state, TaskState::Succeeded);
        assert_eq!(saved.tasks[&parent_id].state, TaskState::AwaitingDecision);
        assert_eq!(waker.wakes.len(), 1);
        assert_eq!(waker.wakes[0].child_run_id, child_run);

        let restarted = SupervisorRuntime::new(temp.path());
        restarted.tick(id, now(), &mut waker).unwrap();
        assert_eq!(waker.wakes.len(), 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn control_surface_is_idempotent_owned_and_durable() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let initial = vec![InitialTask {
            task_id: "child".into(),
            parent_task_id: None,
            dependencies: vec!["root".into()],
            instruction: "secret child instruction".into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
        }];
        let started = runtime
            .start(
                "caller-a",
                "operation-a",
                "secret root instruction".into(),
                initial.clone(),
                None,
                now(),
            )
            .unwrap();
        assert_eq!(started.state, SupervisorRunState::Running);
        assert_eq!(started.tasks.len(), 2);
        assert_eq!(
            started
                .tasks
                .iter()
                .find(|task| task.task_id.0 == "child")
                .and_then(|task| task.parent_task_id.as_ref())
                .map(|task| task.0.as_str()),
            Some("root")
        );
        assert_eq!(
            runtime
                .start(
                    "caller-a",
                    "operation-a",
                    "secret root instruction".into(),
                    initial,
                    None,
                    now(),
                )
                .unwrap()
                .supervisor_run_id,
            started.supervisor_run_id
        );
        assert!(
            runtime
                .start(
                    "caller-a",
                    "operation-a",
                    "different".into(),
                    vec![],
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("reused")
        );
        assert!(
            runtime
                .get("caller-b", started.supervisor_run_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            runtime
                .get("caller-a", started.supervisor_run_id)
                .unwrap()
                .unwrap(),
            started
        );
        assert_eq!(
            runtime
                .list("caller-a", Some(SupervisorRunState::Running))
                .unwrap()
                .len(),
            1
        );
        let page = runtime
            .list_page("caller-a", Some(SupervisorRunState::Running), 0, 1)
            .unwrap();
        assert_eq!(page.runs.len(), 1);
        assert!(page.next_cursor.is_none());
        assert!(
            runtime
                .list_page("caller-b", None, 0, 1)
                .unwrap()
                .runs
                .is_empty()
        );
        let (events, cursor) = runtime
            .events("caller-a", started.supervisor_run_id, 0, 10)
            .unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(cursor.next_sequence, 4);
        assert!(
            runtime
                .events("caller-b", started.supervisor_run_id, 0, 10)
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
        assert!(
            runtime
                .cancel(
                    "caller-b",
                    started.supervisor_run_id,
                    "foreign".into(),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
        assert!(
            runtime
                .resolve_escalation(
                    "caller-b",
                    started.supervisor_run_id,
                    OperationId::new(),
                    EscalationDecision::Resume,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
        let run = runtime
            .supervisor
            .load(started.supervisor_run_id)
            .unwrap()
            .unwrap();
        let escalated = runtime
            .apply(
                &run,
                now(),
                SupervisorEventSource::Admission,
                SupervisorEventKind::Escalate {
                    task_id: None,
                    reason: "operator decision required".into(),
                    safe_evidence: "safe evidence".into(),
                    choices: vec!["resume".into()],
                },
            )
            .unwrap();
        let escalation_id = escalated.escalation.as_ref().unwrap().escalation_id;
        let resumed = runtime
            .resolve_escalation(
                "caller-a",
                started.supervisor_run_id,
                escalation_id,
                EscalationDecision::Resume,
                now(),
            )
            .unwrap();
        assert_eq!(resumed.state, SupervisorRunState::Running);
        let cancelled = runtime
            .cancel(
                "caller-a",
                started.supervisor_run_id,
                "operator requested".into(),
                now(),
            )
            .unwrap();
        assert_eq!(cancelled.state, SupervisorRunState::Cancelled);
        assert_eq!(
            SupervisorRuntime::new(temp.path())
                .list("caller-a", None)
                .unwrap()
                .len(),
            1
        );
        runtime.tick_all(now(), &mut Waker::default()).unwrap();
    }

    #[test]
    fn cancel_rejects_unsafe_or_unbounded_presented_reasons() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let run = runtime
            .start(
                "caller",
                "operation",
                "root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        for reason in [
            String::new(),
            "clear\u{1b}[2J".into(),
            "line\nbreak".into(),
            "direction\u{202e}override".into(),
            "x".repeat(MAX_SUPERVISOR_REASON_BYTES + 1),
        ] {
            assert!(
                runtime
                    .cancel("caller", run.supervisor_run_id, reason, now())
                    .is_err()
            );
        }
        assert_eq!(
            runtime
                .cancel(
                    "caller",
                    run.supervisor_run_id,
                    "operator requested stop".into(),
                    now(),
                )
                .unwrap()
                .state,
            SupervisorRunState::Cancelled
        );
    }

    #[test]
    fn start_rejects_an_unresolvable_initial_dag() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let error = runtime
            .start(
                "caller",
                "operation",
                "root".into(),
                vec![InitialTask {
                    task_id: "child".into(),
                    parent_task_id: None,
                    dependencies: vec!["missing".into()],
                    instruction: "child".into(),
                    required_artifact_contract: NO_ARTIFACT_CONTRACT,
                }],
                Some("strict".into()),
                now(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("missing dependency or cycle"));
        let parsed: InitialTask = serde_json::from_value(serde_json::json!({
            "task_id": "default-contract",
            "instruction": "body"
        }))
        .unwrap();
        assert_eq!(parsed.required_artifact_contract, NO_ARTIFACT_CONTRACT);
    }

    #[test]
    fn workspace_listing_exposes_only_explicitly_scoped_runs() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let first = WorkspaceId::new();
        let second = WorkspaceId::new();
        let visible = runtime
            .start_for_workspace(
                "caller-a",
                first,
                "scoped-a",
                "root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        runtime
            .start_for_workspace(
                "caller-b",
                second,
                "scoped-b",
                "root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        runtime
            .start("legacy", "unscoped", "root".into(), Vec::new(), None, now())
            .unwrap();

        let listed = runtime.list_workspace(first).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].supervisor_run_id, visible.supervisor_run_id);
        assert!(
            runtime
                .list_workspace(WorkspaceId::new())
                .unwrap()
                .is_empty()
        );
    }
}
