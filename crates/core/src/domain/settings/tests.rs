use super::{ModalSelectionMode, Settings, Theme};

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
}

#[test]
fn settings_round_trip_through_json() {
    let settings = Settings {
        theme: Theme::Dark,
        modal_selection_mode: ModalSelectionMode::Prompt,
    };
    let json = serde_json::to_string(&settings).unwrap();
    assert!(json.contains("\"theme\":\"dark\""));
    assert!(json.contains("\"modal_selection_mode\":\"prompt\""));
    let back: Settings = serde_json::from_str(&json).unwrap();
    assert_eq!(back, settings);
    // Exercise the derived Clone / Debug.
    assert_eq!(settings.clone(), settings);
    assert!(format!("{settings:?}").contains("Dark"));
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
