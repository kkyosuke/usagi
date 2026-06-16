//! `usagi mcp`: run the issue MCP server over stdio.
//!
//! This is a thin transport wrapper that reads newline-delimited JSON-RPC
//! messages from stdin and writes replies to stdout, delegating all protocol
//! and tool logic to [`crate::presentation::mcp::issue::McpServer`] (which is unit
//! tested). The blocking stdin loop itself is not unit tested — like `hop`'s
//! TUI entry point it is excluded from coverage.

use std::env;
use std::io::{self, BufRead, Write};

use anyhow::Result;

use crate::presentation::mcp::issue::McpServer;
use crate::usecase::session;

/// Entry point for `usagi mcp`: serve issue tools for the current repository
/// over stdio until the client closes the input stream.
///
/// The server is launched from the agent's working directory, which may sit
/// inside a session tree (`<workspace>/.usagi/sessions/<name>/`). Issues belong
/// to the workspace, so we resolve back to its root rather than writing into a
/// throwaway session copy (see [`session::workspace_root`]).
pub fn run() -> Result<()> {
    let server = McpServer::new(session::workspace_root(&env::current_dir()?));
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
