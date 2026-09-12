//! session 系 MCP tool（usagi のセッション操作）。実行と session 状態の権威は daemon に
//! あり、各 tool は daemon への IPC クライアントになる（設計は
//! document/proposals/01-entry-surfaces.md）。委譲系（delegate_*）は既存 tool を順に
//! 呼ぶ合成 tool。note / todo / decision はセッション内限定。

use crate::mcp::tool::{Tool, ToolDescriptor};
use std::sync::OnceLock;
use usagi_core::domain::user_decision::UserDecisionPolicy;
use usagi_core::infrastructure::client::{DispatchToolAction, SessionAction};

/// session 系 tool の一覧（オーケストレーションの delegate_* を含む）。
#[must_use]
pub fn tools() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor::session(SessionCreate, SessionAction::Create),
        ToolDescriptor::session(SessionList, SessionAction::List),
        ToolDescriptor::session(SessionStatus, SessionAction::Status),
        ToolDescriptor::session(SessionPrompt, SessionAction::Prompt),
        ToolDescriptor::session(SessionComplete, SessionAction::Complete),
        ToolDescriptor::session(SessionPr, SessionAction::Pr),
        ToolDescriptor::session(SessionRemove, SessionAction::Remove),
        ToolDescriptor::agent_resume(SessionResume),
        ToolDescriptor::agent_inventory(AgentResumeInventory),
        ToolDescriptor::session(SessionNoteGet, SessionAction::NoteGet),
        ToolDescriptor::session(SessionNoteUpdate, SessionAction::NoteUpdate),
        ToolDescriptor::session(SessionTodoList, SessionAction::TodoList),
        ToolDescriptor::session(SessionTodoAdd, SessionAction::TodoAdd),
        ToolDescriptor::session(SessionTodoUpdate, SessionAction::TodoUpdate),
        ToolDescriptor::session(SessionTodoRemove, SessionAction::TodoRemove),
        ToolDescriptor::session(SessionDecisionList, SessionAction::DecisionList),
        ToolDescriptor::session(SessionDecisionLog, SessionAction::DecisionLog),
        ToolDescriptor::session(SessionDelegateIssue, SessionAction::DelegateIssue),
        ToolDescriptor::session(SessionDelegateBrief, SessionAction::DelegateBrief),
        ToolDescriptor::session(WorkflowStart, SessionAction::WorkflowStart),
        ToolDescriptor::session(WorkflowStatus, SessionAction::WorkflowStatus),
        ToolDescriptor::session(WorkflowInstruct, SessionAction::WorkflowInstruct),
        ToolDescriptor::dispatch(SessionDispatch, DispatchToolAction::Dispatch),
        ToolDescriptor::dispatch(AgentHandoff, DispatchToolAction::AgentHandoff),
        ToolDescriptor::dispatch(AgentPeers, DispatchToolAction::AgentPeers),
        ToolDescriptor::dispatch(AgentMessage, DispatchToolAction::AgentMessage),
        ToolDescriptor::dispatch(AgentMessages, DispatchToolAction::AgentMessages),
        ToolDescriptor::dispatch(AgentMessageAck, DispatchToolAction::AgentMessageAck),
        ToolDescriptor::dispatch(SessionGet, DispatchToolAction::SessionGet),
        ToolDescriptor::dispatch(AgentList, DispatchToolAction::AgentList),
        ToolDescriptor::dispatch(AgentGet, DispatchToolAction::AgentGet),
        ToolDescriptor::dispatch(AgentComplete, DispatchToolAction::AgentComplete),
        ToolDescriptor::dispatch(AgentFail, DispatchToolAction::AgentFail),
        ToolDescriptor::dispatch(AgentInbox, DispatchToolAction::AgentInbox),
        ToolDescriptor::dispatch(AgentInboxAck, DispatchToolAction::AgentInboxAck),
        ToolDescriptor::dispatch(UserDecisionRequest, DispatchToolAction::UserDecisionRequest),
        ToolDescriptor::dispatch(UserDecisionGet, DispatchToolAction::UserDecisionGet),
        ToolDescriptor::dispatch(UserDecisionList, DispatchToolAction::UserDecisionList),
        ToolDescriptor::dispatch(UserDecisionResolve, DispatchToolAction::UserDecisionResolve),
        ToolDescriptor::dispatch(UserDecisionCancel, DispatchToolAction::UserDecisionCancel),
        ToolDescriptor::dispatch(UserDecisionExpire, DispatchToolAction::UserDecisionExpire),
    ]
}
pub struct AgentPeers;
pub struct AgentHandoff;
impl Tool for AgentHandoff {
    fn name(&self) -> &'static str {
        "agent_handoff"
    }
    fn description(&self) -> &'static str {
        "現在の managed session 内で Agent にタスクを委譲する。session の所属・作成者を変更しない。既存の live Agent への会話は agent_message を使う"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"agent":{"oneOf":[{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false},{"type":"object","properties":{"runtime":{"type":"string"},"model":{"type":"string"}},"required":["runtime","model"],"additionalProperties":false}]},"prompt":{"type":"string","minLength":1,"maxLength":16384}},"required":["agent","prompt"],"additionalProperties":false}"#
    }
}
impl Tool for AgentPeers {
    fn name(&self) -> &'static str {
        "agent_peers"
    }
    fn description(&self) -> &'static str {
        "現在の managed session の Agent を列挙する。session の管理権限は共有しない"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{},"additionalProperties":false}"#
    }
}

