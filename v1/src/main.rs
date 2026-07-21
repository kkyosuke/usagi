use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser, Subcommand};

use usagi::infrastructure::process::{self, Limits, Outcome};
use usagi::presentation::mcp::llm::LlmBackend;
use usagi::presentation::mcp::session::{AgentBackend, LaunchPromptDelivery};
use usagi::usecase::session;

/// The production [`AgentBackend`] for `usagi mcp`, wired in here at the
/// composition root so the mcp transport itself stays free of process / store IO
/// and unit-testable.
///
/// `prompt` *queues* the prompt for the target session's worktree rather than
/// running an agent itself: explicit launch requests are reserved for a fresh
/// agent, while an `auto` fallback may be handed to an eligible existing agent
/// after the TUI returns. `send` is the live counterpart: it appends the prompt to
/// `agent_live_prompt_store`, which a currently running TUI drains into the
/// session's existing agent pane.
///
/// `remove` resolves the workspace's effective agent CLI (so the removed
/// session's persisted conversation is discarded with the right adapter) and
/// delegates to [`session::remove`].
struct CliAgentBackend;

impl AgentBackend for CliAgentBackend {
    fn prompt(
        &self,
        worktree: &Path,
        prompt: &str,
        delivery: LaunchPromptDelivery,
    ) -> Result<String, String> {
        let reuse_live_agent = delivery == LaunchPromptDelivery::ReuseLiveAgent;
        usagi::infrastructure::agent_prompt_store::set_with_live_handoff(
            worktree,
            prompt,
            reuse_live_agent,
        )
        .map_err(|e| e.to_string())?;
        match delivery {
            LaunchPromptDelivery::FreshLaunch => Ok(
                "Queued the prompt for this session's next fresh agent launch. The usagi home \
                 screen delivers it as that agent's opening message."
                    .to_string(),
            ),
            LaunchPromptDelivery::ReuseLiveAgent => Ok(
                "Queued the prompt after auto mode found no live TUI consumer. When a TUI is \
                 available, it may deliver the prompt to an existing agent that is not reported \
                 running/waiting, or use it as the opening message for a fresh agent launch."
                    .to_string(),
            ),
        }
    }

    fn send(&self, worktree: &Path, prompt: &str) -> Result<String, String> {
        usagi::infrastructure::agent_live_prompt_store::append(worktree, prompt)
            .map_err(|e| e.to_string())?;
        // Whether a running TUI actually has a live agent pane to drain this queue
        // right now, so the confirmation tells the caller if the prompt was handed
        // to a live consumer or is waiting for one — the live channel is delivered
        // only by such a TUI, and reporting "live" for a prompt no one will drain
        // is what strands it.
        if self.agent_is_live(worktree) {
            Ok(
                "Queued the prompt for this session's running agent pane. A running usagi TUI \
                delivers it to the live agent by pasting it and pressing Enter."
                    .to_string(),
            )
        } else {
            Ok(
                "Appended the prompt to this session's live queue, but no live agent pane is \
                open for it right now, so nothing will deliver it until one opens (launch the \
                session's agent from the usagi home screen). To reserve the prompt for the \
                next fresh launch instead, send it with mode \"queue\"."
                    .to_string(),
            )
        }
    }

    fn agent_is_live(&self, worktree: &Path) -> bool {
        // A live-agent-pane marker published by a running TUI, stamped with that
        // TUI's pid — present only while a TUI holds a live agent pane for this
        // worktree, and read as dead once that TUI is gone (even if it crashed
        // without clearing it). This is the authoritative "the live channel has a
        // consumer" signal, so `session_prompt`'s `auto` mode uses it to prefer the
        // live channel only when the prompt would actually be drained. (The
        // agent-phase file is deliberately not used: it reports `ready` for an idle
        // agent and lingers stale after a TUI quits, which is what made `auto`
        // resolve to a live channel that no one was draining.)
        usagi::infrastructure::agent_live_pane_store::is_live(
            worktree,
            usagi::infrastructure::resource::process_alive,
        )
    }

    fn remove(
        &self,
        workspace_root: &Path,
        name: &str,
        force: bool,
    ) -> Result<session::RemovalOutcome, String> {
        let storage =
            usagi::infrastructure::storage::Storage::open_default().map_err(|e| e.to_string())?;
        let settings = usagi::usecase::settings::effective(&storage, workspace_root)
            .map_err(|e| e.to_string())?;
        let agent = usagi::infrastructure::agent::agent_for(settings.agent_cli);
        session::remove(workspace_root, name, force, agent.as_ref()).map_err(|e| e.to_string())
    }
}

