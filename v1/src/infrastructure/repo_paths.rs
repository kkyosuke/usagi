//! The on-disk layout of a repository's usagi metadata, kept in one place.
//!
//! Everything usagi persists *inside a repository* lives under a single
//! directory at the repository root. Its name is a fact that several layers need
//! — the issue / memory / workspace / history stores join it, the session
//! lifecycle builds `<root>/<state-dir>/sessions/` under it, the `.gitignore`
//! writer targets it, and the recursive session-tree walk skips it — so it is
//! defined here once rather than re-spelled as a literal at each site.
//!
//! This is distinct from [`storage::data_dir`](super::storage::data_dir), the
//! *global* per-user data directory (`$USAGI_HOME` or `~/.usagi`): the two share
//! the `.usagi` basename by convention but are independent directories with
//! different contents and lifetimes, so they keep separate constants.

/// The repository-relative directory holding usagi's per-project metadata
/// (`issues/`, `memory/`, `sessions/`, `state.json`, …): `<repo>/.usagi`.
pub const STATE_DIR: &str = ".usagi";

/// The directory under [`STATE_DIR`] that holds session worktrees, one per
/// session: `<repo>/.usagi/sessions/<name>`. Several layers join it (the
/// session lifecycle builds and reconciles it, the TUI evicts a removed
/// session's tree under it), so it is named here once rather than re-spelled as
/// a `"sessions"` literal at each site.
pub const SESSIONS_DIR: &str = "sessions";

/// The directory under [`STATE_DIR`] where a removed session's tree waits to be
/// deleted: `<repo>/.usagi/trash/<name>-<removal id>`.
///
/// Teardown *retires* a session tree by renaming it here rather than deleting it
/// inline — a rename costs the same whether the tree is empty or holds a
/// multi-gigabyte `target/`, so `session remove` returns without waiting on the
/// disk. The reclamation that actually frees the space runs later, off the
/// caller's critical path (see
/// [`sweep_trash`](crate::usecase::session::sweep_trash)).
///
/// It sits beside `sessions/` rather than inside it so reconcile's stray scan —
/// which reads `.usagi/sessions/` directly — never mistakes a retired tree for a
/// session whose record went missing. Git never sees it either: `.usagi/`'s own
/// `.gitignore` ignores everything it does not explicitly re-include
/// ([`USAGI_GITIGNORE`](super::gitignore::USAGI_GITIGNORE)).
pub const TRASH_DIR: &str = "trash";
