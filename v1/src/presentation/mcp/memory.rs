//! Memory tools for the `usagi` MCP server.
//!
//! These tools let an AI agent persist and recall durable facts across sessions.
//! The surface is intentionally narrower than the `usagi memory` CLI: a single
//! `memory_save` upsert covers both creating and (partially) updating a memory,
//! so there is no separate update tool for the agent to choose between. They are
//! composed by the unified server alongside the issue tools (see
//! [`super::usagi`]), so a single `usagi mcp` process gives an agent both task
//! issues and memories for one repository.
//!
//! Each tool delegates to [`crate::usecase::memory`], keeping this an MCP
//! protocol adapter over the same business logic the CLI uses.

use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_args, to_pretty, McpService};
use crate::usecase::memory::{
    self, MemoryChanges, MemoryFilter, MemorySummaryView, MemoryView, NewMemory,
};

/// The tool names this module serves.
pub const TOOL_NAMES: [&str; 4] = [
    "memory_save",
    "memory_get",
    "memory_search",
    "memory_delete",
];

/// Names of the memory tools this module serves.
pub fn tool_names() -> &'static [&'static str] {
    &TOOL_NAMES
}

/// A JSON-RPC server exposing memory tools for one repository.
pub struct MemoryMcpServer {
    repo: std::path::PathBuf,
}

impl MemoryMcpServer {
    /// Build a server operating on the repository at `repo`.
    pub fn new(repo: impl AsRef<Path>) -> Self {
        Self {
            repo: repo.as_ref().to_path_buf(),
        }
    }

    /// Handle one JSON-RPC message (a single line of input). Returns the JSON
    /// response to write back, or `None` for notifications (which take no
    /// reply).
    pub fn handle_line(&self, line: &str) -> Option<String> {
        super::dispatch_line(self, line)
    }
}

impl McpService for MemoryMcpServer {
    fn server_name(&self) -> &str {
        "usagi-memory"
    }

    fn tool_names(&self) -> &'static [&'static str] {
        &TOOL_NAMES
    }

    fn tool_schemas(&self) -> Value {
        tool_schemas()
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        call_tool(&self.repo, name, arguments)
    }
}

/// Run a memory tool by name against the repository at `repo`.
pub fn call_tool(repo: &Path, name: &str, arguments: Value) -> Result<String, String> {
    match name {
        "memory_save" => tool_save(repo, arguments),
        "memory_get" => tool_get(repo, arguments),
        "memory_search" => tool_search(repo, arguments),
        "memory_delete" => tool_delete(repo, arguments),
        other => Err(format!("unknown tool: {other}")),
    }
}

fn tool_save(repo: &Path, arguments: Value) -> Result<String, String> {
    let args: SaveArgs = parse_args(arguments)?;
    // Map every failure (store error or the missing-title error below) to a
    // tool-facing string in one place, so the upsert body itself stays free of
    // per-call error closures.
    save_upsert(repo, args)
        .map(|saved| to_pretty(&MemoryView::from(&saved)))
        .map_err(|e| e.to_string())
}

/// The upsert behind `memory_save`: partially patch an existing memory (leaving
/// unmentioned fields as-is) or create a new one (which requires a `title`).
///
/// It reuses the usecase's `update` (partial patch) and `save` (create) so the
/// write logic stays single-sourced; `update` returning `None` is the "does not
/// exist yet" signal that routes to creation.
fn save_upsert(repo: &Path, args: SaveArgs) -> anyhow::Result<crate::domain::memory::Memory> {
    let SaveArgs { name, changes } = args;
    if let Some(updated) = memory::update(
        repo,
        &name,
        MemoryChanges {
            title: changes.title.clone(),
            kind: changes.kind,
            related: changes.related.clone(),
            body: changes.body.clone(),
        },
    )? {
        return Ok(updated);
    }
    // No memory by this name yet: create it. A title is required to open one.
    let title = changes
        .title
        .ok_or_else(|| anyhow::anyhow!("`title` is required when creating a new memory"))?;
    memory::save(
        repo,
        NewMemory {
            name,
            title,
            kind: changes.kind.unwrap_or_default(),
            related: changes.related.unwrap_or_default(),
            body: changes.body.unwrap_or_default(),
        },
    )
}

