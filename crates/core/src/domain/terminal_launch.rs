//! Safe, terminal-only launch vocabulary.
//!
//! This intentionally has no agent profile, command string, secret value, or
//! PTY dependency. A daemon resolves the selected profile from trusted local
//! configuration; clients can only select its stable identity.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::domain::{
    agent::EnvironmentVariableName,
    id::{SessionId, WorkspaceId, WorktreeId},
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TerminalProfileId(String);

impl TerminalProfileId {
    /// Creates a canonical terminal profile ID.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalLaunchValidationError::InvalidProfileId`] for an
    /// empty or non-canonical identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, TerminalLaunchValidationError> {
        let value = value.into();
        (!value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        .then_some(Self(value))
        .ok_or(TerminalLaunchValidationError::InvalidProfileId)
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for TerminalProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The registered resource scope in which the daemon may launch a terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalLaunchScope {
    pub workspace_id: WorkspaceId,
    pub session_id: Option<SessionId>,
    pub worktree_id: WorktreeId,
}

/// Client-visible intent. It deliberately contains no command, argv, cwd, or env.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalLaunchRequest {
    pub profile_id: TerminalProfileId,
    pub scope: TerminalLaunchScope,
}

/// Non-secret, immutable provenance saved before an external PTY spawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableTerminalLaunchSnapshot {
    pub schema_version: u16,
    pub request: TerminalLaunchRequest,
    pub profile_revision: u32,
    pub program: String,
    pub working_directory: PathBuf,
    pub environment_allowlist: BTreeSet<EnvironmentVariableName>,
}
impl DurableTerminalLaunchSnapshot {
    pub const SCHEMA_VERSION: u16 = 1;
    /// Constructs the non-secret durable boundary for a resolved profile.
    ///
    /// # Errors
    ///
    /// Returns a typed error when the profile revision, program, or working
    /// directory cannot safely describe a process launch.
    pub fn new(
        request: TerminalLaunchRequest,
        profile_revision: u32,
        program: impl Into<String>,
        working_directory: PathBuf,
        environment_allowlist: impl IntoIterator<Item = EnvironmentVariableName>,
    ) -> Result<Self, TerminalLaunchValidationError> {
        let program = program.into();
        if profile_revision == 0 {
            return Err(TerminalLaunchValidationError::InvalidProfileRevision);
        }
        if program.is_empty() || program.contains('\0') {
            return Err(TerminalLaunchValidationError::InvalidProgram);
        }
        if working_directory.as_os_str().is_empty() {
            return Err(TerminalLaunchValidationError::InvalidWorkingDirectory);
        }
        Ok(Self {
            schema_version: Self::SCHEMA_VERSION,
            request,
            profile_revision,
            program,
            working_directory,
            environment_allowlist: environment_allowlist.into_iter().collect(),
        })
    }
}

