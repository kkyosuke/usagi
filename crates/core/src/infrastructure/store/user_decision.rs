//! Atomic durable decisions and their polling-delivery outbox.
//!
//! A resolve changes the decision and appends its delivery in one replaced JSON
//! document under one lock. A daemon consumer validates the event against that
//! record and acknowledges it only after delivery to the originating run.

#![allow(clippy::missing_errors_doc)] // Store IO errors follow the shared store contract.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
};

use crate::{
    domain::{
        agent::CallerRef,
        id::{UserDecisionId, WorkspaceId},
        user_decision::{
            UserDecision, UserDecisionAnswer, UserDecisionError, UserDecisionOwner,
            UserDecisionStatus,
        },
    },
    infrastructure::persistence::{json_file, store_lock::StoreLock},
};

const FILE: &str = "user-decisions.json";

/// Exact upper bound of the pretty JSON document written by this store.
///
/// Count limits alone do not bound caller-controlled prompt and answer bytes.
/// Four MiB keeps every read/parse/rewrite finite while leaving room for the
/// maximum ordinary pending backlog when requests are much smaller than their
/// individual field ceilings.
const MAX_SERIALIZED_BYTES: usize = 4 * 1024 * 1024;

/// How many resolved / cancelled / expired decisions are kept.
///
/// Every mutation rewrites this whole document, so an unbounded history makes
/// each decision cost more than the last, and a long-lived daemon pays that
/// forever. Terminal records are kept to answer recent retries; an idempotency
/// key that arrives after eviction is refused from a fixed-size tombstone rather
/// than silently creating a second human question.
const TERMINAL_RETENTION: usize = 256;

/// Newest terminal decisions protected even during byte-pressure compaction.
///
/// This is the minimum duplicate-recovery window. Older terminal records may
/// be replaced by idempotency tombstones to meet the aggregate byte budget.
const MIN_TERMINAL_RETENTION: usize = 32;

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

/// A fixed-size probabilistic set remembers evicted idempotency keys forever.
/// False positives fail closed with `IdempotencyExpired`; there are no false
/// negatives, and the durable metadata never grows with daemon lifetime.
const TOMBSTONE_WORDS: usize = 512;
const TOMBSTONE_HASHES: u64 = 4;

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
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    decisions: Vec<UserDecision>,
    events: Vec<UserDecisionResolvedEvent>,
    #[serde(default)]
    expired_idempotency: KeyTombstones,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct KeyTombstones {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    words: Vec<u64>,
}

#[derive(Clone, Copy)]
struct StoreLimits {
    max_serialized_bytes: usize,
    terminal_retention: usize,
    minimum_terminal_retention: usize,
    pending_per_workspace: usize,
    pending_global: usize,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_serialized_bytes: MAX_SERIALIZED_BYTES,
            terminal_retention: TERMINAL_RETENTION,
            minimum_terminal_retention: MIN_TERMINAL_RETENTION,
            pending_per_workspace: PENDING_LIMIT,
            pending_global: GLOBAL_PENDING_LIMIT,
        }
    }
}
pub struct UserDecisionStore {
    dir: PathBuf,
    limits: StoreLimits,
}

