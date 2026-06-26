//! The `Memory` entity: a single durable fact an AI agent should remember across
//! sessions, persisted as a frontmatter markdown file under
//! `<repo>/.usagi/memory/`.
//!
//! Where an [`crate::domain::issue::Issue`] tracks a task (something to *do*), a
//! memory captures knowledge that cannot be derived from the code or git history
//! — the user's preferences, working agreements, project constraints, or
//! pointers to external resources. Each memory is one `<name>.md` file: a small
//! line-based frontmatter (the metadata) followed by a free-form markdown body.
//!
//! The `name` is the memory's stable identity and also its filename, so a memory
//! is addressed by a human-readable slug rather than an assigned number. Parsing
//! and serialization are hand-rolled over a fixed set of fields, mirroring the
//! issue store, to keep the dependency surface small while staying fully
//! testable.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What kind of knowledge a memory holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryType {
    /// Who the user is (role, expertise, preferences).
    User,
    /// Guidance on how to work (corrections, confirmed approaches).
    Feedback,
    /// Ongoing work, goals or constraints not derivable from the code.
    #[default]
    Project,
    /// A pointer to an external resource (URL, dashboard, ticket).
    Reference,
}

impl MemoryType {
    /// The on-disk / display token for this type.
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::User => "user",
            MemoryType::Feedback => "feedback",
            MemoryType::Project => "project",
            MemoryType::Reference => "reference",
        }
    }
}

impl fmt::Display for MemoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MemoryType {
    type Err = ParseMemoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "user" => Ok(MemoryType::User),
            "feedback" => Ok(MemoryType::Feedback),
            "project" => Ok(MemoryType::Project),
            "reference" => Ok(MemoryType::Reference),
            other => Err(ParseMemoryError(format!("invalid type: {other:?}"))),
        }
    }
}

/// A single durable fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    /// Stable, filename-safe identity (also the filename stem).
    pub name: String,
    /// One-line summary of the fact.
    pub title: String,
    /// What kind of knowledge this is.
    pub kind: MemoryType,
    /// Names of related memories (a soft, non-blocking cross-reference).
    pub related: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Markdown body below the frontmatter.
    pub body: String,
}

/// Lightweight metadata view of a [`Memory`] — everything except the body — as
/// stored in the JSON index and surfaced by listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySummary {
    pub name: String,
    pub title: String,
    #[serde(rename = "type")]
    pub kind: MemoryType,
    #[serde(default)]
    pub related: Vec<String>,
    /// File name (relative to the memory directory) backing this memory.
    pub file: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// An error parsing a memory's markdown frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseMemoryError(pub String);

impl fmt::Display for ParseMemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParseMemoryError {}

impl From<crate::domain::frontmatter::ParseFrontmatterError> for ParseMemoryError {
    fn from(e: crate::domain::frontmatter::ParseFrontmatterError) -> Self {
        ParseMemoryError(e.0)
    }
}

/// Turn an arbitrary string into a filename-safe slug: lowercase, with every run
/// of non-alphanumeric characters collapsed to a single hyphen. Falls back to
/// `"memory"` when the input has no usable characters.
pub fn slugify(text: &str) -> String {
    crate::domain::frontmatter::slugify(text, "memory")
}

impl Memory {
    /// The file name backing this memory, e.g. `user-prefers-tabs.md`.
    ///
    /// `name` is interpolated into the path verbatim, so it must already be a
    /// filename-safe slug — the entity does not enforce this itself. Callers that
    /// build a `Memory` from user input go through [`slugify`]; a `Memory` parsed
    /// from a hand-edited file via [`Memory::from_markdown`] carries whatever
    /// `name` the file declared, so the store guards against a traversing name
    /// (`../…`) as defense in depth rather than relying on this method.
    pub fn file_name(&self) -> String {
        format!("{}.md", self.name)
    }

    /// Build the metadata summary for this memory.
    pub fn summary(&self) -> MemorySummary {
        MemorySummary {
            name: self.name.clone(),
            title: self.title.clone(),
            kind: self.kind,
            related: self.related.clone(),
            file: self.file_name(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

mod markdown;

#[cfg(test)]
mod tests;
