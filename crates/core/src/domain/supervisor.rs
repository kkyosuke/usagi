//! Durable supervisor-run domain model and its pure reducer.
//!
//! This module deliberately contains no scheduler or policy interpretation.
//! It records facts admitted by those layers and makes invalid histories
//! unrepresentable in the persisted state.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

use crate::domain::{
    agent::InboxKind,
    id::{AgentRuntimeId, OperationId, SessionId, WorkspaceId, WorktreeId},
    pr_inventory::{GitHubRepository, canonicalize},
};

/// A `UUIDv7` identity for one never-reused supervisor run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SupervisorRunId(Uuid);

impl SupervisorRunId {
    #[must_use]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl fmt::Display for SupervisorRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.hyphenated())
    }
}
impl Serialize for SupervisorRunId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for SupervisorRunId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        let uuid = Uuid::parse_str(&value).map_err(de::Error::custom)?;
        if uuid.hyphenated().to_string() != value || uuid.get_version_num() != 7 {
            return Err(de::Error::custom(
                "supervisor run ID must be canonical UUIDv7",
            ));
        }
        Ok(Self(uuid))
    }
}

/// Opaque stable task identity.  Its spelling is never inferred from a session
/// name; callers may encode a provenance path in it if they need one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

pub const MAX_TASK_ID_BYTES: usize = 128;
pub const MAX_INITIAL_TASKS: usize = 128;
pub const MAX_TASK_DEPENDENCIES: usize = 128;
pub const MAX_SUPERVISOR_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_SUPERVISOR_REASON_BYTES: usize = 4 * 1024;
pub const MAX_SUPERVISOR_KEY_BYTES: usize = 256;
/// Maximum presentation-only Goal label stored in a run snapshot.
pub const MAX_SUPERVISOR_DISPLAY_LABEL_BYTES: usize = 96;
/// Maximum canonical pull-request URL retained as an untrusted verification
/// candidate. The value is never exposed by supervisor query projections.
pub const MAX_ARTIFACT_CANDIDATE_BYTES: usize = 2 * 1024;
/// Maximum task completions retained as handoff context in one Work Run.
pub const MAX_HANDOFF_CONTEXT_ENTRIES: usize = 64;
/// Maximum worker-authored summary retained for one handoff entry.
pub const MAX_HANDOFF_SUMMARY_BYTES: usize = 1024;
/// Maximum compact structured-artifact description retained per entry.
pub const MAX_HANDOFF_ARTIFACT_BYTES: usize = 2 * 1024;
/// Maximum context prefix synthesized into a newly delegated worker prompt.
pub const MAX_HANDOFF_PROMPT_BYTES: usize = 16 * 1024;
/// Maximum daemon-authoritative Work Runs in one workspace UI snapshot.
pub const MAX_SUPERVISOR_WORKSPACE_SNAPSHOT_RUNS: usize = 16;
/// Root plus every initially admitted task can contribute one exact checkout
/// revision to a Goal artifact expectation.
pub const MAX_ARTIFACT_EXPECTATION_HEADS: usize = MAX_INITIAL_TASKS + 1;
/// Artifact provider retries begin here and never exceed the maximum below.
pub const ARTIFACT_RETRY_BASE_SECONDS: i64 = 5;
pub const ARTIFACT_RETRY_MAX_SECONDS: i64 = 5 * 60;

/// Trusted repository and immutable revision an artifact must prove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ArtifactExpectationWire")]
pub struct ArtifactExpectation {
    repository: GitHubRepository,
    head_oid: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    alternate_head_oids: Vec<String>,
}

#[derive(Deserialize)]
struct ArtifactExpectationWire {
    repository: GitHubRepository,
    head_oid: String,
    #[serde(default)]
    alternate_head_oids: Vec<String>,
}

impl ArtifactExpectation {
    #[must_use]
    pub fn new(repository: GitHubRepository, head_oid: &str) -> Option<Self> {
        if !valid_artifact_head(head_oid) {
            return None;
        }
        Some(Self {
            repository,
            head_oid: head_oid.to_ascii_lowercase(),
            alternate_head_oids: Vec::new(),
        })
    }

    /// Builds one immutable expectation from every checkout which contributed
    /// to the supervised result. Duplicate spellings are normalized away.
    #[must_use]
    pub fn from_heads(repository: GitHubRepository, head_oids: &[String]) -> Option<Self> {
        if head_oids.len() > MAX_ARTIFACT_EXPECTATION_HEADS
            || head_oids.iter().any(|head| !valid_artifact_head(head))
        {
            return None;
        }
        let mut heads = head_oids
            .iter()
            .map(|head| head.to_ascii_lowercase())
            .collect::<BTreeSet<_>>();
        let head_oid = heads.pop_first()?;
        Some(Self {
            repository,
            head_oid,
            alternate_head_oids: heads.into_iter().collect(),
        })
    }

    #[must_use]
    pub const fn repository(&self) -> &GitHubRepository {
        &self.repository
    }

    #[must_use]
    pub fn head_oid(&self) -> &str {
        &self.head_oid
    }

    #[must_use]
    pub fn matches_head(&self, candidate: &str) -> bool {
        self.head_oid.eq_ignore_ascii_case(candidate)
            || self
                .alternate_head_oids
                .iter()
                .any(|head| head.eq_ignore_ascii_case(candidate))
    }

    pub fn head_oids(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.head_oid.as_str())
            .chain(self.alternate_head_oids.iter().map(String::as_str))
    }
}

impl TryFrom<ArtifactExpectationWire> for ArtifactExpectation {
    type Error = &'static str;

    fn try_from(wire: ArtifactExpectationWire) -> Result<Self, Self::Error> {
        if wire.alternate_head_oids.len() >= MAX_ARTIFACT_EXPECTATION_HEADS {
            return Err("invalid artifact expectation");
        }
        let mut heads = Vec::with_capacity(1 + wire.alternate_head_oids.len());
        heads.push(wire.head_oid);
        heads.extend(wire.alternate_head_oids);
        Self::from_heads(wire.repository, &heads).ok_or("invalid artifact expectation")
    }
}

fn valid_artifact_head(head: &str) -> bool {
    matches!(head.len(), 40 | 64) && head.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Closed vocabulary for independently verified task outputs.
///
/// The serialized spellings are the durable/wire contract. Keeping the set in
/// the domain makes unsupported contracts impossible to admit while adapters
/// remain responsible only for implementing a known variant.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactContract {
    #[default]
    None,
    GoalReviewReadyPullRequestV1,
}

impl ArtifactContract {
    pub const ALL: [Self; 2] = [Self::None, Self::GoalReviewReadyPullRequestV1];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::GoalReviewReadyPullRequestV1 => "goal_review_ready_pr_v1",
        }
    }

    #[must_use]
    pub const fn requires_verification(self) -> bool {
        !matches!(self, Self::None)
    }
}

impl fmt::Display for ArtifactContract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ArtifactContract {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ArtifactContract {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::ALL
            .into_iter()
            .find(|contract| contract.as_str() == value)
            .ok_or_else(|| de::Error::custom(format!("unsupported artifact contract: {value}")))
    }
}

/// Artifact contract used when a task has no independently verified output.
pub const NO_ARTIFACT_CONTRACT: ArtifactContract = ArtifactContract::None;
/// Contract for a Goal whose terminal condition is a review-ready pull request
/// with all required checks passing.
pub const GOAL_REVIEW_READY_ARTIFACT_CONTRACT: ArtifactContract =
    ArtifactContract::GoalReviewReadyPullRequestV1;

/// Whether text can be embedded in terminal presentation without changing
/// control flow or visual direction. Rendering layers still escape defensively;
/// this is the admission authority for values which are themselves UI labels.
#[must_use]
pub fn presentation_text_is_safe(value: &str) -> bool {
    value.chars().all(|character| {
        !character.is_control()
            && !matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{206f}'
            )
    })
}

impl TaskId {
    /// Creates an opaque task key.
    ///
    /// # Errors
    /// Returns [`SupervisorError::InvalidTaskId`] for an empty key, an unsafe
    /// presentation character, or one over [`MAX_TASK_ID_BYTES`] UTF-8 bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, SupervisorError> {
        let value = value.into();
        if value.trim().is_empty()
            || value.len() > MAX_TASK_ID_BYTES
            || !presentation_text_is_safe(&value)
        {
            return Err(SupervisorError::InvalidTaskId);
        }
        Ok(Self(value))
    }
}

impl<'de> Deserialize<'de> for TaskId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// Coarse run state.  Policy chooses *when* to emit these facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorRunState {
    Planning,
    Running,
    WaitingForDecision,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
    Escalated,
}
impl SupervisorRunState {
    /// Whether ordinary reducer events are fenced until an escalation is
    /// explicitly resolved. `Escalated` is quiescent but remains resumable.
    #[must_use]
    pub const fn blocks_ordinary_events(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Escalated
        )
    }

    /// Compatibility alias for callers compiled against the original API.
    /// This does not mean the run can be discarded: use [`Self::is_finished`]
    /// for retention and idempotency decisions.
    #[deprecated(note = "use blocks_ordinary_events or is_finished for the intended policy")]
    #[must_use]
    pub const fn terminal(self) -> bool {
        self.blocks_ordinary_events()
    }

    /// Whether the run is immutable history and can be removed by retention or
    /// have its start-idempotency reservation recycled.
    #[must_use]
    pub const fn is_finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// State of one node in a task DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Ready,
    Dispatched,
    Running,
    AwaitingDecision,
    Retrying,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
    Blocked,
}
impl TaskState {
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Blocked
        )
    }
}

/// A redaction-safe task contract.  The instruction body is kept durably for
/// workers, while query models expose only its digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskNode {
    pub task_id: TaskId,
    pub supervisor_run_id: SupervisorRunId,
    pub parent_task_id: Option<TaskId>,
    pub dependencies: BTreeSet<TaskId>,
    pub instruction_digest: String,
    pub instruction_body: String,
    pub required_artifact_contract: ArtifactContract,
    pub attempt: u64,
    pub generation: u64,
    pub assigned_dispatch_run: Option<OperationId>,
    /// Set only for a task durably reserved before Agent admission. It lets
    /// recovery distinguish a live in-flight promotion from an orphan.
    #[serde(default)]
    pub promotion_reserved_at: Option<DateTime<Utc>>,
    /// The deterministic retry deadline.  It is part of the aggregate rather
    /// than scheduler memory so a restart cannot make a retry run early.
    pub retry_at: Option<DateTime<Utc>>,
    /// A worker report is not evidence.  Tasks with a non-`none` contract are
    /// held in `Verifying` until an independently recorded result is accepted.
    pub verification_digest: Option<String>,
    /// Retry state for provider-unavailable verification. Both fields are
    /// durable so restart cannot retry early or forget exponential progress.
    #[serde(default)]
    pub verification_attempt: u32,
    #[serde(default)]
    pub verification_retry_at: Option<DateTime<Utc>>,
    /// Pinned before the first remote provider call so later workspace changes
    /// cannot silently redefine the artifact being proved.
    #[serde(default)]
    pub verification_expectation: Option<ArtifactExpectation>,
    pub state: TaskState,
}

/// Immutable limits copied into every supervisor run at creation time.
/// Workspace configuration is deliberately represented by this one value;
/// callers do not get per-request limit overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPolicy {
    pub max_dispatches: u64,
    pub max_concurrency: usize,
    pub max_depth: usize,
    pub max_attempts: u64,
    pub retry_backoff_seconds: i64,
}
impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            max_dispatches: 16,
            max_concurrency: 4,
            max_depth: 8,
            // The workspace default is deliberately fail-closed: retry is
            // enabled only by an explicit immutable run snapshot.
            max_attempts: 1,
            retry_backoff_seconds: 30,
        }
    }
}

/// Durable, redaction-safe record which prevents autonomous progress until a
/// separate authorized-decision feature resolves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalationRecord {
    pub escalation_id: OperationId,
    pub reason: String,
    pub blocking_task_id: Option<TaskId>,
    pub safe_evidence: String,
    pub choices: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// One-to-one fence between a task generation and the concrete worker
