//! Surface-neutral daemon client port.
//!
//! Presentation surfaces submit only typed request bodies through this port.  In
//! particular, a connection failure is not permission to mutate local session
//! state or to allocate a local managed PTY.

use std::fmt;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::agent::{
    AgentProfileId, AgentResumeTarget, CallerRef, ModelSelector, ProviderSessionId,
};
use crate::domain::daemon::{DaemonProcessObservation, DaemonRecord};
use crate::domain::id::{AgentId, SessionId, TerminalRef, WorkspaceId};
use crate::domain::pr_inventory::{PrEntry, PrInventory};
use crate::domain::terminal_launch::{
    TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};
use crate::infrastructure::ipc::{
    Bootstrap, BuildIdentity, ClientHello, ClientId, DaemonGeneration, Envelope, EnvelopeKind,
    ErrorCode, GenerationRole, ProtocolError, ProtocolRange, ProtocolVersion, ResponseOutcome,
    RetryMode, ServerHello, SideEffect, read_json_frame, write_json_frame,
};

/// A daemon request understood by every presentation surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// Revisioned daemon-owned PR inventory. Events are only hints; clients
    /// always converge by reading this snapshot.
    Pr {
        action: PrAction,
        payload: PrRequest,
    },
    /// Manage a daemon-owned periodic metrics subscription.  Metrics are
    /// observational only: they never authorize a client-side fallback.
    Metrics { action: MetricsAction },
    /// A lifecycle mutation. `operation_id` makes accepted work discoverable
    /// after a client disconnects.
    Session {
        action: SessionAction,
        operation_id: String,
        payload: Value,
    },
    /// A terminal attach/resume/resync request addressed only by its stable ref.
    Terminal {
        action: TerminalAction,
        payload: Value,
    },
    /// Start an Agent owned by the daemon. The daemon resolves the selected
    /// session's worktree and its default profile; clients never send argv,
    /// environment values, or a local process fallback.
    Agent {
        operation_id: String,
        intent: AgentLaunchIntent,
    },
    /// Private Codex `SessionStart` hook delivery. The opaque credential binds
    /// the provider-owned ID to one live daemon runtime; callers cannot name a
    /// runtime, session, path, or provider themselves.
    CodexSessionCapture {
        native_session_id: ProviderSessionId,
        caller_context: McpCallerContext,
    },
    /// Read the safe Agent runtime and interrupted-source inventory for one
    /// workspace. Root and managed-session records share this response.
    AgentInventory { workspace: WorkspaceId },
    /// Resume exactly one interrupted runtime selected from `AgentInventory`.
    ResumeAgent {
        operation_id: String,
        target: AgentResumeTarget,
    },
    /// Immediately dispatch a prompt to one durable Agent.  Session creation
    /// and Agent launch remain daemon-owned; this request only names the
    /// product-neutral dispatch intent.
    Dispatch {
        operation_id: String,
        intent: DispatchIntent,
    },
    /// MCP dispatch surface.  Its payload stays JSON at this presentation
    /// boundary; the daemon validates and resolves all identities.
    DispatchTool {
        action: DispatchToolAction,
        operation_id: String,
        payload: Value,
        /// Opaque daemon-minted credential inherited only by a provisioned MCP
        /// child. It is authentication material, never caller identity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_context: Option<McpCallerContext>,
    },
    /// Workspace-scoped human decision surface used by the local TUI. Unlike
    /// `DispatchTool`, this path never accepts agent-originated requests and
    /// does not treat a missing agent credential as authorization.
    UserDecision {
        action: TuiUserDecisionAction,
        payload: Value,
    },
    /// MCP control and observation for a daemon-owned supervisor aggregate.
    /// Caller provenance is derived by the daemon from the IPC context; it is
    /// intentionally not a client-supplied field in this request.
    SupervisorTool {
        action: SupervisorToolAction,
        operation_id: String,
        payload: Value,
    },
}

/// Opaque authentication presented by a daemon-provisioned MCP child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpCallerContext {
    pub credential: String,
}

/// Control vocabulary for the dedicated PR snapshot subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrAction {
    Snapshot,
    Subscribe,
    Unsubscribe,
}

/// A PR request names only a stable session and optional last known revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRequest {
    pub session_id: SessionId,
    pub revision: Option<u64>,
}

/// Source-of-truth PR snapshot. `entries` contains only safe presentation data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSnapshot {
    pub session_id: SessionId,
    pub revision: u64,
    pub entries: Vec<PrEntry>,
}

impl From<(SessionId, PrInventory)> for PrSnapshot {
    fn from((session_id, inventory): (SessionId, PrInventory)) -> Self {
        Self {
            session_id,
            revision: inventory.revision,
            entries: inventory.entries.into_values().collect(),
        }
    }
}

/// A lossy subscription hint. A duplicate, gap, or reorder is resolved by a
/// `PrAction::Snapshot` request using the revision in this payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrUpdated {
    pub session_id: SessionId,
    pub revision: u64,
}

