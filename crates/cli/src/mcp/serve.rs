//! `usagi mcp` の stdio serve ループ。1 行 = 1 JSON-RPC 2.0 メッセージを読み、
//! `initialize` / `tools/list` / `tools/call` / `resources/list` / `resources/read` を
//! 処理して 1 行の応答を返す。
//!
//! 1 接続の lifecycle state と行単位の validation/routing を `handle_line_with_client` に閉じ込め、
//! `serve` は実 IO（stdin/stdout）の反復だけを担う。実 IO は合成ルートが注入するため、routing は
//! ユニットテストできる。`tools/call` は実装済み tool を対応する store / daemon 経路へ送り、
//! issue / memory は cwd の core store usecase、session 系は daemon client へ接続し、
//! tool 個別または daemon のエラーを JSON-RPC エラーへ変換する。

use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use usagi_core::infrastructure::paths::WORKSPACE_ROOT_ENV;
use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::usecase::client::{
    ClientError, DaemonClient, DaemonReply, DaemonRequest, McpCallerContext,
};

use super::protocol::{self, error_code};
use super::runtime_model::{
    ExecutableLocator, PathExecutableLocator, RuntimeModelSnapshot, WorkspaceAgentConfig,
};
use super::tool::{CallerPolicy, ToolDescriptor, ToolError, ToolRoute};
use super::tools::ToolAvailability;
use super::{resources, tools};

/// サーバが話せる MCP プロトコルバージョン。先頭が優先版である。
///
/// このサーバが実装するのは `initialize` / `ping` / `tools/*` / `resources/*` と
/// `notifications/initialized` だけで、この 3 版のいずれでも同じ形をしている。
/// 新しい版が足した機能（elicitation、structured output など）は使わないため、
/// 古い版を名乗るクライアントにも同じ tool 群をそのまま提供できる。
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// クライアントが名乗った版を話せないときに、代わりに提案する版。
const PREFERRED_PROTOCOL_VERSION: &str = SUPPORTED_PROTOCOL_VERSIONS[0];

/// Maximum JSON payload accepted from one stdio line, excluding its trailing LF.
pub const MAX_STDIO_MESSAGE_BYTES: usize = 1024 * 1024;

fn resolve_workspace_root(current_dir: PathBuf, configured_root: Option<PathBuf>) -> PathBuf {
    configured_root
        .filter(|root| !root.as_os_str().is_empty())
        .unwrap_or(current_dir)
}

fn runtime_model_snapshot(
    workspace_root: &Path,
    locator: &dyn ExecutableLocator,
) -> RuntimeModelSnapshot {
    RuntimeModelSnapshot::capture(&WorkspaceAgentConfig::read(workspace_root), locator)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServerState {
    AwaitingInitialize,
    AwaitingInitialized,
    Ready,
}

#[derive(Clone, Copy)]
struct ServerCapabilities<'a> {
    runtime_models: &'a RuntimeModelSnapshot,
    tools: ToolAvailability,
    caller_credential: Option<&'a str>,
}

/// stdin の JSON-RPC を行ごとに処理し、応答を stdout へ書く。EOF で正常終了する。
///
/// トップレベルの誤り処理: **上限内の不正入力 1 行でサーバを止めない**。非 UTF-8 は
/// parse error (`-32700`) に落とす。JSON payload は末尾 LF を除いて
/// [`MAX_STDIO_MESSAGE_BYTES`] bytes まで受け入れ、超過時は応答を生成せず fail-closed で
/// [`io::ErrorKind::InvalidData`] を返す。リクエスト単位のエラー
/// （不正 JSON・未知 method/tool・引数不正・tool 未実装）は `handle_line` が JSON-RPC
/// エラー応答に整形し、ループは継続する。
///
/// `version` は `initialize` の `serverInfo.version` に載せる配布 version（合成ルートが注入）。
///
/// # Errors
///
/// stdin の読み取り、`out` への書き込み、または入力行の上限超過時にエラーを返す。
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
#[coverage(off)] // coverage: reason=composition owner=root-cli expires=2027-01-31 tests=default_serve_returns_a_structured_unavailable_error_for_session_tools
pub fn serve_with_client(
    input: impl BufRead,
    out: &mut dyn Write,
    version: &str,
    client: &mut dyn DaemonClient,
) -> io::Result<()> {
    let workspace_root = resolve_workspace_root(
        std::env::current_dir()?,
        std::env::var_os(WORKSPACE_ROOT_ENV).map(PathBuf::from),
    );
    let global = Storage::open_default()
        .and_then(|storage| storage.load_settings())
        .map_err(|error| io::Error::other(error.to_string()))?;
    let local = WorkspaceSettingsStore::new(&workspace_root)
        .load()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let availability = ToolAvailability::from(&global.with_local(&local));
    let locator = PathExecutableLocator;
    let snapshot = runtime_model_snapshot(&workspace_root, &locator);
    serve_with_client_and_features_and_caller(
        input,
        out,
        version,
        client,
        &snapshot,
        availability,
        None,
    )
}

