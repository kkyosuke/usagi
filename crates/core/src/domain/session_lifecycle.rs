//! Durable, daemon-owned session lifecycle state.
//!
//! This module deliberately does not extend the legacy `WorkspaceState` record.
//! A legacy state file has neither an incarnation nor a durable-operation fence,
//! so treating it as a managed session would let a late worker mutate a guessed
//! target.  Migration must therefore be explicit (see [`WorkspaceLifecycleState`]).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::id::{CompletionFence, DaemonGeneration, OperationId, SessionId, WorkspaceId};

/// The physical availability of a managed session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycle {
    Creating,
    Initializing,
    Available,
    Deleting,
    Failed,
}

/// A runtime's activity; it is intentionally independent of [`SessionLifecycle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    Ready,
    Running,
    Waiting,
    Ended,
    Exited,
}

/// The git relationship of a worktree; it is intentionally independent of lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchStatus {
    New,
    Dirty,
    Local,
    Pushed,
    Synced,
}

/// Permission derived solely from the session lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct LifecycleCapabilities {
    pub can_use: bool,
    pub can_remove: bool,
    pub can_cancel: bool,
    pub can_recover: bool,
}

impl SessionLifecycle {
    #[must_use]
    pub const fn capabilities(self) -> LifecycleCapabilities {
        match self {
            Self::Creating | Self::Initializing => LifecycleCapabilities {
                can_use: false,
                can_remove: false,
                can_cancel: true,
                can_recover: false,
            },
            Self::Available => LifecycleCapabilities {
                can_use: true,
                can_remove: true,
                can_cancel: false,
                can_recover: false,
            },
            Self::Deleting => LifecycleCapabilities {
                can_use: false,
                can_remove: false,
                can_cancel: false,
                can_recover: false,
            },
            Self::Failed => LifecycleCapabilities {
                can_use: false,
                can_remove: true,
                can_cancel: false,
                can_recover: true,
            },
        }
    }
}

/// Immutable setup input captured before commands begin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupPlan {
    pub commands: Vec<String>,
}
/// Immutable delete targets captured before deletion begins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletePlan {
    pub targets: Vec<String>,
    pub force: bool,
}
/// A safe failure classification, not raw worker output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStage {
    Create,
    Initialize,
    Delete,
    Integrity,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub stage: FailureStage,
    pub summary: String,
}

/// One session incarnation and its lifecycle-only data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedSession {
    pub session_id: SessionId,
    pub name: String,
    pub lifecycle: SessionLifecycle,
    pub attempt: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    pub changed_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_plan: Option<SetupPlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_plan: Option<DeletePlan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
}

impl ManagedSession {
    #[must_use]
    pub fn new_creating(name: String, operation_id: OperationId, now: DateTime<Utc>) -> Self {
        Self {
            session_id: SessionId::new(),
            name,
            lifecycle: SessionLifecycle::Creating,
            attempt: 1,
            operation_id: Some(operation_id),
            changed_at: now,
            setup_plan: None,
            delete_plan: None,
            failure: None,
        }
    }
}

/// Durable operation state. Terminal states cannot be restarted under the same ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Accepted,
    Running,
    CancelRequested,
    Succeeded,
    Failed,
    Cancelled,
    Ambiguous,
}
impl OperationStatus {
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Ambiguous
        )
    }
}

/// The journal entry that makes an asynchronous effect durable and fenceable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationJournal {
    pub operation_id: OperationId,
    pub owner_daemon_generation: DaemonGeneration,
    pub status: OperationStatus,
    pub execution_attempt: u64,
    pub progress_revision: u64,
}

/// All v2 lifecycle state for a workspace. `state_revision` increases on every accepted reducer event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceLifecycleState {
    pub format: String,
    pub version: LifecycleFormatVersion,
    pub workspace_id: WorkspaceId,
    pub state_revision: u64,
    pub sessions: Vec<ManagedSession>,
    pub operations: Vec<OperationJournal>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleFormatVersion {
    pub major: u16,
    pub minor: u16,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    UnsupportedFormat,
    UnsupportedVersion,
    DuplicateSessionName,
    InvalidTransition,
    StaleCompletion,
    MissingSession,
    MissingOperation,
}
impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for LifecycleError {}

