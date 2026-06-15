//! Persistence for a single repository's per-repo data.
//!
//! Everything lives inside the repository under `<repo>/.usagi/`, next to the
//! code it describes: `state.json` (the worktree snapshot) and `settings.json`
//! (project-local setting overrides). Writes go through a temp file + rename so
//! a crash never leaves a half-written file behind.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::domain::settings::LocalSettings;
use crate::domain::workspace_state::WorkspaceState;

/// Directory created inside the repository to hold usagi's per-repo data.
const STATE_DIR_NAME: &str = ".usagi";
const STATE_FILE: &str = "state.json";
const SETTINGS_FILE: &str = "settings.json";

const FILE_FORMAT_VERSION: u32 = 1;

/// On-disk shape of `state.json`.
#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    #[serde(flatten)]
    state: WorkspaceState,
}

/// On-disk shape of the per-repo `settings.json`.
#[derive(Debug, Serialize, Deserialize)]
struct LocalSettingsFile {
    version: u32,
    #[serde(flatten)]
    settings: LocalSettings,
}

/// File-based persistence rooted at a repository's `.usagi/` directory.
pub struct WorkspaceStore {
    dir: PathBuf,
}

impl WorkspaceStore {
    /// Open the store for the repository whose primary worktree is `repo_root`.
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            dir: repo_root.as_ref().join(STATE_DIR_NAME),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    pub fn settings_path(&self) -> PathBuf {
        self.dir.join(SETTINGS_FILE)
    }

    /// Load the saved state, or `None` if it has never been written.
    pub fn load(&self) -> Result<Option<WorkspaceState>> {
        let file: Option<StateFile> = self.read_json(&self.state_path())?;
        Ok(file.map(|f| f.state))
    }

    /// Persist `state` to `<repo>/.usagi/state.json`.
    pub fn save(&self, state: &WorkspaceState) -> Result<()> {
        self.write_json(
            &self.state_path(),
            &StateFile {
                version: FILE_FORMAT_VERSION,
                state: state.clone(),
            },
        )
    }

    /// Load the project-local settings, or defaults (all fields unset) if none
    /// have been written.
    pub fn load_settings(&self) -> Result<LocalSettings> {
        let file: Option<LocalSettingsFile> = self.read_json(&self.settings_path())?;
        Ok(file.map(|f| f.settings).unwrap_or_default())
    }

    /// Persist the project-local `settings` to `<repo>/.usagi/settings.json`.
    pub fn save_settings(&self, settings: &LocalSettings) -> Result<()> {
        self.write_json(
            &self.settings_path(),
            &LocalSettingsFile {
                version: FILE_FORMAT_VERSION,
                settings: settings.clone(),
            },
        )
    }

    /// Read and deserialize a JSON file under `.usagi/`, returning `None` if it
    /// does not exist.
    fn read_json<T: DeserializeOwned>(&self, path: &Path) -> Result<Option<T>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context(format!("failed to read {}", path.display())),
        };
        let value =
            serde_json::from_str(&text).context(format!("failed to parse {}", path.display()))?;
        Ok(Some(value))
    }

    /// Serialize `value` and write it atomically (temp file + rename) to `path`,
    /// creating the `.usagi/` directory if needed.
    fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;

        let mut text = serde_json::to_string_pretty(value)?;
        text.push('\n');

        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).context(format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, path).context(format!("failed to replace {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::{BranchStatus, SessionRecord, WorktreeState};
    use chrono::Utc;

    fn sample_state() -> WorkspaceState {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "feature".to_string(),
            root: PathBuf::from("/repo/.usagi/sessions/feature"),
            worktrees: vec![WorktreeState {
                branch: Some("feature".to_string()),
                path: PathBuf::from("/repo/.usagi/sessions/feature"),
                head: "deadbee".to_string(),
                primary: false,
                upstream: Some("origin/feature".to_string()),
                status: BranchStatus::Pushed,
                updated_at: Utc::now(),
            }],
            created_at: Utc::now(),
        });
        state
    }

    #[test]
    fn load_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        let state = sample_state();

        store.save(&state).unwrap();
        assert!(store.state_path().exists());
        assert_eq!(store.load().unwrap(), Some(state));
    }

    #[test]
    fn saved_file_records_the_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        store.save(&sample_state()).unwrap();

        let text = std::fs::read_to_string(store.state_path()).unwrap();
        assert!(text.contains("\"version\": 1"));
    }

    #[test]
    fn dir_points_at_the_usagi_subdirectory() {
        let store = WorkspaceStore::new("/repo");
        assert_eq!(store.dir(), Path::new("/repo/.usagi"));
        assert_eq!(store.state_path(), PathBuf::from("/repo/.usagi/state.json"));
        assert_eq!(
            store.settings_path(),
            PathBuf::from("/repo/.usagi/settings.json")
        );
    }

    #[test]
    fn load_settings_defaults_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        assert_eq!(store.load_settings().unwrap(), LocalSettings::default());
    }

    #[test]
    fn save_then_load_settings_round_trips() {
        use crate::domain::settings::{AgentCli, BranchSource};

        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        let settings = LocalSettings {
            agent_cli: Some(AgentCli::Gemini),
            notifications_enabled: Some(false),
            default_branch_source: Some(BranchSource::Local),
            default_branch: Some("develop".to_string()),
            local_llm_enabled: Some(true),
        };

        store.save_settings(&settings).unwrap();
        assert!(store.settings_path().exists());
        assert_eq!(store.load_settings().unwrap(), settings);
    }

    #[test]
    fn saved_settings_file_records_the_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        store.save_settings(&LocalSettings::default()).unwrap();

        let text = std::fs::read_to_string(store.settings_path()).unwrap();
        assert!(text.contains("\"version\": 1"));
    }

    #[test]
    fn load_settings_errors_on_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.settings_path(), "{ not json").unwrap();
        assert!(store.load_settings().is_err());
    }

    #[test]
    fn save_settings_errors_when_the_directory_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the `.usagi/` directory should be makes create_dir_all fail.
        let blocker = dir.path().join("repo");
        fs::write(&blocker, "not a directory").unwrap();
        let store = WorkspaceStore::new(&blocker);
        assert!(store.save_settings(&LocalSettings::default()).is_err());
    }

    #[test]
    fn load_errors_on_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.state_path(), "{ not json").unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn load_errors_when_state_path_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        // Make state.json a directory so reading it fails with a non-NotFound error.
        fs::create_dir_all(store.state_path()).unwrap();
        assert!(store.load().is_err());
    }
}
