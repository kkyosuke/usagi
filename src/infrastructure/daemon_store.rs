//! File-backed record of the running usagi daemon, and its stop signal.
//!
//! usagi has no shared memory between processes, so a CLI (`usagi daemon
//! status` / `stop`) and the daemon itself coordinate purely through files under
//! `<data-dir>/daemon/`:
//!
//! - `daemon.json` — the [`DaemonRecord`] written by a live daemon at startup and
//!   removed when it exits. Its pid lets any process ask "is a daemon running?"
//!   by checking whether that pid is still alive.
//! - `stop` — a marker a `usagi daemon stop` drops to ask the running daemon to
//!   exit; the daemon polls for it and removes it as it shuts down.
//!
//! Mutations that must not interleave with the daemon's own startup (write the
//! record) are serialised by the caller with a [`StoreLock`] on this directory
//! (see [`crate::usecase::daemon`]); the atomic per-file writes here keep any
//! single file from being observed half-written.
//!
//! [`StoreLock`]: crate::infrastructure::store_lock::StoreLock

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::infrastructure::json_file;
use crate::infrastructure::storage;

/// Subdirectory of the data dir the daemon's files live under.
const DAEMON_SUBDIR: &str = "daemon";
/// File name of the running-daemon record.
const RECORD_FILE: &str = "daemon.json";
/// File name of the stop marker.
const STOP_FILE: &str = "stop";

/// On-disk record of the running daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecord {
    /// Process id of the running daemon. A reader checks it against the live
    /// process table to tell a running daemon from a stale record.
    pub pid: u32,
}

/// The directory the daemon's files live under: `<data-dir>/daemon/`.
pub fn default_dir() -> Result<PathBuf> {
    Ok(storage::data_dir()?.join(DAEMON_SUBDIR))
}

/// Read the daemon record under `dir`, returning `None` when no daemon has
/// registered (the file does not exist).
pub fn read(dir: &Path) -> Result<Option<DaemonRecord>> {
    json_file::read_versioned(&dir.join(RECORD_FILE))
}

/// Write (or replace) the daemon record under `dir`, creating `dir` if needed.
pub fn write(dir: &Path, record: &DaemonRecord) -> Result<()> {
    json_file::write_versioned(dir, &dir.join(RECORD_FILE), record)
}

/// Remove the daemon record under `dir`. A no-op when it is already absent.
pub fn clear(dir: &Path) -> Result<()> {
    remove_if_present(&dir.join(RECORD_FILE))
}

/// Drop the stop marker under `dir`, asking a running daemon to exit. Creates
/// `dir` if needed so the signal can be left even before the daemon's first
/// write (the daemon polls for it regardless).
pub fn request_stop(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).context(format!("failed to create {}", dir.display()))?;
    json_file::write_text_atomic(&dir.join(STOP_FILE), "")
}

/// Whether a stop has been requested, clearing the marker as it reports `true`
/// so the same request is consumed once. Called by the daemon's poll loop.
pub fn take_stop_request(dir: &Path) -> Result<bool> {
    let path = dir.join(STOP_FILE);
    if path.exists() {
        remove_if_present(&path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Remove any leftover stop marker under `dir` without acting on it. Used when a
/// daemon registers, so a stale marker from a previous run cannot make the fresh
/// daemon exit immediately.
pub fn clear_stop_request(dir: &Path) -> Result<()> {
    remove_if_present(&dir.join(STOP_FILE))
}

/// Remove `path`, treating an already-absent file as success.
fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context(format!("failed to remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dir_is_daemon_under_the_data_dir() {
        // Derived purely from the resolved data dir (tested in `storage`), so the
        // check needs no environment mutation — it just asserts the subdir hangs
        // off whatever data dir resolves to.
        let expected = storage::data_dir().unwrap().join(DAEMON_SUBDIR);
        assert_eq!(default_dir().unwrap(), expected);
    }

    #[test]
    fn read_is_none_before_any_write() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read(tmp.path()).unwrap(), None);
    }

    #[test]
    fn write_then_read_round_trips_the_record() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("daemon");
        write(&dir, &DaemonRecord { pid: 4321 }).unwrap();
        assert_eq!(read(&dir).unwrap(), Some(DaemonRecord { pid: 4321 }));
    }

    #[test]
    fn write_replaces_an_earlier_record() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(dir, &DaemonRecord { pid: 1 }).unwrap();
        write(dir, &DaemonRecord { pid: 2 }).unwrap();
        assert_eq!(read(dir).unwrap(), Some(DaemonRecord { pid: 2 }));
    }

    #[test]
    fn clear_removes_the_record_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(dir, &DaemonRecord { pid: 7 }).unwrap();
        clear(dir).unwrap();
        assert_eq!(read(dir).unwrap(), None);
        // A second clear on the already-absent record still succeeds.
        clear(dir).unwrap();
    }

    #[test]
    fn stop_request_is_taken_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert!(!take_stop_request(dir).unwrap());
        request_stop(dir).unwrap();
        assert!(take_stop_request(dir).unwrap());
        // The marker is consumed, so a second poll reports no request.
        assert!(!take_stop_request(dir).unwrap());
    }

    #[test]
    fn request_stop_creates_the_dir_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("not-yet-there");
        request_stop(&dir).unwrap();
        assert!(take_stop_request(&dir).unwrap());
    }

    #[test]
    fn clear_stop_request_drops_the_marker_without_reporting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        request_stop(dir).unwrap();
        clear_stop_request(dir).unwrap();
        assert!(!take_stop_request(dir).unwrap());
        // Idempotent when already absent.
        clear_stop_request(dir).unwrap();
    }

    #[test]
    fn remove_if_present_errors_when_path_is_a_directory() {
        // A directory where a file is expected is neither "removed" nor NotFound;
        // the error surfaces rather than being swallowed.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("recordslot");
        fs::create_dir_all(&dir).unwrap();
        assert!(remove_if_present(&dir).is_err());
    }
}
