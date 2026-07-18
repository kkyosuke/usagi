//! entity 別の永続化ストア。
//!
//! それぞれ [`super::persistence`] の基盤の上に、そのエンティティ固有の採番・
//! ファイル名・派生ファイルを載せる。
//!
//! - [`issue`] — issue の CRUD・採番・`index.json`。
//! - [`memory`] — memory の CRUD・`MEMORY.md` 目次・`index.json`。
//! - [`workspace`] — workspace レジストリ（`workspaces.json`）。
//! - [`state`] — repo の `WorkspaceState`（`<repo>/.usagi/state.json`）。
//! - [`dispatch`] — daemon-owned agent dispatch registry and caller inboxes.

pub mod dispatch;
pub mod issue;
pub mod lifecycle;
pub mod memory;
pub mod pr_inventory;
pub mod state;
pub mod supervisor;
pub mod user_decision;
pub mod workspace;
