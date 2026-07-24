//! Surface-neutral daemon client port.
//!
//! Presentation surfaces submit only typed request bodies through this port.  In
//! particular, a connection failure is not permission to mutate local session
//! state or to allocate a local managed PTY.

use std::fmt;
use std::io::{self, Read, Write};
use std::time::Duration;

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

impl DispatchToolAction {
    /// Whether this action only reads daemon state, so a fresh-connection retry
    /// re-reads safely.
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(
            self,
            Self::SessionGet
                | Self::AgentList
                | Self::AgentGet
                | Self::AgentInbox
                | Self::UserDecisionGet
                | Self::UserDecisionList
        )
    }

    /// Whether this action mutates through the daemon's durable, producer
    /// `OperationId`-keyed dispatch registry, so the same operation replays to
    /// the same final on a fresh connection.
    #[must_use]
    pub const fn is_durable_operation(self) -> bool {
        matches!(self, Self::Dispatch)
    }
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
    /// List exited tombstones in a scope with their final replay locator, exit
    /// status, and workspace-global visibility. It never changes the liveness
    /// contract of [`Inventory`](Self::Inventory); it is an additive query for
    /// terminals that have already exited (#525).
    CompletedInventory,
    /// Raise an exact tombstone's workspace-global visibility to at least
    /// `Observed` under compare-and-swap.
    Observe,
    /// Raise an exact tombstone's workspace-global visibility to `Dismissed`
    /// under compare-and-swap. It does not touch the terminal or its process.
    Dismiss,
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
    /// Query exited tombstones in a scope (#525). The response body is
    /// `{"entries": [CompletedTerminalEntry]}`.
    CompletedInventory {
        scope: TerminalLaunchScope,
    },
    /// Compare-and-swap the exact tombstone's visibility to at least `Observed`.
    /// The response body is `{"visibility": TerminalVisibility, "applied": bool,
    /// "conflict": bool}`.
    Observe {
        terminal: TerminalRef,
        expected_revision: u64,
    },
    /// Compare-and-swap the exact tombstone's visibility to `Dismissed`. Same
    /// response body shape as [`Observe`](Self::Observe).
    Dismiss {
        terminal: TerminalRef,
        expected_revision: u64,
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

    /// Whether this failure is a lost/timed-out request rather than a definitive
    /// server answer. Only transport failures consume the reconnect budget: a
    /// well-formed [`ProtocolError`] means the server responded, so the request
    /// is finished and must not be replayed on a fresh connection.
    #[must_use]
    pub fn is_transport_failure(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Lifecycle(_))
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

/// A monotonic millisecond time source. Only differences between observations
/// are meaningful; the origin is arbitrary and never a wall clock. It is
/// injected so the deadline state machine is deterministic under a controllable
/// fake and never resets an attempt budget from unrelated progress.
pub trait MonotonicClock {
    fn now_ms(&self) -> u64;
}

/// A byte-stream connection whose blocking reads and writes accept a per-call
/// timeout. Implementations translate the timeout to an OS receive/send
/// deadline; a read/write that cannot make progress within it fails with an
/// [`io::ErrorKind::TimedOut`] or [`io::ErrorKind::WouldBlock`]. The connection
/// must not be reused after such a timeout because a partial frame may already
/// have been consumed.
pub trait DeadlineConnection: Read + Write {
    /// # Errors
    ///
    /// Returns an error only when the underlying transport cannot arm the
    /// receive timeout.
    fn set_read_deadline(&mut self, timeout: Duration) -> io::Result<()>;
    /// # Errors
    ///
    /// Returns an error only when the underlying transport cannot arm the send
    /// timeout.
    fn set_write_deadline(&mut self, timeout: Duration) -> io::Result<()>;
}

/// A [`Read`]/[`Write`] adapter that enforces one end-to-end monotonic deadline
/// across every syscall of a single attempt. Because the deadline is recomputed
/// against the fixed target before each read/write, partial progress and
/// unrelated event frames shrink the remaining budget instead of extending it:
/// a peer dribbling bytes still hits the deadline. Once the budget is spent, the
/// next read/write returns `TimedOut` without touching the transport.
///
/// The existing [`IpcClient`] framing runs unchanged over this stream, so its
/// handshake, request write, and response read all become deadline-bounded
/// without any protocol change.
pub struct DeadlineStream<Cl, C> {
    clock: Cl,
    inner: C,
    deadline_ms: u64,
}

impl<Cl: MonotonicClock, C: DeadlineConnection> DeadlineStream<Cl, C> {
    /// Arms a fresh end-to-end deadline `budget_ms` from now over `inner`.
    #[must_use]
    pub fn new(clock: Cl, inner: C, budget_ms: u64) -> Self {
        let deadline_ms = clock.now_ms().saturating_add(budget_ms);
        Self {
            clock,
            inner,
            deadline_ms,
        }
    }

    /// Borrows the wrapped transport for composition-owned observation (for
    /// example cloning a passive lifecycle watcher).
    pub fn get_ref(&self) -> &C {
        &self.inner
    }

    /// Mutably borrows the wrapped transport.
    pub fn get_mut(&mut self) -> &mut C {
        &mut self.inner
    }

    fn remaining(&self) -> io::Result<Duration> {
        let now = self.clock.now_ms();
        if self.deadline_ms > now {
            Ok(Duration::from_millis(self.deadline_ms - now))
        } else {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "IPC attempt deadline exceeded",
            ))
        }
    }
}

