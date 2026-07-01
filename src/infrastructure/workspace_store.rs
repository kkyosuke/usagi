//! Persistence for a single repository's per-repo data.
//!
//! Everything lives inside the repository under `<repo>/.usagi/`, next to the
//! code it describes: `state.json` (the worktree snapshot) and `settings.json`
//! (project-local setting overrides). Writes go through a temp file + rename so
//! a crash never leaves a half-written file behind.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::domain::settings::LocalSettings;
use crate::domain::workspace_state::WorkspaceState;
use crate::infrastructure::json_file;
use crate::infrastructure::repo_paths::STATE_DIR;
use crate::infrastructure::store_lock::StoreLock;

const STATE_FILE: &str = "state.json";
const SETTINGS_FILE: &str = "settings.json";

/// File-based persistence rooted at a repository's `.usagi/` directory.
pub struct WorkspaceStore {
    dir: PathBuf,
}

impl WorkspaceStore {
    /// Open the store for the repository whose primary worktree is `repo_root`.
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            dir: repo_root.as_ref().join(STATE_DIR),
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

    /// Acquire this store's cross-process write lock, blocking until it is free.
    ///
    /// `state.json` is read-modify-write — a mutation loads it, edits the session
    /// list, then saves the whole file — and several usagi processes can share
    /// one workspace (the TUI plus a session's `usagi mcp` server). Hold this
    /// guard across the entire load+save so a concurrent writer cannot read the
    /// same snapshot and overwrite the first writer's change (a lost update). The
    /// individual [`save`](Self::save) is already atomic; the lock serialises the
    /// *sequence*. The per-store `.lock` lives in `.usagi/` and is kept out of git
    /// by usagi's `.gitignore` (see [`crate::infrastructure::gitignore`]).
    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(&self.dir)
    }

    /// Load the saved state, or `None` if it has never been written.
    pub fn load(&self) -> Result<Option<WorkspaceState>> {
        json_file::read_versioned(&self.state_path())
    }

    /// Persist `state` to `<repo>/.usagi/state.json`.
    pub fn save(&self, state: &WorkspaceState) -> Result<()> {
        json_file::write_versioned(&self.dir, &self.state_path(), state)
    }

    /// Load the project-local settings, or defaults (all fields unset) if none
    /// have been written.
    pub fn load_settings(&self) -> Result<LocalSettings> {
        let settings: Option<LocalSettings> = json_file::read_versioned(&self.settings_path())?;
        Ok(settings.unwrap_or_default())
    }

    /// Persist the project-local `settings` to `<repo>/.usagi/settings.json`.
    pub fn save_settings(&self, settings: &LocalSettings) -> Result<()> {
        json_file::write_versioned(&self.dir, &self.settings_path(), settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::{BranchStatus, SessionRecord, WorktreeState};
    use chrono::Utc;
    use std::fs;

    fn sample_state() -> WorkspaceState {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "feature".to_string(),
            display_name: Some("My Feature".to_string()),
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature"),
            worktrees: vec![WorktreeState {
                branch: Some("feature".to_string()),
                path: PathBuf::from("/repo/.usagi/sessions/feature"),
                head: "deadbee".to_string(),
                primary: false,
                upstream: Some("origin/feature".to_string()),
                status: BranchStatus::Pushed,
                diff: None,
                ahead_behind: None,
                pr: Vec::new(),
                updated_at: Utc::now(),
            }],
            created_at: Utc::now(),
            last_active: None,
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
            restore_panes_enabled: Some(false),
            default_branch_source: Some(BranchSource::Local),
            default_branch: Some("develop".to_string()),
            local_llm_enabled: Some(true),
            skill_features: [("pull-request".to_string(), false)].into_iter().collect(),
            env: [(
                "GH_TOKEN".to_string(),
                "op://Private/GitHub/token".to_string(),
            )]
            .into_iter()
            .collect(),
            setup_commands: vec!["npm install".to_string()],
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
    fn load_degrades_an_unknown_branch_status_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        // A state.json written by a *newer* usagi carries a worktree status this
        // build does not know. The whole session list must still load: the
        // unknown status degrades to the default (`New`, re-derived from git on
        // the next refresh) rather than failing every recorded session.
        fs::write(
            store.state_path(),
            r#"{"version":1,"sessions":[{"name":"feature","root":"/repo/.usagi/sessions/feature",
                "worktrees":[{"branch":"feature","path":"/repo/.usagi/sessions/feature",
                "head":"deadbee","status":"teleported","updated_at":"2026-01-01T00:00:00Z"}],
                "created_at":"2026-01-01T00:00:00Z"}],"updated_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let state = store.load().unwrap().unwrap();
        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].worktrees[0].status, BranchStatus::New);
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
    fn load_when_version_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        // A state.json with no `version` key must still load rather than failing
        // the whole file, matching the forward-compatibility kept elsewhere.
        fs::write(
            store.state_path(),
            r#"{"sessions":[],"updated_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let state = store.load().unwrap().unwrap();
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn lock_is_a_dotfile_in_the_usagi_dir_and_does_not_block_save() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());

        // Holding the lock places a `.lock` dotfile inside `.usagi/` (kept out of
        // git by usagi's `.gitignore`) and still lets the holder save and load.
        let lock = store.lock().unwrap();
        assert!(store.dir().join(".lock").is_file());
        let state = sample_state();
        store.save(&state).unwrap();
        assert_eq!(store.load().unwrap(), Some(state));
        drop(lock);
    }

    #[test]
    fn lock_errors_when_the_dir_path_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        // A file where `.usagi/` should be makes acquiring the lock fail.
        let blocker = dir.path().join("repo");
        fs::write(&blocker, "not a directory").unwrap();
        assert!(WorkspaceStore::new(&blocker).lock().is_err());
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