/// Decodes the source-of-truth PR projection received after a hint or reconnect.
/// A malformed payload is a protocol error rather than a partially applied UI state.
///
/// # Errors
///
/// Returns `invalid_argument` when the response does not contain a complete snapshot.
pub fn decode_pr_snapshot(value: Value) -> Result<PrSnapshot, ClientError> {
    serde_json::from_value(value).map_err(|_| {
        ClientError::Protocol(ProtocolError::new(
            ErrorCode::InvalidArgument,
            "invalid PR snapshot response",
        ))
    })
}

/// The MCP operations backed by the daemon-owned dispatch registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchToolAction {
    Dispatch,
    SessionGet,
    AgentList,
    AgentGet,
    AgentComplete,
    AgentFail,
    AgentInbox,
    UserDecisionRequest,
    UserDecisionGet,
    UserDecisionList,
    UserDecisionResolve,
    UserDecisionCancel,
    UserDecisionExpire,
}

/// Human operations exposed to the workspace TUI. Request creation and
/// deadline expiry remain credential-fenced agent operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiUserDecisionAction {
    Get,
    List,
    Resolve,
    Cancel,
}

/// The opt-in supervisor MCP surface.  It is separate from dispatch so adding
/// it cannot change the existing session/agent tool contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorToolAction {
    Start,
    Get,
    List,
    Cancel,
    ResolveEscalation,
    Events,
}

/// Control vocabulary for the daemon metrics stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricsAction {
    Subscribe,
    Unsubscribe,
    Snapshot,
}

/// A deliberately small, versioned snapshot emitted by the daemon.  Counters
/// are process-local observations, not durable state or a control surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonMetrics {
    pub schema_version: u16,
    pub sampled_at_ms: u64,
    /// Daemon process CPU usage since the previous sample, in hundredths of a percent.
    #[serde(default)]
    pub cpu_percent_hundredths: u32,
    /// Daemon process peak resident memory, in bytes.
    #[serde(default)]
    pub resident_memory_bytes: u64,
    pub active_subscribers: u32,
    pub dropped_updates: u64,
    /// PTY output trimmed from the bounded retention window.
    #[serde(default)]
    pub terminal_dropped_bytes: u64,
    /// PTY output merged before registry admission.
    #[serde(default)]
    pub terminal_coalesced_bytes: u64,
    /// PTY output bytes whose reader had to wait for bounded queue capacity.
    #[serde(default)]
    pub terminal_backpressured_bytes: u64,
}

/// Product-neutral Agent launch intent sent by a TUI client.
///
/// The stable scope identity is enough for the daemon to resolve its durable
/// worktree. A session identity resolves that session's worktree; an absent
/// session (`None`) resolves the trusted workspace root. An omitted profile
/// deliberately delegates selection to the daemon's default policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLaunchIntent {
    pub workspace: WorkspaceId,
    /// Owning session; absent for a workspace-root launch.
    pub session: Option<SessionId>,
    pub profile: Option<AgentProfileId>,
}

/// The exclusive worker selector for an immediate dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DispatchAgentIntent {
    Existing {
        agent_id: AgentId,
    },
    New {
        runtime: AgentProfileId,
        model: ModelSelector,
    },
}

/// Product-neutral dispatch input. `caller` is supplied by the authenticated
/// execution context adapter, not selected as a destination by the worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchIntent {
    pub workspace: WorkspaceId,
    pub session_name: String,
    pub caller: CallerRef,
    pub agent: DispatchAgentIntent,
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    Create,
    Remove,
    /// Explicitly starts a new Agent runtime for retained provider-native
    /// conversation metadata. Startup/reconnect paths never issue this action.
    ResumeAgent,
    /// Explicitly validate and adopt legacy `state.json` sessions. This action
    /// is never part of daemon startup or a normal session refresh.
    RecoverLegacy,
    List,
    Status,
    Overview,
    Setup,
    Prompt,
    Complete,
    Pr,
    NoteGet,
    NoteUpdate,
    TodoList,
    TodoAdd,
    TodoUpdate,
    TodoRemove,
    DecisionList,
    DecisionLog,
    DelegateIssue,
    DelegateBrief,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAction {
    /// Reserve and spawn a daemon-owned generic terminal.  The payload is a
    /// [`TerminalLaunchIntent`], never a command line or environment.
    Launch,
    Inventory,
    Attach,
    Resume,
    Resync,
    Input,
    Resize,
    Detach,
}

/// Product-neutral generic terminal launch vocabulary.  It deliberately
/// serializes only a stable profile selector, a fully fenced scope and screen
/// geometry; process provision remains daemon-private.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalLaunchIntent {
    pub request: TerminalLaunchRequest,
    pub geometry: TerminalGeometry,
}

/// Geometry supplied by a terminal client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalGeometry {
    pub cols: u16,
    pub rows: u16,
}

