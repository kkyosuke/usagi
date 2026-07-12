use super::*;
use crate::domain::settings::{LabelColor, SessionLabelDef, SessionLabelMaster, LOCAL_LLM_MODELS};

fn config_with_workspaces(names: &[&str]) -> Config {
    Config::new(
        Settings::default(),
        names.iter().map(|n| n.to_string()).collect(),
    )
}

/// Move the cursor onto the given global field.
fn select_global(config: &mut Config, field: Field) {
    while config.selected_field() != Some(field) {
        config.move_down();
    }
}

#[test]
fn field_labels_are_distinct() {
    assert_eq!(Field::Theme.label(), "Theme");
    assert_eq!(Field::DefaultWorkspace.label(), "Default Workspace");
    assert_eq!(Field::Notifications.label(), "Notifications");
    assert_eq!(Field::AgentCli.label(), "Agent CLI");
    assert_eq!(Field::SessionActionUi.label(), "Session Action UI");
    assert_eq!(Field::KeyScheme.label(), "Terminal Keys");
    assert_eq!(Field::LocalLlm.label(), "Local LLM");
    assert_eq!(Field::LocalLlmModel.label(), "Local LLM Model");
    assert_eq!(Field::EnvVars.label(), "Env Vars");
    assert_eq!(Field::RestorePanes.label(), "Restore Panes");
    assert_eq!(Field::AutostartQueued.label(), "Autostart Queued Prompts");
    assert_eq!(Field::AutostartQueuedLimit.label(), "Autostart Agent Limit");
    assert_eq!(Field::MascotAnimation.label(), "Mascot Animation");
    assert_eq!(Field::ALL.len(), 13);
    assert_eq!(LocalField::AgentCli.label(), "Agent CLI");
    assert_eq!(LocalField::Notifications.label(), "Notifications");
    assert_eq!(LocalField::RestorePanes.label(), "Restore Panes");
    assert_eq!(
        LocalField::AutostartQueued.label(),
        "Autostart Queued Prompts"
    );
    assert_eq!(
        LocalField::AutostartQueuedLimit.label(),
        "Autostart Agent Limit"
    );
    assert_eq!(LocalField::DefaultBranch.label(), "Default Branch");
    assert_eq!(LocalField::BranchSource.label(), "Branch Source");
    assert_eq!(LocalField::SetupCommands.label(), "Setup Commands");
    assert_eq!(LocalField::EnvVars.label(), "Env Vars");
    assert_eq!(LocalField::SessionLabels.label(), "Session Labels");
    assert_eq!(LocalField::ALL.len(), 10);
}

#[test]
fn new_config_starts_at_the_top() {
    let config = config_with_workspaces(&["alpha"]);
    assert_eq!(config.selected_index(), 0);
    assert_eq!(config.selected_field(), Some(Field::Theme));
    assert!(!config.is_save_selected());
    assert_eq!(config.workspaces(), ["alpha"]);
    assert_eq!(*config.settings(), Settings::default());
    // A freshly loaded screen has nothing to save, and no local context.
    assert!(!config.is_dirty());
    assert!(config.local().is_none());
    assert!(config.selected_local_field().is_none());
    // Global scope: fixed field rows, then one shipped-skill feature row.
    assert_eq!(
        config.rows().len(),
        Field::ALL.len() + SkillFeature::ALL.len()
    );
    assert_eq!(
        config.save_index(),
        Field::ALL.len() + SkillFeature::ALL.len()
    );
}

#[test]
fn move_down_advances_through_fields_then_the_save_button_and_wraps() {
    let mut config = config_with_workspaces(&[]);
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::DefaultWorkspace));
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::Notifications));
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::RestorePanes));
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::AutostartQueued));
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::AutostartQueuedLimit));
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::AgentCli));
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::SessionActionUi));
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::KeyScheme));
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::MascotAnimation));
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::LocalLlm));
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::LocalLlmModel));
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::EnvVars));
    // The shipped-skill feature row sits below the fixed fields (not the Save
    // button yet).
    config.move_down();
    assert_eq!(config.selected_field(), None);
    assert_eq!(
        config.selected_skill_feature(),
        Some(SkillFeature::PullRequest)
    );
    assert!(!config.is_save_selected());
    // The Save button sits below the skill rows.
    config.move_down();
    assert_eq!(config.selected_field(), None);
    assert_eq!(config.selected_skill_feature(), None);
    assert!(config.is_save_selected());
    // Wraps from the Save button back to the first field.
    config.move_down();
    assert_eq!(config.selected_field(), Some(Field::Theme));
}

#[test]
fn move_up_wraps_to_the_save_button() {
    let mut config = config_with_workspaces(&[]);
    // From the top field, up wraps to the Save button at the bottom.
    config.move_up();
    assert!(config.is_save_selected());
    // Just above the Save button is the shipped-skill feature row.
    config.move_up();
    assert_eq!(
        config.selected_skill_feature(),
        Some(SkillFeature::PullRequest)
    );
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::EnvVars));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::LocalLlmModel));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::LocalLlm));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::MascotAnimation));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::KeyScheme));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::SessionActionUi));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::AgentCli));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::AutostartQueuedLimit));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::AutostartQueued));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::RestorePanes));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::Notifications));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::DefaultWorkspace));
    config.move_up();
    assert_eq!(config.selected_field(), Some(Field::Theme));
}

#[test]
fn notifications_field_toggles_and_reports_its_value() {
    let mut config = config_with_workspaces(&[]);
    config.move_down();
    config.move_down(); // select Notifications
    assert_eq!(config.selected_field(), Some(Field::Notifications));
    // On by default.
    assert_eq!(config.value_of(Field::Notifications), "On");
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::Notifications), "Off");
    assert!(!config.settings().notifications_enabled);
    // Toggling backward also just flips it back on.
    assert!(config.cycle_selected(false));
    assert_eq!(config.value_of(Field::Notifications), "On");
}

#[test]
fn restore_panes_field_toggles_and_reports_its_value() {
    let mut config = config_with_workspaces(&[]);
    while config.selected_field() != Some(Field::RestorePanes) {
        config.move_down();
    }
    // On by default.
    assert_eq!(config.value_of(Field::RestorePanes), "On");
    assert!(!config.is_changed(Field::RestorePanes));
    // Toggling flips it off (direction is irrelevant for a boolean) and marks it
    // changed from the saved baseline.
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::RestorePanes), "Off");
    assert!(!config.settings().restore_panes_enabled);
    assert!(config.is_changed(Field::RestorePanes));
    // Toggling backward flips it back on.
    assert!(config.cycle_selected(false));
    assert_eq!(config.value_of(Field::RestorePanes), "On");
}

#[test]
fn autostart_queued_field_toggles_and_reports_its_value() {
    let mut config = config_with_workspaces(&[]);
    select_global(&mut config, Field::AutostartQueued);
    // On by default.
    assert_eq!(config.value_of(Field::AutostartQueued), "On");
    assert!(!config.is_changed(Field::AutostartQueued));
    // Toggling flips it off (direction is irrelevant for a boolean) and marks it
    // changed from the saved baseline.
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AutostartQueued), "Off");
    assert!(!config.settings().autostart_queued_prompts);
    assert!(config.is_changed(Field::AutostartQueued));
    // Toggling backward flips it back on.
    assert!(config.cycle_selected(false));
    assert_eq!(config.value_of(Field::AutostartQueued), "On");
}

#[test]
fn autostart_queued_limit_field_cycles_and_reports_its_value() {
    let mut config = config_with_workspaces(&[]);
    select_global(&mut config, Field::AutostartQueuedLimit);
    assert_eq!(config.value_of(Field::AutostartQueuedLimit), "4");
    assert!(!config.is_changed(Field::AutostartQueuedLimit));

    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AutostartQueuedLimit), "6");
    assert_eq!(config.settings().autostart_queued_prompt_limit, 6);
    assert!(config.is_changed(Field::AutostartQueuedLimit));

    assert!(config.cycle_selected(false));
    assert_eq!(config.value_of(Field::AutostartQueuedLimit), "4");
    assert!(!config.is_changed(Field::AutostartQueuedLimit));
}

