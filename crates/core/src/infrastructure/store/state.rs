//! Persistence for a single repository's [`WorkspaceState`] — the sessions
//! created under it and the workspace-root note scratchpad.
//!
//! The state lives inside the repository's channel-specific runtime directory
//! (`<repo>/.usagi/dev/state.json` in development mode), a
//! versioned JSON file written through a temp file + rename so a crash never
//! leaves it half-written. `state.json` is read-modify-write (load, edit the
//! session list, save the whole file), so mutations take the store lock across
//! the whole sequence — several usagi processes can share one repository (the
//! TUI plus a session's `usagi mcp` server).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::domain::workspace_state::WorkspaceState;
use crate::infrastructure::paths::project_data_dir;
use crate::infrastructure::persistence::json_file;
use crate::infrastructure::persistence::store_lock::StoreLock;

const STATE_FILE: &str = "state.json";

/// File-based persistence rooted at a repository's channel-specific runtime
/// directory (`.usagi/dev/` in development mode).
pub struct WorkspaceStateStore {
    dir: PathBuf,
}

impl WorkspaceStateStore {
    /// Open the store for the repository whose primary worktree is `repo_root`.
    #[must_use]
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            dir: project_data_dir(repo_root),
        }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn state_path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    /// Acquire this store's cross-process write lock, blocking until it is free.
    /// Hold the guard across the whole load+save of a mutation so a concurrent
    /// writer cannot read the same snapshot and overwrite the first writer's
    /// change (a lost update).
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired.
    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(&self.dir)
    }

    /// Load the saved state, or `None` if it has never been written.
    ///
    /// # Errors
    ///
    /// Returns an error when `state.json` exists but cannot be read or parsed.
    pub fn load(&self) -> Result<Option<WorkspaceState>> {
        json_file::read_versioned(&self.state_path())
    }

    /// Write the whole state to `state.json`.
    ///
    /// # Errors
    ///
    /// Returns an error when the `.usagi/` directory cannot be created or the
    /// file cannot be written.
    pub fn save(&self, state: &WorkspaceState) -> Result<()> {
        json_file::write_versioned(&self.dir, &self.state_path(), state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::note::Scratchpad;
    use crate::domain::session::{SessionOrigin, SessionRecord};
    use chrono::{TimeZone, Utc};
    use std::fs;

    fn session(name: &str) -> SessionRecord {
        let ts = Utc.with_ymd_and_hms(2026, 6, 20, 0, 0, 0).unwrap();
        SessionRecord {
            name: name.to_string(),
            display_name: None,
            origin: SessionOrigin::Human,
            started_from: None,
            root: format!("/repo/.usagi/sessions/{name}").into(),
            created_at: ts,
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
            environment: std::collections::BTreeMap::new(),
        }
    }

    fn sample_state() -> WorkspaceState {
        WorkspaceState {
            sessions: vec![session("alpha")],
            root_notes: Scratchpad {
                note: Some("root memo".to_string()),
                ..Default::default()
            },
            root_environment: std::collections::BTreeMap::new(),
            updated_at: Utc.with_ymd_and_hms(2026, 6, 20, 1, 0, 0).unwrap(),
        }
    }

    #[test]
    fn dir_and_state_path_point_under_the_project_runtime_directory() {
        let store = WorkspaceStateStore::new("/repo");
        let expected = crate::infrastructure::paths::project_data_dir("/repo");
        assert_eq!(store.dir(), expected);
        assert_eq!(store.state_path(), expected.join("state.json"));
    }

    #[test]
    fn load_is_none_before_anything_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceStateStore::new(tmp.path());
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn save_then_load_round_trips_and_stamps_the_version() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceStateStore::new(tmp.path());
        let state = sample_state();

        store.save(&state).unwrap();
        assert!(store.state_path().is_file());
        let text = fs::read_to_string(store.state_path()).unwrap();
        assert!(text.contains("\"version\": 1"));

        assert_eq!(store.load().unwrap().unwrap(), state);
    }

    #[test]
    fn load_reports_a_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceStateStore::new(tmp.path());
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.state_path(), "{ broken").unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn save_reports_an_error_when_the_dir_cannot_be_created() {
        let tmp = tempfile::tempdir().unwrap();
        // A file where the `.usagi` parent should be makes create_dir_all fail.
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let store = WorkspaceStateStore::new(blocker.join("repo"));
        assert!(store.save(&sample_state()).is_err());
    }

    #[test]
    fn lock_is_a_dotfile_and_does_not_block_save() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WorkspaceStateStore::new(tmp.path());
        let lock = store.lock().unwrap();
        assert!(store.dir().join(".lock").is_file());
        store.save(&sample_state()).unwrap();
        assert_eq!(store.load().unwrap().unwrap().sessions.len(), 1);
        drop(lock);
    }

    #[test]
    fn lock_errors_when_the_dir_path_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let store = WorkspaceStateStore::new(blocker.join("repo"));
        assert!(store.lock().is_err());
    }
}
