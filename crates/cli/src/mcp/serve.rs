//! `usagi mcp` の stdio serve ループ。1 行 = 1 JSON-RPC 2.0 メッセージを読み、
//! `initialize` / `tools/list` / `tools/call` / `resources/list` / `resources/read` を
//! 処理して 1 行の応答を返す。
//!
//! 1 接続の lifecycle state と行単位の validation/routing を `handle_line_with_client` に閉じ込め、
//! `serve` は実 IO（stdin/stdout）の反復だけを担う。実 IO は合成ルートが注入するため、routing は
//! ユニットテストできる。`tools/call` は実装済み tool を対応する store / daemon 経路へ送り、
//! issue / memory は cwd の core store usecase、session 系は daemon client へ接続し、
//! tool 個別または daemon のエラーを JSON-RPC エラーへ変換する。

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};
use usagi_core::usecase::client::{
    ClientError, DaemonClient, DaemonReply, DaemonRequest, McpCallerContext,
};

use super::protocol::{self, error_code};
use super::runtime_model::{PathExecutableLocator, RuntimeModelSnapshot, WorkspaceAgentConfig};
use super::tool::{CallerPolicy, ToolDescriptor, ToolError, ToolRoute};
use super::{resources, tools};

/// サーバが対応する MCP プロトコルバージョン。
const SUPPORTED_PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServerState {
    AwaitingInitialize,
    AwaitingInitialized,
    Ready,
}

/// stdin の JSON-RPC を行ごとに処理し、応答を stdout へ書く。EOF で正常終了する。
///
/// トップレベルの誤り処理: **不正入力 1 行でサーバを止めない**。行は生バイトで読み、
/// 非 UTF-8 はロッシー変換してパースエラー（`handle_line` が `-32700` を返す）に落とす。
/// stdin の真の IO エラー（切断など）だけを伝播して終了する。リクエスト単位のエラー
/// （不正 JSON・未知 method/tool・引数不正・tool 未実装）は `handle_line` が JSON-RPC
/// エラー応答に整形し、ループは継続する。
///
/// `version` は `initialize` の `serverInfo.version` に載せる配布 version（合成ルートが注入）。
///
/// # Errors
///
/// stdin の読み取り、または `out` への書き込みが IO エラーになった場合、そのエラーを返す。
#[coverage(off)]
pub fn serve(input: impl BufRead, out: &mut dyn Write, version: &str) -> io::Result<()> {
    let mut unavailable = UnavailableClient;
    serve_with_client(input, out, version, &mut unavailable)
}

/// As [`serve`], but routes managed-session tools through the supplied daemon
/// client. The stdio server owns no session state and never falls back to a
/// local PTY when the client reports an error.
///
/// # Errors
///
/// Returns only stdin/stdout IO errors; daemon failures are encoded as
/// JSON-RPC responses so one failed tool call does not stop the server.
#[coverage(off)]
pub fn serve_with_client(
    input: impl BufRead,
    out: &mut dyn Write,
    version: &str,
    client: &mut dyn DaemonClient,
) -> io::Result<()> {
    let workspace = std::env::current_dir()?;
    let locator = PathExecutableLocator;
    let config = WorkspaceAgentConfig::read(&workspace);
    let snapshot = RuntimeModelSnapshot::capture(&config, &locator);
    serve_with_client_and_snapshot(input, out, version, client, &snapshot)
}

/// As [`serve_with_client`], with a pre-captured runtime/model snapshot.
/// This is the injection seam for embeddings and deterministic tests.
///
/// # Errors
///
/// Returns stdin/stdout IO errors; protocol and validation errors remain MCP
/// responses so serving continues.
#[coverage(off)]
pub fn serve_with_client_and_snapshot(
    mut input: impl BufRead,
    out: &mut dyn Write,
    version: &str,
    client: &mut dyn DaemonClient,
    snapshot: &RuntimeModelSnapshot,
) -> io::Result<()> {
    // Fail before accepting input if metadata, route, schema, or capability drifted.
    drop(tools::registry());
    let mut buf = Vec::new();
    let mut state = ServerState::AwaitingInitialize;
    loop {
        buf.clear();
        // 生バイトで 1 行読む。真の IO エラーだけ `?` で伝播する。
        if input.read_until(b'\n', &mut buf)? == 0 {
            return Ok(()); // EOF
        }
        // 非 UTF-8 はロッシー変換（不正 JSON になり handle_line が parse error を返す）。
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(response) = handle_line_with_client(line, version, client, snapshot, &mut state)
        {
            writeln!(out, "{response}")?;
        }
    }
}

struct UnavailableClient;
impl DaemonClient for UnavailableClient {
    #[coverage(off)]
    fn request(&mut self, _request: DaemonRequest) -> Result<DaemonReply, ClientError> {
        Err(ClientError::Unavailable(
            "managed daemon client is not configured".into(),
        ))
    }
}

