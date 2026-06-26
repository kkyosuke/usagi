//! The unified `usagi` MCP server.
//!
//! A single `usagi mcp` process exposes everything an agent needs for one
//! workspace: a repository's task issues and durable memories (from
//! [`super::issue`], which already merges [`super::memory`]) plus session
//! orchestration (from [`super::session`]). This composite holds both servers
//! and merges their tool surfaces, so agents register a single `usagi` server
//! instead of one per concern.
//!
//! Issue/memory operations and session operations have very different
//! dependencies — the former are pure repository reads/writes, the latter needs
//! an [`AgentBackend`] that reaches a real agent for `session_prompt` and
//! `session_remove`. Keeping the two servers separate (each independently
//! unit-tested) and composing them here keeps that split clean; this module only owns the
//! merge-and-route glue. The JSON-RPC framing is shared and lives in the parent
//! [`super`] module.

use std::path::PathBuf;

use serde_json::Value;

use super::issue::McpServer as IssueServer;
use super::session::{AgentBackend, SessionMcpServer, TOOL_NAMES as SESSION_TOOLS};
use super::McpService;

/// A JSON-RPC server exposing the full `usagi` tool surface (issue + memory +
/// session) for one workspace.
pub struct UsagiMcpServer {
    /// Issue and memory tools for the workspace's repository.
    issue: IssueServer,
    /// Session orchestration tools for the workspace.
    session: SessionMcpServer,
}

impl UsagiMcpServer {
    /// Build a server delegating `session_prompt` and `session_remove` to
    /// `backend`.
    ///
    /// Issues and memories resolve against `worktree` (the current working tree,
    /// so a session agent's edits stay on its own branch), while session
    /// orchestration resolves against `workspace_root` (the whole workspace).
    /// When the process runs from the workspace root the two paths coincide.
    pub fn new(worktree: PathBuf, workspace_root: PathBuf, backend: Box<dyn AgentBackend>) -> Self {
        Self {
            issue: IssueServer::new(&worktree),
            session: SessionMcpServer::new(workspace_root, backend),
        }
    }

    /// Handle one JSON-RPC message (a single line of input). Returns the JSON
    /// response to write back, or `None` for notifications (which take no
    /// reply).
    pub fn handle_line(&self, line: &str) -> Option<String> {
        super::dispatch_line(self, line)
    }
}

impl McpService for UsagiMcpServer {
    fn server_name(&self) -> &str {
        "usagi"
    }

