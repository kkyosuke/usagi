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
use crate::presentation::tui::widgets::{self, text_input::TextInput};
use console::Style;

/// The themes in the order they cycle through.
const THEMES: [Theme; 3] = [Theme::Light, Theme::Dark, Theme::System];

/// The agent CLIs in the order they cycle through.
pub(super) const AGENT_CLIS: [AgentCli; 3] = [AgentCli::Claude, AgentCli::Codex, AgentCli::Gemini];

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
    /// prompt, or the model row that opens the picker) rather than a left/right
    /// value chooser. Action rows render as a plain label with no chevrons.
    pub action: bool,
    /// Whether this row is inert — shown but not selectable for change (e.g. the
    /// Local LLM Model row before the runtime is installed). Disabled rows
    /// render dimmed and ignore activation.
    pub disabled: bool,
}

/// The open local-LLM install modal: collects the sudo password before the
/// `ollama` runtime is provisioned in the background. Kept terminal-independent
/// so the password entry and confirmation flow are unit-testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallModal {
    password: TextInput,
}

impl InstallModal {
    /// The sudo password typed so far.
    pub fn password(&self) -> &str {
        self.password.value()
    }

    /// The password rendered as bullets, one per character, with a block caret at
    /// the editing position, so it is never shown in the clear yet ←/→/Home/End
    /// move a visible caret. Each character maps to one bullet, so the caret sits
    /// on the right bullet even for multi-byte input.
    pub fn masked(&self) -> String {
        let before = "•".repeat(self.password.before().chars().count());
        let after = "•".repeat(self.password.after().chars().count());
        widgets::block_caret(&before, &after, &Style::new())
    }
}

/// One model row in the [`ModelModal`]: the model name, whether it is already
/// pulled, and whether the cursor is on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRow {
    pub model: &'static str,
    pub installed: bool,
    pub selected: bool,
}

/// The open model-selection modal: a list of the offered models with their
/// install state, navigated with ↑/↓ and confirmed with Enter. Picking an
/// installed model just adopts it; picking an uninstalled one pulls it first.
/// Kept terminal-independent so the navigation and selection are unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelModal {
    /// Cursor index into [`LOCAL_LLM_MODELS`].
    cursor: usize,
    /// Whether each model in [`LOCAL_LLM_MODELS`] is pulled, parallel by index.
    installed: Vec<bool>,
}

impl ModelModal {
    /// Open the modal with the cursor on `current` (the model in use) and each
    /// row flagged by whether it appears in `installed_models`.
    fn new(current: &str, installed_models: &[String]) -> Self {
        let installed = LOCAL_LLM_MODELS
            .iter()
            .map(|m| installed_models.iter().any(|i| i == m))
            .collect();
        let cursor = LOCAL_LLM_MODELS
            .iter()
            .position(|m| *m == current)
            .unwrap_or(0);
        Self { cursor, installed }
    }

    /// Move the cursor up one model, wrapping to the bottom.
    pub fn move_up(&mut self) {
        self.cursor = self
            .cursor
            .checked_sub(1)
            .unwrap_or(LOCAL_LLM_MODELS.len() - 1);
    }

    /// Move the cursor down one model, wrapping to the top.
    pub fn move_down(&mut self) {
        self.cursor = (self.cursor + 1) % LOCAL_LLM_MODELS.len();
    }

    /// The model under the cursor.
    pub fn selected_model(&self) -> &'static str {
        LOCAL_LLM_MODELS[self.cursor]
    }

    /// Whether the model under the cursor is already pulled.
    pub fn selected_installed(&self) -> bool {
        self.installed[self.cursor]
    }

    /// The rows to render, top to bottom.
    pub fn rows(&self) -> Vec<ModelRow> {
        LOCAL_LLM_MODELS
            .iter()
            .enumerate()
            .map(|(i, model)| ModelRow {
                model,
                installed: self.installed[i],
                selected: i == self.cursor,
            })
            .collect()
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
    /// Whether the `ollama` runtime is installed. Seeded when the screen opens;
    /// drives whether the Local LLM row shows an "Install" action or an on/off
    /// toggle, and whether the model row is selectable.
    ollama_installed: bool,
    /// The offered models already pulled (a subset of [`LOCAL_LLM_MODELS`]),
    /// seeded when the screen opens. Drives the install markers in the model
    /// picker and whether a chosen model needs pulling first.
    installed_models: Vec<String>,
    /// The open runtime-install modal, when the user has triggered provisioning
    /// and is entering the sudo password. While set it captures all keys.
    install_modal: Option<InstallModal>,
    /// The open model-selection modal, when the user is picking which model to
    /// use. While set it captures all keys.
    model_modal: Option<ModelModal>,
    /// The provisioning launched in the background and not yet reflected into the
    /// screen, if any. The install runs off-thread (see the global install task),
    /// so when it finishes this records what to apply: the runtime became present,
    /// or a specific model was pulled.
    pending_install: Option<PendingInstall>,
    /// Which settings the screen edits.
    scope: Scope,
    selected_index: usize,
}

