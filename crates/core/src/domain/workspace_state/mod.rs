//! Repository-local compatibility state persisted at
//! `<repo>/.usagi/dev/state.json` in development mode.
//!
//! [`WorkspaceState`] retains the legacy session projection needed to locate
//! per-session scratchpads, plus the scratchpad attached to the workspace root
//! (`⌂ root`). It does **not** fully describe a workspace: production session
//! membership/lifecycle is daemon-owned, while Git status and PR state are
//! derived projections. The repository store
//! ([`crate::infrastructure::store::state`]) reads and writes this compatibility
//! document without promoting its session rows to lifecycle authority.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::note::Scratchpad;
use crate::domain::session::SessionRecord;

/// Legacy session/scratchpad compatibility state for one repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Legacy session rows used for repository-local scratchpad lookup.
    /// Production lifecycle membership is not derived from this list. Empty (and
    /// omitted from the file) when none exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionRecord>,
    /// The note scratchpad attached to the workspace **root** — the same scratch
    /// space sessions carry, but for the workspace itself (the `⌂ root` row).
    /// Empty (the default) is omitted from the file.
    #[serde(default, skip_serializing_if = "Scratchpad::is_empty")]
    pub root_notes: Scratchpad,
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
