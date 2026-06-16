//! The agent port: the single interface usagi drives every agent CLI through.
//!
//! usagi needs a handful of things from whatever agent it launches — how to
//! start it (with usagi's MCP servers and lifecycle hooks wired in) and how much
//! of its context window is in use. Rather than scatter per-CLI `match`es across
//! the codebase, those needs are gathered into the [`Agent`] trait (the port).
//! One adapter per CLI implements it in `infrastructure::agent`, and
//! `infrastructure::agent::agent_for` is the single place that maps an
//! [`AgentCli`](crate::domain::settings::AgentCli) setting to its adapter —
//! adding a new agent is one new adapter plus one arm there.
//!
//! This module is pure domain: the trait and the [`AgentWiring`] policy it is
//! handed. How that policy is rendered into a CLI's invocation lives in the
//! adapters (the infrastructure layer).

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

    /// The full command line `:agent` sends to the shell, wiring usagi's MCP
    /// servers, system prompt, and (where supported) lifecycle hooks in per the
    /// given [`AgentWiring`].
    fn launch_command(&self, wiring: &AgentWiring) -> String;

    /// The agent's current context-window usage in `worktree`, or `None` when it
    /// exposes no readable usage (no session, or a CLI usagi cannot read yet).
    fn usage(&self, worktree: &Path) -> Option<AgentUsage>;
}

/// usagi's wiring policy handed to an [`Agent`] adapter when it builds the launch
/// command. It carries what the wiring depends on; the adapter decides *how* to
/// render it for its CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentWiring {
    /// The resolved usagi binary path the launched agent invokes back through
    /// (MCP servers and lifecycle hooks) — `std::env::current_exe()`, so the
    /// wiring resolves even when usagi is run from a build and not on `$PATH`.
    pub usagi_bin: String,
    /// The local-LLM model to expose for offloading light work, or `None` when
    /// the local LLM is disabled.
    pub local_llm_model: Option<String>,
}
