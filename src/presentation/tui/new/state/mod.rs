//! Pure, terminal-independent state for the New Project screen.
//!
//! The screen offers two ways to start a project, switchable at the top:
//!
//! - **Clone** — mirrors the editor "clone repository" UX: typing the URL
//!   live-updates the suggested directory until the user edits it themselves.
//! - **Existing** — register a directory already on disk; the workspace name is
//!   suggested from the directory's last path segment until edited.
//!
//! Keeping the form logic free of any terminal IO makes it directly testable.

use std::path::PathBuf;

use crate::domain::repository::RepoUrl;
use crate::presentation::tui::widgets::text_input::TextInput;

/// Which kind of project the form is creating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Clone a Git repository into a new directory.
    #[default]
    Clone,
    /// Register a directory that already exists on disk.
    Existing,
}

impl Mode {
    /// The other mode — the screen only has two, so switching is a toggle.
    pub fn other(self) -> Mode {
        match self {
            Mode::Clone => Mode::Existing,
            Mode::Existing => Mode::Clone,
        }
    }

    /// The focusable fields of this mode, in tab order. The mode selector is
    /// always first so Tab can reach it and switch modes.
    fn fields(self) -> &'static [Field] {
        match self {
            Mode::Clone => &[
                Field::Mode,
                Field::Url,
                Field::Location,
                Field::Directory,
                Field::Branch,
            ],
            Mode::Existing => &[Field::Mode, Field::Path, Field::Name],
        }
    }
}

/// A focusable element of the form. Which ones are live depends on the [`Mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The mode selector tab itself.
    Mode,
    // Clone-mode fields.
    Url,
    Location,
    Directory,
    Branch,
    // Existing-mode fields.
    Path,
    Name,
}

/// A validated form ready to be turned into a new project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewProject {
    /// Clone a repository into `<location>/<directory>`.
    Clone(CloneSpec),
    /// Register an existing directory under a workspace name.
    Existing(ExistingSpec),
}

/// A validated request to clone a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneSpec {
    pub url: RepoUrl,
    /// Base directory the project is cloned under.
    pub location: PathBuf,
    /// Final directory name created under `location`.
    pub directory: String,
    /// Branch to check out, or `None` for the repository default.
    pub branch: Option<String>,
}

/// A validated request to register an existing directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingSpec {
    /// The directory to register as a workspace.
    pub path: PathBuf,
    /// Workspace name (suggested from the directory, editable).
    pub name: String,
}

/// Editable state of the New Project screen.
#[derive(Debug, Clone, Default)]
pub struct FormState {
    mode: Mode,
    /// Index into `mode.fields()` of the focused element.
    focus_index: usize,

    // Clone-mode inputs.
    url: TextInput,
    location: TextInput,
    directory: TextInput,
    branch: TextInput,
    /// Once the user edits the directory by hand we stop auto-deriving it.
    directory_dirty: bool,

    // Existing-mode inputs.
    path: TextInput,
    name: TextInput,
    /// Once the user edits the name by hand we stop auto-deriving it.
    name_dirty: bool,
}

