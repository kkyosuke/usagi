//! `usagi llm-mcp`: run the local LLM MCP server over stdio.
//!
//! A thin transport wrapper around [`crate::presentation::mcp::llm::LlmMcpServer`]
//! (which holds the unit-tested protocol logic). The shared read/write loop
//! ([`crate::presentation::mcp::serve`]) does the framing; this file only binds
//! the real stdio handles in [`run`] and provides the [`OllamaBackend`] that
//! shells out to the `ollama` CLI. Neither can be unit-tested, so this file is
//! excluded from coverage (see `scripts/coverage.sh`).

use std::io::{self, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::presentation::mcp::llm::{LlmBackend, LlmMcpServer};
use crate::usecase::doctor::SystemRunner;
use crate::usecase::local_llm;

/// The longest a single `ollama run` may take before it is killed and the call
/// fails. Local generation can be slow, so the budget is generous; its job is to
/// stop a wedged model or unreachable server from blocking the MCP call (and the
/// agent waiting on it) forever.
const ASK_TIMEOUT: Duration = Duration::from_secs(120);
/// How often the wait loop re-polls the child while it runs.
const ASK_POLL: Duration = Duration::from_millis(50);
/// Largest prompt (system + user) sent to `ollama`, so a pathological input
/// cannot exhaust memory before the model even runs.
const MAX_INPUT_BYTES: usize = 256 * 1024;
/// Largest model output captured; anything beyond this is truncated rather than
/// buffered without bound.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// How much of `ollama`'s stderr is echoed back in an error, so a noisy or
/// sensitive diagnostic stream is not relayed to the agent in full.
const MAX_STDERR_BYTES: usize = 4 * 1024;

/// The production [`LlmBackend`]: each completion runs `ollama run <model>`,
/// feeding the prompt on stdin and returning the captured stdout.
struct OllamaBackend {
    model: String,
}

/// Read up to `cap` bytes from `reader`, draining (and discarding) the rest so
/// the child never blocks on a full pipe. Returns the captured bytes and whether
/// the stream was longer than `cap`.
fn read_capped(reader: &mut impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    // Read one past the cap to detect truncation, then drain the remainder.
    let _ = reader.take(cap as u64 + 1).read_to_end(&mut buf);
    let truncated = buf.len() > cap;
    if truncated {
        buf.truncate(cap);
        let _ = io::copy(reader, &mut io::sink());
    }
    (buf, truncated)
}

impl LlmBackend for OllamaBackend {
    fn ask(&self, prompt: &str, system: Option<&str>) -> Result<String, String> {
        // A Homebrew-installed `ollama` runs no server until one is started, and
        // `run` does not auto-start it — so make sure the server is up first,
        // otherwise every call fails with "could not connect to ollama server".
        local_llm::ensure_server_started(&SystemRunner)?;

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

        let mut child = Command::new("ollama")
            .arg("run")
            .arg(&self.model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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

        let status = wait_with_timeout(&mut child, ASK_TIMEOUT);
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

/// Wait for `child` up to `timeout`, returning its exit status, or `None` after
/// killing it when the timeout elapses first.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(ASK_POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Entry point for `usagi llm-mcp`: serve the local LLM `ask` tool for `model`
/// over stdio until the client closes the input stream.
pub fn run(model: String) -> Result<()> {
    let backend = Box::new(OllamaBackend {
        model: model.clone(),
    });
    let server = LlmMcpServer::new(backend, model);
    let stdin = io::stdin();
    let stdout = io::stdout();
    crate::presentation::mcp::serve(&server, stdin.lock(), stdout.lock())?;
    Ok(())
}