/// incarnation that received it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProvenance {
    pub supervisor_run_id: SupervisorRunId,
    pub task_id: TaskId,
    pub parent_task_id: Option<TaskId>,
    pub parent_dispatch_run: Option<OperationId>,
    pub dispatch_run_id: OperationId,
    /// Session owning the worker. Workspace-root Directors have no session.
    #[serde(default)]
    pub worker_session_id: Option<SessionId>,
    pub worker_agent_id: AgentRuntimeId,
    pub worker_worktree_id: WorktreeId,
    pub generation: u64,
}

/// Bounded, worker-authored fact available to workers delegated later in the
/// same Work Run. Raw provider transcripts are never part of this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffContextEntry {
    pub task_id: TaskId,
    pub generation: u64,
    pub dispatch_run_id: OperationId,
    pub outcome: InboxKind,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<String>,
    pub recorded_at: DateTime<Utc>,
}

/// Durable cause of an accepted supervisor event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorEventSource {
    DispatchCompletion,
    DispatchFailure,
    NoReport,
    Timer,
    Cancel,
    Verification,
    Admission,
}

/// Reducer inputs.  Payload bodies are deliberately not copied into event
/// queries; the envelope retains only a payload digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorEventKind {
    AddTask {
        task: TaskNode,
    },
    Dispatch {
        task_id: TaskId,
        generation: u64,
        provenance: RunProvenance,
    },
    Running {
        task_id: TaskId,
        generation: u64,
    },
    SetTaskState {
        task_id: TaskId,
        generation: u64,
        state: TaskState,
    },
    /// Records one exact terminal dispatch for later worker handoff. The entry
    /// is bounded and contains no provider transcript.
    RecordHandoff {
        entry: HandoffContextEntry,
    },
    SetRunState {
        state: SupervisorRunState,
        terminal_reason: Option<String>,
    },
    /// Makes a retry eligible only at its persisted deadline.
    RetryReady {
        task_id: TaskId,
        generation: u64,
    },
    /// Records an independent verification result.  A worker completion
    /// cannot produce this event by itself.
    VerificationResult {
        task_id: TaskId,
        generation: u64,
        passed: bool,
        result_digest: String,
        #[serde(default)]
        safe_summary: String,
    },
    /// Records a retryable provider observation without turning it into a
    /// semantic artifact rejection.
    VerificationDeferred {
        task_id: TaskId,
        generation: u64,
        result_digest: String,
        safe_summary: String,
        retry_at: DateTime<Utc>,
    },
    /// Pins daemon-resolved Git facts before any remote provider call.
    VerificationExpectationRecorded {
        task_id: TaskId,
        generation: u64,
        expectation: ArtifactExpectation,
    },
    /// Pins the latest authenticated Agent report before provider I/O. `None`
    /// is also a durable candidate: it proves the report omitted a usable PR.
    VerificationCandidateRecorded {
        task_id: TaskId,
        generation: u64,
        candidate_pr: Option<String>,
    },
    /// Cancelling is a reducer fact so late dispatch completion cannot revive
    /// the task or run.
    Cancel {
        task_id: Option<TaskId>,
        reason: String,
    },
    Escalate {
        task_id: Option<TaskId>,
        reason: String,
        safe_evidence: String,
        choices: Vec<String>,
    },
    ResolveEscalation {
        escalation_id: OperationId,
        decision: EscalationDecision,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationDecision {
    Resume,
    Cancel,
    Fail,
}

/// Typed human command for a workspace-owned Supervisor Run.
///
/// This command is deliberately separate from the Agent-authenticated MCP
/// surface.  A local UI supplies only durable identities and bounded domain
/// values; workspace authority is resolved from the daemon connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SupervisorWorkspaceCommand {
    Cancel {
        supervisor_run_id: SupervisorRunId,
        reason: String,
    },
    ResolveEscalation {
        supervisor_run_id: SupervisorRunId,
        escalation_id: OperationId,
        decision: EscalationDecision,
    },
}

impl SupervisorWorkspaceCommand {
    #[must_use]
    pub fn supervisor_run_id(&self) -> SupervisorRunId {
        match self {
            Self::Cancel {
                supervisor_run_id, ..
            }
            | Self::ResolveEscalation {
                supervisor_run_id, ..
            } => *supervisor_run_id,
        }
    }
}

/// Append-only event envelope.  `event_id` is the idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorEvent {
    pub sequence: u64,
    pub event_id: OperationId,
    pub causation_id: Option<OperationId>,
    pub correlation_id: Option<OperationId>,
    pub observed_at: DateTime<Utc>,
    pub payload_digest: String,
    pub source: SupervisorEventSource,
    pub kind: SupervisorEventKind,
}

/// Durable aggregate snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorRun {
    pub supervisor_run_id: SupervisorRunId,
    /// Workspace that owns this run. Legacy snapshots created before the TUI
    /// projection was introduced have no value and remain invisible to a
    /// workspace-scoped observer rather than being guessed into one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// Trusted before a Goal worker is spawned. Generic and legacy runs omit
    /// it and therefore cannot satisfy a repository-bound artifact contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_repository: Option<GitHubRepository>,
    pub root_caller_ref: String,
    /// Bounded, presentation-safe Goal summary. Legacy runs omit it and the UI
    /// falls back to the opaque run identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_label: Option<String>,
    pub root_task_digest: String,
    pub root_input_digest: String,
    pub policy_revision: String,
    pub policy: ExecutionPolicy,
    /// Canonical, bounded PR candidates keyed by task. This internal input is
    /// intentionally omitted from redaction-safe query projections.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub verification_candidates: BTreeMap<TaskId, Option<String>>,
    /// Dispatch reservations are committed by the same reducer event as the
    /// dispatch transition.  They make duplicate/replayed admission harmless.
    pub dispatch_reservations: BTreeSet<OperationId>,
    pub escalation: Option<EscalationRecord>,
    pub state_revision: u64,
    pub state: SupervisorRunState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub terminal_at: Option<DateTime<Utc>>,
    pub terminal_reason: Option<String>,
    pub tasks: BTreeMap<TaskId, TaskNode>,
    pub provenance: BTreeMap<TaskId, RunProvenance>,
    /// Completion facts copied into future delegated prompts. This is internal
    /// durable state and is intentionally omitted from redaction-safe queries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handoff_context: Vec<HandoffContextEntry>,
    /// Event IDs already reduced.  This is persisted so journal replay is
    /// idempotent after a crash between append and snapshot write.
    pub applied_events: BTreeSet<OperationId>,
    /// Fixed-size probabilistic tombstone for event IDs removed from
    /// `applied_events` when the journal is compacted. A positive match is
    /// refused as expired rather than silently applying a possibly old ID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    compacted_event_tombstones: Vec<u64>,
}

const EVENT_TOMBSTONE_WORDS: usize = 4_096;
const EVENT_TOMBSTONE_HASHES: u64 = 4;

fn tombstone_bit(id: OperationId, seed: u64) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    for byte in id.to_string().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    usize::try_from(hash % (EVENT_TOMBSTONE_WORDS as u64 * 64)).expect("bit index fits")
}

impl SupervisorRun {
    #[must_use]
    pub fn new(
        root_caller_ref: String,
        root_task_digest: String,
        root_input_digest: String,
        policy_revision: String,
        now: DateTime<Utc>,
    ) -> Self {
        Self::new_with_id(
            SupervisorRunId::new(),
            root_caller_ref,
            root_task_digest,
            root_input_digest,
            policy_revision,
            now,
        )
    }

    #[must_use]
    pub fn new_with_id(
        supervisor_run_id: SupervisorRunId,
        root_caller_ref: String,
        root_task_digest: String,
        root_input_digest: String,
        policy_revision: String,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            supervisor_run_id,
            workspace_id: None,
            artifact_repository: None,
            root_caller_ref,
            display_label: None,
            root_task_digest,
            root_input_digest,
            policy_revision,
            policy: ExecutionPolicy::default(),
            verification_candidates: BTreeMap::new(),
            dispatch_reservations: BTreeSet::new(),
            escalation: None,
            state_revision: 0,
            state: SupervisorRunState::Planning,
            created_at: now,
            updated_at: now,
            terminal_at: None,
            terminal_reason: None,
            tasks: BTreeMap::new(),
            provenance: BTreeMap::new(),
            handoff_context: Vec::new(),
            applied_events: BTreeSet::new(),
            compacted_event_tombstones: Vec::new(),
        }
    }

    /// Whether an event ID belongs to the exact recent window or the compacted
    /// tombstone. Tombstone positives are deliberately fail-closed.
    #[must_use]
    pub fn event_id_status(&self, event_id: OperationId) -> AppliedEventStatus {
        if self.applied_events.contains(&event_id) {
            return AppliedEventStatus::Recent;
        }
        if self.compacted_event_tombstones.is_empty() {
            return AppliedEventStatus::Fresh;
        }
        if self.compacted_event_tombstones.len() != EVENT_TOMBSTONE_WORDS {
            // A malformed durable tombstone must never revive an old event ID.
            return AppliedEventStatus::Expired;
        }
        let present = (0..EVENT_TOMBSTONE_HASHES).all(|seed| {
            let bit = tombstone_bit(event_id, seed);
            self.compacted_event_tombstones[bit / 64] & (1_u64 << (bit % 64)) != 0
        });
        if present {
            AppliedEventStatus::Expired
        } else {
            AppliedEventStatus::Fresh
        }
    }

    /// Whether the fixed-size compaction tombstone has a durable shape this
    /// build can interpret without reviving an expired event ID.
    #[must_use]
    pub fn compaction_state_is_valid(&self) -> bool {
        self.compacted_event_tombstones.is_empty()
            || self.compacted_event_tombstones.len() == EVENT_TOMBSTONE_WORDS
    }

    /// Retain exact IDs for the readable journal window and move older IDs into
    /// a fixed-size fail-closed tombstone.
    pub fn compact_applied_events(&mut self, retained: &BTreeSet<OperationId>) {
        let removed = self
            .applied_events
            .iter()
            .filter(|event_id| !retained.contains(event_id))
            .copied()
            .collect::<Vec<_>>();
        if removed.is_empty() {
            return;
        }
        self.compacted_event_tombstones
            .resize(EVENT_TOMBSTONE_WORDS, 0);
        self.compacted_event_tombstones
            .truncate(EVENT_TOMBSTONE_WORDS);
        for event_id in removed {
            for seed in 0..EVENT_TOMBSTONE_HASHES {
                let bit = tombstone_bit(event_id, seed);
                self.compacted_event_tombstones[bit / 64] |= 1_u64 << (bit % 64);
            }
            self.applied_events.remove(&event_id);
        }
    }

    #[must_use]
    pub fn with_policy(mut self, policy: ExecutionPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Returns a redaction-safe projection for callers.
    #[must_use]
    pub fn query(&self) -> SupervisorRunQuery {
        SupervisorRunQuery {
            supervisor_run_id: self.supervisor_run_id,
            state_revision: self.state_revision,
            state: self.state,
            terminal_at: self.terminal_at,
            terminal_reason: self.terminal_reason.clone(),
            display_label: self.display_label.clone(),
            policy: self.policy.clone(),
            escalation: self.escalation.clone(),
            tasks: self.tasks.values().map(TaskQuery::from).collect(),
            provenance: self.provenance.values().cloned().collect(),
        }
    }
}

/// Query view that excludes task instructions and runtime command lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorRunQuery {
    pub supervisor_run_id: SupervisorRunId,
    pub state_revision: u64,
    pub state: SupervisorRunState,
    pub terminal_at: Option<DateTime<Utc>>,
    pub terminal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_label: Option<String>,
    pub policy: ExecutionPolicy,
    pub escalation: Option<EscalationRecord>,
    pub tasks: Vec<TaskQuery>,
    pub provenance: Vec<RunProvenance>,
}