impl<Cl: MonotonicClock, C: DeadlineConnection> Read for DeadlineStream<Cl, C> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.remaining()?;
        self.inner.set_read_deadline(remaining)?;
        self.inner.read(buf)
    }
}

impl<Cl: MonotonicClock, C: DeadlineConnection> Write for DeadlineStream<Cl, C> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let remaining = self.remaining()?;
        self.inner.set_write_deadline(remaining)?;
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A byte stream that can restart its attempt deadline. A reused connection is
/// rearmed at the start of each new attempt so successful requests share one
/// connection while every attempt still gets its own end-to-end budget.
pub trait RearmableStream: Read + Write {
    fn rearm(&mut self, budget_ms: u64);
}

impl<Cl: MonotonicClock, C: DeadlineConnection> RearmableStream for DeadlineStream<Cl, C> {
    fn rearm(&mut self, budget_ms: u64) {
        self.deadline_ms = self.clock.now_ms().saturating_add(budget_ms);
    }
}

/// One live, single-connection daemon session: exactly one request/response is
/// attempted over it before the reconnect state machine either returns or
/// discards it.
pub trait DaemonSession {
    /// Sends one request and awaits its correlated response within the
    /// connection's currently armed deadline.
    ///
    /// # Errors
    ///
    /// Returns a typed daemon or transport failure. A transport failure
    /// (including a deadline overrun) leaves the effect unknown and the session
    /// unusable.
    fn exchange(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError>;

    /// Restarts the end-to-end deadline budget for a reused connection.
    fn rearm(&mut self, budget_ms: u64);
}

impl<S: RearmableStream> DaemonSession for IpcClient<S> {
    fn exchange(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError> {
        <Self as DaemonClient>::request(self, request)
    }

    fn rearm(&mut self, budget_ms: u64) {
        self.stream.rearm(budget_ms);
    }
}

/// The single source of truth for whether a request may be retried on a fresh
/// connection after a lost or timed-out response. This is a request-class
/// decision, deliberately fail-closed: only proven read-only queries and
/// mutations the daemon replays by a server-backed producer `OperationId` +
/// semantic digest are eligible. A `RequestId` is connection-local correlation
/// only and is never cross-connection idempotency evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryEligibility {
    /// Read-only query (or connection-local subscription management). A fresh
    /// connection re-reads or re-subscribes safely; stale responses are dropped.
    ReadOnly,
    /// Mutation whose durable outcome the daemon replays by producer
    /// `OperationId` + semantic digest, so the same operation converges on the
    /// same final across a new connection.
    DurableOperation,
    /// No cross-connection idempotency evidence: generic Terminal Launch
    /// (#518), terminal input (#519), `RequestId`-only mutations, and Codex
    /// capture. After a request is dispatched the effect is unknown, so it is
    /// never blind-retried on a fresh connection.
    NoCrossConnectionEvidence,
}

impl RetryEligibility {
    /// Classifies a request against the retry eligibility table. Anything not
    /// provably read-only or durably operation-backed fails closed to
    /// [`Self::NoCrossConnectionEvidence`].
    #[must_use]
    pub fn classify(request: &DaemonRequest) -> Self {
        match request {
            DaemonRequest::Pr { .. }
            | DaemonRequest::Metrics { .. }
            | DaemonRequest::AgentInventory { .. } => Self::ReadOnly,
            DaemonRequest::Session { action, .. } => {
                if session_action_is_read_only(*action) {
                    Self::ReadOnly
                } else if session_action_is_durable_operation(*action) {
                    Self::DurableOperation
                } else {
                    Self::NoCrossConnectionEvidence
                }
            }
            DaemonRequest::DispatchTool { action, .. } => {
                if action.is_read_only() {
                    Self::ReadOnly
                } else if action.is_durable_operation() {
                    Self::DurableOperation
                } else {
                    Self::NoCrossConnectionEvidence
                }
            }
            DaemonRequest::SupervisorTool { action, .. } => {
                if supervisor_action_is_read_only(*action) {
                    Self::ReadOnly
                } else if supervisor_action_is_durable_operation(*action) {
                    Self::DurableOperation
                } else {
                    Self::NoCrossConnectionEvidence
                }
            }
            DaemonRequest::UserDecision { action, .. } => {
                if user_decision_action_is_read_only(*action) {
                    Self::ReadOnly
                } else {
                    Self::NoCrossConnectionEvidence
                }
            }
            DaemonRequest::Agent { .. }
            | DaemonRequest::ResumeAgent { .. }
            | DaemonRequest::Dispatch { .. } => Self::DurableOperation,
            DaemonRequest::Terminal { .. } | DaemonRequest::CodexSessionCapture { .. } => {
                Self::NoCrossConnectionEvidence
            }
        }
    }

