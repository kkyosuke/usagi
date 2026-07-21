//! issue 系 MCP tool（`.usagi/issues/` のタスク issue 操作）。CLI の `usagi` には
//! 出さないエージェント向けの IF で、CLI コマンドと同じ core usecase を呼ぶ兄弟。
#![coverage(off)] // LLVM duplicates the serde/tool instantiations across lib and production E2E binaries.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use usagi_core::domain::issue::{Issue, IssueSummary};
use usagi_core::infrastructure::store::issue::IssueStore;
use usagi_core::usecase::issue::{self, IssueFilter, IssuePatch, ListedIssue, NewIssue};

use crate::mcp::tool::{Tool, ToolError};

fn store() -> IssueStore {
    IssueStore::new(
        std::env::current_dir().expect("MCP server already resolved its cwd at startup"),
    )
}

fn writable_store() -> anyhow::Result<IssueStore> {
    let root = std::env::current_dir().expect("MCP server already resolved its cwd at startup");
    issue::ensure_write_allowed(&root)?;
    Ok(IssueStore::new(root))
}

fn invalid_params(error: &serde_json::Error) -> ToolError {
    ToolError::InvalidParams(error.to_string())
}

fn execution_error(error: &anyhow::Error) -> ToolError {
    ToolError::Execution(error.to_string())
}

#[derive(Serialize)]
struct IssueView<'a> {
    number: u32,
    title: &'a str,
    status: usagi_core::domain::issue::IssueStatus,
    priority: usagi_core::domain::issue::IssuePriority,
    labels: &'a [String],
    dependson: &'a [u32],
    related: &'a [u32],
    parent: Option<u32>,
    milestone: Option<&'a str>,
    created_at: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
    body: &'a str,
}

impl<'a> From<&'a Issue> for IssueView<'a> {
    fn from(issue: &'a Issue) -> Self {
        Self {
            number: issue.number,
            title: &issue.title,
            status: issue.status,
            priority: issue.priority,
            labels: &issue.labels,
            dependson: &issue.dependson,
            related: &issue.related,
            parent: issue.parent,
            milestone: issue.milestone.as_deref(),
            created_at: issue.created_at,
            updated_at: issue.updated_at,
            body: &issue.body,
        }
    }
}

#[derive(Serialize)]
struct ListedIssueView<'a> {
    #[serde(flatten)]
    summary: &'a IssueSummary,
    ambiguous: bool,
    ready: bool,
    unmet_deps: &'a [u32],
}

#[derive(Serialize)]
struct PromptView<'a> {
    number: u32,
    title: &'a str,
    prompt: String,
}

impl<'a> From<&'a ListedIssue> for ListedIssueView<'a> {
    fn from(issue: &'a ListedIssue) -> Self {
        Self {
            summary: &issue.summary,
            ambiguous: issue.ambiguous,
            ready: issue.is_ready(),
            unmet_deps: &issue.unmet_deps,
        }
    }
}

#[derive(Deserialize)]
struct NumberArgs {
    number: u32,
}

#[derive(Deserialize)]
struct SearchArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(flatten)]
    filter: IssueFilter,
}

#[derive(Deserialize)]
struct UpdateArgs {
    number: u32,
    #[serde(flatten)]
    patch: IssuePatch,
}

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
        "着手前の作業を backlog 化するときに使う。title 必須で番号を採番し、作成した issue を返す（status は既定で todo）。同じ作成要求の retry が重複番号へ解決される場合は ambiguity error で停止する。issue ファイルは git 追跡下のため、root/main のチェックアウトでは拒否され、session worktree 内でのみ書き込める。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"title":{"type":"string"},"priority":{"type":"string","enum":["high","medium","low"]},"labels":{"type":"array","items":{"type":"string"}},"dependson":{"type":"array","items":{"type":"integer"}},"related":{"type":"array","items":{"type":"integer"}},"parent":{"type":"integer"},"milestone":{"type":"string"},"body":{"type":"string"}},"required":["title"]}"#
    }
    fn call(&self, params: &str) -> Result<String, ToolError> {
        let store = writable_store().map_err(|error| execution_error(&error))?;
        let spec: NewIssue =
            serde_json::from_str(params).map_err(|error| invalid_params(&error))?;
        let created =
            issue::create(&store, spec, Utc::now()).map_err(|error| execution_error(&error))?;
        Ok(serde_json::to_string_pretty(&IssueView::from(&created))
            .expect("MCP wire views must serialize"))
    }
}

/// `issue_get` — issue の詳細を取得する。
pub struct IssueGet;

impl Tool for IssueGet {
    fn name(&self) -> &'static str {
        "issue_get"
    }
    fn description(&self) -> &'static str {
        "issue の全文（メタデータ＋本文）を番号で 1 件参照するときに使う。存在しない番号なら null を返す。同番号の source Markdown が複数あれば exact path を含む ambiguity error で停止し、任意の 1 件を返さない。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"}},"required":["number"]}"#
    }
    fn call(&self, params: &str) -> Result<String, ToolError> {
        let args: NumberArgs =
            serde_json::from_str(params).map_err(|error| invalid_params(&error))?;
        let issue = issue::get(&store(), args.number).map_err(|error| execution_error(&error))?;
        issue.as_ref().map_or_else(
            || Ok("null".to_owned()),
            |issue| {
                Ok(serde_json::to_string_pretty(&IssueView::from(issue))
                    .expect("MCP wire views must serialize"))
            },
        )
    }
}

