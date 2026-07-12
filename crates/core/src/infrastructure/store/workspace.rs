#![coverage(off)]

//! The workspace registry: the list of workspaces the user has opened.
//!
//! It lives in the global per-user data directory (see
//! [`paths::data_dir`](crate::infrastructure::paths::data_dir)) as
//! `workspaces.json`, a versioned JSON file written through a temp file + rename
//! so a crash never leaves it half-written. Several usagi processes share this
//! one store, so mutations take the store lock across the whole read-modify-write.

use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::workspace::Workspace;
use crate::infrastructure::paths::data_dir;
use crate::infrastructure::persistence::json_file;
use crate::infrastructure::persistence::store_lock::StoreLock;

const WORKSPACES_FILE: &str = "workspaces.json";

/// The `workspaces.json` payload, borrowed for writes so the list need not be
/// cloned into an owned wrapper just to stamp the version envelope.
#[derive(Serialize)]
struct WorkspacesRef<'a> {
    workspaces: &'a [Workspace],
}

/// The `workspaces.json` payload as read back (the version envelope is stripped
/// by [`json_file::read_versioned`]).
#[derive(Deserialize)]
struct WorkspacesOwned {
    workspaces: Vec<Workspace>,
}

/// File-based persistence for the workspace registry, rooted at the data
/// directory.
pub struct Storage {
    dir: PathBuf,
}

impl Storage {
    /// Open storage rooted at the default data directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the data directory cannot be determined
    /// (see [`data_dir`]).
    pub fn open_default() -> Result<Self> {
        Ok(Self::new(data_dir()?))
    }

    /// Open storage rooted at an explicit directory (mainly for tests).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Acquire this store's cross-process write lock, blocking until it is free.
    ///
    /// `workspaces.json` is read-modify-write — a mutation loads the list, edits
    /// it, then saves the whole file — and several usagi processes share this one
    /// global store (every TUI instance plus each session's `usagi mcp` server).
    /// Hold this guard across the entire load+save so a concurrent writer cannot
    /// read the same snapshot and overwrite the first writer's change (a lost
    /// update). The individual [`save_workspaces`](Self::save_workspaces) is
    /// already atomic; the lock serialises the *sequence*.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired (see
    /// [`StoreLock::acquire`]).
    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(&self.dir)
    }

    /// Load all workspaces; returns an empty list if the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when `workspaces.json` exists but cannot be read or parsed.
    pub fn load_workspaces(&self) -> Result<Vec<Workspace>> {
        let file: Option<WorkspacesOwned> =
            json_file::read_versioned(&self.dir.join(WORKSPACES_FILE))?;
        Ok(file.map(|f| f.workspaces).unwrap_or_default())
    }

    /// Write the whole workspace list to `workspaces.json`.
    ///
    /// # Errors
    ///
    /// Returns an error when the data directory cannot be created or the file
    /// cannot be written.
    pub fn save_workspaces(&self, workspaces: &[Workspace]) -> Result<()> {
        json_file::write_versioned(
            &self.dir,
            &self.dir.join(WORKSPACES_FILE),
            &WorkspacesRef { workspaces },
        )
    }

    /// Stamp `updated_at` onto the workspace named `name` and persist the change,
    /// returning the touched workspace, or `None` when no workspace has that name.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be read or written.
    pub fn touch_workspace(
        &self,
        name: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<Option<Workspace>> {
        let mut workspaces = self.load_workspaces()?;
        let Some(workspace) = workspaces.iter_mut().find(|w| w.name == name) else {
            return Ok(None);
        };
        workspace.updated_at = updated_at;
        let touched = workspace.clone();
        self.save_workspaces(&workspaces)?;
        Ok(Some(touched))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_storage() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = Storage::new(dir.path().join("usagi"));
        (dir, storage)
    }

    #[test]
    fn workspaces_round_trip_through_disk() {
        let (_dir, storage) = temp_storage();
        assert!(storage.load_workspaces().unwrap().is_empty());

        let workspaces = vec![Workspace::new("alpha", "/tmp/alpha")];
        storage.save_workspaces(&workspaces).unwrap();
        assert!(storage.dir().join("workspaces.json").is_file());

        let loaded = storage.load_workspaces().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "alpha");
    }

    #[test]
    fn touch_workspace_stamps_the_named_entry_and_ignores_others() {
        let (_dir, storage) = temp_storage();
        let base = Workspace::new("alpha", "/tmp/alpha");
        storage
            .save_workspaces(std::slice::from_ref(&base))
            .unwrap();

        let later = base.updated_at + chrono::Duration::seconds(60);
        let touched = storage.touch_workspace("alpha", later).unwrap();
        assert_eq!(touched.unwrap().updated_at, later);
        assert_eq!(storage.load_workspaces().unwrap()[0].updated_at, later);

        // An unknown name touches nothing and reports it.
        assert!(storage.touch_workspace("missing", later).unwrap().is_none());
    }

    #[test]
    fn open_default_roots_storage_under_the_data_directory() {
        let _guard = crate::test_support::process_env_guard();
        unsafe {
            std::env::set_var(
                crate::infrastructure::paths::DATA_DIR_ENV,
                "/tmp/usagi-ws-home",
            );
        }
        assert_eq!(
            Storage::open_default().unwrap().dir(),
            Path::new("/tmp/usagi-ws-home")
        );
        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    fn read_json_reports_a_parse_error() {
        let (_dir, storage) = temp_storage();
        fs::create_dir_all(storage.dir()).unwrap();
        fs::write(storage.dir().join(WORKSPACES_FILE), "{ broken").unwrap();
        assert!(storage.load_workspaces().is_err());
    }

    #[test]
    fn read_json_reports_a_non_not_found_error() {
        let (_dir, storage) = temp_storage();
        // A directory where the file is expected fails to read with an error
        // other than NotFound, exercising that arm of read.
        fs::create_dir_all(storage.dir().join(WORKSPACES_FILE)).unwrap();
        assert!(storage.load_workspaces().is_err());
    }

    #[test]
    fn write_json_reports_an_error_when_dir_cannot_be_created() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        // Place a *file* where the storage directory's parent should be, so
        // create_dir_all fails inside write_json.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let storage = Storage::new(blocker.join("nested"));
        assert!(storage.save_workspaces(&[]).is_err());
    }

    #[test]
    fn lock_is_a_dotfile_and_does_not_block_save() {
        let (_dir, storage) = temp_storage();
        // Holding the lock places a `.lock` dotfile in the data dir and still lets
        // the holder load and save (the lock serialises across processes, not
        // against the holder itself).
        let lock = storage.lock().unwrap();
        assert!(storage.dir().join(".lock").is_file());
        let workspaces = vec![Workspace::new("alpha", "/tmp/alpha")];
        storage.save_workspaces(&workspaces).unwrap();
        assert_eq!(storage.load_workspaces().unwrap().len(), 1);
        drop(lock);
    }

    #[test]
    fn lock_errors_when_the_dir_path_is_a_file() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        // A file where the data directory should be makes acquiring the lock fail.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let storage = Storage::new(blocker.join("nested"));
        assert!(storage.lock().is_err());
    }
}