#[test]
fn mascot_animation_field_toggles_and_reports_its_value() {
    let mut config = config_with_workspaces(&[]);
    while config.selected_field() != Some(Field::MascotAnimation) {
        config.move_down();
    }
    // On by default, unchanged from the saved baseline.
    assert_eq!(config.value_of(Field::MascotAnimation), "On");
    assert!(!config.is_changed(Field::MascotAnimation));
    // Toggling flips it off (direction is irrelevant for a boolean) and marks it
    // changed.
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::MascotAnimation), "Off");
    assert!(!config.settings().mascot_animation_enabled);
    assert!(config.is_changed(Field::MascotAnimation));
    // Toggling backward flips it back on.
    assert!(config.cycle_selected(false));
    assert_eq!(config.value_of(Field::MascotAnimation), "On");
}

#[test]
fn agent_cli_field_cycles_through_each_cli() {
    let mut config = config_with_workspaces(&[]);
    select_global(&mut config, Field::AgentCli);
    assert_eq!(config.selected_field(), Some(Field::AgentCli));
    // Claude by default, cycling forward
    // Claude -> Codex -> sakana.ai -> Gemini -> Antigravity -> Claude.
    assert_eq!(config.value_of(Field::AgentCli), "Claude");
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "Codex");
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "sakana.ai");
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "Gemini");
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "Antigravity");
    // Wraps back to Claude.
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "Claude");
    // And cycles backward too (wrapping to the last value).
    assert!(config.cycle_selected(false));
    assert_eq!(config.value_of(Field::AgentCli), "Antigravity");
}

#[test]
fn agent_cli_field_cycles_only_through_installed_agents() {
    // Only Claude and SakanaAi are installed: the selector skips
    // the uninstalled Codex and Gemini entirely.
    let mut config = config_with_workspaces(&[]);
    config.set_available_agent_clis(vec![AgentCli::Claude, AgentCli::SakanaAi]);
    select_global(&mut config, Field::AgentCli);
    assert_eq!(config.value_of(Field::AgentCli), "Claude");
    // Claude -> sakana.ai -> Claude (Codex and Gemini are not offered).
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "sakana.ai");
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "Claude");
}

#[test]
fn agent_cli_field_keeps_the_saved_value_selectable_when_uninstalled() {
    // The configured agent (Gemini) is not installed, but only Codex is. Merely
    // opening the screen must not lose the saved value, so Gemini stays shown and
    // is offered alongside the installed Codex.
    let settings = Settings {
        agent_cli: AgentCli::Gemini,
        ..Settings::default()
    };
    let mut config = Config::new(settings, Vec::new());
    config.set_available_agent_clis(vec![AgentCli::Codex]);
    select_global(&mut config, Field::AgentCli);
    // Untouched, the saved (uninstalled) value is still displayed.
    assert_eq!(config.value_of(Field::AgentCli), "Gemini");
    // Cycling moves to the installed Codex. Once the user deliberately leaves the
    // uninstalled agent it is no longer offered, so it does not come back — only
    // installed agents remain (here just Codex).
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "Codex");
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "Codex");
}

#[test]
fn session_action_ui_field_cycles_between_menu_and_prompt() {
    let mut config = config_with_workspaces(&[]);
    // Navigate down until the Session Action UI row is selected.
    while config.selected_field() != Some(Field::SessionActionUi) {
        config.move_down();
    }
    // Menu by default.
    assert_eq!(config.value_of(Field::SessionActionUi), "Menu");
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::SessionActionUi), "Prompt");
    // Wraps back to Menu.
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::SessionActionUi), "Menu");
    // And cycles backward too.
    assert!(config.cycle_selected(false));
    assert_eq!(config.value_of(Field::SessionActionUi), "Prompt");
}

#[test]
fn key_scheme_field_cycles_between_prefix_and_alt() {
    let mut config = config_with_workspaces(&[]);
    // Navigate down until the 没入 key-scheme row is selected.
    while config.selected_field() != Some(Field::KeyScheme) {
        config.move_down();
    }
    // The Ctrl-O prefix is the default.
    assert_eq!(config.value_of(Field::KeyScheme), "Ctrl-O prefix");
    assert!(!config.is_changed(Field::KeyScheme));
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::KeyScheme), "Alt chords");
    assert!(config.is_changed(Field::KeyScheme));
    // Wraps back to the prefix.
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::KeyScheme), "Ctrl-O prefix");
    // And cycles backward too.
    assert!(config.cycle_selected(false));
    assert_eq!(config.value_of(Field::KeyScheme), "Alt chords");
}

#[test]
fn value_of_renders_theme_and_default_workspace() {
    let mut config = config_with_workspaces(&["alpha"]);
    assert_eq!(config.value_of(Field::Theme), "System");
    assert_eq!(config.value_of(Field::DefaultWorkspace), "(none)");

    config.settings.default_workspace = Some("alpha".to_string());
    assert_eq!(config.value_of(Field::DefaultWorkspace), "alpha");
}

#[test]
fn cycling_theme_forward_walks_the_order_and_wraps() {
    let mut config = config_with_workspaces(&[]);
    // The cursor starts on Theme, which defaults to System.
    assert_eq!(config.settings().theme, Theme::System);
    assert!(config.cycle_selected(true));
    assert_eq!(config.settings().theme, Theme::Light);
    assert!(config.cycle_selected(true));
    assert_eq!(config.settings().theme, Theme::Dark);
    assert!(config.cycle_selected(true));
    assert_eq!(config.settings().theme, Theme::System);
}

#[test]
fn cycling_theme_backward_walks_the_reverse_order() {
    let mut config = config_with_workspaces(&[]);
    assert_eq!(config.settings().theme, Theme::System);
    assert!(config.cycle_selected(false));
    assert_eq!(config.settings().theme, Theme::Dark);
    assert!(config.cycle_selected(false));
    assert_eq!(config.settings().theme, Theme::Light);
    assert!(config.cycle_selected(false));
    assert_eq!(config.settings().theme, Theme::System);
}

#[test]
fn cycling_default_workspace_forward_walks_none_then_each_name() {
    let mut config = config_with_workspaces(&["alpha", "beta"]);
    config.move_down(); // select Default Workspace

    assert_eq!(config.settings().default_workspace, None);
    assert!(config.cycle_selected(true));
    assert_eq!(
        config.settings().default_workspace.as_deref(),
        Some("alpha")
    );
    assert!(config.cycle_selected(true));
    assert_eq!(config.settings().default_workspace.as_deref(), Some("beta"));
    // Wraps from the last name back to None.
    assert!(config.cycle_selected(true));
    assert_eq!(config.settings().default_workspace, None);
}

#[test]
fn cycling_default_workspace_backward_wraps_to_the_last_name() {
    let mut config = config_with_workspaces(&["alpha", "beta"]);
    config.move_down(); // select Default Workspace

    assert!(config.cycle_selected(false));
    assert_eq!(config.settings().default_workspace.as_deref(), Some("beta"));
    assert!(config.cycle_selected(false));
    assert_eq!(
        config.settings().default_workspace.as_deref(),
        Some("alpha")
    );
    assert!(config.cycle_selected(false));
    assert_eq!(config.settings().default_workspace, None);
}

#[test]
fn cycling_default_workspace_is_a_noop_without_workspaces() {
    let mut config = config_with_workspaces(&[]);
    config.move_down(); // select Default Workspace
    assert!(!config.cycle_selected(true));
    assert_eq!(config.settings().default_workspace, None);
    assert!(!config.cycle_selected(false));
    assert_eq!(config.settings().default_workspace, None);
}

