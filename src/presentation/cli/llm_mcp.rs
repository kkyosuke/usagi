//! `usagi llm-mcp`: run the local LLM MCP server over stdio.
//!
//! A thin transport wrapper around [`crate::presentation::mcp_llm::LlmMcpServer`]
//! (which holds the unit-tested protocol logic). This file owns the two pieces
//! that cannot be unit-tested and so are excluded from coverage: the blocking
//! stdin loop and the [`OllamaBackend`] that shells out to the `ollama` CLI.

use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};

use anyhow::Result;

use crate::presentation::mcp_llm::{LlmBackend, LlmMcpServer};
use crate::usecase::doctor::SystemRunner;
use crate::usecase::local_llm;

/// The production [`LlmBackend`]: each completion runs `ollama run <model>`,
/// feeding the prompt on stdin and returning the captured stdout.
struct OllamaBackend {
    model: String,
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

        let mut child = Command::new("ollama")
            .arg("run")
            .arg(&self.model)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start ollama: {e}"))?;

        child
            .stdin
            .take()
            .ok_or_else(|| "failed to open ollama stdin".to_string())?
            .write_all(full.as_bytes())
            .map_err(|e| format!("failed to write prompt to ollama: {e}"))?;

        let output = child
            .wait_with_output()
            .map_err(|e| format!("failed to read ollama output: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "ollama exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// Entry point for `usagi llm-mcp`: serve the local LLM `ask` tool for `model`
/// over stdio until the client closes the input stream.
pub fn run(model: String) -> Result<()> {
    let server = LlmMcpServer::new(
        Box::new(OllamaBackend {
            model: model.clone(),
        }),
        model,
    );
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(&line) {
            writeln!(out, "{response}")?;
            out.flush()?;
        }
    }
    Ok(())
}