/// The longest a single `ollama run` may take before it is killed and the call
/// fails. Local generation can be slow, so the budget is generous; its job is to
/// stop a wedged model or unreachable server from blocking the MCP call (and the
/// agent waiting on it) forever.
const ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// How often the subprocess lifecycle is re-polled.
const ASK_POLL: std::time::Duration = std::time::Duration::from_millis(50);
/// Cleanup is reserved inside [`ASK_TIMEOUT`], keeping the MCP call's complete
/// terminate/reap/drain lifecycle within that wall-clock budget.
const ASK_TERMINATE_GRACE: std::time::Duration = std::time::Duration::from_millis(250);
const ASK_REAP_GRACE: std::time::Duration = std::time::Duration::from_millis(250);
/// Largest prompt (system + user) sent to `ollama`, so a pathological input
/// cannot exhaust memory before the model even runs.
const MAX_INPUT_BYTES: usize = 256 * 1024;
/// Largest model output captured; anything beyond this is truncated rather than
/// buffered without bound.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// How much of `ollama`'s stderr is echoed back in an error, so a noisy or
/// sensitive diagnostic stream is not relayed to the agent in full.
const MAX_STDERR_BYTES: usize = 4 * 1024;

/// The production [`LlmBackend`] for `usagi llm-mcp`, wired in here at the
/// composition root so the llm-mcp transport stays free of subprocess IO and
/// unit-testable. Each completion runs `ollama run <model>`, feeding the prompt on
/// stdin and returning the captured stdout.
struct OllamaBackend {
    model: String,
}

impl LlmBackend for OllamaBackend {
    fn ask(&self, prompt: &str, system: Option<&str>) -> Result<String, String> {
        // A Homebrew-installed `ollama` runs no server until one is started, and
        // `run` does not auto-start it — so make sure the server is up first,
        // otherwise every call fails with "could not connect to ollama server".
        usagi::usecase::local_llm::ensure_server_started(&usagi::usecase::doctor::SystemRunner)?;

        // Ollama's `run` takes a single prompt; a system instruction is folded
        // in ahead of the prompt, separated by a blank line.
        let full = match system {
            Some(system) => format!("{system}\n\n{prompt}"),
            None => prompt.to_string(),
        };
        if full.len() > MAX_INPUT_BYTES {
            return Err(format!(
                "prompt is too large ({} bytes; limit is {MAX_INPUT_BYTES})",
                full.len()
            ));
        }

        let mut command = std::process::Command::new("ollama");
        command
            .arg("run")
            .arg(&self.model)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let outcome = process::run(
            command,
            Some(full.into_bytes()),
            Limits {
                timeout: ASK_TIMEOUT,
                terminate_grace: ASK_TERMINATE_GRACE,
                reap_grace: ASK_REAP_GRACE,
                poll_interval: ASK_POLL,
                stdout_cap: MAX_OUTPUT_BYTES,
                stderr_cap: MAX_STDERR_BYTES,
            },
        )
        .map_err(|e| format!("failed to start ollama: {e}"))?;
        let Outcome::Exited(output) = outcome else {
            return Err(format!(
                "ollama exceeded its {ASK_TIMEOUT:?} end-to-end deadline"
            ));
        };
        // A failed stdout read must not be reported as a complete (empty) reply.
        let stdout = output
            .stdout
            .map_err(|e| format!("failed to read ollama output: {e}"))?;
        let stderr = output.stderr.unwrap_or_default();
        if !output.status.success() {
            let mut detail = String::from_utf8_lossy(&stderr.bytes).trim().to_string();
            if stderr.truncated {
                detail.push_str(" …(truncated)");
            }
            return Err(format!("ollama exited with {}: {detail}", output.status));
        }
        Ok(String::from_utf8_lossy(&stdout.bytes).trim().to_string())
    }
}

