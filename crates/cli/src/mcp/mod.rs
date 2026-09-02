//! MCP サーバ（`usagi mcp`）の presentation。エージェント向けの tool 面（IF）を持つ。
//! `tools` の [`tool::ToolDescriptor`] registry が metadata、schema、execution route、
//! caller policy を一つに束ね、dispatch は名前で descriptor を引いて schema を検証してから
//! route ごとの実行先へ送る。
//!
//! stdio 上の JSON-RPC 2.0 の serve ループ（`initialize` / `tools/list` / `tools/call`）は
//! [`serve`] が担う。issue / memory の Store route は接続時に固定した store root を core
//! usecase 経由で操作し、session / agent / terminal / supervisor route は core IPC client を
//! 介して daemon-owned usecase へ委譲する。presentation 自身は business logic を所有しない。

pub mod protocol;
pub mod resources;
pub mod runtime_model;
pub mod serve;
pub mod tool;
pub mod tools;

pub use serve::{
    serve, serve_with_client, serve_with_client_and_caller, serve_with_client_and_caller_at,
    serve_with_client_and_caller_at_roots, serve_with_client_and_features,
    serve_with_client_and_snapshot,
};
use tool::ToolError;

/// tool 名でレジストリを引き、Store route の adapter seam を直接実行する。
///
/// stdio の `tools/call` は [`serve`] が descriptor route を解釈する。daemon route はこの
/// helper を通らない。
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
    let arguments = serde_json::from_str(params)
        .map_err(|error| ToolError::InvalidParams(error.to_string()))?;
    let store_root = current_store_root()?;
    tool.call_store(&arguments, &store_root)
}

#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=dispatch_routes_to_a_known_tool
fn current_store_root() -> Result<std::path::PathBuf, ToolError> {
    std::env::current_dir().map_err(|error| ToolError::Execution(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{ToolError, dispatch};

    #[test]
    fn dispatch_routes_to_a_known_tool() {
        // Store route ではない既知 tool は、この direct helper では未実装になる。
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

    #[test]
    fn dispatch_rejects_malformed_json_before_the_adapter() {
        assert!(matches!(
            dispatch("session_create", "{"),
            Err(ToolError::InvalidParams(_))
        ));
    }
}
