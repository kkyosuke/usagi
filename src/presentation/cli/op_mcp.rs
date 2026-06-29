//! `usagi op-mcp`: run the 1Password CLI MCP server over stdio.
//!
//! A thin transport wrapper around [`crate::presentation::mcp::op::OpMcpServer`]
//! (which holds the unit-tested protocol logic). The shared read/write loop
//! ([`crate::presentation::mcp::serve`]) does the framing; this file only builds
//! the server and runs the loop.
//!
//! The genuine process IO is injected: the production [`OpBackend`] that shells
//! out to `op` lives in the (coverage-excluded) binary entry point, while tests
//! drive [`run`] with an in-memory backend and buffers.

use std::io::{BufRead, Write};

use anyhow::Result;

use crate::presentation::mcp::op::{OpBackend, OpMcpServer};

/// Entry point for `usagi op-mcp`: serve the 1Password read tools, reading
/// JSON-RPC requests from `input` and writing replies to `output` until the
/// input stream closes. `backend` performs the actual `op` invocation (the
/// production backend shells out to the 1Password CLI); it is injected so the
/// transport is exercised in tests without a signed-in account or the `op`
/// binary.
pub fn run(backend: Box<dyn OpBackend>, input: impl BufRead, output: impl Write) -> Result<()> {
    let server = OpMcpServer::new(backend);
    crate::presentation::mcp::serve(&server, input, output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// An in-memory [`OpBackend`] that echoes the args it received, so the
    /// transport can be driven without 1Password.
    struct FakeBackend;

    impl OpBackend for FakeBackend {
        fn run(&self, args: &[String]) -> Result<String, String> {
            Ok(format!("op {}", args.join(" ")))
        }
    }

    #[test]
    fn run_returns_when_the_input_stream_is_empty() {
        // An empty input means immediate EOF: `run` builds the server and exits Ok
        // without writing anything.
        let input = Cursor::new(Vec::new());
        let mut output = Vec::new();
        run(Box::new(FakeBackend), input, &mut output).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn run_routes_a_read_call_to_the_backend() {
        // An `op_read` tools/call reaches the injected backend and its answer is
        // written back on `output`, carrying the request id.
        let request = "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\
                        \"params\":{\"name\":\"op_read\",\
                        \"arguments\":{\"reference\":\"op://V/I/f\"}}}\n";
        let input = Cursor::new(request.as_bytes().to_vec());
        let mut output = Vec::new();
        run(Box::new(FakeBackend), input, &mut output).unwrap();
        let reply = String::from_utf8(output).unwrap();
        assert!(reply.contains("\"id\":3"));
        // The fake's completion shows the precise `op` subcommand was routed.
        assert!(reply.contains("op read --no-newline op://V/I/f"));
    }
}
