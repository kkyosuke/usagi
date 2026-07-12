//! issue 系 MCP tool（`.usagi/issues/` のタスク issue 操作）。CLI の `usagi` には
//! 出さないエージェント向けの IF で、CLI コマンドと同じ core usecase を呼ぶ兄弟。

use crate::mcp::tool::Tool;

/// issue 系 tool の一覧。
#[must_use]
pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(IssueCreate),
        Box::new(IssueGet),
        Box::new(IssueToPrompt),
        Box::new(IssueSearch),
        Box::new(IssueUpdate),
        Box::new(IssueDelete),
    ]
}

/// `issue_create` — issue を新規作成する。
pub struct IssueCreate;

impl Tool for IssueCreate {
    fn name(&self) -> &'static str {
        "issue_create"
    }
    fn description(&self) -> &'static str {
        "タスク issue を新規作成する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"title":{"type":"string"},"priority":{"type":"string","enum":["high","medium","low"]},"labels":{"type":"array","items":{"type":"string"}},"dependson":{"type":"array","items":{"type":"integer"}},"related":{"type":"array","items":{"type":"integer"}},"parent":{"type":"integer"},"milestone":{"type":"string"},"body":{"type":"string"}},"required":["title"]}"#
    }
}

/// `issue_get` — issue の詳細を取得する。
pub struct IssueGet;

impl Tool for IssueGet {
    fn name(&self) -> &'static str {
        "issue_get"
    }
    fn description(&self) -> &'static str {
        "番号を指定して issue を取得する（無ければ null）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"}},"required":["number"]}"#
    }
}

/// `issue_to_prompt` — issue を実行プロンプトに整形する。
pub struct IssueToPrompt;

impl Tool for IssueToPrompt {
    fn name(&self) -> &'static str {
        "issue_to_prompt"
    }
    fn description(&self) -> &'static str {
        "issue をエージェント向けの実行プロンプトに整形する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"}},"required":["number"]}"#
    }
}

/// `issue_search` — issue を検索・一覧する（`query` 省略で全件）。
pub struct IssueSearch;

impl Tool for IssueSearch {
    fn name(&self) -> &'static str {
        "issue_search"
    }
    fn description(&self) -> &'static str {
        "issue を全文検索・絞り込みする（query 省略で全件、ready/unmet_deps を付与）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"query":{"type":"string"},"status":{"type":"string","enum":["todo","in-progress","done"]},"priority":{"type":"string","enum":["high","medium","low"]},"label":{"type":"string"},"parent":{"type":"integer"},"milestone":{"type":"string"},"ready":{"type":"boolean"}}}"#
    }
}

/// `issue_update` — issue のメタデータ・本文を更新する。
pub struct IssueUpdate;

impl Tool for IssueUpdate {
    fn name(&self) -> &'static str {
        "issue_update"
    }
    fn description(&self) -> &'static str {
        "issue のメタデータ（status など）・本文を更新する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"},"title":{"type":"string"},"status":{"type":"string","enum":["todo","in-progress","done"]},"priority":{"type":"string","enum":["high","medium","low"]},"labels":{"type":"array","items":{"type":"string"}},"dependson":{"type":"array","items":{"type":"integer"}},"related":{"type":"array","items":{"type":"integer"}},"parent":{"type":["integer","null"]},"milestone":{"type":["string","null"]},"body":{"type":"string"}},"required":["number"]}"#
    }
}

/// `issue_delete` — issue を削除する。
pub struct IssueDelete;

impl Tool for IssueDelete {
    fn name(&self) -> &'static str {
        "issue_delete"
    }
    fn description(&self) -> &'static str {
        "番号を指定して issue を削除する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"}},"required":["number"]}"#
    }
}
