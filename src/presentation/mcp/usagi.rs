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

use std::path::{Path, PathBuf};

use crate::usecase::doctor::CommandRunner;

use serde::Deserialize;
use serde_json::{json, Value};

use super::issue::McpServer as IssueServer;
use super::session::{
    resolve_session_agent, AgentBackend, PromptMode, SessionMcpServer, TOOL_NAMES as SESSION_TOOLS,
};
use super::McpService;

/// The composite-only orchestration tool this server adds on top of the merged
/// issue/memory and session surfaces (see [`UsagiMcpServer::tool_delegate_issue`]).
const DELEGATE_ISSUE_TOOL: &str = "session_delegate_issue";

/// Issue tools that write to the repository's **git-tracked** `.usagi/issues/`
/// store. When the server runs from the workspace root (`worktree ==
/// workspace_root`) these are refused, so the root can never dirty the tracked
/// repo — the technical enforcement of "root does not modify the tracked
/// repository" (principle 2).
///
/// Memory writes (`memory_save` / `memory_delete`) are **not** listed: the memory
/// store (`.usagi/memory/`) is git-ignored (see `.usagi/.gitignore`), so writing
/// it at the root leaves the tracked tree clean and needs no guard. Read/format
/// tools (`issue_get` / `issue_search` / `issue_to_prompt` / `memory_get` /
/// `memory_search`) and every `session_*` tool likewise stay allowed, as they are
/// what a coordinator needs and do not touch the tracked store.
const ROOT_FORBIDDEN_TOOLS: [&str; 3] = ["issue_create", "issue_update", "issue_delete"];

/// Whether two paths name the same directory, comparing canonicalized forms (so a
/// symlinked or `/tmp` ⇄ `/private/tmp` difference still matches) and falling back
/// to a plain comparison when a path cannot be canonicalized (e.g. it does not yet
/// exist).
fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// A JSON-RPC server exposing the full `usagi` tool surface (issue + memory +
/// session) for one workspace.
pub struct UsagiMcpServer {
    /// Issue and memory tools for the workspace's repository.
    issue: IssueServer,
    /// Session orchestration tools for the workspace.
    session: SessionMcpServer,
    /// True when the process runs from the workspace root (`worktree` and
    /// `workspace_root` coincide). In that case the write-guardrail refuses the
    /// issue-write tools that mutate the git-tracked store (see
    /// [`ROOT_FORBIDDEN_TOOLS`]).
    at_workspace_root: bool,
    /// The workspace root directory.
    workspace_root: PathBuf,
}

impl UsagiMcpServer {
    /// Build a server delegating `session_prompt` and `session_remove` to
    /// `backend`.
    ///
    /// Issues and memories resolve against `worktree` (the current working tree,
    /// so a session agent's edits stay on its own branch), while session
    /// orchestration resolves against `workspace_root` (the whole workspace).
    /// When the process runs from the workspace root the two paths coincide.
    pub fn new(
        worktree: PathBuf,
        workspace_root: PathBuf,
        backend: Box<dyn AgentBackend>,
        runner: Box<dyn CommandRunner>,
    ) -> Self {
        let at_workspace_root = same_dir(&worktree, &workspace_root);
        Self {
            issue: IssueServer::new(&worktree),
            session: SessionMcpServer::new(workspace_root.clone(), &worktree, backend, runner),
            at_workspace_root,
            workspace_root,
        }
    }

    /// Handle one JSON-RPC message (a single line of input). Returns the JSON
    /// response to write back, or `None` for notifications (which take no
    /// reply).
    pub fn handle_line(&self, line: &str) -> Option<String> {
        super::dispatch_line(self, line)
    }

