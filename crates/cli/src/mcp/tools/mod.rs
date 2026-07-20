//! MCP tool アダプタの置き場。tool は系統ごとにファイルを分け（`issue` / `memory` /
//! `session`）、各 tool が 1 struct として `Tool` を実装する。`mcp/mod.rs` の dispatch が
//! 名前でこのレジストリを引いて `Tool::call` を呼ぶ。
//!
//! 各アダプタは presentation に徹する — store 系は usagi-core の usecase を直接呼び、
//! session 系は usagi-core の IPC クライアント経由で daemon に委譲し、結果を JSON に
//! 整形する（独自のビジネスロジックは持たない）。CLI のコマンドハンドラ
//! （`crate::cli::commands`）は同じ core usecase を呼ぶ兄弟である。

pub mod issue;
pub mod memory;
pub mod session;
pub mod supervisor;

use super::tool::Tool;

/// 公開する全 MCP tool のレジストリ（issue / memory / session を連結）。
#[must_use]
pub fn registry() -> Vec<Box<dyn Tool>> {
    let mut tools = issue::tools();
    tools.extend(memory::tools());
    tools.extend(session::tools());
    tools.extend(supervisor::tools());
    tools
}

#[cfg(test)]
mod tests {
    use super::registry;
    use crate::mcp::tool::ToolError;

    /// 全 tool の IF メタデータが健全である（名前一意・説明非空・スキーマが JSON object・
    /// session / supervisor の既定 `call` は未実装）。store tool は durable effect を持つため、
    /// call の被覆は専用テストに委ねる。
    #[test]
    fn every_tool_has_valid_metadata() {
        let reg = registry();
        assert_eq!(reg.len(), 47); // issue 6 + memory 4 + session 31 + supervisor 6

        let mut seen = std::collections::HashSet::new();
        for tool in &reg {
            let name = tool.name();
            assert!(seen.insert(name));
            assert!(!tool.description().is_empty());

            let schema: serde_json::Value = serde_json::from_str(tool.input_schema()).unwrap();
            assert_eq!(schema["type"], "object");
            assert!(schema.get("properties").is_some());

            if !name.starts_with("issue_") && !name.starts_with("memory_") {
                assert!(matches!(tool.call("{}"), Err(ToolError::Unimplemented(n)) if n == name));
            }
        }
    }

    /// 系統ごとの tool 数を固定する（IF の増減に気づけるように）。
    #[test]
    fn each_category_contributes_its_tools() {
        assert_eq!(super::issue::tools().len(), 6);
        assert_eq!(super::memory::tools().len(), 4);
        assert_eq!(super::session::tools().len(), 31);
        assert_eq!(super::supervisor::tools().len(), 6);
    }
}