/// Typed terminal command payloads.  Keeping these vocabulary types next to
/// the shared daemon client prevents UI/CLI adapters from inventing local PTY
/// fallback fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum TerminalRequest {
    Launch {
        intent: TerminalLaunchIntent,
    },
    Inventory {
        scope: TerminalLaunchScope,
    },
    Attach {
        terminal: TerminalRef,
    },
    Resume {
        terminal: TerminalRef,
        after_offset: u64,
    },
    Resync {
        terminal: TerminalRef,
    },
    Input {
        terminal: TerminalRef,
        subscription: u64,
        input_seq: u64,
        bytes: Vec<u8>,
    },
    Resize {
        terminal: TerminalRef,
        geometry: TerminalGeometry,
    },
    Detach {
        terminal: TerminalRef,
        subscription: u64,
    },
}

/// Re-exported selection type makes callers name the only accepted launch
/// selector, rather than constructing an untyped JSON payload.
pub type TerminalProfileSelection = TerminalProfileId;

/// The result exposed to CLI and MCP adapters.
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonReply {
    Ok(Value),
    Accepted {
        operation_id: String,
        revision: u64,
        /// Admission payload. Agent admission carries the fenced terminal that
        /// was spawned by the daemon; clients must not rediscover it by name.
        body: Value,
    },
}

/// Typed daemon failure.  Surfaces may render its safe details, but must not
/// infer that a local fallback is safe.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    Protocol(ProtocolError),
    Unavailable(String),
    /// The connected daemon is a different known executable artifact. This is
    /// an effect-free trigger: the old daemon and its terminals remain alive
    /// until a generation-handoff consumer accepts the operation.
    RolloverRequired(crate::infrastructure::ipc::BuildRolloverTrigger),
    /// One peer could not prove an exact artifact identity. Callers must not
    /// fall back to version/target equality or blind stop/start.
    BuildIdentityUnavailable,
    /// A daemon lifecycle transition could not safely establish a verified
    /// endpoint. Callers must not replace it with a local implementation.
    Lifecycle(String),
}

impl ClientError {
    #[must_use]
    pub fn retry_mode(&self) -> RetryMode {
        match self {
            Self::Protocol(error) => error.retry_mode,
            Self::Unavailable(_) | Self::Lifecycle(_) => RetryMode::Reconnect,
            Self::RolloverRequired(_) | Self::BuildIdentityUnavailable => RetryMode::Manual,
        }
    }

    #[must_use]
    pub fn side_effect(&self) -> SideEffect {
        match self {
            Self::Protocol(error) => error.side_effect,
            Self::Unavailable(_) | Self::Lifecycle(_) => SideEffect::PartialOrUnknown,
            Self::RolloverRequired(_) | Self::BuildIdentityUnavailable => SideEffect::None,
        }
    }

    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Protocol(error) => error.code,
            Self::Unavailable(_) | Self::Lifecycle(_) | Self::BuildIdentityUnavailable => {
                ErrorCode::Unavailable
            }
            Self::RolloverRequired(_) => ErrorCode::Busy,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(error) => write!(f, "{:?}: {}", error.code, error.message),
            Self::Unavailable(message) => write!(f, "Unavailable: {message}"),
            Self::RolloverRequired(trigger) => write!(
                f,
                "RolloverRequired: daemon build rollover operation {}",
                trigger.operation_id.0
            ),
            Self::BuildIdentityUnavailable => {
                f.write_str("BuildIdentityUnavailable: exact daemon artifact is unknown")
            }
            Self::Lifecycle(message) => write!(f, "Lifecycle: {message}"),
        }
    }
}
impl std::error::Error for ClientError {}

/// Common port implemented by the composition root's Unix IPC client.
pub trait DaemonClient {
    /// Submit one request. Implementations preserve correlation and never
    /// substitute a local managed-session implementation when this fails.
    ///
    /// # Errors
    ///
    /// Returns a typed daemon or transport failure without attempting a local
    /// managed-session fallback.
    fn request(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError>;
}

/// A synchronous framed implementation of [`DaemonClient`].  Unix socket
/// discovery and autospawn stay at the composition root; this type works over
/// any injected byte stream and is therefore usable in black-box tests too.
pub struct IpcClient<S> {
    stream: S,
    protocol: ProtocolVersion,
    daemon_generation: DaemonGeneration,
    server_build: BuildIdentity,
    next_request: u64,
    policy: ClientPolicy,
}

#[derive(Clone, Copy)]
struct ExpectedOwner<'a> {
    record: &'a DaemonRecord,
    generation: &'a DaemonGeneration,
    peer_pid: u32,
    observation: DaemonProcessObservation,
}

impl<S: Read + Write> IpcClient<S> {
    /// Performs the mandatory hello handshake before returning a usable client.
    ///
    /// # Errors
    ///
    /// Returns a typed protocol error from the peer, or an unavailable error
    /// when the byte stream cannot complete the handshake.
    pub fn connect(
        stream: S,
        client_id: String,
        connection_nonce: String,
        policy: ClientPolicy,
        build: BuildIdentity,
    ) -> Result<Self, ClientError> {
        Self::connect_with(stream, client_id, connection_nonce, policy, build, None)
    }

