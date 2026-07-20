use super::{DefaultModel, LocalSettings, ModalSelectionMode, Settings, Theme};

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
    assert_eq!(Settings::default().theme, Theme::System);
    assert_eq!(
        Settings::default().modal_selection_mode,
        ModalSelectionMode::Action
    );
    assert_eq!(Settings::default().default_model, DefaultModel::OpenAi);
}

#[test]
fn settings_round_trip_through_json() {
    let settings = Settings {
        theme: Theme::Dark,
        modal_selection_mode: ModalSelectionMode::Prompt,
        default_model: DefaultModel::Claude,
    };
    let json = serde_json::to_string(&settings).unwrap();
    assert!(json.contains("\"theme\":\"dark\""));
    assert!(json.contains("\"modal_selection_mode\":\"prompt\""));
    assert!(json.contains("\"default_model\":\"claude\""));
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
fn local_settings_overlay_only_explicit_fields() {
    let global = Settings {
        theme: Theme::Dark,
        modal_selection_mode: ModalSelectionMode::Action,
        default_model: DefaultModel::Claude,
    };
    let local = LocalSettings {
        modal_selection_mode: Some(ModalSelectionMode::Prompt),
        ..LocalSettings::default()
    };

    assert_eq!(
        global.with_local(&local),
        Settings {
            theme: Theme::Dark,
            modal_selection_mode: ModalSelectionMode::Prompt,
            default_model: DefaultModel::Claude,
        }
    );
}

#[test]
fn local_settings_missing_and_unknown_values_defer_to_global() {
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
fn full_settings_convert_to_explicit_local_overrides() {
    let settings = Settings {
        theme: Theme::Light,
        modal_selection_mode: ModalSelectionMode::Prompt,
        default_model: DefaultModel::Claude,
    };
    let local = LocalSettings::from(&settings);
    assert_eq!(Settings::default().with_local(&local), settings);
    assert!(format!("{local:?}").contains("Prompt"));
    assert_eq!(local.clone(), local);
}