#[test]
fn an_unknown_current_workspace_resets_to_the_first_choice() {
    let mut config = config_with_workspaces(&["alpha", "beta"]);
    // A name that is no longer registered (e.g. removed since it was set).
    config.settings.default_workspace = Some("ghost".to_string());
    config.move_down(); // select Default Workspace

    // Treated as index 0 (None), so cycling forward lands on the first name.
    assert!(config.cycle_selected(true));
    assert_eq!(
        config.settings().default_workspace.as_deref(),
        Some("alpha")
    );
}

#[test]
fn editing_a_field_marks_it_and_the_config_dirty() {
    let mut config = config_with_workspaces(&[]);
    // Nothing is changed to start with.
    assert!(!config.is_dirty());
    assert!(Field::ALL.iter().all(|&f| !config.is_changed(f)));

    // Cycling the theme makes that field — and the config — dirty, while the
    // untouched fields stay clean.
    assert!(config.cycle_selected(true));
    assert!(config.is_dirty());
    assert!(config.is_changed(Field::Theme));
    assert!(!config.is_changed(Field::Notifications));
    assert!(!config.is_changed(Field::AgentCli));
    assert!(!config.is_changed(Field::DefaultWorkspace));
}

#[test]
fn returning_a_field_to_its_saved_value_clears_its_changed_flag() {
    let mut config = config_with_workspaces(&[]);
    config.move_down();
    config.move_down(); // Notifications
                        // Flip it off (dirty), then back on (clean again).
    assert!(config.cycle_selected(true));
    assert!(config.is_changed(Field::Notifications));
    assert!(config.cycle_selected(true));
    assert!(!config.is_changed(Field::Notifications));
    assert!(!config.is_dirty());
}

#[test]
fn mark_saved_adopts_the_edits_as_the_new_baseline() {
    let mut config = config_with_workspaces(&[]);
    assert!(config.cycle_selected(true)); // edit the theme
    assert!(config.is_dirty());
    config.mark_saved();
    // The current edits are now the saved state, so nothing is dirty.
    assert!(!config.is_dirty());
    assert!(!config.is_changed(Field::Theme));
    // A further edit becomes dirty again, relative to the new baseline.
    assert!(config.cycle_selected(true));
    assert!(config.is_dirty());
}

#[test]
fn cycling_the_save_button_is_a_noop() {
    let mut config = config_with_workspaces(&["alpha"]);
    config.move_up(); // wraps onto the Save button
    assert!(config.is_save_selected());
    assert!(!config.cycle_selected(true));
    assert!(!config.cycle_selected(false));
    // The settings are untouched by cycling the button.
    assert!(!config.is_dirty());
}

#[test]
fn rows_render_global_field_values() {
    let config = config_with_workspaces(&["alpha"]);
    let rows = config.rows();
    assert_eq!(rows.len(), Field::ALL.len() + SkillFeature::ALL.len());
    assert_eq!(rows[0].label, "Theme");
    assert_eq!(rows[0].value, "System");
    assert_eq!(rows[3].label, "Restore Panes");
    assert_eq!(rows[3].value, "On");
    assert_eq!(rows[4].label, "Autostart Queued Prompts");
    assert_eq!(rows[4].value, "On");
    assert_eq!(rows[5].label, "Autostart Agent Limit");
    assert_eq!(rows[5].value, "4");
    assert_eq!(rows[6].label, "Agent CLI");
    assert_eq!(rows[6].value, "Claude");
    assert_eq!(rows[7].label, "Session Action UI");
    assert_eq!(rows[7].value, "Menu");
    // The 没入 key scheme defaults to the Ctrl-O prefix.
    assert_eq!(rows[8].label, "Terminal Keys");
    assert_eq!(rows[8].value, "Ctrl-O prefix");
    assert_eq!(rows[9].label, "Mascot Animation");
    assert_eq!(rows[9].value, "On");
    // The runtime is not yet installed: the Local LLM row offers "Install" and
    // the model row is inert (shown as "—") until the runtime is present.
    assert_eq!(rows[10].label, "Local LLM");
    assert_eq!(rows[10].value, "Install");
    assert!(rows[10].action);
    assert_eq!(rows[11].label, "Local LLM Model");
    assert_eq!(rows[11].value, "—");
    assert!(rows[11].disabled);
    assert_eq!(rows[12].label, "Env Vars");
    assert_eq!(rows[12].value, "Edit (none)");
    assert!(rows[12].action);
    assert!(!rows[12].disabled);
    // The shipped-skill feature row follows the fixed fields: a plain on/off
    // chooser, on by default and neither an action nor disabled.
    assert_eq!(rows[13].label, "PR Skills");
    assert_eq!(rows[13].value, "On");
    assert!(!rows[13].action);
    assert!(!rows[13].disabled);
    assert!(rows.iter().all(|r| !r.changed));
}

#[test]
fn global_env_vars_row_opens_a_multiline_editor_and_applies_valid_bindings() {
    let mut config = config_with_workspaces(&[]);
    select_global(&mut config, Field::EnvVars);
    assert!(config.env_row_active());
    // The Env Vars row is an action row, so ←/→ have nothing to cycle.
    assert!(!config.cycle_selected(true));
    assert_eq!(config.value_of(Field::EnvVars), "Edit (none)");

    config.open_env_modal();
    assert_eq!(config.env_modal().unwrap().lines(), &["".to_string()]);
    for c in "GH_TOKEN=op://Private/GH/token".chars() {
        config.env_modal_insert(c);
    }
    config.apply_env_modal();

    assert!(config.env_modal().is_none());
    assert_eq!(
        config.settings().env.get("GH_TOKEN").map(String::as_str),
        Some("op://Private/GH/token")
    );
    assert_eq!(config.value_of(Field::EnvVars), "Edit (1 var)");
    assert!(config.is_dirty());
    let env_row = Field::ALL
        .iter()
        .position(|field| *field == Field::EnvVars)
        .unwrap();
    assert!(config.rows()[env_row].changed);
}

#[test]
fn global_env_editor_seeds_from_existing_bindings_and_label_counts_them() {
    let settings = Settings {
        env: [
            ("A_TOKEN".to_string(), "op://v/i/a".to_string()),
            ("B_TOKEN".to_string(), "op://v/i/b".to_string()),
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let mut config = Config::new(settings, Vec::new());
    assert_eq!(config.value_of(Field::EnvVars), "Edit (2 vars)");

    select_global(&mut config, Field::EnvVars);
    config.open_env_modal();

    assert_eq!(
        config.env_modal().unwrap().lines(),
        &[
            "A_TOKEN=op://v/i/a".to_string(),
            "B_TOKEN=op://v/i/b".to_string()
        ]
    );
}

// --- local LLM field ---------------------------------------------------

/// Move the cursor onto the Local LLM toggle row.
fn select_local_llm(config: &mut Config) {
    while config.selected_field() != Some(Field::LocalLlm) {
        config.move_down();
    }
}

/// Move the cursor onto the Local LLM Model row.
fn select_local_llm_model(config: &mut Config) {
    while config.selected_field() != Some(Field::LocalLlmModel) {
        config.move_down();
    }
}

#[test]
fn local_llm_row_shows_install_until_installed_then_a_toggle() {
    let mut config = config_with_workspaces(&[]);
    select_local_llm(&mut config);
    // The runtime is not installed yet: the row is an install action that opens
    // the modal, and the rows() flag marks it as an action button.
    assert!(!config.ollama_installed());
    assert_eq!(config.value_of(Field::LocalLlm), "Install");
    assert!(config.local_llm_needs_install());
    assert!(
        config.rows()[Field::ALL
            .iter()
            .position(|f| *f == Field::LocalLlm)
            .unwrap()]
        .action
    );
    // Cycling does nothing while uninstalled (activation opens the modal).
    assert!(!config.cycle_selected(true));

    // Once the runtime is installed it turns on and becomes an on/off toggle.
    config.mark_ollama_installed();
    assert!(config.ollama_installed());
    assert_eq!(config.value_of(Field::LocalLlm), "On");
    assert!(config.settings().local_llm.enabled);
    assert!(config.is_changed(Field::LocalLlm));
    // Now it is a value chooser, not an install action.
    assert!(!config.local_llm_needs_install());
    assert!(
        !config.rows()[Field::ALL
            .iter()
            .position(|f| *f == Field::LocalLlm)
            .unwrap()]
        .action
    );
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::LocalLlm), "Off");
}

