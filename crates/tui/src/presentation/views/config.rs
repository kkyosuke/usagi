//! Config screen state and rendering.

use usagi_core::domain::settings::{ModalSelectionMode, Settings, Theme};
use usagi_core::usecase::settings::{SettingsPort, SettingsScope};

use crate::presentation::layouts::mascot_screen;
use crate::presentation::theme::Style;
use crate::presentation::widgets::select;

const TITLE: &str = "Config";
const FOOTER: &str = "Tab: scope  ↑↓: select  ←→: change  Enter: confirm  Esc: back";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Field {
    #[default]
    Theme,
    ModalSelectionMode,
    Save,
}

#[derive(Debug, Clone)]
struct ScopeSettings {
    saved: Settings,
    draft: Settings,
}

impl ScopeSettings {
    fn new(saved: Settings) -> Self {
        Self {
            draft: saved.clone(),
            saved,
        }
    }

    fn is_dirty(&self) -> bool {
        self.draft != self.saved
    }
}

/// Editable Config screen state.  Each scope owns an independent saved value
/// and draft, so switching scopes never discards an unsaved edit.
#[derive(Debug, Clone)]
pub struct Config {
    scope: SettingsScope,
    field: Field,
    global: ScopeSettings,
    workspace: ScopeSettings,
    notice: Option<String>,
}

impl Config {
    /// Read both settings scopes from `port` and initialise independent drafts.
    /// A failed initial read falls back to defaults while surfacing a safe error.
    #[must_use]
    pub fn load(port: &mut dyn SettingsPort) -> Self {
        let (global, global_error) = read_scope(port, SettingsScope::Global);
        let (workspace, workspace_error) = read_scope(port, SettingsScope::Workspace);
        Self {
            scope: SettingsScope::Global,
            field: Field::Theme,
            global: ScopeSettings::new(global),
            workspace: ScopeSettings::new(workspace),
            notice: global_error.or(workspace_error),
        }
    }

    /// Returns the selected persistence scope.
    #[must_use]
    pub fn scope(&self) -> SettingsScope {
        self.scope
    }

    /// Returns the selected editable setting.
    #[must_use]
    pub fn field(&self) -> Field {
        self.field
    }

    /// Move to the next setting or Save action.
    pub fn next_field(&mut self) {
        self.field = match self.field {
            Field::Theme => Field::ModalSelectionMode,
            Field::ModalSelectionMode => Field::Save,
            Field::Save => Field::Theme,
        };
        self.notice = None;
    }

    /// Move to the previous editable setting.
    pub fn previous_field(&mut self) {
        self.field = match self.field {
            Field::Theme => Field::Save,
            Field::ModalSelectionMode => Field::Theme,
            Field::Save => Field::ModalSelectionMode,
        };
        self.notice = None;
    }

    /// Returns whether the selected scope has an unsaved draft.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.current().is_dirty()
    }

    /// Returns the selected scope's editable settings.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.current().draft
    }

    /// Returns the saved global modal interaction for newly opened workspaces.
    #[must_use]
    pub fn global_modal_selection_mode(&self) -> ModalSelectionMode {
        self.global.saved.modal_selection_mode
    }

    /// Returns the latest save or load feedback, if any.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Switch between global and workspace settings without changing drafts.
    pub fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            SettingsScope::Global => SettingsScope::Workspace,
            SettingsScope::Workspace => SettingsScope::Global,
        };
        self.notice = None;
    }

    /// Cycle the selected scope's theme. Either arrow direction uses the same
    /// two non-system alternatives before returning to system.
    pub fn cycle_theme(&mut self, forward: bool) {
        let theme = &mut self.current_mut().draft.theme;
        *theme = match (*theme, forward) {
            (Theme::System, true) | (Theme::Light, false) => Theme::Dark,
            (Theme::Dark, true) | (Theme::System, false) => Theme::Light,
            (Theme::Light, true) | (Theme::Dark, false) => Theme::System,
        };
        self.notice = None;
    }

    /// Toggle how Overview and Closeup accept a command.
    pub fn cycle_modal_selection_mode(&mut self) {
        let mode = &mut self.current_mut().draft.modal_selection_mode;
        *mode = match *mode {
            ModalSelectionMode::Action => ModalSelectionMode::Prompt,
            ModalSelectionMode::Prompt => ModalSelectionMode::Action,
        };
        self.notice = None;
    }

    /// Change the focused select value. Returns false for the Save action.
    pub fn cycle_selected(&mut self, forward: bool) -> bool {
        match self.field {
            Field::Theme => self.cycle_theme(forward),
            Field::ModalSelectionMode => self.cycle_modal_selection_mode(),
            Field::Save => return false,
        }
        true
    }

    /// Returns whether the focused row is the enabled Save action.
    #[must_use]
    pub fn can_save(&self) -> bool {
        self.field == Field::Save && self.is_dirty()
    }

    /// Save the selected scope when it is dirty. Returns true only after a
    /// successful persistence, allowing the caller to close the screen.
    pub fn save(&mut self, port: &mut dyn SettingsPort) -> bool {
        if !self.is_dirty() {
            return false;
        }
        let scope = self.scope;
        let draft = self.current().draft.clone();
        match port.save(scope, &draft) {
            Ok(()) => {
                self.current_mut().saved = draft;
                self.notice = Some("saved".to_string());
                true
            }
            Err(error) => {
                self.notice = Some(format!("Save failed: {error}"));
                false
            }
        }
    }

    fn current(&self) -> &ScopeSettings {
        match self.scope {
            SettingsScope::Global => &self.global,
            SettingsScope::Workspace => &self.workspace,
        }
    }

    fn current_mut(&mut self) -> &mut ScopeSettings {
        match self.scope {
            SettingsScope::Global => &mut self.global,
            SettingsScope::Workspace => &mut self.workspace,
        }
    }
}

