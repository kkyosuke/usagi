//! `usagi config`: show or edit usagi's global configuration.
//!
//! usagi's configuration file is the global `settings.json` (see
//! [`crate::infrastructure::storage`]). `usagi config` prints the current
//! settings; `usagi config --edit` opens the file in `$EDITOR`, then validates
//! the result on save, reverting to the previous version if the edit produced
//! invalid configuration.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use shell_words;

use crate::domain::settings::{AgentCli, Settings, Theme};
use crate::infrastructure::storage::Storage;

/// Entry point for `usagi config`.
///
/// With `edit`, opens the settings file in the user's editor and validates the
/// result; otherwise prints the current settings.
pub fn run(edit: bool) -> Result<()> {
    let storage = Storage::open_default()?;
    let settings = if edit {
        edit_config(&storage, &EnvEditor)?
    } else {
        storage.load_settings()?
    };
    for line in render_settings(&settings) {
        println!("{line}");
    }
    Ok(())
}

/// Opens a path in an editor; abstracted so [`edit_config`] is testable without
/// launching a real editor.
trait Editor {
    /// Open `path` for editing and return once the editor has exited.
    fn edit(&self, path: &Path) -> Result<()>;
}

/// The production [`Editor`]: launches the user's `$EDITOR`.
struct EnvEditor;

impl Editor for EnvEditor {
    fn edit(&self, path: &Path) -> Result<()> {
        let editor_args = editor_command();
        let editor_bin = &editor_args[0];
        let status = std::process::Command::new(editor_bin)
            .args(&editor_args[1..])
            .arg(path)
            .status()
            .with_context(|| format!("failed to launch editor `{editor_bin}`"))?;
        if !status.success() {
            bail!("editor `{editor_bin}` exited with an error");
        }
        Ok(())
    }
}

/// The editor command to run: `$EDITOR`, then `$VISUAL`, then a platform default.
fn editor_command() -> Vec<String> {
    if let Some(editor) = non_empty_env("EDITOR") {
        if let Ok(args) = shell_words::split(&editor) {
            if !args.is_empty() {
                return args;
            }
        }
    }

    if let Some(visual) = non_empty_env("VISUAL") {
        if let Ok(args) = shell_words::split(&visual) {
            if !args.is_empty() {
                return args;
            }
        }
    }

    vec![default_editor(std::env::consts::OS).to_string()]
}

/// Read an environment variable, treating an unset or empty value as absent.
fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The fallback editor when neither `$EDITOR` nor `$VISUAL` is set.
fn default_editor(os: &str) -> &'static str {
    match os {
        "windows" => "notepad",
        _ => "vi",
    }
}

/// Open the settings file in `editor`, then validate the result.
///
/// The current settings are materialized first so the editor opens populated,
/// normalized content. If the edited file no longer parses into valid
/// [`Settings`] (bad JSON, missing required fields, or a wrong type), the
/// previous file is restored and the parse error is surfaced.
fn edit_config(storage: &Storage, editor: &dyn Editor) -> Result<Settings> {
    // Materialize the current settings so the file exists and is normalized.
    let current = storage.load_settings()?;
    storage.save_settings(&current)?;

    let path = storage.settings_path();
    let backup = fs::read_to_string(&path)?;

    editor.edit(&path)?;

    match storage.load_settings() {
        Ok(settings) => Ok(settings),
        Err(error) => {
            // Roll back to the last valid configuration so usagi stays usable.
            fs::write(&path, &backup)?;
            Err(error)
                .context("the edited configuration was invalid; reverted to the previous version")
        }
    }
}

/// Render settings as aligned `key  value` lines for display.
fn render_settings(settings: &Settings) -> Vec<String> {
    let workspace_root = settings
        .workspace_root
        .as_ref()
        .map(|path| path.display().to_string());
    vec![
        format!("theme                  {}", theme_label(settings.theme)),
        format!(
            "default_workspace      {}",
            settings.default_workspace.as_deref().unwrap_or("(none)")
        ),
        format!(
            "workspace_root         {}",
            workspace_root.as_deref().unwrap_or("(none)")
        ),
        format!("notifications_enabled  {}", settings.notifications_enabled),
        format!("agent_cli              {}", agent_label(settings.agent_cli)),
        format!("local_llm_enabled      {}", settings.local_llm.enabled),
        format!("local_llm_model        {}", settings.local_llm.model),
    ]
}

/// The on-disk label for a [`Theme`].
fn theme_label(theme: Theme) -> &'static str {
    match theme {
        Theme::Light => "light",
        Theme::Dark => "dark",
        Theme::System => "system",
    }
}

