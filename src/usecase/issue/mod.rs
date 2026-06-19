//! Business logic for task issues: create, read, update, delete, list and
//! search issues stored under `<repo>/.usagi/issues/`.
//!
//! Listing and searching annotate each issue with its dependency *readiness* —
//! whether every issue it `dependson` is already `done` — so callers can surface
//! the tasks that are actually ready to pick up.

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use chrono::Utc;

use crate::domain::issue::{Issue, IssuePriority, IssueStatus, IssueSummary};
use crate::infrastructure::issue_store::IssueStore;

mod gantt;
mod stats;
mod tree;

pub use gantt::gantt;
pub use stats::{group, GroupBy, IssueStats};
pub use tree::dependency_tree;

/// Fields needed to open a new issue. The number and timestamps are assigned by
/// [`create`].
pub struct NewIssue {
    pub title: String,
    pub priority: IssuePriority,
    pub labels: Vec<String>,
    pub dependson: Vec<u32>,
    pub related: Vec<u32>,
    pub parent: Option<u32>,
    pub milestone: Option<String>,
    pub body: String,
}

/// A partial update to an existing issue: every `Some` field is applied, every
/// `None` field is left unchanged.
#[derive(Default)]
pub struct IssueChanges {
    pub title: Option<String>,
    pub status: Option<IssueStatus>,
    pub priority: Option<IssuePriority>,
    pub labels: Option<Vec<String>>,
    pub dependson: Option<Vec<u32>>,
    pub related: Option<Vec<u32>>,
    /// Outer `None` leaves the parent unchanged; `Some(None)` clears it;
    /// `Some(Some(n))` sets it.
    pub parent: Option<Option<u32>>,
    /// Outer `None` leaves the milestone unchanged; `Some(None)` clears it;
    /// `Some(Some(name))` sets it.
    pub milestone: Option<Option<String>>,
    pub body: Option<String>,
}

impl IssueChanges {
    /// Whether this update would change anything.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.status.is_none()
            && self.priority.is_none()
            && self.labels.is_none()
            && self.dependson.is_none()
            && self.related.is_none()
            && self.parent.is_none()
            && self.milestone.is_none()
            && self.body.is_none()
    }
}

/// Filters applied to listings and searches. An unset field matches everything.
#[derive(Default)]
pub struct IssueFilter {
    pub status: Option<IssueStatus>,
    pub priority: Option<IssuePriority>,
    pub label: Option<String>,
    pub parent: Option<u32>,
    pub milestone: Option<String>,
    /// Keep only issues that are ready to start (not done, all deps done).
    pub ready_only: bool,
}

impl IssueFilter {
    fn matches(&self, listed: &ListedIssue) -> bool {
        let s = &listed.summary;
        self.status.is_none_or(|status| s.status == status)
            && self.priority.is_none_or(|priority| s.priority == priority)
            && self
                .label
                .as_ref()
                .is_none_or(|label| s.labels.iter().any(|l| l == label))
            && self.parent.is_none_or(|parent| s.parent == Some(parent))
            && self
                .milestone
                .as_ref()
                .is_none_or(|milestone| s.milestone.as_deref() == Some(milestone.as_str()))
            && (!self.ready_only || listed.is_ready())
    }
}

/// An issue summary annotated with dependency readiness.
pub struct ListedIssue {
    pub summary: IssueSummary,
    /// Numbers this issue depends on that are not yet `done` (including any that
    /// no longer exist).
    pub unmet_deps: Vec<u32>,
}

impl ListedIssue {
    /// Ready to start: not already done, with every dependency done.
    pub fn is_ready(&self) -> bool {
        self.summary.status != IssueStatus::Done && self.unmet_deps.is_empty()
    }
}

/// Create a new issue, assigning the next number and timestamps. Returns the
/// stored issue.
pub fn create(repo_root: &Path, new: NewIssue) -> Result<Issue> {
    let store = IssueStore::new(repo_root);
    let now = Utc::now();
    let issue = Issue {
        number: store.max_number()? + 1,
        title: new.title,
        status: IssueStatus::Todo,
        priority: new.priority,
        labels: new.labels,
        dependson: new.dependson,
        related: new.related,
        parent: new.parent,
        milestone: new.milestone,
        created_at: now,
        updated_at: now,
        body: new.body,
    };
    store.write(&issue)?;
    Ok(issue)
}

