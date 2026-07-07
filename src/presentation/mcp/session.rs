//! MCP server exposing session orchestration as tools.
//!
//! Where [`super::issue`] exposes a repository's issues and [`super::llm`]
//! exposes a local model, this server lets an agent drive usagi's own session
//! lifecycle: create a parallel worktree session, list the existing ones, poll
//! each session's orchestration status (agent phase + per-worktree git status),
//! hand a prompt to the agent of a specific session, list the pull requests
//! discovered for a session, and remove a session it no longer needs. This turns
//! a coordinating agent into an orchestrator that can spin up isolated worktrees,
//! delegate work into them, watch them for completion, and tear them down again.
//!
//! A single `session_prompt` tool delivers work to a session over two channels —
//! the *launch queue* (delivered as the agent's opening message the next time its
//! pane is freshly launched) and the *live queue* (typed into an already-running
//! pane) — chosen by its `mode`. `auto` (the default) picks the live channel when
//! a live agent pane is detected for the session and the launch queue otherwise,
//! so a caller need not know whether the session's agent is currently running;
//! the result reports which channel actually took the prompt.
//!
//! Session creation and listing delegate to [`crate::usecase::session`], so the
//! MCP surface stays a thin protocol adapter over the same logic the CLI and
//! TUI use. The operations that need a real agent or real filesystem — handing a
//! prompt to a session's agent, live-sending a prompt, and removing a session
//! (which discards that agent's conversation) — are abstracted behind
//! [`AgentBackend`] so the
//! dispatch logic is fully unit-tested without touching the filesystem; the
//! production backend (which queues the prompts for the session's worktree and
//! resolves the configured agent for removal) lives in the thin stdio entry
//! point (`presentation/cli/mcp.rs`). The JSON-RPC framing is shared with the
//! other servers and lives in the parent [`super`] module.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_args, to_pretty, McpService};
use crate::domain::settings::AgentCli;
use crate::domain::workspace_state::{SessionAgent, SessionOrigin, SessionRecord};
use crate::usecase::doctor::CommandRunner;
use crate::usecase::session;

/// Names of the session tools this server exposes. The unified `usagi` server
/// ([`super::usagi`]) uses this to route `tools/call` for these names to the
/// embedded session server.
pub const TOOL_NAMES: [&str; 8] = [
    "session_create",
    "session_list",
    "session_status",
    "session_prompt",
    "session_pr",
    "session_remove",
    "session_note_get",
    "session_note_update",
];

/// Largest prompt (in bytes) accepted by `session_prompt`.
///
/// The tool takes arbitrary agent-authored text and hands it to a session's
/// agent over either channel — the launch-time queue (persisted to a file) or the
/// live queue the TUI types into a running pane. Neither has a natural bound, so
/// without a cap one runaway call could persist an enormous file, or make the
/// watcher paste a multi-megabyte blob into a pane in one go. The MCP transport
/// already refuses a request *line* over 64 MiB (`MAX_REQUEST_LINE_BYTES` in the
/// parent module); this is a much tighter per-prompt bound
/// that still comfortably fits any real task description, applied before the
/// prompt reaches the backend so an oversized one is rejected as a tool error
/// rather than written to disk.
const MAX_PROMPT_BYTES: usize = 128 * 1024;

/// The reserved `session_prompt` target that addresses the workspace's **root
/// row** — the `⌂ root` coordinator — instead of a named session. A child
/// session's agent reports its completion by prompting this target (push-style
/// completion report): with the default `auto` mode the report is delivered live
/// to the coordinator's running agent pane, or queued for its next launch when
/// none is open. Spelled with a leading `:` so it can never be mistaken for — or
/// shadowed by — a real session, whose name is a git branch / directory component
/// (a leading `-` is rejected, but a `:` is legal, so the sentinel stays outside
/// the space of names usagi itself creates).
pub(crate) const ROOT_TARGET: &str = ":root";

/// Reject a prompt argument that exceeds [`MAX_PROMPT_BYTES`], naming the tool in
/// the error so the caller knows which argument to trim. Used by `session_prompt`,
/// whose prompt flows to a session's agent over whichever channel it resolves to.
fn check_prompt_len(tool: &str, prompt: &str) -> Result<(), String> {
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "{tool} prompt is too large ({} bytes; limit is {MAX_PROMPT_BYTES})",
            prompt.len()
        ));
    }
    Ok(())
}

/// Resolve the optional per-session agent overrides an MCP caller passed
/// (`agent_cli` / `model`) into a [`SessionAgent`].
///
/// `agent_cli` is matched case-insensitively against each CLI's launch command,
/// display name, and serde label via [`AgentCli::from_name`]; an unrecognised
/// name is a tool error naming the accepted values so the caller can correct it.
/// `model` is passed through untouched (the usecase trims it and drops an empty
/// value on write) — no allowlist is imposed, since model names differ per CLI
/// and change often. Both absent yields the default: the session follows the
/// workspace effective settings and the CLI's own default model. Shared by
/// `session_create` and the composite's `session_delegate_issue`.
pub(crate) fn resolve_session_agent(
    runner: &dyn CommandRunner,
    agent_cli: Option<&str>,
    model: Option<String>,
) -> Result<SessionAgent, String> {
    let cli = match agent_cli {
        Some(name) => {
            let parsed = AgentCli::from_name(name).ok_or_else(|| {
                format!(
                    "unknown agent_cli {name:?}: expected one of \
                     claude, codex, sakana.ai, gemini, antigravity"
                )
            })?;

            let capable = crate::usecase::agent::mcp_capable_clis(runner);
            if !capable.contains(&parsed) {
                let capable_names: Vec<String> = capable
                    .iter()
                    .map(|c| c.display_name().to_lowercase())
                    .collect();
                return Err(format!(
                    "agent_cli {name:?} is not installed or not MCP-capable. \
                     Available installed MCP-capable agents: {capable_names:?}"
                ));
            }

            Some(parsed)
        }
        None => None,
    };
    Ok(SessionAgent { cli, model })
}

/// Drives the parts of session orchestration that touch a real agent or a real
/// filesystem — handing a session's agent a launch-time prompt, live-sending a
/// prompt, and removing a session (which discards that agent's conversation).
/// Abstracted so the server's
/// protocol handling can be tested with a fake backend that never touches the
/// filesystem or a real agent.
pub trait AgentBackend {
    /// Deliver `prompt` to the agent rooted at `worktree` — the production
    /// backend queues it for the session's next fresh agent launch — returning a
    /// confirmation message (`Ok`) or an error message to surface to the agent
    /// (`Err`).
    fn prompt(&self, worktree: &Path, prompt: &str) -> Result<String, String>;

    /// Deliver `prompt` to the agent that is already running in the session
    /// rooted at `worktree`. The production backend appends it to a live queue
    /// that the running TUI drains into the existing agent pane; if no such pane
    /// is open, it waits there until one is.
    fn send(&self, worktree: &Path, prompt: &str) -> Result<String, String>;

