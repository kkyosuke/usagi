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
use crate::domain::id::{AgentId, OperationId, SessionId, TerminalRef, WorkspaceId};
use crate::domain::pr_inventory::{PrEntry, PrInventory};
use crate::domain::session_lifecycle::AgentPhase;
use crate::domain::terminal_launch::{
    TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};
use crate::infrastructure::ipc::{
    Bootstrap, BuildIdentity, Capability, ClientHello, ClientId, ClientWorkspace, DaemonGeneration,
    Envelope, EnvelopeKind, ErrorCode, GenerationRole, ProtocolError, ProtocolRange,
    ProtocolVersion, ResponseOutcome, RetryMode, ServerHello, SideEffect,
    TERMINAL_CHECKPOINT_REVISION, TERMINAL_WIRE_GENERATION, TerminalInputReplayMode,
    TerminalSnapshotMode, client_advertised_capabilities, client_required_capabilities,
    is_workspace_mismatch, read_json_frame, terminal_input_replay_mode, terminal_snapshot_mode,
    write_json_frame,
};

#[cfg(test)]
use crate::infrastructure::ipc::{OWNER_GENERATION_ROUTING_CAPABILITY, WORKSPACE_FENCE_CAPABILITY};

/// A daemon request understood by every presentation surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// Ask the currently active daemon to hand authority to its verified
    /// standby. The old active drives the process-local admission barrier;
    /// clients only supply the durable operation identity.
    Rollover { operation_id: String },
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
    /// Private agent lifecycle phase report delivered by a documented provider
    /// hook. The opaque credential binds the report to one live daemon runtime;
    /// callers cannot name a runtime, session, path, or provider themselves,
    /// and the phase itself is a closed non-sensitive vocabulary.
    AgentPhaseReport {
        phase: AgentPhase,
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
        /// Opaque daemon-minted capability used to authenticate the durable
        /// caller scope. The daemon combines the resolved scope with the
        /// handshake client incarnation; neither value is sufficient alone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_context: Option<McpCallerContext>,
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
    /// Committed PTY output never scanned for PRs because the deferred
    /// projection queue was full.
    #[serde(default)]
    pub pr_projection_dropped_bytes: u64,
    /// Committed PTY output merged into an already queued projection chunk.
    #[serde(default)]
    pub pr_projection_coalesced_bytes: u64,
    /// Discontinuities recorded so a PR scan never joins across dropped bytes.
    #[serde(default)]
    pub pr_projection_gaps: u64,
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

/// The canonical semantic intent of one Agent launch.
///
/// This string, not the producer-issued `OperationId`, is what makes a launch
/// *mean* the same thing: the daemon conflicts a reused operation identity whose
/// key differs, and both sides derive the wire
/// [`agent_operation_digest`](crate::infrastructure::ipc::agent_operation_digest)
/// from it, so a client can refuse a final that belongs to another intent. It
/// lives here — beside the request it summarizes — as the single authority both
/// the daemon owner and every client compute from.
#[must_use]
pub fn agent_launch_semantic_key(intent: &AgentLaunchIntent) -> String {
    format!(
        "{}:{}:{}",
        intent.workspace.as_str(),
        intent
            .session
            .map_or_else(|| "workspace-root".to_owned(), |session| session.as_str()),
        intent
            .profile
            .as_ref()
            .map_or_else(|| "<default>".to_owned(), ToString::to_string),
    )
}

/// The prefix every exact-resume key of one scope shares.
///
/// A caller that only knows the scope — a legacy resume that lets the daemon
/// resolve the exact target — uses this to recognize a stored key as its own
/// scope's without re-deriving the key format itself.
#[must_use]
pub fn agent_resume_scope_prefix(workspace: WorkspaceId, session: Option<SessionId>) -> String {
    format!(
        "resume:{workspace}:{}:",
        session.map_or_else(|| "workspace-root".to_owned(), |session| session.as_str())
    )
}

/// The canonical semantic intent of one exact Agent resume.
///
/// The whole opaque target participates: a resume that names another
/// continuation, source, runtime, or adapter revision is a different intent even
/// under the same scope.
#[must_use]
pub fn agent_resume_semantic_key(target: &AgentResumeTarget) -> String {
    format!(
        "{}{}:{}:{}:{}:{}",
        agent_resume_scope_prefix(target.workspace_id, target.session_id),
        target.worktree_id,
        target.continuation,
        target.source,
        target.runtime_id,
        target.adapter_revision,
    )
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
    /// Read the recorded final outcome of one durable input operation without
    /// writing anything. It is the only way a client resolves an
    /// acknowledgement it lost, and it never converts an unknown operation into
    /// a PTY write (#519).
    InputOutcome,
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
    /// Producer-issued durable identity of this logical launch, carried
    /// unchanged from the UI effect that decided to open a terminal. The daemon
    /// keys its durable record on it, so a lost response, a reconnect, or a
    /// restart replays the same terminal instead of spawning a second one, and
    /// the same id with a different canonical intent is an idempotency conflict.
    /// Additive on the wire: a peer that predates it omits the field and keeps
    /// the previous server-issued identity (#518).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_operation: Option<OperationId>,
}

impl TerminalLaunchIntent {
    /// The canonical intent digest a repeated `launch_operation` must match.
    ///
    /// It covers exactly what makes two launches the same request: the trusted
    /// profile selector, the fully fenced scope, and the screen geometry. A
    /// different scope, profile, or geometry under the same producer id is a
    /// conflict rather than a replay.
    #[must_use]
    pub fn canonical_digest(&self) -> String {
        crate::domain::terminal_launch::canonical_launch_digest(
            &self.request,
            self.geometry.cols,
            self.geometry.rows,
        )
    }
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
        /// Ordering number local to this connection epoch's fresh subscription.
        /// A fresh epoch restarts it at zero, so it is never cross-connection
        /// operation identity.
        input_seq: u64,
        /// Producer-issued durable identity of this logical input, stable across
        /// request retry, reconnect, and reattach. Additive on the wire: a peer
        /// that predates the ledger simply omits it and keeps the
        /// connection-local sequence contract (#519).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_operation: Option<OperationId>,
        bytes: Vec<u8>,
    },
    /// Read the recorded final of one durable input operation. The response body
    /// is `{"outcome": "final", "ack": InputAck}` when the daemon still holds the
    /// record, and `{"outcome": "unknown"}` when it never saw it or its bounded
    /// ledger already released it. Unknown is a typed uncertainty, never a
    /// licence to write the bytes again.
    InputOutcome {
        terminal: TerminalRef,
        input_operation: OperationId,
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
    /// Another process held the cross-process bootstrap section for longer than
    /// this surface's bounded wait, so no connection was ever attempted.
    ///
    /// It is deliberately distinct from [`Self::Unavailable`]: a daemon may well
    /// be running and healthy, and the correct response is to try again shortly
    /// rather than to report the daemon as absent. No request was written, so
    /// the side effect is definitively none.
    BootstrapContended,
}

impl ClientError {
    #[must_use]
    pub fn retry_mode(&self) -> RetryMode {
        match self {
            Self::Protocol(error) => error.retry_mode,
            Self::Unavailable(_) | Self::Lifecycle(_) | Self::BootstrapContended => {
                RetryMode::Reconnect
            }
            Self::RolloverRequired(_) | Self::BuildIdentityUnavailable => RetryMode::Manual,
        }
    }

    #[must_use]
    pub fn side_effect(&self) -> SideEffect {
        match self {
            Self::Protocol(error) => error.side_effect,
            Self::Unavailable(_) | Self::Lifecycle(_) => SideEffect::PartialOrUnknown,
            Self::RolloverRequired(_)
            | Self::BuildIdentityUnavailable
            | Self::BootstrapContended => SideEffect::None,
        }
    }

    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Protocol(error) => error.code,
            Self::Unavailable(_) | Self::Lifecycle(_) | Self::BuildIdentityUnavailable => {
                ErrorCode::Unavailable
            }
            Self::RolloverRequired(_) | Self::BootstrapContended => ErrorCode::Busy,
        }
    }

    /// Whether this failure is a lost/timed-out request rather than a definitive
    /// server answer. Only transport failures consume the reconnect budget: a
    /// well-formed [`ProtocolError`] means the server responded, so the request
    /// is finished and must not be replayed on a fresh connection.
    #[must_use]
    pub fn is_transport_failure(&self) -> bool {
        // Bootstrap contention happens before any socket exists, so it is not a
        // lost request: nothing was dispatched and nothing needs discarding.
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
            Self::BootstrapContended => f.write_str(
                "BootstrapContended: another usagi process is establishing the daemon connection",
            ),
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
    /// Capabilities the daemon advertised in its hello. Kept because a
    /// capability, not the negotiated revision alone, decides whether snapshots
    /// may be treated as semantic checkpoints.
    server_capabilities: Vec<String>,
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
        workspace: ClientWorkspace,
    ) -> Result<Self, ClientError> {
        Self::connect_with(
            stream,
            client_id,
            connection_nonce,
            policy,
            build,
            workspace,
            None,
        )
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
        workspace: ClientWorkspace,
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
            workspace,
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
        workspace: ClientWorkspace,
        expected_owner: Option<ExpectedOwner<'_>>,
    ) -> Result<Self, ClientError> {
        let expected_nonce = connection_nonce.clone();
        let required_capabilities =
            client_required_capabilities(expected_owner.is_some(), &workspace);
        let hello = Bootstrap::ClientHello(ClientHello {
            client_id: ClientId(client_id),
            connection_nonce,
            expected_daemon_generation: expected_owner
                .as_ref()
                .map(|owner| (*owner.generation).clone()),
            supported_protocols: vec![ProtocolRange {
                generation: TERMINAL_WIRE_GENERATION,
                min_revision: 0,
                // Revision 2 carries the semantic screen checkpoint. An older
                // daemon still negotiates revision 1, which this client treats
                // as legacy (it never parses the raw tail).
                max_revision: TERMINAL_CHECKPOINT_REVISION,
            }],
            // Advertised, not required: this is what *this* client can do, and
            // it is the daemon that needs it — a rollover may only leave a
            // draining generation behind while every connected client can
            // still address it (#508).
            capabilities: client_advertised_capabilities(),
            required_capabilities,
            build,
            workspace: Some(workspace),
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
                    server_capabilities: hello.capabilities,
                    next_request: 0,
                    policy,
                })
            }
            // A workspace refusal is definitive and asserts nothing about
            // ownership, so it is surfaced verbatim even on the owner-fenced
            // path. Folding it into `ownership_unknown` would leave the caller
            // with an unactionable error for a mismatch it can fix by working in
            // the daemon's workspace.
            Some(Bootstrap::Error(error)) if is_workspace_mismatch(&error) => {
                Err(ClientError::Protocol(error))
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

    /// The generation this connection is bound to, as the daemon named it in the
    /// handshake.
    ///
    /// This is what lets a client key a connection by its owner without reading
    /// the generation registry: the peer that answered has already said which
    /// generation it is, so a lane held for a terminal can be matched against
    /// that terminal's `TerminalRef.daemon_generation` with no IO at all
    /// ([`owner_routing`](crate::usecase::owner_routing)).
    #[must_use]
    pub fn daemon_generation(&self) -> &DaemonGeneration {
        &self.daemon_generation
    }

    /// How this connection must treat terminal attach / resync snapshots.
    ///
    /// Derived from the negotiated protocol version **and** the daemon's
    /// advertised capabilities, so a daemon that cannot serve semantic
    /// checkpoints fails closed to a limited view instead of having its raw byte
    /// tail parsed.
    #[must_use]
    pub fn terminal_snapshot_mode(&self) -> TerminalSnapshotMode {
        terminal_snapshot_mode(self.protocol, &self.server_capabilities)
    }

    /// How this connection may resolve a terminal input whose acknowledgement
    /// was lost.
    ///
    /// Derived from the daemon's advertised capabilities, so a daemon without
    /// the durable operation ledger fails closed: the client keeps the
    /// uncertainty latched instead of sending the bytes a second time.
    #[must_use]
    pub fn terminal_input_replay_mode(&self) -> TerminalInputReplayMode {
        terminal_input_replay_mode(&self.server_capabilities)
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
        && Capability::DaemonOwnerIdentity.is_advertised_by(&hello.capabilities)
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
    fn rollover_request_round_trips_with_its_durable_operation() {
        let request = DaemonRequest::Rollover {
            operation_id: "build-rollover-v1-test".into(),
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "kind": "rollover",
                "operation_id": "build-rollover-v1-test"
            })
        );
        assert_eq!(
            serde_json::from_value::<DaemonRequest>(encoded).unwrap(),
            request
        );
    }

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

