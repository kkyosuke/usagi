//! `usagi config`: show or edit usagi's global configuration.
//!
//! usagi's configuration file is the global `settings.json` (see
//! [`crate::infrastructure::storage`]). `usagi config` prints the current
//! settings; `usagi config --edit` opens a private copy in `$EDITOR`, validates
//! the result, then commits it only if the shared file has not changed.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::domain::settings::{AgentCli, Settings, SkillFeature, Theme};
use crate::infrastructure::json_file;
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

/// The editor command (binary + arguments) to run: `$EDITOR`, then `$VISUAL`,
/// then a platform default.
fn editor_command() -> Vec<String> {
    editor_command_env("EDITOR")
        .or_else(|| editor_command_env("VISUAL"))
        .unwrap_or_else(|| vec![default_editor(std::env::consts::OS).to_string()])
}

/// Parse an editor environment variable into a command vector using POSIX shell
/// rules, so values like `code --wait` split into `["code", "--wait"]` without
/// spawning a shell. Returns `None` when the variable is unset/empty, fails to
/// parse (e.g. an unbalanced quote), or contains only whitespace.
fn editor_command_env(name: &str) -> Option<Vec<String>> {
    let value = non_empty_env(name)?;
    let args = shell_words::split(&value).ok()?;
    (!args.is_empty()).then_some(args)
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
/// The editor receives a mode-0600 private copy containing normalized settings;
/// the shared file is never used as editor scratch space. After the editor exits,
/// the complete candidate is parsed before the store lock is acquired. The
/// candidate is atomically saved only when the shared file's byte revision still
/// matches the revision captured before editing.
fn edit_config(storage: &Storage, editor: &dyn Editor) -> Result<Settings> {
    let (base_revision, current) = {
        let _lock = storage.lock()?;
        (
            read_revision(&storage.settings_path())?,
            storage.load_settings()?,
        )
    };

    let mut candidate_file = tempfile::Builder::new()
        .prefix("usagi-settings-")
        .suffix(".json")
        .tempfile()
        .context("failed to create a private configuration copy")?;
    candidate_file
        .write_all(json_file::serialize_versioned(&current)?.as_bytes())
        .context("failed to populate the private configuration copy")?;
    candidate_file
        .as_file_mut()
        .flush()
        .context("failed to flush the private configuration copy")?;
    let candidate_path = candidate_file.path().to_path_buf();

    editor.edit(&candidate_path)?;
    if !candidate_path.exists() {
        bail!("the editor removed the private configuration copy; live settings were not changed");
    }

    let candidate = json_file::read_versioned::<Settings>(&candidate_path)
        .context("the edited configuration was invalid; live settings were not changed")?
        .context("the edited configuration was missing; live settings were not changed")?
        .sanitized();

    let _lock = storage.lock()?;
    if read_revision(&storage.settings_path())? != base_revision {
        bail!(
            "configuration conflict: settings changed while the editor was open; live settings were not changed"
        );
    }
    storage.save_settings(&candidate)?;
    Ok(candidate)
}

/// The exact content identity used as the editor transaction's base revision.
/// Missing is distinct from an empty file so first-write races also conflict.
fn read_revision(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context(format!("failed to read {}", path.display())),
    }
}

/// Render settings as aligned `key  value` lines for display.
fn render_settings(settings: &Settings) -> Vec<String> {
    let workspace_root = settings
        .workspace_root
        .as_ref()
        .map(|path| path.display().to_string());
    let mut lines = vec![
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
        format!("restore_panes_enabled  {}", settings.restore_panes_enabled),
        format!(
            "autostart_queued       {}",
            settings.autostart_queued_prompts
        ),
        format!(
            "autostart_limit        {}",
            settings.autostart_queued_prompt_limit
        ),
        format!(
            "auto_reclaim_merged    {}",
            settings
                .auto_reclaim_merged_sessions
                .map(|minutes| format!("{minutes} min"))
                .unwrap_or_else(|| "off".to_string())
        ),
        format!("agent_cli              {}", agent_label(settings.agent_cli)),
        format!(
            "session_action_ui      {}",
            session_action_ui_label(settings.session_action_ui)
        ),
        format!("sidebar                {}", sidebar_label(settings.sidebar)),
        format!(
            "key_scheme             {}",
            key_scheme_label(settings.key_scheme)
        ),
        format!(
            "mascot_animation       {}",
            settings.mascot_animation_enabled
        ),
        format!(
            "terminal_scrollback    {}",
            settings.terminal_scrollback_lines
        ),
        format!("local_llm_enabled      {}", settings.local_llm.enabled),
        format!("local_llm_model        {}", settings.local_llm.model),
        format!("env                   {} vars", settings.env().count()),
        "workspace_env         (workspace override/addition)".to_string(),
    ];
    // One line per toggleable shipped-skill feature, keyed by its stable id.
    for feature in SkillFeature::ALL {
        lines.push(format!(
            "skill:{:<16}{}",
            feature.id(),
            settings.skill_feature_enabled(feature)
        ));
    }
    lines
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
    agent.config_label()
}

