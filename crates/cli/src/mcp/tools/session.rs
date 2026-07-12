//! session 系 MCP tool（usagi のセッション操作）。実行と session 状態の権威は daemon に
//! あり、各 tool は daemon への IPC クライアントになる（設計は
//! document/proposals/01-entry-surfaces.md）。委譲系（delegate_*）は既存 tool を順に
//! 呼ぶ合成 tool。note / todo / decision はセッション内限定。

use crate::mcp::tool::Tool;

/// session 系 tool の一覧（オーケストレーションの delegate_* を含む）。
#[must_use]
pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(SessionCreate),
        Box::new(SessionList),
        Box::new(SessionStatus),
        Box::new(SessionPrompt),
        Box::new(SessionComplete),
        Box::new(SessionPr),
        Box::new(SessionRemove),
        Box::new(SessionNoteGet),
        Box::new(SessionNoteUpdate),
        Box::new(SessionTodoList),
        Box::new(SessionTodoAdd),
        Box::new(SessionTodoUpdate),
        Box::new(SessionTodoRemove),
        Box::new(SessionDecisionList),
        Box::new(SessionDecisionLog),
        Box::new(SessionDelegateIssue),
        Box::new(SessionDelegateBrief),
    ]
}

/// `session_create` — セッション（worktree）を作成する。
pub struct SessionCreate;

impl Tool for SessionCreate {
    fn name(&self) -> &'static str {
        "session_create"
    }
    fn description(&self) -> &'static str {
        "セッション（worktree）を作成する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"},"agent_cli":{"type":"string"},"model":{"type":"string"}},"required":["name"]}"#
    }
}

/// `session_list` — セッション一覧を返す（state.json の軽量クエリ）。
pub struct SessionList;

impl Tool for SessionList {
    fn name(&self) -> &'static str {
        "session_list"
    }
    fn description(&self) -> &'static str {
        "セッション一覧を返す（state.json の軽量クエリ）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{}}"#
    }
}

/// `session_status` — 各セッションの進捗（phase・worktree の git 状態）を返す。
pub struct SessionStatus;

impl Tool for SessionStatus {
    fn name(&self) -> &'static str {
        "session_status"
    }
    fn description(&self) -> &'static str {
        "各セッションの進捗（agent phase・worktree の status/dirty/merged）を返す"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{}}"#
    }
}

/// `session_prompt` — セッションのエージェントにプロンプトを送る。
pub struct SessionPrompt;

impl Tool for SessionPrompt {
    fn name(&self) -> &'static str {
        "session_prompt"
    }
    fn description(&self) -> &'static str {
        "セッションのエージェントにプロンプトを送る（mode で配送先を選ぶ）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"},"prompt":{"type":"string"},"mode":{"type":"string","enum":["auto","queue","live"]},"agent_cli":{"type":"string"},"model":{"type":"string"}},"required":["name","prompt"]}"#
    }
}

/// `session_complete` — 親（または root）へ完了を報告する（セッション内限定）。
pub struct SessionComplete;

impl Tool for SessionComplete {
    fn name(&self) -> &'static str {
        "session_complete"
    }
    fn description(&self) -> &'static str {
        "親（または root）へ完了を報告する（セッション内限定）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}"#
    }
}

/// `session_pr` — セッションに紐づく PR を取得する。
pub struct SessionPr;

impl Tool for SessionPr {
    fn name(&self) -> &'static str {
        "session_pr"
    }
    fn description(&self) -> &'static str {
        "セッションに紐づく PR とマージ状態を取得する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}"#
    }
}

/// `session_remove` — セッション（worktree）を削除する。
pub struct SessionRemove;

impl Tool for SessionRemove {
    fn name(&self) -> &'static str {
        "session_remove"
    }
    fn description(&self) -> &'static str {
        "セッション（worktree）を削除する（dirty があれば force が必要）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"},"force":{"type":"boolean"}},"required":["name"]}"#
    }
}

/// `session_note_get` — 現在のセッションのメモを取得する（セッション内限定）。
pub struct SessionNoteGet;

