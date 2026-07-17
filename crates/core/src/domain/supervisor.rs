//! Durable supervisor-run domain model and its pure reducer.
//!
//! This module deliberately contains no scheduler or policy interpretation.
//! It records facts admitted by those layers and makes invalid histories
//! unrepresentable in the persisted state.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

use crate::domain::id::{AgentRuntimeId, OperationId, SessionId, WorktreeId};

/// A `UUIDv7` identity for one never-reused supervisor run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SupervisorRunId(Uuid);

impl SupervisorRunId {
    #[must_use]
    #[allow(clippy::new_without_default)]
    #[coverage(off)]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl fmt::Display for SupervisorRunId {
    #[coverage(off)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.hyphenated())
    }
}
impl Serialize for SupervisorRunId {
    #[coverage(off)]
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for SupervisorRunId {
    #[coverage(off)]
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let uuid = Uuid::parse_str(&value).map_err(de::Error::custom)?;
        if uuid.hyphenated().to_string() != value || uuid.get_version_num() != 7 {
            return Err(de::Error::custom(
                "supervisor run ID must be canonical UUIDv7",
            ));
        }
        Ok(Self(uuid))
    }
}

/// Opaque stable task identity.  Its spelling is never inferred from a session
/// name; callers may encode a provenance path in it if they need one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

impl TaskId {
    /// Creates an opaque task key.
    ///
    /// # Errors
    /// Returns [`SupervisorError::InvalidTaskId`] for an empty key.
    #[coverage(off)]
    pub fn new(value: impl Into<String>) -> Result<Self, SupervisorError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(SupervisorError::InvalidTaskId);
        }
        Ok(Self(value))
    }
}

/// Coarse run state.  Policy chooses *when* to emit these facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorRunState {
    Planning,
    Running,
    WaitingForDecision,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
    Escalated,
}
impl SupervisorRunState {
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Escalated
        )
    }
}

/// State of one node in a task DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Ready,
    Dispatched,
    Running,
    AwaitingDecision,
    Retrying,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
    Blocked,
}
impl TaskState {
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Blocked
        )
    }
}

/// A redaction-safe task contract.  The instruction body is kept durably for
/// workers, while query models expose only its digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskNode {
    pub task_id: TaskId,
    pub supervisor_run_id: SupervisorRunId,
    pub parent_task_id: Option<TaskId>,
    pub dependencies: BTreeSet<TaskId>,
    pub instruction_digest: String,
    pub instruction_body: String,
    pub required_artifact_contract: String,
    pub attempt: u64,
    pub generation: u64,
    pub assigned_dispatch_run: Option<OperationId>,
    pub state: TaskState,
}

/// One-to-one fence between a task generation and the concrete worker
/// incarnation that received it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProvenance {
    pub supervisor_run_id: SupervisorRunId,
    pub task_id: TaskId,
    pub parent_task_id: Option<TaskId>,
    pub parent_dispatch_run: Option<OperationId>,
    pub dispatch_run_id: OperationId,
    pub worker_session_id: SessionId,
    pub worker_agent_id: AgentRuntimeId,
    pub worker_worktree_id: WorktreeId,
    pub generation: u64,
}

/// Durable cause of an accepted supervisor event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorEventSource {
    DispatchCompletion,
    DispatchFailure,
    NoReport,
    Timer,
    Cancel,
    Verification,
    Admission,
}

/// Reducer inputs.  Payload bodies are deliberately not copied into event
/// queries; the envelope retains only a payload digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorEventKind {
    AddTask {
        task: TaskNode,
    },
    Dispatch {
        task_id: TaskId,
        generation: u64,
        provenance: RunProvenance,
    },
    Running {
        task_id: TaskId,
        generation: u64,
    },
    SetTaskState {
        task_id: TaskId,
        generation: u64,
        state: TaskState,
    },
    SetRunState {
        state: SupervisorRunState,
        terminal_reason: Option<String>,
    },
}

