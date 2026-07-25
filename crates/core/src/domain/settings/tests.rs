use super::{DefaultModel, EnvBindings, LocalSettings, ModalSelectionMode, Settings, Theme};

fn bindings(pairs: &[(&str, &str)]) -> EnvBindings {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn theme_default_is_system() {
    assert_eq!(Theme::default(), Theme::System);
}

#[test]
fn theme_tokens_round_trip_snake_case() {
    for (theme, token) in [
        (Theme::Light, "light"),
        (Theme::Dark, "dark"),
        (Theme::System, "system"),
    ] {
        assert_eq!(serde_json::to_value(theme).unwrap(), token);
        assert_eq!(
            serde_json::from_str::<Theme>(&format!("\"{token}\"")).unwrap(),
            theme
        );
    }
}

#[test]
fn theme_degrades_an_unrecognised_token_to_system() {
    // A value a newer usagi might write degrades to the default rather than
    // failing the parse.
    assert_eq!(
        serde_json::from_str::<Theme>("\"midnight\"").unwrap(),
        Theme::System
    );
}

#[test]
fn settings_default_uses_the_system_theme() {
    let settings = Settings::default();
    assert_eq!(settings.theme, Theme::System);
    assert_eq!(settings.modal_selection_mode, ModalSelectionMode::Action);
    assert_eq!(settings.default_model, DefaultModel::OpenAi);
    assert!(settings.issue_enabled);
    assert!(settings.memory_enabled);
    assert!(settings.env.is_empty());
}

#[test]
fn settings_round_trip_through_json() {
    let settings = Settings {
        theme: Theme::Dark,
        modal_selection_mode: ModalSelectionMode::Prompt,
        default_model: DefaultModel::Claude,
        issue_enabled: false,
        memory_enabled: false,
        env: bindings(&[("GH_TOKEN", "op://Private/GitHub/token")]),
    };
    let json = serde_json::to_string(&settings).unwrap();
    assert!(json.contains("\"env\":{\"GH_TOKEN\":\"op://Private/GitHub/token\"}"));
    assert!(json.contains("\"theme\":\"dark\""));
    assert!(json.contains("\"modal_selection_mode\":\"prompt\""));
    assert!(json.contains("\"default_model\":\"claude\""));
    assert!(json.contains("\"issue_enabled\":false"));
    assert!(json.contains("\"memory_enabled\":false"));
    let back: Settings = serde_json::from_str(&json).unwrap();
    assert_eq!(back, settings);
    // Exercise the derived Clone / Debug.
    assert_eq!(settings.clone(), settings);
    assert!(format!("{settings:?}").contains("Dark"));
}

#[test]
fn default_model_tokens_select_the_expected_agent_profile() {
    assert_eq!(DefaultModel::Claude.profile_id(), "claude");
    assert_eq!(DefaultModel::OpenAi.profile_id(), "codex");
    assert_eq!(
        serde_json::to_value(DefaultModel::OpenAi).unwrap(),
        "openai"
    );
    assert_eq!(
        serde_json::from_str::<DefaultModel>("\"future_provider\"").unwrap(),
        DefaultModel::OpenAi
    );
}

#[test]
fn settings_tolerate_a_missing_field_and_an_unknown_theme() {
    // An empty object falls back to the default theme.
    assert_eq!(
        serde_json::from_str::<Settings>("{}").unwrap(),
        Settings::default()
    );
    // A hand-edited unknown theme degrades to System while the file still loads.
    let loaded: Settings = serde_json::from_str(r#"{"theme":"neon"}"#).unwrap();
    assert_eq!(loaded.theme, Theme::System);
}

#[test]
fn modal_selection_mode_tokens_round_trip_and_unknown_values_use_action() {
    for (mode, token) in [
        (ModalSelectionMode::Action, "action"),
        (ModalSelectionMode::Prompt, "prompt"),
    ] {
        assert_eq!(serde_json::to_value(mode).unwrap(), token);
        assert_eq!(
            serde_json::from_str::<ModalSelectionMode>(&format!("\"{token}\"")).unwrap(),
            mode
        );
    }
    assert_eq!(
        serde_json::from_str::<ModalSelectionMode>("\"future_mode\"").unwrap(),
        ModalSelectionMode::Action
    );
}

#[test]
fn local_settings_overlay_only_workspace_owned_fields() {
    let global = Settings {
        theme: Theme::Dark,
        modal_selection_mode: ModalSelectionMode::Action,
        default_model: DefaultModel::Claude,
        issue_enabled: true,
        memory_enabled: false,
        env: EnvBindings::new(),
    };
    let local = LocalSettings {
        default_model: Some(DefaultModel::OpenAi),
        issue_enabled: Some(false),
        ..LocalSettings::default()
    };

    assert_eq!(
        global.with_local(&local),
        Settings {
            theme: Theme::Dark,
            modal_selection_mode: ModalSelectionMode::Action,
            default_model: DefaultModel::OpenAi,
            issue_enabled: false,
            memory_enabled: false,
            env: EnvBindings::new(),
        }
    );
}

#[test]
fn workspace_env_adds_to_and_overrides_the_global_bindings() {
    let global = Settings {
        env: bindings(&[
            ("GH_TOKEN", "op://Private/GitHub/token"),
            ("RUST_LOG", "info"),
        ]),
        ..Settings::default()
    };
    let local = LocalSettings {
        env: bindings(&[
            ("RUST_LOG", "debug"),
            ("PROJECT", "usagi"),
            // Unusable bindings never reach the effective environment.
            ("1BAD", "value"),
            ("BLANK", "  "),
        ]),
        ..LocalSettings::default()
    };

    let effective = global.with_local(&local);
    assert_eq!(
        effective.env_bindings().collect::<Vec<_>>(),
        [
            ("GH_TOKEN", "op://Private/GitHub/token"),
            ("PROJECT", "usagi"),
            ("RUST_LOG", "debug"),
        ]
    );
    assert_eq!(
        local.env_bindings().collect::<Vec<_>>(),
        [("PROJECT", "usagi"), ("RUST_LOG", "debug")]
    );
}

#[test]
fn a_config_save_keeps_the_workspace_owned_env() {
    // The Config surface edits the merged view; saving it must not copy the
    // inherited global bindings into the workspace file.
    let local = LocalSettings {
        env: bindings(&[("PROJECT", "usagi")]),
        ..LocalSettings::default()
    };
    let merged = Settings {
        env: bindings(&[("GH_TOKEN", "op://Private/GitHub/token")]),
        default_model: DefaultModel::Claude,
        ..Settings::default()
    }
    .with_local(&local);

    let saved = local.clone().with_config(&merged);
    assert_eq!(saved.env, local.env);
    assert_eq!(saved.default_model, Some(DefaultModel::Claude));
    // A workspace initialized from global defaults starts with no bindings.
    assert!(LocalSettings::from(&merged).env.is_empty());
}

#[test]
fn local_settings_ignore_global_only_and_unknown_workspace_values() {
    let local: LocalSettings = serde_json::from_str(
        r#"{"theme":"future","modal_selection_mode":"future","default_model":"future"}"#,
    )
    .unwrap();
    assert_eq!(local, LocalSettings::default());
    assert_eq!(
        serde_json::from_str::<LocalSettings>("{}").unwrap(),
        LocalSettings::default()
    );
}

#[test]
fn full_settings_convert_to_workspace_owned_values_only() {
    let settings = Settings {
        theme: Theme::Light,
        modal_selection_mode: ModalSelectionMode::Prompt,
        default_model: DefaultModel::Claude,
        issue_enabled: false,
        memory_enabled: true,
        env: EnvBindings::new(),
    };
    let local = LocalSettings::from(&settings);
    assert_eq!(
        Settings::default().with_local(&local),
        Settings {
            theme: Theme::System,
            modal_selection_mode: ModalSelectionMode::Action,
            default_model: DefaultModel::Claude,
            issue_enabled: false,
            memory_enabled: true,
            env: EnvBindings::new(),
        }
    );
    let json = serde_json::to_string(&local).unwrap();
    assert!(!json.contains("theme"));
    assert!(!json.contains("modal_selection_mode"));
    assert!(format!("{local:?}").contains("Claude"));
    assert_eq!(local.clone(), local);
}
