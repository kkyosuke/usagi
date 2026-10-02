//! User-owned session favorites, separate from daemon lifecycle authority.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::domain::id::SessionId;
use crate::infrastructure::paths::project_data_dir;
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};

#[derive(Default, Serialize, Deserialize)]
struct Favorites {
    sessions: BTreeSet<SessionId>,
}

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
    /// Returns an error if the file cannot be read or decoded.
    pub fn load(&self) -> Result<BTreeSet<SessionId>> {
        let favorites: Favorites =
            json_file::read_versioned(&self.dir.join("session-favorites.json"))?
                .unwrap_or_default();
        Ok(favorites.sessions)
    }

    /// Toggle only this identity while preserving concurrent writers' choices.
    ///
    /// # Errors
    /// Returns an error if locking, reading, or durable writing fails.
    pub fn toggle(&self, session: SessionId) -> Result<BTreeSet<SessionId>> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut favorites = self.load()?;
        if !favorites.remove(&session) {
            favorites.insert(session);
        }
        json_file::write_versioned(
            &self.dir,
            &self.dir.join("session-favorites.json"),
            &Favorites {
                sessions: favorites.clone(),
            },
        )?;
        Ok(favorites)
    }
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
    fn corrupt_preferences_are_not_overwritten() {
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionFavoritesStore::new(workspace.path());
        std::fs::create_dir_all(&store.dir).unwrap();
        let path = store.dir.join("session-favorites.json");
        std::fs::write(&path, "broken").unwrap();
        assert!(store.load().is_err());
        assert!(store.toggle(SessionId::new()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "broken");
    }
}