    /// Whether a lost or timed-out response permits one more attempt on a fresh
    /// connection.
    #[must_use]
    pub fn may_retry_on_new_connection(self) -> bool {
        matches!(self, Self::ReadOnly | Self::DurableOperation)
    }
}

const fn session_action_is_read_only(action: SessionAction) -> bool {
    matches!(
        action,
        SessionAction::List
            | SessionAction::Status
            | SessionAction::Overview
            | SessionAction::Pr
            | SessionAction::NoteGet
            | SessionAction::TodoList
            | SessionAction::DecisionList
    )
}

const fn session_action_is_durable_operation(action: SessionAction) -> bool {
    // The IPC contract documents durable, `OperationId`-keyed replay for these
    // lifecycle mutations (create/remove/resume across daemon restarts). Other
    // mutating actions stay fail-closed until their server-backed durable
    // contract is proven.
    matches!(
        action,
        SessionAction::Create | SessionAction::Remove | SessionAction::ResumeAgent
    )
}

const fn supervisor_action_is_read_only(action: SupervisorToolAction) -> bool {
    matches!(
        action,
        SupervisorToolAction::Get | SupervisorToolAction::List | SupervisorToolAction::Events
    )
}

const fn supervisor_action_is_durable_operation(action: SupervisorToolAction) -> bool {
    matches!(action, SupervisorToolAction::Start)
}

const fn user_decision_action_is_read_only(action: TuiUserDecisionAction) -> bool {
    matches!(
        action,
        TuiUserDecisionAction::Get | TuiUserDecisionAction::List
    )
}

/// A resilient [`DaemonClient`] that enforces [`ClientPolicy`] end to end. Each
/// attempt (the initial one and each reconnect) consumes exactly one
/// independent monotonic deadline budget spanning connect/handshake, request
/// write, and response read. `reconnect_attempts` bounds the additional
/// attempts, and [`RetryEligibility`] is the only gate on whether a lost
/// response may be replayed on a fresh connection.
///
/// A successful connection is reused across requests (so MCP keeps one socket
/// for its lifetime); a transport failure discards it and never reuses a
/// partially written frame or socket.
pub struct PolicyClient<Cl, K, S> {
    clock: Cl,
    policy: ClientPolicy,
    connect: K,
    session: Option<S>,
}

impl<Cl, K, S> PolicyClient<Cl, K, S>
where
    Cl: MonotonicClock + Clone,
    K: FnMut(Cl, u64) -> Result<S, ClientError>,
    S: DaemonSession,
{
    /// Builds a policy client. `initial` is the eagerly established first
    /// session (so surfaces that fail fast on an absent daemon keep doing so);
    /// `connect` establishes a fresh deadline-armed session for each reconnect.
    #[must_use]
    pub fn new(clock: Cl, policy: ClientPolicy, connect: K, initial: Option<S>) -> Self {
        Self {
            clock,
            policy,
            connect,
            session: initial,
        }
    }
}

impl<Cl, K, S> DaemonClient for PolicyClient<Cl, K, S>
where
    Cl: MonotonicClock + Clone,
    K: FnMut(Cl, u64) -> Result<S, ClientError>,
    S: DaemonSession,
{
    fn request(&mut self, request: DaemonRequest) -> Result<DaemonReply, ClientError> {
        let eligibility = RetryEligibility::classify(&request);
        let attempts = 1u32.saturating_add(u32::from(self.policy.reconnect_attempts));
        // Overwritten by every non-returning failure path below; the loop always
        // runs at least once, so the initial value is a formality.
        let mut last =
            ClientError::Unavailable("daemon connection could not be established".into());
        for _ in 0..attempts {
            let session = match self.session {
                // A reused connection restarts its budget for this attempt.
                Some(ref mut session) => {
                    session.rearm(self.policy.timeout_ms);
                    session
                }
                // A fresh connection begins this attempt's end-to-end budget; the
                // returned session's deadline continues into the request exchange.
                None => match (self.connect)(self.clock.clone(), self.policy.timeout_ms) {
                    Ok(session) => self.session.insert(session),
                    Err(error) => {
                        // No request was dispatched, so retrying a new connection
                        // is safe for every request class within the budget.
                        last = error;
                        continue;
                    }
                },
            };
            match session.exchange(request.clone()) {
                Ok(reply) => return Ok(reply),
                Err(error) => {
                    if !error.is_transport_failure() {
                        // A well-formed protocol error is a definitive answer;
                        // the healthy connection is kept for reuse.
                        return Err(error);
                    }
                    // Never reuse a timed-out or broken socket.
                    self.session = None;
                    last = error;
                    if !eligibility.may_retry_on_new_connection() {
                        // Effect is unknown; stop even with budget remaining.
                        break;
                    }
                }
            }
        }
        Err(last)
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

#[cfg(test)]
mod deadline_and_retry_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::io::Cursor;
    use std::rc::Rc;

    use crate::domain::id::WorkspaceId;
    use crate::infrastructure::ipc::{
        ConnectionId, DaemonGeneration, GenerationRole, ProtocolLimits, ProtocolVersion,
        ServerHello, read_frame,
    };

    // ---- Fake clock -------------------------------------------------------

    #[derive(Clone, Default)]
    struct FakeClock(Rc<Cell<u64>>);
    impl MonotonicClock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
    }
    impl FakeClock {
        fn advance(&self, ms: u64) {
            self.0.set(self.0.get() + ms);
        }
    }

    // ---- Retry state machine over a fake session --------------------------

    #[derive(Clone, Default)]
    struct Counters {
        connects: Rc<Cell<usize>>,
        exchanges: Rc<Cell<usize>>,
        rearms: Rc<Cell<usize>>,
    }

    struct FakeSession {
        counters: Counters,
        outcomes: Rc<RefCell<VecDeque<Result<DaemonReply, ClientError>>>>,
    }
    impl DaemonSession for FakeSession {
        fn exchange(&mut self, _request: DaemonRequest) -> Result<DaemonReply, ClientError> {
            self.counters
                .exchanges
                .set(self.counters.exchanges.get() + 1);
            self.outcomes.borrow_mut().pop_front().unwrap()
        }
        fn rearm(&mut self, _budget_ms: u64) {
            self.counters.rearms.set(self.counters.rearms.get() + 1);
        }
    }

    fn ok_reply() -> DaemonReply {
        DaemonReply::Ok(serde_json::json!({"ok": true}))
    }
    fn transport_error() -> ClientError {
        ClientError::Unavailable("stalled".into())
    }

    #[allow(clippy::type_complexity)]
    fn policy_client(
        policy: ClientPolicy,
        connect_outcomes: Vec<Result<(), ClientError>>,
        exchange_outcomes: Vec<Result<DaemonReply, ClientError>>,
        with_initial_session: bool,
        counters: &Counters,
    ) -> PolicyClient<
        FakeClock,
        impl FnMut(FakeClock, u64) -> Result<FakeSession, ClientError>,
        FakeSession,
    > {
        let outcomes = Rc::new(RefCell::new(VecDeque::from(exchange_outcomes)));
        let connect_deque = Rc::new(RefCell::new(VecDeque::from(connect_outcomes)));
        let make = {
            let counters = counters.clone();
            let outcomes = outcomes.clone();
            move || FakeSession {
                counters: counters.clone(),
                outcomes: outcomes.clone(),
            }
        };
        let initial = with_initial_session.then(&make);
        let connect = {
            let counters = counters.clone();
            move |_clock: FakeClock, _budget: u64| -> Result<FakeSession, ClientError> {
                counters.connects.set(counters.connects.get() + 1);
                connect_deque
                    .borrow_mut()
                    .pop_front()
                    .unwrap()
                    .map(|()| make())
            }
        };
        PolicyClient::new(FakeClock::default(), policy, connect, initial)
    }

    #[test]
    fn read_only_retries_on_a_fresh_connection_within_budget() {
        let counters = Counters::default();
        let mut client = policy_client(
            ClientPolicy::cli(),
            vec![Ok(()), Ok(())],
            vec![Err(transport_error()), Ok(ok_reply())],
            false,
            &counters,
        );
        assert_eq!(
            client
                .request(DaemonRequest::Metrics {
                    action: MetricsAction::Snapshot
                })
                .unwrap(),
            ok_reply()
        );
        // initial attempt + one reconnect, each an independent end-to-end budget.
        assert_eq!(counters.connects.get(), 2);
        assert_eq!(counters.exchanges.get(), 2);
    }

    #[test]
    fn budget_exhaustion_is_typed_unavailable_with_unknown_side_effect() {
        let counters = Counters::default();
        let mut client = policy_client(
            ClientPolicy::cli(),
            vec![Ok(()), Ok(())],
            vec![Err(transport_error()), Err(transport_error())],
            false,
            &counters,
        );
        let error = client
            .request(DaemonRequest::Metrics {
                action: MetricsAction::Snapshot,
            })
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::Unavailable);
        assert_eq!(error.side_effect(), SideEffect::PartialOrUnknown);
        assert_eq!(counters.connects.get(), 2);
        assert_eq!(counters.exchanges.get(), 2);
    }

