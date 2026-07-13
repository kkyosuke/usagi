//! Typed v2 resource identities and pure fencing checks.
//!
//! Names, paths, PIDs, and daemon-local counters are attributes, never effecting
//! resource keys.  Every ID is a lowercase, hyphenated UUID on the wire.  A
//! resource recreation receives a fresh ID; callers must retain and compare the
//! complete aggregate reference rather than resolving an old reference by name.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

/// Why an ID string cannot be accepted as a v2 resource ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdParseError {
    /// The value is not a UUID.
    InvalidUuid,
    /// The UUID spelling is valid but not the lowercase canonical wire form.
    NonCanonical,
    /// The UUID has a version other than the one required by this ID type.
    WrongVersion {
        /// The required UUID version number.
        expected: usize,
        /// The parsed UUID version number, if it has one.
        actual: Option<usize>,
    },
}

impl fmt::Display for IdParseError {
    #[coverage(off)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUuid => f.write_str("ID must be a UUID"),
            Self::NonCanonical => f.write_str("ID must be a lowercase canonical UUID"),
            Self::WrongVersion { expected, actual } => {
                write!(f, "ID must be UUIDv{expected}, got {actual:?}")
            }
        }
    }
}

impl std::error::Error for IdParseError {}

#[coverage(off)]
fn parse_uuid(value: &str, expected_version: Option<usize>) -> Result<Uuid, IdParseError> {
    let uuid = Uuid::parse_str(value).map_err(|_| IdParseError::InvalidUuid)?;
    if uuid.hyphenated().to_string() != value {
        return Err(IdParseError::NonCanonical);
    }
    if let Some(expected) = expected_version
        && uuid.get_version_num() != expected
    {
        return Err(IdParseError::WrongVersion {
            expected,
            actual: Some(uuid.get_version_num()),
        });
    }
    Ok(uuid)
}

macro_rules! resource_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        ///
        /// It is an opaque resource incarnation, not a name, path, PID, or
        /// daemon-local counter.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Uuid);

        impl $name {
            /// Issues a never-reused `UUIDv4` resource incarnation.
            #[allow(clippy::new_without_default)]
            #[must_use]
            #[coverage(off)]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Parses one lowercase canonical UUID string.
            ///
            /// # Errors
            ///
            /// Returns [`IdParseError`] when `value` is not canonical UUID text.
            #[coverage(off)]
            pub fn parse(value: &str) -> Result<Self, IdParseError> {
                parse_uuid(value, None).map(Self)
            }

            /// Returns the canonical UUID string used on the wire and in stores.
            #[must_use]
            #[coverage(off)]
            pub fn as_str(&self) -> String {
                self.0.hyphenated().to_string()
            }
        }

        impl fmt::Display for $name {
            #[coverage(off)]
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.as_str())
            }
        }

        impl Serialize for $name {
            #[coverage(off)]
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            #[coverage(off)]
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::parse(&value).map_err(de::Error::custom)
            }
        }
    };
}

resource_id!(
    WorkspaceId,
    "Identity of one registered workspace incarnation."
);
resource_id!(SessionId, "Identity of one session record incarnation.");
resource_id!(WorktreeId, "Identity of one physical checkout incarnation.");
resource_id!(TerminalId, "Identity of one terminal reservation.");
resource_id!(AgentRuntimeId, "Identity of one Agent process runtime.");
resource_id!(ClientId, "Identity of one client process.");
resource_id!(ConnectionId, "Identity of one socket connection.");
resource_id!(RequestId, "Identity of one RPC within a client process.");
resource_id!(
    DaemonGeneration,
    "Identity of one daemon process generation."
);

/// Identity of one durable mutation.  It is `UUIDv7` so a producer may use its
/// timestamp only for admission expiry of a new mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OperationId(Uuid);

impl OperationId {
    /// Issues a `UUIDv7` durable-operation identity.
    #[allow(clippy::new_without_default)]
    #[must_use]
    #[coverage(off)]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Parses a lowercase canonical `UUIDv7` string.
    ///
    /// # Errors
    ///
    /// Returns [`IdParseError`] when `value` is malformed, noncanonical, or not
    /// a `UUIDv7` value.
    #[coverage(off)]
    pub fn parse(value: &str) -> Result<Self, IdParseError> {
        parse_uuid(value, Some(7)).map(Self)
    }

    /// Returns the canonical UUID string used on the wire and in stores.
    #[must_use]
    #[coverage(off)]
    pub fn as_str(&self) -> String {
        self.0.hyphenated().to_string()
    }
}

impl fmt::Display for OperationId {
    #[coverage(off)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl Serialize for OperationId {
    #[coverage(off)]
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for OperationId {
    #[coverage(off)]
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

/// A wire protocol generation and its additive revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProtocolVersion {
    /// Breaking wire-contract generation. Zero is reserved and invalid.
    pub generation: u16,
    /// Additive revision within [`generation`](Self::generation).
    pub revision: u16,
}

impl ProtocolVersion {
    /// Creates a protocol version.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolVersionError`] when `generation` is zero.
    #[coverage(off)]
    pub const fn new(generation: u16, revision: u16) -> Result<Self, ProtocolVersionError> {
        if generation == 0 {
            return Err(ProtocolVersionError::ZeroGeneration);
        }
        Ok(Self {
            generation,
            revision,
        })
    }
}

#[derive(Deserialize)]
struct ProtocolVersionWire {
    generation: u16,
    revision: u16,
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    #[coverage(off)]
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ProtocolVersionWire::deserialize(deserializer)?;
        Self::new(wire.generation, wire.revision).map_err(de::Error::custom)
    }
}

