//! Pure, terminal-independent state for the configuration screen.
//!
//! Holds the settings being edited, the registered workspace names the default
//! workspace can cycle through, and the cursor position. Keeping the editing
//! logic free of any terminal IO makes it directly testable.
//!
//! When the screen is opened for a specific project (see [`Config::with_local`])
//! it additionally edits that project's **local overrides** — per-project values
//! for the agent CLI and notifications that fall back to the global settings
//! when left unset. Those rows are appended below the global fields.

use crate::domain::settings::{
    AgentCli, BranchSource, LocalSettings, Settings, Theme, LOCAL_LLM_MODELS,
};

/// The themes in the order they cycle through.
const THEMES: [Theme; 3] = [Theme::Light, Theme::Dark, Theme::System];

/// The agent CLIs in the order they cycle through.
pub(super) const AGENT_CLIS: [AgentCli; 2] = [AgentCli::Claude, AgentCli::Gemini];

/// The branch sources in the order they cycle through.
pub(super) const BRANCH_SOURCES: [BranchSource; 2] = [BranchSource::Local, BranchSource::Remote];

/// An editable global settings field, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Theme,
    DefaultWorkspace,
    Notifications,
    AgentCli,
    /// The local LLM enable toggle — or an "Install" action when the runtime /
    /// model is not yet present.
    LocalLlm,
    /// Which local LLM model is used (and installed on selection).
    LocalLlmModel,
}

impl Field {
    /// The fields shown on the screen, top to bottom.
    pub const ALL: [Field; 6] = [
        Field::Theme,
        Field::DefaultWorkspace,
        Field::Notifications,
        Field::AgentCli,
        Field::LocalLlm,
        Field::LocalLlmModel,
    ];

    /// The label shown beside the field's value.
    pub fn label(self) -> &'static str {
        match self {
            Field::Theme => "Theme",
            Field::DefaultWorkspace => "Default Workspace",
            Field::Notifications => "Notifications",
            Field::AgentCli => "Agent CLI",
            Field::LocalLlm => "Local LLM",
            Field::LocalLlmModel => "Local LLM Model",
        }
    }
}

/// An editable project-local override field, in display order. Each can either
/// follow the global setting or override it for the current project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalField {
    AgentCli,
    Notifications,
    DefaultBranch,
}

impl LocalField {
    /// The local override fields shown on the screen, top to bottom.
    pub const ALL: [LocalField; 3] = [
        LocalField::AgentCli,
        LocalField::Notifications,
        LocalField::DefaultBranch,
    ];

    /// The label shown beside the field's value.
    pub fn label(self) -> &'static str {
        match self {
            LocalField::AgentCli => "Local · Agent CLI",
            LocalField::Notifications => "Local · Notifications",
            LocalField::DefaultBranch => "Local · Default Branch",
        }
    }
}

/// One selectable row's display data, used by the renderer regardless of whether
/// the row is a global field or a project-local override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowView {
    pub label: &'static str,
    pub value: String,
    pub changed: bool,
}

/// A project's local overrides being edited, plus the last-saved baseline so
/// unsaved edits can be detected.
#[derive(Debug, Clone)]
struct LocalEdit {
    settings: LocalSettings,
    baseline: LocalSettings,
}

/// The settings being edited together with the cursor position.
///
/// Edits are held in `settings` and not written anywhere until the user saves.
/// `baseline` is the last-saved snapshot, so comparing the two tells us which
/// fields carry unsaved changes (and whether anything is dirty at all). When a
/// project context is present, `local` carries that project's overrides the same
/// way.
#[derive(Debug, Clone)]
pub struct Config {
    settings: Settings,
    /// The last-saved settings, used to detect unsaved edits.
    baseline: Settings,
    /// Registered workspace names the default workspace cycles through.
    workspaces: Vec<String>,
    /// The project-local overrides being edited, when opened for a project.
    local: Option<LocalEdit>,
    /// Whether the local LLM runtime and the selected model are present. Seeded
    /// when the screen opens; drives whether the Local LLM row shows an
    /// "Install" action or an on/off toggle.
    local_llm_installed: bool,
    selected_index: usize,
}

