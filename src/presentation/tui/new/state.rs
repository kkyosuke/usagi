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
    url: String,
    location: String,
    directory: String,
    branch: String,
    /// Once the user edits the directory by hand we stop auto-deriving it.
    directory_dirty: bool,

    // Existing-mode inputs.
    path: String,
    name: String,
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
        &self.url
    }

    pub fn location(&self) -> &str {
        &self.location
    }

    pub fn directory(&self) -> &str {
        &self.directory
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn focus(&self) -> Field {
        self.mode.fields()[self.focus_index]
    }

    /// Pre-fill the location field (the default place to create the project).
    pub fn set_location(&mut self, value: &str) {
        self.location = value.to_string();
    }

    /// Switch to the other mode, keeping focus on the mode selector so the next
    /// arrow press keeps toggling without first having to move focus.
    pub fn toggle_mode(&mut self) {
        self.mode = self.mode.other();
        self.focus_index = 0;
    }

    /// Append a character to the focused field. No-op when the mode selector is
    /// focused (it is not a text input).
    pub fn insert_char(&mut self, c: char) {
        match self.focus() {
            Field::Mode => {}
            Field::Url => {
                self.url.push(c);
                self.sync_directory();
            }
            Field::Location => self.location.push(c),
            Field::Directory => {
                self.directory.push(c);
                self.directory_dirty = true;
            }
            Field::Branch => self.branch.push(c),
            Field::Path => {
                self.path.push(c);
                self.sync_name();
            }
            Field::Name => {
                self.name.push(c);
                self.name_dirty = true;
            }
        }
    }

    /// Delete the last character of the focused field.
    pub fn backspace(&mut self) {
        match self.focus() {
            Field::Mode => {}
            Field::Url => {
                self.url.pop();
                self.sync_directory();
            }
            Field::Location => {
                self.location.pop();
            }
            Field::Directory => {
                self.directory.pop();
                // Emptying the field re-enables auto-derivation so a later URL
                // edit refills it — matching how editors restore the suggestion.
                // We don't refill immediately, so the user can clear it and type
                // a custom name without the suggestion fighting their input.
                if self.directory.is_empty() {
                    self.directory_dirty = false;
                }
            }
            Field::Branch => {
                self.branch.pop();
            }
            Field::Path => {
                self.path.pop();
                self.sync_name();
            }
            Field::Name => {
                self.name.pop();
                // Mirror the directory field: clearing it restores auto-derivation.
                if self.name.is_empty() {
                    self.name_dirty = false;
                }
            }
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
        self.directory =
            crate::domain::repository::suggest_directory(&self.url).unwrap_or_default();
    }

    /// Re-derive the workspace name from the path unless the user has edited it.
    fn sync_name(&mut self) {
        if self.name_dirty {
            return;
        }
        self.name = suggest_name(&self.path);
    }

    /// Validate the form into a [`NewProject`], or return a user-facing error.
    pub fn validate(&self) -> Result<NewProject, String> {
        match self.mode {
            Mode::Clone => self.validate_clone().map(NewProject::Clone),
            Mode::Existing => self.validate_existing().map(NewProject::Existing),
        }
    }

    fn validate_clone(&self) -> Result<CloneSpec, String> {
        let url = RepoUrl::parse(&self.url).map_err(|e| e.to_string())?;
        let directory = self.directory.trim();
        let directory = if directory.is_empty() {
            url.directory_name()
        } else {
            directory.to_string()
        };
        let location = self.location.trim();
        if location.is_empty() {
            return Err("choose where to create the project".to_string());
        }
        let location = PathBuf::from(location);
        let branch = match self.branch.trim() {
            "" => None,
            b => Some(b.to_string()),
        };
        Ok(CloneSpec {
            url,
            location,
            directory,
            branch,
        })
    }

    fn validate_existing(&self) -> Result<ExistingSpec, String> {
        let path = self.path.trim();
        if path.is_empty() {
            return Err("choose an existing directory".to_string());
        }
        let name = match self.name.trim() {
            "" => suggest_name(path),
            n => n.to_string(),
        };
        if name.is_empty() {
            return Err("enter a name for the workspace".to_string());
        }
        Ok(ExistingSpec {
            path: PathBuf::from(path),
            name,
        })
    }
}

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
mod tests {
    use super::*;

    fn type_str(state: &mut FormState, s: &str) {
        for c in s.chars() {
            state.insert_char(c);
        }
    }

    /// Move focus to a specific field from anywhere, via repeated `focus_next`.
    fn focus_to(state: &mut FormState, field: Field) {
        while state.focus() != field {
            state.focus_next();
        }
    }

    #[test]
    fn typing_url_auto_fills_directory() {
        let mut state = FormState::new();
        focus_to(&mut state, Field::Url);
        type_str(&mut state, "https://github.com/owner/repo.git");
        assert_eq!(state.directory(), "repo");
    }

    #[test]
    fn editing_directory_stops_auto_derivation() {
        let mut state = FormState::new();
        focus_to(&mut state, Field::Url);
        type_str(&mut state, "https://github.com/owner/repo");
        assert_eq!(state.directory(), "repo");

        focus_to(&mut state, Field::Directory);
        type_str(&mut state, "-fork");
        assert_eq!(state.directory(), "repo-fork");

        // Further URL edits must not clobber the user's directory.
        focus_to(&mut state, Field::Url);
        type_str(&mut state, "2");
        assert_eq!(state.directory(), "repo-fork");
    }

    #[test]
    fn clearing_directory_restores_auto_derivation() {
        let mut state = FormState::new();
        focus_to(&mut state, Field::Url);
        type_str(&mut state, "https://github.com/owner/repo");
        focus_to(&mut state, Field::Directory);
        for _ in 0.."repo".len() {
            state.backspace();
        }
        // Cleared, and not immediately refilled, so a custom name is possible.
        assert_eq!(state.directory(), "");
        // Back on the URL field, typing should re-derive again.
        focus_to(&mut state, Field::Url);
        type_str(&mut state, "-x");
        assert_eq!(state.directory(), "repo-x");
    }

    #[test]
    fn editing_the_location_field() {
        let mut state = FormState::new();
        state.set_location("/base");
        assert_eq!(state.location(), "/base");

        focus_to(&mut state, Field::Location);
        state.insert_char('x');
        assert_eq!(state.location(), "/basex");
        state.backspace();
        assert_eq!(state.location(), "/base");
    }

    #[test]
    fn focus_cycles_through_clone_fields_including_mode() {
        let mut state = FormState::new();
        assert_eq!(state.focus(), Field::Mode);
        state.focus_next();
        assert_eq!(state.focus(), Field::Url);
        state.focus_next();
        assert_eq!(state.focus(), Field::Location);
        state.focus_next();
        assert_eq!(state.focus(), Field::Directory);
        state.focus_next();
        assert_eq!(state.focus(), Field::Branch);
        state.focus_next();
        // Wraps back to the mode selector.
        assert_eq!(state.focus(), Field::Mode);
        state.focus_prev();
        assert_eq!(state.focus(), Field::Branch);
    }

    #[test]
    fn focus_cycles_through_existing_fields() {
        let mut state = FormState::new();
        state.toggle_mode();
        assert_eq!(state.mode(), Mode::Existing);
        assert_eq!(state.focus(), Field::Mode);
        state.focus_next();
        assert_eq!(state.focus(), Field::Path);
        state.focus_next();
        assert_eq!(state.focus(), Field::Name);
        state.focus_next();
        assert_eq!(state.focus(), Field::Mode);
    }

    #[test]
    fn toggle_mode_switches_between_the_two_modes() {
        let mut state = FormState::new();
        assert_eq!(state.mode(), Mode::Clone);
        state.toggle_mode();
        assert_eq!(state.mode(), Mode::Existing);
        // Focus returns to the mode selector so repeated toggles work.
        assert_eq!(state.focus(), Field::Mode);
        state.toggle_mode();
        assert_eq!(state.mode(), Mode::Clone);
    }

    #[test]
    fn typing_on_the_mode_selector_is_ignored() {
        let mut state = FormState::new();
        assert_eq!(state.focus(), Field::Mode);
        state.insert_char('x');
        state.backspace();
        assert_eq!(state.url(), "");
        assert_eq!(state.location(), "");
    }

    #[test]
    fn typing_path_auto_fills_name() {
        let mut state = FormState::new();
        state.toggle_mode();
        focus_to(&mut state, Field::Path);
        type_str(&mut state, "/home/me/projects/my-app");
        assert_eq!(state.name(), "my-app");
    }

    #[test]
    fn editing_name_stops_auto_derivation_and_clearing_restores_it() {
        let mut state = FormState::new();
        state.toggle_mode();
        focus_to(&mut state, Field::Path);
        type_str(&mut state, "/home/me/app");
        assert_eq!(state.name(), "app");

        focus_to(&mut state, Field::Name);
        type_str(&mut state, "-x");
        assert_eq!(state.name(), "app-x");

        // Path edits no longer clobber the custom name.
        focus_to(&mut state, Field::Path);
        type_str(&mut state, "y");
        assert_eq!(state.name(), "app-x");

        // Clearing the name re-enables derivation.
        focus_to(&mut state, Field::Name);
        for _ in 0.."app-x".len() {
            state.backspace();
        }
        assert_eq!(state.name(), "");
        focus_to(&mut state, Field::Path);
        type_str(&mut state, "z");
        assert_eq!(state.name(), "appyz");
    }

    #[test]
    fn validate_clone_succeeds_with_derived_directory() {
        let mut state = FormState::new();
        state.set_location("/base");
        focus_to(&mut state, Field::Url);
        type_str(&mut state, "git@github.com:owner/repo.git");
        assert!(matches!(
            state.validate().unwrap(),
            NewProject::Clone(spec)
                if spec.url.as_str() == "git@github.com:owner/repo.git"
                    && spec.location == std::path::Path::new("/base")
                    && spec.directory == "repo"
                    && spec.branch.is_none()
        ));
    }

    #[test]
    fn validate_clone_keeps_explicit_branch_and_directory() {
        let mut state = FormState::new();
        state.set_location("/base");
        focus_to(&mut state, Field::Url);
        type_str(&mut state, "https://github.com/owner/repo.git");
        // Clear the auto-filled directory, then type a custom one.
        focus_to(&mut state, Field::Directory);
        for _ in 0.."repo".len() {
            state.backspace();
        }
        type_str(&mut state, "my-dir");
        focus_to(&mut state, Field::Branch);
        type_str(&mut state, "develop");
        assert!(matches!(
            state.validate().unwrap(),
            NewProject::Clone(spec)
                if spec.directory == "my-dir" && spec.branch.as_deref() == Some("develop")
        ));
    }

    #[test]
    fn validate_clone_derives_directory_when_field_is_empty() {
        let mut state = FormState::new();
        state.set_location("/base");
        focus_to(&mut state, Field::Url);
        type_str(&mut state, "https://github.com/owner/repo.git");
        // Clear the auto-filled directory so validate falls back to the URL.
        focus_to(&mut state, Field::Directory);
        for _ in 0.."repo".len() {
            state.backspace();
        }
        assert_eq!(state.directory(), "");
        assert!(matches!(
            state.validate().unwrap(),
            NewProject::Clone(spec) if spec.directory == "repo"
        ));
    }

    #[test]
    fn validate_clone_rejects_empty_url() {
        let state = FormState::new();
        assert!(state.validate().is_err());
    }

    #[test]
    fn validate_clone_rejects_empty_location() {
        let mut state = FormState::new();
        focus_to(&mut state, Field::Url);
        type_str(&mut state, "https://github.com/owner/repo.git");
        // Location left blank: validation fails even with a valid URL.
        let err = state.validate().unwrap_err();
        assert!(err.contains("create"));
    }

    #[test]
    fn validate_existing_succeeds_with_derived_name() {
        let mut state = FormState::new();
        state.toggle_mode();
        focus_to(&mut state, Field::Path);
        type_str(&mut state, "/home/me/my-app");
        assert!(matches!(
            state.validate().unwrap(),
            NewProject::Existing(spec)
                if spec.path == std::path::Path::new("/home/me/my-app") && spec.name == "my-app"
        ));
    }

    #[test]
    fn validate_existing_keeps_explicit_name() {
        let mut state = FormState::new();
        state.toggle_mode();
        focus_to(&mut state, Field::Path);
        type_str(&mut state, "/home/me/my-app");
        focus_to(&mut state, Field::Name);
        for _ in 0.."my-app".len() {
            state.backspace();
        }
        type_str(&mut state, "custom");
        assert!(matches!(
            state.validate().unwrap(),
            NewProject::Existing(spec) if spec.name == "custom"
        ));
    }

    #[test]
    fn validate_existing_rejects_empty_path() {
        let mut state = FormState::new();
        state.toggle_mode();
        let err = state.validate().unwrap_err();
        assert!(err.contains("directory"));
    }

    #[test]
    fn validate_existing_rejects_a_path_with_no_final_segment() {
        let mut state = FormState::new();
        state.toggle_mode();
        focus_to(&mut state, Field::Path);
        // The root has no final segment, so no name can be derived.
        type_str(&mut state, "/");
        let err = state.validate().unwrap_err();
        assert!(err.contains("name"));
    }

    #[test]
    fn backspace_on_url_re_derives_the_directory() {
        let mut state = FormState::new();
        focus_to(&mut state, Field::Url);
        type_str(&mut state, "https://github.com/owner/repos");
        assert_eq!(state.directory(), "repos");
        // Deleting the trailing character re-derives the directory.
        state.backspace();
        assert_eq!(state.url(), "https://github.com/owner/repo");
        assert_eq!(state.directory(), "repo");
    }

    #[test]
    fn backspace_edits_the_branch_field() {
        let mut state = FormState::new();
        focus_to(&mut state, Field::Branch);
        type_str(&mut state, "dev");
        state.backspace();
        assert_eq!(state.branch(), "de");
    }

    #[test]
    fn backspace_on_path_re_derives_the_name() {
        let mut state = FormState::new();
        state.toggle_mode();
        focus_to(&mut state, Field::Path);
        type_str(&mut state, "/home/me/apps");
        assert_eq!(state.name(), "apps");
        // Deleting the trailing character re-derives the name.
        state.backspace();
        assert_eq!(state.path(), "/home/me/app");
        assert_eq!(state.name(), "app");
    }

    #[test]
    fn suggest_name_handles_trailing_slash_and_empty() {
        assert_eq!(suggest_name("/a/b/c"), "c");
        assert_eq!(suggest_name("/a/b/"), "b");
        assert_eq!(suggest_name(""), "");
    }
}
