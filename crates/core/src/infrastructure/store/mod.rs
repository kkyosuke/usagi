//! entity 別の永続化ストア。
//!
//! それぞれ [`super::persistence`] の基盤の上に、そのエンティティ固有の採番・
//! ファイル名・派生ファイルを載せる。
//!
//! - [`issue`] — issue の CRUD・採番・`index.json`。
//! - [`memory`] — memory の CRUD・`MEMORY.md` 目次・`index.json`。
//! - [`workspace`] — workspace レジストリ（`workspaces.json`）。
//! - [`state`] — repo の `WorkspaceState`（debug は `<repo>/.usagi/dev/state.json`）。
//! - [`dispatch`] — daemon-owned agent dispatch registry and caller inboxes.

/// State of rebuildable files after a source-of-truth mutation committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivedState {
    /// Every derived file reflects the committed Markdown source.
    Fresh,
    /// The source committed, but at least one derived file must be rebuilt.
    RebuildNeeded,
}

/// Successful source mutation together with the state of its derived files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationOutcome<T> {
    /// Store-specific committed value.
    pub value: T,
    /// Whether rebuildable files are fresh after the source commit.
    pub derived: DerivedState,
}

impl<T> MutationOutcome<T> {
    #[must_use]
    pub const fn new(value: T, derived: DerivedState) -> Self {
        Self { value, derived }
    }
}

pub mod dispatch;
pub mod issue;
pub mod lifecycle;
pub mod memory;
pub mod pr_inventory;
pub mod state;
pub mod supervisor;
pub mod user_decision;
pub mod workspace;
