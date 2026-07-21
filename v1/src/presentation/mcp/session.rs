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
//! the *launch queue* and the *live queue* (typed into an already-running pane) —
//! chosen by its `mode`. An explicit `queue` is reserved for the next fresh agent
//! launch. `auto` (the default) picks the live channel when a live agent pane is
//! detected and otherwise stores a launch prompt that may also be handed to an
//! eligible existing agent after a TUI restart. The result reports which channel
//! actually took the prompt.
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

use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_args, to_pretty, McpService};
use crate::domain::settings::AgentCli;
use crate::domain::workspace_state::{SessionAgent, SessionOrigin, SessionRecord};
use crate::usecase::agent::AgentModelProbe;
use crate::usecase::doctor::CommandRunner;
use crate::usecase::session;

/// Names of the session tools this server exposes. The unified `usagi` server
/// ([`super::usagi`]) uses this to route `tools/call` for these names to the
/// embedded session server.
pub const TOOL_NAMES: [&str; 15] = [
    "session_create",
    "session_list",
    "session_status",
    "session_prompt",
    "session_complete",
    "session_pr",
    "session_remove",
    "session_note_get",
    "session_note_update",
    "session_todo_list",
    "session_todo_add",
    "session_todo_update",
    "session_todo_remove",
    "session_decision_list",
    "session_decision_log",
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
/// to the coordinator's running agent pane, or persisted for a later existing/fresh
/// agent when none is open. Spelled with a leading `:` so it can never be mistaken
/// for — or shadowed by — a real session, whose name is a git branch / directory component
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

/// Apply the same model normalisation used by the session usecase before a
/// value is validated: surrounding whitespace is irrelevant, and a blank value
/// clears the override instead of asking a probe about an empty model name.
fn normalize_session_agent(agent: SessionAgent) -> SessionAgent {
    SessionAgent {
        cli: agent.cli,
        model: agent
            .model
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty()),
    }
}

/// Resolve the optional per-session agent overrides an MCP caller passed
/// (`agent_cli` / `model`) into a [`SessionAgent`].
///
/// `agent_cli` is matched case-insensitively against each CLI's launch command,
/// display name, and serde label via [`AgentCli::from_name`]; an unrecognised
/// name is a tool error naming the accepted values so the caller can correct it.
/// `model` is carried to the caller, which resolves the effective CLI and checks
/// it with [`AgentModelProbe`] before any session state or prompt queue is
/// changed. Both absent yields the default: the session follows the workspace
/// effective settings and the CLI's own default model. Shared by
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

/// Whether a prompt persisted in the launch queue must start a fresh agent or
/// may be handed to an eligible existing agent after the TUI resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchPromptDelivery {
    /// Preserve the explicit launch-queue contract: this prompt is the opening
    /// message for the next fresh agent process.
    FreshLaunch,
    /// The prompt reached the launch queue only because `auto` found no live TUI
    /// consumer. A later TUI may hand it to an existing agent that is not
    /// reported running/waiting instead of waiting behind that pane forever.
    ReuseLiveAgent,
}

/// Drives the parts of session orchestration that touch a real agent or a real
/// filesystem — handing a session's agent a launch-time prompt, live-sending a
/// prompt, and removing a session (which discards that agent's conversation).
/// Abstracted so the server's
/// protocol handling can be tested with a fake backend that never touches the
/// filesystem or a real agent.
pub trait AgentBackend {
    /// Persist `prompt` in the launch queue rooted at `worktree`, returning a
    /// confirmation message (`Ok`) or an error message to surface to the agent
    /// (`Err`). `delivery` preserves whether the caller explicitly requested a
    /// fresh launch or `auto` merely fell back because no live TUI was detected.
    fn prompt(
        &self,
        worktree: &Path,
        prompt: &str,
        delivery: LaunchPromptDelivery,
    ) -> Result<String, String>;

    /// Deliver `prompt` to the agent that is already running in the session
    /// rooted at `worktree`. The production backend appends it to a live queue
    /// that the running TUI drains into the existing agent pane; if no such pane
    /// is open, it waits there until one is.
    fn send(&self, worktree: &Path, prompt: &str) -> Result<String, String>;

