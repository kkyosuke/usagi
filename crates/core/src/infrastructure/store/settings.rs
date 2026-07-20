//! Workspace-local settings persistence.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::domain::settings::LocalSettings;
use crate::infrastructure::paths::project_data_dir;
use crate::infrastructure::persistence::json_file;
use crate::infrastructure::persistence::store_lock::StoreLock;

const SETTINGS_FILE: &str = "settings.json";

/// File-backed local overrides for one workspace identity.
pub struct WorkspaceSettingsStore {
    dir: PathBuf,
}

impl WorkspaceSettingsStore {
    #[must_use]
    pub fn new(workspace_root: impl AsRef<Path>) -> Self {
        Self {
            dir: project_data_dir(workspace_root),
        }
    }

    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.join(SETTINGS_FILE)
    }

    /// Acquire the project store lock before a write.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be acquired.
    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(&self.dir)
    }

    /// Load local overrides; a missing file is the empty overlay.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings file cannot be read or parsed.
    pub fn load(&self) -> Result<LocalSettings> {
        Ok(json_file::read_versioned(&self.path())?.unwrap_or_default())
    }

    /// Atomically and durably persist local overrides.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory or settings file cannot be written.
    pub fn save(&self, settings: &LocalSettings) -> Result<()> {
        json_file::write_versioned(&self.dir, &self.path(), settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::{LocalSettings, ModalSelectionMode};
    use std::fs;

    #[test]
    fn missing_settings_are_empty_and_save_round_trips_under_lock() {
        let workspace = tempfile::tempdir().unwrap();
        let store = WorkspaceSettingsStore::new(workspace.path());
        assert_eq!(store.load().unwrap(), LocalSettings::default());

        let settings = LocalSettings {
            modal_selection_mode: Some(ModalSelectionMode::Prompt),
            ..LocalSettings::default()
        };
        let _lock = store.lock().unwrap();
        store.save(&settings).unwrap();
        assert_eq!(store.load().unwrap(), settings);
        assert!(store.path().is_file());
        assert!(store.path().parent().unwrap().join(".lock").is_file());
        assert!(
            fs::read_to_string(store.path())
                .unwrap()
                .contains("\"version\": 1")
        );
    }

    #[test]
    fn corrupt_settings_are_reported() {
        let workspace = tempfile::tempdir().unwrap();
        let store = WorkspaceSettingsStore::new(workspace.path());
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), "{ broken").unwrap();
        assert!(store.load().is_err());
    }
}
