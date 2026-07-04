//! The config screen's modal overlays and the [`Config`] methods that drive them.
//!
//! The Local LLM rows open one of two modals: [`InstallModal`] collects the sudo
//! password before the `ollama` runtime is provisioned, and [`ModelModal`] lists
//! the offered models so one can be picked (and pulled if needed). The
//! workspace-local action rows open a text editor each: [`SetupCommandsModal`]
//! for the per-session setup commands and [`EnvModal`] for the 1Password-backed
//! environment bindings. All are kept terminal-independent so their entry /
//! navigation flows are unit-testable. The `Config` methods here are the event
//! loop's seam onto those modals — opening, closing, routing keys, and reading
//! the result — split out of the parent [`super`] module to keep the core editor
//! state focused.

use super::{Config, LocalField};
use crate::domain::settings::{SecretEnv, SessionLabelMaster, LOCAL_LLM_MODELS};
use crate::presentation::tui::widgets::{self, text_area::TextArea, text_input::TextInput};
use console::Style;

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

/// The open workspace setup-command editor. Each line is one shell command that
/// will be run in the session root after creating a new session; blank lines are
/// accepted while editing and dropped when applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupCommandsModal {
    area: TextArea,
}

impl SetupCommandsModal {
    fn new(commands: &[String]) -> Self {
        Self {
            area: TextArea::from_text(&commands.join("\n")),
        }
    }

    /// The command lines currently in the editor.
    pub fn lines(&self) -> &[String] {
        self.area.lines()
    }

    /// The caret position as `(row, byte_col)` for rendering.
    pub fn cursor(&self) -> (usize, usize) {
        self.area.cursor()
    }

    fn commands(&self) -> Vec<String> {
        let text = self.area.text();
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// The open workspace-env editor. Each line is one `NAME=op://vault/item/field`
/// binding: the environment variable name, then the 1Password reference to
/// resolve into it at pane launch. Malformed lines (no `=`, an invalid name, or a
/// blank reference) are accepted while editing and dropped when applied, mirroring
/// the filtering [`crate::domain::settings::LocalSettings::env`] applies at read
/// time. Only the reference is stored — never a resolved secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvModal {
    area: TextArea,
}

impl EnvModal {
    fn new(env: &SecretEnv) -> Self {
        Self {
            area: TextArea::from_text(&crate::domain::settings::format_env_bindings(env)),
        }
    }

    /// The binding lines currently in the editor.
    pub fn lines(&self) -> &[String] {
        self.area.lines()
    }

    /// The caret position as `(row, byte_col)` for rendering.
    pub fn cursor(&self) -> (usize, usize) {
        self.area.cursor()
    }

    /// The valid `NAME → reference` bindings currently in the editor (see
    /// [`crate::domain::settings::parse_env_bindings`] for the filtering rule).
    fn bindings(&self) -> SecretEnv {
        crate::domain::settings::parse_env_bindings(&self.area.text())
    }
}

/// The open session-label editor. Each line is one `id | name | color | icon`
/// label definition — the master 切替's `Tab` / digit keys assign from. Malformed
/// lines (a blank id or name) are accepted while editing and dropped when applied,
/// and a duplicate id keeps only its first definition, mirroring the coercion
/// [`crate::domain::settings::parse_session_labels`] applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLabelsModal {
    area: TextArea,
}

impl SessionLabelsModal {
    fn new(master: &SessionLabelMaster) -> Self {
        Self {
            area: TextArea::from_text(&crate::domain::settings::format_session_labels(master)),
        }
    }

    /// The label lines currently in the editor.
    pub fn lines(&self) -> &[String] {
        self.area.lines()
    }

    /// The caret position as `(row, byte_col)` for rendering.
    pub fn cursor(&self) -> (usize, usize) {
        self.area.cursor()
    }

    /// The valid label master currently in the editor (see
    /// [`crate::domain::settings::parse_session_labels`] for the filtering rule).
    fn master(&self) -> SessionLabelMaster {
        crate::domain::settings::parse_session_labels(&self.area.text())
    }
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
            .unwrap_or(LOCAL_LLM_MODELS.len().saturating_sub(1));
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

/// The modal-driving seam onto [`Config`]: opening, closing, key routing, and
/// reading the result of the install and model-selection modals.
impl Config {
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

