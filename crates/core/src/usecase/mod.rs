//! usecase 層。domain を組み合わせてアプリケーションの操作を表す。
//! TUI 面・daemon 面の両方から呼ばれるロジックだけを置き、
//! v2 では必要になった時点で実装を追加する。
//!
//! - [`issue`] — issue の CRUD（create（採番）/ get / list / update / delete）。
//!   人間向け CLI と MCP tool の双方が呼ぶ。
//! - [`memory`] — memory の CRUD（save（slug・upsert）/ get / list / delete）。
//! - [`env`] — global / workspace の環境変数 binding を子プロセス環境へ解決する
//!   （literal はそのまま、`op://` は注入された [`env::SecretResolver`] 経由）。
//! - [`note`] — session / root の scratchpad 操作（note / todo / decision）を
//!   `state.json` 上で行う（`session_note_* / session_todo_* / session_decision_*`
//!   と TUI が呼ぶ中身）。
//! - [`session`] — git worktree と repo `state.json` を合成する session lifecycle
//!   （create / remove）と state 操作（list / get / touch / record / `remove_record`）。
//! - [`workspace`] — global registry 上の workspace open（path 解決・登録・touch）と、
//!   welcome 画面向け recent overview の構築。
//! - [`workspace_guard`] — エージェントのツール呼び出しを cwd（session / root モード）に応じて
//!   許可判定する純粋ロジック。Claude の `PreToolUse` フックが呼ぶ `guard-workspace` の中身。
//! - [`vt_screen`] — raw PTY バイト列を `rows × cols` の文字グリッドへ解釈する純粋な
//!   VT parser（`VtScreen`）。TUI と daemon が共有する単一 parser authority で、
//!   描画（selection / link / cursor marker）は presentation 側に残す。
//! - [`owner_routing`] — planned restart 中に active / draining の 2 generation が
//!   並存する間、request を owner generation の endpoint へ配送する client 側の
//!   routing（trusted endpoint 解決・inventory merge・generation 別 connection）。
//! - [`agent_phase`] — daemon と TUI が共有する Agent phase の集約分類・順位。

pub mod agent;
pub mod agent_phase;
pub mod claude_sandbox;
pub mod client;
pub mod env;
pub mod issue;
pub mod memory;
pub mod note;
pub mod owner_routing;
pub mod pr_inventory;
pub mod session;
pub mod settings;
pub mod vt_screen;
pub mod workspace;
pub mod workspace_guard;
