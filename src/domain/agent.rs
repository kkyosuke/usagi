//! The agent port: the single interface usagi drives every agent CLI through.
//!
//! usagi needs a handful of things from whatever agent it launches — how to
//! start it (with usagi's MCP servers and system prompt wired in), the program
//! it runs, and how much of its context window is in use. Rather than scatter
//! per-CLI `match`es across the codebase, those needs are gathered into the
//! [`Agent`] trait (the port). One adapter per CLI implements it in
//! `infrastructure::agent`, and `infrastructure::agent::agent_for` is the single
//! place that maps an [`AgentCli`] setting to its adapter — adding a new agent is
//! one new adapter plus one arm there.
//!
//! This module is pure domain: the trait and the wiring *policy* types (what to
//! wire). How that policy is rendered into a specific CLI's invocation (flags, a
//! `settings.json`, …) is the adapter's job and lives in the infrastructure
//! layer.

use std::path::Path;

use crate::domain::agent_usage::AgentUsage;

/// What usagi requires of the agent CLI it drives, as one interface.
///
/// Implemented once per CLI in `infrastructure::agent`. `Send + Sync` so a single
/// adapter can be shared (via `Arc`) between the render loop and the background
/// session watcher.
pub trait Agent: Send + Sync {
    /// The program name run inside the embedded shell (e.g. `claude`).
    fn program(&self) -> &'static str;

    /// The full command line `:agent` sends to the shell, rendering usagi's
    /// [`AgentWiring`] policy into this CLI's own invocation (inline flags, or
    /// nothing when the CLI takes its configuration from elsewhere).
    fn launch_command(&self, wiring: &AgentWiring) -> String;

    /// The agent's current context-window usage in `worktree`, or `None` when it
    /// exposes no readable usage (no session, or a CLI usagi cannot read yet).
    fn usage(&self, worktree: &Path) -> Option<AgentUsage>;
}

/// usagi's wiring policy handed to an [`Agent`] adapter when it builds the launch
/// command: which MCP servers to expose and the session-scoped system prompt to
/// append. The adapter decides *how* to pass them to its CLI; this only says
/// *what* to wire, so the policy stays in the domain and out of the adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentWiring {
    /// The MCP servers usagi exposes to the agent (always the issue server; the
    /// local-LLM server too when it is enabled).
    pub mcp_servers: Vec<McpServer>,
    /// The session-scoped system prompt usagi appends (the worktree note, plus
    /// the delegation note when the local LLM is on).
    pub system_prompt: String,
}

/// One MCP server usagi wires into an agent: the stdio command (and its args)
/// that launches it, under a short `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// The key the server is registered under (e.g. `usagi`).
    pub name: String,
    /// The program that serves it over stdio (e.g. `usagi`).
    pub command: String,
    /// The arguments passed to `command` (e.g. `["mcp"]`).
    pub args: Vec<String>,
}