/// Append-only event envelope.  `event_id` is the idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorEvent {
    pub sequence: u64,
    pub event_id: OperationId,
    pub causation_id: Option<OperationId>,
    pub correlation_id: Option<OperationId>,
    pub observed_at: DateTime<Utc>,
    pub payload_digest: String,
    pub source: SupervisorEventSource,
    pub kind: SupervisorEventKind,
}

/// Durable aggregate snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorRun {
    pub supervisor_run_id: SupervisorRunId,
    pub root_caller_ref: String,
    pub root_task_digest: String,
    pub root_input_digest: String,
    pub policy_revision: String,
    pub state_revision: u64,
    pub state: SupervisorRunState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub terminal_at: Option<DateTime<Utc>>,
    pub terminal_reason: Option<String>,
    pub tasks: BTreeMap<TaskId, TaskNode>,
    pub provenance: BTreeMap<TaskId, RunProvenance>,
    /// Event IDs already reduced.  This is persisted so journal replay is
    /// idempotent after a crash between append and snapshot write.
    pub applied_events: BTreeSet<OperationId>,
}

impl SupervisorRun {
    #[must_use]
    #[coverage(off)]
    pub fn new(
        root_caller_ref: String,
        root_task_digest: String,
        root_input_digest: String,
        policy_revision: String,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            supervisor_run_id: SupervisorRunId::new(),
            root_caller_ref,
            root_task_digest,
            root_input_digest,
            policy_revision,
            state_revision: 0,
            state: SupervisorRunState::Planning,
            created_at: now,
            updated_at: now,
            terminal_at: None,
            terminal_reason: None,
            tasks: BTreeMap::new(),
            provenance: BTreeMap::new(),
            applied_events: BTreeSet::new(),
        }
    }

    /// Returns a redaction-safe projection for callers.
    #[must_use]
    #[coverage(off)]
    pub fn query(&self) -> SupervisorRunQuery {
        SupervisorRunQuery {
            supervisor_run_id: self.supervisor_run_id,
            state_revision: self.state_revision,
            state: self.state,
            terminal_at: self.terminal_at,
            terminal_reason: self.terminal_reason.clone(),
            tasks: self.tasks.values().map(TaskQuery::from).collect(),
            provenance: self.provenance.values().cloned().collect(),
        }
    }
}

/// Query view that excludes task instructions and runtime command lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorRunQuery {
    pub supervisor_run_id: SupervisorRunId,
    pub state_revision: u64,
    pub state: SupervisorRunState,
    pub terminal_at: Option<DateTime<Utc>>,
    pub terminal_reason: Option<String>,
    pub tasks: Vec<TaskQuery>,
    pub provenance: Vec<RunProvenance>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskQuery {
    pub task_id: TaskId,
    pub parent_task_id: Option<TaskId>,
    pub dependencies: BTreeSet<TaskId>,
    pub instruction_digest: String,
    pub required_artifact_contract: String,
    pub attempt: u64,
    pub generation: u64,
    pub assigned_dispatch_run: Option<OperationId>,
    pub state: TaskState,
}
impl From<&TaskNode> for TaskQuery {
    fn from(task: &TaskNode) -> Self {
        Self {
            task_id: task.task_id.clone(),
            parent_task_id: task.parent_task_id.clone(),
            dependencies: task.dependencies.clone(),
            instruction_digest: task.instruction_digest.clone(),
            required_artifact_contract: task.required_artifact_contract.clone(),
            attempt: task.attempt,
            generation: task.generation,
            assigned_dispatch_run: task.assigned_dispatch_run,
            state: task.state,
        }
    }
}