#[derive(Parser)]
#[command(
    name = "usagi",
    version,
    about = "TUI/CLI for managing AI agent workflows"
)]
struct Cli {
    /// Defaults to `hop` (the welcome screen) when no subcommand is given.
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Record a running agent's lifecycle phase (invoked by agent hooks)
    #[command(hide = true)]
    AgentPhase {
        /// The phase the agent's hook is reporting
        #[arg(value_enum)]
        phase: usagi::presentation::cli::agent_phase::Phase,
    },
    /// Clean up stale session worktrees by launching an AI agent in the background
    Clean {
        /// Report the worktrees the agent would remove without deleting anything
        #[arg(long)]
        dry_run: bool,
        /// Override the configured agent CLI for this run (claude / codex / sakana.ai / gemini / antigravity)
        #[arg(long, value_name = "NAME")]
        agent: Option<String>,
    },
    /// Print a shell completion script for Tab completion
    Completion {
        /// Which shell to generate the completion script for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Show usagi's configuration (or edit it with --edit)
    // Hidden from `usagi --help`: global settings are edited from the welcome
    // screen's Config. This command stays available for raw settings inspection
    // and `--edit` fields the TUI does not expose (e.g. `workspace_root`).
    #[command(hide = true)]
    Config {
        /// Open the configuration file in $EDITOR and validate it on save
        #[arg(long)]
        edit: bool,
    },
    /// Check required tools and offer to install anything missing
    Doctor {
        /// Install everything missing without asking (otherwise prompt first)
        #[arg(long)]
        fix: bool,
    },
    /// Show which usagi features each agent CLI supports
    Feature,
    /// Deny an agent tool call that escapes its session worktree (invoked by a Claude PreToolUse hook)
    #[command(hide = true)]
    GuardWorkspace,
    /// Run Claude inside the required platform OS sandbox
    #[command(hide = true)]
    ClaudeSandbox {
        /// `session` makes cwd writable; `root` keeps it read-only
        #[arg(long)]
        mode: String,
        /// Additional canonicalizable writable roots
        #[arg(long = "writable-root")]
        writable_roots: Vec<PathBuf>,
        /// Command and arguments to execute inside the sandbox
        #[arg(last = true, required = true)]
        command: Vec<OsString>,
    },
    /// Hop into the usagi welcome screen
    Hop,
    /// Print the square-pixel usagi marks (flip / half)
    Icon {
        /// Which mark to show (defaults to all)
        #[arg(value_enum, default_value = "all")]
        view: usagi::presentation::cli::icon::IconView,
    },
    /// Register the current directory as a project (or clone one into it with --git)
    Init {
        /// Clone this repository URL into <repo-name>/ under the current directory
        #[arg(long, value_name = "URL")]
        git: Option<String>,
    },
    /// Initialize AI agent configuration files (like CLAUDE.md, .clinerules, .aider.conf.yml)
    InitAgent {
        /// Overwrite existing files without prompting
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Manage task issues stored in .usagi/issues/
    ///
    /// Hidden from the CLI: issues are operated by AI agents via the MCP server.
    #[command(hide = true)]
    Issue {
        #[command(subcommand)]
        command: usagi::presentation::cli::issue::IssueCommand,
    },
    /// Manage durable agent memories stored in .usagi/memory/
    ///
    /// Hidden from the CLI: memories are operated by AI agents via the MCP server.
    #[command(hide = true)]
    Memory {
        #[command(subcommand)]
        command: usagi::presentation::cli::memory::MemoryCommand,
    },
    /// Store the 1Password credential used to resolve workspace `op://` env vars
    Op {
        #[command(subcommand)]
        command: usagi::presentation::cli::op::OpCommand,
    },
    /// Run the local LLM MCP server over stdio (for AI agents to offload work)
    ///
    /// Hidden from the CLI: launched by AI agents, not invoked by hand.
    #[command(hide = true)]
    LlmMcp {
        /// The Ollama model completions run against
        #[arg(long, value_name = "MODEL", default_value = usagi::domain::settings::DEFAULT_LOCAL_LLM_MODEL)]
        model: String,
    },
    /// Run the usagi MCP server over stdio (issue / memory / session tools for AI agents)
    ///
    /// Hidden from the CLI: launched by AI agents, not invoked by hand.
    #[command(hide = true)]
    Mcp,
    /// Play a usagi animation (1=走る 2=増える 3,4=読み込み 5=マスコット)
    Run {
        /// Which animation to play (1–5)
        #[arg(value_name = "N", default_value_t = 1)]
        n: u8,
    },
    /// Sync the current repository's worktree state to .usagi/state.json
    Status,
    /// Download and install a released usagi binary
    Update {
        /// Choose the release version to install
        #[arg(short = 'v')]
        select_version: bool,
    },
}

fn main() -> anyhow::Result<()> {
    // Honor the NO_COLOR convention (https://no-color.org/), which `console`'s
    // built-in detection ignores. Done before any output so it applies to both
    // the CLI commands and the TUI. The decision is the unit-tested pure helper;
    // here we read the real environment and apply the global toggle.
    if usagi::presentation::color::should_disable_colors(
        std::env::var("NO_COLOR").ok().as_deref(),
        std::env::var("CLICOLOR_FORCE").ok().as_deref(),
    ) {
        console::set_colors_enabled(false);
    }

    let cli = Cli::parse();

    // No subcommand behaves the same as `usagi hop`.
    let command = cli.command.unwrap_or(Commands::Hop);

    let name = command_name(&command);
    let result = match command {
        Commands::AgentPhase { phase } => {
            usagi::presentation::cli::agent_phase::run(phase, std::io::stdin().lock())
        }
        Commands::Clean { dry_run, agent } => {
            usagi::presentation::cli::clean::run(dry_run, agent, spawn_detached)
        }
        Commands::Completion { shell } => {
            let mut cmd = Cli::command();
            let mut stdout = std::io::stdout();
            usagi::presentation::cli::completion::write(shell, &mut cmd, &mut stdout);
            Ok(())
        }
        Commands::Config { edit } => usagi::presentation::cli::config::run(edit),
        Commands::Doctor { fix } => usagi::presentation::cli::doctor::run(fix),
        Commands::Feature => usagi::presentation::cli::feature::run(),
        Commands::GuardWorkspace => {
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            usagi::presentation::cli::guard_workspace::run(stdin.lock(), stdout.lock())
        }
        Commands::ClaudeSandbox {
            mode,
            writable_roots,
            command,
        } => usagi::presentation::cli::claude_sandbox::run(
            usagi::presentation::cli::claude_sandbox::Mode::parse(&mode)?,
            writable_roots,
            command,
        ),
        Commands::Hop => {
            // Materialise usagi's shipped skills under the data dir before the TUI
            // launches any agent, so each session worktree's `.claude/skills`
            // symlink resolves to current content. Best-effort.
            let _ = usagi::infrastructure::skills::materialize_default();
            usagi::presentation::cli::hop::run(usagi::presentation::tui::app::run)
        }
        Commands::Icon { view } => usagi::presentation::cli::icon::run(view),
        Commands::Init { git } => usagi::presentation::cli::init::run(git),
        Commands::InitAgent { yes } => usagi::presentation::cli::init_agent::run(yes),
        Commands::Issue { command } => usagi::presentation::cli::issue::run(command),
        Commands::Memory { command } => usagi::presentation::cli::memory::run(command),
        Commands::Op { command } => {
            let mut stdout = std::io::stdout();
            usagi::presentation::cli::op::run(
                command,
                &usagi::infrastructure::secret_store::SystemSecretStore,
                Some(Box::new(|| {
                    console::Term::stderr()
                        .read_secure_line()
                        .map_err(Into::into)
                })),
                &mut stdout,
            )
        }
        Commands::LlmMcp { model } => {
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            usagi::presentation::cli::llm_mcp::run(
                Box::new(OllamaBackend {
                    model: model.clone(),
                }),
                model,
                stdin.lock(),
                stdout.lock(),
            )
        }
        Commands::Mcp => {
            install_mcp_panic_log_hook();
            // A session created over MCP symlinks each worktree at the skills dir;
            // materialise it here so the target exists. Best-effort.
            let _ = usagi::infrastructure::skills::materialize_default();
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            usagi::presentation::cli::mcp::run(
                Box::new(CliAgentBackend),
                Box::new(usagi::usecase::agent::CliAgentModelProbe),
                stdin.lock(),
                stdout.lock(),
            )
        }
        Commands::Run { n } => usagi::presentation::cli::run::run(n),
        Commands::Status => usagi::presentation::cli::status::run(),
        Commands::Update { select_version } => {
            usagi::presentation::cli::update::run(select_version)
        }
    };

    trace_command(name, result.is_ok());
    if let Err(error) = &result {
        log_error(error);
    }
    result
}

/// The stable name a subcommand is traced under (its `usagi <name>` word). The
/// `mcp` / `llm-mcp` long-running servers are excluded: they run for a whole
/// session and would only ever record one open-ended "still running" line.
fn command_name(command: &Commands) -> Option<&'static str> {
    match command {
        Commands::AgentPhase { .. } => Some(usagi::domain::agent_phase::AGENT_PHASE_COMMAND),
        Commands::Clean { .. } => Some("clean"),
        Commands::Completion { .. } => Some("completion"),
        Commands::Config { .. } => Some("config"),
        Commands::Doctor { .. } => Some("doctor"),
        Commands::Feature => Some("feature"),
        Commands::GuardWorkspace => Some("guard-workspace"),
        Commands::ClaudeSandbox { .. } => Some("claude-sandbox"),
        Commands::Hop => Some("hop"),
        Commands::Icon { .. } => Some("icon"),
        Commands::Init { .. } => Some("init"),
        Commands::InitAgent { .. } => Some("init-agent"),
        Commands::Issue { .. } => Some("issue"),
        Commands::Memory { .. } => Some("memory"),
        Commands::Op { .. } => Some("op"),
        Commands::Run { .. } => Some("run"),
        Commands::Status => Some("status"),
        Commands::Update { .. } => Some("update"),
        Commands::LlmMcp { .. } | Commands::Mcp => None,
    }
}

/// Best-effort: record the finished CLI command (and whether it succeeded) to the
/// operation trace, when tracing is enabled. A no-op for the long-running servers
/// and whenever tracing is off.
fn trace_command(name: Option<&'static str>, ok: bool) {
    use usagi::domain::trace::{TraceCategory, TraceEvent};
    if let Some(name) = name {
        usagi::infrastructure::trace_log::TraceLog::record(
            TraceEvent::now(TraceCategory::Cli, name).with_detail(if ok { "ok" } else { "error" }),
        );
    }
}

/// Best-effort: append `error` (with its full cause chain) to today's log file
/// and prune files older than the retention window. Any failure here is
/// swallowed so logging never masks the original error on its way to stderr.
fn log_error(error: &anyhow::Error) {
    usagi::infrastructure::error_log::ErrorLog::record(&format!("{error:#}"));
}

/// Install an MCP-specific panic hook before the long-running stdio server starts.
///
/// `dispatch_tool_call` catches panics from individual tools and turns them into
/// MCP `isError` results so the process keeps serving future requests; Rust still
/// runs the panic hook before the unwind reaches that catch boundary. Recording
/// the hook here makes the original panic site and payload inspectable in
/// `<data dir>/logs/` even when the client only sees a sanitized tool error. The
/// previous hook is chained so normal stderr diagnostics are preserved.
fn install_mcp_panic_log_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        usagi::infrastructure::error_log::ErrorLog::record(&format!(
            "panic while running usagi mcp: {info}\nbacktrace:\n{backtrace}"
        ));
        previous(info);
    }));
}

