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

use std::path::{Path, PathBuf};

/// Which launch surface an adapter is planning for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// A human-facing session pane that stays open and can receive further input.
    Interactive,
    /// A one-shot, non-interactive run used for autonomous background work.
    Headless,
}

/// A normalized request to build an agent launch.
///
/// Existing adapters still receive the historical trait methods
/// ([`Agent::launch_command`] and [`Agent::headless_command`]) and may migrate to
/// this request shape one at a time. The shape is intentionally small: it carries
/// the common wiring policy plus the mode-specific prompt/resume knobs without
/// committing adapters to a shared rendering implementation yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchRequest<'a> {
    /// usagi's wiring policy for MCP servers, hooks, model flags, and root/session
    /// context.
    pub wiring: &'a AgentWiring,
    /// Whether an interactive launch should resume prior conversation history.
    /// Ignored for headless launches.
    pub resume: bool,
    /// Opening message for an interactive launch, when the CLI supports one.
    pub initial_prompt: Option<&'a str>,
    /// One-shot prompt for a headless launch.
    pub prompt: Option<&'a str>,
    /// Which launch surface is requested.
    pub mode: LaunchMode,
}

impl<'a> LaunchRequest<'a> {
    /// Build an interactive launch request equivalent to [`Agent::launch_command`].
    pub fn interactive(
        wiring: &'a AgentWiring,
        resume: bool,
        initial_prompt: Option<&'a str>,
    ) -> Self {
        Self {
            wiring,
            resume,
            initial_prompt,
            prompt: None,
            mode: LaunchMode::Interactive,
        }
    }

    /// Build a headless launch request equivalent to [`Agent::headless_command`].
    pub fn headless(wiring: &'a AgentWiring, prompt: &'a str) -> Self {
        Self {
            wiring,
            resume: false,
            initial_prompt: None,
            prompt: Some(prompt),
            mode: LaunchMode::Headless,
        }
    }
}

/// A shell-neutral command plan: one program plus its argv vector.
///
/// Current adapters still return shell strings directly. This type is the shared
/// migration target for future adapter work: build a `LaunchPlan` first, then
/// render it to the legacy `sh -c`-ready string with [`shell_escaped`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// Program/executable name (for example `claude`, `codex`, or `gemini`).
    pub program: String,
    /// Arguments passed to `program`, excluding argv[0].
    pub args: Vec<String>,
}

impl LaunchPlan {
    /// Start a plan for `program`.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// Append one argument and return the updated plan.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append many arguments and return the updated plan.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Render the plan as a single `sh -c`-ready command line.
    ///
    /// Every token, including the program name, is single-quoted via
    /// [`shell_single_quote`], so spaces, newlines, `$`, backticks, and embedded
    /// quotes inside model names, paths, or prompts cannot escape their argument.
    pub fn shell_escaped(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(shell_single_quote)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Wrap `text` as one POSIX-shell argument using single-quote escaping.
///
/// This is the canonical shell-argument escaper for agent launch planning. The
/// infrastructure adapters expose their existing helper as a thin wrapper around
/// this function so future `LaunchPlan` migration and current command rendering
/// share one escaping rule.
pub fn shell_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Append a POSIX-shell lifecycle fallback that records `exited` after an
/// interactive Agent process returns, while preserving that process's exit
/// status. Some CLIs have no SessionEnd hook; without this, the pane can remain
/// advertised as an Agent input consumer until PTY exit is observed.
#[cfg(not(windows))]
pub fn with_exit_phase(command: &str, usagi_bin: &str) -> String {
    format!(
        "{command}; _usagi_agent_status=$?; {} 'agent-phase' 'exited' </dev/null; exit $_usagi_agent_status",
        shell_single_quote(usagi_bin)
    )
}

/// Windows shells do not share the POSIX status/quoting syntax. Until their
/// adapters expose a shell-neutral exit hook, preserve the existing launch
/// command; native lifecycle hooks still report the phases they support.
#[cfg(windows)]
pub fn with_exit_phase(command: &str, _usagi_bin: &str) -> String {
    command.to_string()
}

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
    /// Adapters that use such a flag must still retain an unattended hard
    /// boundary; Claude wraps both launch surfaces in usagi's OS sandbox.
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
    /// Extra filesystem roots the adapter may make writable for a sandboxed
    /// launch.
    ///
    /// This is data only: callers resolve any tool-specific or git-specific
    /// paths before constructing the wiring, while each adapter decides whether
    /// and how to render them. Codex uses these alongside usagi's own data
    /// directory in `sandbox_workspace_write.writable_roots`; the usecase layer
    /// also supplies the project `.usagi/` store for session launches so MCP
    /// sub-session delegation can mutate workspace state without asking. Other
    /// adapters may ignore these roots.
    pub sandbox_writable_roots: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wiring() -> AgentWiring {
        AgentWiring {
            usagi_bin: "/bin/usagi".to_string(),
            local_llm_model: Some("qwen2.5-coder".to_string()),
            model: Some("large model".to_string()),
            is_root: false,
            sandbox_writable_roots: Vec::new(),
        }
    }

    #[test]
    fn shell_single_quote_escapes_one_shell_argument() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        assert_eq!(shell_single_quote("a'b"), r"'a'\''b'");
        assert_eq!(shell_single_quote("$x `y` \"z\""), "'$x `y` \"z\"'");
    }

    #[test]
    #[cfg(not(windows))]
    fn exit_phase_wrapper_runs_after_the_agent_and_quotes_the_usagi_binary() {
        assert_eq!(
            with_exit_phase("codex --resume", "/opt/usagi build/usagi"),
            "codex --resume; _usagi_agent_status=$?; '/opt/usagi build/usagi' 'agent-phase' 'exited' </dev/null; exit $_usagi_agent_status"
        );
    }

    #[test]
    #[cfg(windows)]
    fn exit_phase_wrapper_preserves_the_command_on_windows() {
        assert_eq!(
            with_exit_phase("codex --resume", r"C:\Program Files\usagi.exe"),
            "codex --resume"
        );
    }

    #[test]
    fn launch_plan_renders_a_shell_escaped_legacy_command_line() {
        let plan = LaunchPlan::new("codex")
            .arg("--model")
            .arg("gpt with spaces")
            .args(["exec", "say 'hello'"]);

        assert_eq!(
            plan.shell_escaped(),
            r"'codex' '--model' 'gpt with spaces' 'exec' 'say '\''hello'\'''"
        );
    }

    #[test]
    fn launch_request_constructors_encode_interactive_and_headless_modes() {
        let wiring = wiring();

        let interactive = LaunchRequest::interactive(&wiring, true, Some("start here"));
        assert_eq!(interactive.mode, LaunchMode::Interactive);
        assert_eq!(interactive.wiring, &wiring);
        assert!(interactive.resume);
        assert_eq!(interactive.initial_prompt, Some("start here"));
        assert_eq!(interactive.prompt, None);

        let headless = LaunchRequest::headless(&wiring, "clean stale sessions");
        assert_eq!(headless.mode, LaunchMode::Headless);
        assert_eq!(headless.wiring, &wiring);
        assert!(!headless.resume);
        assert_eq!(headless.initial_prompt, None);
        assert_eq!(headless.prompt, Some("clean stale sessions"));
    }
}