/// A malformed protocol-version value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolVersionError {
    /// Generation zero is reserved so a missing legacy value cannot look valid.
    ZeroGeneration,
}

impl fmt::Display for ProtocolVersionError {
    #[coverage(off)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("protocol generation must be greater than zero")
    }
}

impl std::error::Error for ProtocolVersionError {}

/// A complete terminal ownership scope.  This is the only terminal key accepted
/// by an effecting command.
///
/// ```compile_fail
/// use usagi_core::domain::id::{SessionId, WorkspaceId};
///
/// let _: WorkspaceId = SessionId::new();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRef {
    /// Daemon generation that owns the terminal.
    pub daemon_generation: DaemonGeneration,
    /// Terminal reservation identity.
    pub terminal_id: TerminalId,
    /// Workspace containing the terminal.
    pub workspace_id: WorkspaceId,
    /// Managed session owning the terminal; absent for a workspace-root terminal.
    pub session_id: Option<SessionId>,
    /// Checkout containing the terminal.
    pub worktree_id: WorktreeId,
}

impl TerminalRef {
    /// Returns whether `candidate` is exactly the currently registered terminal.
    /// A mismatch is stale; no name or path fallback is permitted.
    #[must_use]
    #[coverage(off)]
    pub fn fences(&self, candidate: &Self) -> bool {
        self == candidate
    }
}

/// A runtime reference scoped to one Agent pane and its terminal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRuntimeRef {
    /// Runtime reservation identity.
    pub agent_runtime_id: AgentRuntimeId,
    /// The exact terminal which hosts this runtime.
    pub terminal: TerminalRef,
    /// Session owning the Agent runtime.
    pub session_id: SessionId,
}

impl AgentRuntimeRef {
    /// Builds a runtime reference only when the terminal is in `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`ScopeError`] for root terminals or a terminal from another
    /// session.
    #[coverage(off)]
    pub fn new(
        agent_runtime_id: AgentRuntimeId,
        terminal: TerminalRef,
        session_id: SessionId,
    ) -> Result<Self, ScopeError> {
        if terminal.session_id != Some(session_id) {
            return Err(ScopeError::SessionDoesNotOwnTerminal);
        }
        Ok(Self {
            agent_runtime_id,
            terminal,
            session_id,
        })
    }

    /// Returns whether `candidate` is exactly this runtime pane.
    #[must_use]
    #[coverage(off)]
    pub fn fences(&self, candidate: &Self) -> bool {
        self == candidate
    }
}

/// An invalid ownership scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeError {
    /// An Agent runtime must be scoped to the same managed session as its terminal.
    SessionDoesNotOwnTerminal,
}

impl fmt::Display for ScopeError {
    #[coverage(off)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Agent runtime session must own its terminal")
    }
}

impl std::error::Error for ScopeError {}

/// A late-worker completion fence.  Every field must match before a reducer may
/// mutate a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionFence {
    /// Workspace containing the session.
    pub workspace_id: WorkspaceId,
    /// Session record incarnation.
    pub session_id: SessionId,
    /// Durable intent being completed.
    pub operation_id: OperationId,
    /// Daemon generation that accepted the execution.
    pub owner_daemon_generation: DaemonGeneration,
    /// Execution attempt number.
    pub execution_attempt: u64,
    /// Session lifecycle attempt number.
    pub lifecycle_attempt: u64,
    /// State revision expected by the worker.
    pub expected_revision: u64,
}

impl CompletionFence {
    /// Returns whether a late completion may apply to the currently registered
    /// operation state.  Any mismatch must be recorded as a no-op.
    #[must_use]
    #[coverage(off)]
    pub fn fences(&self, candidate: &Self) -> bool {
        self == candidate
    }
}

/// The fail-closed result of decoding a legacy record which lacks a v2 identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyIdentityError {
    /// No typed incarnation was stored.
    Missing,
    /// Multiple possible resources match legacy name/path/PID evidence.
    Ambiguous,
}

impl fmt::Display for LegacyIdentityError {
    #[coverage(off)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("legacy record has no typed identity"),
            Self::Ambiguous => f.write_str("legacy record identity is ambiguous"),
        }
    }
}

impl std::error::Error for LegacyIdentityError {}

/// Accepts a migrated identity only when it is present and unambiguous.
/// Legacy name/path/PID evidence is deliberately not returned as a usable key.
///
/// # Errors
///
/// Returns [`LegacyIdentityError`] so callers retain the record for diagnostics
/// but never treat it as a normal resource.
#[coverage(off)]
pub fn migrate_identity<T>(identity: Option<T>, ambiguous: bool) -> Result<T, LegacyIdentityError> {
    if ambiguous {
        return Err(LegacyIdentityError::Ambiguous);
    }
    identity.ok_or(LegacyIdentityError::Missing)
}

#[cfg(test)]
mod tests;
