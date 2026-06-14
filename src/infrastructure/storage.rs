use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::domain::settings::Settings;
use crate::domain::workspace::Workspace;

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
    version: u32,
    workspaces: Vec<Workspace>,
}

/// On-disk shape of `settings.json`.
#[derive(Serialize, Deserialize)]
struct SettingsFile {
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

    /// Path to the global `settings.json` file (it may not exist yet).
    pub fn settings_path(&self) -> PathBuf {
        self.dir.join(SETTINGS_FILE)
    }

    /// Load all workspaces; returns an empty list if the file does not exist.
    pub fn load_workspaces(&self) -> Result<Vec<Workspace>> {
        let file: Option<WorkspacesFile> = self.read_json(WORKSPACES_FILE)?;
        Ok(file.map(|f| f.workspaces).unwrap_or_default())
    }

    pub fn save_workspaces(&self, workspaces: &[Workspace]) -> Result<()> {
        self.write_json(
            WORKSPACES_FILE,
            &WorkspacesFile {
                version: FILE_FORMAT_VERSION,
                workspaces: workspaces.to_vec(),
            },
        )
    }

    /// Load settings; returns defaults if the file does not exist.
    pub fn load_settings(&self) -> Result<Settings> {
        let file: Option<SettingsFile> = self.read_json(SETTINGS_FILE)?;
        Ok(file.map(|f| f.settings).unwrap_or_default())
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        self.write_json(
            SETTINGS_FILE,
            &SettingsFile {
                version: FILE_FORMAT_VERSION,
                settings: settings.clone(),
            },
        )
    }

    fn read_json<T: DeserializeOwned>(&self, file_name: &str) -> Result<Option<T>> {
        let path = self.dir.join(file_name);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context(format!("failed to read {}", path.display())),
        };
        let value =
            serde_json::from_str(&text).context(format!("failed to parse {}", path.display()))?;
        Ok(Some(value))
    }

    fn write_json<T: Serialize>(&self, file_name: &str, value: &T) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;
        let path = self.dir.join(file_name);
        let mut text = serde_json::to_string_pretty(value)?;
        text.push('\n');
        // Write to a temp file then rename so a crash never leaves a half-written file.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).context(format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &path).context(format!("failed to replace {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::Theme;

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