/// Serves one daemon-claimed MCP child. The credential is held only in this
/// process's memory and is never read from or copied into its environment.
///
/// # Errors
///
/// Returns an I/O error when settings cannot be loaded or the MCP stream
/// cannot be served.
#[coverage(off)] // coverage: reason=composition owner=root-cli expires=2027-01-31 tests=mcp_e2e
pub fn serve_with_client_and_caller(
    input: impl BufRead,
    out: &mut dyn Write,
    version: &str,
    client: &mut dyn DaemonClient,
    caller_credential: &str,
) -> io::Result<()> {
    let workspace_root = resolve_workspace_root(
        std::env::current_dir()?,
        std::env::var_os(WORKSPACE_ROOT_ENV).map(PathBuf::from),
    );
    let global = Storage::open_default()
        .and_then(|storage| storage.load_settings())
        .map_err(|error| io::Error::other(error.to_string()))?;
    let local = WorkspaceSettingsStore::new(&workspace_root)
        .load()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let availability = ToolAvailability::from(&global.with_local(&local));
    let snapshot = runtime_model_snapshot(&workspace_root, &PathExecutableLocator);
    serve_with_client_and_features_and_caller(
        input,
        out,
        version,
        client,
        &snapshot,
        availability,
        Some(caller_credential),
    )
}

/// As [`serve_with_client`], with a pre-captured runtime/model snapshot.
/// This is the injection seam for embeddings and deterministic tests.
///
/// # Errors
///
/// Returns stdin/stdout IO errors; protocol and validation errors remain MCP
/// responses so serving continues.
pub fn serve_with_client_and_snapshot(
    input: impl BufRead,
    out: &mut dyn Write,
    version: &str,
    client: &mut dyn DaemonClient,
    snapshot: &RuntimeModelSnapshot,
) -> io::Result<()> {
    serve_with_client_and_features(
        input,
        out,
        version,
        client,
        snapshot,
        ToolAvailability::default(),
    )
}

/// As [`serve_with_client_and_snapshot`], with feature availability captured for
/// the same server lifetime. Disabled families are absent from both
/// `tools/list` and `tools/call`.
///
/// # Errors
///
/// Returns stdin/stdout IO errors; protocol and validation errors remain MCP
/// responses so serving continues.
pub fn serve_with_client_and_features(
    input: impl BufRead,
    out: &mut dyn Write,
    version: &str,
    client: &mut dyn DaemonClient,
    snapshot: &RuntimeModelSnapshot,
    availability: ToolAvailability,
) -> io::Result<()> {
    serve_with_client_and_features_and_caller(
        input,
        out,
        version,
        client,
        snapshot,
        availability,
        None,
    )
}

fn serve_with_client_and_features_and_caller(
    mut input: impl BufRead,
    out: &mut dyn Write,
    version: &str,
    client: &mut dyn DaemonClient,
    snapshot: &RuntimeModelSnapshot,
    availability: ToolAvailability,
    caller_credential: Option<&str>,
) -> io::Result<()> {
    // Fail before accepting input if metadata, route, schema, or capability drifted.
    drop(tools::registry_with_availability(availability));
    let mut buf = Vec::with_capacity(MAX_STDIO_MESSAGE_BYTES + 1);
    let mut state = ServerState::AwaitingInitialize;
    let capabilities = ServerCapabilities {
        runtime_models: snapshot,
        tools: availability,
        caller_credential,
    };
    loop {
        buf.clear();
        if read_bounded_line(&mut input, &mut buf)? == 0 {
            return Ok(()); // EOF
        }
        let Ok(line) = std::str::from_utf8(&buf) else {
            writeln!(
                out,
                "{}",
                protocol::error(Value::Null, error_code::PARSE_ERROR, "parse error")
            )?;
            continue;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(response) =
            handle_line_with_client(line, version, client, capabilities, &mut state)
        {
            writeln!(out, "{response}")?;
        }
    }
}

/// Read one LF-delimited message without allowing the destination to grow beyond
/// the payload limit plus the delimiter. Oversize input is not drained: the stdio
/// connection is closed by the caller so no unbounded work follows rejection.
fn read_bounded_line(input: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<usize> {
    let mut bounded = (&mut *input).take((MAX_STDIO_MESSAGE_BYTES + 1) as u64);
    let read = bounded.read_until(b'\n', buf)?;
    let has_lf = buf.last() == Some(&b'\n');
    if read > MAX_STDIO_MESSAGE_BYTES && !has_lf {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("MCP stdio message exceeds {MAX_STDIO_MESSAGE_BYTES} byte limit"),
        ));
    }
    Ok(read)
}

struct UnavailableClient;
impl DaemonClient for UnavailableClient {
    fn request(&mut self, _request: DaemonRequest) -> Result<DaemonReply, ClientError> {
        Err(ClientError::Unavailable(
            "managed daemon client is not configured".into(),
        ))
    }
}

/// 1 リクエスト行を処理して応答文字列を返す。通知（`id` 無し）は `None`。
#[cfg(test)]
fn handle_line(line: &str, version: &str) -> Option<String> {
    let mut unavailable = UnavailableClient;
    let mut state = ServerState::Ready;
    handle_line_with_client(
        line,
        version,
        &mut unavailable,
        ServerCapabilities {
            runtime_models: &RuntimeModelSnapshot::default(),
            tools: ToolAvailability::default(),
            caller_credential: None,
        },
        &mut state,
    )
}

fn handle_line_with_client(
    line: &str,
    version: &str,
    client: &mut dyn DaemonClient,
    capabilities: ServerCapabilities<'_>,
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
            capabilities,
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
fn respond(
    method: &str,
    id: Value,
    params: Option<&Value>,
    version: &str,
    client: &mut dyn DaemonClient,
    capabilities: ServerCapabilities<'_>,
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
        "tools/list" => protocol::success(
            id,
            tools_list_result(capabilities.runtime_models, capabilities.tools),
        ),
        "tools/call" => tools_call(
            id,
            params,
            client,
            capabilities.runtime_models,
            capabilities.tools,
            capabilities.caller_credential,
        ),
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
fn initialize_result(params: Option<&Value>, version: &str) -> Result<Value, &'static str> {
    let Some(protocol_version) = params
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str)
    else {
        return Err("missing protocolVersion");
    };
    // MCP は「話せない版を名乗られたら、話せる版を返す」ことを求めており、
    // 拒否は求めていない。ここで落とすと、まだ 2024-11-05 を送るクライアントが
    // 接続できず、tool を 1 つも使えないまま終わる。
    let negotiated = SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .find(|supported| **supported == protocol_version)
        .copied()
        .unwrap_or(PREFERRED_PROTOCOL_VERSION);
    Ok(json!({
        "protocolVersion": negotiated,
        "capabilities": { "tools": {}, "resources": {} },
        "serverInfo": { "name": "usagi", "version": version },
    }))
}