/// Per-request end-to-end deadline budgets for the TUI's terminal lanes.
///
/// A terminal lane is a *persistent* connection: it owns the attachments and the
/// exactly-once input ledger of every pane sharing it, so it cannot be handed to
/// the reconnecting [`PolicyClient`] — a transparent reconnect would silently
/// void subscriptions the panes still believe in. Instead the lane keeps one
/// connection and re-arms its deadline transport before every request, which
/// gives each request the same attempt-scoped budget guarantee without any
/// reconnect.
///
/// The connection itself is established under the surface [`ClientPolicy`]
/// budget, because opening it may have to bootstrap (and cold-start) a daemon.
/// Only the per-request budgets below are charged to the render thread, and they
/// are deliberately far smaller than `ClientPolicy::tui().timeout_ms`: a hung
/// daemon must cost one keystroke a fraction of a second, not two seconds.
///
/// | budget | actions | why this size |
/// |---|---|---|
/// | [`Self::POLL_MS`] | `Resume`, `Resize` | stateless and sub-millisecond in normal operation; a missed one only drops a frame |
/// | [`Self::INPUT_MS`] | `Input`, `InputOutcome`, `Detach` | a keystroke's PTY write plus its acknowledgement, and the read-only ledger query that resolves a lost one |
/// | [`Self::SNAPSHOT_MS`] | `Attach`, `Resync`, `Inventory`, `CompletedInventory`, `Observe`, `Dismiss` | serializes a screen checkpoint or scans a scope, so it is legitimately slower than a keystroke |
/// | [`Self::LAUNCH_MS`] | `Launch` | spawns a process; it runs on the per-request [`PolicyClient`] path, never on a lane |
///
/// Exceeding a budget is a transport failure: the socket may hold a partial
/// frame, so the lane is dropped and the client's connection epoch advances,
/// which is what makes every pane re-attach instead of trusting a subscription
/// the daemon no longer has.
///
/// That consequence is why these budgets are not as small as a frame. Dropping
/// the lane costs every pane a re-attach, and the keystrokes typed during that
/// window are refused with visible feedback rather than delivered late. A budget
/// tight enough to trip on a *busy* daemon would therefore lose real input on a
/// loaded machine. Each budget is sized above a healthy round trip under load and
/// below anything a user would call a freeze; shrinking the render thread's
/// exposure further is a frame-budget question, tracked by
/// [#551](../../../.usagi/issues/551-fix-tui-home-frame-loop-daemon-rpc.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalLaneBudget;

