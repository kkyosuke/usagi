//! The on-disk layout of a repository's usagi metadata, kept in one place.
//!
//! Everything usagi persists *inside a repository* lives under a single
//! directory at the repository root. Its name is a fact that several layers need
//! — the issue / memory / workspace stores join it, the `.gitignore` writer
//! targets it — so it is defined here once rather than re-spelled as a literal
//! at each site.
//!
//! This is distinct from [`storage::data_dir`](super::storage::data_dir), the
//! *global* per-user data directory (`$USAGI_HOME` or `~/.usagi`): the two share
//! the `.usagi` basename by convention but are independent directories with
//! different contents and lifetimes, so they keep separate constants.

/// The repository-relative directory holding usagi's per-project metadata
/// (`issues/`, `memory/`, `state.json`, …): `<repo>/.usagi`.
pub const STATE_DIR: &str = ".usagi";