    fn tool_schemas(&self) -> Value {
        // Advertise the issue/memory tools followed by the session tools, so a
        // single `usagi` server exposes all of them. `into_schema_array` keeps a
        // malformed sub-schema from panicking `tools/list` (see its docs).
        let mut tools = super::into_schema_array(self.issue.tool_schemas());
        tools.extend(super::into_schema_array(self.session.tool_schemas()));
        Value::Array(tools)
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        // Session tools go to the session server; everything else (issue,
        // memory, and unknown-tool errors) is handled by the issue server.
        if SESSION_TOOLS.contains(&name) {
            self.session.call_tool(name, arguments)
        } else {
            self.issue.call_tool(name, arguments)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::git::test_command as git_cmd;
    use crate::presentation::mcp::session::AgentBackend;
    use crate::presentation::mcp::PROTOCOL_VERSION;
    use serde_json::json;
    use std::fs;
    use std::path::Path;

    /// A backend that returns a fixed reply, so the unified server's routing of
    /// `session_prompt` to the session server can be exercised without a real
    /// agent.
    struct StubBackend;
    impl AgentBackend for StubBackend {
        fn prompt(&self, _worktree: &Path, _prompt: &str) -> Result<String, String> {
            Ok("delegated".to_string())
        }

        fn remove(
            &self,
            _workspace_root: &Path,
            _name: &str,
            _force: bool,
        ) -> Result<crate::usecase::session::RemovalOutcome, String> {
            Ok(crate::usecase::session::RemovalOutcome {
                removed: true,
                dirty: Vec::new(),
            })
        }
    }

    fn server_at(root: &Path) -> UsagiMcpServer {
        UsagiMcpServer::new(
            root.to_path_buf(),
            root.to_path_buf(),
            Box::new(StubBackend),
        )
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
    fn reply(server: &UsagiMcpServer, request: Value) -> Value {
        let line = serde_json::to_string(&request).unwrap();
        let response = server.handle_line(&line).expect("expected a reply");
        serde_json::from_str(&response).unwrap()
    }

    /// Call a tool and return the parsed tool-result object.
    fn call(server: &UsagiMcpServer, name: &str, arguments: Value) -> Value {
        reply(
            server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        )["result"]
            .clone()
    }

    #[test]
    fn initialize_advertises_the_unified_server() {
        let tmp = tempfile::tempdir().unwrap();
        let res = reply(
            &server_at(tmp.path()),
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        );
        assert_eq!(res["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(res["result"]["serverInfo"]["name"], "usagi");
    }

    #[test]
    fn tools_list_merges_issue_memory_and_session_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let res = reply(
            &server_at(tmp.path()),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        let tools = res["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // 7 issue + 6 memory + 4 session.
        assert_eq!(names.len(), 17);
        assert!(names.contains(&"issue_create"));
        assert!(names.contains(&"issue_to_prompt"));
        assert!(names.contains(&"memory_save"));
        assert!(names.contains(&"session_create"));
        assert!(names.contains(&"session_list"));
        assert!(names.contains(&"session_prompt"));
        assert!(names.contains(&"session_remove"));
    }

    #[test]
    fn issue_tools_route_to_the_issue_server() {
        let tmp = tempfile::tempdir().unwrap();
        let result = call(&server_at(tmp.path()), "issue_list", json!({}));
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "[]");
    }

    #[test]
    fn memory_tools_route_to_the_issue_server() {
        let tmp = tempfile::tempdir().unwrap();
        let result = call(&server_at(tmp.path()), "memory_list", json!({}));
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "[]");
    }

    #[test]
    fn issue_and_memory_operate_on_the_worktree_not_the_workspace_root() {
        // When the agent runs inside a session, issues and memories must be
        // written to its own worktree (so they ride its branch to `main`),
        // while session orchestration still targets the workspace root.
        let workspace = tempfile::tempdir().unwrap();
        let worktree = workspace
            .path()
            .join(".usagi")
            .join("sessions")
            .join("work");
        fs::create_dir_all(&worktree).unwrap();
        let server = UsagiMcpServer::new(
            worktree.clone(),
            workspace.path().to_path_buf(),
            Box::new(StubBackend),
        );

        let created = call(&server, "issue_create", json!({"title": "in session"}));
        assert_eq!(created["isError"], false);
        let saved = call(
            &server,
            "memory_save",
            json!({"name": "note", "title": "note", "body": "remember", "type": "project"}),
        );
        assert_eq!(saved["isError"], false);

        // Both stores live under the worktree, never the workspace root.
        assert!(worktree.join(".usagi/issues").read_dir().unwrap().count() > 0);
        assert!(workspace.path().join(".usagi/issues").read_dir().is_err());
        assert!(worktree.join(".usagi/memory").exists());
        assert!(!workspace.path().join(".usagi/memory").exists());
    }

    #[test]
    fn session_tools_route_to_the_session_server() {
        let tmp = tempfile::tempdir().unwrap();
        let result = call(&server_at(tmp.path()), "session_list", json!({}));
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "[]");
    }

    #[test]
    fn session_prompt_routes_through_to_the_backend() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path());
        call(&server, "session_create", json!({"name":"work"}));

        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"do it"}),
        );
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "delegated");
    }

    #[test]
    fn session_remove_routes_through_to_the_backend() {
        let tmp = tempfile::tempdir().unwrap();
        // The stub backend reports a clean removal, so the unified server's
        // routing of session_remove to the session server is exercised end to end.
        let result = call(
            &server_at(tmp.path()),
            "session_remove",
            json!({"name":"gone"}),
        );
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body, json!({"name":"gone","removed":true,"dirty":[]}));
    }

    #[test]
    fn unknown_tool_is_reported_as_a_tool_error() {
        let tmp = tempfile::tempdir().unwrap();
        let result = call(&server_at(tmp.path()), "frobnicate", json!({}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }
}