impl WorkspaceLifecycleState {
    pub const FORMAT: &'static str = "usagi-workspace-lifecycle";
    pub const MAJOR: u16 = 2;
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, now: DateTime<Utc>) -> Self {
        Self {
            format: Self::FORMAT.into(),
            version: LifecycleFormatVersion {
                major: Self::MAJOR,
                minor: 0,
            },
            workspace_id,
            state_revision: 0,
            sessions: vec![],
            operations: vec![],
            updated_at: now,
        }
    }
    /// # Errors
    ///
    /// Returns an error when the envelope is not the exact version this reducer understands.
    pub fn validate(&self) -> Result<(), LifecycleError> {
        if self.format != Self::FORMAT {
            return Err(LifecycleError::UnsupportedFormat);
        }
        if self.version.major != Self::MAJOR || self.version.minor != 0 {
            return Err(LifecycleError::UnsupportedVersion);
        }
        Ok(())
    }
    fn changed(&mut self, now: DateTime<Utc>) {
        self.state_revision += 1;
        self.updated_at = now;
    }
}

/// Input to the pure reducer. Only the daemon's command handler may persist its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    ReserveCreate {
        name: String,
        operation: OperationJournal,
    },
    CreateCompleted {
        fence: CompletionFence,
        setup_plan: Option<SetupPlan>,
    },
    BeginRemove {
        session_id: SessionId,
        operation: OperationJournal,
        delete_plan: DeletePlan,
    },
    Completed {
        fence: CompletionFence,
    },
    Failed {
        fence: CompletionFence,
        failure: Failure,
    },
    ReconcileInterrupted {
        session_id: SessionId,
        operation_id: OperationId,
        stage: FailureStage,
    },
    RequestCancel {
        operation_id: OperationId,
    },
}

/// Applies one lifecycle event. A stale completion is a no-op error, never a name lookup.
///
/// # Errors
///
/// Returns an error for an invalid transition, unsupported envelope, or stale fence.
pub fn reduce(
    state: &mut WorkspaceLifecycleState,
    event: LifecycleEvent,
    now: DateTime<Utc>,
) -> Result<(), LifecycleError> {
    state.validate()?;
    match event {
        LifecycleEvent::ReserveCreate { name, operation } => {
            if state.sessions.iter().any(|s| s.name == name) {
                return Err(LifecycleError::DuplicateSessionName);
            }
            let id = operation.operation_id;
            state
                .sessions
                .push(ManagedSession::new_creating(name, id, now));
            state.operations.push(operation);
            state.changed(now);
            Ok(())
        }
        LifecycleEvent::BeginRemove {
            session_id,
            operation,
            delete_plan,
        } => {
            let s = state
                .sessions
                .iter_mut()
                .find(|s| s.session_id == session_id)
                .ok_or(LifecycleError::MissingSession)?;
            if !matches!(
                s.lifecycle,
                SessionLifecycle::Available | SessionLifecycle::Failed
            ) {
                return Err(LifecycleError::InvalidTransition);
            }
            s.lifecycle = SessionLifecycle::Deleting;
            s.attempt += 1;
            s.operation_id = Some(operation.operation_id);
            s.delete_plan = Some(delete_plan);
            s.setup_plan = None;
            s.failure = None;
            s.changed_at = now;
            state.operations.push(operation);
            state.changed(now);
            Ok(())
        }
        LifecycleEvent::RequestCancel { operation_id } => {
            let op = state
                .operations
                .iter_mut()
                .find(|o| o.operation_id == operation_id)
                .ok_or(LifecycleError::MissingOperation)?;
            if op.status.terminal() {
                return Err(LifecycleError::InvalidTransition);
            }
            op.status = OperationStatus::CancelRequested;
            op.progress_revision += 1;
            state.changed(now);
            Ok(())
        }
        LifecycleEvent::CreateCompleted { fence, setup_plan } => {
            create_completed(state, &fence, setup_plan, now)
        }
        LifecycleEvent::Completed { fence } => complete(state, &fence, now, |s| {
            if s.lifecycle != SessionLifecycle::Deleting {
                return Err(LifecycleError::InvalidTransition);
            }
            Ok(true)
        }),
        LifecycleEvent::Failed { fence, failure } => complete(state, &fence, now, |s| {
            s.lifecycle = SessionLifecycle::Failed;
            s.failure = Some(failure);
            Ok(false)
        }),
        LifecycleEvent::ReconcileInterrupted {
            session_id,
            operation_id,
            stage,
        } => {
            let s = state
                .sessions
                .iter_mut()
                .find(|s| s.session_id == session_id && s.operation_id == Some(operation_id))
                .ok_or(LifecycleError::StaleCompletion)?;
            s.lifecycle = SessionLifecycle::Failed;
            s.failure = Some(Failure {
                stage,
                summary: "interrupted; explicit recovery required".into(),
            });
            s.changed_at = now;
            state.changed(now);
            Ok(())
        }
    }
}