    #[test]
    fn durable_mutation_retries_like_a_read_only_query() {
        let counters = Counters::default();
        let mut client = policy_client(
            ClientPolicy::cli(),
            vec![Ok(()), Ok(())],
            vec![Err(transport_error()), Ok(ok_reply())],
            false,
            &counters,
        );
        assert_eq!(
            client
                .request(DaemonRequest::Session {
                    action: SessionAction::Create,
                    operation_id: "op".into(),
                    payload: serde_json::json!({}),
                })
                .unwrap(),
            ok_reply()
        );
        assert_eq!(counters.connects.get(), 2);
    }

    #[test]
    fn ineligible_mutation_never_blind_retries_on_a_new_connection() {
        let counters = Counters::default();
        let mut client = policy_client(
            ClientPolicy::cli(),
            // budget for a second connection exists, but it must stay unused.
            vec![Ok(()), Ok(())],
            vec![Err(transport_error()), Ok(ok_reply())],
            false,
            &counters,
        );
        let error = client
            .request(DaemonRequest::Terminal {
                action: TerminalAction::Input,
                payload: serde_json::json!({}),
            })
            .unwrap_err();
        assert!(error.is_transport_failure());
        // Exactly one connection and one attempt: effect is unknown, not retried.
        assert_eq!(counters.connects.get(), 1);
        assert_eq!(counters.exchanges.get(), 1);
    }

