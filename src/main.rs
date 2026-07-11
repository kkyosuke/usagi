use std::path::Path;

use clap::{CommandFactory, Parser, Subcommand};

use usagi::presentation::mcp::child_io::{read_capped, wait_with_timeout, WaitableChild};
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
/// How often the wait loop re-polls the child while it runs.
const ASK_POLL: std::time::Duration = std::time::Duration::from_millis(50);
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

        let mut child = std::process::Command::new("ollama")
            .arg("run")
            .arg(&self.model)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start ollama: {e}"))?;

        // Spawn the stdin writer and the stdout/stderr drains *all up front*,
        // before waiting, so no single full pipe can deadlock: while we feed up to
        // 256 KiB of prompt, ollama's output is drained concurrently, and vice
        // versa. (Writing the whole prompt before starting to drain would deadlock
        // if ollama emitted enough output to fill its stdout pipe before consuming
        // all of stdin.) Dropping `stdin` at the end of the writer thread closes it
        // so ollama sees EOF and begins generating.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open ollama stdin".to_string())?;
        let input = full.into_bytes();
        let stdin_writer = std::thread::spawn(move || {
            use std::io::Write as _;
            let _ = stdin.write_all(&input);
        });

        let mut out = child
            .stdout
            .take()
            .ok_or_else(|| "failed to open ollama stdout".to_string())?;
        let mut err = child
            .stderr
            .take()
            .ok_or_else(|| "failed to open ollama stderr".to_string())?;
        let out_reader = std::thread::spawn(move || read_capped(&mut out, MAX_OUTPUT_BYTES));
        let err_reader = std::thread::spawn(move || read_capped(&mut err, MAX_STDERR_BYTES));

        let status = wait_with_timeout(&mut RealChild(child), ASK_TIMEOUT, ASK_POLL);
        // The writer thread finishes when the prompt is written, or when a killed
        // ollama closes its stdin read-end (write_all then errors out) — so this
        // join never hangs.
        let _ = stdin_writer.join();
        let stdout_result = out_reader
            .join()
            .unwrap_or_else(|_| Ok((Vec::new(), false)));
        let stderr_result = err_reader
            .join()
            .unwrap_or_else(|_| Ok((Vec::new(), false)));

        let Some(status) = status else {
            return Err(format!(
                "ollama did not finish within {ASK_TIMEOUT:?} and was terminated"
            ));
        };
        // A failed stdout read must not be reported as a complete (empty) reply.
        let (stdout, _) =
            stdout_result.map_err(|e| format!("failed to read ollama output: {e}"))?;
        let (stderr, stderr_truncated) = stderr_result.unwrap_or((Vec::new(), false));
        if !status.success() {
            let mut detail = String::from_utf8_lossy(&stderr).trim().to_string();
            if stderr_truncated {
                detail.push_str(" …(truncated)");
            }
            return Err(format!("ollama exited with {status}: {detail}"));
        }
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    }
}

/// The production [`WaitableChild`] for [`wait_with_timeout`]: a thin newtype over
/// a live `ollama run` child that delegates the three lifecycle calls to
/// `std::process::Child`. The wait-loop decision logic lives (and is tested) in
/// [`usagi::presentation::mcp::child_io`]; this real-process delegation stays here
/// at the composition root, like the MCP backends above.
struct RealChild(std::process::Child);