fn create_completed(
    state: &mut WorkspaceLifecycleState,
    fence: &CompletionFence,
    setup_plan: Option<SetupPlan>,
    now: DateTime<Utc>,
) -> Result<(), LifecycleError> {
    let (pos, operation_pos) = fenced_session(state, fence)?;
    if state.sessions[pos].lifecycle != SessionLifecycle::Creating {
        return Err(LifecycleError::InvalidTransition);
    }
    state.sessions[pos].lifecycle = if setup_plan.is_some() {
        SessionLifecycle::Initializing
    } else {
        SessionLifecycle::Available
    };
    state.sessions[pos].setup_plan = setup_plan;
    state.sessions[pos].changed_at = now;
    let op = &mut state.operations[operation_pos];
    op.progress_revision += 1;
    if state.sessions[pos].lifecycle == SessionLifecycle::Available {
        op.status = OperationStatus::Succeeded;
        state.sessions[pos].operation_id = None;
    }
    state.changed(now);
    Ok(())
}

fn fenced_session(
    state: &WorkspaceLifecycleState,
    fence: &CompletionFence,
) -> Result<(usize, usize), LifecycleError> {
    if fence.workspace_id != state.workspace_id || fence.expected_revision != state.state_revision {
        return Err(LifecycleError::StaleCompletion);
    }
    let operation_pos = state
        .operations
        .iter()
        .position(|o| o.operation_id == fence.operation_id)
        .ok_or(LifecycleError::StaleCompletion)?;
    let op = &state.operations[operation_pos];
    if op.owner_daemon_generation != fence.owner_daemon_generation
        || op.execution_attempt != fence.execution_attempt
        || op.status.terminal()
    {
        return Err(LifecycleError::StaleCompletion);
    }
    let session_pos = state
        .sessions
        .iter()
        .position(|s| {
            s.session_id == fence.session_id
                && s.attempt == fence.lifecycle_attempt
                && s.operation_id == Some(fence.operation_id)
        })
        .ok_or(LifecycleError::StaleCompletion)?;
    Ok((session_pos, operation_pos))
}

