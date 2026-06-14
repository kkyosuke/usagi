//! `usagi mcp`: run the issue MCP server over stdio.
//!
//! This is a thin transport wrapper that reads newline-delimited JSON-RPC
//! messages from stdin and writes replies to stdout, delegating all protocol
//! and tool logic to [`crate::presentation::mcp::McpServer`] (which is unit
//! tested). The blocking stdin loop itself is not unit tested — like `hop`'s
//! TUI entry point it is excluded from coverage.

use std::env;
use std::io::{self, BufRead, Write};

use anyhow::Result;

use crate::presentation::mcp::McpServer;

/// Entry point for `usagi mcp`: serve issue tools for the current repository
/// over stdio until the client closes the input stream.
pub fn run() -> Result<()> {
    let server = McpServer::new(env::current_dir()?);
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
