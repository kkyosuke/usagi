use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::settings::Settings;
use crate::domain::workspace::Workspace;
use crate::infrastructure::json_file;
use crate::infrastructure::store_lock::StoreLock;

/// Environment variable that overrides the default data directory.
pub const DATA_DIR_ENV: &str = "USAGI_HOME";
/// Directory created under the user's home directory by default.
const DATA_DIR_NAME: &str = ".usagi";

const WORKSPACES_FILE: &str = "workspaces.json";
const SETTINGS_FILE: &str = "settings.json";

const FILE_FORMAT_VERSION: u32 = 1;

/// Resolve the directory where usagi stores its data.
///
/// `$USAGI_HOME` takes precedence; otherwise `~/.usagi` is used.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs::home_dir().context("could not determine the home directory")?;
    Ok(home.join(DATA_DIR_NAME))
}

/// On-disk shape of `workspaces.json`.
#[derive(Serialize, Deserialize)]
struct WorkspacesFile {
    // `default` so a file missing `version` (hand-edited, corrupted, or written
    // by a hypothetical format that dropped it) still loads, matching the
    // forward-compatibility the rest of the on-disk types keep (`serde(default)`
    // / `serde(alias)`, no `deny_unknown_fields`). A missing version reads as 0.
    #[serde(default)]
    version: u32,
    workspaces: Vec<Workspace>,
}

/// On-disk shape of `settings.json`.
#[derive(Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    version: u32,
    #[serde(flatten)]
    settings: Settings,
}

/// File-based persistence for workspaces and settings.
pub struct Storage {
    dir: PathBuf,
}

impl Storage {
    /// Open storage rooted at the default data directory.
    pub fn open_default() -> Result<Self> {
        Ok(Self::new(data_dir()?))
    }

