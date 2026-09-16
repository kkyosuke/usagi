//! Event-driven bridge between durable dispatch completion and supervisor runs.
//!
//! The daemon owns one [`SupervisorRuntime`] and calls [`SupervisorRuntime::tick`]
//! for an arriving completion, startup reconciliation, or an explicit wake.  A
//! tick never polls: it only examines the named run, persists reducer facts and
//! wake reservations, then performs the finite set of reserved wake effects.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use usagi_core::{
    domain::{
        agent::{Agent, AgentProfileId, InboxKind, RunStatus, StructuredResult},
        id::{
            AgentId, AgentRuntimeId, AgentRuntimeRef, OperationId, SessionId, WorkspaceId,
            WorktreeId,
        },
        pr_inventory::{GitHubRepository, canonicalize},
        supervisor::{
            ARTIFACT_RETRY_BASE_SECONDS, ARTIFACT_RETRY_MAX_SECONDS, ArtifactContract,
            ArtifactExpectation, EscalationDecision, GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            HandoffContextEntry, MAX_HANDOFF_ARTIFACT_BYTES, MAX_HANDOFF_PROMPT_BYTES,
            MAX_HANDOFF_SUMMARY_BYTES, MAX_INITIAL_TASKS, MAX_SUPERVISOR_DISPLAY_LABEL_BYTES,
            MAX_SUPERVISOR_KEY_BYTES, MAX_SUPERVISOR_REASON_BYTES, MAX_SUPERVISOR_TEXT_BYTES,
            MAX_SUPERVISOR_WORKSPACE_SNAPSHOT_RUNS, MAX_TASK_DEPENDENCIES, NO_ARTIFACT_CONTRACT,
            RunProvenance, SupervisorEvent, SupervisorEventKind, SupervisorEventSource,
            SupervisorRun, SupervisorRunId, SupervisorRunQuery, SupervisorRunState,
            SupervisorWorkspaceCommand, TaskId, TaskNode, TaskState,
            admit_child_dispatch_reservation, presentation_text_is_safe, reduce,
        },
    },
    infrastructure::{
        persistence::json_file,
        store::{
            dispatch::DispatchStore,
            supervisor::{
                EventCursor, EventQuery, RUN_LIST_RESPONSE_MAX_BYTES, SupervisorRunPage,
                SupervisorStore,
            },
        },
    },
};

const MISSING_DISPATCH_ESCALATION_REASON: &str =
    "no worker dispatch reservation was produced for a ready task";

/// Redaction-safe input delivered to the parent-agent wake adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionWake {
    pub supervisor_run_id: SupervisorRunId,
    pub parent_task_id: TaskId,
    pub parent_generation: u64,
    pub parent: RunProvenance,
    pub child_run_id: OperationId,
    pub outcome: WakeOutcome,
    pub dag: Vec<(TaskId, TaskState)>,
    pub remaining_budget_summary: String,
}

/// The safe terminal fact passed to a decision maker; worker terminal output is
/// deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeOutcome {
    pub kind: InboxKind,
    pub summary: String,
}

/// Redaction-safe result produced outside the supervisor lock by an independent
/// artifact verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactVerification {
    pub status: ArtifactVerificationStatus,
    pub result_digest: String,
    pub safe_summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactVerificationStatus {
    Verified,
    Rejected,
    Retryable,
}

/// Goal instruction and repository provenance admitted as one semantic unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalSpecification {
    pub instruction: String,
    pub artifact_repository: GitHubRepository,
}

impl GoalSpecification {
    #[must_use]
    pub const fn new(instruction: String, artifact_repository: GitHubRepository) -> Self {
        Self {
            instruction,
            artifact_repository,
        }
    }
}

/// Provider boundary used after a worker completion has moved a contracted
/// task into `Verifying`. Worker-controlled output is input, never authority.
pub trait ArtifactVerifier {
    fn verify(
        &mut self,
        contract: ArtifactContract,
        result: Option<&StructuredResult>,
        expectation: &ArtifactExpectation,
        previous_verification_digest: Option<&str>,
    ) -> ArtifactVerification;
}

/// Exact checkout identity which may have contributed the Goal artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArtifactWorktreeRef {
    pub session_id: Option<SessionId>,
    pub worktree_id: WorktreeId,
}

/// Exact task fence and worker-reported candidate prepared under the supervisor
/// lock, then independently verified without holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactVerificationRequest {
    pub supervisor_run_id: SupervisorRunId,
    pub task_id: TaskId,
    pub generation: u64,
    pub verification_attempt: u32,
    pub previous_verification_digest: Option<String>,
    pub workspace_id: WorkspaceId,
    pub contract: ArtifactContract,
    pub repository: GitHubRepository,
    pub result: Option<StructuredResult>,
    pub expectation: Option<ArtifactExpectation>,
    pub worktrees: Vec<ArtifactWorktreeRef>,
}

enum ArtifactReportTrigger {
    Recovery,
    Fresh(Option<StructuredResult>),
}

/// Durable Goal operation whose reserved root still needs exact provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGoalPromotion {
    pub operation_id: String,
    pub reserved_at: DateTime<Utc>,
    pub workspace_id: WorkspaceId,
    pub worker_profile_id: Option<AgentProfileId>,
    pub worker_semantic_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCallerPromotion {
    pub start_operation_id: String,
    pub dispatch_operation_id: String,
    pub workspace_id: WorkspaceId,
    pub worker_session_id: Option<SessionId>,
    pub worker_agent_id: AgentId,
    pub worker_profile_id: AgentProfileId,
    pub worker_runtime_id: AgentRuntimeId,
    pub worker_semantic_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDelegatedPromotion {
    pub operation_id: String,
    pub reserved_at: DateTime<Utc>,
    pub workspace_id: WorkspaceId,
    pub worker_session_id: Option<SessionId>,
    pub worker_agent_id: Option<AgentId>,
    pub worker_profile_id: Option<AgentProfileId>,
    pub worker_semantic_digest: Option<String>,
}

/// Exact prompt snapshot reserved for one supervised child admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedDispatchReservation {
    pub run: SupervisorRunQuery,
    pub prompt: String,
}

/// Opaque identity of the exact Supervisor task generation which owns an
/// authenticated dispatch. Composition compares this before and after session
/// creation so an A-to-B ownership replacement cannot pass a boolean check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchSupervisionFence {
    supervisor_run_id: SupervisorRunId,
    task_id: TaskId,
    generation: u64,
}

/// Completed contracted dispatch whose independent artifact verification has
/// not reached a terminal supervisor state yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingArtifactVerification {
    pub dispatch_run_id: OperationId,
}

/// Exact live Agent and safe context needed for a human-requested retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryWork {
    pub provenance: RunProvenance,
    pub reason: String,
    pub safe_evidence: String,
}

/// An aborted task whose Agent admission may have succeeded before Supervisor
/// provenance was bound. The durable Supervisor reservation prepares every
/// field except the Agent-owned runtime fence; recovery joins that exact
/// operation outcome without guessing by workspace, session name, or process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWorkerStop {
    operation_id: OperationId,
    workspace_id: WorkspaceId,
    supervisor_run_id: SupervisorRunId,
    task_id: TaskId,
    parent_task_id: Option<TaskId>,
    parent_dispatch_run: Option<OperationId>,
    generation: u64,
    requires_session: bool,
    worker_session_id: Option<SessionId>,
    worker_agent_id: Option<AgentId>,
    worker_runtime_id: Option<AgentRuntimeId>,
    worker_profile_id: Option<AgentProfileId>,
    worker_semantic_digest: Option<String>,
}

impl PendingWorkerStop {
    #[must_use]
    pub const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Completes the stop fence from the exact Agent operation outcome.
    ///
    /// # Errors
    /// Returns an error when the admitted runtime is outside the reserved
    /// workspace or root/delegated scope.
    pub fn provenance(&self, worker: &AgentRuntimeRef) -> Result<RunProvenance> {
        if !self.matches_worker_scope(worker) {
            anyhow::bail!("unbound supervisor worker is outside its reserved scope");
        }
        Ok(RunProvenance {
            supervisor_run_id: self.supervisor_run_id,
            task_id: self.task_id.clone(),
            parent_task_id: self.parent_task_id.clone(),
            parent_dispatch_run: self.parent_dispatch_run,
            dispatch_run_id: self.operation_id,
            worker_session_id: worker.session_id,
            worker_agent_id: worker.agent_runtime_id,
            worker_worktree_id: worker.terminal.worktree_id,
            generation: self.generation,
        })
    }

    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    /// Whether an Agent operation outcome belongs to this exact reserved
    /// workspace/session scope. A mismatch proves an operation-ID collision;
    /// it must neither be bound nor interrupted as this Supervisor worker.
    #[must_use]
    pub fn matches_worker_scope(&self, worker: &AgentRuntimeRef) -> bool {
        let session_matches = match self.worker_session_id {
            Some(expected) => worker.session_id == Some(expected),
            None => true,
        };
        let runtime_matches = match self.worker_runtime_id {
            Some(expected) => worker.agent_runtime_id == expected,
            None => true,
        };
        worker.terminal.workspace_id == self.workspace_id
            && worker.terminal.session_id == worker.session_id
            && self.requires_session == worker.session_id.is_some()
            && session_matches
            && runtime_matches
    }

    #[must_use]
    pub fn worker_profile_id(&self) -> Option<&AgentProfileId> {
        self.worker_profile_id.as_ref()
    }

    #[must_use]
    pub const fn worker_agent_id(&self) -> Option<AgentId> {
        self.worker_agent_id
    }

    #[must_use]
    pub fn worker_semantic_digest(&self) -> Option<&str> {
        self.worker_semantic_digest.as_deref()
    }
}

