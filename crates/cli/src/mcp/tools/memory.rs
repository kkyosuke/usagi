//! memory 系 MCP tool（`.usagi/memory/` のエージェントメモリ操作）。CLI の `usagi` には
//! 出さないエージェント向けの IF で、CLI コマンドと同じ core usecase を呼ぶ兄弟。

use chrono::Utc;
use serde::{Deserialize, Serialize};
use usagi_core::domain::memory::{Memory, MemorySummary};
use usagi_core::infrastructure::store::memory::MemoryStore;
use usagi_core::usecase::memory::{self, MemoryFilter, MemoryPatch};

use crate::mcp::tool::{Tool, ToolError};

#[coverage(off)] // Production cwd resolution is exercised by the process E2E.
fn store() -> MemoryStore {
    MemoryStore::new(
        std::env::current_dir().expect("MCP server already resolved its cwd at startup"),
    )
}

fn parse<T: for<'de> Deserialize<'de>>(params: &str) -> Result<T, ToolError> {
    serde_json::from_str(params).map_err(|error| invalid_params(&error))
}

fn invalid_params(error: &serde_json::Error) -> ToolError {
    ToolError::InvalidParams(error.to_string())
}

fn output(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).expect("MCP wire views must serialize")
}

fn execution<T>(result: anyhow::Result<T>) -> Result<T, ToolError> {
    result.map_err(|error| execution_error(&error))
}

fn execution_error(error: &anyhow::Error) -> ToolError {
    ToolError::Execution(error.to_string())
}

#[derive(Serialize)]
struct MemoryView<'a> {
    name: &'a str,
    title: &'a str,
    #[serde(rename = "type")]
    kind: usagi_core::domain::memory::MemoryType,
    related: &'a [String],
    created_at: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
    body: &'a str,
}

impl<'a> From<&'a Memory> for MemoryView<'a> {
    fn from(memory: &'a Memory) -> Self {
        Self {
            name: &memory.name,
            title: &memory.title,
            kind: memory.kind,
            related: &memory.related,
            created_at: memory.created_at,
            updated_at: memory.updated_at,
            body: &memory.body,
        }
    }
}

#[derive(Serialize)]
struct MemorySummaryView<'a> {
    #[serde(flatten)]
    summary: &'a MemorySummary,
}

#[derive(Deserialize)]
struct SaveArgs {
    name: String,
    #[serde(flatten)]
    patch: MemoryPatch,
}

#[derive(Deserialize)]
struct NameArgs {
    name: String,
}

#[derive(Deserialize)]
struct SearchArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(flatten)]
    filter: MemoryFilter,
}

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
    fn call(&self, params: &str) -> Result<String, ToolError> {
        let args: SaveArgs = parse(params)?;
        let saved = execution(memory::save_partial(
            &store(),
            &args.name,
            args.patch,
            Utc::now(),
        ))?;
        Ok(output(&MemoryView::from(&saved)))
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
    fn call(&self, params: &str) -> Result<String, ToolError> {
        let args: NameArgs = parse(params)?;
        let memory = execution(memory::get(&store(), &args.name))?;
        memory.as_ref().map_or_else(
            || Ok("null".to_owned()),
            |memory| Ok(output(&MemoryView::from(memory))),
        )
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
    fn call(&self, params: &str) -> Result<String, ToolError> {
        let args: SearchArgs = parse(params)?;
        let memories = execution(memory::search(
            &store(),
            args.query.as_deref().unwrap_or(""),
            &args.filter,
        ))?;
        Ok(output(
            &memories
                .iter()
                .map(|summary| MemorySummaryView { summary })
                .collect::<Vec<_>>(),
        ))
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
    fn call(&self, params: &str) -> Result<String, ToolError> {
        let args: NameArgs = parse(params)?;
        let deleted = execution(memory::delete(&store(), &args.name))?;
        Ok(output(
            &serde_json::json!({"name": args.name, "deleted": deleted}),
        ))
    }
}
