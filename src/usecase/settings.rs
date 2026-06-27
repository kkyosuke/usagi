use std::path::Path;

use anyhow::Result;

use crate::domain::settings::{AgentCli, LocalSettings, Settings, Theme};
use crate::infrastructure::storage::Storage;
use crate::infrastructure::workspace_store::WorkspaceStore;

/// Load the current settings (defaults if none have been saved yet).
pub fn load(storage: &Storage) -> Result<Settings> {
    storage.load_settings()
}

/// Persist the given settings as-is, serialised against concurrent writers.
///
/// `settings.json` is shared by several usagi processes (every TUI instance plus
/// each session's `usagi mcp` server). The store lock is held across this
/// one-shot write so it cannot land between a concurrent [`update_settings`]'s
/// load and save and silently drop that writer's change (a lost update).
pub fn save(storage: &Storage, settings: &Settings) -> Result<()> {
    let _lock = storage.lock()?;
    storage.save_settings(settings)
}

/// Load the global settings, apply `edit`, persist the result, and return it.
///
/// The single load→edit→save→return shape every global setter shares, so each
/// setter is one line naming the field it touches. The store lock is held across
/// the whole load→edit→save so a concurrent writer cannot read the same snapshot
/// and overwrite this change — a lost update (see [`Storage::lock`]).
fn update_settings(storage: &Storage, edit: impl FnOnce(&mut Settings)) -> Result<Settings> {
    let _lock = storage.lock()?;
    let mut settings = storage.load_settings()?;
    edit(&mut settings);
    storage.save_settings(&settings)?;
    Ok(settings)
}

/// Load the project-local overrides for `repo_root`, apply `edit`, persist the
/// result, and return it — the local counterpart to [`update_settings`], holding
/// the project store lock across the whole sequence for the same reason.
fn update_local(repo_root: &Path, edit: impl FnOnce(&mut LocalSettings)) -> Result<LocalSettings> {
    let store = WorkspaceStore::new(repo_root);
    let _lock = store.lock()?;
    let mut local = store.load_settings()?;
    edit(&mut local);
    store.save_settings(&local)?;
    Ok(local)
}

/// Change the UI theme and persist it.
pub fn set_theme(storage: &Storage, theme: Theme) -> Result<Settings> {
    update_settings(storage, |s| s.theme = theme)
}

/// Set or clear the default workspace and persist it.
pub fn set_default_workspace(storage: &Storage, name: Option<String>) -> Result<Settings> {
    update_settings(storage, |s| s.default_workspace = name)
}

/// Enable or disable desktop notifications and persist the choice.
pub fn set_notifications_enabled(storage: &Storage, enabled: bool) -> Result<Settings> {
    update_settings(storage, |s| s.notifications_enabled = enabled)
}

/// Change which agent CLI usagi drives and persist it.
pub fn set_agent_cli(storage: &Storage, agent_cli: AgentCli) -> Result<Settings> {
    update_settings(storage, |s| s.agent_cli = agent_cli)
}

/// Load the project-local setting overrides for the repository at `repo_root`
/// (all fields unset if none have been saved).
pub fn load_local(repo_root: &Path) -> Result<LocalSettings> {
    WorkspaceStore::new(repo_root).load_settings()
}

/// Persist the project-local setting overrides for the repository at `repo_root`,
/// serialised against concurrent writers (see [`save`] for why).
pub fn save_local(repo_root: &Path, local: &LocalSettings) -> Result<()> {
    let store = WorkspaceStore::new(repo_root);
    let _lock = store.lock()?;
    store.save_settings(local)
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
    update_local(repo_root, |l| l.agent_cli = agent_cli)
}

/// Override desktop notifications for a single project, or clear the override
/// with `None`. Returns the updated local settings.
pub fn set_local_notifications_enabled(
    repo_root: &Path,
    enabled: Option<bool>,
) -> Result<LocalSettings> {
    update_local(repo_root, |l| l.notifications_enabled = enabled)
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
        // The save is serialised behind the project store lock.
        assert!(repo.join(".usagi/.lock").is_file());
    }

    #[test]
    fn save_and_load_round_trip_under_the_store_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("global"));
        let settings = Settings {
            theme: Theme::Dark,
            ..Default::default()
        };
        save(&storage, &settings).unwrap();
        assert_eq!(load(&storage).unwrap(), settings);
        // The one-shot save holds the store lock so it cannot interleave with a
        // concurrent setter's load→edit→save (see Storage::lock).
        assert!(storage.dir().join(".lock").is_file());
    }

    #[test]
    fn global_setters_persist_holding_the_store_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("global"));
        // A global setter runs load→edit→save under the lock; the change sticks
        // and the per-store lock file is present.
        let updated = set_theme(&storage, Theme::Dark).unwrap();
        assert_eq!(updated.theme, Theme::Dark);
        assert_eq!(load(&storage).unwrap().theme, Theme::Dark);
        assert!(storage.dir().join(".lock").is_file());
    }
}
