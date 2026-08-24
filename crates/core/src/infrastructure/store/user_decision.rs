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

/// How many resolved / cancelled / expired decisions are kept.
///
/// Every mutation rewrites this whole document, so an unbounded history makes
/// each decision cost more than the last, and a long-lived daemon pays that
/// forever. Terminal records are kept only to answer a retry of the request that
/// produced them: an idempotency key that arrives after eviction creates a fresh
/// decision, which is the same outcome as if the retry had never been sent.
const TERMINAL_RETENTION: usize = 256;

/// How many unanswered decisions one workspace may hold at once.
///
/// Pending records are never evicted, so this is the bound that has to refuse
/// rather than drop. It is per workspace so one busy workspace cannot starve the
/// others of the ability to ask a question.
const PENDING_LIMIT: usize = 128;

/// Hard ceiling across the daemon-wide document.
///
/// Workspaces may be retired and newly adopted without bound over the daemon's
/// lifetime, so a per-workspace ceiling alone does not bound this shared file.
const GLOBAL_PENDING_LIMIT: usize = PENDING_LIMIT * 2;

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
    /// Marks every due pending decision terminal. Expiry deliberately produces
    /// no delivery event: an answer must never be invented for a timed-out
    /// caller.
    pub fn expire_due(&self, now: DateTime<Utc>) -> Result<Vec<UserDecisionId>> {
        // Nothing due is the overwhelmingly common case for a maintenance tick.
        // Deciding that from a lock-free read keeps an idle daemon from taking
        // the store lock and fsyncing a document it would not change. The read is
        // consistent because every write replaces the file atomically, and the
        // authoritative decision is still made under the lock below.
        if !self
            .load()?
            .decisions
            .iter()
            .any(|decision| Self::is_due(decision, now))
        {
            return Ok(Vec::new());
        }
        self.mutate(|state| {
            let mut expired = Vec::new();
            for decision in &mut state.decisions {
                if Self::is_due(decision, now) {
                    decision.status = UserDecisionStatus::Expired;
                    decision.resolved_at = Some(now);
                    expired.push(decision.decision_id);
                }
            }
            expired
        })
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
            // Admission is charged before the record exists, so a refusal
            // leaves the store byte-for-byte as it was.
            let all_pending = state
                .decisions
                .iter()
                .filter(|item| item.status == UserDecisionStatus::Pending)
                .count();
            if all_pending >= GLOBAL_PENDING_LIMIT {
                return Err(UserDecisionError::PendingLimitReached);
            }
            let pending = state
                .decisions
                .iter()
                .filter(|item| {
                    item.owner.workspace_id == decision.owner.workspace_id
                        && item.status == UserDecisionStatus::Pending
                })
                .count();
            if pending >= PENDING_LIMIT {
                return Err(UserDecisionError::PendingLimitReached);
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
    fn is_due(decision: &UserDecision, now: DateTime<Utc>) -> bool {
        decision.status == UserDecisionStatus::Pending
            && decision.expires_at.is_some_and(|deadline| deadline <= now)
    }
    fn load(&self) -> Result<State> {
        Ok(json_file::read(&self.path())?.unwrap_or_default())
    }
    fn mutate<T>(&self, f: impl FnOnce(&mut State) -> T) -> Result<T> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut state = self.load()?;
        let result = f(&mut state);
        retain_bounded(&mut state);
        json_file::write_atomic(&self.dir, &self.path(), &state)?;
        Ok(result)
    }
}