    /// Delegate an issue to a fresh session in one call: render the issue as an
    /// agent prompt, create a new session, and queue that prompt for the
    /// session's first launch. This is the composite's own orchestration tool —
    /// it does not add new business logic, it drives the existing sub-tools
    /// (`issue_to_prompt`, `session_create`, `session_prompt`) so their behaviour
    /// stays single-sourced. The primitives remain available for callers that
    /// need to tweak the prompt or target an existing session.
    fn tool_delegate_issue(&self, arguments: Value) -> Result<String, String> {
        let args: DelegateIssueArgs = super::parse_args(arguments)?;
        // Drive the sub-servers through their typed helpers (not their serialized
        // tool output), so the orchestration reuses their logic without parsing
        // JSON text back out. Each step surfaces the sub-server's own error.
        //
        // 1. Resolve and validate the agent override. An unknown agent_cli is surfaced
        //    early, before reading files or committing validation.
        let agent =
            resolve_session_agent(&*self.session.runner, args.agent_cli.as_deref(), args.model)?;

        // 2. Render the issue as a ready-to-run prompt (errors if it is missing).
        let rendered = self.issue.render_prompt(args.number)?;

        // Verify that the issue file exists in the base commit of the repository
        let local_settings =
            crate::usecase::settings::load_local(&self.workspace_root).unwrap_or_default();
        let base = crate::infrastructure::git::resolve_base_ref(
            &self.workspace_root,
            local_settings.branch_source(),
            local_settings.default_branch(),
        );
        let base_ref = base.unwrap_or_else(|| "HEAD".to_string());
        let relative_issue_path = format!(".usagi/issues/{}", rendered.file_name);

        if !crate::infrastructure::git::file_exists_at_rev(
            &self.workspace_root,
            &base_ref,
            &relative_issue_path,
        ) {
            return Err(format!(
                "issue #{} is not committed to the base branch ({}) yet: \
                 uncommitted issues will not be present in the new session's worktree. \
                 Please commit and merge this issue using a triage session first, \
                 or create a new triage session manually with session_create + session_prompt.",
                args.number, base_ref
            ));
        }

        // 3. Create a fresh session for the issue (default name: issue-<number>),
        //    pinning the optional per-session agent CLI / model so the delegated
        //    session launches with them. A duplicate name surfaces the session
        //    server's own error.
        let name = args
            .name
            .unwrap_or_else(|| format!("issue-{}", args.number));
        let created = self.session.create_session(&name, agent)?;
        // 4. Deliver the prompt. A freshly created session has no live pane, so the
        //    launch queue is always the right channel here.
        let (channel, _detail) =
            self.session
                .deliver_prompt(&name, &rendered.prompt, PromptMode::Queue)?;

        Ok(super::to_pretty(&json!({
            "issue": rendered.number,
            "title": rendered.title,
            "session": created.name,
            "root": created.root,
            "worktrees": created.worktrees,
            "delivered_to": channel.as_str(),
        })))
    }
}

/// Arguments for [`UsagiMcpServer::tool_delegate_issue`].
#[derive(Deserialize)]
struct DelegateIssueArgs {
    /// The issue to delegate.
    number: u32,
    /// Session name to create; defaults to `issue-<number>`.
    #[serde(default)]
    name: Option<String>,
    /// Optional agent CLI the delegated session launches with, overriding the
    /// workspace effective `agent_cli`. Accepts `claude` / `codex` / `sakana.ai`
    /// / `gemini` / `antigravity` (case-insensitive).
    #[serde(default)]
    agent_cli: Option<String>,
    /// Optional model the session's agent CLI runs (rendered as `--model` / `-m`).
    #[serde(default)]
    model: Option<String>,
}

/// The JSON Schema for the composite's `session_delegate_issue` tool.
fn delegate_issue_schema() -> Value {
    json!({
        "name": DELEGATE_ISSUE_TOOL,
        "description": "Delegate an issue to a fresh parallel session in one step: \
            render the issue as a ready-to-run agent prompt, create a new session \
            (worktree + branch usagi/<name>), and queue that prompt for the \
            session's first agent launch. A convenience over calling \
            issue_to_prompt + session_create + session_prompt yourself; use those \
            primitives instead when you need to tweak the prompt or target an \
            existing session. `name` defaults to issue-<number>. Optionally pin the \
            agent CLI and model this session launches with (agent_cli / model) — so \
            a coordinator can route a light issue to a small model and a heavy one \
            to a large model. Errors if the issue does not exist, the agent_cli is \
            unknown, or the session name is already taken. Returns \
            { issue, title, session, root, worktrees, delivered_to }.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "number": { "type": "integer", "description": "Issue number to delegate" },
                "name": { "type": "string", "description": "Session name to create (default: issue-<number>)" },
                "agent_cli": {
                    "type": "string",
                    "enum": ["claude", "codex", "sakana.ai", "gemini", "antigravity"],
                    "description": "Agent CLI the delegated session launches (default: the workspace effective agent_cli)"
                },
                "model": {
                    "type": "string",
                    "description": "Model the session's agent CLI runs (default: the CLI's own default)"
                }
            },
            "required": ["number"]
        }
    })
}