/// 1 リクエスト行を処理して応答文字列を返す。通知（`id` 無し）は `None`。
#[cfg(test)]
#[coverage(off)]
fn handle_line(line: &str, version: &str) -> Option<String> {
    let mut unavailable = UnavailableClient;
    let mut state = ServerState::Ready;
    handle_line_with_client(
        line,
        version,
        &mut unavailable,
        &RuntimeModelSnapshot::default(),
        &mut state,
    )
}

#[coverage(off)]
fn handle_line_with_client(
    line: &str,
    version: &str,
    client: &mut dyn DaemonClient,
    snapshot: &RuntimeModelSnapshot,
    state: &mut ServerState,
) -> Option<String> {
    let Ok(request) = serde_json::from_str::<Value>(line) else {
        return Some(
            protocol::error(Value::Null, error_code::PARSE_ERROR, "parse error").to_string(),
        );
    };
    let Some(object) = request.as_object() else {
        return Some(
            protocol::error(Value::Null, error_code::INVALID_REQUEST, "invalid request")
                .to_string(),
        );
    };
    let is_notification = !object.contains_key("id");
    let response_id = match object.get("id") {
        Some(id) if valid_id(id) => id.clone(),
        Some(_) | None => Value::Null,
    };
    let invalid = |code, message: &str| {
        (!is_notification).then(|| protocol::error(response_id.clone(), code, message).to_string())
    };

    if object.get("jsonrpc") != Some(&Value::String(protocol::VERSION.to_owned())) {
        return invalid(error_code::INVALID_REQUEST, "jsonrpc must be \"2.0\"");
    }
    if object.contains_key("id") && !valid_id(&object["id"]) {
        return invalid(
            error_code::INVALID_REQUEST,
            "id must be a string or integer",
        );
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return invalid(error_code::INVALID_REQUEST, "method must be a string");
    };
    if object
        .get("params")
        .is_some_and(|params| !params.is_object())
    {
        return invalid(error_code::INVALID_PARAMS, "params must be an object");
    }

    if is_notification {
        handle_notification(method, state);
        return None;
    }

    Some(
        respond(
            method,
            response_id,
            object.get("params"),
            version,
            client,
            snapshot,
            state,
        )
        .to_string(),
    )
}

fn valid_id(id: &Value) -> bool {
    id.is_string() || id.as_i64().is_some() || id.as_u64().is_some()
}

fn handle_notification(method: &str, state: &mut ServerState) {
    if method == "notifications/initialized" && *state == ServerState::AwaitingInitialized {
        *state = ServerState::Ready;
    }
}

/// method 別に応答 `Value` を組み立てる。
#[coverage(off)]
fn respond(
    method: &str,
    id: Value,
    params: Option<&Value>,
    version: &str,
    client: &mut dyn DaemonClient,
    snapshot: &RuntimeModelSnapshot,
    state: &mut ServerState,
) -> Value {
    if method == "initialize" {
        if *state != ServerState::AwaitingInitialize {
            return protocol::error(
                id,
                error_code::INVALID_REQUEST,
                "initialize is only allowed once at connection start",
            );
        }
        return match initialize_result(params, version) {
            Ok(result) => {
                *state = ServerState::AwaitingInitialized;
                protocol::success(id, result)
            }
            Err(message) => protocol::error(id, error_code::INVALID_PARAMS, message),
        };
    }
    if method == "notifications/initialized" {
        return protocol::error(
            id,
            error_code::INVALID_REQUEST,
            "notifications/initialized must be a notification",
        );
    }
    if method != "ping" && *state != ServerState::Ready {
        return protocol::error(id, error_code::INVALID_REQUEST, "server is not initialized");
    }
    match method {
        "ping" => protocol::success(id, json!({})),
        "tools/list" => protocol::success(id, tools_list_result(snapshot)),
        "tools/call" => tools_call(id, params, client, snapshot),
        "resources/list" => protocol::success(id, resources::list_result()),
        "resources/read" => resources_read(id, params),
        other => protocol::error(
            id,
            error_code::METHOD_NOT_FOUND,
            &format!("method not found: {other}"),
        ),
    }
}

/// `initialize` の結果（プロトコル版・capabilities・serverInfo）。
#[coverage(off)]
fn initialize_result(params: Option<&Value>, version: &str) -> Result<Value, &'static str> {
    let Some(protocol_version) = params
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str)
    else {
        return Err("missing protocolVersion");
    };
    if protocol_version != SUPPORTED_PROTOCOL_VERSION {
        return Err("unsupported protocolVersion; server supports 2025-06-18");
    }
    Ok(json!({
        "protocolVersion": SUPPORTED_PROTOCOL_VERSION,
        "capabilities": { "tools": {}, "resources": {} },
        "serverInfo": { "name": "usagi", "version": version },
    }))
}

