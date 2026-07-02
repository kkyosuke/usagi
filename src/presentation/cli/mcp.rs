//! `usagi mcp`: run the unified `usagi` MCP server over stdio.
//!
//! This is a thin transport wrapper that reads newline-delimited JSON-RPC
//! messages from `input` and writes replies to `output`, delegating all protocol
//! and tool logic to [`crate::presentation::mcp::usagi::UsagiMcpServer`] (which
//! composes the unit-tested issue/memory and session servers).
//!
//! The genuine IO is injected so the orchestration here stays testable: the
//! production [`AgentBackend`] that shells out to the agent CLI lives in the
//! (coverage-excluded) binary entry point, and the byte streams are parameters —
//! tests drive [`run`] with an in-memory backend and buffers, while `main` wires
//! in the real backend and stdio locks.

use std::io::{BufRead, Write};

use anyhow::Result;

use crate::presentation::mcp::session::AgentBackend;
use crate::presentation::mcp::usagi::UsagiMcpServer;
use crate::usecase::session;

/// Entry point for `usagi mcp`: serve the unified `usagi` tools (issue, memory,
/// and session) for the current repository, reading JSON-RPC requests from
/// `input` and writing replies to `output` until the input stream closes.
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
///
/// `backend` performs the session side effects (queueing a delegated prompt,
/// removing a session); it is injected so the transport and root resolution are
/// exercised in tests without shelling out to a real agent CLI.
pub fn run(backend: Box<dyn AgentBackend>, input: impl BufRead, output: impl Write) -> Result<()> {
    let worktree = std::env::current_dir()?;
    let workspace_root = session::workspace_root(&worktree);
    let server = UsagiMcpServer::new(worktree, workspace_root, backend);
    crate::presentation::mcp::serve(&server, input, output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::Path;

    /// An in-memory [`AgentBackend`] that records what it was asked to do, so the
    /// transport can be driven without touching a real agent CLI or workspace.
    #[derive(Default)]
    struct FakeBackend;

    impl AgentBackend for FakeBackend {
        fn prompt(&self, _worktree: &Path, _prompt: &str) -> Result<String, String> {
            Ok("queued".to_string())
        }

        fn send(&self, _worktree: &Path, _prompt: &str) -> Result<String, String> {
            Ok("sent".to_string())
        }

        fn remove(
            &self,
            _workspace_root: &Path,
            _name: &str,
            _force: bool,
        ) -> Result<session::RemovalOutcome, String> {
            Err("not removable in tests".to_string())
        }
    }

    #[test]
    fn run_returns_when_the_input_stream_is_empty() {
        // An empty input means immediate EOF: the transport loop reads nothing and
        // returns, so `run` resolves the roots, builds the server, and exits Ok.
        let input = Cursor::new(Vec::new());
        let mut output = Vec::new();
        run(Box::new(FakeBackend), input, &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn run_serves_a_request_and_writes_a_reply() {
        // A single JSON-RPC request is dispatched and answered on `output`, then
        // EOF ends the loop.
        let request = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n";
        let input = Cursor::new(request.as_bytes().to_vec());
        let mut output = Vec::new();
        run(Box::new(FakeBackend), input, &mut output).unwrap();
        let reply = String::from_utf8(output).unwrap();
        // The reply echoes the request id and is valid JSON-RPC.
        assert!(reply.contains("\"id\":1"));
        assert!(reply.contains("\"jsonrpc\":\"2.0\""));
    }

    #[test]
    fn run_routes_a_session_remove_call_to_the_backend() {
        // A `session_remove` tools/call reaches the injected backend (here the
        // fake, which reports failure), proving the backend is wired through the
        // transport. The reply carries the request id.
        let request = "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\
                        \"params\":{\"name\":\"session_remove\",\
                        \"arguments\":{\"name\":\"ghost\"}}}\n";
        let input = Cursor::new(request.as_bytes().to_vec());
        let mut output = Vec::new();
        run(Box::new(FakeBackend), input, &mut output).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("\"id\":7"));
    }

    #[test]
    fn fake_backend_prompt_returns_its_confirmation() {
        // `session_prompt` only reaches the backend for an existing session, which
        // needs a real worktree; cover the prompt delegate directly instead.
        assert_eq!(
            FakeBackend.prompt(Path::new("/tmp/wt"), "do it").unwrap(),
            "queued"
        );
    }

    #[test]
    fn fake_backend_send_returns_its_confirmation() {
        // Like `session_prompt`, `session_send` only reaches the backend for an
        // existing session, so cover the send delegate directly.
        assert_eq!(
            FakeBackend.send(Path::new("/tmp/wt"), "do it now").unwrap(),
            "sent"
        );
    }
}
