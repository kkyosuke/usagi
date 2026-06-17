//! MCP server exposing session orchestration as tools.
//!
//! Where [`super::issue`] exposes a repository's issues and [`super::llm`]
//! exposes a local model, this server lets an agent drive usagi's own session
//! lifecycle: create a parallel worktree session, list the existing ones, and
//! hand a prompt to the agent of a specific session. This turns a coordinating
//! agent into an orchestrator that can spin up isolated worktrees and delegate
//! work into them.
//!
//! Session creation and listing delegate to [`crate::usecase::session`], so the
//! MCP surface stays a thin protocol adapter over the same logic the CLI and
//! TUI use. Sending a prompt to a session's agent is abstracted behind
//! [`AgentBackend`] so the dispatch logic is fully unit-tested without launching
//! a real agent; the production backend (which shells out to the configured
//! agent CLI) lives in the thin stdio entry point
//! (`presentation/cli/session_mcp.rs`). The JSON-RPC framing is shared with the
//! other servers and lives in the parent [`super`] module.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use super::McpService;
use crate::domain::workspace_state::SessionRecord;
use crate::usecase::session;

/// Names of the session tools this server exposes. The unified `usagi` server
/// ([`super::usagi`]) uses this to route `tools/call` for these names to the
/// embedded session server.
pub const TOOL_NAMES: [&str; 3] = ["session_create", "session_list", "session_prompt"];

/// Sends a prompt to the agent rooted at a session's worktree. Abstracted so the
/// server's protocol handling can be tested with a fake backend that never
/// shells out to a real agent.
pub trait AgentBackend {
    /// Run `prompt` against a fresh agent invocation rooted at `worktree`,
    /// returning its final output text (`Ok`) or an error message to surface to
    /// the agent (`Err`).
    fn prompt(&self, worktree: &Path, prompt: &str) -> Result<String, String>;
}

/// A JSON-RPC server exposing session tools for one workspace.
pub struct SessionMcpServer {
    /// Workspace root that owns `.usagi/sessions/` and the `state.json` tracking
    /// every session.
    workspace_root: PathBuf,
    /// Delegate that actually drives a session's agent for `session_prompt`.
    backend: Box<dyn AgentBackend>,
}

impl SessionMcpServer {
    /// Build a server operating on the workspace at `workspace_root`, delegating
    /// `session_prompt` to `backend`.
    pub fn new(workspace_root: PathBuf, backend: Box<dyn AgentBackend>) -> Self {
        Self {
            workspace_root,
            backend,
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
        let created =
            session::create(&self.workspace_root, &args.name).map_err(|e| e.to_string())?;
        Ok(to_pretty(&json!({
            "name": created.name,
            "root": created.root,
            "worktrees": created.worktrees,
        })))
    }

    fn tool_list(&self) -> Result<String, String> {
        let sessions = session::list(&self.workspace_root).map_err(|e| e.to_string())?;
        Ok(to_pretty(&sessions_to_json(&sessions)))
    }

    fn tool_prompt(&self, arguments: Value) -> Result<String, String> {
        let args: PromptArgs = parse_args(arguments)?;
        let sessions = session::list(&self.workspace_root).map_err(|e| e.to_string())?;
        let target = sessions
            .iter()
            .find(|s| s.name == args.name)
            .ok_or_else(|| format!("no such session: \"{}\"", args.name))?;
        self.backend.prompt(&target.root, &args.prompt)
    }
}

impl McpService for SessionMcpServer {
    fn server_name(&self) -> &str {
        "usagi-session"
    }

