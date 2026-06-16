//! MCP server exposing a repository's task issues as tools.
//!
//! Every tool delegates to [`crate::usecase::issue`], so the MCP surface stays a
//! thin protocol adapter over the same business logic the CLI uses. The
//! JSON-RPC framing is shared with the other server and lives in the parent
//! [`super`] module; this file only supplies the issue tools.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use super::McpService;
use crate::domain::issue::{Issue, IssuePriority, IssueStatus};
use crate::usecase::issue::{self, IssueChanges, IssueFilter, ListedIssue, NewIssue};

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
        issue_tool_schemas()
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        match name {
            "issue_create" => self.tool_create(arguments),
            "issue_get" => self.tool_get(arguments),
            "issue_list" => self.tool_list(arguments),
            "issue_search" => self.tool_search(arguments),
            "issue_update" => self.tool_update(arguments),
            "issue_delete" => self.tool_delete(arguments),
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
    ready: bool,
}

impl ListArgs {
    fn filter(self) -> IssueFilter {
        IssueFilter {
            status: self.status,
            priority: self.priority,
            label: self.label,
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
    ready: bool,
}

impl SearchArgs {
    fn filter(&self) -> IssueFilter {
        IssueFilter {
            status: self.status,
            priority: self.priority,
            label: self.label.clone(),
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
            body: self.body,
        }
    }
}

/// Deserialize tool arguments, mapping any error to a tool-facing message.
fn parse_args<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

// --- JSON helpers ----------------------------------------------------------

fn issue_to_json(issue: &Issue) -> Value {
    json!({
        "number": issue.number,
        "title": issue.title,
        "status": issue.status,
        "priority": issue.priority,
        "labels": issue.labels,
        "dependson": issue.dependson,
        "created_at": issue.created_at.to_rfc3339(),
        "updated_at": issue.updated_at.to_rfc3339(),
        "body": issue.body,
    })
}

fn listed_to_json(items: &[ListedIssue]) -> Value {
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

fn to_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// JSON Schemas for the issue tools advertised via `tools/list`.
fn issue_tool_schemas() -> Value {
    let status = json!({ "type": "string", "enum": ["todo", "in-progress", "done"] });
    let priority = json!({ "type": "string", "enum": ["high", "medium", "low"] });
    let labels = json!({ "type": "array", "items": { "type": "string" } });
    let deps = json!({ "type": "array", "items": { "type": "integer" } });

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::mcp::PROTOCOL_VERSION;

    /// Parse a handler reply back into JSON for assertions.
    fn reply(server: &McpServer, request: Value) -> Value {
        let line = serde_json::to_string(&request).unwrap();
        let response = server.handle_line(&line).expect("expected a reply");
        serde_json::from_str(&response).unwrap()
    }

    /// Call a tool and return the parsed tool-result object.
    fn call(server: &McpServer, name: &str, arguments: Value) -> Value {
        reply(
            server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        )["result"]
            .clone()
    }

    /// The text payload of a tool result, parsed back into JSON.
    fn tool_json(result: &Value) -> Value {
        let text = result["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn initialize_advertises_tools_capability() {
        let server = McpServer::new(tempfile::tempdir().unwrap().path());
        let res = reply(
            &server,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        );
        assert_eq!(res["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(res["result"]["capabilities"]["tools"].is_object());
        assert_eq!(res["result"]["serverInfo"]["name"], "usagi");
    }

    #[test]
    fn ping_returns_empty_result() {
        let server = McpServer::new(tempfile::tempdir().unwrap().path());
        let res = reply(&server, json!({"jsonrpc":"2.0","id":7,"method":"ping"}));
        assert_eq!(res["id"], 7);
        assert_eq!(res["result"], json!({}));
    }

    #[test]
    fn tools_list_returns_all_six_tools() {
        let server = McpServer::new(tempfile::tempdir().unwrap().path());
        let res = reply(
            &server,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        let tools = res["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 6);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"issue_create"));
        assert!(names.contains(&"issue_delete"));
    }

    #[test]
    fn notifications_get_no_reply() {
        let server = McpServer::new(tempfile::tempdir().unwrap().path());
        let line = json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string();
        assert!(server.handle_line(&line).is_none());
    }

    #[test]
    fn parse_error_is_reported() {
        let server = McpServer::new(tempfile::tempdir().unwrap().path());
        let res: Value = serde_json::from_str(&server.handle_line("{ not json").unwrap()).unwrap();
        assert_eq!(res["error"]["code"], -32700);
        assert_eq!(res["id"], Value::Null);
    }

    #[test]
    fn missing_method_is_an_invalid_request() {
        let server = McpServer::new(tempfile::tempdir().unwrap().path());
        let res = reply(&server, json!({"jsonrpc":"2.0","id":1,"foo":"bar"}));
        assert_eq!(res["error"]["code"], -32600);
    }

    #[test]
    fn unknown_method_is_not_found() {
        let server = McpServer::new(tempfile::tempdir().unwrap().path());
        let res = reply(
            &server,
            json!({"jsonrpc":"2.0","id":1,"method":"frobnicate"}),
        );
        assert_eq!(res["error"]["code"], -32601);
    }

    #[test]
    fn tool_call_without_a_name_is_invalid_params() {
        let server = McpServer::new(tempfile::tempdir().unwrap().path());
        let res = reply(
            &server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}),
        );
        assert_eq!(res["error"]["code"], -32602);
    }

    #[test]
    fn unknown_tool_is_reported_as_tool_error() {
        let server = McpServer::new(tempfile::tempdir().unwrap().path());
        let result = call(&server, "issue_nonexistent", json!({}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    #[test]
    fn create_get_list_update_delete_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let server = McpServer::new(tmp.path());

        // create #1 (base) and #2 (depends on #1)
        let created = call(
            &server,
            "issue_create",
            json!({"title":"base","priority":"high","labels":["cli"]}),
        );
        assert_eq!(created["isError"], false);
        assert_eq!(tool_json(&created)["number"], 1);

        call(
            &server,
            "issue_create",
            json!({"title":"blocked","dependson":[1]}),
        );

        // get #1
        let got = call(&server, "issue_get", json!({"number":1}));
        assert_eq!(tool_json(&got)["title"], "base");
        // get missing -> null
        let missing = call(&server, "issue_get", json!({"number":99}));
        assert_eq!(tool_json(&missing), Value::Null);

        // list: #1 ready, #2 blocked by #1
        let listed = tool_json(&call(&server, "issue_list", json!({})));
        let arr = listed.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["ready"], true);
        assert_eq!(arr[1]["ready"], false);
        assert_eq!(arr[1]["unmet_deps"], json!([1]));

        // ready-only filter keeps just #1
        let ready = tool_json(&call(&server, "issue_list", json!({"ready":true})));
        assert_eq!(ready.as_array().unwrap().len(), 1);

        // search by title
        let found = tool_json(&call(&server, "issue_search", json!({"query":"blocked"})));
        assert_eq!(found.as_array().unwrap().len(), 1);

        // update #1 -> done, then #2 becomes ready
        let updated = call(&server, "issue_update", json!({"number":1,"status":"done"}));
        assert_eq!(tool_json(&updated)["status"], "done");
        let listed = tool_json(&call(&server, "issue_list", json!({})));
        assert_eq!(listed.as_array().unwrap()[1]["ready"], true);

        // update missing -> tool error
        let bad = call(
            &server,
            "issue_update",
            json!({"number":99,"status":"done"}),
        );
        assert_eq!(bad["isError"], true);

        // delete #1
        let deleted = call(&server, "issue_delete", json!({"number":1}));
        assert_eq!(tool_json(&deleted), json!({"number":1,"deleted":true}));
        let again = call(&server, "issue_delete", json!({"number":1}));
        assert_eq!(tool_json(&again)["deleted"], false);
    }

    #[test]
    fn tool_call_without_arguments_defaults_to_empty() {
        let server = McpServer::new(tempfile::tempdir().unwrap().path());
        // No `arguments` field: issue_list takes none, so it should succeed.
        let res = reply(
            &server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"issue_list"}}),
        );
        assert_eq!(res["result"]["isError"], false);
        assert_eq!(tool_json(&res["result"]), json!([]));
    }

    #[test]
    fn invalid_arguments_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let server = McpServer::new(tmp.path());
        // issue_create requires a title.
        let result = call(&server, "issue_create", json!({"priority":"high"}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("invalid arguments"));
    }

    #[test]
    fn usecase_errors_surface_for_every_tool() {
        // A file where the `.usagi` directory should be makes the store fail,
        // exercising each tool's error path.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".usagi"), "blocker").unwrap();
        let server = McpServer::new(tmp.path());

        for (name, args) in [
            ("issue_create", json!({"title":"x"})),
            ("issue_get", json!({"number":1})),
            ("issue_list", json!({})),
            ("issue_search", json!({"query":"x"})),
            ("issue_update", json!({"number":1,"status":"done"})),
            ("issue_delete", json!({"number":1})),
        ] {
            let result = call(&server, name, args);
            assert_eq!(result["isError"], true, "{name} should error");
        }
    }
}
