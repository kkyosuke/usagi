//! The tool input schemas for the issue MCP server.
//!
//! The JSON *output* shape for issues is the single source of truth in
//! [`crate::usecase::issue`] ([`IssueView`](crate::usecase::issue::IssueView) /
//! [`ListedIssueView`](crate::usecase::issue::ListedIssueView)), consumed here
//! and by the CLI alike.

use serde_json::{json, Value};

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
            "name": "issue_to_prompt",
            "description": "Render an issue as a ready-to-run agent prompt (instructing the \
                agent to implement it per the repo workflow). Feed the returned `prompt` to \
                `session_prompt` to have a session's agent work the issue. Errors if the \
                issue does not exist.",
            "inputSchema": {
                "type": "object",
                "properties": { "number": { "type": "integer" } },
                "required": ["number"]
            }
        },
        {
            "name": "issue_search",
            "description": "List issues, each annotated with dependency readiness \
                (ready = every dependency is done). Give `query` to full-text search \
                titles and bodies (case-insensitive); omit it to list every issue. \
                All filters are optional and combine with the query.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Full-text query; omit to list all issues" },
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
