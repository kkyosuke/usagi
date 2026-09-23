//! daemon IPC の request / reply 語彙。
//!
//! ここにあるのは wire 契約そのもの（`serde` で (de)serialize される typed request、
//! reply、typed failure と、それらから派生する pure な semantic key）だけである。
//! 接続の張り方・retry・deadline といった「どう運ぶか」は
//! [`crate::infrastructure::client`] が持ち、この module からは参照しない。
//!
//! 契約と接続実装を別ファイルに分けているのは、protocol を変える変更と transport を
//! 変える変更が同じ review に混ざらないようにするためである。

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::agent::{
    AgentIntegrationRevision, AgentProfileId, AgentResumeTarget, CallerRef, ModelSelector,
    ProviderSessionId,
};
use crate::domain::id::{AgentId, OperationId, SessionId, TerminalRef, WorkspaceId};
use crate::domain::pr_inventory::{PrEntry, PrInventory};
use crate::domain::session_lifecycle::AgentPhase;
use crate::domain::terminal_launch::{TerminalLaunchRequest, TerminalLaunchScope};

use super::{ErrorCode, ProtocolError, RetryMode, SideEffect};

/// A daemon request understood by every presentation surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DaemonRequest {
    /// Claim the single private MCP channel for the caller's live Agent process
    /// group. The daemon derives the runtime from OS peer identity and returns
    /// only the process-local bearer used on this already established channel.
    McpChildClaim,
    /// Ask the currently active daemon to hand authority to its verified
    /// standby. The old active drives the process-local admission barrier and
    /// revalidates any explicit Agent selection; the client supplies no process
    /// handle or provider-native conversation identity.
    Rollover {
        operation_id: String,
        /// Explicit request to stop every live Agent inside the active-control
        /// barrier and exact-resume it after successor promotion.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        restart_agents: Option<DaemonRestartAgents>,
    },
    /// Observe or explicitly release one workspace held by the live daemon.
    /// This control surface is unbound because neither operation reads a
    /// caller-selected workspace resource.
    Tenant {
        action: TenantAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<String>,
        #[serde(default)]
        force: bool,
    },
    /// Revisioned daemon-owned PR inventory. Events are only hints; clients
    /// always converge by reading this snapshot.
    Pr {
        action: PrAction,
        payload: PrRequest,
    },
    /// Batch form used by resident projections so observing N sessions costs one
    /// IPC exchange rather than N sequential requests.
    PrBatch { payload: PrBatchRequest },
    /// Hide one exact PR identity for one stable session.
    PrDismiss { payload: PrDismissRequest },
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
    /// Opt-in workspace-root launch carrying one goal. Keeping this separate
    /// means old clients and classic launch semantics are unchanged.
    AgentGoal {
        operation_id: String,
        intent: AgentGoalIntent,
    },
    /// Private Codex `SessionStart` hook delivery. The opaque credential binds
    /// the provider-owned ID to one live daemon runtime; callers cannot name a
    /// runtime, session, path, or provider themselves.
    CodexSessionCapture {
        native_session_id: ProviderSessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_context: Option<McpCallerContext>,
    },
    /// Private agent lifecycle phase report delivered by a documented provider
    /// hook. The opaque credential binds the report to one live daemon runtime;
    /// callers cannot name a runtime, session, path, or provider themselves,
    /// and the phase itself is a closed non-sensitive vocabulary.
    AgentPhaseReport {
        phase: AgentPhase,
        /// Present only for a provider's structured starting event:
        /// `SessionStart` or Antigravity's `PreInvocation`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native_session_id: Option<ProviderSessionId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_context: Option<McpCallerContext>,
    },
    /// Read the safe Agent runtime and interrupted-source inventory for one
    /// workspace. Human callers see root and managed-session records together;
    /// an Agent caller sees only sessions it created.
    AgentInventory {
        workspace: WorkspaceId,
        /// Present only for an Agent-originated MCP request. Human TUI/CLI
        /// callers omit it and retain workspace-wide control.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_context: Option<McpCallerContext>,
    },
    /// Read the same runtime inventory together with daemon-authoritative
    /// per-session dispatch status. Process-level cross-project views use this
    /// instead of treating a coarse live PTY as proof that dispatch is running.
    AgentWorkspaceObservation { workspace: WorkspaceId },
    /// Read the redaction-safe durable Work Runs owned by the connection's
    /// workspace. This TUI-only observation never accepts an Agent credential.
    SupervisorSnapshot { workspace: WorkspaceId },
    WorkflowSnapshot {
        workspace: WorkspaceId,
        session: SessionId,
    },
    WorkflowControl {
        workspace: WorkspaceId,
        session: SessionId,
        operation_id: OperationId,
        command: crate::domain::workflow::WorkflowCommand,
    },
    /// Mutate one durable Supervisor Run through the workspace-bound human
    /// control plane. The daemon verifies the requested workspace against the
    /// connection and replays the command by its durable operation identity.
    SupervisorControl {
        workspace: WorkspaceId,
        operation_id: OperationId,
        command: crate::domain::supervisor::SupervisorWorkspaceCommand,
    },
    /// Diagnose launch-time hook/MCP integration revisions against the invoking
    /// binary without exposing rendered configuration or provider identity.
    DiagnoseAgents {
        workspace: WorkspaceId,
        expected: Vec<crate::domain::agent::AgentIntegrationRevision>,
    },
    /// Plan a machine-wide Agent restart without changing process state.
    /// The later rollover must present this exact runtime selection again
    /// after closing active-control admission.
    PlanDaemonRestartAgents {
        expected: Vec<AgentIntegrationRevision>,
        force: bool,
    },
    /// Stop only daemon-owned Agents whose launch-time integration is older
    /// than the invoking binary. A reported running phase requires `force`;
    /// generic terminals are never part of this operation.
    RestartAgents {
        workspace: WorkspaceId,
        expected: Vec<crate::domain::agent::AgentIntegrationRevision>,
        runtimes: Vec<crate::domain::id::AgentRuntimeRef>,
        force: bool,
    },
    /// Resume exactly one interrupted runtime selected from `AgentInventory`.
    /// Agent-originated requests are limited to a session that caller created.
    ResumeAgent {
        operation_id: String,
        target: AgentResumeTarget,
        /// Present only for an Agent-originated MCP request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caller_context: Option<McpCallerContext>,
    },
    /// Repair-only exact resume. The old revision still fences the retained
    /// source, while the active daemon re-resolves hooks/MCP with its current
    /// profile revision.
    ResumeAgentWithCurrentIntegration {
        operation_id: String,
        target: AgentResumeTarget,
        expected_revision: u32,
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
        /// Opaque daemon-minted credential returned only to a claimed MCP child.
        /// It is authentication material, never caller identity.
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

/// Exact live-Agent selection carried by the rollover request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRestartAgents {
    pub expected: Vec<AgentIntegrationRevision>,
    pub runtimes: Vec<crate::domain::id::AgentRuntimeRef>,
    pub force: bool,
}

