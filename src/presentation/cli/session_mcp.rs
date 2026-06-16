//! `usagi session-mcp`: run the session orchestration MCP server over stdio.
//!
//! A thin transport wrapper around
//! [`crate::presentation::mcp::session::SessionMcpServer`] (which holds the
//! unit-tested protocol logic). This file binds the real stdio handles, derives
//! the workspace root from the launch directory, and provides the production
//! [`AgentBackend`] that shells out to the configured agent CLI in headless
//! print mode. Like `mcp`'s and `llm-mcp`'s entry points it is excluded from
//! coverage (see `scripts/coverage.sh`).

use std::env;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;

use crate::domain::settings::AgentCli;
use crate::infrastructure::storage::Storage;
use crate::presentation::mcp::session::{AgentBackend, SessionMcpServer};
use crate::usecase::{session, settings};

/// The production [`AgentBackend`]: each prompt runs the configured agent CLI in
/// headless print mode (`<agent> -p <prompt>`) inside the session's worktree,
/// returning the captured stdout. No MCP servers are wired into this child, so a
/// delegated session cannot recursively spawn further sessions.
struct CliAgentBackend {
    cli: AgentCli,
}

impl AgentBackend for CliAgentBackend {
    fn prompt(&self, worktree: &Path, prompt: &str) -> Result<String, String> {
        let program = self.cli.command();
        let output = Command::new(program)
            .arg("-p")
            .arg(prompt)
            .current_dir(worktree)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("failed to start {program}: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "{program} exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

/// Entry point for `usagi session-mcp`: serve the session tools for the
/// workspace that owns the launch directory over stdio until the client closes
/// the input stream.
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
    let server = SessionMcpServer::new(workspace_root, backend);

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