fn read_scope(port: &mut dyn SettingsPort, scope: SettingsScope) -> (Settings, Option<String>) {
    match port.read(scope) {
        Ok(settings) => (settings, None),
        Err(error) => (Settings::default(), Some(format!("Load failed: {error}"))),
    }
}

/// Render a Config frame using its current scope, draft, and feedback.
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, config: &Config) -> Vec<String> {
    mascot_screen::render(raw_height, raw_width, TITLE, FOOTER, |width| {
        let scope = match config.scope() {
            SettingsScope::Global => "Scope: [Global]   Workspace",
            SettingsScope::Workspace => "Scope: Global   [Workspace]",
        };
        let mut lines = vec![
            mascot_screen::centered_line(width, scope, Style::new()),
            mascot_screen::centered_line(
                width,
                &select::render(
                    "Theme",
                    theme_name(config.settings().theme),
                    config.field() == Field::Theme,
                    config.settings().theme != config.current().saved.theme,
                ),
                Style::new(),
            ),
            mascot_screen::centered_line(
                width,
                &select::render(
                    "Modal mode",
                    modal_selection_mode_name(config.settings().modal_selection_mode),
                    config.field() == Field::ModalSelectionMode,
                    config.settings().modal_selection_mode
                        != config.current().saved.modal_selection_mode,
                ),
                Style::new(),
            ),
            String::new(),
            mascot_screen::centered_line(
                width,
                &select::action(
                    if config.notice() == Some("saved") {
                        "saved"
                    } else {
                        "Save"
                    },
                    config.field() == Field::Save,
                    config.is_dirty(),
                ),
                Style::new(),
            ),
        ];
        if let Some(notice) = config.notice() {
            lines.push(mascot_screen::centered_line(
                width,
                notice,
                Style::new().dim(),
            ));
        }
        lines
    })
}

fn theme_name(theme: Theme) -> &'static str {
    match theme {
        Theme::Light => "light",
        Theme::Dark => "dark",
        Theme::System => "system",
    }
}

