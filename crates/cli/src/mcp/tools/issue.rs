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
        "着手前の作業を backlog 化するときに使う。title 必須で番号を採番し、作成した issue を返す（status は既定で todo）。issue ファイルは git 追跡下のため、root/main のチェックアウトでは拒否され、session worktree 内でのみ書き込める。"
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
        "issue の全文（メタデータ＋本文）を番号で 1 件参照するときに使う。存在しない番号なら null を返す。"
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
        "issue を agent が着手できる実行プロンプト文字列に整形するときに使う。委譲前の下ごしらえ用で、session_delegate_issue はこの整形を内部で行う。"
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
        "着手候補や関連 issue を探すときに使う。query の全文検索と status/priority/label/parent/milestone/ready フィルタで絞り込み、各件に依存充足（ready）と未充足依存（unmet_deps）を付与して返す。query 省略で全件。"
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
        "既存 issue のメタデータ（status/priority/labels/dependson など）や本文を変更するときに使う。渡したフィールドだけ更新する。status を書けるのはその issue を担当する session だけ（単一書き手）で、root/main のチェックアウトでは拒否される。"
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
        "不要になった issue を番号指定で削除するときに使う。git 追跡下のため session worktree 内でのみ実行でき、削除は元に戻せない。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"}},"required":["number"]}"#
    }
}