/// `issue_to_prompt` — issue を実行プロンプトに整形する。
pub struct IssueToPrompt;

impl Tool for IssueToPrompt {
    fn name(&self) -> &'static str {
        "issue_to_prompt"
    }
    fn description(&self) -> &'static str {
        "issue を agent が着手できる実行プロンプト文字列に整形するときに使う。委譲前の下ごしらえ用で、session_delegate_issue はこの整形を内部で行う。同番号 source が複数あれば ambiguity error で prompt を生成しない。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"}},"required":["number"]}"#
    }
    fn call(&self, params: &str) -> Result<String, ToolError> {
        let args: NumberArgs =
            serde_json::from_str(params).map_err(|error| invalid_params(&error))?;
        let Some(issue) =
            issue::get(&store(), args.number).map_err(|error| execution_error(&error))?
        else {
            return Err(ToolError::Execution(format!("no issue #{}", args.number)));
        };
        Ok(serde_json::to_string_pretty(&PromptView {
            number: issue.number,
            title: &issue.title,
            prompt: issue::to_prompt(&issue),
        })
        .expect("MCP wire views must serialize"))
    }
}

/// `issue_search` — issue を検索・一覧する（`query` 省略で全件）。
pub struct IssueSearch;

impl Tool for IssueSearch {
    fn name(&self) -> &'static str {
        "issue_search"
    }
    fn description(&self) -> &'static str {
        "着手候補や関連 issue を探すときに使う。query の全文検索と status/priority/label/parent/milestone/ready フィルタで絞り込み、各件に番号衝突（ambiguous）・依存充足（ready）・未充足依存（unmet_deps）を付与して返す。query 省略で parse 可能な全 source を返し、同番号 sibling も exact filename の別 row として列挙する。parse 不能な sibling も番号衝突の判定には含める。ambiguous な行とその番号への依存は ready にならない。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"query":{"type":"string"},"status":{"type":"string","enum":["todo","in-progress","done"]},"priority":{"type":"string","enum":["high","medium","low"]},"label":{"type":"string"},"parent":{"type":"integer"},"milestone":{"type":"string"},"ready":{"type":"boolean"}}}"#
    }
    fn call(&self, params: &str) -> Result<String, ToolError> {
        let args: SearchArgs =
            serde_json::from_str(params).map_err(|error| invalid_params(&error))?;
        let issues = issue::search(&store(), args.query.as_deref().unwrap_or(""), &args.filter)
            .map_err(|error| execution_error(&error))?;
        Ok(serde_json::to_string_pretty(
            &issues.iter().map(ListedIssueView::from).collect::<Vec<_>>(),
        )
        .expect("MCP wire views must serialize"))
    }
}

/// `issue_update` — issue のメタデータ・本文を更新する。
pub struct IssueUpdate;

impl Tool for IssueUpdate {
    fn name(&self) -> &'static str {
        "issue_update"
    }
    fn description(&self) -> &'static str {
        "既存 issue のメタデータ（status/priority/labels/dependson など）や本文を変更するときに使う。渡したフィールドだけ更新する。同番号 source が複数あれば ambiguity error で全 sibling を不変に保つ。status を書けるのはその issue を担当する session だけ（単一書き手）で、root/main のチェックアウトでは拒否される。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"},"title":{"type":"string"},"status":{"type":"string","enum":["todo","in-progress","done"]},"priority":{"type":"string","enum":["high","medium","low"]},"labels":{"type":"array","items":{"type":"string"}},"dependson":{"type":"array","items":{"type":"integer"}},"related":{"type":"array","items":{"type":"integer"}},"parent":{"type":["integer","null"]},"milestone":{"type":["string","null"]},"body":{"type":"string"}},"required":["number"]}"#
    }
    fn call(&self, params: &str) -> Result<String, ToolError> {
        let store = writable_store().map_err(|error| execution_error(&error))?;
        let args: UpdateArgs =
            serde_json::from_str(params).map_err(|error| invalid_params(&error))?;
        let Some(updated) = issue::update(&store, args.number, args.patch, Utc::now())
            .map_err(|error| execution_error(&error))?
        else {
            return Err(ToolError::Execution(format!("no issue #{}", args.number)));
        };
        Ok(serde_json::to_string_pretty(&IssueView::from(&updated))
            .expect("MCP wire views must serialize"))
    }
}

/// `issue_delete` — issue を削除する。
pub struct IssueDelete;

impl Tool for IssueDelete {
    fn name(&self) -> &'static str {
        "issue_delete"
    }
    fn description(&self) -> &'static str {
        "不要になった issue を番号指定で削除するときに使う。同番号 source が複数あれば ambiguity error で 1 件も削除しない。git 追跡下のため session worktree 内でのみ実行でき、成功した削除は元に戻せない。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"}},"required":["number"]}"#
    }
    fn call(&self, params: &str) -> Result<String, ToolError> {
        let store = writable_store().map_err(|error| execution_error(&error))?;
        let args: NumberArgs =
            serde_json::from_str(params).map_err(|error| invalid_params(&error))?;
        let deleted =
            issue::delete(&store, args.number).map_err(|error| execution_error(&error))?;
        Ok(serde_json::to_string_pretty(
            &serde_json::json!({"number": args.number, "deleted": deleted}),
        )
        .expect("MCP wire views must serialize"))
    }
}
