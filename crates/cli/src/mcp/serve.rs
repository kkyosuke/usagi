//! `usagi mcp` の stdio serve ループ。1 行 = 1 JSON-RPC 2.0 メッセージを読み、
//! `initialize` / `tools/list` / `tools/call` を処理して 1 行の応答を返す。
//!
//! `handle_line`（str → 応答文字列 or なし）に純粋なルーティングを閉じ込め、`serve` は
//! 実 IO（stdin/stdout）の反復だけを担う。実 IO は合成ルートが注入するため、ルーティングは
//! ユニットテストできる。tool 本体は未実装スタブなので、`tools/call` はどの tool も
//! 「未実装」エラーを返し、`tools/list` と `initialize` は実際に応答する。

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

use super::protocol::{self, error_code};
use super::tool::ToolError;
use super::{dispatch, tools};

/// サーバが宣言する MCP プロトコルバージョン。クライアントが `initialize` で別の版を
/// 要求したらそれをエコーし、無ければこの既定を返す。
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// stdin の JSON-RPC を行ごとに処理し、応答を stdout へ書く。EOF で正常終了する。
///
/// `version` は `initialize` の `serverInfo.version` に載せる配布 version（合成ルートが注入）。
///
/// # Errors
///
/// 入力の読み取り、または `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn serve(input: impl BufRead, out: &mut dyn Write, version: &str) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_line(&line, version) {
            writeln!(out, "{response}")?;
        }
    }
    Ok(())
}

/// 1 リクエスト行を処理して応答文字列を返す。通知（`id` 無し）は `None`。
fn handle_line(line: &str, version: &str) -> Option<String> {
    let Ok(request) = serde_json::from_str::<Value>(line) else {
        return Some(
            protocol::error(Value::Null, error_code::PARSE_ERROR, "parse error").to_string(),
        );
    };
    let method = request.get("method").and_then(Value::as_str);
    let id = request.get("id").cloned();
    match (method, id) {
        // 通常のリクエスト（method ＋ id）。
        (Some(method), Some(id)) => {
            Some(respond(method, id, request.get("params"), version).to_string())
        }
        // method が無いのに id がある＝不正リクエスト。
        (None, Some(id)) => {
            Some(protocol::error(id, error_code::INVALID_REQUEST, "missing method").to_string())
        }
        // 通知（method のみ、id 無し）と、method も id も無い行は応答しない。
        (Some(_) | None, None) => None,
    }
}

/// method 別に応答 `Value` を組み立てる。
fn respond(method: &str, id: Value, params: Option<&Value>, version: &str) -> Value {
    match method {
        "initialize" => protocol::success(id, initialize_result(params, version)),
        "ping" => protocol::success(id, json!({})),
        "tools/list" => protocol::success(id, tools_list_result()),
        "tools/call" => tools_call(id, params),
        other => protocol::error(
            id,
            error_code::METHOD_NOT_FOUND,
            &format!("method not found: {other}"),
        ),
    }
}

/// `initialize` の結果（プロトコル版・capabilities・serverInfo）。
fn initialize_result(params: Option<&Value>, version: &str) -> Value {
    let protocol_version = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "usagi", "version": version },
    })
}

/// `tools/list` の結果（全 tool の name / description / inputSchema）。
fn tools_list_result() -> Value {
    let tools: Vec<Value> = tools::registry()
        .iter()
        .map(|tool| {
            // 各 tool の input_schema は妥当な JSON（tools のテストで検証済み）。
            let schema: Value = serde_json::from_str(tool.input_schema()).unwrap();
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "inputSchema": schema,
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// `tools/call` を処理する。現状は全 tool が未実装スタブのため、存在すれば「未実装」、
/// 無ければ「method not found」を返す。
fn tools_call(id: Value, params: Option<&Value>) -> Value {
    let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
        return protocol::error(id, error_code::INVALID_PARAMS, "missing tool name");
    };
    let arguments = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    match dispatch(name, &arguments.to_string()) {
        Err(ToolError::UnknownTool(tool)) => protocol::error(
            id,
            error_code::METHOD_NOT_FOUND,
            &format!("unknown tool: {tool}"),
        ),
        // 全 tool が未実装スタブ。将来 Ok(result) を返す tool は MCP の content に包む。
        _ => protocol::error(
            id,
            error_code::INTERNAL_ERROR,
            &format!("tool not yet implemented: {name}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_PROTOCOL_VERSION, handle_line, serve};
    use serde_json::Value;

    /// 1 行を処理して応答 `Value` を得る（通知は `None`）。
    fn call(line: &str) -> Option<Value> {
        handle_line(line, "9.9.9").map(|s| serde_json::from_str(&s).unwrap())
    }

    #[test]
    fn initialize_echoes_protocol_and_reports_server_version() {
        let v = call(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#).unwrap();
        assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(v["result"]["serverInfo"]["name"], "usagi");
        assert_eq!(v["result"]["serverInfo"]["version"], "9.9.9");
        assert!(v["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_falls_back_to_default_protocol() {
        let v = call(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).unwrap();
        assert_eq!(v["result"]["protocolVersion"], DEFAULT_PROTOCOL_VERSION);
    }

    #[test]
    fn ping_returns_empty_result() {
        let v = call(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#).unwrap();
        assert!(v["result"].is_object());
        assert_eq!(v["id"], 2);
    }

    #[test]
    fn tools_list_returns_every_tool_with_schema() {
        let v = call(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#).unwrap();
        let tools = v["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 27);
        // 各要素が name / description / inputSchema(object) を持つ。
        for tool in tools {
            assert!(tool["name"].as_str().is_some());
            assert!(tool["description"].as_str().is_some());
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn tools_call_known_tool_reports_unimplemented() {
        let v = call(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"issue_create","arguments":{"title":"x"}}}"#).unwrap();
        assert_eq!(v["error"]["code"], -32603);
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("issue_create")
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
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let v = call(r#"{"jsonrpc":"2.0","id":7,"method":"resources/list"}"#).unwrap();
        assert_eq!(v["error"]["code"], -32601);
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
    fn serve_reads_lines_skips_blanks_and_writes_responses() {
        let input = "\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
        let mut out = Vec::new();
        serve(input.as_bytes(), &mut out, "9.9.9").unwrap();
        let text = String::from_utf8(out).unwrap();
        // ping には 1 応答、空行と通知には応答なし＝出力は 1 行。
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"id\":1"));
    }
}
