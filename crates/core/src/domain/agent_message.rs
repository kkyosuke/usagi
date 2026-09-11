//! Provider-independent messages between participants of one managed session.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::id::{AgentId, OperationId};

/// A review refers to immutable Git objects, never a mutable branch name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTarget {
    pub base_sha: String,
    pub head_sha: String,
}

impl ReviewTarget {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        [&self.base_sha, &self.head_sha].into_iter().all(|sha| {
            matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) && self.base_sha.len() == self.head_sha.len()
    }
}

/// Messages do not terminate a dispatch run or change its caller binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Message,
    ReviewRequest,
    Approved,
    ChangesRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessage {
    /// Caller-generated identity, reused only for a retry of this message.
    pub message_id: OperationId,
    pub to_agent_id: AgentId,
    pub kind: MessageKind,
    pub body: String,
    #[serde(default)]
    pub in_reply_to: Option<OperationId>,
    #[serde(default)]
    pub review: Option<ReviewTarget>,
}

impl SendMessage {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.body.trim().is_empty()
            && self.body.len() <= 16 * 1024
            && !self.body.contains('\0')
            && self.review.as_ref().is_none_or(ReviewTarget::is_valid)
            && match self.kind {
                MessageKind::Message => self.review.is_none(),
                MessageKind::ReviewRequest => self.review.is_some() && self.in_reply_to.is_none(),
                MessageKind::Approved | MessageKind::ChangesRequested => {
                    self.review.is_some() && self.in_reply_to.is_some()
                }
            }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub from_agent_id: AgentId,
    pub from_run_id: OperationId,
    #[serde(flatten)]
    pub message: SendMessage,
    pub created_at: DateTime<Utc>,
    pub acknowledged: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_targets_and_message_shapes_are_bounded() {
        let target = ReviewTarget {
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
        };
        assert!(target.is_valid());
        assert!(
            ReviewTarget {
                base_sha: "A".repeat(64),
                head_sha: "0".repeat(64)
            }
            .is_valid()
        );
        for sha in ["main".to_owned(), "z".repeat(40), "a".repeat(64)] {
            assert!(
                !ReviewTarget {
                    head_sha: sha,
                    ..target.clone()
                }
                .is_valid()
            );
        }
        let mut message = SendMessage {
            message_id: OperationId::new(),
            to_agent_id: AgentId::new(),
            kind: MessageKind::Message,
            body: "review".into(),
            in_reply_to: None,
            review: None,
        };
        assert!(message.is_valid());
        assert!(
            SendMessage {
                body: "a".repeat(16384),
                ..message.clone()
            }
            .is_valid()
        );
        assert!(
            !SendMessage {
                body: "あ".repeat(5462),
                ..message.clone()
            }
            .is_valid()
        );
        assert!(
            !SendMessage {
                kind: MessageKind::ReviewRequest,
                ..message.clone()
            }
            .is_valid()
        );
        for body in [" ".to_owned(), "a\0b".to_owned(), "a".repeat(16385)] {
            assert!(
                !SendMessage {
                    body,
                    ..message.clone()
                }
                .is_valid()
            );
        }
        message.review = Some(target);
        assert!(!message.is_valid());
        message.kind = MessageKind::ReviewRequest;
        assert!(message.is_valid());
        message.in_reply_to = Some(OperationId::new());
        assert!(!message.is_valid());
        for kind in [MessageKind::Approved, MessageKind::ChangesRequested] {
            message.kind = kind;
            assert!(message.is_valid());
            assert!(
                !SendMessage {
                    review: None,
                    ..message.clone()
                }
                .is_valid()
            );
            assert!(
                !SendMessage {
                    in_reply_to: None,
                    ..message.clone()
                }
                .is_valid()
            );
        }
        message.review.as_mut().unwrap().head_sha = "main".into();
        assert!(!message.is_valid());
    }
}
