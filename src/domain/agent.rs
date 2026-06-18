//! The agent port: the single interface usagi drives every agent CLI through.
//!
//! usagi needs a handful of things from whatever agent it launches — chiefly how
//! to start it (with usagi's MCP servers and lifecycle hooks wired in). Rather
//! than scatter per-CLI `match`es across the codebase, those needs are gathered
//! into the [`Agent`] trait (the port).
//! One adapter per CLI implements it in `infrastructure::agent`, and
//! `infrastructure::agent::agent_for` is the single place that maps an
//! [`AgentCli`](crate::domain::settings::AgentCli) setting to its adapter —
//! adding a new agent is one new adapter plus one arm there.
//!
//! This module is pure domain: the trait and the [`AgentWiring`] policy it is
//! handed. How that policy is rendered into a CLI's invocation lives in the
//! adapters (the infrastructure layer).

/// What usagi requires of the agent CLI it drives, as one interface.
///
/// Implemented once per CLI in `infrastructure::agent`. `Send + Sync` so the
/// shared adapter (`Arc<dyn Agent>`) is free to cross threads.
pub trait Agent: Send + Sync {
    /// The program name run inside the embedded shell (e.g. `claude`).
    fn program(&self) -> &'static str;

    /// The full command line `:agent` sends to the shell, wiring usagi's MCP
    /// servers, system prompt, and (where supported) lifecycle hooks in per the
    /// given [`AgentWiring`].
    fn launch_command(&self, wiring: &AgentWiring) -> String;
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
