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
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use fs2::FileExt;

/// Name of the per-store lock file placed inside the store directory.
pub const LOCK_FILE_NAME: &str = ".lock";

/// How long [`StoreLock::acquire`] waits for the lock before giving up. A holder
/// normally releases within milliseconds (one read-modify-write of a small
/// directory), so this generously absorbs contention while still turning a stuck
/// holder — a live process wedged mid-operation — into a reported error rather
/// than an indefinitely frozen UI. (A *crashed* holder is not the concern: the
/// OS drops an `flock` when the holding process dies.)
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often [`StoreLock::acquire`] re-tries while waiting for the lock.
const ACQUIRE_POLL: Duration = Duration::from_millis(20);

/// An held exclusive advisory lock on a store directory. The lock is released
/// when this guard is dropped.
#[must_use = "the lock is released as soon as the guard is dropped"]
#[derive(Debug)]
pub struct StoreLock {
    file: File,
}

impl StoreLock {
    /// Acquire the exclusive lock for the store rooted at `dir`, waiting up to
    /// [`ACQUIRE_TIMEOUT`] for it. Creates `dir` and the lock file if they do not
    /// exist; errors if the lock cannot be taken within the timeout.
    ///
    /// The returned guard must be held for the whole read-modify-write so other
    /// processes serialise behind it.
    pub fn acquire(dir: &Path) -> Result<Self> {
        Self::acquire_with_timeout(dir, ACQUIRE_TIMEOUT)
    }

    /// [`acquire`](Self::acquire) with an explicit wait budget, so tests can use a
    /// short one. Polls a non-blocking `try_lock` rather than blocking forever, so
    /// a holder wedged mid-operation surfaces as an error the caller can report
    /// instead of hanging the UI.
    fn acquire_with_timeout(dir: &Path, timeout: Duration) -> Result<Self> {
        fs::create_dir_all(dir).context(format!("failed to create {}", dir.display()))?;
        let path = Self::path(dir);
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .context(format!("failed to open {}", path.display()))?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { file }),
                // Held by another process (or, rarely, a transient lock error):
                // keep polling until the deadline, then surface it rather than
                // wait forever.
                Err(e) => {
                    if Instant::now() >= deadline {
                        return Err(anyhow::Error::new(e)).context(format!(
                            "timed out waiting for the store lock {} (another usagi \
                             process may be stuck holding it)",
                            path.display()
                        ));
                    }
                    std::thread::sleep(ACQUIRE_POLL);
                }
            }
        }
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
    fn acquire_times_out_when_the_lock_is_held() {
        // While another holder has the lock, an acquire with a short budget gives
        // up with an error instead of blocking forever (exercises the deadline-
        // reached branch).
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("store");
        let held = StoreLock::acquire(&dir).unwrap();

        let err = StoreLock::acquire_with_timeout(&dir, Duration::from_millis(50)).unwrap_err();
        assert!(err
            .to_string()
            .contains("timed out waiting for the store lock"));
        drop(held);
    }

    #[test]
    fn acquire_succeeds_after_a_holder_releases_mid_wait() {
        // A holder releases shortly after a second acquire starts waiting, so the
        // poll loop (the sleep-and-retry branch) eventually wins the lock rather
        // than timing out.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("store");
        fs::create_dir_all(&dir).unwrap();
        let held = StoreLock::acquire(&dir).unwrap();

        let dir2 = dir.clone();
        let handle = thread::spawn(move || {
            // Generous budget: the holder is dropped well within it.
            StoreLock::acquire_with_timeout(&dir2, Duration::from_secs(5)).unwrap()
        });
        thread::sleep(Duration::from_millis(60));
        drop(held);
        let acquired = handle.join().unwrap();
        drop(acquired);
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