fn complete<F>(
    state: &mut WorkspaceLifecycleState,
    fence: &CompletionFence,
    now: DateTime<Utc>,
    mutate: F,
) -> Result<(), LifecycleError>
where
    F: FnOnce(&mut ManagedSession) -> Result<bool, LifecycleError>,
{
    let (pos, operation_pos) = fenced_session(state, fence)?;
    let op = &mut state.operations[operation_pos];
    let remove = mutate(&mut state.sessions[pos])?;
    op.status = OperationStatus::Succeeded;
    op.progress_revision += 1;
    if remove {
        state.sessions.remove(pos);
    } else {
        let s = &mut state.sessions[pos];
        s.operation_id = None;
        s.changed_at = now;
    }
    state.changed(now);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap()
    }
    fn op() -> OperationJournal {
        OperationJournal {
            operation_id: OperationId::new(),
            owner_daemon_generation: DaemonGeneration::new(),
            status: OperationStatus::Running,
            execution_attempt: 1,
            progress_revision: 0,
        }
    }
    fn fence(
        s: &WorkspaceLifecycleState,
        session: &ManagedSession,
        op: &OperationJournal,
    ) -> CompletionFence {
        CompletionFence {
            workspace_id: s.workspace_id,
            session_id: session.session_id,
            operation_id: op.operation_id,
            owner_daemon_generation: op.owner_daemon_generation,
            execution_attempt: 1,
            lifecycle_attempt: session.attempt,
            expected_revision: s.state_revision,
        }
    }
    #[test]
    fn lifecycle_axes_and_capabilities_stay_separate() {
        assert!(SessionLifecycle::Available.capabilities().can_use);
        assert_eq!(AgentPhase::Running, AgentPhase::Running);
        assert_eq!(BranchStatus::Dirty, BranchStatus::Dirty);
    }
    #[test]
    fn creation_completion_and_reverse_snapshot_are_fenced() {
        let mut state = WorkspaceLifecycleState::new(WorkspaceId::new(), now());
        let operation = op();
        reduce(
            &mut state,
            LifecycleEvent::ReserveCreate {
                name: "a".into(),
                operation: operation.clone(),
            },
            now(),
        )
        .unwrap();
        let session = state.sessions[0].clone();
        let valid = fence(&state, &session, &operation);
        reduce(
            &mut state,
            LifecycleEvent::CreateCompleted {
                fence: valid.clone(),
                setup_plan: None,
            },
            now(),
        )
        .unwrap();
        assert_eq!(state.sessions[0].lifecycle, SessionLifecycle::Available);
        assert_eq!(
            reduce(
                &mut state,
                LifecycleEvent::CreateCompleted {
                    fence: valid,
                    setup_plan: None
                },
                now()
            ),
            Err(LifecycleError::StaleCompletion)
        );
    }
    #[test]
    fn delete_recreation_rejects_old_worker() {
        let mut state = WorkspaceLifecycleState::new(WorkspaceId::new(), now());
        let create = op();
        reduce(
            &mut state,
            LifecycleEvent::ReserveCreate {
                name: "x".into(),
                operation: create.clone(),
            },
            now(),
        )
        .unwrap();
        let old = state.sessions[0].clone();
        let cf = fence(&state, &old, &create);
        reduce(
            &mut state,
            LifecycleEvent::CreateCompleted {
                fence: cf,
                setup_plan: None,
            },
            now(),
        )
        .unwrap();
        let del = op();
        reduce(
            &mut state,
            LifecycleEvent::BeginRemove {
                session_id: old.session_id,
                operation: del.clone(),
                delete_plan: DeletePlan {
                    targets: vec!["x".into()],
                    force: false,
                },
            },
            now(),
        )
        .unwrap();
        let df = fence(&state, &state.sessions[0].clone(), &del);
        reduce(&mut state, LifecycleEvent::Completed { fence: df }, now()).unwrap();
        let fresh = op();
        reduce(
            &mut state,
            LifecycleEvent::ReserveCreate {
                name: "x".into(),
                operation: fresh,
            },
            now(),
        )
        .unwrap();
        let old_fence = fence(&state, &old, &create);
        assert_eq!(
            reduce(
                &mut state,
                LifecycleEvent::Failed {
                    fence: old_fence,
                    failure: Failure {
                        stage: FailureStage::Create,
                        summary: "late".into()
                    }
                },
                now()
            ),
            Err(LifecycleError::StaleCompletion)
        );
    }
    #[test]
    fn crash_does_not_replay_setup_and_unknown_format_fails_closed() {
        let mut state = WorkspaceLifecycleState::new(WorkspaceId::new(), now());
        let operation = op();
        reduce(
            &mut state,
            LifecycleEvent::ReserveCreate {
                name: "a".into(),
                operation: operation.clone(),
            },
            now(),
        )
        .unwrap();
        let f = fence(&state, &state.sessions[0].clone(), &operation);
        reduce(
            &mut state,
            LifecycleEvent::CreateCompleted {
                fence: f,
                setup_plan: Some(SetupPlan {
                    commands: vec!["non-idempotent".into()],
                }),
            },
            now(),
        )
        .unwrap();
        let id = state.sessions[0].session_id;
        reduce(
            &mut state,
            LifecycleEvent::ReconcileInterrupted {
                session_id: id,
                operation_id: operation.operation_id,
                stage: FailureStage::Initialize,
            },
            now(),
        )
        .unwrap();
        assert_eq!(state.sessions[0].lifecycle, SessionLifecycle::Failed);
        state.format = "legacy".into();
        assert_eq!(state.validate(), Err(LifecycleError::UnsupportedFormat));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn reducer_covers_recovery_cancel_and_transition_failures() {
        let mut state = WorkspaceLifecycleState::new(WorkspaceId::new(), now());
        assert!(SessionLifecycle::Creating.capabilities().can_cancel);
        assert!(SessionLifecycle::Initializing.capabilities().can_cancel);
        assert!(!SessionLifecycle::Deleting.capabilities().can_use);
        assert!(SessionLifecycle::Failed.capabilities().can_recover);
        assert_eq!(
            format!("{}", LifecycleError::MissingOperation),
            "MissingOperation"
        );
        state.version.minor = 1;
        assert_eq!(state.validate(), Err(LifecycleError::UnsupportedVersion));
        assert_eq!(
            reduce(
                &mut state,
                LifecycleEvent::RequestCancel {
                    operation_id: OperationId::new(),
                },
                now(),
            ),
            Err(LifecycleError::UnsupportedVersion)
        );
        state.version.minor = 0;
        let operation = op();
        reduce(
            &mut state,
            LifecycleEvent::ReserveCreate {
                name: "a".into(),
                operation: operation.clone(),
            },
            now(),
        )
        .unwrap();
        assert_eq!(
            reduce(
                &mut state,
                LifecycleEvent::ReserveCreate {
                    name: "a".into(),
                    operation: op()
                },
                now()
            ),
            Err(LifecycleError::DuplicateSessionName)
        );
        assert_eq!(
            reduce(
                &mut state,
                LifecycleEvent::RequestCancel {
                    operation_id: OperationId::new()
                },
                now()
            ),
            Err(LifecycleError::MissingOperation)
        );
        reduce(
            &mut state,
            LifecycleEvent::RequestCancel {
                operation_id: operation.operation_id,
            },
            now(),
        )
        .unwrap();
        assert_eq!(state.operations[0].status, OperationStatus::CancelRequested);
        state.operations[0].status = OperationStatus::Succeeded;
        assert_eq!(
            reduce(
                &mut state,
                LifecycleEvent::RequestCancel {
                    operation_id: operation.operation_id
                },
                now()
            ),
            Err(LifecycleError::InvalidTransition)
        );
        let id = state.sessions[0].session_id;
        assert_eq!(
            reduce(
                &mut state,
                LifecycleEvent::BeginRemove {
                    session_id: id,
                    operation: op(),
                    delete_plan: DeletePlan {
                        targets: vec![],
                        force: false
                    }
                },
                now()
            ),
            Err(LifecycleError::InvalidTransition)
        );
        assert_eq!(
            reduce(
                &mut state,
                LifecycleEvent::BeginRemove {
                    session_id: SessionId::new(),
                    operation: op(),
                    delete_plan: DeletePlan {
                        targets: vec![],
                        force: false
                    }
                },
                now()
            ),
            Err(LifecycleError::MissingSession)
        );
        assert_eq!(
            reduce(
                &mut state,
                LifecycleEvent::ReconcileInterrupted {
                    session_id: SessionId::new(),
                    operation_id: OperationId::new(),
                    stage: FailureStage::Create,
                },
                now(),
            ),
            Err(LifecycleError::StaleCompletion)
        );

        let mut available = WorkspaceLifecycleState::new(WorkspaceId::new(), now());
        let create = op();
        reduce(
            &mut available,
            LifecycleEvent::ReserveCreate {
                name: "b".into(),
                operation: create.clone(),
            },
            now(),
        )
        .unwrap();
        let cf = fence(&available, &available.sessions[0].clone(), &create);
        reduce(
            &mut available,
            LifecycleEvent::CreateCompleted {
                fence: cf,
                setup_plan: None,
            },
            now(),
        )
        .unwrap();
        let stale_create = available.sessions[0].clone();
        let create_again = OperationJournal {
            operation_id: OperationId::new(),
            owner_daemon_generation: DaemonGeneration::new(),
            status: OperationStatus::Running,
            execution_attempt: 1,
            progress_revision: 0,
        };
        available.sessions[0].operation_id = Some(create_again.operation_id);
        available.operations.push(create_again.clone());
        let invalid_create = fence(&available, &stale_create, &create_again);
        assert_eq!(
            reduce(
                &mut available,
                LifecycleEvent::CreateCompleted {
                    fence: invalid_create,
                    setup_plan: None
                },
                now()
            ),
            Err(LifecycleError::InvalidTransition)
        );
        available.sessions[0].operation_id = None;
        available.operations.pop();
        let remove = op();
        let sid = available.sessions[0].session_id;
        reduce(
            &mut available,
            LifecycleEvent::BeginRemove {
                session_id: sid,
                operation: remove.clone(),
                delete_plan: DeletePlan {
                    targets: vec![],
                    force: true,
                },
            },
            now(),
        )
        .unwrap();
        let bad = fence(&available, &available.sessions[0].clone(), &remove);
        let mut bad_session = available.sessions[0].clone();
        bad_session.lifecycle = SessionLifecycle::Available;
        available.sessions[0] = bad_session;
        assert_eq!(
            reduce(
                &mut available,
                LifecycleEvent::Completed { fence: bad },
                now()
            ),
            Err(LifecycleError::InvalidTransition)
        );
        available.sessions[0].lifecycle = SessionLifecycle::Deleting;
        let ff = fence(&available, &available.sessions[0].clone(), &remove);
        reduce(
            &mut available,
            LifecycleEvent::Failed {
                fence: ff,
                failure: Failure {
                    stage: FailureStage::Delete,
                    summary: "failed".into(),
                },
            },
            now(),
        )
        .unwrap();
        assert_eq!(available.sessions[0].lifecycle, SessionLifecycle::Failed);
    }
}
