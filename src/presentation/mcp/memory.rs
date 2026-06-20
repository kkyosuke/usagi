//! Memory tools for the `usagi` MCP server.
//!
//! These tools let an AI agent persist and recall durable facts across sessions,
//! mirroring the `usagi memory` CLI. They are merged into the same server that
//! exposes the issue tools (see [`super::issue`]), so a single `usagi mcp`
//! process gives an agent both task issues and memories for one repository.
//!
//! Each tool delegates to [`crate::usecase::memory`], keeping this an MCP
//! protocol adapter over the same business logic the CLI uses.

use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_args, to_pretty};
use crate::domain::memory::MemoryType;
use crate::usecase::memory::{
    self, MemoryChanges, MemoryFilter, MemorySummaryView, MemoryView, NewMemory,
};

/// The tool names this module serves.
pub fn tool_names() -> &'static [&'static str] {
    &[
        "memory_save",
        "memory_get",
        "memory_list",
        "memory_search",
        "memory_update",
        "memory_delete",
    ]
}

/// Run a memory tool by name against the repository at `repo`.
pub fn call_tool(repo: &Path, name: &str, arguments: Value) -> Result<String, String> {
    match name {
        "memory_save" => tool_save(repo, arguments),
        "memory_get" => tool_get(repo, arguments),
        "memory_list" => tool_list(repo, arguments),
        "memory_search" => tool_search(repo, arguments),
        "memory_update" => tool_update(repo, arguments),
        "memory_delete" => tool_delete(repo, arguments),
        other => Err(format!("unknown tool: {other}")),
    }
}

fn tool_save(repo: &Path, arguments: Value) -> Result<String, String> {
    let args: SaveArgs = parse_args(arguments)?;
    let saved = memory::save(
        repo,
        NewMemory {
            name: args.name,
            title: args.title,
            kind: args.kind,
            related: args.related,
            body: args.body,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(to_pretty(&MemoryView::from(&saved)))
}

fn tool_get(repo: &Path, arguments: Value) -> Result<String, String> {
    let args: NameArgs = parse_args(arguments)?;
    match memory::get(repo, &args.name).map_err(|e| e.to_string())? {
        Some(m) => Ok(to_pretty(&MemoryView::from(&m))),
        None => Ok(to_pretty(&Value::Null)),
    }
}

fn tool_list(repo: &Path, arguments: Value) -> Result<String, String> {
    let args: FilterArgs = parse_args(arguments)?;
    let items = memory::list(repo, &args.filter()).map_err(|e| e.to_string())?;
    Ok(to_pretty(&summary_views(&items)))
}

fn tool_search(repo: &Path, arguments: Value) -> Result<String, String> {
    let args: SearchArgs = parse_args(arguments)?;
    let items = memory::search(repo, &args.query, &args.filter()).map_err(|e| e.to_string())?;
    Ok(to_pretty(&summary_views(&items)))
}

fn tool_update(repo: &Path, arguments: Value) -> Result<String, String> {
    let args: UpdateArgs = parse_args(arguments)?;
    let name = args.name.clone();
    match memory::update(repo, &name, args.changes()).map_err(|e| e.to_string())? {
        Some(updated) => Ok(to_pretty(&MemoryView::from(&updated))),
        None => Err(format!("no memory '{name}'")),
    }
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
    title: String,
    #[serde(default, rename = "type")]
    kind: MemoryType,
    #[serde(default)]
    related: Vec<String>,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
struct NameArgs {
    name: String,
}

#[derive(Deserialize)]
struct FilterArgs {
    #[serde(default, rename = "type")]
    kind: Option<MemoryType>,
}

impl FilterArgs {
    fn filter(self) -> MemoryFilter {
        MemoryFilter { kind: self.kind }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default, rename = "type")]
    kind: Option<MemoryType>,
}

impl SearchArgs {
    fn filter(&self) -> MemoryFilter {
        MemoryFilter { kind: self.kind }
    }
}

#[derive(Deserialize)]
struct UpdateArgs {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<MemoryType>,
    #[serde(default)]
    related: Option<Vec<String>>,
    #[serde(default)]
    body: Option<String>,
}

impl UpdateArgs {
    fn changes(self) -> MemoryChanges {
        MemoryChanges {
            title: self.title,
            kind: self.kind,
            related: self.related,
            body: self.body,
        }
    }
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
            "description": "Save a durable fact to remember across sessions. \
                Updates the memory in place if the name already exists. Returns it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Stable slug identity" },
                    "title": { "type": "string", "description": "One-line summary" },
                    "type": kind,
                    "related": related,
                    "body": { "type": "string", "description": "Markdown body of the fact" }
                },
                "required": ["name", "title"]
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
            "name": "memory_list",
            "description": "List memories (newest first), optionally filtered by type.",
            "inputSchema": {
                "type": "object",
                "properties": { "type": kind }
            }
        },
        {
            "name": "memory_search",
            "description": "Full-text search memory names, titles and bodies \
                (case-insensitive).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "type": kind
                },
                "required": ["query"]
            }
        },
        {
            "name": "memory_update",
            "description": "Update fields of a memory. Only provided fields change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "title": { "type": "string" },
                    "type": kind,
                    "related": related,
                    "body": { "type": "string" }
                },
                "required": ["name"]
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