impl McpService for UsagiMcpServer {
    fn server_name(&self) -> &str {
        "usagi"
    }

    fn tool_schemas(&self) -> Value {
        // Advertise the issue/memory tools followed by the session tools, then the
        // composite's own orchestration tool, so a single `usagi` server exposes
        // all of them. `into_schema_array` keeps a malformed sub-schema from
        // panicking `tools/list` (see its docs).
        let mut tools = super::into_schema_array(self.issue.tool_schemas());
        tools.extend(super::into_schema_array(self.session.tool_schemas()));
        tools.push(delegate_issue_schema());
        Value::Array(tools)
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        // Root guardrail: the issue store is git-tracked, so refuse the issue-write
        // tools before routing when running at the workspace root. This keeps a
        // coordinator running `usagi mcp` at the root from dirtying the tracked
        // repo — those writes belong on a session's own branch. (Memory writes are
        // not guarded: the memory store is git-ignored, so they leave the tracked
        // tree clean.)
        if self.at_workspace_root && ROOT_FORBIDDEN_TOOLS.contains(&name) {
            return Err(format!(
                "{name} is refused at the workspace root: it would modify the \
                 git-tracked issue store. Run issue writes from inside a session \
                 worktree (create or open one with session_create / \
                 session_delegate_issue) so the change rides that session's branch."
            ));
        }
        // The composite owns `session_delegate_issue` (it spans both sub-servers);
        // session tools go to the session server; everything else (issue, memory,
        // and unknown-tool errors) is handled by the issue server.
        if name == DELEGATE_ISSUE_TOOL {
            self.tool_delegate_issue(arguments)
        } else if SESSION_TOOLS.contains(&name) {
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

    use crate::usecase::doctor::CommandRunner;

    /// A runner that reports a fixed allowlist of programs as available.
    struct FakeRunner(Vec<&'static str>);

    impl CommandRunner for FakeRunner {
        fn available(&self, program: &str) -> bool {
            self.0.contains(&program)
        }
        fn run(&self, _program: &str, _args: &[&str]) -> std::io::Result<bool> {
            Ok(true)
        }
        fn check(&self, _program: &str, _args: &[&str]) -> bool {
            true
        }
        fn spawn(&self, _program: &str, _args: &[&str]) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A backend that returns a fixed reply, so the unified server's routing of
    /// session prompt/send routing to the session server can be exercised without a real
    /// agent.
    struct StubBackend;
    impl AgentBackend for StubBackend {
        fn prompt(&self, _worktree: &Path, _prompt: &str) -> Result<String, String> {
            Ok("delegated".to_string())
        }

        fn send(&self, _worktree: &Path, _prompt: &str) -> Result<String, String> {
            Ok("sent".to_string())
        }

        fn agent_is_live(&self, _worktree: &Path) -> bool {
            false
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
        let runner = Box::new(FakeRunner(vec!["claude", "codex", "codex-fugu"]));
        UsagiMcpServer::new(
            root.to_path_buf(),
            root.to_path_buf(),
            Box::new(StubBackend),
            runner,
        )
    }

    /// Build a session-mode server: the worktree is distinct from the workspace
    /// root, so the process is *not* at the root and the write-guardrail does not
    /// apply (issue/memory writes are allowed). Mirrors a real session, whose
    /// worktree lives under `.usagi/sessions/<name>`.
    fn session_server_at(root: &Path) -> (UsagiMcpServer, PathBuf) {
        let worktree = root.join(".usagi").join("sessions").join("work");
        fs::create_dir_all(&worktree).unwrap();
        let runner = Box::new(FakeRunner(vec!["claude", "codex", "codex-fugu"]));
        let server = UsagiMcpServer::new(
            worktree.clone(),
            root.to_path_buf(),
            Box::new(StubBackend),
            runner,
        );
        (server, worktree)
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

    /// Commit the created issues in the test repo to the current branch.
    fn commit_issues(dir: &Path) {
        let run = |args: &[&str]| {
            assert!(git_cmd(dir).args(args).status().unwrap().success());
        };
        run(&["add", ".usagi/issues/"]);
        run(&["commit", "-q", "-m", "add issues"]);
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
        // 6 issue + 4 memory + 8 session + 1 composite orchestration tool.
        assert_eq!(names.len(), 19);
        assert!(names.contains(&"issue_create"));
        assert!(names.contains(&"issue_to_prompt"));
        assert!(names.contains(&"issue_search"));
        assert!(names.contains(&"memory_save"));
        assert!(names.contains(&"session_create"));
        assert!(names.contains(&"session_list"));
        assert!(names.contains(&"session_status"));
        assert!(names.contains(&"session_prompt"));
        assert!(names.contains(&"session_pr"));
        assert!(names.contains(&"session_remove"));
        assert!(names.contains(&"session_note_get"));
        assert!(names.contains(&"session_note_update"));
        assert!(names.contains(&"session_delegate_issue"));
        // The list / send / update tools were folded into search / session_prompt
        // / memory_save.
        assert!(!names.contains(&"issue_list"));
        assert!(!names.contains(&"memory_list"));
        assert!(!names.contains(&"memory_update"));
        assert!(!names.contains(&"session_send"));
    }

    #[test]
    fn issue_tools_route_to_the_issue_server() {
        let tmp = tempfile::tempdir().unwrap();
        let result = call(&server_at(tmp.path()), "issue_search", json!({}));
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "[]");
    }

    #[test]
    fn memory_tools_route_to_the_issue_server() {
        let tmp = tempfile::tempdir().unwrap();
        let result = call(&server_at(tmp.path()), "memory_search", json!({}));
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
        let runner = Box::new(FakeRunner(vec!["claude", "codex", "codex-fugu"]));
        let server = UsagiMcpServer::new(
            worktree.clone(),
            workspace.path().to_path_buf(),
            Box::new(StubBackend),
            runner,
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
        // session_status likewise routes to the session server (no sessions yet).
        let status = call(&server_at(tmp.path()), "session_status", json!({}));
        assert_eq!(status["isError"], false);
        assert_eq!(status["content"][0]["text"], "[]");
    }

    #[test]
    fn session_prompt_routes_through_to_the_backend() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path());
        call(&server, "session_create", json!({"name":"work"}));

        // The stub reports no live pane, so `auto` queues for launch (the `prompt`
        // delegate), and the result reports the channel it took.
        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"do it"}),
        );
        assert_eq!(result["isError"], false);
        let body: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
            .expect("delivery report");
        assert_eq!(body["delivered_to"], "queue");
        assert_eq!(body["detail"], "delegated");
    }

    #[test]
    fn session_prompt_live_mode_routes_through_to_the_backend_send() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path());
        call(&server, "session_create", json!({"name":"work"}));