    fn tool_schemas(&self) -> Value {
        session_tool_schemas()
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        match name {
            "session_create" => self.tool_create(arguments),
            "session_list" => self.tool_list(),
            "session_prompt" => self.tool_prompt(arguments),
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

// --- argument shapes -------------------------------------------------------

#[derive(Deserialize)]
struct CreateArgs {
    name: String,
}

#[derive(Deserialize)]
struct PromptArgs {
    name: String,
    prompt: String,
}

/// Deserialize tool arguments, mapping any error to a tool-facing message.
fn parse_args<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

// --- JSON helpers ----------------------------------------------------------

fn sessions_to_json(sessions: &[SessionRecord]) -> Value {
    Value::Array(
        sessions
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "root": s.root,
                    "created_at": s.created_at.to_rfc3339(),
                    "worktrees": s.worktrees.iter().map(|wt| json!({
                        "path": wt.path,
                        "branch": wt.branch,
                        "head": wt.head,
                        "primary": wt.primary,
                        "status": wt.status.as_str(),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

fn to_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// JSON Schemas for the session tools advertised via `tools/list`.
fn session_tool_schemas() -> Value {
    json!([
        {
            "name": "session_create",
            "description": "Create a new usagi session: a parallel worktree under \
                .usagi/sessions/<name>/ on a fresh branch for every repository in \
                the workspace. Returns the session name, root, and worktree paths.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Session name (also the new branch name in every repository)"
                    }
                },
                "required": ["name"]
            }
        },
        {
            "name": "session_list",
            "description": "List the workspace's existing sessions, each with its \
                root path, creation time, and per-repository worktrees.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "session_prompt",
            "description": "Send a prompt to the agent of a specific session and \
                return its response. The agent runs in that session's worktree, so \
                its work is isolated on the session branch. Use this to delegate a \
                task to a parallel session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Target session name" },
                    "prompt": { "type": "string", "description": "The task or question for the session's agent" }
                },
                "required": ["name", "prompt"]
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::git::test_command as git_cmd;
    use crate::presentation::mcp::PROTOCOL_VERSION;
    use std::cell::RefCell;
    use std::fs;
    use std::rc::Rc;

    type CallLog = Rc<RefCell<Vec<(PathBuf, String)>>>;

    /// A backend that records the calls it received and returns a scripted
    /// result, so the server's dispatch can be tested without a real agent. The
    /// call log is shared via `Rc` so a test can inspect it after the backend is
    /// moved into the server.
    struct FakeBackend {
        result: Result<String, String>,
        calls: CallLog,
    }

    impl FakeBackend {
        fn ok(reply: &str) -> Self {
            Self {
                result: Ok(reply.to_string()),
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn err(message: &str) -> Self {
            Self {
                result: Err(message.to_string()),
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    impl AgentBackend for FakeBackend {
        fn prompt(&self, worktree: &Path, prompt: &str) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((worktree.to_path_buf(), prompt.to_string()));
            self.result.clone()
        }
    }

    /// Initialise a throwaway git repo with one commit on `main`.
    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            assert!(git_cmd(dir).args(args).status().unwrap().success());
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@e.com"]);
        run(&["config", "user.name", "t"]);
        fs::write(dir.join("code.txt"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    /// Parse a handler reply back into JSON for assertions.
    fn reply(server: &SessionMcpServer, request: Value) -> Value {
        let line = serde_json::to_string(&request).unwrap();
        let response = server.handle_line(&line).expect("expected a reply");
        serde_json::from_str(&response).unwrap()
    }

    /// Call a tool and return the parsed tool-result object.
    fn call(server: &SessionMcpServer, name: &str, arguments: Value) -> Value {
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

    fn server_at(root: &Path, backend: FakeBackend) -> SessionMcpServer {
        SessionMcpServer::new(root.to_path_buf(), Box::new(backend))
    }

    #[test]
    fn initialize_advertises_the_session_server() {
        let tmp = tempfile::tempdir().unwrap();
        let res = reply(
            &server_at(tmp.path(), FakeBackend::ok("x")),
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        );
        assert_eq!(res["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(res["result"]["capabilities"]["tools"].is_object());
        assert_eq!(res["result"]["serverInfo"]["name"], "usagi-session");
    }

    #[test]
    fn ping_returns_empty_result() {
        let tmp = tempfile::tempdir().unwrap();
        let res = reply(
            &server_at(tmp.path(), FakeBackend::ok("x")),
            json!({"jsonrpc":"2.0","id":7,"method":"ping"}),
        );
        assert_eq!(res["id"], 7);
        assert_eq!(res["result"], json!({}));
    }

    #[test]
    fn tools_list_returns_the_three_session_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let res = reply(
            &server_at(tmp.path(), FakeBackend::ok("x")),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        let tools = res["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec!["session_create", "session_list", "session_prompt"]
        );
    }

    #[test]
    fn create_then_list_round_trip() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("x"));

        // No sessions yet.
        assert_eq!(
            tool_json(&call(&server, "session_list", json!({}))),
            json!([])
        );

        // Create one: the result carries the name and the worktree path.
        let created = call(&server, "session_create", json!({"name":"feature-x"}));
        assert_eq!(created["isError"], false);
        let body = tool_json(&created);
        assert_eq!(body["name"], "feature-x");
        let wt = root.path().join(".usagi/sessions/feature-x");
        assert_eq!(body["root"], wt.to_str().unwrap());

        // It now appears in the list with its worktree branch.
        let listed = tool_json(&call(&server, "session_list", json!({})));
        let arr = listed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "feature-x");
        assert_eq!(arr[0]["worktrees"][0]["branch"], "feature-x");
    }

    #[test]
    fn create_duplicate_is_a_tool_error() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("x"));
        call(&server, "session_create", json!({"name":"dup"}));

        let again = call(&server, "session_create", json!({"name":"dup"}));
        assert_eq!(again["isError"], true);
        assert!(again["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("already exists"));
    }

    #[test]
    fn prompt_resolves_the_session_and_forwards_to_the_backend() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let backend = FakeBackend::ok("done");
        let calls = backend.calls.clone(); // inspect after the backend is moved in
        let server = server_at(root.path(), backend);
        call(&server, "session_create", json!({"name":"work"}));

        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"add a test"}),
        );
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "done");

        // The backend was invoked once with the session's worktree root and the
        // prompt text verbatim.
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, root.path().join(".usagi/sessions/work"));
        assert_eq!(calls[0].1, "add a test");
    }

    #[test]
    fn prompt_for_an_unknown_session_is_a_tool_error() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("x"));
        let result = call(
            &server,
            "session_prompt",
            json!({"name":"ghost","prompt":"hi"}),
        );
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no such session"));
    }

    #[test]
    fn prompt_surfaces_backend_errors_as_tool_errors() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::err("agent crashed"));
        call(&server, "session_create", json!({"name":"w"}));
        let result = call(&server, "session_prompt", json!({"name":"w","prompt":"hi"}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("agent crashed"));
    }

    #[test]
    fn prompt_surfaces_a_list_error() {
        // A file where the `.usagi` directory should be makes session listing
        // fail, exercising the prompt tool's error path before any session is
        // resolved.
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".usagi"), "blocker").unwrap();
        let server = server_at(tmp.path(), FakeBackend::ok("x"));
        let result = call(&server, "session_prompt", json!({"name":"w","prompt":"hi"}));
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn invalid_arguments_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let server = server_at(tmp.path(), FakeBackend::ok("x"));
        // session_create requires a name.
        let result = call(&server, "session_create", json!({}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("invalid arguments"));
        // session_prompt requires both name and prompt.
        let result = call(&server, "session_prompt", json!({"name":"w"}));
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn unknown_tool_is_reported_as_a_tool_error() {
        let tmp = tempfile::tempdir().unwrap();
        let server = server_at(tmp.path(), FakeBackend::ok("x"));
        let result = call(&server, "session_frobnicate", json!({}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    #[test]
    fn tool_call_without_a_name_is_invalid_params() {
        let tmp = tempfile::tempdir().unwrap();
        let res = reply(
            &server_at(tmp.path(), FakeBackend::ok("x")),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}),
        );
        assert_eq!(res["error"]["code"], -32602);
    }

    #[test]
    fn list_without_arguments_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        // No `arguments` field: session_list takes none, so it should succeed.
        let res = reply(
            &server_at(tmp.path(), FakeBackend::ok("x")),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"session_list"}}),
        );
        assert_eq!(res["result"]["isError"], false);
        assert_eq!(tool_json(&res["result"]), json!([]));
    }

