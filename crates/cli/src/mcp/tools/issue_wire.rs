//! Serde-only wire projections for issue MCP tools.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use usagi_core::domain::issue::{Issue, IssueSummary};
use usagi_core::usecase::issue::{IssueFilter, IssuePatch, ListedIssue};

#[derive(Deserialize)]
pub(super) struct NumberArgs {
    pub(super) number: u32,
}

#[derive(Deserialize)]
pub(super) struct SearchArgs {
    #[serde(default)]
    pub(super) query: Option<String>,
    #[serde(flatten)]
    pub(super) filter: IssueFilter,
}

#[derive(Deserialize)]
pub(super) struct UpdateArgs {
    pub(super) number: u32,
    #[serde(flatten)]
    pub(super) patch: IssuePatch,
}

#[derive(Serialize)]
pub(super) struct IssueView<'a> {
    number: u32,
    title: &'a str,
    status: usagi_core::domain::issue::IssueStatus,
    priority: usagi_core::domain::issue::IssuePriority,
    labels: &'a [String],
    dependson: &'a [u32],
    related: &'a [u32],
    parent: Option<u32>,
    milestone: Option<&'a str>,
    created_at: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
    body: &'a str,
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
            created_at: issue.created_at,
            updated_at: issue.updated_at,
            body: &issue.body,
        }
    }
}

#[derive(Serialize)]
pub(super) struct ListedIssueView<'a> {
    #[serde(flatten)]
    summary: &'a IssueSummary,
    ambiguous: bool,
    ready: bool,
    unmet_deps: &'a [u32],
}

impl<'a> From<&'a ListedIssue> for ListedIssueView<'a> {
    fn from(issue: &'a ListedIssue) -> Self {
        Self {
            summary: &issue.summary,
            ambiguous: issue.ambiguous,
            ready: issue.is_ready(),
            unmet_deps: &issue.unmet_deps,
        }
    }
}

#[derive(Serialize)]
pub(super) struct PromptView<'a> {
    pub(super) number: u32,
    pub(super) title: &'a str,
    pub(super) prompt: String,
}