pub struct AgentMessage;
impl Tool for AgentMessage {
    fn name(&self) -> &'static str {
        "agent_message"
    }
    fn description(&self) -> &'static str {
        "同じ session の Agent へ会話・レビュー依頼・判定を durable に保存する。message_id は UUIDv7 で retry 時に再利用する。実行完了を意味しない"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"message_id":{"type":"string"},"to_agent_id":{"type":"string"},"kind":{"enum":["message","review_request","approved","changes_requested"]},"body":{"type":"string","minLength":1,"maxLength":16384},"in_reply_to":{"type":"string"},"review":{"type":"object","properties":{"base_sha":{"type":"string"},"head_sha":{"type":"string"}},"required":["base_sha","head_sha"],"additionalProperties":false}},"required":["message_id","to_agent_id","kind","body"],"additionalProperties":false}"#
    }
}

pub struct AgentMessages;
impl Tool for AgentMessages {
    fn name(&self) -> &'static str {
        "agent_messages"
    }
    fn description(&self) -> &'static str {
        "自分が送受信した peer message を保存順に読む。after には前ページ末尾の message_id を渡す。未読は受信分だけ。read では ACK しない"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"after":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100},"unread_only":{"type":"boolean"}},"additionalProperties":false}"#
    }
}

pub struct AgentMessageAck;
impl Tool for AgentMessageAck {
    fn name(&self) -> &'static str {
        "agent_message_ack"
    }
    fn description(&self) -> &'static str {
        "処理した受信 peer message を明示的に ACK する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"message_id":{"type":"string"}},"required":["message_id"],"additionalProperties":false}"#
    }
}

