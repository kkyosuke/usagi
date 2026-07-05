//! Serde views of memories shared by the CLI (`--json`) and MCP presentations.
//!
//! The on-the-wire field set for a memory lives here once (a single source of
//! truth), so adding a field updates both surfaces at the same place rather than
//! risking a hand-duplicated `json!`/derive drifting out of sync. The shared
//! shape of these views — borrowing fields and rendering timestamps as owned
//! `String`s — is described in [`crate::usecase::view`].

use serde::Serialize;

use crate::domain::memory::{Memory, MemorySummary, MemoryType};
use crate::usecase::view::timestamp;

/// JSON view of a full memory (including the body).
#[derive(Serialize)]
pub struct MemoryView<'a> {
    pub name: &'a str,
    pub title: &'a str,
    #[serde(rename = "type")]
    pub kind: MemoryType,
    pub related: &'a [String],
    pub created_at: String,
    pub updated_at: String,
    pub body: &'a str,
}

impl<'a> From<&'a Memory> for MemoryView<'a> {
    fn from(memory: &'a Memory) -> Self {
        Self {
            name: &memory.name,
            title: &memory.title,
            kind: memory.kind,
            related: &memory.related,
            created_at: timestamp(&memory.created_at),
            updated_at: timestamp(&memory.updated_at),
            body: &memory.body,
        }
    }
}

/// JSON view of a memory summary (the index metadata, no body).
#[derive(Serialize)]
pub struct MemorySummaryView<'a> {
    pub name: &'a str,
    pub title: &'a str,
    #[serde(rename = "type")]
    pub kind: MemoryType,
    pub related: &'a [String],
    pub file: &'a str,
    pub created_at: String,
    pub updated_at: String,
}

impl<'a> From<&'a MemorySummary> for MemorySummaryView<'a> {
    fn from(summary: &'a MemorySummary) -> Self {
        Self {
            name: &summary.name,
            title: &summary.title,
            kind: summary.kind,
            related: &summary.related,
            file: &summary.file,
            created_at: timestamp(&summary.created_at),
            updated_at: timestamp(&summary.updated_at),
        }
    }
}