/// Composition-root adapter. Implementations use the persisted parent
/// provenance to resolve/restart the parent session and send the request.
pub trait DecisionWaker {
    /// # Errors
    ///
    /// Returns an error when the parent session cannot safely receive the wake.
    fn wake(&mut self, wake: &DecisionWake) -> Result<()>;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RuntimeState {
    wakes: BTreeMap<String, WakeReservation>,
    starts: BTreeMap<String, StartReservation>,
    #[serde(default)]
    controls: BTreeMap<String, ControlReservation>,
    #[serde(default)]
    expired_wakes: KeyTombstones,
    #[serde(default)]
    expired_starts: KeyTombstones,
    #[serde(default)]
    expired_controls: KeyTombstones,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WakeReservation {
    wake: DecisionWake,
    delivered: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StartReservation {
    semantic_key: String,
    supervisor_run_id: SupervisorRunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artifact_repository: Option<GitHubRepository>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_id: Option<WorkspaceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    caller_dispatch_run_id: Option<OperationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_runtime_id: Option<AgentRuntimeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_profile_id: Option<AgentProfileId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_semantic_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_field_names)] // Each field names the exact durable identity it fences.
struct CallerDispatchReservation {
    dispatch_run_id: OperationId,
    worker_session_id: Option<SessionId>,
    worker_agent_id: AgentId,
    worker_runtime_id: AgentRuntimeId,
}

/// Complete start intent passed from the public admission variants to the
/// single durable start transaction. Grouping these fields keeps workspace,
/// artifact, worker, and caller-dispatch fences together instead of allowing
/// positional `Option` arguments to be swapped accidentally.
struct SupervisorStartRequest<'a> {
    caller: &'a str,
    workspace: Option<WorkspaceId>,
    operation_id: &'a str,
    root_task: String,
    root_artifact_contract: ArtifactContract,
    artifact_repository: Option<GitHubRepository>,
    worker_profile_id: Option<AgentProfileId>,
    worker_semantic_digest: Option<String>,
    caller_dispatch: Option<&'a CallerDispatchReservation>,
    initial_tasks: Vec<InitialTask>,
    policy_selector: Option<String>,
    now: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ControlReservation {
    semantic_digest: String,
    supervisor_run_id: SupervisorRunId,
    reserved_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct KeyTombstones {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    words: Vec<u64>,
}

const TOMBSTONE_WORDS: usize = 512;
const TOMBSTONE_HASHES: u64 = 4;
const DELEGATED_TASK_PREFIX: &str = "delegated-";
const DELEGATED_TASK_DIGEST_PREFIX: &str = "delegated-operation:";
const AMBIGUOUS_STOP_RESERVATION: &str =
    "Agent operation belongs to multiple aborted supervisor reservations";
#[cfg(not(test))]
const MAX_START_RESERVATIONS: usize = 256;
#[cfg(test)]
const MAX_START_RESERVATIONS: usize = 8;
#[cfg(not(test))]
const MAX_WAKE_RESERVATIONS: usize = 512;
#[cfg(test)]
const MAX_WAKE_RESERVATIONS: usize = 8;
#[cfg(not(test))]
const RETAIN_DELIVERED_WAKES: usize = 128;
/// Runtime metadata is rewritten atomically and read on recovery paths. Bound
/// the complete document so a corrupt or legacy payload cannot dictate memory.
const MAX_RUNTIME_STATE_BYTES: usize = 16 * 1024 * 1024;
#[cfg(not(test))]
const MAX_CONTROL_RESERVATIONS: usize = 512;
#[cfg(test)]
const MAX_CONTROL_RESERVATIONS: usize = 8;
#[cfg(test)]
const RETAIN_DELIVERED_WAKES: usize = 4;

/// Applies the same serialized budget to every read-only supervisor query, not
/// only list pages. The caller maps this capacity refusal to `resource_exhausted`.
///
/// # Errors
/// Returns an error when serialization fails or the response exceeds the budget.
pub fn bounded_supervisor_query(value: serde_json::Value) -> Result<serde_json::Value> {
    if serde_json::to_vec(&value)?.len() > RUN_LIST_RESPONSE_MAX_BYTES {
        anyhow::bail!("supervisor query response capacity is exhausted");
    }
    Ok(value)
}

impl KeyTombstones {
    fn bit(key: &str, seed: u64) -> usize {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        for byte in key.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        usize::try_from(hash % (TOMBSTONE_WORDS as u64 * 64)).expect("bit index fits")
    }

    fn contains(&self, key: &str) -> bool {
        self.words.len() == TOMBSTONE_WORDS
            && (0..TOMBSTONE_HASHES).all(|seed| {
                let bit = Self::bit(key, seed);
                self.words[bit / 64] & (1_u64 << (bit % 64)) != 0
            })
    }

    fn insert(&mut self, key: &str) {
        self.words.resize(TOMBSTONE_WORDS, 0);
        self.words.truncate(TOMBSTONE_WORDS);
        for seed in 0..TOMBSTONE_HASHES {
            let bit = Self::bit(key, seed);
            self.words[bit / 64] |= 1_u64 << (bit % 64);
        }
    }
}

impl RuntimeState {
    fn validate_limits(&self) -> Result<()> {
        let tombstones_are_valid = |tombstones: &KeyTombstones| {
            tombstones.words.is_empty() || tombstones.words.len() == TOMBSTONE_WORDS
        };
        let controls_are_valid = self.controls.iter().all(|(operation, reservation)| {
            OperationId::parse(operation).is_ok()
                && is_semantic_digest(&reservation.semantic_digest)
        });
        let starts_are_valid = self.starts.iter().all(|(operation, reservation)| {
            let caller_shape = reservation.caller_dispatch_run_id.is_some()
                == reservation.worker_agent_id.is_some()
                && reservation.caller_dispatch_run_id.is_some()
                    == reservation.worker_runtime_id.is_some()
                && (reservation.caller_dispatch_run_id.is_none()
                    || (reservation.worker_profile_id.is_some()
                        && reservation.worker_semantic_digest.is_some()
                        && reservation.workspace_id.is_some()))
                && (reservation.caller_dispatch_run_id.is_some()
                    || reservation.worker_session_id.is_none());
            !operation.is_empty() && is_semantic_digest(&reservation.semantic_key) && caller_shape
        });
        if self.starts.len() > MAX_START_RESERVATIONS
            || self.wakes.len() > MAX_WAKE_RESERVATIONS
            || self.controls.len() > MAX_CONTROL_RESERVATIONS
            || !starts_are_valid
            || !controls_are_valid
            || !tombstones_are_valid(&self.expired_starts)
            || !tombstones_are_valid(&self.expired_wakes)
            || !tombstones_are_valid(&self.expired_controls)
        {
            anyhow::bail!("supervisor runtime metadata exceeds or violates its hard limit");
        }
        Ok(())
    }

    fn migrate_start_semantics(&mut self) -> bool {
        let mut changed = false;
        for reservation in self.starts.values_mut() {
            if !is_semantic_digest(&reservation.semantic_key) {
                reservation.semantic_key = semantic_digest(reservation.semantic_key.as_bytes());
                changed = true;
            }
        }
        changed
    }

    fn compact_delivered_wakes(&mut self) {
        let undelivered = self
            .wakes
            .values()
            .filter(|reservation| !reservation.delivered)
            .count();
        let keep_delivered = RETAIN_DELIVERED_WAKES.min(
            MAX_WAKE_RESERVATIONS
                .saturating_sub(undelivered)
                .min(self.wakes.len()),
        );
        let remove = self
            .wakes
            .values()
            .filter(|reservation| reservation.delivered)
            .count()
            .saturating_sub(keep_delivered);
        let keys = self
            .wakes
            .iter()
            .filter(|(_, reservation)| reservation.delivered)
            .take(remove)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            self.wakes.remove(&key);
            self.expired_wakes.insert(&key);
        }
    }
}

fn is_semantic_digest(value: &str) -> bool {
    value.len() == 71
        && value.strip_prefix("sha256:").is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialTask {
    pub task_id: String,
    /// Durable manager task. Omitted children belong to the root Director task.
    #[serde(default)]
    pub parent_task_id: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub instruction: String,
    #[serde(default)]
    pub required_artifact_contract: ArtifactContract,
}

fn bounded_nonempty(name: &str, value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max {
        anyhow::bail!("invalid {name}: expected 1..={max} UTF-8 bytes");
    }
    Ok(())
}

fn bounded_safe_label(name: &str, value: &str, max: usize) -> Result<()> {
    bounded_nonempty(name, value, max)?;
    if !presentation_text_is_safe(value) {
        anyhow::bail!(
            "invalid {name}: control and bidirectional formatting characters are forbidden"
        );
    }
    Ok(())
}

fn validate_start_input(
    operation_id: &str,
    root_task: &str,
    initial_tasks: &[InitialTask],
    policy_selector: Option<&str>,
) -> Result<()> {
    bounded_nonempty(
        "supervisor idempotency key",
        operation_id,
        MAX_SUPERVISOR_KEY_BYTES,
    )?;
    bounded_nonempty("supervisor root task", root_task, MAX_SUPERVISOR_TEXT_BYTES)?;
    if initial_tasks.len() > MAX_INITIAL_TASKS {
        anyhow::bail!("invalid initial task count: maximum is {MAX_INITIAL_TASKS}");
    }
    if let Some(policy_selector) = policy_selector {
        bounded_nonempty(
            "supervisor policy selector",
            policy_selector,
            MAX_SUPERVISOR_KEY_BYTES,
        )?;
    }
    for task in initial_tasks {
        TaskId::new(&task.task_id).map_err(anyhow::Error::msg)?;
        if let Some(parent) = &task.parent_task_id {
            TaskId::new(parent).map_err(anyhow::Error::msg)?;
        }
        if task.dependencies.len() > MAX_TASK_DEPENDENCIES {
            anyhow::bail!("invalid task dependency count: maximum is {MAX_TASK_DEPENDENCIES}");
        }
        for dependency in &task.dependencies {
            TaskId::new(dependency).map_err(anyhow::Error::msg)?;
        }
        bounded_nonempty(
            "supervisor task instruction",
            &task.instruction,
            MAX_SUPERVISOR_TEXT_BYTES,
        )?;
    }
    Ok(())
}

fn control_semantic_digest(command: &SupervisorWorkspaceCommand) -> Result<String> {
    let encoded = serde_json::to_vec(command)?;
    Ok(semantic_digest(&encoded))
}

fn semantic_digest(value: &[u8]) -> String {
    encode_digest(Sha256::digest(value))
}

fn encode_digest(digest: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = digest.as_ref();
    let mut value = String::with_capacity("sha256:".len() + digest.len() * 2);
    value.push_str("sha256:");
    for &byte in digest {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}

fn validate_control_command(command: &SupervisorWorkspaceCommand) -> Result<()> {
    if let SupervisorWorkspaceCommand::Cancel { reason, .. } = command {
        bounded_safe_label(
            "supervisor cancellation reason",
            reason,
            MAX_SUPERVISOR_REASON_BYTES,
        )?;
    }
    Ok(())
}

fn control_event(
    run: &SupervisorRun,
    operation_id: OperationId,
    semantic_digest: String,
    command: &SupervisorWorkspaceCommand,
    now: DateTime<Utc>,
) -> SupervisorEvent {
    let (source, kind) = match command {
        SupervisorWorkspaceCommand::Cancel { reason, .. } => (
            SupervisorEventSource::Cancel,
            SupervisorEventKind::Cancel {
                task_id: None,
                reason: reason.clone(),
            },
        ),
        SupervisorWorkspaceCommand::ResolveEscalation {
            escalation_id,
            decision,
            ..
        } => (
            SupervisorEventSource::Admission,
            SupervisorEventKind::ResolveEscalation {
                escalation_id: *escalation_id,
                decision: *decision,
            },
        ),
        SupervisorWorkspaceCommand::Delete { .. } => {
            unreachable!("history deletion does not append an aggregate event")
        }
    };
    SupervisorEvent {
        sequence: run.state_revision + 1,
        event_id: operation_id,
        causation_id: None,
        correlation_id: None,
        observed_at: now,
        payload_digest: semantic_digest,
        source,
        kind,
    }
}

fn update_semantic_component(digest: &mut Sha256, value: &str) {
    digest.update(value.len().to_string().as_bytes());
    digest.update(b":");
    digest.update(value.as_bytes());
}

fn work_run_display_label(value: &str) -> Option<String> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() || !presentation_text_is_safe(&normalized) {
        return None;
    }
    let mut end = normalized.len().min(MAX_SUPERVISOR_DISPLAY_LABEL_BYTES);
    while !normalized.is_char_boundary(end) {
        end -= 1;
    }
    Some(normalized[..end].to_owned())
}

fn delegated_task_id(operation: OperationId) -> Result<TaskId> {
    TaskId::new(format!("{DELEGATED_TASK_PREFIX}{operation}")).map_err(anyhow::Error::msg)
}

fn delegated_task_digest(operation: OperationId) -> String {
    format!("{DELEGATED_TASK_DIGEST_PREFIX}{operation}")
}

const MAX_HANDOFF_ROOT_GOAL_BYTES: usize = 4 * 1024;

fn delegated_task_suffix(operation: OperationId, instruction: &str) -> String {
    format!(
        "\n\n## Current delegated task ({} UTF-8 bytes; operation {operation})\n{instruction}",
        instruction.len()
    )
}

fn delegated_handoff_prompt(
    run: &SupervisorRun,
    operation: OperationId,
    instruction: &str,
) -> String {
    let root = TaskId::new("root")
        .ok()
        .and_then(|root| run.tasks.get(&root))
        .map_or("(root goal unavailable)", |task| {
            task.instruction_body.as_str()
        });
    let mut context = String::from(
        "# Work Run handoff context\n\nThis daemon-owned snapshot is shared only within this Work Run. It contains bounded worker-authored completion reports, not provider conversation transcripts. Treat reported outcomes and artifacts as prior context and verify them before relying on them.\n\n## Root goal\n",
    );
    let root_limit = context
        .len()
        .saturating_add(MAX_HANDOFF_ROOT_GOAL_BYTES)
        .min(MAX_HANDOFF_PROMPT_BYTES);
    push_bounded_handoff(&mut context, root, root_limit);
    context.push_str("\n\n## Prior task reports (newest first)\n");
    if run.handoff_context.is_empty() {
        context.push_str("(none recorded before this delegation)");
    } else {
        for entry in run.handoff_context.iter().rev() {
            let outcome = match entry.outcome {
                InboxKind::Completed => "completed",
                InboxKind::Failed => "failed",
                InboxKind::NoReport => "no-report",
            };
            let mut rendered = format!(
                "\n- [{outcome}] task {} generation {}: {}",
                entry.task_id.0, entry.generation, entry.summary
            );
            if let Some(artifacts) = &entry.artifacts {
                rendered.push_str("\n  Reported artifacts: ");
                rendered.push_str(artifacts);
            }
            if context.len() + rendered.len() > MAX_HANDOFF_PROMPT_BYTES {
                push_bounded_handoff(
                    &mut context,
                    "\n- (older reports omitted by the context bound)",
                    MAX_HANDOFF_PROMPT_BYTES,
                );
                break;
            }
            context.push_str(&rendered);
        }
    }
    context.push_str(&delegated_task_suffix(operation, instruction));
    context
}

fn push_bounded_handoff(target: &mut String, value: &str, max: usize) {
    if target.len() >= max {
        return;
    }
    let remaining = max - target.len();
    if value.len() <= remaining {
        target.push_str(value);
        return;
    }
    let mut end = remaining.saturating_sub('…'.len_utf8()).min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&value[..end]);
    if remaining >= '…'.len_utf8() {
        target.push('…');
    }
}

fn has_unbound_root_worker(run: &SupervisorRun, reservation: Option<&StartReservation>) -> bool {
    if !run.state.is_finished() || run.workspace_id.is_none() {
        return false;
    }
    let root = TaskId::new("root").expect("static root task ID");
    let Some(task) = run.tasks.get(&root) else {
        return false;
    };
    let caller_reserved = match reservation {
        Some(item) => item.caller_dispatch_run_id.is_some(),
        None => false,
    };
    (task.required_artifact_contract == GOAL_REVIEW_READY_ARTIFACT_CONTRACT
        || task.promotion_reserved_at.is_some()
        || caller_reserved)
        && task.assigned_dispatch_run.is_none()
        && !run.provenance.contains_key(&root)
}

fn is_delegated_reservation(task: &TaskNode, operation: OperationId) -> bool {
    task.task_id.0 == format!("{DELEGATED_TASK_PREFIX}{operation}")
        && task.instruction_digest == delegated_task_digest(operation)
}

fn delegated_worker_semantic_digest(
    worker_agent_id: Option<AgentId>,
    session_name: Option<&str>,
    prompt: &str,
) -> Option<String> {
    let (Some(worker_agent_id), Some(session_name)) = (worker_agent_id, session_name) else {
        return None;
    };
    Some(usagi_core::infrastructure::ipc::agent_operation_digest(
        &usagi_core::infrastructure::ipc::agent_dispatch_semantic_key(
            session_name,
            worker_agent_id,
            prompt,
        ),
    ))
}

fn has_caller_root_reservation(state: &RuntimeState, run_id: SupervisorRunId) -> bool {
    for reservation in state.starts.values() {
        if reservation.supervisor_run_id == run_id && reservation.caller_dispatch_run_id.is_some() {
            return true;
        }
    }
    false
}

fn child_dispatch_policy_denial(
    run: &SupervisorRun,
    parent_task_id: &TaskId,
) -> Result<Option<String>> {
    match admit_child_dispatch_reservation(run, parent_task_id) {
        Ok(()) => Ok(None),
        Err(usagi_core::domain::supervisor::SupervisorError::PolicyDenied(reason)) => {
            Ok(Some(reason))
        }
        Err(error) => Err(anyhow::Error::new(error)),
    }
}

fn delegated_worker_matches_reservation(
    run_workspace_id: Option<WorkspaceId>,
    worker: &AgentRuntimeRef,
    child_agent: Option<&Agent>,
    task: &TaskNode,
    child_dispatch: &usagi_core::domain::agent::DispatchRun,
    child_semantic_digest: Option<&String>,
) -> bool {
    let workspace_matches = run_workspace_id == Some(worker.terminal.workspace_id);
    let has_session = worker.session_id.is_some();
    let child_session_matches = match child_agent {
        Some(agent) => agent.session_id == worker.session_id,
        None => true,
    };
    let reserved_session_matches = match task.promotion_worker_session_id {
        Some(expected) => worker.session_id == Some(expected),
        None => true,
    };
    let profile_matches = match task.promotion_worker_profile_id.as_ref() {
        Some(expected) => match child_agent {
            Some(agent) => &agent.runtime == expected,
            None => false,
        },
        None => true,
    };
    let agent_matches = match task.promotion_worker_agent_id {
        Some(expected) => child_dispatch.agent_id == expected,
        None => true,
    };
    let semantic_matches = match task.promotion_worker_semantic_digest.as_ref() {
        Some(expected) => child_semantic_digest == Some(expected),
        None => true,
    };
    workspace_matches
        && has_session
        && child_session_matches
        && reserved_session_matches
        && profile_matches
        && agent_matches
        && semantic_matches
}

fn validate_provenance_chain(
    run: &SupervisorRun,
    task_id: &TaskId,
    provenance: &RunProvenance,
) -> Result<()> {
    let mut task_id = task_id;
    let mut provenance = provenance;
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(task_id.clone()) {
            anyhow::bail!("supervisor provenance parent chain contains a cycle");
        }
        let task = run
            .tasks
            .get(task_id)
            .context("supervisor provenance task is missing")?;
        if task.supervisor_run_id != run.supervisor_run_id
            || provenance.supervisor_run_id != run.supervisor_run_id
            || provenance.task_id != *task_id
            || provenance.generation != task.generation
            || task.assigned_dispatch_run != Some(provenance.dispatch_run_id)
            || provenance.parent_task_id != task.parent_task_id
            || task.promotion_reserved_at.is_some()
            || task
                .promotion_parent_dispatch_run
                .is_some_and(|parent| provenance.parent_dispatch_run != Some(parent))
            || task
                .promotion_worker_session_id
                .is_some_and(|session| provenance.worker_session_id != Some(session))
        {
            anyhow::bail!("supervisor provenance fence is stale");
        }
        let Some(parent_task_id) = task.parent_task_id.as_ref() else {
            if provenance.parent_dispatch_run.is_some()
                || task.promotion_parent_dispatch_run.is_some()
            {
                anyhow::bail!("supervisor root provenance has a parent dispatch");
            }
            return Ok(());
        };
        let parent_dispatch_run = provenance
            .parent_dispatch_run
            .context("supervisor child provenance has no parent dispatch")?;
        run.tasks
            .get(parent_task_id)
            .filter(|parent| parent.supervisor_run_id == run.supervisor_run_id)
            .context("supervisor provenance parent task is missing")?;
        if task.promotion_parent_dispatch_run == Some(parent_dispatch_run) {
            return Ok(());
        }
        let parent = run
            .provenance
            .get(parent_task_id)
            .filter(|parent| parent.dispatch_run_id == parent_dispatch_run)
            .context("supervisor provenance parent authority is missing")?;
        task_id = parent_task_id;
        provenance = parent;
    }
}

#[derive(Debug, Clone, Copy)]
struct TaskDispatchAuthority {
    operation_id: OperationId,
    committed: bool,
}

fn live_task_dispatch_authority(
    state: &RuntimeState,
    run: &SupervisorRun,
    task_id: &TaskId,
    visiting: &mut BTreeSet<TaskId>,
) -> Result<Option<TaskDispatchAuthority>> {
    if !visiting.insert(task_id.clone()) {
        anyhow::bail!("supervisor promotion parent chain contains a cycle");
    }
    let result = live_task_dispatch_authority_inner(state, run, task_id, visiting);
    visiting.remove(task_id);
    result
}

fn live_task_dispatch_authority_inner(
    state: &RuntimeState,
    run: &SupervisorRun,
    task_id: &TaskId,
    visiting: &mut BTreeSet<TaskId>,
) -> Result<Option<TaskDispatchAuthority>> {
    if let Some(provenance) = run.provenance.get(task_id) {
        validate_provenance_chain(run, task_id, provenance)?;
        return Ok(Some(TaskDispatchAuthority {
            operation_id: provenance.dispatch_run_id,
            committed: true,
        }));
    }
    let Some(task) = run.tasks.get(task_id) else {
        return Ok(None);
    };
    if task.supervisor_run_id != run.supervisor_run_id
        || task.generation != 1
        || task.assigned_dispatch_run.is_some()
        || task.state != TaskState::Ready
    {
        anyhow::bail!("supervisor promotion task fence is stale");
    }
    if task_id.0 == "root" {
        if task.parent_task_id.is_some()
            || task.promotion_parent_dispatch_run.is_some()
            || task.promotion_worker_session_id.is_some()
        {
            anyhow::bail!("Goal root promotion shape is stale");
        }
        let mut matches = state
            .starts
            .iter()
            .filter(|(_, reservation)| reservation.supervisor_run_id == run.supervisor_run_id)
            .filter(|(_, reservation)| {
                (task.required_artifact_contract == GOAL_REVIEW_READY_ARTIFACT_CONTRACT
                    && reservation.caller_dispatch_run_id.is_none())
                    || (task.required_artifact_contract == NO_ARTIFACT_CONTRACT
                        && reservation.caller_dispatch_run_id.is_some())
            })
            .map(|(operation_id, reservation)| {
                reservation
                    .caller_dispatch_run_id
                    .map_or_else(|| OperationId::parse(operation_id), Ok)
            });
        let Some(operation) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            anyhow::bail!("supervisor root has multiple promotion reservations");
        }
        let operation = operation.context("supervisor root promotion operation is invalid")?;
        return Ok(Some(TaskDispatchAuthority {
            operation_id: operation,
            committed: false,
        }));
    }

