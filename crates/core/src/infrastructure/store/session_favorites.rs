//! Workspace-local session favorites and sidebar display order.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::id::SessionId;
use crate::infrastructure::paths::project_data_dir;
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};

#[derive(Default, Serialize, Deserialize)]
struct SessionList<T> {
    sessions: T,
}

const MAX_PREFERENCE_BYTES: usize = 1024 * 1024;

/// Workspace-local preferences keyed by session incarnation, never by name.
pub struct SessionFavoritesStore {
    dir: PathBuf,
}

impl SessionFavoritesStore {
    #[must_use]
    pub fn new(workspace: &Path) -> Self {
        Self {
            dir: project_data_dir(workspace),
        }
    }

    /// Read favorites, treating a missing file as an empty preference set.
    ///
    /// # Errors
    /// Returns an error if the file is not regular, cannot be read or decoded,
    /// or uses a newer schema.
    pub fn load(&self) -> Result<BTreeSet<SessionId>> {
        let path = self.dir.join("session-favorites.json");
        read_session_list(&path)
    }

    /// Read the last manually arranged sidebar order for this workspace.
    ///
    /// # Errors
    /// Returns an error for an unreadable or unsupported preference file.
    pub fn load_order(&self) -> Result<Vec<SessionId>> {
        read_session_list(&self.dir.join("session-order.json"))
    }

    /// Atomically replace the entire display preference on an explicit move.
    ///
    /// # Errors
    /// Returns an error if the existing order is unreadable or the write fails.
    pub fn save_order(&self, sessions: &[SessionId]) -> Result<()> {
        let path = self.dir.join("session-order.json");
        self.load_order()?;
        let list = SessionList { sessions };
        json_file::write_versioned_bounded(&self.dir, &path, &list, MAX_PREFERENCE_BYTES)
    }

    /// Toggle only this identity while preserving concurrent writers' choices.
    ///
    /// # Errors
    /// Returns an error if locking, reading, or durable writing fails.
    pub fn toggle(&self, session: SessionId) -> Result<BTreeSet<SessionId>> {
        let _lock = StoreLock::acquire(&self.dir)?;
        self.toggle_locked(session)
    }

    /// Toggle while allowing an owner leaving the workspace to cancel its lock
    /// wait. Once acquired, the complete read-modify-write finishes under the
    /// guard so cancellation cannot interrupt a durable mutation.
    ///
    /// # Errors
    /// Returns an error on cancellation, locking, reading, or durable writing.
    pub fn toggle_cancellable(
        &self,
        session: SessionId,
        cancelled: impl Fn() -> bool,
    ) -> Result<BTreeSet<SessionId>> {
        let _lock = StoreLock::acquire_cancellable(&self.dir, cancelled)?;
        self.toggle_locked(session)
    }

    fn toggle_locked(&self, session: SessionId) -> Result<BTreeSet<SessionId>> {
        let mut favorites = self.load()?;
        if !favorites.remove(&session) {
            favorites.insert(session);
        }
        json_file::write_versioned(
            &self.dir,
            &self.dir.join("session-favorites.json"),
            &SessionList {
                sessions: favorites.clone(),
            },
        )?;
        Ok(favorites)
    }
}

