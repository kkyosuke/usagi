//! Pure, terminal-independent state for the configuration screen.
//!
//! Holds the settings being edited, the registered workspace names the default
//! workspace can cycle through, and the cursor position. Keeping the editing
//! logic free of any terminal IO makes it directly testable.
//!
//! The screen has two scopes (see [`Scope`]). The global scope (CLI / welcome)
//! edits the application-wide [`Settings`]. The local scope (the workspace home
//! screen) edits a single project's **local overrides** — per-project values for
//! the agent CLI, notifications, and default branch that fall back to the global
//! settings when left unset. Only one scope's fields are shown at a time.

use crate::domain::settings::{
    AgentCli, BranchSource, LocalSettings, SessionActionUi, Settings, Theme, LOCAL_LLM_MODELS,
};

/// The themes in the order they cycle through.
const THEMES: [Theme; 3] = [Theme::Light, Theme::Dark, Theme::System];

/// The agent CLIs in the order they cycle through.
pub(super) const AGENT_CLIS: [AgentCli; 2] = [AgentCli::Claude, AgentCli::Gemini];

/// The 在席 (Focus) action UIs in the order they cycle through.
pub(super) const SESSION_ACTION_UIS: [SessionActionUi; 2] =
    [SessionActionUi::Menu, SessionActionUi::Prompt];

/// The branch sources in the order they cycle through.
pub(super) const BRANCH_SOURCES: [BranchSource; 2] = [BranchSource::Local, BranchSource::Remote];

/// An editable global settings field, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Theme,
    DefaultWorkspace,
    Notifications,
    AgentCli,
    /// How 在席 (Focus) mode presents a session's runnable commands.
    SessionActionUi,
    /// The local LLM enable toggle — or an "Install" action when the runtime /
    /// model is not yet present.
    LocalLlm,
    /// Which local LLM model is used (and installed on selection).
    LocalLlmModel,
}

impl Field {
    /// The fields shown on the screen, top to bottom.
    pub const ALL: [Field; 7] = [
        Field::Theme,
        Field::DefaultWorkspace,
        Field::Notifications,
        Field::AgentCli,
        Field::SessionActionUi,
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
            Field::SessionActionUi => "Session Action UI",
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
    /// Which branch new session worktrees are cut from (the detected default, or
    /// a specific branch).
    DefaultBranch,
    /// Whether that branch is taken in its local or remote-tracking form.
    BranchSource,
}

impl LocalField {
    /// The local override fields shown on the screen, top to bottom.
    pub const ALL: [LocalField; 4] = [
        LocalField::AgentCli,
        LocalField::Notifications,
        LocalField::DefaultBranch,
        LocalField::BranchSource,
    ];

    /// The label shown beside the field's value.
    pub fn label(self) -> &'static str {
        match self {
            LocalField::AgentCli => "Agent CLI",
            LocalField::Notifications => "Notifications",
            LocalField::DefaultBranch => "Default Branch",
            LocalField::BranchSource => "Branch Source",
        }
    }
}

/// Which set of settings the screen is editing. Each scope shows only its own
/// fields, so the global and local settings are never edited on the same screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The application-wide [`Settings`] (opened from the CLI or welcome menu).
    Global,
    /// A single project's [`LocalSettings`] overrides (opened from the workspace
    /// home screen). The global settings are still loaded, read-only, to show the
    /// value each unset override falls back to.
    Local,
}

/// One selectable row's display data, used by the renderer regardless of whether
/// the row is a global field or a project-local override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowView {
    pub label: &'static str,
    pub value: String,
    pub changed: bool,
    /// Whether this row is an action button (e.g. the Local LLM "Install"
    /// prompt) rather than a left/right value chooser. Action rows render as a
    /// plain label with no chevrons.
    pub action: bool,
}

/// The open local-LLM install modal: collects the sudo password before the
/// runtime is provisioned in the background. Kept terminal-independent so the
/// password entry and confirmation flow are unit-testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallModal {
    password: String,
}

impl InstallModal {
    /// The sudo password typed so far.
    pub fn password(&self) -> &str {
        &self.password
    }

