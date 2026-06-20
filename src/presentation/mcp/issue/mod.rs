//! The `usagi` MCP server, exposing a repository's task issues and durable
//! memories as tools.
//!
//! Every tool delegates to [`crate::usecase::issue`] or [`crate::usecase::memory`],
//! so the MCP surface stays a thin protocol adapter over the same business logic
//! the CLI uses. The issue tools live in this file; the memory tools are supplied
//! by [`super::memory`] and merged in by [`McpServer::tool_schemas`] and
//! [`McpServer::call_tool`], so a single `usagi mcp` process serves both. The
//! JSON-RPC framing is shared with the other servers and lives in the parent
//! [`super`] module.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use super::McpService;
use crate::domain::issue::{IssuePriority, IssueStatus};
use crate::usecase::issue::{self, IssueChanges, IssueFilter, NewIssue};

/// A JSON-RPC server exposing issue tools for one repository.
pub struct McpServer {
    repo: PathBuf,
}

impl McpServer {
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

    fn tool_create(&self, arguments: Value) -> Result<String, String> {
        let args: CreateArgs = parse_args(arguments)?;
        let created = issue::create(
            &self.repo,
            NewIssue {
                title: args.title,
                priority: args.priority,
                labels: args.labels,
                dependson: args.dependson,
                related: args.related,
                parent: args.parent,
                milestone: args.milestone,
                body: args.body,
            },
        )
        .map_err(|e| e.to_string())?;
        Ok(to_pretty(&issue_to_json(&created)))
    }

    fn tool_get(&self, arguments: Value) -> Result<String, String> {
        let args: NumberArgs = parse_args(arguments)?;
        match issue::get(&self.repo, args.number).map_err(|e| e.to_string())? {
            Some(issue) => Ok(to_pretty(&issue_to_json(&issue))),
            None => Ok(to_pretty(&Value::Null)),
        }
    }

    fn tool_to_prompt(&self, arguments: Value) -> Result<String, String> {
        let args: NumberArgs = parse_args(arguments)?;
        match issue::get(&self.repo, args.number).map_err(|e| e.to_string())? {
            Some(issue) => Ok(to_pretty(&json!({
                "number": issue.number,
                "title": issue.title,
                "prompt": issue::to_prompt(&issue),
            }))),
            None => Err(format!("no issue #{}", args.number)),
        }
    }

    fn tool_list(&self, arguments: Value) -> Result<String, String> {
        let args: ListArgs = parse_args(arguments)?;
        let items = issue::list(&self.repo, &args.filter()).map_err(|e| e.to_string())?;
        Ok(to_pretty(&listed_to_json(&items)))
    }

    fn tool_search(&self, arguments: Value) -> Result<String, String> {
        let args: SearchArgs = parse_args(arguments)?;
        let items =
            issue::search(&self.repo, &args.query, &args.filter()).map_err(|e| e.to_string())?;
        Ok(to_pretty(&listed_to_json(&items)))
    }

    fn tool_update(&self, arguments: Value) -> Result<String, String> {
        let args: UpdateArgs = parse_args(arguments)?;
        let number = args.number;
        match issue::update(&self.repo, number, args.changes()).map_err(|e| e.to_string())? {
            Some(updated) => Ok(to_pretty(&issue_to_json(&updated))),
            None => Err(format!("no issue #{number}")),
        }
    }

    fn tool_delete(&self, arguments: Value) -> Result<String, String> {
        let args: NumberArgs = parse_args(arguments)?;
        let deleted = issue::delete(&self.repo, args.number).map_err(|e| e.to_string())?;
        Ok(to_pretty(
            &json!({ "number": args.number, "deleted": deleted }),
        ))
    }
}

impl McpService for McpServer {
    fn server_name(&self) -> &str {
        "usagi"
    }