    /// Performs a handshake authorized by the established stream's OS peer
    /// PID, its process-start observation, the durable record, and locator
    /// generation.
    ///
    /// # Errors
    ///
    /// Returns an effect-zero ownership error unless all evidence agrees.
    #[allow(clippy::too_many_arguments)] // Keeps every independent ownership fence explicit.
    pub fn connect_expected_owner(
        stream: S,
        client_id: String,
        connection_nonce: String,
        policy: ClientPolicy,
        build: BuildIdentity,
        record: &DaemonRecord,
        generation: &DaemonGeneration,
        peer_pid: u32,
        observation: DaemonProcessObservation,
    ) -> Result<Self, ClientError> {
        Self::connect_with(
            stream,
            client_id,
            connection_nonce,
            policy,
            build,
            Some(ExpectedOwner {
                record,
                generation,
                peer_pid,
                observation,
            }),
        )
        .map_err(|error| match error {
            ClientError::Protocol(error) => ClientError::Protocol(error),
            other => ClientError::Protocol(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                other.to_string(),
            )),
        })
    }

    fn connect_with(
        mut stream: S,
        client_id: String,
        connection_nonce: String,
        policy: ClientPolicy,
        build: BuildIdentity,
        expected_owner: Option<ExpectedOwner<'_>>,
    ) -> Result<Self, ClientError> {
        let expected_nonce = connection_nonce.clone();
        let mut required_capabilities = vec![
            "request.correlation.v1".into(),
            "pr.snapshot.v1".into(),
            "build.artifact.v1".into(),
        ];
        if expected_owner.is_some() {
            required_capabilities.push("daemon.owner-identity.v1".into());
        }
        let hello = Bootstrap::ClientHello(ClientHello {
            client_id: ClientId(client_id),
            connection_nonce,
            expected_daemon_generation: expected_owner
                .as_ref()
                .map(|owner| (*owner.generation).clone()),
            supported_protocols: vec![ProtocolRange {
                generation: 1,
                min_revision: 0,
                max_revision: 1,
            }],
            capabilities: vec![],
            required_capabilities,
            build,
        });
        write_json_frame(&mut stream, &hello, 1_048_576)
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        match read_json_frame::<Bootstrap>(&mut stream, 1_048_576)
            .map_err(|error| ClientError::Unavailable(error.to_string()))?
        {
            Some(Bootstrap::ServerHello(hello)) => {
                if hello.connection_nonce != expected_nonce {
                    return Err(ClientError::Protocol(ProtocolError::new(
                        ErrorCode::Unauthenticated,
                        "daemon hello nonce does not match this connection",
                    )));
                }
                if let Some(owner) = expected_owner {
                    verify_owner_binding(&hello, &owner).map_err(ClientError::Protocol)?;
                }
                Ok(Self {
                    stream,
                    protocol: hello.protocol,
                    daemon_generation: hello.daemon_generation,
                    server_build: hello.build,
                    next_request: 0,
                    policy,
                })
            }
            Some(Bootstrap::Error(_error)) if expected_owner.is_some() => {
                Err(ClientError::Protocol(ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "daemon owner handshake failed before authentication",
                )))
            }
            Some(Bootstrap::Error(error)) => Err(ClientError::Protocol(error)),
            Some(Bootstrap::ClientHello(_)) | None => Err(ClientError::Unavailable(
                "daemon closed before a server hello".into(),
            )),
        }
    }

    /// Returns the build identity advertised by the daemon during the mandatory
    /// handshake. Composition roots use this only to decide whether their
    /// running binary must replace an older daemon; it is not protocol
    /// compatibility negotiation.
    #[must_use]
    pub fn server_build(&self) -> &BuildIdentity {
        &self.server_build
    }

    /// Borrows the authenticated byte stream for composition-owned passive
    /// lifecycle observation. Callers must not consume bytes through this
    /// reference while requests are in flight.
    #[must_use]
    pub const fn transport(&self) -> &S {
        &self.stream
    }
}

fn verify_owner_binding(
    hello: &ServerHello,
    owner: &ExpectedOwner<'_>,
) -> Result<(), ProtocolError> {
    let valid = owner.peer_pid == owner.record.pid
        && owner.observation == DaemonProcessObservation::Exact
        && &hello.daemon_generation == owner.generation
        && hello.generation_role == GenerationRole::Active
        && hello
            .capabilities
            .iter()
            .any(|capability| capability == "daemon.owner-identity.v1")
        && hello.daemon_process.as_ref() == Some(owner.record);
    if valid {
        Ok(())
    } else {
        Err(ProtocolError::new(
            ErrorCode::OwnershipUnknown,
            "daemon endpoint owner does not match OS peer, record, and generation",
        ))
    }
}