    /// The password rendered as bullets, one per character, so it is never
    /// shown in the clear.
    pub fn masked(&self) -> String {
        "•".repeat(self.password.chars().count())
    }
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
/// Edits are held in `settings`/`local` and not written anywhere until the user
/// saves. `baseline` is the last-saved snapshot, so comparing the two tells us
/// which fields carry unsaved changes (and whether anything is dirty at all).
/// `scope` decides which set of fields is shown and edited; the other set is
/// either absent (global scope) or kept read-only for fallback display (local
/// scope).
#[derive(Debug, Clone)]
pub struct Config {
    /// The global settings. Editable in the global scope; in the local scope
    /// they are read-only and only used to render each override's fallback value.
    settings: Settings,
    /// The last-saved global settings, used to detect unsaved edits.
    baseline: Settings,
    /// Registered workspace names the default workspace cycles through.
    workspaces: Vec<String>,
    /// The repository's branch names the local Default Branch field cycles
    /// through (after the "auto" choice). Empty outside the local scope, or when
    /// the workspace is not a single git repository.
    branches: Vec<String>,
    /// The project-local overrides being edited, present in the local scope.
    local: Option<LocalEdit>,
    /// Whether the local LLM runtime and the selected model are present. Seeded
    /// when the screen opens; drives whether the Local LLM row shows an
    /// "Install" action or an on/off toggle.
    local_llm_installed: bool,
    /// The open install modal, when the user has triggered provisioning and is
    /// entering the sudo password. While set it captures all keys.
    install_modal: Option<InstallModal>,
    /// Which settings the screen edits.
    scope: Scope,
    selected_index: usize,
}

impl Config {
    /// Builds the editor for the application-wide global settings, with the
    /// cursor at the top.
    ///
    /// `workspaces` are the names the default-workspace field can cycle through.
    /// The supplied settings double as the initial saved baseline, so a freshly
    /// opened screen reports no unsaved changes.
    pub fn new(settings: Settings, workspaces: Vec<String>) -> Self {
        Self {
            baseline: settings.clone(),
            settings,
            workspaces,
            branches: Vec::new(),
            local: None,
            local_llm_installed: false,
            install_modal: None,
            scope: Scope::Global,
            selected_index: 0,
        }
    }