/// `tools/list` の結果（全 tool の name / description / inputSchema）。
#[coverage(off)]
fn tools_list_result(snapshot: &RuntimeModelSnapshot) -> Value {
    let tools: Vec<Value> = tools::registry()
        .iter()
        .map(|tool| {
            // 各 tool の input_schema は妥当な JSON（tools のテストで検証済み）。
            let mut schema: Value = serde_json::from_str(tool.input_schema()).unwrap();
            if matches!(tool.name(), "session_dispatch" | "session_delegate_brief") {
                schema["properties"]["agent"] = snapshot.agent_schema();
            }
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "inputSchema": schema,
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// `tools/call` を処理する。実装済み tool を store / daemon 経路へ送り、未実装 tool と
/// daemon の protocol error は JSON-RPC エラーとして返す。
#[coverage(off)]
fn tools_call(
    id: Value,
    params: Option<&Value>,
    client: &mut dyn DaemonClient,
    snapshot: &RuntimeModelSnapshot,
) -> Value {
    let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
        return protocol::error(id, error_code::INVALID_PARAMS, "missing tool name");
    };
    if params
        .and_then(|params| params.get("arguments"))
        .is_some_and(|arguments| !arguments.is_object())
    {
        return protocol::error(
            id,
            error_code::INVALID_PARAMS,
            "arguments must be an object",
        );
    }
    let mut arguments = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let registry = tools::registry();
    let Some(descriptor) = registry.iter().find(|descriptor| descriptor.name() == name) else {
        return protocol::error(
            id,
            error_code::METHOD_NOT_FOUND,
            &format!("unknown tool: {name}"),
        );
    };
    let mut schema: Value = serde_json::from_str(descriptor.input_schema()).unwrap();
    if matches!(name, "session_dispatch" | "session_delegate_brief") {
        schema["properties"]["agent"] = snapshot.agent_schema();
        if let Some(agent) = arguments.get("agent")
            && let Err(message) = snapshot.validate_agent(agent)
        {
            return protocol::error(id, error_code::INVALID_PARAMS, &message);
        }
    }
    if let Err(ToolError::InvalidParams(message)) = descriptor.validate(&arguments, &schema) {
        return protocol::error(id, error_code::INVALID_PARAMS, &message);
    }
    apply_caller_policy(descriptor.caller_policy(), &mut arguments);
    if matches!(name, "session_create" | "session_delegate_issue")
        && let Err(message) = snapshot.normalize_legacy_agent(&mut arguments)
    {
        return protocol::error(id, error_code::INVALID_PARAMS, &message);
    }
    execute_tool(id, descriptor, arguments, client)
}

#[coverage(off)]
fn execute_tool(
    id: Value,
    descriptor: &ToolDescriptor,
    arguments: Value,
    client: &mut dyn DaemonClient,
) -> Value {
    match descriptor.route() {
        ToolRoute::Session(action) => {
            let operation_id = usagi_core::domain::id::OperationId::new().as_str();
            match client.request(DaemonRequest::Session {
                action,
                operation_id,
                payload: arguments,
            }) {
                Ok(DaemonReply::Accepted {
                    operation_id,
                    revision,
                    ..
                }) => protocol::success(
                    id,
                    json!({"content":[{"type":"text","text":format!("accepted operation {operation_id} (revision {revision})")}]}),
                ),
                Ok(DaemonReply::Ok(value)) => protocol::success(
                    id,
                    json!({"content":[{"type":"text","text":value.to_string()}]}),
                ),
                Err(error) => protocol::error(id, error_code::INTERNAL_ERROR, &error.to_string()),
            }
        }
        ToolRoute::Dispatch(action) => {
            let operation_id = usagi_core::domain::id::OperationId::new().as_str();
            match client.request(DaemonRequest::DispatchTool {
                action,
                operation_id,
                payload: arguments,
                caller_context: std::env::var("USAGI_MCP_CALLER_CREDENTIAL")
                    .ok()
                    .filter(|credential| !credential.is_empty())
                    .map(|credential| McpCallerContext { credential }),
            }) {
                Ok(DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body)) => {
                    protocol::success(
                        id,
                        json!({"content":[{"type":"text","text":body.to_string()}]}),
                    )
                }
                Err(error) => protocol::error(id, error_code::INTERNAL_ERROR, &error.to_string()),
            }
        }
        ToolRoute::Supervisor(action) => {
            let operation_id = arguments
                .get("idempotency_key")
                .and_then(Value::as_str)
                .map_or_else(
                    || usagi_core::domain::id::OperationId::new().as_str(),
                    ToOwned::to_owned,
                );
            match client.request(DaemonRequest::SupervisorTool {
                action,
                operation_id,
                payload: arguments,
            }) {
                Ok(DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body)) => {
                    protocol::success(
                        id,
                        json!({"content":[{"type":"text","text":body.to_string()}]}),
                    )
                }
                Err(error) => protocol::error(id, error_code::INTERNAL_ERROR, &error.to_string()),
            }
        }
        ToolRoute::Store => store_tool_call(id, descriptor, &arguments),
        ToolRoute::Unavailable(reason) => protocol::error(
            id,
            error_code::INTERNAL_ERROR,
            &format!("tool unavailable: {reason}"),
        ),
    }
}

