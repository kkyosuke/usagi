//! Durable, owner-fenced requests for a human decision.

#![allow(clippy::missing_errors_doc)] // Typed validation errors are documented by UserDecisionError.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    agent::CallerRef,
    id::{OperationId, SessionId, UserDecisionId, WorkspaceId},
};

pub const USER_DECISION_TITLE_MAX_BYTES: usize = 256;
pub const USER_DECISION_PROMPT_MAX_BYTES: usize = 16 * 1024;
pub const USER_DECISION_OPTION_MAX_COUNT: usize = 32;
pub const USER_DECISION_OPTION_ID_MAX_BYTES: usize = 128;
pub const USER_DECISION_OPTION_LABEL_MAX_BYTES: usize = 256;
pub const USER_DECISION_OPTION_DESCRIPTION_MAX_BYTES: usize = 2 * 1024;
pub const USER_DECISION_FREEFORM_MAX_BYTES: usize = 16 * 1024;
pub const USER_DECISION_IDEMPOTENCY_KEY_MAX_BYTES: usize = 256;
pub const USER_DECISION_MAX_LIFETIME_HOURS: i64 = 7 * 24;

/// Shared resource ceilings for caller-controlled decision data.
///
/// MCP schemas publish these same values, while the domain remains authoritative
/// and measures UTF-8 bytes rather than Unicode scalar values.
pub struct UserDecisionPolicy;

impl UserDecisionPolicy {
    pub const TITLE_MAX_BYTES: usize = USER_DECISION_TITLE_MAX_BYTES;
    pub const PROMPT_MAX_BYTES: usize = USER_DECISION_PROMPT_MAX_BYTES;
    pub const OPTION_COUNT_MAX: usize = USER_DECISION_OPTION_MAX_COUNT;
    pub const OPTION_ID_MAX_BYTES: usize = USER_DECISION_OPTION_ID_MAX_BYTES;
    pub const OPTION_LABEL_MAX_BYTES: usize = USER_DECISION_OPTION_LABEL_MAX_BYTES;
    pub const OPTION_DESCRIPTION_MAX_BYTES: usize = USER_DECISION_OPTION_DESCRIPTION_MAX_BYTES;
    pub const IDEMPOTENCY_KEY_MAX_BYTES: usize = USER_DECISION_IDEMPOTENCY_KEY_MAX_BYTES;
    pub const FREEFORM_ANSWER_MAX_BYTES: usize = USER_DECISION_FREEFORM_MAX_BYTES;
}

/// Immutable owner provenance captured from the authenticated execution context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDecisionOwner {
    pub workspace_id: WorkspaceId,
    /// The managed-session scope, or `None` for the daemon-owned workspace root.
    pub session_id: Option<SessionId>,
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
    InvalidRequest,
    InvalidOption,
    FreeformNotAllowed,
    Terminal,
    Expired,
    IdempotencyConflict,
    /// This daemon already holds as many unanswered decisions as it will.
    ///
    /// Pending decisions are the one class this store may never evict — each is
    /// a caller blocked on an answer — so a saturated store refuses the *new*
    /// request instead of dropping an old one. The refusal has no effect at all,
    /// which makes it safe for the caller to retry once a person has worked
    /// through the backlog.
    PendingLimitReached,
}

impl UserDecision {
    /// Validates the bounded, answerable request before it reaches durable
    /// storage. This is the authority; transport schemas are only guidance.
    pub fn validate_request(&self) -> Result<(), UserDecisionError> {
        let bounded_nonempty = |value: &str, max: usize| {
            !value.trim().is_empty() && value.len() <= max && !value.contains('\0')
        };
        if !bounded_nonempty(&self.title, USER_DECISION_TITLE_MAX_BYTES)
            || !bounded_nonempty(&self.prompt, USER_DECISION_PROMPT_MAX_BYTES)
            || self.options.len() > USER_DECISION_OPTION_MAX_COUNT
            || (self.options.is_empty() && !self.allow_freeform)
            || self
                .idempotency_key
                .as_ref()
                .is_some_and(|key| !bounded_nonempty(key, USER_DECISION_IDEMPOTENCY_KEY_MAX_BYTES))
            || self.expires_at.is_some_and(|expires_at| {
                expires_at <= self.created_at
                    || expires_at
                        > self.created_at
                            + chrono::Duration::hours(USER_DECISION_MAX_LIFETIME_HOURS)
            })
        {
            return Err(UserDecisionError::InvalidRequest);
        }
        let mut ids = std::collections::BTreeSet::new();
        for option in &self.options {
            if !bounded_nonempty(&option.id, USER_DECISION_OPTION_ID_MAX_BYTES)
                || !bounded_nonempty(&option.label, USER_DECISION_OPTION_LABEL_MAX_BYTES)
                || option.description.as_ref().is_some_and(|description| {
                    description.len() > USER_DECISION_OPTION_DESCRIPTION_MAX_BYTES
                        || description.contains('\0')
                })
                || !ids.insert(option.id.as_str())
            {
                return Err(UserDecisionError::InvalidRequest);
            }
        }
        Ok(())
    }