    /// Builds the editor for a single project's local overrides, seeded from
    /// `local`. Only the local override rows are shown; `global` is kept
    /// read-only so each unset override can display the value it falls back to.
    ///
    /// `branches` are the repository's branch names the Default Branch field can
    /// cycle through (after the "auto" choice); pass an empty list when the
    /// workspace is not a single git repository.
    pub fn workspace(global: Settings, local: LocalSettings, branches: Vec<String>) -> Self {
        Self {
            baseline: global.clone(),
            settings: global,
            workspaces: Vec::new(),
            branches,
            local: Some(LocalEdit {
                baseline: local.clone(),
                settings: local,
            }),
            local_llm_installed: false,
            install_modal: None,
            scope: Scope::Local,
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

    /// Whether the focused row is the Local LLM "Install" action — the row that,
    /// when activated (Space/Enter), opens the install modal instead of cycling
    /// a value. True only while the runtime/model is not yet present.
    pub fn local_llm_needs_install(&self) -> bool {
        matches!(self.selected_field(), Some(Field::LocalLlm)) && !self.local_llm_installed
    }

    /// Open the install modal, ready to collect the sudo password. A no-op
    /// unless the focused row is the Local LLM install action.
    pub fn open_install_modal(&mut self) {
        if self.local_llm_needs_install() {
            self.install_modal = Some(InstallModal::default());
        }
    }

    /// The open install modal, if any. While present the event loop routes every
    /// key into it.
    pub fn install_modal(&self) -> Option<&InstallModal> {
        self.install_modal.as_ref()
    }

    /// Close the install modal (cancel, or after provisioning finishes).
    pub fn close_install_modal(&mut self) {
        self.install_modal = None;
    }

    /// Append a typed character to the modal's password. A no-op when no modal
    /// is open.
    pub fn install_modal_push(&mut self, c: char) {
        if let Some(modal) = &mut self.install_modal {
            modal.password.push(c);
        }
    }

    /// Delete the last character of the modal's password (Backspace).
    pub fn install_modal_backspace(&mut self) {
        if let Some(modal) = &mut self.install_modal {
            modal.password.pop();
        }
    }

    /// The sudo password entered in the modal, ready to hand to the installer.
    /// `None` when no modal is open.
    pub fn install_modal_password(&self) -> Option<String> {
        self.install_modal.as_ref().map(|m| m.password.clone())
    }

    /// Mark the local LLM as installed and turn it on, so the row becomes an
    /// on/off toggle (now "On") and the change is saved with the rest.
    pub fn mark_local_llm_installed(&mut self) {
        self.local_llm_installed = true;
        self.settings.local_llm.enabled = true;
    }

    /// Move the cursor onto the Local LLM Model row, so the user can pick which
    /// model to use right after the runtime is installed.
    pub fn focus_model_row(&mut self) {
        if let Some(i) = Field::ALL.iter().position(|f| *f == Field::LocalLlmModel) {
            self.selected_index = i;
        }
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

    /// The screen title for the current scope.
    pub fn title(&self) -> &'static str {
        match self.scope {
            Scope::Global => "Config",
            Scope::Local => "Workspace Config",
        }
    }

    /// The screen subtitle for the current scope.
    pub fn subtitle(&self) -> &'static str {
        match self.scope {
            Scope::Global => "Adjust your global preferences",
            Scope::Local => "Adjust this workspace's settings",
        }
    }

    /// Number of selectable field rows shown for the current scope.
    fn field_count(&self) -> usize {
        match self.scope {
            Scope::Global => Field::ALL.len(),
            Scope::Local => LocalField::ALL.len(),
        }
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

    /// The global field currently under the cursor, or `None` when the screen is
    /// in the local scope or the Save button is selected.
    pub fn selected_field(&self) -> Option<Field> {
        if self.scope != Scope::Global {
            return None;
        }
        Field::ALL.get(self.selected_index).copied()
    }

    /// The local override field under the cursor, or `None` when the screen is in
    /// the global scope or the Save button is selected.
    pub fn selected_local_field(&self) -> Option<LocalField> {
        if self.scope != Scope::Local {
            return None;
        }
        LocalField::ALL.get(self.selected_index).copied()
    }

    /// Whether the scope's settings differ from their last-saved baseline.
    pub fn is_dirty(&self) -> bool {
        match self.scope {
            Scope::Global => self.settings != self.baseline,
            Scope::Local => self
                .local
                .as_ref()
                .is_some_and(|l| l.settings != l.baseline),
        }
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
            Field::SessionActionUi => {
                self.settings.session_action_ui != self.baseline.session_action_ui
            }
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
                local.settings.default_branch != local.baseline.default_branch
            }
            LocalField::BranchSource => {
                local.settings.default_branch_source != local.baseline.default_branch_source
            }
        }
    }

    /// Adopt the current edits as the saved baseline, clearing the dirty state.
    /// Call this once the scope's settings have been persisted.
    pub fn mark_saved(&mut self) {
        match self.scope {
            Scope::Global => self.baseline = self.settings.clone(),
            Scope::Local => {
                if let Some(local) = &mut self.local {
                    local.baseline = local.settings.clone();
                }
            }
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
            Field::SessionActionUi => {
                session_action_ui_label(self.settings.session_action_ui).to_string()
            }
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
            // The default branch has no global counterpart: an unset value uses
            // the repository's detected default ("auto"), a set value names the
            // branch to cut from.
            LocalField::DefaultBranch => match &local.settings.default_branch {
                None => "Default (auto)".to_string(),
                Some(branch) => branch.clone(),
            },
            // The branch source likewise has no global counterpart: an unset
            // value shows the default it resolves to, a set value shows itself.
            LocalField::BranchSource => match local.settings.default_branch_source {
                None => format!("Default ({})", branch_source_label(BranchSource::default())),
                Some(source) => branch_source_label(source).to_string(),
            },
        }
    }