#[coverage(off)]
fn store_tool_call(id: Value, descriptor: &ToolDescriptor, arguments: &Value) -> Value {
    match descriptor.call_store(arguments) {
        Ok(result) => protocol::success(
            id,
            json!({"content":[{"type":"text","text":result}], "isError": false}),
        ),
        Err(ToolError::UnknownTool(tool)) => protocol::error(
            id,
            error_code::METHOD_NOT_FOUND,
            &format!("unknown tool: {tool}"),
        ),
        Err(ToolError::InvalidParams(message)) => {
            protocol::error(id, error_code::INVALID_PARAMS, &message)
        }
        Err(ToolError::Execution(message)) => {
            protocol::error(id, error_code::INTERNAL_ERROR, &message)
        }
        Err(ToolError::Unimplemented(name)) => protocol::error(
            id,
            error_code::INTERNAL_ERROR,
            &format!("tool not yet implemented: {name}"),
        ),
    }
}

#[coverage(off)]
fn apply_caller_policy(policy: CallerPolicy, arguments: &mut Value) {
    if policy == CallerPolicy::SessionCredential
        && let Ok(credential) = std::env::var("USAGI_MCP_CALLER_CREDENTIAL")
        && !credential.is_empty()
    {
        arguments["_caller_credential"] = Value::String(credential);
    }
}

