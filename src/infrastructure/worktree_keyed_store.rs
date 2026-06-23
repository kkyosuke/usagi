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
///
/// The hash is **FNV-1a**, not the standard library's [`DefaultHasher`]: that
/// hasher's algorithm and seed are an unspecified implementation detail that may
/// change between Rust releases, which would silently orphan every existing
/// phase/prompt file the moment usagi was rebuilt on a newer toolchain. FNV-1a is
/// a fixed, specified construction, so the name a given path maps to is stable
/// for the life of the on-disk format. The path is hashed via its lossy UTF-8
/// form so the derivation is identical on every platform; on the rare path that
/// is not valid UTF-8 a collision is still caught by each store stamping and
/// re-checking the worktree it wrote (see the module docs).
///
/// [`DefaultHasher`]: std::collections::hash_map::DefaultHasher
pub fn file_name(canonical: &Path) -> String {
    // FNV-1a (64-bit): start from the offset basis, then for each byte XOR it in
    // and multiply by the prime. <http://www.isthe.com/chongo/tech/comp/fnv/>
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in canonical.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// The key a worktree is stored under: its canonical path. When the path cannot
/// be resolved (e.g. it no longer exists) it falls back to the path made
/// absolute against the current directory, and only then to the path as given —
/// so a writer and reader that pass the same worktree, even as a relative path,
/// still derive a matching name (a raw relative path and its absolute form would
/// otherwise hash differently and the reader would miss the writer's file).
pub fn key(worktree: &Path) -> PathBuf {
    worktree
        .canonicalize()
        .or_else(|_| std::path::absolute(worktree))
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
    fn file_name_hashes_a_known_path_to_a_fixed_value() {
        // Locks the cross-version stability contract: FNV-1a of these exact
        // strings must never change, or every existing on-disk file would be
        // orphaned. If this fails, the hash construction was altered.
        assert_eq!(file_name(Path::new("/tmp/x")), "6cc122ddf274426c");
        assert_eq!(file_name(Path::new("/Users/a/proj")), "3b1b270bb17c20a0");
    }

    #[test]
    fn key_falls_back_to_an_absolute_path_when_unresolvable() {
        // An absolute path that does not exist cannot be canonicalized, so it is
        // returned verbatim — the writer and reader still derive a matching name.
        let missing = Path::new("/usagi/does/not/exist");
        assert_eq!(key(missing), missing.to_path_buf());

        // A *relative* unresolvable path is made absolute against the current
        // directory rather than left relative, so a writer passing `./wt` and a
        // reader passing the absolute form still agree on the file name.
        let relative = Path::new("usagi-nonexistent-rel");
        let keyed = key(relative);
        assert!(keyed.is_absolute());
        assert_eq!(keyed, std::env::current_dir().unwrap().join(relative));
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
