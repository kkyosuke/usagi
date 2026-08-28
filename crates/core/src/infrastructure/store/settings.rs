//! Workspace-local Agent, Issue, and Memory settings persistence.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::domain::settings::{LocalSettings, validate_env_limits};
use crate::infrastructure::paths::{RuntimeMode, project_data_dir, project_data_dir_for};
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

    /// Address a workspace settings store in an explicit runtime mode.
    ///
    /// This keeps callers that already own a channel from re-reading an
    /// unrelated ambient `USAGI_RUNTIME_MODE` value.
    #[must_use]
    pub fn new_for_mode(workspace_root: impl AsRef<Path>, mode: RuntimeMode) -> Self {
        Self {
            dir: project_data_dir_for(workspace_root, mode),
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
        let settings: LocalSettings = json_file::read_versioned(&self.path())?.unwrap_or_default();
        validate_env_limits(&settings.env)?;
        Ok(settings)
    }

    /// Atomically and durably persist local overrides.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory or settings file cannot be written.
    pub fn save(&self, settings: &LocalSettings) -> Result<()> {
        validate_env_limits(&settings.env)?;
        json_file::write_versioned(&self.dir, &self.path(), settings)
    }

    /// Persist workspace defaults when no local settings file exists yet.
    ///
    /// The project lock makes concurrent initializers converge on the first
    /// complete file. Existing workspace choices are never overwritten.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock, existence check, or write fails.
    pub fn initialize(&self, settings: &LocalSettings) -> Result<()> {
        let _lock = self.lock()?;
        if self.path().try_exists()? {
            return Ok(());
        }
        self.save(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::{DefaultModel, LocalSettings};
    use std::fs;

    #[test]
    fn missing_settings_are_empty_and_save_round_trips_under_lock() {
        let workspace = tempfile::tempdir().unwrap();
        let store = WorkspaceSettingsStore::new(workspace.path());
        assert_eq!(store.load().unwrap(), LocalSettings::default());

        let settings = LocalSettings {
            default_model: Some(DefaultModel::Claude),
            issue_enabled: Some(false),
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

    #[test]
    fn initialize_writes_once_without_overwriting_workspace_choices() {
        let workspace = tempfile::tempdir().unwrap();
        let store = WorkspaceSettingsStore::new(workspace.path());
        let initial = LocalSettings {
            default_model: Some(DefaultModel::Claude),
            issue_enabled: Some(false),
            memory_enabled: Some(true),
            team_template: Some(crate::domain::settings::TeamTemplate::Flat),
            env: [("PROJECT".to_owned(), "usagi".to_owned())]
                .into_iter()
                .collect(),
        };
        store.initialize(&initial).unwrap();
        assert_eq!(store.load().unwrap(), initial);

        store.initialize(&LocalSettings::default()).unwrap();
        assert_eq!(store.load().unwrap(), initial);
    }

    #[test]
    fn explicit_mode_does_not_follow_the_ambient_process_channel() {
        let workspace = tempfile::tempdir().unwrap();
        let local = WorkspaceSettingsStore::new_for_mode(workspace.path(), RuntimeMode::Local);
        let production =
            WorkspaceSettingsStore::new_for_mode(workspace.path(), RuntimeMode::Production);

        assert_eq!(
            local.path(),
            workspace.path().join(".usagi/local/settings.json")
        );
        assert_eq!(
            production.path(),
            workspace.path().join(".usagi/settings.json")
        );
    }

    #[test]
    fn load_and_save_refuse_the_domain_env_limit() {
        use crate::domain::settings::{EnvLimitError, MAX_ENV_BINDINGS};

        let workspace = tempfile::tempdir().unwrap();
        let store = WorkspaceSettingsStore::new(workspace.path());
        let oversized = LocalSettings {
            env: (0..=MAX_ENV_BINDINGS)
                .map(|index| (format!("VALUE_{index}"), "literal".to_owned()))
                .collect(),
            ..LocalSettings::default()
        };
        let save_error = store.save(&oversized).unwrap_err();
        assert!(save_error.downcast_ref::<EnvLimitError>().is_some());
        assert!(!store.path().exists());

        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(
            store.path(),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "env": oversized.env,
            }))
            .unwrap(),
        )
        .unwrap();
        let load_error = store.load().unwrap_err();
        assert!(load_error.downcast_ref::<EnvLimitError>().is_some());
    }
}