pub struct UserDecisionRequest;
impl Tool for UserDecisionRequest {
    fn name(&self) -> &'static str {
        "user_decision_request"
    }
    fn description(&self) -> &'static str {
        "現在の agent run に人間の判断を durable に要求し、pending decision を即時返す。回答は user_decision_get で取得する"
    }
    fn input_schema(&self) -> &'static str {
        static SCHEMA: OnceLock<String> = OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "title": bounded_string_schema(UserDecisionPolicy::TITLE_MAX_BYTES, true),
                    "prompt": bounded_string_schema(UserDecisionPolicy::PROMPT_MAX_BYTES, true),
                    "options": {
                        "type": "array",
                        "maxItems": UserDecisionPolicy::OPTION_COUNT_MAX,
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": bounded_string_schema(UserDecisionPolicy::OPTION_ID_MAX_BYTES, true),
                                "label": bounded_string_schema(UserDecisionPolicy::OPTION_LABEL_MAX_BYTES, true),
                                "description": bounded_string_schema(
                                    UserDecisionPolicy::OPTION_DESCRIPTION_MAX_BYTES,
                                    false,
                                ),
                            },
                            "required": ["id", "label"],
                            "additionalProperties": false,
                        },
                    },
                    "allow_freeform": {"type": "boolean"},
                    "expires_at": {"type": "string"},
                    "idempotency_key": bounded_string_schema(
                        UserDecisionPolicy::IDEMPOTENCY_KEY_MAX_BYTES,
                        true,
                    ),
                },
                "required": ["title", "prompt", "options"],
                "additionalProperties": false,
            })
            .to_string()
        })
    }
}
pub struct UserDecisionGet;
impl Tool for UserDecisionGet {
    fn name(&self) -> &'static str {
        "user_decision_get"
    }
    fn description(&self) -> &'static str {
        "現在の agent が所有する decision を取得する。pending の間は同じ decision_id を polling し、terminal 応答を受け取る"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"decision_id":{"type":"string"}},"required":["decision_id"],"additionalProperties":false}"#
    }
}
pub struct UserDecisionList;
impl Tool for UserDecisionList {
    fn name(&self) -> &'static str {
        "user_decision_list"
    }
    fn description(&self) -> &'static str {
        "現在の agent run が所有する pending decision を返す。TUI は workspace の pending decision を表示する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{},"additionalProperties":false}"#
    }
}
pub struct UserDecisionResolve;
impl Tool for UserDecisionResolve {
    fn name(&self) -> &'static str {
        "user_decision_resolve"
    }
    fn description(&self) -> &'static str {
        "pending decision に option または許可された freeform を一度だけ記録する"
    }
    fn input_schema(&self) -> &'static str {
        static SCHEMA: OnceLock<String> = OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "decision_id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "answer": {
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "kind": {"const": "option"},
                                    "option_id": bounded_string_schema(
                                        UserDecisionPolicy::OPTION_ID_MAX_BYTES,
                                        true,
                                    ),
                                },
                                "required": ["kind", "option_id"],
                                "additionalProperties": false,
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "kind": {"const": "freeform"},
                                    "text": bounded_string_schema(
                                        UserDecisionPolicy::FREEFORM_ANSWER_MAX_BYTES,
                                        true,
                                    ),
                                },
                                "required": ["kind", "text"],
                                "additionalProperties": false,
                            },
                        ],
                    },
                },
                "required": ["decision_id", "answer"],
                "additionalProperties": false,
            })
            .to_string()
        })
    }
}

/// Goal and instruction text share the daemon's bound for durable workflow text.
const WORKFLOW_TEXT_MAX_BYTES: usize = 16 * 1024;

/// The participants a workflow can be started with, spelled the way the rest of
/// the product spells them. Publishing the closed set lets a caller discover the
/// vocabulary from `tools/list` instead of guessing and being refused.
fn participant_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "string",
        "enum": usagi_core::domain::settings::DefaultModel::ALL
            .iter()
            .map(|model| model.selector())
            .collect::<Vec<_>>(),
    })
}

