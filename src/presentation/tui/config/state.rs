//! Pure, terminal-independent state for the configuration screen.
//!
//! Holds the settings being edited, the registered workspace names the default
//! workspace can cycle through, and the cursor position. Keeping the editing
//! logic free of any terminal IO makes it directly testable.

use crate::domain::settings::{AgentCli, Settings, Theme};

/// The themes in the order they cycle through.
const THEMES: [Theme; 3] = [Theme::Light, Theme::Dark, Theme::System];

/// The agent CLIs in the order they cycle through.
pub(super) const AGENT_CLIS: [AgentCli; 2] = [AgentCli::Claude, AgentCli::Gemini];

/// An editable settings field, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Theme,
    DefaultWorkspace,
    Notifications,
    AgentCli,
}

impl Field {
    /// The fields shown on the screen, top to bottom.
    pub const ALL: [Field; 4] = [
        Field::Theme,
        Field::DefaultWorkspace,
        Field::Notifications,
        Field::AgentCli,
    ];

    /// The label shown beside the field's value.
    pub fn label(self) -> &'static str {
        match self {
            Field::Theme => "Theme",
            Field::DefaultWorkspace => "Default Workspace",
            Field::Notifications => "Notifications",
            Field::AgentCli => "Agent CLI",
        }
    }
}

/// The settings being edited together with the cursor position.
#[derive(Debug, Clone)]
pub struct Config {
    settings: Settings,
    /// Registered workspace names the default workspace cycles through.
    workspaces: Vec<String>,
    selected_index: usize,
}

impl Config {
    /// Builds the editor for the given settings, with the cursor at the top.
    ///
    /// `workspaces` are the names the default-workspace field can cycle through.
    pub fn new(settings: Settings, workspaces: Vec<String>) -> Self {
        Self {
            settings,
            workspaces,
            selected_index: 0,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn workspaces(&self) -> &[String] {
        &self.workspaces
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// The field currently under the cursor.
    pub fn selected_field(&self) -> Field {
        Field::ALL[self.selected_index]
    }

    /// Move the cursor up one row, wrapping to the bottom.
    pub fn move_up(&mut self) {
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(Field::ALL.len() - 1);
    }

    /// Move the cursor down one row, wrapping to the top.
    pub fn move_down(&mut self) {
        self.selected_index = (self.selected_index + 1) % Field::ALL.len();
    }

    /// The display value for a field, e.g. `"Dark"` or `"(none)"`.
    pub fn value_of(&self, field: Field) -> String {
        match field {
            Field::Theme => theme_label(self.settings.theme).to_string(),
            Field::DefaultWorkspace => self
                .settings
                .default_workspace
                .clone()
                .unwrap_or_else(|| "(none)".to_string()),
            Field::Notifications => if self.settings.notifications_enabled {
                "On"
            } else {
                "Off"
            }
            .to_string(),
            Field::AgentCli => agent_cli_label(self.settings.agent_cli).to_string(),
        }
    }

    /// Advance the selected field's value to its next (or previous) choice,
    /// wrapping. Returns `true` when the value actually changed, so the caller
    /// can persist it.
    pub fn cycle_selected(&mut self, forward: bool) -> bool {
        match self.selected_field() {
            Field::Theme => {
                self.settings.theme = cycle_theme(self.settings.theme, forward);
                true
            }
            Field::DefaultWorkspace => self.cycle_default_workspace(forward),
            Field::Notifications => {
                // A boolean toggle: direction is irrelevant, it always flips.
                self.settings.notifications_enabled = !self.settings.notifications_enabled;
                true
            }
            Field::AgentCli => {
                self.settings.agent_cli = cycle_agent_cli(self.settings.agent_cli, forward);
                true
            }
        }
    }

    /// Cycle the default workspace through `None` then each registered name.
    /// A no-op (returns `false`) when no workspaces are registered.
    fn cycle_default_workspace(&mut self, forward: bool) -> bool {
        if self.workspaces.is_empty() {
            return false;
        }
        // The choices are `None` (index 0) followed by each workspace name.
        let len = self.workspaces.len() + 1;
        let current = match &self.settings.default_workspace {
            None => 0,
            // An unknown name (e.g. a since-removed workspace) behaves like None.
            Some(name) => self
                .workspaces
                .iter()
                .position(|w| w == name)
                .map_or(0, |i| i + 1),
        };
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        self.settings.default_workspace = if next == 0 {
            None
        } else {
            Some(self.workspaces[next - 1].clone())
        };
        true
    }
}

/// The human-readable label for a theme.
fn theme_label(theme: Theme) -> &'static str {
    match theme {
        Theme::Light => "Light",
        Theme::Dark => "Dark",
        Theme::System => "System",
    }
}

/// The theme one step after `theme` in cycle order (or before, when `forward`
/// is false), wrapping at the ends.
fn cycle_theme(theme: Theme, forward: bool) -> Theme {
    let i = THEMES.iter().position(|&t| t == theme).unwrap_or(0);
    let len = THEMES.len();
    let next = if forward {
        (i + 1) % len
    } else {
        (i + len - 1) % len
    };
    THEMES[next]
}

/// The human-readable label for an agent CLI.
pub(super) fn agent_cli_label(cli: AgentCli) -> &'static str {
    match cli {
        AgentCli::Claude => "Claude",
        AgentCli::Gemini => "Gemini",
    }
}

/// The agent CLI one step after `cli` in cycle order (or before, when `forward`
/// is false), wrapping at the ends.
fn cycle_agent_cli(cli: AgentCli, forward: bool) -> AgentCli {
    let i = AGENT_CLIS.iter().position(|&c| c == cli).unwrap_or(0);
    let len = AGENT_CLIS.len();
    let next = if forward {
        (i + 1) % len
    } else {
        (i + len - 1) % len
    };
    AGENT_CLIS[next]
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(Field::ALL.len(), 4);
    }