/// Operations on the live daemon's in-memory tenant registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantAction {
    Inventory,
    Retire,
}

/// Safe, bounded status data for one workspace currently held by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantSummary {
    pub root: String,
    pub sessions: usize,
    /// Runtime records which may still name a live process. Ownership-unknown
    /// records are included so status never reports a false zero.
    pub live_runtimes: usize,
}

/// The source-of-truth live tenant inventory, ordered by canonical root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantInventory {
    pub tenants: Vec<TenantSummary>,
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
}

/// A PR request names only a stable session and optional last known revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRequest {
    pub session_id: SessionId,
    pub revision: Option<u64>,
}

/// One bounded read of several session inventories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrBatchRequest {
    pub session_ids: Vec<SessionId>,
}

/// User-owned tombstone mutation for one canonical URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrDismissRequest {
    pub session_id: SessionId,
    pub url: String,
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
    AgentHandoff,
    AgentPeers,
    AgentMessage,
    AgentMessages,
    AgentMessageAck,
    SessionGet,
    AgentList,
    AgentGet,
    TerminalList,
    TerminalRead,
    AgentComplete,
    AgentFail,
    AgentInbox,
    AgentInboxAck,
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
                | Self::AgentPeers
                | Self::AgentMessages
                | Self::AgentList
                | Self::AgentGet
                | Self::TerminalList
                | Self::TerminalRead
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
        matches!(self, Self::Dispatch | Self::AgentHandoff)
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