fn tool_get(repo: &Path, arguments: Value) -> Result<String, String> {
    let args: NameArgs = parse_args(arguments)?;
    match memory::get(repo, &args.name).map_err(|e| e.to_string())? {
        Some(m) => Ok(to_pretty(&MemoryView::from(&m))),
        None => Ok(to_pretty(&Value::Null)),
    }
}

fn tool_search(repo: &Path, arguments: Value) -> Result<String, String> {
    let SearchArgs { query, filter } = parse_args(arguments)?;
    // An omitted `query` lists every memory: an empty needle matches all, so one
    // code path (`search`) subsumes what a separate `list` tool would do.
    let items =
        memory::search(repo, query.as_deref().unwrap_or(""), &filter).map_err(|e| e.to_string())?;
    Ok(to_pretty(&summary_views(&items)))
}

fn tool_delete(repo: &Path, arguments: Value) -> Result<String, String> {
    let args: NameArgs = parse_args(arguments)?;
    let deleted = memory::delete(repo, &args.name).map_err(|e| e.to_string())?;
    Ok(to_pretty(&json!({ "name": args.name, "deleted": deleted })))
}

// --- argument shapes -------------------------------------------------------

#[derive(Deserialize)]
struct SaveArgs {
    name: String,
    // All content fields are optional so `memory_save` can act as a partial
    // upsert: on an existing memory only the provided fields change, while
    // creating a new one requires a `title` (enforced in the handler).
    #[serde(flatten)]
    changes: MemoryChanges,
}

#[derive(Deserialize)]
struct NameArgs {
    name: String,
}

#[derive(Deserialize)]
struct SearchArgs {
    /// Absent lists every memory (an empty needle matches all); present filters by
    /// a full-text match. Optional so the one search tool subsumes a plain list.
    #[serde(default)]
    query: Option<String>,
    #[serde(flatten)]
    filter: MemoryFilter,
}

// --- JSON serialisation ----------------------------------------------------

/// Build the JSON-output views for a list of memory summaries. The field set is
/// the SSoT [`MemorySummaryView`], shared with the CLI.
fn summary_views(items: &[crate::domain::memory::MemorySummary]) -> Vec<MemorySummaryView<'_>> {
    items.iter().map(MemorySummaryView::from).collect()
}

/// JSON Schemas for the memory tools advertised via `tools/list`.
pub fn tool_schemas() -> Value {
    let kind = json!({
        "type": "string",
        "enum": ["user", "feedback", "project", "reference"],
        "description": "user | feedback | project | reference"
    });
    let related = json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "Names of related memories"
    });

    json!([
        {
            "name": "memory_save",
            "description": "Save a durable fact to remember across sessions (upsert). \
                If a memory with this `name` already exists, only the fields you pass \
                are changed and the rest are left as-is (so you can update just the \
                body without resetting its type); otherwise a new memory is created, \
                for which `title` is required. Returns the stored memory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Stable slug identity" },
                    "title": { "type": "string", "description": "One-line summary (required when creating)" },
                    "type": kind,
                    "related": related,
                    "body": { "type": "string", "description": "Markdown body of the fact" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "memory_get",
            "description": "Fetch one memory by name (null if it does not exist).",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "memory_search",
            "description": "List memories (newest first). Give `query` to full-text \
                search names, titles and bodies (case-insensitive); omit it to list \
                every memory. Optionally filtered by type.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Full-text query; omit to list all memories" },
                    "type": kind
                }
            }
        },
        {
            "name": "memory_delete",
            "description": "Delete a memory by name.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        }
    ])
}

#[cfg(test)]
mod tests;
