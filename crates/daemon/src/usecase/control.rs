#![coverage(off)]

//! Typed, pure session/control state transitions owned by the daemon.

#![allow(clippy::missing_errors_doc)]

use serde::{Deserialize, Serialize};
use usagi_core::domain::id::{AgentRuntimeId, OperationId, SessionId, WorkspaceId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycle {
    Creating,
    Available,
    Deleting,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    Ready,
    Running,
    Waiting,
    Ended,
    Exited,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchStatus {
    New,
    Dirty,
    Local,
    Pushed,
    Synced,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptState {
    Queued,
    Claimed,
    TerminalReserved,
    InputAcknowledged,
    Running,
    RetryWait,
    DeadLetter,
    Ambiguous,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlError {
    Unavailable,
    AmbiguousTarget,
    StaleReport,
    InvalidTransition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub id: AgentRuntimeId,
    pub phase: AgentPhase,
    pub phase_revision: u64,
    #[serde(skip)]
    token: String,
    #[serde(skip)]
    source_seq: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub workspace_id: WorkspaceId,
    pub id: SessionId,
    pub lifecycle: SessionLifecycle,
    pub branch: BranchStatus,
    pub state_revision: u64,
    pub runtimes: Vec<RuntimeSnapshot>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationAccepted {
    pub operation_id: OperationId,
    pub operation_revision: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prompt {
    pub operation_id: OperationId,
    pub target: AgentRuntimeId,
    pub state: PromptState,
    pub attempts: u8,
}

/// Apply a phase report only for the exact capability and a newer source sequence.
pub fn report_phase(
    runtime: &mut RuntimeSnapshot,
    token: &str,
    source_seq: u64,
    phase: AgentPhase,
) -> Result<bool, ControlError> {
    if runtime.token != token || runtime.phase == AgentPhase::Exited {
        return Err(ControlError::StaleReport);
    }
    if source_seq <= runtime.source_seq {
        return Ok(false);
    }
    runtime.source_seq = source_seq;
    runtime.phase = phase;
    runtime.phase_revision += 1;
    Ok(true)
}
/// Resolve an omitted prompt target only where one eligible agent pane exists.
pub fn resolve_target(
    session: &SessionSnapshot,
    target: Option<AgentRuntimeId>,
) -> Result<AgentRuntimeId, ControlError> {
    if session.lifecycle != SessionLifecycle::Available {
        return Err(ControlError::Unavailable);
    }
    let eligible: Vec<_> = session
        .runtimes
        .iter()
        .filter(|r| r.phase != AgentPhase::Exited)
        .collect();
    match target {
        Some(id) if eligible.iter().any(|r| r.id == id) => Ok(id),
        Some(_) => Err(ControlError::StaleReport),
        None if eligible.len() == 1 => Ok(eligible[0].id),
        None => Err(ControlError::AmbiguousTarget),
    }
}
/// Advance the durable input transaction. ACK loss is intentionally ambiguous.
pub fn advance_prompt(prompt: &mut Prompt, next: PromptState) -> Result<(), ControlError> {
    if prompt.state == PromptState::InputAcknowledged && next == PromptState::RetryWait {
        prompt.state = PromptState::Ambiguous;
        return Ok(());
    }
    if matches!(
        (&prompt.state, &next),
        (PromptState::Queued, PromptState::Claimed)
            | (PromptState::Claimed, PromptState::TerminalReserved)
            | (
                PromptState::TerminalReserved,
                PromptState::InputAcknowledged
            )
            | (PromptState::InputAcknowledged, PromptState::Running)
    ) {
        prompt.state = next;
        return Ok(());
    }
    Err(ControlError::InvalidTransition)
}
/// Begin removal by fencing all ordinary prompt/spawn delivery.
pub fn begin_remove(
    session: &mut SessionSnapshot,
    expected_revision: u64,
) -> Result<OperationAccepted, ControlError> {
    if session.state_revision != expected_revision {
        return Err(ControlError::StaleReport);
    }
    session.lifecycle = SessionLifecycle::Deleting;
    session.state_revision += 1;
    Ok(OperationAccepted {
        operation_id: OperationId::new(),
        operation_revision: session.state_revision,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::many_single_char_names)]
    use super::*;
    fn session() -> SessionSnapshot {
        SessionSnapshot {
            workspace_id: WorkspaceId::new(),
            id: SessionId::new(),
            lifecycle: SessionLifecycle::Available,
            branch: BranchStatus::New,
            state_revision: 1,
            runtimes: vec![RuntimeSnapshot {
                id: AgentRuntimeId::new(),
                phase: AgentPhase::Ready,
                phase_revision: 0,
                token: "t".into(),
                source_seq: 0,
            }],
        }
    }
    #[test]
    fn phase_requires_token_and_new_sequence() {
        let mut s = session();
        assert!(report_phase(&mut s.runtimes[0], "t", 1, AgentPhase::Running).unwrap());
        assert!(!report_phase(&mut s.runtimes[0], "t", 1, AgentPhase::Waiting).unwrap());
        assert_eq!(
            report_phase(&mut s.runtimes[0], "bad", 2, AgentPhase::Waiting),
            Err(ControlError::StaleReport)
        );
        s.runtimes[0].phase = AgentPhase::Exited;
        assert_eq!(
            report_phase(&mut s.runtimes[0], "t", 2, AgentPhase::Running),
            Err(ControlError::StaleReport)
        );
    }
    #[test]
    fn target_is_never_guessed() {
        let mut s = session();
        let id = s.runtimes[0].id;
        assert_eq!(resolve_target(&s, None), Ok(id));
        s.runtimes.push(RuntimeSnapshot {
            id: AgentRuntimeId::new(),
            phase: AgentPhase::Ready,
            phase_revision: 0,
            token: "x".into(),
            source_seq: 0,
        });
        assert_eq!(resolve_target(&s, None), Err(ControlError::AmbiguousTarget));
        assert_eq!(
            resolve_target(&s, Some(AgentRuntimeId::new())),
            Err(ControlError::StaleReport)
        );
        s.lifecycle = SessionLifecycle::Deleting;
        assert_eq!(resolve_target(&s, Some(id)), Err(ControlError::Unavailable));
    }
    #[test]
    fn prompt_ack_loss_is_not_retried() {
        let mut p = Prompt {
            operation_id: OperationId::new(),
            target: AgentRuntimeId::new(),
            state: PromptState::Queued,
            attempts: 0,
        };
        for state in [
            PromptState::Claimed,
            PromptState::TerminalReserved,
            PromptState::InputAcknowledged,
        ] {
            advance_prompt(&mut p, state).unwrap();
        }
        advance_prompt(&mut p, PromptState::RetryWait).unwrap();
        assert_eq!(p.state, PromptState::Ambiguous);
        assert_eq!(
            advance_prompt(&mut p, PromptState::Running),
            Err(ControlError::InvalidTransition)
        );
    }
    #[test]
    fn remove_fences_future_delivery() {
        let mut s = session();
        let rev = s.state_revision;
        let accepted = begin_remove(&mut s, rev).unwrap();
        assert_eq!(accepted.operation_revision, 2);
        assert_eq!(resolve_target(&s, None), Err(ControlError::Unavailable));
        assert_eq!(begin_remove(&mut s, rev), Err(ControlError::StaleReport));
    }
}