fn read_session_list<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO must not wait for a writer before its type can be checked.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(T::default());
        }
        Err(error) => {
            return Err(error).context(format!("failed to read {}", path.display()));
        }
    };
    // Inspect and read the same descriptor so a path replacement cannot
    // substitute a pipe or device after validation.
    anyhow::ensure!(
        file.metadata()
            .context(format!("failed to inspect {}", path.display()))?
            .is_file(),
        "session preference is not a regular file: {}",
        path.display()
    );
    let mut text = String::new();
    file.take(MAX_PREFERENCE_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .context(format!("failed to read {}", path.display()))?;
    anyhow::ensure!(
        text.len() <= MAX_PREFERENCE_BYTES,
        "session preference exceeds the {MAX_PREFERENCE_BYTES}-byte limit: {}",
        path.display()
    );
    let list: SessionList<T> = json_file::decode_supported_version(path, &text)?;
    Ok(list.sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favorites_survive_reopen_and_writers_preserve_other_sessions() {
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionFavoritesStore::new(workspace.path());
        let other = SessionFavoritesStore::new(workspace.path());
        let first = SessionId::new();
        let second = SessionId::new();
        assert!(store.load().unwrap().is_empty());
        assert_eq!(store.toggle(first).unwrap(), BTreeSet::from([first]));
        assert_eq!(other.load().unwrap(), BTreeSet::from([first]));
        assert_eq!(
            other.toggle(second).unwrap(),
            BTreeSet::from([first, second])
        );
        assert_eq!(store.toggle(first).unwrap(), BTreeSet::from([second]));
        assert!(!other.load().unwrap().contains(&SessionId::new()));
        assert!(other.toggle(second).unwrap().is_empty());
    }

    #[test]
    fn session_order_is_workspace_local_and_failed_writes_preserve_the_last_order() {
        use crate::infrastructure::persistence::json_file::{
            AtomicWriteStage, fail_next_atomic_write,
        };

        let workspace = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let store = SessionFavoritesStore::new(workspace.path());
        let [first, second] = std::array::from_fn(|_| SessionId::new());
        store.save_order(&[second, first]).unwrap();
        store.toggle(first).unwrap();
        assert_eq!(
            SessionFavoritesStore::new(other.path())
                .load_order()
                .unwrap(),
            [] as [SessionId; 0]
        );
        fail_next_atomic_write(
            &store.dir.join("session-order.json"),
            AtomicWriteStage::Rename,
        );
        assert!(store.save_order(&[first, second]).is_err());
        assert_eq!(store.load_order().unwrap(), [second, first]);
        assert_eq!(store.load().unwrap(), BTreeSet::from([first]));
    }

    #[test]
    fn cancelling_a_toggle_preserves_preferences_and_a_live_toggle_still_saves() {
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionFavoritesStore::new(workspace.path());
        let first = SessionId::new();
        let second = SessionId::new();
        assert_eq!(
            store.toggle_cancellable(first, || false).unwrap(),
            BTreeSet::from([first])
        );
        let held = StoreLock::acquire(&store.dir).unwrap();
        assert!(store.toggle_cancellable(second, || true).is_err());
        assert_eq!(store.load().unwrap(), BTreeSet::from([first]));
        drop(held);
    }

    #[test]
    fn failed_save_preserves_previous_favorites_and_workspaces_are_isolated() {
        use crate::infrastructure::persistence::json_file::{
            AtomicWriteStage, fail_next_atomic_write,
        };
        let workspace = tempfile::tempdir().unwrap();
        let other_workspace = tempfile::tempdir().unwrap();
        let store = SessionFavoritesStore::new(workspace.path());
        let other = SessionFavoritesStore::new(other_workspace.path());
        let session = SessionId::new();
        store.toggle(session).unwrap();
        assert!(other.load().unwrap().is_empty());
        fail_next_atomic_write(
            &store.dir.join("session-favorites.json"),
            AtomicWriteStage::Rename,
        );
        assert!(store.toggle(session).is_err());
        assert_eq!(store.load().unwrap(), BTreeSet::from([session]));
    }

    #[test]
    fn newer_preferences_are_rejected_without_losing_their_contents() {
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionFavoritesStore::new(workspace.path());
        std::fs::create_dir_all(&store.dir).unwrap();
        let path = store.dir.join("session-favorites.json");
        let source = format!(
            r#"{{"version":{},"sessions":[],"future_field":"keep me"}}"#,
            json_file::FILE_FORMAT_VERSION + 1,
        );
        std::fs::write(&path, &source).unwrap();
        assert!(store.load().is_err());
        assert!(store.toggle(SessionId::new()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
    }

    #[test]
    fn corrupt_preferences_are_not_overwritten() {
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionFavoritesStore::new(workspace.path());
        std::fs::create_dir_all(&store.dir).unwrap();
        let path = store.dir.join("session-favorites.json");
        std::fs::write(&path, "broken").unwrap();
        assert!(store.load().is_err());
        assert!(store.toggle(SessionId::new()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken");

        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_PREFERENCE_BYTES as u64 + 1)
            .unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn legacy_preferences_remain_readable_and_invalid_utf8_is_preserved() {
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionFavoritesStore::new(workspace.path());
        std::fs::create_dir_all(&store.dir).unwrap();
        let path = store.dir.join("session-favorites.json");
        let session = SessionId::new();
        let legacy = serde_json::json!({"sessions": [session]});
        std::fs::write(&path, legacy.to_string()).unwrap();
        assert_eq!(store.load().unwrap(), BTreeSet::from([session]));
        std::fs::write(&path, [0xff]).unwrap();
        assert!(store.load().is_err());
        assert!(store.toggle(session).is_err());
        assert_eq!(std::fs::read(path).unwrap(), [0xff]);
    }

    #[test]
    fn directory_preferences_are_rejected_without_replacing_the_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionFavoritesStore::new(workspace.path());
        let path = store.dir.join("session-favorites.json");
        std::fs::create_dir_all(&path).unwrap();
        assert!(store.load().is_err());
        assert!(store.toggle(SessionId::new()).is_err());
        assert!(path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn an_unopenable_preference_path_is_rejected_without_replacing_it() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let store = SessionFavoritesStore::new(workspace.path());
        std::fs::create_dir_all(&store.dir).unwrap();
        let path = store.dir.join("session-favorites.json");
        symlink("session-favorites.json", &path).unwrap();
        assert!(store.load().is_err());
        assert!(store.toggle(SessionId::new()).is_err());
        assert_eq!(
            std::fs::read_link(path).unwrap(),
            Path::new("session-favorites.json")
        );
    }
}