    /// Open storage rooted at an explicit directory (mainly for tests).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

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
    /// update, e.g. a dropped registration or a clobbered `touch`). The individual
    /// [`save_workspaces`](Self::save_workspaces) is already atomic; the lock
    /// serialises the *sequence*. The per-store `.lock` lives in the data dir and,
    /// being a dotfile, is never parsed as data.
    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(&self.dir)
    }

    /// Path to the global `settings.json` file (it may not exist yet).
    pub fn settings_path(&self) -> PathBuf {
        self.dir.join(SETTINGS_FILE)
    }

    /// Load all workspaces; returns an empty list if the file does not exist.
    pub fn load_workspaces(&self) -> Result<Vec<Workspace>> {
        let file: Option<WorkspacesFile> = json_file::read(&self.dir.join(WORKSPACES_FILE))?;
        Ok(file.map(|f| f.workspaces).unwrap_or_default())
    }

    pub fn save_workspaces(&self, workspaces: &[Workspace]) -> Result<()> {
        json_file::write_atomic(
            &self.dir,
            &self.dir.join(WORKSPACES_FILE),
            &WorkspacesFile {
                version: FILE_FORMAT_VERSION,
                workspaces: workspaces.to_vec(),
            },
        )
    }

    /// Load settings; returns defaults if the file does not exist.
    ///
    /// Loaded settings are sanitized (see [`Settings::sanitized`]) because the
    /// file can be hand-edited: a `local_llm.model` outside the known allowlist
    /// is dropped before it can reach the agent launch command.
    pub fn load_settings(&self) -> Result<Settings> {
        let file: Option<SettingsFile> = json_file::read(&self.settings_path())?;
        Ok(file.map(|f| f.settings.sanitized()).unwrap_or_default())
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        json_file::write_atomic(
            &self.dir,
            &self.settings_path(),
            &SettingsFile {
                version: FILE_FORMAT_VERSION,
                settings: settings.clone(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::Theme;
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
    fn settings_round_trip_through_disk() {
        let (_dir, storage) = temp_storage();
        assert_eq!(storage.load_settings().unwrap(), Settings::default());

        let settings = Settings {
            theme: Theme::Dark,
            ..Default::default()
        };
        storage.save_settings(&settings).unwrap();

        assert_eq!(storage.load_settings().unwrap(), settings);
    }

    #[test]
    fn load_settings_sanitizes_a_hand_edited_local_llm_model() {
        let (_dir, storage) = temp_storage();
        fs::create_dir_all(storage.dir()).unwrap();
        // A user (or synced dotfiles) hand-edits settings.json with a model name
        // that never came from usagi's allowlist. On load it must be dropped so it
        // cannot reach the agent launch command.
        fs::write(
            storage.settings_path(),
            r#"{"version":1,"local_llm":{"enabled":true,"model":"x';touch /tmp/pwned;'"}}"#,
        )
        .unwrap();
        let loaded = storage.load_settings().unwrap();
        assert_eq!(
            loaded.local_llm.model,
            crate::domain::settings::DEFAULT_LOCAL_LLM_MODEL
        );
        // Other hand-edited fields still load (only the model is policed).
        assert!(loaded.local_llm.enabled);
    }

    #[test]
    fn load_settings_when_version_is_missing() {
        let (_dir, storage) = temp_storage();
        fs::create_dir_all(storage.dir()).unwrap();
        // A settings.json with no `version` key (hand-edited or from a format that
        // dropped it) must still load rather than failing the whole file.
        fs::write(
            storage.settings_path(),
            r#"{"notifications_enabled":false}"#,
        )
        .unwrap();
        let loaded = storage.load_settings().unwrap();
        assert!(!loaded.notifications_enabled);
    }

    #[test]
    fn load_settings_degrades_unknown_enum_values_instead_of_failing() {
        let (_dir, storage) = temp_storage();
        fs::create_dir_all(storage.dir()).unwrap();
        // A settings.json written by a *newer* usagi (or hand-edited) carries an
        // unknown `agent_cli` and `theme`. The whole file must still load: the
        // unknown enums fall back to their defaults while the known fields below
        // them are preserved.
        fs::write(
            storage.settings_path(),
            r#"{"version":1,"theme":"midnight","agent_cli":"sakana","notifications_enabled":false}"#,
        )
        .unwrap();
        let loaded = storage.load_settings().unwrap();
        assert_eq!(loaded.theme, Theme::default());
        assert_eq!(
            loaded.agent_cli,
            crate::domain::settings::AgentCli::default()
        );
        // A field after the unrecognised ones still loaded.
        assert!(!loaded.notifications_enabled);
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
        // other than NotFound, exercising that arm of read_json.
        fs::create_dir_all(storage.dir().join(SETTINGS_FILE)).unwrap();
        assert!(storage.load_settings().is_err());
    }

    #[test]
    fn write_json_reports_an_error_when_dir_cannot_be_created() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        // Place a *file* where the storage directory's parent should be, so
        // create_dir_all fails inside write_json.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let storage = Storage::new(blocker.join("nested"));
        assert!(storage.save_settings(&Settings::default()).is_err());
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

    #[test]
    fn data_dir_prefers_env_override_then_falls_back() {
        // Serialize $USAGI_HOME mutation against other globals-mutating tests.
        let _guard = crate::test_support::process_env_guard();
        std::env::set_var(DATA_DIR_ENV, "/tmp/usagi-unit-home");
        assert_eq!(data_dir().unwrap(), PathBuf::from("/tmp/usagi-unit-home"));
        assert_eq!(
            Storage::open_default().unwrap().dir(),
            Path::new("/tmp/usagi-unit-home")
        );

        // An empty override is ignored in favour of the home-directory default.
        std::env::set_var(DATA_DIR_ENV, "");
        assert!(data_dir().unwrap().ends_with(".usagi"));

        std::env::remove_var(DATA_DIR_ENV);
        assert!(data_dir().unwrap().ends_with(".usagi"));
    }
}
