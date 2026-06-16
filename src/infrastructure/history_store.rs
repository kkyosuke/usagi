//! Persistence for a single repository's command history.
//!
//! Every command run in the workspace screen is appended to
//! `<repo>/.usagi/history.json`, next to the `state.json` that describes the
//! same repository. Writes go through a temp file + rename so a crash never
//! leaves a half-written `history.json` behind.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::domain::history::HistoryEntry;
use crate::infrastructure::json_file;

/// Directory created inside the repository to hold usagi's per-repo data.
const STATE_DIR_NAME: &str = ".usagi";
const HISTORY_FILE: &str = "history.json";

const FILE_FORMAT_VERSION: u32 = 1;

/// On-disk shape of `history.json`.
#[derive(Debug, Serialize, Deserialize)]
struct HistoryFile {
    version: u32,
    entries: Vec<HistoryEntry>,
}

/// File-based persistence for a repository's command history, rooted at its
/// `.usagi/` directory.
pub struct HistoryStore {
    dir: PathBuf,
}

impl HistoryStore {
    /// Open the store for the repository whose primary worktree is `repo_root`.
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            dir: repo_root.as_ref().join(STATE_DIR_NAME),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn history_path(&self) -> PathBuf {
        self.dir.join(HISTORY_FILE)
    }

    /// Load the recorded history, oldest first. Returns an empty vector if the
    /// file has never been written.
    pub fn load(&self) -> Result<Vec<HistoryEntry>> {
        let file: Option<HistoryFile> = json_file::read(&self.history_path())?;
        Ok(file.map(|f| f.entries).unwrap_or_default())
    }

    /// Append a single executed `command` to the history, stamped with the
    /// current time. Reads the existing entries, adds the new one, and rewrites
    /// the file atomically.
    pub fn append(&self, command: impl Into<String>) -> Result<()> {
        let mut entries = self.load()?;
        entries.push(HistoryEntry::now(command));
        self.save(&entries)
    }

    /// Persist `entries` to `<repo>/.usagi/history.json`.
    fn save(&self, entries: &[HistoryEntry]) -> Result<()> {
        json_file::write_atomic(
            &self.dir,
            &self.history_path(),
            &HistoryFile {
                version: FILE_FORMAT_VERSION,
                entries: entries.to_vec(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn load_returns_empty_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn append_accumulates_entries_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());

        store.append("man").unwrap();
        store.append("doctor").unwrap();

        let entries = store.load().unwrap();
        let commands: Vec<&str> = entries.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(commands, vec!["man", "doctor"]);
        assert!(store.history_path().exists());
    }

    #[test]
    fn saved_file_records_the_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        store.append("man").unwrap();

        let text = fs::read_to_string(store.history_path()).unwrap();
        assert!(text.contains("\"version\": 1"));
    }

    #[test]
    fn dir_points_at_the_usagi_subdirectory() {
        let store = HistoryStore::new("/repo");
        assert_eq!(store.dir(), Path::new("/repo/.usagi"));
        assert_eq!(
            store.history_path(),
            PathBuf::from("/repo/.usagi/history.json")
        );
    }

    #[test]
    fn load_errors_on_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.history_path(), "{ not json").unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn load_errors_when_history_path_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        // Make history.json a directory so reading it fails with a non-NotFound error.
        fs::create_dir_all(store.history_path()).unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn append_errors_when_load_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        // A corrupt existing file makes the read-before-append fail.
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.history_path(), "{ not json").unwrap();
        assert!(store.append("man").is_err());
    }
}