    /// Open the setup-command editor on the workspace-local Setup Commands row.
    /// A no-op outside the local scope or on any other row.
    pub fn open_setup_modal(&mut self) {
        if !matches!(self.selected_local_field(), Some(LocalField::SetupCommands)) {
            return;
        }
        let commands = self
            .local
            .as_ref()
            .map(|l| l.settings.setup_commands.clone())
            .unwrap_or_default();
        self.setup_modal = Some(SetupCommandsModal::new(&commands));
    }

    /// The open setup-command editor, if any. While present the event loop routes
    /// every key into it.
    pub fn setup_modal(&self) -> Option<&SetupCommandsModal> {
        self.setup_modal.as_ref()
    }

    /// Close the setup-command editor without applying its current buffer.
    pub fn close_setup_modal(&mut self) {
        self.setup_modal = None;
    }

    /// Apply the setup-command editor's non-empty, trimmed lines into the local
    /// settings, then close it. A no-op when no editor is open.
    pub fn apply_setup_modal(&mut self) {
        let Some(modal) = self.setup_modal.take() else {
            return;
        };
        if let Some(local) = &mut self.local {
            local.settings.setup_commands = modal.commands();
        }
    }

    pub fn setup_modal_insert(&mut self, c: char) {
        if let Some(modal) = &mut self.setup_modal {
            modal.area.insert(c);
        }
    }

    pub fn setup_modal_newline(&mut self) {
        if let Some(modal) = &mut self.setup_modal {
            modal.area.newline();
        }
    }

    pub fn setup_modal_backspace(&mut self) {
        if let Some(modal) = &mut self.setup_modal {
            modal.area.backspace();
        }
    }

    pub fn setup_modal_delete_forward(&mut self) {
        if let Some(modal) = &mut self.setup_modal {
            modal.area.delete_forward();
        }
    }

    pub fn setup_modal_cursor_left(&mut self) {
        if let Some(modal) = &mut self.setup_modal {
            modal.area.move_left();
        }
    }

    pub fn setup_modal_cursor_right(&mut self) {
        if let Some(modal) = &mut self.setup_modal {
            modal.area.move_right();
        }
    }

    pub fn setup_modal_cursor_up(&mut self) {
        if let Some(modal) = &mut self.setup_modal {
            modal.area.move_up();
        }
    }

    pub fn setup_modal_cursor_down(&mut self) {
        if let Some(modal) = &mut self.setup_modal {
            modal.area.move_down();
        }
    }

    pub fn setup_modal_cursor_home(&mut self) {
        if let Some(modal) = &mut self.setup_modal {
            modal.area.move_home();
        }
    }

    pub fn setup_modal_cursor_end(&mut self) {
        if let Some(modal) = &mut self.setup_modal {
            modal.area.move_end();
        }
    }

    /// Open the workspace-env editor on the Env Vars row. A no-op outside the
    /// global/local Env Vars rows. In the global scope it edits
    /// [`Settings::env`](crate::domain::settings::Settings::env); in the local
    /// scope it edits [`LocalSettings::env`](crate::domain::settings::LocalSettings::env).
    pub fn open_env_modal(&mut self) {
        if !self.env_row_active() {
            return;
        }
        let env = self
            .local
            .as_ref()
            .map(|local| &local.settings.env)
            .unwrap_or(&self.settings.env);
        self.env_modal = Some(EnvModal::new(env));
    }

    /// The open workspace-env editor, if any. While present the event loop routes
    /// every key into it.
    pub fn env_modal(&self) -> Option<&EnvModal> {
        self.env_modal.as_ref()
    }

    /// Close the workspace-env editor without applying its current buffer.
    pub fn close_env_modal(&mut self) {
        self.env_modal = None;
    }

    /// Apply the env editor's valid bindings into the current scope's settings,
    /// then close it. A no-op when no editor is open.
    pub fn apply_env_modal(&mut self) {
        let Some(modal) = self.env_modal.take() else {
            return;
        };
        if let Some(local) = &mut self.local {
            local.settings.env = modal.bindings();
        } else {
            self.settings.env = modal.bindings();
        }
    }

    pub fn env_modal_insert(&mut self, c: char) {
        if let Some(modal) = &mut self.env_modal {
            modal.area.insert(c);
        }
    }

    pub fn env_modal_newline(&mut self) {
        if let Some(modal) = &mut self.env_modal {
            modal.area.newline();
        }
    }

    pub fn env_modal_backspace(&mut self) {
        if let Some(modal) = &mut self.env_modal {
            modal.area.backspace();
        }
    }

