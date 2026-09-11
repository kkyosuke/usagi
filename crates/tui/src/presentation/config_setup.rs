//! Session setup editor input and persistence orchestration.

use usagi_core::usecase::settings::SettingsPort;

use crate::presentation::views::config::Config;
use crate::usecase::application::{Key, Terminal};

use super::{ConfigStep, run_workspace_loading, save_environment_responsive};

pub(super) fn save_setup_commands_responsive(
    term: &mut dyn Terminal,
    form: &mut Config,
    settings: &mut dyn SettingsPort,
) -> bool {
    let Some(commands) = form.setup_commands_save_request() else {
        return false;
    };
    let result = if settings.background_operations() {
        run_workspace_loading(term, "Saving session setup…", false, || {
            settings.save_workspace_setup_commands(&commands)
        })
    } else {
        settings.save_workspace_setup_commands(&commands)
    };
    form.finish_setup_commands_save(commands, result)
}

pub(super) fn save_config_source_responsive(
    term: &mut dyn Terminal,
    form: &mut Config,
    settings: &mut dyn SettingsPort,
) -> bool {
    if form.is_editing_setup_commands() {
        save_setup_commands_responsive(term, form, settings)
    } else {
        save_environment_responsive(term, form, settings)
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn step_setup_commands_editor(
    config: &mut Config,
    key: Key,
    settings: &mut dyn SettingsPort,
) -> ConfigStep {
    match key {
        Key::Enter if config.is_setup_commands_save_focused() => {
            if settings.background_operations() {
                return ConfigStep::SaveSource;
            }
            config.save_setup_commands(settings);
        }
        Key::Enter => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.newline();
            }
        }
        Key::Tab => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.toggle_save_focus(true);
            }
        }
        Key::Backspace => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.backspace();
            }
        }
        Key::Delete => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.delete_forward();
            }
        }
        Key::Left => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.move_cursor(false);
            }
        }
        Key::Right => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.move_cursor(true);
            }
        }
        Key::Up => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.move_vertical(false);
            }
        }
        Key::Down => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.move_vertical(true);
            }
        }
        Key::Home | Key::LineStart => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.move_edge(false);
            }
        }
        Key::End | Key::LineEnd => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.move_edge(true);
            }
        }
        Key::Char(character) if !character.is_control() => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.insert(&character.to_string());
            }
        }
        Key::Paste(text) => {
            if let Some(editor) = config.setup_commands_editor_mut() {
                editor.paste(&text);
            }
        }
        Key::Escape => config.cancel_setup_commands(),
        _ => {}
    }
    ConfigStep::Stay
}