/// Bounded, redaction-safe workspace projection consumed by the local TUI.
/// Workspace ownership is resolved by the daemon connection; callers cannot
/// use this value to widen their scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorWorkspaceSnapshot {
    pub workspace_id: WorkspaceId,
    pub runs: Vec<SupervisorRunQuery>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskQuery {
    pub task_id: TaskId,
    pub parent_task_id: Option<TaskId>,
    pub dependencies: BTreeSet<TaskId>,
    pub instruction_digest: String,
    pub required_artifact_contract: ArtifactContract,
    pub attempt: u64,
    pub generation: u64,
    pub assigned_dispatch_run: Option<OperationId>,
    #[serde(default)]
    pub verification_attempt: u32,
    #[serde(default)]
    pub verification_retry_at: Option<DateTime<Utc>>,
    pub state: TaskState,
}
impl From<&TaskNode> for TaskQuery {
    fn from(task: &TaskNode) -> Self {
        Self {
            task_id: task.task_id.clone(),
            parent_task_id: task.parent_task_id.clone(),
            dependencies: task.dependencies.clone(),
            instruction_digest: task.instruction_digest.clone(),
            required_artifact_contract: task.required_artifact_contract,
            attempt: task.attempt,
            generation: task.generation,
            assigned_dispatch_run: task.assigned_dispatch_run,
            verification_attempt: task.verification_attempt,
            verification_retry_at: task.verification_retry_at,
            state: task.state,
        }
    }
}

/// Rejection that leaves the aggregate unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorError {
    InvalidTaskId,
    DuplicateTask,
    MissingTask,
    MissingDependency(TaskId),
    SelfDependency,
    Cycle,
    PolicyDenied(String),
    ParentMismatch,
    ProvenanceMismatch,
    DependencyIncomplete,
    InvalidTransition,
    StaleGeneration,
    TerminalRun,
    ExpiredEventId,
    SequenceGap { expected: u64, actual: u64 },
}

/// Idempotency classification for a reducer event ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppliedEventStatus {
    Fresh,
    Recent,
    Expired,
}
impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for SupervisorError {}

/// Applies one event exactly once.  A duplicate event ID is an intentional
/// no-op; a future sequence is rejected so callers cannot skip history.
///
/// # Errors
///
/// Returns a typed rejection without changing `run` when the event is stale,
/// out of sequence, invalid for its task/provenance, or mutates a finished or
/// explicitly quiescent run.
pub fn reduce(run: &mut SupervisorRun, event: &SupervisorEvent) -> Result<(), SupervisorError> {
    match run.event_id_status(event.event_id) {
        AppliedEventStatus::Recent => return Ok(()),
        AppliedEventStatus::Expired => return Err(SupervisorError::ExpiredEventId),
        AppliedEventStatus::Fresh => {}
    }
    let expected = run.state_revision + 1;
    if event.sequence != expected {
        return Err(SupervisorError::SequenceGap {
            expected,
            actual: event.sequence,
        });
    }
    if run.state.blocks_ordinary_events()
        && !matches!(event.kind, SupervisorEventKind::ResolveEscalation { .. })
    {
        return Err(SupervisorError::TerminalRun);
    }
    let mut next = run.clone();
    match &event.kind {
        SupervisorEventKind::AddTask { task } => add_task(&mut next, task.clone())?,
        SupervisorEventKind::Dispatch {
            task_id,
            generation,
            provenance,
        } => dispatch(
            &mut next,
            task_id,
            *generation,
            provenance.clone(),
            event.observed_at,
        )?,
        SupervisorEventKind::Running {
            task_id,
            generation,
        } => set_task(&mut next, task_id, *generation, TaskState::Running)?,
        SupervisorEventKind::SetTaskState {
            task_id,
            generation,
            state,
        } => set_task(&mut next, task_id, *generation, *state)?,
        SupervisorEventKind::RecordHandoff { entry } => {
            record_handoff(&mut next, entry.clone(), event.observed_at)?;
        }
        SupervisorEventKind::SetRunState {
            state,
            terminal_reason,
        } => {
            next.state = *state;
            if state.blocks_ordinary_events() {
                next.terminal_at = Some(event.observed_at);
                next.terminal_reason.clone_from(terminal_reason);
            }
        }
        SupervisorEventKind::RetryReady {
            task_id,
            generation,
        } => retry_ready(&mut next, task_id, *generation, event.observed_at)?,
        SupervisorEventKind::VerificationResult { .. }
        | SupervisorEventKind::VerificationDeferred { .. }
        | SupervisorEventKind::VerificationExpectationRecorded { .. }
        | SupervisorEventKind::VerificationCandidateRecorded { .. } => {
            reduce_verification_event(&mut next, event)?;
        }
        SupervisorEventKind::Cancel { task_id, reason } => {
            cancel(&mut next, task_id.as_ref(), reason, event.observed_at)?;
        }
        SupervisorEventKind::Escalate {
            task_id,
            reason,
            safe_evidence,
            choices,
        } => escalate(
            &mut next,
            event.event_id,
            task_id.clone(),
            reason.clone(),
            safe_evidence.clone(),
            choices.clone(),
            event.observed_at,
        ),
        SupervisorEventKind::ResolveEscalation {
            escalation_id,
            decision,
        } => resolve_escalation(&mut next, *escalation_id, *decision, event.observed_at)?,
    }
    next.state_revision = event.sequence;
    next.updated_at = event.observed_at;
    next.applied_events.insert(event.event_id);
    *run = next;
    Ok(())
}

fn record_handoff(
    run: &mut SupervisorRun,
    entry: HandoffContextEntry,
    observed_at: DateTime<Utc>,
) -> Result<(), SupervisorError> {
    let task = run
        .tasks
        .get(&entry.task_id)
        .ok_or(SupervisorError::MissingTask)?;
    let provenance = run
        .provenance
        .get(&entry.task_id)
        .ok_or(SupervisorError::ProvenanceMismatch)?;
    if task.generation != entry.generation
        || task.assigned_dispatch_run != Some(entry.dispatch_run_id)
        || provenance.generation != entry.generation
        || provenance.dispatch_run_id != entry.dispatch_run_id
        || entry.recorded_at != observed_at
    {
        return Err(SupervisorError::ProvenanceMismatch);
    }
    let outcome_matches_state = match entry.outcome {
        InboxKind::Completed => {
            task.state == TaskState::Succeeded || task.state == TaskState::Verifying
        }
        InboxKind::Failed | InboxKind::NoReport => task.state == TaskState::Failed,
    };
    if !outcome_matches_state
        || entry.summary.trim().is_empty()
        || entry.summary.len() > MAX_HANDOFF_SUMMARY_BYTES
        || !presentation_text_is_safe(&entry.summary)
        || entry.artifacts.as_ref().is_some_and(|artifacts| {
            artifacts.trim().is_empty()
                || artifacts.len() > MAX_HANDOFF_ARTIFACT_BYTES
                || !presentation_text_is_safe(artifacts)
        })
    {
        return Err(SupervisorError::InvalidTransition);
    }
    if let Some(existing) = run
        .handoff_context
        .iter()
        .find(|existing| existing.dispatch_run_id == entry.dispatch_run_id)
    {
        return if existing == &entry {
            Ok(())
        } else {
            Err(SupervisorError::ProvenanceMismatch)
        };
    }
    if run.handoff_context.len() == MAX_HANDOFF_CONTEXT_ENTRIES {
        run.handoff_context.remove(0);
    }
    run.handoff_context.push(entry);
    Ok(())
}

fn reduce_verification_event(
    run: &mut SupervisorRun,
    event: &SupervisorEvent,
) -> Result<(), SupervisorError> {
    match &event.kind {
        SupervisorEventKind::VerificationResult {
            task_id,
            generation,
            passed,
            result_digest,
            safe_summary,
        } => verification_result(
            run,
            task_id,
            *generation,
            *passed,
            result_digest,
            safe_summary,
            event,
        ),
        SupervisorEventKind::VerificationDeferred {
            task_id,
            generation,
            result_digest,
            safe_summary,
            retry_at,
        } => verification_deferred(
            run,
            task_id,
            *generation,
            result_digest,
            safe_summary,
            *retry_at,
            event.observed_at,
        ),
        SupervisorEventKind::VerificationExpectationRecorded {
            task_id,
            generation,
            expectation,
        } => record_verification_expectation(run, task_id, *generation, expectation),
        SupervisorEventKind::VerificationCandidateRecorded {
            task_id,
            generation,
            candidate_pr,
        } => record_verification_candidate(run, task_id, *generation, candidate_pr.as_deref()),
        _ => unreachable!("caller selects only verification events"),
    }
}

fn add_task(run: &mut SupervisorRun, mut task: TaskNode) -> Result<(), SupervisorError> {
    if task.supervisor_run_id != run.supervisor_run_id {
        return Err(SupervisorError::ParentMismatch);
    }
    if task.dependencies.contains(&task.task_id) {
        return Err(SupervisorError::SelfDependency);
    }
    if run.tasks.contains_key(&task.task_id) {
        return Err(SupervisorError::DuplicateTask);
    }
    if let Some(parent) = &task.parent_task_id
        && !run.tasks.contains_key(parent)
    {
        return Err(SupervisorError::MissingTask);
    }
    for dependency in &task.dependencies {
        if !run.tasks.contains_key(dependency) {
            return Err(SupervisorError::MissingDependency(dependency.clone()));
        }
    }
    task.state = if deps_succeeded(&run.tasks, &task.dependencies) {
        TaskState::Ready
    } else {
        TaskState::Pending
    };
    run.tasks.insert(task.task_id.clone(), task);
    Ok(())
}

fn deps_succeeded(tasks: &BTreeMap<TaskId, TaskNode>, deps: &BTreeSet<TaskId>) -> bool {
    deps.iter().all(|id| {
        tasks
            .get(id)
            .is_some_and(|task| task.state == TaskState::Succeeded)
    })
}

