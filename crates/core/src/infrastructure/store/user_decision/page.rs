//! Revision-fenced, byte-bounded pages of the complete pending observation.

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use crate::{
    domain::user_decision::UserDecision,
    infrastructure::ipc::{UserDecisionCursor, UserDecisionPage},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserDecisionPageError {
    RevisionConflict,
    InvalidCursor,
    PageTooSmall,
}

pub(super) fn pending_page(
    decisions: Vec<UserDecision>,
    cursor: Option<&UserDecisionCursor>,
    max_page_bytes: usize,
) -> Result<Result<UserDecisionPage, UserDecisionPageError>> {
    let revision = Sha256::digest(serde_json::to_vec(&decisions)?).iter().fold(
        String::with_capacity(64),
        |mut revision, byte| {
            let _ = write!(revision, "{byte:02x}");
            revision
        },
    );
    if let Some(cursor) = cursor {
        if cursor.revision != revision {
            return Ok(Err(UserDecisionPageError::RevisionConflict));
        }
        if cursor.offset == 0 || cursor.offset >= decisions.len() {
            return Ok(Err(UserDecisionPageError::InvalidCursor));
        }
    }
    let start = cursor.map_or(0, |cursor| cursor.offset);
    let mut page = UserDecisionPage {
        decisions: Vec::new(),
        next_cursor: None,
    };
    if decisions.is_empty() {
        return Ok(if serde_json::to_vec(&page)?.len() <= max_page_bytes {
            Ok(page)
        } else {
            Err(UserDecisionPageError::PageTooSmall)
        });
    }

    let mut record_bytes = 0;
    let mut best_end = None;
    // Serialize each record once. Re-serializing each growing candidate page
    // would make every read quadratic in caller-controlled question bytes.
    for (index, decision) in decisions.iter().enumerate().skip(start) {
        record_bytes += serde_json::to_vec(decision)?.len() + usize::from(index > start);
        let end = index + 1;
        page.next_cursor = (end < decisions.len()).then(|| UserDecisionCursor {
            revision: revision.clone(),
            offset: end,
        });
        let page_bytes = serde_json::to_vec(&page)?.len() + record_bytes;
        if page_bytes <= max_page_bytes {
            best_end = Some(end);
        }
    }
    let Some(end) = best_end else {
        return Ok(Err(UserDecisionPageError::PageTooSmall));
    };
    page.next_cursor = (end < decisions.len()).then_some(UserDecisionCursor {
        revision,
        offset: end,
    });
    page.decisions = decisions
        .into_iter()
        .skip(start)
        .take(end - start)
        .collect();
    Ok(Ok(page))
}