impl TerminalLaneBudget {
    /// Stateless poll of already-produced output, or a geometry change. The one
    /// action that may be this tight: nothing is lost by missing it, because the
    /// next frame simply asks again.
    pub const POLL_MS: u64 = 50;
    /// One keystroke's round trip, or the read-only resolution of one whose
    /// acknowledgement was lost.
    pub const INPUT_MS: u64 = 750;
    /// An atomic screen snapshot or a scope listing.
    pub const SNAPSHOT_MS: u64 = 1_000;
    /// A daemon-owned process spawn.
    pub const LAUNCH_MS: u64 = 2_000;
    /// Establishing (or re-establishing) a lane's own connection: one
    /// connect + handshake against an already-running daemon.
    ///
    /// This is one *attempt*, not a cold start — the readiness probe that follows
    /// a `daemon start` runs its own bounded retry loop, and each of its attempts
    /// gets a fresh budget — so a render-thread lane facing a daemon that listens
    /// but never completes a handshake spends this instead of the surface policy
    /// budget it used to inherit.
    pub const CONNECT_MS: u64 = 1_000;

    /// The budget one terminal action's request may spend end to end.
    #[must_use]
    pub const fn for_action(action: TerminalAction) -> u64 {
        match action {
            TerminalAction::Resume | TerminalAction::Resize => Self::POLL_MS,
            TerminalAction::Input | TerminalAction::InputOutcome | TerminalAction::Detach => {
                Self::INPUT_MS
            }
            TerminalAction::Attach
            | TerminalAction::Resync
            | TerminalAction::Inventory
            | TerminalAction::CompletedInventory
            | TerminalAction::Observe
            | TerminalAction::Dismiss => Self::SNAPSHOT_MS,
            TerminalAction::Launch => Self::LAUNCH_MS,
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
    /// Whether a read / write deadline is currently in effect on the transport.
    /// Set by the first successful arm, which happens while the peer is still
    /// connected, and never cleared: the deadline it installed keeps bounding
    /// this attempt even if a later re-arm is refused.
    read_armed: bool,
    write_armed: bool,
}

impl<Cl: MonotonicClock, C: DeadlineConnection> DeadlineStream<Cl, C> {
    /// Arms a fresh end-to-end deadline `budget_ms` from now over `inner`.
    #[must_use]
    pub fn new(clock: Cl, inner: C, budget_ms: u64) -> Self {
        let deadline_ms = clock.now_ms().saturating_add(budget_ms);
        let mut stream = Self {
            clock,
            inner,
            deadline_ms,
            read_armed: false,
            write_armed: false,
        };
        // Arm both directions immediately, while the peer is certainly still
        // connected. This is what makes a later re-arm safe to skip, and it is
        // best effort: a transport that refuses it here simply arms on first use.
        stream.arm_read().ok();
        stream.arm_write().ok();
        stream
    }

    /// Install the remaining budget for reads.
    ///
    /// A peer that answers and immediately closes makes `setsockopt` fail on some
    /// platforms (macOS returns `EINVAL` for a disconnected socket) even though
    /// the bytes it already sent are buffered and readable. Failing the read
    /// there would turn a definitive typed answer — the workspace-fence refusal
    /// is exactly such an answer, sent right before the daemon closes — into "the
    /// daemon is unavailable". So a refused re-arm keeps the deadline that is
    /// already in effect, and only a transport that has never been armed
    /// propagates the failure.
    fn arm_read(&mut self) -> io::Result<()> {
        let remaining = self.remaining()?;
        match self.inner.set_read_deadline(remaining) {
            Ok(()) => {
                self.read_armed = true;
                Ok(())
            }
            Err(_) if self.read_armed => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Install the remaining budget for writes, under the same contract as
    /// [`Self::arm_read`].
    fn arm_write(&mut self) -> io::Result<()> {
        let remaining = self.remaining()?;
        match self.inner.set_write_deadline(remaining) {
            Ok(()) => {
                self.write_armed = true;
                Ok(())
            }
            Err(_) if self.write_armed => Ok(()),
            Err(error) => Err(error),
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
        self.arm_read()?;
        self.inner.read(buf)
    }
}

impl<Cl: MonotonicClock, C: DeadlineConnection> Write for DeadlineStream<Cl, C> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.arm_write()?;
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
    /// (#518), terminal input, `RequestId`-only mutations, and Codex capture.
    /// After a request is dispatched the effect is unknown, so it is never
    /// blind-retried on a fresh connection. Terminal input keeps this
    /// classification even with a durable operation identity: the client
    /// resolves the lost acknowledgement with a read-only
    /// [`TerminalAction::InputOutcome`] query rather than by writing again.
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
            | DaemonRequest::AgentInventory { .. }
            // Resolving a durable input operation only reads the daemon's
            // ledger, so a lost response is safely re-read on a fresh
            // connection. Every other terminal action stays fail-closed below.
            | DaemonRequest::Terminal {
                action: TerminalAction::InputOutcome,
                ..
            } => Self::ReadOnly,
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
            DaemonRequest::Rollover { .. }
            | DaemonRequest::Agent { .. }
            | DaemonRequest::ResumeAgent { .. }
            | DaemonRequest::Dispatch { .. } => Self::DurableOperation,
            DaemonRequest::Terminal { .. }
            | DaemonRequest::CodexSessionCapture { .. }
            | DaemonRequest::AgentPhaseReport { .. } => Self::NoCrossConnectionEvidence,
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

    /// The workspace a test client declares. Bound, so the hello also carries the
    /// required workspace-fence capability like every production surface.
    fn test_workspace() -> ClientWorkspace {
        ClientWorkspace::Bound {
            root: "/workspace".into(),
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
            capabilities: vec![Capability::DaemonOwnerIdentity.wire_name().into()],
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
                test_workspace(),
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

    #[test]
    fn a_workspace_bound_client_declares_its_workspace_and_requires_the_fence() {
        let client = IpcClient::connect(
            bootstrap_script(&Bootstrap::ServerHello(ServerHello {
                connection_nonce: "nonce".into(),
                connection_id: crate::infrastructure::ipc::ConnectionId("connection".into()),
                daemon_generation: DaemonGeneration("daemon".into()),
                generation_role: GenerationRole::Active,
                protocol: ProtocolVersion {
                    generation: TERMINAL_WIRE_GENERATION,
                    revision: TERMINAL_CHECKPOINT_REVISION,
                },
                capabilities: vec![],
                build: client_build(),
                limits: crate::infrastructure::ipc::ProtocolLimits::default(),
                daemon_process: None,
            })),
            "client".into(),
            "nonce".into(),
            ClientPolicy::tui(),
            client_build(),
            test_workspace(),
        )
        .unwrap();

        let sent = read_json_frame::<serde_json::Value>(
            &mut Cursor::new(client.transport().output.clone()),
            1_048_576,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            sent["workspace"],
            serde_json::json!({
                "scope": "bound",
                "root": "/workspace",
            })
        );
        // The fence is required, so a daemon that does not enforce it cannot
        // silently serve this client another workspace's state.
        let required = sent["required_capabilities"].as_array().unwrap();
        assert!(required.contains(&serde_json::json!(WORKSPACE_FENCE_CAPABILITY)));
        // Owner-generation routing is advertised rather than required: it is the
        // daemon that consults it before leaving a draining generation behind.
        assert!(
            sent["capabilities"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(OWNER_GENERATION_ROUTING_CAPABILITY))
        );
        assert!(!required.contains(&serde_json::json!(OWNER_GENERATION_ROUTING_CAPABILITY)));

        // An unbound connection names no workspace resource, so it does not
        // require the fence and stays usable against any daemon.
        let unbound = IpcClient::connect(
            bootstrap_script(&Bootstrap::ServerHello(owner_hello(
                &DaemonRecord::identified(4321, "process-start"),
                &DaemonGeneration("generation".into()),
            ))),
            "client".into(),
            "nonce".into(),
            ClientPolicy::tui(),
            client_build(),
            ClientWorkspace::Unbound,
        )
        .unwrap();
        let sent = read_json_frame::<serde_json::Value>(
            &mut Cursor::new(unbound.transport().output.clone()),
            1_048_576,
        )
        .unwrap()
        .unwrap();
        assert_eq!(sent["workspace"], serde_json::json!({"scope": "unbound"}));
        assert!(
            !sent["required_capabilities"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(WORKSPACE_FENCE_CAPABILITY))
        );

        // A client that selected a workspace to open declares that workspace and
        // requires the fence for the same reason: without it the daemon would
        // answer with the sessions of the workspace it happens to serve.
        let selected = IpcClient::connect(
            bootstrap_script(&Bootstrap::ServerHello(owner_hello(
                &DaemonRecord::identified(4321, "process-start"),
                &DaemonGeneration("generation".into()),
            ))),
            "client".into(),
            "nonce".into(),
            ClientPolicy::tui(),
            client_build(),
            ClientWorkspace::Selected {
                root: "/workspace".into(),
            },
        )
        .unwrap();
        let sent = read_json_frame::<serde_json::Value>(
            &mut Cursor::new(selected.transport().output.clone()),
            1_048_576,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            sent["workspace"],
            serde_json::json!({"scope": "selected", "root": "/workspace"})
        );
        assert!(
            sent["required_capabilities"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(WORKSPACE_FENCE_CAPABILITY))
        );
    }

    #[test]
    fn a_workspace_refusal_survives_the_owner_fenced_handshake_verbatim() {
        // The owner path folds pre-authentication failures into
        // `ownership_unknown`, but a workspace refusal must reach the caller as
        // itself: it is definitive, asserts nothing about ownership, and is the
        // only error the user can act on by working in the daemon's workspace.
        let record = DaemonRecord::identified(4321, "process-start");
        let generation = DaemonGeneration("generation".into());
        let refusal = crate::infrastructure::ipc::workspace_admission(
            Some(&ClientWorkspace::Bound {
                root: "/workspace/other".into(),
            }),
            "/workspace/root",
        )
        .unwrap_err();

        let error = IpcClient::connect_expected_owner(
            bootstrap_script(&Bootstrap::Error(refusal.clone())),
            "client".into(),
            "nonce".into(),
            ClientPolicy::cli(),
            client_build(),
            test_workspace(),
            &record,
            &generation,
            record.pid,
            DaemonProcessObservation::Exact,
        )
        .err()
        .unwrap();
        assert_eq!(error.code(), ErrorCode::PermissionDenied);
        assert_eq!(error.side_effect(), SideEffect::None);
        assert_eq!(error.retry_mode(), RetryMode::Never);
        let mut surfaced = None;
        if let ClientError::Protocol(error) = error {
            surfaced = Some(error);
        }
        assert_eq!(
            surfaced.expect("the refusal stays a typed protocol error"),
            refusal
        );
    }

    #[test]
    fn client_advertises_the_checkpoint_revision_and_derives_its_snapshot_mode() {
        use crate::infrastructure::ipc::TERMINAL_SCREEN_CHECKPOINT_CAPABILITY;

        let peer = |revision: u16, capabilities: Vec<String>| {
            Bootstrap::ServerHello(ServerHello {
                connection_nonce: "nonce".into(),
                connection_id: crate::infrastructure::ipc::ConnectionId("connection".into()),
                daemon_generation: DaemonGeneration("daemon".into()),
                generation_role: GenerationRole::Active,
                protocol: ProtocolVersion {
                    generation: TERMINAL_WIRE_GENERATION,
                    revision,
                },
                capabilities,
                build: client_build(),
                limits: crate::infrastructure::ipc::ProtocolLimits::default(),
                daemon_process: None,
            })
        };
        let connect = |message: &Bootstrap| {
            IpcClient::connect(
                bootstrap_script(message),
                "client".into(),
                "nonce".into(),
                ClientPolicy::tui(),
                client_build(),
                test_workspace(),
            )
            .unwrap()
        };

        let checkpoint = connect(&peer(
            TERMINAL_CHECKPOINT_REVISION,
            vec![TERMINAL_SCREEN_CHECKPOINT_CAPABILITY.into()],
        ));
        assert_eq!(
            checkpoint.terminal_snapshot_mode(),
            TerminalSnapshotMode::Checkpoint
        );

        // The client offers revision 2 so a checkpoint daemon can select it.
        let sent = read_json_frame::<serde_json::Value>(
            &mut Cursor::new(checkpoint.transport().output.clone()),
            1_048_576,
        )
        .unwrap()
        .unwrap();
        assert_eq!(sent["kind"], "client_hello");
        assert_eq!(
            sent["supported_protocols"],
            serde_json::json!([ProtocolRange {
                generation: TERMINAL_WIRE_GENERATION,
                min_revision: 0,
                max_revision: TERMINAL_CHECKPOINT_REVISION,
            }])
        );

        // An older daemon (revision 1) and an advertisement gap both fail closed.
        for legacy in [
            peer(1, vec![TERMINAL_SCREEN_CHECKPOINT_CAPABILITY.into()]),
            peer(TERMINAL_CHECKPOINT_REVISION, vec![]),
        ] {
            assert_eq!(
                connect(&legacy).terminal_snapshot_mode(),
                TerminalSnapshotMode::LegacyFailClosed
            );
        }
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

    /// Bootstrap contention is a distinct, effect-free, retryable answer: no
    /// socket existed, so nothing was dispatched and nothing needs discarding.
    /// Collapsing it into `Unavailable` would tell a surface the daemon is gone
    /// when it is merely busy being connected to by someone else.
    #[test]
    fn bootstrap_contention_is_busy_effect_free_and_not_a_transport_failure() {
        let error = ClientError::BootstrapContended;
        assert_eq!(error.code(), ErrorCode::Busy);
        assert_eq!(error.retry_mode(), RetryMode::Reconnect);
        assert_eq!(error.side_effect(), SideEffect::None);
        assert!(!error.is_transport_failure());
        assert_ne!(error, ClientError::Unavailable(String::new()));
        assert!(error.to_string().starts_with("BootstrapContended:"));
    }

    /// The render thread's budgets are the point of the lane split: a keystroke
    /// or a tab switch must cost a fraction of the surface policy budget, and a
    /// stateless poll must cost less again. The connection budget stays the
    /// surface policy's, because opening a lane may have to cold-start a daemon.
    #[test]
    fn terminal_lane_budgets_are_ordered_and_far_below_the_surface_policy() {
        use TerminalAction::{
            Attach, CompletedInventory, Detach, Dismiss, Input, InputOutcome, Inventory, Launch,
            Observe, Resize, Resume, Resync,
        };

        const {
            assert!(TerminalLaneBudget::POLL_MS < TerminalLaneBudget::INPUT_MS);
            assert!(TerminalLaneBudget::INPUT_MS < TerminalLaneBudget::SNAPSHOT_MS);
            assert!(TerminalLaneBudget::SNAPSHOT_MS < ClientPolicy::tui().timeout_ms);
            assert!(TerminalLaneBudget::LAUNCH_MS == ClientPolicy::tui().timeout_ms);
            // Re-establishing a lane against a daemon that listens but never
            // completes a handshake must not cost the render thread the surface
            // policy budget it used to inherit.
            assert!(TerminalLaneBudget::CONNECT_MS < ClientPolicy::tui().timeout_ms);
            assert!(TerminalLaneBudget::CONNECT_MS >= TerminalLaneBudget::SNAPSHOT_MS);
        }

        for action in [Resume, Resize] {
            assert_eq!(
                TerminalLaneBudget::for_action(action),
                TerminalLaneBudget::POLL_MS
            );
        }
        for action in [Input, InputOutcome, Detach] {
            assert_eq!(
                TerminalLaneBudget::for_action(action),
                TerminalLaneBudget::INPUT_MS
            );
        }
        for action in [
            Attach,
            Resync,
            Inventory,
            CompletedInventory,
            Observe,
            Dismiss,
        ] {
            assert_eq!(
                TerminalLaneBudget::for_action(action),
                TerminalLaneBudget::SNAPSHOT_MS
            );
        }
        assert_eq!(
            TerminalLaneBudget::for_action(Launch),
            TerminalLaneBudget::LAUNCH_MS
        );

        // Resolving a lost input acknowledgement is the only lane action the
        // retry table lets a fresh connection replay; the write itself is not.
        assert!(
            RetryEligibility::classify(&DaemonRequest::Terminal {
                action: InputOutcome,
                payload: Value::Null,
            })
            .may_retry_on_new_connection()
        );
        assert!(
            !RetryEligibility::classify(&DaemonRequest::Terminal {
                action: Input,
                payload: Value::Null,
            })
            .may_retry_on_new_connection()
        );
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
            test_workspace(),
        )
        .unwrap();
        assert_eq!(client.server_build().version, "test");
        // The connection knows which generation answered it. That is what lets a
        // client hold one lane per owner and match a terminal against it without
        // reading the generation registry per request
        // ([`owner_routing`](crate::usecase::owner_routing)).
        assert_eq!(client.daemon_generation().0, "daemon");
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
                test_workspace(),
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
                client_build(),
                test_workspace(),
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
                client_build(),
                test_workspace(),
            ),
            Err(ClientError::Unavailable(_))
        ));
        assert!(matches!(
            IpcClient::connect(
                Broken,
                "c".into(),
                "n".into(),
                ClientPolicy::tui(),
                client_build(),
                test_workspace(),
            ),
            Err(ClientError::Unavailable(_))
        ));
        assert!(matches!(
            IpcClient::connect(
                ReadFails { output: vec![] },
                "c".into(),
                "n".into(),
                ClientPolicy::tui(),
                client_build(),
                test_workspace(),
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
            server_capabilities: Vec::new(),
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
            server_capabilities: Vec::new(),
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
            server_capabilities: Vec::new(),
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
                caller_context: None,
            },
            DaemonRequest::UserDecision {
                action: TuiUserDecisionAction::List,
                payload: session_payload(),
            },
            // Resolving a durable input operation only reads the daemon's
            // ledger, so losing its response is safely re-read (#519).
            DaemonRequest::Terminal {
                action: TerminalAction::InputOutcome,
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
                caller_context: None,
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
                caller_context: None,
            },
            DaemonRequest::UserDecision {
                action: TuiUserDecisionAction::Resolve,
                payload: session_payload(),
            },
            DaemonRequest::Terminal {
                action: TerminalAction::Input,
                payload: session_payload(),
            },
            DaemonRequest::AgentPhaseReport {
                phase: AgentPhase::Waiting,
                caller_context: McpCallerContext {
                    credential: "runtime-secret".into(),
                },
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

    /// The workspace a test client declares. Bound, so the hello also carries the
    /// required workspace-fence capability like every production surface.
    fn test_workspace() -> ClientWorkspace {
        ClientWorkspace::Bound {
            root: "/workspace".into(),
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
            test_workspace(),
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
            test_workspace(),
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

    #[test]
    fn a_transport_that_refuses_to_rearm_keeps_the_answer_it_already_holds() {
        /// A connection that accepts the first arm and refuses every later one,
        /// like a Unix socket whose peer answered and closed (macOS returns
        /// `EINVAL` from `setsockopt` once the socket is disconnected, while the
        /// bytes it already sent stay readable).
        struct ClosesAfterAnswering {
            readable: Cursor<Vec<u8>>,
            arms: usize,
            arms_allowed: usize,
        }
        impl Read for ClosesAfterAnswering {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.readable.read(buf)
            }
        }
        impl Write for ClosesAfterAnswering {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl DeadlineConnection for ClosesAfterAnswering {
            fn set_read_deadline(&mut self, _timeout: Duration) -> io::Result<()> {
                self.arms += 1;
                if self.arms > self.arms_allowed {
                    return Err(io::Error::from_raw_os_error(22));
                }
                Ok(())
            }
            fn set_write_deadline(&mut self, timeout: Duration) -> io::Result<()> {
                self.set_read_deadline(timeout)
            }
        }

        // The construction arm succeeds, so the deadline it installed still bounds
        // this attempt and the refused re-arm must not discard the peer's answer.
        let clock = FakeClock::default();
        let mut stream = DeadlineStream::new(
            clock.clone(),
            ClosesAfterAnswering {
                readable: Cursor::new(b"refused".to_vec()),
                arms: 0,
                arms_allowed: 2,
            },
            100,
        );
        let mut buf = [0u8; 7];
        assert_eq!(stream.read(&mut buf).unwrap(), 7);
        assert_eq!(&buf, b"refused");
        assert_eq!(stream.write(b"x").unwrap(), 1);
        assert!(stream.flush().is_ok());

        // The deadline itself still applies: an exhausted budget fails closed
        // rather than reading without one.
        clock.advance(200);
        assert_eq!(
            stream.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );

        // A transport that could never be armed has no deadline in effect, so the
        // failure propagates instead of leaving an unbounded read.
        let mut never = DeadlineStream::new(
            FakeClock::default(),
            ClosesAfterAnswering {
                readable: Cursor::new(b"refused".to_vec()),
                arms: 0,
                arms_allowed: 0,
            },
            100,
        );
        assert_eq!(never.read(&mut buf).unwrap_err().raw_os_error(), Some(22),);
        assert_eq!(never.write(b"x").unwrap_err().raw_os_error(), Some(22));
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
                test_workspace(),
            );
            assert!(matches!(result, Err(ClientError::Unavailable(_))));
            assert!(started.elapsed() < Duration::from_secs(5));
            // `_server_sock` is held open until here so the write side is not a broken pipe.
        }

        /// The workspace-fence refusal is written immediately before the daemon
        /// closes the connection, so the client reads it off a socket whose peer
        /// is already gone. On macOS re-arming the read deadline of such a socket
        /// fails, which used to turn this definitive answer into "the daemon is
        /// unavailable" — and with it every workspace-mismatch message.
        #[test]
        fn a_refusal_written_just_before_the_peer_closes_is_still_read() {
            let refusal = crate::infrastructure::ipc::workspace_admission(
                Some(&ClientWorkspace::Selected {
                    root: "/workspace/other".into(),
                }),
                "/workspace/root",
            )
            .unwrap_err();

            let (client_sock, server_sock) = UnixStream::pair().unwrap();
            let clock = RealClock {
                origin: Instant::now(),
            };
            // Armed while the peer is connected, exactly as a client does right
            // after connecting and before it writes its hello.
            let mut stream = DeadlineStream::new(clock, UnixDeadline(client_sock), 200);

            let mut server = server_sock;
            write_json_frame(&mut server, &Bootstrap::Error(refusal.clone()), 1_048_576).unwrap();
            drop(server);

            let answer = read_json_frame::<Bootstrap>(&mut stream, 1_048_576).unwrap();
            let mut surfaced = None;
            if let Some(Bootstrap::Error(error)) = answer {
                surfaced = Some(error);
            }
            assert_eq!(surfaced, Some(refusal));
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
                test_workspace(),
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

    #[test]
    fn the_launch_intent_carries_a_producer_id_additively_and_digests_its_intent() {
        use crate::domain::terminal_launch::{
            TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
        };
        let request = TerminalLaunchRequest {
            profile_id: TerminalProfileId::new("login-shell").unwrap(),
            scope: TerminalLaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: None,
                worktree_id: crate::domain::id::WorktreeId::new(),
            },
        };
        let anonymous = TerminalLaunchIntent {
            request: request.clone(),
            geometry: TerminalGeometry { cols: 80, rows: 24 },
            launch_operation: None,
        };
        let json = serde_json::to_value(&anonymous).unwrap();
        assert!(
            json.get("launch_operation").is_none(),
            "a peer without a producer id sends the previous wire shape"
        );
        assert_eq!(
            serde_json::from_value::<TerminalLaunchIntent>(json).unwrap(),
            anonymous
        );

        let operation = OperationId::new();
        let keyed = TerminalLaunchIntent {
            launch_operation: Some(operation),
            ..anonymous.clone()
        };
        let round_trip: TerminalLaunchIntent =
            serde_json::from_value(serde_json::to_value(&keyed).unwrap()).unwrap();
        assert_eq!(round_trip.launch_operation, Some(operation));
        // The producer id is not part of the intent's identity: the same request
        // under two ids digests the same, and a changed geometry does not.
        assert_eq!(keyed.canonical_digest(), anonymous.canonical_digest());
        let resized = TerminalLaunchIntent {
            geometry: TerminalGeometry {
                cols: 100,
                rows: 24,
            },
            ..keyed
        };
        assert_ne!(resized.canonical_digest(), anonymous.canonical_digest());
    }

    /// #522: the canonical semantic key is what a daemon and its clients must
    /// agree on to correlate an Agent final, so every part of the intent that
    /// changes the meaning of a launch changes the key.
    #[test]
    fn the_agent_launch_semantic_key_covers_the_whole_intent() {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let intent = AgentLaunchIntent {
            workspace,
            session: Some(session),
            profile: None,
        };
        let key = agent_launch_semantic_key(&intent);
        assert_eq!(key, agent_launch_semantic_key(&intent.clone()));
        assert!(key.contains(&workspace.as_str()));
        assert!(key.contains(&session.as_str()));
        assert!(key.ends_with("<default>"));

        // A workspace-root launch, another scope, and another profile are each a
        // different intent under the same producer identity.
        for other in [
            AgentLaunchIntent {
                session: None,
                ..intent.clone()
            },
            AgentLaunchIntent {
                session: Some(SessionId::new()),
                ..intent.clone()
            },
            AgentLaunchIntent {
                workspace: WorkspaceId::new(),
                ..intent.clone()
            },
            AgentLaunchIntent {
                profile: Some(AgentProfileId::new("codex").unwrap()),
                ..intent.clone()
            },
        ] {
            assert_ne!(key, agent_launch_semantic_key(&other), "{other:?}");
        }
    }

    /// #522: a resume names one exact interrupted source, so its key stays distinct
    /// from an ordinary launch in the same scope and from any other target.
    #[test]
    fn the_agent_resume_semantic_key_covers_the_exact_target() {
        use crate::domain::agent::AgentResumeTarget;
        use crate::domain::id::{
            AgentContinuationRef, AgentResumeSourceId, AgentRuntimeId, WorktreeId,
        };

        let target = AgentResumeTarget {
            continuation: AgentContinuationRef::new(),
            source: AgentResumeSourceId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
            runtime_id: AgentRuntimeId::new(),
            adapter_revision: 3,
        };
        let key = agent_resume_semantic_key(&target);
        assert!(key.starts_with("resume:"));
        assert_eq!(key, agent_resume_semantic_key(&target.clone()));
        assert_ne!(
            key,
            agent_launch_semantic_key(&AgentLaunchIntent {
                workspace: target.workspace_id,
                session: target.session_id,
                profile: None,
            })
        );
        for other in [
            AgentResumeTarget {
                runtime_id: AgentRuntimeId::new(),
                ..target.clone()
            },
            AgentResumeTarget {
                adapter_revision: 4,
                ..target.clone()
            },
            AgentResumeTarget {
                session_id: None,
                ..target.clone()
            },
        ] {
            assert_ne!(key, agent_resume_semantic_key(&other), "{other:?}");
        }
    }
}