    /// The display rows for the current scope, in display order. The Save button
    /// is not included.
    pub fn rows(&self) -> Vec<RowView> {
        match self.scope {
            Scope::Global => Field::ALL
                .iter()
                .map(|&field| RowView {
                    label: field.label(),
                    value: self.value_of(field),
                    changed: self.is_changed(field),
                    // The Local LLM row is an action button while the runtime is
                    // not yet installed; everything else is a value chooser.
                    action: field == Field::LocalLlm && !self.local_llm_installed,
                })
                .collect(),
            Scope::Local => LocalField::ALL
                .iter()
                .map(|&field| RowView {
                    label: field.label(),
                    value: self.value_of_local(field),
                    changed: self.is_local_changed(field),
                    action: false,
                })
                .collect(),
        }
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
            Field::SessionActionUi => {
                self.settings.session_action_ui = cycle_enum(
                    self.settings.session_action_ui,
                    &SESSION_ACTION_UIS,
                    forward,
                );
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
    /// value. Returns `true` when a value changed, and `false` when there was
    /// nothing to cycle (the Default Branch field with no branches to choose).
    fn cycle_local(&mut self, field: LocalField, forward: bool) -> bool {
        // The Default Branch cycles branch names, so it needs both the branch
        // list and the local edit; it is handled separately to keep the borrows
        // disjoint. Every other local field cycles a fixed set in place.
        match field {
            LocalField::DefaultBranch => self.cycle_default_branch(forward),
            LocalField::AgentCli => {
                let local = self.local_edit_mut();
                local.settings.agent_cli =
                    cycle_optional(local.settings.agent_cli, &AGENT_CLIS, forward);
                true
            }
            LocalField::Notifications => {
                let local = self.local_edit_mut();
                local.settings.notifications_enabled = cycle_optional(
                    local.settings.notifications_enabled,
                    &[true, false],
                    forward,
                );
                true
            }
            LocalField::BranchSource => {
                // Local-only setting: toggle between the two concrete sources,
                // treating an unset value as the default. It is always stored.
                let local = self.local_edit_mut();
                let current = local.settings.default_branch_source.unwrap_or_default();
                local.settings.default_branch_source =
                    Some(cycle_enum(current, &BRANCH_SOURCES, forward));
                true
            }
        }
    }

    /// The local edit being modified. Only called from a local-field cycle,
    /// which is reachable solely when a local context exists.
    fn local_edit_mut(&mut self) -> &mut LocalEdit {
        self.local
            .as_mut()
            .expect("a local field is only selectable with a local context")
    }

    /// Cycle the local Default Branch through "auto" (the detected default) then
    /// each of the repository's branches, wrapping. A no-op (returns `false`)
    /// when no branches are available to choose from.
    fn cycle_default_branch(&mut self, forward: bool) -> bool {
        if self.branches.is_empty() {
            return false;
        }
        let local = self
            .local
            .as_mut()
            .expect("a local field is only selectable with a local context");
        // The choices are "auto" (`None`, index 0) followed by each branch name.
        let len = self.branches.len() + 1;
        let current = match &local.settings.default_branch {
            None => 0,
            // A branch that is no longer present behaves like "auto".
            Some(name) => self
                .branches
                .iter()
                .position(|b| b == name)
                .map_or(0, |i| i + 1),
        };
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        local.settings.default_branch = if next == 0 {
            None
        } else {
            Some(self.branches[next - 1].clone())
        };
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

/// The human-readable label for a 在席 (Focus) action UI style.
pub(super) fn session_action_ui_label(ui: SessionActionUi) -> &'static str {
    match ui {
        SessionActionUi::Menu => "Menu",
        SessionActionUi::Prompt => "Prompt",
    }
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
        assert_eq!(Field::SessionActionUi.label(), "Session Action UI");
        assert_eq!(Field::LocalLlm.label(), "Local LLM");
        assert_eq!(Field::LocalLlmModel.label(), "Local LLM Model");
        assert_eq!(Field::ALL.len(), 7);
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
        // Global-only: seven field rows.
        assert_eq!(config.rows().len(), 7);
        assert_eq!(config.save_index(), 7);
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
        assert_eq!(config.selected_field(), Some(Field::SessionActionUi));
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
        assert_eq!(config.selected_field(), Some(Field::SessionActionUi));
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
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[0].label, "Theme");
        assert_eq!(rows[0].value, "System");
        assert_eq!(rows[3].label, "Agent CLI");
        assert_eq!(rows[3].value, "Claude");
        assert_eq!(rows[4].label, "Session Action UI");
        assert_eq!(rows[4].value, "Menu");
        // The local LLM is off and not yet installed: the row offers "Install".
        assert_eq!(rows[5].label, "Local LLM");
        assert_eq!(rows[5].value, "Install");
        assert_eq!(rows[6].label, "Local LLM Model");
        assert_eq!(rows[6].value, "qwen2.5-coder:7b");
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
        // Not installed yet: the row is an install action that opens the modal,
        // and the rows() flag marks it as an action button.
        assert!(!config.local_llm_installed());
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

        // Once installed it turns on and becomes an on/off toggle.
        config.mark_local_llm_installed();
        assert!(config.local_llm_installed());
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
        assert_eq!(modal.masked(), "");

        // Typing builds the password; it renders only as bullets.
        config.install_modal_push('p');
        config.install_modal_push('w');
        config.install_modal_backspace();
        config.install_modal_push('z');
        assert_eq!(config.install_modal_password().as_deref(), Some("pz"));
        assert_eq!(config.install_modal().unwrap().masked(), "••");

        // Finishing the install closes the modal, marks it installed, and drops
        // the cursor onto the model row so a model can be chosen.
        config.mark_local_llm_installed();
        config.focus_model_row();
        config.close_install_modal();
        assert!(config.install_modal().is_none());
        assert_eq!(config.selected_field(), Some(Field::LocalLlmModel));
        // Edits to a closed modal are no-ops (and yield no password).
        config.install_modal_push('x');
        config.install_modal_backspace();
        assert!(config.install_modal_password().is_none());
    }

    #[test]
    fn local_llm_model_row_cycles_and_requires_reinstall() {
        let mut config = config_with_workspaces(&[]);
        config.mark_local_llm_installed(); // pretend the default model is present
        select_local_llm_model(&mut config);
        assert_eq!(config.local_llm_model(), "qwen2.5-coder:7b");
        // The model row is a chooser, not an install action.
        assert!(!config.local_llm_needs_install());
        // Arrows cycle the model and reset the installed flag (the new model may
        // not be pulled), so the Local LLM row reverts to "Install".
        assert!(config.cycle_selected(true));
        assert_eq!(config.local_llm_model(), "qwen2.5-coder:3b");
        assert!(!config.local_llm_installed());
        assert!(config.is_changed(Field::LocalLlmModel));
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
        // The local scope shows just the four override rows — no global fields.
        assert_eq!(config.field_count(), 4);
        assert_eq!(config.save_index(), 4);
        // The cursor starts on the first local field, not a global one.
        assert_eq!(config.selected_field(), None);
        assert_eq!(config.selected_local_field(), Some(LocalField::AgentCli));
        let rows = config.rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].label, "Agent CLI");
        assert_eq!(rows[1].label, "Notifications");
        assert_eq!(rows[2].label, "Default Branch");
        assert_eq!(rows[3].label, "Branch Source");
        // Unset overrides display the value they fall back to.
        assert!(rows[0].value.contains("Global"));
        assert!(rows[0].value.contains("Claude"));
        assert!(rows[1].value.contains("Global"));
        assert!(rows[1].value.contains("On"));
        // The default branch has no global counterpart: unset means "auto".
        assert!(rows[2].value.contains("Default"));
        assert!(rows[2].value.contains("auto"));
        // The branch source likewise shows its default (Remote).
        assert!(rows[3].value.contains("Default"));
        assert!(rows[3].value.contains("Remote"));
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
            Some(LocalField::DefaultBranch)
        );
        config.move_down();
        assert_eq!(
            config.selected_local_field(),
            Some(LocalField::BranchSource)
        );
        config.move_down();
        assert!(config.is_save_selected());
        assert!(config.selected_local_field().is_none());
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

    #[test]
    fn local_field_labels_are_distinct() {
        assert_eq!(LocalField::AgentCli.label(), "Agent CLI");
        assert_eq!(LocalField::Notifications.label(), "Notifications");
        assert_eq!(LocalField::DefaultBranch.label(), "Default Branch");
        assert_eq!(LocalField::BranchSource.label(), "Branch Source");
        assert_eq!(LocalField::ALL.len(), 4);
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
}
