//! Shared addressing for the per-worktree files usagi stores under its data
//! directory: [`agent_state_store`](super::agent_state_store) (a session's
//! lifecycle phase) and [`agent_prompt_store`](super::agent_prompt_store) (a
//! prompt queued for a session's agent).
//!
//! Both stores have a writer and a reader in different processes that never share
//! memory, so they must agree on a file path purely from the worktree directory.
//! That "worktree → file name" derivation is the single fact kept here: the
//! worktree's canonical path hashed to a stable, filesystem-safe hex name under
//! `<data-dir>/<subdir>/`. Keeping it in one place means a change to the hashing
//! cannot make the two stores derive different names for the same worktree.
//!
//! Each store still stamps its own file with the worktree it belongs to and
//! checks it on read, so a hash collision (or a stale file synced from another
//! machine) is detected and ignored rather than misattributed.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::infrastructure::storage;

/// The directory a store's files live under: `<data-dir>/<subdir>/`.
pub fn dir(subdir: &str) -> Result<PathBuf> {
    Ok(storage::data_dir()?.join(subdir))
}

/// The file name a worktree's data is stored under: a stable hash of its
/// canonical path rendered as hex, so the writer and reader agree on it without
/// listing the directory. Pure given `canonical`.
pub fn file_name(canonical: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The key a worktree is stored under: its canonical path, falling back to the
/// path as given when it cannot be resolved (e.g. it no longer exists), so the
/// writer and reader still derive the same name.
pub fn key(worktree: &Path) -> PathBuf {
    worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf())
}

/// The full path of `worktree`'s file under `subdir`.
pub fn path_for(subdir: &str, worktree: &Path) -> Result<PathBuf> {
    Ok(dir(subdir)?.join(file_name(&key(worktree))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_is_stable_and_hex() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = key(dir.path());
        let name = file_name(&canonical);
        assert_eq!(name.len(), 16);
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        // Same input → same name; the writer and reader rely on this.
        assert_eq!(name, file_name(&canonical));
    }

    #[test]
    fn key_falls_back_to_the_given_path_when_unresolvable() {
        // A path that does not exist cannot be canonicalized, so it is returned
        // verbatim — the writer and reader still derive a matching name.
        let missing = Path::new("/usagi/does/not/exist");
        assert_eq!(key(missing), missing.to_path_buf());
    }

    #[test]
    fn path_for_joins_the_subdir_and_hashed_name() {
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(storage::DATA_DIR_ENV, dir.path());
        let wt = tempfile::tempdir().unwrap();
        let path = path_for("agent-state", wt.path()).unwrap();
        assert_eq!(
            path,
            dir.path()
                .join("agent-state")
                .join(file_name(&key(wt.path())))
        );
        std::env::remove_var(storage::DATA_DIR_ENV);
    }
}
