//! Serde views of memories shared by the CLI (`--json`) and MCP presentations.
//!
//! The on-the-wire field set for a memory lives here once (a single source of
//! truth), so adding a field updates both surfaces at the same place rather than
//! risking a hand-duplicated `json!`/derive drifting out of sync. Both surfaces
//! consume these via `serde_json` (`to_string_pretty` / `to_value`).
//!
//! Timestamps are rendered with [`chrono::DateTime::to_rfc3339`] (a `+00:00`
//! offset) to match the rest of the JSON surface.

use serde::Serialize;

use crate::domain::memory::{Memory, MemorySummary, MemoryType};

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
            created_at: memory.created_at.to_rfc3339(),
            updated_at: memory.updated_at.to_rfc3339(),
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
            created_at: summary.created_at.to_rfc3339(),
            updated_at: summary.updated_at.to_rfc3339(),
        }
    }
}
