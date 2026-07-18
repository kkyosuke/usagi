//! Durable, owner-fenced requests for a human decision.

#![allow(clippy::missing_errors_doc)] // Typed validation errors are documented by UserDecisionError.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    agent::CallerRef,
    id::{OperationId, SessionId, UserDecisionId, WorkspaceId},
};

/// Immutable owner provenance captured from the authenticated execution context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDecisionOwner {
    pub workspace_id: WorkspaceId,
    pub session_id: SessionId,
    pub caller: CallerRef,
    pub run_id: OperationId,
}

/// One stable machine-selectable choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDecisionOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// A valid human answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UserDecisionAnswer {
    Option { option_id: String },
    Freeform { text: String },
}

/// Terminal and non-terminal decision states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserDecisionStatus {
    Pending,
    Resolved,
    Cancelled,
    Expired,
}

/// A complete durable decision record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDecision {
    pub decision_id: UserDecisionId,
    pub owner: UserDecisionOwner,
    pub title: String,
    pub prompt: String,
    pub options: Vec<UserDecisionOption>,
    pub allow_freeform: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub idempotency_key: Option<String>,
    pub status: UserDecisionStatus,
    pub answer: Option<UserDecisionAnswer>,
    pub created_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
}

/// Validation and compare-and-set failures that never mutate a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserDecisionError {
    InvalidOption,
    FreeformNotAllowed,
    Terminal,
    Expired,
    IdempotencyConflict,
}

impl UserDecision {
    /// Validates an answer without changing durable state.
    pub fn validate_answer(
        &self,
        answer: &UserDecisionAnswer,
        now: DateTime<Utc>,
    ) -> Result<(), UserDecisionError> {
        if self.status != UserDecisionStatus::Pending {
            return Err(UserDecisionError::Terminal);
        }
        if self.expires_at.is_some_and(|deadline| deadline <= now) {
            return Err(UserDecisionError::Expired);
        }
        match answer {
            UserDecisionAnswer::Option { option_id }
                if self.options.iter().any(|option| option.id == *option_id) =>
            {
                Ok(())
            }
            UserDecisionAnswer::Option { .. } => Err(UserDecisionError::InvalidOption),
            UserDecisionAnswer::Freeform { text } if self.allow_freeform && !text.is_empty() => {
                Ok(())
            }
            UserDecisionAnswer::Freeform { .. } => Err(UserDecisionError::FreeformNotAllowed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn decision() -> UserDecision {
        UserDecision {
            decision_id: UserDecisionId::new(),
            owner: UserDecisionOwner {
                workspace_id: WorkspaceId::new(),
                session_id: SessionId::new(),
                caller: CallerRef {
                    session_id: SessionId::new(),
                    agent_id: super::super::id::AgentId::new(),
                },
                run_id: OperationId::new(),
            },
            title: "title".into(),
            prompt: "prompt".into(),
            options: vec![UserDecisionOption {
                id: "yes".into(),
                label: "Yes".into(),
                description: None,
            }],
            allow_freeform: false,
            expires_at: None,
            idempotency_key: None,
            status: UserDecisionStatus::Pending,
            answer: None,
            created_at: Utc::now(),
            resolved_at: None,
        }
    }
    #[test]
    fn answer_validation_is_fail_closed() {
        let mut item = decision();
        let now = Utc::now();
        assert!(
            item.validate_answer(
                &UserDecisionAnswer::Option {
                    option_id: "yes".into()
                },
                now
            )
            .is_ok()
        );
        assert_eq!(
            item.validate_answer(
                &UserDecisionAnswer::Option {
                    option_id: "no".into()
                },
                now
            ),
            Err(UserDecisionError::InvalidOption)
        );
        assert_eq!(
            item.validate_answer(&UserDecisionAnswer::Freeform { text: "x".into() }, now),
            Err(UserDecisionError::FreeformNotAllowed)
        );
        item.status = UserDecisionStatus::Cancelled;
        assert_eq!(
            item.validate_answer(
                &UserDecisionAnswer::Option {
                    option_id: "yes".into()
                },
                now
            ),
            Err(UserDecisionError::Terminal)
        );
    }
}