    #[test]
    fn parse_error_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let server = server_at(tmp.path(), FakeBackend::ok("x"));
        let res: Value = serde_json::from_str(&server.handle_line("{ not json").unwrap()).unwrap();
        assert_eq!(res["error"]["code"], -32700);
        assert_eq!(res["id"], Value::Null);
    }

    #[test]
    fn missing_method_is_an_invalid_request() {
        let tmp = tempfile::tempdir().unwrap();
        let res = reply(
            &server_at(tmp.path(), FakeBackend::ok("x")),
            json!({"jsonrpc":"2.0","id":1,"foo":"bar"}),
        );
        assert_eq!(res["error"]["code"], -32600);
    }

    #[test]
    fn unknown_method_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let res = reply(
            &server_at(tmp.path(), FakeBackend::ok("x")),
            json!({"jsonrpc":"2.0","id":1,"method":"frobnicate"}),
        );
        assert_eq!(res["error"]["code"], -32601);
    }

    #[test]
    fn notifications_get_no_reply() {
        let tmp = tempfile::tempdir().unwrap();
        let line = json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string();
        assert!(server_at(tmp.path(), FakeBackend::ok("x"))
            .handle_line(&line)
            .is_none());
    }

    #[test]
    fn list_surfaces_a_usecase_error() {
        // A file where the `.usagi` directory should be makes the store fail.
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".usagi"), "blocker").unwrap();
        let server = server_at(tmp.path(), FakeBackend::ok("x"));
        let result = call(&server, "session_list", json!({}));
        assert_eq!(result["isError"], true);
    }
}
