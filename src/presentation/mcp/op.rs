//! MCP server exposing the 1Password CLI (`op`) as a small set of read tools.
//!
//! Where [`super::llm`] exposes a local model and [`super::issue`] exposes a
//! repository's task issues, this server exposes a focused, read-only slice of
//! 1Password: resolving secret references (`op://vault/item/field`), reading an
//! item, and listing items / vaults. An AI agent can fetch the credentials a
//! task needs (API keys, connection strings, …) on demand instead of having
//! them pasted into prompts or committed to the repo.
//!
//! The JSON-RPC framing is shared with the other servers (see [`super`]). Every
//! tool reduces to one `op` invocation, so the CLI call is abstracted behind a
//! single [`OpBackend::run`]: all argument-building and dispatch logic is
//! unit-tested with a fake backend that records the args and never shells out,
//! while the production backend (which runs the real `op` binary) lives in the
//! thin stdio entry point (the composition root, coverage-excluded).
//!
//! Authentication is the caller's responsibility: `op` reads its session from
//! the environment (an interactive `op signin`, or an `OP_SERVICE_ACCOUNT_TOKEN`
//! for non-interactive use). This server never handles credentials itself — it
//! only forwards the agent's request to an already-authenticated `op`.

use serde::Deserialize;
use serde_json::{json, Value};

use super::McpService;

/// Runs one invocation of the `op` (1Password) CLI. Abstracted so the server's
/// protocol and argument-building logic can be tested with a fake backend that
/// never shells out; the production backend that runs the real `op` binary lives
/// at the composition root (`main.rs`), like the other MCP backends.
pub trait OpBackend {
    /// Run `op` with `args`, returning its stdout (`Ok`) or an error message to
    /// surface to the agent (`Err`).
    fn run(&self, args: &[String]) -> Result<String, String>;
}

/// A JSON-RPC server exposing read-only 1Password tools backed by the `op` CLI.
pub struct OpMcpServer {
    backend: Box<dyn OpBackend>,
}

impl OpMcpServer {
    /// Build a server that runs `op` through `backend`.
    pub fn new(backend: Box<dyn OpBackend>) -> Self {
        Self { backend }
    }

    /// Handle one JSON-RPC message (a single line of input). Returns the JSON
    /// response to write back, or `None` for notifications (which take no
    /// reply).
    pub fn handle_line(&self, line: &str) -> Option<String> {
        super::dispatch_line(self, line)
    }

    fn tool_read(&self, arguments: Value) -> Result<String, String> {
        let args: ReadArgs = super::parse_args(arguments)?;
        let reference = require_non_empty("reference", &args.reference)?;
        self.backend
            .run(&["read".into(), "--no-newline".into(), reference])
    }

    fn tool_item_get(&self, arguments: Value) -> Result<String, String> {
        let args: ItemGetArgs = super::parse_args(arguments)?;
        let item = require_non_empty("item", &args.item)?;
        let mut cli = vec![
            "item".into(),
            "get".into(),
            item,
            "--format".into(),
            "json".into(),
        ];
        push_optional(&mut cli, "--vault", args.vault.as_deref());
        push_optional(&mut cli, "--fields", args.fields.as_deref());
        self.backend.run(&cli)
    }

    fn tool_item_list(&self, arguments: Value) -> Result<String, String> {
        let args: ItemListArgs = super::parse_args(arguments)?;
        let mut cli = vec![
            "item".into(),
            "list".into(),
            "--format".into(),
            "json".into(),
        ];
        push_optional(&mut cli, "--vault", args.vault.as_deref());
        self.backend.run(&cli)
    }

    fn tool_vault_list(&self, _arguments: Value) -> Result<String, String> {
        self.backend.run(&[
            "vault".into(),
            "list".into(),
            "--format".into(),
            "json".into(),
        ])
    }

    fn tool_whoami(&self, _arguments: Value) -> Result<String, String> {
        self.backend
            .run(&["whoami".into(), "--format".into(), "json".into()])
    }
}

impl McpService for OpMcpServer {
    fn server_name(&self) -> &str {
        "usagi-op"
    }

