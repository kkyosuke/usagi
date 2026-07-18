//! Daemon push を Home controller が受け取る projection に変換する。
//!
//! IPC event schema の decode は transport adapter の責務である。この module は
//! decode 済みの typed push だけを受け、`ProtocolError` の安全な envelope fields
//! 以外を TUI state へ渡さない。

use std::collections::VecDeque;

use usagi_core::domain::id::{AgentRuntimeRef, UserDecisionId, WorkspaceId};
use usagi_core::domain::session_lifecycle::AgentPhase;
use usagi_core::domain::user_decision::UserDecision;
use usagi_core::infrastructure::ipc::ProtocolError;

use crate::usecase::application::controller::{BackendEvent, Feedback, SafeError, SafeMessage};

/// A decoded daemon push relevant to Home's phase and feedback projection.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonPush {
    /// A phase update for one fully fenced Agent runtime.
    RuntimePhase {
        runtime: AgentRuntimeRef,
        phase: AgentPhase,
    },
    /// Progress text which the daemon event schema has marked safe to display.
    OperationProgress(SafeMessage),
    /// A failed durable operation.
    OperationError(ProtocolError),
    /// A terminal action or stream failure.
    TerminalError(ProtocolError),
    /// The client lost its daemon connection.
    Disconnected,
    /// The client restored its daemon connection.
    Reconnected,
    /// The daemon requires an atomic snapshot replacement.
    ResyncRequired,
    /// Atomic workspace snapshot used on attach/reconnect/resync.
    DecisionsSnapshot {
        workspace: WorkspaceId,
        decisions: Vec<UserDecision>,
    },
    /// Resolve confirmation; only this event removes the local pending row.
    DecisionResolved {
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
    },
    /// Safe resolve failure. The reducer preserves the editor draft for retry.
    DecisionError {
        workspace: WorkspaceId,
        decision_id: UserDecisionId,
        error: ProtocolError,
    },
}

/// Converts decoded daemon pushes into events consumed by the Home reducer.
#[derive(Debug, Default)]
pub struct DaemonPushAdapter {
    pending: VecDeque<DaemonPush>,
}

impl DaemonPushAdapter {
    /// Queues one push from the IPC reader. Queue order is retained so a
    /// keyless runtime cycle applies daemon state in delivery order.
    pub fn push(&mut self, push: DaemonPush) {
        self.pending.push_back(push);
    }

    /// Returns the next TUI-local projection event, if any.
    #[must_use]
    pub fn next_event(&mut self) -> Option<BackendEvent> {
        self.pending.pop_front().map(adapt_push)
    }
}

/// Converts one decoded push without retaining any wire payload in UI state.
#[must_use]
pub fn adapt_push(push: DaemonPush) -> BackendEvent {
    match push {
        DaemonPush::RuntimePhase { runtime, phase } => {
            BackendEvent::RuntimePhase { runtime, phase }
        }
        DaemonPush::OperationProgress(message) => {
            BackendEvent::Feedback(Feedback::Progress(message))
        }
        DaemonPush::OperationError(error) => {
            BackendEvent::Feedback(Feedback::OperationError(safe_error(error)))
        }
        DaemonPush::TerminalError(error) => {
            BackendEvent::Feedback(Feedback::TerminalError(safe_error(error)))
        }
        DaemonPush::Disconnected => BackendEvent::Feedback(Feedback::Disconnected),
        DaemonPush::Reconnected => BackendEvent::Feedback(Feedback::Reconnected),
        DaemonPush::ResyncRequired => BackendEvent::Feedback(Feedback::ResyncRequired),
        DaemonPush::DecisionsSnapshot {
            workspace,
            decisions,
        } => BackendEvent::Decisions {
            workspace,
            decisions,
        },
        DaemonPush::DecisionResolved {
            workspace,
            decision_id,
        } => BackendEvent::DecisionResolved {
            workspace,
            decision_id,
        },
        DaemonPush::DecisionError {
            workspace,
            decision_id,
            error,
        } => BackendEvent::DecisionError {
            workspace,
            decision_id,
            error: safe_error(error),
        },
    }
}

fn safe_error(error: ProtocolError) -> SafeError {
    SafeError {
        message: SafeMessage::new(error.message),
        error_id: error.error_id,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use usagi_core::domain::id::{
        AgentRuntimeId, DaemonGeneration, SessionId, TerminalId, TerminalRef, WorkspaceId,
        WorktreeId,
    };
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};

    use super::*;
    use crate::usecase::application::controller::{
        AppEvent, AppState, Target, TargetPhase, update,
    };

    fn runtime(workspace: WorkspaceId, session: SessionId) -> AgentRuntimeRef {
        AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            TerminalRef {
                workspace_id: workspace,
                worktree_id: WorktreeId::new(),
                session_id: Some(session),
                terminal_id: TerminalId::new(),
                daemon_generation: DaemonGeneration::new(),
            },
            session,
        )
        .unwrap()
    }

    #[test]
    fn fake_pushes_update_phase_and_connection_feedback_without_a_key() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let runtime = runtime(workspace, session);
        let mut adapter = DaemonPushAdapter::default();
        adapter.push(DaemonPush::RuntimePhase {
            runtime,
            phase: AgentPhase::Waiting,
        });
        adapter.push(DaemonPush::Disconnected);
        adapter.push(DaemonPush::Reconnected);
        adapter.push(DaemonPush::ResyncRequired);

        let mut state = AppState::home(workspace, vec![session]);
        while let Some(event) = adapter.next_event() {
            let _ = update(&mut state, AppEvent::Backend(event));
        }

        assert_eq!(
            state.phase_for(Target::Session(session)),
            TargetPhase::Waiting
        );
        assert_eq!(state.feedback(), Some(&Feedback::ResyncRequired));
    }

    #[test]
    fn error_adapter_retains_only_safe_message_and_error_id() {
        let mut error = ProtocolError::new(ErrorCode::Internal, "Could not attach terminal");
        error.error_id = "err-safe-42".into();
        error.details = Some(json!({"panic": "secret token", "backtrace": "internal"}));

        assert_eq!(
            adapt_push(DaemonPush::TerminalError(error)),
            BackendEvent::Feedback(Feedback::TerminalError(SafeError {
                message: SafeMessage::new("Could not attach terminal"),
                error_id: "err-safe-42".into(),
            }))
        );

        assert_eq!(
            adapt_push(DaemonPush::OperationProgress(SafeMessage::new(
                "Creating session"
            ))),
            BackendEvent::Feedback(Feedback::Progress(SafeMessage::new("Creating session")))
        );

        let mut operation_error = ProtocolError::new(ErrorCode::Busy, "Session is busy");
        operation_error.error_id = "err-safe-43".into();
        assert_eq!(
            adapt_push(DaemonPush::OperationError(operation_error)),
            BackendEvent::Feedback(Feedback::OperationError(SafeError {
                message: SafeMessage::new("Session is busy"),
                error_id: "err-safe-43".into(),
            }))
        );
    }
}
