//! MCP tool の共通インターフェース。

use std::fmt;

/// MCP tool の実行インターフェース。
///
/// 各 tool は wire 上の名前・説明・入力スキーマ（`tools/list` に載る IF）と、呼び出し方
/// （`call`）を知る。dispatch は「型ごとに分岐する巨大な match」ではなく、名前で
/// レジストリを引いて `call` を呼ぶ一様な経路になる。
///
/// ロジックは usagi-core の usecase（issue / memory）と daemon への IPC（session）へ
/// 委譲する方針で、CLI のコマンドハンドラ（`crate::cli::commands`）と同じ core usecase を
/// 呼ぶ兄弟である。現状は **tool 面の枠だけ**で、`call` は既定実装（未実装を返すスタブ）の
/// ままにし、中身を実装する tool だけがこれをオーバーライドする。
pub trait Tool {
    /// wire 上の tool 名（例: `"issue_create"`）。
    fn name(&self) -> &'static str;

    /// tool の説明（`tools/list` に載る）。
    fn description(&self) -> &'static str;

    /// 入力パラメータの JSON Schema（`tools/list` に載る）。
    fn input_schema(&self) -> &'static str;

    /// tool を実行する。`params` は JSON-RPC の引数（JSON 文字列）、結果も JSON 文字列。
    ///
    /// 既定は未実装スタブ。中身（core usecase 呼び出し・daemon IPC・整形）を実装する
    /// tool はこのメソッドをオーバーライドする。
    ///
    /// # Errors
    ///
    /// 実行に失敗した場合や未実装の場合、`ToolError` を返す。
    fn call(&self, _params: &str) -> Result<String, ToolError> {
        Err(ToolError::Unimplemented(self.name()))
    }
}

/// tool の dispatch・実行のエラー。
#[derive(Debug, PartialEq, Eq)]
pub enum ToolError {
    /// 指定された名前の tool が存在しない。
    UnknownTool(String),
    /// tool の枠だけがあり、中身が未実装。
    Unimplemented(&'static str),
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::UnknownTool(name) => write!(f, "unknown tool `{name}`"),
            ToolError::Unimplemented(name) => write!(f, "tool `{name}` is not yet implemented"),
        }
    }
}

impl std::error::Error for ToolError {}

#[cfg(test)]
mod tests {
    use super::ToolError;

    #[test]
    fn display_covers_both_variants() {
        assert_eq!(
            ToolError::UnknownTool("nope".into()).to_string(),
            "unknown tool `nope`"
        );
        assert_eq!(
            ToolError::Unimplemented("issue_create").to_string(),
            "tool `issue_create` is not yet implemented"
        );
    }

    #[test]
    fn derives_and_error_trait() {
        let err = ToolError::Unimplemented("a");
        assert_eq!(err, ToolError::Unimplemented("a"));
        assert!(format!("{err:?}").contains("Unimplemented"));
        let as_error: &dyn std::error::Error = &err;
        assert!(as_error.to_string().contains("not yet implemented"));
    }
}
