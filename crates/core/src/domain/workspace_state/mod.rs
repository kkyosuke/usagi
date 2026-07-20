//! The `WorkspaceState` aggregate: everything usagi tracks for one workspace,
//! persisted as build-channel-specific runtime state (`<repo>/.usagi/dev/state.json`
//! for debug builds).
//!
//! A workspace is fully described by the sessions created under it, plus a note
//! scratchpad attached to the workspace **root** (the `⌂ root` row, which belongs
//! to no session). This is the root of the repository-local persisted state and
//! the aggregate the repo store
//! ([`crate::infrastructure::store::state`]) reads and writes.
//!
//! Per-worktree git status (branch status, diff, ahead/behind) is derived from
//! git and will attach to sessions when the git layer lands; it is intentionally
//! absent here for now.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::note::Scratchpad;
use crate::domain::session::SessionRecord;

/// State of a workspace: the sessions created under it plus the root scratchpad.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Sessions created under `.usagi/sessions/`, across all repositories in the
    /// workspace tree. Empty (and omitted from the file) when none exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionRecord>,
    /// The note scratchpad attached to the workspace **root** — the same scratch
    /// space sessions carry, but for the workspace itself (the `⌂ root` row).
    /// Empty (the default) is omitted from the file.
    #[serde(default, skip_serializing_if = "Scratchpad::is_empty")]
    pub root_notes: Scratchpad,
    /// The environment variables attached to the workspace **root** (the
    /// `⌂ root` row) — the same per-target env sessions carry, but for the
    /// workspace itself. A stable `name -> value` map; empty (the default) is
    /// omitted from the file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub root_environment: BTreeMap<String, String>,
    /// When the state was last refreshed.
    pub updated_at: DateTime<Utc>,
}

impl WorkspaceState {
    /// A fresh, empty workspace state stamped `updated_at` with the current time.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            root_notes: Scratchpad::default(),
            root_environment: BTreeMap::new(),
            updated_at: Utc::now(),
        }
    }
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