impl<S: Read + Write> DaemonClient for IpcClient<S> {
    fn request(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError> {
        self.next_request += 1;
        // Terminal owners use this correlation ID as part of their input
        // dedupe fence, so it must satisfy the canonical resource-ID contract
        // they validate at the server boundary.  Other request kinds retain
        // the compact per-connection sequence used by their response cache.
        let request_id = if matches!(&request, DaemonRequest::Terminal { .. }) {
            crate::infrastructure::ipc::RequestId(format!(
                "00000000-0000-4000-8000-{:012x}",
                self.next_request
            ))
        } else {
            crate::infrastructure::ipc::RequestId(self.next_request.to_string())
        };
        let envelope = Envelope {
            protocol: self.protocol,
            daemon_generation: self.daemon_generation.clone(),
            kind: EnvelopeKind::Request {
                request_id: request_id.clone(),
                timeout_ms: Some(self.policy.timeout_ms),
                body: serde_json::to_value(request).expect("daemon request serializes"),
            },
        };
        write_json_frame(&mut self.stream, &envelope, 1_048_576)
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        loop {
            let response = read_json_frame::<Envelope>(&mut self.stream, 1_048_576)
                .map_err(|error| ClientError::Unavailable(error.to_string()))?
                .ok_or_else(|| {
                    ClientError::Unavailable("daemon closed while awaiting response".into())
                })?;
            if let EnvelopeKind::Response {
                request_id: received,
                outcome,
                body,
            } = response.kind
            {
                if received != request_id {
                    continue;
                }
                return match outcome {
                    ResponseOutcome::Ok => Ok(DaemonReply::Ok(body)),
                    ResponseOutcome::Accepted {
                        operation_id,
                        operation_revision,
                    } => Ok(DaemonReply::Accepted {
                        operation_id: operation_id.0,
                        revision: operation_revision,
                        body,
                    }),
                    ResponseOutcome::Error(error) => Err(ClientError::Protocol(error)),
                };
            }
        }
    }
}

#[cfg(test)]
mod metrics_schema_tests {
    use super::{DaemonMetrics, DaemonRequest, MetricsAction};

    #[test]
    fn metrics_schema_is_tagged_and_versioned() {
        assert_eq!(
            serde_json::to_value(DaemonRequest::Metrics {
                action: MetricsAction::Subscribe,
            })
            .unwrap(),
            serde_json::json!({"kind": "metrics", "action": "subscribe"})
        );
        let snapshot: DaemonMetrics = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "sampled_at_ms": 42,
            "cpu_percent_hundredths": 123,
            "resident_memory_bytes": 456,
            "active_subscribers": 2,
            "dropped_updates": 3,
            "terminal_dropped_bytes": 4,
            "terminal_coalesced_bytes": 5,
            "terminal_backpressured_bytes": 6
        }))
        .unwrap();
        assert_eq!(snapshot.schema_version, 1);
        assert_eq!(snapshot.cpu_percent_hundredths, 123);
        assert_eq!(snapshot.resident_memory_bytes, 456);
        assert_eq!(snapshot.terminal_dropped_bytes, 4);
        assert_eq!(snapshot.terminal_coalesced_bytes, 5);
        assert_eq!(snapshot.terminal_backpressured_bytes, 6);

        let legacy_snapshot: DaemonMetrics = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "sampled_at_ms": 42,
            "active_subscribers": 2,
            "dropped_updates": 3
        }))
        .unwrap();
        assert_eq!(legacy_snapshot.cpu_percent_hundredths, 0);
        assert_eq!(legacy_snapshot.resident_memory_bytes, 0);
        assert_eq!(legacy_snapshot.terminal_dropped_bytes, 0);
        assert_eq!(legacy_snapshot.terminal_coalesced_bytes, 0);
        assert_eq!(legacy_snapshot.terminal_backpressured_bytes, 0);
    }
}

/// Per-surface timeout/reconnect policy. Retry is intentionally explicit: a
/// mutation may only be retried with its original request/operation identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientPolicy {
    pub timeout_ms: u64,
    pub reconnect_attempts: u8,
}