/// Fetch a single issue by number.
pub fn get(repo_root: &Path, number: u32) -> Result<Option<Issue>> {
    IssueStore::new(repo_root).read(number)
}

/// List issues matching `filter`, annotated with dependency readiness.
pub fn list(repo_root: &Path, filter: &IssueFilter) -> Result<Vec<ListedIssue>> {
    let all = IssueStore::new(repo_root).summaries()?;
    let done = done_numbers(all.iter().map(|s| (s.number, s.status)));
    Ok(annotate(all, &done)
        .into_iter()
        .filter(|l| filter.matches(l))
        .collect())
}

/// Full-text search issue titles and bodies (case-insensitive), then apply
/// `filter`. Results are annotated with dependency readiness.
pub fn search(repo_root: &Path, query: &str, filter: &IssueFilter) -> Result<Vec<ListedIssue>> {
    let issues = IssueStore::new(repo_root).scan()?;
    let done = done_numbers(issues.iter().map(|i| (i.number, i.status)));
    // Case-fold with Unicode-aware `to_lowercase` and match on `str::contains`,
    // so the fold works for non-ASCII text (the UI is Japanese) and a multi-byte
    // needle can never match across a character boundary — both of which the
    // previous ASCII byte-window matching got wrong.
    let needle = query.to_lowercase();
    let matched: Vec<IssueSummary> = issues
        .into_iter()
        .filter(|i| {
            if needle.is_empty() {
                return true;
            }
            i.title.to_lowercase().contains(&needle) || i.body.to_lowercase().contains(&needle)
        })
        .map(|i| i.summary())
        .collect();
    Ok(annotate(matched, &done)
        .into_iter()
        .filter(|l| filter.matches(l))
        .collect())
}

/// Apply `changes` to the issue with `number`. Returns the updated issue, or
/// `None` if no such issue exists.
pub fn update(repo_root: &Path, number: u32, changes: IssueChanges) -> Result<Option<Issue>> {
    let store = IssueStore::new(repo_root);
    let Some(mut issue) = store.read(number)? else {
        return Ok(None);
    };
    if let Some(title) = changes.title {
        issue.title = title;
    }
    if let Some(status) = changes.status {
        issue.status = status;
    }
    if let Some(priority) = changes.priority {
        issue.priority = priority;
    }
    if let Some(labels) = changes.labels {
        issue.labels = labels;
    }
    if let Some(dependson) = changes.dependson {
        issue.dependson = dependson;
    }
    if let Some(related) = changes.related {
        issue.related = related;
    }
    if let Some(parent) = changes.parent {
        issue.parent = parent;
    }
    if let Some(milestone) = changes.milestone {
        issue.milestone = milestone;
    }
    if let Some(body) = changes.body {
        issue.body = body;
    }
    issue.updated_at = Utc::now();
    store.write(&issue)?;
    Ok(Some(issue))
}

/// Delete the issue with `number`, returning whether it existed.
pub fn delete(repo_root: &Path, number: u32) -> Result<bool> {
    IssueStore::new(repo_root).remove(number)
}

/// Annotate an in-memory set of issues with dependency readiness, without
/// touching disk. Used by callers (e.g. the TUI) that already hold the issues.
pub fn annotate_all(issues: &[Issue]) -> Vec<ListedIssue> {
    let done = done_numbers(issues.iter().map(|i| (i.number, i.status)));
    annotate(issues.iter().map(Issue::summary).collect(), &done)
}

/// Collect the set of issue numbers whose status is `done`.
fn done_numbers(items: impl Iterator<Item = (u32, IssueStatus)>) -> HashSet<u32> {
    items
        .filter(|(_, status)| *status == IssueStatus::Done)
        .map(|(number, _)| number)
        .collect()
}

/// Pair each summary with the dependency numbers that are not yet done.
fn annotate(summaries: Vec<IssueSummary>, done: &HashSet<u32>) -> Vec<ListedIssue> {
    summaries
        .into_iter()
        .map(|summary| {
            let unmet_deps = summary
                .dependson
                .iter()
                .copied()
                .filter(|d| !done.contains(d))
                .collect();
            ListedIssue {
                summary,
                unmet_deps,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