    pub fn env_modal_delete_forward(&mut self) {
        if let Some(modal) = &mut self.env_modal {
            modal.area.delete_forward();
        }
    }

    pub fn env_modal_cursor_left(&mut self) {
        if let Some(modal) = &mut self.env_modal {
            modal.area.move_left();
        }
    }

    pub fn env_modal_cursor_right(&mut self) {
        if let Some(modal) = &mut self.env_modal {
            modal.area.move_right();
        }
    }

    pub fn env_modal_cursor_up(&mut self) {
        if let Some(modal) = &mut self.env_modal {
            modal.area.move_up();
        }
    }

    pub fn env_modal_cursor_down(&mut self) {
        if let Some(modal) = &mut self.env_modal {
            modal.area.move_down();
        }
    }

    pub fn env_modal_cursor_home(&mut self) {
        if let Some(modal) = &mut self.env_modal {
            modal.area.move_home();
        }
    }

    pub fn env_modal_cursor_end(&mut self) {
        if let Some(modal) = &mut self.env_modal {
            modal.area.move_end();
        }
    }

    /// Open the session-label editor on the workspace-local Session Labels row,
    /// seeded from the project's override when set, otherwise from the effective
    /// global master (so editing starts from the labels currently in effect). A
    /// no-op outside the local scope or on any other row.
    pub fn open_session_labels_modal(&mut self) {
        if !self.session_labels_row_active() {
            return;
        }
        let master = self
            .local
            .as_ref()
            .and_then(|local| local.settings.session_labels.as_ref())
            .unwrap_or(&self.settings.session_labels);
        self.session_labels_modal = Some(SessionLabelsModal::new(master));
    }

    /// The open session-label editor, if any. While present the event loop routes
    /// every key into it.
    pub fn session_labels_modal(&self) -> Option<&SessionLabelsModal> {
        self.session_labels_modal.as_ref()
    }

    /// Close the session-label editor without applying its current buffer.
    pub fn close_session_labels_modal(&mut self) {
        self.session_labels_modal = None;
    }

    /// Apply the session-label editor's valid labels into the local override,
    /// then close it. A buffer identical to the effective global master stores
    /// `None` (defer to global) so leaving the seeded labels untouched keeps the
    /// project following the global set; any other buffer — including an empty one
    /// (the feature off for this project) — stores the override. A no-op when no
    /// editor is open or outside the local scope.
    pub fn apply_session_labels_modal(&mut self) {
        let Some(modal) = self.session_labels_modal.take() else {
            return;
        };
        let master = modal.master();
        let override_value = (master != self.settings.session_labels).then_some(master);
        if let Some(local) = &mut self.local {
            local.settings.session_labels = override_value;
        }
    }

    pub fn session_labels_modal_insert(&mut self, c: char) {
        if let Some(modal) = &mut self.session_labels_modal {
            modal.area.insert(c);
        }
    }

    pub fn session_labels_modal_newline(&mut self) {
        if let Some(modal) = &mut self.session_labels_modal {
            modal.area.newline();
        }
    }

    pub fn session_labels_modal_backspace(&mut self) {
        if let Some(modal) = &mut self.session_labels_modal {
            modal.area.backspace();
        }
    }

    pub fn session_labels_modal_delete_forward(&mut self) {
        if let Some(modal) = &mut self.session_labels_modal {
            modal.area.delete_forward();
        }
    }

    pub fn session_labels_modal_cursor_left(&mut self) {
        if let Some(modal) = &mut self.session_labels_modal {
            modal.area.move_left();
        }
    }

    pub fn session_labels_modal_cursor_right(&mut self) {
        if let Some(modal) = &mut self.session_labels_modal {
            modal.area.move_right();
        }
    }

    pub fn session_labels_modal_cursor_up(&mut self) {
        if let Some(modal) = &mut self.session_labels_modal {
            modal.area.move_up();
        }
    }

    pub fn session_labels_modal_cursor_down(&mut self) {
        if let Some(modal) = &mut self.session_labels_modal {
            modal.area.move_down();
        }
    }

    pub fn session_labels_modal_cursor_home(&mut self) {
        if let Some(modal) = &mut self.session_labels_modal {
            modal.area.move_home();
        }
    }

    pub fn session_labels_modal_cursor_end(&mut self) {
        if let Some(modal) = &mut self.session_labels_modal {
            modal.area.move_end();
        }
    }
}