        // `live` mode forces the live channel (the `send` delegate) regardless of
        // whether a pane is detected.
        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"do it now","mode":"live"}),
        );
        assert_eq!(result["isError"], false);
        let body: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
            .expect("delivery report");
        assert_eq!(body["delivered_to"], "live");
        assert_eq!(body["detail"], "sent");
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

    #[test]
    fn delegate_issue_renders_creates_and_queues_in_one_call() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path());
        // An issue to delegate. Seed it straight through the issue sub-server: at
        // the workspace root the composite refuses issue_create, but a coordinator
        // still delegates issues that already exist in the tracked store.
        server
            .issue
            .call_tool(
                "issue_create",
                json!({"title":"Add doctor","body":"Diagnose the env."}),
            )
            .unwrap();
        commit_issues(root.path());

        let result = call(&server, "session_delegate_issue", json!({"number":1}));
        assert_eq!(result["isError"], false);
        let body: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["issue"], 1);
        assert_eq!(body["title"], "Add doctor");
        // Default session name is issue-<number>, freshly created (queue channel).
        assert_eq!(body["session"], "issue-1");
        assert_eq!(body["delivered_to"], "queue");

        // The session really exists now (create ran through to the workspace).
        let listed = call(&server, "session_list", json!({}));
        let listed: Value =
            serde_json::from_str(listed["content"][0]["text"].as_str().unwrap()).unwrap();
        let names: Vec<&str> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"issue-1"));
        // A delegated session is created through the MCP server, so its recorded
        // origin marks it agent-launched.
        let delegated = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "issue-1")
            .unwrap();
        assert_eq!(delegated["origin"], "mcp");
    }

    #[test]
    fn delegate_issue_accepts_an_explicit_session_name() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path());
        // Seed through the sub-server (the composite refuses issue_create at root).
        server
            .issue
            .call_tool("issue_create", json!({"title":"task"}))
            .unwrap();
        commit_issues(root.path());

        let result = call(
            &server,
            "session_delegate_issue",
            json!({"number":1,"name":"my-work"}),
        );
        assert_eq!(result["isError"], false);
        let body: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["session"], "my-work");
    }

    #[test]
    fn root_refuses_the_issue_write_tools() {
        // At the workspace root (worktree == workspace_root) every tool that
        // mutates the git-tracked issue store is refused with the "run it from a
        // session" guidance, regardless of arguments — the guardrail fires before
        // the tool runs.
        let tmp = tempfile::tempdir().unwrap();
        let server = server_at(tmp.path());
        for tool in ROOT_FORBIDDEN_TOOLS {
            let result = call(&server, tool, json!({}));
            assert_eq!(result["isError"], true, "{tool} must be refused at root");
            let text = result["content"][0]["text"].as_str().unwrap();
            // The message is the guardrail's, not a downstream arg/parse error.
            assert!(text.contains("workspace root"), "{tool}: {text}");
            assert!(text.contains("session"), "{tool}: {text}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn root_is_detected_when_the_paths_differ_textually_but_resolve_to_one_dir() {
        // A coordinator may launch usagi from a path that is not the canonical
        // one (e.g. through a symlinked directory). The root check compares
        // canonicalized forms, so worktree and workspace_root that are textually
        // different yet resolve to the same directory are still recognised as the
        // root — and the issue-write guardrail fires.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir_all(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_ne!(link, real, "paths must differ textually for this test");

        let runner = Box::new(FakeRunner(vec!["claude", "codex", "codex-fugu"]));
        let server = UsagiMcpServer::new(link, real, Box::new(StubBackend), runner);
        let result = call(&server, "issue_create", json!({"title": "x"}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("workspace root"));
    }

    #[test]
    fn root_allows_memory_writes_reads_and_session_tools() {
        // The memory store is git-ignored, so memory writes are allowed at the
        // root; the read/format issue+memory tools and all session tools stay
        // usable there too — a coordinator polls and delegates from the root.
        let tmp = tempfile::tempdir().unwrap();
        let server = server_at(tmp.path());
        // Seed one issue straight through the sub-server (the composite refuses
        // issue writes at root) so the issue read tools have data to return.
        server
            .issue
            .call_tool("issue_create", json!({"title": "seed"}))
            .unwrap();

        for (tool, args) in [
            // Memory writes are not guarded (git-ignored store).
            (
                "memory_save",
                json!({"name":"m","title":"m","body":"b","type":"project"}),
            ),
            ("memory_delete", json!({"name": "m"})),
            // Reads / formatting.
            ("issue_search", json!({})),
            ("issue_get", json!({"number": 1})),
            ("issue_to_prompt", json!({"number": 1})),
            ("memory_search", json!({})),
            ("memory_get", json!({"name": "m"})),
            ("session_list", json!({})),
            ("session_status", json!({})),
        ] {
            let result = call(&server, tool, args);
            assert_eq!(result["isError"], false, "{tool} must be allowed at root");
        }
    }

    #[test]
    fn session_allows_every_issue_and_memory_write_tool() {
        // In a session worktree (worktree != workspace_root) the guardrail is off,
        // so the full create → update → delete and save → delete lifecycles run —
        // proving no regression for the common case.
        let tmp = tempfile::tempdir().unwrap();
        let (server, _worktree) = session_server_at(tmp.path());

        let created = call(&server, "issue_create", json!({"title": "task"}));
        assert_eq!(created["isError"], false);
        let updated = call(
            &server,
            "issue_update",
            json!({"number": 1, "status": "in-progress"}),
        );
        assert_eq!(updated["isError"], false);
        let deleted = call(&server, "issue_delete", json!({"number": 1}));
        assert_eq!(deleted["isError"], false);

        let saved = call(
            &server,
            "memory_save",
            json!({"name":"m","title":"m","body":"b","type":"project"}),
        );
        assert_eq!(saved["isError"], false);
        let removed = call(&server, "memory_delete", json!({"name": "m"}));
        assert_eq!(removed["isError"], false);
    }

    #[test]
    fn fake_runner_non_probe_methods_are_inert() {
        let runner = FakeRunner(vec![]);
        assert!(runner.run("x", &[]).unwrap());
        assert!(runner.check("x", &[]));
        assert!(runner.spawn("x", &[]).is_ok());
    }

    #[test]
    fn delegate_issue_pins_the_agent_cli_and_model_on_the_created_session() {
        use crate::domain::settings::AgentCli;
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path());
        server
            .issue
            .call_tool("issue_create", json!({"title":"heavy design"}))
            .unwrap();
        commit_issues(root.path());

        let result = call(
            &server,
            "session_delegate_issue",
            json!({"number":1,"agent_cli":"claude","model":"claude-opus-4-8"}),
        );
        assert_eq!(result["isError"], false);

        // The delegated session carries the pinned CLI / model in state.json, so it
        // will launch with them (auto-start / pane recovery honour the override).
        let store = crate::infrastructure::workspace_store::WorkspaceStore::new(root.path());
        let session = &store.load().unwrap().unwrap().sessions[0];
        assert_eq!(session.name, "issue-1");
        assert_eq!(session.agent.cli, Some(AgentCli::Claude));
        assert_eq!(session.agent.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn delegate_issue_with_an_unknown_agent_cli_errors_before_creating_a_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path());
        server
            .issue
            .call_tool("issue_create", json!({"title":"task"}))
            .unwrap();
        commit_issues(root.path());

        let result = call(
            &server,
            "session_delegate_issue",
            json!({"number":1,"agent_cli":"gpt"}),
        );
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown agent_cli"));
        // No session was created despite the issue existing.
        let listed = call(&server, "session_list", json!({}));
        assert_eq!(listed["content"][0]["text"], "[]");
    }

    #[test]
    fn delegate_issue_errors_when_the_issue_is_missing() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path());
        // No issue #1 exists, so rendering the prompt fails and nothing is created.
        let result = call(&server, "session_delegate_issue", json!({"number":1}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no issue #1"));
        // No stray session was created.
        let listed = call(&server, "session_list", json!({}));
        assert_eq!(listed["content"][0]["text"], "[]");
    }

    #[test]
    fn delegate_issue_refuses_uncommitted_issue() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path());
        server
            .issue
            .call_tool("issue_create", json!({"title":"uncommitted issue"}))
            .unwrap();

        // The issue is created but not committed yet, so delegate_issue must error.
        let result = call(&server, "session_delegate_issue", json!({"number":1}));
        assert_eq!(result["isError"], true);
        let err_text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            err_text.contains("is not committed to the base branch"),
            "{err_text}"
        );
        // No session was created.
        let listed = call(&server, "session_list", json!({}));
        assert_eq!(listed["content"][0]["text"], "[]");
    }

    #[test]
    fn delegate_issue_without_a_resolvable_base_uses_head_in_the_error() {
        let root = tempfile::tempdir().unwrap();
        let server = server_at(root.path());
        server
            .issue
            .call_tool("issue_create", json!({"title":"non git issue"}))
            .unwrap();

        let result = call(&server, "session_delegate_issue", json!({"number":1}));
        assert_eq!(result["isError"], true);
        let err_text = result["content"][0]["text"].as_str().unwrap();
        assert!(err_text.contains("base branch (HEAD)"), "{err_text}");
        assert_eq!(
            call(&server, "session_list", json!({}))["content"][0]["text"],
            "[]"
        );
    }
}