/// Ephemeral resolved values for one spawn. Values are never serializable or durable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTerminalLaunch {
    pub snapshot: DurableTerminalLaunchSnapshot,
    pub environment: BTreeMap<EnvironmentVariableName, String>,
}
impl ResolvedTerminalLaunch {
    /// Pairs the durable snapshot with ephemeral non-secret environment values.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalLaunchValidationError::InvalidEnvironment`] when a
    /// value is not allowlisted or contains a NUL byte.
    pub fn new(
        snapshot: DurableTerminalLaunchSnapshot,
        environment: BTreeMap<EnvironmentVariableName, String>,
    ) -> Result<Self, TerminalLaunchValidationError> {
        if environment
            .keys()
            .any(|name| !snapshot.environment_allowlist.contains(name))
            || environment.values().any(|value| value.contains('\0'))
        {
            return Err(TerminalLaunchValidationError::InvalidEnvironment);
        }
        Ok(Self {
            snapshot,
            environment,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalLaunchValidationError {
    InvalidProfileId,
    InvalidProfileRevision,
    InvalidProgram,
    InvalidWorkingDirectory,
    InvalidEnvironment,
    UnknownProfile { profile_id: TerminalProfileId },
    DisabledProfile { profile_id: TerminalProfileId },
    ProfileRevisionMismatch { expected: u32, actual: u32 },
    SnapshotSchemaMismatch { expected: u16, actual: u16 },
    ScopeMismatch,
}
impl fmt::Display for TerminalLaunchValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "terminal launch validation failed: {self:?}")
    }
}
impl std::error::Error for TerminalLaunchValidationError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::{SessionId, WorkspaceId, WorktreeId};
    fn request() -> TerminalLaunchRequest {
        TerminalLaunchRequest {
            profile_id: TerminalProfileId::new("login-shell").unwrap(),
            scope: TerminalLaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: Some(SessionId::new()),
                worktree_id: WorktreeId::new(),
            },
        }
    }
    #[test]
    fn request_is_selection_only_and_snapshot_redacts_values() {
        let snapshot = DurableTerminalLaunchSnapshot::new(
            request(),
            1,
            "/bin/sh",
            PathBuf::from("."),
            [EnvironmentVariableName::new("TERM").unwrap()],
        )
        .unwrap();
        let resolved = ResolvedTerminalLaunch::new(
            snapshot.clone(),
            BTreeMap::from([(
                EnvironmentVariableName::new("TERM").unwrap(),
                "xterm-256color".into(),
            )]),
        )
        .unwrap();
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("xterm-256color"));
        assert_eq!(resolved.environment.len(), 1);
    }
    #[test]
    fn rejects_untrusted_or_invalid_values() {
        assert!(TerminalProfileId::new("bad value").is_err());
        let snapshot = DurableTerminalLaunchSnapshot::new(request(), 1, "", PathBuf::from("."), [])
            .unwrap_err();
        assert_eq!(snapshot, TerminalLaunchValidationError::InvalidProgram);
        assert_eq!(
            DurableTerminalLaunchSnapshot::new(request(), 0, "sh", PathBuf::from("."), [])
                .unwrap_err(),
            TerminalLaunchValidationError::InvalidProfileRevision
        );
        assert_eq!(
            DurableTerminalLaunchSnapshot::new(request(), 1, "sh", PathBuf::new(), []).unwrap_err(),
            TerminalLaunchValidationError::InvalidWorkingDirectory
        );
        let snapshot =
            DurableTerminalLaunchSnapshot::new(request(), 1, "sh", PathBuf::from("."), []).unwrap();
        assert_eq!(
            ResolvedTerminalLaunch::new(
                snapshot,
                BTreeMap::from([(EnvironmentVariableName::new("TERM").unwrap(), "x".into())])
            )
            .unwrap_err(),
            TerminalLaunchValidationError::InvalidEnvironment
        );
    }
    #[test]
    fn typed_errors_are_displayable() {
        let profile = TerminalProfileId::new("login-shell").unwrap();
        let errors = [
            TerminalLaunchValidationError::InvalidProfileId,
            TerminalLaunchValidationError::InvalidProfileRevision,
            TerminalLaunchValidationError::InvalidProgram,
            TerminalLaunchValidationError::InvalidWorkingDirectory,
            TerminalLaunchValidationError::InvalidEnvironment,
            TerminalLaunchValidationError::UnknownProfile {
                profile_id: profile.clone(),
            },
            TerminalLaunchValidationError::DisabledProfile {
                profile_id: profile,
            },
            TerminalLaunchValidationError::ProfileRevisionMismatch {
                expected: 1,
                actual: 2,
            },
            TerminalLaunchValidationError::SnapshotSchemaMismatch {
                expected: 1,
                actual: 2,
            },
            TerminalLaunchValidationError::ScopeMismatch,
        ];
        assert!(errors.iter().all(|error| {
            error
                .to_string()
                .starts_with("terminal launch validation failed")
        }));
    }
}