    fn tool_schemas(&self) -> Value {
        json!([
            {
                "name": "op_read",
                "description": "Resolve a 1Password secret reference (op://vault/item/field) to its \
                                value via the `op` CLI. Use this to fetch a single credential a task \
                                needs (API key, token, password) without it being pasted into a prompt.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "reference": { "type": "string", "description": "Secret reference, e.g. op://Private/GitHub/token" }
                    },
                    "required": ["reference"]
                }
            },
            {
                "name": "op_item_get",
                "description": "Get a 1Password item as JSON via the `op` CLI. Optionally scope to a \
                                vault and/or restrict to specific fields (a comma-separated list).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "item": { "type": "string", "description": "Item name, ID, or share link" },
                        "vault": { "type": "string", "description": "Vault name or ID to scope the lookup to" },
                        "fields": { "type": "string", "description": "Comma-separated fields to return (e.g. username,password)" }
                    },
                    "required": ["item"]
                }
            },
            {
                "name": "op_item_list",
                "description": "List 1Password items as JSON via the `op` CLI, optionally scoped to a \
                                vault. Returns metadata only (no secret values).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "vault": { "type": "string", "description": "Vault name or ID to scope the listing to" }
                    }
                }
            },
            {
                "name": "op_vault_list",
                "description": "List the 1Password vaults the signed-in account can access, as JSON, \
                                via the `op` CLI.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "op_whoami",
                "description": "Report the 1Password account `op` is currently signed in as, as JSON. \
                                Use this to check authentication before reading secrets.",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ])
    }

    fn call_tool(&self, name: &str, arguments: Value) -> Result<String, String> {
        match name {
            "op_read" => self.tool_read(arguments),
            "op_item_get" => self.tool_item_get(arguments),
            "op_item_list" => self.tool_item_list(arguments),
            "op_vault_list" => self.tool_vault_list(arguments),
            "op_whoami" => self.tool_whoami(arguments),
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

/// Trim `value` and reject it as a tool error if it is empty, so a blank
/// required argument fails fast with a clear message instead of running `op`
/// with an empty operand.
fn require_non_empty(field: &str, value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("`{field}` must not be empty"));
    }
    Ok(trimmed.to_string())
}

/// Append `flag value` to `cli` when `value` is present and non-blank. A
/// supplied-but-blank optional is treated as "omitted" rather than passing an
/// empty operand to `op`.
fn push_optional(cli: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            cli.push(flag.to_string());
            cli.push(trimmed.to_string());
        }
    }
}

/// Arguments for the `op_read` tool.
#[derive(Deserialize)]
struct ReadArgs {
    reference: String,
}

/// Arguments for the `op_item_get` tool.
#[derive(Deserialize)]
struct ItemGetArgs {
    item: String,
    #[serde(default)]
    vault: Option<String>,
    #[serde(default)]
    fields: Option<String>,
}

/// Arguments for the `op_item_list` tool.
#[derive(Deserialize)]
struct ItemListArgs {
    #[serde(default)]
    vault: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::mcp::PROTOCOL_VERSION;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A backend that records the `op` args it received and returns a scripted
    /// result through a shared handle, so a test can read the args back after the
    /// server (which owns the backend) has run — all without the real `op` binary.
    #[derive(Clone)]
    struct FakeBackend {
        result: Result<String, String>,
        calls: Rc<RefCell<Vec<Vec<String>>>>,
    }

