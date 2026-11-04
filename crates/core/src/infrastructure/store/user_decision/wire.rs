//! Bounded transport for legacy records larger than a single IPC frame.
use super::UserDecisionPageError;
use crate::{
    domain::{
        id::{UserDecisionId, WorkspaceId},
        user_decision::UserDecision,
    },
    infrastructure::ipc::UserDecisionCursor,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

/// New admissions leave ample frame space for the envelope and future answer.
pub const MAX_RECORD_BYTES: usize = 512 * 1024;
pub const MAX_RECOVERY_RECORD_BYTES: usize = 4 * 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionReference {
    pub decision_id: UserDecisionId,
    pub workspace: WorkspaceId,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionChunk {
    pub decision_id: UserDecisionId,
    pub revision: String,
    pub offset: usize,
    /// Concatenate data in offset order, then parse the complete JSON record.
    pub data: String,
    pub next_cursor: Option<UserDecisionCursor>,
}

/// # Panics
/// Only if a JSON-serializable decision violates its serialization contract.
#[must_use]
pub fn record_bytes(decision: &UserDecision) -> usize {
    serde_json::to_vec(decision)
        .expect("decision is JSON serializable")
        .len()
}

#[must_use]
pub fn revision(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut value, byte| {
            let _ = write!(value, "{byte:02x}");
            value
        })
}

/// # Panics
/// Only if a JSON-serializable decision violates its serialization contract.
#[must_use]
pub fn reference(decision: &UserDecision) -> DecisionReference {
    DecisionReference {
        decision_id: decision.decision_id,
        workspace: decision.owner.workspace_id,
        revision: revision(
            &serde_json::to_string(decision).expect("decision is JSON serializable"),
        ),
    }
}

/// Full records retain the existing response shape. Oversized legacy records
/// use revision-fenced UTF-8 chunks, with no durable mutation or truncation.
///
/// # Panics
/// Only if the JSON-serializable record violates its serialization contract.
///
/// # Errors
/// Rejects stale or invalid cursors and budgets too small for a chunk header.
pub fn response(
    decision: &UserDecision,
    cursor: Option<&UserDecisionCursor>,
    budget: usize,
) -> Result<serde_json::Value, UserDecisionPageError> {
    let text = serde_json::to_string(decision).expect("decision is JSON serializable");
    if cursor.is_none() && text.len() <= budget {
        return Ok(serde_json::to_value(decision).expect("decision is JSON serializable"));
    }
    let revision = revision(&text);
    let start = cursor.map_or(0, |cursor| cursor.offset);
    if let Some(cursor) = cursor {
        if cursor.revision != revision {
            return Err(UserDecisionPageError::RevisionConflict);
        }
        if start >= text.len() || !text.is_char_boundary(start) {
            return Err(UserDecisionPageError::InvalidCursor);
        }
    }
    let mut chunk = DecisionChunk {
        decision_id: decision.decision_id,
        revision: revision.clone(),
        offset: start,
        data: String::new(),
        next_cursor: Some(UserDecisionCursor {
            revision,
            offset: text.len(),
        }),
    };
    let overhead = serde_json::to_vec(&chunk)
        .expect("chunk is JSON serializable")
        .len();
    // Six bytes per input byte also bounds JSON escaping. Round down to UTF-8.
    let mut end = (start + budget.saturating_sub(overhead) / 6)
        .min(start + CHUNK_BYTES)
        .min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == start {
        return Err(UserDecisionPageError::PageTooSmall);
    }
    text[start..end].clone_into(&mut chunk.data);
    if end == text.len() {
        chunk.next_cursor = None;
    } else {
        chunk
            .next_cursor
            .as_mut()
            .expect("continuation exists")
            .offset = end;
    }
    Ok(serde_json::to_value(chunk).expect("chunk is JSON serializable"))
}
