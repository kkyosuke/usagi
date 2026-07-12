//! JSON-RPC 2.0 のエンベロープ（成功・エラー応答）を組み立てるヘルパ。
//!
//! `serve` はリクエストを `serde_json::Value` として素で扱い（派生型を持たず、カバレッジを
//! 単純に保つ）、応答の整形だけをここに集約する。

use serde_json::{Map, Value};

/// JSON-RPC のバージョン文字列。
pub const VERSION: &str = "2.0";

/// JSON-RPC 標準のエラーコード。
pub mod error_code {
    /// 不正な JSON を受け取った。
    pub const PARSE_ERROR: i32 = -32700;
    /// リクエストの形が不正。
    pub const INVALID_REQUEST: i32 = -32600;
    /// メソッド（tool）が存在しない。
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// パラメータが不正。
    pub const INVALID_PARAMS: i32 = -32602;
    /// サーバ内部エラー（未実装 tool を含む）。
    pub const INTERNAL_ERROR: i32 = -32603;
}

/// 成功応答（`result`）を組み立てる。`id` / `result` は move で消費する。
#[must_use]
pub fn success(id: Value, result: Value) -> Value {
    Value::Object(Map::from_iter([
        ("jsonrpc".to_owned(), Value::from(VERSION)),
        ("id".to_owned(), id),
        ("result".to_owned(), result),
    ]))
}

/// エラー応答（`error`）を組み立てる。`id` は move で消費する。
#[must_use]
pub fn error(id: Value, code: i32, message: &str) -> Value {
    let body = Map::from_iter([
        ("code".to_owned(), Value::from(code)),
        ("message".to_owned(), Value::from(message)),
    ]);
    Value::Object(Map::from_iter([
        ("jsonrpc".to_owned(), Value::from(VERSION)),
        ("id".to_owned(), id),
        ("error".to_owned(), Value::Object(body)),
    ]))
}

#[cfg(test)]
mod tests {
    use super::{error, error_code, success};
    use serde_json::json;

    #[test]
    fn success_wraps_result_with_id() {
        let v = success(json!(1), json!({ "ok": true }));
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["ok"], true);
        assert!(v.get("error").is_none());
    }

    #[test]
    fn error_wraps_code_and_message() {
        let v = error(json!("abc"), error_code::METHOD_NOT_FOUND, "nope");
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], "abc");
        assert_eq!(v["error"]["code"], error_code::METHOD_NOT_FOUND);
        assert_eq!(v["error"]["message"], "nope");
        assert!(v.get("result").is_none());
    }
}
