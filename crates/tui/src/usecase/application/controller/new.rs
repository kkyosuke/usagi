//! Shared validation for the New workspace form and creation request.

use std::path::PathBuf;

use usagi_core::domain::presentation_text::presentation_text_is_safe;

/// The New form's two backend-backed operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewMode {
    /// Clone a git repository into a new child directory.
    Clone,
    /// Register an existing directory as a workspace.
    Existing,
}

/// TUI-local values validated before the screen graph dispatches a workspace
/// creation request. Strings keep validation independent from input widgets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewForm {
    pub repository: String,
    pub location: String,
    pub directory: String,
    pub branch: String,
    pub path: String,
    pub name: String,
}

/// A validated request retained across a failed operation so retry never loses
/// the user's form values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewRequest {
    Clone {
        repository: String,
        destination: PathBuf,
        branch: Option<String>,
    },
    Existing {
        path: PathBuf,
        name: String,
    },
}

/// Validation errors that are safe to render in the New form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewValidationError {
    RepositoryRequired,
    RepositoryInvalid,
    LocationRequired,
    LocationInvalid,
    DirectoryRequired,
    DirectoryInvalid,
    BranchInvalid,
    PathRequired,
    PathInvalid,
    NameRequired,
    NameInvalid,
}

impl NewValidationError {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::RepositoryRequired => "repository URL is required",
            Self::RepositoryInvalid => "repository URL must be a single safe line",
            Self::LocationRequired => "clone location is required",
            Self::LocationInvalid => "clone location must be a single safe line",
            Self::DirectoryRequired => "directory name is required",
            Self::DirectoryInvalid => "directory name must be a safe name without path separators",
            Self::BranchInvalid => "branch name must be a single safe line",
            Self::PathRequired => "directory path is required",
            Self::PathInvalid => "directory path must be a single safe line",
            Self::NameRequired => "workspace name is required",
            Self::NameInvalid => "workspace name must be a single safe line",
        }
    }
}

/// Build a backend request from a form after trimming optional whitespace.
///
/// # Errors
///
/// Returns a safe field-specific validation error when a required value is
/// empty after trimming.
pub fn validate_new_form(mode: NewMode, form: &NewForm) -> Result<NewRequest, NewValidationError> {
    match mode {
        NewMode::Clone => {
            let repository = required(&form.repository, NewValidationError::RepositoryRequired)?;
            validate_presentation_field(&repository, NewValidationError::RepositoryInvalid)?;
            let location = required(&form.location, NewValidationError::LocationRequired)?;
            validate_presentation_field(&location, NewValidationError::LocationInvalid)?;
            let directory = required(&form.directory, NewValidationError::DirectoryRequired)?;
            if is_invalid_directory_name(&directory) {
                return Err(NewValidationError::DirectoryInvalid);
            }
            let branch = trimmed(&form.branch);
            if let Some(branch) = &branch {
                validate_presentation_field(branch, NewValidationError::BranchInvalid)?;
            }
            Ok(NewRequest::Clone {
                repository,
                destination: PathBuf::from(location).join(directory),
                branch,
            })
        }
        NewMode::Existing => {
            let path = required(&form.path, NewValidationError::PathRequired)?;
            validate_presentation_field(&path, NewValidationError::PathInvalid)?;
            let name = required(&form.name, NewValidationError::NameRequired)?;
            validate_presentation_field(&name, NewValidationError::NameInvalid)?;
            Ok(NewRequest::Existing {
                path: PathBuf::from(path),
                name,
            })
        }
    }
}

fn required(value: &str, error: NewValidationError) -> Result<String, NewValidationError> {
    trimmed(value).ok_or(error)
}

fn validate_presentation_field(
    value: &str,
    error: NewValidationError,
) -> Result<(), NewValidationError> {
    presentation_text_is_safe(value).then_some(()).ok_or(error)
}

/// A clone destination is created as a single child directory under the chosen
/// location, so its name must not traverse (`.`/`..`) or contain a path
/// separator. Rejecting these before submit keeps `location.join(directory)`
/// from escaping the location.
fn is_invalid_directory_name(directory: &str) -> bool {
    directory == "."
        || directory == ".."
        || directory.contains(['/', '\\'])
        || !presentation_text_is_safe(directory)
}

fn trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}
