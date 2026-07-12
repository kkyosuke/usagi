//! usecase 層。domain を組み合わせてアプリケーションの操作を表す。
//! TUI 面・daemon 面の両方から呼ばれるロジックだけを置き、
//! v2 では必要になった時点で実装を追加する。
//!
//! - [`issue`] — issue の CRUD（create（採番）/ get / list / update / delete）。
//!   人間向け CLI と MCP tool の双方が呼ぶ。
//! - [`memory`] — memory の CRUD（save（slug・upsert）/ get / list / delete）。
//! - [`session`] — repo `state.json` 上の session state 操作（list / get / touch /
//!   record / remove）。git worktree の作成・破棄は git 層の担当で、ここでは
//!   記録される状態だけを扱う。

pub mod issue;
pub mod memory;
pub mod session;