fn bounded_string_schema(maximum: usize, nonempty: bool) -> serde_json::Value {
    let mut schema = serde_json::json!({
        "type": "string",
        "maxLength": maximum,
        "x-maxUtf8Bytes": maximum,
    });
    if nonempty {
        schema["minLength"] = serde_json::json!(1);
    }
    schema
}
pub struct UserDecisionCancel;
impl Tool for UserDecisionCancel {
    fn name(&self) -> &'static str {
        "user_decision_cancel"
    }
    fn description(&self) -> &'static str {
        "pending decision を回答を配送せず cancel する"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"decision_id":{"type":"string"}},"required":["decision_id"],"additionalProperties":false}"#
    }
}
pub struct UserDecisionExpire;
impl Tool for UserDecisionExpire {
    fn name(&self) -> &'static str {
        "user_decision_expire"
    }
    fn description(&self) -> &'static str {
        "pending decision を回答を配送せず expire する"
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
        "認証済み caller が作成した session を upsert し、agent に prompt を即時実行させる。別 caller や人間が作成した同名 session は再利用できない"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"session":{"type":"object","properties":{"name":{"type":"string"},"role":{"type":"string"}},"required":["name"],"additionalProperties":false},"agent":{"oneOf":[{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false},{"type":"object","properties":{"runtime":{"type":"string"},"model":{"type":"string"}},"required":["runtime","model"],"additionalProperties":false}]},"prompt":{"type":"string"}},"required":["session","agent","prompt"],"additionalProperties":false}"#
    }
}
pub struct SessionGet;
impl Tool for SessionGet {
    fn name(&self) -> &'static str {
        "session_get"
    }
    fn description(&self) -> &'static str {
        "認証済み caller が作成した session の agent 一覧と現在または最後の task を返す"
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
        "認証済み caller が作成した session に属する agent を session / status で絞り込み一覧する"
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
        "認証済み caller が作成した session に属する agent の run 履歴と結果要約を返す"
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
        "caller 自身の durable inbox をACKせずbounded pageで返す"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"cursor":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":100},"since":{"type":"string"},"unread_only":{"type":"boolean"}},"additionalProperties":false}"#
    }
}
pub struct AgentInboxAck;
impl Tool for AgentInboxAck {
    fn name(&self) -> &'static str {
        "agent_inbox_ack"
    }
    fn description(&self) -> &'static str {
        "agent_inboxで処理済みのnext_cursorまでをdurableにACKする"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"cursor":{"type":"integer","minimum":1}},"required":["cursor"],"additionalProperties":false}"#
    }
}

/// `session_create` — セッション（worktree）を作成する。
pub struct SessionCreate;

impl Tool for SessionCreate {
    fn name(&self) -> &'static str {
        "session_create"
    }
    fn description(&self) -> &'static str {
        "新しい作業用セッション（隔離された git worktree）を daemon に作らせるときに使う。認証済み Agent が作成した session はその exact caller だけが管理できる。name 必須。base_ref は任意の fully-qualified local/remote-tracking branch ref。agent_cli は deprecated で、runtime/model を使う。実行と状態の権威は daemon にあり、worktree 作成と lifecycle store 更新が完了してから応答する。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"},"role":{"type":"string"},"base_ref":{"type":"string"},"runtime":{"type":"string"},"agent_cli":{"type":"string","deprecated":true},"model":{"type":"string"}},"required":["name"]}"#
    }
}

/// `session_resume` — explicitly starts a new daemon-owned Agent runtime for
/// retained provider-native conversation metadata.
pub struct SessionResume;
impl Tool for SessionResume {
    fn name(&self) -> &'static str {
        "session_resume"
    }
    fn description(&self) -> &'static str {
        "agent_resume_inventory が返した exact target を指定し、認証済み caller が作成した session の中断 runtime を再開する。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"target":{"type":"object","properties":{"continuation":{"type":"string"},"source":{"type":"string"},"workspace_id":{"type":"string"},"session_id":{"type":["string","null"]},"worktree_id":{"type":"string"},"runtime_id":{"type":"string"},"adapter_revision":{"type":"integer","minimum":1}},"required":["continuation","source","workspace_id","session_id","worktree_id","runtime_id","adapter_revision"],"additionalProperties":false}},"required":["target"],"additionalProperties":false}"#
    }
}

/// `agent_resume_inventory` — returns safe exact targets for root and managed
/// Agent histories in one workspace.
pub struct AgentResumeInventory;
impl Tool for AgentResumeInventory {
    fn name(&self) -> &'static str {
        "agent_resume_inventory"
    }
    fn description(&self) -> &'static str {
        "認証済み caller が作成した managed session に属する Agent runtime と exact resume target を provider ID なしで列挙する。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"workspace_id":{"type":"string"}},"required":["workspace_id"],"additionalProperties":false}"#
    }
}

