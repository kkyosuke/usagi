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
        Box::new(SessionRecoverLegacy),
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
        Box::new(SessionDispatch),
        Box::new(SessionGet),
        Box::new(AgentList),
        Box::new(AgentGet),
        Box::new(AgentComplete),
        Box::new(AgentFail),
        Box::new(AgentInbox),
        Box::new(UserDecisionRequest),
        Box::new(UserDecisionGet),
    ]
}
pub struct UserDecisionRequest;
impl Tool for UserDecisionRequest {
    fn name(&self) -> &'static str {
        "user_decision_request"
    }
    fn description(&self) -> &'static str {
        "現在の agent run に人間の判断を durable に要求し、待たずに decision ID を返す"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"title":{"type":"string"},"prompt":{"type":"string"},"options":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"label":{"type":"string"},"description":{"type":"string"}},"required":["id","label"],"additionalProperties":false}},"allow_freeform":{"type":"boolean"},"expires_at":{"type":"string"},"idempotency_key":{"type":"string"}},"required":["title","prompt","options"],"additionalProperties":false}"#
    }
}
pub struct UserDecisionGet;
impl Tool for UserDecisionGet {
    fn name(&self) -> &'static str {
        "user_decision_get"
    }
    fn description(&self) -> &'static str {
        "recovery/debug のため現在の agent が所有する decision を取得する。回答 polling の主経路には使わない"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"decision_id":{"type":"string"}},"required":["decision_id"],"additionalProperties":false}"#
    }
}

/// `session_dispatch` — session を upsert して agent に prompt を即時 dispatch する。
pub struct SessionDispatch;
impl Tool for SessionDispatch {
    fn name(&self) -> &'static str {
        "session_dispatch"
    }
    fn description(&self) -> &'static str {
        "session を upsert し、agent に prompt を即時実行させる"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"session":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false},"agent":{"oneOf":[{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false},{"type":"object","properties":{"runtime":{"type":"string"},"model":{"type":"string"}},"required":["runtime","model"],"additionalProperties":false}]},"prompt":{"type":"string"}},"required":["session","agent","prompt"],"additionalProperties":false}"#
    }
}
pub struct SessionGet;
impl Tool for SessionGet {
    fn name(&self) -> &'static str {
        "session_get"
    }
    fn description(&self) -> &'static str {
        "session の agent 一覧と現在または最後の task を返す"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}"#
    }
}
pub struct AgentList;
impl Tool for AgentList {
    fn name(&self) -> &'static str {
        "agent_list"
    }
    fn description(&self) -> &'static str {
        "agent を session / status で絞り込み一覧する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"session":{"type":"string"},"status":{"type":"string","enum":["idle","running","exited","failed"]}},"additionalProperties":false}"#
    }
}
pub struct AgentGet;
impl Tool for AgentGet {
    fn name(&self) -> &'static str {
        "agent_get"
    }
    fn description(&self) -> &'static str {
        "agent の run 履歴と結果要約を返す"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"agent_id":{"type":"string"}},"required":["agent_id"],"additionalProperties":false}"#
    }
}
pub struct AgentComplete;
impl Tool for AgentComplete {
    fn name(&self) -> &'static str {
        "agent_complete"
    }
    fn description(&self) -> &'static str {
        "現在の run の成功を caller inbox へ配送する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"summary":{"type":"string"},"result":{"type":"object","properties":{"pr":{"type":"string"},"commits":{"type":"array","items":{"type":"string"}},"changed_files":{"type":"array","items":{"type":"string"}},"verification":{"type":"string"}},"additionalProperties":false},"run_id":{"type":"string"}},"required":["summary"],"additionalProperties":false}"#
    }
}
pub struct AgentFail;
impl Tool for AgentFail {
    fn name(&self) -> &'static str {
        "agent_fail"
    }
    fn description(&self) -> &'static str {
        "現在の run の失敗を caller inbox へ配送する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"summary":{"type":"string"},"error":{"type":"string"},"run_id":{"type":"string"}},"required":["summary"],"additionalProperties":false}"#
    }
}
pub struct AgentInbox;
impl Tool for AgentInbox {
    fn name(&self) -> &'static str {
        "agent_inbox"
    }
    fn description(&self) -> &'static str {
        "caller 自身の durable inbox を返す"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"since":{"type":"string"},"unread_only":{"type":"boolean"}},"additionalProperties":false}"#
    }
}

/// `session_create` — セッション（worktree）を作成する。
pub struct SessionCreate;

