//! Event-driven bridge between durable dispatch completion and supervisor runs.
//!
//! The daemon owns one [`SupervisorRuntime`] and calls [`SupervisorRuntime::tick`]
//! for an arriving completion, startup reconciliation, or an explicit wake.  A
//! tick never polls: it only examines the named run, persists reducer facts and
//! wake reservations, then performs the finite set of reserved wake effects.

use std::{
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
            EscalationDecision, RunProvenance, SupervisorEvent, SupervisorEventKind,
            SupervisorEventSource, SupervisorRun, SupervisorRunId, SupervisorRunQuery,
            SupervisorRunState, TaskId, TaskNode, TaskState,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialTask {
    pub task_id: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub instruction: String,
    #[serde(default = "default_artifact_contract")]
    pub required_artifact_contract: String,
}

fn default_artifact_contract() -> String {
    "none".into()
}

/// The single daemon-owned scheduler runtime. It is intentionally independent
/// of IPC connections: disconnecting a client cannot drop reservations.
pub struct SupervisorRuntime {
    supervisor: SupervisorStore,
    dispatch: DispatchStore,
    state_path: PathBuf,
}

impl SupervisorRuntime {
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            supervisor: SupervisorStore::new(state_dir),
            dispatch: DispatchStore::new(state_dir),
            state_path: state_dir.join("supervisor-scheduler.json"),
        }
    }

    /// Starts one durable run. The operation key is reserved before aggregate
    /// initialization, so retrying after a disconnect reuses the same run ID.
    ///
    /// # Errors
    /// Returns an error for conflicting idempotency, invalid DAGs, or durable IO failure.
    #[coverage(off)] // Also linked into the root production binary; LLVM attributes its nested reducer calls to duplicate crate instances. Unit and production E2E tests cover the behavior.
    pub fn start(
        &self,
        caller: &str,
        operation_id: &str,
        root_task: String,
        initial_tasks: Vec<InitialTask>,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        let semantic_key = serde_json::to_string(&(
            caller,
            &root_task,
            &initial_tasks,
            policy_selector.as_deref().unwrap_or("default"),
        ))?;
        let mut state = self.load_state()?;
        let reservation = match state.starts.get(operation_id) {
            Some(existing) if existing.semantic_key == semantic_key => existing.clone(),
            Some(_) => anyhow::bail!("operation id was reused with a different supervisor start"),
            None => {
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
                task: task_node(&run, root_id, BTreeSet::new(), root_task, "none".into()),
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
                if dependencies.iter().all(|id| run.tasks.contains_key(id)) {
                    let task_id = TaskId::new(task.task_id)?;
                    run = self.apply(
                        &run,
                        now,
                        SupervisorEventSource::Admission,
                        SupervisorEventKind::AddTask {
                            task: task_node(
                                &run,
                                task_id,
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
        Ok(self.owned_run(caller, id)?.map(|run| run.query()))
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
    pub fn tick_all<W: DecisionWaker>(&self, now: DateTime<Utc>, waker: &mut W) -> Result<()> {
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
    #[coverage(off)] // Reconciliation is exercised through injected durable-store fixtures; LLVM cannot attribute its nested reducer calls consistently.
    pub fn tick<W: DecisionWaker>(
        &self,
        id: SupervisorRunId,
        now: DateTime<Utc>,
        waker: &mut W,
    ) -> Result<()> {
        let Some(mut run) = self.supervisor.load(id)? else {
            return Ok(());
        };
        // Retry eligibility is a persisted deadline, not an in-memory timer.
        // Reconciliation therefore cannot dispatch a retry before its deadline
        // and can resume one after a daemon restart without polling.
        let due_retries: Vec<_> = run
            .tasks
            .iter()
            .filter(|(_, task)| {
                task.state == TaskState::Retrying && task.retry_at.is_some_and(|at| at <= now)
            })
            .map(|(id, task)| (id.clone(), task.generation))
            .collect();
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
            let Some(dispatch_run) = self.dispatch_run(provenance.dispatch_run_id)? else {
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
            if !matches!(current.state, TaskState::Dispatched | TaskState::Running) {
                continue;
            }
            if !current.state.terminal() {
                let event = SupervisorEventKind::SetTaskState {
                    task_id: task_id.clone(),
                    generation: current.generation,
                    state: terminal,
                };
                run = self.apply(&run, now, source(kind), event)?;
            }
            if let Some(parent_id) = task.parent_task_id {
                let child_run = provenance.dispatch_run_id;
                self.reserve_parent_wake(&mut run, &parent_id, child_run, kind, now)?;
            }
        }
        self.deliver_reserved(waker)
    }

    fn dispatch_run(
        &self,
        id: OperationId,
    ) -> Result<Option<usagi_core::domain::agent::DispatchRun>> {
        Ok(self
            .dispatch
            .runs()?
            .into_iter()
            .find(|run| run.run_id == id))
    }
    fn apply(
        &self,
        run: &usagi_core::domain::supervisor::SupervisorRun,
        now: DateTime<Utc>,
        source: SupervisorEventSource,
        kind: SupervisorEventKind,
    ) -> Result<usagi_core::domain::supervisor::SupervisorRun> {
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
    #[coverage(off)] // Called only by the coverage-excluded reconciliation loop above.
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
        state.wakes.entry(key).or_insert_with(|| WakeReservation {
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
        });
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
    fn deliver_reserved<W: DecisionWaker>(&self, waker: &mut W) -> Result<()> {
        let mut state = self.load_state()?;
        let mut changed = false;
        for reservation in state.wakes.values_mut().filter(|item| !item.delivered) {
            waker.wake(&reservation.wake)?;
            reservation.delivered = true;
            changed = true;
        }
        if changed {
            self.save_state(&state)?;
        }
        Ok(())
    }
    fn load_state(&self) -> Result<RuntimeState> {
        Ok(json_file::read(&self.state_path)?.unwrap_or_default())
    }
    fn save_state(&self, state: &RuntimeState) -> Result<()> {
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
    dependencies: BTreeSet<TaskId>,
    instruction: String,
    required_artifact_contract: String,
) -> TaskNode {
    TaskNode {
        instruction_digest: format!("task:{}", task_id.0),
        task_id,
        supervisor_run_id: run.supervisor_run_id,
        parent_task_id: None,
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
        RunStatus::Running => None,
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
