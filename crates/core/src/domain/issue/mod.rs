//! The `Issue` entity: a unit of task tracking persisted as a frontmatter
//! markdown file under `<repo>/.usagi/issues/`.
//!
//! Each issue is a single `NNN-<slug>.md` file whose top block is a small,
//! line-based frontmatter (the metadata) followed by a free-form markdown body.
//! The format mirrors the hand-written issues this project already keeps under
//! `.usagi/issues/`, so the same files read well to both humans and agents.
//!
//! Parsing and serialization are hand-rolled over a fixed, known set of fields
//! rather than pulling in a YAML crate: the project standardizes on JSON for
//! machine data (see `document/`), and a focused parser keeps the dependency
//! surface small while staying fully testable.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::frontmatter::str_enum;

/// Where an issue sits in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IssueStatus {
    /// Not started.
    #[default]
    Todo,
    /// Being worked on.
    InProgress,
    /// Finished.
    Done,
}

str_enum!(IssueStatus, ParseIssueError, "status", {
    Todo => "todo",
    InProgress => "in-progress",
    Done => "done",
});

/// How urgent an issue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IssuePriority {
    High,
    #[default]
    Medium,
    Low,
}

str_enum!(IssuePriority, ParseIssueError, "priority", {
    High => "high",
    Medium => "medium",
    Low => "low",
});

/// A single tracked task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Stable, monotonically assigned number (also the filename prefix).
    pub number: u32,
    pub title: String,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    /// Free-form labels.
    pub labels: Vec<String>,
    /// Numbers of issues that must be `done` before this one can start.
    pub dependson: Vec<u32>,
    /// Numbers of issues related to this one without blocking it (a soft,
    /// non-blocking cross-reference, unlike `dependson`).
    pub related: Vec<u32>,
    /// Number of the parent issue this one belongs to (an epic ⊃ sub-task
    /// hierarchy), if any. Distinct from `dependson`, which is a precondition.
    pub parent: Option<u32>,
    /// Milestone this issue is grouped under, if any.
    pub milestone: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Markdown body below the frontmatter.
    pub body: String,
}

/// Lightweight metadata view of an [`Issue`] — everything except the body — as
/// stored in the JSON index and surfaced by listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueSummary {
    pub number: u32,
    pub title: String,
    pub status: IssueStatus,
    pub priority: IssuePriority,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub dependson: Vec<u32>,
    #[serde(default)]
    pub related: Vec<u32>,
    #[serde(default)]
    pub parent: Option<u32>,
    #[serde(default)]
    pub milestone: Option<String>,
    /// File name (relative to the issues directory) backing this issue.
    pub file: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// An error parsing an issue's markdown frontmatter. Unified with the memory
/// parse error as a single [`crate::domain::frontmatter::ParseError`]; kept under
/// this name for the issue module's public API.
pub use crate::domain::frontmatter::ParseError as ParseIssueError;

impl Issue {
    /// A filename-safe slug derived from the title: lowercase, with every run of
    /// non-alphanumeric characters collapsed to a single hyphen. Falls back to
    /// `"issue"` when the title has no usable characters.
    #[must_use]
    pub fn slug(&self) -> String {
        crate::domain::frontmatter::slugify(&self.title, "issue")
    }

    /// The file name backing this issue, e.g. `001-add-doctor.md`.
    #[must_use]
    pub fn file_name(&self) -> String {
        format!("{:03}-{}.md", self.number, self.slug())
    }

    /// Build the metadata summary for this issue.
    #[must_use]
    pub fn summary(&self) -> IssueSummary {
        IssueSummary {
            number: self.number,
            title: self.title.clone(),
            status: self.status,
            priority: self.priority,
            labels: self.labels.clone(),
            dependson: self.dependson.clone(),
            related: self.related.clone(),
            parent: self.parent,
            milestone: self.milestone.clone(),
            file: self.file_name(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

mod markdown;

#[cfg(test)]
mod tests;
