//! `usagi llm-mcp`: run the local LLM MCP server over stdio.
//!
//! A thin transport wrapper around [`crate::presentation::mcp::llm::LlmMcpServer`]
//! (which holds the unit-tested protocol logic). The shared read/write loop
//! ([`crate::presentation::mcp::serve`]) does the framing; this file only builds
//! the server and runs the loop.
//!
//! The genuine IO is injected so the wiring stays testable: the production
//! [`LlmBackend`] that shells out to the `ollama` CLI lives in the
//! (coverage-excluded) binary entry point, and the byte streams are parameters —
//! tests drive [`run`] with an in-memory backend and buffers, while `main` wires
//! in the real backend and stdio locks.

use std::io::{BufRead, Write};

use anyhow::Result;

use crate::presentation::mcp::llm::{LlmBackend, LlmMcpServer};

/// Entry point for `usagi llm-mcp`: serve the local LLM `ask` tool for `model`,
/// reading JSON-RPC requests from `input` and writing replies to `output` until
/// the input stream closes. `backend` performs the actual completion (the
/// production backend shells out to `ollama`); it is injected so the transport is
/// exercised in tests without a local model runtime.
pub fn run(
    backend: Box<dyn LlmBackend>,
    model: String,
    input: impl BufRead,
    output: impl Write,
) -> Result<()> {
    let server = LlmMcpServer::new(backend, model);
    crate::presentation::mcp::serve(&server, input, output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// An in-memory [`LlmBackend`] that echoes a canned completion, so the
    /// transport can be driven without a local model runtime.
    struct FakeBackend;

    impl LlmBackend for FakeBackend {
        fn ask(&self, prompt: &str, _system: Option<&str>) -> Result<String, String> {
            Ok(format!("answer: {prompt}"))
        }
    }

    #[test]
    fn run_returns_when_the_input_stream_is_empty() {
        // An empty input means immediate EOF: `run` builds the server and exits Ok
        // without writing anything.
        let input = Cursor::new(Vec::new());
        let mut output = Vec::new();
        run(
            Box::new(FakeBackend),
            "test-model".to_string(),
            input,
            &mut output,
        )
        .unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn run_routes_an_ask_call_to_the_backend() {
        // A `local_llm_ask` tools/call reaches the injected backend and its answer
        // is written back on `output`, carrying the request id.
        let request = "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\
                        \"params\":{\"name\":\"local_llm_ask\",\
                        \"arguments\":{\"prompt\":\"ping\"}}}\n";
        let input = Cursor::new(request.as_bytes().to_vec());
        let mut output = Vec::new();
        run(
            Box::new(FakeBackend),
            "test-model".to_string(),
            input,
            &mut output,
        )
        .unwrap();
        let reply = String::from_utf8(output).unwrap();
        assert!(reply.contains("\"id\":3"));
        // The fake's completion (`answer: ping`) is relayed in the reply.
        assert!(reply.contains("answer: ping"));
    }
}
