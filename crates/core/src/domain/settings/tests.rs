use super::{
    AvailableModels, DEFAULT_LOCAL_LLM_MODEL, DefaultModel, EnvBindings, LOCAL_LLM_MODELS,
    LocalLlm, LocalSettings, ModalSelectionMode, Settings, Theme,
};

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
    assert_eq!(settings.local_llm, LocalLlm::default());
    assert!(!settings.local_llm.enabled);
    assert_eq!(settings.local_llm.model, DEFAULT_LOCAL_LLM_MODEL);
    assert!(settings.env.is_empty());
}

#[test]
fn local_llm_model_is_sanitized_to_the_closed_vocabulary() {
    for model in LOCAL_LLM_MODELS {
        let settings = Settings {
            local_llm: LocalLlm {
                enabled: true,
                model: model.to_owned(),
            },
            ..Settings::default()
        };
        assert_eq!(settings.sanitized().local_llm.model, model);
    }

    let settings = Settings {
        local_llm: LocalLlm {
            enabled: true,
            model: "x'; touch /tmp/pwned; #\"\\\n".to_owned(),
        },
        ..Settings::default()
    }
    .sanitized();
    assert!(settings.local_llm.enabled);
    assert_eq!(settings.local_llm.model, DEFAULT_LOCAL_LLM_MODEL);
}

