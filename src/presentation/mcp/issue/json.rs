//! JSON serialisation of issues and the tool input schemas for the issue MCP
//! server.

use serde_json::{json, Value};

use crate::domain::issue::Issue;
use crate::usecase::issue::ListedIssue;

pub(super) fn issue_to_json(issue: &Issue) -> Value {
    json!({
        "number": issue.number,
        "title": issue.title,
        "status": issue.status,
        "priority": issue.priority,
        "labels": issue.labels,
        "dependson": issue.dependson,
        "related": issue.related,
        "parent": issue.parent,
        "milestone": issue.milestone,
        "created_at": issue.created_at.to_rfc3339(),
        "updated_at": issue.updated_at.to_rfc3339(),
        "body": issue.body,
    })
}

pub(super) fn listed_to_json(items: &[ListedIssue]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|l| {
                json!({
                    "number": l.summary.number,
                    "title": l.summary.title,
                    "status": l.summary.status,
                    "priority": l.summary.priority,
                    "labels": l.summary.labels,
                    "dependson": l.summary.dependson,
                    "related": l.summary.related,
                    "parent": l.summary.parent,
                    "milestone": l.summary.milestone,
                    "file": l.summary.file,
                    "created_at": l.summary.created_at.to_rfc3339(),
                    "updated_at": l.summary.updated_at.to_rfc3339(),
                    "ready": l.is_ready(),
                    "unmet_deps": l.unmet_deps,
                })
            })
            .collect(),
    )
}

pub(super) fn to_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// JSON Schemas for the issue tools advertised via `tools/list`.
pub(super) fn issue_tool_schemas() -> Value {
    let status = json!({ "type": "string", "enum": ["todo", "in-progress", "done"] });
    let priority = json!({ "type": "string", "enum": ["high", "medium", "low"] });
    let labels = json!({ "type": "array", "items": { "type": "string" } });
    let deps = json!({ "type": "array", "items": { "type": "integer" } });
    let related = json!({
        "type": "array",
        "items": { "type": "integer" },
        "description": "Related (non-blocking) issue numbers"
    });
    let parent = json!({ "type": "integer", "description": "Parent issue number" });
    let milestone = json!({ "type": "string", "description": "Milestone name" });

    json!([
        {
            "name": "issue_create",
            "description": "Create a new task issue. Returns the created issue.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "priority": priority,
                    "labels": labels,
                    "dependson": deps,
                    "related": related,
                    "parent": parent,
                    "milestone": milestone,
                    "body": { "type": "string", "description": "Markdown body" }
                },
                "required": ["title"]
            }
        },
        {
            "name": "issue_get",
            "description": "Fetch one issue by number (null if it does not exist).",
            "inputSchema": {
                "type": "object",
                "properties": { "number": { "type": "integer" } },
                "required": ["number"]
            }
        },
        {
            "name": "issue_list",
            "description": "List issues, each annotated with dependency readiness \
                (ready = every dependency is done). Optional filters.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": status,
                    "priority": priority,
                    "label": { "type": "string" },
                    "parent": { "type": "integer", "description": "Keep only children of this issue" },
                    "milestone": { "type": "string", "description": "Keep only issues in this milestone" },
                    "ready": { "type": "boolean", "description": "Only issues ready to start" }
                }
            }
        },
        {
            "name": "issue_search",
            "description": "Full-text search issue titles and bodies (case-insensitive).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "status": status,
                    "priority": priority,
                    "label": { "type": "string" },
                    "parent": { "type": "integer", "description": "Keep only children of this issue" },
                    "milestone": { "type": "string", "description": "Keep only issues in this milestone" },
                    "ready": { "type": "boolean" }
                },
                "required": ["query"]
            }
        },
        {
            "name": "issue_update",
            "description": "Update fields of an issue. Only provided fields change.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "number": { "type": "integer" },
                    "title": { "type": "string" },
                    "status": status,
                    "priority": priority,
                    "labels": labels,
                    "dependson": deps,
                    "related": related,
                    "parent": { "type": ["integer", "null"], "description": "Parent issue number; null clears it" },
                    "milestone": { "type": ["string", "null"], "description": "Milestone name; null clears it" },
                    "body": { "type": "string" }
                },
                "required": ["number"]
            }
        },
        {
            "name": "issue_delete",
            "description": "Delete an issue by number.",
            "inputSchema": {
                "type": "object",
                "properties": { "number": { "type": "integer" } },
                "required": ["number"]
            }
        }
    ])
}