/// What a launched background install will change once it finishes — tracked so
/// the screen can reflect the right state when the global install task reports
/// completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingInstall {
    /// The `ollama` runtime is being installed.
    Runtime,
    /// `model` is being pulled.
    Model(String),
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
            ollama_installed: false,
            installed_models: Vec::new(),
            install_modal: None,
            model_modal: None,
            pending_install: None,
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
            ollama_installed: false,
            installed_models: Vec::new(),
            install_modal: None,
            model_modal: None,
            pending_install: None,
            scope: Scope::Local,
            selected_index: 0,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Record whether the `ollama` runtime is installed. Called when the screen
    /// opens, after probing the system.
    pub fn set_ollama_installed(&mut self, installed: bool) {
        self.ollama_installed = installed;
    }

    /// Whether the `ollama` runtime is installed.
    pub fn ollama_installed(&self) -> bool {
        self.ollama_installed
    }

    /// Record which offered models are already pulled. Called when the screen
    /// opens, after probing the system.
    pub fn set_installed_models(&mut self, models: Vec<String>) {
        self.installed_models = models;
    }

    /// Whether `model` has already been pulled.
    fn model_installed(&self, model: &str) -> bool {
        self.installed_models.iter().any(|m| m == model)
    }

    /// The currently selected local LLM model name.
    pub fn local_llm_model(&self) -> &str {
        &self.settings.local_llm.model
    }

    /// Whether the focused row is the Local LLM "Install" action — the row that,
    /// when activated (Space/Enter), opens the install modal instead of cycling
    /// a value. True only while the `ollama` runtime is not yet present.
    pub fn local_llm_needs_install(&self) -> bool {
        matches!(self.selected_field(), Some(Field::LocalLlm)) && !self.ollama_installed
    }

    /// Whether the focused row is the (active) Local LLM Model row — the row
    /// that, when activated, opens the model picker. True only once the runtime
    /// is installed; before that the model row is inert.
    pub fn model_row_active(&self) -> bool {
        matches!(self.selected_field(), Some(Field::LocalLlmModel)) && self.ollama_installed
    }

    /// Open the runtime-install modal, ready to collect the sudo password. A
    /// no-op unless the focused row is the Local LLM install action.
    pub fn open_install_modal(&mut self) {
        if self.local_llm_needs_install() {
            self.install_modal = Some(InstallModal::default());
        }
    }

    /// Open the model-selection modal on the model in use, with each row flagged
    /// by its install state. A no-op unless the focused row is the active model
    /// row (i.e. the runtime is installed).
    pub fn open_model_modal(&mut self) {
        if self.model_row_active() {
            self.model_modal = Some(ModelModal::new(
                &self.settings.local_llm.model,
                &self.installed_models,
            ));
        }
    }

    /// The open model modal, if any. While present the event loop routes every
    /// key into it.
    pub fn model_modal(&self) -> Option<&ModelModal> {
        self.model_modal.as_ref()
    }

    /// Close the model modal (cancel, or after a selection is made).
    pub fn close_model_modal(&mut self) {
        self.model_modal = None;
    }

    /// Move the model modal's cursor up one row. A no-op when no modal is open.
    pub fn model_modal_up(&mut self) {
        if let Some(modal) = &mut self.model_modal {
            modal.move_up();
        }
    }

    /// Move the model modal's cursor down one row. A no-op when no modal is open.
    pub fn model_modal_down(&mut self) {
        if let Some(modal) = &mut self.model_modal {
            modal.move_down();
        }
    }

    /// The model under the model modal's cursor, or `None` when no modal is open.
    pub fn model_modal_selection(&self) -> Option<&'static str> {
        self.model_modal.as_ref().map(|m| m.selected_model())
    }

    /// Whether the model under the model modal's cursor is already pulled.
    /// `false` when no modal is open.
    pub fn model_modal_selection_installed(&self) -> bool {
        self.model_modal
            .as_ref()
            .is_some_and(|m| m.selected_installed())
    }

    /// Adopt `model` as the one in use (an edit, saved with the rest). Used when
    /// an already-installed model is picked from the modal.
    pub fn select_model(&mut self, model: &str) {
        self.settings.local_llm.model = model.to_string();
    }

    /// Record that `model` was just pulled and adopt it as the one in use. Used
    /// when an uninstalled model is picked and pulled from the modal.
    pub fn mark_model_installed(&mut self, model: &str) {
        if !self.model_installed(model) {
            self.installed_models.push(model.to_string());
        }
        self.select_model(model);
    }

    /// Record what the just-launched background install will change, so its
    /// completion can be reflected into the screen later.
    pub fn set_pending_install(&mut self, pending: PendingInstall) {
        self.pending_install = Some(pending);
    }

    /// Take the pending background install, if any, clearing it so the
    /// completion is reflected exactly once.
    pub fn take_pending_install(&mut self) -> Option<PendingInstall> {
        self.pending_install.take()
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

    /// Insert a typed character at the caret of the modal's password. A no-op
    /// when no modal is open.
    pub fn install_modal_push(&mut self, c: char) {
        if let Some(modal) = &mut self.install_modal {
            modal.password.insert(c);
        }
    }

    /// Delete the character before the caret of the modal's password (Backspace).
    pub fn install_modal_backspace(&mut self) {
        if let Some(modal) = &mut self.install_modal {
            modal.password.backspace();
        }
    }

    /// Delete the character at the caret of the modal's password (the `Del` key).
    pub fn install_modal_delete_forward(&mut self) {
        if let Some(modal) = &mut self.install_modal {
            modal.password.delete_forward();
        }
    }

    /// Move the password caret one character left.
    pub fn install_modal_cursor_left(&mut self) {
        if let Some(modal) = &mut self.install_modal {
            modal.password.move_left();
        }
    }

    /// Move the password caret one character right.
    pub fn install_modal_cursor_right(&mut self) {
        if let Some(modal) = &mut self.install_modal {
            modal.password.move_right();
        }
    }

    /// Move the password caret to the start of the line.
    pub fn install_modal_cursor_home(&mut self) {
        if let Some(modal) = &mut self.install_modal {
            modal.password.move_home();
        }
    }

    /// Move the password caret to the end of the line.
    pub fn install_modal_cursor_end(&mut self) {
        if let Some(modal) = &mut self.install_modal {
            modal.password.move_end();
        }
    }

    /// The sudo password entered in the modal, ready to hand to the installer.
    /// `None` when no modal is open.
    pub fn install_modal_password(&self) -> Option<String> {
        self.install_modal
            .as_ref()
            .map(|m| m.password.value().to_string())
    }

    /// Mark the `ollama` runtime as installed and turn the local LLM on, so the
    /// Local LLM row becomes an on/off toggle (now "On"), the model row becomes
    /// selectable, and the change is saved with the rest.
    pub fn mark_ollama_installed(&mut self) {
        self.ollama_installed = true;
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
            // Before the runtime is present the row is an install action; once
            // installed it becomes a plain on/off toggle.
            Field::LocalLlm => {
                if self.ollama_installed {
                    on_off(self.settings.local_llm.enabled).to_string()
                } else {
                    "Install".to_string()
                }
            }
            // The model row is inert until the runtime is installed; afterwards
            // it shows the model in use (with an install marker) and opens the
            // picker when activated.
            Field::LocalLlmModel => {
                if !self.ollama_installed {
                    "—".to_string()
                } else if self.model_installed(&self.settings.local_llm.model) {
                    self.settings.local_llm.model.clone()
                } else {
                    format!("{} (未導入)", self.settings.local_llm.model)
                }
            }
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
                    // not yet installed; the model row is an action (opening the
                    // picker) once installed. Everything else is a value chooser.
                    action: match field {
                        Field::LocalLlm => !self.ollama_installed,
                        Field::LocalLlmModel => self.ollama_installed,
                        _ => false,
                    },
                    // The model row is inert until the runtime is installed.
                    disabled: field == Field::LocalLlmModel && !self.ollama_installed,
                })
                .collect(),
            Scope::Local => LocalField::ALL
                .iter()
                .map(|&field| RowView {
                    label: field.label(),
                    value: self.value_of_local(field),
                    changed: self.is_local_changed(field),
                    action: false,
                    disabled: false,
                })
                .collect(),
        }
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

/// The human-readable label for an agent CLI.
fn agent_cli_label(cli: AgentCli) -> &'static str {
    match cli {
        AgentCli::Claude => "Claude",
        AgentCli::Codex => "Codex",
        AgentCli::Gemini => "Gemini",
    }
}

/// The human-readable label for a 在席 (Focus) action UI style.
fn session_action_ui_label(ui: SessionActionUi) -> &'static str {
    match ui {
        SessionActionUi::Menu => "Menu",
        SessionActionUi::Prompt => "Prompt",
    }
}

/// The human-readable label for a branch source.
fn branch_source_label(source: BranchSource) -> &'static str {
    match source {
        BranchSource::Local => "Local",
        BranchSource::Remote => "Remote",
    }
}

mod cycling;

#[cfg(test)]
mod tests;
