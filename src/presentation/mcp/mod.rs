//! MCP (Model Context Protocol) servers and their shared JSON-RPC plumbing.
//!
//! usagi speaks MCP over stdio so AI agents (Claude Code etc.) can drive it with
//! the same operations a human uses on the CLI. The servers here:
//!
//! - [`usagi`] is the single server launched by `usagi mcp`. It composes the
//!   issue/memory and session servers below so one process exposes a
//!   repository's task issues, its durable memories, and session orchestration
//!   under one `usagi` registration.
//! - [`issue`] exposes a repository's task issues — and, merged into the same
//!   server, its [`memory`] tools.
//! - [`session`] exposes session orchestration (create / list / prompt) as tools.
//! - [`llm`] exposes a locally-running model as a single delegation tool.
//!
//! All speak JSON-RPC 2.0 with newline-delimited messages and implement the
//! small subset MCP needs (`initialize`, `tools/list`, `tools/call`, `ping`)
//! directly over `serde_json` — no async runtime, so dispatch stays synchronous
//! and unit-testable. The framing (parsing, method dispatch, response shaping)
//! is identical between them and lives here; each server only supplies the
//! parts that differ via [`McpService`].

pub mod child_io;
pub mod issue;
pub mod llm;
pub mod memory;
pub mod session;
pub mod usagi;

use std::io::{BufRead, Write};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

/// MCP protocol version these servers implement.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// Deserialize tool arguments into `T`, mapping any error to a tool-facing
/// message. Shared by every MCP server's tool handlers.
pub(crate) fn parse_args<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|e| format!("invalid arguments: {e}"))
}

/// Pretty-print a serialisable tool result as JSON, falling back to an empty
/// string on the (practically unreachable) serialisation error. Shared by every
/// MCP server's tool handlers.
pub(crate) fn to_pretty<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Unwrap a tool-schema [`Value`] into its array of entries, used by composite
/// servers to merge the schemas of the servers they wrap.
///
/// The schema builders return JSON arrays by construction, so this normally just
/// takes the inner `Vec`. A non-array (a construction bug) degrades to no
/// entries rather than panicking: `tools/list` is on the hot path and a panic
/// there would abort the whole stdio server — taking every tool down — instead
/// of merely advertising fewer tools.
pub(crate) fn into_schema_array(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}

