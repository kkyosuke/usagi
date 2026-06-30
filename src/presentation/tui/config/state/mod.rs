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
    AgentCli, BranchSource, KeyScheme, LocalSettings, SessionActionUi, Settings, SkillFeature,
    Theme,
};

mod modal;
pub use modal::{InstallModal, ModelModal, ModelRow, SetupCommandsModal};

/// The themes in the order they cycle through.
const THEMES: [Theme; 3] = [Theme::Light, Theme::Dark, Theme::System];

/// The 在席 (Focus) action UIs in the order they cycle through.
pub(super) const SESSION_ACTION_UIS: [SessionActionUi; 2] =
    [SessionActionUi::Menu, SessionActionUi::Prompt];

/// The 没入 key schemes in the order they cycle through.
pub(super) const KEY_SCHEMES: [KeyScheme; 2] = [KeyScheme::Prefix, KeyScheme::Alt];

/// The branch sources in the order they cycle through.
pub(super) const BRANCH_SOURCES: [BranchSource; 2] = [BranchSource::Local, BranchSource::Remote];

/// An editable global settings field, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Theme,
    DefaultWorkspace,
    Notifications,
    /// Whether each session's open panes are restored on startup.
    RestorePanes,
    AgentCli,
    /// How 在席 (Focus) mode presents a session's runnable commands.
    SessionActionUi,
    /// How the embedded terminal (没入) reserves its navigation keys.
    KeyScheme,
    /// Whether the home-screen sidebar mascot reacts to interaction.
    MascotAnimation,
    /// The local LLM enable toggle — or an "Install" action when the runtime /
    /// model is not yet present.
    LocalLlm,
    /// Which local LLM model is used (and installed on selection).
    LocalLlmModel,
}

impl Field {
    /// The fields shown on the screen, top to bottom.
    pub const ALL: [Field; 10] = [
        Field::Theme,
        Field::DefaultWorkspace,
        Field::Notifications,
        Field::RestorePanes,
        Field::AgentCli,
        Field::SessionActionUi,
        Field::KeyScheme,
        Field::MascotAnimation,
        Field::LocalLlm,
        Field::LocalLlmModel,
    ];

    /// The label shown beside the field's value.
    pub fn label(self) -> &'static str {
        match self {
            Field::Theme => "Theme",
            Field::DefaultWorkspace => "Default Workspace",
            Field::Notifications => "Notifications",
            Field::RestorePanes => "Restore Panes",
            Field::AgentCli => "Agent CLI",
            Field::SessionActionUi => "Session Action UI",
            Field::KeyScheme => "Terminal Keys",
            Field::MascotAnimation => "Mascot Animation",
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
    /// Whether each session's open panes are restored on startup.
    RestorePanes,
    /// Which branch new session worktrees are cut from (the detected default, or
    /// a specific branch).
    DefaultBranch,
    /// Whether that branch is taken in its local or remote-tracking form.
    BranchSource,
    /// Commands run in the session root after a new session is created.
    SetupCommands,
}

impl LocalField {
    /// The local override fields shown on the screen, top to bottom.
    pub const ALL: [LocalField; 6] = [
        LocalField::AgentCli,
        LocalField::Notifications,
        LocalField::RestorePanes,
        LocalField::DefaultBranch,
        LocalField::BranchSource,
        LocalField::SetupCommands,
    ];

