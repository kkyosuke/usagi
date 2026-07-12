//! Product-neutral agent launch vocabulary.
//!
//! This module describes a selected agent profile and the immutable intent to
//! launch it. It deliberately does not describe a CLI syntax, shell escaping,
//! PTY/IO, secrets, or provisioning. Those are adapter responsibilities.

use std::{collections::BTreeSet, fmt, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::id::{SessionId, WorkspaceId, WorktreeId};

/// Stable, code-defined identity of an agent profile.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentProfileId(String);

impl AgentProfileId {
    /// Creates a canonical profile ID.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchValidationError::InvalidProfileId`] for an empty or
    /// non-canonical identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, LaunchValidationError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if valid {
            Ok(Self(value))
        } else {
            Err(LaunchValidationError::InvalidProfileId)
        }
    }

    /// Returns the stable profile ID.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A closed vocabulary of product-neutral capabilities.
///
/// This is intentionally unrelated to IPC negotiation, terminal
/// authorization, and lifecycle capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCapability {
    Resume,
    InitialPrompt,
    Headless,
    PhaseReporting,
    McpWiring,
}

/// A product-neutral launch interaction mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    Interactive,
    Headless,
}

/// A code-defined static descriptor for an available agent profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: AgentProfileId,
    pub display_name: String,
    pub revision: u32,
    pub capabilities: BTreeSet<AgentCapability>,
    pub allowed_modes: BTreeSet<LaunchMode>,
}

impl AgentProfile {
    /// Constructs static profile metadata without executable or CLI details.
    #[must_use]
    pub fn new(
        id: AgentProfileId,
        display_name: impl Into<String>,
        revision: u32,
        capabilities: impl IntoIterator<Item = AgentCapability>,
        allowed_modes: impl IntoIterator<Item = LaunchMode>,
    ) -> Self {
        Self {
            id,
            display_name: display_name.into(),
            revision,
            capabilities: capabilities.into_iter().collect(),
            allowed_modes: allowed_modes.into_iter().collect(),
        }
    }
}

/// An adapter-opaque, syntactically validated model selector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelSelector(String);

impl ModelSelector {
    /// Creates a selector without imposing an adapter-specific allowlist.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchValidationError::InvalidModelSelector`] when `value` is
    /// empty, too long, or contains control characters.
    pub fn new(value: impl Into<String>) -> Result<Self, LaunchValidationError> {
        let value = value.into();
        if !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control) {
            Ok(Self(value))
        } else {
            Err(LaunchValidationError::InvalidModelSelector)
        }
    }

    /// Returns the opaque selector text for an adapter.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The workspace/session/worktree incarnation to which a launch belongs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchScope {
    pub workspace_id: WorkspaceId,
    pub session_id: SessionId,
    pub worktree_id: WorktreeId,
}

/// Immutable, pre-resolution launch intent. It contains no rendered command,
/// adapter-private configuration, or secret material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRequest {
    pub profile_id: AgentProfileId,
    pub mode: LaunchMode,
    pub model: Option<ModelSelector>,
    pub resume: bool,
    pub initial_prompt: Option<String>,
    pub scope: LaunchScope,
    pub required_capabilities: BTreeSet<AgentCapability>,
}

impl LaunchRequest {
    /// Returns all capabilities implied by this request and explicitly needed
    /// by the caller.
    #[must_use]
    pub fn required_capabilities(&self) -> BTreeSet<AgentCapability> {
        let mut required = self.required_capabilities.clone();
        if self.resume {
            required.insert(AgentCapability::Resume);
        }
        if self.initial_prompt.is_some() {
            required.insert(AgentCapability::InitialPrompt);
        }
        if self.mode == LaunchMode::Headless {
            required.insert(AgentCapability::Headless);
        }
        required
    }
}

/// A public environment-variable name. A plan stores only this allowlist, not
/// values; adapters inject any values after durable resolution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvironmentVariableName(String);

impl EnvironmentVariableName {
    /// Creates a portable environment variable name.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchValidationError::InvalidEnvironmentVariableName`] for
    /// invalid names.
    pub fn new(value: impl Into<String>) -> Result<Self, LaunchValidationError> {
        let value = value.into();
        let mut chars = value.bytes();
        let valid = chars
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            && chars.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if valid {
            Ok(Self(value))
        } else {
            Err(LaunchValidationError::InvalidEnvironmentVariableName)
        }
    }
}

