//! Durable, daemon-owned session lifecycle state.
//!
//! This module deliberately does not extend the legacy `WorkspaceState` record.
//! A legacy state file has neither an incarnation nor a durable-operation fence,
//! so treating it as a managed session would let a late worker mutate a guessed
//! target.  Migration must therefore be explicit (see [`WorkspaceLifecycleState`]).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::id::{
    CompletionFence, DaemonGeneration, OperationId, SessionId, WorkspaceId, WorktreeId,
};

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
    /// Physical checkout incarnation.  It is minted with the session reservation
    /// and survives daemon restart; a display name is never used as its key.
    pub worktree_id: WorktreeId,
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
            worktree_id: WorktreeId::new(),
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

    /// Adopts a checkout that predates daemon ownership without performing a
    /// worktree effect.  Its identity is minted exactly once when the daemon
    /// lifecycle store is initialized.
    #[must_use]
    pub fn adopt_available(name: String, created_at: DateTime<Utc>) -> Self {
        Self {
            session_id: SessionId::new(),
            worktree_id: WorktreeId::new(),
            name,
            lifecycle: SessionLifecycle::Available,
            attempt: 1,
            operation_id: None,
            changed_at: created_at,
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
    /// Canonical action and target captured at admission for durable
    /// operation-id idempotency across connection and daemon restart.
    #[serde(default)]
    pub semantic_key: String,
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
    /// Repairs snapshots written by the legacy reducer that marked a failed
    /// session's operation as succeeded and detached the operation identity.
    ///
    /// The repair is deliberately fail-closed: the failed session remains
    /// unavailable, and only the operation whose canonical action/target
    /// matches the recorded failure is rewritten as a terminal failure.
    #[must_use]
    pub fn repair_legacy_failed_outcomes(&mut self, now: DateTime<Utc>) -> usize {
        let mut repaired = 0;
        for session_pos in 0..self.sessions.len() {
            let session = &self.sessions[session_pos];
            if session.lifecycle != SessionLifecycle::Failed {
                continue;
            }
            let Some(failure) = &session.failure else {
                continue;
            };
            let action = if failure.stage == FailureStage::Delete {
                "remove"
            } else {
                "create"
            };
            let semantic_key = format!("{action}:{}", session.name);
            let operation_pos = session
                .operation_id
                .and_then(|operation_id| {
                    self.operations.iter().position(|operation| {
                        operation.operation_id == operation_id
                            && operation.semantic_key == semantic_key
                            && operation.status == OperationStatus::Succeeded
                    })
                })
                .or_else(|| {
                    self.operations.iter().rposition(|operation| {
                        operation.semantic_key == semantic_key
                            && operation.status == OperationStatus::Succeeded
                    })
                });
            let Some(operation_pos) = operation_pos else {
                continue;
            };
            let operation = &mut self.operations[operation_pos];
            if operation.status == OperationStatus::Succeeded {
                operation.status = OperationStatus::Failed;
                operation.progress_revision += 1;
                self.sessions[session_pos].operation_id = Some(operation.operation_id);
                repaired += 1;
            }
        }
        if repaired != 0 {
            self.changed(now);
        }
        repaired
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
        LifecycleEvent::Completed { fence } => complete(state, &fence, now),
        LifecycleEvent::Failed { fence, failure } => fail(state, &fence, failure, now),
        LifecycleEvent::ReconcileInterrupted {
            session_id,
            operation_id,
            stage,
        } => {
            let operation_pos = state
                .operations
                .iter()
                .position(|operation| operation.operation_id == operation_id);
            if operation_pos.is_some_and(|position| state.operations[position].status.terminal()) {
                return Err(LifecycleError::StaleCompletion);
            }
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
            if let Some(operation_pos) = operation_pos {
                let operation = &mut state.operations[operation_pos];
                operation.status = OperationStatus::Failed;
                operation.progress_revision += 1;
            }
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
            // Managed-session completions always carry a session fence; a
            // workspace-root operation never reaches this session reducer.
            fence.session_id == Some(s.session_id)
                && s.attempt == fence.lifecycle_attempt
                && s.operation_id == Some(fence.operation_id)
        })
        .ok_or(LifecycleError::StaleCompletion)?;
    Ok((session_pos, operation_pos))
}

fn complete(
    state: &mut WorkspaceLifecycleState,
    fence: &CompletionFence,
    now: DateTime<Utc>,
) -> Result<(), LifecycleError> {
    let (pos, operation_pos) = fenced_session(state, fence)?;
    if state.sessions[pos].lifecycle != SessionLifecycle::Deleting {
        return Err(LifecycleError::InvalidTransition);
    }
    let op = &mut state.operations[operation_pos];
    op.status = OperationStatus::Succeeded;
    op.progress_revision += 1;
    state.sessions.remove(pos);
    state.changed(now);
    Ok(())
}

fn fail(
    state: &mut WorkspaceLifecycleState,
    fence: &CompletionFence,
    failure: Failure,
    now: DateTime<Utc>,
) -> Result<(), LifecycleError> {
    let (pos, operation_pos) = fenced_session(state, fence)?;
    let session = &mut state.sessions[pos];
    session.lifecycle = SessionLifecycle::Failed;
    session.failure = Some(failure);
    session.changed_at = now;
    let operation = &mut state.operations[operation_pos];
    operation.status = OperationStatus::Failed;
    operation.progress_revision += 1;
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
            semantic_key: "test".into(),
        }
    }
    fn fence(
        s: &WorkspaceLifecycleState,
        session: &ManagedSession,
        op: &OperationJournal,
    ) -> CompletionFence {
        CompletionFence {
            workspace_id: s.workspace_id,
            session_id: Some(session.session_id),
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
        let adopted = ManagedSession::adopt_available("legacy".into(), now());
        assert_eq!(adopted.name, "legacy");
        assert_eq!(adopted.lifecycle, SessionLifecycle::Available);
        assert_eq!(adopted.changed_at, now());
        assert!(adopted.operation_id.is_none());
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
        assert_eq!(state.operations[0].status, OperationStatus::Failed);
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
            semantic_key: "test".into(),
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
        assert_eq!(available.operations[1].status, OperationStatus::Failed);
        assert_eq!(
            available.sessions[0].operation_id,
            Some(remove.operation_id)
        );
    }

    #[test]
    fn legacy_failed_session_repairs_the_matching_succeeded_operation() {
        let mut state = WorkspaceLifecycleState::new(WorkspaceId::new(), now());
        let create = op();
        reduce(
            &mut state,
            LifecycleEvent::ReserveCreate {
                name: "legacy".into(),
                operation: create.clone(),
            },
            now(),
        )
        .unwrap();
        let create_fence = fence(&state, &state.sessions[0].clone(), &create);
        reduce(
            &mut state,
            LifecycleEvent::CreateCompleted {
                fence: create_fence,
                setup_plan: None,
            },
            now(),
        )
        .unwrap();
        let remove = OperationJournal {
            semantic_key: "remove:legacy".into(),
            ..op()
        };
        let session_id = state.sessions[0].session_id;
        reduce(
            &mut state,
            LifecycleEvent::BeginRemove {
                session_id,
                operation: remove.clone(),
                delete_plan: DeletePlan {
                    targets: vec!["legacy".into()],
                    force: false,
                },
            },
            now(),
        )
        .unwrap();
        // Recreate the contradictory shape emitted before durable failures:
        // the session failed, while the matching operation said succeeded and
        // the relationship between both records was cleared.
        state.sessions[0].lifecycle = SessionLifecycle::Failed;
        state.sessions[0].failure = Some(Failure {
            stage: FailureStage::Delete,
            summary: "worktree removal failed".into(),
        });
        state.sessions[0].operation_id = None;
        state.operations[1].status = OperationStatus::Succeeded;
        let revision = state.state_revision;

        assert_eq!(state.repair_legacy_failed_outcomes(now()), 1);
        assert_eq!(state.state_revision, revision + 1);
        assert_eq!(state.operations[0].status, OperationStatus::Succeeded);
        assert_eq!(state.operations[1].status, OperationStatus::Failed);
        assert_eq!(state.sessions[0].operation_id, Some(remove.operation_id));
        assert_eq!(state.repair_legacy_failed_outcomes(now()), 0);

        let mut skipped = WorkspaceLifecycleState::new(WorkspaceId::new(), now());
        skipped
            .sessions
            .push(ManagedSession::adopt_available("available".into(), now()));
        let mut failed_without_detail = ManagedSession::adopt_available("missing".into(), now());
        failed_without_detail.lifecycle = SessionLifecycle::Failed;
        skipped.sessions.push(failed_without_detail);
        let mut failed_create = ManagedSession::adopt_available("create".into(), now());
        failed_create.lifecycle = SessionLifecycle::Failed;
        failed_create.failure = Some(Failure {
            stage: FailureStage::Create,
            summary: "failed".into(),
        });
        skipped.sessions.push(failed_create);
        assert_eq!(skipped.repair_legacy_failed_outcomes(now()), 0);
    }

    #[test]
    fn reconcile_rejects_a_terminal_operation_as_stale() {
        let mut state = WorkspaceLifecycleState::new(WorkspaceId::new(), now());
        let operation = op();
        reduce(
            &mut state,
            LifecycleEvent::ReserveCreate {
                name: "terminal".into(),
                operation: operation.clone(),
            },
            now(),
        )
        .unwrap();
        state.operations[0].status = OperationStatus::Succeeded;
        let session_id = state.sessions[0].session_id;

        assert_eq!(
            reduce(
                &mut state,
                LifecycleEvent::ReconcileInterrupted {
                    session_id,
                    operation_id: operation.operation_id,
                    stage: FailureStage::Create,
                },
                now(),
            ),
            Err(LifecycleError::StaleCompletion)
        );
    }
}
