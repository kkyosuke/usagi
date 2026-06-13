//! Pure, terminal-independent state for the New Project screen.
//!
//! Keeping the form logic free of any terminal IO makes it directly testable
//! and mirrors the editor "clone repository" UX: typing the URL live-updates
//! the suggested directory until the user edits the directory themselves.

use crate::domain::repository::RepoUrl;

/// The fields of the form, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Url,
    Directory,
    Branch,
}

impl Field {
    const ORDER: [Field; 3] = [Field::Url, Field::Directory, Field::Branch];

    fn index(self) -> usize {
        Self::ORDER.iter().position(|f| *f == self).unwrap()
    }

    fn next(self) -> Field {
        Self::ORDER[(self.index() + 1) % Self::ORDER.len()]
    }

    fn prev(self) -> Field {
        Self::ORDER[(self.index() + Self::ORDER.len() - 1) % Self::ORDER.len()]
    }
}

/// A validated form ready to be turned into a new project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProject {
    pub url: RepoUrl,
    pub directory: String,
    /// Branch to check out, or `None` for the repository default.
    pub branch: Option<String>,
}

/// Editable state of the New Project screen.
#[derive(Debug, Clone, Default)]
pub struct FormState {
    url: String,
    directory: String,
    branch: String,
    focus_field: FocusField,
    /// Once the user edits the directory by hand we stop auto-deriving it.
    directory_dirty: bool,
}

// `Field` does not implement Default, so keep an internal default-able mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FocusField {
    #[default]
    Url,
    Directory,
    Branch,
}

impl From<FocusField> for Field {
    fn from(f: FocusField) -> Self {
        match f {
            FocusField::Url => Field::Url,
            FocusField::Directory => Field::Directory,
            FocusField::Branch => Field::Branch,
        }
    }
}

impl From<Field> for FocusField {
    fn from(f: Field) -> Self {
        match f {
            Field::Url => FocusField::Url,
            Field::Directory => FocusField::Directory,
            Field::Branch => FocusField::Branch,
        }
    }
}

impl FormState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn directory(&self) -> &str {
        &self.directory
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    pub fn focus(&self) -> Field {
        self.focus_field.into()
    }

    /// Append a character to the focused field.
    pub fn insert_char(&mut self, c: char) {
        match self.focus_field {
            FocusField::Url => {
                self.url.push(c);
                self.sync_directory();
            }
            FocusField::Directory => {
                self.directory.push(c);
                self.directory_dirty = true;
            }
            FocusField::Branch => self.branch.push(c),
        }
    }

    /// Delete the last character of the focused field.
    pub fn backspace(&mut self) {
        match self.focus_field {
            FocusField::Url => {
                self.url.pop();
                self.sync_directory();
            }
            FocusField::Directory => {
                self.directory.pop();
                // Emptying the field re-enables auto-derivation so a later URL
                // edit refills it — matching how editors restore the suggestion.
                // We don't refill immediately, so the user can clear it and type
                // a custom name without the suggestion fighting their input.
                if self.directory.is_empty() {
                    self.directory_dirty = false;
                }
            }
            FocusField::Branch => {
                self.branch.pop();
            }
        }
    }

    pub fn focus_next(&mut self) {
        self.focus_field = Field::from(self.focus_field).next().into();
    }

    pub fn focus_prev(&mut self) {
        self.focus_field = Field::from(self.focus_field).prev().into();
    }

    /// Re-derive the directory from the URL unless the user has edited it.
    fn sync_directory(&mut self) {
        if self.directory_dirty {
            return;
        }
        self.directory =
            crate::domain::repository::suggest_directory(&self.url).unwrap_or_default();
    }

    /// Validate the form into a [`NewProject`], or return a user-facing error.
    pub fn validate(&self) -> Result<NewProject, String> {
        let url = RepoUrl::parse(&self.url).map_err(|e| e.to_string())?;
        let directory = self.directory.trim();
        let directory = if directory.is_empty() {
            url.directory_name()
        } else {
            directory.to_string()
        };
        let branch = match self.branch.trim() {
            "" => None,
            b => Some(b.to_string()),
        };
        Ok(NewProject {
            url,
            directory,
            branch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_str(state: &mut FormState, s: &str) {
        for c in s.chars() {
            state.insert_char(c);
        }
    }

    #[test]
    fn typing_url_auto_fills_directory() {
        let mut state = FormState::new();
        type_str(&mut state, "https://github.com/owner/repo.git");
        assert_eq!(state.directory(), "repo");
    }

    #[test]
    fn editing_directory_stops_auto_derivation() {
        let mut state = FormState::new();
        type_str(&mut state, "https://github.com/owner/repo");
        assert_eq!(state.directory(), "repo");

        state.focus_next(); // move to directory
        assert_eq!(state.focus(), Field::Directory);
        type_str(&mut state, "-fork");
        assert_eq!(state.directory(), "repo-fork");

        // Further URL edits must not clobber the user's directory.
        state.focus_prev();
        type_str(&mut state, "2");
        assert_eq!(state.directory(), "repo-fork");
    }

    #[test]
    fn clearing_directory_restores_auto_derivation() {
        let mut state = FormState::new();
        type_str(&mut state, "https://github.com/owner/repo");
        state.focus_next();
        for _ in 0.."repo".len() {
            state.backspace();
        }
        // Cleared, and not immediately refilled, so a custom name is possible.
        assert_eq!(state.directory(), "");
        // Back on the URL field, typing should re-derive again.
        state.focus_prev();
        type_str(&mut state, "-x");
        assert_eq!(state.directory(), "repo-x");
    }

    #[test]
    fn focus_cycles_through_fields() {
        let mut state = FormState::new();
        assert_eq!(state.focus(), Field::Url);
        state.focus_next();
        assert_eq!(state.focus(), Field::Directory);
        state.focus_next();
        assert_eq!(state.focus(), Field::Branch);
        state.focus_next();
        assert_eq!(state.focus(), Field::Url);
        state.focus_prev();
        assert_eq!(state.focus(), Field::Branch);
    }

    #[test]
    fn validate_succeeds_with_derived_directory() {
        let mut state = FormState::new();
        type_str(&mut state, "git@github.com:owner/repo.git");
        let project = state.validate().unwrap();
        assert_eq!(project.url.as_str(), "git@github.com:owner/repo.git");
        assert_eq!(project.directory, "repo");
        assert_eq!(project.branch, None);
    }

    #[test]
    fn validate_keeps_explicit_branch_and_directory() {
        let mut state = FormState::new();
        type_str(&mut state, "https://github.com/owner/repo.git");
        // Clear the auto-filled directory, then type a custom one.
        state.focus_next();
        for _ in 0.."repo".len() {
            state.backspace();
        }
        type_str(&mut state, "my-dir");
        state.focus_next();
        type_str(&mut state, "develop");
        let project = state.validate().unwrap();
        assert_eq!(project.directory, "my-dir");
        assert_eq!(project.branch.as_deref(), Some("develop"));
    }

    #[test]
    fn validate_rejects_empty_url() {
        let state = FormState::new();
        assert!(state.validate().is_err());
    }
}
