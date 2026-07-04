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

use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_args, to_pretty, McpService};
use crate::domain::issue::{IssuePriority, IssueStatus};
use crate::usecase::issue::{
    self, IssueChanges, IssueFilter, IssueView, ListedIssueView, NewIssue,
};

/// A JSON-RPC server exposing issue tools for one repository.
pub struct McpServer {
    repo: PathBuf,
}

/// An issue rendered as a ready-to-run agent prompt (see
/// [`McpServer::render_prompt`]): the issue's number and title plus the prompt
/// text fed to a session's agent.
pub(crate) struct RenderedPrompt {
    pub number: u32,
    pub title: String,
    pub prompt: String,
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
        Ok(to_pretty(&IssueView::from(&created)))
    }

    fn tool_get(&self, arguments: Value) -> Result<String, String> {
        let args: NumberArgs = parse_args(arguments)?;
        match issue::get(&self.repo, args.number).map_err(|e| e.to_string())? {
            Some(issue) => Ok(to_pretty(&IssueView::from(&issue))),
            None => Ok(to_pretty(&Value::Null)),
        }
    }

    fn tool_to_prompt(&self, arguments: Value) -> Result<String, String> {
        let args: NumberArgs = parse_args(arguments)?;
        let rendered = self.render_prompt(args.number)?;
        Ok(to_pretty(&json!({
            "number": rendered.number,
            "title": rendered.title,
            "prompt": rendered.prompt,
        })))
    }

    /// Render issue `number` as a ready-to-run agent prompt, returning it as typed
    /// fields (not JSON text). Shared by the `issue_to_prompt` tool and the unified
    /// server's `session_delegate_issue`, so the composite can delegate an issue
    /// without parsing this tool's serialized output back out. Errors if the issue
    /// does not exist.
    pub(crate) fn render_prompt(&self, number: u32) -> Result<RenderedPrompt, String> {
        match issue::get(&self.repo, number).map_err(|e| e.to_string())? {
            Some(issue) => Ok(RenderedPrompt {
                number: issue.number,
                title: issue.title.clone(),
                prompt: issue::to_prompt(&issue),
            }),
            None => Err(format!("no issue #{number}")),
        }
    }

    fn tool_search(&self, arguments: Value) -> Result<String, String> {
        let SearchArgs { query, filter } = parse_args(arguments)?;
        // An omitted `query` lists every issue: an empty needle matches all, so
        // one code path (`search`) subsumes what a separate `list` tool would do.
        let items = issue::search(&self.repo, query.as_deref().unwrap_or(""), &filter.filter())
            .map_err(|e| e.to_string())?;
        Ok(to_pretty(&listed_views(&items)))
    }

    fn tool_update(&self, arguments: Value) -> Result<String, String> {
        let args: UpdateArgs = parse_args(arguments)?;
        let number = args.number;
        match issue::update(&self.repo, number, args.changes()).map_err(|e| e.to_string())? {
            Some(updated) => Ok(to_pretty(&IssueView::from(&updated))),
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
        // server exposes both for a repository. `into_schema_array` keeps a
        // malformed sub-schema from panicking `tools/list` (see its docs).
        let mut tools = super::into_schema_array(issue_tool_schemas());
        tools.extend(super::into_schema_array(super::memory::tool_schemas()));
        Value::Array(tools)
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        match name {
            "issue_create" => self.tool_create(arguments),
            "issue_get" => self.tool_get(arguments),
            "issue_to_prompt" => self.tool_to_prompt(arguments),
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

/// The filter fields `issue_search` accepts (flattened into its args), and their
/// mapping to [`IssueFilter`], defined once.
#[derive(Deserialize)]
struct FilterArgs {
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

impl FilterArgs {
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
    /// Absent lists every issue (an empty needle matches all); present filters by
    /// a full-text match. Optional so the one search tool subsumes a plain list.
    #[serde(default)]
    query: Option<String>,
    #[serde(flatten)]
    filter: FilterArgs,
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

/// Build the JSON-output views for a list of issues. The field set is the SSoT
/// [`ListedIssueView`], shared with the CLI.
fn listed_views(items: &[issue::ListedIssue]) -> Vec<ListedIssueView<'_>> {
    items.iter().map(ListedIssueView::from).collect()
}

mod json;
use json::issue_tool_schemas;

#[cfg(test)]
mod tests;