impl KeyTombstones {
    fn bit(owner: &UserDecisionOwner, key: &str, seed: u64) -> usize {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let owner_session = owner
            .session_id
            .map_or_else(|| "-".into(), |id| id.as_str());
        let caller_session = owner
            .caller
            .session_id
            .map_or_else(|| "-".into(), |id| id.as_str());
        let parts = [
            owner.workspace_id.as_str(),
            owner_session,
            caller_session,
            owner.caller.agent_id.as_str(),
            owner.run_id.as_str(),
            key.into(),
        ];
        for part in parts {
            for byte in part.bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        usize::try_from(hash % (TOMBSTONE_WORDS as u64 * 64)).expect("bit index fits")
    }

    fn contains(&self, owner: &UserDecisionOwner, key: &str) -> bool {
        self.words.len() == TOMBSTONE_WORDS
            && (0..TOMBSTONE_HASHES).all(|seed| {
                let bit = Self::bit(owner, key, seed);
                self.words[bit / 64] & (1_u64 << (bit % 64)) != 0
            })
    }

    fn insert(&mut self, owner: &UserDecisionOwner, key: &str) {
        self.words.resize(TOMBSTONE_WORDS, 0);
        self.words.truncate(TOMBSTONE_WORDS);
        for seed in 0..TOMBSTONE_HASHES {
            let bit = Self::bit(owner, key, seed);
            self.words[bit / 64] |= 1_u64 << (bit % 64);
        }
    }

    fn valid(&self) -> bool {
        self.words.is_empty() || self.words.len() == TOMBSTONE_WORDS
    }
}

impl UserDecisionStore {
    #[must_use]
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().into(),
            limits: StoreLimits::default(),
        }
    }
    #[cfg(test)]
    fn with_limits(dir: impl AsRef<Path>, limits: StoreLimits) -> Self {
        Self {
            dir: dir.as_ref().into(),
            limits,
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
        if let Err(error) = decision.validate_request() {
            return Ok(Err(error));
        }
        self.mutate_decision(|state| {
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
            if let Some(key) = &decision.idempotency_key
                && state.expired_idempotency.contains(&decision.owner, key)
            {
                return Err(UserDecisionError::IdempotencyExpired);
            }
            // Admission is charged before the record exists, so a refusal
            // leaves the store byte-for-byte as it was.
            let all_pending = state
                .decisions
                .iter()
                .filter(|item| item.status == UserDecisionStatus::Pending)
                .count();
            if all_pending >= self.limits.pending_global {
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
            if pending >= self.limits.pending_per_workspace {
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
        if let Err(error) = answer.validate_resource_policy() {
            return Ok(Err(error));
        }
        self.mutate_decision(|state| {
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
        self.mutate_decision(|state| {
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
        let path = self.path();
        let file = match File::open(&path) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).context(format!("failed to read {}", path.display()));
            }
        };
        let Some(mut file) = file else {
            return Ok(State::default());
        };
        let max_bytes = u64::try_from(self.limits.max_serialized_bytes)
            .expect("serialized byte limit fits u64");
        let mut text = String::new();
        file.by_ref()
            .take(max_bytes.saturating_add(1))
            .read_to_string(&mut text)
            .context(format!("failed to read {}", path.display()))?;
        if text.len() > self.limits.max_serialized_bytes {
            anyhow::bail!(
                "user decision document exceeds its {} byte hard limit",
                self.limits.max_serialized_bytes
            );
        }
        let state: State =
            serde_json::from_str(&text).context(format!("failed to parse {}", path.display()))?;
        if state
            .decisions
            .iter()
            .any(|decision| decision.validate_resource_policy().is_err())
            || state
                .events
                .iter()
                .any(|event| event.answer.validate_resource_policy().is_err())
            || !state.expired_idempotency.valid()
        {
            anyhow::bail!("user decision document violates the resource policy");
        }
        let mut pending_by_workspace = BTreeMap::new();
        for decision in state
            .decisions
            .iter()
            .filter(|decision| decision.status == UserDecisionStatus::Pending)
        {
            *pending_by_workspace
                .entry(decision.owner.workspace_id)
                .or_insert(0_usize) += 1;
        }
        if pending_by_workspace.values().sum::<usize>() > self.limits.pending_global
            || pending_by_workspace
                .values()
                .any(|pending| *pending > self.limits.pending_per_workspace)
        {
            anyhow::bail!("user decision document exceeds its pending hard limit");
        }
        Ok(state)
    }
    fn mutate<T>(&self, f: impl FnOnce(&mut State) -> T) -> Result<T> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut state = self.load()?;
        let result = f(&mut state);
        if !compact_bounded(&mut state, self.limits)? {
            anyhow::bail!("user decision store capacity is exhausted by protected records");
        }
        json_file::write_atomic(&self.dir, &self.path(), &state)?;
        Ok(result)
    }

    fn mutate_decision<T>(
        &self,
        f: impl FnOnce(&mut State) -> Result<T, UserDecisionError>,
    ) -> Result<Result<T, UserDecisionError>> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut state = self.load()?;
        let result = f(&mut state);
        if result.is_err() {
            return Ok(result);
        }
        if !compact_bounded(&mut state, self.limits)? {
            return Ok(Err(UserDecisionError::CapacityReached));
        }
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
fn compact_bounded(state: &mut State, limits: StoreLimits) -> Result<bool> {
    let referenced: Vec<UserDecisionId> =
        state.events.iter().map(|event| event.decision_id).collect();
    let evictable = |decision: &UserDecision| {
        decision.status != UserDecisionStatus::Pending
            && !referenced.contains(&decision.decision_id)
    };
    let mut evictable_count = state.decisions.iter().filter(|d| evictable(d)).count();
    let over = evictable_count.saturating_sub(limits.terminal_retention);
    if over > 0 {
        drop_oldest_evictable(state, &referenced, over);
        evictable_count -= over;
    }

    while serialized_len(state)? > limits.max_serialized_bytes {
        if evictable_count <= limits.minimum_terminal_retention {
            return Ok(false);
        }
        drop_oldest_evictable(state, &referenced, 1);
        evictable_count -= 1;
    }
    Ok(true)
}

/// Removes up to `count` append-ordered terminal records and remembers any
/// idempotency keys before the records disappear.
fn drop_oldest_evictable(state: &mut State, referenced: &[UserDecisionId], mut count: usize) {
    let mut expired_keys = Vec::new();
    state.decisions.retain(|decision| {
        let evictable = decision.status != UserDecisionStatus::Pending
            && !referenced.contains(&decision.decision_id);
        if count == 0 || !evictable {
            return true;
        }
        count -= 1;
        if let Some(key) = &decision.idempotency_key {
            expired_keys.push((decision.owner.clone(), key.clone()));
        }
        false
    });
    for (owner, key) in expired_keys {
        state.expired_idempotency.insert(&owner, &key);
    }
    debug_assert_eq!(count, 0, "evictable count was computed from the same state");
}

fn serialized_len(state: &State) -> Result<usize> {
    Ok(serde_json::to_string_pretty(state)?.len() + 1)
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
        user_decision::{UserDecisionOption, UserDecisionOwner, UserDecisionPolicy},
    };
    use chrono::TimeZone as _;
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
    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0)
            .single()
            .unwrap()
    }
    fn small_limits(max_serialized_bytes: usize, terminal_retention: usize) -> StoreLimits {
        StoreLimits {
            max_serialized_bytes,
            terminal_retention,
            minimum_terminal_retention: 0,
            pending_per_workspace: 8,
            pending_global: 16,
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
        decision.created_at = now - chrono::Duration::seconds(1);
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
    fn invalid_request_is_refused_before_the_store_changes() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let mut decision = item();
        decision.options.clear();

        assert_eq!(
            store.create(decision).unwrap(),
            Err(UserDecisionError::InvalidRequest)
        );
        assert!(!store.path().exists());
    }

    #[test]
    fn oversized_answer_is_effect_free_and_oversized_state_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        let mut decision = item();
        decision.allow_freeform = true;
        decision.idempotency_key = None;
        let workspace = decision.owner.workspace_id;
        let id = decision.decision_id;
        store.create(decision).unwrap().unwrap();
        let before = std::fs::read(store.path()).unwrap();

        assert_eq!(
            store
                .resolve(
                    workspace,
                    id,
                    UserDecisionAnswer::Freeform {
                        text: "a".repeat(UserDecisionPolicy::FREEFORM_ANSWER_MAX_BYTES + 1),
                    },
                    Utc::now(),
                )
                .unwrap(),
            Err(UserDecisionError::InvalidRequest)
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert!(store.events().unwrap().is_empty());

        let mut state = store.load().unwrap();
        state.decisions[0].title = "t".repeat(UserDecisionPolicy::TITLE_MAX_BYTES + 1);
        std::fs::write(store.path(), serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(store.get(workspace, id).is_err());
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

    #[test]
    fn exact_serialized_budget_refuses_each_growing_mutation_with_zero_effect() {
        let temp = tempfile::tempdir().unwrap();
        let mut decision = item();
        decision.created_at = fixed_now();
        let exact = serialized_len(&State {
            decisions: vec![decision.clone()],
            ..State::default()
        })
        .unwrap();
        let store = UserDecisionStore::with_limits(temp.path(), small_limits(exact, 8));
        store.create(decision.clone()).unwrap().unwrap();
        assert_eq!(std::fs::metadata(store.path()).unwrap().len(), exact as u64);
        let before = std::fs::read(store.path()).unwrap();

        assert_eq!(
            store
                .resolve(
                    decision.owner.workspace_id,
                    decision.decision_id,
                    UserDecisionAnswer::Option {
                        option_id: "a".into(),
                    },
                    fixed_now(),
                )
                .unwrap(),
            Err(UserDecisionError::CapacityReached)
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);

        let mut second = decision.clone();
        second.decision_id = UserDecisionId::new();
        second.idempotency_key = None;
        assert_eq!(
            store.create(second).unwrap(),
            Err(UserDecisionError::CapacityReached)
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);

        let oversized = tempfile::tempdir().unwrap();
        let tiny = UserDecisionStore::with_limits(oversized.path(), small_limits(64, 8));
        std::fs::write(tiny.path(), vec![b' '; 65]).unwrap();
        assert!(
            tiny.events()
                .unwrap_err()
                .to_string()
                .contains("hard limit")
        );
    }

    #[test]
    fn expiry_capacity_failure_uses_the_generic_effect_zero_transaction() {
        let temp = tempfile::tempdir().unwrap();
        let mut decision = item();
        decision.created_at = fixed_now();
        decision.expires_at = Some(fixed_now() + chrono::Duration::seconds(1));
        let exact = serialized_len(&State {
            decisions: vec![decision.clone()],
            ..State::default()
        })
        .unwrap();
        let limits = StoreLimits {
            minimum_terminal_retention: 1,
            ..small_limits(exact, 8)
        };
        let store = UserDecisionStore::with_limits(temp.path(), limits);
        store.create(decision.clone()).unwrap().unwrap();
        let before = std::fs::read(store.path()).unwrap();
        assert!(
            store
                .expire_due(fixed_now() + chrono::Duration::seconds(1))
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert_eq!(
            store
                .get(decision.owner.workspace_id, decision.decision_id)
                .unwrap()
                .unwrap()
                .status,
            UserDecisionStatus::Pending
        );
    }

    #[test]
    fn byte_eviction_tombstones_old_keys_across_restart_and_keeps_recent_retries() {
        let temp = tempfile::tempdir().unwrap();
        let mut first = item();
        first.created_at = fixed_now();
        first.prompt = "p".repeat(UserDecisionPolicy::PROMPT_MAX_BYTES);
        first.idempotency_key = Some("first-key".into());
        let mut second = first.clone();
        second.decision_id = UserDecisionId::new();
        second.idempotency_key = Some("second-key".into());

        let mut tombstones = KeyTombstones::default();
        tombstones.insert(&first.owner, "first-key");
        let compacted = State {
            decisions: vec![second.clone()],
            events: Vec::new(),
            expired_idempotency: tombstones,
        };
        let budget = serialized_len(&compacted).unwrap();
        let limits = small_limits(budget, 8);
        let store = UserDecisionStore::with_limits(temp.path(), limits);
        store.create(first.clone()).unwrap().unwrap();
        store
            .terminal(
                first.owner.workspace_id,
                first.decision_id,
                UserDecisionStatus::Cancelled,
                fixed_now(),
            )
            .unwrap()
            .unwrap();

        let mut recent_retry = first.clone();
        recent_retry.decision_id = UserDecisionId::new();
        assert_eq!(
            store.create(recent_retry).unwrap().unwrap().decision_id,
            first.decision_id
        );
        store.create(second.clone()).unwrap().unwrap();
        assert!(std::fs::metadata(store.path()).unwrap().len() <= budget as u64);

        let reopened = UserDecisionStore::with_limits(temp.path(), limits);
        let before = std::fs::read(reopened.path()).unwrap();
        let mut expired_retry = first;
        expired_retry.decision_id = UserDecisionId::new();
        assert_eq!(
            reopened.create(expired_retry).unwrap(),
            Err(UserDecisionError::IdempotencyExpired)
        );
        assert_eq!(std::fs::read(reopened.path()).unwrap(), before);

        let expected = second.decision_id;
        second.decision_id = UserDecisionId::new();
        assert_eq!(
            reopened.create(second).unwrap().unwrap().decision_id,
            expected
        );
    }

    #[test]
    fn byte_pressure_preserves_the_minimum_recent_retry_window() {
        let mut oldest = item();
        oldest.idempotency_key = None;
        oldest.status = UserDecisionStatus::Cancelled;
        oldest.resolved_at = Some(fixed_now());
        let mut newest = oldest.clone();
        newest.decision_id = UserDecisionId::new();
        newest.created_at += chrono::Duration::seconds(1);
        newest.resolved_at = Some(newest.created_at);
        let one_record_budget = serialized_len(&State {
            decisions: vec![newest.clone()],
            ..State::default()
        })
        .unwrap();
        let limits = StoreLimits {
            minimum_terminal_retention: 1,
            ..small_limits(one_record_budget, 8)
        };
        let mut state = State {
            decisions: vec![oldest, newest.clone()],
            ..State::default()
        };
        assert!(compact_bounded(&mut state, limits).unwrap());
        assert_eq!(state.decisions, vec![newest]);

        let before = state.decisions.clone();
        let impossible = StoreLimits {
            max_serialized_bytes: one_record_budget - 1,
            ..limits
        };
        assert!(!compact_bounded(&mut state, impossible).unwrap());
        assert_eq!(state.decisions, before);
    }

    #[test]
    fn small_budget_fake_clock_bounds_all_terminal_kinds_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let limits = small_limits(12 * 1024, 4);
        let store = UserDecisionStore::with_limits(temp.path(), limits);
        let workspace = WorkspaceId::new();
        let epoch = fixed_now();

        for index in 0..60 {
            let mut decision = distinct(workspace);
            let now = epoch + chrono::Duration::seconds(index * 2);
            decision.created_at = now;
            let id = decision.decision_id;
            match index % 3 {
                0 => {
                    store.create(decision).unwrap().unwrap();
                    store
                        .resolve(
                            workspace,
                            id,
                            UserDecisionAnswer::Option {
                                option_id: "a".into(),
                            },
                            now,
                        )
                        .unwrap()
                        .unwrap();
                    assert!(store.ack_event(id).unwrap());
                }
                1 => {
                    store.create(decision).unwrap().unwrap();
                    store
                        .terminal(workspace, id, UserDecisionStatus::Cancelled, now)
                        .unwrap()
                        .unwrap();
                }
                _ => {
                    decision.expires_at = Some(now + chrono::Duration::seconds(1));
                    store.create(decision).unwrap().unwrap();
                    assert_eq!(
                        store
                            .expire_due(now + chrono::Duration::seconds(1))
                            .unwrap(),
                        vec![id]
                    );
                }
            }
            assert!(std::fs::metadata(store.path()).unwrap().len() <= 12 * 1024);
        }

        let reopened = UserDecisionStore::with_limits(temp.path(), limits);
        let state = reopened.load().unwrap();
        assert!(state.events.is_empty());
        assert!(state.decisions.len() <= 4);
    }

    #[test]
    fn byte_pressure_never_discards_pending_or_unacknowledged_records() {
        let temp = tempfile::tempdir().unwrap();
        let mut awaited = item();
        awaited.created_at = fixed_now();
        awaited.idempotency_key = None;
        awaited.allow_freeform = true;
        let answer = UserDecisionAnswer::Freeform {
            text: "a".repeat(4 * 1024),
        };
        let mut resolved = awaited.clone();
        resolved.status = UserDecisionStatus::Resolved;
        resolved.answer = Some(answer.clone());
        resolved.resolved_at = Some(fixed_now());
        let expected = State {
            decisions: vec![resolved],
            events: vec![UserDecisionResolvedEvent {
                decision_id: awaited.decision_id,
                recipient: awaited.owner.caller.clone(),
                answer: answer.clone(),
                created_at: fixed_now(),
            }],
            expired_idempotency: KeyTombstones::default(),
        };
        let budget = serialized_len(&expected).unwrap();
        let store = UserDecisionStore::with_limits(temp.path(), small_limits(budget, 8));
        store.create(awaited.clone()).unwrap().unwrap();
        store
            .resolve(
                awaited.owner.workspace_id,
                awaited.decision_id,
                answer,
                fixed_now(),
            )
            .unwrap()
            .unwrap();
        let before = std::fs::read(store.path()).unwrap();

        let mut blocked = distinct(awaited.owner.workspace_id);
        blocked.created_at = fixed_now();
        assert_eq!(
            store.create(blocked.clone()).unwrap(),
            Err(UserDecisionError::CapacityReached)
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), before);
        assert!(
            store
                .get(awaited.owner.workspace_id, awaited.decision_id)
                .unwrap()
                .is_some()
        );
        assert_eq!(store.events().unwrap().len(), 1);

        assert!(store.ack_event(awaited.decision_id).unwrap());
        store.create(blocked.clone()).unwrap().unwrap();
        assert_eq!(
            store.pending(blocked.owner.workspace_id).unwrap(),
            vec![blocked]
        );
    }

    #[test]
    fn malformed_tombstones_and_forged_pending_overflow_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::with_limits(temp.path(), small_limits(64 * 1024, 8));
        let malformed = State {
            expired_idempotency: KeyTombstones { words: vec![1] },
            ..State::default()
        };
        std::fs::write(store.path(), serde_json::to_vec(&malformed).unwrap()).unwrap();
        assert!(
            store
                .load()
                .unwrap_err()
                .to_string()
                .contains("resource policy")
        );

        let workspace = WorkspaceId::new();
        let overflow = State {
            decisions: vec![distinct(workspace), distinct(workspace)],
            ..State::default()
        };
        let strict = StoreLimits {
            pending_per_workspace: 1,
            ..small_limits(64 * 1024, 8)
        };
        let store = UserDecisionStore::with_limits(temp.path(), strict);
        std::fs::write(store.path(), serde_json::to_vec(&overflow).unwrap()).unwrap();
        assert!(
            store
                .load()
                .unwrap_err()
                .to_string()
                .contains("pending hard limit")
        );

        let mut root_owner = item().owner;
        root_owner.session_id = None;
        root_owner.caller.session_id = None;
        let mut root_tombstones = KeyTombstones::default();
        root_tombstones.insert(&root_owner, "root-key");
        assert!(root_tombstones.contains(&root_owner, "root-key"));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_document_keeps_its_io_context() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let store = UserDecisionStore::new(temp.path());
        std::fs::write(store.path(), b"{}").unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let error = store.events().unwrap_err();
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        assert!(format!("{error:#}").contains("failed to read"));
    }
}