#[test]
fn install_modal_collects_a_masked_password_and_focuses_the_model_row() {
    let mut config = config_with_workspaces(&[]);
    // The modal only opens from the uninstalled Local LLM install action.
    config.open_install_modal();
    assert!(config.install_modal().is_none());
    select_local_llm(&mut config);
    config.open_install_modal();
    let modal = config.install_modal().expect("modal opened");
    assert_eq!(modal.password(), "");
    // Empty: the masked string is just the block caret — a single (reversed) cell.
    // Strip styling so the assertion holds whether or not colours are enabled.
    let masked = |config: &Config| {
        console::strip_ansi_codes(&config.install_modal().unwrap().masked()).into_owned()
    };
    assert_eq!(console::strip_ansi_codes(&modal.masked()).into_owned(), " ");

    // Typing builds the password; it renders only as bullets, with the block
    // caret as a trailing cell while typing at the end.
    config.install_modal_push('p');
    config.install_modal_push('w');
    config.install_modal_backspace();
    config.install_modal_push('z');
    assert_eq!(config.install_modal_password().as_deref(), Some("pz"));
    assert_eq!(masked(&config), "•• ");

    // The caret can move into the middle and edit there: Home then Del removes
    // the first character, so "pz" becomes "z".
    config.install_modal_cursor_home();
    config.install_modal_delete_forward();
    assert_eq!(config.install_modal_password().as_deref(), Some("z"));
    // The caret now sits on the remaining bullet (no trailing cell).
    assert_eq!(masked(&config), "•");
    // End parks it past the bullet again; ←/→ step over the single character.
    config.install_modal_cursor_end();
    config.install_modal_cursor_left();
    config.install_modal_cursor_right();
    assert_eq!(masked(&config), "• ");

    // Finishing the install closes the modal, marks the runtime installed, and
    // drops the cursor onto the model row so a model can be chosen.
    config.mark_ollama_installed();
    config.focus_model_row();
    config.close_install_modal();
    assert!(config.install_modal().is_none());
    assert_eq!(config.selected_field(), Some(Field::LocalLlmModel));
    // Edits to a closed modal are no-ops (and yield no password).
    config.install_modal_push('x');
    config.install_modal_backspace();
    config.install_modal_delete_forward();
    config.install_modal_cursor_left();
    config.install_modal_cursor_right();
    config.install_modal_cursor_home();
    config.install_modal_cursor_end();
    assert!(config.install_modal_password().is_none());
}

#[test]
fn model_row_is_inert_until_the_runtime_is_installed() {
    let mut config = config_with_workspaces(&[]);
    select_local_llm_model(&mut config);
    // Before the runtime is present the row is disabled: it neither opens the
    // picker nor cycles a value.
    assert!(!config.model_row_active());
    config.open_model_modal();
    assert!(config.model_modal().is_none());
    assert!(!config.cycle_selected(true));

    // Once the runtime is installed the row becomes active (it opens the picker).
    config.mark_ollama_installed();
    select_local_llm_model(&mut config);
    assert!(config.model_row_active());
    // It is an action row (opens the modal), never a value chooser.
    assert!(!config.cycle_selected(true));
    let model_row = &config.rows()[Field::ALL
        .iter()
        .position(|f| *f == Field::LocalLlmModel)
        .unwrap()];
    assert!(model_row.action);
    assert!(!model_row.disabled);
}

#[test]
fn model_modal_lists_models_with_install_state_and_navigates() {
    let mut config = config_with_workspaces(&[]);
    config.mark_ollama_installed();
    config.set_installed_models(vec!["qwen2.5-coder:7b".to_string()]);
    select_local_llm_model(&mut config);
    config.open_model_modal();
    let modal = config.model_modal().expect("modal opened");

    // The rows mirror the offered models; only the pulled one is marked
    // installed, and the cursor starts on the model in use.
    let rows = modal.rows();
    assert_eq!(rows.len(), LOCAL_LLM_MODELS.len());
    assert_eq!(rows[0].model, "qwen2.5-coder:7b");
    assert!(rows[0].installed);
    assert!(rows[0].selected);
    assert!(rows[1..].iter().all(|r| !r.installed));

    // ↓ moves onto an uninstalled model; the selection reports as such.
    config.model_modal_down();
    assert_eq!(config.model_modal_selection(), Some("qwen2.5-coder:3b"));
    assert!(!config.model_modal_selection_installed());
    // ↑ wraps back to the installed model.
    config.model_modal_up();
    assert_eq!(config.model_modal_selection(), Some("qwen2.5-coder:7b"));
    assert!(config.model_modal_selection_installed());
}

#[test]
fn selecting_an_installed_model_adopts_it() {
    let mut config = config_with_workspaces(&[]);
    config.mark_ollama_installed();
    config.set_installed_models(vec![
        "qwen2.5-coder:7b".to_string(),
        "qwen2.5:7b".to_string(),
    ]);
    select_local_llm_model(&mut config);
    config.open_model_modal();
    // Walk down to the already-installed "qwen2.5:7b" (index 3) and adopt it.
    config.model_modal_down();
    config.model_modal_down();
    config.model_modal_down();
    assert_eq!(config.model_modal_selection(), Some("qwen2.5:7b"));
    assert!(config.model_modal_selection_installed());
    config.select_model("qwen2.5:7b");
    config.close_model_modal();
    assert!(config.model_modal().is_none());
    assert_eq!(config.local_llm_model(), "qwen2.5:7b");
    assert!(config.is_changed(Field::LocalLlmModel));
}

#[test]
fn pulling_an_uninstalled_model_marks_it_installed_and_adopts_it() {
    let mut config = config_with_workspaces(&[]);
    config.mark_ollama_installed();
    select_local_llm_model(&mut config);
    config.open_model_modal();
    config.model_modal_down(); // onto "qwen2.5-coder:3b" (not pulled)
    assert!(!config.model_modal_selection_installed());

    // Pulling records the model as installed and adopts it as the one in use.
    config.mark_model_installed("qwen2.5-coder:3b");
    config.close_model_modal();
    assert_eq!(config.local_llm_model(), "qwen2.5-coder:3b");
    // Reopening shows it now flagged as installed.
    select_local_llm_model(&mut config);
    config.open_model_modal();
    let rows = config.model_modal().unwrap().rows();
    assert!(rows[1].installed);
    assert_eq!(config.value_of(Field::LocalLlmModel), "qwen2.5-coder:3b");
}

#[test]
fn model_row_shows_an_undownloaded_marker_for_an_unpulled_model() {
    let mut config = config_with_workspaces(&[]);
    config.mark_ollama_installed();
    // No models pulled yet: the model in use is shown with the 未導入 marker.
    assert_eq!(
        config.value_of(Field::LocalLlmModel),
        "qwen2.5-coder:7b (未導入)"
    );
    // Once pulled the marker drops.
    config.set_installed_models(vec!["qwen2.5-coder:7b".to_string()]);
    assert_eq!(config.value_of(Field::LocalLlmModel), "qwen2.5-coder:7b");
}

