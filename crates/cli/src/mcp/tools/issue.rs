#![cfg_attr(test, coverage(off))]
// coverage: reason=generic_monomorphization owner=root-cli expires=2027-01-31 tests=every_issue_adapter_maps_malformed_json_to_invalid_params,shipping_issue_adapters_cover_defensive_parsing_and_missing_projection,mcp_store_tools_round_trip_through_stdio_and_durable_files

//! issue 系 MCP tool（`.usagi/issues/` のタスク issue 操作）。人間向け CLI には出さない
//! エージェント向け IF で、Store route から core issue usecase を呼ぶ。
use chrono::Utc;
use std::path::Path;
use usagi_core::infrastructure::store::issue::IssueStore;
use usagi_core::usecase::issue::{self, NewIssue};

use super::wire::{execution, output, parse};
use crate::mcp::tool::{Tool, ToolDescriptor, ToolError};

fn store(root: &Path) -> IssueStore {
    IssueStore::new(root)
}

fn writable_store(root: &Path) -> anyhow::Result<IssueStore> {
    issue::ensure_write_allowed(root)?;
    Ok(IssueStore::new(root))
}

use super::issue_wire::{
    IssueView, ListedIssueView, NumberArgs, PromptView, SearchArgs, UpdateArgs,
};

/// issue 系 tool の一覧。
#[must_use]
pub fn tools() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor::workspace_store(IssueCreate),
        ToolDescriptor::workspace_store(IssueGet),
        ToolDescriptor::workspace_store(IssueToPrompt),
        ToolDescriptor::workspace_store(IssueSearch),
        ToolDescriptor::workspace_store(IssueUpdate),
        ToolDescriptor::workspace_store(IssueDelete),
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
    fn call(&self, params: &str, store_root: &Path) -> Result<String, ToolError> {
        let spec: NewIssue = parse(params)?;
        let store = execution(writable_store(store_root))?;
        let created = execution(issue::create(&store, spec, Utc::now()))?;
        Ok(output(&IssueView::from(&created)))
    }
}

/// `issue_get` — issue の詳細を取得する。
pub struct IssueGet;

impl IssueGet {
    pub(super) fn call_with_store(params: &str, store: &IssueStore) -> Result<String, ToolError> {
        let args: NumberArgs = parse(params)?;
        let issue = execution(issue::get(store, args.number))?;
        Ok(output(&issue.as_ref().map(IssueView::from)))
    }
}

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
    fn call(&self, params: &str, store_root: &Path) -> Result<String, ToolError> {
        Self::call_with_store(params, &store(store_root))
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
    fn call(&self, params: &str, store_root: &Path) -> Result<String, ToolError> {
        let args: NumberArgs = parse(params)?;
        let Some(issue) = execution(issue::get(&store(store_root), args.number))? else {
            return Err(ToolError::Execution(format!("no issue #{}", args.number)));
        };
        Ok(output(&PromptView {
            number: issue.number,
            title: &issue.title,
            prompt: issue::to_prompt(&issue),
        }))
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
    fn call(&self, params: &str, store_root: &Path) -> Result<String, ToolError> {
        let args: SearchArgs = parse(params)?;
        let issues = execution(issue::search(
            &store(store_root),
            args.query.as_deref().unwrap_or(""),
            &args.filter,
        ))?;
        Ok(output(
            &issues.iter().map(ListedIssueView::from).collect::<Vec<_>>(),
        ))
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
    fn call(&self, params: &str, store_root: &Path) -> Result<String, ToolError> {
        let args: UpdateArgs = parse(params)?;
        let store = execution(writable_store(store_root))?;
        let Some(updated) = execution(issue::update(&store, args.number, args.patch, Utc::now()))?
        else {
            return Err(ToolError::Execution(format!("no issue #{}", args.number)));
        };
        Ok(output(&IssueView::from(&updated)))
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
    fn call(&self, params: &str, store_root: &Path) -> Result<String, ToolError> {
        let args: NumberArgs = parse(params)?;
        let store = execution(writable_store(store_root))?;
        let deleted = execution(issue::delete(&store, args.number))?;
        Ok(output(
            &serde_json::json!({"number": args.number, "deleted": deleted}),
        ))
    }
}
