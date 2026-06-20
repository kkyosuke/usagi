//! `usagi mcp`: run the unified `usagi` MCP server over stdio.
//!
//! This is a thin transport wrapper that reads newline-delimited JSON-RPC
//! messages from stdin and writes replies to stdout, delegating all protocol
//! and tool logic to [`crate::presentation::mcp::usagi::UsagiMcpServer`] (which
//! composes the unit-tested issue/memory and session servers). The blocking
//! stdin loop and the production [`AgentBackend`] that shells out to the agent
//! CLI are not unit tested — like `hop`'s TUI entry point they are excluded from
//! coverage.

use std::env;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::domain::settings::AgentCli;
use crate::infrastructure::storage::Storage;
use crate::presentation::mcp::session::AgentBackend;
use crate::presentation::mcp::usagi::UsagiMcpServer;
use crate::usecase::{session, settings};

/// Default ceiling (seconds) for a single `session_prompt` agent run before it is
/// terminated. Generous enough for a real task, but bounded so a hung agent can
/// never wedge the server. Overridable via `USAGI_AGENT_TIMEOUT_SECS`.
const DEFAULT_AGENT_TIMEOUT_SECS: u64 = 600;

/// How often we poll the child for completion while waiting out the timeout.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Grace period for finishing the drain of a child's stdout/stderr *after* it has
/// already exited. Output normally reaches EOF at once, but a lingering grandchild
/// that inherited the pipe could otherwise keep a reader blocked forever (and with
/// it the single-threaded server); past this we proceed with the bytes captured so
/// far rather than waiting indefinitely.
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// The production [`AgentBackend`]: each `session_prompt` runs the configured
/// agent CLI in headless print mode (`<agent> -p <prompt>`) inside the session's
/// worktree, returning the captured stdout. No MCP servers are wired into this
/// child, so a delegated session cannot recursively spawn further sessions.
struct CliAgentBackend {
    cli: AgentCli,
}

impl AgentBackend for CliAgentBackend {
    fn prompt(&self, worktree: &Path, prompt: &str) -> Result<String, String> {
        let program = self.cli.command();
        let mut child = Command::new(program)
            .arg("-p")
            .arg(prompt)
            .current_dir(worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start {program}: {e}"))?;

        // Stream stdout/stderr off their own threads so a chatty agent can never
        // deadlock by filling a pipe buffer while we poll for completion below.
        let out_rx = spawn_drain(child.stdout.take().expect("stdout is piped"));
        let err_rx = spawn_drain(child.stderr.take().expect("stderr is piped"));

        // Wait for the agent, but kill it if it overruns the timeout: the MCP
        // server is single-threaded over stdio, so an unbounded run would block
        // every later request and surface to the client as an opaque
        // "internal error". A clear, terminating error is far more useful.
        let status = match agent_timeout() {
            None => child
                .wait()
                .map_err(|e| format!("failed to wait for {program}: {e}"))?,
            Some(limit) => wait_with_timeout(&mut child, program, limit)?,
        };

        // The child has exited; gather its output, but only for a bounded grace so
        // a grandchild still holding the pipe open cannot wedge the server here.
        let stdout = collect(&out_rx, OUTPUT_DRAIN_GRACE);
        let stderr = collect(&err_rx, OUTPUT_DRAIN_GRACE);
        if !status.success() {
            return Err(format!(
                "{program} exited with {status}: {}",
                String::from_utf8_lossy(&stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    }
}

/// Poll the child to completion, killing it if it overruns `limit`.
fn wait_with_timeout(
    child: &mut Child,
    program: &str,
    limit: Duration,
) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{program} did not respond within {}s and was terminated; \
                     raise USAGI_AGENT_TIMEOUT_SECS (or set it to 0 to wait \
                     indefinitely)",
                    limit.as_secs()
                ));
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(e) => return Err(format!("failed to wait for {program}: {e}")),
        }
    }
}

/// Spawn a thread that reads `pipe` to EOF, forwarding chunks over a channel.
/// Streaming (rather than `read_to_end` + `join`) lets the caller bound how long
/// it waits for output, so a reader can never block the prompt permanently.
fn spawn_drain(mut pipe: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

/// Collect everything a [`spawn_drain`] thread sends until the pipe closes (the
/// channel disconnects) or `grace` elapses, returning the bytes captured so far.
/// A normal child reaches EOF at once, so the buffered chunks drain immediately
/// and this returns promptly; `grace` only bounds the pathological case where the
/// stream stays open after the child exits.
fn collect(rx: &Receiver<Vec<u8>>, grace: Duration) -> Vec<u8> {
    let deadline = Instant::now() + grace;
    let mut out = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }
    out
}

/// The `session_prompt` timeout, read from `USAGI_AGENT_TIMEOUT_SECS` (an
/// unset or unparseable value falls back to [`DEFAULT_AGENT_TIMEOUT_SECS`]).
/// `0` disables the timeout and waits indefinitely, restoring the previous
/// unbounded behaviour.
fn agent_timeout() -> Option<Duration> {
    let secs = env::var("USAGI_AGENT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_AGENT_TIMEOUT_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Entry point for `usagi mcp`: serve the unified `usagi` tools (issue, memory,
/// and session) for the current repository over stdio until the client closes
/// the input stream.
///
/// The server is launched from the agent's working directory, which may sit
/// inside a session tree (`<workspace>/.usagi/sessions/<name>/`). Issues,
/// memories, and sessions all belong to the workspace, so we resolve back to its
/// root rather than writing into a throwaway session copy (see
/// [`session::workspace_root`]).
pub fn run() -> Result<()> {
    let workspace_root = session::workspace_root(&env::current_dir()?);

    // The agent CLI used to fulfil `session_prompt`, resolved from the effective
    // settings (project-local over the global default, which is Claude). Any
    // failure to read settings falls back to the default agent.
    let cli = Storage::open_default()
        .and_then(|storage| settings::effective(&storage, &workspace_root))
        .map(|settings| settings.agent_cli)
        .unwrap_or_default();

    let backend = Box::new(CliAgentBackend { cli });
    let server = UsagiMcpServer::new(workspace_root, backend);

    let stdin = io::stdin();
    let stdout = io::stdout();
    crate::presentation::mcp::serve(&server, stdin.lock(), stdout.lock())?;
    Ok(())
}