/// `resources/read` を処理する。`uri` を取り出して resource レジストリを引き、本文を
/// `contents` に包んで返す。`uri` 欠落は `INVALID_PARAMS`、未知 URI は resource が無い旨の
/// `INVALID_PARAMS` を返す。
#[coverage(off)]
fn resources_read(id: Value, params: Option<&Value>) -> Value {
    let Some(uri) = params.and_then(|p| p.get("uri")).and_then(Value::as_str) else {
        return protocol::error(id, error_code::INVALID_PARAMS, "missing resource uri");
    };
    match resources::read_result(uri) {
        Some(result) => protocol::success(id, result),
        None => protocol::error(
            id,
            error_code::INVALID_PARAMS,
            &format!("unknown resource: {uri}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SUPPORTED_PROTOCOL_VERSION, ServerState, handle_line, handle_line_with_client, serve,
        serve_with_client, serve_with_client_and_snapshot,
    };
    use crate::mcp::runtime_model::{
        ExecutableLocator, RuntimeModelSnapshot, WorkspaceAgentConfig,
    };
    use serde_json::Value;
    use usagi_core::usecase::client::{ClientError, DaemonClient, DaemonReply, DaemonRequest};

    struct RecordingClient {
        reply: Result<DaemonReply, ClientError>,
        requests: Vec<DaemonRequest>,
    }

    struct FakeLocator(&'static [&'static str]);
    impl ExecutableLocator for FakeLocator {
        fn is_available(&self, executable: &str) -> bool {
            self.0.contains(&executable)
        }
    }
    impl DaemonClient for RecordingClient {
        fn request(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError> {
            self.requests.push(request);
            self.reply.clone()
        }
    }

    /// 1 行を処理して応答 `Value` を得る（通知は `None`）。
    fn call(line: &str) -> Option<Value> {
        handle_line(line, "9.9.9").map(|s| serde_json::from_str(&s).unwrap())
    }

    fn valid_arguments(name: &str, snapshot: &RuntimeModelSnapshot) -> Value {
        fn value(schema: &Value) -> Value {
            if let Some(value) = schema.get("const") {
                return value.clone();
            }
            if let Some(value) = schema
                .get("enum")
                .and_then(Value::as_array)
                .and_then(|values| values.first())
            {
                return value.clone();
            }
            if let Some(schema) = schema
                .get("oneOf")
                .and_then(Value::as_array)
                .and_then(|schemas| schemas.first())
            {
                return value(schema);
            }
            match schema.get("type").and_then(Value::as_str) {
                Some("object") => {
                    let mut result = serde_json::Map::new();
                    let properties = schema["properties"].as_object().unwrap();
                    for key in schema
                        .get("required")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        result.insert(key.to_owned(), value(&properties[key]));
                    }
                    Value::Object(result)
                }
                Some("array") => serde_json::json!([]),
                Some("string") => serde_json::json!("value"),
                Some("integer") => schema
                    .get("minimum")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!(0)),
                Some("number") => serde_json::json!(0),
                Some("boolean") => serde_json::json!(false),
                _ => Value::Null,
            }
        }

        // Exercise the generic fixture builder's branches even when no current
        // required field uses these schema types.
        assert_eq!(value(&serde_json::json!({"type":"number"})), 0);
        assert_eq!(value(&serde_json::json!({"type":"boolean"})), false);
        assert_eq!(value(&serde_json::json!({})), Value::Null);

        let registry = crate::mcp::tools::registry();
        let descriptor = registry
            .iter()
            .find(|descriptor| descriptor.name() == name)
            .unwrap();
        let mut schema: Value = serde_json::from_str(descriptor.input_schema()).unwrap();
        if matches!(name, "session_dispatch" | "session_delegate_brief") {
            schema["properties"]["agent"] = snapshot.agent_schema();
        }
        value(&schema)
    }

    fn initialize(line: &str) -> Value {
        let mut client = RecordingClient {
            reply: Ok(DaemonReply::Ok(serde_json::json!({}))),
            requests: vec![],
        };
        let mut state = ServerState::AwaitingInitialize;
        serde_json::from_str(
            &handle_line_with_client(
                line,
                "9.9.9",
                &mut client,
                &RuntimeModelSnapshot::default(),
                &mut state,
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn initialized_input(request: &str) -> String {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"{SUPPORTED_PROTOCOL_VERSION}\"}}}}\n{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}\n{request}"
        )
    }

    fn last_response(output: &[u8]) -> Value {
        serde_json::from_str(std::str::from_utf8(output).unwrap().lines().last().unwrap()).unwrap()
    }

    #[test]
    fn initialize_negotiates_supported_protocol_and_reports_server_version() {
        let v = initialize(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        );
        assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(v["result"]["serverInfo"]["name"], "usagi");
        assert_eq!(v["result"]["serverInfo"]["version"], "9.9.9");
        assert!(v["result"]["capabilities"]["tools"].is_object());
        assert!(v["result"]["capabilities"]["resources"].is_object());
    }

    #[test]
    fn initialize_rejects_missing_and_unsupported_protocol_versions() {
        let missing = initialize(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        assert_eq!(missing["error"]["code"], -32602);
        let unsupported = initialize(
            r#"{"jsonrpc":"2.0","id":"v","method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
        );
        assert_eq!(unsupported["id"], "v");
        assert_eq!(unsupported["error"]["code"], -32602);
    }

    #[test]
    fn ping_returns_empty_result() {
        let v = call(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#).unwrap();
        assert!(v["result"].is_object());
        assert_eq!(v["id"], 2);
        let large = call(r#"{"jsonrpc":"2.0","id":18446744073709551615,"method":"ping"}"#).unwrap();
        assert_eq!(large["id"], serde_json::json!(u64::MAX));
    }

    #[test]
    fn tools_list_returns_every_tool_with_schema() {
        let v = call(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#).unwrap();
        let tools = v["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 48);
        // 各要素が name / description / inputSchema(object) を持つ。
        for tool in tools {
            assert!(tool["name"].as_str().is_some());
            assert!(tool["description"].as_str().is_some());
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn tools_call_store_tool_returns_content() {
        let v = call(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"issue_get","arguments":{"number":4294967295}}}"#).unwrap();
        assert_eq!(v["result"]["content"][0]["text"], "null");
        assert_eq!(v["result"]["isError"], false);
    }

    #[test]
    fn tools_call_store_tool_maps_invalid_arguments_and_execution_errors() {
        let invalid = call(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"issue_get","arguments":{}}}"#).unwrap();
        assert_eq!(invalid["error"]["code"], -32602);

        let missing = call(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"issue_to_prompt","arguments":{"number":4294967295}}}"#).unwrap();
        assert_eq!(missing["error"]["code"], -32603);
        assert!(
            missing["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no issue")
        );
    }

    #[test]
    fn tools_call_unknown_tool_is_method_not_found() {
        let v = call(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope"}}"#)
            .unwrap();
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn tools_call_without_name_is_invalid_params() {
        let v = call(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{}}"#).unwrap();
        assert_eq!(v["error"]["code"], -32602);
        let arguments = call(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"session_list","arguments":[]}}"#,
        )
        .unwrap();
        assert_eq!(arguments["error"]["code"], -32602);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let v = call(r#"{"jsonrpc":"2.0","id":7,"method":"resources/subscribe"}"#).unwrap();
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn resources_list_returns_the_orchestration_guide() {
        let v = call(r#"{"jsonrpc":"2.0","id":10,"method":"resources/list"}"#).unwrap();
        let resources = v["result"]["resources"].as_array().unwrap();
        assert!(
            resources
                .iter()
                .any(|r| r["uri"] == "usagi://guides/orchestration")
        );
        for resource in resources {
            assert!(resource["uri"].as_str().is_some());
            assert!(resource["name"].as_str().is_some());
            assert!(resource["mimeType"].as_str().is_some());
        }
    }

    #[test]
    fn resources_read_returns_the_guide_body_for_a_known_uri() {
        let v = call(r#"{"jsonrpc":"2.0","id":11,"method":"resources/read","params":{"uri":"usagi://guides/orchestration"}}"#).unwrap();
        let contents = v["result"]["contents"].as_array().unwrap();
        assert_eq!(contents[0]["uri"], "usagi://guides/orchestration");
        assert_eq!(contents[0]["mimeType"], "text/markdown");
        assert!(
            contents[0]["text"]
                .as_str()
                .unwrap()
                .contains("orchestration")
        );
    }

    #[test]
    fn resources_read_unknown_uri_is_invalid_params() {
        let v = call(r#"{"jsonrpc":"2.0","id":12,"method":"resources/read","params":{"uri":"usagi://guides/nope"}}"#).unwrap();
        assert_eq!(v["error"]["code"], -32602);
    }

    #[test]
    fn resources_read_without_uri_is_invalid_params() {
        let v = call(r#"{"jsonrpc":"2.0","id":13,"method":"resources/read","params":{}}"#).unwrap();
        assert_eq!(v["error"]["code"], -32602);
    }

    #[test]
    fn invalid_json_is_parse_error_with_null_id() {
        let v = call("not json").unwrap();
        assert_eq!(v["error"]["code"], -32700);
        assert_eq!(v["id"], Value::Null);
    }

    #[test]
    fn request_without_method_is_invalid_request() {
        let v = call(r#"{"jsonrpc":"2.0","id":8}"#).unwrap();
        assert_eq!(v["error"]["code"], -32600);
    }

    #[test]
    fn notification_without_id_has_no_response() {
        assert!(call(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    }

    #[test]
    fn malformed_without_method_or_id_is_ignored() {
        assert!(call(r#"{"jsonrpc":"2.0"}"#).is_none());
    }

    #[test]
    fn raw_stdio_validates_json_rpc_envelopes_and_preserves_error_ids() {
        let input = concat!(
            "not json\n",
            "[]\n",
            "1\n",
            "{\"id\":1,\"method\":\"ping\"}\n",
            "{\"jsonrpc\":\"1.0\",\"id\":2,\"method\":\"ping\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"method\":\"ping\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1.5,\"method\":\"ping\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":7}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":\"p\",\"method\":\"ping\",\"params\":[]}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":7}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":[]}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"ping\"}\n",
        );
        let mut output = Vec::new();
        serve(input.as_bytes(), &mut output, "9.9.9").unwrap();
        let responses: Vec<Value> = std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(responses.len(), 10);
        assert_eq!(responses[0]["error"]["code"], -32700);
        assert_eq!(responses[0]["id"], Value::Null);
        for response in &responses[1..8] {
            assert_eq!(response["error"]["code"], -32600);
        }
        assert_eq!(responses[1]["id"], Value::Null);
        assert_eq!(responses[3]["id"], 1);
        assert_eq!(responses[4]["id"], 2);
        assert_eq!(responses[5]["id"], Value::Null);
        assert_eq!(responses[6]["id"], Value::Null);
        assert_eq!(responses[7]["id"], 3);
        assert_eq!(responses[8]["error"]["code"], -32602);
        assert_eq!(responses[8]["id"], "p");
        assert_eq!(responses[9]["result"], serde_json::json!({}));
    }

    #[test]
    fn raw_stdio_negotiates_version_and_enforces_lifecycle_without_effects() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"session_list\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"session_list\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2024-11-05\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"session_list\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"session_list\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":6,\"method\":\"tools/call\",\"params\":{\"name\":\"session_list\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n",
        );
        let mut output = Vec::new();
        let mut client = RecordingClient {
            reply: Ok(DaemonReply::Ok(serde_json::json!({"effect":true}))),
            requests: vec![],
        };
        serve_with_client(input.as_bytes(), &mut output, "9.9.9", &mut client).unwrap();
        let responses: Vec<Value> = std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(responses.len(), 7);
        assert_eq!(responses[0]["error"]["code"], -32600);
        assert_eq!(responses[1]["error"]["code"], -32602);
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(responses[2]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(responses[3]["error"]["code"], -32600);
        assert_eq!(responses[4]["error"]["code"], -32600);
        assert!(
            responses[5]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("effect")
        );
        assert_eq!(responses[6]["error"]["code"], -32600);
        assert_eq!(client.requests.len(), 1);
    }

    #[test]
    fn serve_reads_lines_skips_blanks_and_writes_responses() {
        let input = "\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
        let mut out = Vec::new();
        serve(input.as_bytes(), &mut out, "9.9.9").unwrap();
        let text = String::from_utf8(out).unwrap();
        // ping には 1 応答、空行と通知には応答なし＝出力は 1 行。
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"id\":1"));
    }

    #[test]
    fn serve_survives_non_utf8_line_and_keeps_serving() {
        // 非 UTF-8 の行 → パースエラーで返し、続く正常な ping にも応答する（サーバは落ちない）。
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(&[0xff, 0xfe, b'\n']);
        input.extend_from_slice(br#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#);
        input.push(b'\n');

        let mut out = Vec::new();
        serve(input.as_slice(), &mut out, "9.9.9").unwrap();
        let text = String::from_utf8(out).unwrap();

        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let parse_error: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parse_error["error"]["code"], -32700);
        let ping: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(ping["id"], 9);
    }

    #[test]
    fn managed_session_tools_use_the_injected_daemon_client() {
        for (name, reply) in [
            (
                "session_create",
                Ok(DaemonReply::Accepted {
                    operation_id: "op".into(),
                    revision: 3,
                    body: serde_json::json!(null),
                }),
            ),
            (
                "session_remove",
                Ok(DaemonReply::Ok(serde_json::json!({"removed":true}))),
            ),
            (
                "session_prompt",
                Ok(DaemonReply::Accepted {
                    operation_id: "op".into(),
                    revision: 3,
                    body: serde_json::json!(null),
                }),
            ),
        ] {
            let snapshot = RuntimeModelSnapshot::default();
            let arguments = valid_arguments(name, &snapshot);
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
            ) + "\n";
            let input = initialized_input(&request);
            let mut out = Vec::new();
            let mut client = RecordingClient {
                reply,
                requests: vec![],
            };
            serve_with_client(input.as_bytes(), &mut out, "9.9.9", &mut client).unwrap();
            assert_eq!(client.requests.len(), 1);
            assert!(String::from_utf8(out).unwrap().contains("content"));
        }
    }

    #[test]
    fn observation_scratchpad_and_delegate_tools_route_to_the_daemon() {
        for name in [
            "session_list",
            "session_status",
            "session_complete",
            "session_pr",
            "session_note_get",
            "session_note_update",
            "session_todo_list",
            "session_todo_add",
            "session_todo_update",
            "session_todo_remove",
            "session_decision_list",
            "session_decision_log",
            "session_delegate_issue",
            "session_delegate_brief",
        ] {
            let snapshot = RuntimeModelSnapshot::default();
            let arguments = valid_arguments(name, &snapshot);
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
            ) + "\n";
            let input = initialized_input(&request);
            let mut out = Vec::new();
            let mut client = RecordingClient {
                reply: Ok(DaemonReply::Ok(serde_json::json!({"connected":true}))),
                requests: vec![],
            };
            serve_with_client(input.as_bytes(), &mut out, "9.9.9", &mut client).unwrap();
            assert_eq!(client.requests.len(), 1, "{name}");
            assert!(String::from_utf8(out).unwrap().contains("connected"));
        }
    }

    #[test]
    fn delegate_brief_requires_one_validated_agent_selector() {
        let snapshot = RuntimeModelSnapshot::capture(
            &WorkspaceAgentConfig::from_allowlists(vec!["sonnet".into()], vec![]),
            &FakeLocator(&["claude"]),
        );
        for arguments in [
            r#"{"brief":"triage"}"#,
            r#"{"brief":"triage","agent":{"id":"a","runtime":"claude","model":"sonnet"}}"#,
            r#"{"brief":"triage","agent":{"runtime":"claude"}}"#,
        ] {
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"session_delegate_brief","arguments":{arguments}}}}}"#
            ) + "\n";
            let input = initialized_input(&request);
            let mut out = Vec::new();
            let mut client = RecordingClient {
                reply: Ok(DaemonReply::Ok(serde_json::json!({"unexpected":true}))),
                requests: vec![],
            };
            serve_with_client_and_snapshot(
                input.as_bytes(),
                &mut out,
                "9.9.9",
                &mut client,
                &snapshot,
            )
            .unwrap();
            assert_eq!(client.requests.len(), 0);
            assert_eq!(last_response(&out)["error"]["code"], -32602);
        }
    }

    #[test]
    fn dispatch_tools_use_the_injected_daemon_client() {
        for (name, action) in [
            (
                "session_dispatch",
                usagi_core::usecase::client::DispatchToolAction::Dispatch,
            ),
            (
                "session_get",
                usagi_core::usecase::client::DispatchToolAction::SessionGet,
            ),
            (
                "agent_list",
                usagi_core::usecase::client::DispatchToolAction::AgentList,
            ),
            (
                "agent_get",
                usagi_core::usecase::client::DispatchToolAction::AgentGet,
            ),
            (
                "agent_complete",
                usagi_core::usecase::client::DispatchToolAction::AgentComplete,
            ),
            (
                "agent_fail",
                usagi_core::usecase::client::DispatchToolAction::AgentFail,
            ),
            (
                "agent_inbox",
                usagi_core::usecase::client::DispatchToolAction::AgentInbox,
            ),
            (
                "user_decision_request",
                usagi_core::usecase::client::DispatchToolAction::UserDecisionRequest,
            ),
            (
                "user_decision_get",
                usagi_core::usecase::client::DispatchToolAction::UserDecisionGet,
            ),
            (
                "user_decision_list",
                usagi_core::usecase::client::DispatchToolAction::UserDecisionList,
            ),
            (
                "user_decision_resolve",
                usagi_core::usecase::client::DispatchToolAction::UserDecisionResolve,
            ),
            (
                "user_decision_cancel",
                usagi_core::usecase::client::DispatchToolAction::UserDecisionCancel,
            ),
            (
                "user_decision_expire",
                usagi_core::usecase::client::DispatchToolAction::UserDecisionExpire,
            ),
        ] {
            let snapshot = RuntimeModelSnapshot::capture(
                &WorkspaceAgentConfig::from_allowlists(vec!["sonnet".into()], vec![]),
                &FakeLocator(&["claude"]),
            );
            let arguments = valid_arguments(name, &snapshot);
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
            ) + "\n";
            let input = initialized_input(&request);
            let mut out = Vec::new();
            let mut client = RecordingClient {
                reply: Ok(DaemonReply::Ok(serde_json::json!({"ok":true}))),
                requests: vec![],
            };
            serve_with_client_and_snapshot(
                input.as_bytes(),
                &mut out,
                "9.9.9",
                &mut client,
                &snapshot,
            )
            .unwrap();
            assert!(String::from_utf8(out).unwrap().contains("ok"));
            assert!(
                matches!(&client.requests[0], DaemonRequest::DispatchTool { action: actual, .. } if *actual == action)
            );
        }
    }

    #[test]
    fn unimplemented_daemon_tools_return_json_rpc_errors() {
        for name in [
            "session_dispatch",
            "session_get",
            "agent_list",
            "agent_get",
            "agent_complete",
            "agent_fail",
            "agent_inbox",
            "supervisor_start",
            "supervisor_get",
            "supervisor_list",
            "supervisor_cancel",
            "supervisor_resolve_escalation",
            "supervisor_events",
        ] {
            let snapshot = RuntimeModelSnapshot::capture(
                &WorkspaceAgentConfig::from_allowlists(vec!["sonnet".into()], vec![]),
                &FakeLocator(&["claude"]),
            );
            let arguments = valid_arguments(name, &snapshot);
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
            ) + "\n";
            let input = initialized_input(&request);
            let mut out = Vec::new();
            let mut client = RecordingClient {
                reply: Err(ClientError::Protocol(
                    usagi_core::infrastructure::ipc::ProtocolError::new(
                        usagi_core::infrastructure::ipc::ErrorCode::InvalidArgument,
                        "daemon tool action is not implemented",
                    ),
                )),
                requests: vec![],
            };
            serve_with_client_and_snapshot(
                input.as_bytes(),
                &mut out,
                "9.9.9",
                &mut client,
                &snapshot,
            )
            .unwrap();
            let response = last_response(&out);
            assert_eq!(response["error"]["code"], -32603, "{name}");
            assert!(
                response["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("not implemented")),
                "{name}"
            );
            assert_eq!(client.requests.len(), 1, "{name}");
        }
    }

    #[test]
    fn dispatch_schema_and_parser_use_the_captured_snapshot() {
        let snapshot = RuntimeModelSnapshot::capture(
            &WorkspaceAgentConfig::default(),
            &FakeLocator(&["claude"]),
        );
        // An empty config never publishes a runtime even when its executable exists.
        let input = initialized_input("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n");
        let mut out = Vec::new();
        let mut client = RecordingClient {
            reply: Ok(DaemonReply::Ok(serde_json::json!({}))),
            requests: vec![],
        };
        serve_with_client_and_snapshot(input.as_bytes(), &mut out, "9.9.9", &mut client, &snapshot)
            .unwrap();
        let listed = last_response(&out);
        let dispatch = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "session_dispatch")
            .unwrap();
        assert_eq!(
            dispatch["inputSchema"]["properties"]["agent"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let snapshot = RuntimeModelSnapshot::capture(
            &WorkspaceAgentConfig::from_allowlists(vec!["sonnet".into()], vec![]),
            &FakeLocator(&["claude"]),
        );
        let input = initialized_input(
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"session_dispatch\",\"arguments\":{\"session\":{\"name\":\"a\"},\"agent\":{\"runtime\":\"claude\",\"model\":\"opus\"},\"prompt\":\"p\"}}}\n",
        );
        let mut out = Vec::new();
        serve_with_client_and_snapshot(input.as_bytes(), &mut out, "9.9.9", &mut client, &snapshot)
            .unwrap();
        assert!(String::from_utf8(out).unwrap().contains("not allowed"));
    }

    #[test]
    fn default_serve_returns_a_structured_unavailable_error_for_session_tools() {
        let input = initialized_input(
            "\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"session_create\",\"arguments\":{\"name\":\"a\"}}}\n",
        );
        let mut out = Vec::new();
        serve(input.as_bytes(), &mut out, "9.9.9").unwrap();
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("managed daemon client is not configured"));
    }
}