    let Some(operation_id) = task_id.0.strip_prefix(DELEGATED_TASK_PREFIX) else {
        return Ok(None);
    };
    let operation = OperationId::parse(operation_id)
        .context("delegated parent promotion operation is invalid")?;
    if !is_delegated_reservation(task, operation)
        || task.required_artifact_contract != NO_ARTIFACT_CONTRACT
        || task.promotion_reserved_at.is_none()
    {
        return Ok(None);
    }
    let parent_task_id = task
        .parent_task_id
        .as_ref()
        .context("delegated parent promotion has no parent task")?;
    if task.promotion_parent_dispatch_run.is_some() {
        run.tasks
            .get(parent_task_id)
            .filter(|parent| parent.supervisor_run_id == run.supervisor_run_id)
            .context("delegated parent promotion task is missing")?;
    } else {
        let parent = live_task_dispatch_authority(state, run, parent_task_id, visiting)?
            .context("delegated parent promotion authority is missing")?;
        if !parent.committed {
            anyhow::bail!("delegated parent promotion has no durable parent fence");
        }
    }
    Ok(Some(TaskDispatchAuthority {
        operation_id: operation,
        committed: false,
    }))
}

/// The single daemon-owned scheduler runtime. It is intentionally independent
/// of IPC connections: disconnecting a client cannot drop reservations.
pub struct SupervisorRuntime {
    supervisor: SupervisorStore,
    dispatch: DispatchStore,
    state_path: PathBuf,
    apply_fail_at: Cell<Option<usize>>,
    apply_calls: Cell<usize>,
    #[cfg(test)]
    dispatch_registry_reads: Cell<usize>,
}

