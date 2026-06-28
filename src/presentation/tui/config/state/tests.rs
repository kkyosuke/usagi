use super::*;
use crate::domain::settings::LOCAL_LLM_MODELS;

fn config_with_workspaces(names: &[&str]) -> Config {
    Config::new(
        Settings::default(),
        names.iter().map(|n| n.to_string()).collect(),
    )
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
    assert_eq!(Field::RestorePanes.label(), "Restore Panes");
    assert_eq!(Field::MascotAnimation.label(), "Mascot Animation");
    assert_eq!(Field::ALL.len(), 10);
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
    // Global scope: ten fixed field rows, then one shipped-skill feature row.
    assert_eq!(config.rows().len(), 10 + SkillFeature::ALL.len());
    assert_eq!(config.save_index(), 10 + SkillFeature::ALL.len());
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
    config.move_down();
    config.move_down();
    config.move_down();
    config.move_down(); // select Agent CLI (after the Restore Panes row)
    assert_eq!(config.selected_field(), Some(Field::AgentCli));
    // Claude by default, cycling forward Claude -> Codex -> sakana.ai -> Gemini -> Claude.
    assert_eq!(config.value_of(Field::AgentCli), "Claude");
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "Codex");
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "sakana.ai");
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "Gemini");
    // Wraps back to Claude.
    assert!(config.cycle_selected(true));
    assert_eq!(config.value_of(Field::AgentCli), "Claude");
    // And cycles backward too (wrapping to the last value).
    assert!(config.cycle_selected(false));
    assert_eq!(config.value_of(Field::AgentCli), "Gemini");
}

#[test]
fn agent_cli_field_cycles_only_through_installed_agents() {
    // Only Claude and sakana.ai (codex-fugu) are installed: the selector skips
    // the uninstalled Codex and Gemini entirely.
    let mut config = config_with_workspaces(&[]);
    config.set_available_agent_clis(vec![AgentCli::Claude, AgentCli::CodexFugu]);
    config.move_down();
    config.move_down();
    config.move_down();
    config.move_down(); // select Agent CLI (after the Restore Panes row)
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
    config.move_down();
    config.move_down();
    config.move_down();
    config.move_down(); // select Agent CLI (after the Restore Panes row)
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
    assert_eq!(rows.len(), 10 + SkillFeature::ALL.len());
    assert_eq!(rows[0].label, "Theme");
    assert_eq!(rows[0].value, "System");
    assert_eq!(rows[3].label, "Restore Panes");
    assert_eq!(rows[3].value, "On");
    assert_eq!(rows[4].label, "Agent CLI");
    assert_eq!(rows[4].value, "Claude");
    assert_eq!(rows[5].label, "Session Action UI");
    assert_eq!(rows[5].value, "Menu");
    // The 没入 key scheme defaults to the Ctrl-O prefix.
    assert_eq!(rows[6].label, "Terminal Keys");
    assert_eq!(rows[6].value, "Ctrl-O prefix");
    assert_eq!(rows[7].label, "Mascot Animation");
    assert_eq!(rows[7].value, "On");
    // The runtime is not yet installed: the Local LLM row offers "Install" and
    // the model row is inert (shown as "—") until the runtime is present.
    assert_eq!(rows[8].label, "Local LLM");
    assert_eq!(rows[8].value, "Install");
    assert!(rows[8].action);
    assert_eq!(rows[9].label, "Local LLM Model");
    assert_eq!(rows[9].value, "—");
    assert!(rows[9].disabled);
    // The shipped-skill feature row follows the fixed fields: a plain on/off
    // chooser, on by default and neither an action nor disabled.
    assert_eq!(rows[10].label, "PR Skills");
    assert_eq!(rows[10].value, "On");
    assert!(!rows[10].action);
    assert!(!rows[10].disabled);
    assert!(rows.iter().all(|r| !r.changed));
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
    // The local scope shows the five override rows, then the shipped-skill
    // feature row(s) — no global fixed fields.
    assert_eq!(config.field_count(), 5 + SkillFeature::ALL.len());
    assert_eq!(config.save_index(), 5 + SkillFeature::ALL.len());
    // The cursor starts on the first local field, not a global one.
    assert_eq!(config.selected_field(), None);
    assert_eq!(config.selected_local_field(), Some(LocalField::AgentCli));
    let rows = config.rows();
    assert_eq!(rows.len(), 5 + SkillFeature::ALL.len());
    assert_eq!(rows[0].label, "Agent CLI");
    assert_eq!(rows[1].label, "Notifications");
    assert_eq!(rows[2].label, "Restore Panes");
    assert_eq!(rows[3].label, "Default Branch");
    assert_eq!(rows[4].label, "Branch Source");
    // The skill feature row falls back to the global value (on) when unset.
    assert_eq!(rows[5].label, "PR Skills");
    assert!(rows[5].value.contains("Global"));
    assert!(rows[5].value.contains("On"));
    // Unset overrides display the value they fall back to.
    assert!(rows[0].value.contains("Global"));
    assert!(rows[0].value.contains("Claude"));
    assert!(rows[1].value.contains("Global"));
    assert!(rows[1].value.contains("On"));
    // The restore-panes override also falls back to the global value (On).
    assert!(rows[2].value.contains("Global"));
    assert!(rows[2].value.contains("On"));
    // The default branch has no global counterpart: unset means "auto".
    assert!(rows[3].value.contains("Default"));
    assert!(rows[3].value.contains("auto"));
    // The branch source likewise shows its default (Remote).
    assert!(rows[4].value.contains("Default"));
    assert!(rows[4].value.contains("Remote"));
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
        Some(LocalField::DefaultBranch)
    );
    config.move_down();
    assert_eq!(
        config.selected_local_field(),
        Some(LocalField::BranchSource)
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

    // None (follow global) -> Claude -> Codex -> CodexFugu -> Gemini -> None.
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::Claude));
    assert!(config
        .value_of_local(LocalField::AgentCli)
        .contains("Override"));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::Codex));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::CodexFugu));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::Gemini));
    assert!(config.cycle_selected(true));
    assert_eq!(config.local().unwrap().agent_cli, None);
    // Backward from None wraps to the last value.
    assert!(config.cycle_selected(false));
    assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::Gemini));
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
    assert_eq!(LocalField::DefaultBranch.label(), "Default Branch");
    assert_eq!(LocalField::BranchSource.label(), "Branch Source");
    assert_eq!(LocalField::ALL.len(), 5);
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