impl ClientPolicy {
    #[must_use]
    pub const fn tui() -> Self {
        Self {
            timeout_ms: 2_000,
            reconnect_attempts: 3,
        }
    }
    #[must_use]
    pub const fn cli() -> Self {
        Self {
            timeout_ms: 10_000,
            reconnect_attempts: 1,
        }
    }
    #[must_use]
    pub const fn mcp() -> Self {
        Self {
            timeout_ms: 30_000,
            reconnect_attempts: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    fn client_build() -> BuildIdentity {
        BuildIdentity {
            version: "test".into(),
            commit: "test".into(),
            target: "test".into(),
            artifact: "test-artifact".into(),
        }
    }

    fn owner_hello(record: &DaemonRecord, generation: &DaemonGeneration) -> ServerHello {
        ServerHello {
            connection_nonce: "nonce".into(),
            connection_id: crate::infrastructure::ipc::ConnectionId("connection".into()),
            daemon_generation: generation.clone(),
            generation_role: GenerationRole::Active,
            protocol: ProtocolVersion {
                generation: 1,
                revision: 1,
            },
            capabilities: vec!["daemon.owner-identity.v1".into()],
            build: client_build(),
            limits: crate::infrastructure::ipc::ProtocolLimits::default(),
            daemon_process: Some(record.clone()),
        }
    }

    #[test]
    fn owner_binding_requires_peer_process_record_and_generation_to_all_match() {
        let record = DaemonRecord::identified(4321, "process-start");
        let generation = DaemonGeneration("generation".into());
        let hello = owner_hello(&record, &generation);
        let exact = ExpectedOwner {
            record: &record,
            generation: &generation,
            peer_pid: record.pid,
            observation: DaemonProcessObservation::Exact,
        };
        assert!(verify_owner_binding(&hello, &exact).is_ok());

        for invalid in [
            ExpectedOwner {
                peer_pid: record.pid + 1,
                ..exact
            },
            ExpectedOwner {
                observation: DaemonProcessObservation::IdentityMismatch,
                ..exact
            },
            ExpectedOwner {
                observation: DaemonProcessObservation::Gone,
                ..exact
            },
            ExpectedOwner {
                observation: DaemonProcessObservation::Unknown,
                ..exact
            },
        ] {
            let error = verify_owner_binding(&hello, &invalid).unwrap_err();
            assert_eq!(error.code, ErrorCode::OwnershipUnknown);
            assert_eq!(error.side_effect, SideEffect::None);
        }

        let mut wrong_generation = hello.clone();
        wrong_generation.daemon_generation = DaemonGeneration("replacement".into());
        let mut draining = hello.clone();
        draining.generation_role = GenerationRole::Draining;
        let mut missing_capability = hello.clone();
        missing_capability.capabilities.clear();
        let mut wrong_record = hello.clone();
        wrong_record.daemon_process = Some(DaemonRecord::identified(record.pid, "replacement"));
        for invalid in [wrong_generation, draining, missing_capability, wrong_record] {
            let error = verify_owner_binding(&invalid, &exact).unwrap_err();
            assert_eq!(error.code, ErrorCode::OwnershipUnknown);
            assert_eq!(error.side_effect, SideEffect::None);
        }
    }

    struct Scripted {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }
    impl Write for Scripted {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("read failed"))
        }
    }