impl Tool for SessionNoteGet {
    fn name(&self) -> &'static str {
        "session_note_get"
    }
    fn description(&self) -> &'static str {
        "現在のセッションのメモを取得する（セッション内限定）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{}}"#
    }
}

/// `session_note_update` — 現在のセッションのメモを更新する（セッション内限定）。
pub struct SessionNoteUpdate;

impl Tool for SessionNoteUpdate {
    fn name(&self) -> &'static str {
        "session_note_update"
    }
    fn description(&self) -> &'static str {
        "現在のセッションのメモを更新する（空文字でクリア。セッション内限定）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"note":{"type":"string"}},"required":["note"]}"#
    }
}

/// `session_todo_list` — 現在のセッションのチェックリストを返す（セッション内限定）。
pub struct SessionTodoList;

impl Tool for SessionTodoList {
    fn name(&self) -> &'static str {
        "session_todo_list"
    }
    fn description(&self) -> &'static str {
        "現在のセッションのチェックリストを返す（セッション内限定）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{}}"#
    }
}

/// `session_todo_add` — チェックリストに項目を追加する（セッション内限定）。
pub struct SessionTodoAdd;

impl Tool for SessionTodoAdd {
    fn name(&self) -> &'static str {
        "session_todo_add"
    }
    fn description(&self) -> &'static str {
        "チェックリストに項目を追加する（text は trim・非空必須。セッション内限定）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#
    }
}

/// `session_todo_update` — チェックリストの項目を更新する（セッション内限定）。
pub struct SessionTodoUpdate;

impl Tool for SessionTodoUpdate {
    fn name(&self) -> &'static str {
        "session_todo_update"
    }
    fn description(&self) -> &'static str {
        "チェックリストの項目を更新する（done と text の少なくとも一方が必須。セッション内限定）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"index":{"type":"integer"},"done":{"type":"boolean"},"text":{"type":"string"}},"required":["index"]}"#
    }
}

/// `session_todo_remove` — チェックリストの項目を削除する（セッション内限定）。
pub struct SessionTodoRemove;

impl Tool for SessionTodoRemove {
    fn name(&self) -> &'static str {
        "session_todo_remove"
    }
    fn description(&self) -> &'static str {
        "チェックリストの項目を index 指定で削除する（セッション内限定）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"index":{"type":"integer"}},"required":["index"]}"#
    }
}

/// `session_decision_list` — 意思決定ログを返す（セッション内限定）。
pub struct SessionDecisionList;

impl Tool for SessionDecisionList {
    fn name(&self) -> &'static str {
        "session_decision_list"
    }
    fn description(&self) -> &'static str {
        "セッションの意思決定ログを返す（セッション内限定）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{}}"#
    }
}

/// `session_decision_log` — 意思決定ログに追記する（セッション内限定）。
pub struct SessionDecisionLog;

impl Tool for SessionDecisionLog {
    fn name(&self) -> &'static str {
        "session_decision_log"
    }
    fn description(&self) -> &'static str {
        "意思決定ログに追記する（at はサーバが付与。text は trim・非空必須。セッション内限定）"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#
    }
}

/// `session_delegate_issue` — issue を新セッションに委譲して着手させる（合成 tool）。
pub struct SessionDelegateIssue;

impl Tool for SessionDelegateIssue {
    fn name(&self) -> &'static str {
        "session_delegate_issue"
    }
    fn description(&self) -> &'static str {
        "issue をプロンプト化→セッション作成→起動時キュー投入までを 1 tool で行う"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"},"name":{"type":"string"},"agent_cli":{"type":"string"},"model":{"type":"string"}},"required":["number"]}"#
    }
}

/// `session_delegate_brief` — 事前 issue を要さない起源フローの入口（合成 tool）。
pub struct SessionDelegateBrief;

impl Tool for SessionDelegateBrief {
    fn name(&self) -> &'static str {
        "session_delegate_brief"
    }
    fn description(&self) -> &'static str {
        "ブリーフからトリアージ/設計セッションを作成し起動時キューに投入する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"brief":{"type":"string"},"name":{"type":"string"},"agent_cli":{"type":"string"},"model":{"type":"string"}},"required":["brief"]}"#
    }
}