impl Config {
    /// Builds the editor for the given global settings, with the cursor at the
    /// top and no project-local section.
    ///
    /// `workspaces` are the names the default-workspace field can cycle through.
    /// The supplied settings double as the initial saved baseline, so a freshly
    /// opened screen reports no unsaved changes.
    pub fn new(settings: Settings, workspaces: Vec<String>) -> Self {
        Self {
            baseline: settings.clone(),
            settings,
            workspaces,
            local: None,
            local_llm_installed: false,
            selected_index: 0,
        }
    }

    /// Builds the editor with a project-local overrides section seeded from
    /// `local`. The global fields come first, then the local override rows.
    pub fn with_local(settings: Settings, workspaces: Vec<String>, local: LocalSettings) -> Self {
        Self {
            baseline: settings.clone(),
            settings,
            workspaces,
            local: Some(LocalEdit {
                baseline: local.clone(),
                settings: local,
            }),
            local_llm_installed: false,
            selected_index: 0,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Record whether the local LLM runtime and selected model are installed.
    /// Called when the screen opens, after probing the system.
    pub fn set_local_llm_installed(&mut self, installed: bool) {
        self.local_llm_installed = installed;
    }

    /// Whether the local LLM runtime and selected model are present.
    pub fn local_llm_installed(&self) -> bool {
        self.local_llm_installed
    }

    /// The currently selected local LLM model name.
    pub fn local_llm_model(&self) -> &str {
        &self.settings.local_llm.model
    }

    /// The model to install when the focused row is activated with **Enter**, or
    /// `None` when Enter should behave normally (save / toggle / cycle).
    ///
    /// Enter installs when the Local LLM row needs it (not yet installed) or when
    /// the model row is focused (selecting a model installs it).
    pub fn enter_installs_model(&self) -> Option<String> {
        match self.selected_field() {
            Some(Field::LocalLlm) if !self.local_llm_installed => Some(self.model_string()),
            Some(Field::LocalLlmModel) => Some(self.model_string()),
            _ => None,
        }
    }

    /// The model to install when the focused row is activated with an **arrow**
    /// key, or `None` when arrows should cycle/toggle as usual.
    ///
    /// Only the not-yet-installed Local LLM row installs on arrows — there is no
    /// toggle to cycle until it is installed. The model row keeps cycling.
    pub fn arrow_installs_model(&self) -> Option<String> {
        match self.selected_field() {
            Some(Field::LocalLlm) if !self.local_llm_installed => Some(self.model_string()),
            _ => None,
        }
    }

    /// Mark the local LLM as installed and turn it on, so the row becomes an
    /// on/off toggle (now "On") and the change is saved with the rest.
    pub fn mark_local_llm_installed(&mut self) {
        self.local_llm_installed = true;
        self.settings.local_llm.enabled = true;
    }

    fn model_string(&self) -> String {
        self.settings.local_llm.model.clone()
    }

    /// The project-local overrides being edited, if this screen has a project
    /// context.
    pub fn local(&self) -> Option<&LocalSettings> {
        self.local.as_ref().map(|l| &l.settings)
    }

    pub fn workspaces(&self) -> &[String] {
        &self.workspaces
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Number of local override rows (0 when there is no project context).
    fn local_count(&self) -> usize {
        if self.local.is_some() {
            LocalField::ALL.len()
        } else {
            0
        }
    }

    /// Number of selectable field rows (global fields plus any local ones).
    fn field_count(&self) -> usize {
        Field::ALL.len() + self.local_count()
    }

    /// The cursor row index of the Save button: it sits just below the fields.
    pub fn save_index(&self) -> usize {
        self.field_count()
    }

    /// The total number of selectable rows: every field plus the Save button.
    fn row_count(&self) -> usize {
        self.field_count() + 1
    }

    /// Whether the cursor is on the Save button rather than a field.
    pub fn is_save_selected(&self) -> bool {
        self.selected_index == self.save_index()
    }

    /// The global field currently under the cursor, or `None` when a local field
    /// or the Save button is selected.
    pub fn selected_field(&self) -> Option<Field> {
        Field::ALL.get(self.selected_index).copied()
    }

    /// The local override field under the cursor, or `None` otherwise.
    pub fn selected_local_field(&self) -> Option<LocalField> {
        self.local.as_ref()?;
        let base = Field::ALL.len();
        if self.selected_index >= base && self.selected_index < base + LocalField::ALL.len() {
            Some(LocalField::ALL[self.selected_index - base])
        } else {
            None
        }
    }

    /// Whether any field — global or local — differs from its last-saved baseline.
    pub fn is_dirty(&self) -> bool {
        self.settings != self.baseline
            || self
                .local
                .as_ref()
                .is_some_and(|l| l.settings != l.baseline)
    }

    /// Whether a specific global field's value differs from the saved baseline.
    pub fn is_changed(&self, field: Field) -> bool {
        match field {
            Field::Theme => self.settings.theme != self.baseline.theme,
            Field::DefaultWorkspace => {
                self.settings.default_workspace != self.baseline.default_workspace
            }
            Field::Notifications => {
                self.settings.notifications_enabled != self.baseline.notifications_enabled
            }
            Field::AgentCli => self.settings.agent_cli != self.baseline.agent_cli,
            Field::LocalLlm => self.settings.local_llm.enabled != self.baseline.local_llm.enabled,
            Field::LocalLlmModel => self.settings.local_llm.model != self.baseline.local_llm.model,
        }
    }

    /// Whether a specific local override field differs from the saved baseline.
    fn is_local_changed(&self, field: LocalField) -> bool {
        let Some(local) = &self.local else {
            return false;
        };
        match field {
            LocalField::AgentCli => local.settings.agent_cli != local.baseline.agent_cli,
            LocalField::Notifications => {
                local.settings.notifications_enabled != local.baseline.notifications_enabled
            }
            LocalField::DefaultBranch => {
                local.settings.default_branch_source != local.baseline.default_branch_source
            }
        }
    }

    /// Adopt the current edits (global and local) as the saved baseline, clearing
    /// the dirty state. Call this once the settings have been persisted.
    pub fn mark_saved(&mut self) {
        self.baseline = self.settings.clone();
        if let Some(local) = &mut self.local {
            local.baseline = local.settings.clone();
        }
    }

    /// Move the cursor up one row, wrapping to the bottom (the Save button).
    pub fn move_up(&mut self) {
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.row_count() - 1);
    }

    /// Move the cursor down one row, wrapping to the top.
    pub fn move_down(&mut self) {
        self.selected_index = (self.selected_index + 1) % self.row_count();
    }

    /// The display value for a global field, e.g. `"Dark"` or `"(none)"`.
    pub fn value_of(&self, field: Field) -> String {
        match field {
            Field::Theme => theme_label(self.settings.theme).to_string(),
            Field::DefaultWorkspace => self
                .settings
                .default_workspace
                .clone()
                .unwrap_or_else(|| "(none)".to_string()),
            Field::Notifications => on_off(self.settings.notifications_enabled).to_string(),
            Field::AgentCli => agent_cli_label(self.settings.agent_cli).to_string(),
            // Before the runtime/model are present the row is an install action;
            // once installed it becomes a plain on/off toggle.
            Field::LocalLlm => {
                if self.local_llm_installed {
                    on_off(self.settings.local_llm.enabled).to_string()
                } else {
                    "Install".to_string()
                }
            }
            Field::LocalLlmModel => self.settings.local_llm.model.clone(),
        }
    }

    /// The display value for a local override field. When unset it shows the
    /// effective (global) value it falls back to; when set it shows the override.
    pub fn value_of_local(&self, field: LocalField) -> String {
        let Some(local) = &self.local else {
            return String::new();
        };
        match field {
            LocalField::AgentCli => match local.settings.agent_cli {
                None => format!("Global ({})", agent_cli_label(self.settings.agent_cli)),
                Some(cli) => format!("Override: {}", agent_cli_label(cli)),
            },
            LocalField::Notifications => match local.settings.notifications_enabled {
                None => format!("Global ({})", on_off(self.settings.notifications_enabled)),
                Some(on) => format!("Override: {}", on_off(on)),
            },
            // The branch source has no global counterpart: an unset value simply
            // shows the default it resolves to, and a set value shows itself.
            LocalField::DefaultBranch => match local.settings.default_branch_source {
                None => format!("Default ({})", branch_source_label(BranchSource::default())),
                Some(source) => branch_source_label(source).to_string(),
            },
        }
    }

    /// The display rows, in order: each global field, then each local override
    /// field (when present). The Save button is not included.
    pub fn rows(&self) -> Vec<RowView> {
        let mut rows: Vec<RowView> = Field::ALL
            .iter()
            .map(|&field| RowView {
                label: field.label(),
                value: self.value_of(field),
                changed: self.is_changed(field),
            })
            .collect();
        if self.local.is_some() {
            for &field in &LocalField::ALL {
                rows.push(RowView {
                    label: field.label(),
                    value: self.value_of_local(field),
                    changed: self.is_local_changed(field),
                });
            }
        }
        rows
    }

    /// Advance the selected field's value to its next (or previous) choice,
    /// wrapping. The edit is held in memory only — nothing is persisted until
    /// the user saves. Returns `true` when a value actually changed, and `false`
    /// when there was nothing to cycle (the Save button, or a default-workspace
    /// field with no registered workspaces).
    pub fn cycle_selected(&mut self, forward: bool) -> bool {
        if let Some(field) = self.selected_field() {
            return self.cycle_global(field, forward);
        }
        if let Some(field) = self.selected_local_field() {
            return self.cycle_local(field, forward);
        }
        // The cursor is on the Save button: nothing to cycle.
        false
    }

    /// Cycle a global field's value.
    fn cycle_global(&mut self, field: Field, forward: bool) -> bool {
        match field {
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
            Field::LocalLlm => {
                // Only meaningful once installed: flip the on/off toggle. While
                // not installed the row is an install action handled by the
                // event layer, so there is nothing to cycle.
                if self.local_llm_installed {
                    self.settings.local_llm.enabled = !self.settings.local_llm.enabled;
                    true
                } else {
                    false
                }
            }
            Field::LocalLlmModel => {
                self.settings.local_llm.model =
                    cycle_str(&self.settings.local_llm.model, &LOCAL_LLM_MODELS, forward);
                // A different model may not be pulled yet, so it must be
                // (re)installed before use.
                self.local_llm_installed = false;
                true
            }
        }
    }

    /// Cycle a local override field through "follow global" then each concrete
    /// value. Always changes something, so returns `true`.
    fn cycle_local(&mut self, field: LocalField, forward: bool) -> bool {
        let local = self
            .local
            .as_mut()
            .expect("a local field is only selectable with a local context");
        match field {
            LocalField::AgentCli => {
                local.settings.agent_cli =
                    cycle_optional(local.settings.agent_cli, &AGENT_CLIS, forward);
            }
            LocalField::Notifications => {
                local.settings.notifications_enabled = cycle_optional(
                    local.settings.notifications_enabled,
                    &[true, false],
                    forward,
                );
            }
            LocalField::DefaultBranch => {
                // Local-only setting: toggle between the two concrete sources,
                // treating an unset value as the default. It is always stored.
                let current = local.settings.default_branch_source.unwrap_or_default();
                local.settings.default_branch_source =
                    Some(cycle_enum(current, &BRANCH_SOURCES, forward));
            }
        }
        true
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

/// `"On"`/`"Off"` for a boolean notification toggle.
fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "On"
    } else {
        "Off"
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

/// The human-readable label for a branch source.
pub(super) fn branch_source_label(source: BranchSource) -> &'static str {
    match source {
        BranchSource::Local => "Local",
        BranchSource::Remote => "Remote",
    }
}

/// The string one step after `current` in `choices` (or before, when `forward`
/// is false), wrapping at the ends. An unknown `current` (e.g. a model no longer
/// offered) starts from the first choice.
fn cycle_str(current: &str, choices: &[&str], forward: bool) -> String {
    let i = choices.iter().position(|&c| c == current).unwrap_or(0);
    let len = choices.len();
    let next = if forward {
        (i + 1) % len
    } else {
        (i + len - 1) % len
    };
    choices[next].to_string()
}

/// The value one step after `current` in `choices` (or before, when `forward` is
/// false), wrapping at the ends. Used for a fixed, non-optional set of choices.
fn cycle_enum<T: Copy + PartialEq>(current: T, choices: &[T], forward: bool) -> T {
    let i = choices.iter().position(|&c| c == current).unwrap_or(0);
    let len = choices.len();
    let next = if forward {
        (i + 1) % len
    } else {
        (i + len - 1) % len
    };
    choices[next]
}

/// Cycle an optional override through `None` (follow global) then each value in
/// `choices`, wrapping. Forward order is `None → choices[0] → … → None`.
fn cycle_optional<T: Copy + PartialEq>(
    current: Option<T>,
    choices: &[T],
    forward: bool,
) -> Option<T> {
    // Index 0 is `None`; indices 1.. map onto `choices`.
    let len = choices.len() + 1;
    let current_index = match current {
        None => 0,
        Some(value) => choices
            .iter()
            .position(|&c| c == value)
            .map_or(0, |i| i + 1),
    };
    let next = if forward {
        (current_index + 1) % len
    } else {
        (current_index + len - 1) % len
    };
    if next == 0 {
        None
    } else {
        Some(choices[next - 1])
    }
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
        assert_eq!(Field::LocalLlm.label(), "Local LLM");
        assert_eq!(Field::LocalLlmModel.label(), "Local LLM Model");
        assert_eq!(Field::ALL.len(), 6);
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
        // Global-only: six field rows.
        assert_eq!(config.rows().len(), 6);
        assert_eq!(config.save_index(), 6);
    }

    #[test]
    fn move_down_advances_through_fields_then_the_save_button_and_wraps() {
        let mut config = config_with_workspaces(&[]);
        config.move_down();
        assert_eq!(config.selected_field(), Some(Field::DefaultWorkspace));
        config.move_down();
        assert_eq!(config.selected_field(), Some(Field::Notifications));
        config.move_down();
        assert_eq!(config.selected_field(), Some(Field::AgentCli));
        config.move_down();
        assert_eq!(config.selected_field(), Some(Field::LocalLlm));
        config.move_down();
        assert_eq!(config.selected_field(), Some(Field::LocalLlmModel));
        // The Save button sits below the last field.
        config.move_down();
        assert_eq!(config.selected_field(), None);
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
        config.move_up();
        assert_eq!(config.selected_field(), Some(Field::LocalLlmModel));
        config.move_up();
        assert_eq!(config.selected_field(), Some(Field::LocalLlm));
        config.move_up();
        assert_eq!(config.selected_field(), Some(Field::AgentCli));
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
    fn agent_cli_field_cycles_between_claude_and_gemini() {
        let mut config = config_with_workspaces(&[]);
        config.move_down();
        config.move_down();
        config.move_down(); // select Agent CLI
        assert_eq!(config.selected_field(), Some(Field::AgentCli));
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
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].label, "Theme");
        assert_eq!(rows[0].value, "System");
        assert_eq!(rows[3].label, "Agent CLI");
        assert_eq!(rows[3].value, "Claude");
        // The local LLM is off and not yet installed: the row offers "Install".
        assert_eq!(rows[4].label, "Local LLM");
        assert_eq!(rows[4].value, "Install");
        assert_eq!(rows[5].label, "Local LLM Model");
        assert_eq!(rows[5].value, "qwen2.5-coder:7b");
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
        // Not installed yet: the row is an install action, and an arrow press
        // wants to install the current model rather than cycle.
        assert!(!config.local_llm_installed());
        assert_eq!(config.value_of(Field::LocalLlm), "Install");
        assert_eq!(
            config.arrow_installs_model().as_deref(),
            Some("qwen2.5-coder:7b")
        );
        assert_eq!(
            config.enter_installs_model().as_deref(),
            Some("qwen2.5-coder:7b")
        );
        // Cycling does nothing while uninstalled (the event layer installs).
        assert!(!config.cycle_selected(true));

        // Once installed it turns on and becomes an on/off toggle.
        config.mark_local_llm_installed();
        assert!(config.local_llm_installed());
        assert_eq!(config.value_of(Field::LocalLlm), "On");
        assert!(config.settings().local_llm.enabled);
        assert!(config.is_changed(Field::LocalLlm));
        // Now arrows/Enter toggle rather than install.
        assert!(config.arrow_installs_model().is_none());
        assert!(config.enter_installs_model().is_none());
        assert!(config.cycle_selected(true));
        assert_eq!(config.value_of(Field::LocalLlm), "Off");
    }

    #[test]
    fn local_llm_model_row_cycles_and_requires_reinstall() {
        let mut config = config_with_workspaces(&[]);
        config.mark_local_llm_installed(); // pretend the default model is present
        select_local_llm_model(&mut config);
        assert_eq!(config.local_llm_model(), "qwen2.5-coder:7b");
        // Enter on the model row always installs the selected model.
        assert_eq!(
            config.enter_installs_model().as_deref(),
            Some("qwen2.5-coder:7b")
        );
        // Arrows cycle the model and reset the installed flag (the new model may
        // not be pulled), so the Local LLM row reverts to "Install".
        assert!(config.cycle_selected(true));
        assert_eq!(config.local_llm_model(), "qwen2.5-coder:3b");
        assert!(!config.local_llm_installed());
        assert!(config.is_changed(Field::LocalLlmModel));
        // The model row does not install on arrows (only on Enter).
        assert!(config.arrow_installs_model().is_none());
        // Cycling backward wraps to the last model.
        select_local_llm_model(&mut config);
        assert!(config.cycle_selected(false));
        assert_eq!(config.local_llm_model(), "qwen2.5-coder:7b");
        assert!(config.cycle_selected(false));
        assert_eq!(config.local_llm_model(), "qwen2.5:7b");
    }

    #[test]
    fn cycle_str_starts_from_the_first_choice_for_an_unknown_value() {
        // A model no longer offered behaves like index 0, so forward lands on
        // the second choice.
        assert_eq!(
            cycle_str("ghost-model", &LOCAL_LLM_MODELS, true),
            LOCAL_LLM_MODELS[1]
        );
    }

    // --- local overrides ---------------------------------------------------

    fn local_config() -> Config {
        Config::with_local(Settings::default(), Vec::new(), LocalSettings::default())
    }

    #[test]
    fn with_local_appends_local_rows_and_grows_the_layout() {
        let config = local_config();
        assert!(config.local().is_some());
        // Six global fields + three local fields.
        assert_eq!(config.field_count(), 9);
        assert_eq!(config.save_index(), 9);
        let rows = config.rows();
        assert_eq!(rows.len(), 9);
        assert_eq!(rows[6].label, "Local · Agent CLI");
        assert_eq!(rows[7].label, "Local · Notifications");
        assert_eq!(rows[8].label, "Local · Default Branch");
        // Unset overrides display the value they fall back to.
        assert!(rows[6].value.contains("Global"));
        assert!(rows[6].value.contains("Claude"));
        assert!(rows[7].value.contains("Global"));
        assert!(rows[7].value.contains("On"));
        // The branch source has no global counterpart: it shows its default.
        assert!(rows[8].value.contains("Default"));
        assert!(rows[8].value.contains("Remote"));
    }

    #[test]
    fn local_fields_are_selectable_after_the_global_ones() {
        let mut config = local_config();
        for _ in 0..Field::ALL.len() {
            config.move_down();
        }
        // First local field.
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
            Some(LocalField::DefaultBranch)
        );
        config.move_down();
        assert!(config.is_save_selected());
        assert!(config.selected_local_field().is_none());
    }

