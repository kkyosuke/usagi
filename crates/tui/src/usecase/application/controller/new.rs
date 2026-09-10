//! New workspace validation and reducer.

use std::path::PathBuf;

use usagi_core::domain::presentation_text::presentation_text_is_safe;

use super::{AppState, Effect, HomeSnapshot, Notice, PendingToken, SafeMessage};

/// The New form's two backend-backed operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewMode {
    /// Clone a git repository into a new child directory.
    Clone,
    /// Register an existing directory as a workspace.
    Existing,
}

/// TUI-local editable fields for the New form. The reducer deliberately owns
/// strings rather than presentation widgets, keeping backend validation and
/// retry independent from terminal IO.
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

/// The New surface either keeps its form or has attached its freshly created
/// workspace to Home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewRoute {
    Form,
    Home(Box<AppState>),
}

/// New-form reducer input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewEvent {
    /// Submit the current form.
    Submit,
    /// Retry the most recently failed backend request without clearing fields.
    Retry,
    /// Backend completion for a pending clone or registration operation.
    Result {
        token: PendingToken,
        result: Result<HomeSnapshot, Notice>,
    },
}

/// Stateful New flow. A token fences late completions, while the form itself is
/// never replaced on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewState {
    route: NewRoute,
    mode: NewMode,
    form: NewForm,
    pending: Option<PendingToken>,
    failed: Option<NewRequest>,
    error: Option<Notice>,
    progress: Option<SafeMessage>,
    next_token: u64,
}

impl NewState {
    #[must_use]
    pub fn new(mode: NewMode, form: NewForm) -> Self {
        Self {
            route: NewRoute::Form,
            mode,
            form,
            pending: None,
            failed: None,
            error: None,
            progress: None,
            next_token: 1,
        }
    }

    #[must_use]
    pub const fn route(&self) -> &NewRoute {
        &self.route
    }
    #[must_use]
    pub const fn mode(&self) -> NewMode {
        self.mode
    }
    #[must_use]
    pub const fn form(&self) -> &NewForm {
        &self.form
    }
    #[must_use]
    pub const fn pending(&self) -> Option<PendingToken> {
        self.pending
    }
    #[must_use]
    pub fn error(&self) -> Option<&Notice> {
        self.error.as_ref()
    }
    #[must_use]
    pub fn progress(&self) -> Option<&SafeMessage> {
        self.progress.as_ref()
    }

    fn request(&mut self, request: NewRequest) -> Vec<Effect> {
        if self.pending.is_some() {
            return Vec::new();
        }
        let token = PendingToken(self.next_token);
        self.next_token += 1;
        self.pending = Some(token);
        self.failed = None;
        self.error = None;
        match request {
            NewRequest::Clone {
                repository,
                destination,
                branch,
            } => {
                self.progress = Some(SafeMessage::new("Cloning repository…"));
                vec![Effect::CloneProject {
                    repository,
                    destination,
                    branch,
                    token,
                }]
            }
            NewRequest::Existing { path, name } => {
                self.progress = Some(SafeMessage::new("Registering workspace…"));
                vec![Effect::RegisterWorkspace { path, name, token }]
            }
        }
    }
}

/// Reduce one New-form event and return the project/git/registry port request.
#[must_use]
pub fn update_new(state: &mut NewState, event: NewEvent) -> Vec<Effect> {
    match event {
        NewEvent::Submit if matches!(state.route, NewRoute::Form) && state.pending.is_none() => {
            match validate_new_form(state.mode, &state.form) {
                Ok(request) => state.request(request),
                Err(error) => {
                    state.error = Some(Notice::new(error.message()));
                    Vec::new()
                }
            }
        }
        NewEvent::Retry if matches!(state.route, NewRoute::Form) && state.pending.is_none() => {
            state
                .failed
                .clone()
                .map_or_else(Vec::new, |request| state.request(request))
        }
        NewEvent::Result { token, result } if state.pending == Some(token) => {
            state.pending = None;
            state.progress = None;
            match result {
                Ok(snapshot) => {
                    state.route = NewRoute::Home(Box::new(AppState::home(
                        snapshot.workspace,
                        snapshot.sessions,
                    )));
                    state.failed = None;
                    state.error = None;
                }
                Err(error) => {
                    state.failed = validate_new_form(state.mode, &state.form).ok();
                    state.error = Some(error);
                }
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}