fn modal_selection_mode_name(mode: ModalSelectionMode) -> &'static str {
    match mode {
        ModalSelectionMode::Action => "action",
        ModalSelectionMode::Prompt => "prompt",
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Field, render};
    use std::io;
    use usagi_core::domain::settings::{ModalSelectionMode, Settings, Theme};
    use usagi_core::usecase::settings::{SettingsPort, SettingsScope};

    #[derive(Default)]
    struct FakeSettingsPort {
        global: Settings,
        workspace: Settings,
        fail_read: Option<SettingsScope>,
        fail_save: bool,
    }

    impl SettingsPort for FakeSettingsPort {
        fn read(&mut self, scope: SettingsScope) -> io::Result<Settings> {
            if self.fail_read == Some(scope) {
                return Err(io::Error::other("settings unavailable"));
            }
            Ok(match scope {
                SettingsScope::Global => self.global.clone(),
                SettingsScope::Workspace => self.workspace.clone(),
            })
        }

        fn save(&mut self, scope: SettingsScope, settings: &Settings) -> io::Result<()> {
            if self.fail_save {
                return Err(io::Error::other("disk unavailable"));
            }
            match scope {
                SettingsScope::Global => self.global = settings.clone(),
                SettingsScope::Workspace => self.workspace = settings.clone(),
            }
            Ok(())
        }
    }

    #[test]
    fn scopes_keep_independent_drafts_and_saved_values() {
        let mut port = FakeSettingsPort {
            global: Settings {
                theme: Theme::Light,
                ..Settings::default()
            },
            workspace: Settings {
                theme: Theme::Dark,
                ..Settings::default()
            },
            ..FakeSettingsPort::default()
        };
        let mut config = Config::load(&mut port);
        let initial = render(24, 80, &config).join("\n");
        assert!(initial.contains("Theme") && initial.contains("light"));
        config.cycle_theme(true);
        config.save(&mut port);
        config.toggle_scope();

        assert_eq!(config.settings().theme, Theme::Dark);
        assert!(!config.is_dirty());
        assert_eq!(port.global.theme, Theme::System);
        assert_eq!(port.workspace.theme, Theme::Dark);

        let workspace = render(24, 80, &config).join("\n");
        assert!(workspace.contains("Theme") && workspace.contains("dark"));
        config.cycle_theme(false);
        config.save(&mut port);
        assert_eq!(port.workspace.theme, Theme::System);
        config.toggle_scope();
        assert_eq!(config.scope(), SettingsScope::Global);
    }

    #[test]
    fn failed_save_keeps_the_draft_dirty_for_retry() {
        let mut port = FakeSettingsPort {
            fail_save: true,
            ..FakeSettingsPort::default()
        };
        let mut config = Config::load(&mut port);
        config.cycle_theme(true);
        config.save(&mut port);

        assert_eq!(config.settings().theme, Theme::Dark);
        assert!(config.is_dirty());
        assert_eq!(config.notice(), Some("Save failed: disk unavailable"));

        port.fail_save = false;
        config.save(&mut port);
        assert!(!config.is_dirty());
        assert_eq!(port.global.theme, Theme::Dark);
    }

    #[test]
    fn render_shows_scope_theme_state_and_footer() {
        let mut port = FakeSettingsPort::default();
        let config = Config::load(&mut port);
        let frame = render(24, 80, &config).join("\n");

        assert!(frame.contains("Config"));
        assert!(frame.contains("Scope: [Global]"));
        assert!(frame.contains("Theme") && frame.contains("system"));
        assert!(frame.contains("Modal mode") && frame.contains("action"));
        assert!(frame.contains("[ Save ]"));
        assert!(frame.contains("Esc: back"));
    }

    #[test]
    fn load_error_and_workspace_draft_are_rendered_without_losing_the_form() {
        let mut port = FakeSettingsPort {
            fail_read: Some(SettingsScope::Global),
            workspace: Settings {
                theme: Theme::Dark,
                ..Settings::default()
            },
            ..FakeSettingsPort::default()
        };
        let mut config = Config::load(&mut port);

        assert_eq!(config.notice(), Some("Load failed: settings unavailable"));
        let error_frame = render(24, 80, &config).join("\n");
        assert!(error_frame.contains("Load failed: settings unavailable"));
        config.toggle_scope();
        config.cycle_theme(true);
        let frame = render(24, 80, &config).join("\n");

        assert!(frame.contains("Scope: Global   [Workspace]"));
        assert!(frame.contains("Theme") && frame.contains("light"));
        assert!(frame.contains('●'));
    }

    #[test]
    fn save_is_selectable_only_with_an_unsaved_change() {
        let mut port = FakeSettingsPort::default();
        let mut config = Config::load(&mut port);
        config.next_field();
        config.next_field();
        assert_eq!(config.field(), Field::Save);
        assert!(!config.can_save());

        config.previous_field();
        config.cycle_modal_selection_mode();
        config.cycle_modal_selection_mode();
        config.cycle_selected(true);
        assert_eq!(
            config.settings().modal_selection_mode,
            ModalSelectionMode::Prompt
        );
        config.next_field();
        assert!(config.can_save());
        assert!(config.save(&mut port));
        assert_eq!(config.notice(), Some("saved"));
        assert!(!config.is_dirty());
        assert!(render(24, 80, &config).join("\n").contains("[ saved ]"));
    }

    #[test]
    fn field_navigation_wraps_and_save_refuses_a_clean_draft() {
        let mut port = FakeSettingsPort::default();
        let mut config = Config::load(&mut port);
        config.previous_field();
        assert_eq!(config.field(), Field::Save);
        assert!(!config.cycle_selected(true));
        assert!(!config.save(&mut port));

        config.previous_field();
        assert_eq!(config.field(), Field::ModalSelectionMode);
        config.previous_field();
        assert_eq!(config.field(), Field::Theme);
        config.next_field();
        config.next_field();
        config.next_field();
        assert_eq!(config.field(), Field::Theme);
    }
}