/// `session_list` — セッション一覧を返す（state.json の軽量クエリ）。
pub struct SessionList;

impl Tool for SessionList {
    fn name(&self) -> &'static str {
        "session_list"
    }
    fn description(&self) -> &'static str {
        "認証済み Agent が自分で作成したセッションの一覧を素早く得るときに使う。daemon の state を軽量に読むだけで、worktree の git 状態などの重い情報は含まない（詳細は session_status）。"
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
        "認証済み Agent が自分で作成した各セッションの進捗（agent の phase、worktree の status/dirty/merged）を観測するときに使う。委譲したセッションが生存中か・変更が入っているかの判定に使う。"
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
        "認証済み caller が作成した実行中の session agent に追加指示を送る。name と prompt は必須。live（既定）は live agent が無ければ失敗し、agent を起動したい場合は session_dispatch を使う。queue は次回起動まで待たせることを意図した場合だけ使う。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"},"prompt":{"type":"string"},"mode":{"type":"string","enum":["queue","live"]},"agent_cli":{"type":"string"},"model":{"type":"string"}},"required":["name","prompt"]}"#
    }
}

/// `session_complete` — dispatch 元の直近 caller へ完了を報告する（セッション内限定）。
pub struct SessionComplete;

impl Tool for SessionComplete {
    fn name(&self) -> &'static str {
        "session_complete"
    }
    fn description(&self) -> &'static str {
        "dispatch された自セッションの作業完了を、保存済み binding が示す直近 caller の durable inbox へ報告するときに使う。message 必須。宛先を推測せず、自セッション内からのみ呼べる。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}"#
    }
}

/// `workflow_start` — セッションの実装＋レビュー workflow を開始する。
pub struct WorkflowStart;

impl Tool for WorkflowStart {
    fn name(&self) -> &'static str {
        "workflow_start"
    }
    fn description(&self) -> &'static str {
        "認証済み caller が作成したセッションで、実装＋レビューの workflow を開始するときに使う。name と goal は必須。実装担当が計画担当とレビュー担当を同じセッション内で起動し、レビュー承認と PR の独立検証まで daemon が進行を所有する。進行状況は workflow_status で観測する。自分自身が動いているセッションに対しては呼べない。planner / implementer / reviewer は省略時に workspace が最後に開始できた組合せを使い、未知の綴りは拒否する。"
    }
    fn input_schema(&self) -> &'static str {
        static SCHEMA: OnceLock<String> = OnceLock::new();
        SCHEMA
            .get_or_init(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "goal": bounded_string_schema(WORKFLOW_TEXT_MAX_BYTES, true),
                        "planner": participant_schema(),
                        "implementer": participant_schema(),
                        "reviewer": participant_schema(),
                    },
                    "required": ["name", "goal"],
                    "additionalProperties": false,
                })
                .to_string()
            })
            .as_str()
    }
}

/// `workflow_status` — セッションの workflow の進捗を取得する。
pub struct WorkflowStatus;

impl Tool for WorkflowStatus {
    fn name(&self) -> &'static str {
        "workflow_status"
    }
    fn description(&self) -> &'static str {
        "認証済み caller が作成したセッションの workflow の進捗（工程・担当・修正回数・待ち理由・PR）を観測するときに使う。name 必須。工程が Needs attention（判断待ち）か PR ready（完了）なら人の判断が要る。1 回ごとに PR の実検証（git と gh）を伴うため、密なポーリングはしない。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false}"#
    }
}

/// `workflow_instruct` — 進行中の workflow へ追加指示を送る。
pub struct WorkflowInstruct;