    #[test]
    fn a_definitive_protocol_error_returns_without_reconnecting() {
        let counters = Counters::default();
        let mut client = policy_client(
            ClientPolicy::cli(),
            vec![Ok(())],
            vec![Err(ClientError::Protocol(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "bad",
            )))],
            false,
            &counters,
        );
        let error = client
            .request(DaemonRequest::Metrics {
                action: MetricsAction::Snapshot,
            })
            .unwrap_err();
        assert!(matches!(error, ClientError::Protocol(_)));
        assert_eq!(counters.connects.get(), 1);
        assert_eq!(counters.exchanges.get(), 1);
        // The healthy connection is kept for reuse.
        assert!(client.session.is_some());
    }

    #[test]
    fn a_connect_failure_retries_for_every_request_class() {
        let counters = Counters::default();
        // An ineligible request still retries when nothing was dispatched yet.
        let mut client = policy_client(
            ClientPolicy::cli(),
            vec![Err(transport_error()), Ok(())],
            vec![Ok(ok_reply())],
            false,
            &counters,
        );
        assert_eq!(
            client
                .request(DaemonRequest::Terminal {
                    action: TerminalAction::Input,
                    payload: serde_json::json!({}),
                })
                .unwrap(),
            ok_reply()
        );
        assert_eq!(counters.connects.get(), 2);
        assert_eq!(counters.exchanges.get(), 1);
    }

    #[test]
    fn a_reused_connection_is_rearmed_and_serves_without_reconnecting() {
        let counters = Counters::default();
        let mut client = policy_client(
            ClientPolicy::mcp(),
            vec![],
            vec![Ok(ok_reply())],
            true,
            &counters,
        );
        assert_eq!(
            client
                .request(DaemonRequest::AgentInventory {
                    workspace: WorkspaceId::new(),
                })
                .unwrap(),
            ok_reply()
        );
        assert_eq!(counters.rearms.get(), 1);
        assert_eq!(counters.connects.get(), 0);
        assert!(client.session.is_some());
    }

    #[test]
    fn a_reused_connection_reconnects_only_after_a_transport_failure() {
        let counters = Counters::default();
        let mut client = policy_client(
            ClientPolicy::tui(),
            vec![Ok(())],
            vec![Err(transport_error()), Ok(ok_reply())],
            true,
            &counters,
        );
        assert_eq!(
            client
                .request(DaemonRequest::Session {
                    action: SessionAction::Remove,
                    operation_id: "op".into(),
                    payload: serde_json::json!({}),
                })
                .unwrap(),
            ok_reply()
        );
        // Reused session rearmed once, its exchange stalled, then one reconnect.
        assert_eq!(counters.rearms.get(), 1);
        assert_eq!(counters.connects.get(), 1);
        assert_eq!(counters.exchanges.get(), 2);
    }

    // ---- Eligibility classification table ---------------------------------

    #[test]
    #[allow(clippy::too_many_lines)]
    fn retry_eligibility_follows_the_request_class_table() {
        use RetryEligibility::{DurableOperation, NoCrossConnectionEvidence, ReadOnly};
        let session_payload = || serde_json::json!({});
        let read_only = [
            DaemonRequest::Pr {
                action: PrAction::Snapshot,
                payload: PrRequest {
                    session_id: SessionId::new(),
                    revision: None,
                },
            },
            DaemonRequest::Metrics {
                action: MetricsAction::Snapshot,
            },
            DaemonRequest::AgentInventory {
                workspace: WorkspaceId::new(),
            },
            DaemonRequest::Session {
                action: SessionAction::List,
                operation_id: String::new(),
                payload: session_payload(),
            },
            DaemonRequest::DispatchTool {
                action: DispatchToolAction::AgentList,
                operation_id: String::new(),
                payload: session_payload(),
                caller_context: None,
            },
            DaemonRequest::SupervisorTool {
                action: SupervisorToolAction::List,
                operation_id: String::new(),
                payload: session_payload(),
            },
            DaemonRequest::UserDecision {
                action: TuiUserDecisionAction::List,
                payload: session_payload(),
            },
        ];
        for request in &read_only {
            assert_eq!(RetryEligibility::classify(request), ReadOnly, "{request:?}");
        }

        let durable = [
            DaemonRequest::Session {
                action: SessionAction::Create,
                operation_id: "op".into(),
                payload: session_payload(),
            },
            DaemonRequest::DispatchTool {
                action: DispatchToolAction::Dispatch,
                operation_id: "op".into(),
                payload: session_payload(),
                caller_context: None,
            },
            DaemonRequest::SupervisorTool {
                action: SupervisorToolAction::Start,
                operation_id: "op".into(),
                payload: session_payload(),
            },
            DaemonRequest::Agent {
                operation_id: "op".into(),
                intent: AgentLaunchIntent {
                    workspace: WorkspaceId::new(),
                    session: None,
                    profile: None,
                },
            },
        ];
        for request in &durable {
            assert_eq!(
                RetryEligibility::classify(request),
                DurableOperation,
                "{request:?}"
            );
        }

        let ineligible = [
            DaemonRequest::Session {
                action: SessionAction::Prompt,
                operation_id: "op".into(),
                payload: session_payload(),
            },
            DaemonRequest::DispatchTool {
                action: DispatchToolAction::AgentComplete,
                operation_id: "op".into(),
                payload: session_payload(),
                caller_context: None,
            },
            DaemonRequest::SupervisorTool {
                action: SupervisorToolAction::Cancel,
                operation_id: "op".into(),
                payload: session_payload(),
            },
            DaemonRequest::UserDecision {
                action: TuiUserDecisionAction::Resolve,
                payload: session_payload(),
            },
            DaemonRequest::Terminal {
                action: TerminalAction::Input,
                payload: session_payload(),
            },
        ];
        for request in &ineligible {
            assert_eq!(
                RetryEligibility::classify(request),
                NoCrossConnectionEvidence,
                "{request:?}"
            );
        }

        assert!(ReadOnly.may_retry_on_new_connection());
        assert!(DurableOperation.may_retry_on_new_connection());
        assert!(!NoCrossConnectionEvidence.may_retry_on_new_connection());
    }

    // ---- Deadline transport (fake clock) ----------------------------------

    /// An in-memory [`DeadlineConnection`] that serves scripted frame bytes and,
    /// once exhausted, stalls. Every read advances the fake clock so partial
    /// progress and event floods shrink the shared attempt budget.
    struct ScriptedConn {
        clock: FakeClock,
        readable: Cursor<Vec<u8>>,
        written: Vec<u8>,
        advance_per_read: u64,
        stall_advance: u64,
        stall_writes: bool,
    }
    impl ScriptedConn {
        fn new(clock: FakeClock, readable: Vec<u8>) -> Self {
            Self {
                clock,
                readable: Cursor::new(readable),
                written: Vec::new(),
                advance_per_read: 0,
                stall_advance: 1,
                stall_writes: false,
            }
        }
        fn advancing(mut self, per_read: u64, stall: u64) -> Self {
            self.advance_per_read = per_read;
            self.stall_advance = stall;
            self
        }
        fn stalling_writes(mut self) -> Self {
            self.stall_writes = true;
            self
        }
    }
    impl Read for ScriptedConn {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let read = self.readable.read(buf)?;
            if read == 0 {
                self.clock.advance(self.stall_advance);
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                self.clock.advance(self.advance_per_read);
                Ok(read)
            }
        }
    }
    impl Write for ScriptedConn {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.stall_writes {
                self.clock.advance(self.stall_advance);
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl DeadlineConnection for ScriptedConn {
        fn set_read_deadline(&mut self, _timeout: Duration) -> io::Result<()> {
            Ok(())
        }
        fn set_write_deadline(&mut self, _timeout: Duration) -> io::Result<()> {
            Ok(())
        }
    }

    fn test_build() -> BuildIdentity {
        BuildIdentity {
            version: "test".into(),
            commit: "test".into(),
            target: "test".into(),
            artifact: "client-artifact".into(),
        }
    }
    fn protocol() -> ProtocolVersion {
        ProtocolVersion {
            generation: 1,
            revision: 1,
        }
    }
    fn server_hello_bootstrap() -> Bootstrap {
        Bootstrap::ServerHello(ServerHello {
            connection_nonce: "n".into(),
            connection_id: ConnectionId("c".into()),
            daemon_generation: DaemonGeneration("d".into()),
            generation_role: GenerationRole::Active,
            protocol: protocol(),
            capabilities: vec![],
            build: test_build(),
            limits: ProtocolLimits::default(),
            daemon_process: None,
        })
    }
    fn server_hello_frame() -> Vec<u8> {
        let mut bytes = Vec::new();
        write_json_frame(&mut bytes, &server_hello_bootstrap(), 1_048_576).unwrap();
        bytes
    }
    fn response_frame(request_id: &str, outcome: ResponseOutcome) -> Vec<u8> {
        let mut bytes = Vec::new();
        let envelope = Envelope {
            protocol: protocol(),
            daemon_generation: DaemonGeneration("d".into()),
            kind: EnvelopeKind::Response {
                request_id: crate::infrastructure::ipc::RequestId(request_id.into()),
                outcome,
                body: serde_json::json!({"ok": true}),
            },
        };
        write_json_frame(&mut bytes, &envelope, 1_048_576).unwrap();
        bytes
    }
    fn connect_deadline(
        clock: FakeClock,
        conn: ScriptedConn,
        budget_ms: u64,
    ) -> Result<IpcClient<DeadlineStream<FakeClock, ScriptedConn>>, ClientError> {
        IpcClient::connect(
            DeadlineStream::new(clock, conn, budget_ms),
            "c".into(),
            "n".into(),
            ClientPolicy::tui(),
            test_build(),
        )
    }

    #[test]
    fn a_hello_stall_returns_a_bounded_unavailable() {
        let clock = FakeClock::default();
        let result = connect_deadline(
            clock.clone(),
            ScriptedConn::new(clock.clone(), vec![]).advancing(0, 5_000),
            2_000,
        );
        assert!(matches!(result, Err(ClientError::Unavailable(_))));
    }

    #[test]
    fn no_response_after_handshake_times_out() {
        let clock = FakeClock::default();
        let mut client = connect_deadline(
            clock.clone(),
            ScriptedConn::new(clock.clone(), server_hello_frame()).advancing(0, 5_000),
            2_000,
        )
        .unwrap();
        let error = client
            .request(DaemonRequest::Metrics {
                action: MetricsAction::Snapshot,
            })
            .unwrap_err();
        assert!(matches!(error, ClientError::Unavailable(_)));
    }

    #[test]
    fn a_write_stall_before_hello_returns_unavailable() {
        let clock = FakeClock::default();
        let result = IpcClient::connect(
            DeadlineStream::new(
                clock.clone(),
                ScriptedConn::new(clock.clone(), vec![])
                    .advancing(0, 5_000)
                    .stalling_writes(),
                2_000,
            ),
            "c".into(),
            "n".into(),
            ClientPolicy::tui(),
            test_build(),
        );
        assert!(matches!(result, Err(ClientError::Unavailable(_))));
    }

    #[test]
    fn a_partial_response_header_then_stall_times_out() {
        let clock = FakeClock::default();
        let mut readable = server_hello_frame();
        readable.extend_from_slice(&[0x00, 0x00]); // 2 of 4 length-prefix bytes, then nothing
        let mut client = connect_deadline(
            clock.clone(),
            ScriptedConn::new(clock.clone(), readable).advancing(0, 5_000),
            2_000,
        )
        .unwrap();
        let error = client
            .request(DaemonRequest::Metrics {
                action: MetricsAction::Snapshot,
            })
            .unwrap_err();
        assert!(matches!(error, ClientError::Unavailable(_)));
    }

    #[test]
    fn a_wrong_request_event_flood_cannot_extend_the_deadline() {
        let clock = FakeClock::default();
        let mut readable = server_hello_frame();
        for _ in 0..50 {
            readable.extend(response_frame("other", ResponseOutcome::Ok));
        }
        // Each read costs time; a never-ending flood still hits the deadline.
        let mut client = connect_deadline(
            clock.clone(),
            ScriptedConn::new(clock.clone(), readable).advancing(300, 300),
            2_000,
        )
        .unwrap();
        let error = client
            .request(DaemonRequest::Metrics {
                action: MetricsAction::Snapshot,
            })
            .unwrap_err();
        assert!(matches!(error, ClientError::Unavailable(_)));
        assert!(clock.now_ms() >= 2_000, "deadline was actually reached");
    }

    #[test]
    fn a_successful_exchange_runs_over_the_deadline_stream() {
        let clock = FakeClock::default();
        let mut readable = server_hello_frame();
        readable.extend(response_frame("1", ResponseOutcome::Ok));
        let mut client = connect_deadline(
            clock.clone(),
            ScriptedConn::new(clock.clone(), readable),
            10_000,
        )
        .unwrap();
        // Exercise the DaemonSession adapter (rearm + exchange) directly.
        DaemonSession::rearm(&mut client, 10_000);
        let reply = DaemonSession::exchange(
            &mut client,
            DaemonRequest::Metrics {
                action: MetricsAction::Snapshot,
            },
        )
        .unwrap();
        assert!(matches!(reply, DaemonReply::Ok(_)));
    }

    #[test]
    fn deadline_stream_rearm_and_accessors_track_the_budget() {
        let clock = FakeClock::default();
        let mut stream = DeadlineStream::new(
            clock.clone(),
            ScriptedConn::new(clock.clone(), vec![1, 2, 3, 4]),
            100,
        );
        assert!(stream.get_ref().written.is_empty());

        clock.advance(200); // past the deadline
        let mut buf = [0u8; 4];
        assert_eq!(
            stream.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            stream.write(b"x").unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );

        RearmableStream::rearm(&mut stream, 100); // now 200 + 100 = 300
        assert_eq!(stream.read(&mut buf).unwrap(), 4);
        assert_eq!(stream.write(b"ab").unwrap(), 2);
        assert!(stream.flush().is_ok());
        stream.get_mut().written.push(9);
        assert_eq!(stream.get_ref().written, b"ab\x09");
    }

    // ---- Real UnixStream pair + real clock --------------------------------

    #[cfg(unix)]
    mod unix_pair {
        use super::*;
        use std::os::unix::net::UnixStream;
        use std::thread;
        use std::time::Instant;

        #[derive(Clone)]
        struct RealClock {
            origin: Instant,
        }
        impl MonotonicClock for RealClock {
            fn now_ms(&self) -> u64 {
                u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
            }
        }
        struct UnixDeadline(UnixStream);
        impl Read for UnixDeadline {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl Write for UnixDeadline {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.flush()
            }
        }
        impl DeadlineConnection for UnixDeadline {
            fn set_read_deadline(&mut self, timeout: Duration) -> io::Result<()> {
                self.0.set_read_timeout(Some(timeout))
            }
            fn set_write_deadline(&mut self, timeout: Duration) -> io::Result<()> {
                self.0.set_write_timeout(Some(timeout))
            }
        }

        fn bounded_policy() -> ClientPolicy {
            ClientPolicy {
                timeout_ms: 200,
                reconnect_attempts: 0,
            }
        }

        #[test]
        fn unix_deadline_delegates_reads_writes_and_arming() {
            let (client_sock, mut peer) = UnixStream::pair().unwrap();
            let mut conn = UnixDeadline(client_sock);
            conn.set_read_deadline(Duration::from_millis(100)).unwrap();
            conn.set_write_deadline(Duration::from_millis(100)).unwrap();
            conn.write_all(b"hi").unwrap();
            conn.flush().unwrap();
            let mut received = [0u8; 2];
            peer.read_exact(&mut received).unwrap();
            assert_eq!(&received, b"hi");
            peer.write_all(b"yo").unwrap();
            let mut echoed = [0u8; 2];
            conn.read_exact(&mut echoed).unwrap();
            assert_eq!(&echoed, b"yo");
        }

        #[test]
        fn a_peer_that_stalls_before_hello_times_out_and_does_not_hang() {
            let (client_sock, _server_sock) = UnixStream::pair().unwrap();
            let clock = RealClock {
                origin: Instant::now(),
            };
            let started = Instant::now();
            let result = IpcClient::connect(
                DeadlineStream::new(clock, UnixDeadline(client_sock), 200),
                "c".into(),
                "n".into(),
                bounded_policy(),
                test_build(),
            );
            assert!(matches!(result, Err(ClientError::Unavailable(_))));
            assert!(started.elapsed() < Duration::from_secs(5));
            // `_server_sock` is held open until here so the write side is not a broken pipe.
        }

        #[test]
        fn a_peer_that_answers_hello_then_stalls_times_out_bounded() {
            let (client_sock, server_sock) = UnixStream::pair().unwrap();
            let server = thread::spawn(move || {
                let mut server = server_sock;
                // Consume the client hello, answer it, then never respond.
                read_frame(&mut server).unwrap();
                write_json_frame(&mut server, &server_hello_bootstrap(), 1_048_576).unwrap();
                thread::sleep(Duration::from_millis(500));
            });
            let clock = RealClock {
                origin: Instant::now(),
            };
            let mut client = IpcClient::connect(
                DeadlineStream::new(clock, UnixDeadline(client_sock), 200),
                "c".into(),
                "n".into(),
                bounded_policy(),
                test_build(),
            )
            .unwrap();
            let started = Instant::now();
            let error = client
                .request(DaemonRequest::Metrics {
                    action: MetricsAction::Snapshot,
                })
                .unwrap_err();
            assert!(matches!(error, ClientError::Unavailable(_)));
            assert!(started.elapsed() < Duration::from_secs(5));
            server.join().unwrap();
        }
    }
}
