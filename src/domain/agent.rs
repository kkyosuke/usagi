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

use std::path::Path;

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
    ///
    /// When `resume` is set the CLI is asked to continue its previous
    /// conversation in the worktree (Claude's `--continue`), so reopening a
    /// session picks up where it left off; CLIs without a resume notion ignore
    /// it. The caller decides whether to resume from
    /// [`has_resumable_session`](Self::has_resumable_session).
    ///
    /// `initial_prompt` is an opening message to hand the agent on launch (e.g. a
    /// prompt queued for the session via MCP `session_prompt`, delivered through
    /// [`agent_prompt_store`](crate::infrastructure::agent_prompt_store)). When
    /// `Some`, the agent starts in its interactive mode already working on that
    /// prompt; when `None` it starts idle. A CLI that cannot take an opening
    /// prompt ignores it and launches plain.
    fn launch_command(
        &self,
        wiring: &AgentWiring,
        resume: bool,
        initial_prompt: Option<&str>,
    ) -> String;

    /// Whether a prior conversation this CLI could resume exists for the
    /// worktree at `dir` — checked so `:agent` only passes the resume flag when
    /// continuing would actually succeed, falling back to a fresh launch
    /// otherwise. A CLI without resumable history always returns `false`.
    fn has_resumable_session(&self, dir: &Path) -> bool;

    /// Discard any persisted conversation this CLI keeps for the worktree at
    /// `dir`, so removing a session also clears its chat history there and a
    /// session recreated at the same path later starts fresh instead of resuming
    /// the old conversation. The mirror of
    /// [`has_resumable_session`](Self::has_resumable_session): what that finds,
    /// this clears. Best-effort — a CLI with no stored history does nothing.
    fn forget_session(&self, dir: &Path);

    /// The command line that runs this CLI **headlessly** (non-interactive,
    /// one-shot) on `prompt`, wiring usagi's MCP servers in per `wiring` so the
    /// agent can drive usagi while it works, then exits when done.
    ///
    /// Unlike [`launch_command`](Self::launch_command) this never opens an
    /// interactive session: it is built for `usagi clean`, which spawns it
    /// detached in the background to let the agent autonomously triage and remove
    /// stale session worktrees with no human at the keyboard. Because nobody is
    /// there to answer them, each adapter passes its CLI's permission-bypass flag
    /// so the agent can act (delete worktrees, run git) without approval prompts.
    ///
    /// The result is a single `sh -c`-ready line, mirroring `launch_command`'s
    /// contract: every interpolated value — the `prompt` and the `wiring` paths —
    /// is escaped via [`super::util::shell_single_quote`](crate::infrastructure)
    /// so an apostrophe in a path or prompt cannot break out of the shell
    /// argument. Lifecycle hooks are deliberately omitted: a headless run reports
    /// no interactive phase, so there is nothing for usagi to watch.
    fn headless_command(&self, wiring: &AgentWiring, prompt: &str) -> String;

    /// Setup or configure any external state needed by this agent before it launches.
    /// Returns `Ok(())` on success, or an error message.
    fn provision(&self, _wiring: &AgentWiring) -> Result<(), String> {
        Ok(())
    }
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
    /// The model the launched agent CLI itself should run — rendered as that
    /// CLI's own model flag by the adapter (`--model` for Claude, `-m` for
    /// Codex / Gemini) — or `None` to let the CLI use its configured default.
    ///
    /// This is the injection point for choosing an agent's model; the adapters
    /// already render it. It is currently always `None`
    /// ([`Settings::agent_wiring`](crate::domain::settings::Settings::agent_wiring)
    /// leaves it unset), so usagi launches each CLI on its own default until a
    /// model source (a setting or a launch-time argument) fills it in.
    pub model: Option<String>,
    /// Whether the agent is launched at the workspace root (the coordinator row)
    /// rather than inside a session worktree.
    pub is_root: bool,
}