    /// Whether a live agent pane currently exists for the session rooted at
    /// `worktree`. `session_prompt`'s `auto` mode uses this to choose between the
    /// live channel ([`send`](Self::send)) and the launch queue
    /// ([`prompt`](Self::prompt)). The production backend answers from the
    /// per-worktree agent-phase file the TUI maintains (present while a pane is
    /// alive, cleared when it dies); a wrong answer is not fatal — the live queue
    /// simply waits for a pane if one is not actually open.
    fn agent_is_live(&self, worktree: &Path) -> bool;

    /// Remove session `name` under `workspace_root`, resolving the workspace's
    /// configured agent CLI so the session's persisted conversation is discarded
    /// along with its worktrees. Without `force`, a session with uncommitted
    /// changes is left untouched and the [`session::RemovalOutcome`] reports the
    /// dirty worktrees; with `force`, those changes are discarded. Returns an
    /// error message to surface to the agent (`Err`).
    fn remove(
        &self,
        workspace_root: &Path,
        name: &str,
        force: bool,
    ) -> Result<session::RemovalOutcome, String>;
}

/// A JSON-RPC server exposing session tools for one workspace.
pub struct SessionMcpServer {
    /// Workspace root that owns `.usagi/sessions/` and the `state.json` tracking
    /// every session.
    workspace_root: PathBuf,
    /// The name of the session the MCP process is running inside, if any.
    /// `None` when the server runs from the workspace root (not inside a session).
    current_session: Option<String>,
    /// Delegate that actually drives a session's agent for `session_prompt`
    /// (over either delivery channel) and its live-pane detection.
    backend: Box<dyn AgentBackend>,
    /// Probes external tools (like checking if an agent CLI is installed on the PATH).
    pub(crate) runner: Box<dyn CommandRunner>,
}

