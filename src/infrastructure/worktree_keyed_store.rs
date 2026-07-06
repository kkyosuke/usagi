//! Shared addressing for the per-worktree files usagi stores under its data
//! directory: [`agent_state_store`](super::agent_state_store) (a session's
//! lifecycle phase), [`agent_prompt_store`](super::agent_prompt_store) (a prompt
//! queued for a session's agent), live prompts, PR links, and the restore-state
//! snapshots.
//!
//! These stores may have a writer and a reader in different processes that never
//! share memory, so they must agree on a file path purely from the worktree
//! directory.
//! That "worktree → file name" derivation is the single fact kept here: the
//! worktree's canonical path hashed to a stable, filesystem-safe hex name under
//! `<data-dir>/<subdir>/`. Keeping it in one place means a change to the hashing
//! cannot make stores derive different names for the same worktree.
//!
//! Each store still stamps its own file with the worktree it belongs to and
//! checks it on read, so a hash collision (or a stale file synced from another
//! machine) is detected and ignored rather than misattributed.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{de::DeserializeOwned, Serialize};

use crate::infrastructure::json_file;
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

/// A JSON envelope stamped with the worktree/workspace path it belongs to.
///
/// Stores key their files by a hash of a path, so the file itself must repeat
/// that path. Readers compare the stamp to the key they expected and treat a
/// mismatch as absent, which makes hash collisions or synced stale files safe.
pub trait WorktreeStamped {
    /// The worktree/workspace path recorded inside the file.
    fn stamped(&self) -> &Path;
}

/// Read `path` only when its stamp matches `key`.
///
/// A missing file or a parseable file stamped for another worktree reads as
/// absent. The collision case is left on disk for its rightful owner; an
/// unreadable/corrupt file can never be delivered to anyone, so it is cleared.
pub fn read_ours<T>(path: &Path, key: &Path) -> Option<T>
where
    T: DeserializeOwned + WorktreeStamped,
{
    match json_file::read::<T>(path) {
        Ok(Some(file)) if file.stamped() == key => Some(file),
        Ok(Some(_)) | Ok(None) => None,
        Err(_) => {
            let _ = fs::remove_file(path);
            None
        }
    }
}

/// Write an already-stamped envelope atomically.
///
/// The type bound keeps call sites honest: only files that carry an explicit
/// worktree/workspace stamp use this helper.
pub fn write_stamped<T>(dir: &Path, path: &Path, value: &T) -> Result<()>
where
    T: Serialize + WorktreeStamped,
{
    json_file::write_atomic(dir, path, value)
}

/// Clear the file addressed by `worktree` under `subdir` (best-effort).
pub fn clear(subdir: &str, worktree: &Path) {
    if let Ok(path) = path_for(subdir, worktree) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct TestFile {
        worktree: PathBuf,
        value: String,
    }

    impl WorktreeStamped for TestFile {
        fn stamped(&self) -> &Path {
            &self.worktree
        }
    }

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

        // The empty path resolves through neither `canonicalize` nor
        // `std::path::absolute`, so it falls through to the raw path verbatim
        // (the final fallback). It never arises in practice but must not panic.
        assert_eq!(key(Path::new("")), Path::new("").to_path_buf());
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

    #[test]
    fn write_stamped_and_read_ours_round_trip_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let key = key(wt.path());
        let path = dir.path().join(file_name(&key));
        write_stamped(
            dir.path(),
            &path,
            &TestFile {
                worktree: key.clone(),
                value: "ours".to_string(),
            },
        )
        .unwrap();
        assert_eq!(
            read_ours::<TestFile>(&path, &key),
            Some(TestFile {
                worktree: key,
                value: "ours".to_string(),
            })
        );
    }

    #[test]
    fn read_ours_preserves_a_collision_but_clears_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let wt = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let wt_key = key(wt.path());
        let other_key = key(other.path());
        let path = dir.path().join(file_name(&wt_key));

        write_stamped(
            dir.path(),
            &path,
            &TestFile {
                worktree: other_key.clone(),
                value: "theirs".to_string(),
            },
        )
        .unwrap();
        assert_eq!(read_ours::<TestFile>(&path, &wt_key), None);
        assert!(path.exists());
        assert_eq!(
            read_ours::<TestFile>(&path, &other_key).unwrap().value,
            "theirs"
        );

        fs::write(&path, "not json").unwrap();
        assert_eq!(read_ours::<TestFile>(&path, &wt_key), None);
        assert!(!path.exists());
        assert_eq!(read_ours::<TestFile>(&path, &wt_key), None);
    }

    #[test]
    fn clear_removes_the_addressed_file() {
        let _guard = crate::test_support::process_env_guard();
        let data = tempfile::tempdir().unwrap();
        std::env::set_var(storage::DATA_DIR_ENV, data.path());
        let wt = tempfile::tempdir().unwrap();
        let path = path_for("test-stamped", wt.path()).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "x").unwrap();
        clear("test-stamped", wt.path());
        assert!(!path.exists());
        // Clearing again is a harmless no-op.
        clear("test-stamped", wt.path());
        std::env::remove_var(storage::DATA_DIR_ENV);
    }
}
