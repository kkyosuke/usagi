use anyhow::Result;

use crate::domain::settings::{Settings, Theme};
use crate::infrastructure::storage::Storage;

/// Load the current settings (defaults if none have been saved yet).
pub fn load(storage: &Storage) -> Result<Settings> {
    storage.load_settings()
}

/// Change the UI theme and persist it.
pub fn set_theme(storage: &Storage, theme: Theme) -> Result<Settings> {
    let mut settings = storage.load_settings()?;
    settings.theme = theme;
    storage.save_settings(&settings)?;
    Ok(settings)
}

/// Set or clear the default workspace and persist it.
pub fn set_default_workspace(storage: &Storage, name: Option<String>) -> Result<Settings> {
    let mut settings = storage.load_settings()?;
    settings.default_workspace = name;
    storage.save_settings(&settings)?;
    Ok(settings)
}