    /// The label shown beside the field's value.
    pub fn label(self) -> &'static str {
        match self {
            LocalField::AgentCli => "Agent CLI",
            LocalField::Notifications => "Notifications",
            LocalField::RestorePanes => "Restore Panes",
            LocalField::DefaultBranch => "Default Branch",
            LocalField::BranchSource => "Branch Source",
            LocalField::SetupCommands => "Setup Commands",
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
    /// The agent CLIs offered in the Agent CLI selector — those installed on the
    /// PATH, in [`AgentCli::ALL`] order. Seeded when the screen opens; until then
    /// it defaults to every agent so nothing is hidden before the probe runs. The
    /// selector additionally keeps the currently-configured value selectable even
    /// when it is not installed (see [`Config::agent_cli_choices`]).
    available_agent_clis: Vec<AgentCli>,
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
    /// The open setup-commands editor, when the workspace-local Setup Commands
    /// row is being edited. While set it captures all keys.
    setup_modal: Option<SetupCommandsModal>,
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
            available_agent_clis: AgentCli::ALL.to_vec(),
            ollama_installed: false,
            installed_models: Vec::new(),
            install_modal: None,
            model_modal: None,
            setup_modal: None,
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
            available_agent_clis: AgentCli::ALL.to_vec(),
            ollama_installed: false,
            installed_models: Vec::new(),
            install_modal: None,
            model_modal: None,
            setup_modal: None,
            pending_install: None,
            scope: Scope::Local,
            selected_index: 0,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Record which agent CLIs are installed on the PATH, in [`AgentCli::ALL`]
    /// order. Called when the screen opens, after probing the system, so the
    /// Agent CLI selector only cycles through agents the user can actually launch.
    pub fn set_available_agent_clis(&mut self, available: Vec<AgentCli>) {
        self.available_agent_clis = available;
    }

    /// The concrete agent CLIs the Agent CLI selector cycles through: those
    /// installed on the PATH, plus `keep` (the currently-set value) even when it
    /// is not installed so an existing setting stays visible and selectable.
    /// Always in [`AgentCli::ALL`] order. `keep` is the global value (always
    /// `Some`) or the local override (`None` when following the global setting).
    fn agent_cli_choices(&self, keep: Option<AgentCli>) -> Vec<AgentCli> {
        AgentCli::ALL
            .into_iter()
            .filter(|cli| Some(*cli) == keep || self.available_agent_clis.contains(cli))
            .collect()
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

    /// Whether the focused row is the workspace-local Setup Commands action row.
    pub fn setup_row_active(&self) -> bool {
        matches!(self.selected_local_field(), Some(LocalField::SetupCommands))
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

    /// Number of fixed (non-skill) field rows shown for the current scope, i.e.
    /// the count before the shipped-skill feature rows are appended.
    fn base_field_count(&self) -> usize {
        match self.scope {
            Scope::Global => Field::ALL.len(),
            Scope::Local => LocalField::ALL.len(),
        }
    }

    /// Number of selectable field rows shown for the current scope: the fixed
    /// fields, then one row per toggleable shipped-skill feature.
    fn field_count(&self) -> usize {
        self.base_field_count() + SkillFeature::ALL.len()
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

    /// The shipped-skill feature row under the cursor, or `None` when a fixed
    /// field or the Save button is selected. Skill rows sit just below the fixed
    /// fields in both scopes, so this maps any index past them onto
    /// [`SkillFeature::ALL`].
    pub fn selected_skill_feature(&self) -> Option<SkillFeature> {
        let base = self.base_field_count();
        self.selected_index
            .checked_sub(base)
            .and_then(|i| SkillFeature::ALL.get(i).copied())
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
            Field::RestorePanes => {
                self.settings.restore_panes_enabled != self.baseline.restore_panes_enabled
            }
            Field::AgentCli => self.settings.agent_cli != self.baseline.agent_cli,
            Field::SessionActionUi => {
                self.settings.session_action_ui != self.baseline.session_action_ui
            }
            Field::KeyScheme => self.settings.key_scheme != self.baseline.key_scheme,
            Field::MascotAnimation => {
                self.settings.mascot_animation_enabled != self.baseline.mascot_animation_enabled
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
            LocalField::RestorePanes => {
                local.settings.restore_panes_enabled != local.baseline.restore_panes_enabled
            }
            LocalField::DefaultBranch => {
                local.settings.default_branch != local.baseline.default_branch
            }
            LocalField::BranchSource => {
                local.settings.default_branch_source != local.baseline.default_branch_source
            }
            LocalField::SetupCommands => {
                local.settings.setup_commands != local.baseline.setup_commands
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
            Field::RestorePanes => on_off(self.settings.restore_panes_enabled).to_string(),
            Field::AgentCli => self.settings.agent_cli.display_name().to_string(),
            Field::SessionActionUi => {
                session_action_ui_label(self.settings.session_action_ui).to_string()
            }
            Field::KeyScheme => key_scheme_label(self.settings.key_scheme).to_string(),
            Field::MascotAnimation => on_off(self.settings.mascot_animation_enabled).to_string(),
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
                None => format!("Global ({})", self.settings.agent_cli.display_name()),
                Some(cli) => format!("Override: {}", cli.display_name()),
            },
            LocalField::Notifications => match local.settings.notifications_enabled {
                None => format!("Global ({})", on_off(self.settings.notifications_enabled)),
                Some(on) => format!("Override: {}", on_off(on)),
            },
            LocalField::RestorePanes => match local.settings.restore_panes_enabled {
                None => format!("Global ({})", on_off(self.settings.restore_panes_enabled)),
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
            LocalField::SetupCommands => {
                let count = local.settings.setup_commands().count();
                match count {
                    0 => "Edit (none)".to_string(),
                    1 => "Edit (1 command)".to_string(),
                    n => format!("Edit ({n} commands)"),
                }
            }
        }
    }

    /// The display value for a shipped-skill feature row. In the global scope it
    /// is the plain on/off state; in the local scope it shows the project
    /// override, or the effective global value it falls back to when unset.
    pub fn value_of_skill(&self, feature: SkillFeature) -> String {
        match self.scope {
            Scope::Global => on_off(self.settings.skill_feature_enabled(feature)).to_string(),
            Scope::Local => match self
                .local
                .as_ref()
                .and_then(|l| l.settings.skill_feature_override(feature))
            {
                None => format!(
                    "Global ({})",
                    on_off(self.settings.skill_feature_enabled(feature))
                ),
                Some(on) => format!("Override: {}", on_off(on)),
            },
        }
    }

    /// Whether a shipped-skill feature row differs from the saved baseline. In
    /// the global scope it compares the effective on/off value; in the local
    /// scope it compares the raw override (set vs. unset, on vs. off).
    fn is_skill_changed(&self, feature: SkillFeature) -> bool {
        match self.scope {
            Scope::Global => {
                self.settings.skill_feature_enabled(feature)
                    != self.baseline.skill_feature_enabled(feature)
            }
            Scope::Local => self.local.as_ref().is_some_and(|l| {
                l.settings.skill_feature_override(feature)
                    != l.baseline.skill_feature_override(feature)
            }),
        }
    }

    /// The display rows for the current scope, in display order: the fixed fields
    /// first, then one row per toggleable shipped-skill feature. The Save button
    /// is not included.
    pub fn rows(&self) -> Vec<RowView> {
        let mut rows: Vec<RowView> = match self.scope {
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
                    action: field == LocalField::SetupCommands,
                    disabled: false,
                })
                .collect(),
        };
        // The shipped-skill feature toggles sit below the fixed fields in both
        // scopes; each is a plain on/off (or Global/Override) value chooser.
        for &feature in &SkillFeature::ALL {
            rows.push(RowView {
                label: feature.label(),
                value: self.value_of_skill(feature),
                changed: self.is_skill_changed(feature),
                action: false,
                disabled: false,
            });
        }
        rows
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

/// The human-readable label for a 在席 (Focus) action UI style.
fn session_action_ui_label(ui: SessionActionUi) -> &'static str {
    match ui {
        SessionActionUi::Menu => "Menu",
        SessionActionUi::Prompt => "Prompt",
    }
}

/// The human-readable label for a 没入 key scheme — naming the claimed key so the
/// trade-off (one chord vs. needing Option=Meta) is legible at a glance.
fn key_scheme_label(scheme: KeyScheme) -> &'static str {
    match scheme {
        KeyScheme::Prefix => "Ctrl-O prefix",
        KeyScheme::Alt => "Alt chords",
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