    impl FakeBackend {
        fn ok(reply: &str) -> Self {
            Self {
                result: Ok(reply.to_string()),
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn err(message: &str) -> Self {
            Self {
                result: Err(message.to_string()),
                calls: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    impl OpBackend for FakeBackend {
        fn run(&self, args: &[String]) -> Result<String, String> {
            self.calls.borrow_mut().push(args.to_vec());
            self.result.clone()
        }
    }

    fn server_ok(reply: &str) -> OpMcpServer {
        OpMcpServer::new(Box::new(FakeBackend::ok(reply)))
    }

    /// Build a server over a recording fake, run one tool call, and return the
    /// exact `op` args the backend received. The shared `calls` handle is kept on
    /// this side of the box so the args can be read back after the call.
    fn run_and_capture(tool: &str, arguments: Value) -> Vec<String> {
        let backend = FakeBackend::ok("{}");
        let calls = backend.calls.clone();
        let server = OpMcpServer::new(Box::new(backend));
        call(&server, tool, arguments);
        let recorded = calls.borrow().clone();
        assert_eq!(recorded.len(), 1, "expected exactly one op invocation");
        recorded.into_iter().next().unwrap()
    }

    /// Parse a handler reply back into JSON for assertions.
    fn reply(server: &OpMcpServer, request: Value) -> Value {
        let line = serde_json::to_string(&request).unwrap();
        let response = server.handle_line(&line).expect("expected a reply");
        serde_json::from_str(&response).unwrap()
    }

    fn call(server: &OpMcpServer, name: &str, arguments: Value) -> Value {
        reply(
            server,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        )["result"]
            .clone()
    }

    #[test]
    fn initialize_advertises_the_op_server() {
        let res = reply(
            &server_ok("x"),
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        );
        assert_eq!(res["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(res["result"]["capabilities"]["tools"].is_object());
        assert_eq!(res["result"]["serverInfo"]["name"], "usagi-op");
    }

    #[test]
    fn ping_returns_empty_result() {
        let res = reply(
            &server_ok("x"),
            json!({"jsonrpc":"2.0","id":9,"method":"ping"}),
        );
        assert_eq!(res["id"], 9);
        assert_eq!(res["result"], json!({}));
    }

    #[test]
    fn tools_list_advertises_all_five_tools() {
        let res = reply(
            &server_ok("x"),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        );
        let tools = res["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "op_read",
                "op_item_get",
                "op_item_list",
                "op_vault_list",
                "op_whoami"
            ]
        );
    }

    #[test]
    fn read_builds_the_op_read_args_and_returns_the_value() {
        let backend = FakeBackend::ok("s3cr3t");
        let server = OpMcpServer::new(Box::new(backend));
        let result = call(
            &server,
            "op_read",
            json!({"reference":"op://Private/GitHub/token"}),
        );
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "s3cr3t");
    }
    #[test]
    fn read_args_are_exactly_what_op_expects() {
        // The reference is trimmed and `--no-newline` is set so the raw secret
        // comes back without a trailing LF.
        let recorded = run_and_capture("op_read", json!({"reference":" op://V/I/f "}));
        assert_eq!(
            recorded,
            vec![
                "read".to_string(),
                "--no-newline".into(),
                "op://V/I/f".into()
            ]
        );
    }

    #[test]
    fn read_with_a_blank_reference_is_a_tool_error() {
        let result = call(&server_ok("x"), "op_read", json!({"reference":"   "}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("`reference` must not be empty"));
    }

    #[test]
    fn read_without_a_reference_is_an_invalid_argument_error() {
        let result = call(&server_ok("x"), "op_read", json!({}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("invalid arguments"));
    }

    #[test]
    fn item_get_minimal_args() {
        let recorded = run_and_capture("op_item_get", json!({"item":"GitHub"}));
        assert_eq!(recorded, vec!["item", "get", "GitHub", "--format", "json"]);
    }

    #[test]
    fn item_get_with_vault_and_fields_appends_both_flags() {
        let recorded = run_and_capture(
            "op_item_get",
            json!({"item":"GitHub","vault":"Private","fields":"username,password"}),
        );
        assert_eq!(
            recorded,
            vec![
                "item",
                "get",
                "GitHub",
                "--format",
                "json",
                "--vault",
                "Private",
                "--fields",
                "username,password"
            ]
        );
    }

    #[test]
    fn item_get_ignores_blank_optional_flags() {
        // A supplied-but-blank vault/fields is treated as omitted.
        let recorded = run_and_capture(
            "op_item_get",
            json!({"item":"GitHub","vault":"   ","fields":""}),
        );
        assert_eq!(recorded, vec!["item", "get", "GitHub", "--format", "json"]);
    }

    #[test]
    fn item_get_with_a_blank_item_is_a_tool_error() {
        let result = call(&server_ok("x"), "op_item_get", json!({"item":" "}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("`item` must not be empty"));
    }

    #[test]
    fn item_list_minimal_and_with_vault() {
        let bare = run_and_capture("op_item_list", json!({}));
        assert_eq!(bare, vec!["item", "list", "--format", "json"]);

        let scoped = run_and_capture("op_item_list", json!({"vault":"Private"}));
        assert_eq!(
            scoped,
            vec!["item", "list", "--format", "json", "--vault", "Private"]
        );
    }

    #[test]
    fn vault_list_args() {
        let recorded = run_and_capture("op_vault_list", json!({}));
        assert_eq!(recorded, vec!["vault", "list", "--format", "json"]);
    }

    #[test]
    fn whoami_args() {
        let recorded = run_and_capture("op_whoami", json!({}));
        assert_eq!(recorded, vec!["whoami", "--format", "json"]);
    }

    #[test]
    fn backend_errors_surface_as_tool_errors() {
        let server = OpMcpServer::new(Box::new(FakeBackend::err("not signed in")));
        let result = call(&server, "op_read", json!({"reference":"op://V/I/f"}));
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not signed in"));
    }

    #[test]
    fn unknown_tool_is_reported_as_a_tool_error() {
        let result = call(&server_ok("x"), "op_frobnicate", json!({}));
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
