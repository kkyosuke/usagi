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
use std::io;
use std::path::Path;

use anyhow::Result;

use crate::infrastructure::agent_prompt_store;
use crate::presentation::mcp::session::AgentBackend;
use crate::presentation::mcp::usagi::UsagiMcpServer;
use crate::usecase::session;

/// The production [`AgentBackend`]: `session_prompt` *queues* the prompt for the
/// target session's worktree rather than running an agent itself. The `usagi mcp`
/// process cannot reach into a running TUI to drive a pane, so it leaves the
/// prompt in [`agent_prompt_store`] and the home screen delivers it the next time
/// it freshly launches that session's agent pane — the agent then opens in the
/// session's right-hand pane already working on the prompt (see
/// [`crate::presentation::tui::home`]). This keeps a delegated prompt visible and
/// interactive in the session it belongs to, instead of running detached.
struct CliAgentBackend;

impl AgentBackend for CliAgentBackend {
    fn prompt(&self, worktree: &Path, prompt: &str) -> Result<String, String> {
        agent_prompt_store::set(worktree, prompt).map_err(|e| e.to_string())?;
        Ok(
            "Queued the prompt for this session's agent. It is delivered as the agent's \
            opening message the next time the session's agent pane is launched from the \
            usagi home screen (focus the session, then run `agent`)."
                .to_string(),
        )
    }
}

/// Entry point for `usagi mcp`: serve the unified `usagi` tools (issue, memory,
/// and session) for the current repository over stdio until the client closes
/// the input stream.
///
/// The server is launched from the agent's working directory, which may sit
/// inside a session tree (`<workspace>/.usagi/sessions/<name>/`). The two tool
/// families resolve their root differently:
///
/// - **Issues and memories** operate on the *current worktree* (`current_dir`),
///   so a session agent's edits land on its own branch and reach `main` through
///   the session's PR instead of dirtying the workspace checkout. Issue
///   numbering still scans every worktree to stay collision-free (see
///   [`crate::usecase::issue`]).
/// - **Session orchestration** operates on the whole *workspace*, so we resolve
///   back to its root (see [`session::workspace_root`]).
pub fn run() -> Result<()> {
    let worktree = env::current_dir()?;
    let workspace_root = session::workspace_root(&worktree);

    let backend = Box::new(CliAgentBackend);
    let server = UsagiMcpServer::new(worktree, workspace_root, backend);

    let stdin = io::stdin();
    let stdout = io::stdout();
    crate::presentation::mcp::serve(&server, stdin.lock(), stdout.lock())?;
    Ok(())
}