/// `tools/list` の結果（全 tool の name / description / inputSchema）。
fn tools_list_result(snapshot: &RuntimeModelSnapshot, availability: ToolAvailability) -> Value {
    let tools: Vec<Value> = tools::registry_with_availability(availability)
        .iter()
        .map(|tool| {
            // 各 tool の input_schema は妥当な JSON（tools のテストで検証済み）。
            let mut schema: Value = serde_json::from_str(tool.input_schema()).unwrap();
            if let Some(agent) = agent_selector_schema(snapshot, tool.name()) {
                schema["properties"]["agent"] = agent;
            }
            if matches!(tool.name(), "session_create" | "session_delegate_issue") {
                schema["properties"]["runtime"] = RuntimeModelSnapshot::runtime_schema();
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
fn tools_call(
    id: Value,
    params: Option<&Value>,
    client: &mut dyn DaemonClient,
    snapshot: &RuntimeModelSnapshot,
    availability: ToolAvailability,
    caller_credential: Option<&str>,
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
    let registry = tools::registry_with_availability(availability);
    let Some(descriptor) = registry.iter().find(|descriptor| descriptor.name() == name) else {
        return protocol::error(
            id,
            error_code::METHOD_NOT_FOUND,
            &format!("unknown tool: {name}"),
        );
    };
    let mut schema: Value = serde_json::from_str(descriptor.input_schema()).unwrap();
    if let Some(agent_schema) = agent_selector_schema(snapshot, name) {
        schema["properties"]["agent"] = agent_schema;
        if let Some(agent) = arguments.get("agent")
            && let Err(message) = validate_agent_selector(snapshot, name, agent)
        {
            return protocol::error(id, error_code::INVALID_PARAMS, &message);
        }
    }
    if matches!(name, "session_create" | "session_delegate_issue") {
        schema["properties"]["runtime"] = RuntimeModelSnapshot::runtime_schema();
    }
    if let Err(ToolError::InvalidParams(message)) = descriptor.validate(&arguments, &schema) {
        return protocol::error(id, error_code::INVALID_PARAMS, &message);
    }
    apply_caller_policy(
        descriptor.caller_policy(),
        &mut arguments,
        caller_credential,
    );
    if matches!(name, "session_create" | "session_delegate_issue")
        && let Err(message) = snapshot.normalize_legacy_agent(&mut arguments)
    {
        return protocol::error(id, error_code::INVALID_PARAMS, &message);
    }
    execute_tool(id, descriptor, arguments, client, caller_credential)
}

fn execute_tool(
    id: Value,
    descriptor: &ToolDescriptor,
    arguments: Value,
    client: &mut dyn DaemonClient,
    caller_credential: Option<&str>,
) -> Value {
    match descriptor.route() {
        ToolRoute::AgentInventory => {
            let Some(workspace) = exact_workspace_id(&arguments) else {
                return protocol::error(
                    id,
                    error_code::INVALID_PARAMS,
                    "workspace_id must be a canonical resource ID",
                );
            };
            daemon_body_response(
                id,
                client.request(DaemonRequest::AgentInventory { workspace }),
            )
        }
        ToolRoute::AgentResume => {
            let operation_id = usagi_core::domain::id::OperationId::new().as_str();
            let request = if let Some(target) = arguments.get("target").cloned() {
                let Ok(target) = serde_json::from_value(target) else {
                    return protocol::error(
                        id,
                        error_code::INVALID_PARAMS,
                        "target must be an exact Agent resume target",
                    );
                };
                DaemonRequest::ResumeAgent {
                    operation_id,
                    target,
                }
            } else {
                DaemonRequest::Session {
                    action: usagi_core::usecase::client::SessionAction::ResumeAgent,
                    operation_id,
                    payload: arguments,
                }
            };
            daemon_body_response(id, client.request(request))
        }
        ToolRoute::Session(action) => {
            let operation_id = usagi_core::domain::id::OperationId::new().as_str();
            session_tool_response(
                id,
                client.request(DaemonRequest::Session {
                    action,
                    operation_id,
                    payload: arguments,
                }),
            )
        }
        ToolRoute::Dispatch(action) => {
            let operation_id = usagi_core::domain::id::OperationId::new().as_str();
            daemon_body_response(
                id,
                client.request(DaemonRequest::DispatchTool {
                    action,
                    operation_id,
                    payload: arguments,
                    caller_context: caller_credential.map(|credential| McpCallerContext {
                        credential: credential.to_owned(),
                    }),
                }),
            )
        }
        ToolRoute::Supervisor(action) => {
            let operation_id = arguments
                .get("idempotency_key")
                .and_then(Value::as_str)
                .map_or_else(
                    || usagi_core::domain::id::OperationId::new().as_str(),
                    ToOwned::to_owned,
                );
            daemon_body_response(
                id,
                client.request(DaemonRequest::SupervisorTool {
                    action,
                    operation_id,
                    payload: arguments,
                    caller_context: caller_credential.map(|credential| McpCallerContext {
                        credential: credential.to_owned(),
                    }),
                }),
            )
        }
        ToolRoute::Store => store_tool_call(id, descriptor, &arguments),
        ToolRoute::Unavailable(reason) => protocol::error(
            id,
            error_code::INTERNAL_ERROR,
            &format!("tool unavailable: {reason}"),
        ),
    }
}

/// The live `agent` selector schema this tool advertises, if it takes one.
///
/// `session_dispatch` targets an existing session, so it may name an Agent that
/// already belongs to it. `session_delegate_brief` creates the session it
/// dispatches into, so it advertises only the new-agent branches: an `agent.id`
/// there can never pass the daemon's ownership check, and rejecting it at the
/// schema keeps the composite operation from ever starting.
fn agent_selector_schema(snapshot: &RuntimeModelSnapshot, tool: &str) -> Option<Value> {
    match tool {
        "session_dispatch" => Some(snapshot.agent_schema()),
        "session_delegate_brief" => Some(snapshot.new_agent_schema()),
        _ => None,
    }
}

fn validate_agent_selector(
    snapshot: &RuntimeModelSnapshot,
    tool: &str,
    agent: &Value,
) -> Result<(), String> {
    if tool == "session_delegate_brief" {
        return snapshot.validate_new_agent(agent);
    }
    snapshot.validate_agent(agent)
}

fn exact_workspace_id(arguments: &Value) -> Option<usagi_core::domain::id::WorkspaceId> {
    arguments
        .get("workspace_id")
        .and_then(Value::as_str)
        .and_then(|value| usagi_core::domain::id::WorkspaceId::parse(value).ok())
}

/// Renders one session-tool reply: an acceptance, a body, or an error that keeps
/// the daemon's machine-readable detail.
fn session_tool_response(id: Value, reply: Result<DaemonReply, ClientError>) -> Value {
    match reply {
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
        Err(error) => protocol::error_with_data(
            id,
            error_code::INTERNAL_ERROR,
            &error.to_string(),
            daemon_error_data(&error),
        ),
    }
}

/// The daemon's machine-readable error detail, plus the side effect it reports,
/// as JSON-RPC `error.data`.
///
/// A message alone cannot tell a caller what to do next when a composite
/// operation failed part-way. `session_delegate_brief` is the case that needs it:
/// whether the session it created was rolled back decides whether the caller
/// retries or reconciles.
fn daemon_error_data(error: &ClientError) -> Option<Value> {
    let ClientError::Protocol(error) = error else {
        return None;
    };
    let details = error.details.clone()?;
    Some(json!({
        "side_effect": error.side_effect,
        "details": details,
    }))
}

fn daemon_body_response(id: Value, reply: Result<DaemonReply, ClientError>) -> Value {
    match reply {
        Ok(DaemonReply::Accepted { body, .. } | DaemonReply::Ok(body)) => protocol::success(
            id,
            json!({"content":[{"type":"text","text":body.to_string()}]}),
        ),
        Err(error) => protocol::error(id, error_code::INTERNAL_ERROR, &error.to_string()),
    }
}

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

#[cfg(test)]
fn normalize_caller_credential(credential: Option<String>) -> Option<String> {
    match credential {
        Some(credential) if !credential.is_empty() => Some(credential),
        Some(_) | None => None,
    }
}

fn apply_caller_policy(
    policy: CallerPolicy,
    arguments: &mut Value,
    caller_credential: Option<&str>,
) {
    if policy == CallerPolicy::SessionCredential
        && let Some(credential) = caller_credential
    {
        arguments["_caller_credential"] = Value::String(credential.to_owned());
    }
}

/// `resources/read` を処理する。`uri` を取り出して resource レジストリを引き、本文を
/// `contents` に包んで返す。`uri` 欠落は `INVALID_PARAMS`、未知 URI は resource が無い旨の
/// `INVALID_PARAMS` を返す。
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
        MAX_STDIO_MESSAGE_BYTES, PREFERRED_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS,
        ServerCapabilities, ServerState, agent_selector_schema, apply_caller_policy,
        daemon_error_data, execute_tool, handle_line, handle_line_with_client,
        normalize_caller_credential, read_bounded_line, resolve_workspace_root,
        runtime_model_snapshot, serve, serve_with_client, serve_with_client_and_features,
        serve_with_client_and_snapshot, session_tool_response,
    };
    use crate::mcp::runtime_model::{
        ExecutableLocator, RuntimeModelSnapshot, WorkspaceAgentConfig,
    };
    use crate::mcp::tool::{CallerPolicy, Tool, ToolDescriptor, ToolError, ToolRoute};
    use crate::mcp::tools::{ToolAvailability, registry};
    use serde_json::Value;
    use std::io::{BufReader, Cursor, ErrorKind, Write};
    use std::path::PathBuf;
    use tempfile::tempdir;
    use usagi_core::usecase::client::{ClientError, DaemonClient, DaemonReply, DaemonRequest};

    struct RecordingClient {
        reply: Result<DaemonReply, ClientError>,
        requests: Vec<DaemonRequest>,
    }

    struct ErrorTool(fn() -> ToolError);
    impl Tool for ErrorTool {
        fn name(&self) -> &'static str {
            "error_fixture"
        }

        fn description(&self) -> &'static str {
            "error mapping fixture"
        }

        fn input_schema(&self) -> &'static str {
            r#"{"type":"object","properties":{}}"#
        }

        fn call(&self, _params: &str) -> Result<String, ToolError> {
            Err((self.0)())
        }
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
        if let Some(agent) = agent_selector_schema(snapshot, name) {
            schema["properties"]["agent"] = agent;
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
                ServerCapabilities {
                    runtime_models: &RuntimeModelSnapshot::default(),
                    tools: ToolAvailability::default(),
                    caller_credential: None,
                },
                &mut state,
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn initialized_input(request: &str) -> String {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"{PREFERRED_PROTOCOL_VERSION}\"}}}}\n{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}\n{request}"
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
    fn runtime_models_are_read_from_the_trusted_workspace_root() {
        let workspace = tempdir().unwrap();
        let session_worktree = tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".usagi")).unwrap();
        std::fs::write(
            workspace.path().join(".usagi/config.toml"),
            "[agents.codex]\nmodels = [\"gpt-5\"]\n",
        )
        .unwrap();

        let workspace_root = resolve_workspace_root(
            session_worktree.path().to_path_buf(),
            Some(workspace.path().to_path_buf()),
        );
        let snapshot = runtime_model_snapshot(&workspace_root, &FakeLocator(&["codex"]));
        let schema = snapshot.agent_schema();
        let branches = schema["oneOf"].as_array().unwrap();

        assert_eq!(branches.len(), 2);
        assert_eq!(branches[1]["properties"]["runtime"]["const"], "codex");
        assert_eq!(
            branches[1]["properties"]["model"]["enum"],
            serde_json::json!(["gpt-5"])
        );
        assert_eq!(
            resolve_workspace_root(session_worktree.path().to_path_buf(), Some(PathBuf::new())),
            session_worktree.path()
        );
    }

    #[test]
    fn initialize_rejects_a_missing_protocol_version() {
        let missing = initialize(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        assert_eq!(missing["error"]["code"], -32602);
        let not_a_string = initialize(
            r#"{"jsonrpc":"2.0","id":"v","method":"initialize","params":{"protocolVersion":7}}"#,
        );
        assert_eq!(not_a_string["id"], "v");
        assert_eq!(not_a_string["error"]["code"], -32602);
    }

    /// MCP requires the server to answer with a version it speaks rather than
    /// to refuse. A client pinned to an older revision must still reach the
    /// tools, so every version this server speaks is echoed back verbatim.
    #[test]
    fn initialize_echoes_every_protocol_version_this_server_speaks() {
        for version in SUPPORTED_PROTOCOL_VERSIONS {
            let accepted = initialize(&format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{version}"}}}}"#
            ));
            assert!(accepted.get("error").is_none(), "{version}: {accepted}");
            assert_eq!(accepted["result"]["protocolVersion"], *version);
            assert!(accepted["result"]["capabilities"]["tools"].is_object());
        }
    }

    /// An unknown version is answered with the preferred one — the counter-offer
    /// the protocol asks for — and the client decides whether to continue.
    #[test]
    fn initialize_counter_offers_its_preferred_version_for_an_unknown_one() {
        let unknown = initialize(
            r#"{"jsonrpc":"2.0","id":9,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        );
        assert!(unknown.get("error").is_none(), "{unknown}");
        assert_eq!(
            unknown["result"]["protocolVersion"],
            PREFERRED_PROTOCOL_VERSION
        );
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
        assert_eq!(tools.len(), 49);
        // 各要素が name / description / inputSchema(object) を持つ。
        for tool in tools {
            assert!(tool["name"].as_str().is_some());
            assert!(tool["description"].as_str().is_some());
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn disabled_tool_families_are_neither_listed_nor_callable() {
        let input = initialized_input(concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"issue_search\",\"arguments\":{}}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"memory_search\",\"arguments\":{}}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"session_delegate_issue\",\"arguments\":{\"number\":1}}}\n",
        ));
        let mut out = Vec::new();
        let mut client = RecordingClient {
            reply: Ok(DaemonReply::Ok(serde_json::json!({"unexpected": true}))),
            requests: vec![],
        };
        serve_with_client_and_features(
            input.as_bytes(),
            &mut out,
            "9.9.9",
            &mut client,
            &RuntimeModelSnapshot::default(),
            ToolAvailability::new(false, false),
        )
        .unwrap();

        let responses = std::str::from_utf8(&out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let names = responses[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 38);
        assert!(names.iter().all(|name| !name.starts_with("issue_")));
        assert!(names.iter().all(|name| !name.starts_with("memory_")));
        assert!(!names.contains(&"session_delegate_issue"));
        for response in &responses[2..] {
            assert_eq!(response["error"]["code"], -32601);
        }
        assert!(client.requests.is_empty());
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
    fn direct_initialized_request_and_legacy_migration_conflict_are_rejected() {
        let initialized_request =
            call(r#"{"jsonrpc":"2.0","id":1,"method":"notifications/initialized"}"#).unwrap();
        assert_eq!(initialized_request["error"]["code"], -32600);

        let conflict = call(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"session_create","arguments":{"name":"one","agent_cli":"claude","runtime":"codex","model":"gpt-5"}}}"#,
        )
        .unwrap();
        assert_eq!(conflict["error"]["code"], -32602);
        assert!(
            conflict["error"]["message"]
                .as_str()
                .unwrap()
                .contains("cannot be combined")
        );
    }

    #[test]
    fn exact_route_validation_and_unavailable_routes_map_protocol_errors() {
        let invalid_workspace = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"agent_resume_inventory","arguments":{"workspace_id":"not-a-resource-id"}}}"#,
        )
        .unwrap();
        assert_eq!(invalid_workspace["error"]["code"], -32602);

        let mut client = RecordingClient {
            reply: Ok(DaemonReply::Ok(serde_json::json!({}))),
            requests: vec![],
        };
        let registry = registry();
        let resume = registry
            .iter()
            .find(|descriptor| descriptor.name() == "session_resume")
            .unwrap();
        let invalid_target = execute_tool(
            serde_json::json!(2),
            resume,
            serde_json::json!({"target":{"continuation": 7}}),
            &mut client,
            None,
        );
        assert_eq!(invalid_target["error"]["code"], -32602);

        let unavailable = ToolDescriptor::new(
            Box::new(ErrorTool(|| ToolError::Unimplemented("unused"))),
            ToolRoute::Unavailable("disabled"),
            CallerPolicy::Public,
        );
        let response = execute_tool(
            serde_json::json!(3),
            &unavailable,
            serde_json::json!({}),
            &mut client,
            None,
        );
        assert_eq!(response["error"]["code"], -32603);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("disabled")
        );
    }

    #[test]
    fn store_error_variants_and_caller_policies_keep_their_mapping() {
        let mut client = RecordingClient {
            reply: Ok(DaemonReply::Ok(serde_json::json!({}))),
            requests: vec![],
        };
        for (tool, expected) in [
            (
                ErrorTool(|| ToolError::UnknownTool("gone".into())),
                crate::mcp::protocol::error_code::METHOD_NOT_FOUND,
            ),
            (
                ErrorTool(|| ToolError::Unimplemented("later")),
                crate::mcp::protocol::error_code::INTERNAL_ERROR,
            ),
            (
                ErrorTool(|| ToolError::InvalidParams("invalid".into())),
                crate::mcp::protocol::error_code::INVALID_PARAMS,
            ),
        ] {
            let descriptor =
                ToolDescriptor::new(Box::new(tool), ToolRoute::Store, CallerPolicy::Public);
            assert_eq!(descriptor.name(), "error_fixture");
            assert_eq!(descriptor.description(), "error mapping fixture");
            assert!(descriptor.input_schema().contains("object"));
            let response = execute_tool(
                serde_json::json!(1),
                &descriptor,
                serde_json::json!({}),
                &mut client,
                None,
            );
            assert_eq!(response["error"]["code"], expected);
        }

        for policy in [
            CallerPolicy::Public,
            CallerPolicy::AgentCredential,
            CallerPolicy::DaemonProvenance,
        ] {
            let mut arguments = serde_json::json!({});
            apply_caller_policy(policy, &mut arguments, Some("secret"));
            assert!(arguments.get("_caller_credential").is_none());
        }
        let mut arguments = serde_json::json!({});
        apply_caller_policy(CallerPolicy::SessionCredential, &mut arguments, None);
        assert!(arguments.get("_caller_credential").is_none());
        apply_caller_policy(
            CallerPolicy::SessionCredential,
            &mut arguments,
            Some("secret"),
        );
        assert_eq!(arguments["_caller_credential"], "secret");

        let registry = registry();
        let dispatch = registry
            .iter()
            .find(|descriptor| descriptor.name() == "session_get")
            .unwrap();
        let response = execute_tool(
            serde_json::json!(2),
            dispatch,
            serde_json::json!({"name":"one"}),
            &mut client,
            Some("secret"),
        );
        assert!(response.get("result").is_some());
        assert!(matches!(
            client.requests.last(),
            Some(DaemonRequest::DispatchTool { caller_context: Some(context), .. })
                if context.credential == "secret"
        ));

        let supervisor = registry
            .iter()
            .find(|descriptor| descriptor.name() == "supervisor_list")
            .unwrap();
        let response = execute_tool(
            serde_json::json!(3),
            supervisor,
            serde_json::json!({}),
            &mut client,
            Some("secret"),
        );
        assert!(response.get("result").is_some());
        assert!(matches!(
            client.requests.last(),
            Some(DaemonRequest::SupervisorTool { caller_context: Some(context), .. })
                if context.credential == "secret"
        ));

        assert_eq!(normalize_caller_credential(None), None);
        assert_eq!(normalize_caller_credential(Some(String::new())), None);
        assert_eq!(
            normalize_caller_credential(Some("secret".into())),
            Some("secret".into())
        );
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
    fn bounded_stdio_reader_accepts_payload_at_limit_and_rejects_limit_plus_one() {
        for payload_len in [MAX_STDIO_MESSAGE_BYTES - 1, MAX_STDIO_MESSAGE_BYTES] {
            let mut input = vec![b' '; payload_len];
            input.push(b'\n');
            let mut reader = Cursor::new(input);
            let mut buf = Vec::new();
            assert_eq!(
                read_bounded_line(&mut reader, &mut buf).unwrap(),
                payload_len + 1
            );
            assert_eq!(buf.len(), payload_len + 1);
        }

        let mut input = vec![b' '; MAX_STDIO_MESSAGE_BYTES + 1];
        input.push(b'\n');
        let mut reader = Cursor::new(input);
        let mut buf = Vec::new();
        let error = read_bounded_line(&mut reader, &mut buf).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(buf.len(), MAX_STDIO_MESSAGE_BYTES + 1);
    }

    #[test]
    fn boundary_sized_notifications_have_zero_effects_and_oversize_fails_closed() {
        let notification =
            b"{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"session_list\",\"arguments\":{}}}";
        for payload_len in [
            MAX_STDIO_MESSAGE_BYTES - 1,
            MAX_STDIO_MESSAGE_BYTES,
            MAX_STDIO_MESSAGE_BYTES + 1,
        ] {
            let mut input = notification.to_vec();
            input.resize(payload_len, b' ');
            input.push(b'\n');
            let mut output = Vec::new();
            let mut client = RecordingClient {
                reply: Ok(DaemonReply::Ok(serde_json::json!({"effect": true}))),
                requests: vec![],
            };
            let result = serve_with_client_and_snapshot(
                input.as_slice(),
                &mut output,
                "9.9.9",
                &mut client,
                &RuntimeModelSnapshot::default(),
            );
            if payload_len <= MAX_STDIO_MESSAGE_BYTES {
                assert!(result.is_ok());
            } else {
                assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData);
            }
            assert!(output.is_empty());
            assert!(client.requests.is_empty());
        }
    }

    #[test]
    fn unterminated_multichunk_oversize_input_fails_closed_with_bounded_buffer() {
        let input = vec![b'x'; MAX_STDIO_MESSAGE_BYTES + 4096];
        let mut reader = BufReader::with_capacity(17, Cursor::new(input));
        let mut buf = Vec::new();
        let error = read_bounded_line(&mut reader, &mut buf).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(buf.len(), MAX_STDIO_MESSAGE_BYTES + 1);
    }

    #[test]
    fn oversize_invalid_utf8_request_and_notification_have_zero_effects() {
        for prefix in [
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"session_list\",\"arguments\":{}}}".as_slice(),
            b"{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"session_list\",\"arguments\":{}}}".as_slice(),
        ] {
            let mut input = prefix.to_vec();
            input.resize(MAX_STDIO_MESSAGE_BYTES + 1, 0xff);
            input.push(b'\n');
            let mut output = Vec::new();
            let mut client = RecordingClient {
                reply: Ok(DaemonReply::Ok(serde_json::json!({"effect": true}))),
                requests: vec![],
            };
            let error = serve_with_client_and_snapshot(
                BufReader::with_capacity(31, Cursor::new(input)),
                &mut output,
                "9.9.9",
                &mut client,
                &RuntimeModelSnapshot::default(),
            )
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData);
            assert!(output.is_empty());
            assert!(client.requests.is_empty());
        }
    }

    #[test]
    fn raw_stdio_negotiates_version_and_enforces_lifecycle_without_effects() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"session_list\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"session_list\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{}}\n",
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
        serve_with_client_and_snapshot(
            input.as_bytes(),
            &mut output,
            "9.9.9",
            &mut client,
            &RuntimeModelSnapshot::default(),
        )
        .unwrap();
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
    fn non_utf8_parse_error_propagates_output_failure() {
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(ErrorKind::BrokenPipe, "closed"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::new(ErrorKind::BrokenPipe, "closed"))
            }
        }

        let mut writer = FailingWriter;
        assert_eq!(writer.flush().unwrap_err().kind(), ErrorKind::BrokenPipe);
        let error = serve([0xff, b'\n'].as_slice(), &mut writer, "9.9.9").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::BrokenPipe);
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
            serve_with_client_and_snapshot(
                input.as_bytes(),
                &mut out,
                "9.9.9",
                &mut client,
                &RuntimeModelSnapshot::default(),
            )
            .unwrap();
            assert_eq!(client.requests.len(), 1);
            assert!(String::from_utf8(out).unwrap().contains("content"));
        }
    }

    #[test]
    fn agent_resume_tools_forward_safe_exact_wire_requests() {
        use usagi_core::domain::{
            agent::AgentResumeTarget,
            id::{
                AgentContinuationRef, AgentResumeSourceId, AgentRuntimeId, SessionId, WorkspaceId,
                WorktreeId,
            },
        };

        let workspace = WorkspaceId::new();
        let target = AgentResumeTarget {
            continuation: AgentContinuationRef::new(),
            source: AgentResumeSourceId::new(),
            workspace_id: workspace,
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
            runtime_id: AgentRuntimeId::new(),
            adapter_revision: 1,
        };
        for (name, arguments) in [
            (
                "agent_resume_inventory",
                serde_json::json!({"workspace_id": workspace}),
            ),
            (
                "session_resume",
                serde_json::json!({"target": target.clone()}),
            ),
            ("session_resume", serde_json::json!({"name": "legacy"})),
        ] {
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{arguments}}}}}"#
            ) + "\n";
            let input = initialized_input(&request);
            let mut out = Vec::new();
            let mut client = RecordingClient {
                reply: Ok(DaemonReply::Ok(serde_json::json!({"safe":true}))),
                requests: vec![],
            };
            serve_with_client(input.as_bytes(), &mut out, "9.9.9", &mut client).unwrap();
            assert_eq!(client.requests.len(), 1);
            let actual = &client.requests[0];
            assert!(
                (name == "agent_resume_inventory"
                    && matches!(actual, DaemonRequest::AgentInventory { workspace: actual } if *actual == workspace))
                    || (arguments.get("target").is_some()
                        && matches!(actual, DaemonRequest::ResumeAgent { target: actual, .. } if actual == &target))
                    || (arguments.get("target").is_none()
                        && matches!(
                            actual,
                            DaemonRequest::Session {
                                action: usagi_core::usecase::client::SessionAction::ResumeAgent,
                                payload,
                                ..
                            } if payload == &arguments
                        ))
            );
            assert!(String::from_utf8(out).unwrap().contains("safe"));
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
            // `session_delegate_brief` advertises only runtime/model selectors,
            // so its arguments are satisfiable only against a snapshot that has
            // at least one available runtime.
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
                reply: Ok(DaemonReply::Ok(serde_json::json!({"connected":true}))),
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
            // A lone existing-agent selector is refused too: the session this
            // call creates cannot already own an Agent, so admitting it would
            // build a worktree only to reject it afterwards (#611).
            r#"{"brief":"triage","agent":{"id":"existing"}}"#,
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

    /// A composite operation that failed part-way must reach the caller with the
    /// state it needs to act on, not just a sentence (#611).
    #[test]
    fn a_partial_daemon_failure_reaches_the_caller_as_error_data() {
        use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, SideEffect};

        let mut protocol =
            ProtocolError::new(ErrorCode::OwnershipUnknown, "could not be completed");
        protocol.side_effect = SideEffect::PartialOrUnknown;
        protocol.details = Some(serde_json::json!({"reconcile":"retained"}));
        let data = daemon_error_data(&ClientError::Protocol(protocol.clone())).unwrap();
        assert_eq!(data["side_effect"], "partial_or_unknown");
        assert_eq!(data["details"]["reconcile"], "retained");

        // The rendered session-tool response carries it as JSON-RPC `error.data`.
        let rendered = session_tool_response(
            serde_json::json!(1),
            Err(ClientError::Protocol(protocol.clone())),
        );
        assert_eq!(
            rendered["error"]["code"],
            crate::mcp::protocol::error_code::INTERNAL_ERROR
        );
        assert_eq!(
            rendered["error"]["data"]["details"]["reconcile"],
            "retained"
        );

        // An error the daemon did not annotate carries no data, and a transport
        // failure has nothing to reconcile at all.
        protocol.details = None;
        assert!(daemon_error_data(&ClientError::Protocol(protocol)).is_none());
        assert!(daemon_error_data(&ClientError::Unavailable("down".into())).is_none());
        assert!(
            session_tool_response(
                serde_json::json!(1),
                Err(ClientError::Unavailable("down".into()))
            )["error"]
                .get("data")
                .is_none()
        );
    }

    /// The two success shapes a session tool answers with: an acceptance line for
    /// a durably admitted operation, and the body for a completed one.
    #[test]
    fn session_tool_responses_render_acceptance_and_body() {
        let accepted = session_tool_response(
            serde_json::json!(1),
            Ok(DaemonReply::Accepted {
                operation_id: "op".into(),
                revision: 4,
                body: serde_json::json!({}),
            }),
        );
        assert_eq!(
            accepted["result"]["content"][0]["text"],
            "accepted operation op (revision 4)"
        );

        let body = session_tool_response(
            serde_json::json!(1),
            Ok(DaemonReply::Ok(serde_json::json!({"name":"one"}))),
        );
        assert_eq!(body["result"]["content"][0]["text"], r#"{"name":"one"}"#);
    }

    /// The two dispatching tools do not share one `agent` schema: only
    /// `session_dispatch` can honour an existing Agent (#611).
    #[test]
    fn tools_list_publishes_an_existing_agent_branch_only_where_it_can_be_honoured() {
        let snapshot = RuntimeModelSnapshot::capture(
            &WorkspaceAgentConfig::from_allowlists(vec!["sonnet".into()], vec![]),
            &FakeLocator(&["claude"]),
        );
        let input = initialized_input("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n");
        let mut out = Vec::new();
        let mut client = RecordingClient {
            reply: Ok(DaemonReply::Ok(serde_json::json!({}))),
            requests: vec![],
        };
        serve_with_client_and_snapshot(input.as_bytes(), &mut out, "9.9.9", &mut client, &snapshot)
            .unwrap();
        let tools = last_response(&out)["result"]["tools"]
            .as_array()
            .unwrap()
            .clone();
        let branches = |name: &str| {
            tools.iter().find(|tool| tool["name"] == name).unwrap()["inputSchema"]["properties"]
                    ["agent"]["oneOf"]
                    .as_array()
                    .unwrap()
                    .clone()
        };

        let dispatch = branches("session_dispatch");
        assert_eq!(dispatch.len(), 2);
        assert_eq!(dispatch[0]["required"], serde_json::json!(["id"]));

        let delegate = branches("session_delegate_brief");
        assert_eq!(delegate.len(), 1);
        assert_eq!(
            delegate[0]["required"],
            serde_json::json!(["runtime", "model"])
        );

        // A tool that takes no agent selector is left untouched.
        assert!(
            agent_selector_schema(&snapshot, "session_create").is_none(),
            "only the dispatching tools carry an agent selector"
        );
    }

    #[test]
    fn tools_list_exposes_sakana_for_dispatch_and_legacy_session_creation() {
        let snapshot = RuntimeModelSnapshot::capture(
            &WorkspaceAgentConfig::from_runtime_allowlists([(
                "sakana-ai",
                vec!["fugu-model".into()],
            )]),
            &FakeLocator(&["codex-fugu"]),
        );
        let input = initialized_input("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n");
        let mut out = Vec::new();
        let mut client = RecordingClient {
            reply: Ok(DaemonReply::Ok(serde_json::json!({}))),
            requests: vec![],
        };
        serve_with_client_and_snapshot(input.as_bytes(), &mut out, "9.9.9", &mut client, &snapshot)
            .unwrap();
        let listed = last_response(&out);
        let tools = listed["result"]["tools"].as_array().unwrap();
        let dispatch = tools
            .iter()
            .find(|tool| tool["name"] == "session_dispatch")
            .unwrap();
        assert_eq!(
            dispatch["inputSchema"]["properties"]["agent"]["oneOf"][1]["properties"]["runtime"]["const"],
            "sakana-ai"
        );
        let create = tools
            .iter()
            .find(|tool| tool["name"] == "session_create")
            .unwrap();
        assert_eq!(
            create["inputSchema"]["properties"]["runtime"]["enum"],
            serde_json::json!(["claude", "codex", "sakana-ai"])
        );
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
