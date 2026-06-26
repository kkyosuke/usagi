use std::path::Path;

use clap::{Parser, Subcommand};

use usagi::presentation::mcp::child_io::{read_capped, wait_with_timeout, WaitableChild};
use usagi::presentation::mcp::llm::LlmBackend;
use usagi::presentation::mcp::session::AgentBackend;
use usagi::usecase::session;

/// The production [`AgentBackend`] for `usagi mcp`, wired in here at the
/// composition root so the mcp transport itself stays free of process / store IO
/// and unit-testable.
///
/// `prompt` *queues* the prompt for the target session's worktree rather than
/// running an agent itself: the `usagi mcp` process cannot reach into a running
/// TUI to drive a pane, so it leaves the prompt in `agent_prompt_store` and the
/// home screen delivers it the next time it freshly launches that session's
/// agent pane.
///
/// `remove` resolves the workspace's effective agent CLI (so the removed
/// session's persisted conversation is discarded with the right adapter) and
/// delegates to [`session::remove`].
struct CliAgentBackend;

impl AgentBackend for CliAgentBackend {
    fn prompt(&self, worktree: &Path, prompt: &str) -> Result<String, String> {
        usagi::infrastructure::agent_prompt_store::set(worktree, prompt)
            .map_err(|e| e.to_string())?;
        Ok(
            "Queued the prompt for this session's agent. It is delivered as the agent's \
            opening message the next time the session's agent pane is launched from the \
            usagi home screen (focus the session, then run `agent`)."
                .to_string(),
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
        use std::io::Write as _;

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

        // Feed the prompt, then drop stdin so ollama sees EOF and starts.
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| "failed to open ollama stdin".to_string())?;
            stdin
                .write_all(full.as_bytes())
                .map_err(|e| format!("failed to write prompt to ollama: {e}"))?;
        }

        // Drain stdout/stderr on threads (capped) so a large output cannot
        // deadlock on a full pipe, while the main thread bounds the wait.
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
        let (stdout, _) = out_reader.join().unwrap_or_default();
        let (stderr, stderr_truncated) = err_reader.join().unwrap_or_default();

        let Some(status) = status else {
            return Err(format!(
                "ollama did not finish within {ASK_TIMEOUT:?} and was terminated"
            ));
        };
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
        /// Override the configured agent CLI for this run (claude / codex / codex-fugu / gemini)
        #[arg(long, value_name = "NAME")]
        agent: Option<String>,
    },
    /// Show usagi's configuration (or edit it with --edit)
    Config {
        /// Open the configuration file in $EDITOR and validate it on save
        #[arg(long)]
        edit: bool,
    },
    /// Check that required tools are installed
    Doctor {
        /// Try to install missing tools (or print manual steps)
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
}

fn main() -> anyhow::Result<()> {
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
        Commands::Config { edit } => usagi::presentation::cli::config::run(edit),
        Commands::Doctor { fix } => usagi::presentation::cli::doctor::run(fix),
        Commands::Feature => usagi::presentation::cli::feature::run(),
        Commands::GuardWorkspace => {
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            usagi::presentation::cli::guard_workspace::run(stdin.lock(), stdout.lock())
        }
        Commands::Hop => usagi::presentation::cli::hop::run(usagi::presentation::tui::app::run),
        Commands::Icon { view } => usagi::presentation::cli::icon::run(view),
        Commands::Init { git } => usagi::presentation::cli::init::run(git),
        Commands::Issue { command } => usagi::presentation::cli::issue::run(command),
        Commands::Memory { command } => usagi::presentation::cli::memory::run(command),
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
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            usagi::presentation::cli::mcp::run(
                Box::new(CliAgentBackend),
                stdin.lock(),
                stdout.lock(),
            )
        }
        Commands::Run { n } => usagi::presentation::cli::run::run(n),
        Commands::Status => usagi::presentation::cli::status::run(),
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
        Commands::AgentPhase { .. } => Some("agent-phase"),
        Commands::Clean { .. } => Some("clean"),
        Commands::Config { .. } => Some("config"),
        Commands::Doctor { .. } => Some("doctor"),
        Commands::Feature => Some("feature"),
        Commands::GuardWorkspace => Some("guard-workspace"),
        Commands::Hop => Some("hop"),
        Commands::Icon { .. } => Some("icon"),
        Commands::Init { .. } => Some("init"),
        Commands::Issue { .. } => Some("issue"),
        Commands::Memory { .. } => Some("memory"),
        Commands::Run { .. } => Some("run"),
        Commands::Status => Some("status"),
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