/// Drop the terminal decisions past [`TERMINAL_RETENTION`], oldest first.
///
/// Three classes are never dropped, because dropping them would lose state a
/// caller is still owed:
///
/// - **pending** — someone is blocked waiting for the answer;
/// - **referenced by an un-acknowledged event** — the answer has not reached its
///   caller, and [`UserDecisionStore::get_for_event`] validates the delivery
///   against this record;
/// - **the newest terminal records** — a retry carrying the same idempotency key
///   is answered from these instead of asking the person twice.
///
/// Running on every mutation keeps the bound a property of the document rather
/// than of a maintenance tick that may never run.
fn retain_bounded(state: &mut State) {
    let referenced: Vec<UserDecisionId> =
        state.events.iter().map(|event| event.decision_id).collect();
    let evictable = |decision: &UserDecision| {
        decision.status != UserDecisionStatus::Pending
            && !referenced.contains(&decision.decision_id)
    };
    let evictable_count = state.decisions.iter().filter(|d| evictable(d)).count();
    let Some(mut over) = evictable_count.checked_sub(TERMINAL_RETENTION) else {
        return;
    };
    if over == 0 {
        return;
    }
    // `decisions` is append-ordered, so removing from the front removes the
    // oldest — the ones least likely to be retried.
    state.decisions.retain(|decision| {
        if over > 0 && evictable(decision) {
            over -= 1;
            return false;
        }
        true
    });
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
    fn compatible_resolved_event_is_consumed_once() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let decision = item();
        store.create(decision.clone()).unwrap().unwrap();
        store
            .resolve(
                decision.owner.workspace_id,
                decision.decision_id,
                UserDecisionAnswer::Option {
                    option_id: "a".into(),
                },
                Utc::now(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(store.consume_events().unwrap(), Ok(1));
        assert_eq!(store.consume_events().unwrap(), Ok(0));
    }

    #[test]
    fn an_expiry_sweep_with_nothing_due_performs_no_write() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        // With no decisions at all the sweep must not even create the document,
        // so an idle daemon's maintenance tick costs one read and no fsync.
        assert!(store.expire_due(Utc::now()).unwrap().is_empty());
        assert!(!store.path().exists());

        // A decision that is not yet due must not be rewritten either. An atomic
        // replacement always lands on a new inode, so the inode is what proves no
        // write happened rather than merely no visible change.
        let mut decision = item();
        decision.expires_at = Some(Utc::now() + chrono::Duration::seconds(60));
        store.create(decision).unwrap().unwrap();
        let before = std::fs::metadata(store.path()).unwrap().ino();
        assert!(store.expire_due(Utc::now()).unwrap().is_empty());
        assert_eq!(std::fs::metadata(store.path()).unwrap().ino(), before);
    }

    #[test]
    fn due_pending_decisions_expire_without_creating_a_delivery() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let mut decision = item();
        let now = Utc::now();
        decision.expires_at = Some(now);
        store.create(decision.clone()).unwrap().unwrap();

        assert_eq!(store.expire_due(now).unwrap(), vec![decision.decision_id]);
        assert!(store.expire_due(now).unwrap().is_empty());
        assert_eq!(
            store
                .get(decision.owner.workspace_id, decision.decision_id)
                .unwrap()
                .unwrap()
                .status,
            UserDecisionStatus::Expired
        );
        assert!(store.events().unwrap().is_empty());
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

    /// A decision for the same workspace as `item()` but with its own identity,
    /// so a test can fill the store without tripping idempotency.
    fn distinct(workspace: WorkspaceId) -> UserDecision {
        let mut decision = item();
        decision.owner.workspace_id = workspace;
        decision.decision_id = UserDecisionId::new();
        decision.idempotency_key = None;
        decision
    }

    /// Every mutation rewrites the whole document, so an unbounded history makes
    /// each decision cost more than the last for as long as the daemon lives.
    #[test]
    fn terminal_decisions_are_bounded_while_pending_ones_are_never_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let now = Utc::now();

        let mut still_pending = Vec::new();
        for index in 0..3 {
            let decision = distinct(workspace);
            store.create(decision.clone()).unwrap().unwrap();
            let _ = index;
            still_pending.push(decision.decision_id);
        }

        // Well past the retention bound, all of it terminal.
        for _ in 0..(TERMINAL_RETENTION + 50) {
            let decision = distinct(workspace);
            let id = decision.decision_id;
            store.create(decision).unwrap().unwrap();
            store
                .terminal(workspace, id, UserDecisionStatus::Cancelled, now)
                .unwrap()
                .unwrap();
        }

        let kept = store.load().unwrap().decisions;
        let terminal = kept
            .iter()
            .filter(|d| d.status != UserDecisionStatus::Pending)
            .count();
        assert!(
            terminal <= TERMINAL_RETENTION,
            "history grew past its bound: {terminal}"
        );
        // A caller blocked on an answer is never evicted to make room.
        for id in still_pending {
            assert!(
                kept.iter().any(|d| d.decision_id == id),
                "a pending decision was dropped"
            );
        }
    }

    /// An answer that has not reached its caller yet is not history: dropping it
    /// would make `get_for_event` refuse the delivery it is meant to validate.
    #[test]
    fn a_decision_with_an_unacknowledged_delivery_survives_retention() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let now = Utc::now();

        let awaited = distinct(workspace);
        let awaited_id = awaited.decision_id;
        store.create(awaited).unwrap().unwrap();
        store
            .resolve(
                workspace,
                awaited_id,
                UserDecisionAnswer::Option {
                    option_id: "a".into(),
                },
                now,
            )
            .unwrap()
            .unwrap();

        for _ in 0..(TERMINAL_RETENTION + 10) {
            let decision = distinct(workspace);
            let id = decision.decision_id;
            store.create(decision).unwrap().unwrap();
            store
                .terminal(workspace, id, UserDecisionStatus::Cancelled, now)
                .unwrap()
                .unwrap();
        }

        let event = store
            .events()
            .unwrap()
            .into_iter()
            .find(|event| event.decision_id == awaited_id)
            .expect("the delivery is still outstanding");
        assert!(
            store.get_for_event(&event).unwrap().is_some(),
            "retention dropped a record an outstanding delivery validates against"
        );
    }

    /// Pending records cannot be evicted, so saturation has to refuse the new
    /// request. The refusal must leave the store exactly as it was.
    #[test]
    fn a_saturated_workspace_refuses_new_decisions_without_any_effect() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let workspace = WorkspaceId::new();
        for _ in 0..PENDING_LIMIT {
            store.create(distinct(workspace)).unwrap().unwrap();
        }

        let before = std::fs::read(store.path()).unwrap();
        assert_eq!(
            store.create(distinct(workspace)).unwrap(),
            Err(UserDecisionError::PendingLimitReached)
        );
        assert_eq!(
            std::fs::read(store.path()).unwrap(),
            before,
            "a refused decision changed the durable document"
        );

        // Another workspace is unaffected: the bound is per workspace so one
        // busy workspace cannot stop the others from asking anything.
        let other = WorkspaceId::new();
        assert!(store.create(distinct(other)).unwrap().is_ok());
    }

    #[test]
    fn pending_decisions_are_also_bounded_across_all_workspaces() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        for workspace in [WorkspaceId::new(), WorkspaceId::new()] {
            for _ in 0..PENDING_LIMIT {
                store.create(distinct(workspace)).unwrap().unwrap();
            }
        }

        let before = std::fs::read(store.path()).unwrap();
        assert_eq!(
            store.create(distinct(WorkspaceId::new())).unwrap(),
            Err(UserDecisionError::PendingLimitReached)
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
    }
}