impl SupervisorRuntime {
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            supervisor: SupervisorStore::new(state_dir),
            dispatch: DispatchStore::new(state_dir),
            state_path: state_dir.join("supervisor-scheduler.json"),
            apply_fail_at: Cell::new(None),
            apply_calls: Cell::new(0),
            #[cfg(test)]
            dispatch_registry_reads: Cell::new(0),
        }
    }

    #[cfg(test)]
    fn fail_apply_at(&self, call: usize) {
        self.apply_fail_at.set(Some(call));
    }

    /// Starts one durable run. The operation key is reserved before aggregate
    /// initialization, so retrying after a disconnect reuses the same run ID.
    ///
    /// # Errors
    /// Returns an error for conflicting idempotency, invalid DAGs, or durable IO failure.
    ///
    #[allow(clippy::too_many_lines)]
    pub fn start(
        &self,
        caller: &str,
        operation_id: &str,
        root_task: String,
        initial_tasks: Vec<InitialTask>,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.start_scoped(SupervisorStartRequest {
            caller,
            workspace: None,
            operation_id,
            root_task,
            root_artifact_contract: NO_ARTIFACT_CONTRACT,
            artifact_repository: None,
            worker_profile_id: None,
            worker_semantic_digest: None,
            caller_dispatch: None,
            initial_tasks,
            policy_selector,
            now,
        })
    }

    /// Starts a run owned by one daemon-admitted workspace. This is the
    /// production entry point; the unscoped wrapper remains for legacy callers
    /// and deterministic domain fixtures.
    ///
    /// # Errors
    ///
    /// Returns an error when admission input is invalid or the durable run
    /// cannot be initialized.
    #[allow(clippy::too_many_arguments)]
    pub fn start_for_workspace(
        &self,
        caller: &str,
        workspace: WorkspaceId,
        operation_id: &str,
        root_task: String,
        initial_tasks: Vec<InitialTask>,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.start_scoped(SupervisorStartRequest {
            caller,
            workspace: Some(workspace),
            operation_id,
            root_task,
            root_artifact_contract: NO_ARTIFACT_CONTRACT,
            artifact_repository: None,
            worker_profile_id: None,
            worker_semantic_digest: None,
            caller_dispatch: None,
            initial_tasks,
            policy_selector,
            now,
        })
    }

    /// Reserves a generic Supervisor root together with the authenticated
    /// dispatch which must own it. Persisting this join before aggregate
    /// creation makes a crash between start and provenance binding recoverable
    /// and prevents the same dispatch from starting another retained run.
    ///
    /// # Errors
    /// Returns an error when the caller dispatch, Agent, runtime scope, or
    /// durable start reservation conflicts.
    #[allow(clippy::too_many_arguments)]
    pub fn start_for_workspace_caller_dispatch(
        &self,
        caller: &str,
        workspace: WorkspaceId,
        operation_id: &str,
        root_task: String,
        policy_selector: Option<String>,
        dispatch_run_id: OperationId,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.ensure_supervisor_start_dispatch_available(operation_id, dispatch_run_id)?;
        let dispatch = self
            .dispatch
            .run(dispatch_run_id)?
            .context("supervisor caller dispatch does not exist")?;
        if dispatch.status != RunStatus::Running {
            anyhow::bail!("supervisor caller dispatch has closed supervisor ownership");
        }
        let agent = self
            .dispatch
            .agent_in_workspace(workspace, dispatch.agent_id)?
            .context("supervisor caller Agent does not exist")?;
        let binding = self
            .dispatch
            .binding(dispatch_run_id)?
            .context("supervisor caller binding does not exist")?;
        if agent.session_id != worker.session_id
            || binding.worker.agent_id != dispatch.agent_id
            || binding.worker.session_id != worker.session_id
            || worker.terminal.workspace_id != workspace
            || worker.terminal.session_id != worker.session_id
        {
            anyhow::bail!("supervisor caller worker is outside its authenticated scope");
        }
        let admission = self
            .dispatch
            .admission(dispatch_run_id)?
            .context("supervisor caller admission does not exist")?;
        let caller_dispatch = CallerDispatchReservation {
            dispatch_run_id,
            worker_session_id: worker.session_id,
            worker_agent_id: dispatch.agent_id,
            worker_runtime_id: worker.agent_runtime_id,
        };
        self.start_scoped(SupervisorStartRequest {
            caller,
            workspace: Some(workspace),
            operation_id,
            root_task,
            root_artifact_contract: NO_ARTIFACT_CONTRACT,
            artifact_repository: None,
            worker_profile_id: Some(agent.runtime),
            worker_semantic_digest: Some(usagi_core::infrastructure::ipc::agent_operation_digest(
                &admission.semantic_key,
            )),
            caller_dispatch: Some(&caller_dispatch),
            initial_tasks: Vec::new(),
            policy_selector,
            now,
        })
    }

    /// Starts a Goal run and binds its root task to an already admitted
    /// workspace-root Agent dispatch. Production reserves before spawn and
    /// calls [`Self::bind_reserved_workspace_root_dispatch`] afterwards; this
    /// composed entry point remains useful to exact-retry callers and tests.
    ///
    /// Retrying after any durable partial write is safe: `start_for_workspace`
    /// reuses the run and an exact existing root provenance is returned as-is.
    ///
    /// # Errors
    ///
    /// Returns an error when the worker is not rooted in the requested
    /// workspace, the dispatch identity is absent, or durable state cannot be
    /// initialized and bound consistently.
    #[allow(clippy::too_many_arguments)]
    pub fn start_for_workspace_root_dispatch(
        &self,
        caller: &str,
        workspace: WorkspaceId,
        operation_id: &str,
        goal: GoalSpecification,
        policy_selector: Option<String>,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.reserve_goal_for_workspace(
            caller,
            workspace,
            operation_id,
            goal,
            policy_selector,
            now,
        )?;
        self.bind_reserved_workspace_root_dispatch(operation_id, worker, now)
    }

    fn load_started_run(&self, id: SupervisorRunId) -> Result<SupervisorRun> {
        self.supervisor
            .load(id)?
            .ok_or_else(|| anyhow::anyhow!("supervisor run disappeared during root binding"))
    }

    fn load_indexed_runs(
        &self,
        ids: impl IntoIterator<Item = SupervisorRunId>,
    ) -> Result<Vec<SupervisorRun>> {
        let mut runs = Vec::new();
        for id in ids {
            runs.push(self.load_indexed_run(id)?);
        }
        Ok(runs)
    }

    fn load_indexed_run(&self, id: SupervisorRunId) -> Result<SupervisorRun> {
        self.supervisor
            .load(id)?
            .ok_or_else(|| anyhow::anyhow!("indexed supervisor run disappeared"))
    }

    fn unfinished_runs(&self) -> Result<Vec<SupervisorRun>> {
        self.load_indexed_runs(self.supervisor.unfinished_run_ids()?)
    }

    fn aborted_runs(&self) -> Result<Vec<SupervisorRun>> {
        self.load_indexed_runs(self.supervisor.aborted_run_ids()?)
    }

    /// Resolves a supervised parent from either committed provenance or the
    /// durable promotion reservation which necessarily precedes its Agent
    /// spawn. The reservation cases close the interval in which a freshly
    /// started root or delegated Agent can claim MCP before its exact runtime
    /// fence has been bound to the Supervisor aggregate.
    #[allow(clippy::too_many_lines)] // Classification keeps live, pending, and retained ownership in one fail-closed decision.
    fn supervised_parent(
        &self,
        parent_dispatch_run: OperationId,
    ) -> Result<Option<(SupervisorRun, TaskId)>> {
        let state = self.load_state()?;
        let pending_delegated = delegated_task_id(parent_dispatch_run)?;
        let mut matches = Vec::new();

        for run in self.unfinished_runs()? {
            let mut task_ids = BTreeSet::new();
            for (task_id, provenance) in run
                .provenance
                .iter()
                .filter(|(_, provenance)| provenance.dispatch_run_id == parent_dispatch_run)
            {
                let task = run
                    .tasks
                    .get(task_id)
                    .context("supervised dispatch provenance task is missing")?;
                if !matches!(task.state, TaskState::Dispatched | TaskState::Running) {
                    anyhow::bail!("parent dispatch has closed supervisor ownership");
                }
                let dispatch = self
                    .dispatch
                    .run(parent_dispatch_run)?
                    .context("supervised parent dispatch is missing")?;
                if !matches!(dispatch.status, RunStatus::Preparing | RunStatus::Running) {
                    anyhow::bail!("parent dispatch has closed supervisor ownership");
                }
                validate_provenance_chain(&run, task_id, provenance)?;
                task_ids.insert(task_id.clone());
            }

            let root_reservations = state
                .starts
                .iter()
                .filter(|(operation, reservation)| {
                    reservation.supervisor_run_id == run.supervisor_run_id
                        && (reservation.caller_dispatch_run_id == Some(parent_dispatch_run)
                            || (reservation.caller_dispatch_run_id.is_none()
                                && operation.as_str() == parent_dispatch_run.to_string()
                                && run.artifact_repository.is_some()))
                })
                .collect::<Vec<_>>();
            if root_reservations.len() > 1 {
                anyhow::bail!("supervisor root has multiple promotion reservations");
            }
            if let Some((_, reservation)) = root_reservations.first().copied() {
                let root = TaskId::new("root")?;
                if !run.provenance.contains_key(&root) {
                    let workspace_id = run
                        .workspace_id
                        .context("supervisor root promotion has no workspace authority")?;
                    self.ensure_pending_operation_matches_reservation(
                        parent_dispatch_run,
                        workspace_id,
                        reservation.worker_session_id.is_some(),
                        reservation.worker_session_id,
                        reservation.worker_profile_id.as_ref(),
                        reservation.worker_agent_id,
                        reservation.worker_semantic_digest.as_deref(),
                    )?;
                    live_task_dispatch_authority(&state, &run, &root, &mut BTreeSet::new())?
                        .context("supervisor root promotion reservation has no authority")?;
                    task_ids.insert(root);
                }
            }

            if let Some(task) = run.tasks.get(&pending_delegated)
                && !run.provenance.contains_key(&pending_delegated)
                && is_delegated_reservation(task, parent_dispatch_run)
            {
                let workspace_id = run
                    .workspace_id
                    .context("delegated parent promotion has no workspace authority")?;
                self.ensure_pending_operation_matches_reservation(
                    parent_dispatch_run,
                    workspace_id,
                    true,
                    task.promotion_worker_session_id,
                    task.promotion_worker_profile_id.as_ref(),
                    task.promotion_worker_agent_id,
                    task.promotion_worker_semantic_digest.as_deref(),
                )?;
                live_task_dispatch_authority(
                    &state,
                    &run,
                    &pending_delegated,
                    &mut BTreeSet::new(),
                )?
                .context("delegated parent promotion reservation has no authority")?;
                task_ids.insert(pending_delegated.clone());
            }

            matches.extend(task_ids.into_iter().map(|task_id| (run.clone(), task_id)));
        }

        let mut matches = matches.into_iter();
        let Some(found) = matches.next() else {
            if state
                .expired_starts
                .contains(&parent_dispatch_run.to_string())
                || !self
                    .retained_dispatch_owners(&state, parent_dispatch_run)?
                    .is_empty()
            {
                anyhow::bail!("parent dispatch has stale supervisor ownership");
            }
            return Ok(None);
        };
        if matches.next().is_some() {
            anyhow::bail!("parent dispatch belongs to multiple supervisor runs");
        }
        let retained = self.retained_dispatch_owners(&state, parent_dispatch_run)?;
        let live_owner = (found.0.supervisor_run_id, found.1.clone());
        if state
            .expired_starts
            .contains(&parent_dispatch_run.to_string())
            || !retained.contains(&live_owner)
            || retained.iter().any(|owner| owner != &live_owner)
        {
            anyhow::bail!("parent dispatch has conflicting retained supervisor ownership");
        }
        Ok(Some(found))
    }

    fn ensure_new_delegated_operation_is_unused(
        &self,
        child_dispatch_run: OperationId,
        task_id: &TaskId,
        allow_existing_agent_operation: bool,
    ) -> Result<()> {
        let state = self.load_state()?;
        if (!allow_existing_agent_operation
            && (self.dispatch.run(child_dispatch_run)?.is_some()
                || self.dispatch.admission(child_dispatch_run)?.is_some()))
            || state.starts.contains_key(&child_dispatch_run.to_string())
            || state
                .expired_starts
                .contains(&child_dispatch_run.to_string())
            || !self
                .retained_dispatch_owners(&state, child_dispatch_run)?
                .is_empty()
        {
            anyhow::bail!("delegated dispatch operation is already in use");
        }
        if self
            .supervisor
            .runs()?
            .iter()
            .any(|run| run.tasks.contains_key(task_id))
        {
            anyhow::bail!("delegated dispatch operation already owns a supervisor task");
        }
        Ok(())
    }

    fn dispatch_profile(&self, operation: OperationId) -> Result<AgentProfileId> {
        let dispatch = self
            .dispatch
            .run(operation)?
            .context("dispatch operation is unavailable")?;
        self.dispatch
            .agent(dispatch.agent_id)?
            .map(|agent| agent.runtime)
            .context("dispatch Agent is unavailable")
    }

    /// Whether a live Director Work run owns this exact dispatch through
    /// committed provenance or its preceding promotion reservation.
    ///
    /// This read-only query lets the composition layer apply Work Run-only
    /// policy before session creation without changing classic dispatch.
    ///
    /// # Errors
    /// Returns an error when the durable Supervisor inventory is unavailable.
    pub fn supervises_dispatch(&self, dispatch_run: OperationId) -> Result<bool> {
        Ok(self.supervision_fence(dispatch_run)?.is_some())
    }

    /// Returns the exact live Supervisor task generation which owns a dispatch.
    ///
    /// # Errors
    /// Returns an error when durable ownership is missing or internally stale.
    pub fn supervision_fence(
        &self,
        dispatch_run: OperationId,
    ) -> Result<Option<DispatchSupervisionFence>> {
        let Some((run, task_id)) = self.supervised_parent(dispatch_run)? else {
            return Ok(None);
        };
        let generation = run
            .tasks
            .get(&task_id)
            .context("supervised dispatch task is missing")?
            .generation;
        Ok(Some(DispatchSupervisionFence {
            supervisor_run_id: run.supervisor_run_id,
            task_id,
            generation,
        }))
    }

    /// Refuses to attach one authenticated dispatch to multiple unfinished
    /// Supervisor roots while permitting an exact start-operation replay.
    ///
    /// # Errors
    /// Returns an error when the dispatch has another live Supervisor owner or
    /// its current ownership fence is stale.
    pub fn ensure_supervisor_start_dispatch_available(
        &self,
        start_operation_id: &str,
        dispatch_run: OperationId,
    ) -> Result<()> {
        let state = self.load_state()?;
        let target = state
            .starts
            .get(start_operation_id)
            .map(|reservation| reservation.supervisor_run_id);
        if state.expired_starts.contains(&dispatch_run.to_string()) {
            anyhow::bail!("dispatch already belongs to another retained supervisor run");
        }
        let owners = self.retained_dispatch_owners(&state, dispatch_run)?;
        if owners.is_empty() {
            return Ok(());
        }
        if owners.len() == 1 && target == Some(owners[0].0) && owners[0].1.0 == "root" {
            let Some(run) = self.supervisor.load(owners[0].0)? else {
                // The start reservation is written before aggregate
                // initialization. Its exact operation retry owns the right to
                // recreate that same reserved run ID.
                return Ok(());
            };
            if let Some(provenance) = run.provenance.get(&owners[0].1)
                && provenance.dispatch_run_id == dispatch_run
            {
                validate_provenance_chain(&run, &owners[0].1, provenance)?;
            }
            return Ok(());
        }
        anyhow::bail!("dispatch already belongs to another retained supervisor run")
    }

    /// Adds and binds a child dispatch beneath the exact supervised parent
    /// dispatch. A caller outside a Supervisor Run is a no-op, preserving
    /// classic delegation semantics.
    ///
    /// # Errors
    /// Returns an error for conflicting replay, cross-workspace provenance, or
    /// durable reducer failure.
    pub fn attach_delegated_dispatch(
        &self,
        parent_dispatch_run: OperationId,
        child_operation_id: &str,
        instruction: String,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<Option<SupervisorRunQuery>> {
        let worker_session_id = worker
            .session_id
            .ok_or_else(|| anyhow::anyhow!("delegated worker has no managed session"))?;
        if self
            .reserve_delegated_dispatch_inner(
                parent_dispatch_run,
                child_operation_id,
                instruction,
                Some(worker_session_id),
                None,
                None,
                true,
                false,
                now,
            )?
            .is_none()
        {
            return Ok(None);
        }
        self.bind_reserved_delegated_dispatch(child_operation_id, worker, now)
    }

    #[allow(clippy::too_many_lines)]
    fn start_scoped(&self, request: SupervisorStartRequest<'_>) -> Result<SupervisorRunQuery> {
        let SupervisorStartRequest {
            caller,
            workspace,
            operation_id,
            root_task,
            root_artifact_contract,
            artifact_repository,
            worker_profile_id,
            worker_semantic_digest,
            caller_dispatch,
            initial_tasks,
            policy_selector,
            now,
        } = request;
        validate_start_input(
            operation_id,
            &root_task,
            &initial_tasks,
            policy_selector.as_deref(),
        )?;
        let mut start_semantics = Sha256::new();
        update_semantic_component(&mut start_semantics, caller);
        update_semantic_component(&mut start_semantics, &root_task);
        update_semantic_component(&mut start_semantics, root_artifact_contract.as_str());
        update_semantic_component(
            &mut start_semantics,
            artifact_repository
                .as_ref()
                .map_or("none", GitHubRepository::as_str),
        );
        update_semantic_component(&mut start_semantics, &initial_tasks.len().to_string());
        for task in &initial_tasks {
            update_semantic_component(&mut start_semantics, &task.task_id);
            update_semantic_component(
                &mut start_semantics,
                task.parent_task_id.as_deref().unwrap_or("root"),
            );
            update_semantic_component(&mut start_semantics, &task.dependencies.len().to_string());
            for dependency in &task.dependencies {
                update_semantic_component(&mut start_semantics, dependency);
            }
            update_semantic_component(&mut start_semantics, &task.instruction);
            update_semantic_component(
                &mut start_semantics,
                task.required_artifact_contract.as_str(),
            );
        }
        update_semantic_component(
            &mut start_semantics,
            policy_selector.as_deref().unwrap_or("default"),
        );
        let semantic_key = encode_digest(start_semantics.finalize());
        let mut state = self.load_state()?;
        let reservation = match state.starts.get(operation_id) {
            Some(existing) if existing.semantic_key == semantic_key => {
                if existing.workspace_id.is_some() && existing.workspace_id != workspace {
                    anyhow::bail!("operation id was reused from a different workspace");
                }
                let adopt_workspace = existing.workspace_id.is_none() && workspace.is_some();
                let adopt_repository =
                    existing.artifact_repository.is_none() && artifact_repository.is_some();
                if adopt_workspace || adopt_repository {
                    if let Some(run) = self.supervisor.load(existing.supervisor_run_id)? {
                        if run.workspace_id != workspace {
                            anyhow::bail!("operation id was reused from a different workspace");
                        }
                        if adopt_repository && run.artifact_repository != artifact_repository {
                            anyhow::bail!(
                                "operation id was reused with a different artifact repository"
                            );
                        }
                    } else if caller_dispatch.is_some()
                        || root_artifact_contract != GOAL_REVIEW_READY_ARTIFACT_CONTRACT
                        || artifact_repository.is_none()
                    {
                        anyhow::bail!("legacy supervisor start has no durable workspace authority");
                    }
                }
                if existing.caller_dispatch_run_id.is_some()
                    && (existing.caller_dispatch_run_id
                        != caller_dispatch.map(|item| item.dispatch_run_id)
                        || existing.worker_session_id
                            != caller_dispatch.and_then(|item| item.worker_session_id)
                        || existing.worker_agent_id
                            != caller_dispatch.map(|item| item.worker_agent_id)
                        || existing.worker_runtime_id
                            != caller_dispatch.map(|item| item.worker_runtime_id))
                {
                    anyhow::bail!("operation id was reused with a different caller dispatch");
                }
                if existing
                    .artifact_repository
                    .as_ref()
                    .zip(artifact_repository.as_ref())
                    .is_some_and(|(existing, requested)| existing != requested)
                {
                    anyhow::bail!("operation id was reused with a different artifact repository");
                }
                if existing
                    .worker_profile_id
                    .as_ref()
                    .zip(worker_profile_id.as_ref())
                    .is_some_and(|(existing, requested)| existing != requested)
                {
                    anyhow::bail!("operation id was reused with a different Agent runtime");
                }
                if existing
                    .worker_semantic_digest
                    .as_ref()
                    .zip(worker_semantic_digest.as_ref())
                    .is_some_and(|(existing, requested)| existing != requested)
                {
                    anyhow::bail!("operation id was reused with a different Agent intent");
                }
                let mut existing = existing.clone();
                if adopt_workspace
                    || adopt_repository
                    || (existing.caller_dispatch_run_id.is_none() && caller_dispatch.is_some())
                    || (existing.worker_profile_id.is_none() && worker_profile_id.is_some())
                    || (existing.worker_semantic_digest.is_none()
                        && worker_semantic_digest.is_some())
                {
                    existing.workspace_id = workspace;
                    existing
                        .artifact_repository
                        .clone_from(&artifact_repository);
                    if let Some(caller_dispatch) = caller_dispatch {
                        existing.caller_dispatch_run_id = Some(caller_dispatch.dispatch_run_id);
                        existing.worker_session_id = caller_dispatch.worker_session_id;
                        existing.worker_agent_id = Some(caller_dispatch.worker_agent_id);
                        existing.worker_runtime_id = Some(caller_dispatch.worker_runtime_id);
                    }
                    existing.worker_profile_id.clone_from(&worker_profile_id);
                    existing
                        .worker_semantic_digest
                        .clone_from(&worker_semantic_digest);
                    state
                        .starts
                        .insert(operation_id.to_owned(), existing.clone());
                    self.save_state(&state)?;
                }
                existing
            }
            Some(_) => anyhow::bail!("operation id was reused with a different supervisor start"),
            None => {
                if state.expired_starts.contains(operation_id) {
                    anyhow::bail!("supervisor start idempotency window expired");
                }
                self.ensure_start_capacity(&mut state)?;
                let reservation = StartReservation {
                    semantic_key,
                    supervisor_run_id: SupervisorRunId::new(),
                    artifact_repository: artifact_repository.clone(),
                    workspace_id: workspace,
                    caller_dispatch_run_id: caller_dispatch.map(|item| item.dispatch_run_id),
                    worker_session_id: caller_dispatch.and_then(|item| item.worker_session_id),
                    worker_agent_id: caller_dispatch.map(|item| item.worker_agent_id),
                    worker_runtime_id: caller_dispatch.map(|item| item.worker_runtime_id),
                    worker_profile_id,
                    worker_semantic_digest,
                };
                state
                    .starts
                    .insert(operation_id.to_owned(), reservation.clone());
                self.save_state(&state)?;
                reservation
            }
        };
        let policy_revision = policy_selector.unwrap_or_else(|| "default".into());
        let mut run = if let Some(existing) = self.supervisor.load(reservation.supervisor_run_id)? {
            if existing.root_caller_ref != caller
                || existing.workspace_id != workspace
                || existing.artifact_repository != artifact_repository
                || existing.policy_revision != policy_revision
            {
                anyhow::bail!("supervisor start reservation does not match its durable run");
            }
            if existing.state != SupervisorRunState::Planning {
                return Ok(existing.query());
            }
            existing
        } else {
            let mut run = SupervisorRun::new_with_id(
                reservation.supervisor_run_id,
                caller.to_owned(),
                operation_id.to_owned(),
                operation_id.to_owned(),
                policy_revision,
                now,
            );
            run.workspace_id = workspace;
            run.artifact_repository = artifact_repository;
            run.display_label = work_run_display_label(&root_task);
            self.supervisor.initialize(&run)?;
            run
        };
        let root_id = TaskId::new("root")?;
        if let Some(root) = run.tasks.get(&root_id) {
            if root.parent_task_id.is_some()
                || !root.dependencies.is_empty()
                || root.instruction_body != root_task
                || root.required_artifact_contract != root_artifact_contract
            {
                anyhow::bail!("supervisor root task conflicts with its start reservation");
            }
        } else {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::Admission,
                SupervisorEventKind::AddTask {
                    task: {
                        let mut task = task_node(
                            &run,
                            root_id,
                            None,
                            BTreeSet::new(),
                            root_task,
                            root_artifact_contract,
                        );
                        if root_artifact_contract == GOAL_REVIEW_READY_ARTIFACT_CONTRACT
                            || reservation.caller_dispatch_run_id.is_some()
                        {
                            task.promotion_reserved_at = Some(now);
                        }
                        task
                    },
                },
            )?;
        }
        let mut pending = initial_tasks;
        while !pending.is_empty() {
            let before = pending.len();
            let mut remaining = Vec::new();
            for task in pending {
                let dependencies = task
                    .dependencies
                    .iter()
                    .map(|value| TaskId::new(value.clone()))
                    .collect::<Result<BTreeSet<_>, _>>()?;
                let parent =
                    TaskId::new(task.parent_task_id.clone().unwrap_or_else(|| "root".into()))?;
                let task_id = TaskId::new(task.task_id.clone())?;
                if let Some(existing) = run.tasks.get(&task_id) {
                    if existing.parent_task_id.as_ref() != Some(&parent)
                        || existing.dependencies != dependencies
                        || existing.instruction_body != task.instruction
                        || existing.required_artifact_contract != task.required_artifact_contract
                    {
                        anyhow::bail!(
                            "supervisor initial task conflicts with its start reservation"
                        );
                    }
                } else if dependencies.iter().all(|id| run.tasks.contains_key(id))
                    && run.tasks.contains_key(&parent)
                {
                    run = self.apply(
                        &run,
                        now,
                        SupervisorEventSource::Admission,
                        SupervisorEventKind::AddTask {
                            task: task_node(
                                &run,
                                task_id,
                                Some(parent),
                                dependencies,
                                task.instruction,
                                task.required_artifact_contract,
                            ),
                        },
                    )?;
                } else {
                    remaining.push(task);
                }
            }
            if remaining.len() == before {
                anyhow::bail!("initial task DAG has a missing dependency or cycle");
            }
            pending = remaining;
        }
        run = self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::SetRunState {
                state: SupervisorRunState::Running,
                terminal_reason: None,
            },
        )?;
        Ok(run.query())
    }

    /// Lists the bounded retained runs that explicitly belong to one
    /// workspace. Legacy unscoped snapshots are excluded fail-closed.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable supervisor index or a selected run
    /// snapshot cannot be read consistently.
    pub fn list_workspace(&self, workspace: WorkspaceId) -> Result<Vec<SupervisorRunQuery>> {
        self.supervisor
            .workspace_runs(workspace, MAX_SUPERVISOR_WORKSPACE_SNAPSHOT_RUNS)
    }

    /// Reads one run only when its durable workspace fence matches.
    ///
    /// # Errors
    /// Returns an error when durable state cannot be read consistently.
    pub fn get_for_workspace(
        &self,
        workspace: WorkspaceId,
        id: SupervisorRunId,
    ) -> Result<Option<SupervisorRunQuery>> {
        let Some(run) = self.supervisor.load(id)? else {
            return Ok(None);
        };
        if run.workspace_id != Some(workspace) {
            return Ok(None);
        }
        Ok(Some(run.query()))
    }

    /// Resolves artifact rework to the exact blocking Agent before the control
    /// event clears its escalation fence. Other escalation kinds return none
    /// and can resume without an Agent prompt.
    ///
    /// # Errors
    /// Returns an error when required artifact provenance is stale, missing,
    /// corrupt, or cannot be read.
    pub fn retry_work_for_workspace(
        &self,
        workspace: WorkspaceId,
        id: SupervisorRunId,
        escalation_id: OperationId,
    ) -> Result<Option<RetryWork>> {
        let Some(run) = self.supervisor.load(id)? else {
            return Ok(None);
        };
        if run.workspace_id != Some(workspace) || run.state != SupervisorRunState::Escalated {
            return Ok(None);
        }
        let escalation = run
            .escalation
            .as_ref()
            .filter(|item| item.escalation_id == escalation_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor retry escalation fence is stale"))?;
        let Some(task_id) = escalation.blocking_task_id.as_ref() else {
            return Ok(None);
        };
        let task = run
            .tasks
            .get(task_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor retry task is missing"))?;
        if task.state != TaskState::Verifying
            || task.required_artifact_contract != GOAL_REVIEW_READY_ARTIFACT_CONTRACT
        {
            return Ok(None);
        }
        let provenance = run
            .provenance
            .get(task_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor retry provenance is missing"))?;
        if provenance.generation != task.generation
            || task.assigned_dispatch_run != Some(provenance.dispatch_run_id)
        {
            anyhow::bail!("supervisor retry provenance fence is stale");
        }
        Ok(Some(RetryWork {
            provenance: provenance.clone(),
            reason: escalation.reason.clone(),
            safe_evidence: escalation.safe_evidence.clone(),
        }))
    }

    /// Applies one human command to a run owned by the connection workspace.
    ///
    /// The operation reservation is saved before the aggregate event. A retry
    /// after any storage or connection failure therefore either commits that
    /// exact semantic command or replays its already-committed event; reusing
    /// the operation identity for another command is rejected globally.
    ///
    /// # Errors
    /// Returns an error for a foreign run, invalid transition, conflicting or
    /// expired operation identity, capacity exhaustion, or durable IO failure.
    pub fn control_for_workspace(
        &self,
        workspace: WorkspaceId,
        operation_id: OperationId,
        command: &SupervisorWorkspaceCommand,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        if matches!(command, SupervisorWorkspaceCommand::Delete { .. }) {
            anyhow::bail!("supervisor history deletion uses the delete control path");
        }
        let supervisor_run_id = command.supervisor_run_id();
        let run = self
            .supervisor
            .load(supervisor_run_id)?
            .filter(|run| run.workspace_id == Some(workspace))
            .ok_or_else(|| anyhow::anyhow!("supervisor run does not belong to this workspace"))?;
        validate_control_command(command)?;
        let semantic_digest = control_semantic_digest(command)?;
        let operation_key = operation_id.to_string();
        let mut state = self.load_state()?;
        if state.expired_controls.contains(&operation_key) {
            anyhow::bail!("supervisor control operation is outside the retained replay window");
        }
        if let Some(existing) = state.controls.get(&operation_key) {
            if existing.supervisor_run_id != supervisor_run_id
                || existing.semantic_digest != semantic_digest
            {
                anyhow::bail!("supervisor control operation conflicts with its reservation");
            }
        } else {
            let event = control_event(&run, operation_id, semantic_digest.clone(), command, now);
            // Prove the domain transition before reserving the operation. Once
            // this check passes, every later failure is storage-only and the
            // durable reservation can be retried without changing semantics.
            let mut candidate = run.clone();
            reduce(&mut candidate, &event).map_err(anyhow::Error::msg)?;
            self.ensure_control_capacity(&mut state)?;
            state.controls.insert(
                operation_key,
                ControlReservation {
                    semantic_digest: semantic_digest.clone(),
                    supervisor_run_id,
                    reserved_at: now,
                },
            );
            self.save_state(&state)?;
        }

        let event = control_event(&run, operation_id, semantic_digest, command, now);
        if matches!(
            run.event_id_status(operation_id),
            usagi_core::domain::supervisor::AppliedEventStatus::Fresh
        ) {
            let mut candidate = run.clone();
            reduce(&mut candidate, &event).map_err(anyhow::Error::msg)?;
        }
        self.apply_event(&run, &event).map(|run| run.query())
    }

    /// Deletes one terminal run owned by the connection workspace.
    ///
    /// The operation reservation is durable before files are removed. A retry
    /// after the snapshot disappears therefore returns the same receipt, while
    /// a first request for an absent run is refused. The observed revision is
    /// part of the command digest and is rechecked under the store lock.
    ///
    /// # Errors
    ///
    /// Returns an error for a foreign, active, stale, conflicting, expired, or
    /// durably unreadable run.
    pub fn delete_for_workspace(
        &self,
        workspace: WorkspaceId,
        operation_id: OperationId,
        command: &SupervisorWorkspaceCommand,
        now: DateTime<Utc>,
    ) -> Result<usagi_core::domain::supervisor::SupervisorRunDeletion> {
        let SupervisorWorkspaceCommand::Delete {
            supervisor_run_id,
            observed_state_revision,
        } = command
        else {
            anyhow::bail!("supervisor delete command is required");
        };
        let semantic_digest = control_semantic_digest(command)?;
        let operation_key = operation_id.to_string();
        let mut state = self.load_state()?;
        if state.expired_controls.contains(&operation_key) {
            anyhow::bail!("supervisor control operation is outside the retained replay window");
        }
        let replay = if let Some(existing) = state.controls.get(&operation_key) {
            if existing.supervisor_run_id != *supervisor_run_id
                || existing.semantic_digest != semantic_digest
            {
                anyhow::bail!("supervisor control operation conflicts with its reservation");
            }
            true
        } else {
            false
        };

        let run = self.supervisor.load(*supervisor_run_id)?;
        if let Some(run) = &run {
            if run.workspace_id != Some(workspace) {
                anyhow::bail!("supervisor run does not belong to this workspace");
            }
            if !run.state.is_finished() {
                anyhow::bail!("supervisor run must finish before deletion");
            }
            if run.state_revision != *observed_state_revision {
                anyhow::bail!(
                    "stale supervisor state revision: expected {observed_state_revision}, got {}",
                    run.state_revision
                );
            }
        } else if !replay {
            anyhow::bail!("supervisor run does not exist");
        }

        if !replay {
            self.ensure_control_capacity(&mut state)?;
            state.controls.insert(
                operation_key,
                ControlReservation {
                    semantic_digest,
                    supervisor_run_id: *supervisor_run_id,
                    reserved_at: now,
                },
            );
            self.save_state(&state)?;
        }
        if run.is_some() {
            self.supervisor
                .delete_finished(*supervisor_run_id, *observed_state_revision)?;
        }
        let mut start_keys = Vec::new();
        for (key, reservation) in &state.starts {
            if reservation.supervisor_run_id == *supervisor_run_id {
                start_keys.push(key.clone());
            }
        }
        if !start_keys.is_empty() {
            for key in start_keys {
                let caller_dispatch = state.starts[&key].caller_dispatch_run_id;
                let _ = state.starts.remove(&key);
                state.expired_starts.insert(&key);
                if let Some(caller_dispatch) = caller_dispatch {
                    state.expired_starts.insert(&caller_dispatch.to_string());
                }
            }
            self.save_state(&state)?;
        }
        Ok(usagi_core::domain::supervisor::SupervisorRunDeletion {
            supervisor_run_id: *supervisor_run_id,
            state_revision: *observed_state_revision,
        })
    }

    /// Reports whether a workspace still owns a non-terminal supervised run.
    /// Legacy unscoped snapshots cannot be attributed and are excluded.
    ///
    /// # Errors
    /// Returns an error when the durable supervisor index cannot be read.
    pub fn has_unfinished_workspace(&self, workspace: WorkspaceId) -> Result<bool> {
        self.supervisor.has_unfinished_workspace(workspace)
    }

    /// Reads one caller-owned durable run.
    ///
    /// # Errors
    /// Returns an error when durable state cannot be read.
    pub fn get(&self, caller: &str, id: SupervisorRunId) -> Result<Option<SupervisorRunQuery>> {
        match self.owned_run(caller, id)? {
            Some(run) => Ok(Some(run.query())),
            None => Ok(None),
        }
    }

    /// Lists caller-owned durable runs.
    ///
    /// # Errors
    /// Returns an error when durable state cannot be listed or replayed.
    pub fn list(
        &self,
        caller: &str,
        state: Option<SupervisorRunState>,
    ) -> Result<Vec<SupervisorRunQuery>> {
        Ok(self
            .supervisor
            .runs()?
            .into_iter()
            .filter(|run| {
                run.root_caller_ref == caller && state.is_none_or(|value| run.state == value)
            })
            .map(|run| run.query())
            .collect())
    }

    /// Lists one bounded caller-owned page using the durable run index.
    ///
    /// # Errors
    /// Returns an error when the cursor, durable state, or response budget is invalid.
    pub fn list_page(
        &self,
        caller: &str,
        state: Option<SupervisorRunState>,
        cursor: usize,
        limit: usize,
    ) -> Result<SupervisorRunPage> {
        self.supervisor.runs_page(caller, state, cursor, limit)
    }

    /// Commits a fenced cancellation.
    ///
    /// # Errors
    /// Returns an error for an unknown owner, invalid transition, or durable IO failure.
    pub fn cancel(
        &self,
        caller: &str,
        id: SupervisorRunId,
        reason: String,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        bounded_safe_label(
            "supervisor cancellation reason",
            &reason,
            MAX_SUPERVISOR_REASON_BYTES,
        )?;
        let run = self
            .owned_run(caller, id)?
            .ok_or_else(|| anyhow::anyhow!("supervisor run does not exist for this caller"))?;
        self.apply(
            &run,
            now,
            SupervisorEventSource::Cancel,
            SupervisorEventKind::Cancel {
                task_id: None,
                reason,
            },
        )
        .map(|run| run.query())
    }

    /// Commits an authorized escalation decision.
    ///
    /// # Errors
    /// Returns an error for an invalid owner/fence/transition or durable IO failure.
    pub fn resolve_escalation(
        &self,
        caller: &str,
        id: SupervisorRunId,
        escalation_id: OperationId,
        decision: EscalationDecision,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        let run = self
            .owned_run(caller, id)?
            .ok_or_else(|| anyhow::anyhow!("supervisor run does not exist for this caller"))?;
        self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::ResolveEscalation {
                escalation_id,
                decision,
            },
        )
        .map(|run| run.query())
    }

    /// Returns redaction-safe event metadata for one caller-owned run.
    ///
    /// # Errors
    /// Returns an error for an unknown owner or durable IO failure.
    pub fn events(
        &self,
        caller: &str,
        id: SupervisorRunId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<(Vec<EventQuery>, EventCursor)> {
        self.owned_run(caller, id)?
            .ok_or_else(|| anyhow::anyhow!("supervisor run does not exist for this caller"))?;
        self.supervisor.events(
            id,
            EventCursor {
                next_sequence: after_sequence.saturating_add(1),
            },
            limit,
        )
    }

    /// Reconciles every durable run after startup or a completion wake.
    ///
    /// # Errors
    /// Returns the first durable reconciliation or wake delivery failure.
    pub fn tick_all(&self, now: DateTime<Utc>, waker: &mut dyn DecisionWaker) -> Result<()> {
        let dispatch_runs = self.dispatch_runs()?;
        let mut first_failure = None;
        for id in self.supervisor.unfinished_run_ids()? {
            if let Err(error) = self.tick_run(id, now, &dispatch_runs) {
                first_failure.get_or_insert(error);
            }
        }
        if let Err(error) = self.deliver_reserved(now, waker) {
            first_failure.get_or_insert(error);
        }
        first_failure.map_or(Ok(()), Err)
    }

    fn owned_run(&self, caller: &str, id: SupervisorRunId) -> Result<Option<SupervisorRun>> {
        Ok(self
            .supervisor
            .load(id)?
            .filter(|run| run.root_caller_ref == caller))
    }

    /// Reconciles one run and delivers each durably reserved wake at least once.
    /// A repeat/restart is safe because reducer event IDs and wake reservation
    /// keys are stable (`child dispatch run` + `parent decision generation`).
    ///
    /// # Errors
    ///
    /// Returns an error when durable state cannot be read or committed, or the
    /// waker cannot deliver a reserved request.
    ///
    /// # Panics
    ///
    /// Panics only if an already-corrupt supervisor snapshot contains
    /// provenance for a missing task or parent.
    pub fn tick(
        &self,
        id: SupervisorRunId,
        now: DateTime<Utc>,
        waker: &mut dyn DecisionWaker,
    ) -> Result<()> {
        let dispatch_runs = self.dispatch_runs()?;
        self.tick_run(id, now, &dispatch_runs)?;
        self.deliver_reserved(now, waker)
    }

    fn tick_run(
        &self,
        id: SupervisorRunId,
        now: DateTime<Utc>,
        dispatch_runs: &[usagi_core::domain::agent::DispatchRun],
    ) -> Result<()> {
        let Some(mut run) = self.supervisor.load(id)? else {
            return Ok(());
        };
        // Retry eligibility is a persisted deadline, not an in-memory timer.
        // Reconciliation therefore cannot dispatch a retry before its deadline
        // and can resume one after a daemon restart without polling.
        let mut due_retries = Vec::new();
        for (id, task) in &run.tasks {
            if task.state == TaskState::Retrying
                && matches!(task.retry_at, Some(retry_at) if retry_at <= now)
            {
                due_retries.push((id.clone(), task.generation));
            }
        }
        for (task_id, generation) in due_retries {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::Timer,
                SupervisorEventKind::RetryReady {
                    task_id,
                    generation,
                },
            )?;
        }
        for (task_id, provenance) in run.provenance.clone() {
            let Some(dispatch_run) = dispatch_runs
                .iter()
                .find(|run| run.run_id == provenance.dispatch_run_id)
            else {
                continue;
            };
            let Some((terminal, kind)) = terminal(dispatch_run.status) else {
                continue;
            };
            let task = run
                .tasks
                .get(&task_id)
                .cloned()
                .expect("provenance task exists");
            if task.state == TaskState::Dispatched {
                let event = SupervisorEventKind::Running {
                    task_id: task_id.clone(),
                    generation: task.generation,
                };
                run = self.apply(&run, now, SupervisorEventSource::DispatchCompletion, event)?;
            }
            let current = run.tasks.get(&task_id).expect("task retained");
            if matches!(current.state, TaskState::Dispatched | TaskState::Running) {
                let event = SupervisorEventKind::SetTaskState {
                    task_id: task_id.clone(),
                    generation: current.generation,
                    state: terminal,
                };
                run = self.apply(&run, now, source(kind), event)?;
            } else if !current.state.terminal() && current.state != TaskState::Verifying {
                continue;
            }
            run = self.record_terminal_handoff(run, &task_id, &provenance, kind, now)?;
            if let Some(parent_id) = task.parent_task_id {
                let child_run = provenance.dispatch_run_id;
                self.reserve_parent_wake(&mut run, &parent_id, child_run, kind, now)?;
            }
        }
        run = self.finalize_terminal_tasks(run, now)?;
        // A ready task without a dispatch reservation is not progress. The
        // previous scheduler left such runs in `running` forever when selector
        // resolution or dispatch admission produced no worker. Persist a typed
        // escalation so callers can distinguish an actionable stop from a live
        // scheduler and inspect the exact blocking task.
        if run.state == SupervisorRunState::Running
            && let Some((task_id, _)) = run.tasks.iter().find(|(task_id, task)| {
                let legacy_goal_promotion = task_id.0 == "root"
                    && task.required_artifact_contract == GOAL_REVIEW_READY_ARTIFACT_CONTRACT
                    && !run.provenance.contains_key(*task_id);
                task.state == TaskState::Ready
                    && task.assigned_dispatch_run.is_none()
                    && task.promotion_reserved_at.is_none()
                    && !legacy_goal_promotion
            })
        {
            let _ = self.apply(
                &run,
                now,
                SupervisorEventSource::DispatchFailure,
                SupervisorEventKind::Escalate {
                    task_id: Some(task_id.clone()),
                    reason: MISSING_DISPATCH_ESCALATION_REASON.into(),
                    safe_evidence:
                        "runtime/model selection or dispatch admission did not assign a run".into(),
                    choices: vec!["resume".into(), "cancel".into()],
                },
            )?;
        }
        Ok(())
    }

    fn dispatch_runs(&self) -> Result<Vec<usagi_core::domain::agent::DispatchRun>> {
        #[cfg(test)]
        self.dispatch_registry_reads
            .set(self.dispatch_registry_reads.get() + 1);
        self.dispatch.runs()
    }
    fn apply(
        &self,
        run: &usagi_core::domain::supervisor::SupervisorRun,
        now: DateTime<Utc>,
        source: SupervisorEventSource,
        kind: SupervisorEventKind,
    ) -> Result<usagi_core::domain::supervisor::SupervisorRun> {
        let call = self.apply_calls.get();
        self.apply_calls.set(call + 1);
        if self.apply_fail_at.get() == Some(call) {
            anyhow::bail!("injected supervisor apply failure");
        }
        let event = SupervisorEvent {
            sequence: run.state_revision + 1,
            event_id: OperationId::new(),
            causation_id: None,
            correlation_id: None,
            observed_at: now,
            payload_digest: "scheduler".into(),
            source,
            kind,
        };
        self.apply_event(run, &event)
    }

    fn apply_event(
        &self,
        run: &usagi_core::domain::supervisor::SupervisorRun,
        event: &SupervisorEvent,
    ) -> Result<usagi_core::domain::supervisor::SupervisorRun> {
        self.supervisor
            .apply(run.supervisor_run_id, run.state_revision, event)
    }
    fn outcome(&self, child: OperationId, fallback: InboxKind) -> Result<WakeOutcome> {
        let message = match self.dispatch.binding(child)? {
            Some(binding) => self
                .dispatch
                .inbox(&binding.caller)
                .ok()
                .and_then(|messages| messages.into_iter().find(|message| message.run_id == child)),
            None => None,
        };
        Ok(message.map_or(
            WakeOutcome {
                kind: fallback,
                summary: "worker terminal state committed without an inbox report".into(),
            },
            |message| WakeOutcome {
                kind: message.kind,
                summary: message.summary,
            },
        ))
    }

    fn ensure_start_capacity(&self, state: &mut RuntimeState) -> Result<()> {
        if state.starts.len() < MAX_START_RESERVATIONS {
            return Ok(());
        }
        let mut recyclable = Vec::new();
        for (key, reservation) in &state.starts {
            match self.supervisor.load(reservation.supervisor_run_id)? {
                // A crash can leave the reservation before aggregate
                // initialization. Keep its exact run identity so the same
                // operation can finish initialization on retry.
                Some(run)
                    if run.state.is_finished()
                        && !has_unbound_root_worker(&run, Some(reservation)) =>
                {
                    recyclable.push((run.terminal_at.or(Some(run.updated_at)), key.clone()));
                }
                None | Some(_) => {}
            }
        }
        recyclable.sort();
        for (_, key) in recyclable {
            if state.starts.len() < MAX_START_RESERVATIONS {
                break;
            }
            let caller_dispatch = state.starts[&key].caller_dispatch_run_id;
            let _ = state.starts.remove(&key);
            state.expired_starts.insert(&key);
            if let Some(caller_dispatch) = caller_dispatch {
                state.expired_starts.insert(&caller_dispatch.to_string());
            }
        }
        if state.starts.len() >= MAX_START_RESERVATIONS {
            anyhow::bail!("supervisor start reservation capacity is exhausted");
        }
        Ok(())
    }

    fn ensure_control_capacity(&self, state: &mut RuntimeState) -> Result<()> {
        if state.controls.len() < MAX_CONTROL_RESERVATIONS {
            return Ok(());
        }
        let mut recyclable = Vec::new();
        for (key, reservation) in &state.controls {
            match self.supervisor.load(reservation.supervisor_run_id)? {
                None => recyclable.push((reservation.reserved_at, key.clone())),
                Some(run) if run.state.is_finished() => {
                    recyclable.push((reservation.reserved_at, key.clone()));
                }
                Some(_) => {}
            }
        }
        recyclable.sort();
        for (_, key) in recyclable {
            if state.controls.len() < MAX_CONTROL_RESERVATIONS {
                break;
            }
            state.controls.remove(&key);
            state.expired_controls.insert(&key);
        }
        if state.controls.len() >= MAX_CONTROL_RESERVATIONS {
            anyhow::bail!("supervisor control reservation capacity is exhausted");
        }
        Ok(())
    }

    fn load_state(&self) -> Result<RuntimeState> {
        let mut state: RuntimeState =
            json_file::read_bounded(&self.state_path, MAX_RUNTIME_STATE_BYTES)?.unwrap_or_default();
        let migrated = state.migrate_start_semantics();
        state.validate_limits()?;
        if migrated {
            self.save_state(&state)?;
        }
        Ok(state)
    }
    fn save_state(&self, state: &RuntimeState) -> Result<()> {
        state.validate_limits()?;
        anyhow::ensure!(
            serde_json::to_vec(state)?.len() <= MAX_RUNTIME_STATE_BYTES,
            "supervisor runtime metadata exceeds its serialized byte limit"
        );
        json_file::write_atomic(
            self.state_path.parent().expect("state path has parent"),
            &self.state_path,
            state,
        )
    }
}

fn task_node(
    run: &SupervisorRun,
    task_id: TaskId,
    parent_task_id: Option<TaskId>,
    dependencies: BTreeSet<TaskId>,
    instruction: String,
    required_artifact_contract: ArtifactContract,
) -> TaskNode {
    TaskNode {
        instruction_digest: format!("task:{}", task_id.0),
        task_id,
        supervisor_run_id: run.supervisor_run_id,
        parent_task_id,
        dependencies,
        instruction_body: instruction,
        required_artifact_contract,
        attempt: 1,
        generation: 1,
        assigned_dispatch_run: None,
        promotion_reserved_at: None,
        promotion_parent_dispatch_run: None,
        promotion_worker_session_id: None,
        promotion_worker_profile_id: None,
        promotion_worker_agent_id: None,
        promotion_worker_semantic_digest: None,
        retry_at: None,
        verification_digest: None,
        verification_attempt: 0,
        verification_retry_at: None,
        verification_expectation: None,
        state: TaskState::Pending,
    }
}

fn artifact_worktrees(run: &SupervisorRun) -> Vec<ArtifactWorktreeRef> {
    let mut worktrees = run
        .provenance
        .iter()
        .filter_map(|(id, provenance)| {
            let current = run.tasks.get(id)?;
            (current.generation == provenance.generation
                && matches!(current.state, TaskState::Succeeded | TaskState::Verifying))
            .then_some(ArtifactWorktreeRef {
                session_id: provenance.worker_session_id,
                worktree_id: provenance.worker_worktree_id,
            })
        })
        .collect::<Vec<_>>();
    worktrees.sort();
    worktrees.dedup();
    worktrees
}

fn compact_handoff_text(value: &str, max: usize) -> String {
    let mut compact = String::new();
    let mut pending_space = false;
    let mut truncated = false;
    for character in value.chars() {
        if character.is_whitespace() {
            pending_space = !compact.is_empty();
            continue;
        }
        if !presentation_text_is_safe(&character.to_string()) {
            continue;
        }
        if pending_space {
            if compact.len() + 1 > max {
                truncated = true;
                break;
            }
            compact.push(' ');
            pending_space = false;
        }
        if compact.len() + character.len_utf8() > max {
            truncated = true;
            break;
        }
        compact.push(character);
    }
    if truncated {
        let mut end = max.saturating_sub('…'.len_utf8()).min(compact.len());
        while end > 0 && !compact.is_char_boundary(end) {
            end -= 1;
        }
        compact.truncate(end);
        if max >= '…'.len_utf8() {
            compact.push('…');
        }
    }
    if compact.is_empty() {
        "worker supplied no safe summary text".into()
    } else {
        compact
    }
}

fn structured_artifact_summary(result: &StructuredResult) -> Option<String> {
    const MAX_ITEMS: usize = 8;
    const MAX_ITEM_BYTES: usize = 256;

    let mut facts = Vec::new();
    if let Some(pr) = result.pr.as_deref() {
        facts.push(format!("PR {}", compact_handoff_text(pr, MAX_ITEM_BYTES)));
    }
    if !result.commits.is_empty() {
        let commits = result
            .commits
            .iter()
            .take(MAX_ITEMS)
            .map(|commit| compact_handoff_text(commit, MAX_ITEM_BYTES))
            .collect::<Vec<_>>()
            .join(", ");
        let omitted = result.commits.len().saturating_sub(MAX_ITEMS);
        facts.push(if omitted == 0 {
            format!("commits {commits}")
        } else {
            format!("commits {commits} (+{omitted} omitted)")
        });
    }
    if !result.changed_files.is_empty() {
        let files = result
            .changed_files
            .iter()
            .take(MAX_ITEMS)
            .map(|file| compact_handoff_text(file, MAX_ITEM_BYTES))
            .collect::<Vec<_>>()
            .join(", ");
        let omitted = result.changed_files.len().saturating_sub(MAX_ITEMS);
        facts.push(if omitted == 0 {
            format!("files {files}")
        } else {
            format!("files {files} (+{omitted} omitted)")
        });
    }
    if let Some(verification) = result.verification.as_deref() {
        facts.push(format!(
            "verification {}",
            compact_handoff_text(verification, MAX_ITEM_BYTES)
        ));
    }
    if facts.is_empty() {
        return None;
    }
    Some(compact_handoff_text(
        &facts.join("; "),
        MAX_HANDOFF_ARTIFACT_BYTES,
    ))
}

fn terminal(status: RunStatus) -> Option<(TaskState, InboxKind)> {
    match status {
        RunStatus::Preparing | RunStatus::Running => None,
        RunStatus::Completed => Some((TaskState::Succeeded, InboxKind::Completed)),
        RunStatus::Failed => Some((TaskState::Failed, InboxKind::Failed)),
        RunStatus::NoReport => Some((TaskState::Failed, InboxKind::NoReport)),
    }
}
fn source(kind: InboxKind) -> SupervisorEventSource {
    match kind {
        InboxKind::Completed => SupervisorEventSource::DispatchCompletion,
        InboxKind::Failed => SupervisorEventSource::DispatchFailure,
        InboxKind::NoReport => SupervisorEventSource::NoReport,
    }
}

mod reservation;

mod obligations;

#[cfg(test)]
mod tests;
