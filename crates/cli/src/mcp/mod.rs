//! MCP サーバ（`usagi mcp`）の presentation。エージェント向けの tool 面（IF）を持つ:
//! どんな tool・入力があるかを `Tool` トレイト実装のレジストリ（`tools`）で定義し、
//! dispatch は名前でレジストリを引いて `Tool::call` を呼ぶ一様な経路にする。
//!
//! stdio 上の JSON-RPC 2.0 フレーミング（serve ループ・`tools/list` / `tools/call` の配線）は
//! 後続で追加する。現状は tool 面の枠（レジストリ + 名前 dispatch）までで、各 tool の
//! `call` は未実装スタブ。ロジックは usagi-core の usecase（issue / memory）と daemon への
//! IPC（session）へ委譲する方針で、CLI のコマンドハンドラと同じ core usecase を呼ぶ兄弟。

pub mod tool;
pub mod tools;

use std::io::Write;

use tool::ToolError;
use usagi_core::domain::AppInfo;

/// tool 名でレジストリを引いて実行する（将来の `tools/call` の実体）。
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

/// MCP 面の起動を示す ready 行を `out` に書き出す。
///
/// # Errors
///
/// `out` への書き込みに失敗した場合、そのエラーを返す。
pub fn write_ready_line(out: &mut impl Write, info: &AppInfo) -> std::io::Result<()> {
    writeln!(out, "{} mcp ready", info.describe())
}

#[cfg(test)]
mod tests {
    use super::{ToolError, dispatch, write_ready_line};
    use usagi_core::domain::AppInfo;

    #[test]
    fn write_ready_line_marks_mcp_surface() {
        let info = AppInfo {
            name: "usagi",
            version: "0.1.0",
        };
        let mut buf = Vec::new();
        write_ready_line(&mut buf, &info).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "usagi v0.1.0 mcp ready\n");
    }

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
