//! MCP (Model Context Protocol) servers and their shared JSON-RPC plumbing.
//!
//! usagi speaks MCP over stdio so AI agents (Claude Code etc.) can drive it with
//! the same operations a human uses on the CLI. Three servers live here:
//!
//! - [`issue`] exposes a repository's task issues — and, merged into the same
//!   server, its [`memory`] tools — so one `usagi mcp` process gives an agent
//!   both task issues and durable memories for a repository.
//! - [`llm`] exposes a locally-running model as a single delegation tool.
//! - [`session`] exposes session orchestration (create / list / prompt) as tools.
//!
//! All speak JSON-RPC 2.0 with newline-delimited messages and implement the
//! small subset MCP needs (`initialize`, `tools/list`, `tools/call`, `ping`)
//! directly over `serde_json` — no async runtime, so dispatch stays synchronous
//! and unit-testable. The framing (parsing, method dispatch, response shaping)
//! is identical between the two and lives here; each server only supplies the
//! parts that differ via [`McpService`].

pub mod issue;
pub mod llm;
pub mod memory;
pub mod session;

use serde_json::{json, Value};

/// MCP protocol version these servers implement.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// The per-server behaviour an MCP server must supply. The JSON-RPC framing is
/// handled once by [`dispatch_line`]; implementors only describe their identity
/// and tools.
pub trait McpService {
    /// `serverInfo.name` advertised during `initialize`.
    fn server_name(&self) -> &str;

    /// Tool schemas advertised via `tools/list`.
    fn tool_schemas(&self) -> Value;

    /// Run a tool by name, returning its text payload (`Ok`) or an error
    /// message to surface to the agent (`Err`).
    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String>;
}

/// Handle one JSON-RPC message (a single line of input) for `service`. Returns
/// the JSON response to write back, or `None` for notifications (which carry no
/// id and take no reply).
pub fn dispatch_line(service: &dyn McpService, line: &str) -> Option<String> {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return Some(error_response(Value::Null, -32700, "parse error")),
    };

    let method = value.get("method").and_then(Value::as_str);
    let id = value.get("id").cloned();
    match (method, id) {
        // A request without a method is malformed.
        (None, _) => Some(error_response(
            Value::Null,
            -32600,
            "invalid request: missing method",
        )),
        // No id means a notification: act on it but send no reply.
        (Some(_), None) => None,
        (Some(method), Some(id)) => {
            Some(dispatch_request(service, method, value.get("params"), id))
        }
    }
}

/// Dispatch a request (one that expects a reply) to its handler.
fn dispatch_request(
    service: &dyn McpService,
    method: &str,
    params: Option<&Value>,
    id: Value,
) -> String {
    match method {
        "initialize" => success_response(id, initialize_result(service.server_name())),
        "ping" => success_response(id, json!({})),
        "tools/list" => success_response(id, json!({ "tools": service.tool_schemas() })),
        "tools/call" => dispatch_tool_call(service, params, id),
        other => error_response(id, -32601, &format!("method not found: {other}")),
    }
}

/// Handle `tools/call`: resolve the tool name, run it, and wrap the outcome as
/// MCP tool result content.
fn dispatch_tool_call(service: &dyn McpService, params: Option<&Value>, id: Value) -> String {
    let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
        return error_response(id, -32602, "invalid params: missing tool name");
    };
    let arguments = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    let result = match service.call_tool(name, arguments) {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }),
    };
    success_response(id, result)
}

/// Wrap `result` as a JSON-RPC success response for `id`.
pub fn success_response(id: Value, result: Value) -> String {
    serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        .unwrap_or_default()
}

/// Wrap a `code` / `message` pair as a JSON-RPC error response for `id`.
pub fn error_response(id: Value, code: i64, message: &str) -> String {
    serde_json::to_string(
        &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    )
    .unwrap_or_default()
}

/// The `initialize` result advertising `name` as the server identity.
fn initialize_result(name: &str) -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": name, "version": env!("CARGO_PKG_VERSION") },
    })
}
