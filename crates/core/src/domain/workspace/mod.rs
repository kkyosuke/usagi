//! The `Workspace` entity: a directory registered with usagi, addressed by a
//! unique display name and its absolute path.
//!
//! The struct is a plain value object — it carries no behaviour beyond its
//! [`Workspace::new`] constructor, which stamps the creation and update times.
//! It derives `serde` so the workspace registry (an infrastructure concern) can
//! persist it as JSON without the domain knowing where or how it is stored.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A workspace registered with usagi.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Unique display name of the workspace.
    pub name: String,
    /// Absolute path to the workspace directory.
    pub path: PathBuf,
    /// When the workspace was registered.
    pub created_at: DateTime<Utc>,
    /// When the workspace was last used or modified.
    pub updated_at: DateTime<Utc>,
}

impl Workspace {
    /// Build a workspace, stamping `created_at` and `updated_at` with the current
    /// time (both equal at creation).
    #[must_use]
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        let now = Utc::now();
        Self {
            name: name.into(),
            path: path.into(),
            created_at: now,
            updated_at: now,
        }
    }
}

/// A registered [`Workspace`] enriched with the at-a-glance figures the welcome
/// screen's recent list and the project-selection screen show beside it: how many
/// sessions it has, how many of its issues are still open, and how many pull
/// requests have been discovered across its sessions.
///
/// A plain value object: it carries the numbers already computed for a workspace,
/// so the presentation layer can render the "recent" cards without touching
/// storage. The workspace's own `updated_at` carries the last-used time, so it is
/// not duplicated here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceOverview {
    /// The workspace these figures describe.
    pub workspace: Workspace,
    /// Sessions recorded under the workspace.
    pub session_count: usize,
    /// Issues in the workspace's issue store that are not yet `done`.
    pub open_issue_count: usize,
    /// Unique pull requests recorded across the workspace's sessions.
    pub pr_count: usize,
}

impl WorkspaceOverview {
    /// Pair a workspace with its session, open-issue and pull-request counts.
    #[must_use]
    pub fn new(
        workspace: Workspace,
        session_count: usize,
        open_issue_count: usize,
        pr_count: usize,
    ) -> Self {
        Self {
            workspace,
            session_count,
            open_issue_count,
            pr_count,
        }
    }
}

#[cfg(test)]
mod tests;
