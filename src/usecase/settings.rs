use std::path::Path;

use anyhow::Result;

use crate::domain::settings::{AgentCli, LocalSettings, Settings, Theme};
use crate::infrastructure::storage::Storage;
use crate::infrastructure::workspace_store::WorkspaceStore;

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

/// Load the project-local setting overrides for the repository at `repo_root`
/// (all fields unset if none have been saved).
pub fn load_local(repo_root: &Path) -> Result<LocalSettings> {
    WorkspaceStore::new(repo_root).load_settings()
}

/// Persist the project-local setting overrides for the repository at `repo_root`.
pub fn save_local(repo_root: &Path, local: &LocalSettings) -> Result<()> {
    WorkspaceStore::new(repo_root).save_settings(local)
}

/// The effective settings for a project: the global settings with the
/// repository's local overrides applied on top.
pub fn effective(storage: &Storage, repo_root: &Path) -> Result<Settings> {
    let global = storage.load_settings()?;
    let local = load_local(repo_root)?;
    Ok(global.with_local(&local))
}

/// Override the agent CLI for a single project, or clear the override with
/// `None`. Returns the updated local settings.
pub fn set_local_agent_cli(repo_root: &Path, agent_cli: Option<AgentCli>) -> Result<LocalSettings> {
    let mut local = load_local(repo_root)?;
    local.agent_cli = agent_cli;
    save_local(repo_root, &local)?;
    Ok(local)
}

/// Override desktop notifications for a single project, or clear the override
/// with `None`. Returns the updated local settings.
pub fn set_local_notifications_enabled(
    repo_root: &Path,
    enabled: Option<bool>,
) -> Result<LocalSettings> {
    let mut local = load_local(repo_root)?;
    local.notifications_enabled = enabled;
    save_local(repo_root, &local)?;
    Ok(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_overrides_round_trip_and_resolve_against_global() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let storage = Storage::new(tmp.path().join("global"));
        // Global baseline: claude + notifications on.
        storage.save_settings(&Settings::default()).unwrap();

        // No local file yet: effective == global, local is empty.
        assert!(load_local(repo).unwrap().is_empty());
        let effective_default = effective(&storage, repo).unwrap();
        assert_eq!(effective_default.agent_cli, AgentCli::Claude);
        assert!(effective_default.notifications_enabled);

        // Override the agent CLI for this project only.
        let local = set_local_agent_cli(repo, Some(AgentCli::Gemini)).unwrap();
        assert_eq!(local.agent_cli, Some(AgentCli::Gemini));
        assert_eq!(local.notifications_enabled, None);

        // ...and the notification toggle.
        set_local_notifications_enabled(repo, Some(false)).unwrap();

        // Effective settings reflect both overrides; global is untouched.
        let resolved = effective(&storage, repo).unwrap();
        assert_eq!(resolved.agent_cli, AgentCli::Gemini);
        assert!(!resolved.notifications_enabled);
        assert_eq!(storage.load_settings().unwrap(), Settings::default());

        // Clearing an override falls back to the global value again.
        set_local_agent_cli(repo, None).unwrap();
        assert_eq!(
            effective(&storage, repo).unwrap().agent_cli,
            AgentCli::Claude
        );
    }

    #[test]
    fn save_local_persists_to_the_repo_usagi_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        save_local(
            repo,
            &LocalSettings {
                agent_cli: Some(AgentCli::Gemini),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(repo.join(".usagi/settings.json").is_file());
    }
}