/// How much of the daemon's Agent concurrency is in use, as the authority that
/// admits Agent launches sees it.
///
/// This is the **Agent runtime** pool only. It is neither the generic terminal
/// capacity nor a supervisor run's `ExecutionPolicy.max_concurrency`, and the two
/// numbers are never summed with another pool's. `in_use` counts exactly what
/// admission counts, so a client can tell "the next launch is refused" from
/// "there is room" without re-deriving the daemon's rule.
///
/// The pair travels as one object on purpose: a reader can never combine an
/// `in_use` from one sample with a `limit` from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConcurrency {
    /// Agent runtimes currently holding a concurrency slot.
    pub in_use: u32,
    /// Slots the daemon admits at a time.
    pub limit: u32,
}

impl AgentConcurrency {
    /// Whether the next Agent launch would be refused for concurrency.
    ///
    /// A `limit` of zero is saturated at any usage, which keeps a degenerate
    /// policy from reading as "there is room".
    #[must_use]
    pub const fn is_saturated(self) -> bool {
        self.in_use >= self.limit
    }

    /// Whether usage has reached `numerator / denominator` of the limit.
    ///
    /// The comparison is a cross-multiplication in `u64`, so it neither divides
    /// by a zero limit nor loses the last slot to integer truncation.
    #[must_use]
    pub fn reaches_fraction(self, numerator: u32, denominator: u32) -> bool {
        u64::from(self.in_use) * u64::from(denominator)
            >= u64::from(self.limit) * u64::from(numerator)
    }
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
    /// Agent concurrency as the daemon's admission authority sees it, or `None`
    /// when this daemon does not report it (a peer older than schema 3, or an
    /// authority that has not published a level yet). `None` is deliberately
    /// distinct from `in_use: 0`: a client must not draw "no Agent is running"
    /// from a daemon that simply said nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_concurrency: Option<AgentConcurrency>,
    /// Long-lived daemon workers that exited unexpectedly in this process.
    #[serde(default)]
    pub failed_background_workers: u8,
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

/// One bounded objective admitted as a workspace-root Director launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGoalIntent {
    pub workspace: WorkspaceId,
    pub profile: Option<AgentProfileId>,
    pub goal: String,
}

/// Maximum UTF-8 size of one goal accepted by the daemon.
pub const MAX_AGENT_GOAL_BYTES: usize = 16 * 1024;

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

/// Canonical idempotency meaning of one goal-driven launch. The goal is part of
/// the durable launch request, so reusing an operation for different text must
/// conflict even when workspace and provider are identical.
#[must_use]
pub fn agent_goal_semantic_key(intent: &AgentGoalIntent) -> String {
    let launch = AgentLaunchIntent {
        workspace: intent.workspace,
        session: None,
        profile: intent.profile.clone(),
    };
    format!(
        "{}\ngoal:{}:{}",
        agent_launch_semantic_key(&launch),
        intent.goal.len(),
        intent.goal
    )
}

/// Canonical idempotency meaning of a session dispatch after its exact worker
/// Agent has been reserved. Supervisor promotion stores the digest of this key
/// before spawn and compares it with the Agent admission at bind time.
#[must_use]
pub fn agent_dispatch_semantic_key(
    session_name: &str,
    worker_agent_id: AgentId,
    prompt: &str,
) -> String {
    format!("dispatch:{session_name}:{worker_agent_id}:{prompt}")
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
    /// Stop quiescent Agents in the session while retaining exact provider
    /// resume metadata and the worktree.
    Sleep,
    /// Inspect or remove Git resources absent from daemon lifecycle state.
    Clean,
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
    /// Start the session's implementation/review workflow on behalf of the
    /// human who is running this MCP client.
    WorkflowStart,
    /// Read one session's workflow progress.
    WorkflowStatus,
    /// Send one durable instruction to a running workflow.
    WorkflowInstruct,
    /// End one session's workflow so the session can start another.
    WorkflowFinish,
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
        /// The viewport this client is attaching with.
        ///
        /// A terminal is shared by every window attached to it and its single
        /// PTY takes the smallest of their viewports, so a claim on that shared
        /// size lives exactly as long as the attachment that stated it. Carrying
        /// it here lets one request state the claim and take the snapshot it
        /// produced, instead of a separate `Resize` round trip on every attach.
        /// Additive on the wire: a peer that predates it omits the field and
        /// states its viewport with `Resize` alone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        geometry: Option<TerminalGeometry>,
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
        /// connection-local sequence contract.
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