    /// Whether a live agent pane currently exists for the session rooted at
    /// `worktree`. `session_prompt`'s `auto` mode uses this to choose between the
    /// live channel ([`send`](Self::send)) and the launch queue
    /// ([`prompt`](Self::prompt)). The production backend answers from the
    /// pid-stamped live-pane marker a running TUI publishes and clears. A stale
    /// marker whose TUI pid is dead is treated as absent, so `auto` falls back to
    /// the durable launch channel instead of stranding work in the live queue.
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
    /// Checks an explicit model against the models currently available to its
    /// effective agent CLI. A failed or unsupported probe is fail-closed.
    model_probe: Box<dyn AgentModelProbe>,
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
        model_probe: Box<dyn AgentModelProbe>,
    ) -> Self {
        let current_session = derive_current_session(worktree, &workspace_root);
        Self {
            workspace_root,
            current_session,
            backend,
            runner,
            model_probe,
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
        let agent = self.resolve_agent_override(args.agent_cli.as_deref(), args.model)?;
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
            self.creation_error_message(name, &e)
        })
    }

    /// Turn a raw session-creation failure into the message surfaced to the
    /// calling agent. A permission-denied error while taking the workspace's
    /// `.usagi/` store lock means this MCP server was blocked from writing the
    /// parent workspace. The usual cause is that it is running inside a
    /// *sandboxed* session whose filesystem grant is limited to its own worktree
    /// (`.usagi/sessions/<name>/`), so it cannot create a sibling session rooted
    /// at the workspace. Replace the cryptic `Operation not permitted` surfaced
    /// from the lock file with the actual constraint and the supported path:
    /// delegate from the workspace-root coordinator — e.g. hand the task to
    /// `:root` with `session_prompt` and let it run the delegation.
    fn creation_error_message(&self, name: &str, err: &anyhow::Error) -> String {
        if !is_permission_denied(err) {
            return err.to_string();
        }
        match &self.current_session {
            Some(parent) => format!(
                "cannot create session \"{name}\" from inside session \"{parent}\": writing the \
                 workspace at {} was denied (permission denied). A sandboxed session can only \
                 write its own worktree, so it cannot spawn a sibling session. Delegate from the \
                 workspace-root coordinator instead — e.g. hand the task to `:root` with \
                 `session_prompt` and let it run the delegation.",
                self.workspace_root.display()
            ),
            None => format!(
                "cannot create session \"{name}\": writing the workspace at {} was denied \
                 (permission denied). Make sure the workspace `.usagi/` directory is writable.",
                self.workspace_root.display()
            ),
        }
    }

    fn tool_list(&self) -> Result<String, String> {
        let sessions = session::list(&self.workspace_root).map_err(|e| e.to_string())?;
        Ok(to_pretty(&sessions_to_json(&sessions)))
    }

    fn tool_prompt(&self, arguments: Value) -> Result<String, String> {
        let args: PromptArgs = parse_args(arguments)?;
        check_prompt_len("session_prompt", &args.prompt)?;
        let (target_root, channel) = self.resolve_prompt_delivery(&args.name, args.mode)?;
        let has_agent_override = args.agent_cli.is_some() || args.model.is_some();
        let stored_agent = if args.name == ROOT_TARGET {
            if has_agent_override {
                return Err(
                    "session_prompt agent_cli/model cannot be used with the :root target"
                        .to_string(),
                );
            }
            None
        } else {
            let existing = self.find_session(&args.name)?.agent;
            let cli = match args.agent_cli.as_deref() {
                Some(agent_cli) => resolve_session_agent(&*self.runner, Some(agent_cli), None)?.cli,
                None => existing.cli,
            };
            let candidate = self.prepare_session_agent(SessionAgent {
                cli,
                model: args.model.or_else(|| existing.model.clone()),
            })?;
            // Validate before persisting an override or appending to either
            // durable prompt queue. Even the live queue can become a fresh-launch
            // input if its pane exits before the TUI drains it, so every explicit
            // stored model must be positively checked on every prompt call.
            // Preparing can also normalise legacy state and bind a model-only
            // override to the effective CLI that was actually probed. Persist
            // that checked pair before either queue can launch it.
            if has_agent_override || candidate != existing {
                Some(self.set_session_agent(&args.name, candidate)?)
            } else {
                None
            }
        };
        let detail = self.dispatch_prompt(&target_root, &args.prompt, channel, args.mode)?;
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

    /// Report completion to the coordinator that created the current session.
    ///
    /// The return address is deliberately not supplied by the agent: session
    /// creation captured it once in `SessionRecord::started_from`. A top-level
    /// session therefore reports to the workspace root, while a nested session
    /// reports to its parent session. This keeps the completion call both small
    /// and resistant to an agent accidentally notifying the wrong coordinator.
    fn tool_complete(&self, arguments: Value) -> Result<String, String> {
        let args: CompleteArgs = parse_args(arguments)?;
        let name = self.require_session("session_complete")?;
        let current = self.find_session(name)?;
        let target = current.started_from.as_deref().unwrap_or(ROOT_TARGET);
        let report = format!("Session \"{name}\" completed:\n\n{}", args.message);
        check_prompt_len("session_complete", &report)?;
        self.validate_stored_session_agent(target)?;
        let (channel, detail) = self.deliver_prompt(target, &report, PromptMode::Auto)?;
        Ok(to_pretty(&json!({
            "session": name,
            "reported_to": target,
            "delivered_to": channel.as_str(),
            "detail": detail,
        })))
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
        let (target_root, channel) = self.resolve_prompt_delivery(name, mode)?;
        let detail = self.dispatch_prompt(&target_root, prompt, channel, mode)?;
        Ok((channel, detail))
    }

    /// Resolve a target and delivery channel once.
    ///
    /// An explicit `mode="queue"` aimed at a target that already has a live agent
    /// pane is rejected rather than silently queued. The launch queue only feeds
    /// the next *fresh* agent launch, and a live pane means no fresh launch is
    /// coming, so such a prompt would sit undelivered until the running agent
    /// exits — the "accepted but nothing happens" strand. `auto` already routes a
    /// live target to the live channel; the error steers the caller there (or to
    /// removing and relaunching the session), so an accepted prompt is never
    /// stranded. A target with no live pane keeps the fresh-launch contract.
    fn resolve_prompt_delivery(
        &self,
        name: &str,
        mode: PromptMode,
    ) -> Result<(PathBuf, Channel), String> {
        let target_root = self.resolve_target(name)?;
        let channel = match mode {
            PromptMode::Queue if self.backend.agent_is_live(&target_root) => {
                return Err(format!(
                    "\"{name}\" already has a running agent pane, so mode=\"queue\" would strand \
                     this prompt: the launch queue only feeds the next fresh agent launch, which \
                     will not happen while that pane is live. Send it with mode=\"auto\" (or \
                     \"live\") to reach the running agent, or remove and relaunch the session for \
                     a fresh queued start."
                ));
            }
            PromptMode::Queue => Channel::Queue,
            PromptMode::Live => Channel::Live,
            PromptMode::Auto if self.backend.agent_is_live(&target_root) => Channel::Live,
            PromptMode::Auto => Channel::Queue,
        };
        Ok((target_root, channel))
    }

    /// Append one already-resolved prompt to its selected backend queue.
    fn dispatch_prompt(
        &self,
        target_root: &Path,
        prompt: &str,
        channel: Channel,
        mode: PromptMode,
    ) -> Result<String, String> {
        let detail = match channel {
            Channel::Queue => {
                let delivery = match mode {
                    PromptMode::Auto => LaunchPromptDelivery::ReuseLiveAgent,
                    // `Live` cannot currently resolve to the launch channel, but
                    // fail closed to the stricter fresh-launch contract if that
                    // routing changes later.
                    PromptMode::Queue | PromptMode::Live => LaunchPromptDelivery::FreshLaunch,
                };
                self.backend.prompt(target_root, prompt, delivery)?
            }
            Channel::Live => self.backend.send(target_root, prompt)?,
        };
        Ok(detail)
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
    /// `model`: the override is recorded before the prompt is delivered. An
    /// explicit queued prompt is opened by the requested agent on the next fresh
    /// launch. Live delivery and a reusable `auto` fallback can only affect future
    /// launches; an already-running agent keeps whatever CLI/model started it.
    fn set_session_agent(&self, name: &str, agent: SessionAgent) -> Result<SessionAgent, String> {
        session::set_agent(&self.workspace_root, name, agent).map_err(|e| e.to_string())
    }

    /// Parse and validate a per-session agent override before session creation.
    /// Shared with the unified server's delegate tools so an unavailable model
    /// cannot leave behind a worktree whose first queued launch is doomed.
    pub(crate) fn resolve_agent_override(
        &self,
        agent_cli: Option<&str>,
        model: Option<String>,
    ) -> Result<SessionAgent, String> {
        self.prepare_session_agent(resolve_session_agent(&*self.runner, agent_cli, model)?)
    }

    /// Normalise an override and positively validate an explicit model.
    ///
    /// A model without a CLI is resolved against the workspace's effective CLI,
    /// then that CLI is stored in the returned value. Models are CLI-specific;
    /// binding the pair prevents a later workspace-default change from launching
    /// a model with a different, unchecked CLI. `None` intentionally needs no
    /// probe because it asks the CLI to choose its own current default.
    fn prepare_session_agent(&self, agent: SessionAgent) -> Result<SessionAgent, String> {
        let mut agent = normalize_session_agent(agent);
        let Some(model) = agent.model.as_deref() else {
            return Ok(agent);
        };
        let cli = match agent.cli {
            Some(cli) => cli,
            None => {
                let cli = crate::usecase::settings::effective_for(&self.workspace_root)
                    .map_err(|e| format!("failed to resolve the effective agent CLI: {e}"))?
                    .agent_cli;
                agent.cli = Some(cli);
                cli
            }
        };
        crate::usecase::agent::require_available_model(&*self.model_probe, cli, model)?;
        Ok(agent)
    }

    /// Recheck the stored model of a durable prompt target. Completion reports
    /// use the same queues as `session_prompt`, so a non-live parent can be
    /// auto-started by the report even though `session_complete` has no model
    /// arguments of its own.
    fn validate_stored_session_agent(&self, name: &str) -> Result<(), String> {
        if name == ROOT_TARGET {
            return Ok(());
        }
        let existing = self.find_session(name)?.agent;
        let prepared = self.prepare_session_agent(existing.clone())?;
        if prepared != existing {
            self.set_session_agent(name, prepared)?;
        }
        Ok(())
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

    /// The name of the session this process runs inside, or an error naming
    /// `tool` when it runs from the workspace root — the todo / decision tools,
    /// like the note tools, act on the current session only.
    fn require_session(&self, tool: &str) -> Result<&str, String> {
        self.current_session
            .as_deref()
            .ok_or_else(|| format!("{tool} is only available from inside a session worktree"))
    }

    fn tool_todo_list(&self) -> Result<String, String> {
        let name = self.require_session("session_todo_list")?;
        let todos = session::get_todos(&self.workspace_root, session::NoteTarget::Session(name))
            .map_err(|e| e.to_string())?;
        Ok(to_pretty(&json!({ "name": name, "todos": todos })))
    }

    fn tool_todo_add(&self, arguments: Value) -> Result<String, String> {
        let args: TodoAddArgs = parse_args(arguments)?;
        let name = self.require_session("session_todo_add")?;
        let todos = session::add_todo(
            &self.workspace_root,
            session::NoteTarget::Session(name),
            &args.text,
        )
        .map_err(|e| e.to_string())?;
        Ok(to_pretty(&json!({ "name": name, "todos": todos })))
    }

    fn tool_todo_update(&self, arguments: Value) -> Result<String, String> {
        let args: TodoUpdateArgs = parse_args(arguments)?;
        let name = self.require_session("session_todo_update")?;
        let target = session::NoteTarget::Session(name);
        if args.done.is_none() && args.text.is_none() {
            return Err("session_todo_update needs `done` and/or `text` to change".to_string());
        }
        // Apply the text edit first, then the checked-state toggle, so a call
        // carrying both lands as one logical update. Each mutation returns the
        // updated checklist, so we echo the last one applied.
        let mut todos = None;
        if let Some(text) = &args.text {
            todos = Some(
                session::edit_todo(&self.workspace_root, target, args.index, text)
                    .map_err(|e| e.to_string())?,
            );
        }
        if let Some(done) = args.done {
            todos = Some(
                session::set_todo_done(&self.workspace_root, target, args.index, done)
                    .map_err(|e| e.to_string())?,
            );
        }
        let todos = todos.expect("guard above ensures `done` or `text` is present");
        Ok(to_pretty(&json!({ "name": name, "todos": todos })))
    }

    fn tool_todo_remove(&self, arguments: Value) -> Result<String, String> {
        let args: TodoRemoveArgs = parse_args(arguments)?;
        let name = self.require_session("session_todo_remove")?;
        let todos = session::remove_todo(
            &self.workspace_root,
            session::NoteTarget::Session(name),
            args.index,
        )
        .map_err(|e| e.to_string())?;
        Ok(to_pretty(&json!({ "name": name, "todos": todos })))
    }

    fn tool_decision_list(&self) -> Result<String, String> {
        let name = self.require_session("session_decision_list")?;
        let decisions =
            session::get_decisions(&self.workspace_root, session::NoteTarget::Session(name))
                .map_err(|e| e.to_string())?;
        Ok(to_pretty(&json!({ "name": name, "decisions": decisions })))
    }

    fn tool_decision_log(&self, arguments: Value) -> Result<String, String> {
        let args: DecisionLogArgs = parse_args(arguments)?;
        let name = self.require_session("session_decision_log")?;
        let decisions = session::log_decision(
            &self.workspace_root,
            session::NoteTarget::Session(name),
            Utc::now(),
            &args.text,
        )
        .map_err(|e| e.to_string())?;
        Ok(to_pretty(&json!({ "name": name, "decisions": decisions })))
    }
}

impl McpService for SessionMcpServer {
    fn server_name(&self) -> &str {
        "usagi-session"
    }

    fn tool_names(&self) -> &'static [&'static str] {
        &TOOL_NAMES
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
            "session_complete" => self.tool_complete(arguments),
            "session_pr" => self.tool_pr(arguments),
            "session_remove" => self.tool_remove(arguments),
            "session_note_get" => self.tool_note_get(),
            "session_note_update" => self.tool_note_update(arguments),
            "session_todo_list" => self.tool_todo_list(),
            "session_todo_add" => self.tool_todo_add(arguments),
            "session_todo_update" => self.tool_todo_update(arguments),
            "session_todo_remove" => self.tool_todo_remove(arguments),
            "session_decision_list" => self.tool_decision_list(),
            "session_decision_log" => self.tool_decision_log(arguments),
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

// --- argument shapes -------------------------------------------------------

#[derive(Deserialize)]
struct TodoAddArgs {
    /// The todo text (trimmed; must be non-empty).
    text: String,
}

#[derive(Deserialize)]
struct TodoUpdateArgs {
    /// Zero-based position of the todo to change.
    index: usize,
    /// New checked state, when toggling.
    #[serde(default)]
    done: Option<bool>,
    /// New text, when editing.
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct TodoRemoveArgs {
    /// Zero-based position of the todo to remove.
    index: usize,
}

#[derive(Deserialize)]
struct DecisionLogArgs {
    /// What was decided and why (trimmed; must be non-empty).
    text: String,
}

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

#[derive(Deserialize)]
struct CompleteArgs {
    /// Concise completion report delivered to the session's recorded caller.
    message: String,
}

/// How `session_prompt` chooses between the launch queue and the live pane.
#[derive(Deserialize, Clone, Copy, Default, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PromptMode {
    /// Deliver live when a live pane is detected, otherwise queue for launch.
    #[default]
    Auto,
    /// Queue for the session's next fresh agent launch. Rejected when the target
    /// already has a live agent pane (no fresh launch is coming, so the prompt
    /// would strand); use `auto`/`live` to reach the running agent instead.
    Queue,
    /// Always append to the live queue for an already-running pane.
    Live,
}

/// The channel a prompt was actually delivered over, reported back to the caller
/// as `delivered_to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Whether an error chain's root cause is a permission-denied filesystem error.
/// Rust maps both `EACCES` and `EPERM` ("Operation not permitted") to
/// [`std::io::ErrorKind::PermissionDenied`], so this catches a sandbox denial on
/// the workspace `.usagi/` store lock just as well as an ordinary read-only
/// directory. Walks the whole `anyhow` chain because the io error is wrapped in
/// `StoreLock::acquire`'s "failed to open ..." context.
fn is_permission_denied(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<std::io::Error>(),
            Some(io) if io.kind() == std::io::ErrorKind::PermissionDenied
        )
    })
}

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
                    "removal": s.removal.map(|phase| phase.as_str()),
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
                        "description": "Explicit model the session's agent CLI runs. It must appear in that CLI's current dynamic model catalog; unavailable or unverifiable models are rejected. Omit this field to use the CLI's own default."
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
                \"running\" / \"waiting\" / \"ended\" / \"exited\", or \"none\" when no agent \
                pane has run in it) and, per worktree, the git status (\"new\" / \
                \"dirty\" / \"local\" / \"pushed\" / \"synced\") plus `dirty` and \
                `merged` booleans. `merged` is true when the default branch \
                already contains all of the worktree (status \"synced\"). \
                `removal` is null normally, \"git_teardown\" or \"context_cleanup\" \
                while a retryable removal is in progress, and \"orphaned\" for an \
                unrecorded tree quarantined from automatic deletion. Read-only and \
                cheap — it reads the cached state.json and the \
                agent-phase files with no git spawn, so the values are as fresh \
                as the latest workspace sync. A coordinator watches for \
                agent_phase \"ended\" (the child turn completed), \"exited\" (the process \
                disappeared; its outcome still requires inspection), and `merged` \
                (work landed), then removes successful sessions or retries/escalates \
                incomplete work.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "session_prompt",
            "description": "Deliver a prompt to a specific session's agent, so you \
                can delegate a task to a parallel session. Work stays isolated on \
                the session's worktree branch. It does not run the agent or return \
                its response here. Two delivery channels, chosen by `mode`: the \
                launch queue persists the prompt; explicit `queue` reserves it as \
                the opening message for the next fresh launch, while an `auto` \
                fallback may also reach an eligible existing agent after the TUI returns. \
                The live queue types it into an already-running agent pane (waiting \
                if none is open yet). `mode` defaults to `auto`, which delivers live \
                when the session has a live agent pane and queues otherwise — so you \
                need not know whether the agent is currently running. Optionally set `agent_cli` and/or \
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
                        "description": "auto (default): live if a pane is running, else queue for launch. queue: the next fresh launch (rejected if the target already has a running pane, since it would never be delivered — use auto/live). live: always the running pane (waits if none open)."
                    },
                    "agent_cli": {
                        "type": "string",
                        "enum": ["claude", "codex", "sakana.ai", "gemini", "antigravity"],
                        "description": "Agent CLI this session should launch with from now on (default: leave the existing CLI override unchanged)"
                    },
                    "model": {
                        "type": "string",
                        "description": "Explicit model this session's agent CLI should launch with from now on. The effective CLI/model pair is checked before state or queues change; unavailable or unverifiable models are rejected. Omit to keep the existing override; blank clears it and uses the CLI default."
                    }
                },
                "required": ["name", "prompt"]
            }
        },
        {
            "name": "session_complete",
            "description": "Report that the current session has completed. The destination is resolved automatically from the caller recorded when this session was created: a nested session reports to its parent session, and a top-level session reports to the workspace root coordinator. The report is delivered live when that coordinator has a running agent pane and queued otherwise. Only available from inside a session worktree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "Concise completion report, including the result and any useful PR or verification details"
                    }
                },
                "required": ["message"]
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
                    "name": { "type": "string", "description": "Target session name; must form a valid Git branch as usagi/<name> and be at most 250 UTF-8 bytes" }
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
        },
        {
            "name": "session_todo_list",
            "description": "List the current session's lightweight checklist — throwaway, \
                machine-local todos for this session's work, distinct from the git-tracked \
                issue store. Returns { name, todos } where each todo is { text, done }. \
                Only available from inside a session worktree.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "session_todo_add",
            "description": "Append a todo to the current session's checklist. The text is \
                trimmed and must be non-empty; the todo starts unchecked. Returns \
                { name, todos } with the checklist now stored. Only available from inside \
                a session worktree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The todo text" }
                },
                "required": ["text"]
            }
        },
        {
            "name": "session_todo_update",
            "description": "Change the todo at `index` (zero-based) in the current session's \
                checklist: pass `done` to check/uncheck it and/or `text` to rewrite it. At \
                least one of the two is required. Returns { name, todos }. Fails when the \
                index is out of range. Only available from inside a session worktree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "index": { "type": "integer", "minimum": 0, "description": "Zero-based todo position" },
                    "done": { "type": "boolean", "description": "New checked state" },
                    "text": { "type": "string", "description": "New todo text" }
                },
                "required": ["index"]
            }
        },
        {
            "name": "session_todo_remove",
            "description": "Remove the todo at `index` (zero-based) from the current session's \
                checklist. Returns { name, todos } with the checklist now stored. Fails when \
                the index is out of range. Only available from inside a session worktree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "index": { "type": "integer", "minimum": 0, "description": "Zero-based todo position" }
                },
                "required": ["index"]
            }
        },
        {
            "name": "session_decision_list",
            "description": "List the current session's decision log — an append-only record \
                of what the agent decided and why, so a coordinator can follow the reasoning \
                without replaying the transcript. Returns { name, decisions } where each entry \
                is { at, text } (at is an RFC3339 UTC timestamp). Only available from inside a \
                session worktree.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "session_decision_log",
            "description": "Append a decision to the current session's log, timestamped now. \
                Record why an approach was chosen while the reasoning is fresh. The text is \
                trimmed and must be non-empty. Returns { name, decisions } with the log now \
                stored. Only available from inside a session worktree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "What was decided and why" }
                },
                "required": ["text"]
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::PrLink;
    use crate::infrastructure::git;
    use crate::infrastructure::git::test_command as git_cmd;
    use crate::infrastructure::pr_link_store;
    use crate::presentation::mcp::PROTOCOL_VERSION;
    use crate::usecase::agent::ModelAvailability;
    use std::cell::RefCell;
    use std::fs;
    use std::rc::Rc;

    type CallLog = Rc<RefCell<Vec<(PathBuf, String)>>>;
    type PromptDeliveryLog = Rc<RefCell<Vec<LaunchPromptDelivery>>>;
    type RemoveLog = Rc<RefCell<Vec<(PathBuf, String, bool)>>>;
    type ModelProbeLog = Rc<RefCell<Vec<(AgentCli, String)>>>;

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

    /// Model probe used by the existing session tests: unless a test is about
    /// model validation itself, every explicit model is considered available.
    struct AvailableModelProbe;

    impl AgentModelProbe for AvailableModelProbe {
        fn probe_model(&self, _cli: AgentCli, _model: &str) -> ModelAvailability {
            ModelAvailability::Available
        }
    }

    struct RecordingModelProbe {
        result: ModelAvailability,
        calls: ModelProbeLog,
    }

    impl RecordingModelProbe {
        fn new(result: ModelAvailability) -> (Self, ModelProbeLog) {
            let calls = Rc::new(RefCell::new(Vec::new()));
            (
                Self {
                    result,
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }

    impl AgentModelProbe for RecordingModelProbe {
        fn probe_model(&self, cli: AgentCli, model: &str) -> ModelAvailability {
            self.calls.borrow_mut().push((cli, model.to_string()));
            self.result.clone()
        }
    }

    /// A backend that records the calls it received and returns a scripted
    /// result, so the server's dispatch can be tested without a real agent. The
    /// call logs are shared via `Rc` so a test can inspect them after the backend
    /// is moved into the server.
    struct FakeBackend {
        result: Result<String, String>,
        calls: CallLog,
        prompt_deliveries: PromptDeliveryLog,
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
                prompt_deliveries: Rc::new(RefCell::new(Vec::new())),
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
                prompt_deliveries: Rc::new(RefCell::new(Vec::new())),
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
        fn prompt(
            &self,
            worktree: &Path,
            prompt: &str,
            delivery: LaunchPromptDelivery,
        ) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((worktree.to_path_buf(), prompt.to_string()));
            self.prompt_deliveries.borrow_mut().push(delivery);
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
        server_at_with_model_probe(root, backend, Box::new(AvailableModelProbe))
    }

    fn server_at_with_model_probe(
        root: &Path,
        backend: FakeBackend,
        model_probe: Box<dyn AgentModelProbe>,
    ) -> SessionMcpServer {
        let runner = Box::new(FakeRunner(vec!["claude", "codex", "codex-fugu"]));
        SessionMcpServer::new(
            root.to_path_buf(),
            root,
            Box::new(backend),
            runner,
            model_probe,
        )
    }

    fn server_in_session(root: &Path, name: &str, backend: FakeBackend) -> SessionMcpServer {
        server_in_session_with_model_probe(root, name, backend, Box::new(AvailableModelProbe))
    }

    fn server_in_session_with_model_probe(
        root: &Path,
        name: &str,
        backend: FakeBackend,
        model_probe: Box<dyn AgentModelProbe>,
    ) -> SessionMcpServer {
        let worktree = root.join(".usagi").join("sessions").join(name);
        let runner = Box::new(FakeRunner(vec!["claude", "codex", "codex-fugu"]));
        SessionMcpServer::new(
            root.to_path_buf(),
            &worktree,
            Box::new(backend),
            runner,
            model_probe,
        )
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
                "session_complete",
                "session_pr",
                "session_remove",
                "session_note_get",
                "session_note_update",
                "session_todo_list",
                "session_todo_add",
                "session_todo_update",
                "session_todo_remove",
                "session_decision_list",
                "session_decision_log",
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
    fn complete_reports_to_the_recorded_parent_session() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let root_server = server_at(root.path(), FakeBackend::ok("x"));
        call(&root_server, "session_create", json!({"name":"parent"}));

        // Creating the child from the parent's worktree captures only the parent
        // session name as its return address.
        let parent_server = server_in_session(root.path(), "parent", FakeBackend::ok("x"));
        call(&parent_server, "session_create", json!({"name":"child"}));

        let backend = FakeBackend::ok("sent").with_live(true);
        let calls = backend.calls.clone();
        let child_server = server_in_session(root.path(), "child", backend);
        let result = call(
            &child_server,
            "session_complete",
            json!({"message":"PR #42 is ready; tests pass."}),
        );

        assert_eq!(result["isError"], false);
        let body = tool_json(&result);
        assert_eq!(body["session"], "child");
        assert_eq!(body["reported_to"], "parent");
        assert_eq!(body["delivered_to"], "live");
        assert_eq!(
            calls.borrow()[0].0,
            root.path().join(".usagi/sessions/parent")
        );
        assert_eq!(
            calls.borrow()[0].1,
            "Session \"child\" completed:\n\nPR #42 is ready; tests pass."
        );
    }

    #[test]
    fn complete_rechecks_the_parent_model_before_queueing_the_report() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let root_server = server_at(root.path(), FakeBackend::ok("x"));
        let parent = call(
            &root_server,
            "session_create",
            json!({"name":"parent","agent_cli":"codex","model":"gpt-old"}),
        );
        assert_eq!(parent["isError"], false);

        let parent_server = server_in_session(root.path(), "parent", FakeBackend::ok("x"));
        let child = call(&parent_server, "session_create", json!({"name":"child"}));
        assert_eq!(child["isError"], false);

        let backend = FakeBackend::ok("queued");
        let backend_calls = backend.calls.clone();
        let (probe, probe_calls) = RecordingModelProbe::new(ModelAvailability::Unavailable {
            available_models: vec!["gpt-new".to_string()],
        });
        let child_server =
            server_in_session_with_model_probe(root.path(), "child", backend, Box::new(probe));
        let result = call(
            &child_server,
            "session_complete",
            json!({"message":"Implementation complete."}),
        );

        assert_eq!(result["isError"], true);
        assert!(backend_calls.borrow().is_empty());
        assert_eq!(
            *probe_calls.borrow(),
            vec![(AgentCli::Codex, "gpt-old".to_string())]
        );
        assert_eq!(
            session::list(root.path())
                .unwrap()
                .into_iter()
                .find(|session| session.name == "parent")
                .unwrap()
                .agent
                .model,
            Some("gpt-old".to_string())
        );
    }

    #[test]
    fn complete_binds_a_legacy_parent_model_to_the_checked_cli() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        session::create_with_agent(
            root.path(),
            "parent",
            SessionAgent {
                cli: None,
                model: Some("legacy-model".to_string()),
            },
            SessionOrigin::Mcp,
            None,
        )
        .unwrap();
        let parent_server = server_in_session(root.path(), "parent", FakeBackend::ok("x"));
        call(&parent_server, "session_create", json!({"name":"child"}));

        let backend = FakeBackend::ok("queued");
        let backend_calls = backend.calls.clone();
        let (probe, probe_calls) = RecordingModelProbe::new(ModelAvailability::Available);
        let child_server =
            server_in_session_with_model_probe(root.path(), "child", backend, Box::new(probe));
        let result = call(
            &child_server,
            "session_complete",
            json!({"message":"Implementation complete."}),
        );

        assert_eq!(result["isError"], false);
        assert_eq!(backend_calls.borrow().len(), 1);
        assert_eq!(
            *probe_calls.borrow(),
            vec![(AgentCli::Claude, "legacy-model".to_string())]
        );
        let parent = session::list(root.path())
            .unwrap()
            .into_iter()
            .find(|session| session.name == "parent")
            .unwrap();
        assert_eq!(parent.agent.cli, Some(AgentCli::Claude));
        assert_eq!(parent.agent.model.as_deref(), Some("legacy-model"));
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn complete_from_a_top_level_session_reports_to_root_and_is_session_only() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let root_server = server_at(root.path(), FakeBackend::ok("x"));
        call(&root_server, "session_create", json!({"name":"worker"}));

        let backend = FakeBackend::ok("queued");
        let calls = backend.calls.clone();
        let worker_server = server_in_session(root.path(), "worker", backend);
        let result = call(
            &worker_server,
            "session_complete",
            json!({"message":"Implementation complete."}),
        );
        let body = tool_json(&result);
        assert_eq!(body["reported_to"], ROOT_TARGET);
        assert_eq!(body["delivered_to"], "queue");
        assert_eq!(calls.borrow()[0].0, root.path());

        let outside = call(
            &root_server,
            "session_complete",
            json!({"message":"not in a session"}),
        );
        assert_eq!(outside["isError"], true);
        assert!(outside["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("only available from inside a session worktree"));
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
    fn create_rejects_git_ref_invalid_names_without_workspace_effects() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, data.path());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let branches_before = git::local_branches(root.path());
        let server = server_at(root.path(), FakeBackend::ok("x"));

        let result = call(&server, "session_create", json!({"name":"bad@{name"}));

        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("valid Git branch"));
        assert_eq!(git::local_branches(root.path()), branches_before);
        assert!(!root.path().join(".usagi").exists());
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn create_permission_denied_inside_session_explains_sandboxed_delegation() {
        use anyhow::Context as _;

        let root = tempfile::tempdir().unwrap();
        let server = server_in_session(root.path(), "parent", FakeBackend::ok("x"));
        let err = Err::<(), _>(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Operation not permitted",
        ))
        .with_context(|| {
            format!(
                "failed to open {}",
                root.path().join(".usagi/.lock").display()
            )
        })
        .unwrap_err();

        assert!(is_permission_denied(&err));
        let message = server.creation_error_message("child", &err);

        assert!(message.contains("cannot create session \"child\" from inside session \"parent\""));
        assert!(message.contains("sandboxed session can only write its own worktree"));
        assert!(message.contains("`session_prompt`"));
        assert!(message.contains("`:root`"));
    }

    #[test]
    fn create_permission_denied_at_workspace_root_reports_unwritable_store() {
        use anyhow::Context as _;

        let root = tempfile::tempdir().unwrap();
        // Server at the workspace root: no `current_session`, so the message is
        // the root variant rather than the sub-session delegation hint.
        let server = server_at(root.path(), FakeBackend::ok("x"));
        let err = Err::<(), _>(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Operation not permitted",
        ))
        .with_context(|| {
            format!(
                "failed to open {}",
                root.path().join(".usagi/.lock").display()
            )
        })
        .unwrap_err();

        let message = server.creation_error_message("child", &err);

        assert!(message.contains("cannot create session \"child\""));
        assert!(!message.contains("from inside session"));
        assert!(message.contains("workspace `.usagi/` directory is writable"));
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
        let prompt_deliveries = backend.prompt_deliveries.clone();
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
        assert_eq!(
            *prompt_deliveries.borrow(),
            vec![LaunchPromptDelivery::FreshLaunch]
        );

        let session = session::list(root.path())
            .unwrap()
            .into_iter()
            .find(|s| s.name == "work")
            .unwrap();
        assert_eq!(session.agent.cli, Some(AgentCli::SakanaAi));
        assert_eq!(session.agent.model.as_deref(), Some("fugu-ultra"));
    }

    #[test]
    fn unavailable_prompt_model_errors_before_state_or_queue_changes() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let backend = FakeBackend::ok("done");
        let backend_calls = backend.calls.clone();
        let (probe, probe_calls) = RecordingModelProbe::new(ModelAvailability::Unavailable {
            available_models: vec!["gpt-available".to_string()],
        });
        let server = server_at_with_model_probe(root.path(), backend, Box::new(probe));
        call(
            &server,
            "session_create",
            json!({"name":"work","agent_cli":"codex"}),
        );

        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"do it","model":"gpt-missing"}),
        );

        assert_eq!(result["isError"], true);
        let error = result["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("gpt-missing"), "{error}");
        assert!(error.contains("gpt-available"), "{error}");
        assert!(backend_calls.borrow().is_empty());
        assert_eq!(
            *probe_calls.borrow(),
            vec![(AgentCli::Codex, "gpt-missing".to_string())]
        );
        let stored = session::list(root.path()).unwrap().remove(0).agent;
        assert_eq!(stored.cli, Some(AgentCli::Codex));
        assert_eq!(stored.model, None);
    }

    #[test]
    fn unverifiable_prompt_model_fails_closed_before_queueing() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let backend = FakeBackend::ok("done");
        let backend_calls = backend.calls.clone();
        let (probe, _) = RecordingModelProbe::new(ModelAvailability::Unverifiable {
            reason: "the CLI has no model catalog command".to_string(),
        });
        let server = server_at_with_model_probe(root.path(), backend, Box::new(probe));
        call(
            &server,
            "session_create",
            json!({"name":"work","agent_cli":"claude"}),
        );

        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"do it","model":"opus"}),
        );

        assert_eq!(result["isError"], true);
        let error = result["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("could not verify"), "{error}");
        assert!(
            error.contains("clear the explicit model override"),
            "{error}"
        );
        assert!(backend_calls.borrow().is_empty());
    }

    #[test]
    fn blank_prompt_model_clears_the_override_without_probing() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let setup = server_at(root.path(), FakeBackend::ok("setup"));
        call(
            &setup,
            "session_create",
            json!({"name":"work","agent_cli":"codex","model":"gpt-old"}),
        );
        drop(setup);

        let backend = FakeBackend::ok("done");
        let backend_calls = backend.calls.clone();
        let (probe, probe_calls) = RecordingModelProbe::new(ModelAvailability::Unavailable {
            available_models: Vec::new(),
        });
        let server = server_at_with_model_probe(root.path(), backend, Box::new(probe));
        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"use the default","model":"   "}),
        );

        assert_eq!(result["isError"], false);
        assert!(probe_calls.borrow().is_empty());
        assert_eq!(backend_calls.borrow().len(), 1);
        let stored = session::list(root.path()).unwrap().remove(0).agent;
        assert_eq!(stored.cli, Some(AgentCli::Codex));
        assert_eq!(stored.model, None);
    }

    #[test]
    fn queued_prompt_rechecks_an_already_stored_model() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let setup = server_at(root.path(), FakeBackend::ok("setup"));
        call(
            &setup,
            "session_create",
            json!({"name":"work","agent_cli":"codex","model":"gpt-old"}),
        );
        drop(setup);

        let backend = FakeBackend::ok("done");
        let backend_calls = backend.calls.clone();
        let (probe, probe_calls) = RecordingModelProbe::new(ModelAvailability::Unavailable {
            available_models: vec!["gpt-new".to_string()],
        });
        let server = server_at_with_model_probe(root.path(), backend, Box::new(probe));
        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"start now"}),
        );

        assert_eq!(result["isError"], true);
        assert!(backend_calls.borrow().is_empty());
        assert_eq!(
            *probe_calls.borrow(),
            vec![(AgentCli::Codex, "gpt-old".to_string())]
        );
        // Revalidation does not erase the user's stored choice; the caller can
        // explicitly clear or replace it in a later request.
        assert_eq!(
            session::list(root.path()).unwrap().remove(0).agent.model,
            Some("gpt-old".to_string())
        );
    }

    #[test]
    fn live_prompt_to_a_running_pane_rechecks_its_stored_model_before_appending() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let setup = server_at(root.path(), FakeBackend::ok("setup"));
        call(
            &setup,
            "session_create",
            json!({"name":"work","agent_cli":"codex","model":"gpt-old"}),
        );
        drop(setup);

        let backend = FakeBackend::ok("sent").with_live(true);
        let backend_calls = backend.calls.clone();
        let (probe, probe_calls) = RecordingModelProbe::new(ModelAvailability::Unavailable {
            available_models: vec!["gpt-new".to_string()],
        });
        let server = server_at_with_model_probe(root.path(), backend, Box::new(probe));
        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"follow up now"}),
        );

        assert_eq!(result["isError"], true);
        assert!(backend_calls.borrow().is_empty());
        assert_eq!(
            *probe_calls.borrow(),
            vec![(AgentCli::Codex, "gpt-old".to_string())]
        );
        assert_eq!(
            session::list(root.path()).unwrap().remove(0).agent.model,
            Some("gpt-old".to_string())
        );
    }

    #[test]
    fn explicit_live_prompt_without_a_running_pane_rechecks_the_stored_model() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let setup = server_at(root.path(), FakeBackend::ok("setup"));
        call(
            &setup,
            "session_create",
            json!({"name":"work","agent_cli":"codex","model":"gpt-old"}),
        );
        drop(setup);

        let backend = FakeBackend::ok("sent").with_live(false);
        let backend_calls = backend.calls.clone();
        let (probe, probe_calls) = RecordingModelProbe::new(ModelAvailability::Unavailable {
            available_models: vec!["gpt-new".to_string()],
        });
        let server = server_at_with_model_probe(root.path(), backend, Box::new(probe));
        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"start when possible","mode":"live"}),
        );

        assert_eq!(result["isError"], true);
        assert!(backend_calls.borrow().is_empty());
        assert_eq!(
            *probe_calls.borrow(),
            vec![(AgentCli::Codex, "gpt-old".to_string())]
        );
    }

    #[test]
    fn unavailable_create_model_leaves_no_session_or_worktree() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let (probe, _) = RecordingModelProbe::new(ModelAvailability::Unavailable {
            available_models: vec!["gpt-available".to_string()],
        });
        let server =
            server_at_with_model_probe(root.path(), FakeBackend::ok("done"), Box::new(probe));

        let result = call(
            &server,
            "session_create",
            json!({"name":"work","agent_cli":"codex","model":"gpt-missing"}),
        );

        assert_eq!(result["isError"], true);
        assert!(session::list(root.path()).unwrap().is_empty());
        assert!(!root.path().join(".usagi/sessions/work").exists());
    }

    #[test]
    fn model_without_cli_is_checked_and_bound_to_the_effective_workspace_cli() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let (probe, probe_calls) = RecordingModelProbe::new(ModelAvailability::Available);
        let server =
            server_at_with_model_probe(root.path(), FakeBackend::ok("done"), Box::new(probe));

        let result = call(
            &server,
            "session_create",
            json!({"name":"work","model":"default-cli-model"}),
        );

        assert_eq!(result["isError"], false);
        assert_eq!(
            *probe_calls.borrow(),
            vec![(AgentCli::Claude, "default-cli-model".to_string())]
        );
        let stored = session::list(root.path()).unwrap().remove(0).agent;
        assert_eq!(stored.cli, Some(AgentCli::Claude));
        assert_eq!(stored.model.as_deref(), Some("default-cli-model"));
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn prompt_binds_a_legacy_model_only_override_before_queueing() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        session::create_with_agent(
            root.path(),
            "work",
            SessionAgent {
                cli: None,
                model: Some("legacy-model".to_string()),
            },
            SessionOrigin::Mcp,
            None,
        )
        .unwrap();
        let backend = FakeBackend::ok("queued");
        let backend_calls = backend.calls.clone();
        let (probe, probe_calls) = RecordingModelProbe::new(ModelAvailability::Available);
        let server = server_at_with_model_probe(root.path(), backend, Box::new(probe));

        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"start now"}),
        );

        assert_eq!(result["isError"], false);
        assert_eq!(backend_calls.borrow().len(), 1);
        assert_eq!(
            *probe_calls.borrow(),
            vec![(AgentCli::Claude, "legacy-model".to_string())]
        );
        let stored = session::list(root.path()).unwrap().remove(0).agent;
        assert_eq!(stored.cli, Some(AgentCli::Claude));
        assert_eq!(stored.model.as_deref(), Some("legacy-model"));
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn model_validation_surfaces_effective_settings_errors_before_creation() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("settings.json"), "not json").unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("done"));

        let result = call(
            &server,
            "session_create",
            json!({"name":"work","model":"some-model"}),
        );

        assert_eq!(result["isError"], true);
        let error = result["content"][0]["text"].as_str().unwrap();
        assert!(error.contains("failed to resolve the effective agent CLI"));
        assert!(session::list(root.path()).unwrap().is_empty());
        assert!(!root.path().join(".usagi/sessions/work").exists());
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
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
        assert_eq!(session.agent.cli, Some(AgentCli::SakanaAi));
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
    fn prompt_agent_override_for_an_unknown_session_errors_before_queueing() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let backend = FakeBackend::ok("done");
        let calls = backend.calls.clone();
        let server = server_at(root.path(), backend);

        let result = call(
            &server,
            "session_prompt",
            json!({"name":"ghost","prompt":"hi","model":"fugu-ultra"}),
        );
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no such session"));
        assert!(calls.borrow().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn prompt_agent_override_surfaces_store_write_errors_before_queueing() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let backend = FakeBackend::ok("done");
        let calls = backend.calls.clone();
        let server = server_at(root.path(), backend);
        call(&server, "session_create", json!({"name":"work"}));

        let state_dir = root.path().join(".usagi");
        let original_mode = fs::metadata(&state_dir).unwrap().permissions().mode();
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o500)).unwrap();
        let result = call(
            &server,
            "session_prompt",
            json!({"name":"work","prompt":"hi","model":"fugu-ultra"}),
        );
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(original_mode)).unwrap();

        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("failed to create"));
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn set_session_agent_surfaces_usecase_errors() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_at(root.path(), FakeBackend::ok("done"));

        let err = server
            .set_session_agent("ghost", SessionAgent::default())
            .unwrap_err();
        assert!(err.contains("no sessions recorded"), "{err}");
    }

    #[test]
    fn deliver_prompt_rejects_oversized_prompts_before_resolving_target() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let backend = FakeBackend::ok("done");
        let calls = backend.calls.clone();
        let server = server_at(root.path(), backend);

        let err = server
            .deliver_prompt(
                "ghost",
                &"x".repeat(MAX_PROMPT_BYTES + 1),
                PromptMode::Queue,
            )
            .unwrap_err();
        assert!(err.contains("session_prompt prompt is too large"), "{err}");
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

        // `queue` forces the launch channel when no live pane is detected…
        let queued_backend = FakeBackend::ok("q").with_live(false);
        let queued_deliveries = queued_backend.prompt_deliveries.clone();
        let queued = call(
            &server_at(root.path(), queued_backend),
            "session_prompt",
            json!({"name":"w","prompt":"hi","mode":"queue"}),
        );
        assert_eq!(tool_json(&queued)["delivered_to"], "queue");
        assert_eq!(
            *queued_deliveries.borrow(),
            vec![LaunchPromptDelivery::FreshLaunch]
        );

        // …and `live` forces the live channel even though none is detected.
        let live = call(
            &server_at(root.path(), FakeBackend::ok("l").with_live(false)),
            "session_prompt",
            json!({"name":"w","prompt":"hi","mode":"live"}),
        );
        assert_eq!(tool_json(&live)["delivered_to"], "live");
    }

    #[test]
    fn explicit_queue_to_a_live_pane_is_rejected_not_stranded() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        call(
            &server_at(root.path(), FakeBackend::ok("x")),
            "session_create",
            json!({"name":"w"}),
        );

        // A live pane means no fresh launch is coming, so `queue` (fresh-launch
        // only) would never be delivered. Reject with an actionable error instead
        // of silently accepting a prompt that strands on disk.
        let backend = FakeBackend::ok("q").with_live(true);
        let deliveries = backend.prompt_deliveries.clone();
        let result = call(
            &server_at(root.path(), backend),
            "session_prompt",
            json!({"name":"w","prompt":"hi","mode":"queue"}),
        );

        assert_eq!(result["isError"], true);
        let message = result["content"][0]["text"].as_str().unwrap();
        assert!(
            message.contains("already has a running agent pane"),
            "{message}"
        );
        assert!(message.contains("mode=\"auto\""), "{message}");
        // Nothing was appended to any queue.
        assert!(deliveries.borrow().is_empty());
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
        // durable fallback — again without requiring any session. Unlike an
        // explicit `queue`, this fallback may be handed to an eligible agent that
        // survived a TUI restart.
        let backend = FakeBackend::ok("queued").with_live(false);
        let calls = backend.calls.clone();
        let prompt_deliveries = backend.prompt_deliveries.clone();
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
        assert_eq!(
            *prompt_deliveries.borrow(),
            vec![LaunchPromptDelivery::ReuseLiveAgent]
        );
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
                PrLink::new(12, "https://github.com/o/r/pull/12"),
                PrLink::new(34, "https://github.com/o/r/pull/34"),
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
            &[PrLink::new(7, "https://github.com/o/r/pull/7")],
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
        // session_note_update requires a note before it checks the current session.
        let result = call(&server, "session_note_update", json!({}));
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
    fn todo_add_list_update_and_remove_round_trip() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_in_session(root.path(), "work", FakeBackend::ok("x"));
        call(&server, "session_create", json!({"name":"work"}));

        // Empty to start.
        let list = call(&server, "session_todo_list", json!({}));
        assert_eq!(list["isError"], false);
        assert_eq!(tool_json(&list)["todos"], json!([]));

        // Add two todos.
        let add = call(
            &server,
            "session_todo_add",
            json!({"text":"  write tests  "}),
        );
        assert_eq!(add["isError"], false);
        assert_eq!(tool_json(&add)["todos"], json!([{"text":"write tests"}]));
        call(&server, "session_todo_add", json!({"text":"ship it"}));

        // Update: check index 0 and rewrite its text in one call.
        let upd = call(
            &server,
            "session_todo_update",
            json!({"index":0,"done":true,"text":"write more tests"}),
        );
        assert_eq!(upd["isError"], false);
        let body = tool_json(&upd);
        assert_eq!(
            body["todos"][0],
            json!({"text":"write more tests","done":true})
        );
        assert_eq!(body["todos"][1], json!({"text":"ship it"}));

        // A text-only update keeps the checked state (no `done` field passed).
        let text_only = call(
            &server,
            "session_todo_update",
            json!({"index":0,"text":"write even more tests"}),
        );
        assert_eq!(
            tool_json(&text_only)["todos"][0],
            json!({"text":"write even more tests","done":true})
        );
        // A done-only update leaves the text alone.
        let done_only = call(
            &server,
            "session_todo_update",
            json!({"index":0,"done":false}),
        );
        assert_eq!(
            tool_json(&done_only)["todos"][0],
            json!({"text":"write even more tests"})
        );

        // Remove index 0.
        let rem = call(&server, "session_todo_remove", json!({"index":0}));
        assert_eq!(rem["isError"], false);
        assert_eq!(tool_json(&rem)["todos"], json!([{"text":"ship it"}]));
    }

    #[test]
    fn todo_update_reports_bad_input_and_out_of_range() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_in_session(root.path(), "work", FakeBackend::ok("x"));
        call(&server, "session_create", json!({"name":"work"}));

        // Neither `done` nor `text` → rejected before touching the store.
        let empty = call(&server, "session_todo_update", json!({"index":0}));
        assert_eq!(empty["isError"], true);
        assert!(empty["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("`done` and/or `text`"));

        // Out-of-range index on an empty checklist.
        let oor = call(
            &server,
            "session_todo_update",
            json!({"index":5,"done":true}),
        );
        assert_eq!(oor["isError"], true);
        assert!(oor["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("out of range"));
    }

    #[test]
    fn decision_log_and_list_round_trip_with_timestamps() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_in_session(root.path(), "work", FakeBackend::ok("x"));
        call(&server, "session_create", json!({"name":"work"}));

        let empty = call(&server, "session_decision_list", json!({}));
        assert_eq!(tool_json(&empty)["decisions"], json!([]));

        let log = call(
            &server,
            "session_decision_log",
            json!({"text":"  chose approach A over B  "}),
        );
        assert_eq!(log["isError"], false);
        let body = tool_json(&log);
        assert_eq!(body["decisions"][0]["text"], "chose approach A over B");
        // The server stamps `at`; it parses back as an RFC3339 timestamp.
        let at = body["decisions"][0]["at"].as_str().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(at).is_ok());

        let list = call(&server, "session_decision_list", json!({}));
        assert_eq!(tool_json(&list)["decisions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn todo_and_decision_tools_surface_usecase_errors() {
        // Inside a session worktree whose name is *not* recorded in state.json:
        // `require_session` resolves (the name is derived from the path), but every
        // usecase call then fails with "no such session", exercising each handler's
        // error path (its `map_err` conversion to a tool error).
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        let server = server_in_session(root.path(), "ghost", FakeBackend::ok("x"));
        // Record a *different* session so state.json exists but "ghost" is absent:
        // every scratchpad op then fails with "no such session".
        call(&server, "session_create", json!({"name":"other"}));
        for (tool, args) in [
            ("session_todo_list", json!({})),
            ("session_todo_add", json!({"text":"x"})),
            ("session_todo_update", json!({"index":0,"text":"x"})),
            ("session_todo_update", json!({"index":0,"done":true})),
            ("session_todo_remove", json!({"index":0})),
            ("session_decision_list", json!({})),
            ("session_decision_log", json!({"text":"x"})),
        ] {
            let r = call(&server, tool, args);
            assert_eq!(
                r["isError"], true,
                "{tool} should surface the usecase error"
            );
            assert!(r["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("no such session"));
        }
    }

    #[test]
    fn todo_and_decision_tools_fail_when_not_inside_a_session() {
        let root = tempfile::tempdir().unwrap();
        let server = server_at(root.path(), FakeBackend::ok("x"));
        for (tool, args) in [
            ("session_todo_list", json!({})),
            ("session_todo_add", json!({"text":"x"})),
            ("session_todo_update", json!({"index":0,"done":true})),
            ("session_todo_remove", json!({"index":0})),
            ("session_decision_list", json!({})),
            ("session_decision_log", json!({"text":"x"})),
        ] {
            let result = call(&server, tool, args);
            assert_eq!(result["isError"], true, "{tool} should error at root");
            assert!(result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("inside a session worktree"));
        }
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
