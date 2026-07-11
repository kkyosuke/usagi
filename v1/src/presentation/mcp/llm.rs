//! MCP server exposing a local LLM as a single delegation tool.
//!
//! Where [`super::issue`] exposes issue operations, this server exposes a single
//! `local_llm_ask` tool backed by a locally-running model (served via Ollama). A
//! cloud agent (Claude Code etc.) can delegate light, low-stakes work —
//! summaries, naming, boilerplate, simple transforms — to the local model,
//! spending its own (metered) tokens only on the work that needs them.
//!
//! The JSON-RPC framing is shared with the issue server (see [`super`]). The
//! model call itself is abstracted behind [`LlmBackend`] so the dispatch logic
//! is fully unit-tested without invoking a real model; the production Ollama
//! backend lives in the thin stdio entry point (`presentation/cli/llm_mcp.rs`).

use serde::Deserialize;
use serde_json::{json, Value};

use super::McpService;

const TOOL_NAMES: [&str; 1] = ["local_llm_ask"];

/// Runs a prompt against the local model. Abstracted so the server's protocol
/// handling can be tested with a fake backend that never shells out.
pub trait LlmBackend {
    /// Ask the model `prompt` (optionally prefixed with a `system` instruction),
    /// returning its completion text (`Ok`) or an error message to surface to
    /// the agent (`Err`).
    fn ask(&self, prompt: &str, system: Option<&str>) -> Result<String, String>;
}

/// A JSON-RPC server exposing the local LLM `ask` tool.
pub struct LlmMcpServer {
    backend: Box<dyn LlmBackend>,
    /// The model name advertised in the tool description (for the agent's
    /// benefit); the backend is already bound to it.
    model: String,
}

impl LlmMcpServer {
    /// Build a server that delegates completions to `backend`, advertising
    /// `model` in its tool description.
    pub fn new(backend: Box<dyn LlmBackend>, model: impl Into<String>) -> Self {
        Self {
            backend,
            model: model.into(),
        }
    }

    /// Handle one JSON-RPC message (a single line of input). Returns the JSON
    /// response to write back, or `None` for notifications (which take no
    /// reply).
    pub fn handle_line(&self, line: &str) -> Option<String> {
        super::dispatch_line(self, line)
    }

    fn tool_ask(&self, arguments: Value) -> Result<String, String> {
        let args: AskArgs =
            serde_json::from_value(arguments).map_err(|e| format!("invalid arguments: {e}"))?;
        self.backend.ask(&args.prompt, args.system.as_deref())
    }
}

impl McpService for LlmMcpServer {
    fn server_name(&self) -> &str {
        "usagi-llm"
    }