impl WaitableChild for RealChild {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }
    fn kill(&mut self) -> std::io::Result<()> {
        self.0.kill()
    }
    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.0.wait()
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
    /// Control the per-machine background daemon (start / stop / status)
    ///
    /// Hidden from `usagi --help`: the daemon has no user-visible behaviour yet
    /// (it only supervises itself). The control plane lands ahead of the work
    /// that moves agent PTY ownership into it — see `document/proposals/02-daemon.md`.
    #[command(hide = true)]
    Daemon {
        #[command(subcommand)]
        command: usagi::presentation::cli::daemon::DaemonCommand,
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
    /// Update the default branch from its remote and distribute it into each session worktree (only where it merges cleanly)
    Update {
        /// Fetch and report what would change without modifying any branch or worktree
        #[arg(long)]
        dry_run: bool,
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
        Commands::Daemon { command } => {
            let dir = usagi::infrastructure::daemon_store::default_dir()?;
            let mut stdout = std::io::stdout();
            usagi::presentation::cli::daemon::run(
                command,
                &dir,
                &usagi::infrastructure::resource::process_alive,
                &|| spawn_daemon(&dir),
                &|| run_daemon_serve(&dir),
                &mut stdout,
            )
        }
        Commands::Doctor { fix } => usagi::presentation::cli::doctor::run(fix),
        Commands::Feature => usagi::presentation::cli::feature::run(),
        Commands::GuardWorkspace => {
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            usagi::presentation::cli::guard_workspace::run(stdin.lock(), stdout.lock())
        }
        Commands::Hop => {
            // Materialise usagi's shipped skills under the data dir before the TUI
            // launches any agent, so each session worktree's `.claude/skills`
            // symlink resolves to current content. Best-effort.
            let _ = usagi::infrastructure::skills::materialize_default();
            // Capture this executable generation before the long-running TUI
            // starts. A later `cargo run` rebuild may replace target/debug/usagi,
            // but this process must keep identifying as the binary it began as.
            #[cfg(unix)]
            let _ = usagi::infrastructure::daemon_client::build_identity();
            // Autospawn the daemon that owns the agent terminals, so the TUI can
            // attach to it (and agents keep running after the TUI closes).
            // Best-effort and idempotent: with a daemon already running this is
            // a no-op, and with no daemon at all the terminal pool falls back to
            // TUI-local PTYs (the pre-daemon behaviour).
            if let Ok(dir) = usagi::infrastructure::daemon_store::default_dir() {
                let _ = usagi::usecase::daemon::start(
                    &dir,
                    &usagi::infrastructure::resource::process_alive,
                    &|| spawn_daemon(&dir),
                );
            }
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
        Commands::Update { dry_run } => usagi::presentation::cli::update::run(dry_run),
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
        // `daemon serve` is the long-running loop — excluded like the mcp servers
        // below; the short control subcommands are traced.
        Commands::Daemon { command } => match command {
            usagi::presentation::cli::daemon::DaemonCommand::Serve => None,
            _ => Some("daemon"),
        },
        Commands::Doctor { .. } => Some("doctor"),
        Commands::Feature => Some("feature"),
        Commands::GuardWorkspace => Some("guard-workspace"),
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

/// How often the daemon's control plane beats: the stop-request check and the
/// session monitor tick.
const DAEMON_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// How often the IPC endpoint is serviced while clients are connected. This is
/// the ceiling on input echo latency for an attached TUI (a keystroke waits at
/// most one tick to reach the PTY, and its output at most one more to stream
/// back), so it is much shorter than the control-plane beat; with no clients
/// the loop falls back to [`DAEMON_POLL`] so an idle daemon stays cheap.
const DAEMON_IPC_TICK: std::time::Duration = std::time::Duration::from_millis(15);

/// Run the daemon supervisor loop in the foreground (the body of `usagi daemon
/// serve`, launched detached by `usagi daemon start`).
///
/// It claims the single-instance slot for this pid — exiting quietly if another
/// live daemon already holds it — then polls for a stop request until one
/// arrives, and releases the slot on the way out. The decisions
/// (register / take-stop / deregister) are the unit-tested usecase/store calls;
/// this composition-root wrapper only supplies the real process table, the sleep,
/// and stderr, so it stays out of coverage like [`spawn_detached`].
fn run_daemon_serve(dir: &Path) -> anyhow::Result<()> {
    use usagi::usecase::daemon::RegisterOutcome;
    let pid = std::process::id();
    match usagi::usecase::daemon::register(
        dir,
        pid,
        &usagi::infrastructure::resource::process_alive,
    )? {
        RegisterOutcome::AlreadyRunning { pid } => {
            eprintln!("usagi daemon already running (pid {pid}); exiting");
            return Ok(());
        }
        RegisterOutcome::Registered => {}
    }

    // Bind the IPC socket that clients connect to for the session feed. A stale
    // socket file from a crashed daemon is removed first (this daemon owns the
    // single-instance slot, so any leftover is dead). Best-effort: if the socket
    // cannot be bound the daemon still runs its monitor, just without IPC.
    let socket_path = usagi::infrastructure::daemon_ipc::socket_path(dir);
    #[cfg(unix)]
    let build = usagi::infrastructure::daemon_client::build_identity().unwrap_or_default();
    #[cfg(not(unix))]
    let build = String::new();
    let mut server = DaemonIpcServer::bind(&socket_path, build);
    server.replace_sessions(usagi::infrastructure::daemon_sessions_store::read(dir)?);
    server.adopt_persisted_terminals(dir);

    let mut next_control = std::time::Instant::now();
    let mut next_reap = std::time::Instant::now();
    loop {
        // The control-plane beat runs on the slow cadence regardless of how
        // fast the IPC endpoint is being serviced.
        if std::time::Instant::now() >= next_control {
            next_control = std::time::Instant::now() + DAEMON_POLL;
            if usagi::infrastructure::daemon_store::take_stop_request(dir)? {
                break;
            }
            // Refresh the monitored-sessions snapshot. Best-effort: a transient
            // store error must not tear the daemon down, so it is logged and
            // the loop continues. On a change, push the fresh snapshot to every
            // subscribed client.
            let previous_sessions = server.sessions().to_vec();
            match usagi::usecase::daemon::monitor_tick_cached(
                dir,
                &previous_sessions,
                &daemon_gather,
            ) {
                Ok(Some(current_sessions)) => {
                    server.notify_session_transitions(&previous_sessions, &current_sessions);
                    server.replace_sessions(current_sessions);
                    server.broadcast_sessions();
                }
                Ok(None) => {}
                Err(error) => eprintln!("usagi daemon: session monitor tick failed: {error:#}"),
            }
            server.consume_queued_start(dir);
        }
        // Accept any newly connected clients, answer whatever they have sent,
        // and stream terminal output to attached clients.
        let reap_terminals = std::time::Instant::now() >= next_reap;
        if reap_terminals {
            next_reap = std::time::Instant::now() + DAEMON_POLL;
        }
        server.poll(dir, reap_terminals);
        std::thread::sleep(if server.has_clients() {
            DAEMON_IPC_TICK
        } else {
            DAEMON_POLL
        });
    }

    server.shutdown(dir, &socket_path);
    usagi::usecase::daemon::deregister(dir)
}

/// The daemon's single-threaded IPC endpoint: a non-blocking [`UnixListener`] and
/// the connected clients, driven a step at a time from the serve loop. It owns
/// the real socket IO — accepting, reading, writing, disconnect detection — and
/// delegates every protocol decision (which reply, who is subscribed) to the
/// unit-tested [`usagi::usecase::daemon_ipc`] / [`usagi::domain::daemon_ipc`].
/// Composition-root IO, excluded from coverage like the rest of this file.
///
/// Single-threaded on purpose: with no worker threads there are no locks around
/// the registry or the client table, and a client's request is answered within
/// one [`DAEMON_POLL`] tick — fast enough for the session feed.
#[cfg(unix)]
struct DaemonIpcServer {
    listener: Option<std::os::unix::net::UnixListener>,
    clients: std::collections::HashMap<u64, IpcClient>,
    registry: usagi::usecase::daemon_ipc::SubscriberRegistry,
    /// The executable generation captured when this daemon started. Terminal
    /// clients must identify with the same value before spawning or attaching.
    build: String,
    next_id: u64,
    /// The daemon-owned terminals, keyed by the id assigned at spawn. Holding
    /// the [`PtySession`] here — not on any client — is what makes a terminal
    /// outlive the client that asked for it: a client disconnecting only drops
    /// its socket, never these. Dropping a session kills its process group.
    ///
    /// [`PtySession`]: usagi::infrastructure::pty::PtySession
    terminals: std::collections::HashMap<
        usagi::domain::daemon_ipc::TerminalId,
        usagi::infrastructure::pty::PtySession,
    >,
    /// The pure mirror of `terminals` (id → worktree/pid) for the tested
    /// bookkeeping, and the id allocator.
    terminal_registry: usagi::usecase::daemon_ipc::TerminalRegistry,
    /// Which clients are attached to which terminal's output feed, and how far
    /// into its backlog each has been pushed.
    attach_table: usagi::usecase::daemon_ipc::AttachTable,
    /// Terminal ids restored from `terminals.json` after an abnormal daemon
    /// exit. Their processes are alive, but the old PTY master fd is gone, so
    /// they cannot be screen-attached; the daemon keeps them only to avoid id
    /// reuse and to kill them on deliberate stop.
    adopted_terminals: std::collections::HashSet<usagi::domain::daemon_ipc::TerminalId>,
    /// Latest monitored-session snapshot. The daemon monitor owns refreshes;
    /// IPC `ListSessions` / `Subscribe` answer from this cache instead of
    /// re-reading `sessions.json` on every socket poll.
    session_cache: Vec<usagi::domain::daemon::SessionSnapshot>,
}

/// One connected client: its stream and the decoder reassembling frames from its
/// partial reads.
#[cfg(unix)]
struct IpcClient {
    stream: std::os::unix::net::UnixStream,
    decoder: usagi::domain::daemon_ipc::FrameDecoder,
    /// Set only after this connection's `Hello` matched the daemon executable
    /// generation. Session-feed messages do not need it; terminal IO does.
    terminal_build_verified: bool,
}

#[cfg(unix)]
impl DaemonIpcServer {
    /// Claim and start at most one durable queued request per control tick.
    /// Keeping the batch to one bounds environment/provisioning latency while
    /// repeated ticks fill the configured concurrency budget.
    fn consume_queued_start(&mut self, dir: &Path) {
        use usagi::domain::daemon_ipc::ServerMessage;
        use usagi::infrastructure::agent_start_store::{self, StartState};

        let settings = usagi::infrastructure::storage::Storage::open_default()
            .and_then(|storage| storage.load_settings())
            .unwrap_or_default();
        if !settings.autostart_queued_prompts
            || self.terminals.len() + self.adopted_terminals.len()
                >= settings.autostart_queued_prompt_limit
        {
            return;
        }
        let owner = format!("daemon:{}", std::process::id());
        let start_dir = usagi::infrastructure::storage::data_dir()
            .map(|data| data.join("agent-start-requests"))
            .unwrap_or_default();
        for worktree in agent_start_store::queued_worktrees_in(&start_dir) {
            let existing = self
                .terminal_registry
                .ids()
                .iter()
                .copied()
                .find(|id| self.terminal_registry.belongs_to(*id, &worktree));
            if existing.is_some()
                && !agent_start_store::read(&worktree)
                    .is_some_and(|request| request.reuse_live_agent)
            {
                continue;
            }
            let request =
                match agent_start_store::claim(&worktree, &owner, std::time::SystemTime::now()) {
                    Ok(Some(request)) => request,
                    Ok(None) => continue,
                    Err(error) => {
                        eprintln!("usagi daemon: claiming queued start failed: {error:#}");
                        continue;
                    }
                };
            if let Some(terminal) = existing {
                let mut input = request.prompt.as_bytes().to_vec();
                input.push(b'\r');
                match self.write_terminal(terminal, &input) {
                    Ok(()) => {
                        let _ = agent_start_store::advance(
                            &worktree,
                            request.id,
                            &owner,
                            StartState::Running { terminal },
                        );
                        let _ = usagi::infrastructure::agent_prompt_store::take(&worktree);
                    }
                    Err(error) => {
                        let _ = agent_start_store::fail(&worktree, request.id, &owner, &error);
                    }
                }
                break;
            }
            let result = (|| -> anyhow::Result<u64> {
                let cli = request.agent.cli.unwrap_or(settings.agent_cli);
                let agent = usagi::infrastructure::agent::agent_for(cli);
                let exe = std::env::current_exe()?.to_string_lossy().into_owned();
                let base = settings.agent_wiring(&exe);
                let wiring = usagi::usecase::agent::wiring_for_launch(
                    &base,
                    request.agent.model.clone(),
                    &worktree,
                    usagi::domain::agent::LaunchMode::Interactive,
                    &usagi::infrastructure::git::git_common_dir,
                );
                agent.provision(&wiring).map_err(anyhow::Error::msg)?;
                let command = usagi::domain::agent::with_exit_phase(
                    &agent.launch_command(&wiring, agent.has_resumable_session(&worktree), None),
                    &wiring.usagi_bin,
                );
                let workspace = usagi::usecase::session::workspace_root(&worktree);
                let env = usagi::infrastructure::env_resolver::resolve_workspace_env(&workspace);
                let terminal =
                    match self.spawn_terminal(worktree.clone(), Some(&command), &env, 80, 24, 1000)
                    {
                        ServerMessage::Spawned { terminal, .. } => terminal,
                        ServerMessage::Error { message } => anyhow::bail!(message),
                        _ => unreachable!("spawn_terminal returns spawn result"),
                    };
                self.persist_terminals(dir);
                let mut input = request.prompt.as_bytes().to_vec();
                input.push(b'\r');
                if let Err(error) = self.write_terminal(terminal, &input) {
                    self.kill_terminal(dir, terminal, None);
                    anyhow::bail!(error);
                }
                Ok(terminal)
            })();
            match result {
                Ok(terminal) => {
                    if let Err(error) = agent_start_store::advance(
                        &worktree,
                        request.id,
                        &owner,
                        StartState::Running { terminal },
                    ) {
                        // The terminal is deliberately retained: killing after a
                        // successful input write could discard work. Its durable
                        // registry lets the next daemon detect ownership instead
                        // of spawning a duplicate.
                        eprintln!("usagi daemon: committing queued start failed: {error:#}");
                    } else {
                        let _ = usagi::infrastructure::agent_prompt_store::take(&worktree);
                    }
                }
                Err(error) => {
                    let _ = agent_start_store::fail(
                        &worktree,
                        request.id,
                        &owner,
                        &format!("{error:#}"),
                    );
                    eprintln!("usagi daemon: queued start failed: {error:#}");
                }
            }
            break;
        }
    }

    /// Bind the listener at `path`, removing any stale socket file first. Returns
    /// a server with no listener (IPC disabled) if binding fails, so the daemon
    /// keeps monitoring regardless.
    fn bind(path: &Path, build: String) -> Self {
        let _ = std::fs::remove_file(path);
        let listener = match std::os::unix::net::UnixListener::bind(path) {
            Ok(listener) => match listener.set_nonblocking(true) {
                Ok(()) => {
                    // Spawn requests carry the resolved workspace environment
                    // (secrets included), so only the owner may connect.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ =
                            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                    }
                    Some(listener)
                }
                Err(error) => {
                    eprintln!("usagi daemon: could not set the IPC socket non-blocking: {error}");
                    None
                }
            },
            Err(error) => {
                eprintln!("usagi daemon: could not bind the IPC socket: {error}");
                None
            }
        };
        Self {
            listener,
            clients: std::collections::HashMap::new(),
            registry: usagi::usecase::daemon_ipc::SubscriberRegistry::new(),
            build,
            next_id: 0,
            terminals: std::collections::HashMap::new(),
            terminal_registry: usagi::usecase::daemon_ipc::TerminalRegistry::new(),
            attach_table: usagi::usecase::daemon_ipc::AttachTable::new(),
            adopted_terminals: std::collections::HashSet::new(),
            session_cache: Vec::new(),
        }
    }

    fn sessions(&self) -> &[usagi::domain::daemon::SessionSnapshot] {
        &self.session_cache
    }

    fn replace_sessions(&mut self, sessions: Vec<usagi::domain::daemon::SessionSnapshot>) {
        self.session_cache = sessions;
    }

    /// Restore live terminal records left by a daemon crash. The PTY stream
    /// cannot be recovered, but the pid is adopted so stop/kill can reap it and
    /// new terminal ids do not alias old persisted panes.
    fn adopt_persisted_terminals(&mut self, dir: &Path) {
        let records = usagi::infrastructure::daemon_terminals_store::read(dir).unwrap_or_default();
        let had_persisted_records = !records.is_empty();
        let persisted: Vec<_> = records.into_iter().map(Into::into).collect();
        let (adopt, _) = usagi::usecase::daemon_ipc::plan_adopt_terminals(
            &persisted,
            &usagi::infrastructure::resource::process_alive,
        );
        for terminal in &adopt {
            self.terminal_registry.insert_known(
                terminal.terminal,
                terminal.worktree.clone(),
                terminal.pid,
            );
            self.adopted_terminals.insert(terminal.terminal);
        }
        if had_persisted_records {
            self.persist_terminals(dir);
        }
    }

    /// Whether any client is connected — the serve loop's cue to tick fast
    /// (attached clients are latency-sensitive) or slow (idle daemon).
    fn has_clients(&self) -> bool {
        !self.clients.is_empty()
    }

    /// Accept pending connections, service each client's buffered input, push
    /// new terminal output to attached clients, and reap exited terminals.
    fn poll(&mut self, dir: &Path, reap_terminals: bool) {
        self.accept_pending();
        self.service_clients(dir);
        self.stream_output();
        if reap_terminals {
            self.reap_exited(dir);
        }
    }

    /// Accept every connection waiting on the listener (non-blocking), assigning
    /// each a fresh id.
    fn accept_pending(&mut self) {
        let Some(listener) = &self.listener else {
            return;
        };
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let id = self.next_id;
                    self.next_id += 1;
                    self.clients.insert(
                        id,
                        IpcClient {
                            stream,
                            decoder: usagi::domain::daemon_ipc::FrameDecoder::new(),
                            terminal_build_verified: false,
                        },
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    /// Read and answer whatever each client has sent, dropping clients that have
    /// disconnected.
    fn service_clients(&mut self, dir: &Path) {
        let ids: Vec<u64> = self.clients.keys().copied().collect();
        for id in ids {
            if !self.service_one(id, dir) {
                self.drop_client(id);
            }
        }
    }

    /// Forget a disconnected client everywhere it is tracked.
    fn drop_client(&mut self, id: u64) {
        self.clients.remove(&id);
        self.registry.remove(id);
        self.attach_table.remove_client(id);
    }

    /// Drain one client's readable bytes, dispatch each complete message, and
    /// write its reply. Returns `false` when the client has disconnected (or
    /// errored) and should be dropped.
    fn service_one(&mut self, id: u64, dir: &Path) -> bool {
        use std::io::Read as _;
        let mut buf = [0u8; 4096];
        loop {
            let read = match self.clients.get_mut(&id) {
                Some(client) => client.stream.read(&mut buf),
                None => return false,
            };
            match read {
                // A zero-length read means the peer closed the connection.
                Ok(0) => return false,
                Ok(n) => {
                    if let Some(client) = self.clients.get_mut(&id) {
                        client.decoder.feed(&buf[..n]);
                    }
                    if !self.dispatch_frames(id, dir) {
                        return false;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(_) => return false,
            }
        }
    }

    /// Pull every complete frame the client has buffered, dispatch it, and write
    /// any reply. Returns `false` on a framing error or a failed write.
    fn dispatch_frames(&mut self, id: u64, dir: &Path) -> bool {
        loop {
            let frame = match self.clients.get_mut(&id) {
                Some(client) => client.decoder.next_frame(),
                None => return false,
            };
            let payload = match frame {
                Ok(Some(payload)) => payload,
                Ok(None) => return true,
                Err(error) => {
                    eprintln!("usagi daemon: dropping client on framing error: {error}");
                    return false;
                }
            };
            let message = match usagi::infrastructure::daemon_ipc::decode_message(&payload) {
                Ok(message) => message,
                Err(error) => {
                    eprintln!("usagi daemon: dropping client on bad message: {error:#}");
                    return false;
                }
            };
            use usagi::usecase::daemon_ipc::Action;
            let action = usagi::usecase::daemon_ipc::handle(
                message,
                id,
                &mut self.registry,
                &self.session_cache,
            );
            let build_verified = self
                .clients
                .get(&id)
                .is_some_and(|client| client.terminal_build_verified);
            if action.requires_build_handshake() && !build_verified {
                if !self.send(
                    id,
                    &usagi::domain::daemon_ipc::ServerMessage::Error {
                        message: "daemon build handshake required before terminal operations"
                            .to_string(),
                    },
                ) {
                    return false;
                }
                continue;
            }
            let alive = match action {
                Action::Hello {
                    build: client_build,
                } => {
                    if let Some(client) = self.clients.get_mut(&id) {
                        client.terminal_build_verified =
                            usagi::usecase::daemon_ipc::builds_match(&client_build, &self.build);
                    }
                    let build = self.build.clone();
                    self.send(
                        id,
                        &usagi::domain::daemon_ipc::ServerMessage::Hello { build },
                    )
                }
                Action::Reply(reply) => self.send(id, &reply),
                Action::Spawn {
                    worktree,
                    command,
                    env,
                    cols,
                    rows,
                    scrollback,
                } => {
                    let reply = self.spawn_terminal(
                        worktree,
                        command.as_deref(),
                        &env,
                        cols,
                        rows,
                        scrollback,
                    );
                    self.persist_terminals(dir);
                    self.send(id, &reply)
                }
                Action::Kill(terminal) => {
                    let reply = self.kill_terminal(dir, terminal, Some(id));
                    self.send(id, &reply)
                }
                Action::Attach { terminal, worktree } => {
                    self.attach_client(id, terminal, &worktree)
                }
                Action::Detach(terminal) => {
                    self.attach_table.detach(id, terminal);
                    true
                }
                Action::Keys {
                    terminal,
                    data,
                    request_id,
                } => {
                    let result = self.write_terminal(terminal, &data);
                    match usagi::usecase::daemon_ipc::build_input_result(
                        request_id, terminal, result,
                    ) {
                        Some(reply) => self.send(id, &reply),
                        None => true,
                    }
                }
                Action::Resize(terminal, cols, rows) => {
                    self.resize_terminal(terminal, cols, rows);
                    true
                }
                Action::Scrollback(terminal, offset) => self.push_screen_at(id, terminal, offset),
                Action::Nothing => true,
            };
            if !alive {
                return false;
            }
        }
    }

    /// Attach client `id` to `terminal`'s output feed — after checking the
    /// terminal really runs in `worktree`, so a stale persisted id cannot latch
    /// onto another worktree's terminal — and paint its current screen. Later
    /// output arrives via `stream_output`. Returns `false` when a write to the
    /// client fails.
    fn attach_client(&mut self, id: u64, terminal: u64, worktree: &Path) -> bool {
        use usagi::domain::daemon_ipc::ServerMessage;
        if !self.terminal_registry.belongs_to(terminal, worktree) {
            return self.send(
                id,
                &ServerMessage::AttachRejected {
                    terminal,
                    reason: usagi::domain::daemon_ipc::AttachFailure::Missing,
                },
            );
        }
        if !self.terminals.contains_key(&terminal) {
            return self.send(
                id,
                &ServerMessage::AttachRejected {
                    terminal,
                    reason: usagi::domain::daemon_ipc::AttachFailure::Adopted,
                },
            );
        }
        let pid = self
            .terminal_registry
            .entry(terminal)
            .map(|entry| entry.pid)
            .unwrap_or(0);
        self.attach_table.attach(id, terminal);
        self.send(id, &ServerMessage::Attached { terminal, pid })
            && self.push_screen_at(id, terminal, 0)
    }

    /// Send `terminal`'s viewport at `scrollback` to client `id`, clamped by the
    /// daemon-owned parser, and move that client's backlog cursor to the offset
    /// the snapshot corresponds to.
    fn push_screen_at(&mut self, id: u64, terminal: u64, scrollback: usize) -> bool {
        let Some(snapshot) = self
            .terminals
            .get(&terminal)
            .map(|session| session.screen_snapshot_at(scrollback))
        else {
            return true;
        };
        if !self.send(
            id,
            &usagi::domain::daemon_ipc::ServerMessage::Screen {
                terminal,
                contents: snapshot.contents,
                scrollback: snapshot.scrollback,
            },
        ) {
            return false;
        }
        self.attach_table
            .set_cursor(id, terminal, snapshot.backlog_offset);
        self.attach_table.set_viewport(
            id,
            terminal,
            snapshot.scrollback,
            snapshot.primary_high_water,
        );
        true
    }

    /// Write input bytes to `terminal`. The returned result is correlated back
    /// to acknowledged callers; fire-and-forget interactive input simply logs
    /// the same error and continues serving the connection.
    fn write_terminal(&mut self, terminal: u64, data: &[u8]) -> Result<(), String> {
        let Some(session) = self.terminals.get_mut(&terminal) else {
            let message = format!("no daemon terminal {terminal} is available for input");
            eprintln!("usagi daemon: {message}");
            return Err(message);
        };
        session.write(data).map_err(|error| {
            let message = format!("writing to daemon terminal {terminal}: {error:#}");
            eprintln!("usagi daemon: {message}");
            message
        })
    }

    /// Resize `terminal`, if it is running.
    fn resize_terminal(&mut self, terminal: u64, cols: u16, rows: u16) {
        if let Some(session) = self.terminals.get_mut(&terminal) {
            session.resize(rows, cols);
        }
    }

    /// Push each attached client the output bytes it has not seen yet, as raw
    /// deltas from the terminal's backlog — or a full screen snapshot when the
    /// client fell so far behind that its bytes were evicted. Terminals with no
    /// attached client are skipped, so an unobserved terminal costs nothing.
    fn stream_output(&mut self) {
        for terminal in self.attach_table.terminals() {
            self.stream_terminal_output(terminal);
        }
    }

    /// The per-terminal step of [`stream_output`](Self::stream_output).
    fn stream_terminal_output(&mut self, terminal: u64) {
        use usagi::usecase::daemon_ipc::ScreenUpdate;
        let clients = self.attach_table.clients_for(terminal);
        if clients.is_empty() {
            return;
        }
        let Some(backlog) = self
            .terminals
            .get(&terminal)
            .and_then(|session| session.output_backlog())
        else {
            return;
        };
        let primary_high_water = self
            .terminals
            .get(&terminal)
            .map(|session| session.primary_scrollback_high_water())
            .unwrap_or(0);
        let (plan, end) = {
            let backlog = backlog
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                usagi::usecase::daemon_ipc::plan_screen_updates(
                    &backlog,
                    &clients,
                    primary_high_water,
                ),
                backlog.end(),
            )
        };
        for (client, update) in plan {
            let delivered = match update {
                ScreenUpdate::Output(data) => {
                    if self.send(
                        client,
                        &usagi::domain::daemon_ipc::ServerMessage::Output { terminal, data },
                    ) {
                        self.attach_table.set_cursor(client, terminal, end);
                        self.attach_table
                            .set_viewport(client, terminal, 0, primary_high_water);
                        true
                    } else {
                        false
                    }
                }
                // The snapshot re-reads the live screen (and its own offset), so
                // it also covers anything appended since the plan was made.
                ScreenUpdate::Snapshot { offset } => self.push_screen_at(client, terminal, offset),
            };
            if !delivered {
                self.drop_client(client);
            }
        }
    }

    /// Notify attachers of — and forget — every terminal whose process has
    /// exited. The final output was flushed by `stream_output` (the reader
    /// appends bytes before it flips liveness, and this runs after the flush in
    /// the same poll), so attachers see everything the terminal printed before
    /// the `Exited`. Dropping the [`PtySession`] reaps the child and records an
    /// abnormal exit to the error log.
    ///
    /// [`PtySession`]: usagi::infrastructure::pty::PtySession
    fn reap_exited(&mut self, dir: &Path) {
        for terminal in self.terminal_registry.ids() {
            let exited = self
                .terminals
                .get_mut(&terminal)
                .is_some_and(|session| !session.is_alive() || session.poll_exit());
            if !exited {
                continue;
            }
            // One last delta flush so no tail output is lost to the removal.
            self.stream_terminal_output(terminal);
            self.remove_terminal(dir, terminal, None);
        }
    }

    /// Forget `terminal` everywhere and tell its attachers (minus `skip`, a
    /// killer that gets a `Killed` reply instead) that it is gone. Dropping the
    /// removed [`PtySession`] kills / reaps its process group.
    ///
    /// [`PtySession`]: usagi::infrastructure::pty::PtySession
    fn remove_terminal(&mut self, dir: &Path, terminal: u64, skip: Option<u64>) {
        self.terminals.remove(&terminal);
        if self.adopted_terminals.remove(&terminal) {
            if let Some(entry) = self.terminal_registry.entry(terminal) {
                kill_process_group(entry.pid);
            }
        }
        self.terminal_registry.remove(terminal);
        self.persist_terminals(dir);
        for client in self.attach_table.remove_terminal(terminal) {
            if Some(client) == skip {
                continue;
            }
            if !self.send(
                client,
                &usagi::domain::daemon_ipc::ServerMessage::Exited { terminal },
            ) {
                self.drop_client(client);
            }
        }
    }

    /// Spawn a new daemon-owned terminal in `worktree` and report its id and
    /// pid. The [`PtySession`] is stored in `self.terminals`, so it lives on
    /// independently of the requesting client. `command` (an agent launch line)
    /// runs as a shell argument when given; `env` is injected into the child.
    ///
    /// [`PtySession`]: usagi::infrastructure::pty::PtySession
    fn spawn_terminal(
        &mut self,
        worktree: std::path::PathBuf,
        command: Option<&str>,
        env: &std::collections::BTreeMap<String, String>,
        cols: u16,
        rows: u16,
        scrollback: usize,
    ) -> usagi::domain::daemon_ipc::ServerMessage {
        use usagi::domain::daemon_ipc::ServerMessage;
        match usagi::infrastructure::pty::PtySession::spawn_streamed(
            &worktree,
            rows,
            cols,
            command,
            scrollback,
            env,
            DAEMON_OUTPUT_BACKLOG_BYTES,
        ) {
            Ok(session) => {
                let pid = session.process_id().unwrap_or(0);
                let terminal = self.terminal_registry.allocate(worktree.clone(), pid);
                self.terminals.insert(terminal, session);
                ServerMessage::Spawned {
                    terminal,
                    worktree,
                    pid,
                }
            }
            Err(error) => ServerMessage::Error {
                message: format!(
                    "failed to spawn terminal for {}: {error:#}",
                    worktree.display()
                ),
            },
        }
    }

    /// Kill the daemon-owned terminal `terminal` (a no-op reply when none is
    /// running under that id). `killer` is answered with `Killed`; other
    /// attachers learn of the death via `Exited`.
    fn kill_terminal(
        &mut self,
        dir: &Path,
        terminal: u64,
        killer: Option<u64>,
    ) -> usagi::domain::daemon_ipc::ServerMessage {
        self.remove_terminal(dir, terminal, killer);
        usagi::domain::daemon_ipc::ServerMessage::Killed { terminal }
    }

    /// Push the current snapshot to every subscribed client, dropping any whose
    /// write fails.
    fn broadcast_sessions(&mut self) {
        let message = usagi::domain::daemon_ipc::ServerMessage::Sessions {
            sessions: self.session_cache.clone(),
        };
        for id in self.registry.subscribers() {
            if !self.send(id, &message) {
                self.drop_client(id);
            }
        }
    }

    fn notify_session_transitions(
        &mut self,
        previous: &[usagi::domain::daemon::SessionSnapshot],
        current: &[usagi::domain::daemon::SessionSnapshot],
    ) {
        let enabled = usagi::infrastructure::storage::Storage::open_default()
            .and_then(|storage| storage.load_settings())
            .map(|settings| settings.notifications_enabled)
            .unwrap_or(true);
        if !enabled {
            return;
        }
        let previous: std::collections::HashMap<_, _> = previous
            .iter()
            .map(|session| {
                (
                    (session.workspace.clone(), session.name.clone()),
                    session.activity,
                )
            })
            .collect();
        for session in current {
            let attached = session.worktree.as_deref().is_some_and(|worktree| {
                self.terminal_registry.ids().into_iter().any(|terminal| {
                    self.terminal_registry.belongs_to(terminal, worktree)
                        && self.attach_table.has_terminal(terminal)
                })
            });
            let key = (session.workspace.clone(), session.name.clone());
            if let Some(kind) = usagi::usecase::daemon_ipc::should_notify_activity(
                previous.get(&key).copied().flatten(),
                session.activity,
                attached,
            ) {
                notify(&session.name, kind);
            }
        }
    }

    fn persist_terminals(&self, dir: &Path) {
        let records: Vec<_> = self
            .terminal_registry
            .ids()
            .into_iter()
            .filter_map(|terminal| {
                self.terminal_registry.entry(terminal).map(|entry| {
                    usagi::infrastructure::daemon_terminals_store::DaemonTerminalRecord {
                        terminal,
                        worktree: entry.worktree.clone(),
                        pid: entry.pid,
                        adopted: self.adopted_terminals.contains(&terminal),
                    }
                })
            })
            .collect();
        if records.is_empty() && !dir.exists() {
            return;
        }
        if let Err(error) = usagi::infrastructure::daemon_terminals_store::write(dir, &records) {
            eprintln!("usagi daemon: failed to persist terminals: {error:#}");
        }
    }

    /// Encode and write one message to client `id`. Returns `false` when the
    /// client is gone or the write fails.
    fn send(&mut self, id: u64, message: &usagi::domain::daemon_ipc::ServerMessage) -> bool {
        use std::io::Write as _;
        let Ok(bytes) = usagi::infrastructure::daemon_ipc::encode_message(message) else {
            return false;
        };
        match self.clients.get_mut(&id) {
            Some(client) => client.stream.write_all(&bytes).is_ok(),
            None => false,
        }
    }

    /// Kill every daemon-owned terminal and remove the socket file as the daemon
    /// shuts down, so a deliberate `daemon stop` does not leak orphaned shells.
    /// (Terminals survive a *client* disconnect — that is the point — but not the
    /// daemon that owns them exiting.)
    fn shutdown(&mut self, dir: &Path, path: &Path) {
        // Dropping each session signals its process group; clearing the map does
        // that for all of them.
        for terminal in self.adopted_terminals.drain() {
            if let Some(entry) = self.terminal_registry.entry(terminal) {
                kill_process_group(entry.pid);
            }
        }
        self.terminals.clear();
        self.terminal_registry = usagi::usecase::daemon_ipc::TerminalRegistry::new();
        self.persist_terminals(dir);
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(not(unix))]
struct DaemonIpcServer;

#[cfg(not(unix))]
impl DaemonIpcServer {
    fn bind(_path: &Path, _build: String) -> Self {
        Self
    }

    fn has_clients(&self) -> bool {
        false
    }

    fn adopt_persisted_terminals(&mut self, _dir: &Path) {}

    fn poll(&mut self, _dir: &Path, _reap_terminals: bool) {}

    fn consume_queued_start(&mut self, _dir: &Path) {}

    fn notify_session_transitions(
        &self,
        _previous_sessions: &[usagi::domain::daemon::SessionSnapshot],
        _current_sessions: &[usagi::domain::daemon::SessionSnapshot],
    ) {
    }

    fn broadcast_sessions(&mut self) {}

    fn sessions(&self) -> &[usagi::domain::daemon::SessionSnapshot] {
        &[]
    }

    fn replace_sessions(&mut self, _sessions: Vec<usagi::domain::daemon::SessionSnapshot>) {}

    fn shutdown(&mut self, _dir: &Path, _path: &Path) {}
}

fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn notify(label: &str, kind: usagi::usecase::daemon_ipc::ActivityNoticeKind) {
    let message = match kind {
        usagi::usecase::daemon_ipc::ActivityNoticeKind::Waiting => {
            format!("{label} が入力待ちです")
        }
        usagi::usecase::daemon_ipc::ActivityNoticeKind::Done => {
            format!("{label} が完了しました")
        }
    };
    let _ = notify_rust::Notification::new()
        .summary("usagi")
        .body(&format!("(\\_/)\n(='.'=)\n{message}"))
        .show();
}

/// Bytes of raw output retained per daemon terminal for streaming exact live
/// viewport deltas to attached clients. A client that falls further behind than
/// this is resynchronised with a bounded daemon screen snapshot, so the cap only
/// bounds memory, never correctness. 256 KiB absorbs a solid burst of agent
/// output between two IPC ticks with plenty of margin.
#[cfg(unix)]
const DAEMON_OUTPUT_BACKLOG_BYTES: usize = 256 * 1024;

/// Gather the daemon's view of every monitored session from the real stores.
/// The composition-root adapter for [`usagi::usecase::daemon::gather`]: it wires
/// the workspace list, per-workspace session load, and per-worktree phase read to
/// their live implementations. Coverage-excluded store IO like the rest of this
/// file; the aggregation it drives is unit-tested in the usecase.
fn daemon_gather() -> Vec<usagi::domain::daemon::SessionSnapshot> {
    usagi::usecase::daemon::gather(
        &daemon_list_roots,
        &daemon_load_sessions,
        &usagi::infrastructure::agent_state_store::read,
    )
}

/// The roots of the registered workspaces, or an empty list when they cannot be
/// read (the daemon simply monitors nothing rather than failing the tick).
fn daemon_list_roots() -> Vec<std::path::PathBuf> {
    match usagi::infrastructure::storage::Storage::open_default().and_then(|s| s.load_workspaces())
    {
        Ok(workspaces) => workspaces.into_iter().map(|w| w.path).collect(),
        Err(_) => Vec::new(),
    }
}

/// Each session in the workspace rooted at `root`, as its name and worktree
/// paths. An unreadable state file yields no sessions for that workspace.
fn daemon_load_sessions(root: &Path) -> Vec<usagi::usecase::daemon::SessionWorktrees> {
    match usagi::infrastructure::workspace_store::WorkspaceStore::new(root).load() {
        Ok(Some(state)) => state
            .sessions
            .into_iter()
            .map(|session| {
                (
                    session.name,
                    session.worktrees.into_iter().map(|w| w.path).collect(),
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Spawn `usagi daemon serve` detached in the background, so the daemon outlives
/// the `usagi daemon start` invocation (and the TUI). Its stdout/stderr are
/// appended to `<dir>/serve.log`. Composition-root IO, excluded from coverage
/// like [`spawn_detached`].
fn spawn_daemon(dir: &Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::fs::OpenOptions;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("locating the usagi executable")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating daemon directory {}", dir.display()))?;
    let log_path = dir.join("serve.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening log file {}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("opening log file {}", log_path.display()))?;

    let mut builder = Command::new(exe);
    builder
        .arg("daemon")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    // Detach from usagi's process group so the daemon keeps running after the
    // launching `usagi daemon start` (and the TUI) exits.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        builder.process_group(0);
    }
    let child = builder.spawn().context("spawning the usagi daemon")?;
    #[cfg(unix)]
    {
        let mut child = child;
        // `start` is the TUI's daemon autospawn barrier. Do not return in the
        // fork→register→bind gap: an immediate queued autostart would otherwise
        // see no record/socket and fall back to a TUI-owned PTY.
        let socket = usagi::infrastructure::daemon_ipc::socket_path(dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let registered = usagi::infrastructure::daemon_store::read(dir)
                .ok()
                .flatten()
                .is_some_and(|record| usagi::infrastructure::resource::process_alive(record.pid));
            if registered && socket.exists() {
                break;
            }
            if let Some(status) = child
                .try_wait()
                .context("checking the spawned usagi daemon")?
            {
                anyhow::bail!("usagi daemon exited before becoming ready: {status}");
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out waiting for the usagi daemon socket {}",
                    socket.display()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
    #[cfg(not(unix))]
    drop(child);
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