// --- local overrides ---------------------------------------------------

fn local_config() -> Config {
    Config::workspace(Settings::default(), LocalSettings::default(), Vec::new())
}

fn local_config_with_branches(branches: &[&str]) -> Config {
    Config::workspace(
        Settings::default(),
        LocalSettings::default(),
        branches.iter().map(|b| b.to_string()).collect(),
    )
}

#[test]
fn local_scope_shows_only_the_local_override_rows() {
    let config = local_config();
    assert!(config.local().is_some());
    // The local scope shows the ten override rows, then the shipped-skill
    // feature row(s) — no global fixed fields.
    assert_eq!(config.field_count(), 10 + SkillFeature::ALL.len());
    assert_eq!(config.save_index(), 10 + SkillFeature::ALL.len());
    // The cursor starts on the first local field, not a global one.
    assert_eq!(config.selected_field(), None);
    assert_eq!(config.selected_local_field(), Some(LocalField::AgentCli));
    let rows = config.rows();
    assert_eq!(rows.len(), 10 + SkillFeature::ALL.len());
    assert_eq!(rows[0].label, "Agent CLI");
    assert_eq!(rows[1].label, "Notifications");
    assert_eq!(rows[2].label, "Restore Panes");
    assert_eq!(rows[3].label, "Autostart Queued Prompts");
    assert_eq!(rows[4].label, "Autostart Agent Limit");
    assert_eq!(rows[5].label, "Default Branch");
    assert_eq!(rows[6].label, "Branch Source");
    assert_eq!(rows[7].label, "Setup Commands");
    assert!(rows[7].action);
    assert_eq!(rows[7].value, "Edit (none)");
    assert_eq!(rows[8].label, "Env Vars");
    assert!(rows[8].action);
    assert_eq!(rows[8].value, "Edit (none)");
    // The session-label override is an action row too; unset it defers to the
    // effective global master (the default 5-label kanban set).
    assert_eq!(rows[9].label, "Session Labels");
    assert!(rows[9].action);
    assert_eq!(rows[9].value, "Edit (global: 5 labels)");
    // The skill feature row falls back to the global value (on) when unset.
    assert_eq!(rows[10].label, "PR Skills");
    assert!(rows[10].value.contains("Global"));
    assert!(rows[10].value.contains("On"));
    // Unset overrides display the value they fall back to.
    assert!(rows[0].value.contains("Global"));
    assert!(rows[0].value.contains("Claude"));
    assert!(rows[1].value.contains("Global"));
    assert!(rows[1].value.contains("On"));
    // The restore-panes override also falls back to the global value (On).
    assert!(rows[2].value.contains("Global"));
    assert!(rows[2].value.contains("On"));
    // The autostart-queued override also falls back to the global value (On).
    assert!(rows[3].value.contains("Global"));
    assert!(rows[3].value.contains("On"));
    // The autostart limit override falls back to the global value.
    assert!(rows[4].value.contains("Global"));
    assert!(rows[4].value.contains("4"));
    // The default branch has no global counterpart: unset means "auto".
    assert!(rows[5].value.contains("Default"));
    assert!(rows[5].value.contains("auto"));
    // The branch source likewise shows its default (Remote).
    assert!(rows[6].value.contains("Default"));
    assert!(rows[6].value.contains("Remote"));
}

#[test]
fn local_fields_are_selectable_then_the_save_button() {
    let mut config = local_config();
    // First local field is under the cursor from the start.
    assert_eq!(config.selected_field(), None);
    assert_eq!(config.selected_local_field(), Some(LocalField::AgentCli));
    config.move_down();
    assert_eq!(
        config.selected_local_field(),
        Some(LocalField::Notifications)
    );
    config.move_down();
    assert_eq!(
        config.selected_local_field(),
        Some(LocalField::RestorePanes)
    );
    config.move_down();
    assert_eq!(
        config.selected_local_field(),
        Some(LocalField::AutostartQueued)
    );
    config.move_down();
    assert_eq!(
        config.selected_local_field(),
        Some(LocalField::AutostartQueuedLimit)
    );
    config.move_down();
    assert_eq!(
        config.selected_local_field(),
        Some(LocalField::DefaultBranch)
    );
    config.move_down();
    assert_eq!(
        config.selected_local_field(),
        Some(LocalField::BranchSource)
    );
    config.move_down();
    assert_eq!(
        config.selected_local_field(),
        Some(LocalField::SetupCommands)
    );
    config.move_down();
    assert_eq!(config.selected_local_field(), Some(LocalField::EnvVars));
    config.move_down();
    assert_eq!(
        config.selected_local_field(),
        Some(LocalField::SessionLabels)
    );
    // Below the fixed local fields sits the shipped-skill feature row, then Save.
    config.move_down();
    assert_eq!(config.selected_local_field(), None);
    assert_eq!(
        config.selected_skill_feature(),
        Some(SkillFeature::PullRequest)
    );
    assert!(!config.is_save_selected());
    config.move_down();
    assert!(config.is_save_selected());
    assert!(config.selected_local_field().is_none());
    assert_eq!(config.selected_skill_feature(), None);
}

/// Move the cursor onto the given local field.
fn select_local(config: &mut Config, field: LocalField) {
    while config.selected_local_field() != Some(field) {
        config.move_down();
    }
}

#[test]
fn cycling_a_local_branch_source_override_toggles_local_and_remote() {
    let mut config = local_config();
    select_local(&mut config, LocalField::BranchSource);
    // Unset shows the default it resolves to.
    assert_eq!(
        config.value_of_local(LocalField::BranchSource),
        "Default (Remote)"
    );
    // Forward from the default (Remote) wraps to Local, then back to Remote.
    assert!(config.cycle_selected(true));
    assert_eq!(
        config.local().unwrap().default_branch_source,
        Some(BranchSource::Local)
    );
    assert_eq!(config.value_of_local(LocalField::BranchSource), "Local");
    assert!(config.cycle_selected(true));
    assert_eq!(
        config.local().unwrap().default_branch_source,
        Some(BranchSource::Remote)
    );
    assert_eq!(config.value_of_local(LocalField::BranchSource), "Remote");
    // Backward toggles the other way.
    assert!(config.cycle_selected(false));
    assert_eq!(
        config.local().unwrap().default_branch_source,
        Some(BranchSource::Local)
    );
}

#[test]
fn cycling_a_local_default_branch_walks_auto_then_each_branch() {
    let mut config = local_config_with_branches(&["develop", "main"]);
    select_local(&mut config, LocalField::DefaultBranch);
    // Unset means "auto" (the detected default branch).
    assert_eq!(
        config.value_of_local(LocalField::DefaultBranch),
        "Default (auto)"
    );
    assert_eq!(config.local().unwrap().default_branch, None);

    // auto -> develop -> main -> auto.
    assert!(config.cycle_selected(true));
    assert_eq!(
        config.local().unwrap().default_branch.as_deref(),
        Some("develop")
    );
    assert_eq!(config.value_of_local(LocalField::DefaultBranch), "develop");
    assert!(config.cycle_selected(true));
    assert_eq!(
        config.local().unwrap().default_branch.as_deref(),
        Some("main")
    );
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().default_branch, None);
    // Backward from auto wraps to the last branch.
    assert!(config.cycle_selected(false));
    assert_eq!(
        config.local().unwrap().default_branch.as_deref(),
        Some("main")
    );
}

#[test]
fn cycling_the_default_branch_without_branches_is_a_noop() {
    let mut config = local_config(); // no branches available
    select_local(&mut config, LocalField::DefaultBranch);
    assert!(!config.cycle_selected(true));
    assert_eq!(config.local().unwrap().default_branch, None);
    assert!(!config.cycle_selected(false));
    assert_eq!(config.local().unwrap().default_branch, None);
}

