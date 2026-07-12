//! The "recent" items the welcome screen lists: the things the user last opened,
//! either a single [`WorkspaceOverview`] or a [`UniteOverview`] — a union of
//! workspaces opened together.
//!
//! [`Recent`] is the tagged union of the two, so the presentation layer can hold
//! one `Vec<Recent>` and render each entry differently by variant. The figures a
//! unite shows are derived from its members (a plain fold), so the caller only
//! assembles the members and the domain aggregates them.

use chrono::{DateTime, Utc};

use super::workspace::WorkspaceOverview;

/// A union (unite) of workspaces the user opened together, with the at-a-glance
/// figures the recent list shows for the group.
///
/// The members are kept in the order they were opened; the first is the
/// *primary*. The group's counts are the sums across its members and its
/// last-used time is the most recent among them, all derived on demand rather
/// than stored, so a member's counts and the group's can never drift apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniteOverview {
    members: Vec<WorkspaceOverview>,
}

impl UniteOverview {
    /// Build a unite from its member workspaces (open order; first is primary).
    #[must_use]
    pub fn new(members: Vec<WorkspaceOverview>) -> Self {
        Self { members }
    }

    /// The member workspaces, in open order.
    #[must_use]
    pub fn members(&self) -> &[WorkspaceOverview] {
        &self.members
    }

    /// The primary (first) member's workspace name, or `""` when the unite is
    /// empty.
    #[must_use]
    pub fn primary_name(&self) -> &str {
        self.members
            .first()
            .map_or("", |member| member.workspace.name.as_str())
    }

    /// How many members there are beyond the primary (0 for a lone workspace).
    #[must_use]
    pub fn extra_count(&self) -> usize {
        self.members.len().saturating_sub(1)
    }

    /// The most recent `updated_at` across the members, or `None` when the unite
    /// is empty.
    #[must_use]
    pub fn updated_at(&self) -> Option<DateTime<Utc>> {
        self.members
            .iter()
            .map(|member| member.workspace.updated_at)
            .max()
    }

    /// Total sessions across the members.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.members.iter().map(|member| member.session_count).sum()
    }

    /// Total not-yet-`done` issues across the members.
    #[must_use]
    pub fn open_issue_count(&self) -> usize {
        self.members
            .iter()
            .map(|member| member.open_issue_count)
            .sum()
    }

    /// Total unique pull requests across the members.
    #[must_use]
    pub fn pr_count(&self) -> usize {
        self.members.iter().map(|member| member.pr_count).sum()
    }
}

/// One entry in the recent list: either a single workspace or a unite of
/// workspaces opened together. The welcome screen renders the two variants
/// differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recent {
    /// A single workspace and its counts.
    Workspace(WorkspaceOverview),
    /// A union of workspaces opened together, and the group's aggregated counts.
    Unite(UniteOverview),
}

impl Recent {
    /// This entry's last-used time — the workspace's own for a single workspace,
    /// the most recent member's for a unite (`None` for an empty unite). The
    /// caller sorts the recent list most-recent-first on this.
    #[must_use]
    pub fn updated_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Recent::Workspace(overview) => Some(overview.workspace.updated_at),
            Recent::Unite(unite) => unite.updated_at(),
        }
    }
}

#[cfg(test)]
mod tests;