impl SessionMcpServer {
    /// Build a server operating on the workspace at `workspace_root`, delegating
    /// `session_prompt` delivery to `backend`. The `worktree` is the agent's current
    /// working directory; when it sits under `.usagi/sessions/<name>/` the server
    /// derives the current session name from it, enabling the note self-access
    /// tools (`session_note_get` / `session_note_update`).
    pub fn new(
        workspace_root: PathBuf,
        worktree: &Path,
        backend: Box<dyn AgentBackend>,
        runner: Box<dyn CommandRunner>,
    ) -> Self {
        let current_session = derive_current_session(worktree, &workspace_root);
        Self {
            workspace_root,
            current_session,
            backend,
            runner,
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
        let agent = resolve_session_agent(&*self.runner, args.agent_cli.as_deref(), args.model)?;
        let created = self.create_session(&args.name, agent)?;
        Ok(to_pretty(&json!({
            "name": created.name,
            "root": created.root,
            "worktrees": created.worktrees,
        })))
    }

    /// Create the session named `name`, recording `agent` as its per-session
    /// agent CLI / model override, and return it as the typed
    /// [`session::CreatedSession`]. Shared by the `session_create` tool and the
    /// unified server's `session_delegate_issue`, so the composite can create a
    /// session without parsing the tool's serialized output back out. Pass
    /// [`SessionAgent::default`] to follow the workspace effective settings.
    ///
    /// The MCP server runs headless, so a creation failure would otherwise only
    /// travel back to the calling agent and never reach a log file. The full chain
    /// — matching the TUI's `session create "<name>" failed: ...` wording — is
    /// recorded before the short message is surfaced to the client, so failures
    /// stay inspectable in `<data dir>/logs/`.
    pub(crate) fn create_session(
        &self,
        name: &str,
        agent: SessionAgent,
    ) -> Result<session::CreatedSession, String> {
        // The MCP server is the agent-facing entry point, so every session it
        // creates — whether from `session_create` or `session_delegate_issue` —
        // is recorded with an MCP origin, distinguishing it from a session a
        // person cut interactively in the TUI.
        // Record which session this one was started from: the session this MCP
        // process is running inside (`current_session`). `None` when the agent is
        // running at the workspace root (the coordinator has no parent session),
        // so a root-launched session carries no lineage. This is what lets a
        // reader tell which session a given session was started from.
        let started_from = self.current_session.clone();
        session::create_with_agent(
            &self.workspace_root,
            name,
            agent,
            SessionOrigin::Mcp,
            started_from,
        )
        .map_err(|e| {
            crate::infrastructure::error_log::ErrorLog::record(&format!(
                "mcp session_create \"{name}\" failed: {e:#}"
            ));
            e.to_string()
        })
    }

    fn tool_list(&self) -> Result<String, String> {
        let sessions = session::list(&self.workspace_root).map_err(|e| e.to_string())?;
        Ok(to_pretty(&sessions_to_json(&sessions)))
    }

    fn tool_prompt(&self, arguments: Value) -> Result<String, String> {
        let args: PromptArgs = parse_args(arguments)?;
        check_prompt_len("session_prompt", &args.prompt)?;
        let stored_agent = if args.agent_cli.is_some() || args.model.is_some() {
            if args.name == ROOT_TARGET {
                return Err(
                    "session_prompt agent_cli/model cannot be used with the :root target"
                        .to_string(),
                );
            }
            let existing = self.find_session(&args.name)?.agent;
            let cli = match args.agent_cli.as_deref() {
                Some(agent_cli) => resolve_session_agent(&*self.runner, Some(agent_cli), None)?.cli,
                None => existing.cli,
            };
            let model = args.model.or(existing.model);
            Some(self.set_session_agent(&args.name, SessionAgent { cli, model })?)
        } else {
            None
        };
        let (channel, detail) = self.deliver_prompt(&args.name, &args.prompt, args.mode)?;
        // Report the channel that actually took the prompt, so a caller using
        // `auto` sees whether it reached a running pane or was queued for launch.
        let mut result = json!({
            "name": args.name,
            "delivered_to": channel.as_str(),
            "detail": detail,
        });
        if let Some(agent) = stored_agent {
            result["agent"] = session_agent_to_json(&agent);
        }
        Ok(to_pretty(&result))
    }

    /// Deliver `prompt` to `name` over the channel chosen by `mode`, returning the
    /// channel actually used and the backend's confirmation. `name` is either a
    /// session name or the reserved [`ROOT_TARGET`] (the `⌂ root` coordinator).
    /// Shared by the `session_prompt` tool and the unified server's
    /// `session_delegate_issue` (which passes [`PromptMode::Queue`], since a freshly
    /// created session has no live pane).
    pub(crate) fn deliver_prompt(
        &self,
        name: &str,
        prompt: &str,
        mode: PromptMode,
    ) -> Result<(Channel, String), String> {
        check_prompt_len("session_prompt", prompt)?;
        let target_root = self.resolve_target(name)?;
        // Resolve which channel takes the prompt. `auto` asks the backend whether
        // a live pane exists; the explicit modes force one channel.
        let channel = match mode {
            PromptMode::Queue => Channel::Queue,
            PromptMode::Live => Channel::Live,
            PromptMode::Auto if self.backend.agent_is_live(&target_root) => Channel::Live,
            PromptMode::Auto => Channel::Queue,
        };
        let detail = match channel {
            Channel::Queue => self.backend.prompt(&target_root, prompt)?,
            Channel::Live => self.backend.send(&target_root, prompt)?,
        };
        Ok((channel, detail))
    }

    /// Resolve a `session_prompt` target to the worktree root its prompt is
    /// delivered to. The reserved [`ROOT_TARGET`] maps to the workspace root (the
    /// coordinator's `⌂ root` row, which belongs to no session), so a child can
    /// push a completion report up to the coordinator; any other name maps to the
    /// matching session's root and errors when no such session exists. Both the
    /// live and launch channels are addressed purely by this worktree path (the
    /// root row's is the workspace root itself), so no other resolution differs.
    fn resolve_target(&self, name: &str) -> Result<PathBuf, String> {
        if name == ROOT_TARGET {
            Ok(self.workspace_root.clone())
        } else {
            Ok(self.find_session(name)?.root)
        }
    }

    /// Persist a new per-session agent CLI / model override for `name`.
    ///
    /// This is used by `session_prompt` when a caller supplies `agent_cli` and/or
    /// `model`: the override is recorded before the prompt is delivered, so a
    /// queued prompt is opened by the requested agent on the next fresh launch.
    /// Live delivery can only affect future launches; the already-running pane
    /// keeps whatever CLI/model started it.
    fn set_session_agent(&self, name: &str, agent: SessionAgent) -> Result<SessionAgent, String> {
        session::set_agent(&self.workspace_root, name, agent).map_err(|e| e.to_string())
    }

    fn tool_list_status(&self) -> Result<String, String> {
        let statuses = session::statuses(&self.workspace_root).map_err(|e| e.to_string())?;
        Ok(to_pretty(&statuses_to_json(&statuses)))
    }

    fn tool_pr(&self, arguments: Value) -> Result<String, String> {
        let args: PrArgs = parse_args(arguments)?;
        let prs = session::pr_links(&self.workspace_root, &args.name).map_err(|e| e.to_string())?;
        // A PR's state reflects the session's branch integration (usagi never
        // queries GitHub): once the work is merged, every PR reads "merged",
        // otherwise "open" — a PR closed without merging is not distinguishable.
        let state = if prs.merged { "merged" } else { "open" };
        Ok(to_pretty(&json!({
            "name": args.name,
            "root": prs.root,
            "merged": prs.merged,
            "pr": prs.prs.iter().map(|pr| json!({
                "number": pr.number,
                "url": pr.url,
                "state": state,
            })).collect::<Vec<_>>(),
        })))
    }

    fn tool_remove(&self, arguments: Value) -> Result<String, String> {
        let args: RemoveArgs = parse_args(arguments)?;
        let outcome = self
            .backend
            .remove(&self.workspace_root, &args.name, args.force)?;
        Ok(to_pretty(&json!({
            "name": args.name,
            "removed": outcome.removed,
            "dirty": outcome.dirty,
        })))
    }

    fn tool_note_get(&self) -> Result<String, String> {
        let name = self
            .current_session
            .as_deref()
            .ok_or("session_note_get is only available from inside a session worktree")?;
        let note = session::get_note(&self.workspace_root, name).map_err(|e| e.to_string())?;
        Ok(to_pretty(&json!({
            "name": name,
            "note": note,
        })))
    }

    fn tool_note_update(&self, arguments: Value) -> Result<String, String> {
        let args: NoteUpdateArgs = parse_args(arguments)?;
        let name = self
            .current_session
            .as_deref()
            .ok_or("session_note_update is only available from inside a session worktree")?;
        let stored =
            session::set_note(&self.workspace_root, name, &args.note).map_err(|e| e.to_string())?;
        Ok(to_pretty(&json!({
            "name": name,
            "note": stored,
        })))
    }

    fn find_session(&self, name: &str) -> Result<SessionRecord, String> {
        let sessions = session::list(&self.workspace_root).map_err(|e| e.to_string())?;
        sessions
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("no such session: \"{}\"", name))
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
            "session_status" => self.tool_list_status(),
            "session_prompt" => self.tool_prompt(arguments),
            "session_pr" => self.tool_pr(arguments),
            "session_remove" => self.tool_remove(arguments),
            "session_note_get" => self.tool_note_get(),
            "session_note_update" => self.tool_note_update(arguments),
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

// --- argument shapes -------------------------------------------------------

#[derive(Deserialize)]
struct CreateArgs {
    name: String,
    /// Optional agent CLI this session launches with, overriding the workspace
    /// effective `agent_cli`. Accepts `claude` / `codex` / `sakana.ai` /
    /// `gemini` / `antigravity` (case-insensitive). Absent defers to the workspace setting.
    #[serde(default)]
    agent_cli: Option<String>,
    /// Optional model the session's agent CLI runs (rendered as `--model` / `-m`).
    /// Absent lets the CLI use its configured default.
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct PromptArgs {
    name: String,
    prompt: String,
    /// Which delivery channel to use; defaults to [`PromptMode::Auto`].
    #[serde(default)]
    mode: PromptMode,
    /// Optional agent CLI this session should launch with from now on. When
    /// present, `session_prompt` stores the override before queuing/sending the
    /// prompt so the next fresh launch uses this CLI. Absent leaves any existing
    /// per-session CLI override unchanged.
    #[serde(default)]
    agent_cli: Option<String>,
    /// Optional model this session's agent CLI should launch with from now on.
    /// Absent leaves any existing per-session model override unchanged.
    #[serde(default)]
    model: Option<String>,
}

/// How `session_prompt` chooses between the launch queue and the live pane.
#[derive(Deserialize, Clone, Copy, Default, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PromptMode {
    /// Deliver live when a live pane is detected, otherwise queue for launch.
    #[default]
    Auto,
    /// Always queue for the session's next fresh agent launch.
    Queue,
    /// Always append to the live queue for an already-running pane.
    Live,
}

/// The channel a prompt was actually delivered over, reported back to the caller
/// as `delivered_to`.
#[derive(Clone, Copy)]
pub(crate) enum Channel {
    Queue,
    Live,
}

impl Channel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Channel::Queue => "queue",
            Channel::Live => "live",
        }
    }
}

#[derive(Deserialize)]
struct PrArgs {
    name: String,
}