    struct ReadFails {
        output: Vec<u8>,
    }
    impl Read for ReadFails {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("read failed"))
        }
    }
    impl Write for ReadFails {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Write for Broken {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("write failed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn bootstrap_script(message: &Bootstrap) -> Scripted {
        let mut input = Vec::new();
        write_json_frame(&mut input, &message, 1_048_576).unwrap();
        Scripted {
            input: Cursor::new(input),
            output: vec![],
        }
    }

    #[test]
    fn exact_owner_handshake_maps_every_pre_authentication_failure_to_effect_zero() {
        let record = DaemonRecord::identified(4321, "process-start");
        let generation = DaemonGeneration("generation".into());
        let connect = |stream| {
            IpcClient::connect_expected_owner(
                stream,
                "client".into(),
                "nonce".into(),
                ClientPolicy::cli(),
                client_build(),
                &record,
                &generation,
                record.pid,
                DaemonProcessObservation::Exact,
            )
        };

        assert!(
            connect(bootstrap_script(&Bootstrap::ServerHello(owner_hello(
                &record,
                &generation,
            ))))
            .is_ok()
        );

        let protocol_error = connect(bootstrap_script(&Bootstrap::Error(ProtocolError::new(
            ErrorCode::Busy,
            "not authenticated",
        ))))
        .err()
        .unwrap();
        assert_eq!(protocol_error.code(), ErrorCode::OwnershipUnknown);
        assert_eq!(protocol_error.side_effect(), SideEffect::None);

        let unavailable = connect(Scripted {
            input: Cursor::new(vec![]),
            output: vec![],
        })
        .err()
        .unwrap();
        assert_eq!(unavailable.code(), ErrorCode::OwnershipUnknown);
        assert_eq!(unavailable.side_effect(), SideEffect::None);

        let mut wrong_nonce = owner_hello(&record, &generation);
        wrong_nonce.connection_nonce = "other-connection".into();
        let unauthenticated = connect(bootstrap_script(&Bootstrap::ServerHello(wrong_nonce)))
            .err()
            .unwrap();
        assert_eq!(unauthenticated.code(), ErrorCode::Unauthenticated);
        assert_eq!(unauthenticated.side_effect(), SideEffect::None);
    }

    fn scripted(reply: ResponseOutcome, request_id: &str) -> Scripted {
        let protocol = ProtocolVersion {
            generation: 1,
            revision: 1,
        };
        let generation = DaemonGeneration("daemon".into());
        let hello = Bootstrap::ServerHello(crate::infrastructure::ipc::ServerHello {
            connection_nonce: "nonce".into(),
            connection_id: crate::infrastructure::ipc::ConnectionId("connection".into()),
            daemon_generation: generation.clone(),
            generation_role: crate::infrastructure::ipc::GenerationRole::Active,
            protocol,
            capabilities: vec![],
            build: BuildIdentity {
                version: "test".into(),
                commit: "test".into(),
                target: "test".into(),
                artifact: "server-artifact".into(),
            },
            limits: crate::infrastructure::ipc::ProtocolLimits::default(),
            daemon_process: None,
        });
        let response = Envelope {
            protocol,
            daemon_generation: generation.clone(),
            kind: EnvelopeKind::Response {
                request_id: crate::infrastructure::ipc::RequestId(request_id.into()),
                outcome: reply,
                body: serde_json::json!({"ok":true}),
            },
        };
        let mut input = Vec::new();
        write_json_frame(&mut input, &hello, 1_048_576).unwrap();
        let event = Envelope {
            protocol,
            daemon_generation: generation.clone(),
            kind: EnvelopeKind::Event {
                subscription_id: crate::infrastructure::ipc::SubscriptionId("s".into()),
                stream_ref: crate::infrastructure::ipc::StreamRef {
                    stream_id: crate::infrastructure::ipc::StreamId("stream".into()),
                    epoch: "epoch".into(),
                },
                stream_sequence: 1,
                body: serde_json::json!({}),
            },
        };
        write_json_frame(&mut input, &event, 1_048_576).unwrap();
        let unrelated = Envelope {
            protocol,
            daemon_generation: generation.clone(),
            kind: EnvelopeKind::Response {
                request_id: crate::infrastructure::ipc::RequestId("other".into()),
                outcome: ResponseOutcome::Ok,
                body: serde_json::json!({}),
            },
        };
        write_json_frame(&mut input, &unrelated, 1_048_576).unwrap();
        write_json_frame(&mut input, &response, 1_048_576).unwrap();
        Scripted {
            input: Cursor::new(input),
            output: vec![],
        }
    }

    #[test]
    fn unavailable_is_reconnectable_but_has_unknown_side_effect() {
        let error = ClientError::Unavailable("daemon is absent".into());
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.retry_mode(), RetryMode::Reconnect);
        assert_eq!(error.side_effect(), SideEffect::PartialOrUnknown);
        assert_eq!(error.to_string(), "Unavailable: daemon is absent");
    }

    #[test]
    fn build_rollover_and_unknown_identity_are_typed_effect_free_failures() {
        let running =
            crate::infrastructure::ipc::build_identity("1", "a", "test", "debug", &"a".repeat(64));
        let expected =
            crate::infrastructure::ipc::build_identity("1", "b", "test", "debug", &"b".repeat(64));
        let trigger =
            crate::infrastructure::ipc::build_rollover_trigger(&running, &expected, "local", false)
                .unwrap();
        let rollover = ClientError::RolloverRequired(trigger.clone());
        assert_eq!(rollover.code(), ErrorCode::Busy);
        assert_eq!(rollover.retry_mode(), RetryMode::Manual);
        assert_eq!(rollover.side_effect(), SideEffect::None);
        assert!(rollover.to_string().contains(&trigger.operation_id.0));

        let unknown = ClientError::BuildIdentityUnavailable;
        assert_eq!(unknown.code(), ErrorCode::Unavailable);
        assert_eq!(unknown.retry_mode(), RetryMode::Manual);
        assert_eq!(unknown.side_effect(), SideEffect::None);
        assert!(
            unknown
                .to_string()
                .contains("exact daemon artifact is unknown")
        );
    }

    #[test]
    fn policies_are_surface_specific() {
        assert!(ClientPolicy::tui().timeout_ms < ClientPolicy::cli().timeout_ms);
        assert!(ClientPolicy::mcp().timeout_ms > ClientPolicy::cli().timeout_ms);
    }

    #[test]
    fn pr_snapshot_decoder_accepts_only_complete_source_of_truth_payloads() {
        let session = SessionId::new();
        let identity =
            crate::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/1").unwrap();
        let mut inventory = PrInventory::default();
        inventory.discover([identity]);
        let projected = PrSnapshot::from((session, inventory));
        assert_eq!(projected.session_id, session);
        assert_eq!(projected.revision, 1);
        assert_eq!(projected.entries.len(), 1);
        let snapshot = PrSnapshot {
            session_id: session,
            revision: 4,
            entries: vec![],
        };
        assert_eq!(
            decode_pr_snapshot(serde_json::to_value(&snapshot).unwrap()).unwrap(),
            snapshot
        );
        assert!(matches!(
            decode_pr_snapshot(serde_json::json!({"revision": 4})),
            Err(ClientError::Protocol(_))
        ));
    }

    #[test]
    fn client_handshakes_and_preserves_accepted_operation() {
        let stream = scripted(
            ResponseOutcome::Accepted {
                operation_id: crate::infrastructure::ipc::OperationId("op".into()),
                operation_revision: 7,
            },
            "1",
        );
        let mut client = IpcClient::connect(
            stream,
            "client".into(),
            "nonce".into(),
            ClientPolicy::cli(),
            client_build(),
        )
        .unwrap();
        assert_eq!(client.server_build().version, "test");
        assert_eq!(
            client
                .request(DaemonRequest::Session {
                    action: SessionAction::Create,
                    operation_id: "op".into(),
                    payload: serde_json::json!({})
                })
                .unwrap(),
            DaemonReply::Accepted {
                operation_id: "op".into(),
                revision: 7,
                body: serde_json::json!({"ok": true}),
            }
        );
    }

    #[test]
    fn protocol_errors_are_rendered_and_keep_their_retry_contract() {
        let mut error = ProtocolError::new(ErrorCode::OwnershipUnknown, "owner vanished");
        error.retry_mode = RetryMode::Manual;
        error.side_effect = SideEffect::Applied;
        let error = ClientError::Protocol(error);
        assert_eq!(error.code(), ErrorCode::OwnershipUnknown);
        assert_eq!(error.retry_mode(), RetryMode::Manual);
        assert_eq!(error.side_effect(), SideEffect::Applied);
        assert!(error.to_string().contains("owner vanished"));
        assert_eq!(
            ClientError::Lifecycle("state changed".into()).to_string(),
            "Lifecycle: state changed"
        );
    }

    #[test]
    fn client_returns_ok_and_protocol_error_replies() {
        for (reply, expect_error) in [
            (ResponseOutcome::Ok, false),
            (
                ResponseOutcome::Error(ProtocolError::new(ErrorCode::Busy, "busy")),
                true,
            ),
        ] {
            let stream = scripted(reply, "00000000-0000-4000-8000-000000000001");
            let mut client = IpcClient::connect(
                stream,
                "client".into(),
                "nonce".into(),
                ClientPolicy::cli(),
                client_build(),
            )
            .unwrap();
            let result = client.request(DaemonRequest::Terminal {
                action: TerminalAction::Resync,
                payload: serde_json::json!({}),
            });
            if expect_error {
                assert!(matches!(
                    result,
                    Err(ClientError::Protocol(error)) if error.code == ErrorCode::Busy
                ));
            } else {
                assert!(matches!(
                    result,
                    Ok(DaemonReply::Ok(value)) if value["ok"] == true
                ));
            }
        }
    }

    #[test]
    fn client_rejects_error_and_missing_handshakes() {
        let protocol_error = ProtocolError::new(ErrorCode::ProtocolMismatch, "nope");
        let mut bytes = Vec::new();
        write_json_frame(&mut bytes, &Bootstrap::Error(protocol_error), 1_048_576).unwrap();
        assert!(matches!(
            IpcClient::connect(
                Scripted {
                    input: Cursor::new(bytes),
                    output: vec![]
                },
                "c".into(),
                "n".into(),
                ClientPolicy::tui(),
                client_build()
            ),
            Err(ClientError::Protocol(_))
        ));
        assert!(matches!(
            IpcClient::connect(
                Scripted {
                    input: Cursor::new(vec![]),
                    output: vec![]
                },
                "c".into(),
                "n".into(),
                ClientPolicy::tui(),
                client_build()
            ),
            Err(ClientError::Unavailable(_))
        ));
        assert!(matches!(
            IpcClient::connect(
                Broken,
                "c".into(),
                "n".into(),
                ClientPolicy::tui(),
                client_build()
            ),
            Err(ClientError::Unavailable(_))
        ));
        assert!(matches!(
            IpcClient::connect(
                ReadFails { output: vec![] },
                "c".into(),
                "n".into(),
                ClientPolicy::tui(),
                client_build()
            ),
            Err(ClientError::Unavailable(_))
        ));
    }

    #[test]
    fn request_maps_transport_failures_to_unavailable() {
        let protocol = ProtocolVersion {
            generation: 1,
            revision: 1,
        };
        let request = DaemonRequest::Terminal {
            action: TerminalAction::Attach,
            payload: serde_json::json!({}),
        };
        let server_build = BuildIdentity {
            version: "test".into(),
            commit: "test".into(),
            target: "test".into(),
            artifact: "server-artifact".into(),
        };
        let mut broken = IpcClient {
            stream: Broken,
            protocol,
            daemon_generation: DaemonGeneration("d".into()),
            server_build: server_build.clone(),
            next_request: 0,
            policy: ClientPolicy::tui(),
        };
        assert!(matches!(
            broken.request(request.clone()),
            Err(ClientError::Unavailable(_))
        ));
        let mut closed = IpcClient {
            stream: Scripted {
                input: Cursor::new(vec![]),
                output: vec![],
            },
            protocol,
            daemon_generation: DaemonGeneration("d".into()),
            server_build: server_build.clone(),
            next_request: 0,
            policy: ClientPolicy::tui(),
        };
        assert!(matches!(
            closed.request(request),
            Err(ClientError::Unavailable(_))
        ));
        let mut read_fails = IpcClient {
            stream: ReadFails { output: vec![] },
            protocol,
            daemon_generation: DaemonGeneration("d".into()),
            server_build,
            next_request: 0,
            policy: ClientPolicy::tui(),
        };
        assert!(matches!(
            read_fails.request(DaemonRequest::Terminal {
                action: TerminalAction::Attach,
                payload: serde_json::json!({}),
            }),
            Err(ClientError::Unavailable(_))
        ));
        assert!(
            Scripted {
                input: Cursor::new(vec![]),
                output: vec![]
            }
            .flush()
            .is_ok()
        );
        let mut broken = Broken;
        assert!(broken.read(&mut []).is_err());
        assert!(broken.flush().is_ok());
        let mut read_fails = ReadFails { output: vec![] };
        assert!(read_fails.flush().is_ok());
    }
}
