//! Atomic durable decisions and their polling-delivery outbox.
//!
//! A resolve changes the decision and appends its delivery in one replaced JSON
//! document under one lock. A daemon consumer validates the event against that
//! record and acknowledges it only after delivery to the originating run.

#![allow(clippy::missing_errors_doc)] // Store IO errors follow the shared store contract.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::{
    domain::{
        agent::CallerRef,
        id::{UserDecisionId, WorkspaceId},
        user_decision::{UserDecision, UserDecisionAnswer, UserDecisionError, UserDecisionStatus},
    },
    infrastructure::persistence::{json_file, store_lock::StoreLock},
};

const FILE: &str = "user-decisions.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDecisionResolvedEvent {
    pub decision_id: UserDecisionId,
    pub recipient: CallerRef,
    pub answer: UserDecisionAnswer,
    pub created_at: DateTime<Utc>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserDecisionDeliveryError {
    Inconsistent,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    decisions: Vec<UserDecision>,
    events: Vec<UserDecisionResolvedEvent>,
}
pub struct UserDecisionStore {
    dir: PathBuf,
}

impl UserDecisionStore {
    #[must_use]
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().into(),
        }
    }
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.join(FILE)
    }
    pub fn get(&self, workspace: WorkspaceId, id: UserDecisionId) -> Result<Option<UserDecision>> {
        Ok(self
            .load()?
            .decisions
            .into_iter()
            .find(|item| item.decision_id == id && item.owner.workspace_id == workspace))
    }
    pub fn pending(&self, workspace: WorkspaceId) -> Result<Vec<UserDecision>> {
        Ok(self
            .load()?
            .decisions
            .into_iter()
            .filter(|item| {
                item.owner.workspace_id == workspace && item.status == UserDecisionStatus::Pending
            })
            .collect())
    }
    pub fn events(&self) -> Result<Vec<UserDecisionResolvedEvent>> {
        Ok(self.load()?.events)
    }
    /// Returns the resolved durable record only when it still agrees with an
    /// outbox event.  This prevents a consumer from routing an answer based on
    /// forged or stale event data.
    pub fn get_for_event(&self, event: &UserDecisionResolvedEvent) -> Result<Option<UserDecision>> {
        Ok(self.load()?.decisions.into_iter().find(|decision| {
            decision.decision_id == event.decision_id
                && decision.owner.caller == event.recipient
                && decision.status == UserDecisionStatus::Resolved
                && decision.answer.as_ref() == Some(&event.answer)
        }))
    }
    /// Acknowledges one validated delivery. Repeated acknowledgements are a
    /// safe no-op, which makes reconnect recovery idempotent.
    pub fn ack_event(&self, id: UserDecisionId) -> Result<bool> {
        self.mutate(|state| {
            let Some(index) = state
                .events
                .iter()
                .position(|event| event.decision_id == id)
            else {
                return false;
            };
            state.events.remove(index);
            true
        })
    }
    pub fn consume_events(&self) -> Result<Result<usize, UserDecisionDeliveryError>> {
        self.mutate(|state| {
            let consistent = state.events.iter().all(|event| {
                state.decisions.iter().any(|decision| {
                    decision.decision_id == event.decision_id
                        && decision.owner.caller == event.recipient
                        && decision.status == UserDecisionStatus::Resolved
                        && decision.answer.as_ref() == Some(&event.answer)
                })
            });
            if !consistent {
                return Err(UserDecisionDeliveryError::Inconsistent);
            }
            let consumed = state.events.len();
            state.events.clear();
            Ok(consumed)
        })
    }
    pub fn create(
        &self,
        decision: UserDecision,
    ) -> Result<Result<UserDecision, UserDecisionError>> {
        self.mutate(|state| {
            if let Some(key) = &decision.idempotency_key
                && let Some(existing) = state.decisions.iter().find(|item| {
                    item.owner == decision.owner && item.idempotency_key.as_ref() == Some(key)
                })
            {
                return if same_request(existing, &decision) {
                    Ok(existing.clone())
                } else {
                    Err(UserDecisionError::IdempotencyConflict)
                };
            }
            state.decisions.push(decision.clone());
            Ok(decision)
        })
    }
    pub fn resolve(
        &self,
        workspace: WorkspaceId,
        id: UserDecisionId,
        answer: UserDecisionAnswer,
        now: DateTime<Utc>,
    ) -> Result<Result<UserDecision, UserDecisionError>> {
        self.mutate(|state| {
            let Some(item) = state
                .decisions
                .iter_mut()
                .find(|item| item.decision_id == id && item.owner.workspace_id == workspace)
            else {
                return Err(UserDecisionError::Terminal);
            };
            item.validate_answer(&answer, now)?;
            item.status = UserDecisionStatus::Resolved;
            item.answer = Some(answer.clone());
            item.resolved_at = Some(now);
            state.events.push(UserDecisionResolvedEvent {
                decision_id: id,
                recipient: item.owner.caller.clone(),
                answer,
                created_at: now,
            });
            Ok(item.clone())
        })
    }
    pub fn terminal(
        &self,
        workspace: WorkspaceId,
        id: UserDecisionId,
        status: UserDecisionStatus,
        now: DateTime<Utc>,
    ) -> Result<Result<UserDecision, UserDecisionError>> {
        self.mutate(|state| {
            let Some(item) = state
                .decisions
                .iter_mut()
                .find(|item| item.decision_id == id && item.owner.workspace_id == workspace)
            else {
                return Err(UserDecisionError::Terminal);
            };
            if item.status != UserDecisionStatus::Pending {
                return Err(UserDecisionError::Terminal);
            }
            item.status = status;
            item.resolved_at = Some(now);
            Ok(item.clone())
        })
    }
    fn load(&self) -> Result<State> {
        Ok(json_file::read(&self.path())?.unwrap_or_default())
    }
    fn mutate<T>(&self, f: impl FnOnce(&mut State) -> T) -> Result<T> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut state = self.load()?;
        let result = f(&mut state);
        json_file::write_atomic(&self.dir, &self.path(), &state)?;
        Ok(result)
    }
}
fn same_request(a: &UserDecision, b: &UserDecision) -> bool {
    a.title == b.title
        && a.prompt == b.prompt
        && a.options == b.options
        && a.allow_freeform == b.allow_freeform
        && a.expires_at == b.expires_at
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        id::{AgentId, OperationId, SessionId},
        user_decision::{UserDecisionOption, UserDecisionOwner},
    };
    fn item() -> UserDecision {
        UserDecision {
            decision_id: UserDecisionId::new(),
            owner: UserDecisionOwner {
                workspace_id: WorkspaceId::new(),
                session_id: Some(SessionId::new()),
                caller: CallerRef {
                    session_id: Some(SessionId::new()),
                    agent_id: AgentId::new(),
                },
                run_id: OperationId::new(),
            },
            title: "t".into(),
            prompt: "p".into(),
            options: vec![UserDecisionOption {
                id: "a".into(),
                label: "A".into(),
                description: None,
            }],
            allow_freeform: false,
            expires_at: None,
            idempotency_key: Some("k".into()),
            status: UserDecisionStatus::Pending,
            answer: None,
            created_at: Utc::now(),
            resolved_at: None,
        }
    }
    #[test]
    fn retry_and_resolve_are_durable_and_once() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let decision = item();
        let workspace = decision.owner.workspace_id;
        assert_eq!(
            store.create(decision.clone()).unwrap().unwrap().decision_id,
            decision.decision_id
        );
        assert_eq!(
            store.create(decision.clone()).unwrap().unwrap().decision_id,
            decision.decision_id
        );
        let resolved = store
            .resolve(
                workspace,
                decision.decision_id,
                UserDecisionAnswer::Option {
                    option_id: "a".into(),
                },
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(resolved.status, UserDecisionStatus::Resolved);
        let event = store.events().unwrap().pop().unwrap();
        assert_eq!(event.decision_id, decision.decision_id);
        assert_eq!(event.recipient, decision.owner.caller);
        assert_eq!(store.get_for_event(&event).unwrap(), Some(resolved.clone()));
        assert!(store.ack_event(event.decision_id).unwrap());
        assert!(!store.ack_event(event.decision_id).unwrap());
        assert!(store.events().unwrap().is_empty());
        assert_eq!(
            store
                .resolve(
                    workspace,
                    decision.decision_id,
                    UserDecisionAnswer::Option {
                        option_id: "a".into()
                    },
                    Utc::now()
                )
                .unwrap(),
            Err(UserDecisionError::Terminal)
        );
        assert_eq!(
            UserDecisionStore::new(temp.path())
                .get(workspace, decision.decision_id)
                .unwrap()
                .unwrap()
                .answer,
            resolved.answer
        );
    }

    #[test]
    fn consumer_rejects_an_event_without_its_resolved_record() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let decision = item();
        let state = serde_json::json!({
            "decisions": [],
            "events": [{
                "decision_id": decision.decision_id,
                "recipient": decision.owner.caller,
                "answer": {"kind":"option", "option_id":"a"},
                "created_at": Utc::now(),
            }],
        });
        std::fs::write(store.path(), serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(
            store.consume_events().unwrap(),
            Err(UserDecisionDeliveryError::Inconsistent)
        );
        assert_eq!(store.events().unwrap().len(), 1);
    }
    #[test]
    fn foreign_or_terminal_changes_do_not_deliver() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let decision = item();
        store.create(decision.clone()).unwrap().unwrap();
        assert_eq!(
            store
                .resolve(
                    WorkspaceId::new(),
                    decision.decision_id,
                    UserDecisionAnswer::Option {
                        option_id: "a".into()
                    },
                    Utc::now()
                )
                .unwrap(),
            Err(UserDecisionError::Terminal)
        );
        store
            .terminal(
                decision.owner.workspace_id,
                decision.decision_id,
                UserDecisionStatus::Cancelled,
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        assert!(store.events().unwrap().is_empty());
    }

    #[test]
    fn store_lists_pending_and_rejects_conflicting_key_and_terminal_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let decision = item();
        let workspace = decision.owner.workspace_id;
        assert!(
            store
                .get(workspace, decision.decision_id)
                .unwrap()
                .is_none()
        );
        store.create(decision.clone()).unwrap().unwrap();
        assert_eq!(store.pending(workspace).unwrap(), vec![decision.clone()]);
        assert!(store.pending(WorkspaceId::new()).unwrap().is_empty());

        let mut conflict = decision.clone();
        conflict.title = "other".into();
        assert_eq!(
            store.create(conflict).unwrap(),
            Err(UserDecisionError::IdempotencyConflict)
        );
        let expired = store
            .terminal(
                workspace,
                decision.decision_id,
                UserDecisionStatus::Expired,
                Utc::now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(expired.status, UserDecisionStatus::Expired);
        assert!(store.pending(workspace).unwrap().is_empty());
        assert_eq!(
            store
                .terminal(
                    workspace,
                    decision.decision_id,
                    UserDecisionStatus::Cancelled,
                    Utc::now(),
                )
                .unwrap(),
            Err(UserDecisionError::Terminal)
        );
        assert_eq!(
            store
                .terminal(
                    WorkspaceId::new(),
                    decision.decision_id,
                    UserDecisionStatus::Cancelled,
                    Utc::now(),
                )
                .unwrap(),
            Err(UserDecisionError::Terminal)
        );
    }

    #[test]
    fn distinct_requests_without_an_idempotency_key_are_created() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let mut first = item();
        first.idempotency_key = None;
        let mut second = first.clone();
        second.decision_id = UserDecisionId::new();
        let workspace = second.owner.workspace_id;
        assert_eq!(store.create(first).unwrap().unwrap().title, "t");
        assert_eq!(store.create(second).unwrap().unwrap().title, "t");
        assert_eq!(store.pending(workspace).unwrap().len(), 2);
    }

    #[test]
    fn idempotency_comparison_checks_every_request_field() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let decision = item();
        store.create(decision.clone()).unwrap().unwrap();
        let mut variants = Vec::new();
        let mut prompt = decision.clone();
        prompt.prompt = "changed".into();
        variants.push(prompt);
        let mut options = decision.clone();
        options.options[0].label = "changed".into();
        variants.push(options);
        let mut freeform = decision.clone();
        freeform.allow_freeform = true;
        variants.push(freeform);
        let mut expiry = decision.clone();
        expiry.expires_at = Some(Utc::now());
        variants.push(expiry);
        for changed in variants {
            assert_eq!(
                store.create(changed).unwrap(),
                Err(UserDecisionError::IdempotencyConflict)
            );
        }
    }
}