#[derive(Deserialize)]
struct RemoveArgs {
    name: String,
    /// Discard uncommitted changes instead of refusing; defaults to `false` when
    /// the caller omits it.
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
struct NoteUpdateArgs {
    /// New note text. An empty string (or one that trims to empty) clears the note.
    note: String,
}

// --- helpers ---------------------------------------------------------------

/// Derive the session name from the worktree path when it sits under
/// `<workspace_root>/.usagi/sessions/<name>/`, returning `None` when the
/// worktree is not inside a session directory.
fn derive_current_session(worktree: &Path, workspace_root: &Path) -> Option<String> {
    use crate::infrastructure::repo_paths::{SESSIONS_DIR, STATE_DIR};
    let sessions_dir = workspace_root.join(STATE_DIR).join(SESSIONS_DIR);
    worktree
        .strip_prefix(&sessions_dir)
        .ok()?
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
}

// --- JSON helpers ----------------------------------------------------------

fn sessions_to_json(sessions: &[SessionRecord]) -> Value {
    Value::Array(
        sessions
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "display_name": s.display_name,
                    "origin": s.origin.as_str(),
                    "started_from": s.started_from,
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

/// Serialize the session statuses for `session_status`. Each session carries its
/// agent lifecycle phase (`none` when no pane has run there) and each worktree's
/// cached git status plus the `dirty` / `merged` booleans a coordinator polls.
fn statuses_to_json(statuses: &[session::SessionStatus]) -> Value {
    Value::Array(
        statuses
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "display_name": s.display_name,
                    "origin": s.origin.as_str(),
                    "started_from": s.started_from,
                    "root": s.root,
                    "agent_phase": s.agent_phase.map_or("none", |p| p.as_str()),
                    "worktrees": s.worktrees.iter().map(|wt| json!({
                        "path": wt.path,
                        "branch": wt.branch,
                        "status": wt.status.as_str(),
                        "dirty": wt.dirty,
                        "merged": wt.merged,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

/// Serialize the stored per-session agent override in a stable shape for MCP
/// tool results.
fn session_agent_to_json(agent: &SessionAgent) -> Value {
    json!({
        "cli": agent.cli.map(|cli| cli.command()),
        "model": agent.model.as_deref(),
    })
}

/// JSON Schemas for the session tools advertised via `tools/list`.
fn session_tool_schemas() -> Value {
    json!([
        {
            "name": "session_create",
            "description": "Create a new usagi session: a parallel worktree under \
                .usagi/sessions/<name>/ on a fresh branch usagi/<name> for every \
                repository in the workspace. Optionally pin the agent CLI and model \
                this session launches with (agent_cli / model), overriding the \
                workspace's effective settings for just this session — so you can \
                send a light task to a small model and a heavy one to a large model. \
                Returns the session name, root, and worktree paths.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Session name (the branch it cuts in every repository is usagi/<name>)"
                    },
                    "agent_cli": {
                        "type": "string",
                        "enum": ["claude", "codex", "sakana.ai", "gemini", "antigravity"],
                        "description": "Agent CLI this session launches (default: the workspace effective agent_cli)"
                    },
                    "model": {
                        "type": "string",
                        "description": "Model the session's agent CLI runs, e.g. a specific Claude/Codex/Gemini model (default: the CLI's own default)"
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
            "name": "session_status",
            "description": "Report each session's orchestration status for a \
                coordinating agent to poll: its agent lifecycle phase (\"ready\" / \
                \"running\" / \"waiting\" / \"ended\", or \"none\" when no agent \
                pane has run in it) and, per worktree, the git status (\"new\" / \
                \"dirty\" / \"local\" / \"pushed\" / \"synced\") plus `dirty` and \
                `merged` booleans. `merged` is true when the default branch \
                already contains all of the worktree (status \"synced\"). \
                Read-only and cheap — it reads the cached state.json and the \
                agent-phase files with no git spawn, so the values are as fresh \
                as the latest workspace sync. A coordinator watches for \
                agent_phase \"ended\" (child finished) and `merged` (work landed) \
                to know a session is done, then removes it and delegates the next \
                issue.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "session_prompt",
            "description": "Deliver a prompt to a specific session's agent, so you \
                can delegate a task to a parallel session. Work stays isolated on \
                the session's worktree branch. It does not run the agent or return \
                its response here. Two delivery channels, chosen by `mode`: the \
                launch queue delivers the prompt as the agent's opening message the \
                next time that session's pane is freshly launched from the usagi \
                home screen; the live queue types it into an already-running agent \
                pane (waiting if none is open yet). `mode` defaults to `auto`, \
                which delivers live when the session has a live agent pane and \
                queues for launch otherwise — so you need not know whether the \
                agent is currently running. Optionally set `agent_cli` and/or \
                `model` to re-pin the session's future agent launches before the \
                prompt is delivered; a currently running live pane is not \
                restarted, so the override applies after it is relaunched. The \
                result's `delivered_to` reports \
                which channel took the prompt (\"live\" or \"queue\"). Pass the \
                reserved name \":root\" to target the workspace's root row (the \
                coordinator running there) instead of a session: a child session's \
                agent uses this to push a completion report up to the coordinator \
                the moment it finishes, so the coordinator advances without polling.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Target session name, or \":root\" for the workspace root row (the coordinator)" },
                    "prompt": { "type": "string", "description": "The task or question for the session's agent" },
                    "mode": {
                        "type": "string",
                        "enum": ["auto", "queue", "live"],
                        "description": "auto (default): live if a pane is running, else queue for launch. queue: always the next fresh launch. live: always the running pane (waits if none open)."
                    },
                    "agent_cli": {
                        "type": "string",
                        "enum": ["claude", "codex", "codex-fugu", "gemini", "agy"],
                        "description": "Agent CLI this session should launch with from now on (default: leave the existing CLI override unchanged)"
                    },
                    "model": {
                        "type": "string",
                        "description": "Model this session's agent CLI should launch with from now on (default: leave the existing model override unchanged; blank clears it)"
                    }
                },
                "required": ["name", "prompt"]
            }
        },
        {
            "name": "session_pr",
            "description": "Return the pull requests recorded for a specific \
                session. These are the PR URLs harvested from that session's \
                agent output and shown as PR badges in the TUI. Each PR carries a \
                `state` (\"merged\" once the session's branches are all merged \
                into the default branch, else \"open\"), and the result's top-level \
                `merged` reports the same signal. State is derived from the cached \
                worktree status (usagi never queries GitHub), so a PR closed \
                without merging is not distinguished from an open one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Target session name" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "session_remove",
            "description": "Remove a session: tear down every repository's worktree \
                and its session branch, drop any copied files, discard the \
                session agent's conversation, and forget it in state.json. \
                Without force, a session whose worktrees have uncommitted changes \
                is left untouched and the result lists those dirty worktrees \
                (removed=false); set force=true to discard the changes and remove \
                it anyway. Returns { name, removed, dirty }.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the session to remove" },
                    "force": {
                        "type": "boolean",
                        "description": "Discard uncommitted changes instead of refusing (default false)"
                    }
                },
                "required": ["name"]
            }
        },
        {
            "name": "session_note_get",
            "description": "Return the free-form note stored for the current session \
                (the session whose worktree the agent is running inside). \
                Returns { name, note } where note is null when none has been written. \
                Only available from inside a session worktree.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "session_note_update",
            "description": "Set (or clear) the free-form note for the current session. \
                The note is stored verbatim; trailing whitespace is trimmed and an \
                empty note clears it. Returns { name, note } with the value now stored \
                (null when cleared). Only available from inside a session worktree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": {
                        "type": "string",
                        "description": "Note text to store. Pass an empty string to clear."
                    }
                },
                "required": ["note"]
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::PrLink;
    use crate::infrastructure::git::test_command as git_cmd;
    use crate::infrastructure::pr_link_store;
    use crate::presentation::mcp::PROTOCOL_VERSION;
    use std::cell::RefCell;
    use std::fs;
    use std::rc::Rc;

    type CallLog = Rc<RefCell<Vec<(PathBuf, String)>>>;
    type RemoveLog = Rc<RefCell<Vec<(PathBuf, String, bool)>>>;

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

    /// A backend that records the calls it received and returns a scripted
    /// result, so the server's dispatch can be tested without a real agent. The
    /// call logs are shared via `Rc` so a test can inspect them after the backend
    /// is moved into the server.
    struct FakeBackend {
        result: Result<String, String>,
        calls: CallLog,
        remove_result: Result<session::RemovalOutcome, String>,
        remove_calls: RemoveLog,
        /// What `agent_is_live` reports, so a test can steer `auto` mode toward the
        /// live or the launch channel. Defaults to `false` (no live pane).
        live: bool,
    }

    impl FakeBackend {
        fn ok(reply: &str) -> Self {
            Self {
                result: Ok(reply.to_string()),
                calls: Rc::new(RefCell::new(Vec::new())),
                // A clean removal by default; tests that exercise the remove tool
                // override this with `with_remove`.
                remove_result: Ok(session::RemovalOutcome {
                    removed: true,
                    dirty: Vec::new(),
                }),
                remove_calls: Rc::new(RefCell::new(Vec::new())),
                live: false,
            }
        }

        fn err(message: &str) -> Self {
            Self {
                result: Err(message.to_string()),
                calls: Rc::new(RefCell::new(Vec::new())),
                remove_result: Ok(session::RemovalOutcome {
                    removed: true,
                    dirty: Vec::new(),
                }),
                remove_calls: Rc::new(RefCell::new(Vec::new())),
                live: false,
            }
        }

        /// Script the outcome `session_remove` returns.
        fn with_remove(mut self, outcome: Result<session::RemovalOutcome, String>) -> Self {
            self.remove_result = outcome;
            self
        }

        /// Script whether a live agent pane is detected, steering `auto` mode.
        fn with_live(mut self, live: bool) -> Self {
            self.live = live;
            self
        }
    }

    impl AgentBackend for FakeBackend {
        fn prompt(&self, worktree: &Path, prompt: &str) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((worktree.to_path_buf(), prompt.to_string()));
            self.result.clone()
        }

        fn send(&self, worktree: &Path, prompt: &str) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((worktree.to_path_buf(), prompt.to_string()));
            self.result.clone()
        }

        fn agent_is_live(&self, _worktree: &Path) -> bool {
            self.live
        }

        fn remove(
            &self,
            workspace_root: &Path,
            name: &str,
            force: bool,
        ) -> Result<session::RemovalOutcome, String> {
            self.remove_calls.borrow_mut().push((
                workspace_root.to_path_buf(),
                name.to_string(),
                force,
            ));
            self.remove_result.clone()
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
        let runner = Box::new(FakeRunner(vec!["claude", "codex", "codex-fugu"]));
        SessionMcpServer::new(root.to_path_buf(), root, Box::new(backend), runner)
    }

    fn server_in_session(root: &Path, name: &str, backend: FakeBackend) -> SessionMcpServer {
        let worktree = root.join(".usagi").join("sessions").join(name);
        fs::create_dir_all(&worktree).unwrap();
        let runner = Box::new(FakeRunner(vec!["claude", "codex", "codex-fugu"]));
        SessionMcpServer::new(root.to_path_buf(), &worktree, Box::new(backend), runner)
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
    fn tools_list_returns_the_session_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let res = reply(
            &server_at(tmp.path(), FakeBackend::ok("x")),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        let tools = res["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "session_create",
                "session_list",
                "session_status",
                "session_prompt",
                "session_pr",
                "session_remove",
                "session_note_get",
                "session_note_update",
            ]
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
        // No sidebar override set yet, so display_name is present but null.
        assert_eq!(arr[0]["display_name"], Value::Null);
        // Created through the MCP server, so it is recorded — and surfaced — as an
        // agent-launched session.
        assert_eq!(arr[0]["origin"], "mcp");
        // Created from the workspace root (no parent session), so started_from is null.
        assert_eq!(arr[0]["started_from"], Value::Null);
        // The worktree is checked out on the namespaced session branch.
        assert_eq!(arr[0]["worktrees"][0]["branch"], "usagi/feature-x");
    }

    #[test]
    fn resolve_session_agent_parses_a_known_cli_passes_the_model_and_defaults_when_absent() {
        // A known CLI name (case-insensitive, via AgentCli::from_name) resolves and
        // the model rides through untouched.
        let runner = FakeRunner(vec!["claude", "codex-fugu"]);
        let agent = resolve_session_agent(
            &runner,
            Some("Claude"),
            Some("claude-3-5-sonnet".to_string()),
        )
        .expect("known cli");
        assert_eq!(agent.cli, Some(AgentCli::Claude));
        assert_eq!(agent.model.as_deref(), Some("claude-3-5-sonnet"));
        // The user-facing SakanaAi label resolves.
        assert_eq!(
            resolve_session_agent(&runner, Some("sakana.ai"), None)
                .unwrap()
                .cli,
            Some(AgentCli::SakanaAi)
        );
        // The old launch-command spelling remains a compatibility alias.
        assert_eq!(
            resolve_session_agent(&runner, Some("codex-fugu"), None)
                .unwrap()
                .cli,
            Some(AgentCli::SakanaAi)
        );
        // Neither argument yields the default (follow the workspace settings).
        assert!(resolve_session_agent(&runner, None, None)
            .unwrap()
            .is_unset());
    }

    #[test]
    fn resolve_session_agent_rejects_an_unknown_cli() {
        let runner = FakeRunner(vec!["claude"]);
        let err = resolve_session_agent(&runner, Some("gpt"), None).unwrap_err();
        assert!(err.contains("unknown agent_cli"), "{err}");
        assert!(err.contains("claude"), "{err}");
    }

    #[test]
    fn resolve_session_agent_rejects_uninstalled_cli() {
        let runner = FakeRunner(vec!["claude"]);
        let err = resolve_session_agent(&runner, Some("codex"), None).unwrap_err();
        assert!(err.contains("not installed or not MCP-capable"), "{err}");
        assert!(err.contains("claude"), "{err}");
    }

    #[test]
    fn fake_runner_non_probe_methods_are_inert() {
        let runner = FakeRunner(vec![]);
        assert!(runner.run("x", &[]).unwrap());
        assert!(runner.check("x", &[]));
        assert!(runner.spawn("x", &[]).is_ok());
    }

    #[test]
    fn create_records_the_agent_cli_and_model_override() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("x"));

        let created = call(
            &server,
            "session_create",
            json!({"name":"pinned","agent_cli":"claude","model":"claude-3-5-sonnet"}),
        );
        assert_eq!(created["isError"], false);

        // The override lands on the SessionRecord in state.json.
        let store = crate::infrastructure::workspace_store::WorkspaceStore::new(root.path());
        let session = &store.load().unwrap().unwrap().sessions[0];
        assert_eq!(session.agent.cli, Some(AgentCli::Claude));
        assert_eq!(session.agent.model.as_deref(), Some("claude-3-5-sonnet"));
    }

    #[test]
    fn create_from_within_a_session_records_which_session_it_was_started_from() {
        // An agent whose MCP process runs inside session "coordinator" creates a
        // child session. The child must record that it was started from
        // "coordinator" — the session-lineage answer to "which session started
        // this one?".
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // First cut the parent session so its worktree exists on disk.
        let parent = server_at(root.path(), FakeBackend::ok("x"));
        call(&parent, "session_create", json!({"name":"coordinator"}));

        // Now act as the agent running inside "coordinator" and create a child.
        let inside = server_in_session(root.path(), "coordinator", FakeBackend::ok("x"));
        let created = call(&inside, "session_create", json!({"name":"child"}));
        assert_eq!(created["isError"], false);

        // state.json records the parent on the child, and the parent itself has
        // none (it was cut from the workspace root).
        let store = crate::infrastructure::workspace_store::WorkspaceStore::new(root.path());
        let state = store.load().unwrap().unwrap();
        let child = state.sessions.iter().find(|s| s.name == "child").unwrap();
        assert_eq!(child.started_from.as_deref(), Some("coordinator"));
        let coordinator = state
            .sessions
            .iter()
            .find(|s| s.name == "coordinator")
            .unwrap();
        assert_eq!(coordinator.started_from, None);

        // And it is surfaced to callers through session_list / session_status.
        let listed = tool_json(&call(&inside, "session_list", json!({})));
        let child_json = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "child")
            .unwrap();
        assert_eq!(child_json["started_from"], "coordinator");
    }

    #[test]
    fn create_with_an_unknown_agent_cli_is_a_tool_error_and_creates_nothing() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("x"));