/// A shell-neutral process launch plan, rendered once by an adapter.
///
/// `argv` is an argument vector, never a shell command string. Its values must
/// be public metadata: secret injection happens outside this core contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPlan {
    pub profile_id: AgentProfileId,
    pub profile_revision: u32,
    pub program: String,
    pub argv: Vec<String>,
    pub environment_allowlist: BTreeSet<EnvironmentVariableName>,
    pub working_directory: PathBuf,
}

impl LaunchPlan {
    /// Constructs a shell-neutral, non-secret plan.
    ///
    /// # Errors
    ///
    /// Returns a typed error for empty fields, NULs, or secret-like argument
    /// values. Adapters must pass secret values through provisioning, not here.
    pub fn new(
        profile_id: AgentProfileId,
        profile_revision: u32,
        program: impl Into<String>,
        argv: Vec<String>,
        environment_allowlist: impl IntoIterator<Item = EnvironmentVariableName>,
        working_directory: PathBuf,
    ) -> Result<Self, LaunchValidationError> {
        let program = program.into();
        if program.is_empty() || program.contains('\0') {
            return Err(LaunchValidationError::InvalidProgram);
        }
        if working_directory.as_os_str().is_empty() {
            return Err(LaunchValidationError::InvalidWorkingDirectory);
        }
        if argv.iter().any(|argument| {
            argument.is_empty() || argument.contains('\0') || contains_secret_marker(argument)
        }) {
            return Err(LaunchValidationError::InvalidArgumentVector);
        }
        Ok(Self {
            profile_id,
            profile_revision,
            program,
            argv,
            environment_allowlist: environment_allowlist.into_iter().collect(),
            working_directory,
        })
    }
}

fn contains_secret_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    ["secret=", "token=", "password=", "api_key=", "api-key="]
        .iter()
        .any(|marker| value.contains(marker))
}

/// A durable, serializable resolved launch boundary. It contains immutable
/// intent and a non-secret plan snapshot, never adapter private configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableLaunchSnapshot {
    pub schema_version: u16,
    pub request: LaunchRequest,
    pub plan: LaunchPlan,
}

impl DurableLaunchSnapshot {
    pub const SCHEMA_VERSION: u16 = 1;

    /// Creates the current-version durable snapshot after validation.
    #[must_use]
    pub fn new(request: LaunchRequest, plan: LaunchPlan) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            request,
            plan,
        }
    }
}

/// Typed reasons a request or launch boundary is rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchValidationError {
    InvalidProfileId,
    InvalidModelSelector,
    InvalidEnvironmentVariableName,
    InvalidProgram,
    InvalidArgumentVector,
    InvalidWorkingDirectory,
    EmptyPrompt,
    UnknownProfile { profile_id: AgentProfileId },
    UnsupportedMode { mode: LaunchMode },
    UnsupportedCapability { capability: AgentCapability },
    SnapshotSchemaMismatch { expected: u16, actual: u16 },
    ProfileRevisionMismatch { expected: u32, actual: u32 },
    PlanProvenanceMismatch,
}

impl fmt::Display for LaunchValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProfileId => f.write_str("invalid agent profile ID"),
            Self::InvalidModelSelector => f.write_str("invalid model selector"),
            Self::InvalidEnvironmentVariableName => {
                f.write_str("invalid environment variable name")
            }
            Self::InvalidProgram => f.write_str("invalid launch program"),
            Self::InvalidArgumentVector => f.write_str("invalid or secret launch argument"),
            Self::InvalidWorkingDirectory => f.write_str("invalid launch working directory"),
            Self::EmptyPrompt => f.write_str("initial prompt must not be empty"),
            Self::UnknownProfile { profile_id } => write!(f, "unknown agent profile: {profile_id}"),
            Self::UnsupportedMode { mode } => write!(f, "unsupported launch mode: {mode:?}"),
            Self::UnsupportedCapability { capability } => {
                write!(f, "unsupported agent capability: {capability:?}")
            }
            Self::SnapshotSchemaMismatch { expected, actual } => {
                write!(
                    f,
                    "launch snapshot schema mismatch: expected {expected}, got {actual}"
                )
            }
            Self::ProfileRevisionMismatch { expected, actual } => {
                write!(
                    f,
                    "agent profile revision mismatch: expected {expected}, got {actual}"
                )
            }
            Self::PlanProvenanceMismatch => {
                f.write_str("launch plan does not match request provenance")
            }
        }
    }
}