#[test]
fn setup_commands_row_opens_a_multiline_editor_and_applies_trimmed_commands() {
    let mut config = local_config();
    select_local(&mut config, LocalField::SetupCommands);
    assert!(config.setup_row_active());
    assert!(!config.cycle_selected(true));
    assert_eq!(
        config.value_of_local(LocalField::SetupCommands),
        "Edit (none)"
    );

    config.open_setup_modal();
    assert_eq!(config.setup_modal().unwrap().lines(), &["".to_string()]);
    assert_eq!(config.setup_modal().unwrap().cursor(), (0, 0));
    for c in "first".chars() {
        config.setup_modal_insert(c);
    }
    config.setup_modal_cursor_home();
    config.setup_modal_cursor_right();
    config.setup_modal_cursor_left();
    config.setup_modal_cursor_end();
    config.setup_modal_newline();
    config.setup_modal_cursor_up();
    config.setup_modal_cursor_down();
    for c in " second ".chars() {
        config.setup_modal_insert(c);
    }
    config.setup_modal_backspace();
    config.setup_modal_delete_forward();

    config.apply_setup_modal();

    assert!(config.setup_modal().is_none());
    assert_eq!(
        config.local().unwrap().setup_commands,
        vec!["first".to_string(), "second".to_string()]
    );
    assert_eq!(
        config.value_of_local(LocalField::SetupCommands),
        "Edit (2 commands)"
    );
    assert!(config.is_dirty());
    assert!(config.rows()[7].changed);
}

#[test]
fn closing_setup_commands_modal_discards_the_buffer() {
    let mut config = local_config();
    select_local(&mut config, LocalField::SetupCommands);
    config.open_setup_modal();
    config.setup_modal_insert('x');
    config.close_setup_modal();

    assert!(config.setup_modal().is_none());
    assert!(config.local().unwrap().setup_commands.is_empty());
    assert!(!config.is_dirty());
}

#[test]
fn env_vars_row_opens_a_multiline_editor_and_applies_valid_bindings() {
    let mut config = local_config();
    select_local(&mut config, LocalField::EnvVars);
    assert!(config.env_row_active());
    // The Env Vars row is an action row, so ←/→ have nothing to cycle.
    assert!(!config.cycle_selected(true));
    assert_eq!(config.value_of_local(LocalField::EnvVars), "Edit (none)");

    config.open_env_modal();
    assert_eq!(config.env_modal().unwrap().lines(), &["".to_string()]);
    assert_eq!(config.env_modal().unwrap().cursor(), (0, 0));
    // Exercise every editing / caret method.
    for c in "GH_TOKEN=op://Private/GH/tokenX".chars() {
        config.env_modal_insert(c);
    }
    config.env_modal_cursor_home();
    config.env_modal_cursor_right();
    config.env_modal_cursor_left();
    config.env_modal_cursor_end();
    config.env_modal_backspace(); // drop the trailing X
    config.env_modal_newline();
    config.env_modal_cursor_up();
    config.env_modal_cursor_down();
    // A malformed line (no `=`) and an invalid name are dropped on apply; a blank
    // reference is dropped too. Only the valid binding survives.
    for c in "1BAD=op://x/y/z".chars() {
        config.env_modal_insert(c);
    }
    config.env_modal_delete_forward();

    config.apply_env_modal();

    assert!(config.env_modal().is_none());
    assert_eq!(
        config
            .local()
            .unwrap()
            .env
            .get("GH_TOKEN")
            .map(String::as_str),
        Some("op://Private/GH/token")
    );
    assert!(!config.local().unwrap().env.contains_key("1BAD"));
    assert_eq!(config.value_of_local(LocalField::EnvVars), "Edit (1 var)");
    assert!(config.is_dirty());
    assert!(config.rows()[8].changed);
}

#[test]
fn env_editor_seeds_from_existing_bindings_and_the_label_counts_them() {
    let mut config = local_config();
    config.local.as_mut().unwrap().settings.env = [
        ("A_TOKEN".to_string(), "op://v/i/a".to_string()),
        ("B_TOKEN".to_string(), "op://v/i/b".to_string()),
    ]
    .into_iter()
    .collect();
    assert_eq!(config.value_of_local(LocalField::EnvVars), "Edit (2 vars)");

    // Opening the editor on the Env Vars row seeds the buffer from the bindings
    // in sorted order.
    select_local(&mut config, LocalField::EnvVars);
    config.open_env_modal();
    assert_eq!(
        config.env_modal().unwrap().lines(),
        &[
            "A_TOKEN=op://v/i/a".to_string(),
            "B_TOKEN=op://v/i/b".to_string()
        ]
    );
}

#[test]
fn a_later_binding_line_overrides_an_earlier_one_with_the_same_name() {
    let mut config = local_config();
    select_local(&mut config, LocalField::EnvVars);
    config.open_env_modal();
    for c in "DUP=op://v/i/first".chars() {
        config.env_modal_insert(c);
    }
    config.env_modal_newline();
    for c in "DUP=op://v/i/second".chars() {
        config.env_modal_insert(c);
    }
    config.apply_env_modal();

    assert_eq!(
        config.local().unwrap().env.get("DUP").map(String::as_str),
        Some("op://v/i/second")
    );
}

#[test]
fn closing_env_vars_modal_discards_the_buffer() {
    let mut config = local_config();
    select_local(&mut config, LocalField::EnvVars);
    config.open_env_modal();
    config.env_modal_insert('X');
    config.close_env_modal();

    assert!(config.env_modal().is_none());
    assert!(config.local().unwrap().env.is_empty());
    assert!(!config.is_dirty());
}

#[test]
fn open_env_modal_is_a_noop_off_the_env_row() {
    let mut config = local_config();
    select_local(&mut config, LocalField::SetupCommands);
    config.open_env_modal();
    assert!(config.env_modal().is_none());
}

#[test]
fn apply_env_modal_is_a_noop_when_no_modal_is_open() {
    let mut config = local_config();
    config.apply_env_modal();
    assert!(config.env_modal().is_none());
    assert!(config.local().unwrap().env.is_empty());
}

// --- session labels ------------------------------------------------------

/// A label def with no icon, for terse test masters.
fn label_def(id: &str, name: &str, color: LabelColor) -> SessionLabelDef {
    SessionLabelDef {
        id: id.to_string(),
        name: name.to_string(),
        color,
        icon: None,
    }
}

#[test]
fn session_labels_row_is_an_action_that_defers_to_global_when_unset() {
    let mut config = local_config();
    select_local(&mut config, LocalField::SessionLabels);
    assert!(config.session_labels_row_active());
    // An action row: ←/→ have nothing to cycle.
    assert!(!config.cycle_selected(true));
    // Unset defers to the effective global master (the default 5-label set).
    assert_eq!(
        config.value_of_local(LocalField::SessionLabels),
        "Edit (global: 5 labels)"
    );
    assert!(config.rows()[7].action);
}

#[test]
fn session_labels_editor_seeds_from_the_global_master_when_unset() {
    let mut config = local_config();
    select_local(&mut config, LocalField::SessionLabels);
    config.open_session_labels_modal();
    // Seeded from the default global master, one `id | name | color | icon` line
    // per label.
    let lines = config.session_labels_modal().unwrap().lines();
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[0], "todo | Todo | gray | ○");
    // A non-empty buffer opens with the caret at the end of the last line.
    assert_eq!(config.session_labels_modal().unwrap().cursor().0, 4);
}

#[test]
fn session_labels_editor_seeds_from_an_existing_override() {
    let mut config = local_config();
    config.local.as_mut().unwrap().settings.session_labels = Some(SessionLabelMaster {
        labels: vec![label_def("wip", "WIP", LabelColor::Blue)],
    });
    assert_eq!(
        config.value_of_local(LocalField::SessionLabels),
        "Edit (1 label)"
    );

    select_local(&mut config, LocalField::SessionLabels);
    config.open_session_labels_modal();
    assert_eq!(
        config.session_labels_modal().unwrap().lines(),
        &["wip | WIP | blue".to_string()]
    );
}

