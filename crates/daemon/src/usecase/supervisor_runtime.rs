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
        agent::{InboxKind, RunStatus},
        id::OperationId,
        supervisor::{
            EscalationDecision, MAX_ARTIFACT_CONTRACT_BYTES, MAX_INITIAL_TASKS,
            MAX_SUPERVISOR_KEY_BYTES, MAX_SUPERVISOR_TEXT_BYTES, MAX_TASK_DEPENDENCIES,
            RunProvenance, SupervisorEvent, SupervisorEventKind, SupervisorEventSource,
            SupervisorRun, SupervisorRunId, SupervisorRunQuery, SupervisorRunState, TaskId,
            TaskNode, TaskState,
        },
    },
    infrastructure::{
        persistence::json_file,
        store::{
            dispatch::DispatchStore,
            supervisor::{EventCursor, EventQuery, SupervisorStore},
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
    #[serde(default = "default_artifact_contract")]
    pub required_artifact_contract: String,
}

fn bounded_nonempty(name: &str, value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max {
        anyhow::bail!("invalid {name}: expected 1..={max} UTF-8 bytes");
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
        bounded_nonempty(
            "supervisor artifact contract",
            &task.required_artifact_contract,
            MAX_ARTIFACT_CONTRACT_BYTES,
        )?;
    }
    Ok(())
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=start_rejects_an_unresolvable_initial_dag
fn default_artifact_contract() -> String {
    "none".into()
}

fn push_semantic_component(key: &mut String, value: &str) {
    key.push_str(&value.len().to_string());
    key.push(':');
    key.push_str(value);
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
        validate_start_input(
            operation_id,
            &root_task,
            &initial_tasks,
            policy_selector.as_deref(),
        )?;
        let mut semantic_key = String::new();
        push_semantic_component(&mut semantic_key, caller);
        push_semantic_component(&mut semantic_key, &root_task);
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
            push_semantic_component(&mut semantic_key, &task.required_artifact_contract);
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
        if let Some(run) = self.supervisor.load(reservation.supervisor_run_id)? {
            return Ok(run.query());
        }
        let policy_revision = policy_selector.unwrap_or_else(|| "default".into());
        let mut run = SupervisorRun::new_with_id(
            reservation.supervisor_run_id,
            caller.to_owned(),
            operation_id.to_owned(),
            operation_id.to_owned(),
            policy_revision,
            now,
        );
        self.supervisor.initialize(&run)?;
        let root_id = TaskId::new("root")?;
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
                    "none".into(),
                ),
            },
        )?;
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
                if dependencies.iter().all(|id| run.tasks.contains_key(id))
                    && run.tasks.contains_key(&parent)
                {
                    let task_id = TaskId::new(task.task_id)?;
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
        self.deliver_reserved(waker)
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
                Some(run) if run.state.terminal() => {
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
    required_artifact_contract: String,
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
        id::{AgentId, AgentRuntimeId, SessionId, WorktreeId},
        supervisor::{SupervisorRun, TaskNode},
    };

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap()
    }
    fn task(run: SupervisorRunId, id: &str, parent: Option<&str>) -> TaskNode {
        TaskNode {
            task_id: TaskId::new(id).unwrap(),
            supervisor_run_id: run,
            parent_task_id: parent.map(|id| TaskId::new(id).unwrap()),
            dependencies: BTreeSet::new(),
            instruction_digest: id.into(),
            instruction_body: id.into(),
            required_artifact_contract: "none".into(),
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
            worker_session_id: SessionId::new(),
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
            required_artifact_contract: "none".into(),
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
            validate_start_input("operation", &(exact_root + "x"), &[task], None)
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
                        required_artifact_contract: "none".into(),
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
                required_artifact_contract: "none".into(),
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: Some(
                    "x".repeat(usagi_core::domain::supervisor::MAX_TASK_ID_BYTES + 1),
                ),
                dependencies: Vec::new(),
                instruction: "work".into(),
                required_artifact_contract: "none".into(),
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: None,
                dependencies: vec!["dependency".into(); MAX_TASK_DEPENDENCIES + 1],
                instruction: "work".into(),
                required_artifact_contract: "none".into(),
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: None,
                dependencies: vec![
                    "x".repeat(usagi_core::domain::supervisor::MAX_TASK_ID_BYTES + 1),
                ],
                instruction: "work".into(),
                required_artifact_contract: "none".into(),
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: None,
                dependencies: Vec::new(),
                instruction: "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
                required_artifact_contract: "none".into(),
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: None,
                dependencies: Vec::new(),
                instruction: "work".into(),
                required_artifact_contract: "x".repeat(MAX_ARTIFACT_CONTRACT_BYTES + 1),
            },
        ] {
            assert!(validate_start_input("operation", "root", &[invalid], None).is_err());
        }
        assert!(validate_start_input("operation", "root", &[], Some("")).is_err());

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
            assert!(
                scheduler
                    .start(
                        "caller",
                        &format!("operation-{fail_at}"),
                        "root".into(),
                        vec![InitialTask {
                            task_id: "child".into(),
                            parent_task_id: None,
                            dependencies: vec!["root".into()],
                            instruction: "child".into(),
                            required_artifact_contract: "none".into(),
                        }],
                        None,
                        now(),
                    )
                    .unwrap_err()
                    .to_string()
                    .contains("injected")
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
            required_artifact_contract: "none".into(),
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
                    required_artifact_contract: "none".into(),
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
        assert_eq!(parsed.required_artifact_contract, "none");
    }
}