/// Spawn `command` via `sh -c` detached in the background, with `cwd` as its
/// working directory and its stdout/stderr appended to `log_path`. Returns once
/// the child is spawned — usagi does not wait for it. This is the production
/// spawner `usagi clean` injects into [`usagi::presentation::cli::clean::run`];
/// it lives here at the (coverage-excluded) composition root so that command's
/// orchestration stays a pure, unit-tested flow.
fn spawn_detached(command: &str, cwd: &Path, log_path: &Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::fs::OpenOptions;
    use std::process::{Command, Stdio};

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating log directory {}", parent.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening log file {}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("opening log file {}", log_path.display()))?;

    let mut builder = Command::new("sh");
    builder
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    // Detach from usagi's process group so the agent keeps running after usagi
    // exits (Unix only; on other platforms the child simply outlives the parent).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        builder.process_group(0);
    }
    builder
        .spawn()
        .with_context(|| format!("spawning background agent: {command}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_detached_runs_the_command_with_cwd_and_appends_to_the_log() {
        // The wrapper runs `sh -c <command>` with the given cwd and appends
        // stdout/stderr to the log file, creating its parent directory.
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let log = cwd.join(".usagi").join("clean.log");
        spawn_detached("printf done > marker; printf log-line 1>&2", cwd, &log).unwrap();

        // Poll for the detached child to finish (it is not waited on).
        let marker = cwd.join("marker");
        for _ in 0..100 {
            if marker.exists()
                && std::fs::read_to_string(&log)
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "done");
        assert!(std::fs::read_to_string(&log).unwrap().contains("log-line"));
    }

    #[test]
    fn spawn_detached_errors_when_the_log_path_is_unusable() {
        // A log path whose parent cannot be created (a file stands where a
        // directory is needed) surfaces an error rather than spawning.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let log = blocker.join(".usagi").join("clean.log");
        assert!(spawn_detached("true", dir.path(), &log).is_err());
    }
}
