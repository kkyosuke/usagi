//! usecase 層。domain を組み合わせてアプリケーションの操作を表す。
//! TUI 面・daemon 面の両方から呼ばれるロジックだけを置き、
//! v2 では必要になった時点で実装を追加する。
//!
//! - [`issue`] — issue の CRUD（create（採番）/ get / list / update / delete）。
//!   人間向け CLI と MCP tool の双方が呼ぶ。
//! - [`memory`] — memory の CRUD（save（slug・upsert）/ get / list / delete）。
//! - [`note`] — session / root の scratchpad 操作（note / todo / decision）を
//!   `state.json` 上で行う（`session_note_* / session_todo_* / session_decision_*`
//!   と TUI が呼ぶ中身）。
//! - [`session`] — git worktree と repo `state.json` を合成する session lifecycle
//!   （create / remove）と state 操作（list / get / touch / record / `remove_record`）。
//! - [`workspace`] — global registry 上の workspace open（path 解決・登録・touch）と、
//!   welcome 画面向け recent overview の構築。

pub mod agent;
pub mod client;
pub mod issue;
pub mod memory;
pub mod note;
pub mod session;
pub mod settings;
pub mod workspace;
