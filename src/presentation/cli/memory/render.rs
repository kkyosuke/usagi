//! Output formatting for `usagi memory`: human-readable listings and the
//! `--json` serialisations.

use anyhow::Result;
use serde::Serialize;

use crate::domain::memory::{Memory, MemorySummary, MemoryType};

/// Render a listing (from `list` or `search`) either as JSON or as aligned
/// human-readable lines.
pub(super) fn render_listing(items: Vec<MemorySummary>, json: bool) -> Result<Vec<String>> {
    if json {
        let views: Vec<SummaryJson> = items.iter().map(SummaryJson::from).collect();
        return json_lines(&views);
    }
    Ok(render_list(&items))
}

/// Format a listing as aligned, one-line-per-memory text.
fn render_list(items: &[MemorySummary]) -> Vec<String> {
    if items.is_empty() {
        return vec!["No memories found.".to_string()];
    }
    items
        .iter()
        .map(|s| format!("{:<12} {:<24} {}", s.kind.as_str(), s.name, s.title))
        .collect()
}

/// Serialize `value` to pretty JSON and return it split into lines.
pub(super) fn json_lines<T: Serialize>(value: &T) -> Result<Vec<String>> {
    let text = serde_json::to_string_pretty(value)?;
    Ok(text.lines().map(str::to_string).collect())
}

/// JSON view of a full memory (including the body).
#[derive(Serialize)]
pub(super) struct MemoryJson<'a> {
    name: &'a str,
    title: &'a str,
    #[serde(rename = "type")]
    kind: MemoryType,
    related: &'a [String],
    created_at: String,
    updated_at: String,
    body: &'a str,
}

pub(super) fn memory_json(memory: &Memory) -> MemoryJson<'_> {
    MemoryJson {
        name: &memory.name,
        title: &memory.title,
        kind: memory.kind,
        related: &memory.related,
        created_at: memory.created_at.to_rfc3339(),
        updated_at: memory.updated_at.to_rfc3339(),
        body: &memory.body,
    }
}

/// JSON view of a memory summary (the index metadata, no body).
#[derive(Serialize)]
struct SummaryJson<'a> {
    #[serde(flatten)]
    summary: &'a MemorySummary,
}

impl<'a> From<&'a MemorySummary> for SummaryJson<'a> {
    fn from(summary: &'a MemorySummary) -> Self {
        SummaryJson { summary }
    }
}
