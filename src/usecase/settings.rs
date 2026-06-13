use anyhow::Result;

use crate::domain::settings::{AgentCli, Settings, Theme};
use crate::infrastructure::storage::Storage;

/// Load the current settings (defaults if none have been saved yet).
pub fn load(storage: &Storage) -> Result<Settings> {
    storage.load_settings()
}

/// Persist the given settings as-is.
pub fn save(storage: &Storage, settings: &Settings) -> Result<()> {
    storage.save_settings(settings)
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

/// Enable or disable desktop notifications and persist the choice.
pub fn set_notifications_enabled(storage: &Storage, enabled: bool) -> Result<Settings> {
    let mut settings = storage.load_settings()?;
    settings.notifications_enabled = enabled;
    storage.save_settings(&settings)?;
    Ok(settings)
}

/// Change which agent CLI usagi drives and persist it.
pub fn set_agent_cli(storage: &Storage, agent_cli: AgentCli) -> Result<Settings> {
    let mut settings = storage.load_settings()?;
    settings.agent_cli = agent_cli;
    storage.save_settings(&settings)?;
    Ok(settings)
}
