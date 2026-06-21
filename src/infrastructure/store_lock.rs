//! Cross-process advisory locking for the file-backed stores.
//!
//! The issue and memory stores are read-modify-write: a mutation reads the
//! directory (e.g. to allocate the next issue number or to find stale-named
//! siblings), writes one markdown file, then rebuilds the derived `index.json`
//! / `MEMORY.md` by scanning the whole directory. The per-file write is atomic
//! (temp + rename, see [`crate::infrastructure::json_file`]), but the *sequence*
//! is not: the MCP server and the TUI write the same `.usagi/issues/` and
//! `.usagi/memory/` directories concurrently, so two processes can interleave
//! and lose data (e.g. both allocate the same number, or a stale rebuild wins).
//!
//! [`StoreLock`] serialises those sequences across processes with an exclusive
//! advisory lock (`flock`-style, via [`fs2`]) held on a per-store `.lock` file.
//! Hold the guard for the *entire* read-modify-write of a mutating operation.
//!
//! The lock file is a dotfile (`.lock`) that lives inside the store directory.
//! The store's directory scans only pick up `*.md` files, so the lock file is
//! never parsed as data; `usagi init`'s `.gitignore` additionally keeps it out
//! of git (see [`crate::infrastructure::gitignore`]).

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;

/// Name of the per-store lock file placed inside the store directory.
pub const LOCK_FILE_NAME: &str = ".lock";

/// An held exclusive advisory lock on a store directory. The lock is released
/// when this guard is dropped.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct StoreLock {
    file: File,
}

impl StoreLock {
    /// Acquire the exclusive lock for the store rooted at `dir`, blocking until
    /// it is available. Creates `dir` and the lock file if they do not exist.
    ///
    /// The returned guard must be held for the whole read-modify-write so other
    /// processes serialise behind it.
    pub fn acquire(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).context(format!("failed to create {}", dir.display()))?;
        let path = Self::path(dir);
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .context(format!("failed to open {}", path.display()))?;
        file.lock_exclusive()
            .context(format!("failed to lock {}", path.display()))?;
        Ok(Self { file })
    }

    /// Path of the lock file for the store rooted at `dir`.
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(LOCK_FILE_NAME)
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Best-effort: the OS also drops the lock when the fd closes.
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn path_is_a_dotfile_inside_the_dir() {
        assert_eq!(
            StoreLock::path(Path::new("/repo/.usagi/issues")),
            PathBuf::from("/repo/.usagi/issues/.lock")
        );
    }

    #[test]
    fn acquire_creates_the_directory_and_lock_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("store");
        let guard = StoreLock::acquire(&dir).unwrap();
        assert!(dir.join(LOCK_FILE_NAME).is_file());
        drop(guard);
    }

    #[test]
    fn the_lock_is_mutually_exclusive_across_threads() {
        // A second acquisition blocks until the first guard is dropped, proving
        // the lock serialises holders. (Separate processes use separate fds and
        // are excluded the same way by the OS.)
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("store");
        fs::create_dir_all(&dir).unwrap();

        let held = StoreLock::acquire(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let dir2 = dir.clone();
        let handle = thread::spawn(move || {
            // Use a distinct fd so the OS treats this as a competing holder.
            let path = StoreLock::path(&dir2);
            let file = File::options()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .unwrap();
            file.lock_exclusive().unwrap();
            tx.send(()).unwrap();
            file.unlock().unwrap();
        });

        // While we hold the lock the other thread must not acquire it.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        drop(held);
        // Once released it proceeds.
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn acquire_errors_when_the_dir_path_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not-a-dir");
        fs::write(&path, "x").unwrap();
        // create_dir_all fails because the path is an existing file.
        assert!(StoreLock::acquire(&path).is_err());
    }
}
