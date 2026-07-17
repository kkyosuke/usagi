//! memory 系 MCP tool（`.usagi/memory/` のエージェントメモリ操作）。CLI の `usagi` には
//! 出さないエージェント向けの IF で、CLI コマンドと同じ core usecase を呼ぶ兄弟。

use crate::mcp::tool::Tool;

/// memory 系 tool の一覧。
#[must_use]
pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(MemorySave),
        Box::new(MemoryGet),
        Box::new(MemorySearch),
        Box::new(MemoryDelete),
    ]
}

/// `memory_save` — メモリを保存する（upsert。既存は部分更新、新規は `title` 必須）。
pub struct MemorySave;

impl Tool for MemorySave {
    fn name(&self) -> &'static str {
        "memory_save"
    }
    fn description(&self) -> &'static str {
        "セッションをまたいで残す事実（user/feedback/project/reference）を保存するときに使う。upsert で、既存は渡したフィールドだけ部分更新、新規は title 必須。name が識別子になる。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"},"title":{"type":"string"},"type":{"type":"string","enum":["user","feedback","project","reference"]},"related":{"type":"array","items":{"type":"string"}},"body":{"type":"string"}},"required":["name"]}"#
    }
}

/// `memory_get` — メモリを取得する。
pub struct MemoryGet;

impl Tool for MemoryGet {
    fn name(&self) -> &'static str {
        "memory_get"
    }
    fn description(&self) -> &'static str {
        "保存済みメモリを name 指定で 1 件参照するときに使う。存在しない name なら null を返す。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}"#
    }
}

/// `memory_search` — メモリを検索・一覧する（`query` 省略で全件）。
pub struct MemorySearch;

impl Tool for MemorySearch {
    fn name(&self) -> &'static str {
        "memory_search"
    }
    fn description(&self) -> &'static str {
        "関連するメモリを探すときに使う。query の全文検索と type フィルタで絞り込み、updated_at の新しい順に返す。query 省略で全件。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"query":{"type":"string"},"type":{"type":"string","enum":["user","feedback","project","reference"]}}}"#
    }
}

/// `memory_delete` — メモリを削除する。
pub struct MemoryDelete;

impl Tool for MemoryDelete {
    fn name(&self) -> &'static str {
        "memory_delete"
    }
    fn description(&self) -> &'static str {
        "不要・誤りのメモリを name 指定で削除するときに使う。削除は元に戻せない。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}"#
    }
}
