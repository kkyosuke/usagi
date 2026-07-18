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
    /// The deterministic retry deadline.  It is part of the aggregate rather
    /// than scheduler memory so a restart cannot make a retry run early.
    pub retry_at: Option<DateTime<Utc>>,
    /// A worker report is not evidence.  Tasks with a non-`none` contract are
    /// held in `Verifying` until an independently recorded result is accepted.
    pub verification_digest: Option<String>,
    pub state: TaskState,
}

/// Immutable limits copied into every supervisor run at creation time.
/// Workspace configuration is deliberately represented by this one value;
/// callers do not get per-request limit overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPolicy {
    pub max_dispatches: u64,
    pub max_concurrency: usize,
    pub max_depth: usize,
    pub max_attempts: u64,
    pub retry_backoff_seconds: i64,
}
impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            max_dispatches: 16,
            max_concurrency: 4,
            max_depth: 8,
            // The workspace default is deliberately fail-closed: retry is
            // enabled only by an explicit immutable run snapshot.
            max_attempts: 1,
            retry_backoff_seconds: 30,
        }
    }
}

/// Durable, redaction-safe record which prevents autonomous progress until a
/// separate authorized-decision feature resolves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationRecord {
    pub reason: String,
    pub blocking_task_id: Option<TaskId>,
    pub safe_evidence: String,
    pub choices: Vec<String>,
    pub created_at: DateTime<Utc>,
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
    /// Makes a retry eligible only at its persisted deadline.
    RetryReady {
        task_id: TaskId,
        generation: u64,
    },
    /// Records an independent verification result.  A worker completion
    /// cannot produce this event by itself.
    VerificationResult {
        task_id: TaskId,
        generation: u64,
        passed: bool,
        result_digest: String,
    },
    /// Cancelling is a reducer fact so late dispatch completion cannot revive
    /// the task or run.
    Cancel {
        task_id: Option<TaskId>,
        reason: String,
    },
    Escalate {
        task_id: Option<TaskId>,
        reason: String,
        safe_evidence: String,
        choices: Vec<String>,
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
    pub policy: ExecutionPolicy,
    /// Dispatch reservations are committed by the same reducer event as the
    /// dispatch transition.  They make duplicate/replayed admission harmless.
    pub dispatch_reservations: BTreeSet<OperationId>,
    pub escalation: Option<EscalationRecord>,
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
            policy: ExecutionPolicy::default(),
            dispatch_reservations: BTreeSet::new(),
            escalation: None,
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

    #[must_use]
    #[coverage(off)]
    pub fn with_policy(mut self, policy: ExecutionPolicy) -> Self {
        self.policy = policy;
        self
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
    PolicyDenied(String),
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
        } => dispatch(
            &mut next,
            task_id,
            *generation,
            provenance.clone(),
            event.observed_at,
        )?,
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
        SupervisorEventKind::RetryReady {
            task_id,
            generation,
        } => retry_ready(&mut next, task_id, *generation, event.observed_at)?,
        SupervisorEventKind::VerificationResult {
            task_id,
            generation,
            passed,
            result_digest,
        } => {
            verification_result(
                &mut next,
                task_id,
                *generation,
                *passed,
                result_digest,
                event,
            )?;
        }
        SupervisorEventKind::Cancel { task_id, reason } => {
            cancel(&mut next, task_id.as_ref(), reason, event.observed_at)?;
        }
        SupervisorEventKind::Escalate {
            task_id,
            reason,
            safe_evidence,
            choices,
        } => escalate(
            &mut next,
            task_id.clone(),
            reason.clone(),
            safe_evidence.clone(),
            choices.clone(),
            event.observed_at,
        ),
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
    now: DateTime<Utc>,
) -> Result<(), SupervisorError> {
    if let Err(SupervisorError::PolicyDenied(reason)) = admit_dispatch(run, task_id, &provenance) {
        escalate(
            run,
            Some(task_id.clone()),
            reason,
            "policy limits are evaluated from the durable run snapshot".into(),
            vec!["resume".into(), "cancel".into()],
            now,
        );
        return Ok(());
    }
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
    run.dispatch_reservations
        .insert(task.assigned_dispatch_run.expect("assigned above"));
    Ok(())
}
fn admit_dispatch(
    run: &SupervisorRun,
    task_id: &TaskId,
    provenance: &RunProvenance,
) -> Result<(), SupervisorError> {
    if run.escalation.is_some() || run.state == SupervisorRunState::WaitingForDecision {
        return Err(SupervisorError::PolicyDenied(
            "human decision required".into(),
        ));
    }
    if run.dispatch_reservations.len() as u64 >= run.policy.max_dispatches
        && !run
            .dispatch_reservations
            .contains(&provenance.dispatch_run_id)
    {
        return Err(SupervisorError::PolicyDenied(
            "dispatch budget exhausted".into(),
        ));
    }
    let active = run
        .tasks
        .values()
        .filter(|task| matches!(task.state, TaskState::Dispatched | TaskState::Running))
        .count();
    if active >= run.policy.max_concurrency {
        return Err(SupervisorError::PolicyDenied(
            "concurrency limit reached".into(),
        ));
    }
    let mut depth = 0;
    let mut parent = run
        .tasks
        .get(task_id)
        .and_then(|task| task.parent_task_id.clone());
    while let Some(id) = parent {
        depth += 1;
        parent = run
            .tasks
            .get(&id)
            .and_then(|task| task.parent_task_id.clone());
    }
    if depth > run.policy.max_depth {
        return Err(SupervisorError::PolicyDenied(
            "maximum task depth exceeded".into(),
        ));
    }
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
    if state == TaskState::Failed && task.attempt < run.policy.max_attempts {
        task.attempt += 1;
        task.generation += 1;
        let delay = run
            .policy
            .retry_backoff_seconds
            .saturating_mul(1_i64 << (task.attempt - 2).min(30));
        task.retry_at = Some(run.updated_at + chrono::Duration::seconds(delay));
        task.assigned_dispatch_run = None;
        task.state = TaskState::Retrying;
        return Ok(());
    }
    if state == TaskState::Succeeded && task.required_artifact_contract != "none" {
        task.state = TaskState::Verifying;
        return Ok(());
    }
    task.state = state;
    if state == TaskState::Succeeded {
        project_ready(&mut run.tasks);
    }
    Ok(())
}
fn retry_ready(
    run: &mut SupervisorRun,
    task_id: &TaskId,
    generation: u64,
    now: DateTime<Utc>,
) -> Result<(), SupervisorError> {
    let task = run
        .tasks
        .get_mut(task_id)
        .ok_or(SupervisorError::MissingTask)?;
    if task.generation != generation {
        return Err(SupervisorError::StaleGeneration);
    }
    if task.state != TaskState::Retrying || task.retry_at.is_none_or(|deadline| deadline > now) {
        return Err(SupervisorError::InvalidTransition);
    }
    task.retry_at = None;
    task.state = TaskState::Ready;
    Ok(())
}
fn verification_result(
    run: &mut SupervisorRun,
    task_id: &TaskId,
    generation: u64,
    passed: bool,
    digest: &str,
    event: &SupervisorEvent,
) -> Result<(), SupervisorError> {
    let task = run
        .tasks
        .get_mut(task_id)
        .ok_or(SupervisorError::MissingTask)?;
    if task.generation != generation {
        return Err(SupervisorError::StaleGeneration);
    }
    if task.state != TaskState::Verifying {
        return Err(SupervisorError::InvalidTransition);
    }
    task.verification_digest = Some(digest.into());
    if passed {
        task.state = TaskState::Succeeded;
        project_ready(&mut run.tasks);
    } else {
        escalate(
            run,
            Some(task_id.clone()),
            "artifact verification failed".into(),
            digest.into(),
            vec!["resume".into(), "cancel".into()],
            event.observed_at,
        );
    }
    Ok(())
}
fn cancel(
    run: &mut SupervisorRun,
    task_id: Option<&TaskId>,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<(), SupervisorError> {
    if let Some(id) = task_id {
        let task = run.tasks.get_mut(id).ok_or(SupervisorError::MissingTask)?;
        if !task.state.terminal() {
            task.state = TaskState::Cancelled;
        }
    } else {
        for task in run.tasks.values_mut().filter(|task| !task.state.terminal()) {
            task.state = TaskState::Cancelled;
        }
        run.state = SupervisorRunState::Cancelled;
        run.terminal_at = Some(now);
        run.terminal_reason = Some(reason.into());
    }
    Ok(())
}
fn escalate(
    run: &mut SupervisorRun,
    task_id: Option<TaskId>,
    reason: String,
    safe_evidence: String,
    choices: Vec<String>,
    now: DateTime<Utc>,
) {
    run.escalation = Some(EscalationRecord {
        reason: reason.clone(),
        blocking_task_id: task_id,
        safe_evidence,
        choices,
        created_at: now,
    });
    run.state = SupervisorRunState::Escalated;
    run.terminal_at = Some(now);
    run.terminal_reason = Some(reason);
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
            required_artifact_contract: "none".into(),
            attempt: 1,
            generation: 1,
            assigned_dispatch_run: None,
            retry_at: None,
            verification_digest: None,
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
    fn policy_reserves_dispatch_once_and_escalates_before_an_over_limit_effect() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now())
            .with_policy(ExecutionPolicy {
                max_dispatches: 1,
                max_concurrency: 1,
                max_depth: 0,
                max_attempts: 1,
                retry_backoff_seconds: 1,
            });
        let first = task(run.supervisor_run_id, "first", &[]);
        let second = task(run.supervisor_run_id, "second", &[]);
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: first }),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(2, SupervisorEventKind::AddTask { task: second }),
        )
        .unwrap();
        let id = TaskId::new("first").unwrap();
        let dispatch = OperationId::new();
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: id.clone(),
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
                    task_id: id.clone(),
                    generation: 1,
                    provenance,
                },
            ),
        )
        .unwrap();
        assert_eq!(run.dispatch_reservations.len(), 1);
        let second_id = TaskId::new("second").unwrap();
        let second_provenance = RunProvenance {
            task_id: second_id.clone(),
            dispatch_run_id: OperationId::new(),
            generation: 1,
            ..run.provenance[&TaskId::new("first").unwrap()].clone()
        };
        reduce(
            &mut run,
            &event(
                4,
                SupervisorEventKind::Dispatch {
                    task_id: second_id,
                    generation: 1,
                    provenance: second_provenance,
                },
            ),
        )
        .unwrap();
        assert_eq!(run.state, SupervisorRunState::Escalated);
        assert_eq!(
            run.escalation.as_ref().unwrap().reason,
            "dispatch budget exhausted"
        );
    }

    #[test]
    fn verification_and_retry_are_durable_gates() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now())
            .with_policy(ExecutionPolicy {
                max_dispatches: 3,
                max_concurrency: 1,
                max_depth: 1,
                max_attempts: 2,
                retry_backoff_seconds: 30,
            });
        let mut artifact = task(run.supervisor_run_id, "artifact", &[]);
        artifact.required_artifact_contract = "commit-fence".into();
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: artifact }),
        )
        .unwrap();
        let id = TaskId::new("artifact").unwrap();
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
        reduce(
            &mut run,
            &event(
                2,
                SupervisorEventKind::Dispatch {
                    task_id: id.clone(),
                    generation: 1,
                    provenance,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                3,
                SupervisorEventKind::Running {
                    task_id: id.clone(),
                    generation: 1,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                4,
                SupervisorEventKind::SetTaskState {
                    task_id: id.clone(),
                    generation: 1,
                    state: TaskState::Succeeded,
                },
            ),
        )
        .unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Verifying);
        reduce(
            &mut run,
            &event(
                5,
                SupervisorEventKind::VerificationResult {
                    task_id: id.clone(),
                    generation: 1,
                    passed: true,
                    result_digest: "verified".into(),
                },
            ),
        )
        .unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Succeeded);
    }

    #[test]
    fn retry_cancel_and_failed_verification_cannot_resume_work() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now())
            .with_policy(ExecutionPolicy {
                max_dispatches: 3,
                max_concurrency: 1,
                max_depth: 1,
                max_attempts: 2,
                retry_backoff_seconds: 30,
            });
        let mut retry_task = task(run.supervisor_run_id, "retry", &[]);
        retry_task.required_artifact_contract = "commit-fence".into();
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: retry_task }),
        )
        .unwrap();
        let id = TaskId::new("retry").unwrap();
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
        reduce(
            &mut run,
            &event(
                2,
                SupervisorEventKind::Dispatch {
                    task_id: id.clone(),
                    generation: 1,
                    provenance,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                3,
                SupervisorEventKind::Running {
                    task_id: id.clone(),
                    generation: 1,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                4,
                SupervisorEventKind::SetTaskState {
                    task_id: id.clone(),
                    generation: 1,
                    state: TaskState::Failed,
                },
            ),
        )
        .unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Retrying);
        let generation = run.tasks[&id].generation;
        assert!(matches!(
            reduce(
                &mut run,
                &event(
                    5,
                    SupervisorEventKind::RetryReady {
                        task_id: id.clone(),
                        generation,
                    },
                )
            ),
            Err(SupervisorError::InvalidTransition)
        ));
        let mut due = event(
            5,
            SupervisorEventKind::RetryReady {
                task_id: id.clone(),
                generation,
            },
        );
        due.observed_at += chrono::Duration::seconds(30);
        reduce(&mut run, &due).unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Ready);
    }

    #[test]
    fn cancellation_converges_tasks_and_run_to_terminal_state() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let mut task = task(run.supervisor_run_id, "cancel", &[]);
        task.state = TaskState::Ready;
        let id = task.task_id.clone();
        run.tasks.insert(id.clone(), task);
        reduce(
            &mut run,
            &event(
                1,
                SupervisorEventKind::Cancel {
                    task_id: Some(id.clone()),
                    reason: "task cancelled".into(),
                },
            ),
        )
        .unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Cancelled);
        reduce(
            &mut run,
            &event(
                2,
                SupervisorEventKind::Cancel {
                    task_id: None,
                    reason: "run cancelled".into(),
                },
            ),
        )
        .unwrap();
        assert_eq!(run.state, SupervisorRunState::Cancelled);
        assert!(run.terminal_at.is_some());
    }

    #[test]
    fn failed_verification_escalates_and_records_safe_evidence() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let mut task = task(run.supervisor_run_id, "verify", &[]);
        task.state = TaskState::Verifying;
        run.tasks.insert(task.task_id.clone(), task);
        let id = TaskId::new("verify").unwrap();
        reduce(
            &mut run,
            &event(
                1,
                SupervisorEventKind::VerificationResult {
                    task_id: id,
                    generation: 1,
                    passed: false,
                    result_digest: "mismatch".into(),
                },
            ),
        )
        .unwrap();
        assert_eq!(run.state, SupervisorRunState::Escalated);
        assert_eq!(run.escalation.as_ref().unwrap().safe_evidence, "mismatch");
    }

    #[test]
    fn policy_and_reducer_error_edges_are_explicit() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let id = TaskId::new("task").unwrap();
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
        assert!(matches!(
            dispatch(&mut run, &id, 1, provenance.clone(), now()),
            Err(SupervisorError::MissingTask)
        ));
        run.state = SupervisorRunState::WaitingForDecision;
        assert!(matches!(
            admit_dispatch(&run, &id, &provenance),
            Err(SupervisorError::PolicyDenied(reason)) if reason == "human decision required"
        ));
        run.state = SupervisorRunState::Planning;
        let mut dispatched_task = task(run.supervisor_run_id, "task", &[]);
        dispatched_task.state = TaskState::Dispatched;
        run.tasks.insert(id.clone(), dispatched_task);
        run.policy.max_concurrency = 1;
        assert!(matches!(
            admit_dispatch(&run, &id, &provenance),
            Err(SupervisorError::PolicyDenied(reason)) if reason == "concurrency limit reached"
        ));
        run.policy.max_concurrency = 2;
        let parent = TaskId::new("parent").unwrap();
        let parent_task = task(run.supervisor_run_id, "parent", &[]);
        run.tasks.insert(parent.clone(), parent_task);
        run.tasks.get_mut(&id).unwrap().parent_task_id = Some(parent);
        run.policy.max_depth = 0;
        assert!(matches!(
            admit_dispatch(&run, &id, &provenance),
            Err(SupervisorError::PolicyDenied(reason)) if reason == "maximum task depth exceeded"
        ));
        assert_eq!(
            retry_ready(&mut run, &id, 2, now()),
            Err(SupervisorError::StaleGeneration)
        );
        assert_eq!(
            verification_result(
                &mut run,
                &id,
                1,
                true,
                "digest",
                &event(
                    1,
                    SupervisorEventKind::SetRunState {
                        state: SupervisorRunState::Running,
                        terminal_reason: None
                    }
                )
            ),
            Err(SupervisorError::InvalidTransition)
        );
        assert_eq!(
            verification_result(
                &mut run,
                &id,
                2,
                true,
                "digest",
                &event(
                    1,
                    SupervisorEventKind::SetRunState {
                        state: SupervisorRunState::Running,
                        terminal_reason: None,
                    },
                ),
            ),
            Err(SupervisorError::StaleGeneration)
        );
        run.tasks.get_mut(&id).unwrap().state = TaskState::Ready;
        cancel(&mut run, None, "cancel", now()).unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Cancelled);
    }

    #[test]
    fn explicit_escalation_event_is_durable() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        reduce(
            &mut run,
            &event(
                1,
                SupervisorEventKind::Escalate {
                    task_id: None,
                    reason: "ambiguous provenance".into(),
                    safe_evidence: "fence mismatch".into(),
                    choices: vec!["cancel".into()],
                },
            ),
        )
        .unwrap();
        assert_eq!(run.state, SupervisorRunState::Escalated);
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
        let mut mismatched_parent = child_provenance.clone();
        mismatched_parent.parent_dispatch_run = Some(OperationId::new());
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    4,
                    SupervisorEventKind::Dispatch {
                        task_id: child_id.clone(),
                        generation: 1,
                        provenance: mismatched_parent,
                    },
                ),
            ),
            Err(SupervisorError::ProvenanceMismatch)
        );
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