    fn tool_names(&self) -> &'static [&'static str] {
        &TOOL_NAMES
    }

    fn tool_schemas(&self) -> Value {
        json!([
            {
                "name": "local_llm_ask",
                "description": format!(
                    "Delegate a light, low-stakes task to the local LLM ({}) to save \
                     your own tokens — summarizing, naming, drafting boilerplate, or \
                     simple text transforms. Returns the model's completion text.",
                    self.model
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string", "description": "The task or question for the local model" },
                        "system": { "type": "string", "description": "Optional system instruction prepended to the prompt" }
                    },
                    "required": ["prompt"]
                }
            }
        ])
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        match name {
            "local_llm_ask" => self.tool_ask(arguments),
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

/// Arguments for the `local_llm_ask` tool.
#[derive(Deserialize)]
struct AskArgs {
    prompt: String,
    #[serde(default)]
    system: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::mcp::PROTOCOL_VERSION;
    use std::cell::RefCell;

    /// A backend that records the calls it received and returns a scripted
    /// result, so the server's dispatch can be tested without a real model.
    struct FakeBackend {
        result: Result<String, String>,
        calls: RefCell<Vec<(String, Option<String>)>>,
    }

    impl FakeBackend {
        fn ok(reply: &str) -> Self {
            Self {
                result: Ok(reply.to_string()),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn err(message: &str) -> Self {
            Self {
                result: Err(message.to_string()),
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl LlmBackend for FakeBackend {
        fn ask(&self, prompt: &str, system: Option<&str>) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push((prompt.to_string(), system.map(str::to_string)));
            self.result.clone()
        }
    }

    fn server_ok(reply: &str) -> LlmMcpServer {
        LlmMcpServer::new(Box::new(FakeBackend::ok(reply)), "qwen2.5-coder:7b")
    }

    /// Parse a handler reply back into JSON for assertions.
    fn reply(server: &LlmMcpServer, request: Value) -> Value {
        let line = serde_json::to_string(&request).unwrap();
        let response = server.handle_line(&line).expect("expected a reply");
        serde_json::from_str(&response).unwrap()
    }

    fn call(server: &LlmMcpServer, name: &str, arguments: Value) -> Value {
        reply(
            server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        )["result"]
            .clone()
    }

    #[test]
    fn initialize_advertises_the_llm_server() {
        let res = reply(
            &server_ok("hi"),
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        );
        assert_eq!(res["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(res["result"]["capabilities"]["tools"].is_object());
        assert_eq!(res["result"]["serverInfo"]["name"], "usagi-llm");
    }

    #[test]
    fn ping_returns_empty_result() {
        let res = reply(
            &server_ok("hi"),
            json!({"jsonrpc":"2.0","id":7,"method":"ping"}),
        );
        assert_eq!(res["id"], 7);
        assert_eq!(res["result"], json!({}));
    }

    #[test]
    fn tools_list_advertises_the_ask_tool_with_the_model_name() {
        let server = server_ok("hi");
        assert_eq!(server.tool_names(), ["local_llm_ask"]);

        let res = reply(
            &server,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        let tools = res["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "local_llm_ask");
        // The description names the bound model so the agent knows what it is.
        assert!(tools[0]["description"]
            .as_str()
            .unwrap()
            .contains("qwen2.5-coder:7b"));
    }

    #[test]
    fn ask_forwards_prompt_and_system_to_the_backend() {
        let backend = FakeBackend::ok("a summary");
        let server = LlmMcpServer::new(Box::new(backend), "m");
        let result = call(
            &server,
            "local_llm_ask",
            json!({"prompt":"summarize this","system":"be terse"}),
        );
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "a summary");
    }

    #[test]
    fn ask_without_a_system_prompt_defaults_to_none() {
        // Exercises the `#[serde(default)]` path: no `system` key supplied.
        let server = server_ok("done");
        let result = call(&server, "local_llm_ask", json!({"prompt":"hello"}));
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "done");
    }

    #[test]
    fn ask_surfaces_backend_errors_as_tool_errors() {
        let server = LlmMcpServer::new(Box::new(FakeBackend::err("model offline")), "m");
        let result = call(&server, "local_llm_ask", json!({"prompt":"hi"}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("model offline"));
    }

    #[test]
    fn ask_with_invalid_arguments_is_a_tool_error() {
        // `prompt` is required.
        let result = call(&server_ok("x"), "local_llm_ask", json!({"system":"only"}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("invalid arguments"));
    }

    #[test]
    fn unknown_tool_is_reported_as_a_tool_error() {
        let result = call(&server_ok("x"), "frobnicate", json!({}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    #[test]
    fn tool_call_without_a_name_is_invalid_params() {
        let res = reply(
            &server_ok("x"),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}),
        );
        assert_eq!(res["error"]["code"], -32602);
    }

    #[test]
    fn tool_call_without_arguments_is_an_invalid_argument_error() {
        // No `arguments` defaults to `{}`, which lacks the required `prompt`.
        let res = reply(
            &server_ok("x"),
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"local_llm_ask"}}),
        );
        assert_eq!(res["result"]["isError"], true);
    }

    #[test]
    fn parse_error_is_reported() {
        let res: Value =
            serde_json::from_str(&server_ok("x").handle_line("{ not json").unwrap()).unwrap();
        assert_eq!(res["error"]["code"], -32700);
        assert_eq!(res["id"], Value::Null);
    }

    #[test]
    fn missing_method_is_an_invalid_request() {
        let res = reply(&server_ok("x"), json!({"jsonrpc":"2.0","id":1,"foo":"bar"}));
        assert_eq!(res["error"]["code"], -32600);
    }

    #[test]
    fn notifications_get_no_reply() {
        let line = json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string();
        assert!(server_ok("x").handle_line(&line).is_none());
    }

    #[test]
    fn unknown_method_is_not_found() {
        let res = reply(
            &server_ok("x"),
            json!({"jsonrpc":"2.0","id":1,"method":"nope"}),
        );
        assert_eq!(res["error"]["code"], -32601);
    }
}
