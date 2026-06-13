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
#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspacesFile {
    version: u32,
    workspaces: Vec<Workspace>,
}

/// On-disk shape of `settings.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
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
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
        };
        let value = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(value))
    }

    fn write_json<T: Serialize>(&self, file_name: &str, value: &T) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create {}", self.dir.display()))?;
        let path = self.dir.join(file_name);
        let mut text = serde_json::to_string_pretty(value)?;
        text.push('\n');
        // Write to a temp file then rename so a crash never leaves a half-written file.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    }
}