/// Run the MCP read/write loop for `service` over the given streams: read
/// newline-delimited JSON-RPC requests, skip blank lines, and write each reply
/// back, flushing per line. Generic over its streams so it is driven by stdio in
/// production and by in-memory buffers in tests.
pub fn serve(
    service: &dyn McpService,
    mut input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    // Read raw bytes and decode lossily rather than using `BufRead::lines`, which
    // yields an `Err` on a line containing invalid UTF-8 — propagating that would
    // let one malformed byte sequence from a misbehaving client terminate the
    // whole server. A non-UTF-8 line instead becomes replacement characters that
    // fail to parse as JSON, so [`dispatch_line`] returns a `-32700 parse error`
    // and the loop keeps going. A genuine IO error (e.g. a broken pipe) still
    // propagates and ends the loop.
    let mut raw = Vec::new();
    loop {
        raw.clear();
        if input.read_until(b'\n', &mut raw)? == 0 {
            break; // EOF
        }
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = dispatch_line(service, line) {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
    Ok(())
}

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
    // Per MCP, `arguments` MUST be an object when present. Validate it at the
    // framing layer so a client sending e.g. `"arguments": 42` gets a clear
    // `-32602` rather than a serde type error leaking out of a tool handler. An
    // absent or null `arguments` is treated as the empty object.
    let arguments = match params.and_then(|p| p.get("arguments")) {
        None | Some(Value::Null) => json!({}),
        Some(value @ Value::Object(_)) => value.clone(),
        Some(_) => {
            return error_response(id, -32602, "invalid params: arguments must be an object")
        }
    };

    let outcome = service.call_tool(name, arguments);
    crate::infrastructure::trace_log::TraceLog::record(
        crate::domain::trace::TraceEvent::now(crate::domain::trace::TraceCategory::Mcp, name)
            .with_detail(if outcome.is_ok() { "ok" } else { "error" }),
    );
    let result = match outcome {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal service: every tool call echoes its name back, so the loop's
    /// framing can be exercised without any real business logic.
    struct EchoService;

    impl McpService for EchoService {
        fn server_name(&self) -> &str {
            "echo"
        }

        fn tool_schemas(&self) -> Value {
            json!([])
        }

        fn call_tool(&self, name: &str, _arguments: Value) -> Result<String, String> {
            Ok(format!("called {name}"))
        }
    }

    #[test]
    fn serve_replies_to_requests_but_not_to_blank_lines_or_notifications() {
        // A blank line is skipped, and a notification (a message with a method
        // but no id) is acted on without a reply, so only the `ping` request
        // produces a single line of output.
        let input = concat!(
            " \n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n",
            " \n",
        );
        let mut output = Vec::new();

        serve(&EchoService, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("\"id\":1"));
        assert!(response.contains("\"result\":{}"));
        assert_eq!(response.lines().count(), 1);
    }

    #[test]
    fn serve_advertises_the_service_identity_and_tools() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
        );
        let mut output = Vec::new();

        serve(&EchoService, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("\"name\":\"echo\""));
        assert!(response.contains("\"tools\":[]"));
    }

    #[test]
    fn serve_exits_cleanly_on_eof() {
        let mut output = Vec::new();

        let result = serve(&EchoService, "".as_bytes(), &mut output);

        assert!(result.is_ok());
        assert!(output.is_empty());
    }

    #[test]
    fn serve_processes_tool_calls_via_the_service() {
        let request = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"do_thing","arguments":{}}}"#;
        let input = format!("{request}\n");
        let mut output = Vec::new();

        serve(&EchoService, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("called do_thing"));
    }

    #[test]
    fn serve_answers_a_non_utf8_line_with_a_parse_error_and_keeps_going() {
        // A line of invalid UTF-8 must not terminate the server: it becomes a
        // parse error, and a following valid request is still answered.
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(&[0xff, 0xfe, 0x00, b'\n']); // not valid UTF-8
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n");
        let mut output = Vec::new();

        serve(&EchoService, input.as_slice(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        // Two replies: the parse error for the bad line, then the ping result.
        assert!(response.contains("-32700"));
        assert!(response.contains("\"id\":7"));
        assert_eq!(response.lines().count(), 2);
    }

    #[test]
    fn tool_call_with_non_object_arguments_is_an_invalid_params_error() {
        // `arguments` present but not an object is rejected at the framing layer.
        let request = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"do_thing","arguments":42}}"#;
        let input = format!("{request}\n");
        let mut output = Vec::new();

        serve(&EchoService, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("-32602"));
        assert!(response.contains("arguments must be an object"));
        // The tool was never reached.
        assert!(!response.contains("called do_thing"));
    }

    #[test]
    fn into_schema_array_takes_arrays_and_degrades_other_shapes_to_empty() {
        // An array is unwrapped to its entries…
        assert_eq!(
            into_schema_array(json!([{"name": "a"}, {"name": "b"}])).len(),
            2
        );
        // …and any non-array (a construction bug) degrades to no entries rather
        // than panicking the `tools/list` path.
        assert!(into_schema_array(json!({"not": "an array"})).is_empty());
        assert!(into_schema_array(json!(null)).is_empty());
    }

    #[test]
    fn tool_call_with_null_arguments_is_treated_as_empty() {
        // An explicit null `arguments` is lenient — the same as omitting it.
        let request = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"do_thing","arguments":null}}"#;
        let input = format!("{request}\n");
        let mut output = Vec::new();

        serve(&EchoService, input.as_bytes(), &mut output).unwrap();

        let response = String::from_utf8(output).unwrap();
        assert!(response.contains("called do_thing"));
    }
}