fn dispatch(
    run: &mut SupervisorRun,
    task_id: &TaskId,
    generation: u64,
    provenance: RunProvenance,
    now: DateTime<Utc>,
) -> Result<(), SupervisorError> {
    if let Err(SupervisorError::PolicyDenied(reason)) = admit_dispatch(run, task_id, &provenance) {
        escalate(
            run,
            OperationId::new(),
            Some(task_id.clone()),
            reason,
            "policy limits are evaluated from the durable run snapshot".into(),
            vec!["resume".into(), "cancel".into()],
            now,
        );
        return Ok(());
    }
    let task = run.tasks.get(task_id).ok_or(SupervisorError::MissingTask)?;
    if task.generation != generation {
        return Err(SupervisorError::StaleGeneration);
    }
    if task.state != TaskState::Ready || !deps_succeeded(&run.tasks, &task.dependencies) {
        return Err(SupervisorError::DependencyIncomplete);
    }
    if provenance.supervisor_run_id != run.supervisor_run_id
        || provenance.task_id != *task_id
        || provenance.generation != generation
        || provenance.parent_task_id != task.parent_task_id
        || provenance.dispatch_run_id
            != task
                .assigned_dispatch_run
                .unwrap_or(provenance.dispatch_run_id)
    {
        return Err(SupervisorError::ProvenanceMismatch);
    }
    if let Some(parent) = &task.parent_task_id {
        let parent = run.tasks.get(parent).ok_or(SupervisorError::MissingTask)?;
        if provenance.parent_dispatch_run != parent.assigned_dispatch_run {
            return Err(SupervisorError::ProvenanceMismatch);
        }
    }
    let task = run
        .tasks
        .get_mut(task_id)
        .ok_or(SupervisorError::MissingTask)?;
    task.assigned_dispatch_run = Some(provenance.dispatch_run_id);
    task.state = TaskState::Dispatched;
    run.provenance.insert(task_id.clone(), provenance);
    run.dispatch_reservations
        .insert(task.assigned_dispatch_run.expect("assigned above"));
    Ok(())
}
fn admit_dispatch(
    run: &SupervisorRun,
    task_id: &TaskId,
    provenance: &RunProvenance,
) -> Result<(), SupervisorError> {
    if run.escalation.is_some() || run.state == SupervisorRunState::WaitingForDecision {
        return Err(SupervisorError::PolicyDenied(
            "human decision required".into(),
        ));
    }
    if run.dispatch_reservations.len() as u64 >= run.policy.max_dispatches
        && !run
            .dispatch_reservations
            .contains(&provenance.dispatch_run_id)
    {
        return Err(SupervisorError::PolicyDenied(
            "dispatch budget exhausted".into(),
        ));
    }
    let active = run
        .tasks
        .values()
        .filter(|task| matches!(task.state, TaskState::Dispatched | TaskState::Running))
        .count();
    if active >= run.policy.max_concurrency {
        return Err(SupervisorError::PolicyDenied(
            "concurrency limit reached".into(),
        ));
    }
    let mut depth = 0;
    let mut parent = run
        .tasks
        .get(task_id)
        .and_then(|task| task.parent_task_id.clone());
    while let Some(id) = parent {
        depth += 1;
        parent = run
            .tasks
            .get(&id)
            .and_then(|task| task.parent_task_id.clone());
    }
    if depth > run.policy.max_depth {
        return Err(SupervisorError::PolicyDenied(
            "maximum task depth exceeded".into(),
        ));
    }
    Ok(())
}
fn set_task(
    run: &mut SupervisorRun,
    task_id: &TaskId,
    generation: u64,
    state: TaskState,
) -> Result<(), SupervisorError> {
    let task = run
        .tasks
        .get_mut(task_id)
        .ok_or(SupervisorError::MissingTask)?;
    if task.generation != generation {
        return Err(SupervisorError::StaleGeneration);
    }
    if task.state.terminal() {
        return Err(SupervisorError::InvalidTransition);
    }
    let valid = matches!(
        (task.state, state),
        (
            TaskState::Dispatched | TaskState::AwaitingDecision,
            TaskState::Running
        ) | (
            TaskState::Running
                | TaskState::AwaitingDecision
                | TaskState::Retrying
                | TaskState::Verifying,
            TaskState::Succeeded
                | TaskState::Failed
                | TaskState::Cancelled
                | TaskState::Blocked
                | TaskState::AwaitingDecision
                | TaskState::Retrying
                | TaskState::Verifying
        )
    );
    if !valid {
        return Err(SupervisorError::InvalidTransition);
    }
    if state == TaskState::Failed && task.attempt < run.policy.max_attempts {
        task.attempt += 1;
        task.generation += 1;
        let delay = run
            .policy
            .retry_backoff_seconds
            .saturating_mul(1_i64 << (task.attempt - 2).min(30));
        task.retry_at = Some(run.updated_at + chrono::Duration::seconds(delay));
        task.assigned_dispatch_run = None;
        task.verification_digest = None;
        task.verification_attempt = 0;
        task.verification_retry_at = None;
        task.verification_expectation = None;
        run.verification_candidates.remove(task_id);
        task.state = TaskState::Retrying;
        return Ok(());
    }
    if state == TaskState::Succeeded && task.required_artifact_contract.requires_verification() {
        task.state = TaskState::Verifying;
        return Ok(());
    }
    task.state = state;
    if state == TaskState::Succeeded {
        project_ready(&mut run.tasks);
    }
    Ok(())
}
fn retry_ready(
    run: &mut SupervisorRun,
    task_id: &TaskId,
    generation: u64,
    now: DateTime<Utc>,
) -> Result<(), SupervisorError> {
    let task = run
        .tasks
        .get_mut(task_id)
        .ok_or(SupervisorError::MissingTask)?;
    if task.generation != generation {
        return Err(SupervisorError::StaleGeneration);
    }
    if task.state != TaskState::Retrying || task.retry_at.is_none_or(|deadline| deadline > now) {
        return Err(SupervisorError::InvalidTransition);
    }
    task.retry_at = None;
    task.state = TaskState::Ready;
    Ok(())
}
fn verification_result(
    run: &mut SupervisorRun,
    task_id: &TaskId,
    generation: u64,
    passed: bool,
    digest: &str,
    safe_summary: &str,
    event: &SupervisorEvent,
) -> Result<(), SupervisorError> {
    let task = run
        .tasks
        .get_mut(task_id)
        .ok_or(SupervisorError::MissingTask)?;
    if task.generation != generation {
        return Err(SupervisorError::StaleGeneration);
    }
    if task.state != TaskState::Verifying {
        return Err(SupervisorError::InvalidTransition);
    }
    task.verification_digest = Some(digest.into());
    task.verification_retry_at = None;
    if passed {
        task.state = TaskState::Succeeded;
        project_ready(&mut run.tasks);
    } else {
        escalate(
            run,
            event.event_id,
            Some(task_id.clone()),
            "artifact verification failed".into(),
            if safe_summary.is_empty() {
                digest.into()
            } else {
                safe_summary.into()
            },
            vec!["resume".into(), "cancel".into()],
            event.observed_at,
        );
    }
    Ok(())
}

fn verification_deferred(
    run: &mut SupervisorRun,
    task_id: &TaskId,
    generation: u64,
    digest: &str,
    safe_summary: &str,
    retry_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
) -> Result<(), SupervisorError> {
    let task = run
        .tasks
        .get_mut(task_id)
        .ok_or(SupervisorError::MissingTask)?;
    if task.generation != generation {
        return Err(SupervisorError::StaleGeneration);
    }
    let latest = observed_at + chrono::Duration::seconds(ARTIFACT_RETRY_MAX_SECONDS);
    if task.state != TaskState::Verifying
        || retry_at <= observed_at
        || retry_at > latest
        || digest.is_empty()
        || safe_summary.is_empty()
    {
        return Err(SupervisorError::InvalidTransition);
    }
    task.verification_digest = Some(digest.into());
    task.verification_attempt = task.verification_attempt.saturating_add(1);
    task.verification_retry_at = Some(retry_at);
    Ok(())
}

fn record_verification_expectation(
    run: &mut SupervisorRun,
    task_id: &TaskId,
    generation: u64,
    expectation: &ArtifactExpectation,
) -> Result<(), SupervisorError> {
    let task = run
        .tasks
        .get_mut(task_id)
        .ok_or(SupervisorError::MissingTask)?;
    if task.generation != generation {
        return Err(SupervisorError::StaleGeneration);
    }
    if task.state != TaskState::Verifying {
        return Err(SupervisorError::InvalidTransition);
    }
    match &task.verification_expectation {
        Some(existing) if existing == expectation => Ok(()),
        Some(_) => Err(SupervisorError::ProvenanceMismatch),
        None => {
            task.verification_expectation = Some(expectation.clone());
            Ok(())
        }
    }
}