    /// Revalidates every caller-controlled field loaded from durable storage.
    pub fn validate_resource_policy(&self) -> Result<(), UserDecisionError> {
        self.validate_request()?;
        if let Some(answer) = &self.answer {
            answer.validate_resource_policy()?;
        }
        Ok(())
    }

    /// Validates an answer without changing durable state.
    pub fn validate_answer(
        &self,
        answer: &UserDecisionAnswer,
        now: DateTime<Utc>,
    ) -> Result<(), UserDecisionError> {
        answer.validate_resource_policy()?;
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
            UserDecisionAnswer::Freeform { text }
                if self.allow_freeform
                    && !text.trim().is_empty()
                    && text.len() <= USER_DECISION_FREEFORM_MAX_BYTES
                    && !text.contains('\0') =>
            {
                Ok(())
            }
            UserDecisionAnswer::Freeform { .. } => Err(UserDecisionError::FreeformNotAllowed),
        }
    }
}

impl UserDecisionAnswer {
    /// Enforces the resource half of answer validation without mutating state.
    pub fn validate_resource_policy(&self) -> Result<(), UserDecisionError> {
        let bounded_nonempty = |value: &str, max: usize| {
            !value.trim().is_empty() && value.len() <= max && !value.contains('\0')
        };
        let valid = match self {
            Self::Option { option_id } => {
                bounded_nonempty(option_id, USER_DECISION_OPTION_ID_MAX_BYTES)
            }
            Self::Freeform { text } => bounded_nonempty(text, USER_DECISION_FREEFORM_MAX_BYTES),
        };
        valid.then_some(()).ok_or(UserDecisionError::InvalidRequest)
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
                session_id: Some(SessionId::new()),
                caller: CallerRef {
                    session_id: Some(SessionId::new()),
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

    #[test]
    fn request_validation_requires_a_bounded_answerable_question() {
        let mut item = decision();
        assert!(item.validate_request().is_ok());
        item.options.clear();
        assert_eq!(
            item.validate_request(),
            Err(UserDecisionError::InvalidRequest)
        );
        item.allow_freeform = true;
        assert!(item.validate_request().is_ok());
        item.options = vec![
            UserDecisionOption {
                id: "same".into(),
                label: "A".into(),
                description: None,
            },
            UserDecisionOption {
                id: "same".into(),
                label: "B".into(),
                description: None,
            },
        ];
        assert_eq!(
            item.validate_request(),
            Err(UserDecisionError::InvalidRequest)
        );
        item.options.truncate(1);
        item.prompt = "x".repeat(USER_DECISION_PROMPT_MAX_BYTES + 1);
        assert_eq!(
            item.validate_request(),
            Err(UserDecisionError::InvalidRequest)
        );
        item.prompt = "question".into();
        item.options[0].description =
            Some("x".repeat(USER_DECISION_OPTION_DESCRIPTION_MAX_BYTES + 1));
        assert_eq!(
            item.validate_request(),
            Err(UserDecisionError::InvalidRequest)
        );
        item.options[0].description = Some("unsafe\0description".into());
        assert_eq!(
            item.validate_request(),
            Err(UserDecisionError::InvalidRequest)
        );
    }

    #[test]
    fn resource_policy_measures_utf8_bytes_for_requests_and_answers() {
        let mut item = decision();
        item.title = "界".repeat(UserDecisionPolicy::TITLE_MAX_BYTES / 3 + 1);
        assert!(item.title.chars().count() < UserDecisionPolicy::TITLE_MAX_BYTES);
        assert_eq!(
            item.validate_resource_policy(),
            Err(UserDecisionError::InvalidRequest)
        );

        let answer = UserDecisionAnswer::Freeform {
            text: "界".repeat(UserDecisionPolicy::FREEFORM_ANSWER_MAX_BYTES / 3 + 1),
        };
        assert_eq!(
            answer.validate_resource_policy(),
            Err(UserDecisionError::InvalidRequest)
        );
    }

    #[test]
    fn root_scoped_owner_round_trips_without_a_session() {
        let mut item = decision();
        item.owner.session_id = None;

        let encoded = serde_json::to_value(&item).unwrap();
        let decoded: UserDecision = serde_json::from_value(encoded).unwrap();

        assert_eq!(decoded.owner.session_id, None);
    }

    #[test]
    fn answer_validation_allows_nonempty_freeform_only_when_enabled_and_before_deadline() {
        let mut item = decision();
        let now = Utc::now();
        item.allow_freeform = true;
        assert!(
            item.validate_answer(
                &UserDecisionAnswer::Freeform {
                    text: "because".into()
                },
                now
            )
            .is_ok()
        );
        assert_eq!(
            item.validate_answer(
                &UserDecisionAnswer::Freeform {
                    text: String::new()
                },
                now
            ),
            Err(UserDecisionError::InvalidRequest)
        );
        item.expires_at = Some(now);
        assert_eq!(
            item.validate_answer(
                &UserDecisionAnswer::Option {
                    option_id: "yes".into()
                },
                now
            ),
            Err(UserDecisionError::Expired)
        );
    }
}