#[test]
fn session_labels_row_opens_an_editor_and_applies_a_sanitized_override() {
    let mut config = local_config();
    // Seed a single-label override so the editor opens on a short buffer.
    config.local.as_mut().unwrap().settings.session_labels = Some(SessionLabelMaster {
        labels: vec![label_def("todo", "Todo", LabelColor::Gray)],
    });
    config.local.as_mut().unwrap().baseline.session_labels =
        config.local().unwrap().session_labels.clone();

    select_local(&mut config, LocalField::SessionLabels);
    config.open_session_labels_modal();

    // Exercise every editing / caret method: append a second label line.
    config.session_labels_modal_cursor_end();
    config.session_labels_modal_newline();
    for c in "review | Reviewing | magentaX".chars() {
        config.session_labels_modal_insert(c);
    }
    config.session_labels_modal_cursor_home();
    config.session_labels_modal_cursor_right();
    config.session_labels_modal_cursor_left();
    config.session_labels_modal_cursor_up();
    config.session_labels_modal_cursor_down();
    config.session_labels_modal_cursor_end();
    config.session_labels_modal_backspace(); // drop the trailing X on "magentaX"
    config.session_labels_modal_delete_forward(); // no-op at end of buffer

    config.apply_session_labels_modal();

    assert!(config.session_labels_modal().is_none());
    let master = config
        .local()
        .unwrap()
        .session_labels
        .as_ref()
        .expect("override stored");
    assert_eq!(master.labels().len(), 2);
    assert_eq!(master.labels()[1].id, "review");
    assert_eq!(master.labels()[1].name, "Reviewing");
    assert_eq!(master.labels()[1].color, LabelColor::Magenta);
    assert_eq!(
        config.value_of_local(LocalField::SessionLabels),
        "Edit (2 labels)"
    );
    assert!(config.is_dirty());
    assert!(config.rows()[9].changed);
}

#[test]
fn applying_labels_identical_to_the_global_master_defers_to_global() {
    let mut config = local_config();
    select_local(&mut config, LocalField::SessionLabels);
    // The editor is seeded from the global master; saving it untouched must not
    // create an override — it stays "follow global" (None).
    config.open_session_labels_modal();
    config.apply_session_labels_modal();

    assert!(config.local().unwrap().session_labels.is_none());
    assert!(!config.is_dirty());
}

#[test]
fn clearing_the_labels_editor_turns_the_feature_off_for_the_project() {
    let mut config = local_config();
    // A single-label override, so the editor opens on one short line.
    config.local.as_mut().unwrap().settings.session_labels = Some(SessionLabelMaster {
        labels: vec![label_def("wip", "WIP", LabelColor::Blue)],
    });
    select_local(&mut config, LocalField::SessionLabels);
    config.open_session_labels_modal();
    // Clear the buffer, then apply: an empty (but Some) override turns the
    // manual-status feature off for this project — distinct from the non-empty
    // global set, so it is stored rather than folded to None. The caret opens at
    // the end of the one line; a bounded run of backspaces empties it (extra
    // presses at the start are a safe no-op).
    config.session_labels_modal_cursor_end();
    for _ in 0..64 {
        config.session_labels_modal_backspace();
    }
    assert!(config
        .session_labels_modal()
        .unwrap()
        .lines()
        .iter()
        .all(String::is_empty));
    config.apply_session_labels_modal();

    let master = config
        .local()
        .unwrap()
        .session_labels
        .as_ref()
        .expect("empty override stored");
    assert!(master.is_empty());
    assert_eq!(
        config.value_of_local(LocalField::SessionLabels),
        "Edit (off)"
    );
}

#[test]
fn global_master_off_shows_off_in_the_deferring_row() {
    let global = Settings {
        session_labels: SessionLabelMaster { labels: vec![] },
        ..Default::default()
    };
    let config = Config::workspace(global, LocalSettings::default(), Vec::new());
    assert_eq!(
        config.value_of_local(LocalField::SessionLabels),
        "Edit (global: off)"
    );
}

#[test]
fn closing_session_labels_modal_discards_the_buffer() {
    let mut config = local_config();
    select_local(&mut config, LocalField::SessionLabels);
    config.open_session_labels_modal();
    config.session_labels_modal_insert('X');
    config.close_session_labels_modal();

    assert!(config.session_labels_modal().is_none());
    assert!(config.local().unwrap().session_labels.is_none());
    assert!(!config.is_dirty());
}

#[test]
fn open_session_labels_modal_is_a_noop_off_the_row() {
    let mut config = local_config();
    select_local(&mut config, LocalField::EnvVars);
    config.open_session_labels_modal();
    assert!(config.session_labels_modal().is_none());
}

#[test]
fn apply_session_labels_modal_is_a_noop_when_no_modal_is_open() {
    let mut config = local_config();
    config.apply_session_labels_modal();
    assert!(config.session_labels_modal().is_none());
    assert!(config.local().unwrap().session_labels.is_none());
}

#[test]
fn an_unknown_default_branch_resets_to_the_first_choice() {
    let mut config = local_config_with_branches(&["develop", "main"]);
    // A branch that is no longer present behaves like "auto" (index 0).
    config.local.as_mut().unwrap().settings.default_branch = Some("ghost".to_string());
    select_local(&mut config, LocalField::DefaultBranch);
    assert!(config.cycle_selected(true));
    assert_eq!(
        config.local().unwrap().default_branch.as_deref(),
        Some("develop")
    );
}

#[test]
fn cycling_a_local_agent_cli_override_walks_global_then_each_value() {
    let mut config = local_config();
    // The first local field is selected from the start.
    assert_eq!(config.selected_local_field(), Some(LocalField::AgentCli));

    // None (follow global) -> Claude -> Codex -> SakanaAi -> Gemini
    // -> Antigravity -> None.
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::Claude));
    assert!(config
        .value_of_local(LocalField::AgentCli)
        .contains("Override"));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::Codex));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::SakanaAi));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::Gemini));
    assert!(config.cycle_selected(true));
    assert_eq!(
        config.local().unwrap().agent_cli,
        Some(AgentCli::Antigravity)
    );
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, None);
    // Backward from None wraps to the last value.
    assert!(config.cycle_selected(false));
    assert_eq!(
        config.local().unwrap().agent_cli,
        Some(AgentCli::Antigravity)
    );
}

#[test]
fn cycling_a_local_agent_cli_override_offers_only_installed_agents() {
    // With only Codex installed, the override cycles "follow global" then just
    // Codex — the uninstalled agents are not offered as override targets.
    let mut config = local_config();
    config.set_available_agent_clis(vec![AgentCli::Codex]);
    assert_eq!(config.selected_local_field(), Some(LocalField::AgentCli));

    // None (follow global) -> Codex -> None.
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::Codex));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, None);
}

#[test]
fn cycling_a_local_notifications_override_walks_global_on_off() {
    let mut config = local_config();
    config.move_down(); // select Notifications (second local field)
    assert_eq!(
        config.selected_local_field(),
        Some(LocalField::Notifications)
    );
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().notifications_enabled, Some(true));
    assert!(config
        .value_of_local(LocalField::Notifications)
        .contains("Override"));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().notifications_enabled, Some(false));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().notifications_enabled, None);
}

#[test]
fn cycling_a_local_restore_panes_override_walks_global_on_off() {
    let mut config = local_config();
    select_local(&mut config, LocalField::RestorePanes);
    // Unset shows the global value it falls back to.
    assert!(config
        .value_of_local(LocalField::RestorePanes)
        .contains("Global"));
    // None (follow global) -> On -> Off -> None.
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().restore_panes_enabled, Some(true));
    assert!(config
        .value_of_local(LocalField::RestorePanes)
        .contains("Override"));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().restore_panes_enabled, Some(false));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().restore_panes_enabled, None);
}