    fn tool_schemas(&self) -> Value {
        // Advertise the issue tools followed by the memory tools, so one `usagi`
        // server exposes both for a repository. Both helpers return JSON arrays
        // by construction.
        let mut tools = issue_tool_schemas();
        let memory = super::memory::tool_schemas();
        tools
            .as_array_mut()
            .expect("issue tool schemas are a JSON array")
            .extend(
                memory
                    .as_array()
                    .expect("memory tool schemas are a JSON array")
                    .iter()
                    .cloned(),
            );
        tools
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        match name {
            "issue_create" => self.tool_create(arguments),
            "issue_get" => self.tool_get(arguments),
            "issue_to_prompt" => self.tool_to_prompt(arguments),
            "issue_list" => self.tool_list(arguments),
            "issue_search" => self.tool_search(arguments),
            "issue_update" => self.tool_update(arguments),
            "issue_delete" => self.tool_delete(arguments),
            // Memory tools share this server and operate on the same repository.
            memory if super::memory::tool_names().contains(&memory) => {
                super::memory::call_tool(&self.repo, memory, arguments)
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

// --- argument shapes -------------------------------------------------------

#[derive(Deserialize)]
struct CreateArgs {
    title: String,
    #[serde(default)]
    priority: IssuePriority,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    dependson: Vec<u32>,
    #[serde(default)]
    related: Vec<u32>,
    #[serde(default)]
    parent: Option<u32>,
    #[serde(default)]
    milestone: Option<String>,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
struct NumberArgs {
    number: u32,
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    status: Option<IssueStatus>,
    #[serde(default)]
    priority: Option<IssuePriority>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    parent: Option<u32>,
    #[serde(default)]
    milestone: Option<String>,
    #[serde(default)]
    ready: bool,
}

impl ListArgs {
    fn filter(self) -> IssueFilter {
        IssueFilter {
            status: self.status,
            priority: self.priority,
            label: self.label,
            parent: self.parent,
            milestone: self.milestone,
            ready_only: self.ready,
        }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    status: Option<IssueStatus>,
    #[serde(default)]
    priority: Option<IssuePriority>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    parent: Option<u32>,
    #[serde(default)]
    milestone: Option<String>,
    #[serde(default)]
    ready: bool,
}

impl SearchArgs {
    fn filter(&self) -> IssueFilter {
        IssueFilter {
            status: self.status,
            priority: self.priority,
            label: self.label.clone(),
            parent: self.parent,
            milestone: self.milestone.clone(),
            ready_only: self.ready,
        }
    }
}

#[derive(Deserialize)]
struct UpdateArgs {
    number: u32,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<IssueStatus>,
    #[serde(default)]
    priority: Option<IssuePriority>,
    #[serde(default)]
    labels: Option<Vec<String>>,
    #[serde(default)]
    dependson: Option<Vec<u32>>,
    #[serde(default)]
    related: Option<Vec<u32>>,
    // Tri-state: absent leaves the field unchanged, an explicit JSON `null`
    // clears it, and a value sets it.
    #[serde(default, deserialize_with = "double_option")]
    parent: Option<Option<u32>>,
    #[serde(default, deserialize_with = "double_option")]
    milestone: Option<Option<String>>,
    #[serde(default)]
    body: Option<String>,
}

impl UpdateArgs {
    fn changes(self) -> IssueChanges {
        IssueChanges {
            title: self.title,
            status: self.status,
            priority: self.priority,
            labels: self.labels,
            dependson: self.dependson,
            related: self.related,
            parent: self.parent,
            milestone: self.milestone,
            body: self.body,
        }
    }
}

/// Deserialize an optional field while preserving the distinction between an
/// absent key (`None`) and an explicit `null` (`Some(None)`). Used to let
/// `issue_update` clear `parent`/`milestone` by passing JSON `null`.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

/// Deserialize tool arguments, mapping any error to a tool-facing message.
fn parse_args<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

mod json;
use json::{issue_to_json, issue_tool_schemas, listed_to_json, to_pretty};

#[cfg(test)]
mod tests;