impl std::error::Error for LaunchValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_reject_invalid_public_values() {
        assert_eq!(
            AgentProfileId::new("Claude"),
            Err(LaunchValidationError::InvalidProfileId)
        );
        assert_eq!(
            AgentProfileId::new(String::from("Claude")),
            Err(LaunchValidationError::InvalidProfileId)
        );
        assert_eq!(
            AgentProfileId::new(String::from("test")).unwrap().as_str(),
            "test"
        );
        assert_eq!(
            ModelSelector::new(""),
            Err(LaunchValidationError::InvalidModelSelector)
        );
        assert_eq!(
            EnvironmentVariableName::new("9BAD"),
            Err(LaunchValidationError::InvalidEnvironmentVariableName)
        );
        assert_eq!(
            EnvironmentVariableName::new("A-B"),
            Err(LaunchValidationError::InvalidEnvironmentVariableName)
        );
        assert_eq!(
            EnvironmentVariableName::new(String::from("A-B")),
            Err(LaunchValidationError::InvalidEnvironmentVariableName)
        );
        assert_eq!(
            EnvironmentVariableName::new(String::from("TEST")),
            Ok(EnvironmentVariableName::new("TEST").unwrap())
        );
        assert_eq!(
            LaunchPlan::new(
                AgentProfileId::new("test").unwrap(),
                1,
                "agent",
                vec!["token=hidden".into()],
                [],
                PathBuf::from("."),
            ),
            Err(LaunchValidationError::InvalidArgumentVector)
        );
        assert!(
            LaunchPlan::new(
                AgentProfileId::new("test").unwrap(),
                1,
                "agent",
                vec![],
                [],
                PathBuf::from("."),
            )
            .is_ok()
        );
    }

    #[test]
    fn request_derives_capabilities_from_its_intent() {
        let request = LaunchRequest {
            profile_id: AgentProfileId::new("test").unwrap(),
            mode: LaunchMode::Headless,
            model: None,
            resume: true,
            initial_prompt: Some("continue".into()),
            scope: LaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: SessionId::new(),
                worktree_id: WorktreeId::new(),
            },
            required_capabilities: [AgentCapability::McpWiring].into_iter().collect(),
        };
        assert_eq!(
            request.required_capabilities(),
            [
                AgentCapability::Resume,
                AgentCapability::InitialPrompt,
                AgentCapability::Headless,
                AgentCapability::McpWiring
            ]
            .into_iter()
            .collect()
        );
        let no_optional_capabilities = LaunchRequest {
            resume: false,
            initial_prompt: None,
            mode: LaunchMode::Interactive,
            ..request
        };
        assert_eq!(no_optional_capabilities.required_capabilities().len(), 1);
    }

    #[test]
    fn public_values_and_all_validation_errors_are_displayable() {
        let profile_id = AgentProfileId::new("test").unwrap();
        assert_eq!(profile_id.as_str(), "test");
        assert_eq!(profile_id.to_string(), "test");
        assert_eq!(
            ModelSelector::new("adapter/model").unwrap().as_str(),
            "adapter/model"
        );
        assert_eq!(
            LaunchPlan::new(profile_id.clone(), 1, "", vec![], [], PathBuf::from("."),),
            Err(LaunchValidationError::InvalidProgram)
        );
        assert_eq!(
            LaunchPlan::new(profile_id.clone(), 1, "agent", vec![], [], PathBuf::new()),
            Err(LaunchValidationError::InvalidWorkingDirectory)
        );
        let errors = [
            LaunchValidationError::InvalidProfileId,
            LaunchValidationError::InvalidModelSelector,
            LaunchValidationError::InvalidEnvironmentVariableName,
            LaunchValidationError::InvalidProgram,
            LaunchValidationError::InvalidArgumentVector,
            LaunchValidationError::InvalidWorkingDirectory,
            LaunchValidationError::EmptyPrompt,
            LaunchValidationError::UnknownProfile { profile_id },
            LaunchValidationError::UnsupportedMode {
                mode: LaunchMode::Interactive,
            },
            LaunchValidationError::UnsupportedCapability {
                capability: AgentCapability::PhaseReporting,
            },
            LaunchValidationError::SnapshotSchemaMismatch {
                expected: 1,
                actual: 2,
            },
            LaunchValidationError::ProfileRevisionMismatch {
                expected: 1,
                actual: 2,
            },
            LaunchValidationError::PlanProvenanceMismatch,
        ];
        assert!(errors.iter().all(|error| !error.to_string().is_empty()));
    }
}
