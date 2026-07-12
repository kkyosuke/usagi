//! MCP サーバ（`usagi mcp`）の presentation。エージェント向けの tool 面（IF）を持つ:
//! どんな tool・入力があるかを `Tool` トレイト実装のレジストリ（`tools`）で定義し、
//! dispatch は名前でレジストリを引いて `Tool::call` を呼ぶ一様な経路にする。
//!
//! stdio 上の JSON-RPC 2.0 の serve ループ（`initialize` / `tools/list` / `tools/call`）は
//! [`serve`] が担う。`tools/list` と `initialize` は実際に応答し、`tools/call` は tool を
//! 名前で引いて呼ぶ（各 tool の `call` は未実装スタブなので今は「未実装」エラーを返す）。
//! ロジックは usagi-core の usecase（issue / memory）と daemon への IPC（session）へ
//! 委譲する方針で、CLI のコマンドハンドラと同じ core usecase を呼ぶ兄弟。

pub mod protocol;
pub mod serve;
pub mod tool;
pub mod tools;

pub use serve::serve;
use tool::ToolError;

/// tool 名でレジストリを引いて実行する（`tools/call` の実体）。
///
/// # Errors
///
/// 未知の tool 名なら [`ToolError::UnknownTool`]、tool の実行が失敗すればそのエラーを返す。
pub fn dispatch(name: &str, params: &str) -> Result<String, ToolError> {
    let registry = tools::registry();
    let tool = registry
        .iter()
        .find(|tool| tool.name() == name)
        .ok_or_else(|| ToolError::UnknownTool(name.to_owned()))?;
    tool.call(params)
}

#[cfg(test)]
mod tests {
    use super::{ToolError, dispatch};

    #[test]
    fn dispatch_routes_to_a_known_tool() {
        // 枠だけなので既知の tool は未実装スタブを返す（配線が通っていることの確認）。
        assert_eq!(
            dispatch("session_create", "{}"),
            Err(ToolError::Unimplemented("session_create"))
        );
    }

    #[test]
    fn dispatch_rejects_unknown_tool() {
        assert!(matches!(
            dispatch("does_not_exist", "{}"),
            Err(ToolError::UnknownTool(name)) if name == "does_not_exist"
        ));
    }
}