#[test]
fn settings_round_trip_through_json() {
    let settings = Settings {
        theme: Theme::Dark,
        modal_selection_mode: ModalSelectionMode::Prompt,
        default_model: DefaultModel::Claude,
        issue_enabled: false,
        memory_enabled: false,
        local_llm: LocalLlm {
            enabled: true,
            model: "qwen2.5-coder:3b".to_owned(),
        },
        env: bindings(&[("GH_TOKEN", "op://Private/GitHub/token")]),
    };
    let json = serde_json::to_string(&settings).unwrap();
    assert!(json.contains("\"env\":{\"GH_TOKEN\":\"op://Private/GitHub/token\"}"));
    assert!(json.contains("\"theme\":\"dark\""));
    assert!(json.contains("\"modal_selection_mode\":\"prompt\""));
    assert!(json.contains("\"default_model\":\"claude\""));
    assert!(json.contains("\"issue_enabled\":false"));
    assert!(json.contains("\"memory_enabled\":false"));
    assert!(json.contains("\"local_llm\":{\"enabled\":true,\"model\":\"qwen2.5-coder:3b\"}"));
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
    assert_eq!(DefaultModel::SakanaAi.profile_id(), "sakana-ai");
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
fn every_model_provider_maps_a_selector_profile_and_executable() {
    assert_eq!(
        DefaultModel::ALL.map(DefaultModel::selector),
        ["claude", "codex", "sakana.ai"]
    );
    assert_eq!(
        DefaultModel::ALL.map(DefaultModel::command),
        ["claude", "codex", "codex-fugu"]
    );
    // `sakana.ai` is presented under its product name but launches `codex-fugu`
    // through the `sakana-ai` profile.
    assert_eq!(DefaultModel::SakanaAi.command(), "codex-fugu");
    assert_eq!(DefaultModel::SakanaAi.selector(), "sakana.ai");
}

#[test]
fn selector_resolution_accepts_every_spelling_of_a_provider() {
    for (token, expected) in [
        ("claude", DefaultModel::Claude),
        ("Claude", DefaultModel::Claude),
        ("codex", DefaultModel::OpenAi),
        ("sakana.ai", DefaultModel::SakanaAi),
        ("sakana_ai", DefaultModel::SakanaAi),
        ("sakana-ai", DefaultModel::SakanaAi),
        ("codex-fugu", DefaultModel::SakanaAi),
        ("codex_fugu", DefaultModel::SakanaAi),
        ("  SAKANA.AI  ", DefaultModel::SakanaAi),
    ] {
        assert_eq!(
            DefaultModel::from_selector(token),
            Some(expected),
            "{token}"
        );
    }
    // `openai` is the persisted token, not a launchable CLI name, so the typed
    // vocabulary rejects it alongside unknown and empty input.
    for token in ["openai", "gemini", "", "   "] {
        assert_eq!(DefaultModel::from_selector(token), None, "{token}");
    }
}

#[test]
fn sakana_ai_persists_as_snake_case_and_reads_legacy_tokens() {
    assert_eq!(
        serde_json::to_value(DefaultModel::SakanaAi).unwrap(),
        "sakana_ai"
    );
    for token in ["sakana_ai", "sakana.ai", "codex_fugu"] {
        assert_eq!(
            serde_json::from_str::<DefaultModel>(&format!("\"{token}\"")).unwrap(),
            DefaultModel::SakanaAi,
            "{token}"
        );
    }
    let local: LocalSettings = serde_json::from_str(r#"{"default_model":"sakana.ai"}"#).unwrap();
    assert_eq!(local.default_model, Some(DefaultModel::SakanaAi));
}

#[test]
fn availability_offers_only_installed_providers() {
    let all = AvailableModels::all();
    assert!(!all.is_empty());
    assert_eq!(all.iter().collect::<Vec<_>>(), DefaultModel::ALL);
    assert!(
        DefaultModel::ALL
            .into_iter()
            .all(|model| all.contains(model))
    );

    let none = AvailableModels::default();
    assert!(none.is_empty());
    assert_eq!(none.first(), None);
    assert_eq!(none.next(DefaultModel::Claude), None);
    assert!(none.iter().next().is_none());

    // `codex` remains the first offer whenever it is installed, matching the
    // stored default; otherwise the first installed provider is offered.
    assert_eq!(all.first(), Some(DefaultModel::OpenAi));
    assert_eq!(
        AvailableModels::new([DefaultModel::Claude, DefaultModel::SakanaAi]).first(),
        Some(DefaultModel::Claude)
    );
    assert_eq!(
        AvailableModels::new([DefaultModel::SakanaAi]).first(),
        Some(DefaultModel::SakanaAi)
    );

    // `next` wraps through the installed providers and recovers from a stored
    // choice that is no longer installed.
    assert_eq!(all.next(DefaultModel::Claude), Some(DefaultModel::OpenAi));
    assert_eq!(all.next(DefaultModel::OpenAi), Some(DefaultModel::SakanaAi));
    assert_eq!(all.next(DefaultModel::SakanaAi), Some(DefaultModel::Claude));
    let only_sakana = AvailableModels::new([DefaultModel::SakanaAi]);
    assert_eq!(
        only_sakana.next(DefaultModel::Claude),
        Some(DefaultModel::SakanaAi)
    );
    assert_eq!(
        only_sakana.next(DefaultModel::SakanaAi),
        Some(DefaultModel::SakanaAi)
    );
    // Exercise the derived vocabulary the Config screen and Closeup share.
    assert_eq!(all.clone(), all);
    assert!(format!("{all:?}").contains("claude"));
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
        local_llm: LocalLlm::default(),
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
            local_llm: LocalLlm::default(),
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
fn a_global_config_save_keeps_fields_owned_by_other_settings_surfaces() {
    let latest = Settings {
        theme: Theme::Light,
        default_model: DefaultModel::SakanaAi,
        local_llm: LocalLlm {
            enabled: true,
            model: "qwen2.5-coder:3b".to_owned(),
        },
        env: bindings(&[("GH_TOKEN", "op://Private/GitHub/token")]),
        ..Settings::default()
    };
    let config_draft = Settings {
        theme: Theme::Dark,
        modal_selection_mode: ModalSelectionMode::Prompt,
        default_model: DefaultModel::Claude,
        issue_enabled: false,
        memory_enabled: false,
        ..Settings::default()
    };

    let saved = latest.clone().with_config(&config_draft);
    assert_eq!(saved.theme, Theme::Dark);
    assert_eq!(saved.modal_selection_mode, ModalSelectionMode::Prompt);
    assert_eq!(saved.default_model, DefaultModel::Claude);
    assert!(!saved.issue_enabled);
    assert!(!saved.memory_enabled);
    assert_eq!(saved.local_llm, latest.local_llm);
    assert_eq!(saved.env, latest.env);
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
        local_llm: LocalLlm {
            enabled: true,
            model: "qwen2.5-coder:3b".to_owned(),
        },
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
            local_llm: LocalLlm::default(),
            env: EnvBindings::new(),
        }
    );
    let json = serde_json::to_string(&local).unwrap();
    assert!(!json.contains("theme"));
    assert!(!json.contains("modal_selection_mode"));
    assert!(format!("{local:?}").contains("Claude"));
    assert_eq!(local.clone(), local);
}