#[test]
fn cycling_a_local_autostart_queued_override_walks_global_on_off() {
    let mut config = local_config();
    select_local(&mut config, LocalField::AutostartQueued);
    // Unset shows the global value it falls back to.
    assert!(config
        .value_of_local(LocalField::AutostartQueued)
        .contains("Global"));
    // None (follow global) -> On -> Off -> None.
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().autostart_queued_prompts, Some(true));
    assert!(config
        .value_of_local(LocalField::AutostartQueued)
        .contains("Override"));
    assert!(config.cycle_selected(true));
    assert_eq!(
        config.local().unwrap().autostart_queued_prompts,
        Some(false)
    );
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().autostart_queued_prompts, None);
}

#[test]
fn cycling_a_local_autostart_limit_override_walks_global_then_limits() {
    let mut config = local_config();
    select_local(&mut config, LocalField::AutostartQueuedLimit);
    assert_eq!(
        config.value_of_local(LocalField::AutostartQueuedLimit),
        "Global (4)"
    );

    assert!(config.cycle_selected(true));
    assert_eq!(
        config.local().unwrap().autostart_queued_prompt_limit,
        Some(1)
    );
    assert_eq!(
        config.value_of_local(LocalField::AutostartQueuedLimit),
        "Override: 1"
    );
    assert!(config.cycle_selected(false));
    assert_eq!(config.local().unwrap().autostart_queued_prompt_limit, None);
}

#[test]
fn editing_a_local_override_marks_the_config_dirty_and_mark_saved_clears_it() {
    let mut config = local_config();
    // The first local field (Agent CLI) is under the cursor from the start.
    assert!(!config.is_dirty());
    assert!(config.cycle_selected(true)); // set a local agent override
    assert!(config.is_dirty());
    assert!(config.is_local_changed(LocalField::AgentCli));
    assert!(!config.is_local_changed(LocalField::Notifications));
    // The corresponding row is flagged changed.
    assert!(config.rows()[0].changed);

    config.mark_saved();
    assert!(!config.is_dirty());
    assert!(!config.is_local_changed(LocalField::AgentCli));
}

#[test]
fn value_of_local_shows_overrides_and_is_empty_without_a_context() {
    // Without a local context the helper yields an empty string (it is never
    // rendered in that case).
    let config = config_with_workspaces(&[]);
    assert_eq!(config.value_of_local(LocalField::AgentCli), "");
    // is_local_changed is also false without a context.
    assert!(!config.is_local_changed(LocalField::AgentCli));

    // In the local scope with an override set, it shows the override value.
    let mut local = local_config();
    // Agent CLI is the first local field, already selected.
    local.cycle_selected(true); // Claude override
    assert_eq!(
        local.value_of_local(LocalField::AgentCli),
        "Override: Claude"
    );
    assert_eq!(
        local.value_of_local(LocalField::Notifications),
        "Global (On)"
    );
}

// --- shipped-skill feature toggles -------------------------------------

/// Move the cursor onto the first shipped-skill feature row.
fn select_skill_feature(config: &mut Config) {
    while config.selected_skill_feature().is_none() {
        config.move_down();
    }
}

#[test]
fn global_skill_feature_row_toggles_on_off_and_tracks_dirty() {
    let mut config = config_with_workspaces(&[]);
    select_skill_feature(&mut config);
    assert_eq!(
        config.selected_skill_feature(),
        Some(SkillFeature::PullRequest)
    );
    // On by default, nothing dirty.
    assert_eq!(config.value_of_skill(SkillFeature::PullRequest), "On");
    assert!(!config.is_dirty());

    // Toggling flips it off (a boolean: direction is irrelevant): reflected in
    // the settings, flagged changed, and the config goes dirty.
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of_skill(SkillFeature::PullRequest), "Off");
    assert!(!config
        .settings()
        .skill_feature_enabled(SkillFeature::PullRequest));
    assert!(config.is_dirty());
    assert!(config.rows().last().unwrap().changed);

    // Toggling back on matches the default, so the override map is emptied again
    // and the config is clean (no stray key left behind).
    assert!(config.cycle_selected(false));
    assert_eq!(config.value_of_skill(SkillFeature::PullRequest), "On");
    assert!(config.settings().skill_features.is_empty());
    assert!(!config.is_dirty());
    assert!(!config.rows().last().unwrap().changed);
}

#[test]
fn local_skill_feature_override_cycles_global_on_off() {
    let mut config = local_config();
    select_skill_feature(&mut config);
    // Unset: the override follows the global value (on).
    assert_eq!(
        config.value_of_skill(SkillFeature::PullRequest),
        "Global (On)"
    );
    assert!(!config.is_dirty());

    // None (follow global) -> On -> Off -> None.
    assert!(config.cycle_selected(true));
    assert_eq!(
        config
            .local()
            .unwrap()
            .skill_feature_override(SkillFeature::PullRequest),
        Some(true)
    );
    assert_eq!(
        config.value_of_skill(SkillFeature::PullRequest),
        "Override: On"
    );
    assert!(config.is_dirty());
    assert!(config.cycle_selected(true));
    assert_eq!(
        config
            .local()
            .unwrap()
            .skill_feature_override(SkillFeature::PullRequest),
        Some(false)
    );
    assert_eq!(
        config.value_of_skill(SkillFeature::PullRequest),
        "Override: Off"
    );
    assert!(config.cycle_selected(true));
    assert_eq!(
        config
            .local()
            .unwrap()
            .skill_feature_override(SkillFeature::PullRequest),
        None
    );
    // Back to following global: clean again.
    assert!(!config.is_dirty());
}

#[test]
fn local_field_labels_are_distinct() {
    assert_eq!(LocalField::AgentCli.label(), "Agent CLI");
    assert_eq!(LocalField::Notifications.label(), "Notifications");
    assert_eq!(LocalField::RestorePanes.label(), "Restore Panes");
    assert_eq!(
        LocalField::AutostartQueued.label(),
        "Autostart Queued Prompts"
    );
    assert_eq!(LocalField::DefaultBranch.label(), "Default Branch");
    assert_eq!(LocalField::BranchSource.label(), "Branch Source");
    assert_eq!(LocalField::SetupCommands.label(), "Setup Commands");
    assert_eq!(LocalField::EnvVars.label(), "Env Vars");
    assert_eq!(LocalField::SessionLabels.label(), "Session Labels");
    assert_eq!(LocalField::ALL.len(), 10);
}

#[test]
fn title_and_subtitle_reflect_the_scope() {
    let global = config_with_workspaces(&[]);
    assert_eq!(global.title(), "Config");
    assert_eq!(global.subtitle(), "Adjust your global preferences");

    let local = local_config();
    assert_eq!(local.title(), "Workspace Config");
    assert_eq!(local.subtitle(), "Adjust this workspace's settings");
}

#[test]
fn setup_commands_label_shows_singular_for_one_command() {
    let mut config = local_config();
    select_local(&mut config, LocalField::SetupCommands);
    config.open_setup_modal();
    for c in "bun install".chars() {
        config.setup_modal_insert(c);
    }
    config.apply_setup_modal();
    assert_eq!(
        config.value_of_local(LocalField::SetupCommands),
        "Edit (1 command)"
    );
}

#[test]
fn apply_setup_modal_is_a_noop_when_no_modal_is_open() {
    let mut config = local_config();
    select_local(&mut config, LocalField::SetupCommands);
    // No modal opened — calling apply must not panic and must not change state.
    config.apply_setup_modal();
    assert!(config.setup_modal().is_none());
    assert!(config.local().unwrap().setup_commands.is_empty());
}