impl Tool for WorkflowInstruct {
    fn name(&self) -> &'static str {
        "workflow_instruct"
    }
    fn description(&self) -> &'static str {
        "進行中の workflow へ追加指示を送るときに使う。name と body は必須。recipient は automatic（既定、現在の担当）/ implementer / reviewer。指示は受理時点の担当に固定され、工程が変わっても付け替えない。応答を得られなかった場合に呼び直すと別の指示として積まれるため、同じ内容を繰り返さない。"
    }
    fn input_schema(&self) -> &'static str {
        static SCHEMA: OnceLock<String> = OnceLock::new();
        SCHEMA
            .get_or_init(|| {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "body": bounded_string_schema(WORKFLOW_TEXT_MAX_BYTES, true),
                        "recipient": {
                            "type": "string",
                            "enum": ["automatic", "implementer", "reviewer"],
                        },
                    },
                    "required": ["name", "body"],
                    "additionalProperties": false,
                })
                .to_string()
            })
            .as_str()
    }
}

/// `session_pr` — セッションに紐づく PR を取得する。
pub struct SessionPr;

impl Tool for SessionPr {
    fn name(&self) -> &'static str {
        "session_pr"
    }
    fn description(&self) -> &'static str {
        "セッションに紐づく PR とそのマージ状態を取得するときに使う。name 省略時は認証済み caller 自身のセッション、指定時は caller が作成したセッションだけを読む。委譲先の成果が基点ブランチに入ったか（done）の検知にも使う。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"}},"additionalProperties":false}"#
    }
}

/// `session_remove` — セッション（worktree）を削除する。
pub struct SessionRemove;

impl Tool for SessionRemove {
    fn name(&self) -> &'static str {
        "session_remove"
    }
    fn description(&self) -> &'static str {
        "認証済み caller が作成した不要なセッション（worktree）を破棄するときに使う。name 必須。未コミットの変更（dirty）がある場合は force が必要。integrity orphan の診断不能な残骸や未マージ commit も破棄するときだけ force と purge_orphan を両方指定する。応答は受理で、worktree の撤去は daemon が続ける。完了は session_list で観測する（deleting=進行中 / 消滅=完了 / failed=失敗と理由）。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"name":{"type":"string"},"force":{"type":"boolean"},"purge_orphan":{"type":"boolean"}},"required":["name"]}"#
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
        "既存の committed issue を caller 所有の新しいセッションに委譲して着手させるときに使う。issue のプロンプト化→session 作成→起動時キュー投入を 1 tool で行う。number 必須。同番号 source が複数ある場合は委譲を拒否し、session を作成しない。"
    }
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"number":{"type":"integer"},"name":{"type":"string"},"role":{"type":"string"},"runtime":{"type":"string"},"agent_cli":{"type":"string","deprecated":true},"model":{"type":"string"}},"required":["number"]}"#
    }
}

/// `session_delegate_brief` — 事前 issue を要さない起源フローの入口（合成 tool）。
pub struct SessionDelegateBrief;

impl Tool for SessionDelegateBrief {
    fn name(&self) -> &'static str {
        "session_delegate_brief"
    }
    fn description(&self) -> &'static str {
        "事前 issue の無い作業を始めるときに使う。ブリーフから caller 所有のトリアージ/設計セッションを作成し、選択した agent へ直ちに実行を dispatch する。brief と agent（runtime と model）必須。既存 agent の id は指定できない（session はこの呼び出しが作るため）。委譲先が worktree 内で issue 化する。"
    }
    // `agent` は runtime/model だけの新規 agent selector である。作成前の session に
    // 既存 Agent が所属することはないため、`id` branch は公開しない（#611）。
    fn input_schema(&self) -> &'static str {
        r#"{"type":"object","properties":{"brief":{"type":"string"},"name":{"type":"string"},"role":{"type":"string"},"agent":{"oneOf":[{"type":"object","properties":{"runtime":{"type":"string"},"model":{"type":"string"}},"required":["runtime","model"],"additionalProperties":false}]}},"required":["brief","agent"],"additionalProperties":false}"#
    }
}