/// The on-disk label for an [`AgentCli`].
fn agent_label(agent: AgentCli) -> &'static str {
    match agent {
        AgentCli::Claude => "claude",
        AgentCli::Gemini => "gemini",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An [`Editor`] that overwrites the file with fixed `content` (simulating
    /// the user editing and saving), or leaves it untouched when `None`.
    struct FakeEditor {
        content: Option<&'static str>,
    }

    impl Editor for FakeEditor {
        fn edit(&self, path: &Path) -> Result<()> {
            if let Some(content) = self.content {
                fs::write(path, content)?;
            }
            Ok(())
        }
    }

    fn temp_storage() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path().join("usagi"));
        (dir, storage)
    }

    #[test]
    fn render_settings_lists_every_field() {
        let settings = Settings {
            theme: Theme::Dark,
            default_workspace: Some("usagi".to_string()),
            workspace_root: Some("/home/me/git".into()),
            notifications_enabled: false,
            agent_cli: AgentCli::Gemini,
            local_llm: crate::domain::settings::LocalLlm {
                enabled: true,
                model: "qwen2.5-coder:3b".to_string(),
            },
        };
        let lines = render_settings(&settings);
        assert!(lines[0].contains("dark"));
        assert!(lines[1].contains("usagi"));
        assert!(lines[2].contains("/home/me/git"));
        assert!(lines[3].contains("false"));
        assert!(lines[4].contains("gemini"));
        assert!(lines[5].contains("true"));
        assert!(lines[6].contains("qwen2.5-coder:3b"));
    }

    #[test]
    fn render_settings_shows_none_for_unset_optionals() {
        let lines = render_settings(&Settings::default());
        assert!(lines[1].contains("(none)"));
        assert!(lines[2].contains("(none)"));
    }

    #[test]
    fn theme_and_agent_labels_cover_every_variant() {
        assert_eq!(theme_label(Theme::Light), "light");
        assert_eq!(theme_label(Theme::Dark), "dark");
        assert_eq!(theme_label(Theme::System), "system");
        assert_eq!(agent_label(AgentCli::Claude), "claude");
        assert_eq!(agent_label(AgentCli::Gemini), "gemini");
    }

    #[test]
    fn edit_config_saves_valid_edits() {
        let (_dir, storage) = temp_storage();
        // The editor rewrites the file with a different (valid) theme.
        let editor = FakeEditor {
            content: Some("{\n  \"version\": 1,\n  \"theme\": \"dark\"\n}\n"),
        };
        let settings = edit_config(&storage, &editor).unwrap();
        assert_eq!(settings.theme, Theme::Dark);
        // The change was persisted.
        assert_eq!(storage.load_settings().unwrap().theme, Theme::Dark);
    }

    #[test]
    fn edit_config_keeps_current_settings_when_unchanged() {
        let (_dir, storage) = temp_storage();
        storage
            .save_settings(&Settings {
                agent_cli: AgentCli::Gemini,
                ..Default::default()
            })
            .unwrap();
        // The editor exits without touching the file.
        let settings = edit_config(&storage, &FakeEditor { content: None }).unwrap();
        assert_eq!(settings.agent_cli, AgentCli::Gemini);
    }

    #[test]
    fn edit_config_reverts_invalid_edits() {
        let (_dir, storage) = temp_storage();
        storage
            .save_settings(&Settings {
                theme: Theme::Dark,
                ..Default::default()
            })
            .unwrap();
        // The editor saves malformed JSON.
        let editor = FakeEditor {
            content: Some("{ not valid json"),
        };
        let error = edit_config(&storage, &editor).unwrap_err();
        assert!(error.to_string().contains("invalid"));
        // The previous valid configuration was restored.
        assert_eq!(storage.load_settings().unwrap().theme, Theme::Dark);
    }

    #[test]
    fn editor_command_prefers_editor_then_visual_then_default() {
        let _guard = crate::test_support::process_env_guard();

        std::env::set_var("EDITOR", "my-editor");
        std::env::set_var("VISUAL", "my-visual");
        assert_eq!(editor_command(), vec!["my-editor".to_string()]);

        // An empty EDITOR falls through to VISUAL.
        std::env::set_var("EDITOR", "");
        assert_eq!(editor_command(), vec!["my-visual".to_string()]);

        // Neither set: the platform default.
        std::env::remove_var("EDITOR");
        std::env::remove_var("VISUAL");
        assert_eq!(
            editor_command(),
            vec![default_editor(std::env::consts::OS).to_string()]
        );
    }

    #[test]
    fn editor_command_parses_complex_string() {
        let _guard = crate::test_support::process_env_guard();

        std::env::set_var("EDITOR", "vim -p");
        assert_eq!(
            editor_command(),
            vec!["vim".to_string(), "-p".to_string()]
        );

        std::env::set_var("EDITOR", "code --wait --new-window");
        assert_eq!(
            editor_command(),
            vec!["code".to_string(), "--wait".to_string(), "--new-window".to_string()]
        );
    }

    #[test]
    fn default_editor_is_platform_specific() {
        assert_eq!(default_editor("windows"), "notepad");
        assert_eq!(default_editor("macos"), "vi");
        assert_eq!(default_editor("linux"), "vi");
    }

    #[test]
    fn env_editor_runs_the_configured_command() {
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "{}").unwrap();

        // `true` exits 0 without touching the file.
        std::env::set_var("EDITOR", "true");
        assert!(EnvEditor.edit(&path).is_ok());

        // `false` exits non-zero.
        std::env::set_var("EDITOR", "false");
        let err = EnvEditor.edit(&path).unwrap_err();
        assert!(err.to_string().contains("exited with an error"));

        // A missing binary fails to launch.
        std::env::set_var("EDITOR", "definitely-not-a-real-editor-xyz");
        let err = EnvEditor.edit(&path).unwrap_err();
        assert!(err.to_string().contains("failed to launch"));
    }

    #[test]
    fn run_prints_current_settings() {
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, dir.path());
        let result = run(false);
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
        assert!(result.is_ok());
    }

    #[test]
    fn run_with_edit_launches_the_editor() {
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, dir.path());
        // `true` stands in for an editor that exits without changes.
        std::env::set_var("EDITOR", "true");
        let result = run(true);
        std::env::remove_var("EDITOR");
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
        assert!(result.is_ok());
    }
}