/// Rejection that leaves the aggregate unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    InvalidTaskId,
    DuplicateTask,
    MissingTask,
    MissingDependency(TaskId),
    SelfDependency,
    Cycle,
    ParentMismatch,
    ProvenanceMismatch,
    DependencyIncomplete,
    InvalidTransition,
    StaleGeneration,
    TerminalRun,
    SequenceGap { expected: u64, actual: u64 },
}
impl fmt::Display for SupervisorError {
    #[coverage(off)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SupervisorError {}

/// Applies one event exactly once.  A duplicate event ID is an intentional
/// no-op; a future sequence is rejected so callers cannot skip history.
///
/// # Errors
///
/// Returns a typed rejection without changing `run` when the event is stale,
/// out of sequence, invalid for its task/provenance, or mutates a terminal run.
pub fn reduce(run: &mut SupervisorRun, event: &SupervisorEvent) -> Result<(), SupervisorError> {
    if run.applied_events.contains(&event.event_id) {
        return Ok(());
    }
    let expected = run.state_revision + 1;
    if event.sequence != expected {
        return Err(SupervisorError::SequenceGap {
            expected,
            actual: event.sequence,
        });
    }
    if run.state.terminal() {
        return Err(SupervisorError::TerminalRun);
    }
    let mut next = run.clone();
    match &event.kind {
        SupervisorEventKind::AddTask { task } => add_task(&mut next, task.clone())?,
        SupervisorEventKind::Dispatch {
            task_id,
            generation,
            provenance,
        } => dispatch(&mut next, task_id, *generation, provenance.clone())?,
        SupervisorEventKind::Running {
            task_id,
            generation,
        } => set_task(&mut next, task_id, *generation, TaskState::Running)?,
        SupervisorEventKind::SetTaskState {
            task_id,
            generation,
            state,
        } => set_task(&mut next, task_id, *generation, *state)?,
        SupervisorEventKind::SetRunState {
            state,
            terminal_reason,
        } => {
            next.state = *state;
            if state.terminal() {
                next.terminal_at = Some(event.observed_at);
                next.terminal_reason.clone_from(terminal_reason);
            }
        }
    }
    next.state_revision = event.sequence;
    next.updated_at = event.observed_at;
    next.applied_events.insert(event.event_id);
    *run = next;
    Ok(())
}

fn add_task(run: &mut SupervisorRun, mut task: TaskNode) -> Result<(), SupervisorError> {
    if task.supervisor_run_id != run.supervisor_run_id {
        return Err(SupervisorError::ParentMismatch);
    }
    if task.dependencies.contains(&task.task_id) {
        return Err(SupervisorError::SelfDependency);
    }
    if run.tasks.contains_key(&task.task_id) {
        return Err(SupervisorError::DuplicateTask);
    }
    if let Some(parent) = &task.parent_task_id
        && !run.tasks.contains_key(parent)
    {
        return Err(SupervisorError::MissingTask);
    }
    for dependency in &task.dependencies {
        if !run.tasks.contains_key(dependency) {
            return Err(SupervisorError::MissingDependency(dependency.clone()));
        }
    }
    task.state = if deps_succeeded(&run.tasks, &task.dependencies) {
        TaskState::Ready
    } else {
        TaskState::Pending
    };
    run.tasks.insert(task.task_id.clone(), task);
    Ok(())
}

fn deps_succeeded(tasks: &BTreeMap<TaskId, TaskNode>, deps: &BTreeSet<TaskId>) -> bool {
    deps.iter().all(|id| {
        tasks
            .get(id)
            .is_some_and(|task| task.state == TaskState::Succeeded)
    })
}

fn dispatch(
    run: &mut SupervisorRun,
    task_id: &TaskId,
    generation: u64,
    provenance: RunProvenance,
) -> Result<(), SupervisorError> {
    let task = run.tasks.get(task_id).ok_or(SupervisorError::MissingTask)?;
    if task.generation != generation {
        return Err(SupervisorError::StaleGeneration);
    }
    if task.state != TaskState::Ready || !deps_succeeded(&run.tasks, &task.dependencies) {
        return Err(SupervisorError::DependencyIncomplete);
    }
    if provenance.supervisor_run_id != run.supervisor_run_id
        || provenance.task_id != *task_id
        || provenance.generation != generation
        || provenance.parent_task_id != task.parent_task_id
        || provenance.dispatch_run_id
            != task
                .assigned_dispatch_run
                .unwrap_or(provenance.dispatch_run_id)
    {
        return Err(SupervisorError::ProvenanceMismatch);
    }
    if let Some(parent) = &task.parent_task_id {
        let parent = run.tasks.get(parent).ok_or(SupervisorError::MissingTask)?;
        if provenance.parent_dispatch_run != parent.assigned_dispatch_run {
            return Err(SupervisorError::ProvenanceMismatch);
        }
    }
    let task = run
        .tasks
        .get_mut(task_id)
        .ok_or(SupervisorError::MissingTask)?;
    task.assigned_dispatch_run = Some(provenance.dispatch_run_id);
    task.state = TaskState::Dispatched;
    run.provenance.insert(task_id.clone(), provenance);
    Ok(())
}
fn set_task(
    run: &mut SupervisorRun,
    task_id: &TaskId,
    generation: u64,
    state: TaskState,
) -> Result<(), SupervisorError> {
    let task = run
        .tasks
        .get_mut(task_id)
        .ok_or(SupervisorError::MissingTask)?;
    if task.generation != generation {
        return Err(SupervisorError::StaleGeneration);
    }
    if task.state.terminal() {
        return Err(SupervisorError::InvalidTransition);
    }
    let valid = matches!(
        (task.state, state),
        (TaskState::Dispatched, TaskState::Running)
            | (
                TaskState::Running
                    | TaskState::AwaitingDecision
                    | TaskState::Retrying
                    | TaskState::Verifying,
                TaskState::Succeeded
                    | TaskState::Failed
                    | TaskState::Cancelled
                    | TaskState::Blocked
                    | TaskState::AwaitingDecision
                    | TaskState::Retrying
                    | TaskState::Verifying
            )
    );
    if !valid {
        return Err(SupervisorError::InvalidTransition);
    }
    task.state = state;
    if state == TaskState::Succeeded {
        project_ready(&mut run.tasks);
    }
    Ok(())
}
fn project_ready(tasks: &mut BTreeMap<TaskId, TaskNode>) {
    let ready: Vec<_> = tasks
        .iter()
        .filter(|(_, task)| {
            task.state == TaskState::Pending && deps_succeeded(tasks, &task.dependencies)
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in ready {
        if let Some(task) = tasks.get_mut(&id) {
            task.state = TaskState::Ready;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap()
    }
    fn task(run: SupervisorRunId, id: &str, deps: &[&str]) -> TaskNode {
        TaskNode {
            task_id: TaskId::new(id).unwrap(),
            supervisor_run_id: run,
            parent_task_id: None,
            dependencies: deps.iter().map(|v| TaskId::new(*v).unwrap()).collect(),
            instruction_digest: "digest".into(),
            instruction_body: "secret prompt".into(),
            required_artifact_contract: "contract".into(),
            attempt: 1,
            generation: 1,
            assigned_dispatch_run: None,
            state: TaskState::Pending,
        }
    }
    fn event(seq: u64, kind: SupervisorEventKind) -> SupervisorEvent {
        SupervisorEvent {
            sequence: seq,
            event_id: OperationId::new(),
            causation_id: None,
            correlation_id: None,
            observed_at: now(),
            payload_digest: "d".into(),
            source: SupervisorEventSource::Admission,
            kind,
        }
    }
    #[test]
    fn dag_projects_only_satisfied_tasks_and_duplicate_is_noop() {
        let mut run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "p1".into(),
            now(),
        );
        let first = task(run.supervisor_run_id, "root", &[]);
        let first_event = event(1, SupervisorEventKind::AddTask { task: first });
        reduce(&mut run, &first_event).unwrap();
        let second = task(run.supervisor_run_id, "child", &["root"]);
        reduce(
            &mut run,
            &event(2, SupervisorEventKind::AddTask { task: second }),
        )
        .unwrap();
        assert_eq!(
            run.tasks[&TaskId::new("root").unwrap()].state,
            TaskState::Ready
        );
        assert_eq!(
            run.tasks[&TaskId::new("child").unwrap()].state,
            TaskState::Pending
        );
        reduce(&mut run, &first_event).unwrap();
        assert_eq!(run.state_revision, 2);
        assert!(
            !run.query()
                .tasks
                .iter()
                .any(|task| task.instruction_digest == "secret prompt")
        );
    }
    #[test]
    fn rejects_bad_sequences_and_terminal_mutation() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        assert!(matches!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::SetRunState {
                        state: SupervisorRunState::Running,
                        terminal_reason: None
                    }
                )
            ),
            Err(SupervisorError::SequenceGap { .. })
        ));
        reduce(
            &mut run,
            &event(
                1,
                SupervisorEventKind::SetRunState {
                    state: SupervisorRunState::Cancelled,
                    terminal_reason: Some("x".into()),
                },
            ),
        )
        .unwrap();
        assert!(matches!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::SetRunState {
                        state: SupervisorRunState::Running,
                        terminal_reason: None
                    }
                )
            ),
            Err(SupervisorError::TerminalRun)
        ));
    }

    #[test]
    fn dispatch_provenance_fences_generation_and_unblocks_dependents() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let root = task(run.supervisor_run_id, "root", &[]);
        let child = task(run.supervisor_run_id, "child", &["root"]);
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: root }),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(2, SupervisorEventKind::AddTask { task: child }),
        )
        .unwrap();
        let root_id = TaskId::new("root").unwrap();
        let dispatch = OperationId::new();
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: root_id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id: dispatch,
            worker_session_id: SessionId::new(),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        reduce(
            &mut run,
            &event(
                3,
                SupervisorEventKind::Dispatch {
                    task_id: root_id.clone(),
                    generation: 1,
                    provenance,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                4,
                SupervisorEventKind::Running {
                    task_id: root_id.clone(),
                    generation: 1,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                5,
                SupervisorEventKind::SetTaskState {
                    task_id: root_id,
                    generation: 1,
                    state: TaskState::Succeeded,
                },
            ),
        )
        .unwrap();
        assert_eq!(
            run.tasks[&TaskId::new("child").unwrap()].state,
            TaskState::Ready
        );
        assert_eq!(run.provenance.len(), 1);
        let snapshot = serde_json::to_string(&run).unwrap();
        assert_eq!(
            serde_json::from_str::<SupervisorRun>(&snapshot).unwrap(),
            run
        );
    }

    #[test]
    fn rejects_dag_and_transition_errors_without_mutating_state() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let invalid = task(run.supervisor_run_id, "same", &["same"]);
        assert_eq!(
            reduce(
                &mut run,
                &event(1, SupervisorEventKind::AddTask { task: invalid })
            ),
            Err(SupervisorError::SelfDependency)
        );
        let missing = task(run.supervisor_run_id, "missing", &["gone"]);
        assert!(matches!(
            reduce(
                &mut run,
                &event(1, SupervisorEventKind::AddTask { task: missing })
            ),
            Err(SupervisorError::MissingDependency(_))
        ));
        let mut wrong_run = task(SupervisorRunId::new(), "wrong", &[]);
        wrong_run.state = TaskState::Succeeded;
        assert_eq!(
            reduce(
                &mut run,
                &event(1, SupervisorEventKind::AddTask { task: wrong_run })
            ),
            Err(SupervisorError::ParentMismatch)
        );
        let root = task(run.supervisor_run_id, "root", &[]);
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: root }),
        )
        .unwrap();
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .state = TaskState::Pending;
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::Running {
                        task_id: TaskId::new("root").unwrap(),
                        generation: 2,
                    }
                )
            ),
            Err(SupervisorError::StaleGeneration)
        );
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::Running {
                        task_id: TaskId::new("root").unwrap(),
                        generation: 1,
                    }
                )
            ),
            Err(SupervisorError::InvalidTransition)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn rejects_duplicate_parent_and_dispatch_fences() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let run_id = run.supervisor_run_id;
        let root = task(run.supervisor_run_id, "root", &[]);
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: root }),
        )
        .unwrap();
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::AddTask {
                        task: task(run_id, "root", &[])
                    }
                )
            ),
            Err(SupervisorError::DuplicateTask)
        );
        let mut orphan = task(run.supervisor_run_id, "orphan", &[]);
        orphan.parent_task_id = Some(TaskId::new("gone").unwrap());
        assert_eq!(
            reduce(
                &mut run,
                &event(2, SupervisorEventKind::AddTask { task: orphan })
            ),
            Err(SupervisorError::MissingTask)
        );
        let id = TaskId::new("root").unwrap();
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id: OperationId::new(),
            worker_session_id: SessionId::new(),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::Dispatch {
                        task_id: id.clone(),
                        generation: 2,
                        provenance: provenance.clone()
                    }
                )
            ),
            Err(SupervisorError::StaleGeneration)
        );
        let mut wrong = provenance;
        wrong.task_id = TaskId::new("other").unwrap();
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::Dispatch {
                        task_id: id,
                        generation: 1,
                        provenance: wrong
                    }
                )
            ),
            Err(SupervisorError::ProvenanceMismatch)
        );
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .state = TaskState::Pending;
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::Dispatch {
                        task_id: TaskId::new("root").unwrap(),
                        generation: 1,
                        provenance: RunProvenance {
                            supervisor_run_id: run_id,
                            task_id: TaskId::new("root").unwrap(),
                            parent_task_id: None,
                            parent_dispatch_run: None,
                            dispatch_run_id: OperationId::new(),
                            worker_session_id: SessionId::new(),
                            worker_agent_id: AgentRuntimeId::new(),
                            worker_worktree_id: WorktreeId::new(),
                            generation: 1
                        }
                    }
                )
            ),
            Err(SupervisorError::DependencyIncomplete)
        );
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .state = TaskState::Succeeded;
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::SetTaskState {
                        task_id: TaskId::new("root").unwrap(),
                        generation: 1,
                        state: TaskState::Failed
                    }
                )
            ),
            Err(SupervisorError::InvalidTransition)
        );
    }

    #[test]
    fn child_dispatch_requires_its_parent_dispatch_provenance() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let root = task(run.supervisor_run_id, "root", &[]);
        let mut child = task(run.supervisor_run_id, "child", &[]);
        child.parent_task_id = Some(TaskId::new("root").unwrap());
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: root }),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(2, SupervisorEventKind::AddTask { task: child }),
        )
        .unwrap();
        let root_id = TaskId::new("root").unwrap();
        let root_dispatch = OperationId::new();
        let root_provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: root_id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id: root_dispatch,
            worker_session_id: SessionId::new(),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        reduce(
            &mut run,
            &event(
                3,
                SupervisorEventKind::Dispatch {
                    task_id: root_id,
                    generation: 1,
                    provenance: root_provenance,
                },
            ),
        )
        .unwrap();
        let child_id = TaskId::new("child").unwrap();
        let child_provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: child_id.clone(),
            parent_task_id: Some(TaskId::new("root").unwrap()),
            parent_dispatch_run: Some(root_dispatch),
            dispatch_run_id: OperationId::new(),
            worker_session_id: SessionId::new(),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        reduce(
            &mut run,
            &event(
                4,
                SupervisorEventKind::Dispatch {
                    task_id: child_id,
                    generation: 1,
                    provenance: child_provenance,
                },
            ),
        )
        .unwrap();
    }
}