        let result = call(
            &server,
            "session_create",
            json!({"name":"bad","agent_cli":"gpt"}),
        );
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown agent_cli"));
        // The bad request never created a session.
        assert_eq!(
            tool_json(&call(&server, "session_list", json!({}))),
            json!([])
        );
    }

    #[test]
    fn list_includes_a_sessions_display_name() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("x"));
        call(&server, "session_create", json!({"name":"feature-x"}));

        // A sidebar display name set through the usecase appears in the listing.
        session::set_display_name(root.path(), "feature-x", "Nice Name").unwrap();

        let listed = tool_json(&call(&server, "session_list", json!({})));
        assert_eq!(listed[0]["display_name"], "Nice Name");
    }

    #[test]
    fn create_duplicate_is_a_tool_error_and_is_logged() {
        // Point the data dir at a temp home so the failure's ErrorLog entry is
        // captured here instead of polluting the real `~/.usagi/logs/`.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

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

        // The duplicate failure was also recorded to the error log, in the same
        // wording as the TUI's session-create entries.
        let logs = home.path().join("logs");
        let entry = fs::read_dir(&logs)
            .expect("logs dir exists")
            .next()
            .expect("a log file was written")
            .expect("readable entry");
        let contents = fs::read_to_string(entry.path()).unwrap();
        assert!(contents.contains("mcp session_create \"dup\" failed:"));

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn auto_prompt_queues_for_launch_when_no_live_pane() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No live pane detected, so `auto` resolves to the launch queue.
        let backend = FakeBackend::ok("done").with_live(false);
        let calls = backend.calls.clone(); // inspect after the backend is moved in
        let server = server_at(root.path(), backend);
        call(&server, "session_create", json!({"name":"work"}));

        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"add a test"}),
        );
        assert_eq!(result["isError"], false);
        let body = tool_json(&result);
        assert_eq!(body["delivered_to"], "queue");
        assert_eq!(body["detail"], "done");

        // The backend was invoked once with the session's worktree root and the
        // prompt text verbatim.
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, root.path().join(".usagi/sessions/work"));
        assert_eq!(calls[0].1, "add a test");
    }

    #[test]
    fn prompt_with_agent_override_updates_the_session_before_queueing() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let backend = FakeBackend::ok("done").with_live(false);
        let calls = backend.calls.clone();
        let server = server_at(root.path(), backend);
        call(&server, "session_create", json!({"name":"work"}));

        let result = call(
            &server,
            "session_prompt",
            json!({
                "name": "work",
                "prompt": "heavy follow-up",
                "mode": "queue",
                "agent_cli": "codex-fugu",
                "model": "  fugu-ultra  "
            }),
        );
        assert_eq!(result["isError"], false);
        let body = tool_json(&result);
        assert_eq!(body["delivered_to"], "queue");
        assert_eq!(
            body["agent"],
            json!({"cli":"codex-fugu","model":"fugu-ultra"})
        );
        assert_eq!(calls.borrow().len(), 1);

        let session = session::list(root.path())
            .unwrap()
            .into_iter()
            .find(|s| s.name == "work")
            .unwrap();
        assert_eq!(session.agent.cli, Some(AgentCli::CodexFugu));
        assert_eq!(session.agent.model.as_deref(), Some("fugu-ultra"));
    }

    #[test]
    fn prompt_agent_override_is_a_partial_update() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("done"));
        call(
            &server,
            "session_create",
            json!({"name":"work","agent_cli":"claude","model":"claude-3-5-sonnet"}),
        );

        let model_only = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"same cli, new model","model":"claude-opus"}),
        );
        assert_eq!(model_only["isError"], false);
        let session = session::list(root.path())
            .unwrap()
            .into_iter()
            .find(|s| s.name == "work")
            .unwrap();
        assert_eq!(session.agent.cli, Some(AgentCli::Claude));
        assert_eq!(session.agent.model.as_deref(), Some("claude-opus"));

        let cli_only = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"new cli, same model","agent_cli":"codex-fugu"}),
        );
        assert_eq!(cli_only["isError"], false);
        let session = session::list(root.path())
            .unwrap()
            .into_iter()
            .find(|s| s.name == "work")
            .unwrap();
        assert_eq!(session.agent.cli, Some(AgentCli::CodexFugu));
        assert_eq!(session.agent.model.as_deref(), Some("claude-opus"));
    }

    #[test]
    fn prompt_agent_override_rejects_root_target_and_unknown_cli() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let backend = FakeBackend::ok("done");
        let calls = backend.calls.clone();
        let server = server_at(root.path(), backend);
        call(&server, "session_create", json!({"name":"work"}));

        let root_result = call(
            &server,
            "session_prompt",
            json!({"name":":root","prompt":"report","agent_cli":"claude"}),
        );
        assert_eq!(root_result["isError"], true);
        assert!(root_result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cannot be used with the :root target"));

        let bad_cli = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"hi","agent_cli":"gemini"}),
        );
        assert_eq!(bad_cli["isError"], true);
        assert!(bad_cli["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not installed or not MCP-capable"));

        // Both errors happen before anything reaches a prompt queue.
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn auto_prompt_delivers_live_when_a_pane_is_detected() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // A live pane is detected, so `auto` resolves to the live channel.
        let backend = FakeBackend::ok("sent").with_live(true);
        let calls = backend.calls.clone();
        let server = server_at(root.path(), backend);
        call(&server, "session_create", json!({"name":"work"}));

        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"continue here"}),
        );
        assert_eq!(result["isError"], false);
        let body = tool_json(&result);
        assert_eq!(body["delivered_to"], "live");
        assert_eq!(body["detail"], "sent");

        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, root.path().join(".usagi/sessions/work"));
        assert_eq!(calls[0].1, "continue here");
    }

    #[test]
    fn explicit_mode_overrides_the_detected_pane_state() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        call(
            &server_at(root.path(), FakeBackend::ok("x")),
            "session_create",
            json!({"name":"w"}),
        );

        // `queue` forces the launch channel even though a pane is live…
        let queued = call(
            &server_at(root.path(), FakeBackend::ok("q").with_live(true)),
            "session_prompt",
            json!({"name":"w","prompt":"hi","mode":"queue"}),
        );
        assert_eq!(tool_json(&queued)["delivered_to"], "queue");

        // …and `live` forces the live channel even though none is detected.
        let live = call(
            &server_at(root.path(), FakeBackend::ok("l").with_live(false)),
            "session_prompt",
            json!({"name":"w","prompt":"hi","mode":"live"}),
        );
        assert_eq!(tool_json(&live)["delivered_to"], "live");
    }

    #[test]
    fn prompt_targets_the_root_row_live_without_a_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // The coordinator's pane is live, so `:root` delivers live — and no
        // session need exist: `:root` resolves straight to the workspace root.
        let backend = FakeBackend::ok("reported").with_live(true);
        let calls = backend.calls.clone();
        let server = server_at(root.path(), backend);

        let result = call(
            &server,
            "session_prompt",
            json!({"name":":root","prompt":"issue #101 done, PR #123 opened"}),
        );
        assert_eq!(result["isError"], false);
        let body = tool_json(&result);
        assert_eq!(body["name"], ":root");
        assert_eq!(body["delivered_to"], "live");
        assert_eq!(body["detail"], "reported");

        // The report was addressed to the workspace root itself (the root row's
        // working dir), not any `.usagi/sessions/<name>` path.
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, root.path());
        assert_eq!(calls[0].1, "issue #101 done, PR #123 opened");
    }

    #[test]
    fn prompt_targets_the_root_row_and_queues_when_no_live_coordinator() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        // No live coordinator pane, so `auto` queues the report for the root row's
        // next fresh agent launch — again without requiring any session.
        let backend = FakeBackend::ok("queued").with_live(false);
        let calls = backend.calls.clone();
        let server = server_at(root.path(), backend);

        let result = call(
            &server,
            "session_prompt",
            json!({"name":":root","prompt":"done"}),
        );
        assert_eq!(result["isError"], false);
        let body = tool_json(&result);
        assert_eq!(body["delivered_to"], "queue");
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, root.path());
    }

    #[test]
    fn prompt_surfaces_backend_send_errors_in_live_mode() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::err("agent not reachable"));
        call(&server, "session_create", json!({"name":"w"}));
        let result = call(
            &server,
            "session_prompt",
            json!({"name":"w","prompt":"hi","mode":"live"}),
        );
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("agent not reachable"));
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
    fn prompt_rejects_an_oversized_prompt_before_reaching_the_backend() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let backend = FakeBackend::ok("queued");
        let calls = backend.calls.clone();
        let server = server_at(root.path(), backend);
        call(&server, "session_create", json!({"name":"w"}));
        let huge = "x".repeat(MAX_PROMPT_BYTES + 1);
        let result = call(&server, "session_prompt", json!({"name":"w","prompt":huge}));
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("session_prompt prompt is too large"),
            "{text}"
        );
        // Rejected before the backend, so nothing was persisted to the launch queue.
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn a_prompt_exactly_at_the_cap_is_accepted() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let backend = FakeBackend::ok("sent");
        let calls = backend.calls.clone();
        let server = server_at(root.path(), backend);
        call(&server, "session_create", json!({"name":"w"}));
        // Exactly at the limit is allowed (the check rejects only *over* it), so it
        // reaches the backend verbatim.
        let at_limit = "x".repeat(MAX_PROMPT_BYTES);
        let result = call(
            &server,
            "session_prompt",
            json!({"name":"w","prompt":at_limit}),
        );
        assert_eq!(result["isError"], false);
        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(calls.borrow()[0].1.len(), MAX_PROMPT_BYTES);
    }

    #[test]
    fn check_prompt_len_names_the_tool_and_limit() {
        // A within-limit prompt passes; an over-limit one is rejected with a
        // message naming the tool and the byte limit (covering the helper directly).
        assert!(check_prompt_len("session_prompt", "ok").is_ok());
        let err =
            check_prompt_len("session_prompt", &"x".repeat(MAX_PROMPT_BYTES + 1)).unwrap_err();
        assert!(err.contains("session_prompt prompt is too large"), "{err}");
        assert!(err.contains(&MAX_PROMPT_BYTES.to_string()), "{err}");
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
    fn pr_returns_the_pull_requests_recorded_for_the_session() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("x"));
        call(&server, "session_create", json!({"name":"work"}));
        let worktree = root.path().join(".usagi/sessions/work");
        pr_link_store::add(
            &worktree,
            &[
                PrLink {
                    number: 12,
                    url: "https://github.com/o/r/pull/12".to_string(),
                },
                PrLink {
                    number: 34,
                    url: "https://github.com/o/r/pull/34".to_string(),
                },
            ],
        )
        .unwrap();

        let result = call(&server, "session_pr", json!({"name":"work"}));
        assert_eq!(result["isError"], false);
        assert_eq!(
            tool_json(&result),
            json!({
                "name": "work",
                "root": worktree,
                // A freshly created session's branch is not yet merged, so every
                // PR reads "open".
                "merged": false,
                "pr": [
                    {"number": 12, "url": "https://github.com/o/r/pull/12", "state": "open"},
                    {"number": 34, "url": "https://github.com/o/r/pull/34", "state": "open"},
                ]
            })
        );

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn status_reports_agent_phase_and_worktree_status() {
        // agent_state_store reads under the data dir, so point it at a throwaway
        // home for the duration of the test.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("x"));
        // Before any session exists the tool returns an empty array.
        assert_eq!(
            tool_json(&call(&server, "session_status", json!({}))),
            json!([])
        );
        call(&server, "session_create", json!({"name":"work"}));

        // No agent pane has run: phase reads "none"; the fresh branch reads "local".
        let listed = tool_json(&call(&server, "session_status", json!({})));
        let arr = listed.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "work");
        assert_eq!(arr[0]["display_name"], Value::Null);
        assert_eq!(arr[0]["agent_phase"], "none");
        // session_status surfaces the launch origin too; this one was cut via MCP.
        assert_eq!(arr[0]["origin"], "mcp");
        // Cut from the workspace root, so it has no parent session.
        assert_eq!(arr[0]["started_from"], Value::Null);
        assert_eq!(arr[0]["worktrees"][0]["status"], "local");
        assert_eq!(arr[0]["worktrees"][0]["dirty"], false);
        assert_eq!(arr[0]["worktrees"][0]["merged"], false);
        assert_eq!(arr[0]["worktrees"][0]["branch"], "usagi/work");

        // A recorded phase for the session root surfaces as agent_phase.
        let worktree = root.path().join(".usagi/sessions/work");
        crate::infrastructure::agent_state_store::write(
            &worktree,
            crate::domain::agent_phase::AgentPhase::Ended,
        )
        .unwrap();
        let listed = tool_json(&call(&server, "session_status", json!({})));
        assert_eq!(listed[0]["agent_phase"], "ended");

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn status_surfaces_a_usecase_error() {
        // A file where the `.usagi` directory should be makes the store fail,
        // exercising the tool's error path.
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".usagi"), "blocker").unwrap();
        let server = server_at(tmp.path(), FakeBackend::ok("x"));
        let result = call(&server, "session_status", json!({}));
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn pr_reports_merged_state_once_the_branch_is_synced() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("x"));
        call(&server, "session_create", json!({"name":"work"}));
        let worktree = root.path().join(".usagi/sessions/work");
        pr_link_store::add(
            &worktree,
            &[PrLink {
                number: 7,
                url: "https://github.com/o/r/pull/7".to_string(),
            }],
        )
        .unwrap();

        // Mark the session's only worktree synced (merged into the default branch).
        let store = crate::infrastructure::workspace_store::WorkspaceStore::new(root.path());
        let mut state = store.load().unwrap().unwrap();
        state.sessions[0].worktrees[0].status =
            crate::domain::workspace_state::BranchStatus::Synced;
        store.save(&state).unwrap();

        let body = tool_json(&call(&server, "session_pr", json!({"name":"work"})));
        assert_eq!(body["merged"], true);
        assert_eq!(body["pr"][0]["state"], "merged");
        assert_eq!(body["pr"][0]["number"], 7);

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn pr_for_an_unknown_session_is_a_tool_error() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("x"));
        let result = call(&server, "session_pr", json!({"name":"ghost"}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no such session"));
    }

    #[test]
    fn remove_forwards_to_the_backend_and_formats_a_clean_removal() {
        let root = tempfile::tempdir().unwrap();
        let backend = FakeBackend::ok("x");
        let calls = backend.remove_calls.clone(); // inspect after the move
        let server = server_at(root.path(), backend);

        let result = call(&server, "session_remove", json!({"name":"feature-x"}));
        assert_eq!(result["isError"], false);
        let body = tool_json(&result);
        assert_eq!(body, json!({"name":"feature-x","removed":true,"dirty":[]}));

        // The backend was invoked once with the workspace root, the session name,
        // and force defaulted to false.
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, root.path());
        assert_eq!(calls[0].1, "feature-x");
        assert!(!calls[0].2);
    }

    #[test]
    fn remove_reports_dirty_worktrees_when_blocked() {
        let root = tempfile::tempdir().unwrap();
        let dirty = root.path().join(".usagi/sessions/wip");
        let backend = FakeBackend::ok("x").with_remove(Ok(session::RemovalOutcome {
            removed: false,
            dirty: vec![dirty.clone()],
        }));
        let server = server_at(root.path(), backend);

        let result = call(&server, "session_remove", json!({"name":"wip"}));
        // A blocked removal is a successful tool call whose body says removed=false.
        assert_eq!(result["isError"], false);
        let body = tool_json(&result);
        assert_eq!(body["removed"], false);
        assert_eq!(body["dirty"][0], dirty.to_str().unwrap());
    }

    #[test]
    fn remove_passes_the_force_flag_through() {
        let root = tempfile::tempdir().unwrap();
        let backend = FakeBackend::ok("x");
        let calls = backend.remove_calls.clone();
        let server = server_at(root.path(), backend);

        call(
            &server,
            "session_remove",
            json!({"name":"wip","force":true}),
        );
        assert!(calls.borrow()[0].2);
    }

    #[test]
    fn remove_surfaces_backend_errors_as_tool_errors() {
        let root = tempfile::tempdir().unwrap();
        let backend =
            FakeBackend::ok("x").with_remove(Err("no such session: \"ghost\"".to_string()));
        let server = server_at(root.path(), backend);

        let result = call(&server, "session_remove", json!({"name":"ghost"}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no such session"));
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
        // An unknown `mode` is an invalid argument, too.
        let result = call(
            &server,
            "session_prompt",
            json!({"name":"w","prompt":"hi","mode":"bogus"}),
        );
        assert_eq!(result["isError"], true);
        // session_pr requires a name.
        let result = call(&server, "session_pr", json!({}));
        assert_eq!(result["isError"], true);
        // session_remove requires a name.
        let result = call(&server, "session_remove", json!({}));
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
    fn note_get_returns_null_when_no_note_set() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_in_session(root.path(), "work", FakeBackend::ok("x"));
        call(&server, "session_create", json!({"name":"work"}));

        let result = call(&server, "session_note_get", json!({}));
        assert_eq!(result["isError"], false);
        let body = tool_json(&result);
        assert_eq!(body["name"], "work");
        assert_eq!(body["note"], Value::Null);
    }

    #[test]
    fn note_update_and_get_round_trip() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_in_session(root.path(), "work", FakeBackend::ok("x"));
        call(&server, "session_create", json!({"name":"work"}));

        let update = call(&server, "session_note_update", json!({"note":"memo line"}));
        assert_eq!(update["isError"], false);
        let body = tool_json(&update);
        assert_eq!(body["name"], "work");
        assert_eq!(body["note"], "memo line");

        let get = call(&server, "session_note_get", json!({}));
        assert_eq!(get["isError"], false);
        assert_eq!(tool_json(&get)["note"], "memo line");
    }

    #[test]
    fn note_update_with_empty_string_clears_the_note() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_in_session(root.path(), "work", FakeBackend::ok("x"));
        call(&server, "session_create", json!({"name":"work"}));
        call(
            &server,
            "session_note_update",
            json!({"note":"to be cleared"}),
        );

        let cleared = call(&server, "session_note_update", json!({"note":""}));
        assert_eq!(cleared["isError"], false);
        assert_eq!(tool_json(&cleared)["note"], Value::Null);
    }

    #[test]
    fn note_tools_fail_when_not_inside_a_session() {
        // When the server's worktree is the workspace root (not under
        // .usagi/sessions/<name>), current_session is None and both note tools
        // return an error rather than panicking.
        let root = tempfile::tempdir().unwrap();
        let server = server_at(root.path(), FakeBackend::ok("x"));

        let get = call(&server, "session_note_get", json!({}));
        assert_eq!(get["isError"], true);
        assert!(get["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("inside a session worktree"));

        let upd = call(&server, "session_note_update", json!({"note":"x"}));
        assert_eq!(upd["isError"], true);
        assert!(upd["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("inside a session worktree"));
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

    #[test]
    fn note_get_surfaces_usecase_error_when_no_state_exists() {
        // A server inside a session worktree, but no state.json — get_note fails
        // "no sessions recorded" and the tool surfaces it as a tool error.
        // This also covers the map_err closure in tool_note_get.
        let root = tempfile::tempdir().unwrap();
        let server = server_in_session(root.path(), "work", FakeBackend::ok("x"));
        let result = call(&server, "session_note_get", json!({}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no sessions recorded"));
    }

    #[test]
    fn note_update_surfaces_usecase_error_when_session_not_in_state() {
        // A server inside "work" session, state.json exists but lists "other" —
        // set_note fails "no such session" and the tool surfaces it.
        // This also covers the map_err closure in tool_note_update.
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server_other = server_in_session(root.path(), "other", FakeBackend::ok("x"));
        call(&server_other, "session_create", json!({"name":"other"}));
        // Now there IS a state.json, but it only has "other", not "work".
        let server = server_in_session(root.path(), "work", FakeBackend::ok("x"));
        let result = call(&server, "session_note_update", json!({"note":"x"}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no such session"));
    }
}
