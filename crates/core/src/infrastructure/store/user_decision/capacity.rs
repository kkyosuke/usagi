//! Admission reserves the largest legal answer, its outbox copy and terminal metadata.
use super::{State, StoreLimits, TOMBSTONE_WORDS, serialized_len};
use crate::domain::user_decision::UserDecisionPolicy as Policy;
use crate::domain::user_decision::{UserDecision, UserDecisionStatus};
use anyhow::Result;

// JSON can escape one input byte as six bytes. Timestamp, outbox identity,
// indentation, array delimiters and recovery metadata fit within this margin.
const TERMINAL_OVERHEAD: usize = 1024;
const TOMBSTONE_BOUND: usize = TOMBSTONE_WORDS * 32 + 128;
const MAX_ANSWER_BOUND: usize = Policy::FREEFORM_ANSWER_MAX_BYTES * 6 + 256;

pub(super) fn answer_bound(decision: &UserDecision) -> usize {
    let options: usize = decision
        .options
        .iter()
        .map(|option| option.id.len() * 6 + 16)
        .sum();
    let comment = usize::from(decision.allow_comment) * Policy::COMMENT_MAX_BYTES * 6;
    let freeform = usize::from(decision.allow_freeform) * Policy::FREEFORM_ANSWER_MAX_BYTES * 6;
    (options + comment).max(freeform) + 256
}

pub(super) fn reserved_len(state: &State) -> Result<usize> {
    let pending: usize = state
        .decisions
        .iter()
        .filter(|decision| decision.status == UserDecisionStatus::Pending)
        .map(|decision| 2 * answer_bound(decision) + TERMINAL_OVERHEAD)
        .sum();
    let tombstones = if state
        .decisions
        .iter()
        .any(|decision| decision.idempotency_key.is_some())
    {
        TOMBSTONE_BOUND.saturating_sub(serde_json::to_vec_pretty(&state.expired_idempotency)?.len())
    } else {
        0
    };
    Ok(serialized_len(state)? + pending + tombstones)
}

pub(super) fn recovery_limit(limits: StoreLimits) -> usize {
    limits.max_serialized_bytes
        + limits.pending_global * (2 * MAX_ANSWER_BOUND + TERMINAL_OVERHEAD)
        + TOMBSTONE_BOUND
        + 64
}

pub(super) fn recover(state: &mut State, limits: StoreLimits, file_bytes: usize) -> Result<()> {
    let hard_limit = recovery_limit(limits);
    if state.recovery_bytes > hard_limit - limits.max_serialized_bytes
        || file_bytes
            > limits
                .max_serialized_bytes
                .saturating_add(state.recovery_bytes)
    {
        anyhow::bail!("user decision document exceeds its recovery hard limit");
    }
    // A legacy document was bounded by present bytes only. Give exactly its
    // existing obligations headroom; subsequent admissions still use 4 MiB.
    if state.recovery_bytes == 0 {
        let reserved = reserved_len(state)?;
        if reserved > limits.max_serialized_bytes {
            state.recovery_bytes = reserved - limits.max_serialized_bytes + 64;
        }
    }
    if reserved_len(state)? > limits.max_serialized_bytes + state.recovery_bytes {
        anyhow::bail!("user decision document has insufficient reserved capacity");
    }
    Ok(())
}