fn record_verification_candidate(
    run: &mut SupervisorRun,
    task_id: &TaskId,
    generation: u64,
    candidate_pr: Option<&str>,
) -> Result<(), SupervisorError> {
    let task = run.tasks.get(task_id).ok_or(SupervisorError::MissingTask)?;
    if task.generation != generation {
        return Err(SupervisorError::StaleGeneration);
    }
    if task.state != TaskState::Verifying
        || candidate_pr.is_some_and(|candidate| {
            candidate.is_empty()
                || candidate.len() > MAX_ARTIFACT_CANDIDATE_BYTES
                || !presentation_text_is_safe(candidate)
                || canonicalize(candidate).is_none_or(|identity| identity.as_url() != candidate)
        })
    {
        return Err(SupervisorError::InvalidTransition);
    }
    let candidate = candidate_pr.map(str::to_owned);
    match run.verification_candidates.get(task_id) {
        Some(existing) if existing == &candidate => Ok(()),
        Some(_) => Err(SupervisorError::ProvenanceMismatch),
        None => {
            run.verification_candidates
                .insert(task_id.clone(), candidate);
            Ok(())
        }
    }
}
fn cancel(
    run: &mut SupervisorRun,
    task_id: Option<&TaskId>,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<(), SupervisorError> {
    if let Some(id) = task_id {
        let task = run.tasks.get_mut(id).ok_or(SupervisorError::MissingTask)?;
        if !task.state.terminal() {
            task.state = TaskState::Cancelled;
        }
    } else {
        for task in run.tasks.values_mut().filter(|task| !task.state.terminal()) {
            task.state = TaskState::Cancelled;
        }
        run.state = SupervisorRunState::Cancelled;
        run.terminal_at = Some(now);
        run.terminal_reason = Some(reason.into());
    }
    Ok(())
}
fn escalate(
    run: &mut SupervisorRun,
    escalation_id: OperationId,
    task_id: Option<TaskId>,
    reason: String,
    safe_evidence: String,
    choices: Vec<String>,
    now: DateTime<Utc>,
) {
    run.escalation = Some(EscalationRecord {
        escalation_id,
        reason: reason.clone(),
        blocking_task_id: task_id,
        safe_evidence,
        choices,
        created_at: now,
    });
    run.state = SupervisorRunState::Escalated;
    run.terminal_at = Some(now);
    run.terminal_reason = Some(reason);
}

fn resolve_escalation(
    run: &mut SupervisorRun,
    escalation_id: OperationId,
    decision: EscalationDecision,
    now: DateTime<Utc>,
) -> Result<(), SupervisorError> {
    let escalation = run
        .escalation
        .clone()
        .ok_or(SupervisorError::InvalidTransition)?;
    if escalation.escalation_id != escalation_id {
        return Err(SupervisorError::ProvenanceMismatch);
    }
    run.escalation = None;
    match decision {
        EscalationDecision::Resume => {
            if let Some(task_id) = escalation.blocking_task_id
                && let Some(task) = run.tasks.get_mut(&task_id)
                && task.state == TaskState::Verifying
            {
                // A rejected artifact needs new Agent work and a fresh report,
                // not an immediate replay of the same immutable expectation.
                // AwaitingDecision keeps the periodic verifier quiescent until
                // that exact dispatch reports completion again.
                task.state = TaskState::AwaitingDecision;
                task.verification_digest = None;
                task.verification_attempt = 0;
                task.verification_retry_at = None;
                task.verification_expectation = None;
                run.verification_candidates.remove(&task_id);
            }
            run.state = SupervisorRunState::Running;
            run.terminal_at = None;
            run.terminal_reason = None;
        }
        EscalationDecision::Cancel => cancel(run, None, "escalation cancelled", now)?,
        EscalationDecision::Fail => {
            for task in run.tasks.values_mut().filter(|task| !task.state.terminal()) {
                task.state = TaskState::Failed;
            }
            run.state = SupervisorRunState::Failed;
            run.terminal_at = Some(now);
            run.terminal_reason = Some("escalation resolved as failure".into());
        }
    }
    Ok(())
}
fn project_ready(tasks: &mut BTreeMap<TaskId, TaskNode>) {
    let ready: Vec<_> = tasks
        .iter()
        .filter(|(_, task)| {
            task.state == TaskState::Pending && deps_succeeded(tasks, &task.dependencies)
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in ready {
        tasks
            .get_mut(&id)
            .expect("ready task was selected from the same task map")
            .state = TaskState::Ready;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap()
    }
    fn task(run: SupervisorRunId, id: &str, deps: &[&str]) -> TaskNode {
        TaskNode {
            task_id: TaskId::new(id).unwrap(),
            supervisor_run_id: run,
            parent_task_id: None,
            dependencies: deps.iter().map(|v| TaskId::new(*v).unwrap()).collect(),
            instruction_digest: "digest".into(),
            instruction_body: "secret prompt".into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
            attempt: 1,
            generation: 1,
            assigned_dispatch_run: None,
            promotion_reserved_at: None,
            retry_at: None,
            verification_digest: None,
            verification_attempt: 0,
            verification_retry_at: None,
            verification_expectation: None,
            state: TaskState::Pending,
        }
    }

    #[test]
    fn escalation_is_quiescent_but_not_finished_history() {
        assert!(SupervisorRunState::Escalated.blocks_ordinary_events());
        #[allow(deprecated)]
        {
            assert!(SupervisorRunState::Escalated.terminal());
            assert!(!SupervisorRunState::Running.terminal());
        }
        assert!(!SupervisorRunState::Escalated.is_finished());
        assert!(SupervisorRunState::Succeeded.is_finished());
        assert!(SupervisorRunState::Failed.is_finished());
        assert!(SupervisorRunState::Cancelled.is_finished());
        assert!(!SupervisorRunState::Running.is_finished());
        assert!(!NO_ARTIFACT_CONTRACT.requires_verification());
        assert!(GOAL_REVIEW_READY_ARTIFACT_CONTRACT.requires_verification());
    }

    #[test]
    fn artifact_contract_and_presented_task_id_are_closed_domain_values() {
        assert_eq!(
            serde_json::to_string(&GOAL_REVIEW_READY_ARTIFACT_CONTRACT).unwrap(),
            r#""goal_review_ready_pr_v1""#
        );
        assert_eq!(
            GOAL_REVIEW_READY_ARTIFACT_CONTRACT.to_string(),
            "goal_review_ready_pr_v1"
        );
        assert_eq!(NO_ARTIFACT_CONTRACT.to_string(), "none");
        assert_eq!(
            serde_json::from_str::<ArtifactContract>(r#""none""#).unwrap(),
            NO_ARTIFACT_CONTRACT
        );
        assert!(serde_json::from_str::<ArtifactContract>(r#""unknown""#).is_err());
        let repository = GitHubRepository::from_name_with_owner("acme/repo").unwrap();
        assert!(ArtifactExpectation::new(repository.clone(), "not-an-oid").is_none());
        let expectation = ArtifactExpectation::new(
            repository.clone(),
            "ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD",
        )
        .unwrap();
        assert_eq!(expectation.repository(), &repository);
        assert_eq!(
            expectation.head_oid(),
            "abcdefabcdefabcdefabcdefabcdefabcdefabcd"
        );
        let mut invalid_head = serde_json::to_value(&expectation).unwrap();
        invalid_head["head_oid"] = serde_json::json!("not-an-oid");
        assert!(serde_json::from_value::<ArtifactExpectation>(invalid_head).is_err());
        let mut missing_head = serde_json::to_value(&expectation).unwrap();
        missing_head.as_object_mut().unwrap().remove("head_oid");
        assert!(serde_json::from_value::<ArtifactExpectation>(missing_head).is_err());
        let missing_repository = serde_json::json!({
            "head_oid": "0123456789012345678901234567890123456789"
        });
        assert!(serde_json::from_value::<ArtifactExpectation>(missing_repository).is_err());
        let expectation_event = SupervisorEventKind::VerificationExpectationRecorded {
            task_id: TaskId::new("root").unwrap(),
            generation: 1,
            expectation,
        };
        let expectation_event_wire = serde_json::to_value(&expectation_event).unwrap();
        assert_eq!(
            serde_json::from_value::<SupervisorEventKind>(expectation_event_wire.clone()).unwrap(),
            expectation_event
        );
        let mut missing_expectation = expectation_event_wire.clone();
        missing_expectation["VerificationExpectationRecorded"]
            .as_object_mut()
            .unwrap()
            .remove("expectation");
        assert!(serde_json::from_value::<SupervisorEventKind>(missing_expectation).is_err());
        let mut invalid_event_expectation = expectation_event_wire;
        invalid_event_expectation["VerificationExpectationRecorded"]["expectation"]["head_oid"] =
            serde_json::json!("not-an-oid");
        assert!(serde_json::from_value::<SupervisorEventKind>(invalid_event_expectation).is_err());
        assert!(
            serde_json::from_str::<ArtifactExpectation>(
                r#"{"repository":"acme/repo","repository":"other/repo","head_oid":"0123456789012345678901234567890123456789"}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ArtifactExpectation>(
                r#"{"repository":"acme/repo","head_oid":"0123456789012345678901234567890123456789","head_oid":"0123456789012345678901234567890123456789"}"#,
            )
            .is_err()
        );
        let with_unknown = serde_json::json!({
            "repository": "acme/repo",
            "head_oid": "0123456789012345678901234567890123456789",
            "future_field": true
        });
        assert!(serde_json::from_value::<ArtifactExpectation>(with_unknown).is_ok());
        assert!(serde_json::from_str::<ArtifactExpectation>(r#""not-an-expectation""#).is_err());
        for unsafe_id in [
            "line\nbreak",
            "escape\u{1b}[2J",
            "direction\u{202e}override",
            "isolate\u{2066}text",
            "deprecated-direction\u{206a}text",
        ] {
            assert_eq!(TaskId::new(unsafe_id), Err(SupervisorError::InvalidTaskId));
            assert!(!presentation_text_is_safe(unsafe_id));
        }
        assert_eq!(TaskId::new("安全な-task_1").unwrap().0, "安全な-task_1");
    }

    #[test]
    fn artifact_expectation_accepts_a_bounded_set_of_normalized_heads() {
        let repository = GitHubRepository::from_name_with_owner("acme/repo").unwrap();
        let heads = [
            "ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD",
            "0123456789012345678901234567890123456789",
            "abcdefabcdefabcdefabcdefabcdefabcdefabcd",
        ]
        .map(str::to_owned);
        let alternate = ArtifactExpectation::from_heads(repository.clone(), &heads).unwrap();
        assert!(alternate.matches_head("0123456789012345678901234567890123456789"));
        assert!(alternate.matches_head("ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD"));
        assert_eq!(alternate.head_oids().count(), 2);
        assert!(ArtifactExpectation::from_heads(repository.clone(), &[]).is_none());
        assert!(ArtifactExpectation::from_heads(repository.clone(), &["invalid".into()]).is_none());

        let too_many = (0..=MAX_ARTIFACT_EXPECTATION_HEADS)
            .map(|index| format!("{index:040x}"))
            .collect::<Vec<_>>();
        assert!(ArtifactExpectation::from_heads(repository, &too_many).is_none());
        assert!(
            ArtifactExpectation::try_from(ArtifactExpectationWire {
                repository: GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
                head_oid: "0123456789012345678901234567890123456789".into(),
                alternate_head_oids: vec![
                    "abcdefabcdefabcdefabcdefabcdefabcdefabcd".into();
                    MAX_ARTIFACT_EXPECTATION_HEADS
                ],
            })
            .is_err()
        );
    }
    fn event(seq: u64, kind: SupervisorEventKind) -> SupervisorEvent {
        SupervisorEvent {
            sequence: seq,
            event_id: OperationId::new(),
            causation_id: None,
            correlation_id: None,
            observed_at: now(),
            payload_digest: "d".into(),
            source: SupervisorEventSource::Admission,
            kind,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One reducer fixture keeps provenance, idempotence, bounds, and legacy decoding visibly related.
    fn handoff_context_is_exact_bounded_and_backward_compatible() {
        let mut run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        run.state = SupervisorRunState::Running;
        let task_id = TaskId::new("child").unwrap();
        let mut child = task(run.supervisor_run_id, "child", &[]);
        child.state = TaskState::Succeeded;
        child.assigned_dispatch_run = Some(OperationId::new());
        run.tasks.insert(task_id.clone(), child);
        let supervisor_run_id = run.supervisor_run_id;
        let make_provenance = |dispatch_run_id| RunProvenance {
            supervisor_run_id,
            task_id: task_id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id,
            worker_session_id: Some(SessionId::new()),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        let first_run = OperationId::new();
        run.tasks.get_mut(&task_id).unwrap().assigned_dispatch_run = Some(first_run);
        run.provenance
            .insert(task_id.clone(), make_provenance(first_run));
        let first = HandoffContextEntry {
            task_id: task_id.clone(),
            generation: 1,
            dispatch_run_id: first_run,
            outcome: InboxKind::Completed,
            summary: "entry 0".into(),
            artifacts: Some("PR https://example.test/1".into()),
            recorded_at: now(),
        };
        let mut missing_task = first.clone();
        missing_task.task_id = TaskId::new("missing").unwrap();
        assert_eq!(
            record_handoff(&mut run, missing_task, now()),
            Err(SupervisorError::MissingTask)
        );
        let provenance = run.provenance.remove(&task_id).unwrap();
        assert_eq!(
            record_handoff(&mut run, first.clone(), now()),
            Err(SupervisorError::ProvenanceMismatch)
        );
        run.provenance.insert(task_id.clone(), provenance);
        let mut mismatched = first.clone();
        mismatched.generation += 1;
        assert_eq!(
            record_handoff(&mut run, mismatched, now()),
            Err(SupervisorError::ProvenanceMismatch)
        );
        reduce(
            &mut run,
            &event(
                1,
                SupervisorEventKind::RecordHandoff {
                    entry: first.clone(),
                },
            ),
        )
        .unwrap();
        record_handoff(&mut run, first.clone(), now()).unwrap();
        assert_eq!(run.handoff_context.as_slice(), std::slice::from_ref(&first));
        let mut conflicting = first;
        conflicting.summary = "different".into();
        assert_eq!(
            record_handoff(&mut run, conflicting, now()),
            Err(SupervisorError::ProvenanceMismatch)
        );

        let failed_run = OperationId::new();
        run.tasks.get_mut(&task_id).unwrap().state = TaskState::Failed;
        run.tasks.get_mut(&task_id).unwrap().assigned_dispatch_run = Some(failed_run);
        run.provenance
            .insert(task_id.clone(), make_provenance(failed_run));
        record_handoff(
            &mut run,
            HandoffContextEntry {
                task_id: task_id.clone(),
                generation: 1,
                dispatch_run_id: failed_run,
                outcome: InboxKind::Failed,
                summary: "worker failed safely".into(),
                artifacts: None,
                recorded_at: now(),
            },
            now(),
        )
        .unwrap();

        let no_report_run = OperationId::new();
        run.tasks.get_mut(&task_id).unwrap().assigned_dispatch_run = Some(no_report_run);
        run.provenance
            .insert(task_id.clone(), make_provenance(no_report_run));
        record_handoff(
            &mut run,
            HandoffContextEntry {
                task_id: task_id.clone(),
                generation: 1,
                dispatch_run_id: no_report_run,
                outcome: InboxKind::NoReport,
                summary: "worker exited without a report".into(),
                artifacts: None,
                recorded_at: now(),
            },
            now(),
        )
        .unwrap();

        let verifying_run = OperationId::new();
        run.tasks.get_mut(&task_id).unwrap().state = TaskState::Verifying;
        run.tasks.get_mut(&task_id).unwrap().assigned_dispatch_run = Some(verifying_run);
        run.provenance
            .insert(task_id.clone(), make_provenance(verifying_run));
        record_handoff(
            &mut run,
            HandoffContextEntry {
                task_id: task_id.clone(),
                generation: 1,
                dispatch_run_id: verifying_run,
                outcome: InboxKind::Completed,
                summary: "artifact verification pending".into(),
                artifacts: None,
                recorded_at: now(),
            },
            now(),
        )
        .unwrap();
        run.tasks.get_mut(&task_id).unwrap().state = TaskState::Succeeded;

        for index in 1..=MAX_HANDOFF_CONTEXT_ENTRIES {
            let dispatch_run_id = OperationId::new();
            run.tasks.get_mut(&task_id).unwrap().assigned_dispatch_run = Some(dispatch_run_id);
            run.provenance
                .insert(task_id.clone(), make_provenance(dispatch_run_id));
            record_handoff(
                &mut run,
                HandoffContextEntry {
                    task_id: task_id.clone(),
                    generation: 1,
                    dispatch_run_id,
                    outcome: InboxKind::Completed,
                    summary: format!("entry {index}"),
                    artifacts: None,
                    recorded_at: now(),
                },
                now(),
            )
            .unwrap();
        }
        assert_eq!(run.handoff_context.len(), MAX_HANDOFF_CONTEXT_ENTRIES);
        assert_eq!(run.handoff_context[0].summary, "entry 1");

        let unsafe_run = OperationId::new();
        run.tasks.get_mut(&task_id).unwrap().assigned_dispatch_run = Some(unsafe_run);
        run.provenance
            .insert(task_id.clone(), make_provenance(unsafe_run));
        assert_eq!(
            record_handoff(
                &mut run,
                HandoffContextEntry {
                    task_id,
                    generation: 1,
                    dispatch_run_id: unsafe_run,
                    outcome: InboxKind::Completed,
                    summary: "unsafe\nsummary".into(),
                    artifacts: None,
                    recorded_at: now(),
                },
                now(),
            ),
            Err(SupervisorError::InvalidTransition)
        );

        let mut legacy = serde_json::to_value(&run).unwrap();
        legacy.as_object_mut().unwrap().remove("handoff_context");
        assert!(
            serde_json::from_value::<SupervisorRun>(legacy)
                .unwrap()
                .handoff_context
                .is_empty()
        );
    }

    #[test]
    #[should_panic(expected = "caller selects only verification events")]
    fn verification_reducer_rejects_a_non_verification_event() {
        let mut run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        reduce_verification_event(
            &mut run,
            &event(
                1,
                SupervisorEventKind::SetRunState {
                    state: SupervisorRunState::Failed,
                    terminal_reason: Some("invalid routing fixture".into()),
                },
            ),
        )
        .unwrap();
    }
    #[test]
    fn dag_projects_only_satisfied_tasks_and_duplicate_is_noop() {
        let mut run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "p1".into(),
            now(),
        );
        let first = task(run.supervisor_run_id, "root", &[]);
        let first_event = event(1, SupervisorEventKind::AddTask { task: first });
        reduce(&mut run, &first_event).unwrap();
        let second = task(run.supervisor_run_id, "child", &["root"]);
        reduce(
            &mut run,
            &event(2, SupervisorEventKind::AddTask { task: second }),
        )
        .unwrap();
        assert_eq!(
            run.tasks[&TaskId::new("root").unwrap()].state,
            TaskState::Ready
        );
        assert_eq!(
            run.tasks[&TaskId::new("child").unwrap()].state,
            TaskState::Pending
        );
        reduce(&mut run, &first_event).unwrap();
        assert_eq!(run.state_revision, 2);
        assert!(
            !run.query()
                .tasks
                .iter()
                .any(|task| task.instruction_digest == "secret prompt")
        );
    }
    #[test]
    fn rejects_bad_sequences_and_terminal_mutation() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        assert!(matches!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::SetRunState {
                        state: SupervisorRunState::Running,
                        terminal_reason: None
                    }
                )
            ),
            Err(SupervisorError::SequenceGap { .. })
        ));
        reduce(
            &mut run,
            &event(
                1,
                SupervisorEventKind::SetRunState {
                    state: SupervisorRunState::Cancelled,
                    terminal_reason: Some("x".into()),
                },
            ),
        )
        .unwrap();
        assert!(matches!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::SetRunState {
                        state: SupervisorRunState::Running,
                        terminal_reason: None
                    }
                )
            ),
            Err(SupervisorError::TerminalRun)
        ));
    }

    #[test]
    fn dispatch_provenance_fences_generation_and_unblocks_dependents() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let root = task(run.supervisor_run_id, "root", &[]);
        let child = task(run.supervisor_run_id, "child", &["root"]);
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: root }),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(2, SupervisorEventKind::AddTask { task: child }),
        )
        .unwrap();
        let root_id = TaskId::new("root").unwrap();
        let dispatch = OperationId::new();
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: root_id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id: dispatch,
            worker_session_id: Some(SessionId::new()),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        reduce(
            &mut run,
            &event(
                3,
                SupervisorEventKind::Dispatch {
                    task_id: root_id.clone(),
                    generation: 1,
                    provenance,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                4,
                SupervisorEventKind::Running {
                    task_id: root_id.clone(),
                    generation: 1,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                5,
                SupervisorEventKind::SetTaskState {
                    task_id: root_id,
                    generation: 1,
                    state: TaskState::Succeeded,
                },
            ),
        )
        .unwrap();
        assert_eq!(
            run.tasks[&TaskId::new("child").unwrap()].state,
            TaskState::Ready
        );
        assert_eq!(run.provenance.len(), 1);
        let snapshot = serde_json::to_string(&run).unwrap();
        assert_eq!(
            serde_json::from_str::<SupervisorRun>(&snapshot).unwrap(),
            run
        );
    }

    #[test]
    fn rejects_dag_and_transition_errors_without_mutating_state() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let invalid = task(run.supervisor_run_id, "same", &["same"]);
        assert_eq!(
            reduce(
                &mut run,
                &event(1, SupervisorEventKind::AddTask { task: invalid })
            ),
            Err(SupervisorError::SelfDependency)
        );
        let missing = task(run.supervisor_run_id, "missing", &["gone"]);
        assert!(matches!(
            reduce(
                &mut run,
                &event(1, SupervisorEventKind::AddTask { task: missing })
            ),
            Err(SupervisorError::MissingDependency(_))
        ));
        let mut wrong_run = task(SupervisorRunId::new(), "wrong", &[]);
        wrong_run.state = TaskState::Succeeded;
        assert_eq!(
            reduce(
                &mut run,
                &event(1, SupervisorEventKind::AddTask { task: wrong_run })
            ),
            Err(SupervisorError::ParentMismatch)
        );
        let root = task(run.supervisor_run_id, "root", &[]);
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: root }),
        )
        .unwrap();
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .state = TaskState::Pending;
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::Running {
                        task_id: TaskId::new("root").unwrap(),
                        generation: 2,
                    }
                )
            ),
            Err(SupervisorError::StaleGeneration)
        );
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::Running {
                        task_id: TaskId::new("root").unwrap(),
                        generation: 1,
                    }
                )
            ),
            Err(SupervisorError::InvalidTransition)
        );
    }

    #[test]
    fn policy_reserves_dispatch_once_and_escalates_before_an_over_limit_effect() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now())
            .with_policy(ExecutionPolicy {
                max_dispatches: 1,
                max_concurrency: 1,
                max_depth: 0,
                max_attempts: 1,
                retry_backoff_seconds: 1,
            });
        let first = task(run.supervisor_run_id, "first", &[]);
        let second = task(run.supervisor_run_id, "second", &[]);
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: first }),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(2, SupervisorEventKind::AddTask { task: second }),
        )
        .unwrap();
        let id = TaskId::new("first").unwrap();
        let dispatch = OperationId::new();
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id: dispatch,
            worker_session_id: Some(SessionId::new()),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        reduce(
            &mut run,
            &event(
                3,
                SupervisorEventKind::Dispatch {
                    task_id: id.clone(),
                    generation: 1,
                    provenance,
                },
            ),
        )
        .unwrap();
        assert_eq!(run.dispatch_reservations.len(), 1);
        let second_id = TaskId::new("second").unwrap();
        let second_provenance = RunProvenance {
            task_id: second_id.clone(),
            dispatch_run_id: OperationId::new(),
            generation: 1,
            ..run.provenance[&TaskId::new("first").unwrap()].clone()
        };
        reduce(
            &mut run,
            &event(
                4,
                SupervisorEventKind::Dispatch {
                    task_id: second_id,
                    generation: 1,
                    provenance: second_provenance,
                },
            ),
        )
        .unwrap();
        assert_eq!(run.state, SupervisorRunState::Escalated);
        assert_eq!(
            run.escalation.as_ref().unwrap().reason,
            "dispatch budget exhausted"
        );
    }

    fn verifying_artifact_run() -> (SupervisorRun, TaskId) {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now())
            .with_policy(ExecutionPolicy {
                max_dispatches: 3,
                max_concurrency: 1,
                max_depth: 1,
                max_attempts: 2,
                retry_backoff_seconds: 30,
            });
        let mut artifact = task(run.supervisor_run_id, "artifact", &[]);
        artifact.required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: artifact }),
        )
        .unwrap();
        let id = TaskId::new("artifact").unwrap();
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id: OperationId::new(),
            worker_session_id: Some(SessionId::new()),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        reduce(
            &mut run,
            &event(
                2,
                SupervisorEventKind::Dispatch {
                    task_id: id.clone(),
                    generation: 1,
                    provenance,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                3,
                SupervisorEventKind::Running {
                    task_id: id.clone(),
                    generation: 1,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                4,
                SupervisorEventKind::SetTaskState {
                    task_id: id.clone(),
                    generation: 1,
                    state: TaskState::Succeeded,
                },
            ),
        )
        .unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Verifying);
        (run, id)
    }

    #[test]
    fn verification_and_retry_are_durable_gates() {
        let (mut run, id) = verifying_artifact_run();
        reduce(
            &mut run,
            &event(
                5,
                SupervisorEventKind::VerificationResult {
                    task_id: id.clone(),
                    generation: 1,
                    passed: true,
                    result_digest: "verified".into(),
                    safe_summary: String::new(),
                },
            ),
        )
        .unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Succeeded);
    }

    #[test]
    fn verification_generation_and_state_fences_are_explicit() {
        let (run, id) = verifying_artifact_run();
        let expectation = ArtifactExpectation::new(
            GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
            "0123456789012345678901234567890123456789",
        )
        .unwrap();
        let mut stale_retry = run.clone();
        assert!(matches!(
            reduce(
                &mut stale_retry,
                &event(
                    5,
                    SupervisorEventKind::VerificationDeferred {
                        task_id: id.clone(),
                        generation: 2,
                        result_digest: "provider".into(),
                        safe_summary: "unavailable".into(),
                        retry_at: now() + chrono::Duration::seconds(1),
                    },
                ),
            ),
            Err(SupervisorError::StaleGeneration)
        ));
        let mut stale_expectation = run.clone();
        assert!(matches!(
            reduce(
                &mut stale_expectation,
                &event(
                    5,
                    SupervisorEventKind::VerificationExpectationRecorded {
                        task_id: id.clone(),
                        generation: 2,
                        expectation: expectation.clone(),
                    },
                ),
            ),
            Err(SupervisorError::StaleGeneration)
        ));
        let mut invalid_state = run.clone();
        invalid_state.tasks.get_mut(&id).unwrap().state = TaskState::Running;
        assert!(matches!(
            reduce(
                &mut invalid_state,
                &event(
                    5,
                    SupervisorEventKind::VerificationExpectationRecorded {
                        task_id: id.clone(),
                        generation: 1,
                        expectation: expectation.clone(),
                    },
                ),
            ),
            Err(SupervisorError::InvalidTransition)
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One scenario keeps the immutable expectation and bounded deferral sequence visible.
    fn verification_expectation_is_immutable_and_deadline_is_future_bounded() {
        let (mut run, id) = verifying_artifact_run();
        let expectation = ArtifactExpectation::new(
            GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
            "0123456789012345678901234567890123456789",
        )
        .unwrap();
        for retry_at in [
            now(),
            now() + chrono::Duration::seconds(ARTIFACT_RETRY_MAX_SECONDS + 1),
        ] {
            assert!(matches!(
                reduce(
                    &mut run,
                    &event(
                        5,
                        SupervisorEventKind::VerificationDeferred {
                            task_id: id.clone(),
                            generation: 1,
                            result_digest: "provider".into(),
                            safe_summary: "unavailable".into(),
                            retry_at,
                        },
                    ),
                ),
                Err(SupervisorError::InvalidTransition)
            ));
        }
        for (result_digest, safe_summary) in [("", "unavailable"), ("provider", "")] {
            assert!(matches!(
                reduce(
                    &mut run,
                    &event(
                        5,
                        SupervisorEventKind::VerificationDeferred {
                            task_id: id.clone(),
                            generation: 1,
                            result_digest: result_digest.into(),
                            safe_summary: safe_summary.into(),
                            retry_at: now() + chrono::Duration::seconds(1),
                        },
                    ),
                ),
                Err(SupervisorError::InvalidTransition)
            ));
        }
        reduce(
            &mut run,
            &event(
                5,
                SupervisorEventKind::VerificationExpectationRecorded {
                    task_id: id.clone(),
                    generation: 1,
                    expectation: expectation.clone(),
                },
            ),
        )
        .unwrap();
        assert_eq!(
            run.tasks[&id].verification_expectation.as_ref(),
            Some(&expectation)
        );
        let candidate = "https://github.com/acme/repo/pull/42";
        assert!(matches!(
            reduce(
                &mut run,
                &event(
                    6,
                    SupervisorEventKind::VerificationCandidateRecorded {
                        task_id: TaskId::new("missing").unwrap(),
                        generation: 1,
                        candidate_pr: Some(candidate.into()),
                    },
                ),
            ),
            Err(SupervisorError::MissingTask)
        ));
        assert!(matches!(
            reduce(
                &mut run,
                &event(
                    6,
                    SupervisorEventKind::VerificationCandidateRecorded {
                        task_id: id.clone(),
                        generation: 2,
                        candidate_pr: Some(candidate.into()),
                    },
                ),
            ),
            Err(SupervisorError::StaleGeneration)
        ));
        reduce(
            &mut run,
            &event(
                6,
                SupervisorEventKind::VerificationCandidateRecorded {
                    task_id: id.clone(),
                    generation: 1,
                    candidate_pr: Some(candidate.into()),
                },
            ),
        )
        .unwrap();
        assert_eq!(run.verification_candidates[&id].as_deref(), Some(candidate));
        reduce(
            &mut run,
            &event(
                7,
                SupervisorEventKind::VerificationCandidateRecorded {
                    task_id: id.clone(),
                    generation: 1,
                    candidate_pr: Some(candidate.into()),
                },
            ),
        )
        .unwrap();
        assert!(matches!(
            reduce(
                &mut run,
                &event(
                    8,
                    SupervisorEventKind::VerificationCandidateRecorded {
                        task_id: id.clone(),
                        generation: 1,
                        candidate_pr: Some("https://github.com/acme/repo/pull/43".into()),
                    },
                ),
            ),
            Err(SupervisorError::ProvenanceMismatch)
        ));
        for invalid in [
            "https://example.com/acme/repo/pull/43".to_owned(),
            format!(
                "https://github.com/acme/{}/pull/43",
                "x".repeat(MAX_ARTIFACT_CANDIDATE_BYTES)
            ),
        ] {
            assert!(matches!(
                reduce(
                    &mut run,
                    &event(
                        8,
                        SupervisorEventKind::VerificationCandidateRecorded {
                            task_id: id.clone(),
                            generation: 1,
                            candidate_pr: Some(invalid),
                        },
                    ),
                ),
                Err(SupervisorError::InvalidTransition)
            ));
        }
        let retry_at = now() + chrono::Duration::seconds(1);
        reduce(
            &mut run,
            &event(
                8,
                SupervisorEventKind::VerificationDeferred {
                    task_id: id.clone(),
                    generation: 1,
                    result_digest: "provider-unavailable".into(),
                    safe_summary: "provider temporarily unavailable".into(),
                    retry_at,
                },
            ),
        )
        .unwrap();
        assert_eq!(
            run.tasks[&id].verification_digest.as_deref(),
            Some("provider-unavailable")
        );
        assert_eq!(run.tasks[&id].verification_attempt, 1);
        assert_eq!(run.tasks[&id].verification_retry_at, Some(retry_at));
        let conflicting = ArtifactExpectation::new(
            GitHubRepository::from_name_with_owner("other/repo").unwrap(),
            "0123456789012345678901234567890123456789",
        )
        .unwrap();
        assert!(matches!(
            reduce(
                &mut run,
                &event(
                    9,
                    SupervisorEventKind::VerificationExpectationRecorded {
                        task_id: id.clone(),
                        generation: 1,
                        expectation: conflicting,
                    },
                ),
            ),
            Err(SupervisorError::ProvenanceMismatch)
        ));
    }

    #[test]
    fn retry_cancel_and_failed_verification_cannot_resume_work() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now())
            .with_policy(ExecutionPolicy {
                max_dispatches: 3,
                max_concurrency: 1,
                max_depth: 1,
                max_attempts: 2,
                retry_backoff_seconds: 30,
            });
        let mut retry_task = task(run.supervisor_run_id, "retry", &[]);
        retry_task.required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: retry_task }),
        )
        .unwrap();
        let id = TaskId::new("retry").unwrap();
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id: OperationId::new(),
            worker_session_id: Some(SessionId::new()),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        reduce(
            &mut run,
            &event(
                2,
                SupervisorEventKind::Dispatch {
                    task_id: id.clone(),
                    generation: 1,
                    provenance,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                3,
                SupervisorEventKind::Running {
                    task_id: id.clone(),
                    generation: 1,
                },
            ),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(
                4,
                SupervisorEventKind::SetTaskState {
                    task_id: id.clone(),
                    generation: 1,
                    state: TaskState::Failed,
                },
            ),
        )
        .unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Retrying);
        let generation = run.tasks[&id].generation;
        assert!(matches!(
            reduce(
                &mut run,
                &event(
                    5,
                    SupervisorEventKind::RetryReady {
                        task_id: id.clone(),
                        generation,
                    },
                )
            ),
            Err(SupervisorError::InvalidTransition)
        ));
        let mut due = event(
            5,
            SupervisorEventKind::RetryReady {
                task_id: id.clone(),
                generation,
            },
        );
        due.observed_at += chrono::Duration::seconds(30);
        reduce(&mut run, &due).unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Ready);
    }

    #[test]
    fn cancellation_converges_tasks_and_run_to_terminal_state() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let mut task = task(run.supervisor_run_id, "cancel", &[]);
        task.state = TaskState::Ready;
        let id = task.task_id.clone();
        run.tasks.insert(id.clone(), task);
        reduce(
            &mut run,
            &event(
                1,
                SupervisorEventKind::Cancel {
                    task_id: Some(id.clone()),
                    reason: "task cancelled".into(),
                },
            ),
        )
        .unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Cancelled);
        reduce(
            &mut run,
            &event(
                2,
                SupervisorEventKind::Cancel {
                    task_id: Some(id.clone()),
                    reason: "task cancellation replayed".into(),
                },
            ),
        )
        .unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Cancelled);
        reduce(
            &mut run,
            &event(
                3,
                SupervisorEventKind::Cancel {
                    task_id: None,
                    reason: "run cancelled".into(),
                },
            ),
        )
        .unwrap();
        assert_eq!(run.state, SupervisorRunState::Cancelled);
        assert!(run.terminal_at.is_some());
    }

    #[test]
    fn failed_verification_escalates_and_records_safe_evidence() {
        for (safe_summary, expected) in [
            ("head commit did not match", "head commit did not match"),
            ("", "mismatch"),
        ] {
            let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
            let mut task = task(run.supervisor_run_id, "verify", &[]);
            task.state = TaskState::Verifying;
            run.tasks.insert(task.task_id.clone(), task);
            reduce(
                &mut run,
                &event(
                    1,
                    SupervisorEventKind::VerificationResult {
                        task_id: TaskId::new("verify").unwrap(),
                        generation: 1,
                        passed: false,
                        result_digest: "mismatch".into(),
                        safe_summary: safe_summary.into(),
                    },
                ),
            )
            .unwrap();
            assert_eq!(run.state, SupervisorRunState::Escalated);
            assert_eq!(run.escalation.as_ref().unwrap().safe_evidence, expected);
        }
    }

    #[test]
    fn legacy_verification_event_defaults_the_safe_summary() {
        let legacy = serde_json::json!({
            "VerificationResult": {
                "task_id": "verify",
                "generation": 1,
                "passed": false,
                "result_digest": "legacy-digest"
            }
        });
        let decoded: SupervisorEventKind = serde_json::from_value(legacy).unwrap();
        assert!(matches!(
            decoded,
            SupervisorEventKind::VerificationResult { safe_summary, .. }
                if safe_summary.is_empty()
        ));
    }

    #[test]
    fn policy_and_reducer_error_edges_are_explicit() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let id = TaskId::new("task").unwrap();
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id: OperationId::new(),
            worker_session_id: Some(SessionId::new()),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        assert!(matches!(
            dispatch(&mut run, &id, 1, provenance.clone(), now()),
            Err(SupervisorError::MissingTask)
        ));
        run.state = SupervisorRunState::WaitingForDecision;
        assert!(matches!(
            admit_dispatch(&run, &id, &provenance),
            Err(SupervisorError::PolicyDenied(reason)) if reason == "human decision required"
        ));
        run.state = SupervisorRunState::Planning;
        let mut dispatched_task = task(run.supervisor_run_id, "task", &[]);
        dispatched_task.state = TaskState::Dispatched;
        run.tasks.insert(id.clone(), dispatched_task);
        run.policy.max_concurrency = 1;
        assert!(matches!(
            admit_dispatch(&run, &id, &provenance),
            Err(SupervisorError::PolicyDenied(reason)) if reason == "concurrency limit reached"
        ));
        run.policy.max_concurrency = 2;
        let parent = TaskId::new("parent").unwrap();
        let parent_task = task(run.supervisor_run_id, "parent", &[]);
        run.tasks.insert(parent.clone(), parent_task);
        run.tasks.get_mut(&id).unwrap().parent_task_id = Some(parent);
        run.policy.max_depth = 0;
        assert!(matches!(
            admit_dispatch(&run, &id, &provenance),
            Err(SupervisorError::PolicyDenied(reason)) if reason == "maximum task depth exceeded"
        ));
        assert_eq!(
            retry_ready(&mut run, &id, 2, now()),
            Err(SupervisorError::StaleGeneration)
        );
        assert_eq!(
            verification_result(
                &mut run,
                &id,
                1,
                true,
                "digest",
                "",
                &event(
                    1,
                    SupervisorEventKind::SetRunState {
                        state: SupervisorRunState::Running,
                        terminal_reason: None
                    }
                )
            ),
            Err(SupervisorError::InvalidTransition)
        );
        assert_eq!(
            verification_result(
                &mut run,
                &id,
                2,
                true,
                "digest",
                "",
                &event(
                    1,
                    SupervisorEventKind::SetRunState {
                        state: SupervisorRunState::Running,
                        terminal_reason: None,
                    },
                ),
            ),
            Err(SupervisorError::StaleGeneration)
        );
        run.tasks.get_mut(&id).unwrap().state = TaskState::Ready;
        cancel(&mut run, None, "cancel", now()).unwrap();
        assert_eq!(run.tasks[&id].state, TaskState::Cancelled);
    }

    #[test]
    fn explicit_escalation_event_is_durable() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        reduce(
            &mut run,
            &event(
                1,
                SupervisorEventKind::Escalate {
                    task_id: None,
                    reason: "ambiguous provenance".into(),
                    safe_evidence: "fence mismatch".into(),
                    choices: vec!["cancel".into()],
                },
            ),
        )
        .unwrap();
        assert_eq!(run.state, SupervisorRunState::Escalated);
    }

    #[test]
    fn verification_reducer_propagates_an_invalid_gate_without_mutation() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let mut task = task(run.supervisor_run_id, "task", &[]);
        task.state = TaskState::Ready;
        let id = task.task_id.clone();
        run.tasks.insert(id.clone(), task);
        let before = run.clone();
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    1,
                    SupervisorEventKind::VerificationResult {
                        task_id: id,
                        generation: 1,
                        passed: true,
                        result_digest: "untrusted".into(),
                        safe_summary: String::new(),
                    },
                ),
            ),
            Err(SupervisorError::InvalidTransition)
        );
        assert_eq!(run, before);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn rejects_duplicate_parent_and_dispatch_fences() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let run_id = run.supervisor_run_id;
        let root = task(run.supervisor_run_id, "root", &[]);
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: root }),
        )
        .unwrap();
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::AddTask {
                        task: task(run_id, "root", &[])
                    }
                )
            ),
            Err(SupervisorError::DuplicateTask)
        );
        let mut orphan = task(run.supervisor_run_id, "orphan", &[]);
        orphan.parent_task_id = Some(TaskId::new("gone").unwrap());
        assert_eq!(
            reduce(
                &mut run,
                &event(2, SupervisorEventKind::AddTask { task: orphan })
            ),
            Err(SupervisorError::MissingTask)
        );
        let id = TaskId::new("root").unwrap();
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id: OperationId::new(),
            worker_session_id: Some(SessionId::new()),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::Dispatch {
                        task_id: id.clone(),
                        generation: 2,
                        provenance: provenance.clone()
                    }
                )
            ),
            Err(SupervisorError::StaleGeneration)
        );
        let mut wrong = provenance;
        wrong.task_id = TaskId::new("other").unwrap();
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::Dispatch {
                        task_id: id,
                        generation: 1,
                        provenance: wrong
                    }
                )
            ),
            Err(SupervisorError::ProvenanceMismatch)
        );
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .state = TaskState::Pending;
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::Dispatch {
                        task_id: TaskId::new("root").unwrap(),
                        generation: 1,
                        provenance: RunProvenance {
                            supervisor_run_id: run_id,
                            task_id: TaskId::new("root").unwrap(),
                            parent_task_id: None,
                            parent_dispatch_run: None,
                            dispatch_run_id: OperationId::new(),
                            worker_session_id: Some(SessionId::new()),
                            worker_agent_id: AgentRuntimeId::new(),
                            worker_worktree_id: WorktreeId::new(),
                            generation: 1
                        }
                    }
                )
            ),
            Err(SupervisorError::DependencyIncomplete)
        );
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .state = TaskState::Succeeded;
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    2,
                    SupervisorEventKind::SetTaskState {
                        task_id: TaskId::new("root").unwrap(),
                        generation: 1,
                        state: TaskState::Failed
                    }
                )
            ),
            Err(SupervisorError::InvalidTransition)
        );
    }

    #[test]
    fn child_dispatch_requires_its_parent_dispatch_provenance() {
        let mut run = SupervisorRun::new("c".into(), "t".into(), "i".into(), "p".into(), now());
        let root = task(run.supervisor_run_id, "root", &[]);
        let mut child = task(run.supervisor_run_id, "child", &[]);
        child.parent_task_id = Some(TaskId::new("root").unwrap());
        reduce(
            &mut run,
            &event(1, SupervisorEventKind::AddTask { task: root }),
        )
        .unwrap();
        reduce(
            &mut run,
            &event(2, SupervisorEventKind::AddTask { task: child }),
        )
        .unwrap();
        let root_id = TaskId::new("root").unwrap();
        let root_dispatch = OperationId::new();
        let root_provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: root_id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id: root_dispatch,
            worker_session_id: Some(SessionId::new()),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        reduce(
            &mut run,
            &event(
                3,
                SupervisorEventKind::Dispatch {
                    task_id: root_id,
                    generation: 1,
                    provenance: root_provenance,
                },
            ),
        )
        .unwrap();
        let child_id = TaskId::new("child").unwrap();
        let child_provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: child_id.clone(),
            parent_task_id: Some(TaskId::new("root").unwrap()),
            parent_dispatch_run: Some(root_dispatch),
            dispatch_run_id: OperationId::new(),
            worker_session_id: Some(SessionId::new()),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        };
        let mut mismatched_parent = child_provenance.clone();
        mismatched_parent.parent_dispatch_run = Some(OperationId::new());
        assert_eq!(
            reduce(
                &mut run,
                &event(
                    4,
                    SupervisorEventKind::Dispatch {
                        task_id: child_id.clone(),
                        generation: 1,
                        provenance: mismatched_parent,
                    },
                ),
            ),
            Err(SupervisorError::ProvenanceMismatch)
        );
        reduce(
            &mut run,
            &event(
                4,
                SupervisorEventKind::Dispatch {
                    task_id: child_id,
                    generation: 1,
                    provenance: child_provenance,
                },
            ),
        )
        .unwrap();
    }

    fn assert_escalation_resume_reset(run: &SupervisorRun) {
        assert_eq!(run.state, SupervisorRunState::Running);
        assert!(run.escalation.is_none());
        assert!(run.terminal_at.is_none());
        let root_id = TaskId::new("root").unwrap();
        let root = &run.tasks[&root_id];
        assert_eq!(root.state, TaskState::AwaitingDecision);
        assert_eq!(root.verification_digest, None);
        assert_eq!(root.verification_attempt, 0);
        assert_eq!(root.verification_retry_at, None);
        assert_eq!(root.verification_expectation, None);
        assert!(!run.verification_candidates.contains_key(&root_id));
    }

    #[test]
    fn escalation_resolution_is_fenced_and_applies_each_authorized_decision() {
        fn escalated() -> SupervisorRun {
            let mut run = SupervisorRun::new(
                "caller".into(),
                "task".into(),
                "input".into(),
                "policy".into(),
                now(),
            );
            let mut root = task(run.supervisor_run_id, "root", &[]);
            root.state = TaskState::Verifying;
            root.verification_digest = Some("rejected-digest".into());
            root.verification_attempt = 2;
            root.verification_retry_at = Some(now());
            root.verification_expectation = ArtifactExpectation::new(
                GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
                "0123456789012345678901234567890123456789",
            );
            let root_id = root.task_id.clone();
            run.tasks.insert(root_id.clone(), root);
            run.verification_candidates.insert(
                root_id.clone(),
                Some("https://github.com/acme/repo/pull/42".into()),
            );
            reduce(
                &mut run,
                &event(
                    1,
                    SupervisorEventKind::Escalate {
                        task_id: Some(root_id),
                        reason: "needs authority".into(),
                        safe_evidence: "safe".into(),
                        choices: vec!["resume".into(), "cancel".into(), "fail".into()],
                    },
                ),
            )
            .unwrap();
            run
        }

        let mut resumed = escalated();
        let escalation_id = resumed.escalation.as_ref().unwrap().escalation_id;
        assert_eq!(
            reduce(
                &mut resumed,
                &event(
                    2,
                    SupervisorEventKind::ResolveEscalation {
                        escalation_id: OperationId::new(),
                        decision: EscalationDecision::Resume,
                    },
                ),
            ),
            Err(SupervisorError::ProvenanceMismatch)
        );
        reduce(
            &mut resumed,
            &event(
                2,
                SupervisorEventKind::ResolveEscalation {
                    escalation_id,
                    decision: EscalationDecision::Resume,
                },
            ),
        )
        .unwrap();
        assert_escalation_resume_reset(&resumed);

        let mut cancelled = escalated();
        let escalation_id = cancelled.escalation.as_ref().unwrap().escalation_id;
        reduce(
            &mut cancelled,
            &event(
                2,
                SupervisorEventKind::ResolveEscalation {
                    escalation_id,
                    decision: EscalationDecision::Cancel,
                },
            ),
        )
        .unwrap();
        assert_eq!(cancelled.state, SupervisorRunState::Cancelled);

        let mut failed = escalated();
        let escalation_id = failed.escalation.as_ref().unwrap().escalation_id;
        reduce(
            &mut failed,
            &event(
                2,
                SupervisorEventKind::ResolveEscalation {
                    escalation_id,
                    decision: EscalationDecision::Fail,
                },
            ),
        )
        .unwrap();
        assert_eq!(failed.state, SupervisorRunState::Failed);
        assert_eq!(
            failed.tasks[&TaskId::new("root").unwrap()].state,
            TaskState::Failed
        );
    }

    #[test]
    fn identifiers_and_errors_cover_rejection_display_paths() {
        assert_eq!(
            TaskId::new(" \t").unwrap_err(),
            SupervisorError::InvalidTaskId
        );
        assert_eq!(
            TaskId::new("う".repeat(MAX_TASK_ID_BYTES / 3 + 1)).unwrap_err(),
            SupervisorError::InvalidTaskId
        );
        assert!(TaskId::new(format!("{}aa", "う".repeat((MAX_TASK_ID_BYTES - 2) / 3))).is_ok());
        assert_eq!(
            SupervisorError::SequenceGap {
                expected: 2,
                actual: 4,
            }
            .to_string(),
            "SequenceGap { expected: 2, actual: 4 }"
        );

        let v4 = uuid::Uuid::new_v4().hyphenated().to_string();
        assert!(serde_json::from_str::<SupervisorRunId>(&format!("\"{v4}\"")).is_err());
        let upper = SupervisorRunId::new().to_string().to_uppercase();
        assert!(serde_json::from_str::<SupervisorRunId>(&format!("\"{upper}\"")).is_err());
    }

    #[test]
    fn compacted_event_ids_are_refused_even_with_a_fresh_sequence() {
        let mut run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let old = event(
            1,
            SupervisorEventKind::SetRunState {
                state: SupervisorRunState::Running,
                terminal_reason: None,
            },
        );
        run.applied_events.insert(old.event_id);
        run.compact_applied_events(&BTreeSet::from([old.event_id]));
        assert!(run.compacted_event_tombstones.is_empty());
        run.compact_applied_events(&BTreeSet::new());
        assert_eq!(
            run.event_id_status(old.event_id),
            AppliedEventStatus::Expired
        );
        assert_eq!(reduce(&mut run, &old), Err(SupervisorError::ExpiredEventId));
        assert_eq!(run.state_revision, 0);
    }

    #[test]
    fn task_ids_and_compaction_state_fail_closed_when_deserialized() {
        assert!(serde_json::from_str::<TaskId>("\"task\"").is_ok());
        assert!(serde_json::from_str::<TaskId>("\"\"").is_err());
        assert!(
            serde_json::from_value::<TaskId>(serde_json::json!("x".repeat(MAX_TASK_ID_BYTES + 1)))
                .is_err()
        );

        let mut run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        run.compacted_event_tombstones.push(1);
        assert!(!run.compaction_state_is_valid());
        assert_eq!(
            run.event_id_status(OperationId::new()),
            AppliedEventStatus::Expired
        );
    }

    #[test]
    fn legacy_supervisor_values_default_only_their_compatible_additions() {
        let mut run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let task = task(run.supervisor_run_id, "root", &[]);
        run.tasks.insert(task.task_id.clone(), task.clone());
        let mut value = serde_json::to_value(&run).unwrap();
        value.as_object_mut().unwrap().remove("workspace_id");
        value.as_object_mut().unwrap().remove("artifact_repository");
        let serialized_task = value["tasks"]["root"].as_object_mut().unwrap();
        for field in [
            "promotion_reserved_at",
            "verification_attempt",
            "verification_retry_at",
            "verification_expectation",
        ] {
            serialized_task.remove(field);
        }
        let decoded: SupervisorRun = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.workspace_id, None);
        assert_eq!(decoded.artifact_repository, None);
        assert!(decoded.verification_candidates.is_empty());
        assert_eq!(decoded.tasks[&task.task_id].promotion_reserved_at, None);
        assert_eq!(decoded.tasks[&task.task_id].verification_attempt, 0);
        assert_eq!(decoded.tasks[&task.task_id].verification_retry_at, None);
        assert_eq!(decoded.tasks[&task.task_id].verification_expectation, None);

        let mut query_value = serde_json::to_value(TaskQuery::from(&task)).unwrap();
        query_value
            .as_object_mut()
            .unwrap()
            .remove("verification_attempt");
        query_value
            .as_object_mut()
            .unwrap()
            .remove("verification_retry_at");
        let decoded: TaskQuery = serde_json::from_value(query_value).unwrap();
        assert_eq!(decoded.verification_attempt, 0);
        assert_eq!(decoded.verification_retry_at, None);
    }

    #[test]
    fn workspace_commands_project_their_exact_run_fence() {
        let supervisor_run_id = SupervisorRunId::new();
        assert_eq!(
            SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id,
                reason: "operator cancelled".into(),
            }
            .supervisor_run_id(),
            supervisor_run_id
        );
        assert_eq!(
            SupervisorWorkspaceCommand::ResolveEscalation {
                supervisor_run_id,
                escalation_id: OperationId::new(),
                decision: EscalationDecision::Resume,
            }
            .supervisor_run_id(),
            supervisor_run_id
        );
    }
}