impl FormState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn url(&self) -> &str {
        self.url.value()
    }

    pub fn location(&self) -> &str {
        self.location.value()
    }

    pub fn directory(&self) -> &str {
        self.directory.value()
    }

    pub fn branch(&self) -> &str {
        self.branch.value()
    }

    pub fn path(&self) -> &str {
        self.path.value()
    }

    pub fn name(&self) -> &str {
        self.name.value()
    }

    pub fn focus(&self) -> Field {
        self.mode.fields()[self.focus_index]
    }

    /// The caret position (byte offset) within the focused field, so the renderer
    /// can draw the caret where editing happens. The mode selector is not a text
    /// input, so it reports `0`.
    pub fn focus_cursor(&self) -> usize {
        match self.focus() {
            Field::Mode => 0,
            Field::Url => self.url.cursor(),
            Field::Location => self.location.cursor(),
            Field::Directory => self.directory.cursor(),
            Field::Branch => self.branch.cursor(),
            Field::Path => self.path.cursor(),
            Field::Name => self.name.cursor(),
        }
    }

    /// The [`TextInput`] for `field`, or `None` for the mode selector. Editing
    /// and caret-movement methods route through this so they share one
    /// implementation.
    fn field_mut(&mut self, field: Field) -> Option<&mut TextInput> {
        match field {
            Field::Mode => None,
            Field::Url => Some(&mut self.url),
            Field::Location => Some(&mut self.location),
            Field::Directory => Some(&mut self.directory),
            Field::Branch => Some(&mut self.branch),
            Field::Path => Some(&mut self.path),
            Field::Name => Some(&mut self.name),
        }
    }

    /// Pre-fill the location field (the default place to create the project).
    pub fn set_location(&mut self, value: &str) {
        self.location.set_value(value);
    }

    /// Whether the focused field holds a directory path that can be picked from
    /// the directory browser: Location in Clone mode, the path in Existing mode.
    pub fn focus_is_directory(&self) -> bool {
        matches!(self.focus(), Field::Location | Field::Path)
    }

    /// The current value of the focused directory field, used as the browser's
    /// starting point. Empty for any non-directory field.
    pub fn directory_field_value(&self) -> &str {
        match self.focus() {
            Field::Location => self.location.value(),
            Field::Path => self.path.value(),
            _ => "",
        }
    }

    /// Set the focused directory field to a path chosen in the browser,
    /// re-deriving the workspace name for the Existing path. A no-op when the
    /// focused field is not a directory.
    pub fn set_directory_field(&mut self, value: &str) {
        match self.focus() {
            Field::Location => self.location.set_value(value),
            Field::Path => {
                self.path.set_value(value);
                self.sync_name();
            }
            _ => {}
        }
    }

    /// Overview to the other mode, keeping focus on the mode selector so the next
    /// arrow press keeps toggling without first having to move focus.
    pub fn toggle_mode(&mut self) {
        self.mode = self.mode.other();
        self.focus_index = 0;
    }

    /// Insert a character at the caret of the focused field. No-op when the mode
    /// selector is focused (it is not a text input).
    pub fn insert_char(&mut self, c: char) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.insert(c);
        }
        self.after_edit(field);
    }

    /// Delete the character before the caret of the focused field.
    pub fn backspace(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.backspace();
        }
        self.after_edit(field);
    }

    /// Delete the character at the caret of the focused field (the `Del` key).
    pub fn delete_forward(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.delete_forward();
        }
        self.after_edit(field);
    }

    /// Move the caret one character left within the focused field.
    pub fn cursor_left(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.move_left();
        }
    }

    /// Move the caret one character right within the focused field.
    pub fn cursor_right(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.move_right();
        }
    }

    /// Move the caret to the start of the focused field.
    pub fn cursor_home(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.move_home();
        }
    }

    /// Move the caret to the end of the focused field.
    pub fn cursor_end(&mut self) {
        let field = self.focus();
        if let Some(input) = self.field_mut(field) {
            input.move_end();
        }
    }

    /// Re-run a field's auto-derivation after its text changed. The URL re-derives
    /// the directory; the path re-derives the name; the directory and name track
    /// whether they have been hand-edited (non-empty ⇒ dirty), so emptying either
    /// restores auto-derivation — matching how editors restore the suggestion.
    fn after_edit(&mut self, field: Field) {
        match field {
            Field::Url => self.sync_directory(),
            Field::Directory => self.directory_dirty = !self.directory.is_empty(),
            Field::Path => self.sync_name(),
            Field::Name => self.name_dirty = !self.name.is_empty(),
            _ => {}
        }
    }

    pub fn focus_next(&mut self) {
        let len = self.mode.fields().len();
        self.focus_index = (self.focus_index + 1) % len;
    }

    pub fn focus_prev(&mut self) {
        let len = self.mode.fields().len();
        self.focus_index = (self.focus_index + len - 1) % len;
    }

    /// Re-derive the directory from the URL unless the user has edited it.
    fn sync_directory(&mut self) {
        if self.directory_dirty {
            return;
        }
        let derived =
            crate::domain::repository::suggest_directory(self.url.value()).unwrap_or_default();
        self.directory.set_value(derived);
    }

    /// Re-derive the workspace name from the path unless the user has edited it.
    fn sync_name(&mut self) {
        if self.name_dirty {
            return;
        }
        let derived = suggest_name(self.path.value());
        self.name.set_value(derived);
    }
}

mod validate;

/// Suggest a workspace name from a directory path: its final component.
///
/// Tolerant of trailing slashes (`/a/b/` → `b`) and empty input (`""` → `""`).
fn suggest_name(path: &str) -> String {
    std::path::Path::new(path.trim())
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