impl Tool for SessionCreate {
    fn name(&self) -> &'static str {
        "session_create"
    }
    fn description(&self) -> &'static str {
        "新しい作業用セッション（隔離された git worktree）を daemon に作らせるときに使う。name 必須。agent_cli は deprecated で、runtime/model を使う。実行と状態の権威は daemon にあり、作成は非同期に受理される。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"},"runtime":{"type":"string","enum":["claude","codex"]},"agent_cli":{"type":"string","deprecated":true},"model":{"type":"string"}},"required":["name"]}"#
    }
}

/// `session_recover_legacy` — explicitly validates legacy sessions and, only
/// with `apply: true`, adopts the complete set into daemon lifecycle state.
pub struct SessionRecoverLegacy;
impl Tool for SessionRecoverLegacy {
    fn name(&self) -> &'static str {
        "session_recover_legacy"
    }
    fn description(&self) -> &'static str {
        "legacy state.json session を検証する。既定は dry-run であり、永続化には apply: true を明示する。通常の daemon restart や sidebar refresh はこの操作を実行しない。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"apply":{"type":"boolean","default":false}},"additionalProperties":false}"#
    }
}

/// `session_list` — セッション一覧を返す（state.json の軽量クエリ）。
pub struct SessionList;

impl Tool for SessionList {
    fn name(&self) -> &'static str {
        "session_list"
    }
    fn description(&self) -> &'static str {
        "存在するセッションの一覧を素早く得るときに使う。daemon の state を軽量に読むだけで、worktree の git 状態などの重い情報は含まない（詳細は session_status）。"
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
        "各セッションの進捗（agent の phase、worktree の status/dirty/merged）を観測するときに使う。委譲したセッションが生存中か・変更が入っているかの判定に使う。"
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
        "既存セッションの agent に指示（プロンプト）を送るときに使う。name と prompt が必須。mode で配送先を選ぶ（auto=daemon が live/queue を判定、queue=起動時キュー、live=実行中端末へ直接）。"
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
        "自セッションの作業完了を親（または root）へ報告するときに使う。message 必須。自セッション内からのみ呼べる。"
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
        "セッションに紐づく PR とそのマージ状態を取得するときに使う。委譲先の成果が基点ブランチに入ったか（done）の検知に使う。"
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
        "不要になったセッション（worktree）を破棄するときに使う。name 必須。未コミットの変更（dirty）がある場合は force が必要。"
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
        "現在のセッションの作業メモを参照するときに使う。自セッション内限定。"
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
        "現在のセッションの作業メモを書き換えるときに使う。空文字を渡すとクリアする。自セッション内限定。"
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
        "現在のセッションのチェックリストを参照するときに使う。自セッション内限定。"
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
        "現在のセッションのチェックリストに項目を追加するときに使う。text は trim され非空必須。自セッション内限定。"
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
        "チェックリスト項目の完了状態や文言を index 指定で更新するときに使う。done と text の少なくとも一方が必須。自セッション内限定。"
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
        "チェックリスト項目を index 指定で削除するときに使う。自セッション内限定。"
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
        "セッションの意思決定ログを参照するときに使う。自セッション内限定。"
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
        "重要な判断を意思決定ログに追記するときに使う。text は trim され非空必須、時刻（at）はサーバが付与する。自セッション内限定。"
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
        "既存の committed issue を新しいセッションに委譲して着手させるときに使う。issue のプロンプト化→session 作成→起動時キュー投入を 1 tool で行う。number 必須。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"},"name":{"type":"string"},"runtime":{"type":"string","enum":["claude","codex"]},"agent_cli":{"type":"string","deprecated":true},"model":{"type":"string"}},"required":["number"]}"#
    }
}

/// `session_delegate_brief` — 事前 issue を要さない起源フローの入口（合成 tool）。
pub struct SessionDelegateBrief;

impl Tool for SessionDelegateBrief {
    fn name(&self) -> &'static str {
        "session_delegate_brief"
    }
    fn description(&self) -> &'static str {
        "事前 issue の無い作業を始めるときに使う。ブリーフ（自由記述の指示）からトリアージ/設計セッションを作成し起動時キューに投入する。brief 必須。委譲先が worktree 内で issue 化する。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"brief":{"type":"string"},"name":{"type":"string"},"runtime":{"type":"string","enum":["claude","codex"]},"agent_cli":{"type":"string","deprecated":true},"model":{"type":"string"}},"required":["brief"]}"#
    }
}
