//! Revision-fenced, byte-bounded pages of a scoped pending observation.

use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use crate::{
    domain::{
        id::WorkspaceId,
        user_decision::{UserDecision, UserDecisionOwner},
    },
    infrastructure::ipc::{UserDecisionCursor, UserDecisionPage},
};

/// Selected by the authenticated dispatcher, never by an agent's payload.
#[derive(Clone, Copy)]
pub enum UserDecisionScope<'a> {
    All,
    Workspace(WorkspaceId),
    Owners(&'a [UserDecisionOwner]),
}

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
        deferred: Vec::new(),
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

    let references: Vec<_> = decisions
        .iter()
        .enumerate()
        .map(|(index, decision)| {
            let bytes = super::wire::record_bytes(decision);
            page.next_cursor = (index + 1 < decisions.len()).then(|| UserDecisionCursor {
                revision: revision.clone(),
                offset: index + 1,
            });
            let overhead = serde_json::to_vec(&page)?.len();
            Ok((bytes > super::wire::MAX_RECORD_BYTES
                || (index == start && bytes + overhead > max_page_bytes))
                .then(|| super::wire::reference(decision)))
        })
        .collect::<Result<_>>()?;
    let mut record_bytes = 0;
    let mut deferred_count = 0;
    let mut full_count = 0;
    let mut best_end = None;
    // Measure entries independently. Re-serializing each growing candidate page
    // would make every read quadratic in caller-controlled question bytes.
    for (index, decision) in decisions.iter().enumerate().skip(start) {
        if let Some(reference) = &references[index] {
            record_bytes += serde_json::to_vec(reference)?.len() + usize::from(deferred_count > 0);
            deferred_count += 1;
        } else {
            record_bytes += serde_json::to_vec(decision)?.len() + usize::from(full_count > 0);
            full_count += 1;
        }
        let end = index + 1;
        page.next_cursor = (end < decisions.len()).then(|| UserDecisionCursor {
            revision: revision.clone(),
            offset: end,
        });
        let page_bytes = serde_json::to_vec(&page)?.len()
            + record_bytes
            + if deferred_count > 0 {
                ",\"deferred\":[]".len()
            } else {
                0
            };
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
    for (decision, reference) in decisions
        .into_iter()
        .zip(references)
        .skip(start)
        .take(end - start)
    {
        if let Some(reference) = reference {
            page.deferred.push(reference);
        } else {
            page.decisions.push(decision);
        }
    }
    Ok(Ok(page))
}
