//! `usagi llm-mcp`: run the local LLM MCP server over stdio.
//!
//! A thin transport wrapper around [`crate::presentation::mcp::llm::LlmMcpServer`]
//! (which holds the unit-tested protocol logic). The read/write loop lives in
//! [`serve`], which is generic over its I/O streams so it can be exercised with
//! in-memory buffers and a mock backend. The remaining pieces are the parts that
//! cannot be unit-tested: [`run`], which only binds the real stdio handles, and
//! the [`OllamaBackend`] that shells out to the `ollama` CLI. This file is
//! excluded from coverage (see `scripts/coverage.sh`).

use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};

use anyhow::Result;

use crate::presentation::mcp::llm::{LlmBackend, LlmMcpServer};
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
    let backend = Box::new(OllamaBackend {
        model: model.clone(),
    });
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve(backend, model, stdin.lock(), stdout.lock())
}

/// Run the MCP server read/write loop given a bound backend and I/O streams.
fn serve(
    backend: Box<dyn LlmBackend>,
    model: String,
    stdin: impl BufRead,
    mut stdout: impl Write,
) -> Result<()> {
    let server = LlmMcpServer::new(backend, model);
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(&line) {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    type CallList = Vec<(String, Option<String>)>;

    /// A mock backend to verify the loop's interactions.
    struct FakeBackend {
        result: Result<String, String>,
        calls: Rc<RefCell<CallList>>,
    }

    impl FakeBackend {
        fn new(result: Result<String, String>) -> Self {
            Self {
                result,
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    impl LlmBackend for FakeBackend {
        fn ask(&self, prompt: &str, system: Option<&str>) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((prompt.to_string(), system.map(String::from)));
            self.result.clone()
        }
    }

    #[test]
    fn serve_handles_requests_and_ignores_blank_lines() {
        let backend = Box::new(FakeBackend::new(Ok("summary".into())));
        let model = "test-model".to_string();
        let input = " \n \n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n \n";
        let mut output = Vec::new();

        serve(backend, model, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("\"id\":1"));
        assert!(response.contains("\"result\":{}"));
        // Extra blank lines should not produce responses or errors.
        assert_eq!(response.lines().count(), 1);
    }

    #[test]
    fn serve_exits_cleanly_on_eof() {
        let backend = Box::new(FakeBackend::new(Ok("".into())));
        let input = ""; // Immediate EOF
        let mut output = Vec::new();

        let result = serve(backend, "m".into(), input.as_bytes(), &mut output);

        assert!(result.is_ok());
        assert!(output.is_empty());
    }

    #[test]
    fn serve_processes_tool_calls_via_the_backend() {
        let backend = FakeBackend::new(Ok("A bright sunny day.".into()));
        let calls = backend.calls.clone(); // so we can inspect it later

        let backend_box = Box::new(backend);
        let model = "test-model".to_string();
        let request = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"local_llm_ask","arguments":{"prompt":"describe the weather","system":"be poetic"}}}"#;
        let input = format!("{request}\n");
        let mut output = Vec::new();

        serve(backend_box, model, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();

        // Assert the backend was called with correct arguments
        let received_calls = calls.borrow();
        assert_eq!(received_calls.len(), 1);
        assert_eq!(received_calls[0].0, "describe the weather");
        assert_eq!(received_calls[0].1.as_deref(), Some("be poetic"));

        // Assert the response contains the result from the backend
        assert!(response.contains("A bright sunny day."));
    }
}