    #[test]
    fn cycling_a_local_default_branch_override_toggles_local_and_remote() {
        let mut config = local_config();
        for _ in 0..Field::ALL.len() + 2 {
            config.move_down();
        }
        assert_eq!(
            config.selected_local_field(),
            Some(LocalField::DefaultBranch)
        );
        // Unset shows the default it resolves to.
        assert_eq!(
            config.value_of_local(LocalField::DefaultBranch),
            "Default (Remote)"
        );
        // Forward from the default (Remote) wraps to Local, then back to Remote.
        assert!(config.cycle_selected(true));
        assert_eq!(
            config.local().unwrap().default_branch_source,
            Some(BranchSource::Local)
        );
        assert_eq!(config.value_of_local(LocalField::DefaultBranch), "Local");
        assert!(config.cycle_selected(true));
        assert_eq!(
            config.local().unwrap().default_branch_source,
            Some(BranchSource::Remote)
        );
        assert_eq!(config.value_of_local(LocalField::DefaultBranch), "Remote");
        // Backward toggles the other way.
        assert!(config.cycle_selected(false));
        assert_eq!(
            config.local().unwrap().default_branch_source,
            Some(BranchSource::Local)
        );
    }

    #[test]
    fn cycling_a_local_agent_cli_override_walks_global_then_each_value() {
        let mut config = local_config();
        for _ in 0..Field::ALL.len() {
            config.move_down();
        }
        assert_eq!(config.selected_local_field(), Some(LocalField::AgentCli));

        // None (follow global) -> Claude -> Gemini -> None.
        assert!(config.cycle_selected(true));
        assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::Claude));
        assert!(config
            .value_of_local(LocalField::AgentCli)
            .contains("Override"));
        assert!(config.cycle_selected(true));
        assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::Gemini));
        assert!(config.cycle_selected(true));
        assert_eq!(config.local().unwrap().agent_cli, None);
        // Backward from None wraps to the last value.
        assert!(config.cycle_selected(false));
        assert_eq!(config.local().unwrap().agent_cli, Some(AgentCli::Gemini));
    }

    #[test]
    fn cycling_a_local_notifications_override_walks_global_on_off() {
        let mut config = local_config();
        for _ in 0..Field::ALL.len() + 1 {
            config.move_down();
        }
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
    fn editing_a_local_override_marks_the_config_dirty_and_mark_saved_clears_it() {
        let mut config = local_config();
        for _ in 0..Field::ALL.len() {
            config.move_down();
        }
        assert!(!config.is_dirty());
        assert!(config.cycle_selected(true)); // set a local agent override
        assert!(config.is_dirty());
        assert!(config.is_local_changed(LocalField::AgentCli));
        assert!(!config.is_local_changed(LocalField::Notifications));
        // The corresponding row (first local row, after the six global ones) is
        // flagged changed.
        assert!(config.rows()[Field::ALL.len()].changed);

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

        // With a context and an override set, it shows the override value.
        let mut local = local_config();
        for _ in 0..Field::ALL.len() {
            local.move_down();
        }
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

    #[test]
    fn local_field_labels_are_distinct() {
        assert_eq!(LocalField::AgentCli.label(), "Local · Agent CLI");
        assert_eq!(LocalField::Notifications.label(), "Local · Notifications");
        assert_eq!(LocalField::DefaultBranch.label(), "Local · Default Branch");
        assert_eq!(LocalField::ALL.len(), 3);
    }
}
