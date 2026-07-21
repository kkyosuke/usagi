use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde::{de::DeserializeOwned, Serialize};

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

/// Enable or disable the sidebar mascot's reactions and persist the choice.
pub fn set_mascot_animation_enabled(storage: &Storage, enabled: bool) -> Result<Settings> {
    update_settings(storage, |s| s.mascot_animation_enabled = enabled)
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

/// Persist a Config-screen draft with optimistic, field-level reconciliation.
///
/// `baseline` is the content loaded when the editor opened. Under the store
/// lock, each changed field or map entry is applied to the latest settings.
/// Fields changed only by another writer are preserved; a field
/// changed by both writers to different values aborts the write with a conflict.
pub fn save_revisioned(
    storage: &Storage,
    baseline: &Settings,
    draft: &Settings,
) -> Result<Settings> {
    let _lock = storage.lock()?;
    let current = storage.load_settings()?;
    let merged = merge_fields(baseline, draft, &current)?;
    storage.save_settings(&merged)?;
    Ok(merged)
}

/// Local-settings counterpart to [`save_revisioned`].
pub fn save_local_revisioned(
    repo_root: &Path,
    baseline: &LocalSettings,
    draft: &LocalSettings,
) -> Result<LocalSettings> {
    let store = WorkspaceStore::new(repo_root);
    let _lock = store.lock()?;
    let current = store.load_settings()?;
    let merged = merge_fields(baseline, draft, &current)?;
    store.save_settings(&merged)?;
    Ok(merged)
}

/// Atomically patch only workspace environment bindings from an editor draft.
/// Other local fields always come from the latest file. A concurrent env edit
/// conflicts, while an identical concurrent result is accepted idempotently.
pub fn save_local_env_revisioned(
    repo_root: &Path,
    baseline: &crate::domain::settings::SecretEnv,
    draft: &crate::domain::settings::SecretEnv,
) -> Result<LocalSettings> {
    let store = WorkspaceStore::new(repo_root);
    let _lock = store.lock()?;
    let mut current = store.load_settings()?;
    if draft == baseline {
        return Ok(current);
    }
    if current.env != *baseline && current.env != *draft {
        return Err(conflict_error(&["env".to_string()]));
    }
    current.env = draft.clone();
    store.save_settings(&current)?;
    Ok(current)
}

/// Three-way merge over serialized fields. Object children are reconciled
/// recursively, so independent `local_llm`, skill-feature, and env-map entries
/// remain disjoint while arrays and scalar leaves conflict as one field.
fn merge_fields<T>(baseline: &T, draft: &T, current: &T) -> Result<T>
where
    T: Serialize + DeserializeOwned,
{
    let baseline = serde_json::to_value(baseline)?;
    let draft = serde_json::to_value(draft)?;
    let current = serde_json::to_value(current)?;
    if !baseline.is_object() || !draft.is_object() || !current.is_object() {
        return Err(anyhow!("settings must serialize as an object"));
    }
    let mut conflicts = Vec::new();
    let merged = merge_value(
        Some(&baseline),
        Some(&draft),
        Some(&current),
        "",
        &mut conflicts,
    )
    .expect("root settings object is never removed");
    if !conflicts.is_empty() {
        return Err(conflict_error(&conflicts));
    }
    Ok(serde_json::from_value(merged)?)
}

fn merge_value(
    baseline: Option<&serde_json::Value>,
    draft: Option<&serde_json::Value>,
    current: Option<&serde_json::Value>,
    path: &str,
    conflicts: &mut Vec<String>,
) -> Option<serde_json::Value> {
    if draft == baseline {
        return current.cloned();
    }
    if current == baseline || current == draft {
        return draft.cloned();
    }
    if let (Some(baseline), Some(draft), Some(current)) = (
        baseline.and_then(serde_json::Value::as_object),
        draft.and_then(serde_json::Value::as_object),
        current.and_then(serde_json::Value::as_object),
    ) {
        let fields: BTreeSet<_> = baseline
            .keys()
            .chain(draft.keys())
            .chain(current.keys())
            .cloned()
            .collect();
        let mut merged = serde_json::Map::new();
        for field in fields {
            let child_path = if path.is_empty() {
                field.clone()
            } else {
                format!("{path}.{field}")
            };
            if let Some(value) = merge_value(
                baseline.get(&field),
                draft.get(&field),
                current.get(&field),
                &child_path,
                conflicts,
            ) {
                merged.insert(field, value);
            }
        }
        return Some(serde_json::Value::Object(merged));
    }
    conflicts.push(path.to_string());
    current.cloned()
}

fn conflict_error(fields: &[String]) -> anyhow::Error {
    anyhow!(
        "settings conflict in {}; reload or reconcile the concurrent change, then retry",
        fields.join(", ")
    )
}

/// The effective settings for a project: the global settings with the
/// repository's local overrides applied on top.
pub fn effective(storage: &Storage, repo_root: &Path) -> Result<Settings> {
    let global = storage.load_settings()?;
    let local = load_local(repo_root)?;
    Ok(global.with_local(&local))
}

/// The effective settings for the workspace at `repo_root`, resolving the global
/// data dir itself — a convenience over [`effective`] for callers (e.g. session
/// creation) that hold only the workspace root, not a [`Storage`].
pub fn effective_for(repo_root: &Path) -> Result<Settings> {
    effective(&Storage::open_default()?, repo_root)
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
    use crate::domain::settings::SkillFeature;
    use std::sync::{Arc, Barrier};

    fn env(name: &str, value: &str) -> crate::domain::settings::SecretEnv {
        [(name.to_string(), value.to_string())]
            .into_iter()
            .collect()
    }

    fn run_after_barrier(writer: impl FnOnce() + Send + 'static) {
        let barrier = Arc::new(Barrier::new(2));
        let writer_barrier = Arc::clone(&barrier);
        let handle = std::thread::spawn(move || {
            writer_barrier.wait();
            writer();
        });
        barrier.wait();
        handle.join().unwrap();
    }

    #[test]
    fn config_and_setter_merge_disjoint_fields_and_conflict_on_same_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("global");
        let storage = Storage::new(&path);
        let baseline = load(&storage).unwrap();
        let mut draft = baseline.clone();
        draft.theme = Theme::Dark;

        let writer_path = path.clone();
        run_after_barrier(move || {
            set_notifications_enabled(&Storage::new(writer_path), false).unwrap();
        });
        let merged = save_revisioned(&storage, &baseline, &draft).unwrap();
        assert_eq!(merged.theme, Theme::Dark);
        assert!(!merged.notifications_enabled);

        let baseline = merged;
        let mut draft = baseline.clone();
        draft.theme = Theme::Light;
        let writer_path = path.clone();
        run_after_barrier(move || {
            set_theme(&Storage::new(writer_path), Theme::System).unwrap();
        });
        let error = save_revisioned(&storage, &baseline, &draft).unwrap_err();
        assert!(error.to_string().contains("settings conflict in theme"));
        assert_eq!(load(&storage).unwrap().theme, Theme::System);
    }

    #[test]
    fn env_editor_and_setter_merge_disjoint_fields_and_conflict_on_env() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let baseline = load_local(&repo).unwrap();
        let draft_env = env("EDITOR", "op://editor/value");

        let writer_repo = repo.clone();
        run_after_barrier(move || {
            set_local_agent_cli(&writer_repo, Some(AgentCli::Gemini)).unwrap();
        });
        let merged = save_local_env_revisioned(&repo, &baseline.env, &draft_env).unwrap();
        assert_eq!(merged.agent_cli, Some(AgentCli::Gemini));
        assert_eq!(merged.env, draft_env);

        let baseline_env = merged.env;
        let draft_env = env("EDITOR", "op://editor/new");
        let writer_repo = repo.clone();
        run_after_barrier(move || {
            let current = load_local(&writer_repo).unwrap();
            save_local_env_revisioned(
                &writer_repo,
                &current.env,
                &env("EDITOR", "op://setter/new"),
            )
            .unwrap();
        });
        let error = save_local_env_revisioned(&repo, &baseline_env, &draft_env).unwrap_err();
        assert!(error.to_string().contains("settings conflict in env"));
        assert_eq!(load_local(&repo).unwrap().env["EDITOR"], "op://setter/new");
    }

    #[test]
    fn config_and_env_editor_merge_disjoint_fields_and_conflict_on_env() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let baseline = load_local(&repo).unwrap();
        let mut config_draft = baseline.clone();
        config_draft.notifications_enabled = Some(false);

        let writer_repo = repo.clone();
        let writer_baseline = baseline.env.clone();
        run_after_barrier(move || {
            save_local_env_revisioned(
                &writer_repo,
                &writer_baseline,
                &env("TOKEN", "op://env/value"),
            )
            .unwrap();
        });
        let merged = save_local_revisioned(&repo, &baseline, &config_draft).unwrap();
        assert_eq!(merged.notifications_enabled, Some(false));
        assert_eq!(merged.env["TOKEN"], "op://env/value");

        let baseline = merged;
        let mut config_draft = baseline.clone();
        config_draft.env = env("TOKEN", "op://config/new");
        let writer_repo = repo.clone();
        let writer_baseline = baseline.env.clone();
        run_after_barrier(move || {
            save_local_env_revisioned(
                &writer_repo,
                &writer_baseline,
                &env("TOKEN", "op://env/new"),
            )
            .unwrap();
        });
        let error = save_local_revisioned(&repo, &baseline, &config_draft).unwrap_err();
        assert!(error.to_string().contains("settings conflict in env"));
        assert_eq!(load_local(&repo).unwrap().env["TOKEN"], "op://env/new");
    }

    #[test]
    fn failed_env_save_can_retry_with_the_same_draft() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let repo = file.path().to_path_buf();
        let baseline = crate::domain::settings::SecretEnv::new();
        let draft = env("TOKEN", "op://draft/value");

        assert!(save_local_env_revisioned(&repo, &baseline, &draft).is_err());
        assert_eq!(draft["TOKEN"], "op://draft/value");

        drop(file);
        std::fs::create_dir_all(&repo).unwrap();
        let saved = save_local_env_revisioned(&repo, &baseline, &draft).unwrap();
        assert_eq!(saved.env, draft);
    }

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
    fn effective_merges_global_and_local_env_with_local_winning() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let storage = Storage::new(tmp.path().join("global"));
        storage
            .save_settings(&Settings {
                env: [
                    ("GLOBAL_ONLY".to_string(), "op://global/only".to_string()),
                    ("SHARED".to_string(), "op://global/shared".to_string()),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            })
            .unwrap();
        save_local(
            &repo,
            &LocalSettings {
                env: [
                    ("LOCAL_ONLY".to_string(), "op://local/only".to_string()),
                    ("SHARED".to_string(), "op://local/shared".to_string()),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        )
        .unwrap();

        let resolved = effective(&storage, &repo).unwrap();

        assert_eq!(
            resolved.env().collect::<Vec<_>>(),
            vec![
                ("GLOBAL_ONLY", "op://global/only"),
                ("LOCAL_ONLY", "op://local/only"),
                ("SHARED", "op://local/shared"),
            ]
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

    #[test]
    fn effective_for_resolves_the_global_data_dir_and_applies_local_overrides() {
        let _guard = crate::test_support::process_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, &home);

        // No settings anywhere yet: the PR-skills feature follows its default (on).
        assert!(effective_for(&repo)
            .unwrap()
            .skill_feature_enabled(SkillFeature::PullRequest));

        // A project-local override turns the feature off for just this repo, and
        // `effective_for` reflects it (global ⊕ local).
        let mut local = LocalSettings::default();
        local
            .skill_features
            .insert(SkillFeature::PullRequest.id().to_string(), false);
        save_local(&repo, &local).unwrap();
        assert!(!effective_for(&repo)
            .unwrap()
            .skill_feature_enabled(SkillFeature::PullRequest));

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn mascot_animation_setter_persists_the_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("global"));
        // On by default; disabling it sticks and reloads as `false`.
        assert!(load(&storage).unwrap().mascot_animation_enabled);
        let updated = set_mascot_animation_enabled(&storage, false).unwrap();
        assert!(!updated.mascot_animation_enabled);
        assert!(!load(&storage).unwrap().mascot_animation_enabled);
    }

    #[test]
    fn other_global_setters_persist_their_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::new(tmp.path().join("global"));

        // Default workspace
        let updated = set_default_workspace(&storage, Some("ws".to_string())).unwrap();
        assert_eq!(updated.default_workspace.as_deref(), Some("ws"));
        assert_eq!(
            load(&storage).unwrap().default_workspace.as_deref(),
            Some("ws")
        );

        // Notifications
        let updated = set_notifications_enabled(&storage, false).unwrap();
        assert!(!updated.notifications_enabled);
        assert!(!load(&storage).unwrap().notifications_enabled);

        // Agent CLI
        let updated = set_agent_cli(&storage, AgentCli::Gemini).unwrap();
        assert_eq!(updated.agent_cli, AgentCli::Gemini);
        assert_eq!(load(&storage).unwrap().agent_cli, AgentCli::Gemini);
    }
}