    #[test]
    fn new_config_starts_at_the_top() {
        let config = config_with_workspaces(&["alpha"]);
        assert_eq!(config.selected_index(), 0);
        assert_eq!(config.selected_field(), Field::Theme);
        assert_eq!(config.workspaces(), ["alpha"]);
        assert_eq!(*config.settings(), Settings::default());
    }

    #[test]
    fn move_down_advances_and_wraps() {
        let mut config = config_with_workspaces(&[]);
        config.move_down();
        assert_eq!(config.selected_field(), Field::DefaultWorkspace);
        config.move_down();
        assert_eq!(config.selected_field(), Field::Notifications);
        config.move_down();
        assert_eq!(config.selected_field(), Field::AgentCli);
        // Wraps from the last field back to the first.
        config.move_down();
        assert_eq!(config.selected_field(), Field::Theme);
    }

    #[test]
    fn move_up_wraps_to_the_bottom() {
        let mut config = config_with_workspaces(&[]);
        config.move_up();
        assert_eq!(config.selected_field(), Field::AgentCli);
        config.move_up();
        assert_eq!(config.selected_field(), Field::Notifications);
        config.move_up();
        assert_eq!(config.selected_field(), Field::DefaultWorkspace);
        config.move_up();
        assert_eq!(config.selected_field(), Field::Theme);
    }

    #[test]
    fn notifications_field_toggles_and_reports_its_value() {
        let mut config = config_with_workspaces(&[]);
        config.move_down();
        config.move_down(); // select Notifications
        assert_eq!(config.selected_field(), Field::Notifications);
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
    fn agent_cli_field_cycles_between_claude_and_gemini() {
        let mut config = config_with_workspaces(&[]);
        config.move_down();
        config.move_down();
        config.move_down(); // select Agent CLI
        assert_eq!(config.selected_field(), Field::AgentCli);
        // Claude by default.
        assert_eq!(config.value_of(Field::AgentCli), "Claude");
        assert!(config.cycle_selected(true));
        assert_eq!(config.value_of(Field::AgentCli), "Gemini");
        // Wraps back to Claude.
        assert!(config.cycle_selected(true));
        assert_eq!(config.value_of(Field::AgentCli), "Claude");
        // And cycles backward too.
        assert!(config.cycle_selected(false));
        assert_eq!(config.value_of(Field::AgentCli), "Gemini");
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
}
