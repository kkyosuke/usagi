//! Memory tools for the `usagi` MCP server.
//!
//! These tools let an AI agent persist and recall durable facts across sessions.
//! The surface is intentionally narrower than the `usagi memory` CLI: a single
//! `memory_save` upsert covers both creating and (partially) updating a memory,
//! so there is no separate update tool for the agent to choose between. They are
//! merged into the same server that exposes the issue tools (see [`super::issue`]),
//! so a single `usagi mcp` process gives an agent both task issues and memories
//! for one repository.
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
        "memory_search",
        "memory_delete",
    ]
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
    // `memory_save` is the single upsert tool (it subsumes a separate update).
    // First try a partial patch: `update` reads under its own lock and touches
    // only the fields provided, so an existing memory's unmentioned fields (e.g.
    // its type) are preserved when the caller updates just the body. Reusing the
    // usecase's `update`/`save` keeps the write logic single-sourced.
    let patched = memory::update(
        repo,
        &args.name,
        MemoryChanges {
            title: args.title.clone(),
            kind: args.kind,
            related: args.related.clone(),
            body: args.body.clone(),
        },
    )
    .map_err(|e| e.to_string())?;
    let saved = match patched {
        Some(updated) => updated,
        // No memory by this name yet: create it. A title is required to open one.
        None => {
            let title = args
                .title
                .ok_or("`title` is required when creating a new memory")?;
            memory::save(
                repo,
                NewMemory {
                    name: args.name,
                    title,
                    kind: args.kind.unwrap_or_default(),
                    related: args.related.unwrap_or_default(),
                    body: args.body.unwrap_or_default(),
                },
            )
            .map_err(|e| e.to_string())?
        }
    };
    Ok(to_pretty(&MemoryView::from(&saved)))
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
    let items = memory::search(repo, query.as_deref().unwrap_or(""), &filter.filter())
        .map_err(|e| e.to_string())?;
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
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<MemoryType>,
    #[serde(default)]
    related: Option<Vec<String>>,
    #[serde(default)]
    body: Option<String>,
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
    /// Absent lists every memory (an empty needle matches all); present filters by
    /// a full-text match. Optional so the one search tool subsumes a plain list.
    #[serde(default)]
    query: Option<String>,
    #[serde(flatten)]
    filter: FilterArgs,
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
