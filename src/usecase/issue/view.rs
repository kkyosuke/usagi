//! Serde views of issues shared by the CLI (`--json`) and MCP presentations.
//!
//! The on-the-wire field set for an issue lives here once (a single source of
//! truth), so adding a field updates both surfaces at the same place rather than
//! risking a hand-duplicated `json!`/derive drifting out of sync. Both surfaces
//! consume these via `serde_json` (`to_string_pretty` / `to_value`).
//!
//! Timestamps are rendered with [`chrono::DateTime::to_rfc3339`] (a `+00:00`
//! offset) to match the rest of the JSON surface.

use serde::Serialize;

use super::ListedIssue;
use crate::domain::issue::{Issue, IssuePriority, IssueStatus};

/// JSON view of a full issue (including the body).
#[derive(Serialize)]
pub struct IssueView<'a> {
    pub number: u32,
    pub title: &'a str,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    pub labels: &'a [String],
    pub dependson: &'a [u32],
    pub related: &'a [u32],
    pub parent: Option<u32>,
    pub milestone: Option<&'a str>,
    pub created_at: String,
    pub updated_at: String,
    pub body: &'a str,
}

impl<'a> From<&'a Issue> for IssueView<'a> {
    fn from(issue: &'a Issue) -> Self {
        Self {
            number: issue.number,
            title: &issue.title,
            status: issue.status,
            priority: issue.priority,
            labels: &issue.labels,
            dependson: &issue.dependson,
            related: &issue.related,
            parent: issue.parent,
            milestone: issue.milestone.as_deref(),
            created_at: issue.created_at.to_rfc3339(),
            updated_at: issue.updated_at.to_rfc3339(),
            body: &issue.body,
        }
    }
}

/// JSON view of a listed issue: its metadata plus dependency readiness.
#[derive(Serialize)]
pub struct ListedIssueView<'a> {
    pub number: u32,
    pub title: &'a str,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    pub labels: &'a [String],
    pub dependson: &'a [u32],
    pub related: &'a [u32],
    pub parent: Option<u32>,
    pub milestone: Option<&'a str>,
    pub file: &'a str,
    pub created_at: String,
    pub updated_at: String,
    pub ready: bool,
    pub unmet_deps: &'a [u32],
}

impl<'a> From<&'a ListedIssue> for ListedIssueView<'a> {
    fn from(listed: &'a ListedIssue) -> Self {
        let summary = &listed.summary;
        Self {
            number: summary.number,
            title: &summary.title,
            status: summary.status,
            priority: summary.priority,
            labels: &summary.labels,
            dependson: &summary.dependson,
            related: &summary.related,
            parent: summary.parent,
            milestone: summary.milestone.as_deref(),
            file: &summary.file,
            created_at: summary.created_at.to_rfc3339(),
            updated_at: summary.updated_at.to_rfc3339(),
            ready: listed.is_ready(),
            unmet_deps: &listed.unmet_deps,
        }
    }
}
