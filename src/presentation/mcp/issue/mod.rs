//! The issue MCP server, exposing a repository's task issues as tools.
//!
//! Every tool delegates to [`crate::usecase::issue`], so the MCP surface stays a
//! thin protocol adapter over the same business logic the CLI uses. Durable
//! memory tools live in [`super::memory`], and the unified [`super::usagi`]
//! server composes issue, memory and session servers in one layer. The JSON-RPC
//! framing is shared with the other servers and lives in the parent [`super`]
//! module.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_args, to_pretty, McpService};
use crate::usecase::issue::{
    self, IssueChanges, IssueFilter, IssueView, ListedIssueView, NewIssue,
};

/// Names of the issue tools this server exposes.
pub const TOOL_NAMES: [&str; 6] = [
    "issue_create",
    "issue_get",
    "issue_to_prompt",
    "issue_search",
    "issue_update",
    "issue_delete",
];

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
    pub file_name: String,
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
        let args: NewIssue = parse_args(arguments)?;
        let created = issue::create(&self.repo, args).map_err(|e| e.to_string())?;
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
                file_name: issue.file_name(),
            }),
            None => Err(format!("no issue #{number}")),
        }
    }

    fn tool_search(&self, arguments: Value) -> Result<String, String> {
        let SearchArgs { query, filter } = parse_args(arguments)?;
        // An omitted `query` lists every issue: an empty needle matches all, so
        // one code path (`search`) subsumes what a separate `list` tool would do.
        let items = issue::search(&self.repo, query.as_deref().unwrap_or(""), &filter)
            .map_err(|e| e.to_string())?;
        Ok(to_pretty(&listed_views(&items)))
    }

    fn tool_update(&self, arguments: Value) -> Result<String, String> {
        let args: UpdateArgs = parse_args(arguments)?;
        let number = args.number;
        match issue::update(&self.repo, number, args.changes).map_err(|e| e.to_string())? {
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
        "usagi-issue"
    }

    fn tool_names(&self) -> &'static [&'static str] {
        &TOOL_NAMES
    }

    fn tool_schemas(&self) -> Value {
        issue_tool_schemas()
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        match name {
            "issue_create" => self.tool_create(arguments),
            "issue_get" => self.tool_get(arguments),
            "issue_to_prompt" => self.tool_to_prompt(arguments),
            "issue_search" => self.tool_search(arguments),
            "issue_update" => self.tool_update(arguments),
            "issue_delete" => self.tool_delete(arguments),
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

// --- argument shapes -------------------------------------------------------

#[derive(Deserialize)]
struct NumberArgs {
    number: u32,
}

#[derive(Deserialize)]
struct SearchArgs {
    /// Absent lists every issue (an empty needle matches all); present filters by
    /// a full-text match. Optional so the one search tool subsumes a plain list.
    #[serde(default)]
    query: Option<String>,
    #[serde(flatten)]
    filter: IssueFilter,
}

#[derive(Deserialize)]
struct UpdateArgs {
    number: u32,
    #[serde(flatten)]
    changes: IssueChanges,
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