/// The on-disk label for a [`SessionActionUi`].
fn session_action_ui_label(ui: crate::domain::settings::SessionActionUi) -> &'static str {
    use crate::domain::settings::SessionActionUi;
    match ui {
        SessionActionUi::Menu => "menu",
        SessionActionUi::Prompt => "prompt",
    }
}

/// The on-disk label for a [`Sidebar`].
fn sidebar_label(sidebar: crate::domain::settings::Sidebar) -> &'static str {
    use crate::domain::settings::Sidebar;
    match sidebar {
        Sidebar::Full => "full",
        Sidebar::Rail => "rail",
    }
}

/// The on-disk label for a [`KeyScheme`].
fn key_scheme_label(scheme: crate::domain::settings::KeyScheme) -> &'static str {
    use crate::domain::settings::KeyScheme;
    match scheme {
        KeyScheme::Prefix => "prefix",
        KeyScheme::Alt => "alt",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

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

    struct BarrierEditor {
        content: &'static str,
        opened: Arc<Barrier>,
        resume: Arc<Barrier>,
    }

    impl Editor for BarrierEditor {
        fn edit(&self, path: &Path) -> Result<()> {
            fs::write(path, self.content)?;
            self.opened.wait();
            self.resume.wait();
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
            restore_panes_enabled: false,
            autostart_queued_prompts: false,
            autostart_queued_prompt_limit: 2,
            auto_reclaim_merged_sessions: Some(30),
            agent_cli: AgentCli::Gemini,
            session_action_ui: crate::domain::settings::SessionActionUi::Prompt,
            sidebar: crate::domain::settings::Sidebar::Rail,
            key_scheme: crate::domain::settings::KeyScheme::Alt,
            mascot_animation_enabled: false,
            terminal_scrollback_lines: 1_234,
            local_llm: crate::domain::settings::LocalLlm {
                enabled: true,
                model: "qwen2.5-coder:3b".to_string(),
            },
            env: [(
                "GH_TOKEN".to_string(),
                "op://Private/GitHub/token".to_string(),
            )]
            .into_iter()
            .collect(),
            // The PR-skills feature pinned off, to exercise the skill line.
            skill_features: [("pull-request".to_string(), false)].into_iter().collect(),
            session_labels: crate::domain::settings::SessionLabelMaster::default(),
        };
        let lines = render_settings(&settings);
        assert!(lines[0].contains("dark"));
        assert!(lines[1].contains("usagi"));
        assert!(lines[2].contains("/home/me/git"));
        assert!(lines[3].contains("false")); // notifications_enabled
        assert!(lines[4].contains("false")); // restore_panes_enabled
        assert!(lines[5].contains("false")); // autostart_queued_prompts
        assert!(lines[6].contains("2")); // autostart_queued_prompt_limit
        assert!(lines[7].contains("30 min")); // auto_reclaim_merged_sessions
        assert!(lines[8].contains("gemini"));
        assert!(lines[9].contains("prompt"));
        assert!(lines[10].contains("rail"));
        assert!(lines[11].contains("alt")); // key_scheme
        assert!(lines[12].contains("false")); // mascot_animation_enabled
        assert!(lines[13].contains("1234")); // terminal_scrollback
        assert!(lines[14].contains("true"));
        assert!(lines[15].contains("qwen2.5-coder:3b"));
        assert!(lines[16].contains("1 vars"));
        assert!(lines[17].contains("workspace override"));
        // The shipped-skill feature line shows its id and effective state.
        assert!(lines[18].contains("pull-request"));
        assert!(lines[18].contains("false"));
    }

    #[test]
    fn render_settings_shows_none_for_unset_optionals() {
        let lines = render_settings(&Settings::default());
        assert!(lines[1].contains("(none)"));
        assert!(lines[2].contains("(none)"));
        assert!(lines[7].contains("off"));
        assert!(lines[14].contains("false"));
        assert!(lines[16].contains("0 vars"));
        assert!(lines[17].contains("workspace override"));
    }

    #[test]
    fn theme_and_agent_labels_cover_every_variant() {
        assert_eq!(theme_label(Theme::Light), "light");
        assert_eq!(theme_label(Theme::Dark), "dark");
        assert_eq!(theme_label(Theme::System), "system");
        assert_eq!(agent_label(AgentCli::Claude), "claude");
        assert_eq!(agent_label(AgentCli::Codex), "codex");
        assert_eq!(agent_label(AgentCli::SakanaAi), "sakana_ai");
        assert_eq!(agent_label(AgentCli::Gemini), "gemini");
        assert_eq!(agent_label(AgentCli::Antigravity), "antigravity");
        assert_eq!(
            sidebar_label(crate::domain::settings::Sidebar::Full),
            "full"
        );
        assert_eq!(
            sidebar_label(crate::domain::settings::Sidebar::Rail),
            "rail"
        );
        assert_eq!(
            key_scheme_label(crate::domain::settings::KeyScheme::Prefix),
            "prefix"
        );
        assert_eq!(
            key_scheme_label(crate::domain::settings::KeyScheme::Alt),
            "alt"
        );
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
    fn edit_config_uses_a_private_mode_0600_copy_without_materializing_live_settings() {
        let (_dir, storage) = temp_storage();
        let live_path = storage.settings_path();

        struct InspectingEditor {
            live_path: std::path::PathBuf,
        }
        impl Editor for InspectingEditor {
            fn edit(&self, path: &Path) -> Result<()> {
                assert_ne!(path, self.live_path);
                assert!(!self.live_path.exists());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
                }
                fs::write(path, "{\"version\":1,\"theme\":\"dark\"}")?;
                assert!(!self.live_path.exists());
                Ok(())
            }
        }

        let settings = edit_config(
            &storage,
            &InspectingEditor {
                live_path: live_path.clone(),
            },
        )
        .unwrap();

        assert_eq!(settings.theme, Theme::Dark);
        assert!(live_path.exists());
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
    fn invalid_candidate_preserves_a_concurrent_valid_update() {
        let (_dir, storage) = temp_storage();
        storage
            .save_settings(&Settings {
                theme: Theme::Dark,
                ..Default::default()
            })
            .unwrap();

        struct InvalidEditor {
            storage_dir: std::path::PathBuf,
        }
        impl Editor for InvalidEditor {
            fn edit(&self, path: &Path) -> Result<()> {
                crate::usecase::settings::set_agent_cli(
                    &Storage::new(&self.storage_dir),
                    AgentCli::Gemini,
                )?;
                fs::write(path, "{ not valid json")?;
                Ok(())
            }
        }

        let error = edit_config(
            &storage,
            &InvalidEditor {
                storage_dir: storage.dir().to_path_buf(),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid"));
        let live = storage.load_settings().unwrap();
        assert_eq!(live.theme, Theme::Dark);
        assert_eq!(live.agent_cli, AgentCli::Gemini);
    }

    #[test]
    fn removed_private_copy_preserves_a_concurrent_valid_update() {
        let (_dir, storage) = temp_storage();
        storage
            .save_settings(&Settings {
                theme: Theme::Dark,
                ..Default::default()
            })
            .unwrap();

        struct DeletingEditor {
            storage_dir: std::path::PathBuf,
        }
        impl Editor for DeletingEditor {
            fn edit(&self, path: &Path) -> Result<()> {
                crate::usecase::settings::set_agent_cli(
                    &Storage::new(&self.storage_dir),
                    AgentCli::Gemini,
                )?;
                fs::remove_file(path)?;
                Ok(())
            }
        }

        let error = edit_config(
            &storage,
            &DeletingEditor {
                storage_dir: storage.dir().to_path_buf(),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("removed"));
        assert!(storage.settings_path().exists());
        let live = storage.load_settings().unwrap();
        assert_eq!(live.theme, Theme::Dark);
        assert_eq!(live.agent_cli, AgentCli::Gemini);
    }

    #[test]
    fn same_field_concurrent_update_is_a_conflict() {
        let (_dir, storage) = temp_storage();
        storage.save_settings(&Settings::default()).unwrap();
        let opened = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let storage_dir = storage.dir().to_path_buf();
        let edit_thread = {
            let opened = Arc::clone(&opened);
            let resume = Arc::clone(&resume);
            thread::spawn(move || {
                edit_config(
                    &Storage::new(storage_dir),
                    &BarrierEditor {
                        content: "{\"version\":1,\"theme\":\"dark\"}",
                        opened,
                        resume,
                    },
                )
            })
        };

        opened.wait();
        crate::usecase::settings::set_theme(&storage, Theme::Light).unwrap();
        resume.wait();

        let error = edit_thread.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("conflict"));
        assert_eq!(storage.load_settings().unwrap().theme, Theme::Light);
    }

    #[test]
    fn disjoint_field_concurrent_update_conflicts_and_retry_uses_the_latest_base() {
        let (_dir, storage) = temp_storage();
        storage.save_settings(&Settings::default()).unwrap();
        let opened = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let storage_dir = storage.dir().to_path_buf();
        let edit_thread = {
            let opened = Arc::clone(&opened);
            let resume = Arc::clone(&resume);
            thread::spawn(move || {
                edit_config(
                    &Storage::new(storage_dir),
                    &BarrierEditor {
                        content: "{\"version\":1,\"theme\":\"dark\"}",
                        opened,
                        resume,
                    },
                )
            })
        };

        opened.wait();
        crate::usecase::settings::set_notifications_enabled(&storage, false).unwrap();
        resume.wait();

        let error = edit_thread.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("conflict"));
        let live = storage.load_settings().unwrap();
        assert_eq!(live.theme, Theme::System);
        assert!(!live.notifications_enabled);

        let retried = edit_config(
            &storage,
            &FakeEditor {
                content: Some("{\"version\":1,\"theme\":\"dark\",\"notifications_enabled\":false}"),
            },
        )
        .unwrap();
        assert_eq!(retried.theme, Theme::Dark);
        assert!(!retried.notifications_enabled);
    }

    #[test]
    fn editor_failure_preserves_the_latest_valid_state() {
        let (_dir, storage) = temp_storage();
        storage.save_settings(&Settings::default()).unwrap();

        struct FailingEditor {
            storage_dir: std::path::PathBuf,
        }
        impl Editor for FailingEditor {
            fn edit(&self, _path: &Path) -> Result<()> {
                crate::usecase::settings::set_theme(&Storage::new(&self.storage_dir), Theme::Dark)?;
                bail!("fake editor failed")
            }
        }

        let error = edit_config(
            &storage,
            &FailingEditor {
                storage_dir: storage.dir().to_path_buf(),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("fake editor failed"));
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
        assert_eq!(editor_command(), vec!["vim".to_string(), "-p".to_string()]);

        std::env::set_var("EDITOR", "code --wait --new-window");
        assert_eq!(
            editor_command(),
            vec![
                "code".to_string(),
                "--wait".to_string(),
                "--new-window".to_string()
            ]
        );
    }

    #[test]
    fn editor_command_falls_through_unusable_values() {
        let _guard = crate::test_support::process_env_guard();

        // EDITOR fails shell parsing (unbalanced quote) → fall back to VISUAL.
        std::env::set_var("EDITOR", "vim \"x");
        std::env::set_var("VISUAL", "nano");
        assert_eq!(editor_command(), vec!["nano".to_string()]);

        // EDITOR parses to no words (whitespace only) → fall back to VISUAL.
        std::env::set_var("EDITOR", "   ");
        std::env::set_var("VISUAL", "nano");
        assert_eq!(editor_command(), vec!["nano".to_string()]);

        // VISUAL also fails shell parsing → platform default.
        std::env::set_var("EDITOR", "   ");
        std::env::set_var("VISUAL", "code \"y");
        assert_eq!(
            editor_command(),
            vec![default_editor(std::env::consts::OS).to_string()]
        );

        // VISUAL parses to no words → platform default.
        std::env::set_var("EDITOR", "code \"z");
        std::env::set_var("VISUAL", "   ");
        assert_eq!(
            editor_command(),
            vec![default_editor(std::env::consts::OS).to_string()]
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
