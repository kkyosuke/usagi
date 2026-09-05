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
        &usagi_core::usecase::client::agent_dispatch_semantic_key(
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
        self.start_scoped(
            caller,
            None,
            operation_id,
            root_task,
            NO_ARTIFACT_CONTRACT,
            None,
            None,
            None,
            None,
            initial_tasks,
            policy_selector,
            now,
        )
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
        self.start_scoped(
            caller,
            Some(workspace),
            operation_id,
            root_task,
            NO_ARTIFACT_CONTRACT,
            None,
            None,
            None,
            None,
            initial_tasks,
            policy_selector,
            now,
        )
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
        self.start_scoped(
            caller,
            Some(workspace),
            operation_id,
            root_task,
            NO_ARTIFACT_CONTRACT,
            None,
            Some(agent.runtime),
            Some(usagi_core::infrastructure::ipc::agent_operation_digest(
                &admission.semantic_key,
            )),
            Some(&CallerDispatchReservation {
                dispatch_run_id,
                worker_session_id: worker.session_id,
                worker_agent_id: dispatch.agent_id,
                worker_runtime_id: worker.agent_runtime_id,
            }),
            Vec::new(),
            policy_selector,
            now,
        )
    }

    /// Durably reserves the Goal run before the Agent process is spawned. This
    /// is the first phase of Goal admission and makes the run ID available even
    /// if binding must be reconciled after a daemon restart.
    ///
    /// # Errors
    /// Returns an error when the Goal contract or durable start cannot be
    /// admitted.
    pub fn reserve_goal_for_workspace(
        &self,
        caller: &str,
        workspace: WorkspaceId,
        operation_id: &str,
        goal: GoalSpecification,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.start_scoped(
            caller,
            Some(workspace),
            operation_id,
            goal.instruction,
            GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            Some(goal.artifact_repository),
            None,
            None,
            None,
            Vec::new(),
            policy_selector,
            now,
        )
    }

    /// Goal reservation variant which also pins the selected Agent runtime
    /// family before any process can be spawned.
    ///
    /// # Errors
    /// Returns an error when the reservation conflicts with an existing
    /// operation or cannot be persisted.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_goal_for_workspace_with_profile(
        &self,
        caller: &str,
        workspace: WorkspaceId,
        operation_id: &str,
        goal: GoalSpecification,
        worker_profile_id: AgentProfileId,
        worker_semantic_digest: String,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.start_scoped(
            caller,
            Some(workspace),
            operation_id,
            goal.instruction,
            GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            Some(goal.artifact_repository),
            Some(worker_profile_id),
            Some(worker_semantic_digest),
            None,
            Vec::new(),
            policy_selector,
            now,
        )
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

    /// Binds the exact Agent fence to a previously reserved Goal root. The
    /// operation ID is the durable join key; caller text and Goal content are
    /// never reconstructed by the recovery path.
    ///
    /// # Errors
    /// Returns an error when the reservation, worker scope, dispatch run, or
    /// existing provenance conflicts.
    pub fn bind_reserved_workspace_root_dispatch(
        &self,
        operation_id: &str,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        let dispatch_run_id = OperationId::parse(operation_id)
            .map_err(|_| anyhow::anyhow!("supervisor root dispatch operation is invalid"))?;
        if self.dispatch.run(dispatch_run_id)?.is_some() {
            let state = self.load_state()?;
            if let Some(reservation) = state.starts.get(operation_id)
                && self
                    .load_started_run(reservation.supervisor_run_id)?
                    .artifact_repository
                    .is_none()
            {
                anyhow::bail!("reserved supervisor run is not a Goal run");
            }
        }
        self.bind_reserved_root_task(
            operation_id,
            dispatch_run_id,
            worker,
            GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            true,
            now,
        )
    }

    /// Binds a generic `supervisor_start` root to the exact authenticated Agent
    /// dispatch which invoked the tool. This makes a public run observable from
    /// its first turn instead of manufacturing an unowned Ready task.
    ///
    /// # Errors
    /// Returns an error when the reservation or exact caller dispatch fence is
    /// missing, conflicting, or cannot be persisted.
    pub fn bind_reserved_caller_dispatch(
        &self,
        start_operation_id: &str,
        dispatch_run_id: OperationId,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        self.bind_reserved_root_task(
            start_operation_id,
            dispatch_run_id,
            worker,
            NO_ARTIFACT_CONTRACT,
            false,
            now,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Every persisted root fence is validated together before binding.
    fn bind_reserved_root_task(
        &self,
        start_operation_id: &str,
        dispatch_run_id: OperationId,
        worker: &AgentRuntimeRef,
        expected_contract: ArtifactContract,
        require_workspace_root: bool,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        let dispatch = self
            .dispatch
            .run(dispatch_run_id)?
            .context("supervisor root dispatch does not exist")?;
        let state = self.load_state()?;
        let reservation = state
            .starts
            .get(start_operation_id)
            .context("supervisor root reservation does not exist")?;
        if matches!(reservation.workspace_id, Some(expected) if expected != worker.terminal.workspace_id)
        {
            anyhow::bail!("supervisor root worker is outside its reserved workspace");
        }
        if matches!(reservation.caller_dispatch_run_id, Some(expected) if expected != dispatch_run_id)
        {
            anyhow::bail!("supervisor root caller dispatch conflicts with its reservation");
        }
        let dispatch_agent = self
            .dispatch
            .agent_in_workspace(worker.terminal.workspace_id, dispatch.agent_id)?
            .context("supervisor root dispatch Agent does not exist")?;
        if matches!(reservation.worker_session_id, Some(expected) if Some(expected) != worker.session_id)
            || matches!(reservation.worker_agent_id, Some(expected) if expected != dispatch.agent_id)
            || matches!(reservation.worker_runtime_id, Some(expected) if expected != worker.agent_runtime_id)
            || dispatch_agent.session_id != worker.session_id
        {
            anyhow::bail!("supervisor root worker is outside its reserved Agent scope");
        }
        if let Some(expected_profile) = reservation.worker_profile_id.as_ref()
            && &dispatch_agent.runtime != expected_profile
        {
            anyhow::bail!("supervisor root worker is outside its reserved Agent scope");
        }
        if let Some(expected_digest) = reservation.worker_semantic_digest.as_ref() {
            let admission = self
                .dispatch
                .admission(dispatch_run_id)?
                .context("supervisor root admission does not exist")?;
            if usagi_core::infrastructure::ipc::agent_operation_digest(&admission.semantic_key)
                != *expected_digest
            {
                anyhow::bail!("supervisor root Agent admission has another semantic intent");
            }
        }
        let mut run = self.load_started_run(reservation.supervisor_run_id)?;
        if run.workspace_id != Some(worker.terminal.workspace_id)
            || (require_workspace_root && worker.session_id.is_some())
        {
            anyhow::bail!("supervisor root worker is outside the workspace root scope");
        }
        let root_id = TaskId::new("root")?;
        let root = run
            .tasks
            .get(&root_id)
            .context("supervisor root task is missing")?;
        if root.required_artifact_contract != expected_contract {
            anyhow::bail!("supervisor root reservation has another artifact contract");
        }
        let root_generation = root.generation;
        if run.state == SupervisorRunState::Planning {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::Admission,
                SupervisorEventKind::SetRunState {
                    state: SupervisorRunState::Running,
                    terminal_reason: None,
                },
            )?;
        }
        self.ensure_supervisor_start_dispatch_available(start_operation_id, dispatch_run_id)?;
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: root_id.clone(),
            parent_task_id: None,
            parent_dispatch_run: None,
            dispatch_run_id,
            worker_session_id: worker.session_id,
            worker_agent_id: worker.agent_runtime_id,
            worker_worktree_id: worker.terminal.worktree_id,
            generation: root_generation,
        };
        if let Some(existing) = run.provenance.get(&root_id) {
            if existing == &provenance {
                return Ok(run.query());
            }
            anyhow::bail!("supervisor root dispatch provenance conflicts with the existing run");
        }
        run = self.resume_pending_promotion_escalation(run, &root_id, now)?;
        run = self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::Dispatch {
                task_id: root_id,
                generation: root_generation,
                provenance,
            },
        )?;
        Ok(run.query())
    }

    fn load_started_run(&self, id: SupervisorRunId) -> Result<SupervisorRun> {
        self.supervisor
            .load(id)?
            .ok_or_else(|| anyhow::anyhow!("supervisor run disappeared during root binding"))
    }

    /// Clears only the synthetic escalation produced by an older scheduler
    /// while this exact task's durable Agent promotion was still pending.
    /// Other human and policy escalations remain authoritative.
    fn resume_pending_promotion_escalation(
        &self,
        run: SupervisorRun,
        task_id: &TaskId,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRun> {
        let Some(escalation_id) = run
            .escalation
            .as_ref()
            .filter(|escalation| {
                run.state == SupervisorRunState::Escalated
                    && escalation.blocking_task_id.as_ref() == Some(task_id)
                    && escalation.reason == MISSING_DISPATCH_ESCALATION_REASON
            })
            .map(|escalation| escalation.escalation_id)
        else {
            return Ok(run);
        };
        self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::ResolveEscalation {
                escalation_id,
                decision: EscalationDecision::Resume,
            },
        )
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

    /// Lists Goal reservations whose root dispatch still needs provenance. The
    /// operation ID is returned from durable scheduler metadata rather than
    /// inferred from mutable Agent state.
    ///
    /// # Errors
    /// Returns an error when scheduler metadata or a referenced run is invalid.
    pub fn pending_goal_promotions(&self) -> Result<Vec<PendingGoalPromotion>> {
        let state = self.load_state()?;
        let mut pending = Vec::new();
        for (operation_id, reservation) in state.starts {
            let Some(run) = self.supervisor.load(reservation.supervisor_run_id)? else {
                continue;
            };
            let Some(workspace_id) = run.workspace_id else {
                continue;
            };
            let root = TaskId::new("root")?;
            if run.tasks.get(&root).is_some_and(|task| {
                task.required_artifact_contract == GOAL_REVIEW_READY_ARTIFACT_CONTRACT
            }) && !run.provenance.contains_key(&root)
                && !run.state.is_finished()
            {
                pending.push(PendingGoalPromotion {
                    operation_id,
                    reserved_at: run.created_at,
                    workspace_id,
                    worker_profile_id: reservation.worker_profile_id,
                    worker_semantic_digest: reservation.worker_semantic_digest,
                });
            }
        }
        Ok(pending)
    }

    /// Returns the repository pinned by an existing Goal reservation so an
    /// idempotent admission replay never consults worker-mutable Git config.
    ///
    /// # Errors
    /// Returns an error when scheduler metadata or the reserved run is invalid.
    pub fn reserved_goal_repository(&self, operation_id: &str) -> Result<Option<GitHubRepository>> {
        let state = self.load_state()?;
        let Some(reservation) = state.starts.get(operation_id) else {
            return Ok(None);
        };
        if let Some(repository) = reservation.artifact_repository.clone() {
            if let Some(run) = self.supervisor.load(reservation.supervisor_run_id)?
                && run.artifact_repository.as_ref() != Some(&repository)
            {
                anyhow::bail!("Goal reservation repository conflicts with its durable run");
            }
            return Ok(Some(repository));
        }
        Ok(self
            .supervisor
            .load(reservation.supervisor_run_id)?
            .and_then(|run| run.artifact_repository))
    }

    /// Marks a pre-spawn Goal reservation failed after Agent admission proves a
    /// definite failure. Ambiguous/post-spawn failures must remain pending for
    /// reconciliation instead.
    ///
    /// # Errors
    /// Returns an error when the durable reservation cannot be loaded or
    /// transitioned.
    pub fn fail_reserved_goal(
        &self,
        operation_id: &str,
        reason: String,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        let state = self.load_state()?;
        let reservation = state
            .starts
            .get(operation_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor goal reservation does not exist"))?;
        let mut run = self.load_started_run(reservation.supervisor_run_id)?;
        let root_id = TaskId::new("root")?;
        let root = run
            .tasks
            .get(&root_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor root task is missing"))?;
        if root.required_artifact_contract != GOAL_REVIEW_READY_ARTIFACT_CONTRACT {
            anyhow::bail!("supervisor reservation is not a Goal run");
        }
        if run.state.is_finished() {
            return Ok(run.query());
        }
        run = self.resume_pending_promotion_escalation(run, &root_id, now)?;
        Ok(self
            .apply(
                &run,
                now,
                SupervisorEventSource::DispatchFailure,
                SupervisorEventKind::SetRunState {
                    state: SupervisorRunState::Failed,
                    terminal_reason: Some(reason),
                },
            )?
            .query())
    }

    /// Closes a generic caller-root reservation whose exact authenticated
    /// dispatch can no longer be bound. The retained start fence remains until
    /// worker reconciliation proves the caller operation stopped or absent.
    ///
    /// # Errors
    /// Returns an error when the start is not a generic caller reservation or
    /// its durable run cannot be transitioned.
    pub fn fail_reserved_caller_dispatch(
        &self,
        start_operation_id: &str,
        reason: String,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        let state = self.load_state()?;
        let reservation = state
            .starts
            .get(start_operation_id)
            .filter(|reservation| reservation.caller_dispatch_run_id.is_some())
            .context("supervisor caller reservation does not exist")?;
        let mut run = self.load_started_run(reservation.supervisor_run_id)?;
        let root_id = TaskId::new("root")?;
        let root = run
            .tasks
            .get(&root_id)
            .context("supervisor caller root task is missing")?;
        if root.required_artifact_contract != NO_ARTIFACT_CONTRACT {
            anyhow::bail!("supervisor reservation is not a caller-root run");
        }
        if run.state.is_finished() {
            return Ok(run.query());
        }
        run = self.resume_pending_promotion_escalation(run, &root_id, now)?;
        Ok(self
            .apply(
                &run,
                now,
                SupervisorEventSource::DispatchFailure,
                SupervisorEventKind::SetRunState {
                    state: SupervisorRunState::Failed,
                    terminal_reason: Some(reason),
                },
            )?
            .query())
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

    #[allow(clippy::too_many_arguments)] // The commit boundary compares each independent Agent identity fence explicitly.
    fn ensure_pending_operation_matches_reservation(
        &self,
        operation: OperationId,
        workspace_id: WorkspaceId,
        requires_session: bool,
        worker_session_id: Option<SessionId>,
        worker_profile_id: Option<&AgentProfileId>,
        worker_agent_id: Option<AgentId>,
        worker_semantic_digest: Option<&str>,
    ) -> Result<()> {
        let Some(dispatch) = self.dispatch.run(operation)? else {
            return Ok(());
        };
        if !matches!(dispatch.status, RunStatus::Preparing | RunStatus::Running) {
            anyhow::bail!("pending Supervisor operation has closed supervisor ownership");
        }
        let agent = self
            .dispatch
            .agent_in_workspace(workspace_id, dispatch.agent_id)?
            .context("pending Supervisor operation has foreign Agent ownership")?;
        let session_matches = if requires_session {
            match worker_session_id {
                Some(expected) => agent.session_id == Some(expected),
                None => agent.session_id.is_some(),
            }
        } else {
            agent.session_id.is_none()
        };
        if !session_matches
            || matches!(worker_profile_id, Some(expected) if &agent.runtime != expected)
            || matches!(worker_agent_id, Some(expected) if dispatch.agent_id != expected)
        {
            anyhow::bail!("pending Supervisor operation conflicts with its Agent ownership");
        }
        if let Some(expected) = worker_semantic_digest {
            let admission = self
                .dispatch
                .admission(operation)?
                .context("pending Supervisor operation has no semantic authority")?;
            if usagi_core::infrastructure::ipc::agent_operation_digest(&admission.semantic_key)
                != expected
            {
                anyhow::bail!("pending Supervisor operation conflicts with its Agent semantics");
            }
        }
        Ok(())
    }

    fn retained_dispatch_owners(
        &self,
        state: &RuntimeState,
        dispatch_run: OperationId,
    ) -> Result<Vec<(SupervisorRunId, TaskId)>> {
        let mut pending_roots = state
            .starts
            .values()
            .filter_map(|reservation| {
                if reservation.caller_dispatch_run_id == Some(dispatch_run) {
                    Some((reservation.supervisor_run_id, NO_ARTIFACT_CONTRACT))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if let Some(reservation) = state.starts.get(&dispatch_run.to_string())
            && reservation.caller_dispatch_run_id.is_none()
            && self
                .supervisor
                .load(reservation.supervisor_run_id)?
                .is_none()
        {
            // Before aggregate initialization a Goal and a legacy generic
            // start cannot be distinguished. Conservatively retain the exact
            // operation as a conceptual owner so it cannot become classic.
            pending_roots.push((
                reservation.supervisor_run_id,
                GOAL_REVIEW_READY_ARTIFACT_CONTRACT,
            ));
        }
        let pending_delegated = delegated_task_id(dispatch_run)?;
        let mut owners = Vec::new();
        let mut found_pending_roots = BTreeSet::new();
        for run in self.supervisor.runs()? {
            let mut run_pending_roots = pending_roots
                .iter()
                .filter(|(run_id, _)| *run_id == run.supervisor_run_id)
                .copied()
                .collect::<Vec<_>>();
            if run.artifact_repository.is_some()
                && state
                    .starts
                    .get(&dispatch_run.to_string())
                    .is_some_and(|reservation| {
                        reservation.caller_dispatch_run_id.is_none()
                            && reservation.supervisor_run_id == run.supervisor_run_id
                    })
            {
                run_pending_roots
                    .push((run.supervisor_run_id, GOAL_REVIEW_READY_ARTIFACT_CONTRACT));
            }
            for (task_id, _) in run
                .provenance
                .iter()
                .filter(|(_, provenance)| provenance.dispatch_run_id == dispatch_run)
            {
                let owner = (run.supervisor_run_id, task_id.clone());
                if !owners.contains(&owner) {
                    owners.push(owner);
                }
            }
            for (_, expected_contract) in &run_pending_roots {
                found_pending_roots.insert(run.supervisor_run_id);
                let root = TaskId::new("root")?;
                if let Some(task) = run.tasks.get(&root) {
                    if !(task.supervisor_run_id == run.supervisor_run_id
                        && task.parent_task_id.is_none()
                        && task.required_artifact_contract == *expected_contract
                        && (expected_contract == &NO_ARTIFACT_CONTRACT
                            || run.artifact_repository.is_some()))
                    {
                        anyhow::bail!("retained supervisor root reservation is malformed");
                    }
                } else if run.state != SupervisorRunState::Planning {
                    anyhow::bail!("retained supervisor root reservation is malformed");
                }
                let owner = (run.supervisor_run_id, root);
                if !owners.contains(&owner) {
                    owners.push(owner);
                }
            }
            if run
                .tasks
                .get(&pending_delegated)
                .is_some_and(|task| is_delegated_reservation(task, dispatch_run))
            {
                let owner = (run.supervisor_run_id, pending_delegated.clone());
                if !owners.contains(&owner) {
                    owners.push(owner);
                }
            }
        }
        for (run_id, _) in pending_roots {
            if !found_pending_roots.contains(&run_id) {
                let owner = (run_id, TaskId::new("root")?);
                if !owners.contains(&owner) {
                    owners.push(owner);
                }
            }
        }
        Ok(owners)
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

    /// Persists a delegated task before its Agent spawn. `None` means the
    /// parent dispatch is not supervised and classic delegation is unchanged.
    ///
    /// # Errors
    /// Returns an error for a conflicting child operation or durable reducer
    /// failure.
    #[allow(clippy::too_many_arguments)] // Reservation records every exact child identity before the Agent effect.
    pub fn reserve_delegated_dispatch_for_session(
        &self,
        parent_dispatch_run: OperationId,
        child_operation_id: &str,
        instruction: impl AsRef<str>,
        worker_session_id: SessionId,
        reserved_worker: &usagi_core::domain::agent::Agent,
        session_name: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<DelegatedDispatchReservation>> {
        self.reserve_delegated_dispatch_inner(
            parent_dispatch_run,
            child_operation_id,
            instruction,
            Some(worker_session_id),
            Some(reserved_worker),
            Some(session_name),
            false,
            now,
        )
    }

    #[cfg(test)]
    fn reserve_delegated_dispatch(
        &self,
        parent_dispatch_run: OperationId,
        child_operation_id: &str,
        instruction: impl AsRef<str>,
        now: DateTime<Utc>,
    ) -> Result<Option<DelegatedDispatchReservation>> {
        self.reserve_delegated_dispatch_inner(
            parent_dispatch_run,
            child_operation_id,
            instruction,
            None,
            None,
            None,
            false,
            now,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Validation and durable reservation form one atomic Supervisor boundary.
    fn reserve_delegated_dispatch_inner(
        &self,
        parent_dispatch_run: OperationId,
        child_operation_id: &str,
        instruction: impl AsRef<str>,
        worker_session_id: Option<SessionId>,
        reserved_worker: Option<&usagi_core::domain::agent::Agent>,
        session_name: Option<&str>,
        allow_existing_agent_operation: bool,
        now: DateTime<Utc>,
    ) -> Result<Option<DelegatedDispatchReservation>> {
        let instruction = instruction.as_ref();
        bounded_nonempty(
            "delegated supervisor instruction",
            instruction,
            MAX_SUPERVISOR_TEXT_BYTES,
        )?;
        let child_dispatch_run = OperationId::parse(child_operation_id)
            .context("delegated dispatch operation is invalid")?;
        let Some((mut run, parent_task_id)) = self.supervised_parent(parent_dispatch_run)? else {
            return Ok(None);
        };
        let worker_profile_id = worker_session_id
            .map(|_| self.dispatch_profile(parent_dispatch_run))
            .transpose()?;
        if let Some(worker) = reserved_worker
            && (worker.session_id != worker_session_id
                || worker_profile_id.as_ref() != Some(&worker.runtime))
        {
            anyhow::bail!("delegated Agent reservation is outside its Supervisor scope");
        }
        let worker_agent_id = reserved_worker.map(|worker| worker.agent_id);
        if child_dispatch_run == parent_dispatch_run {
            anyhow::bail!("delegated dispatch operation must differ from its parent");
        }
        let task_id = delegated_task_id(child_dispatch_run)?;
        if let Some(existing) = run.tasks.get(&task_id) {
            if existing.state.terminal()
                && existing.assigned_dispatch_run.is_none()
                && !run.provenance.contains_key(&task_id)
            {
                // A terminal unbound reservation may represent a refusal that
                // never reached Agent durable admission. Burning its operation
                // identity prevents a retry from spawning an unbindable worker.
                anyhow::bail!("delegated task conflicts with its existing supervisor task");
            }
            let authority_operation = if existing.state.terminal() {
                let provenance = run
                    .provenance
                    .get(&task_id)
                    .context("terminal delegated task has no dispatch provenance")?;
                validate_provenance_chain(&run, &task_id, provenance)?;
                provenance.dispatch_run_id
            } else {
                live_task_dispatch_authority(
                    &self.load_state()?,
                    &run,
                    &task_id,
                    &mut BTreeSet::new(),
                )?
                .context("delegated task has no current promotion authority")?
                .operation_id
            };
            let suffix = delegated_task_suffix(child_dispatch_run, instruction);
            let worker_semantic_digest = delegated_worker_semantic_digest(
                worker_agent_id,
                session_name,
                &existing.instruction_body,
            );
            let existing_parent_dispatch = match existing.promotion_parent_dispatch_run {
                Some(parent) => Some(parent),
                None => match run.provenance.get(&task_id) {
                    Some(provenance) => provenance.parent_dispatch_run,
                    None => None,
                },
            };
            if authority_operation != child_dispatch_run
                || !is_delegated_reservation(existing, child_dispatch_run)
                || existing.parent_task_id.as_ref() != Some(&parent_task_id)
                || matches!(existing_parent_dispatch, Some(parent) if parent != parent_dispatch_run)
                || matches!(existing.promotion_worker_session_id, Some(session) if Some(session) != worker_session_id)
                || matches!(&existing.promotion_worker_profile_id, Some(profile) if Some(profile) != worker_profile_id.as_ref())
                || matches!(existing.promotion_worker_agent_id, Some(agent) if Some(agent) != worker_agent_id)
                || matches!(&existing.promotion_worker_semantic_digest, Some(digest) if Some(digest) != worker_semantic_digest.as_ref())
                || (existing.instruction_body != instruction
                    && !existing.instruction_body.ends_with(&suffix))
                || existing.required_artifact_contract != NO_ARTIFACT_CONTRACT
            {
                anyhow::bail!("delegated task conflicts with its existing supervisor task");
            }
            return Ok(Some(DelegatedDispatchReservation {
                run: run.query(),
                prompt: existing.instruction_body.clone(),
            }));
        }
        self.ensure_new_delegated_operation_is_unused(
            child_dispatch_run,
            &task_id,
            allow_existing_agent_operation,
        )?;
        let mut policy_snapshot = run.clone();
        if let Some(parent) = policy_snapshot.tasks.get_mut(&parent_task_id)
            && parent.parent_task_id.is_none()
            && parent.assigned_dispatch_run.is_none()
            && parent.promotion_reserved_at.is_none()
            && has_caller_root_reservation(&self.load_state()?, run.supervisor_run_id)
        {
            // A pre-upgrade generic start can gain its durable caller join on
            // exact replay without a promotion timestamp in the older task
            // snapshot. Count that reserved root for policy admission too.
            parent.promotion_reserved_at = Some(now);
        }
        if let Some(reason) = child_dispatch_policy_denial(&policy_snapshot, &parent_task_id)? {
            if run.escalation.is_none() {
                let event = SupervisorEventKind::Escalate {
                    task_id: Some(parent_task_id.clone()),
                    reason: reason.clone(),
                    safe_evidence: "policy limits were evaluated before the delegated Agent effect"
                        .into(),
                    choices: vec!["resume".into(), "cancel".into()],
                };
                self.apply(&run, now, SupervisorEventSource::Admission, event)?;
            }
            anyhow::bail!("supervisor policy denied delegated dispatch: {reason}");
        }
        let prompt = delegated_handoff_prompt(&run, child_dispatch_run, instruction);
        let worker_semantic_digest =
            delegated_worker_semantic_digest(worker_agent_id, session_name, &prompt);
        let mut task = task_node(
            &run,
            task_id,
            Some(parent_task_id),
            BTreeSet::new(),
            prompt.clone(),
            NO_ARTIFACT_CONTRACT,
        );
        task.instruction_digest = delegated_task_digest(child_dispatch_run);
        task.promotion_reserved_at = Some(now);
        task.promotion_parent_dispatch_run = Some(parent_dispatch_run);
        task.promotion_worker_session_id = worker_session_id;
        task.promotion_worker_profile_id = worker_profile_id;
        task.promotion_worker_agent_id = worker_agent_id;
        task.promotion_worker_semantic_digest = worker_semantic_digest;
        run = self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::AddTask { task },
        )?;
        Ok(Some(DelegatedDispatchReservation {
            run: run.query(),
            prompt,
        }))
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

    /// Generic Supervisor roots whose authenticated caller dispatch was
    /// durably reserved before the root provenance could be bound.
    ///
    /// # Errors
    /// Returns an error when a retained caller reservation is malformed.
    pub fn pending_caller_promotions(&self) -> Result<Vec<PendingCallerPromotion>> {
        let state = self.load_state()?;
        let mut pending = Vec::new();
        for (start_operation_id, reservation) in &state.starts {
            let Some(dispatch_operation_id) = reservation.caller_dispatch_run_id else {
                continue;
            };
            let Some(run) = self.supervisor.load(reservation.supervisor_run_id)? else {
                // The reservation precedes aggregate initialization. Keep it
                // for the exact start retry without blocking other recovery.
                continue;
            };
            if run.state.is_finished() {
                continue;
            }
            let root_id = TaskId::new("root")?;
            let Some(root) = run.tasks.get(&root_id) else {
                if run.state == SupervisorRunState::Planning {
                    continue;
                }
                anyhow::bail!("pending caller root task is missing");
            };
            if reservation.workspace_id != run.workspace_id {
                anyhow::bail!("pending caller root workspace fence is stale");
            }
            if run.provenance.contains_key(&root_id) {
                continue;
            }
            if root.parent_task_id.is_some()
                || root.required_artifact_contract != NO_ARTIFACT_CONTRACT
                || root.state != TaskState::Ready
                || root.generation != 1
            {
                anyhow::bail!("pending caller root reservation is malformed");
            }
            pending.push(PendingCallerPromotion {
                start_operation_id: start_operation_id.clone(),
                dispatch_operation_id: dispatch_operation_id.to_string(),
                workspace_id: run
                    .workspace_id
                    .context("pending caller root has no workspace authority")?,
                worker_session_id: reservation.worker_session_id,
                worker_agent_id: reservation
                    .worker_agent_id
                    .context("pending caller root has no Agent identity authority")?,
                worker_profile_id: reservation
                    .worker_profile_id
                    .clone()
                    .context("pending caller root has no Agent profile authority")?,
                worker_runtime_id: reservation
                    .worker_runtime_id
                    .context("pending caller root has no Agent runtime authority")?,
                worker_semantic_digest: reservation
                    .worker_semantic_digest
                    .clone()
                    .context("pending caller root has no Agent semantic authority")?,
            });
        }
        Ok(pending)
    }

    /// Pending delegated task reservations recoverable from their stable task
    /// IDs and daemon-only origin marker.
    ///
    /// # Errors
    /// Returns an error when retained supervisor state is malformed.
    pub fn pending_delegated_promotions(&self) -> Result<Vec<PendingDelegatedPromotion>> {
        let mut pending = Vec::new();
        for run in self.unfinished_runs()? {
            if run.state.is_finished() {
                continue;
            }
            for task in run.tasks.values().filter(|task| {
                task.assigned_dispatch_run.is_none()
                    && task.state == TaskState::Ready
                    && task.generation == 1
            }) {
                let Some(operation_id) = task.task_id.0.strip_prefix(DELEGATED_TASK_PREFIX) else {
                    continue;
                };
                let Ok(operation) = OperationId::parse(operation_id) else {
                    continue;
                };
                if !is_delegated_reservation(task, operation) {
                    continue;
                }
                pending.push(PendingDelegatedPromotion {
                    operation_id: operation_id.into(),
                    reserved_at: task.promotion_reserved_at.unwrap_or(run.created_at),
                    workspace_id: run
                        .workspace_id
                        .context("delegated promotion has no workspace authority")?,
                    worker_session_id: task.promotion_worker_session_id,
                    worker_agent_id: task.promotion_worker_agent_id,
                    worker_profile_id: task.promotion_worker_profile_id.clone(),
                    worker_semantic_digest: task.promotion_worker_semantic_digest.clone(),
                });
            }
        }
        Ok(pending)
    }

    /// Lists exact operation joins for Agents admitted before a terminal
    /// Supervisor task could bind provenance. A delegated task can be terminal
    /// while its parent run remains live, so retained task state is the stop
    /// authority rather than only the run's terminal state.
    ///
    /// # Errors
    /// Returns an error for malformed durable reservations, ambiguous operation
    /// ownership, or missing delegated parent provenance/promotion authority.
    pub fn pending_worker_stops(&self) -> Result<Vec<PendingWorkerStop>> {
        self.pending_worker_stops_for(None)
    }

    /// Restricts unbound stop recovery to one run for a synchronous human
    /// control response.
    ///
    /// # Errors
    /// Returns the same durable-state errors as [`Self::pending_worker_stops`].
    pub fn pending_worker_stops_for_run(
        &self,
        supervisor_run_id: SupervisorRunId,
    ) -> Result<Vec<PendingWorkerStop>> {
        self.pending_worker_stops_for(Some(supervisor_run_id))
    }

    #[allow(clippy::too_many_lines)] // One inventory pass must reconcile root and recursive child stop fences consistently.
    fn pending_worker_stops_for(
        &self,
        selected_run: Option<SupervisorRunId>,
    ) -> Result<Vec<PendingWorkerStop>> {
        let mut pending = Vec::new();
        let state = self.load_state()?;
        for (operation_id, reservation) in &state.starts {
            let Some(run) = self.supervisor.load(reservation.supervisor_run_id)? else {
                continue;
            };
            if (selected_run.is_some() && selected_run != Some(run.supervisor_run_id))
                || !matches!(
                    run.state,
                    SupervisorRunState::Cancelled | SupervisorRunState::Failed
                )
            {
                continue;
            }
            let Some(workspace_id) = run.workspace_id else {
                continue;
            };
            let root_id = TaskId::new("root")?;
            let Some(root) = run.tasks.get(&root_id) else {
                continue;
            };
            if run.provenance.contains_key(&root_id) {
                continue;
            }
            let (operation_id, requires_session) =
                if let Some(caller_dispatch) = reservation.caller_dispatch_run_id {
                    if root.required_artifact_contract != NO_ARTIFACT_CONTRACT {
                        anyhow::bail!("aborted caller root reservation is malformed");
                    }
                    (caller_dispatch, reservation.worker_session_id.is_some())
                } else {
                    if root.required_artifact_contract != GOAL_REVIEW_READY_ARTIFACT_CONTRACT {
                        continue;
                    }
                    (
                        OperationId::parse(operation_id)
                            .map_err(|_| anyhow::anyhow!("aborted Goal operation is invalid"))?,
                        false,
                    )
                };
            pending.push(PendingWorkerStop {
                operation_id,
                workspace_id,
                supervisor_run_id: run.supervisor_run_id,
                task_id: root_id,
                parent_task_id: None,
                parent_dispatch_run: None,
                generation: root.generation,
                requires_session,
                worker_session_id: reservation.worker_session_id,
                worker_agent_id: reservation.worker_agent_id,
                worker_runtime_id: reservation.worker_runtime_id,
                worker_profile_id: reservation.worker_profile_id.clone(),
                worker_semantic_digest: reservation.worker_semantic_digest.clone(),
            });
        }

        for run in self.supervisor.runs()? {
            if selected_run.is_some() && selected_run != Some(run.supervisor_run_id) {
                continue;
            }
            let Some(workspace_id) = run.workspace_id else {
                continue;
            };
            for task in run.tasks.values() {
                if task.assigned_dispatch_run.is_some()
                    || run.provenance.contains_key(&task.task_id)
                {
                    continue;
                }
                let Some(operation_id) = task.task_id.0.strip_prefix(DELEGATED_TASK_PREFIX) else {
                    continue;
                };
                let Ok(operation_id) = OperationId::parse(operation_id) else {
                    continue;
                };
                if !is_delegated_reservation(task, operation_id) {
                    continue;
                }
                if !task.state.terminal() {
                    continue;
                }
                let Some(parent_task_id) = task.parent_task_id.clone() else {
                    anyhow::bail!("terminal delegated reservation has no parent task");
                };
                if task.supervisor_run_id != run.supervisor_run_id
                    || task.generation != 1
                    || task.required_artifact_contract != NO_ARTIFACT_CONTRACT
                    || task.promotion_reserved_at.is_none()
                {
                    anyhow::bail!("terminal delegated reservation fence is stale");
                }
                let parent_dispatch_run = match task.promotion_parent_dispatch_run {
                    Some(reserved_parent) => {
                        run.tasks
                            .get(&parent_task_id)
                            .filter(|parent| parent.supervisor_run_id == run.supervisor_run_id)
                            .context("terminal delegated reservation parent task is missing")?;
                        reserved_parent
                    }
                    None => run
                        .provenance
                        .get(&parent_task_id)
                        .map(|parent| parent.dispatch_run_id)
                        .context("terminal delegated reservation parent provenance is missing")?,
                };
                pending.push(PendingWorkerStop {
                    operation_id,
                    workspace_id,
                    supervisor_run_id: run.supervisor_run_id,
                    task_id: task.task_id.clone(),
                    parent_task_id: Some(parent_task_id),
                    parent_dispatch_run: Some(parent_dispatch_run),
                    generation: task.generation,
                    requires_session: true,
                    worker_session_id: task.promotion_worker_session_id,
                    worker_agent_id: task.promotion_worker_agent_id,
                    worker_runtime_id: None,
                    worker_profile_id: task.promotion_worker_profile_id.clone(),
                    worker_semantic_digest: task.promotion_worker_semantic_digest.clone(),
                });
            }
        }

        pending.sort_by_key(PendingWorkerStop::operation_id);
        for pair in pending.windows(2) {
            if pair[0].operation_id == pair[1].operation_id && pair[0] != pair[1] {
                return Err(anyhow::Error::msg(AMBIGUOUS_STOP_RESERVATION));
            }
        }
        pending.dedup();
        Ok(pending)
    }

    /// Releases root-operation replay metadata only after Agent reconciliation
    /// stopped an aborted, unbound worker. Mere absence is not proof because
    /// Goal admission may still be waiting behind the Agent owner lock. Delegated
    /// operation identity remains encoded in its retained task and needs no
    /// separate scheduler reservation.
    ///
    /// # Errors
    /// Returns an error when a root candidate no longer matches the exact
    /// terminal run reservation or the durable state cannot be saved.
    pub fn acknowledge_pending_worker_stops(&self, stops: &[PendingWorkerStop]) -> Result<()> {
        let mut state = self.load_state()?;
        let mut changed = false;
        for stop in stops {
            if stop.parent_task_id.is_some() {
                continue;
            }
            let operation_id = stop.operation_id.to_string();
            let matching = state
                .starts
                .iter()
                .filter(|(start_operation, reservation)| {
                    reservation.supervisor_run_id == stop.supervisor_run_id
                        && (start_operation.as_str() == operation_id
                            || reservation.caller_dispatch_run_id == Some(stop.operation_id))
                })
                .map(|(start_operation, reservation)| {
                    (start_operation.clone(), reservation.clone())
                })
                .collect::<Vec<_>>();
            if matching.is_empty() {
                if state.expired_starts.contains(&operation_id) {
                    continue;
                }
                if state.starts.iter().any(|(start_operation, reservation)| {
                    start_operation == &operation_id
                        || reservation.caller_dispatch_run_id == Some(stop.operation_id)
                }) {
                    anyhow::bail!("aborted root operation changed run ownership");
                }
                anyhow::bail!("aborted root operation reservation disappeared");
            }
            if matching.len() != 1 {
                anyhow::bail!("aborted root operation has ambiguous reservations");
            }
            let (start_operation_id, reservation) = &matching[0];
            let Some(run) = self.supervisor.load(stop.supervisor_run_id)? else {
                anyhow::bail!("aborted root run disappeared");
            };
            if reservation.supervisor_run_id != stop.supervisor_run_id
                || !has_unbound_root_worker(&run, Some(reservation))
            {
                anyhow::bail!("aborted root worker stop acknowledgement is stale");
            }
            state.starts.remove(start_operation_id);
            state.expired_starts.insert(start_operation_id);
            state.expired_starts.insert(&operation_id);
            changed = true;
        }
        if changed {
            self.save_state(&state)?;
        }
        Ok(())
    }

    /// Lists completed contracted dispatches whose verification can be safely
    /// replayed after a daemon restart. The dispatch ID comes from persisted
    /// provenance; worker output is never used to select the task.
    ///
    /// # Errors
    /// Returns an error when retained supervisor or dispatch state is invalid.
    pub fn pending_artifact_verifications(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<PendingArtifactVerification>> {
        let mut pending = Vec::new();
        let completed = self
            .dispatch_runs()?
            .into_iter()
            .filter(|dispatch| dispatch.status == RunStatus::Completed)
            .map(|dispatch| dispatch.run_id)
            .collect::<BTreeSet<_>>();
        // This recovery lane is periodic. Hydrate one active aggregate at a
        // time so the 256-run admission bound cannot become a snapshot-sized
        // peak-memory multiplier.
        for id in self.supervisor.unfinished_run_ids()? {
            let run = self.load_indexed_run(id)?;
            if run.state != SupervisorRunState::Running {
                continue;
            }
            for task in run.tasks.values().filter(|task| {
                task.required_artifact_contract == GOAL_REVIEW_READY_ARTIFACT_CONTRACT
                    && task
                        .verification_retry_at
                        .is_none_or(|retry_at| retry_at <= now)
                    && matches!(
                        task.state,
                        TaskState::Dispatched | TaskState::Running | TaskState::Verifying
                    )
            }) {
                let provenance = run.provenance.get(&task.task_id).ok_or_else(|| {
                    anyhow::anyhow!("contracted supervisor task provenance is missing")
                })?;
                if provenance.generation != task.generation
                    || task.assigned_dispatch_run != Some(provenance.dispatch_run_id)
                {
                    anyhow::bail!("contracted supervisor task provenance fence is stale");
                }
                if completed.contains(&provenance.dispatch_run_id) {
                    pending.push(PendingArtifactVerification {
                        dispatch_run_id: provenance.dispatch_run_id,
                    });
                }
            }
        }
        pending.sort_by_key(|item| item.dispatch_run_id.to_string());
        pending.dedup();
        Ok(pending)
    }

    /// Marks a reserved delegated task failed after a definite spawn failure.
    ///
    /// # Errors
    /// Returns an error when no exact reservation exists or reducer state is
    /// inconsistent.
    pub fn fail_reserved_delegated_dispatch(
        &self,
        child_operation_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<SupervisorRunQuery>> {
        let operation = OperationId::parse(child_operation_id)
            .map_err(|_| anyhow::anyhow!("delegated dispatch operation is invalid"))?;
        let task_id = delegated_task_id(operation)?;
        let mut matches = self.unfinished_runs()?.into_iter().filter_map(|run| {
            let task = run.tasks.get(&task_id)?.clone();
            is_delegated_reservation(&task, operation).then_some((run, task))
        });
        let Some((mut run, task)) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            anyhow::bail!("delegated dispatch belongs to multiple supervisor runs");
        }
        if task.state.terminal() {
            return Ok(Some(run.query()));
        }
        run = self.resume_pending_promotion_escalation(run, &task_id, now)?;
        let run = self.apply(
            &run,
            now,
            SupervisorEventSource::DispatchFailure,
            SupervisorEventKind::Cancel {
                task_id: Some(task_id),
                reason: "delegated Agent admission failed before dispatch".into(),
            },
        )?;
        Ok(Some(run.query()))
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
                now,
            )?
            .is_none()
        {
            return Ok(None);
        }
        self.bind_reserved_delegated_dispatch(child_operation_id, worker, now)
    }

    /// Binds one admitted child Agent using only its exact durable reservation.
    /// Parent provenance and instruction remain single-sourced in the DAG.
    ///
    /// # Errors
    /// Returns an error for missing dispatch state, conflicting provenance,
    /// cross-workspace ownership, or reducer persistence failure.
    #[allow(clippy::too_many_lines)] // Binding validates the complete immutable reservation before provenance is committed.
    pub fn bind_reserved_delegated_dispatch(
        &self,
        child_operation_id: &str,
        worker: &AgentRuntimeRef,
        now: DateTime<Utc>,
    ) -> Result<Option<SupervisorRunQuery>> {
        let child_dispatch_run = OperationId::parse(child_operation_id)
            .context("delegated dispatch operation is invalid")?;
        let child_dispatch = self
            .dispatch
            .run(child_dispatch_run)?
            .context("delegated dispatch does not exist")?;
        let task_id = delegated_task_id(child_dispatch_run)?;
        let mut matches = self.unfinished_runs()?.into_iter().filter_map(|run| {
            let task = run.tasks.get(&task_id)?.clone();
            is_delegated_reservation(&task, child_dispatch_run).then_some((run, task))
        });
        let Some((mut run, task)) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            anyhow::bail!("delegated dispatch belongs to multiple supervisor runs");
        }
        let child_agent = if task.promotion_worker_session_id.is_some()
            || task.promotion_worker_profile_id.is_some()
            || task.promotion_worker_agent_id.is_some()
            || task.promotion_worker_semantic_digest.is_some()
        {
            Some(
                self.dispatch
                    .agent_in_workspace(worker.terminal.workspace_id, child_dispatch.agent_id)?
                    .context("delegated dispatch Agent does not exist")?,
            )
        } else {
            None
        };
        let child_semantic_digest = if task.promotion_worker_semantic_digest.is_some() {
            Some(usagi_core::infrastructure::ipc::agent_operation_digest(
                &self
                    .dispatch
                    .admission(child_dispatch_run)?
                    .context("delegated dispatch admission does not exist")?
                    .semantic_key,
            ))
        } else {
            None
        };
        if !delegated_worker_matches_reservation(
            run.workspace_id,
            worker,
            child_agent.as_ref(),
            &task,
            &child_dispatch,
            child_semantic_digest.as_ref(),
        ) {
            anyhow::bail!("delegated worker is outside its reserved supervisor scope");
        }
        let parent_task_id = task
            .parent_task_id
            .clone()
            .context("delegated supervisor task has no parent")?;
        let parent_dispatch_run = if let Some(reserved) = task.promotion_parent_dispatch_run {
            run.tasks
                .get(&parent_task_id)
                .filter(|parent| parent.supervisor_run_id == run.supervisor_run_id)
                .context("delegated parent task is missing")?;
            reserved
        } else {
            let parent = run
                .provenance
                .get(&parent_task_id)
                .context("delegated parent provenance is missing")?;
            validate_provenance_chain(&run, &parent_task_id, parent)?;
            parent.dispatch_run_id
        };
        let state = self.load_state()?;
        let authority = live_task_dispatch_authority(&state, &run, &task_id, &mut BTreeSet::new())?;
        let authority = authority.context("delegated dispatch promotion authority is missing")?;
        if authority.operation_id != child_dispatch_run {
            anyhow::bail!("delegated dispatch promotion fence is stale");
        }
        run = self.resume_pending_promotion_escalation(run, &task_id, now)?;
        let provenance = RunProvenance {
            supervisor_run_id: run.supervisor_run_id,
            task_id: task_id.clone(),
            parent_task_id: Some(parent_task_id),
            parent_dispatch_run: Some(parent_dispatch_run),
            dispatch_run_id: child_dispatch_run,
            worker_session_id: worker.session_id,
            worker_agent_id: worker.agent_runtime_id,
            worker_worktree_id: worker.terminal.worktree_id,
            generation: task.generation,
        };
        if let Some(existing) = run.provenance.get(&task_id) {
            if existing == &provenance {
                return Ok(Some(run.query()));
            }
            anyhow::bail!("delegated dispatch provenance conflicts with the existing task");
        }
        let generation = task.generation;
        run = self.apply(
            &run,
            now,
            SupervisorEventSource::Admission,
            SupervisorEventKind::Dispatch {
                task_id,
                generation,
                provenance,
            },
        )?;
        Ok(Some(run.query()))
    }

    /// Moves a completed contracted dispatch to `Verifying` and captures its
    /// committed structured result under an exact task generation fence.
    ///
    /// # Errors
    /// Returns an error when dispatch/supervisor durable state is inconsistent.
    pub fn prepare_artifact_verification(
        &self,
        dispatch_run_id: OperationId,
        now: DateTime<Utc>,
    ) -> Result<Option<ArtifactVerificationRequest>> {
        self.prepare_artifact_verification_with_trigger(
            dispatch_run_id,
            now,
            ArtifactReportTrigger::Recovery,
        )
    }

    /// Prepares verification after the exact worker explicitly reported again.
    /// This is the only path which can leave artifact-rework waiting state.
    ///
    /// # Errors
    /// Returns an error when dispatch, candidate, or supervisor provenance is
    /// inconsistent or cannot be persisted.
    pub fn prepare_artifact_verification_after_report(
        &self,
        dispatch_run_id: OperationId,
        result: Option<StructuredResult>,
        now: DateTime<Utc>,
    ) -> Result<Option<ArtifactVerificationRequest>> {
        self.prepare_artifact_verification_with_trigger(
            dispatch_run_id,
            now,
            ArtifactReportTrigger::Fresh(result),
        )
    }

    fn prepare_artifact_verification_with_trigger(
        &self,
        dispatch_run_id: OperationId,
        now: DateTime<Utc>,
        trigger: ArtifactReportTrigger,
    ) -> Result<Option<ArtifactVerificationRequest>> {
        let fresh_report = matches!(&trigger, ArtifactReportTrigger::Fresh(_));
        let mut found = self.unfinished_runs()?.into_iter().filter_map(|run| {
            let task = run
                .provenance
                .iter()
                .find(|(_, provenance)| provenance.dispatch_run_id == dispatch_run_id)
                .map(|(task, _)| task.clone());
            task.map(|task| (run, task))
        });
        let Some((mut run, task_id)) = found.next() else {
            return Ok(None);
        };
        if found.next().is_some() {
            anyhow::bail!("dispatch belongs to multiple supervisor runs");
        }
        let task = run
            .tasks
            .get(&task_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor task is missing"))?;
        if task.required_artifact_contract != GOAL_REVIEW_READY_ARTIFACT_CONTRACT
            || run.state != SupervisorRunState::Running
        {
            return Ok(None);
        }
        let task_state = task.state;
        let generation = task.generation;
        let verification_attempt = task.verification_attempt;
        let previous_verification_digest = task.verification_digest.clone();
        let expectation = task.verification_expectation.clone();
        let contract = task.required_artifact_contract;
        let workspace_id = run
            .workspace_id
            .ok_or_else(|| anyhow::anyhow!("artifact run workspace is missing"))?;
        if !fresh_report
            && task
                .verification_retry_at
                .is_some_and(|retry_at| retry_at > now)
        {
            return Ok(None);
        }
        let dispatch = self
            .dispatch
            .run(dispatch_run_id)?
            .ok_or_else(|| anyhow::anyhow!("artifact dispatch is missing"))?;
        if dispatch.status != RunStatus::Completed {
            return Ok(None);
        }
        if task.state == TaskState::Dispatched {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::DispatchCompletion,
                SupervisorEventKind::Running {
                    task_id: task_id.clone(),
                    generation,
                },
            )?;
        }
        if matches!(task_state, TaskState::Dispatched | TaskState::Running)
            || (fresh_report && task_state == TaskState::AwaitingDecision)
        {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::DispatchCompletion,
                SupervisorEventKind::SetTaskState {
                    task_id: task_id.clone(),
                    generation,
                    state: TaskState::Succeeded,
                },
            )?;
        } else if task_state != TaskState::Verifying {
            return Ok(None);
        }
        let Some(repository) = run.artifact_repository.clone() else {
            self.reject_missing_artifact_repository(&run, task_id, generation, now)?;
            return Ok(None);
        };
        let (run, result) = self.prepare_artifact_candidate(
            run,
            &task_id,
            generation,
            dispatch_run_id,
            trigger,
            now,
        )?;
        let worktrees = artifact_worktrees(&run);
        Ok(Some(ArtifactVerificationRequest {
            supervisor_run_id: run.supervisor_run_id,
            task_id,
            generation,
            verification_attempt,
            previous_verification_digest,
            workspace_id,
            contract,
            repository,
            result,
            expectation,
            worktrees,
        }))
    }

    fn prepare_artifact_candidate(
        &self,
        mut run: SupervisorRun,
        task_id: &TaskId,
        generation: u64,
        dispatch_run_id: OperationId,
        trigger: ArtifactReportTrigger,
        now: DateTime<Utc>,
    ) -> Result<(SupervisorRun, Option<StructuredResult>)> {
        let (fresh, reported_result) = match trigger {
            ArtifactReportTrigger::Fresh(result) => (true, result),
            ArtifactReportTrigger::Recovery => {
                let result = match self.dispatch.binding(dispatch_run_id)? {
                    Some(binding) => self
                        .dispatch
                        .inbox(&binding.caller)?
                        .into_iter()
                        .find(|message| message.run_id == dispatch_run_id)
                        .and_then(|message| message.result),
                    None => None,
                };
                (false, result)
            }
        };
        let candidate_pr = reported_result
            .as_ref()
            .and_then(|result| result.pr.as_deref())
            .and_then(canonicalize)
            .map(|identity| identity.as_url().to_owned());
        if fresh || !run.verification_candidates.contains_key(task_id) {
            run = self.apply(
                &run,
                now,
                SupervisorEventSource::Verification,
                SupervisorEventKind::VerificationCandidateRecorded {
                    task_id: task_id.clone(),
                    generation,
                    candidate_pr,
                },
            )?;
        }
        let mut result = reported_result.unwrap_or_default();
        result.pr = run.verification_candidates.get(task_id).cloned().flatten();
        Ok((run, Some(result)))
    }

    fn reject_missing_artifact_repository(
        &self,
        run: &SupervisorRun,
        task_id: TaskId,
        generation: u64,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.apply(
            run,
            now,
            SupervisorEventSource::Verification,
            SupervisorEventKind::VerificationResult {
                task_id,
                generation,
                passed: false,
                result_digest: "missing-pre-spawn-repository".into(),
                safe_summary: "artifact repository was not recorded before Goal worker spawn"
                    .into(),
            },
        )
        .map(drop)
    }

    /// Durably pins trusted Git facts before the provider is queried. Replays
    /// with the same expectation are idempotent; a changed expectation is a
    /// provenance violation.
    ///
    /// # Errors
    /// Returns an error when the request fence is stale or persistence fails.
    pub fn record_artifact_expectation(
        &self,
        request: &ArtifactVerificationRequest,
        expectation: &ArtifactExpectation,
        now: DateTime<Utc>,
    ) -> Result<ArtifactVerificationRequest> {
        let run = self.load_started_run(request.supervisor_run_id)?;
        let task = run
            .tasks
            .get(&request.task_id)
            .ok_or_else(|| anyhow::anyhow!("artifact verification task is missing"))?;
        if task.generation != request.generation
            || task.required_artifact_contract != request.contract
            || task.state != TaskState::Verifying
            || task.verification_attempt != request.verification_attempt
            || run.artifact_repository.as_ref() != Some(&request.repository)
            || expectation.repository() != &request.repository
        {
            anyhow::bail!("artifact expectation fence is stale");
        }
        let run = if task.verification_expectation.as_ref() == Some(expectation) {
            run
        } else {
            self.apply(
                &run,
                now,
                SupervisorEventSource::Verification,
                SupervisorEventKind::VerificationExpectationRecorded {
                    task_id: request.task_id.clone(),
                    generation: request.generation,
                    expectation: expectation.clone(),
                },
            )?
        };
        let mut pinned = request.clone();
        pinned
            .expectation
            .clone_from(&run.tasks[&request.task_id].verification_expectation);
        Ok(pinned)
    }

    /// Commits one independently obtained verification result and finalizes the
    /// run only when every tracked task has succeeded.
    ///
    /// # Errors
    /// Returns an error when the request fence is stale or durable state cannot
    /// be updated.
    pub fn record_artifact_verification(
        &self,
        request: &ArtifactVerificationRequest,
        verification: ArtifactVerification,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
        bounded_nonempty(
            "artifact verification digest",
            &verification.result_digest,
            MAX_SUPERVISOR_KEY_BYTES,
        )?;
        if verification.safe_summary.len() > MAX_SUPERVISOR_TEXT_BYTES {
            anyhow::bail!(
                "invalid artifact verification summary: maximum is {MAX_SUPERVISOR_TEXT_BYTES} UTF-8 bytes"
            );
        }
        let run = self.load_started_run(request.supervisor_run_id)?;
        let task = run
            .tasks
            .get(&request.task_id)
            .ok_or_else(|| anyhow::anyhow!("artifact verification task is missing"))?;
        if task.generation == request.generation
            && task.required_artifact_contract == request.contract
            && (run.state != SupervisorRunState::Running || task.state.terminal())
        {
            return Ok(run.query());
        }
        if task.generation != request.generation
            || task.required_artifact_contract != request.contract
            || run.artifact_repository.as_ref() != Some(&request.repository)
            || task.state != TaskState::Verifying
        {
            anyhow::bail!("artifact verification fence is stale");
        }
        if task.verification_attempt != request.verification_attempt {
            return if task.verification_attempt > request.verification_attempt {
                Ok(run.query())
            } else {
                anyhow::bail!("artifact verification attempt fence is stale")
            };
        }
        if task.verification_expectation != request.expectation {
            anyhow::bail!("artifact verification expectation fence is stale");
        }
        if verification.status == ArtifactVerificationStatus::Verified
            && request.expectation.is_none()
        {
            anyhow::bail!("verified artifact expectation is missing");
        }
        let kind = match verification.status {
            ArtifactVerificationStatus::Verified => SupervisorEventKind::VerificationResult {
                task_id: request.task_id.clone(),
                generation: request.generation,
                passed: true,
                result_digest: verification.result_digest,
                safe_summary: verification.safe_summary,
            },
            ArtifactVerificationStatus::Rejected => SupervisorEventKind::VerificationResult {
                task_id: request.task_id.clone(),
                generation: request.generation,
                passed: false,
                result_digest: verification.result_digest,
                safe_summary: verification.safe_summary,
            },
            ArtifactVerificationStatus::Retryable => {
                let exponent = request.verification_attempt.min(30);
                let delay = ARTIFACT_RETRY_BASE_SECONDS
                    .saturating_mul(1_i64 << exponent)
                    .min(ARTIFACT_RETRY_MAX_SECONDS);
                SupervisorEventKind::VerificationDeferred {
                    task_id: request.task_id.clone(),
                    generation: request.generation,
                    result_digest: verification.result_digest,
                    safe_summary: verification.safe_summary,
                    retry_at: now + chrono::Duration::seconds(delay),
                }
            }
        };
        let run = self.apply(&run, now, SupervisorEventSource::Verification, kind)?;
        Ok(self.finalize_terminal_tasks(run, now)?.query())
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn start_scoped(
        &self,
        caller: &str,
        workspace: Option<WorkspaceId>,
        operation_id: &str,
        root_task: String,
        root_artifact_contract: ArtifactContract,
        artifact_repository: Option<GitHubRepository>,
        worker_profile_id: Option<AgentProfileId>,
        worker_semantic_digest: Option<String>,
        caller_dispatch: Option<&CallerDispatchReservation>,
        initial_tasks: Vec<InitialTask>,
        policy_selector: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRunQuery> {
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

    /// Returns exact worker provenance still owned by aborted runs. Agent
    /// records are the termination authority; this list is the durable join
    /// from a terminal Supervisor fact to those exact runtime identities.
    ///
    /// # Errors
    /// Returns an error when durable state is corrupt or provenance no longer
    /// fences the task generation and dispatch recorded by its run.
    pub fn worker_stop_obligations(&self) -> Result<Vec<(WorkspaceId, RunProvenance)>> {
        self.worker_stop_obligations_for(None)
    }

    /// Restricts bound stop recovery to one run for a synchronous control.
    ///
    /// # Errors
    /// Returns the same durable-state errors as [`Self::worker_stop_obligations`].
    pub fn worker_stop_obligations_for_run(
        &self,
        supervisor_run_id: SupervisorRunId,
    ) -> Result<Vec<(WorkspaceId, RunProvenance)>> {
        self.worker_stop_obligations_for(Some(supervisor_run_id))
    }

    fn worker_stop_obligations_for(
        &self,
        selected_run: Option<SupervisorRunId>,
    ) -> Result<Vec<(WorkspaceId, RunProvenance)>> {
        let mut obligations = Vec::new();
        for run in self.aborted_runs()? {
            if (selected_run.is_some() && selected_run != Some(run.supervisor_run_id))
                || !matches!(
                    run.state,
                    SupervisorRunState::Cancelled | SupervisorRunState::Failed
                )
            {
                continue;
            }
            let Some(workspace) = run.workspace_id else {
                // Legacy unscoped history cannot authorize a process signal.
                // It is intentionally invisible to the workspace control plane.
                continue;
            };
            for (task_id, provenance) in &run.provenance {
                let Some(task) = run.tasks.get(task_id) else {
                    anyhow::bail!("aborted supervisor provenance task is missing");
                };
                if task.assigned_dispatch_run.is_none() && provenance.generation < task.generation {
                    // A completed earlier attempt is historical provenance,
                    // not a worker still owned by the cancelled generation.
                    continue;
                }
                if provenance.supervisor_run_id != run.supervisor_run_id
                    || provenance.task_id != *task_id
                    || provenance.generation != task.generation
                    || task.assigned_dispatch_run != Some(provenance.dispatch_run_id)
                {
                    anyhow::bail!("aborted supervisor worker provenance fence is stale");
                }
                obligations.push((workspace, provenance.clone()));
            }
        }
        obligations.sort_by_key(|(workspace, provenance)| {
            (
                workspace.to_string(),
                provenance.worker_agent_id.as_str().clone(),
            )
        });
        obligations.dedup();
        Ok(obligations)
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

    fn finalize_terminal_tasks(
        &self,
        run: SupervisorRun,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRun> {
        if run.state != SupervisorRunState::Running
            || run.tasks.is_empty()
            || !run.tasks.values().all(|task| task.state.terminal())
        {
            return Ok(run);
        }
        let succeeded = run
            .tasks
            .values()
            .all(|task| task.state == TaskState::Succeeded);
        let (source, terminal_reason) = if succeeded {
            (SupervisorEventSource::DispatchCompletion, None)
        } else if run
            .tasks
            .values()
            .any(|task| task.state == TaskState::Failed)
        {
            (
                SupervisorEventSource::DispatchFailure,
                Some("one or more supervisor tasks failed".into()),
            )
        } else if run
            .tasks
            .values()
            .any(|task| task.state == TaskState::Blocked)
        {
            (
                SupervisorEventSource::DispatchFailure,
                Some("one or more supervisor tasks were blocked".into()),
            )
        } else {
            (
                SupervisorEventSource::Cancel,
                Some("one or more supervisor tasks were cancelled".into()),
            )
        };
        self.apply(
            &run,
            now,
            source,
            SupervisorEventKind::SetRunState {
                state: if succeeded {
                    SupervisorRunState::Succeeded
                } else {
                    SupervisorRunState::Failed
                },
                terminal_reason,
            },
        )
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
    fn reserve_parent_wake(
        &self,
        run: &mut usagi_core::domain::supervisor::SupervisorRun,
        parent_id: &TaskId,
        child_run: OperationId,
        kind: InboxKind,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let parent = run.tasks.get(parent_id).cloned().expect("parent exists");
        let key = format!("{}:{}:{}", child_run, parent_id.0, parent.generation);
        let mut state = self.load_state()?;
        if state.wakes.contains_key(&key) || state.expired_wakes.contains(&key) {
            return Ok(());
        }
        if parent.state == TaskState::Running {
            let event = SupervisorEventKind::SetTaskState {
                task_id: parent_id.clone(),
                generation: parent.generation,
                state: TaskState::AwaitingDecision,
            };
            *run = self.apply(run, now, SupervisorEventSource::DispatchCompletion, event)?;
        }
        let parent = run.tasks.get(parent_id).expect("parent retained");
        if parent.state != TaskState::AwaitingDecision {
            return Ok(());
        }
        let Some(parent_provenance) = run.provenance.get(parent_id).cloned() else {
            return Ok(());
        };
        let outcome = self.outcome(child_run, kind)?;
        state.compact_delivered_wakes();
        if state.wakes.len() >= MAX_WAKE_RESERVATIONS {
            anyhow::bail!("supervisor wake reservation capacity is exhausted");
        }
        state.wakes.insert(
            key,
            WakeReservation {
                wake: DecisionWake {
                    supervisor_run_id: run.supervisor_run_id,
                    parent_task_id: parent_id.clone(),
                    parent_generation: parent.generation,
                    parent: parent_provenance,
                    child_run_id: child_run,
                    outcome,
                    dag: run
                        .tasks
                        .iter()
                        .map(|(id, task)| (id.clone(), task.state))
                        .collect(),
                    remaining_budget_summary: "policy has not configured a budget".into(),
                },
                delivered: false,
            },
        );
        self.save_state(&state)
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
    fn handoff_entry(
        &self,
        task_id: &TaskId,
        generation: u64,
        dispatch_run_id: OperationId,
        fallback: InboxKind,
        recorded_at: DateTime<Utc>,
    ) -> Result<HandoffContextEntry> {
        let message = self.dispatch.binding(dispatch_run_id)?.and_then(|binding| {
            self.dispatch
                .inbox(&binding.caller)
                .ok()
                .and_then(|messages| {
                    messages
                        .into_iter()
                        .find(|message| message.run_id == dispatch_run_id)
                })
        });
        let message = message.filter(|message| message.kind == fallback);
        let summary = message.as_ref().map_or_else(
            || "worker terminal state committed without an inbox report".to_owned(),
            |message| compact_handoff_text(&message.summary, MAX_HANDOFF_SUMMARY_BYTES),
        );
        let artifacts = message
            .as_ref()
            .and_then(|message| message.result.as_ref())
            .and_then(structured_artifact_summary);
        Ok(HandoffContextEntry {
            task_id: task_id.clone(),
            generation,
            dispatch_run_id,
            outcome: fallback,
            summary,
            artifacts,
            recorded_at,
        })
    }
    fn record_terminal_handoff(
        &self,
        run: SupervisorRun,
        task_id: &TaskId,
        provenance: &RunProvenance,
        kind: InboxKind,
        now: DateTime<Utc>,
    ) -> Result<SupervisorRun> {
        let current = run.tasks.get(task_id).expect("task retained");
        let captures_handoff = current.state == TaskState::Succeeded
            || current.state == TaskState::Failed
            || current.state == TaskState::Verifying;
        if !captures_handoff
            || current.generation != provenance.generation
            || current.assigned_dispatch_run != Some(provenance.dispatch_run_id)
            || run
                .handoff_context
                .iter()
                .any(|entry| entry.dispatch_run_id == provenance.dispatch_run_id)
        {
            return Ok(run);
        }
        self.handoff_entry(
            task_id,
            current.generation,
            provenance.dispatch_run_id,
            kind,
            now,
        )
        .and_then(|entry| {
            self.apply(
                &run,
                now,
                source(kind),
                SupervisorEventKind::RecordHandoff { entry },
            )
        })
    }
    fn deliver_reserved(&self, now: DateTime<Utc>, waker: &mut dyn DecisionWaker) -> Result<()> {
        let mut state = self.load_state()?;
        let mut changed = false;
        let mut first_failure = None;
        let pending = state
            .wakes
            .iter()
            .filter(|(_, item)| !item.delivered)
            .map(|(key, item)| (key.clone(), item.wake.clone()))
            .collect::<Vec<_>>();
        for (key, wake) in pending {
            let delivered = waker
                .wake(&wake)
                .and_then(|()| self.resume_parent_after_wake(&wake, now));
            match delivered {
                Ok(()) => {
                    if let Some(reservation) = state.wakes.get_mut(&key) {
                        reservation.delivered = true;
                        changed = true;
                    }
                }
                Err(error) => {
                    first_failure.get_or_insert(error);
                }
            }
        }
        if changed {
            state.compact_delivered_wakes();
            self.save_state(&state)?;
        }
        first_failure.map_or(Ok(()), Err)
    }

    fn resume_parent_after_wake(&self, wake: &DecisionWake, now: DateTime<Utc>) -> Result<()> {
        let Some(run) = self.supervisor.load(wake.supervisor_run_id)? else {
            anyhow::bail!("supervisor parent wake run is unavailable");
        };
        let task = run
            .tasks
            .get(&wake.parent_task_id)
            .ok_or_else(|| anyhow::anyhow!("supervisor parent wake task is unavailable"))?;
        if task.generation != wake.parent_generation
            || run.provenance.get(&wake.parent_task_id) != Some(&wake.parent)
        {
            anyhow::bail!("supervisor parent wake fence is stale");
        }
        if task.state == TaskState::AwaitingDecision {
            self.apply(
                &run,
                now,
                SupervisorEventSource::Admission,
                SupervisorEventKind::SetTaskState {
                    task_id: wake.parent_task_id.clone(),
                    generation: wake.parent_generation,
                    state: TaskState::Running,
                },
            )?;
        } else if task.state != TaskState::Running && !task.state.terminal() {
            anyhow::bail!("supervisor parent wake task is not resumable");
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::{BTreeMap, BTreeSet};
    use usagi_core::domain::{
        agent::{
            Agent, AgentProfileId, AgentStatus, CallerRef, DispatchBinding, DispatchRun,
            InboxMessage, ModelSelector, WorkerRef,
        },
        id::{
            AgentId, AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, SessionId, TerminalId,
            TerminalRef, WorktreeId,
        },
        pr_inventory::GitHubRepository,
        supervisor::{
            ArtifactExpectation, EscalationRecord, MAX_HANDOFF_CONTEXT_ENTRIES, SupervisorRun,
            TaskNode,
        },
    };
    use usagi_core::infrastructure::store::dispatch::{
        AgentAdmissionReservation, CredentialProvenance,
    };

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap()
    }
    fn artifact_repository() -> GitHubRepository {
        GitHubRepository::from_name_with_owner("acme/repo").unwrap()
    }
    fn artifact_expectation() -> ArtifactExpectation {
        ArtifactExpectation::new(
            artifact_repository(),
            "0123456789012345678901234567890123456789",
        )
        .unwrap()
    }
    fn goal(instruction: &str) -> GoalSpecification {
        GoalSpecification::new(instruction.into(), artifact_repository())
    }
    fn root_worker(workspace: WorkspaceId) -> AgentRuntimeRef {
        AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: workspace,
                session_id: None,
                worktree_id: WorktreeId::new(),
            },
            None,
        )
        .unwrap()
    }
    fn delegated_worker(workspace: WorkspaceId) -> AgentRuntimeRef {
        let session = SessionId::new();
        AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id: workspace,
                session_id: Some(session),
                worktree_id: WorktreeId::new(),
            },
            Some(session),
        )
        .unwrap()
    }
    fn persist_caller_dispatch(
        scheduler: &SupervisorRuntime,
        workspace: WorkspaceId,
        operation: OperationId,
        worker: &AgentRuntimeRef,
    ) {
        let agent_id = AgentId::new();
        let agent = Agent {
            agent_id,
            session_id: worker.session_id,
            runtime: AgentProfileId::new("claude").unwrap(),
            model: ModelSelector::new("default").unwrap(),
            status: AgentStatus::Running,
            current_run: Some(operation),
        };
        scheduler
            .dispatch
            .reserve_admission_for_workspace(
                workspace,
                agent,
                DispatchRun {
                    run_id: operation,
                    agent_id,
                    prompt: "caller".into(),
                    started_at: now(),
                    ended_at: None,
                    status: RunStatus::Running,
                },
                DispatchBinding {
                    run_id: operation,
                    caller: CallerRef {
                        session_id: worker.session_id,
                        agent_id,
                    },
                    worker: WorkerRef {
                        session_id: worker.session_id,
                        agent_id,
                    },
                },
                AgentAdmissionReservation {
                    operation_id: operation,
                    semantic_key: "caller-semantic".into(),
                    credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
                },
            )
            .unwrap();
    }
    fn persist_root_dispatch_agent(
        scheduler: &SupervisorRuntime,
        workspace: WorkspaceId,
        operation: OperationId,
    ) {
        let run = scheduler.dispatch.run(operation).unwrap().unwrap();
        scheduler
            .dispatch
            .upsert_agent(
                workspace,
                Agent {
                    agent_id: run.agent_id,
                    session_id: None,
                    runtime: AgentProfileId::new("claude").unwrap(),
                    model: ModelSelector::new("default").unwrap(),
                    status: AgentStatus::Running,
                    current_run: Some(operation),
                },
            )
            .unwrap();
    }
    fn task(run: SupervisorRunId, id: &str, parent: Option<&str>) -> TaskNode {
        TaskNode {
            task_id: TaskId::new(id).unwrap(),
            supervisor_run_id: run,
            parent_task_id: parent.map(|id| TaskId::new(id).unwrap()),
            dependencies: BTreeSet::new(),
            instruction_digest: id.into(),
            instruction_body: id.into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
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
    fn aborted_run(workspace: Option<WorkspaceId>) -> SupervisorRun {
        let mut run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        run.workspace_id = workspace;
        run.state = SupervisorRunState::Cancelled;
        run.terminal_at = Some(now());
        run.terminal_reason = Some("operator cancelled".into());
        run
    }
    fn unbound_goal_run(workspace: Option<WorkspaceId>) -> SupervisorRun {
        let mut run = aborted_run(workspace);
        let mut root = task(run.supervisor_run_id, "root", None);
        root.required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
        run.tasks.insert(root.task_id.clone(), root);
        run
    }
    fn start_reservation(supervisor_run_id: SupervisorRunId) -> StartReservation {
        StartReservation {
            semantic_key: semantic_digest(b"test"),
            supervisor_run_id,
            artifact_repository: None,
            workspace_id: None,
            caller_dispatch_run_id: None,
            worker_session_id: None,
            worker_agent_id: None,
            worker_runtime_id: None,
            worker_profile_id: None,
            worker_semantic_digest: None,
        }
    }
    fn caller_start_reservation(
        supervisor_run_id: SupervisorRunId,
        workspace_id: WorkspaceId,
        dispatch_run_id: OperationId,
    ) -> StartReservation {
        StartReservation {
            semantic_key: semantic_digest(b"caller"),
            supervisor_run_id,
            artifact_repository: None,
            workspace_id: Some(workspace_id),
            caller_dispatch_run_id: Some(dispatch_run_id),
            worker_session_id: None,
            worker_agent_id: Some(AgentId::new()),
            worker_runtime_id: Some(AgentRuntimeId::new()),
            worker_profile_id: Some(AgentProfileId::new("claude").unwrap()),
            worker_semantic_digest: Some(semantic_digest(b"agent")),
        }
    }
    fn root_pending_stop(
        operation_id: OperationId,
        workspace_id: WorkspaceId,
        supervisor_run_id: SupervisorRunId,
    ) -> PendingWorkerStop {
        PendingWorkerStop {
            operation_id,
            workspace_id,
            supervisor_run_id,
            task_id: TaskId::new("root").unwrap(),
            parent_task_id: None,
            parent_dispatch_run: None,
            generation: 1,
            requires_session: false,
            worker_session_id: None,
            worker_agent_id: None,
            worker_runtime_id: None,
            worker_profile_id: None,
            worker_semantic_digest: None,
        }
    }
    fn event(run: &SupervisorRun, kind: SupervisorEventKind) -> SupervisorEvent {
        SupervisorEvent {
            sequence: run.state_revision + 1,
            event_id: OperationId::new(),
            causation_id: None,
            correlation_id: None,
            observed_at: now(),
            payload_digest: "test".into(),
            source: SupervisorEventSource::Admission,
            kind,
        }
    }
    fn provenance(
        run: SupervisorRunId,
        task: &TaskId,
        parent: Option<(&TaskId, OperationId)>,
        dispatch: OperationId,
    ) -> RunProvenance {
        RunProvenance {
            supervisor_run_id: run,
            task_id: task.clone(),
            parent_task_id: parent.as_ref().map(|(id, _)| (*id).clone()),
            parent_dispatch_run: parent.map(|(_, id)| id),
            dispatch_run_id: dispatch,
            worker_session_id: Some(SessionId::new()),
            worker_agent_id: AgentRuntimeId::new(),
            worker_worktree_id: WorktreeId::new(),
            generation: 1,
        }
    }
    #[derive(Default)]
    struct Waker {
        wakes: Vec<DecisionWake>,
    }
    impl DecisionWaker for Waker {
        fn wake(&mut self, wake: &DecisionWake) -> Result<()> {
            self.wakes.push(wake.clone());
            Ok(())
        }
    }

    #[test]
    fn pending_worker_stop_projects_and_checks_every_exact_worker_fence() {
        let workspace = WorkspaceId::new();
        let worker = delegated_worker(workspace);
        let profile = AgentProfileId::new("claude").unwrap();
        let agent_id = AgentId::new();
        let mut pending = root_pending_stop(OperationId::new(), workspace, SupervisorRunId::new());
        pending.requires_session = true;
        pending.worker_session_id = worker.session_id;
        pending.worker_runtime_id = Some(worker.agent_runtime_id);
        pending.worker_profile_id = Some(profile.clone());
        pending.worker_agent_id = Some(agent_id);
        pending.worker_semantic_digest = Some("semantic".into());

        assert!(pending.matches_worker_scope(&worker));
        assert_eq!(pending.worker_profile_id(), Some(&profile));
        assert_eq!(pending.worker_agent_id(), Some(agent_id));
        assert_eq!(pending.worker_semantic_digest(), Some("semantic"));
        assert_eq!(
            pending.provenance(&worker).unwrap().dispatch_run_id,
            pending.operation_id()
        );

        let wrong_session = delegated_worker(workspace);
        assert!(!pending.matches_worker_scope(&wrong_session));
        let wrong_workspace = delegated_worker(WorkspaceId::new());
        assert!(!pending.matches_worker_scope(&wrong_workspace));
        assert!(pending.provenance(&wrong_workspace).is_err());

        let mut terminal = unbound_goal_run(Some(workspace));
        terminal
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .required_artifact_contract = NO_ARTIFACT_CONTRACT;
        let mut caller_reservation = start_reservation(terminal.supervisor_run_id);
        caller_reservation.caller_dispatch_run_id = Some(OperationId::new());
        assert!(has_unbound_root_worker(
            &terminal,
            Some(&caller_reservation)
        ));
        assert!(!has_unbound_root_worker(&terminal, None));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One malformed-state matrix proves every provenance and pending-authority fence fails closed.
    fn provenance_and_pending_authority_validation_is_fail_closed() {
        let workspace = WorkspaceId::new();
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        run.workspace_id = Some(workspace);
        run.state = SupervisorRunState::Running;
        let root_id = TaskId::new("root").unwrap();
        let root_operation = OperationId::new();
        let mut root = task(run.supervisor_run_id, "root", None);
        root.state = TaskState::Running;
        root.assigned_dispatch_run = Some(root_operation);
        run.tasks.insert(root_id.clone(), root);
        let root_provenance = provenance(run.supervisor_run_id, &root_id, None, root_operation);
        run.provenance
            .insert(root_id.clone(), root_provenance.clone());
        validate_provenance_chain(&run, &root_id, &root_provenance).unwrap();

        let missing_id = TaskId::new("missing").unwrap();
        assert!(child_dispatch_policy_denial(&run, &missing_id).is_err());
        let missing = provenance(run.supervisor_run_id, &missing_id, None, OperationId::new());
        assert!(
            validate_provenance_chain(&run, &missing_id, &missing)
                .unwrap_err()
                .to_string()
                .contains("task is missing")
        );
        let mut stale = run.clone();
        stale.tasks.get_mut(&root_id).unwrap().generation = 2;
        assert!(validate_provenance_chain(&stale, &root_id, &root_provenance).is_err());
        let mut rooted_parent = root_provenance.clone();
        rooted_parent.parent_dispatch_run = Some(OperationId::new());
        assert!(
            validate_provenance_chain(&run, &root_id, &rooted_parent)
                .unwrap_err()
                .to_string()
                .contains("root provenance has a parent")
        );

        let child_id = TaskId::new("child").unwrap();
        let child_operation = OperationId::new();
        let mut child = task(run.supervisor_run_id, "child", Some("root"));
        child.state = TaskState::Running;
        child.assigned_dispatch_run = Some(child_operation);
        run.tasks.insert(child_id.clone(), child);
        let child_provenance = provenance(
            run.supervisor_run_id,
            &child_id,
            Some((&root_id, root_operation)),
            child_operation,
        );
        validate_provenance_chain(&run, &child_id, &child_provenance).unwrap();
        let mut no_parent_dispatch = child_provenance.clone();
        no_parent_dispatch.parent_dispatch_run = None;
        assert!(
            validate_provenance_chain(&run, &child_id, &no_parent_dispatch)
                .unwrap_err()
                .to_string()
                .contains("no parent dispatch")
        );
        let mut missing_parent = run.clone();
        missing_parent.tasks.remove(&root_id);
        assert!(
            validate_provenance_chain(&missing_parent, &child_id, &child_provenance)
                .unwrap_err()
                .to_string()
                .contains("parent task is missing")
        );
        let mut historical = run.clone();
        historical
            .tasks
            .get_mut(&child_id)
            .unwrap()
            .promotion_parent_dispatch_run = Some(root_operation);
        historical.provenance.remove(&root_id);
        validate_provenance_chain(&historical, &child_id, &child_provenance).unwrap();
        let mut missing_parent_authority = run.clone();
        missing_parent_authority.provenance.remove(&root_id);
        assert!(
            validate_provenance_chain(&missing_parent_authority, &child_id, &child_provenance,)
                .unwrap_err()
                .to_string()
                .contains("parent authority is missing")
        );
        let mut cyclic = run.clone();
        let cyclic_task = cyclic.tasks.get_mut(&child_id).unwrap();
        cyclic_task.parent_task_id = Some(child_id.clone());
        let mut cyclic_provenance = child_provenance.clone();
        cyclic_provenance.parent_task_id = Some(child_id.clone());
        cyclic_provenance.parent_dispatch_run = Some(child_operation);
        cyclic
            .provenance
            .insert(child_id.clone(), cyclic_provenance.clone());
        assert!(
            validate_provenance_chain(&cyclic, &child_id, &cyclic_provenance)
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );

        let runtime_state = RuntimeState::default();
        assert!(
            live_task_dispatch_authority(&runtime_state, &run, &root_id, &mut BTreeSet::new(),)
                .unwrap()
                .unwrap()
                .committed
        );
        assert!(
            live_task_dispatch_authority(
                &runtime_state,
                &run,
                &TaskId::new("absent").unwrap(),
                &mut BTreeSet::new(),
            )
            .unwrap()
            .is_none()
        );
        let mut already_visiting = BTreeSet::from([root_id.clone()]);
        assert!(
            live_task_dispatch_authority(&runtime_state, &run, &root_id, &mut already_visiting,)
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );

        let mut pending_root_run = run.clone();
        pending_root_run.provenance.clear();
        let pending_root = pending_root_run.tasks.get_mut(&root_id).unwrap();
        pending_root.state = TaskState::Ready;
        pending_root.generation = 1;
        pending_root.assigned_dispatch_run = None;
        pending_root.required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
        pending_root_run.tasks.remove(&child_id);
        assert!(
            live_task_dispatch_authority(
                &runtime_state,
                &pending_root_run,
                &root_id,
                &mut BTreeSet::new(),
            )
            .unwrap()
            .is_none()
        );
        let mut stale_pending = pending_root_run.clone();
        stale_pending.tasks.get_mut(&root_id).unwrap().generation = 2;
        assert!(
            live_task_dispatch_authority(
                &runtime_state,
                &stale_pending,
                &root_id,
                &mut BTreeSet::new(),
            )
            .is_err()
        );
        let mut malformed_root = pending_root_run.clone();
        malformed_root
            .tasks
            .get_mut(&root_id)
            .unwrap()
            .parent_task_id = Some(TaskId::new("parent").unwrap());
        assert!(
            live_task_dispatch_authority(
                &runtime_state,
                &malformed_root,
                &root_id,
                &mut BTreeSet::new(),
            )
            .is_err()
        );

        let mut reserved_state = RuntimeState::default();
        let mut reservation = start_reservation(pending_root_run.supervisor_run_id);
        reservation.workspace_id = Some(workspace);
        reserved_state
            .starts
            .insert(root_operation.to_string(), reservation.clone());
        assert!(!has_caller_root_reservation(
            &reserved_state,
            pending_root_run.supervisor_run_id
        ));
        assert_eq!(
            live_task_dispatch_authority(
                &reserved_state,
                &pending_root_run,
                &root_id,
                &mut BTreeSet::new(),
            )
            .unwrap()
            .unwrap()
            .operation_id,
            root_operation
        );
        let mut generic_root = pending_root_run.clone();
        generic_root
            .tasks
            .get_mut(&root_id)
            .unwrap()
            .required_artifact_contract = NO_ARTIFACT_CONTRACT;
        let mut generic_state = RuntimeState::default();
        let mut generic_reservation = start_reservation(generic_root.supervisor_run_id);
        generic_reservation.caller_dispatch_run_id = Some(root_operation);
        generic_state
            .starts
            .insert(OperationId::new().to_string(), generic_reservation);
        assert!(has_caller_root_reservation(
            &generic_state,
            generic_root.supervisor_run_id
        ));
        assert_eq!(
            live_task_dispatch_authority(
                &generic_state,
                &generic_root,
                &root_id,
                &mut BTreeSet::new(),
            )
            .unwrap()
            .unwrap()
            .operation_id,
            root_operation
        );
        let duplicate_operation = OperationId::new().to_string();
        reserved_state
            .starts
            .insert(duplicate_operation.clone(), reservation.clone());
        assert!(
            live_task_dispatch_authority(
                &reserved_state,
                &pending_root_run,
                &root_id,
                &mut BTreeSet::new(),
            )
            .is_err()
        );
        reserved_state.starts.remove(&duplicate_operation);
        let mut invalid_state = RuntimeState::default();
        invalid_state.starts.insert("invalid".into(), reservation);
        assert!(
            live_task_dispatch_authority(
                &invalid_state,
                &pending_root_run,
                &root_id,
                &mut BTreeSet::new(),
            )
            .unwrap_err()
            .to_string()
            .contains("operation is invalid")
        );

        let ordinary_id = TaskId::new("ordinary").unwrap();
        let mut ordinary_run = pending_root_run.clone();
        let mut ordinary = task(ordinary_run.supervisor_run_id, "ordinary", Some("root"));
        ordinary.state = TaskState::Ready;
        ordinary_run.tasks.insert(ordinary_id.clone(), ordinary);
        assert!(
            live_task_dispatch_authority(
                &RuntimeState::default(),
                &ordinary_run,
                &ordinary_id,
                &mut BTreeSet::new(),
            )
            .unwrap()
            .is_none()
        );

        let invalid_delegated = TaskId::new("delegated-invalid").unwrap();
        let mut delegated_run = pending_root_run.clone();
        let mut invalid_task = task(
            delegated_run.supervisor_run_id,
            invalid_delegated.0.as_str(),
            Some("root"),
        );
        invalid_task.state = TaskState::Ready;
        delegated_run
            .tasks
            .insert(invalid_delegated.clone(), invalid_task);
        assert!(
            live_task_dispatch_authority(
                &RuntimeState::default(),
                &delegated_run,
                &invalid_delegated,
                &mut BTreeSet::new(),
            )
            .unwrap_err()
            .to_string()
            .contains("operation is invalid")
        );

        let delegated_operation = OperationId::new();
        let delegated_id = delegated_task_id(delegated_operation).unwrap();
        let mut delegated = task(
            pending_root_run.supervisor_run_id,
            delegated_id.0.as_str(),
            Some("root"),
        );
        delegated.state = TaskState::Ready;
        delegated.instruction_digest = delegated_task_digest(delegated_operation);
        delegated.promotion_reserved_at = Some(now());
        delegated.promotion_parent_dispatch_run = Some(root_operation);
        let mut delegated_run = pending_root_run.clone();
        delegated_run.tasks.insert(delegated_id.clone(), delegated);
        assert_eq!(
            live_task_dispatch_authority(
                &RuntimeState::default(),
                &delegated_run,
                &delegated_id,
                &mut BTreeSet::new(),
            )
            .unwrap()
            .unwrap()
            .operation_id,
            delegated_operation
        );
        let mut malformed_delegated = delegated_run.clone();
        malformed_delegated
            .tasks
            .get_mut(&delegated_id)
            .unwrap()
            .promotion_reserved_at = None;
        assert!(
            live_task_dispatch_authority(
                &RuntimeState::default(),
                &malformed_delegated,
                &delegated_id,
                &mut BTreeSet::new(),
            )
            .unwrap()
            .is_none()
        );
        delegated_run.tasks.remove(&root_id);
        assert!(
            live_task_dispatch_authority(
                &RuntimeState::default(),
                &delegated_run,
                &delegated_id,
                &mut BTreeSet::new(),
            )
            .unwrap_err()
            .to_string()
            .contains("task is missing")
        );

        let mut legacy_delegated_run = run.clone();
        let mut legacy_delegated = task(
            legacy_delegated_run.supervisor_run_id,
            delegated_id.0.as_str(),
            Some("root"),
        );
        legacy_delegated.state = TaskState::Ready;
        legacy_delegated.instruction_digest = delegated_task_digest(delegated_operation);
        legacy_delegated.promotion_reserved_at = Some(now());
        legacy_delegated_run
            .tasks
            .insert(delegated_id.clone(), legacy_delegated.clone());
        assert!(
            live_task_dispatch_authority(
                &RuntimeState::default(),
                &legacy_delegated_run,
                &delegated_id,
                &mut BTreeSet::new(),
            )
            .unwrap()
            .is_some()
        );
        let mut pending_parent_run = pending_root_run.clone();
        legacy_delegated.supervisor_run_id = pending_parent_run.supervisor_run_id;
        pending_parent_run
            .tasks
            .insert(delegated_id.clone(), legacy_delegated);
        let error = live_task_dispatch_authority(
            &reserved_state,
            &pending_parent_run,
            &delegated_id,
            &mut BTreeSet::new(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("no durable parent fence"),
            "{error:#}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One identity matrix keeps every pending promotion collision fence explicit.
    fn pending_operation_validation_joins_live_agent_identity_and_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        assert!(
            scheduler
                .ensure_pending_operation_matches_reservation(
                    operation, workspace, false, None, None, None, None,
                )
                .is_ok()
        );

        let worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, operation, &worker);
        let dispatch = scheduler.dispatch.run(operation).unwrap().unwrap();
        let agent = scheduler
            .dispatch
            .agent(dispatch.agent_id)
            .unwrap()
            .unwrap();
        let digest = usagi_core::infrastructure::ipc::agent_operation_digest("caller-semantic");
        scheduler
            .ensure_pending_operation_matches_reservation(
                operation,
                workspace,
                false,
                None,
                Some(&agent.runtime),
                Some(agent.agent_id),
                Some(&digest),
            )
            .unwrap();
        for result in [
            scheduler.ensure_pending_operation_matches_reservation(
                operation, workspace, true, None, None, None, None,
            ),
            scheduler.ensure_pending_operation_matches_reservation(
                operation,
                workspace,
                false,
                None,
                Some(&AgentProfileId::new("codex").unwrap()),
                None,
                None,
            ),
            scheduler.ensure_pending_operation_matches_reservation(
                operation,
                workspace,
                false,
                None,
                None,
                Some(AgentId::new()),
                None,
            ),
            scheduler.ensure_pending_operation_matches_reservation(
                operation,
                workspace,
                false,
                None,
                None,
                None,
                Some("wrong-digest"),
            ),
        ] {
            assert!(result.is_err());
        }

        let mut closed = dispatch.clone();
        closed.status = RunStatus::Completed;
        closed.ended_at = Some(now());
        scheduler.dispatch.upsert_run(closed).unwrap();
        assert!(
            scheduler
                .ensure_pending_operation_matches_reservation(
                    operation, workspace, false, None, None, None, None,
                )
                .unwrap_err()
                .to_string()
                .contains("closed supervisor ownership")
        );

        let foreign = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: foreign,
                agent_id: AgentId::new(),
                prompt: "foreign".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .ensure_pending_operation_matches_reservation(
                    foreign, workspace, false, None, None, None, None,
                )
                .unwrap_err()
                .to_string()
                .contains("foreign Agent ownership")
        );

        let no_semantics = OperationId::new();
        let no_semantics_agent = AgentId::new();
        scheduler
            .dispatch
            .upsert_agent(
                workspace,
                Agent {
                    agent_id: no_semantics_agent,
                    session_id: None,
                    runtime: AgentProfileId::new("claude").unwrap(),
                    model: ModelSelector::new("default").unwrap(),
                    status: AgentStatus::Running,
                    current_run: Some(no_semantics),
                },
            )
            .unwrap();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: no_semantics,
                agent_id: no_semantics_agent,
                prompt: "no semantics".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .ensure_pending_operation_matches_reservation(
                    no_semantics,
                    workspace,
                    false,
                    None,
                    None,
                    None,
                    Some("digest"),
                )
                .unwrap_err()
                .to_string()
                .contains("no semantic authority")
        );

        let session_operation = OperationId::new();
        let session_worker = delegated_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, session_operation, &session_worker);
        scheduler
            .ensure_pending_operation_matches_reservation(
                session_operation,
                workspace,
                true,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert!(
            scheduler
                .ensure_pending_operation_matches_reservation(
                    session_operation,
                    workspace,
                    true,
                    Some(SessionId::new()),
                    None,
                    None,
                    None,
                )
                .is_err()
        );

        let root_operation = OperationId::new();
        let parent_worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, root_operation, &parent_worker);
        scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &root_operation.to_string(),
                goal("pending child validation"),
                None,
                &parent_worker,
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "pending child",
                now(),
            )
            .unwrap()
            .unwrap();
        persist_caller_dispatch(
            &scheduler,
            workspace,
            child_operation,
            &root_worker(workspace),
        );
        assert!(
            scheduler
                .supervision_fence(child_operation)
                .unwrap_err()
                .to_string()
                .contains("conflicts with its Agent ownership")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One matrix covers every independent root dispatch reservation fence.
    fn root_dispatch_binding_checks_reserved_identity_contract_and_planning_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let semantic_digest =
            usagi_core::infrastructure::ipc::agent_operation_digest("caller-semantic");

        let missing_agent_operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: missing_agent_operation,
                agent_id: AgentId::new(),
                prompt: "missing Agent".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &missing_agent_operation.to_string(),
                goal("missing Agent"),
                None,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_workspace_root_dispatch(
                    &missing_agent_operation.to_string(),
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("Agent does not exist")
        );

        let goal_operation = OperationId::new();
        let goal_worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, goal_operation, &goal_worker);
        scheduler
            .reserve_goal_for_workspace_with_profile(
                "goal",
                workspace,
                &goal_operation.to_string(),
                goal("profiled goal"),
                AgentProfileId::new("claude").unwrap(),
                semantic_digest.clone(),
                None,
                now(),
            )
            .unwrap();
        scheduler
            .bind_reserved_workspace_root_dispatch(&goal_operation.to_string(), &goal_worker, now())
            .unwrap();

        let profile_operation = OperationId::new();
        let profile_worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, profile_operation, &profile_worker);
        scheduler
            .reserve_goal_for_workspace_with_profile(
                "goal",
                workspace,
                &profile_operation.to_string(),
                goal("wrong profile"),
                AgentProfileId::new("codex").unwrap(),
                semantic_digest.clone(),
                None,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_workspace_root_dispatch(
                    &profile_operation.to_string(),
                    &profile_worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("reserved Agent scope")
        );

        let semantic_operation = OperationId::new();
        let semantic_worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, semantic_operation, &semantic_worker);
        scheduler
            .reserve_goal_for_workspace_with_profile(
                "goal",
                workspace,
                &semantic_operation.to_string(),
                goal("wrong semantics"),
                AgentProfileId::new("claude").unwrap(),
                "wrong-digest".into(),
                None,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_workspace_root_dispatch(
                    &semantic_operation.to_string(),
                    &semantic_worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("another semantic intent")
        );

        let session_operation = OperationId::new();
        let session_worker = delegated_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, session_operation, &session_worker);
        scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &session_operation.to_string(),
                goal("must be root"),
                None,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_workspace_root_dispatch(
                    &session_operation.to_string(),
                    &session_worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("workspace root scope")
        );

        let caller_session_operation = OperationId::new();
        let caller_session_worker = delegated_worker(workspace);
        persist_caller_dispatch(
            &scheduler,
            workspace,
            caller_session_operation,
            &caller_session_worker,
        );
        let caller_session_start = OperationId::new().to_string();
        scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &caller_session_start,
                "session fence".into(),
                None,
                caller_session_operation,
                &caller_session_worker,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_caller_dispatch(
                    &caller_session_start,
                    caller_session_operation,
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("reserved Agent scope")
        );

        let caller_operation = OperationId::new();
        let caller_worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, caller_operation, &caller_worker);
        assert!(
            scheduler
                .start_for_workspace_caller_dispatch(
                    "caller",
                    workspace,
                    &OperationId::new().to_string(),
                    "wrong scope".into(),
                    None,
                    caller_operation,
                    &root_worker(WorkspaceId::new()),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("outside its authenticated scope")
        );
        let start_operation = OperationId::new().to_string();
        scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &start_operation,
                "generic root".into(),
                None,
                caller_operation,
                &caller_worker,
                now(),
            )
            .unwrap();
        let other_operation = OperationId::new();
        let other_worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, other_operation, &other_worker);
        assert!(
            scheduler
                .bind_reserved_caller_dispatch(
                    &start_operation,
                    other_operation,
                    &other_worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("conflicts with its reservation")
        );
        assert!(
            scheduler
                .bind_reserved_caller_dispatch(
                    &start_operation,
                    caller_operation,
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("reserved Agent scope")
        );

        let planning_temp = tempfile::tempdir().unwrap();
        let planning = SupervisorRuntime::new(planning_temp.path());
        let planning_operation = OperationId::new();
        let planning_worker = root_worker(workspace);
        persist_caller_dispatch(&planning, workspace, planning_operation, &planning_worker);
        let planning_start = OperationId::new().to_string();
        planning.fail_apply_at(1);
        assert!(
            planning
                .start_for_workspace_caller_dispatch(
                    "caller",
                    workspace,
                    &planning_start,
                    "planning root".into(),
                    None,
                    planning_operation,
                    &planning_worker,
                    now(),
                )
                .is_err()
        );
        planning.fail_apply_at(2);
        assert!(
            planning
                .bind_reserved_caller_dispatch(
                    &planning_start,
                    planning_operation,
                    &planning_worker,
                    now(),
                )
                .is_err()
        );
        let recovered = planning
            .bind_reserved_caller_dispatch(
                &planning_start,
                planning_operation,
                &planning_worker,
                now(),
            )
            .unwrap();
        assert_eq!(recovered.state, SupervisorRunState::Running);
    }

    fn escalated_retry_run(
        workspace: WorkspaceId,
    ) -> (SupervisorRun, TaskId, OperationId, OperationId) {
        let mut run = SupervisorRun::new(
            "caller".into(),
            "retry-root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        run.workspace_id = Some(workspace);
        run.state = SupervisorRunState::Escalated;
        let task_id = TaskId::new("root").unwrap();
        let dispatch_run_id = OperationId::new();
        let mut root = task(run.supervisor_run_id, "root", None);
        root.state = TaskState::Verifying;
        root.required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
        root.assigned_dispatch_run = Some(dispatch_run_id);
        run.tasks.insert(task_id.clone(), root);
        run.provenance.insert(
            task_id.clone(),
            provenance(run.supervisor_run_id, &task_id, None, dispatch_run_id),
        );
        let escalation_id = OperationId::new();
        run.escalation = Some(EscalationRecord {
            escalation_id,
            reason: "fresh Agent result required".into(),
            blocking_task_id: Some(task_id.clone()),
            safe_evidence: "artifact verification rejected the previous result".into(),
            choices: vec!["resume".into(), "cancel".into()],
            created_at: now(),
        });
        (run, task_id, dispatch_run_id, escalation_id)
    }

    #[test]
    fn display_labels_and_verification_candidates_are_bounded() {
        assert_eq!(work_run_display_label(" \n\t"), None);
        assert_eq!(work_run_display_label("unsafe\u{1b}[2J"), None);
        let expected = "x".repeat(95);
        assert_eq!(
            work_run_display_label(&format!("{}é", "x".repeat(95))).as_deref(),
            Some(expected.as_str())
        );

        let mut candidate_run = SupervisorRun::new(
            "caller".into(),
            "candidate".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        candidate_run.state = SupervisorRunState::Running;
        let candidate_id = TaskId::new("candidate").unwrap();
        let mut candidate_task = task(candidate_run.supervisor_run_id, "candidate", None);
        candidate_task.state = TaskState::Verifying;
        candidate_run
            .tasks
            .insert(candidate_id.clone(), candidate_task);
        let missing_candidate = event(
            &candidate_run,
            SupervisorEventKind::VerificationCandidateRecorded {
                task_id: TaskId::new("missing").unwrap(),
                generation: 1,
                candidate_pr: None,
            },
        );
        assert!(matches!(
            reduce(&mut candidate_run, &missing_candidate),
            Err(usagi_core::domain::supervisor::SupervisorError::MissingTask)
        ));
        let stale_candidate = event(
            &candidate_run,
            SupervisorEventKind::VerificationCandidateRecorded {
                task_id: candidate_id.clone(),
                generation: 2,
                candidate_pr: None,
            },
        );
        assert!(matches!(
            reduce(&mut candidate_run, &stale_candidate),
            Err(usagi_core::domain::supervisor::SupervisorError::StaleGeneration)
        ));
        let recorded_candidate = event(
            &candidate_run,
            SupervisorEventKind::VerificationCandidateRecorded {
                task_id: candidate_id.clone(),
                generation: 1,
                candidate_pr: None,
            },
        );
        reduce(&mut candidate_run, &recorded_candidate).unwrap();
        let replayed_candidate = event(
            &candidate_run,
            SupervisorEventKind::VerificationCandidateRecorded {
                task_id: candidate_id.clone(),
                generation: 1,
                candidate_pr: None,
            },
        );
        reduce(&mut candidate_run, &replayed_candidate).unwrap();
        let conflicting_candidate = event(
            &candidate_run,
            SupervisorEventKind::VerificationCandidateRecorded {
                task_id: candidate_id.clone(),
                generation: 1,
                candidate_pr: Some("https://github.com/acme/repo/pull/42".into()),
            },
        );
        assert!(matches!(
            reduce(&mut candidate_run, &conflicting_candidate),
            Err(usagi_core::domain::supervisor::SupervisorError::ProvenanceMismatch)
        ));
        let invalid_candidate = event(
            &candidate_run,
            SupervisorEventKind::VerificationCandidateRecorded {
                task_id: candidate_id,
                generation: 1,
                candidate_pr: Some("https://example.com/acme/repo/pull/42".into()),
            },
        );
        assert!(matches!(
            reduce(&mut candidate_run, &invalid_candidate),
            Err(usagi_core::domain::supervisor::SupervisorError::InvalidTransition)
        ));
    }

    #[test]
    fn core_reducer_projects_dependents_and_keeps_terminal_cancellation_idempotent() {
        let mut run = SupervisorRun::new(
            "caller".into(),
            "reducer".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        run.state = SupervisorRunState::Running;
        let root_id = TaskId::new("root").unwrap();
        let mut root = task(run.supervisor_run_id, "root", None);
        root.state = TaskState::Running;
        let child_id = TaskId::new("child").unwrap();
        let mut child = task(run.supervisor_run_id, "child", Some("root"));
        child.dependencies.insert(root_id.clone());
        run.tasks.insert(root_id.clone(), root);
        run.tasks.insert(child_id.clone(), child);

        let succeeded = event(
            &run,
            SupervisorEventKind::SetTaskState {
                task_id: root_id.clone(),
                generation: 1,
                state: TaskState::Succeeded,
            },
        );
        reduce(&mut run, &succeeded).unwrap();
        assert_eq!(run.tasks[&child_id].state, TaskState::Ready);
        let cancel_terminal = event(
            &run,
            SupervisorEventKind::Cancel {
                task_id: Some(root_id.clone()),
                reason: "late cancellation replay".into(),
            },
        );
        reduce(&mut run, &cancel_terminal).unwrap();
        assert_eq!(run.tasks[&root_id].state, TaskState::Succeeded);
    }

    #[test]
    fn indexed_recovery_edges_are_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        assert!(
            scheduler
                .load_indexed_runs([SupervisorRunId::new()])
                .unwrap_err()
                .to_string()
                .contains("indexed supervisor run disappeared")
        );

        let workspace = WorkspaceId::new();
        let mut live = SupervisorRun::new(
            "caller".into(),
            "live".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        live.workspace_id = Some(workspace);
        live.state = SupervisorRunState::Running;
        scheduler.supervisor.initialize(&live).unwrap();
        let mut stale_finished = live;
        stale_finished.state = SupervisorRunState::Succeeded;
        stale_finished.terminal_at = Some(now());
        json_file::write_atomic(
            scheduler
                .supervisor
                .snapshot_path(stale_finished.supervisor_run_id)
                .parent()
                .unwrap(),
            &scheduler
                .supervisor
                .snapshot_path(stale_finished.supervisor_run_id),
            &stale_finished,
        )
        .unwrap();
        assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
        assert!(
            scheduler
                .pending_artifact_verifications(now())
                .unwrap()
                .is_empty()
        );

        let aborted_temp = tempfile::tempdir().unwrap();
        let aborted_scheduler = SupervisorRuntime::new(aborted_temp.path());
        let aborted = aborted_run(Some(workspace));
        aborted_scheduler.supervisor.initialize(&aborted).unwrap();
        let mut stale_running = aborted;
        stale_running.state = SupervisorRunState::Running;
        stale_running.terminal_at = None;
        json_file::write_atomic(
            aborted_scheduler
                .supervisor
                .snapshot_path(stale_running.supervisor_run_id)
                .parent()
                .unwrap(),
            &aborted_scheduler
                .supervisor
                .snapshot_path(stale_running.supervisor_run_id),
            &stale_running,
        )
        .unwrap();
        assert!(aborted_scheduler.pending_worker_stops().unwrap().is_empty());
        assert!(
            aborted_scheduler
                .worker_stop_obligations()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn retry_resolution_checks_every_run_escalation_and_agent_fence() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        assert!(
            scheduler
                .retry_work_for_workspace(workspace, SupervisorRunId::new(), OperationId::new(),)
                .unwrap()
                .is_none()
        );

        let (run, task_id, dispatch_run_id, escalation_id) = escalated_retry_run(workspace);
        scheduler.supervisor.initialize(&run).unwrap();
        assert!(
            scheduler
                .retry_work_for_workspace(WorkspaceId::new(), run.supervisor_run_id, escalation_id,)
                .unwrap()
                .is_none()
        );
        assert!(
            scheduler
                .retry_work_for_workspace(workspace, run.supervisor_run_id, OperationId::new(),)
                .unwrap_err()
                .to_string()
                .contains("escalation fence is stale")
        );

        let mut without_blocker = run.clone();
        without_blocker
            .escalation
            .as_mut()
            .unwrap()
            .blocking_task_id = None;
        scheduler.supervisor.initialize(&without_blocker).unwrap();
        assert!(
            scheduler
                .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
                .unwrap()
                .is_none()
        );

        let mut ordinary = run.clone();
        ordinary.tasks.get_mut(&task_id).unwrap().state = TaskState::Ready;
        scheduler.supervisor.initialize(&ordinary).unwrap();
        assert!(
            scheduler
                .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
                .unwrap()
                .is_none()
        );

        let mut without_provenance = run.clone();
        without_provenance.provenance.clear();
        scheduler
            .supervisor
            .initialize(&without_provenance)
            .unwrap();
        assert!(
            scheduler
                .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
                .unwrap_err()
                .to_string()
                .contains("retry provenance is missing")
        );

        let mut without_task = run.clone();
        without_task.tasks.clear();
        scheduler.supervisor.initialize(&without_task).unwrap();
        assert!(
            scheduler
                .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
                .unwrap_err()
                .to_string()
                .contains("retry task is missing")
        );

        let mut stale = run.clone();
        stale.tasks.get_mut(&task_id).unwrap().generation += 1;
        scheduler.supervisor.initialize(&stale).unwrap();
        assert!(
            scheduler
                .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
                .unwrap_err()
                .to_string()
                .contains("retry provenance fence is stale")
        );

        scheduler.supervisor.initialize(&run).unwrap();
        let retry = scheduler
            .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
            .unwrap()
            .unwrap();
        assert_eq!(retry.provenance.dispatch_run_id, dispatch_run_id);
        assert_eq!(retry.reason, "fresh Agent result required");
        assert_eq!(
            retry.safe_evidence,
            "artifact verification rejected the previous result"
        );
    }

    #[test]
    fn wake_delivery_isolates_failures_and_persists_later_successes() {
        struct SelectiveWaker {
            failing_child: OperationId,
            attempted: Vec<OperationId>,
        }
        impl DecisionWaker for SelectiveWaker {
            fn wake(&mut self, wake: &DecisionWake) -> Result<()> {
                self.attempted.push(wake.child_run_id);
                if wake.child_run_id == self.failing_child {
                    anyhow::bail!("injected wake failure");
                }
                Ok(())
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let mut state = RuntimeState::default();
        let mut runs = Vec::new();
        for (key, index) in [("a-fail", 0), ("b-pass", 1)] {
            let mut run = SupervisorRun::new(
                "caller".into(),
                "root".into(),
                "input".into(),
                "policy".into(),
                now(),
            );
            run.state = SupervisorRunState::Running;
            let parent_id = TaskId::new(format!("parent-{index}")).unwrap();
            let parent_dispatch = OperationId::new();
            let mut parent = task(run.supervisor_run_id, &parent_id.0, None);
            parent.state = TaskState::AwaitingDecision;
            parent.assigned_dispatch_run = Some(parent_dispatch);
            let parent_provenance =
                provenance(run.supervisor_run_id, &parent_id, None, parent_dispatch);
            run.tasks.insert(parent_id.clone(), parent);
            run.provenance
                .insert(parent_id.clone(), parent_provenance.clone());
            scheduler.supervisor.initialize(&run).unwrap();

            let child_run_id = OperationId::new();
            state.wakes.insert(
                key.into(),
                WakeReservation {
                    wake: DecisionWake {
                        supervisor_run_id: run.supervisor_run_id,
                        parent_task_id: parent_id.clone(),
                        parent_generation: 1,
                        parent: parent_provenance,
                        child_run_id,
                        outcome: WakeOutcome {
                            kind: InboxKind::Completed,
                            summary: "done".into(),
                        },
                        dag: Vec::new(),
                        remaining_budget_summary: "none".into(),
                    },
                    delivered: false,
                },
            );
            runs.push((run.supervisor_run_id, parent_id, child_run_id));
        }
        scheduler.save_state(&state).unwrap();

        let mut waker = SelectiveWaker {
            failing_child: runs[0].2,
            attempted: Vec::new(),
        };
        assert!(
            scheduler
                .deliver_reserved(now(), &mut waker)
                .unwrap_err()
                .to_string()
                .contains("injected wake failure")
        );
        assert_eq!(waker.attempted, vec![runs[0].2, runs[1].2]);
        let state = scheduler.load_state().unwrap();
        assert!(!state.wakes["a-fail"].delivered);
        assert!(state.wakes["b-pass"].delivered);
        assert_eq!(
            scheduler.supervisor.load(runs[0].0).unwrap().unwrap().tasks[&runs[0].1].state,
            TaskState::AwaitingDecision
        );
        assert_eq!(
            scheduler.supervisor.load(runs[1].0).unwrap().unwrap().tasks[&runs[1].1].state,
            TaskState::Running
        );

        let mut retry = Waker::default();
        scheduler.deliver_reserved(now(), &mut retry).unwrap();
        assert_eq!(retry.wakes.len(), 1);
        assert_eq!(retry.wakes[0].child_run_id, runs[0].2);
        assert!(
            scheduler
                .load_state()
                .unwrap()
                .wakes
                .values()
                .all(|wake| wake.delivered)
        );
    }

    #[test]
    fn tick_all_reports_tick_failure_after_attempting_wake_delivery() {
        struct RejectingWaker {
            attempted: bool,
        }
        impl DecisionWaker for RejectingWaker {
            fn wake(&mut self, _wake: &DecisionWake) -> Result<()> {
                self.attempted = true;
                anyhow::bail!("injected wake failure")
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let mut retrying = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let retrying_id = TaskId::new("retrying").unwrap();
        let mut due = task(retrying.supervisor_run_id, "retrying", None);
        due.state = TaskState::Retrying;
        due.retry_at = Some(now());
        retrying.tasks.insert(retrying_id, due);
        scheduler.supervisor.initialize(&retrying).unwrap();
        let mut state = RuntimeState::default();
        state
            .wakes
            .insert("reject".into(), wake_reservation(0, false));
        scheduler.save_state(&state).unwrap();
        scheduler.fail_apply_at(0);
        let mut waker = RejectingWaker { attempted: false };
        assert!(
            scheduler
                .tick_all(now(), &mut waker)
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        assert!(waker.attempted);
    }

    #[test]
    fn parent_wake_failures_remain_observable_and_replay_is_idempotent() {
        let wake_temp = tempfile::tempdir().unwrap();
        let wake_scheduler = SupervisorRuntime::new(wake_temp.path());
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        run.state = SupervisorRunState::Running;
        let parent_id = TaskId::new("parent").unwrap();
        let parent_dispatch = OperationId::new();
        let mut parent = task(run.supervisor_run_id, "parent", None);
        parent.state = TaskState::AwaitingDecision;
        parent.assigned_dispatch_run = Some(parent_dispatch);
        let parent_provenance =
            provenance(run.supervisor_run_id, &parent_id, None, parent_dispatch);
        run.tasks.insert(parent_id.clone(), parent);
        run.provenance
            .insert(parent_id.clone(), parent_provenance.clone());
        wake_scheduler.supervisor.initialize(&run).unwrap();
        let wake = DecisionWake {
            supervisor_run_id: run.supervisor_run_id,
            parent_task_id: parent_id.clone(),
            parent_generation: 1,
            parent: parent_provenance,
            child_run_id: OperationId::new(),
            outcome: WakeOutcome {
                kind: InboxKind::Completed,
                summary: "done".into(),
            },
            dag: Vec::new(),
            remaining_budget_summary: "none".into(),
        };

        let mut missing_run = wake.clone();
        missing_run.supervisor_run_id = SupervisorRunId::new();
        assert!(
            wake_scheduler
                .resume_parent_after_wake(&missing_run, now())
                .unwrap_err()
                .to_string()
                .contains("run is unavailable")
        );
        let mut missing_task = wake.clone();
        missing_task.parent_task_id = TaskId::new("missing").unwrap();
        assert!(
            wake_scheduler
                .resume_parent_after_wake(&missing_task, now())
                .unwrap_err()
                .to_string()
                .contains("task is unavailable")
        );
        let mut stale_wake = wake.clone();
        stale_wake.parent_generation += 1;
        assert!(
            wake_scheduler
                .resume_parent_after_wake(&stale_wake, now())
                .unwrap_err()
                .to_string()
                .contains("fence is stale")
        );

        let mut not_resumable = run.clone();
        not_resumable.tasks.get_mut(&parent_id).unwrap().state = TaskState::Pending;
        wake_scheduler
            .supervisor
            .initialize(&not_resumable)
            .unwrap();
        assert!(
            wake_scheduler
                .resume_parent_after_wake(&wake, now())
                .unwrap_err()
                .to_string()
                .contains("not resumable")
        );

        wake_scheduler.supervisor.initialize(&run).unwrap();
        wake_scheduler.fail_apply_at(wake_scheduler.apply_calls.get());
        assert!(
            wake_scheduler
                .resume_parent_after_wake(&wake, now())
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        wake_scheduler
            .resume_parent_after_wake(&wake, now())
            .unwrap();
        assert_eq!(
            wake_scheduler
                .supervisor
                .load(run.supervisor_run_id)
                .unwrap()
                .unwrap()
                .tasks[&parent_id]
                .state,
            TaskState::Running
        );
        wake_scheduler
            .resume_parent_after_wake(&wake, now())
            .unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One lifecycle fixture covers verification, escalation, resume, and late-result fencing.
    fn workspace_root_dispatch_is_bound_idempotently_and_completes_the_run() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, operation);
        let worker = root_worker(workspace);
        let unbound = scheduler
            .reserve_goal_for_workspace(
                "goal-composer",
                workspace,
                &operation.to_string(),
                goal("finish the requested work"),
                Some("standard".into()),
                now(),
            )
            .unwrap();
        let mut waker = Waker::default();
        scheduler
            .tick(unbound.supervisor_run_id, now(), &mut waker)
            .unwrap();
        assert_eq!(
            scheduler
                .get("goal-composer", unbound.supervisor_run_id)
                .unwrap()
                .unwrap()
                .state,
            SupervisorRunState::Running
        );
        assert!(
            scheduler
                .supervisor
                .load(unbound.supervisor_run_id)
                .unwrap()
                .unwrap()
                .tasks[&TaskId::new("root").unwrap()]
                .promotion_reserved_at
                .is_some()
        );
        // A pre-fix daemon could persist this escalation between reservation
        // and binding. The new binder must heal that exact legacy snapshot.
        let reserved = scheduler
            .supervisor
            .load(unbound.supervisor_run_id)
            .unwrap()
            .unwrap();
        scheduler
            .apply(
                &reserved,
                now(),
                SupervisorEventSource::DispatchFailure,
                SupervisorEventKind::Escalate {
                    task_id: Some(TaskId::new("root").unwrap()),
                    reason: MISSING_DISPATCH_ESCALATION_REASON.into(),
                    safe_evidence: "pre-fix snapshot".into(),
                    choices: vec!["resume".into(), "cancel".into()],
                },
            )
            .unwrap();
        let first = scheduler
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &operation.to_string(),
                goal("finish the requested work"),
                Some("standard".into()),
                &worker,
                now(),
            )
            .unwrap();
        let replay = scheduler
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &operation.to_string(),
                goal("finish the requested work"),
                Some("standard".into()),
                &worker,
                now(),
            )
            .unwrap();
        assert_eq!(replay, first);
        assert_eq!(first.tasks[0].state, TaskState::Dispatched);
        assert_eq!(first.tasks[0].assigned_dispatch_run, Some(operation));
        assert_eq!(
            scheduler
                .supervisor
                .load(first.supervisor_run_id)
                .unwrap()
                .unwrap()
                .tasks[&TaskId::new("root").unwrap()]
                .promotion_reserved_at,
            None
        );
        assert_eq!(first.provenance[0].worker_session_id, None);

        scheduler
            .tick(first.supervisor_run_id, now(), &mut waker)
            .unwrap();
        let active = scheduler
            .get("goal-composer", first.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(active.state, SupervisorRunState::Running);
        assert!(active.escalation.is_none());

        dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        scheduler
            .tick(first.supervisor_run_id, now(), &mut waker)
            .unwrap();
        let completed = scheduler
            .get("goal-composer", first.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(completed.tasks[0].state, TaskState::Verifying);
        assert_eq!(completed.state, SupervisorRunState::Running);
        let request = scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .unwrap();
        assert_eq!(request.repository, artifact_repository());
        for invalid in [
            ArtifactVerification {
                status: ArtifactVerificationStatus::Verified,
                result_digest: String::new(),
                safe_summary: "verified".into(),
            },
            ArtifactVerification {
                status: ArtifactVerificationStatus::Verified,
                result_digest: "verified".into(),
                safe_summary: "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
            },
        ] {
            assert!(
                scheduler
                    .record_artifact_verification(&request, invalid, now())
                    .is_err()
            );
        }
        assert_eq!(
            scheduler
                .get("goal-composer", first.supervisor_run_id)
                .unwrap()
                .unwrap()
                .tasks[0]
                .state,
            TaskState::Verifying
        );
        let deferred = scheduler
            .record_artifact_verification(
                &request,
                ArtifactVerification {
                    status: ArtifactVerificationStatus::Retryable,
                    result_digest: "provider-unavailable".into(),
                    safe_summary: "pull request verification provider is unavailable".into(),
                },
                now(),
            )
            .unwrap();
        assert_eq!(deferred.state, SupervisorRunState::Running);
        assert_eq!(deferred.tasks[0].state, TaskState::Verifying);
        assert_eq!(deferred.tasks[0].verification_attempt, 1);
        assert_eq!(
            deferred.tasks[0].verification_retry_at,
            Some(now() + chrono::Duration::seconds(ARTIFACT_RETRY_BASE_SECONDS))
        );
        assert!(
            scheduler
                .pending_artifact_verifications(now())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            scheduler
                .record_artifact_verification(
                    &request,
                    ArtifactVerification {
                        status: ArtifactVerificationStatus::Retryable,
                        result_digest: "duplicate".into(),
                        safe_summary: "duplicate".into(),
                    },
                    now(),
                )
                .unwrap(),
            deferred
        );
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap()
                .is_none()
        );
        let due = now() + chrono::Duration::seconds(ARTIFACT_RETRY_BASE_SECONDS);
        assert_eq!(
            scheduler.pending_artifact_verifications(due).unwrap(),
            vec![PendingArtifactVerification {
                dispatch_run_id: operation,
            }]
        );
        let retry = scheduler
            .prepare_artifact_verification(operation, due)
            .unwrap()
            .unwrap();
        let retry = scheduler
            .record_artifact_expectation(&retry, &artifact_expectation(), due)
            .unwrap();
        let rejected = scheduler
            .record_artifact_verification(
                &retry,
                ArtifactVerification {
                    status: ArtifactVerificationStatus::Rejected,
                    result_digest: "draft-pr".into(),
                    safe_summary: "pull request is still a draft".into(),
                },
                due,
            )
            .unwrap();
        assert_eq!(rejected.state, SupervisorRunState::Escalated);
        assert_eq!(
            rejected.escalation.as_ref().unwrap().safe_evidence,
            "pull request is still a draft"
        );
        scheduler
            .resolve_escalation(
                "goal-composer",
                first.supervisor_run_id,
                rejected.escalation.unwrap().escalation_id,
                EscalationDecision::Resume,
                due,
            )
            .unwrap();
        assert!(
            scheduler
                .prepare_artifact_verification(operation, due)
                .unwrap()
                .is_none()
        );
        assert!(
            scheduler
                .supervisor
                .load(first.supervisor_run_id)
                .unwrap()
                .unwrap()
                .verification_candidates
                .is_empty()
        );
        let retry = scheduler
            .prepare_artifact_verification_after_report(
                operation,
                Some(StructuredResult {
                    pr: Some("https://github.com/acme/repo/pull/2".into()),
                    ..StructuredResult::default()
                }),
                due,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            retry
                .result
                .as_ref()
                .and_then(|result| result.pr.as_deref()),
            Some("https://github.com/acme/repo/pull/2")
        );
        assert_eq!(
            scheduler
                .supervisor
                .load(first.supervisor_run_id)
                .unwrap()
                .unwrap()
                .verification_candidates[&TaskId::new("root").unwrap()]
                .as_deref(),
            Some("https://github.com/acme/repo/pull/2")
        );
        let retry = scheduler
            .record_artifact_expectation(&retry, &artifact_expectation(), due)
            .unwrap();
        let completed = scheduler
            .record_artifact_verification(
                &retry,
                ArtifactVerification {
                    status: ArtifactVerificationStatus::Verified,
                    result_digest: "verified".into(),
                    safe_summary: "verified".into(),
                },
                due,
            )
            .unwrap();
        assert_eq!(completed.tasks[0].state, TaskState::Succeeded);
        assert_eq!(completed.state, SupervisorRunState::Succeeded);
        assert!(
            scheduler
                .pending_artifact_verifications(now())
                .unwrap()
                .is_empty()
        );
        let late = scheduler
            .record_artifact_verification(
                &retry,
                ArtifactVerification {
                    status: ArtifactVerificationStatus::Rejected,
                    result_digest: "late-provider-result".into(),
                    safe_summary: "late provider result".into(),
                },
                due,
            )
            .unwrap();
        assert_eq!(late, completed);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One artifact fence fixture covers candidate capture and every stale request dimension.
    fn artifact_verification_preparation_captures_only_the_exact_completed_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: "goal".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, operation);
        let caller = CallerRef {
            session_id: None,
            agent_id: AgentId::new(),
        };
        let structured = StructuredResult {
            pr: Some("https://github.com/acme/repo/pull/1".into()),
            commits: vec!["abc".into()],
            changed_files: vec!["src/lib.rs".into()],
            verification: Some("candidate only".into()),
        };
        scheduler
            .dispatch
            .upsert_binding(DispatchBinding {
                run_id: operation,
                caller: caller.clone(),
                worker: WorkerRef {
                    session_id: None,
                    agent_id: AgentId::new(),
                },
            })
            .unwrap();
        scheduler
            .dispatch
            .append_inbox(
                &caller,
                InboxMessage {
                    run_id: operation,
                    from: WorkerRef {
                        session_id: None,
                        agent_id: AgentId::new(),
                    },
                    kind: InboxKind::Completed,
                    summary: "worker says complete".into(),
                    result: Some(structured.clone()),
                    created_at: now(),
                    read: false,
                },
            )
            .unwrap();
        let run = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &operation.to_string(),
                goal("finish"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        assert_eq!(run.tasks[0].state, TaskState::Dispatched);
        let second_operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: second_operation,
                agent_id: AgentId::new(),
                prompt: "another goal".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, second_operation);
        scheduler
            .start_for_workspace_root_dispatch(
                "another-goal",
                workspace,
                &second_operation.to_string(),
                goal("another finish"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let mut expected_pending = vec![operation, second_operation];
        expected_pending.sort_by_key(ToString::to_string);
        assert_eq!(
            scheduler.pending_artifact_verifications(now()).unwrap(),
            expected_pending
                .into_iter()
                .map(|dispatch_run_id| PendingArtifactVerification { dispatch_run_id })
                .collect::<Vec<_>>()
        );

        let request = scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .unwrap();
        assert_eq!(request.result, Some(structured));
        assert_eq!(request.task_id, TaskId::new("root").unwrap());
        assert_eq!(
            scheduler
                .get("goal", run.supervisor_run_id)
                .unwrap()
                .unwrap()
                .tasks[0]
                .state,
            TaskState::Verifying
        );
        assert!(
            scheduler
                .prepare_artifact_verification(OperationId::new(), now())
                .unwrap()
                .is_none()
        );

        let verified = || ArtifactVerification {
            status: ArtifactVerificationStatus::Verified,
            result_digest: "verified".into(),
            safe_summary: "verified".into(),
        };
        assert!(
            scheduler
                .record_artifact_verification(&request, verified(), now())
                .unwrap_err()
                .to_string()
                .contains("verified artifact expectation is missing")
        );
        let missing_expectation_task = ArtifactVerificationRequest {
            task_id: TaskId::new("missing").unwrap(),
            ..request.clone()
        };
        assert!(
            scheduler
                .record_artifact_expectation(
                    &missing_expectation_task,
                    &artifact_expectation(),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("task is missing")
        );
        let other_expectation = ArtifactExpectation::new(
            GitHubRepository::from_name_with_owner("other/repo").unwrap(),
            "0123456789012345678901234567890123456789",
        )
        .unwrap();
        for stale in [
            ArtifactVerificationRequest {
                generation: request.generation + 1,
                ..request.clone()
            },
            ArtifactVerificationRequest {
                contract: NO_ARTIFACT_CONTRACT,
                ..request.clone()
            },
            ArtifactVerificationRequest {
                verification_attempt: request.verification_attempt + 1,
                ..request.clone()
            },
            ArtifactVerificationRequest {
                repository: GitHubRepository::from_name_with_owner("other/repo").unwrap(),
                ..request.clone()
            },
        ] {
            assert!(
                scheduler
                    .record_artifact_expectation(&stale, &artifact_expectation(), now())
                    .unwrap_err()
                    .to_string()
                    .contains("fence is stale")
            );
        }
        assert!(
            scheduler
                .record_artifact_expectation(&request, &other_expectation, now())
                .unwrap_err()
                .to_string()
                .contains("fence is stale")
        );
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .record_artifact_expectation(&request, &artifact_expectation(), now())
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        let pinned = scheduler
            .record_artifact_expectation(&request, &artifact_expectation(), now())
            .unwrap();
        assert_eq!(
            scheduler
                .record_artifact_expectation(&pinned, &artifact_expectation(), now())
                .unwrap(),
            pinned
        );
        assert!(
            scheduler
                .record_artifact_verification(&request, verified(), now())
                .unwrap_err()
                .to_string()
                .contains("expectation fence is stale")
        );
        let future_attempt = ArtifactVerificationRequest {
            verification_attempt: pinned.verification_attempt + 1,
            ..pinned.clone()
        };
        assert!(
            scheduler
                .record_artifact_verification(&future_attempt, verified(), now())
                .unwrap_err()
                .to_string()
                .contains("attempt fence is stale")
        );
        let wrong_expectation = ArtifactVerificationRequest {
            expectation: Some(other_expectation),
            ..pinned
        };
        assert!(
            scheduler
                .record_artifact_verification(&wrong_expectation, verified(), now())
                .unwrap_err()
                .to_string()
                .contains("expectation fence is stale")
        );

        let missing_task = ArtifactVerificationRequest {
            task_id: TaskId::new("missing").unwrap(),
            ..request.clone()
        };
        assert!(
            scheduler
                .record_artifact_verification(
                    &missing_task,
                    ArtifactVerification {
                        status: ArtifactVerificationStatus::Verified,
                        result_digest: "verified".into(),
                        safe_summary: "verified".into(),
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("task is missing")
        );
        for stale in [
            ArtifactVerificationRequest {
                generation: request.generation + 1,
                ..request.clone()
            },
            ArtifactVerificationRequest {
                contract: NO_ARTIFACT_CONTRACT,
                ..request.clone()
            },
        ] {
            assert!(
                scheduler
                    .record_artifact_verification(
                        &stale,
                        ArtifactVerification {
                            status: ArtifactVerificationStatus::Verified,
                            result_digest: "verified".into(),
                            safe_summary: "verified".into(),
                        },
                        now(),
                    )
                    .unwrap_err()
                    .to_string()
                    .contains("fence is stale")
            );
        }

        let mut unexpectedly_running = scheduler
            .supervisor
            .load(run.supervisor_run_id)
            .unwrap()
            .unwrap();
        unexpectedly_running.state = SupervisorRunState::Running;
        unexpectedly_running
            .tasks
            .get_mut(&request.task_id)
            .unwrap()
            .state = TaskState::Running;
        scheduler
            .supervisor
            .initialize(&unexpectedly_running)
            .unwrap();
        assert!(
            scheduler
                .record_artifact_verification(
                    &request,
                    ArtifactVerification {
                        status: ArtifactVerificationStatus::Verified,
                        result_digest: "verified".into(),
                        safe_summary: "verified".into(),
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("fence is stale")
        );

        let mut duplicate = unexpectedly_running;
        duplicate.supervisor_run_id = SupervisorRunId::new();
        for task in duplicate.tasks.values_mut() {
            task.supervisor_run_id = duplicate.supervisor_run_id;
        }
        for provenance in duplicate.provenance.values_mut() {
            provenance.supervisor_run_id = duplicate.supervisor_run_id;
        }
        scheduler.supervisor.initialize(&duplicate).unwrap();
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap_err()
                .to_string()
                .contains("multiple supervisor runs")
        );
    }

    #[test]
    fn legacy_goal_without_pre_spawn_repository_escalates_instead_of_stalling() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, operation);
        let started = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &operation.to_string(),
                goal("finish"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let mut legacy = scheduler
            .supervisor
            .load(started.supervisor_run_id)
            .unwrap()
            .unwrap();
        legacy.artifact_repository = None;
        scheduler.supervisor.initialize(&legacy).unwrap();

        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap()
                .is_none()
        );
        let escalated = scheduler
            .get("goal", started.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(escalated.state, SupervisorRunState::Escalated);
        assert_eq!(
            escalated.escalation.unwrap().safe_evidence,
            "artifact repository was not recorded before Goal worker spawn"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One corruption fixture keeps terminal status, contracts, and provenance fences visibly related.
    fn artifact_preparation_rejects_nonterminal_wrong_contract_and_corrupt_membership() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: "goal".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, operation);
        let started = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &operation.to_string(),
                goal("finish"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let mut missing_workspace = scheduler
            .supervisor
            .load(started.supervisor_run_id)
            .unwrap()
            .unwrap();
        missing_workspace.workspace_id = None;
        scheduler.supervisor.initialize(&missing_workspace).unwrap();
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap_err()
                .to_string()
                .contains("workspace is missing")
        );
        missing_workspace.workspace_id = Some(workspace);
        scheduler.supervisor.initialize(&missing_workspace).unwrap();
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap()
                .is_none()
        );
        assert!(
            scheduler
                .pending_artifact_verifications(now())
                .unwrap()
                .is_empty()
        );

        let mut unsupported_state = scheduler
            .supervisor
            .load(started.supervisor_run_id)
            .unwrap()
            .unwrap();
        unsupported_state
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .state = TaskState::AwaitingDecision;
        scheduler.supervisor.initialize(&unsupported_state).unwrap();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: "goal".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap()
                .is_none()
        );
        assert!(
            scheduler
                .pending_artifact_verifications(now())
                .unwrap()
                .is_empty()
        );

        let mut wrong_contract = scheduler
            .supervisor
            .load(started.supervisor_run_id)
            .unwrap()
            .unwrap();
        wrong_contract
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .required_artifact_contract = NO_ARTIFACT_CONTRACT;
        scheduler.supervisor.initialize(&wrong_contract).unwrap();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: "goal".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap()
                .is_none()
        );

        let missing_dispatch = OperationId::new();
        let mut corrupt = wrong_contract;
        let corrupt_root = corrupt
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap();
        corrupt_root.required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
        corrupt_root.state = TaskState::Dispatched;
        corrupt
            .provenance
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .dispatch_run_id = missing_dispatch;
        scheduler.supervisor.initialize(&corrupt).unwrap();
        assert!(
            scheduler
                .pending_artifact_verifications(now())
                .unwrap_err()
                .to_string()
                .contains("provenance fence is stale")
        );
        assert!(
            scheduler
                .prepare_artifact_verification(missing_dispatch, now())
                .unwrap_err()
                .to_string()
                .contains("artifact dispatch is missing")
        );

        let mut missing_task = corrupt.clone();
        let provenance = missing_task
            .provenance
            .remove(&TaskId::new("root").unwrap())
            .unwrap();
        missing_task
            .provenance
            .insert(TaskId::new("missing").unwrap(), provenance);
        scheduler.supervisor.initialize(&missing_task).unwrap();
        assert!(
            scheduler
                .pending_artifact_verifications(now())
                .unwrap_err()
                .to_string()
                .contains("provenance is missing")
        );
        assert!(
            scheduler
                .prepare_artifact_verification(missing_dispatch, now())
                .unwrap_err()
                .to_string()
                .contains("supervisor task is missing")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One refusal matrix covers every fail-closed root provenance boundary.
    fn workspace_root_dispatch_refuses_invalid_missing_and_conflicting_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let worker = root_worker(workspace);
        assert!(
            scheduler
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    "not-an-operation",
                    goal("root"),
                    None,
                    &worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("operation is invalid")
        );
        let missing = OperationId::new();
        assert!(
            scheduler
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    &missing.to_string(),
                    goal("root"),
                    None,
                    &worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("dispatch does not exist")
        );
        let operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, operation);
        assert!(
            scheduler
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    &operation.to_string(),
                    goal(""),
                    None,
                    &worker,
                    now(),
                )
                .is_err()
        );
        assert!(
            scheduler
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    &operation.to_string(),
                    goal("root"),
                    None,
                    &root_worker(WorkspaceId::new()),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("outside its reserved workspace")
        );
        scheduler
            .start_for_workspace_root_dispatch(
                "caller",
                workspace,
                &operation.to_string(),
                goal("root"),
                None,
                &worker,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_root_task(
                    &operation.to_string(),
                    operation,
                    &worker,
                    NO_ARTIFACT_CONTRACT,
                    false,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("another artifact contract")
        );
        assert!(
            scheduler
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    &operation.to_string(),
                    goal("root"),
                    None,
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("provenance conflicts")
        );
        assert!(
            scheduler
                .load_started_run(SupervisorRunId::new())
                .unwrap_err()
                .to_string()
                .contains("disappeared")
        );

        let missing_root_temp = tempfile::tempdir().unwrap();
        let missing_root = SupervisorRuntime::new(missing_root_temp.path());
        let missing_root_dispatch = DispatchStore::new(missing_root_temp.path());
        let missing_root_operation = OperationId::new();
        missing_root_dispatch
            .upsert_run(DispatchRun {
                run_id: missing_root_operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&missing_root, workspace, missing_root_operation);
        missing_root.fail_apply_at(0);
        assert!(
            missing_root
                .reserve_goal_for_workspace(
                    "caller",
                    workspace,
                    &missing_root_operation.to_string(),
                    goal("root"),
                    None,
                    now(),
                )
                .is_err()
        );
        let id = missing_root.load_state().unwrap().starts[&missing_root_operation.to_string()]
            .supervisor_run_id;
        let mut incomplete = missing_root.supervisor.load(id).unwrap().unwrap();
        incomplete.state = SupervisorRunState::Running;
        missing_root.supervisor.initialize(&incomplete).unwrap();
        assert!(
            missing_root
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    &missing_root_operation.to_string(),
                    goal("root"),
                    None,
                    &worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("root task is missing")
        );
    }

    #[test]
    fn workspace_root_dispatch_propagates_each_binding_write_failure() {
        for (escalate_before_binding, failed_apply) in [(false, 2), (true, 3), (true, 4)] {
            let temp = tempfile::tempdir().unwrap();
            let scheduler = SupervisorRuntime::new(temp.path());
            let dispatch = DispatchStore::new(temp.path());
            let workspace = WorkspaceId::new();
            let operation = OperationId::new();
            dispatch
                .upsert_run(DispatchRun {
                    run_id: operation,
                    agent_id: AgentId::new(),
                    prompt: String::new(),
                    started_at: now(),
                    ended_at: None,
                    status: RunStatus::Running,
                })
                .unwrap();
            persist_root_dispatch_agent(&scheduler, workspace, operation);
            let started = scheduler
                .reserve_goal_for_workspace(
                    "caller",
                    workspace,
                    &operation.to_string(),
                    goal("root"),
                    None,
                    now(),
                )
                .unwrap();
            if escalate_before_binding {
                let reserved = scheduler
                    .supervisor
                    .load(started.supervisor_run_id)
                    .unwrap()
                    .unwrap();
                scheduler
                    .apply(
                        &reserved,
                        now(),
                        SupervisorEventSource::DispatchFailure,
                        SupervisorEventKind::Escalate {
                            task_id: Some(TaskId::new("root").unwrap()),
                            reason: MISSING_DISPATCH_ESCALATION_REASON.into(),
                            safe_evidence: "pre-fix snapshot".into(),
                            choices: vec!["resume".into(), "cancel".into()],
                        },
                    )
                    .unwrap();
            }
            scheduler.fail_apply_at(failed_apply);

            assert!(
                scheduler
                    .start_for_workspace_root_dispatch(
                        "caller",
                        workspace,
                        &operation.to_string(),
                        goal("root"),
                        None,
                        &root_worker(workspace),
                        now(),
                    )
                    .unwrap_err()
                    .to_string()
                    .contains("injected supervisor apply failure")
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One recovery inventory fixture covers every reservation class and terminal replay.
    fn pending_goal_inventory_and_definite_failure_are_exact() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();

        let unscoped_operation = OperationId::new().to_string();
        scheduler
            .start(
                "caller",
                &unscoped_operation,
                "unscoped".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        let classic_operation = OperationId::new();
        scheduler
            .start_for_workspace(
                "caller",
                workspace,
                &classic_operation.to_string(),
                "classic".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        dispatch
            .upsert_run(DispatchRun {
                run_id: classic_operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_workspace_root_dispatch(
                    &classic_operation.to_string(),
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("not a Goal run")
        );
        let classic_before = scheduler
            .get(
                "caller",
                scheduler.load_state().unwrap().starts[&classic_operation.to_string()]
                    .supervisor_run_id,
            )
            .unwrap()
            .unwrap();
        assert!(
            scheduler
                .fail_reserved_goal(
                    &classic_operation.to_string(),
                    "must not cross contracts".into(),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("not a Goal run")
        );
        assert_eq!(
            scheduler
                .get("caller", classic_before.supervisor_run_id)
                .unwrap()
                .unwrap(),
            classic_before
        );
        assert_eq!(
            scheduler
                .reserved_goal_repository(&OperationId::new().to_string())
                .unwrap(),
            None
        );

        let goal_operation = OperationId::new();
        let goal_run = scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &goal_operation.to_string(),
                goal("goal"),
                None,
                now(),
            )
            .unwrap();
        assert_eq!(
            scheduler
                .reserved_goal_repository(&goal_operation.to_string())
                .unwrap(),
            Some(artifact_repository())
        );
        assert!(
            scheduler
                .reserve_goal_for_workspace(
                    "goal",
                    workspace,
                    &goal_operation.to_string(),
                    GoalSpecification::new(
                        "goal".into(),
                        GitHubRepository::from_name_with_owner("other/repo").unwrap(),
                    ),
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("different supervisor start")
        );
        assert!(
            scheduler
                .pending_artifact_verifications(now())
                .unwrap()
                .is_empty()
        );
        let mut state = scheduler.load_state().unwrap();
        state.starts.insert(
            OperationId::new().to_string(),
            StartReservation {
                semantic_key: semantic_digest(b"orphan"),
                supervisor_run_id: SupervisorRunId::new(),
                artifact_repository: None,
                workspace_id: None,
                caller_dispatch_run_id: None,
                worker_session_id: None,
                worker_agent_id: None,
                worker_runtime_id: None,
                worker_profile_id: None,
                worker_semantic_digest: None,
            },
        );
        scheduler.save_state(&state).unwrap();
        assert_eq!(
            scheduler.pending_goal_promotions().unwrap(),
            vec![PendingGoalPromotion {
                operation_id: goal_operation.to_string(),
                reserved_at: now(),
                workspace_id: workspace,
                worker_profile_id: None,
                worker_semantic_digest: None,
            }]
        );

        // A pre-fix scheduler could escalate the root during the promotion
        // window. Definite failure must still close that durable reservation.
        let reserved = scheduler
            .supervisor
            .load(goal_run.supervisor_run_id)
            .unwrap()
            .unwrap();
        scheduler
            .apply(
                &reserved,
                now(),
                SupervisorEventSource::DispatchFailure,
                SupervisorEventKind::Escalate {
                    task_id: Some(TaskId::new("root").unwrap()),
                    reason: MISSING_DISPATCH_ESCALATION_REASON.into(),
                    safe_evidence: "pre-fix snapshot".into(),
                    choices: vec!["resume".into(), "cancel".into()],
                },
            )
            .unwrap();

        assert!(
            scheduler
                .fail_reserved_goal("missing", "failed".into(), now())
                .unwrap_err()
                .to_string()
                .contains("reservation does not exist")
        );
        let failed = scheduler
            .fail_reserved_goal(
                &goal_operation.to_string(),
                "definite failure".into(),
                now(),
            )
            .unwrap();
        assert_eq!(failed.state, SupervisorRunState::Failed);
        assert_eq!(failed.terminal_reason.as_deref(), Some("definite failure"));
        assert_eq!(
            scheduler
                .fail_reserved_goal(&goal_operation.to_string(), "ignored replay".into(), now())
                .unwrap(),
            failed
        );
        assert!(scheduler.pending_goal_promotions().unwrap().is_empty());
        assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
        assert_eq!(
            scheduler
                .get("goal", goal_run.supervisor_run_id)
                .unwrap()
                .unwrap(),
            failed
        );

        let reservation_without_dispatch = OperationId::new();
        scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &reservation_without_dispatch.to_string(),
                goal("goal"),
                None,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_workspace_root_dispatch(
                    &reservation_without_dispatch.to_string(),
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("dispatch does not exist")
        );
        let dispatch_without_reservation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: dispatch_without_reservation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_workspace_root_dispatch(
                    &dispatch_without_reservation.to_string(),
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("reservation does not exist")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One recovery fixture keeps reservation, collision, escalation, and exact replay assertions together.
    fn delegated_dispatch_is_reserved_before_spawn_and_reconciled_by_exact_operation() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        assert!(!scheduler.supervises_dispatch(root_operation).unwrap());
        dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, root_operation);
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &root_operation.to_string(),
                goal("root work"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        assert!(scheduler.supervises_dispatch(root_operation).unwrap());
        let before_oversized = scheduler
            .get("goal-composer", root.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &OperationId::new().to_string(),
                    "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
                    now(),
                )
                .is_err()
        );
        assert_eq!(
            scheduler
                .get("goal-composer", root.supervisor_run_id)
                .unwrap()
                .unwrap(),
            before_oversized
        );
        let child_operation = OperationId::new();
        let reserved = scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work",
                now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(reserved.run.tasks.len(), 2);
        assert_eq!(reserved.run.provenance.len(), 1);
        assert_eq!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work",
                    now(),
                )
                .unwrap()
                .unwrap(),
            reserved
        );
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "different child work",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("conflicts")
        );
        let pending = scheduler.pending_delegated_promotions().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].operation_id, child_operation.to_string());

        scheduler
            .tick(root.supervisor_run_id, now(), &mut Waker::default())
            .unwrap();
        assert_eq!(
            scheduler
                .get("goal-composer", root.supervisor_run_id)
                .unwrap()
                .unwrap()
                .state,
            SupervisorRunState::Running
        );
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_operation,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .attach_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work".into(),
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("has no managed session")
        );
        let worker = delegated_worker(workspace);
        let attached = scheduler
            .attach_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work".into(),
                &worker,
                now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(attached.state, SupervisorRunState::Running);
        assert!(attached.escalation.is_none());
        assert_eq!(attached.tasks.len(), 2);
        assert_eq!(attached.provenance.len(), 2);
        assert!(
            attached
                .provenance
                .iter()
                .any(|item| item.parent_dispatch_run == Some(root_operation))
        );
        assert_eq!(
            scheduler
                .attach_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work".into(),
                    &worker,
                    now(),
                )
                .unwrap()
                .unwrap(),
            attached
        );
        assert!(
            scheduler
                .attach_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work".into(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("provenance conflicts")
        );

        // A user-defined task whose ID happens to look similar is not a
        // daemon reservation because it does not carry the origin marker.
        let fake_operation = OperationId::new();
        let run = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        scheduler
            .apply(
                &run,
                now(),
                SupervisorEventSource::Admission,
                SupervisorEventKind::AddTask {
                    task: task_node(
                        &run,
                        delegated_task_id(fake_operation).unwrap(),
                        Some(TaskId::new("root").unwrap()),
                        BTreeSet::new(),
                        "ordinary task".into(),
                        NO_ARTIFACT_CONTRACT,
                    ),
                },
            )
            .unwrap();
        assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
    }

    #[test]
    fn promotion_reservations_fence_recursive_delegation_before_provenance_binding() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        let root = scheduler
            .reserve_goal_for_workspace(
                "goal-composer",
                workspace,
                &root_operation.to_string(),
                goal("root work"),
                None,
                now(),
            )
            .unwrap();

        // The Agent can start its MCP child as soon as spawn returns, before
        // the composition root has persisted exact provenance. The durable
        // promotion reservation must already classify that caller as
        // supervised, including after a daemon restart.
        let scheduler = SupervisorRuntime::new(temp.path());
        assert!(scheduler.supervises_dispatch(root_operation).unwrap());

        let child_operation = OperationId::new();
        let child = scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work",
                now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(child.run.supervisor_run_id, root.supervisor_run_id);
        assert!(child.run.provenance.is_empty());
        assert_eq!(
            child
                .run
                .tasks
                .iter()
                .find(|task| task.task_id == delegated_task_id(child_operation).unwrap())
                .unwrap()
                .parent_task_id,
            Some(TaskId::new("root").unwrap())
        );

        // A child can recursively delegate in the same post-spawn/pre-bind
        // interval. Its own reservation is the authoritative parent fence.
        assert!(scheduler.supervises_dispatch(child_operation).unwrap());
        let grandchild_operation = OperationId::new();
        let grandchild = scheduler
            .reserve_delegated_dispatch(
                child_operation,
                &grandchild_operation.to_string(),
                "grandchild work",
                now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(grandchild.run.supervisor_run_id, root.supervisor_run_id);
        assert_eq!(
            grandchild
                .run
                .tasks
                .iter()
                .find(|task| task.task_id == delegated_task_id(grandchild_operation).unwrap())
                .unwrap()
                .parent_task_id,
            Some(delegated_task_id(child_operation).unwrap())
        );
        assert!(!scheduler.supervises_dispatch(OperationId::new()).unwrap());
    }

    #[test]
    fn recursive_unbound_promotions_retain_exact_stop_fences_after_cancel() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        let run = scheduler
            .reserve_goal_for_workspace(
                "goal-composer",
                workspace,
                &root_operation.to_string(),
                goal("root work"),
                None,
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work",
                now(),
            )
            .unwrap()
            .unwrap();
        let grandchild_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                child_operation,
                &grandchild_operation.to_string(),
                "grandchild work",
                now(),
            )
            .unwrap()
            .unwrap();
        scheduler
            .control_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: run.supervisor_run_id,
                    reason: "cancel before provenance binding".into(),
                },
                now(),
            )
            .unwrap();

        let scheduler = SupervisorRuntime::new(temp.path());
        let stops = scheduler
            .pending_worker_stops_for_run(run.supervisor_run_id)
            .unwrap();
        assert_eq!(stops.len(), 3);
        let stop = |operation| {
            stops
                .iter()
                .find(|stop| stop.operation_id == operation)
                .unwrap()
        };
        assert_eq!(stop(root_operation).parent_dispatch_run, None);
        assert_eq!(
            stop(child_operation).parent_dispatch_run,
            Some(root_operation)
        );
        assert_eq!(
            stop(grandchild_operation).parent_dispatch_run,
            Some(child_operation)
        );
    }

    #[test]
    fn child_policy_denial_escalates_durably_before_task_or_agent_effect() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        let root = scheduler
            .reserve_goal_for_workspace(
                "goal-composer",
                workspace,
                &root_operation.to_string(),
                goal("root work"),
                None,
                now(),
            )
            .unwrap();
        let mut stored = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        stored.policy.max_dispatches = 1;
        json_file::write_atomic(
            scheduler
                .supervisor
                .snapshot_path(root.supervisor_run_id)
                .parent()
                .unwrap(),
            &scheduler.supervisor.snapshot_path(root.supervisor_run_id),
            &stored,
        )
        .unwrap();
        let child_operation = OperationId::new();
        scheduler.fail_apply_at(2);
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work",
                    now(),
                )
                .is_err()
        );
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("policy denied")
        );
        let escalated = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(escalated.state, SupervisorRunState::Escalated);
        assert_eq!(
            escalated.escalation.unwrap().reason,
            "dispatch budget exhausted"
        );
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("policy denied")
        );
        assert!(
            !escalated
                .tasks
                .contains_key(&delegated_task_id(child_operation).unwrap())
        );
        assert!(scheduler.dispatch.run(child_operation).unwrap().is_none());
    }

    #[test]
    fn aborted_child_stop_uses_its_reserved_parent_fence_after_parent_retry() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let parent_operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: parent_operation,
                agent_id: AgentId::new(),
                prompt: "parent".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, parent_operation);
        let run = scheduler
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &parent_operation.to_string(),
                goal("parent work"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                parent_operation,
                &child_operation.to_string(),
                "child work",
                now(),
            )
            .unwrap()
            .unwrap();

        // A retry advances the live parent generation and replaces current
        // provenance. Abort cleanup must still use the child's immutable
        // reservation fence instead of the parent's new operation.
        let mut retrying = scheduler
            .supervisor
            .load(run.supervisor_run_id)
            .unwrap()
            .unwrap();
        let root_id = TaskId::new("root").unwrap();
        let retry_operation = OperationId::new();
        let root = retrying.tasks.get_mut(&root_id).unwrap();
        root.generation = 2;
        root.assigned_dispatch_run = Some(retry_operation);
        root.state = TaskState::Dispatched;
        let mut retry_provenance =
            provenance(retrying.supervisor_run_id, &root_id, None, retry_operation);
        retry_provenance.generation = 2;
        retrying.provenance.insert(root_id, retry_provenance);
        scheduler.supervisor.initialize(&retrying).unwrap();
        scheduler
            .control_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: run.supervisor_run_id,
                    reason: "cancel after parent retry".into(),
                },
                now(),
            )
            .unwrap();

        let stops = scheduler
            .pending_worker_stops_for_run(run.supervisor_run_id)
            .unwrap();
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].operation_id, child_operation);
        assert_eq!(stops[0].parent_dispatch_run, Some(parent_operation));
    }

    #[test]
    fn promotion_authority_refuses_stale_child_and_missing_root_fences() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        let root = scheduler
            .reserve_goal_for_workspace(
                "goal-composer",
                workspace,
                &root_operation.to_string(),
                goal("root work"),
                None,
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work",
                now(),
            )
            .unwrap()
            .unwrap();
        let mut stale_child = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        stale_child
            .tasks
            .get_mut(&delegated_task_id(child_operation).unwrap())
            .unwrap()
            .assigned_dispatch_run = Some(OperationId::new());
        scheduler.supervisor.initialize(&stale_child).unwrap();
        assert!(
            scheduler
                .supervises_dispatch(child_operation)
                .unwrap_err()
                .to_string()
                .contains("stale")
        );

        let malformed = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(malformed.path());
        let operation = OperationId::new();
        let reserved = scheduler
            .reserve_goal_for_workspace(
                "goal-composer",
                WorkspaceId::new(),
                &operation.to_string(),
                goal("malformed root"),
                None,
                now(),
            )
            .unwrap();
        let mut missing_root = scheduler
            .supervisor
            .load(reserved.supervisor_run_id)
            .unwrap()
            .unwrap();
        missing_root.tasks.remove(&TaskId::new("root").unwrap());
        scheduler.supervisor.initialize(&missing_root).unwrap();
        assert!(
            scheduler
                .supervises_dispatch(operation)
                .unwrap_err()
                .to_string()
                .contains("has no authority")
        );
    }

    #[test]
    fn promotion_authority_ignores_generic_starts_and_refuses_stale_root_fences() {
        let generic = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(generic.path());
        let operation = OperationId::new();
        scheduler
            .start_for_workspace(
                "caller",
                WorkspaceId::new(),
                &operation.to_string(),
                "generic root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        assert!(!scheduler.supervises_dispatch(operation).unwrap());

        let stale = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(stale.path());
        let operation = OperationId::new();
        let reserved = scheduler
            .reserve_goal_for_workspace(
                "goal-composer",
                WorkspaceId::new(),
                &operation.to_string(),
                goal("stale root"),
                None,
                now(),
            )
            .unwrap();
        let mut run = scheduler
            .supervisor
            .load(reserved.supervisor_run_id)
            .unwrap()
            .unwrap();
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .assigned_dispatch_run = Some(OperationId::new());
        scheduler.supervisor.initialize(&run).unwrap();
        assert!(
            scheduler
                .supervises_dispatch(operation)
                .unwrap_err()
                .to_string()
                .contains("stale")
        );

        // Goal reservations written before the timestamp marker was added
        // remain authoritative through their exact start-operation mapping.
        let legacy = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(legacy.path());
        let operation = OperationId::new();
        let reserved = scheduler
            .reserve_goal_for_workspace(
                "goal-composer",
                WorkspaceId::new(),
                &operation.to_string(),
                goal("legacy root"),
                None,
                now(),
            )
            .unwrap();
        let mut run = scheduler
            .supervisor
            .load(reserved.supervisor_run_id)
            .unwrap()
            .unwrap();
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .promotion_reserved_at = None;
        scheduler.supervisor.initialize(&run).unwrap();
        assert!(scheduler.supervises_dispatch(operation).unwrap());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Committed, pending, and new-root joins share the same terminal Agent fence.
    fn terminal_agent_dispatch_cannot_delegate_or_start_a_supervisor_run() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();

        let committed_operation = OperationId::new();
        let committed_worker = root_worker(workspace);
        persist_caller_dispatch(
            &scheduler,
            workspace,
            committed_operation,
            &committed_worker,
        );
        let committed = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &committed_operation.to_string(),
                goal("committed root"),
                None,
                &committed_worker,
                now(),
            )
            .unwrap();
        let mut committed_run = scheduler
            .supervisor
            .load(committed.supervisor_run_id)
            .unwrap()
            .unwrap();
        committed_run
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .state = TaskState::Verifying;
        scheduler.supervisor.initialize(&committed_run).unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    committed_operation,
                    &OperationId::new().to_string(),
                    "late from verifying",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("closed supervisor ownership")
        );
        committed_run
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .state = TaskState::Dispatched;
        scheduler.supervisor.initialize(&committed_run).unwrap();
        let mut committed_dispatch = scheduler
            .dispatch
            .run(committed_operation)
            .unwrap()
            .unwrap();
        committed_dispatch.status = RunStatus::Completed;
        committed_dispatch.ended_at = Some(now());
        scheduler.dispatch.upsert_run(committed_dispatch).unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    committed_operation,
                    &OperationId::new().to_string(),
                    "late child",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("closed supervisor ownership")
        );

        let pending_operation = OperationId::new();
        let pending_worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, pending_operation, &pending_worker);
        scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &pending_operation.to_string(),
                goal("pending root"),
                None,
                now(),
            )
            .unwrap();
        let mut pending_dispatch = scheduler.dispatch.run(pending_operation).unwrap().unwrap();
        pending_dispatch.status = RunStatus::Failed;
        pending_dispatch.ended_at = Some(now());
        scheduler.dispatch.upsert_run(pending_dispatch).unwrap();
        assert!(
            scheduler
                .supervision_fence(pending_operation)
                .unwrap_err()
                .to_string()
                .contains("closed supervisor ownership")
        );

        let new_root_operation = OperationId::new();
        let new_root_worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, new_root_operation, &new_root_worker);
        let mut terminal_caller = scheduler.dispatch.run(new_root_operation).unwrap().unwrap();
        terminal_caller.status = RunStatus::NoReport;
        terminal_caller.ended_at = Some(now());
        scheduler.dispatch.upsert_run(terminal_caller).unwrap();
        assert!(
            scheduler
                .start_for_workspace_caller_dispatch(
                    "caller",
                    workspace,
                    &OperationId::new().to_string(),
                    "late supervisor".into(),
                    None,
                    new_root_operation,
                    &new_root_worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("closed supervisor ownership")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One sequence covers reservation, parent retry, binding, and stale replay.
    fn parent_retry_keeps_the_reserved_child_fence_and_rejects_the_old_parent() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, root_operation);
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &root_operation.to_string(),
                goal("root work"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work",
                now(),
            )
            .unwrap()
            .unwrap();
        let retry_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: retry_operation,
                agent_id: AgentId::new(),
                prompt: "retried parent".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        let mut retried = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        let root_id = TaskId::new("root").unwrap();
        let root_task = retried.tasks.get_mut(&root_id).unwrap();
        root_task.generation = 2;
        root_task.assigned_dispatch_run = Some(retry_operation);
        root_task.state = TaskState::Dispatched;
        let mut retry_provenance =
            provenance(retried.supervisor_run_id, &root_id, None, retry_operation);
        retry_provenance.generation = 2;
        retried.provenance.insert(root_id, retry_provenance);
        scheduler.supervisor.initialize(&retried).unwrap();

        assert!(
            scheduler
                .supervision_fence(root_operation)
                .unwrap_err()
                .to_string()
                .contains("stale supervisor ownership")
        );
        assert!(
            scheduler
                .supervision_fence(retry_operation)
                .unwrap()
                .is_some()
        );
        assert!(
            scheduler
                .supervision_fence(child_operation)
                .unwrap()
                .is_some()
        );

        dispatch
            .upsert_run(DispatchRun {
                run_id: child_operation,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        scheduler
            .bind_reserved_delegated_dispatch(
                &child_operation.to_string(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap()
            .unwrap();
        let bound = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            bound.provenance[&delegated_task_id(child_operation).unwrap()].parent_dispatch_run,
            Some(root_operation)
        );
        assert_eq!(
            bound.provenance[&TaskId::new("root").unwrap()].dispatch_run_id,
            retry_operation
        );
        assert!(
            scheduler
                .supervision_fence(child_operation)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One replay matrix contrasts pending, bound, legacy, and conflicting reservations.
    fn session_delegation_replays_only_the_exact_reserved_agent_and_semantics() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        let root_worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, root_operation, &root_worker);
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &root_operation.to_string(),
                goal("root"),
                None,
                &root_worker,
                now(),
            )
            .unwrap();

        let child_operation = OperationId::new();
        let child_worker = delegated_worker(workspace);
        let planned = Agent {
            agent_id: AgentId::new(),
            session_id: child_worker.session_id,
            runtime: AgentProfileId::new("claude").unwrap(),
            model: ModelSelector::new("default").unwrap(),
            status: AgentStatus::Idle,
            current_run: None,
        };
        let mut wrong_planned = planned.clone();
        wrong_planned.session_id = Some(SessionId::new());
        assert!(
            scheduler
                .reserve_delegated_dispatch_for_session(
                    root_operation,
                    &OperationId::new().to_string(),
                    "wrong worker",
                    child_worker.session_id.unwrap(),
                    &wrong_planned,
                    "worker",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("outside its Supervisor scope")
        );
        let reserved = scheduler
            .reserve_delegated_dispatch_for_session(
                root_operation,
                &child_operation.to_string(),
                "child",
                child_worker.session_id.unwrap(),
                &planned,
                "worker",
                now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            scheduler
                .reserve_delegated_dispatch_for_session(
                    root_operation,
                    &child_operation.to_string(),
                    "child",
                    child_worker.session_id.unwrap(),
                    &planned,
                    "worker",
                    now(),
                )
                .unwrap()
                .unwrap()
                .prompt,
            reserved.prompt
        );

        let mut admitted = planned.clone();
        admitted.status = AgentStatus::Running;
        admitted.current_run = Some(child_operation);
        let semantic_key = usagi_core::usecase::client::agent_dispatch_semantic_key(
            "worker",
            admitted.agent_id,
            &reserved.prompt,
        );
        scheduler
            .dispatch
            .reserve_admission_for_workspace(
                workspace,
                admitted.clone(),
                DispatchRun {
                    run_id: child_operation,
                    agent_id: admitted.agent_id,
                    prompt: reserved.prompt.clone(),
                    started_at: now(),
                    ended_at: None,
                    status: RunStatus::Running,
                },
                DispatchBinding {
                    run_id: child_operation,
                    caller: CallerRef {
                        session_id: root_worker.session_id,
                        agent_id: scheduler
                            .dispatch
                            .run(root_operation)
                            .unwrap()
                            .unwrap()
                            .agent_id,
                    },
                    worker: WorkerRef {
                        session_id: child_worker.session_id,
                        agent_id: admitted.agent_id,
                    },
                },
                AgentAdmissionReservation {
                    operation_id: child_operation,
                    semantic_key,
                    credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
                },
            )
            .unwrap();
        scheduler
            .bind_reserved_delegated_dispatch(&child_operation.to_string(), &child_worker, now())
            .unwrap()
            .unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch_for_session(
                    root_operation,
                    &child_operation.to_string(),
                    "child",
                    child_worker.session_id.unwrap(),
                    &planned,
                    "worker",
                    now(),
                )
                .unwrap()
                .is_some()
        );
        assert!(
            scheduler
                .reserve_delegated_dispatch_for_session(
                    root_operation,
                    &child_operation.to_string(),
                    "different child",
                    child_worker.session_id.unwrap(),
                    &planned,
                    "worker",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("conflicts with its existing supervisor task")
        );

        let child_dispatch = scheduler.dispatch.run(child_operation).unwrap().unwrap();
        let task_id = delegated_task_id(child_operation).unwrap();
        let bound_run = scheduler
            .unfinished_runs()
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let bound_task = bound_run.tasks.get(&task_id).unwrap();
        let digest = bound_task
            .promotion_worker_semantic_digest
            .as_ref()
            .unwrap();
        assert!(delegated_worker_matches_reservation(
            bound_run.workspace_id,
            &child_worker,
            Some(&admitted),
            bound_task,
            &child_dispatch,
            Some(digest),
        ));
        assert!(!delegated_worker_matches_reservation(
            bound_run.workspace_id,
            &child_worker,
            None,
            bound_task,
            &child_dispatch,
            Some(digest),
        ));

        let legacy_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &legacy_operation.to_string(),
                "legacy child",
                now(),
            )
            .unwrap()
            .unwrap();
        let mut legacy_run = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        legacy_run
            .tasks
            .get_mut(&delegated_task_id(legacy_operation).unwrap())
            .unwrap()
            .promotion_parent_dispatch_run = None;
        scheduler.supervisor.initialize(&legacy_run).unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &legacy_operation.to_string(),
                    "legacy child",
                    now(),
                )
                .unwrap()
                .is_some()
        );
        let legacy_worker = delegated_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, legacy_operation, &legacy_worker);
        scheduler
            .bind_reserved_delegated_dispatch(&legacy_operation.to_string(), &legacy_worker, now())
            .unwrap()
            .unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &legacy_operation.to_string(),
                    "legacy child",
                    now(),
                )
                .unwrap()
                .is_some()
        );

        let attached_operation = OperationId::new();
        let attached_worker = delegated_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, attached_operation, &attached_worker);
        assert!(
            scheduler
                .attach_delegated_dispatch(
                    root_operation,
                    &attached_operation.to_string(),
                    "attached child".into(),
                    &attached_worker,
                    now(),
                )
                .unwrap()
                .is_some()
        );
        assert!(
            scheduler
                .attach_delegated_dispatch(
                    root_operation,
                    &attached_operation.to_string(),
                    "different attached child".into(),
                    &attached_worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("existing supervisor task")
        );

        let occupied_operation = OperationId::new();
        let mut occupied_run = aborted_run(Some(workspace));
        let occupied_id = delegated_task_id(occupied_operation).unwrap();
        let mut occupied = task(occupied_run.supervisor_run_id, occupied_id.0.as_str(), None);
        occupied.state = TaskState::Cancelled;
        occupied_run.tasks.insert(occupied_id, occupied);
        scheduler.supervisor.initialize(&occupied_run).unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &occupied_operation.to_string(),
                    "occupied",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("already owns a supervisor task")
        );

        let mut stale_run = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        stale_run
            .tasks
            .get_mut(&delegated_task_id(child_operation).unwrap())
            .unwrap()
            .generation = 2;
        scheduler.supervisor.initialize(&stale_run).unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch_for_session(
                    root_operation,
                    &child_operation.to_string(),
                    "child",
                    child_worker.session_id.unwrap(),
                    &planned,
                    "worker",
                    now(),
                )
                .is_err()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Each malformed reservation isolates one delegated bind fence.
    fn delegated_binding_rejects_worker_authority_and_stale_operation_fences() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        let root_worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, root_operation, &root_worker);
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &root_operation.to_string(),
                goal("delegated bind fences"),
                None,
                &root_worker,
                now(),
            )
            .unwrap();

        let mismatched_operation = OperationId::new();
        let expected_worker = delegated_worker(workspace);
        persist_caller_dispatch(
            &scheduler,
            workspace,
            mismatched_operation,
            &expected_worker,
        );
        assert!(
            scheduler
                .attach_delegated_dispatch(
                    root_operation,
                    &mismatched_operation.to_string(),
                    "worker mismatch".into(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("reserved supervisor scope")
        );

        let missing_authority_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &missing_authority_operation.to_string(),
                "missing authority",
                now(),
            )
            .unwrap()
            .unwrap();
        let missing_authority_worker = delegated_worker(workspace);
        persist_caller_dispatch(
            &scheduler,
            workspace,
            missing_authority_operation,
            &missing_authority_worker,
        );
        let mut missing_authority_run = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        missing_authority_run
            .tasks
            .get_mut(&delegated_task_id(missing_authority_operation).unwrap())
            .unwrap()
            .promotion_reserved_at = None;
        scheduler
            .supervisor
            .initialize(&missing_authority_run)
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &missing_authority_operation.to_string(),
                    &missing_authority_worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("promotion authority is missing")
        );

        let stale_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &stale_operation.to_string(),
                "stale authority",
                now(),
            )
            .unwrap()
            .unwrap();
        let stale_worker = delegated_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, stale_operation, &stale_worker);
        let mut stale_run = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        let stale_id = delegated_task_id(stale_operation).unwrap();
        let other_operation = OperationId::new();
        let stale_task = stale_run.tasks.get_mut(&stale_id).unwrap();
        stale_task.state = TaskState::Dispatched;
        stale_task.assigned_dispatch_run = Some(other_operation);
        stale_task.promotion_reserved_at = None;
        let root_id = TaskId::new("root").unwrap();
        let mut stale_provenance = provenance(
            stale_run.supervisor_run_id,
            &stale_id,
            Some((&root_id, root_operation)),
            other_operation,
        );
        stale_provenance.worker_session_id = stale_worker.session_id;
        stale_provenance.worker_agent_id = stale_worker.agent_runtime_id;
        stale_provenance.worker_worktree_id = stale_worker.terminal.worktree_id;
        stale_run
            .provenance
            .insert(stale_id.clone(), stale_provenance);
        scheduler.supervisor.initialize(&stale_run).unwrap();
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &stale_operation.to_string(),
                    &stale_worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("promotion fence is stale")
        );
    }

    #[test]
    fn one_dispatch_never_moves_between_retained_supervisor_roots() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let dispatch_operation = OperationId::new();
        let worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, dispatch_operation, &worker);
        let first_operation = OperationId::new().to_string();
        let first = scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &first_operation,
                "first work".into(),
                None,
                dispatch_operation,
                &worker,
                now(),
            )
            .unwrap();
        scheduler
            .bind_reserved_caller_dispatch(&first_operation, dispatch_operation, &worker, now())
            .unwrap();

        scheduler
            .ensure_supervisor_start_dispatch_available(&first_operation, dispatch_operation)
            .unwrap();
        scheduler
            .bind_reserved_caller_dispatch(&first_operation, dispatch_operation, &worker, now())
            .unwrap();
        let second_operation = OperationId::new().to_string();
        assert!(
            scheduler
                .ensure_supervisor_start_dispatch_available(&second_operation, dispatch_operation,)
                .unwrap_err()
                .to_string()
                .contains("another retained supervisor run")
        );
        assert_eq!(scheduler.list_workspace(workspace).unwrap().len(), 1);

        scheduler
            .control_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: first.supervisor_run_id,
                    reason: "first finished".into(),
                },
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .supervision_fence(dispatch_operation)
                .unwrap_err()
                .to_string()
                .contains("stale supervisor ownership")
        );
        scheduler
            .ensure_supervisor_start_dispatch_available(&first_operation, dispatch_operation)
            .unwrap();
        scheduler
            .bind_reserved_caller_dispatch(&first_operation, dispatch_operation, &worker, now())
            .unwrap();
        assert!(
            scheduler
                .ensure_supervisor_start_dispatch_available(&second_operation, dispatch_operation)
                .unwrap_err()
                .to_string()
                .contains("another retained supervisor run")
        );
        assert_eq!(scheduler.list_workspace(workspace).unwrap().len(), 1);
    }

    #[test]
    fn supervised_parent_rejects_duplicate_pending_and_historical_owners() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let pending_operation = OperationId::new();
        let pending_worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, pending_operation, &pending_worker);
        let pending_start = OperationId::new().to_string();
        scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &pending_start,
                "pending".into(),
                None,
                pending_operation,
                &pending_worker,
                now(),
            )
            .unwrap();
        let mut state = scheduler.load_state().unwrap();
        let reservation = state.starts[&pending_start].clone();
        state
            .starts
            .insert(OperationId::new().to_string(), reservation);
        scheduler.save_state(&state).unwrap();
        assert!(
            scheduler
                .supervision_fence(pending_operation)
                .unwrap_err()
                .to_string()
                .contains("multiple promotion reservations")
        );

        let retained_temp = tempfile::tempdir().unwrap();
        let retained = SupervisorRuntime::new(retained_temp.path());
        let operation = OperationId::new();
        let worker = root_worker(workspace);
        persist_caller_dispatch(&retained, workspace, operation, &worker);
        retained
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &operation.to_string(),
                goal("active"),
                None,
                &worker,
                now(),
            )
            .unwrap();
        let mut historical = aborted_run(Some(workspace));
        let root_id = TaskId::new("root").unwrap();
        let mut root = task(historical.supervisor_run_id, "root", None);
        root.state = TaskState::Cancelled;
        root.assigned_dispatch_run = Some(operation);
        historical.tasks.insert(root_id.clone(), root);
        historical.provenance.insert(
            root_id.clone(),
            provenance(historical.supervisor_run_id, &root_id, None, operation),
        );
        retained.supervisor.initialize(&historical).unwrap();
        assert!(
            retained
                .supervision_fence(operation)
                .unwrap_err()
                .to_string()
                .contains("conflicting retained supervisor ownership")
        );

        let malformed_temp = tempfile::tempdir().unwrap();
        let malformed = SupervisorRuntime::new(malformed_temp.path());
        let malformed_dispatch = OperationId::new();
        let malformed_run = aborted_run(Some(workspace));
        malformed.supervisor.initialize(&malformed_run).unwrap();
        let mut malformed_state = RuntimeState::default();
        malformed_state.starts.insert(
            OperationId::new().to_string(),
            caller_start_reservation(
                malformed_run.supervisor_run_id,
                workspace,
                malformed_dispatch,
            ),
        );
        malformed.save_state(&malformed_state).unwrap();
        assert!(
            malformed
                .supervision_fence(malformed_dispatch)
                .unwrap_err()
                .to_string()
                .contains("root reservation is malformed")
        );
    }

    #[test]
    fn caller_dispatch_reservation_survives_partial_start_and_blocks_another_root() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let dispatch_operation = OperationId::new();
        let worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, dispatch_operation, &worker);
        let first_operation = OperationId::new().to_string();
        scheduler.fail_apply_at(1);
        assert!(
            scheduler
                .start_for_workspace_caller_dispatch(
                    "caller",
                    workspace,
                    &first_operation,
                    "first work".into(),
                    None,
                    dispatch_operation,
                    &worker,
                    now(),
                )
                .is_err()
        );

        let second_operation = OperationId::new().to_string();
        assert!(
            scheduler
                .start_for_workspace_caller_dispatch(
                    "caller",
                    workspace,
                    &second_operation,
                    "second work".into(),
                    None,
                    dispatch_operation,
                    &worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("another retained supervisor run")
        );

        let recovered = scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &first_operation,
                "first work".into(),
                None,
                dispatch_operation,
                &worker,
                now(),
            )
            .unwrap();
        let pending = scheduler.pending_caller_promotions().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].start_operation_id, first_operation);
        assert_eq!(
            pending[0].dispatch_operation_id,
            dispatch_operation.to_string()
        );

        scheduler
            .bind_reserved_caller_dispatch(
                &pending[0].start_operation_id,
                dispatch_operation,
                &worker,
                now(),
            )
            .unwrap();
        assert!(scheduler.pending_caller_promotions().unwrap().is_empty());
        assert_eq!(
            scheduler
                .get("caller", recovered.supervisor_run_id)
                .unwrap()
                .unwrap()
                .tasks[0]
                .state,
            TaskState::Dispatched
        );
    }

    #[test]
    fn legacy_pending_caller_root_still_consumes_a_child_policy_slot() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let caller_operation = OperationId::new();
        let worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, caller_operation, &worker);
        let started = scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &OperationId::new().to_string(),
                "legacy pending root".into(),
                None,
                caller_operation,
                &worker,
                now(),
            )
            .unwrap();
        let mut run = scheduler
            .supervisor
            .load(started.supervisor_run_id)
            .unwrap()
            .unwrap();
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .promotion_reserved_at = None;
        scheduler.supervisor.initialize(&run).unwrap();

        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    caller_operation,
                    &OperationId::new().to_string(),
                    "child",
                    now(),
                )
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn caller_dispatch_failure_closes_only_the_exact_generic_root_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let caller_operation = OperationId::new();
        let worker = root_worker(workspace);
        persist_caller_dispatch(&scheduler, workspace, caller_operation, &worker);
        let start_operation = OperationId::new().to_string();
        let started = scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &start_operation,
                "work".into(),
                None,
                caller_operation,
                &worker,
                now(),
            )
            .unwrap();

        scheduler.fail_apply_at(2);
        assert!(
            scheduler
                .fail_reserved_caller_dispatch(&start_operation, "write failed".into(), now())
                .is_err()
        );
        let failed = scheduler
            .fail_reserved_caller_dispatch(&start_operation, "spawn failed".into(), now())
            .unwrap();
        assert_eq!(failed.state, SupervisorRunState::Failed);
        assert!(scheduler.pending_caller_promotions().unwrap().is_empty());
        assert_eq!(
            scheduler
                .fail_reserved_caller_dispatch(&start_operation, "replay".into(), now())
                .unwrap()
                .supervisor_run_id,
            started.supervisor_run_id
        );
        assert!(
            scheduler
                .fail_reserved_caller_dispatch(
                    &OperationId::new().to_string(),
                    "missing".into(),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("reservation does not exist")
        );

        let malformed_operation = OperationId::new();
        let malformed_worker = root_worker(workspace);
        persist_caller_dispatch(
            &scheduler,
            workspace,
            malformed_operation,
            &malformed_worker,
        );
        let malformed_start = OperationId::new().to_string();
        let malformed = scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &malformed_start,
                "malformed".into(),
                None,
                malformed_operation,
                &malformed_worker,
                now(),
            )
            .unwrap();
        let mut run = scheduler
            .supervisor
            .load(malformed.supervisor_run_id)
            .unwrap()
            .unwrap();
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
        scheduler.supervisor.initialize(&run).unwrap();
        assert!(
            scheduler
                .fail_reserved_caller_dispatch(&malformed_start, "malformed".into(), now())
                .unwrap_err()
                .to_string()
                .contains("not a caller-root run")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One durable-state matrix covers every partial caller-root phase.
    fn pending_caller_inventory_skips_partial_phases_and_rejects_stale_shapes() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &OperationId::new().to_string(),
                goal("goal is not a caller root"),
                None,
                now(),
            )
            .unwrap();
        assert!(scheduler.pending_caller_promotions().unwrap().is_empty());

        let missing_dispatch = OperationId::new();
        let missing_run_id = SupervisorRunId::new();
        let mut state = scheduler.load_state().unwrap();
        let missing_start = OperationId::new().to_string();
        state.starts.insert(
            missing_start.clone(),
            caller_start_reservation(missing_run_id, workspace, missing_dispatch),
        );
        scheduler.save_state(&state).unwrap();
        assert!(scheduler.pending_caller_promotions().unwrap().is_empty());
        scheduler
            .ensure_supervisor_start_dispatch_available(&missing_start, missing_dispatch)
            .unwrap();
        let expired_dispatch = OperationId::new();
        state.expired_starts.insert(&expired_dispatch.to_string());
        scheduler.save_state(&state).unwrap();
        assert!(
            scheduler
                .ensure_supervisor_start_dispatch_available(
                    &OperationId::new().to_string(),
                    expired_dispatch,
                )
                .unwrap_err()
                .to_string()
                .contains("retained supervisor run")
        );

        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        run.workspace_id = Some(workspace);
        scheduler.supervisor.initialize(&run).unwrap();
        let start_operation = OperationId::new().to_string();
        state = scheduler.load_state().unwrap();
        state.starts.insert(
            start_operation.clone(),
            caller_start_reservation(run.supervisor_run_id, workspace, OperationId::new()),
        );
        scheduler.save_state(&state).unwrap();
        assert!(scheduler.pending_caller_promotions().unwrap().is_empty());
        let retained_dispatch = state.starts[&start_operation]
            .caller_dispatch_run_id
            .unwrap();
        assert_eq!(
            scheduler
                .retained_dispatch_owners(&state, retained_dispatch)
                .unwrap()
                .len(),
            1
        );

        run.state = SupervisorRunState::Running;
        scheduler.supervisor.initialize(&run).unwrap();
        assert!(
            scheduler
                .retained_dispatch_owners(&state, retained_dispatch)
                .unwrap_err()
                .to_string()
                .contains("reservation is malformed")
        );
        assert!(
            scheduler
                .pending_caller_promotions()
                .unwrap_err()
                .to_string()
                .contains("root task is missing")
        );

        let mut root = task(run.supervisor_run_id, "root", None);
        root.state = TaskState::Ready;
        run.tasks.insert(root.task_id.clone(), root);
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
        run.artifact_repository = Some(artifact_repository());
        scheduler.supervisor.initialize(&run).unwrap();
        assert!(
            scheduler
                .retained_dispatch_owners(&state, retained_dispatch)
                .unwrap_err()
                .to_string()
                .contains("reservation is malformed")
        );
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .required_artifact_contract = NO_ARTIFACT_CONTRACT;
        run.artifact_repository = None;
        scheduler.supervisor.initialize(&run).unwrap();
        state = scheduler.load_state().unwrap();
        state.starts.get_mut(&start_operation).unwrap().workspace_id = Some(WorkspaceId::new());
        scheduler.save_state(&state).unwrap();
        assert!(
            scheduler
                .pending_caller_promotions()
                .unwrap_err()
                .to_string()
                .contains("workspace fence is stale")
        );

        state.starts.get_mut(&start_operation).unwrap().workspace_id = Some(workspace);
        scheduler.save_state(&state).unwrap();
        run.tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .parent_task_id = Some(TaskId::new("parent").unwrap());
        scheduler.supervisor.initialize(&run).unwrap();
        assert!(
            scheduler
                .pending_caller_promotions()
                .unwrap_err()
                .to_string()
                .contains("reservation is malformed")
        );
    }

    #[test]
    fn missing_goal_snapshot_keeps_its_operation_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let operation = OperationId::new();
        let workspace = WorkspaceId::new();
        let specification = goal("partial goal");
        let reserved = scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &operation.to_string(),
                specification.clone(),
                None,
                now(),
            )
            .unwrap();
        let restarted_temp = tempfile::tempdir().unwrap();
        let restarted = SupervisorRuntime::new(restarted_temp.path());
        restarted
            .save_state(&scheduler.load_state().unwrap())
            .unwrap();

        assert!(
            restarted
                .supervision_fence(operation)
                .unwrap_err()
                .to_string()
                .contains("stale supervisor ownership")
        );
        assert!(
            restarted
                .ensure_supervisor_start_dispatch_available(
                    &OperationId::new().to_string(),
                    operation,
                )
                .unwrap_err()
                .to_string()
                .contains("another retained supervisor run")
        );
        assert_eq!(
            restarted
                .reserved_goal_repository(&operation.to_string())
                .unwrap(),
            Some(artifact_repository())
        );
        let replay = restarted
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &operation.to_string(),
                specification,
                None,
                now(),
            )
            .unwrap();
        assert_eq!(replay.supervisor_run_id, reserved.supervisor_run_id);

        let mut legacy_state = scheduler.load_state().unwrap();
        let legacy_reservation = legacy_state.starts.get_mut(&operation.to_string()).unwrap();
        legacy_reservation.artifact_repository = None;
        legacy_reservation.workspace_id = None;
        let legacy_temp = tempfile::tempdir().unwrap();
        let legacy = SupervisorRuntime::new(legacy_temp.path());
        legacy.save_state(&legacy_state).unwrap();
        assert_eq!(
            legacy
                .reserved_goal_repository(&operation.to_string())
                .unwrap(),
            None
        );
        let legacy_replay = legacy
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &operation.to_string(),
                goal("partial goal"),
                None,
                now(),
            )
            .unwrap();
        assert_eq!(legacy_replay.supervisor_run_id, reserved.supervisor_run_id);

        let mut conflicting_state = scheduler.load_state().unwrap();
        conflicting_state
            .starts
            .get_mut(&operation.to_string())
            .unwrap()
            .artifact_repository =
            Some(GitHubRepository::from_name_with_owner("other/repository").unwrap());
        scheduler.save_state(&conflicting_state).unwrap();
        assert!(
            scheduler
                .reserved_goal_repository(&operation.to_string())
                .unwrap_err()
                .to_string()
                .contains("conflicts with its durable run")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Durable replay compares each field independently, including legacy backfill phases.
    fn start_reservation_replay_validates_and_backfills_every_identity_field() {
        let workspace = WorkspaceId::new();

        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let operation = OperationId::new().to_string();
        scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &operation,
                goal("workspace fence"),
                None,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .reserve_goal_for_workspace(
                    "goal",
                    WorkspaceId::new(),
                    &operation,
                    goal("workspace fence"),
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("different workspace")
        );

        let mut state = scheduler.load_state().unwrap();
        state.starts.get_mut(&operation).unwrap().workspace_id = None;
        scheduler.save_state(&state).unwrap();
        assert!(
            scheduler
                .reserve_goal_for_workspace(
                    "goal",
                    WorkspaceId::new(),
                    &operation,
                    goal("workspace fence"),
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("different workspace")
        );
        let mut state = scheduler.load_state().unwrap();
        state
            .starts
            .get_mut(&operation)
            .unwrap()
            .artifact_repository = None;
        scheduler.save_state(&state).unwrap();
        scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &operation,
                goal("workspace fence"),
                None,
                now(),
            )
            .unwrap();

        let repository_temp = tempfile::tempdir().unwrap();
        let repository = SupervisorRuntime::new(repository_temp.path());
        let repository_operation = OperationId::new().to_string();
        let repository_run = repository
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &repository_operation,
                goal("repository fence"),
                None,
                now(),
            )
            .unwrap();
        let mut repository_state = repository.load_state().unwrap();
        repository_state
            .starts
            .get_mut(&repository_operation)
            .unwrap()
            .artifact_repository = None;
        repository.save_state(&repository_state).unwrap();
        let mut repository_snapshot = repository
            .supervisor
            .load(repository_run.supervisor_run_id)
            .unwrap()
            .unwrap();
        repository_snapshot.artifact_repository =
            Some(GitHubRepository::from_name_with_owner("other/repository").unwrap());
        repository
            .supervisor
            .initialize(&repository_snapshot)
            .unwrap();
        assert!(
            repository
                .reserve_goal_for_workspace(
                    "goal",
                    workspace,
                    &repository_operation,
                    goal("repository fence"),
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("different artifact repository")
        );

        let profile_temp = tempfile::tempdir().unwrap();
        let profile = SupervisorRuntime::new(profile_temp.path());
        let profile_operation = OperationId::new().to_string();
        profile
            .reserve_goal_for_workspace_with_profile(
                "goal",
                workspace,
                &profile_operation,
                goal("profile fence"),
                AgentProfileId::new("claude").unwrap(),
                "digest-a".into(),
                None,
                now(),
            )
            .unwrap();
        assert!(
            profile
                .reserve_goal_for_workspace_with_profile(
                    "goal",
                    workspace,
                    &profile_operation,
                    goal("profile fence"),
                    AgentProfileId::new("codex").unwrap(),
                    "digest-a".into(),
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("different Agent runtime")
        );
        assert!(
            profile
                .reserve_goal_for_workspace_with_profile(
                    "goal",
                    workspace,
                    &profile_operation,
                    goal("profile fence"),
                    AgentProfileId::new("claude").unwrap(),
                    "digest-b".into(),
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("different Agent intent")
        );
        let mut profile_state = profile.load_state().unwrap();
        profile_state
            .starts
            .get_mut(&profile_operation)
            .unwrap()
            .artifact_repository =
            Some(GitHubRepository::from_name_with_owner("other/repository").unwrap());
        profile.save_state(&profile_state).unwrap();
        assert!(
            profile
                .reserve_goal_for_workspace_with_profile(
                    "goal",
                    workspace,
                    &profile_operation,
                    goal("profile fence"),
                    AgentProfileId::new("claude").unwrap(),
                    "digest-a".into(),
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("different artifact repository")
        );

        let caller_temp = tempfile::tempdir().unwrap();
        let caller = SupervisorRuntime::new(caller_temp.path());
        let first_dispatch = OperationId::new();
        let first_worker = root_worker(workspace);
        persist_caller_dispatch(&caller, workspace, first_dispatch, &first_worker);
        let caller_start = OperationId::new().to_string();
        caller
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &caller_start,
                "caller fence".into(),
                None,
                first_dispatch,
                &first_worker,
                now(),
            )
            .unwrap();
        caller
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &caller_start,
                "caller fence".into(),
                None,
                first_dispatch,
                &first_worker,
                now(),
            )
            .unwrap();
        let second_dispatch = OperationId::new();
        let second_worker = root_worker(workspace);
        persist_caller_dispatch(&caller, workspace, second_dispatch, &second_worker);
        assert!(
            caller
                .start_for_workspace_caller_dispatch(
                    "caller",
                    workspace,
                    &caller_start,
                    "caller fence".into(),
                    None,
                    second_dispatch,
                    &second_worker,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("different caller dispatch")
        );

        let backfill_temp = tempfile::tempdir().unwrap();
        let backfill = SupervisorRuntime::new(backfill_temp.path());
        let backfill_start = OperationId::new().to_string();
        backfill
            .start_for_workspace(
                "caller",
                workspace,
                &backfill_start,
                "legacy root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        let backfill_dispatch = OperationId::new();
        let backfill_worker = root_worker(workspace);
        persist_caller_dispatch(&backfill, workspace, backfill_dispatch, &backfill_worker);
        backfill
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &backfill_start,
                "legacy root".into(),
                None,
                backfill_dispatch,
                &backfill_worker,
                now(),
            )
            .unwrap();
        let backfilled = backfill.load_state().unwrap();
        assert_eq!(
            backfilled.starts[&backfill_start].caller_dispatch_run_id,
            Some(backfill_dispatch)
        );

        let missing_temp = tempfile::tempdir().unwrap();
        let missing = SupervisorRuntime::new(missing_temp.path());
        let mut missing_state = backfill.load_state().unwrap();
        let missing_reservation = missing_state.starts.get_mut(&backfill_start).unwrap();
        missing_reservation.workspace_id = None;
        missing_reservation.caller_dispatch_run_id = None;
        missing_reservation.worker_session_id = None;
        missing_reservation.worker_agent_id = None;
        missing_reservation.worker_runtime_id = None;
        missing_reservation.worker_profile_id = None;
        missing_reservation.worker_semantic_digest = None;
        missing.save_state(&missing_state).unwrap();
        assert!(
            missing
                .start_for_workspace(
                    "caller",
                    workspace,
                    &backfill_start,
                    "legacy root".into(),
                    Vec::new(),
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("no durable workspace authority")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One durable fixture proves report capture, prompt inheritance, restart replay, and classic isolation together.
    fn completed_child_handoff_is_durable_and_inherited_by_later_delegations() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, root_operation);
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &root_operation.to_string(),
                goal("ship the authentication change"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();

        let first_operation = OperationId::new();
        let first = scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &first_operation.to_string(),
                "inspect the authentication flow",
                now(),
            )
            .unwrap()
            .unwrap();
        let first_prompt = first.prompt.clone();
        assert!(first.prompt.contains("ship the authentication change"));
        assert!(first.prompt.contains("inspect the authentication flow"));
        assert!(
            first
                .prompt
                .contains("none recorded before this delegation")
        );

        let caller = CallerRef {
            session_id: None,
            agent_id: AgentId::new(),
        };
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: first_operation,
                agent_id: AgentId::new(),
                prompt: first.prompt,
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        scheduler
            .dispatch
            .upsert_binding(DispatchBinding {
                run_id: first_operation,
                caller: caller.clone(),
                worker: WorkerRef {
                    session_id: Some(SessionId::new()),
                    agent_id: AgentId::new(),
                },
            })
            .unwrap();
        scheduler
            .dispatch
            .append_inbox(
                &caller,
                InboxMessage {
                    run_id: first_operation,
                    from: WorkerRef {
                        session_id: Some(SessionId::new()),
                        agent_id: AgentId::new(),
                    },
                    kind: InboxKind::Completed,
                    summary: "Mapped OAuth callbacks\nwithout copying a transcript\u{202e}".into(),
                    result: Some(StructuredResult {
                        pr: Some("https://github.com/acme/repo/pull/42".into()),
                        commits: vec!["abc123".into()],
                        changed_files: vec!["src/auth.rs".into()],
                        verification: Some("targeted tests pass".into()),
                    }),
                    created_at: now(),
                    read: false,
                },
            )
            .unwrap();
        scheduler
            .attach_delegated_dispatch(
                root_operation,
                &first_operation.to_string(),
                "inspect the authentication flow".into(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap();
        scheduler
            .tick(root.supervisor_run_id, now(), &mut Waker::default())
            .unwrap();

        let stored = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.handoff_context.len(), 1);
        let terminal_replay = scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &first_operation.to_string(),
                "inspect the authentication flow",
                now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(terminal_replay.prompt, first_prompt);
        assert_eq!(
            stored.handoff_context[0].summary,
            "Mapped OAuth callbacks without copying a transcript"
        );
        assert!(
            stored.handoff_context[0]
                .artifacts
                .as_deref()
                .unwrap()
                .contains("src/auth.rs")
        );

        let second_operation = OperationId::new();
        let second = scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &second_operation.to_string(),
                "implement the callback handler",
                now(),
            )
            .unwrap()
            .unwrap();
        assert!(second.prompt.contains("ship the authentication change"));
        assert!(
            second
                .prompt
                .contains("Mapped OAuth callbacks without copying a transcript")
        );
        assert!(
            second
                .prompt
                .contains("https://github.com/acme/repo/pull/42")
        );
        assert!(second.prompt.contains("abc123"));
        assert!(second.prompt.contains("src/auth.rs"));
        assert!(second.prompt.contains("implement the callback handler"));
        assert!(!second.prompt.contains('\u{202e}'));
        let suffix = delegated_task_suffix(second_operation, "implement the callback handler");
        assert!(second.prompt.len() <= MAX_HANDOFF_PROMPT_BYTES + suffix.len());

        let restarted = SupervisorRuntime::new(temp.path());
        let replay = restarted
            .reserve_delegated_dispatch(
                root_operation,
                &second_operation.to_string(),
                "implement the callback handler",
                now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(replay.prompt, second.prompt);
        assert!(
            restarted
                .reserve_delegated_dispatch(
                    OperationId::new(),
                    &OperationId::new().to_string(),
                    "classic delegation",
                    now(),
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn handoff_rendering_bounds_and_sanitizes_every_report_shape() {
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let mut root = task(run.supervisor_run_id, "root", None);
        root.instruction_body = "é".repeat(MAX_HANDOFF_ROOT_GOAL_BYTES);
        run.tasks.insert(root.task_id.clone(), root);
        for index in 0..MAX_HANDOFF_CONTEXT_ENTRIES {
            let outcome = match index {
                63 => InboxKind::NoReport,
                62 => InboxKind::Failed,
                _ => InboxKind::Completed,
            };
            run.handoff_context.push(HandoffContextEntry {
                task_id: TaskId::new(format!("child-{index}")).unwrap(),
                generation: 1,
                dispatch_run_id: OperationId::new(),
                outcome,
                summary: "s".repeat(MAX_HANDOFF_SUMMARY_BYTES),
                artifacts: (index != 63).then_some("a".repeat(MAX_HANDOFF_ARTIFACT_BYTES)),
                recorded_at: now(),
            });
        }
        let operation = OperationId::new();
        let instruction = "continue with the bounded context";
        let prompt = delegated_handoff_prompt(&run, operation, instruction);
        let suffix = delegated_task_suffix(operation, instruction);
        assert!(prompt.contains("[failed]"));
        assert!(prompt.contains("[no-report]"));
        assert!(prompt.contains("older reports omitted"));
        assert!(prompt.ends_with(&suffix));
        assert!(prompt.len() - suffix.len() <= MAX_HANDOFF_PROMPT_BYTES);

        let mut full = "full".to_owned();
        let full_len = full.len();
        push_bounded_handoff(&mut full, "ignored", full_len);
        assert_eq!(full, "full");
        let mut multibyte = String::new();
        push_bounded_handoff(&mut multibyte, "aébcdef", 5);
        assert_eq!(multibyte, "a…");
        let mut tiny = "xx".to_owned();
        push_bounded_handoff(&mut tiny, "yy", 3);
        assert_eq!(tiny, "xx");

        assert_eq!(
            compact_handoff_text("\n\u{202e}", 16),
            "worker supplied no safe summary text"
        );
        assert_eq!(compact_handoff_text("abc d", 3), "…");
        assert_eq!(compact_handoff_text("aébcd", 5), "a…");
        assert_eq!(
            compact_handoff_text("abcd", 2),
            "worker supplied no safe summary text"
        );
        let many = StructuredResult {
            pr: None,
            commits: vec!["commit".into(); 9],
            changed_files: vec!["file".into(); 10],
            verification: None,
        };
        let artifacts = structured_artifact_summary(&many).unwrap();
        assert!(artifacts.contains("+1 omitted"));
        assert!(artifacts.contains("+2 omitted"));
        assert!(structured_artifact_summary(&StructuredResult::default()).is_none());
    }

    #[test]
    fn handoff_capture_propagates_a_corrupt_dispatch_store() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let task_id = TaskId::new("child").unwrap();
        let dispatch_run_id = OperationId::new();
        let mut child = task(run.supervisor_run_id, "child", None);
        child.state = TaskState::Succeeded;
        child.assigned_dispatch_run = Some(dispatch_run_id);
        run.tasks.insert(task_id.clone(), child);
        let provenance = provenance(run.supervisor_run_id, &task_id, None, dispatch_run_id);
        run.provenance.insert(task_id.clone(), provenance.clone());

        let mut ignored = run.clone();
        ignored.tasks.get_mut(&task_id).unwrap().state = TaskState::Running;
        assert!(
            scheduler
                .record_terminal_handoff(
                    ignored,
                    &task_id,
                    &provenance,
                    InboxKind::Completed,
                    now(),
                )
                .unwrap()
                .handoff_context
                .is_empty()
        );
        let mut stale_generation = provenance.clone();
        stale_generation.generation += 1;
        assert!(
            scheduler
                .record_terminal_handoff(
                    run.clone(),
                    &task_id,
                    &stale_generation,
                    InboxKind::Completed,
                    now(),
                )
                .unwrap()
                .handoff_context
                .is_empty()
        );
        let mut stale_assignment = run.clone();
        stale_assignment
            .tasks
            .get_mut(&task_id)
            .unwrap()
            .assigned_dispatch_run = Some(OperationId::new());
        assert!(
            scheduler
                .record_terminal_handoff(
                    stale_assignment,
                    &task_id,
                    &provenance,
                    InboxKind::Completed,
                    now(),
                )
                .unwrap()
                .handoff_context
                .is_empty()
        );
        let mut already_recorded = run.clone();
        already_recorded.handoff_context.push(HandoffContextEntry {
            task_id: task_id.clone(),
            generation: provenance.generation,
            dispatch_run_id,
            outcome: InboxKind::Completed,
            summary: "already captured".into(),
            artifacts: None,
            recorded_at: now(),
        });
        assert_eq!(
            scheduler
                .record_terminal_handoff(
                    already_recorded,
                    &task_id,
                    &provenance,
                    InboxKind::Completed,
                    now(),
                )
                .unwrap()
                .handoff_context
                .len(),
            1
        );

        run.tasks.get_mut(&task_id).unwrap().state = TaskState::Verifying;
        std::fs::write(scheduler.dispatch.registry_path(), "broken").unwrap();

        assert!(
            scheduler
                .record_terminal_handoff(run, &task_id, &provenance, InboxKind::Completed, now(),)
                .is_err()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One fail-closed matrix exercises the durable child join boundary.
    fn delegated_dispatch_refuses_missing_malformed_and_ambiguous_reservations() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, root_operation);
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &root_operation.to_string(),
                goal("root"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();

        let classic_operation = OperationId::new();
        assert_eq!(
            scheduler
                .reserve_delegated_dispatch(
                    classic_operation,
                    &classic_operation.to_string(),
                    "classic child",
                    now(),
                )
                .unwrap(),
            None
        );
        let before_identity_reuse = scheduler
            .get("goal", root.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &root_operation.to_string(),
                    "recursive self reuse",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("must differ")
        );
        assert_eq!(
            scheduler
                .get("goal", root.supervisor_run_id)
                .unwrap()
                .unwrap(),
            before_identity_reuse
        );

        assert!(
            scheduler
                .reserve_delegated_dispatch(root_operation, "invalid", "child", now())
                .unwrap_err()
                .to_string()
                .contains("operation is invalid")
        );
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    "invalid",
                    String::from("child"),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("operation is invalid")
        );
        let outside = OperationId::new();
        assert!(
            scheduler
                .reserve_delegated_dispatch(root_operation, &outside.to_string(), "", now(),)
                .unwrap_err()
                .to_string()
                .contains("expected 1..=")
        );
        assert_eq!(
            scheduler
                .reserve_delegated_dispatch(
                    OperationId::new(),
                    &outside.to_string(),
                    "child",
                    now(),
                )
                .unwrap(),
            None
        );
        assert_eq!(
            scheduler
                .attach_delegated_dispatch(
                    OperationId::new(),
                    &outside.to_string(),
                    "child".into(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap(),
            None
        );
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch("invalid", &delegated_worker(workspace), now())
                .unwrap_err()
                .to_string()
                .contains("operation is invalid")
        );
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &outside.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("dispatch does not exist")
        );
        dispatch
            .upsert_run(DispatchRun {
                run_id: outside,
                agent_id: AgentId::new(),
                prompt: "outside".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &outside.to_string(),
                    "reused dispatch",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("already in use")
        );
        assert_eq!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &outside.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap(),
            None
        );
        let generic_operation = OperationId::new();
        scheduler
            .start_for_workspace(
                "generic",
                workspace,
                &generic_operation.to_string(),
                "generic work".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &generic_operation.to_string(),
                    "reused start",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("already in use")
        );
        assert!(
            scheduler
                .fail_reserved_delegated_dispatch("invalid", now())
                .unwrap_err()
                .to_string()
                .contains("operation is invalid")
        );
        assert_eq!(
            scheduler
                .fail_reserved_delegated_dispatch(&outside.to_string(), now())
                .unwrap(),
            None
        );

        let mut run = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        for (id, digest) in [
            ("ordinary-task".to_owned(), "ordinary".to_owned()),
            (
                "delegated-not-an-operation".to_owned(),
                "delegated-operation:not-an-operation".to_owned(),
            ),
            (
                format!("delegated-{outside}"),
                "not-the-daemon-origin-marker".to_owned(),
            ),
        ] {
            let id = TaskId::new(id).unwrap();
            let mut child = task_node(
                &run,
                id.clone(),
                Some(TaskId::new("root").unwrap()),
                BTreeSet::new(),
                "ordinary".into(),
                NO_ARTIFACT_CONTRACT,
            );
            child.instruction_digest = digest;
            child.state = TaskState::Ready;
            run.tasks.insert(id, child);
        }
        scheduler.supervisor.initialize(&run).unwrap();
        assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());

        let child_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child",
                now(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(scheduler.pending_delegated_promotions().unwrap().len(), 1);
        let child_id = delegated_task_id(child_operation).unwrap();
        let mut retrying = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        retrying.tasks.get_mut(&child_id).unwrap().state = TaskState::Retrying;
        retrying.tasks.get_mut(&child_id).unwrap().generation = 2;
        scheduler.supervisor.initialize(&retrying).unwrap();
        assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
        retrying.tasks.get_mut(&child_id).unwrap().state = TaskState::Ready;
        retrying.tasks.get_mut(&child_id).unwrap().generation = 1;
        scheduler.supervisor.initialize(&retrying).unwrap();
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_operation,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        let mut malformed = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        malformed.tasks.get_mut(&child_id).unwrap().parent_task_id = None;
        scheduler.supervisor.initialize(&malformed).unwrap();
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &child_operation.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("has no parent")
        );
        malformed.tasks.get_mut(&child_id).unwrap().parent_task_id =
            Some(TaskId::new("root").unwrap());
        malformed
            .tasks
            .get_mut(&child_id)
            .unwrap()
            .promotion_parent_dispatch_run = None;
        malformed.provenance.clear();
        scheduler.supervisor.initialize(&malformed).unwrap();
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &child_operation.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("parent provenance is missing")
        );
    }

    #[test]
    fn duplicate_supervisor_membership_refuses_delegated_dispatch_ambiguity() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, root_operation);
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &root_operation.to_string(),
                goal("root"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child",
                now(),
            )
            .unwrap();

        let mut duplicate = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        duplicate.supervisor_run_id = SupervisorRunId::new();
        for task in duplicate.tasks.values_mut() {
            task.supervisor_run_id = duplicate.supervisor_run_id;
        }
        for provenance in duplicate.provenance.values_mut() {
            provenance.supervisor_run_id = duplicate.supervisor_run_id;
        }
        scheduler.supervisor.initialize(&duplicate).unwrap();

        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &OperationId::new().to_string(),
                    "another child",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("multiple supervisor runs")
        );
        assert!(
            scheduler
                .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
                .unwrap_err()
                .to_string()
                .contains("multiple supervisor runs")
        );
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_operation,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &child_operation.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("multiple supervisor runs")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One sequence contrasts a burned operation with a fresh reservation.
    fn definite_delegated_spawn_failure_closes_the_pending_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, root_operation);
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &root_operation.to_string(),
                goal("root work"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work",
                now(),
            )
            .unwrap()
            .unwrap();

        scheduler
            .tick(root.supervisor_run_id, now(), &mut Waker::default())
            .unwrap();
        let reserved = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(reserved.state, SupervisorRunState::Running);
        let child_id = delegated_task_id(child_operation).unwrap();
        scheduler
            .apply(
                &reserved,
                now(),
                SupervisorEventSource::DispatchFailure,
                SupervisorEventKind::Escalate {
                    task_id: Some(child_id),
                    reason: MISSING_DISPATCH_ESCALATION_REASON.into(),
                    safe_evidence: "pre-fix snapshot".into(),
                    choices: vec!["resume".into(), "cancel".into()],
                },
            )
            .unwrap();

        let failed = scheduler
            .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
            .unwrap()
            .unwrap();
        let child = failed
            .tasks
            .iter()
            .find(|task| task.task_id.0 == format!("delegated-{child_operation}"))
            .unwrap();
        assert_eq!(child.state, TaskState::Cancelled);
        assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
        assert!(
            scheduler
                .supervision_fence(child_operation)
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
        assert!(scheduler.supervises_dispatch(root_operation).unwrap());
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child work",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("existing supervisor task")
        );
        assert_eq!(
            scheduler
                .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
                .unwrap()
                .unwrap(),
            failed
        );
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &OperationId::new().to_string(),
                "replacement child work",
                now(),
            )
            .unwrap()
            .unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Each injected commit point proves that a reservation never reports an uncommitted transition.
    fn goal_and_delegated_reservation_commit_failures_are_reported() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let goal_operation = OperationId::new();
        let goal_run = scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &goal_operation.to_string(),
                goal("goal"),
                None,
                now(),
            )
            .unwrap();
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .fail_reserved_goal(&goal_operation.to_string(), "failed".into(), now())
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        let mut missing_root = scheduler
            .supervisor
            .load(goal_run.supervisor_run_id)
            .unwrap()
            .unwrap();
        missing_root.tasks.clear();
        scheduler.supervisor.initialize(&missing_root).unwrap();
        assert!(
            scheduler
                .fail_reserved_goal(&goal_operation.to_string(), "failed".into(), now())
                .unwrap_err()
                .to_string()
                .contains("root task is missing")
        );

        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let root_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: root_operation,
                agent_id: AgentId::new(),
                prompt: "root".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, root_operation);
        let root = scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &root_operation.to_string(),
                goal("root"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .reserve_delegated_dispatch(
                    root_operation,
                    &child_operation.to_string(),
                    "child",
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child",
                now(),
            )
            .unwrap();
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_operation,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        let reserved = scheduler
            .supervisor
            .load(root.supervisor_run_id)
            .unwrap()
            .unwrap();
        scheduler
            .apply(
                &reserved,
                now(),
                SupervisorEventSource::DispatchFailure,
                SupervisorEventKind::Escalate {
                    task_id: Some(delegated_task_id(child_operation).unwrap()),
                    reason: MISSING_DISPATCH_ESCALATION_REASON.into(),
                    safe_evidence: "pre-fix snapshot".into(),
                    choices: vec!["resume".into(), "cancel".into()],
                },
            )
            .unwrap();
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &child_operation.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        scheduler.fail_apply_at(scheduler.apply_calls.get() + 1);
        assert!(
            scheduler
                .bind_reserved_delegated_dispatch(
                    &child_operation.to_string(),
                    &delegated_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
    }

    #[test]
    fn artifact_transition_commit_failures_remain_retryable() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        scheduler
            .dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: "goal".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, operation);
        scheduler
            .start_for_workspace_root_dispatch(
                "goal",
                workspace,
                &operation.to_string(),
                goal("finish"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();

        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        scheduler.fail_apply_at(scheduler.apply_calls.get() + 1);
        assert!(
            scheduler
                .prepare_artifact_verification(operation, now())
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        let request = scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .unwrap();
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .prepare_artifact_verification_after_report(
                    operation,
                    Some(StructuredResult {
                        pr: Some("https://github.com/acme/repo/pull/2".into()),
                        ..StructuredResult::default()
                    }),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
        let request = scheduler
            .record_artifact_expectation(&request, &artifact_expectation(), now())
            .unwrap();
        scheduler.fail_apply_at(scheduler.apply_calls.get());
        assert!(
            scheduler
                .record_artifact_verification(
                    &request,
                    ArtifactVerification {
                        status: ArtifactVerificationStatus::Verified,
                        result_digest: "verified".into(),
                        safe_summary: "verified".into(),
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
    }

    #[test]
    fn terminal_task_kinds_choose_a_safe_terminal_run_reason() {
        for (task_state, expected_reason) in [
            (TaskState::Failed, "one or more supervisor tasks failed"),
            (
                TaskState::Blocked,
                "one or more supervisor tasks were blocked",
            ),
            (
                TaskState::Cancelled,
                "one or more supervisor tasks were cancelled",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let scheduler = SupervisorRuntime::new(temp.path());
            let mut run = SupervisorRun::new(
                "caller".into(),
                "root".into(),
                "input".into(),
                "policy".into(),
                now(),
            );
            run.state = SupervisorRunState::Running;
            let mut root = task(run.supervisor_run_id, "root", None);
            root.state = task_state;
            run.tasks.insert(TaskId::new("root").unwrap(), root);
            scheduler.supervisor.initialize(&run).unwrap();
            let finalized = scheduler.finalize_terminal_tasks(run, now()).unwrap();
            assert_eq!(finalized.state, SupervisorRunState::Failed);
            assert_eq!(finalized.terminal_reason.as_deref(), Some(expected_reason));
        }
    }

    fn wake_reservation(index: usize, delivered: bool) -> WakeReservation {
        let run = SupervisorRunId::new();
        let parent = TaskId::new(format!("parent-{index}")).unwrap();
        let child = OperationId::new();
        WakeReservation {
            wake: DecisionWake {
                supervisor_run_id: run,
                parent_task_id: parent.clone(),
                parent_generation: 1,
                parent: provenance(run, &parent, None, OperationId::new()),
                child_run_id: child,
                outcome: WakeOutcome {
                    kind: InboxKind::Completed,
                    summary: "done".into(),
                },
                dag: Vec::new(),
                remaining_budget_summary: "none".into(),
            },
            delivered,
        }
    }

    #[test]
    fn terminal_statuses_and_sources_preserve_the_safe_completion_vocabulary() {
        assert_eq!(terminal(RunStatus::Running), None);
        assert_eq!(
            terminal(RunStatus::Completed),
            Some((TaskState::Succeeded, InboxKind::Completed))
        );
        assert_eq!(
            terminal(RunStatus::Failed),
            Some((TaskState::Failed, InboxKind::Failed))
        );
        assert_eq!(
            terminal(RunStatus::NoReport),
            Some((TaskState::Failed, InboxKind::NoReport))
        );
        assert_eq!(
            source(InboxKind::Completed),
            SupervisorEventSource::DispatchCompletion
        );
        assert_eq!(
            source(InboxKind::Failed),
            SupervisorEventSource::DispatchFailure
        );
        assert_eq!(source(InboxKind::NoReport), SupervisorEventSource::NoReport);
    }

    #[test]
    fn read_only_query_responses_have_an_aggregate_serialized_budget() {
        assert_eq!(
            bounded_supervisor_query(serde_json::json!({"runs": []})).unwrap(),
            serde_json::json!({"runs": []})
        );
        assert!(
            bounded_supervisor_query(serde_json::json!({
                "value": "x".repeat(RUN_LIST_RESPONSE_MAX_BYTES)
            }))
            .unwrap_err()
            .to_string()
            .contains("capacity is exhausted")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Every field/count boundary belongs to one admission matrix.
    fn start_input_limits_are_utf8_byte_bounds_before_any_durable_effect() {
        let exact_root = format!("{}a", "う".repeat((MAX_SUPERVISOR_TEXT_BYTES - 1) / 3));
        let exact_id = format!(
            "{}aa",
            "う".repeat((usagi_core::domain::supervisor::MAX_TASK_ID_BYTES - 2) / 3)
        );
        let task = InitialTask {
            task_id: exact_id,
            parent_task_id: None,
            dependencies: Vec::new(),
            instruction: "work".into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
        };
        assert_eq!(exact_root.len(), MAX_SUPERVISOR_TEXT_BYTES);
        validate_start_input(
            "operation",
            &exact_root,
            std::slice::from_ref(&task),
            Some("policy"),
        )
        .unwrap();
        assert!(
            validate_start_input("operation", &(exact_root + "x"), &[task], None,)
                .unwrap_err()
                .to_string()
                .contains("root task")
        );
        assert!(
            validate_start_input(&"x".repeat(MAX_SUPERVISOR_KEY_BYTES + 1), "root", &[], None,)
                .is_err()
        );
        assert!(
            validate_start_input(
                "operation",
                "root",
                &vec![
                    InitialTask {
                        task_id: "task".into(),
                        parent_task_id: None,
                        dependencies: Vec::new(),
                        instruction: "work".into(),
                        required_artifact_contract: NO_ARTIFACT_CONTRACT,
                    };
                    MAX_INITIAL_TASKS + 1
                ],
                None,
            )
            .is_err()
        );
        for invalid in [
            InitialTask {
                task_id: "x".repeat(usagi_core::domain::supervisor::MAX_TASK_ID_BYTES + 1),
                parent_task_id: None,
                dependencies: Vec::new(),
                instruction: "work".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: Some(
                    "x".repeat(usagi_core::domain::supervisor::MAX_TASK_ID_BYTES + 1),
                ),
                dependencies: Vec::new(),
                instruction: "work".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: None,
                dependencies: vec!["dependency".into(); MAX_TASK_DEPENDENCIES + 1],
                instruction: "work".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: None,
                dependencies: vec![
                    "x".repeat(usagi_core::domain::supervisor::MAX_TASK_ID_BYTES + 1),
                ],
                instruction: "work".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            },
            InitialTask {
                task_id: "task".into(),
                parent_task_id: None,
                dependencies: Vec::new(),
                instruction: "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            },
        ] {
            assert!(validate_start_input("operation", "root", &[invalid], None).is_err());
        }
        assert!(validate_start_input("operation", "root", &[], Some("")).is_err());
        assert!(
            serde_json::from_value::<InitialTask>(serde_json::json!({
                "task_id": "task",
                "instruction": "work",
                "required_artifact_contract": "unsupported"
            }))
            .is_err()
        );

        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        assert!(
            scheduler
                .start(
                    "caller",
                    "operation",
                    "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
                    Vec::new(),
                    None,
                    now(),
                )
                .is_err()
        );
        assert!(!scheduler.state_path.exists());
        assert!(!temp.path().join("supervisor-runs").exists());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Start, wake, and human-control retention share one durable metadata contract.
    fn runtime_metadata_compacts_safe_history_and_backpressures_live_state() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let mut state = RuntimeState::default();

        for index in 0..MAX_START_RESERVATIONS {
            let run = SupervisorRun::new(
                "caller".into(),
                format!("task-{index}"),
                "input".into(),
                "policy".into(),
                now(),
            );
            scheduler.supervisor.initialize(&run).unwrap();
            state.starts.insert(
                format!("start-{index}"),
                StartReservation {
                    semantic_key: semantic_digest(format!("semantic-{index}").as_bytes()),
                    supervisor_run_id: run.supervisor_run_id,
                    artifact_repository: None,
                    workspace_id: None,
                    caller_dispatch_run_id: None,
                    worker_session_id: None,
                    worker_agent_id: None,
                    worker_runtime_id: None,
                    worker_profile_id: None,
                    worker_semantic_digest: None,
                },
            );
        }
        assert!(
            scheduler
                .ensure_start_capacity(&mut state)
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );

        let first_id = state.starts["start-0"].supervisor_run_id;
        let mut escalated = scheduler.supervisor.load(first_id).unwrap().unwrap();
        escalated.state = SupervisorRunState::Escalated;
        escalated.terminal_at = Some(now());
        json_file::write_atomic(
            scheduler
                .supervisor
                .snapshot_path(first_id)
                .parent()
                .unwrap(),
            &scheduler.supervisor.snapshot_path(first_id),
            &escalated,
        )
        .unwrap();
        assert!(
            scheduler
                .ensure_start_capacity(&mut state)
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );

        let mut finished = scheduler.supervisor.load(first_id).unwrap().unwrap();
        finished.state = SupervisorRunState::Succeeded;
        finished.terminal_at = Some(now());
        json_file::write_atomic(
            scheduler
                .supervisor
                .snapshot_path(first_id)
                .parent()
                .unwrap(),
            &scheduler.supervisor.snapshot_path(first_id),
            &finished,
        )
        .unwrap();
        let second_id = state.starts["start-1"].supervisor_run_id;
        let mut second_finished = scheduler.supervisor.load(second_id).unwrap().unwrap();
        second_finished.state = SupervisorRunState::Succeeded;
        second_finished.terminal_at = Some(now());
        json_file::write_atomic(
            scheduler
                .supervisor
                .snapshot_path(second_id)
                .parent()
                .unwrap(),
            &scheduler.supervisor.snapshot_path(second_id),
            &second_finished,
        )
        .unwrap();
        let caller_tombstone = OperationId::new();
        state
            .starts
            .get_mut("start-0")
            .unwrap()
            .caller_dispatch_run_id = Some(caller_tombstone);
        scheduler.ensure_start_capacity(&mut state).unwrap();
        assert_eq!(state.starts.len(), MAX_START_RESERVATIONS - 1);
        assert!(state.expired_starts.contains("start-0"));
        assert!(state.expired_starts.contains(&caller_tombstone.to_string()));

        let mut missing = RuntimeState::default();
        for index in 0..=MAX_START_RESERVATIONS {
            missing.starts.insert(
                format!("missing-{index}"),
                StartReservation {
                    semantic_key: semantic_digest(format!("missing-semantic-{index}").as_bytes()),
                    supervisor_run_id: SupervisorRunId::new(),
                    artifact_repository: None,
                    workspace_id: None,
                    caller_dispatch_run_id: None,
                    worker_session_id: None,
                    worker_agent_id: None,
                    worker_runtime_id: None,
                    worker_profile_id: None,
                    worker_semantic_digest: None,
                },
            );
        }
        assert!(
            scheduler
                .ensure_start_capacity(&mut missing)
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );
        assert_eq!(missing.starts.len(), MAX_START_RESERVATIONS + 1);
        assert!(!missing.expired_starts.contains("missing-0"));

        scheduler.save_state(&state).unwrap();
        assert!(
            scheduler
                .start("caller", "start-0", "root".into(), Vec::new(), None, now(),)
                .unwrap_err()
                .to_string()
                .contains("idempotency window expired")
        );

        for index in 0..MAX_WAKE_RESERVATIONS {
            state
                .wakes
                .insert(format!("wake-{index:02}"), wake_reservation(index, true));
        }
        state.compact_delivered_wakes();
        assert_eq!(state.wakes.len(), RETAIN_DELIVERED_WAKES);
        assert!(state.expired_wakes.contains("wake-00"));

        state.wakes.clear();
        for index in 0..=MAX_WAKE_RESERVATIONS {
            state.wakes.insert(
                format!("pending-{index:02}"),
                wake_reservation(index, false),
            );
        }
        state.compact_delivered_wakes();
        assert_eq!(state.wakes.len(), MAX_WAKE_RESERVATIONS + 1);
        assert!(
            scheduler
                .save_state(&state)
                .unwrap_err()
                .to_string()
                .contains("hard limit")
        );

        let run_id = SupervisorRunId::new();
        let parent_id = TaskId::new("parent-capacity").unwrap();
        let child = OperationId::new();
        let mut run = SupervisorRun::new_with_id(
            run_id,
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let mut parent = task(run_id, "parent-capacity", None);
        parent.state = TaskState::AwaitingDecision;
        run.tasks.insert(parent_id.clone(), parent);
        run.provenance.insert(
            parent_id.clone(),
            provenance(run_id, &parent_id, None, OperationId::new()),
        );
        state.wakes.clear();
        state.wakes = (0..MAX_WAKE_RESERVATIONS)
            .map(|index| (format!("full-{index}"), wake_reservation(index, false)))
            .collect();
        scheduler.save_state(&state).unwrap();
        assert!(
            scheduler
                .reserve_parent_wake(&mut run, &parent_id, child, InboxKind::Completed, now(),)
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );
        state.wakes.clear();
        let wake_key = format!("{}:{}:{}", child, parent_id.0, 1);
        state.expired_wakes.insert(&wake_key);
        scheduler.save_state(&state).unwrap();
        scheduler
            .reserve_parent_wake(&mut run, &parent_id, child, InboxKind::Completed, now())
            .unwrap();
        assert!(scheduler.load_state().unwrap().wakes.is_empty());

        let control_run = SupervisorRun::new(
            "workspace-operator".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        scheduler.supervisor.initialize(&control_run).unwrap();
        let command = SupervisorWorkspaceCommand::Cancel {
            supervisor_run_id: control_run.supervisor_run_id,
            reason: "capacity fixture".into(),
        };
        let digest = control_semantic_digest(&command).unwrap();
        let mut controls = RuntimeState::default();
        let mut oldest = None;
        for index in 0..MAX_CONTROL_RESERVATIONS {
            let operation = OperationId::new().to_string();
            oldest.get_or_insert_with(|| operation.clone());
            controls.controls.insert(
                operation,
                ControlReservation {
                    semantic_digest: digest.clone(),
                    supervisor_run_id: control_run.supervisor_run_id,
                    reserved_at: now() + chrono::Duration::seconds(i64::try_from(index).unwrap()),
                },
            );
        }
        assert!(
            scheduler
                .ensure_control_capacity(&mut controls)
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );
        let mut finished_control = control_run;
        finished_control.state = SupervisorRunState::Cancelled;
        finished_control.terminal_at = Some(now());
        json_file::write_atomic(
            scheduler
                .supervisor
                .snapshot_path(finished_control.supervisor_run_id)
                .parent()
                .unwrap(),
            &scheduler
                .supervisor
                .snapshot_path(finished_control.supervisor_run_id),
            &finished_control,
        )
        .unwrap();
        scheduler.ensure_control_capacity(&mut controls).unwrap();
        assert_eq!(controls.controls.len(), MAX_CONTROL_RESERVATIONS - 1);
        assert!(controls.expired_controls.contains(&oldest.unwrap()));
    }

    #[test]
    fn oversized_or_malformed_runtime_metadata_fails_closed_on_load() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let mut state = RuntimeState {
            wakes: (0..=MAX_WAKE_RESERVATIONS)
                .map(|index| (format!("pending-{index}"), wake_reservation(index, false)))
                .collect(),
            ..RuntimeState::default()
        };
        json_file::write_atomic(temp.path(), &scheduler.state_path, &state).unwrap();
        assert!(
            scheduler
                .load_state()
                .unwrap_err()
                .to_string()
                .contains("hard limit")
        );

        state.wakes.clear();
        state.expired_starts.words.push(1);
        json_file::write_atomic(temp.path(), &scheduler.state_path, &state).unwrap();
        assert!(
            scheduler
                .load_state()
                .unwrap_err()
                .to_string()
                .contains("hard limit")
        );

        let mut malformed_control = RuntimeState::default();
        malformed_control.controls.insert(
            "not-an-operation-id".into(),
            ControlReservation {
                semantic_digest: "unbounded-or-invalid".into(),
                supervisor_run_id: SupervisorRunId::new(),
                reserved_at: now(),
            },
        );
        json_file::write_atomic(temp.path(), &scheduler.state_path, &malformed_control).unwrap();
        assert!(
            scheduler
                .load_state()
                .unwrap_err()
                .to_string()
                .contains("hard limit")
        );

        let mut legacy = RuntimeState::default();
        legacy.starts.insert(
            "legacy-operation".into(),
            StartReservation {
                semantic_key: "legacy raw semantic material".into(),
                supervisor_run_id: SupervisorRunId::new(),
                artifact_repository: None,
                workspace_id: None,
                caller_dispatch_run_id: None,
                worker_session_id: None,
                worker_agent_id: None,
                worker_runtime_id: None,
                worker_profile_id: None,
                worker_semantic_digest: None,
            },
        );
        json_file::write_atomic(temp.path(), &scheduler.state_path, &legacy).unwrap();
        let migrated = scheduler.load_state().unwrap();
        assert_eq!(
            migrated.starts["legacy-operation"].semantic_key,
            semantic_digest(b"legacy raw semantic material")
        );

        std::fs::File::create(&scheduler.state_path)
            .unwrap()
            .set_len(u64::try_from(MAX_RUNTIME_STATE_BYTES + 1).unwrap())
            .unwrap();
        assert!(
            scheduler
                .load_state()
                .unwrap_err()
                .to_string()
                .contains("JSON limit")
        );
    }

    #[test]
    fn a_missing_run_is_a_noop_and_does_not_call_the_waker() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let initial = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let mut waker = Waker::default();
        scheduler
            .tick(initial.supervisor_run_id, now(), &mut waker)
            .unwrap();
        assert!(waker.wakes.is_empty());
    }

    #[test]
    fn a_ready_task_without_a_dispatch_reservation_escalates_instead_of_stalling() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let started = scheduler
            .start(
                "caller",
                "operation",
                "root work".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        let mut waker = Waker::default();

        scheduler
            .tick(started.supervisor_run_id, now(), &mut waker)
            .unwrap();

        let stopped = scheduler
            .get("caller", started.supervisor_run_id)
            .unwrap()
            .unwrap();
        assert_eq!(stopped.state, SupervisorRunState::Escalated);
        let escalation = stopped.escalation.unwrap();
        assert_eq!(escalation.blocking_task_id.unwrap().0, "root");
        assert_eq!(
            escalation.reason,
            "no worker dispatch reservation was produced for a ready task"
        );
        assert!(escalation.safe_evidence.contains("runtime/model selection"));
        assert!(waker.wakes.is_empty());
    }

    #[test]
    fn a_dispatch_escalation_persistence_failure_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let started = scheduler
            .start(
                "caller",
                "operation",
                "root work".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        scheduler.fail_apply_at(2);

        let error = scheduler
            .tick(started.supervisor_run_id, now(), &mut Waker::default())
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("injected supervisor apply failure")
        );
    }

    #[test]
    fn tick_reconciles_only_retries_whose_durable_deadline_is_due() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let store = SupervisorStore::new(temp.path());
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let due_id = TaskId::new("due").unwrap();
        let future_id = TaskId::new("future").unwrap();
        let mut due = task(run.supervisor_run_id, "due", None);
        due.state = TaskState::Retrying;
        due.retry_at = Some(now());
        let mut future = task(run.supervisor_run_id, "future", None);
        future.state = TaskState::Retrying;
        future.retry_at = Some(now() + chrono::Duration::seconds(1));
        run.tasks = BTreeMap::from([(due_id.clone(), due), (future_id.clone(), future)]);
        store.initialize(&run).unwrap();

        scheduler.fail_apply_at(0);
        assert!(
            scheduler
                .tick(run.supervisor_run_id, now(), &mut Waker::default())
                .unwrap_err()
                .to_string()
                .contains("injected")
        );
        let scheduler = SupervisorRuntime::new(temp.path());
        scheduler
            .tick(run.supervisor_run_id, now(), &mut Waker::default())
            .unwrap();

        let saved = store.load(run.supervisor_run_id).unwrap().unwrap();
        assert_eq!(saved.tasks[&due_id].state, TaskState::Ready);
        assert_eq!(saved.tasks[&future_id].state, TaskState::Retrying);
    }

    #[test]
    fn start_propagates_each_injected_partial_apply_failure() {
        for fail_at in 0..=2 {
            let temp = tempfile::tempdir().unwrap();
            let scheduler = SupervisorRuntime::new(temp.path());
            scheduler.fail_apply_at(fail_at);
            let operation = format!("operation-{fail_at}");
            let initial_tasks = vec![InitialTask {
                task_id: "child".into(),
                parent_task_id: None,
                dependencies: vec!["root".into()],
                instruction: "child".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            }];
            assert!(
                scheduler
                    .start(
                        "caller",
                        &operation,
                        "root".into(),
                        initial_tasks.clone(),
                        None,
                        now(),
                    )
                    .unwrap_err()
                    .to_string()
                    .contains("injected")
            );
            let recovered = scheduler
                .start(
                    "caller",
                    &operation,
                    "root".into(),
                    initial_tasks,
                    None,
                    now(),
                )
                .unwrap();
            assert_eq!(recovered.state, SupervisorRunState::Running);
            assert_eq!(recovered.tasks.len(), 2);
        }
    }

    #[test]
    fn start_recovery_refuses_inconsistent_partial_snapshots() {
        for (fail_at, expected) in [
            (0, "reservation does not match"),
            (1, "root task conflicts"),
            (2, "initial task conflicts"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let scheduler = SupervisorRuntime::new(temp.path());
            let operation = format!("inconsistent-{fail_at}");
            let initial_tasks = vec![InitialTask {
                task_id: "child".into(),
                parent_task_id: None,
                dependencies: vec!["root".into()],
                instruction: "child".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            }];
            scheduler.fail_apply_at(fail_at);
            assert!(
                scheduler
                    .start(
                        "caller",
                        &operation,
                        "root".into(),
                        initial_tasks.clone(),
                        None,
                        now(),
                    )
                    .is_err()
            );
            let id = scheduler.load_state().unwrap().starts[&operation].supervisor_run_id;
            let mut run = scheduler.supervisor.load(id).unwrap().unwrap();
            if fail_at == 0 {
                run.root_caller_ref = "other-caller".into();
            } else if fail_at == 1 {
                run.tasks
                    .get_mut(&TaskId::new("root").unwrap())
                    .unwrap()
                    .instruction_body = "other root".into();
            } else {
                run.tasks
                    .get_mut(&TaskId::new("child").unwrap())
                    .unwrap()
                    .instruction_body = "other child".into();
            }
            scheduler.supervisor.initialize(&run).unwrap();
            assert!(
                scheduler
                    .start(
                        "caller",
                        &operation,
                        "root".into(),
                        initial_tasks,
                        None,
                        now(),
                    )
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn structured_inbox_report_is_used_for_the_wake_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let run_id = OperationId::new();
        let caller = CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: AgentId::new(),
        };
        dispatch
            .upsert_binding(DispatchBinding {
                run_id,
                caller: caller.clone(),
                worker: WorkerRef {
                    session_id: Some(SessionId::new()),
                    agent_id: AgentId::new(),
                },
            })
            .unwrap();
        dispatch
            .append_inbox(
                &caller,
                InboxMessage {
                    run_id,
                    from: WorkerRef {
                        session_id: Some(SessionId::new()),
                        agent_id: AgentId::new(),
                    },
                    kind: InboxKind::Failed,
                    summary: "safe failure".into(),
                    result: None,
                    created_at: now(),
                    read: false,
                },
            )
            .unwrap();
        assert_eq!(
            scheduler.outcome(run_id, InboxKind::Completed).unwrap(),
            WakeOutcome {
                kind: InboxKind::Failed,
                summary: "safe failure".into(),
            }
        );
    }

    #[test]
    fn incomplete_parent_provenance_is_fail_closed_after_child_completion() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let store = SupervisorStore::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let parent = TaskId::new("parent").unwrap();
        let child = TaskId::new("child").unwrap();
        let child_run = OperationId::new();
        let mut parent_task = task(run.supervisor_run_id, "parent", None);
        parent_task.state = TaskState::AwaitingDecision;
        let mut child_task = task(run.supervisor_run_id, "child", Some("parent"));
        child_task.state = TaskState::Dispatched;
        run.tasks = BTreeMap::from([(parent.clone(), parent_task), (child.clone(), child_task)]);
        run.provenance.insert(
            child.clone(),
            provenance(
                run.supervisor_run_id,
                &child,
                Some((&parent, OperationId::new())),
                child_run,
            ),
        );
        store.initialize(&run).unwrap();
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_run,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::NoReport,
            })
            .unwrap();
        let mut waker = Waker::default();
        scheduler
            .tick(run.supervisor_run_id, now(), &mut waker)
            .unwrap();
        assert_eq!(
            store.load(run.supervisor_run_id).unwrap().unwrap().tasks[&child].state,
            TaskState::Failed
        );
        assert!(waker.wakes.is_empty());
    }

    #[test]
    fn tick_retries_a_partial_parent_wake_and_ignores_nonterminal_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let store = SupervisorStore::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let parent = TaskId::new("parent").unwrap();
        let waiting = TaskId::new("waiting").unwrap();
        let child = TaskId::new("child").unwrap();
        let waiting_run = OperationId::new();
        let child_run = OperationId::new();
        let mut parent_task = task(run.supervisor_run_id, "parent", None);
        parent_task.state = TaskState::Running;
        let mut waiting_task = task(run.supervisor_run_id, "waiting", None);
        waiting_task.state = TaskState::Dispatched;
        let mut child_task = task(run.supervisor_run_id, "child", Some("parent"));
        child_task.state = TaskState::Dispatched;
        run.tasks = BTreeMap::from([
            (parent.clone(), parent_task),
            (waiting.clone(), waiting_task),
            (child.clone(), child_task),
        ]);
        run.provenance.insert(
            waiting.clone(),
            provenance(run.supervisor_run_id, &waiting, None, waiting_run),
        );
        run.provenance.insert(
            child.clone(),
            provenance(
                run.supervisor_run_id,
                &child,
                Some((&parent, OperationId::new())),
                child_run,
            ),
        );
        store.initialize(&run).unwrap();
        for (run_id, status) in [
            (waiting_run, RunStatus::Running),
            (child_run, RunStatus::Completed),
        ] {
            dispatch
                .upsert_run(DispatchRun {
                    run_id,
                    agent_id: AgentId::new(),
                    prompt: "child".into(),
                    started_at: now(),
                    ended_at: None,
                    status,
                })
                .unwrap();
        }

        scheduler.fail_apply_at(1);
        assert!(
            scheduler
                .tick(run.supervisor_run_id, now(), &mut Waker::default())
                .unwrap_err()
                .to_string()
                .contains("injected")
        );
        let scheduler = SupervisorRuntime::new(temp.path());
        scheduler.fail_apply_at(1);
        assert!(
            scheduler
                .tick(run.supervisor_run_id, now(), &mut Waker::default())
                .unwrap_err()
                .to_string()
                .contains("injected")
        );
        let scheduler = SupervisorRuntime::new(temp.path());
        let mut waker = Waker::default();
        scheduler
            .tick(run.supervisor_run_id, now(), &mut waker)
            .unwrap();
        assert_eq!(scheduler.dispatch_registry_reads.get(), 1);
        let saved = store.load(run.supervisor_run_id).unwrap().unwrap();
        assert_eq!(saved.tasks[&waiting].state, TaskState::Dispatched);
        assert_eq!(saved.tasks[&child].state, TaskState::Succeeded);
        assert_eq!(saved.tasks[&parent].state, TaskState::AwaitingDecision);
        assert!(waker.wakes.is_empty());
    }

    #[test]
    fn tick_skips_blocked_provenance_and_accepts_terminal_work_without_a_parent() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let store = SupervisorStore::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let blocked = TaskId::new("blocked").unwrap();
        let standalone = TaskId::new("standalone").unwrap();
        let terminal_parent = TaskId::new("terminal-parent").unwrap();
        let blocked_run = OperationId::new();
        let standalone_run = OperationId::new();
        let mut blocked_task = task(run.supervisor_run_id, "blocked", None);
        blocked_task.state = TaskState::AwaitingDecision;
        let mut standalone_task = task(run.supervisor_run_id, "standalone", None);
        standalone_task.state = TaskState::Dispatched;
        let mut terminal_parent_task = task(run.supervisor_run_id, "terminal-parent", None);
        terminal_parent_task.state = TaskState::Succeeded;
        run.tasks = BTreeMap::from([
            (blocked.clone(), blocked_task),
            (standalone.clone(), standalone_task),
            (terminal_parent.clone(), terminal_parent_task),
        ]);
        run.provenance.insert(
            blocked.clone(),
            provenance(run.supervisor_run_id, &blocked, None, blocked_run),
        );
        run.provenance.insert(
            standalone.clone(),
            provenance(run.supervisor_run_id, &standalone, None, standalone_run),
        );
        scheduler
            .reserve_parent_wake(
                &mut run,
                &terminal_parent,
                OperationId::new(),
                InboxKind::Completed,
                now(),
            )
            .unwrap();
        store.initialize(&run).unwrap();
        for run_id in [blocked_run, standalone_run] {
            dispatch
                .upsert_run(DispatchRun {
                    run_id,
                    agent_id: AgentId::new(),
                    prompt: "work".into(),
                    started_at: now(),
                    ended_at: Some(now()),
                    status: RunStatus::Completed,
                })
                .unwrap();
        }

        scheduler
            .tick(run.supervisor_run_id, now(), &mut Waker::default())
            .unwrap();
        let saved = store.load(run.supervisor_run_id).unwrap().unwrap();
        assert_eq!(saved.tasks[&blocked].state, TaskState::AwaitingDecision);
        assert_eq!(saved.tasks[&standalone].state, TaskState::Succeeded);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // The fixture is a complete durable history.
    fn completion_is_reconciled_once_and_restart_does_not_duplicate_the_parent_wake() {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let store = SupervisorStore::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let initial = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let id = initial.supervisor_run_id;
        store.initialize(&initial).unwrap();
        let parent_id = TaskId::new("parent").unwrap();
        let child_id = TaskId::new("child").unwrap();
        let parent_run = OperationId::new();
        let child_run = OperationId::new();
        let mut run = store.load(id).unwrap().unwrap();
        run = store
            .apply(
                id,
                run.state_revision,
                &event(
                    &run,
                    SupervisorEventKind::SetRunState {
                        state: SupervisorRunState::Running,
                        terminal_reason: None,
                    },
                ),
            )
            .unwrap();
        run = store
            .apply(
                id,
                run.state_revision,
                &event(
                    &run,
                    SupervisorEventKind::AddTask {
                        task: task(id, "parent", None),
                    },
                ),
            )
            .unwrap();
        run = store
            .apply(
                id,
                run.state_revision,
                &event(
                    &run,
                    SupervisorEventKind::Dispatch {
                        task_id: parent_id.clone(),
                        generation: 1,
                        provenance: provenance(id, &parent_id, None, parent_run),
                    },
                ),
            )
            .unwrap();
        run = store
            .apply(
                id,
                run.state_revision,
                &event(
                    &run,
                    SupervisorEventKind::Running {
                        task_id: parent_id.clone(),
                        generation: 1,
                    },
                ),
            )
            .unwrap();
        run = store
            .apply(
                id,
                run.state_revision,
                &event(
                    &run,
                    SupervisorEventKind::AddTask {
                        task: task(id, "child", Some("parent")),
                    },
                ),
            )
            .unwrap();
        let _ = store
            .apply(
                id,
                run.state_revision,
                &event(
                    &run,
                    SupervisorEventKind::Dispatch {
                        task_id: child_id.clone(),
                        generation: 1,
                        provenance: provenance(
                            id,
                            &child_id,
                            Some((&parent_id, parent_run)),
                            child_run,
                        ),
                    },
                ),
            )
            .unwrap();
        dispatch
            .upsert_run(DispatchRun {
                run_id: child_run,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        let mut waker = Waker::default();
        scheduler.tick(id, now(), &mut waker).unwrap();
        let saved = store.load(id).unwrap().unwrap();
        assert_eq!(saved.tasks[&child_id].state, TaskState::Succeeded);
        assert_eq!(saved.tasks[&parent_id].state, TaskState::Running);
        assert_eq!(waker.wakes.len(), 1);
        assert_eq!(waker.wakes[0].child_run_id, child_run);

        dispatch
            .upsert_run(DispatchRun {
                run_id: parent_run,
                agent_id: AgentId::new(),
                prompt: "parent".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        scheduler.tick(id, now(), &mut waker).unwrap();
        let saved = store.load(id).unwrap().unwrap();
        assert_eq!(saved.tasks[&parent_id].state, TaskState::Succeeded);
        assert_eq!(saved.state, SupervisorRunState::Succeeded);
        assert_eq!(waker.wakes.len(), 1);

        let restarted = SupervisorRuntime::new(temp.path());
        restarted.tick(id, now(), &mut waker).unwrap();
        assert_eq!(waker.wakes.len(), 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn control_surface_is_idempotent_owned_and_durable() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let initial = vec![InitialTask {
            task_id: "child".into(),
            parent_task_id: None,
            dependencies: vec!["root".into()],
            instruction: "secret child instruction".into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
        }];
        let started = runtime
            .start(
                "caller-a",
                "operation-a",
                "secret root instruction".into(),
                initial.clone(),
                None,
                now(),
            )
            .unwrap();
        assert_eq!(started.state, SupervisorRunState::Running);
        assert_eq!(started.tasks.len(), 2);
        assert_eq!(
            started
                .tasks
                .iter()
                .find(|task| task.task_id.0 == "child")
                .and_then(|task| task.parent_task_id.as_ref())
                .map(|task| task.0.as_str()),
            Some("root")
        );
        assert_eq!(
            runtime
                .start(
                    "caller-a",
                    "operation-a",
                    "secret root instruction".into(),
                    initial,
                    None,
                    now(),
                )
                .unwrap()
                .supervisor_run_id,
            started.supervisor_run_id
        );
        assert!(
            runtime
                .start(
                    "caller-a",
                    "operation-a",
                    "different".into(),
                    vec![],
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("reused")
        );
        assert!(
            runtime
                .get("caller-b", started.supervisor_run_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            runtime
                .get("caller-a", started.supervisor_run_id)
                .unwrap()
                .unwrap(),
            started
        );
        assert_eq!(
            runtime
                .list("caller-a", Some(SupervisorRunState::Running))
                .unwrap()
                .len(),
            1
        );
        let page = runtime
            .list_page("caller-a", Some(SupervisorRunState::Running), 0, 1)
            .unwrap();
        assert_eq!(page.runs.len(), 1);
        assert!(page.next_cursor.is_none());
        assert!(
            runtime
                .list_page("caller-b", None, 0, 1)
                .unwrap()
                .runs
                .is_empty()
        );
        let (events, cursor) = runtime
            .events("caller-a", started.supervisor_run_id, 0, 10)
            .unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(cursor.next_sequence, 4);
        assert!(
            runtime
                .events("caller-b", started.supervisor_run_id, 0, 10)
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
        assert!(
            runtime
                .cancel(
                    "caller-b",
                    started.supervisor_run_id,
                    "foreign".into(),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
        assert!(
            runtime
                .resolve_escalation(
                    "caller-b",
                    started.supervisor_run_id,
                    OperationId::new(),
                    EscalationDecision::Resume,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
        let run = runtime
            .supervisor
            .load(started.supervisor_run_id)
            .unwrap()
            .unwrap();
        let escalated = runtime
            .apply(
                &run,
                now(),
                SupervisorEventSource::Admission,
                SupervisorEventKind::Escalate {
                    task_id: None,
                    reason: "operator decision required".into(),
                    safe_evidence: "safe evidence".into(),
                    choices: vec!["resume".into()],
                },
            )
            .unwrap();
        let escalation_id = escalated.escalation.as_ref().unwrap().escalation_id;
        let resumed = runtime
            .resolve_escalation(
                "caller-a",
                started.supervisor_run_id,
                escalation_id,
                EscalationDecision::Resume,
                now(),
            )
            .unwrap();
        assert_eq!(resumed.state, SupervisorRunState::Running);
        let cancelled = runtime
            .cancel(
                "caller-a",
                started.supervisor_run_id,
                "operator requested".into(),
                now(),
            )
            .unwrap();
        assert_eq!(cancelled.state, SupervisorRunState::Cancelled);
        assert_eq!(
            SupervisorRuntime::new(temp.path())
                .list("caller-a", None)
                .unwrap()
                .len(),
            1
        );
        runtime.tick_all(now(), &mut Waker::default()).unwrap();
    }

    #[test]
    fn cancel_rejects_unsafe_or_unbounded_presented_reasons() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let run = runtime
            .start(
                "caller",
                "operation",
                "root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        for reason in [
            String::new(),
            "clear\u{1b}[2J".into(),
            "line\nbreak".into(),
            "direction\u{202e}override".into(),
            "x".repeat(MAX_SUPERVISOR_REASON_BYTES + 1),
        ] {
            assert!(
                runtime
                    .cancel("caller", run.supervisor_run_id, reason, now())
                    .is_err()
            );
        }
        assert_eq!(
            runtime
                .cancel(
                    "caller",
                    run.supervisor_run_id,
                    "operator requested stop".into(),
                    now(),
                )
                .unwrap()
                .state,
            SupervisorRunState::Cancelled
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One lifecycle fixture proves workspace fencing, restart replay, conflicts, and stop recovery selection together.
    fn workspace_control_is_durable_scoped_and_projects_exact_stop_obligations() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let dispatch_run = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: dispatch_run,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&runtime, workspace, dispatch_run);
        let worker = root_worker(workspace);
        let run = runtime
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &dispatch_run.to_string(),
                goal("finish the goal"),
                Some("standard".into()),
                &worker,
                now(),
            )
            .unwrap();
        let operation = OperationId::new();
        let command = SupervisorWorkspaceCommand::Cancel {
            supervisor_run_id: run.supervisor_run_id,
            reason: "operator cancelled".into(),
        };
        let cancelled = runtime
            .control_for_workspace(workspace, operation, &command, now())
            .unwrap();
        assert_eq!(cancelled.state, SupervisorRunState::Cancelled);
        assert_eq!(cancelled.provenance.len(), 1);

        let restarted = SupervisorRuntime::new(temp.path());
        assert_eq!(
            restarted
                .control_for_workspace(
                    workspace,
                    operation,
                    &command,
                    now() + chrono::Duration::minutes(1),
                )
                .unwrap(),
            cancelled
        );
        assert!(
            restarted
                .control_for_workspace(
                    workspace,
                    operation,
                    &SupervisorWorkspaceCommand::Cancel {
                        supervisor_run_id: run.supervisor_run_id,
                        reason: "different reason".into(),
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("conflicts with its reservation")
        );
        assert!(
            restarted
                .control_for_workspace(WorkspaceId::new(), OperationId::new(), &command, now())
                .unwrap_err()
                .to_string()
                .contains("does not belong")
        );

        let obligations = restarted.worker_stop_obligations().unwrap();
        assert_eq!(obligations.len(), 1);
        assert_eq!(obligations[0].0, workspace);
        assert_eq!(obligations[0].1.worker_agent_id, worker.agent_runtime_id);
        assert_eq!(
            obligations[0].1.worker_worktree_id,
            worker.terminal.worktree_id
        );
        assert_eq!(
            restarted
                .worker_stop_obligations_for_run(run.supervisor_run_id)
                .unwrap(),
            obligations
        );
        assert!(
            restarted
                .worker_stop_obligations_for_run(SupervisorRunId::new())
                .unwrap()
                .is_empty()
        );

        let mut prior_attempt = restarted
            .supervisor
            .load(run.supervisor_run_id)
            .unwrap()
            .unwrap();
        let root = prior_attempt
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap();
        root.generation += 1;
        root.assigned_dispatch_run = None;
        json_file::write_atomic(
            restarted
                .supervisor
                .snapshot_path(run.supervisor_run_id)
                .parent()
                .unwrap(),
            &restarted.supervisor.snapshot_path(run.supervisor_run_id),
            &prior_attempt,
        )
        .unwrap();
        assert!(restarted.worker_stop_obligations().unwrap().is_empty());

        let mut legacy = SupervisorRun::new(
            "legacy".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        legacy.state = SupervisorRunState::Cancelled;
        legacy.terminal_at = Some(now());
        restarted.supervisor.initialize(&legacy).unwrap();
        assert!(restarted.worker_stop_obligations().unwrap().is_empty());
    }

    #[test]
    fn workspace_delete_is_terminal_revisioned_and_scoped() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();

        let mut active = SupervisorRun::new(
            "goal-composer".into(),
            "active".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        active.workspace_id = Some(workspace);
        runtime.supervisor.initialize(&active).unwrap();
        let active_command = SupervisorWorkspaceCommand::Delete {
            supervisor_run_id: active.supervisor_run_id,
            observed_state_revision: active.state_revision,
        };
        assert!(
            runtime
                .delete_for_workspace(workspace, OperationId::new(), &active_command, now())
                .unwrap_err()
                .to_string()
                .contains("must finish")
        );

        let finished = aborted_run(Some(workspace));
        let id = finished.supervisor_run_id;
        let revision = finished.state_revision;
        runtime.supervisor.initialize(&finished).unwrap();
        let command = SupervisorWorkspaceCommand::Delete {
            supervisor_run_id: id,
            observed_state_revision: revision,
        };
        assert!(
            runtime
                .control_for_workspace(workspace, OperationId::new(), &command, now())
                .unwrap_err()
                .to_string()
                .contains("delete control path")
        );
        assert!(
            runtime
                .delete_for_workspace(
                    workspace,
                    OperationId::new(),
                    &SupervisorWorkspaceCommand::Cancel {
                        supervisor_run_id: id,
                        reason: "not a deletion".into(),
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("delete command is required")
        );
        let expired_operation = OperationId::new();
        let mut durable_state = runtime.load_state().unwrap();
        durable_state
            .expired_controls
            .insert(&expired_operation.to_string());
        runtime.save_state(&durable_state).unwrap();
        assert!(
            runtime
                .delete_for_workspace(workspace, expired_operation, &command, now())
                .unwrap_err()
                .to_string()
                .contains("outside the retained replay window")
        );
        assert!(
            runtime
                .delete_for_workspace(
                    workspace,
                    OperationId::new(),
                    &SupervisorWorkspaceCommand::Delete {
                        supervisor_run_id: id,
                        observed_state_revision: revision + 1,
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("stale supervisor state revision")
        );
        assert!(
            runtime
                .delete_for_workspace(WorkspaceId::new(), OperationId::new(), &command, now())
                .unwrap_err()
                .to_string()
                .contains("does not belong")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Deletion durability and both root reservation forms are one replay contract.
    fn workspace_delete_is_durable_and_replayable() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let finished = aborted_run(Some(workspace));
        let id = finished.supervisor_run_id;
        let revision = finished.state_revision;
        runtime.supervisor.initialize(&finished).unwrap();
        let command = SupervisorWorkspaceCommand::Delete {
            supervisor_run_id: id,
            observed_state_revision: revision,
        };

        let operation = OperationId::new();
        let receipt = runtime
            .delete_for_workspace(workspace, operation, &command, now())
            .unwrap();
        assert_eq!(receipt.supervisor_run_id, id);
        assert_eq!(receipt.state_revision, revision);
        assert!(runtime.supervisor.load(id).unwrap().is_none());
        assert!(runtime.list_workspace(workspace).unwrap().is_empty());

        let restarted = SupervisorRuntime::new(temp.path());
        assert_eq!(
            restarted
                .delete_for_workspace(workspace, operation, &command, now())
                .unwrap(),
            receipt
        );
        assert!(
            restarted
                .delete_for_workspace(
                    workspace,
                    operation,
                    &SupervisorWorkspaceCommand::Delete {
                        supervisor_run_id: id,
                        observed_state_revision: revision + 1,
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("conflicts with its reservation")
        );
        assert!(
            restarted
                .delete_for_workspace(workspace, OperationId::new(), &command, now())
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );

        let reserved_temp = tempfile::tempdir().unwrap();
        let reserved = SupervisorRuntime::new(reserved_temp.path());
        let start_operation = OperationId::new();
        let started = reserved
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &start_operation.to_string(),
                goal("delete reserved root"),
                None,
                now(),
            )
            .unwrap();
        let terminal = reserved
            .control_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: started.supervisor_run_id,
                    reason: "delete".into(),
                },
                now(),
            )
            .unwrap();
        reserved
            .delete_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Delete {
                    supervisor_run_id: terminal.supervisor_run_id,
                    observed_state_revision: terminal.state_revision,
                },
                now(),
            )
            .unwrap();
        let state = reserved.load_state().unwrap();
        assert!(!state.starts.contains_key(&start_operation.to_string()));
        assert!(state.expired_starts.contains(&start_operation.to_string()));

        let caller_temp = tempfile::tempdir().unwrap();
        let caller = SupervisorRuntime::new(caller_temp.path());
        let caller_dispatch = OperationId::new();
        let caller_worker = root_worker(workspace);
        persist_caller_dispatch(&caller, workspace, caller_dispatch, &caller_worker);
        let caller_start = OperationId::new().to_string();
        let started = caller
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &caller_start,
                "delete caller root".into(),
                None,
                caller_dispatch,
                &caller_worker,
                now(),
            )
            .unwrap();
        let terminal = caller
            .control_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: started.supervisor_run_id,
                    reason: "delete".into(),
                },
                now(),
            )
            .unwrap();
        caller
            .delete_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Delete {
                    supervisor_run_id: terminal.supervisor_run_id,
                    observed_state_revision: terminal.state_revision,
                },
                now(),
            )
            .unwrap();
        let state = caller.load_state().unwrap();
        assert!(state.expired_starts.contains(&caller_start));
        assert!(state.expired_starts.contains(&caller_dispatch.to_string()));
    }

    #[test]
    #[should_panic(expected = "history deletion does not append an aggregate event")]
    fn history_deletion_cannot_be_encoded_as_an_aggregate_event() {
        let run = aborted_run(Some(WorkspaceId::new()));
        let command = SupervisorWorkspaceCommand::Delete {
            supervisor_run_id: run.supervisor_run_id,
            observed_state_revision: run.state_revision,
        };
        let _ = control_event(
            &run,
            OperationId::new(),
            "sha256:delete".into(),
            &command,
            now(),
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Root and delegated crash windows share one exact operation-join contract.
    fn aborted_unbound_promotions_join_only_the_exact_agent_operation() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let root_operation = OperationId::new();
        let root_run = runtime
            .reserve_goal_for_workspace(
                "goal-composer",
                workspace,
                &root_operation.to_string(),
                goal("unbound root"),
                None,
                now(),
            )
            .unwrap();
        runtime
            .control_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: root_run.supervisor_run_id,
                    reason: "cancel unbound root".into(),
                },
                now(),
            )
            .unwrap();
        let root_stops = runtime
            .pending_worker_stops_for_run(root_run.supervisor_run_id)
            .unwrap();
        assert_eq!(root_stops.len(), 1);
        assert_eq!(root_stops[0].operation_id(), root_operation);
        assert_eq!(root_stops[0].workspace_id(), workspace);
        let worker = root_worker(workspace);
        let root_provenance = root_stops[0].provenance(&worker).unwrap();
        assert_eq!(root_provenance.worker_agent_id, worker.agent_runtime_id);
        assert!(
            root_stops[0]
                .provenance(&delegated_worker(workspace))
                .unwrap_err()
                .to_string()
                .contains("outside its reserved scope")
        );
        let mut full = runtime.load_state().unwrap();
        for index in 1..MAX_START_RESERVATIONS {
            let live = SupervisorRun::new(
                "caller".into(),
                format!("live-{index}"),
                "input".into(),
                "policy".into(),
                now(),
            );
            runtime.supervisor.initialize(&live).unwrap();
            full.starts.insert(
                format!("live-{index}"),
                StartReservation {
                    semantic_key: semantic_digest(format!("live-{index}").as_bytes()),
                    supervisor_run_id: live.supervisor_run_id,
                    artifact_repository: None,
                    workspace_id: None,
                    caller_dispatch_run_id: None,
                    worker_session_id: None,
                    worker_agent_id: None,
                    worker_runtime_id: None,
                    worker_profile_id: None,
                    worker_semantic_digest: None,
                },
            );
        }
        assert!(
            runtime
                .ensure_start_capacity(&mut full)
                .unwrap_err()
                .to_string()
                .contains("capacity is exhausted")
        );
        assert!(full.starts.contains_key(&root_operation.to_string()));
        runtime
            .acknowledge_pending_worker_stops(&root_stops)
            .unwrap();
        runtime
            .acknowledge_pending_worker_stops(&root_stops)
            .unwrap();
        assert!(
            runtime
                .pending_worker_stops_for_run(root_run.supervisor_run_id)
                .unwrap()
                .is_empty()
        );

        let delegated_temp = tempfile::tempdir().unwrap();
        let delegated_runtime = SupervisorRuntime::new(delegated_temp.path());
        let dispatch = DispatchStore::new(delegated_temp.path());
        let parent_operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: parent_operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&delegated_runtime, workspace, parent_operation);
        let parent = delegated_runtime
            .start_for_workspace_root_dispatch(
                "goal-composer",
                workspace,
                &parent_operation.to_string(),
                goal("delegated parent"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap();
        let child_operation = OperationId::new();
        delegated_runtime
            .reserve_delegated_dispatch(
                parent_operation,
                &child_operation.to_string(),
                "delegated child",
                now(),
            )
            .unwrap()
            .unwrap();
        delegated_runtime
            .control_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: parent.supervisor_run_id,
                    reason: "cancel unbound child".into(),
                },
                now(),
            )
            .unwrap();
        let delegated_stops = delegated_runtime
            .pending_worker_stops_for_run(parent.supervisor_run_id)
            .unwrap();
        assert_eq!(delegated_stops.len(), 1);
        assert_eq!(delegated_stops[0].operation_id(), child_operation);
        let child = delegated_stops[0]
            .provenance(&delegated_worker(workspace))
            .unwrap();
        assert_eq!(child.parent_dispatch_run, Some(parent_operation));
        assert_eq!(child.dispatch_run_id, child_operation);
        assert!(child.worker_session_id.is_some());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One corruption matrix proves every fail-closed unbound-worker recovery boundary.
    fn pending_worker_stop_recovery_rejects_every_corrupt_reservation_shape() {
        let workspace = WorkspaceId::new();

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let mut state = RuntimeState::default();
        for operation in [OperationId::new(), OperationId::new()] {
            let run = unbound_goal_run(Some(workspace));
            runtime.supervisor.initialize(&run).unwrap();
            state.starts.insert(
                operation.to_string(),
                start_reservation(run.supervisor_run_id),
            );
        }
        runtime.save_state(&state).unwrap();
        assert_eq!(runtime.pending_worker_stops().unwrap().len(), 2);

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let running = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        runtime.supervisor.initialize(&running).unwrap();
        let mut state = RuntimeState::default();
        state.starts.insert(
            OperationId::new().to_string(),
            start_reservation(SupervisorRunId::new()),
        );
        state.starts.insert(
            OperationId::new().to_string(),
            start_reservation(running.supervisor_run_id),
        );
        runtime.save_state(&state).unwrap();
        assert!(runtime.pending_worker_stops().unwrap().is_empty());

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let unscoped = unbound_goal_run(None);
        let missing_root = aborted_run(Some(workspace));
        runtime.supervisor.initialize(&unscoped).unwrap();
        runtime.supervisor.initialize(&missing_root).unwrap();
        let mut state = RuntimeState::default();
        state.starts.insert(
            OperationId::new().to_string(),
            start_reservation(unscoped.supervisor_run_id),
        );
        state.starts.insert(
            OperationId::new().to_string(),
            start_reservation(missing_root.supervisor_run_id),
        );
        runtime.save_state(&state).unwrap();
        assert!(runtime.pending_worker_stops().unwrap().is_empty());

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let root = unbound_goal_run(Some(workspace));
        runtime.supervisor.initialize(&root).unwrap();
        let mut state = RuntimeState::default();
        state.starts.insert(
            "invalid-operation".into(),
            start_reservation(root.supervisor_run_id),
        );
        runtime.save_state(&state).unwrap();
        assert!(
            runtime
                .pending_worker_stops()
                .unwrap_err()
                .to_string()
                .contains("operation is invalid")
        );

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let mut invalid_operation = aborted_run(Some(workspace));
        let invalid_task = task(
            invalid_operation.supervisor_run_id,
            "delegated-invalid-operation",
            None,
        );
        invalid_operation
            .tasks
            .insert(invalid_task.task_id.clone(), invalid_task);
        let operation = OperationId::new();
        let mut wrong_digest = aborted_run(Some(workspace));
        let mut wrong_digest_task = task(
            wrong_digest.supervisor_run_id,
            &format!("{DELEGATED_TASK_PREFIX}{operation}"),
            None,
        );
        wrong_digest_task.instruction_digest = "wrong digest".into();
        wrong_digest
            .tasks
            .insert(wrong_digest_task.task_id.clone(), wrong_digest_task);
        runtime.supervisor.initialize(&invalid_operation).unwrap();
        runtime.supervisor.initialize(&wrong_digest).unwrap();
        assert!(runtime.pending_worker_stops().unwrap().is_empty());

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let operation = OperationId::new();
        let mut missing_parent = aborted_run(Some(workspace));
        let mut child = task(
            missing_parent.supervisor_run_id,
            &format!("{DELEGATED_TASK_PREFIX}{operation}"),
            None,
        );
        child.instruction_digest = delegated_task_digest(operation);
        child.promotion_reserved_at = Some(now());
        child.state = TaskState::Cancelled;
        missing_parent.tasks.insert(child.task_id.clone(), child);
        runtime.supervisor.initialize(&missing_parent).unwrap();
        assert!(
            runtime
                .pending_worker_stops()
                .unwrap_err()
                .to_string()
                .contains("has no parent task")
        );

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let operation = OperationId::new();
        let mut missing_provenance = aborted_run(Some(workspace));
        let mut child = task(
            missing_provenance.supervisor_run_id,
            &format!("{DELEGATED_TASK_PREFIX}{operation}"),
            Some("parent"),
        );
        child.instruction_digest = delegated_task_digest(operation);
        child.promotion_reserved_at = Some(now());
        child.state = TaskState::Cancelled;
        missing_provenance
            .tasks
            .insert(child.task_id.clone(), child);
        runtime.supervisor.initialize(&missing_provenance).unwrap();
        assert!(
            runtime
                .pending_worker_stops()
                .unwrap_err()
                .to_string()
                .contains("parent provenance is missing")
        );

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let operation = OperationId::new();
        let root = unbound_goal_run(Some(workspace));
        runtime.supervisor.initialize(&root).unwrap();
        let mut delegated = aborted_run(Some(workspace));
        let parent_id = TaskId::new("parent").unwrap();
        let parent_dispatch = OperationId::new();
        let parent = task(delegated.supervisor_run_id, "parent", None);
        let mut child = task(
            delegated.supervisor_run_id,
            &format!("{DELEGATED_TASK_PREFIX}{operation}"),
            Some("parent"),
        );
        child.instruction_digest = delegated_task_digest(operation);
        child.promotion_reserved_at = Some(now());
        child.state = TaskState::Cancelled;
        delegated.tasks.insert(parent_id.clone(), parent);
        delegated.tasks.insert(child.task_id.clone(), child);
        delegated.provenance.insert(
            parent_id.clone(),
            provenance(
                delegated.supervisor_run_id,
                &parent_id,
                None,
                parent_dispatch,
            ),
        );
        runtime.supervisor.initialize(&delegated).unwrap();
        let mut state = RuntimeState::default();
        state.starts.insert(
            operation.to_string(),
            start_reservation(root.supervisor_run_id),
        );
        runtime.save_state(&state).unwrap();
        assert!(
            runtime
                .pending_worker_stops()
                .unwrap_err()
                .to_string()
                .contains("multiple aborted supervisor reservations")
        );

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let caller_dispatch = OperationId::new();
        let caller_root = unbound_goal_run(Some(workspace));
        runtime.supervisor.initialize(&caller_root).unwrap();
        let mut state = RuntimeState::default();
        state.starts.insert(
            OperationId::new().to_string(),
            caller_start_reservation(caller_root.supervisor_run_id, workspace, caller_dispatch),
        );
        runtime.save_state(&state).unwrap();
        assert!(
            runtime
                .pending_worker_stops()
                .unwrap_err()
                .to_string()
                .contains("caller root reservation is malformed")
        );

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let caller_dispatch = OperationId::new();
        let mut caller_root = unbound_goal_run(Some(workspace));
        caller_root
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .required_artifact_contract = NO_ARTIFACT_CONTRACT;
        runtime.supervisor.initialize(&caller_root).unwrap();
        let mut state = RuntimeState::default();
        state.starts.insert(
            OperationId::new().to_string(),
            caller_start_reservation(caller_root.supervisor_run_id, workspace, caller_dispatch),
        );
        runtime.save_state(&state).unwrap();
        let stops = runtime.pending_worker_stops().unwrap();
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].operation_id(), caller_dispatch);

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let mut non_goal = unbound_goal_run(Some(workspace));
        non_goal
            .tasks
            .get_mut(&TaskId::new("root").unwrap())
            .unwrap()
            .required_artifact_contract = NO_ARTIFACT_CONTRACT;
        runtime.supervisor.initialize(&non_goal).unwrap();
        let mut state = RuntimeState::default();
        state.starts.insert(
            OperationId::new().to_string(),
            start_reservation(non_goal.supervisor_run_id),
        );
        runtime.save_state(&state).unwrap();
        assert!(runtime.pending_worker_stops().unwrap().is_empty());

        let operation = OperationId::new();
        let mut live = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        live.workspace_id = Some(workspace);
        live.state = SupervisorRunState::Running;
        let parent = task(live.supervisor_run_id, "parent", None);
        let mut pending_child = task(
            live.supervisor_run_id,
            &format!("{DELEGATED_TASK_PREFIX}{operation}"),
            Some("parent"),
        );
        pending_child.instruction_digest = delegated_task_digest(operation);
        pending_child.promotion_reserved_at = Some(now());
        pending_child.promotion_parent_dispatch_run = Some(OperationId::new());
        live.tasks.insert(parent.task_id.clone(), parent);
        live.tasks
            .insert(pending_child.task_id.clone(), pending_child.clone());
        runtime.supervisor.initialize(&live).unwrap();
        assert!(runtime.pending_worker_stops().unwrap().is_empty());
        pending_child.state = TaskState::Cancelled;
        pending_child.generation = 2;
        live.tasks
            .insert(pending_child.task_id.clone(), pending_child);
        runtime.supervisor.initialize(&live).unwrap();
        assert!(
            runtime
                .pending_worker_stops()
                .unwrap_err()
                .to_string()
                .contains("reservation fence is stale")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Forged acknowledgements share one matrix so no recovery fence is tested in isolation.
    fn pending_worker_stop_acknowledgement_is_exact_and_idempotent() {
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        let stop = root_pending_stop(operation, workspace, SupervisorRunId::new());

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let mut delegated = stop.clone();
        delegated.parent_task_id = Some(TaskId::new("parent").unwrap());
        runtime
            .acknowledge_pending_worker_stops(&[delegated])
            .unwrap();
        assert!(
            runtime
                .acknowledge_pending_worker_stops(std::slice::from_ref(&stop))
                .unwrap_err()
                .to_string()
                .contains("reservation disappeared")
        );

        let mut state = RuntimeState::default();
        state.expired_starts.insert(&operation.to_string());
        runtime.save_state(&state).unwrap();
        runtime
            .acknowledge_pending_worker_stops(std::slice::from_ref(&stop))
            .unwrap();

        let owned_run = SupervisorRunId::new();
        let mut state = RuntimeState::default();
        state
            .starts
            .insert(operation.to_string(), start_reservation(owned_run));
        runtime.save_state(&state).unwrap();
        assert!(
            runtime
                .acknowledge_pending_worker_stops(std::slice::from_ref(&stop))
                .unwrap_err()
                .to_string()
                .contains("changed run ownership")
        );

        let missing_run = root_pending_stop(operation, workspace, owned_run);
        assert!(
            runtime
                .acknowledge_pending_worker_stops(std::slice::from_ref(&missing_run))
                .unwrap_err()
                .to_string()
                .contains("run disappeared")
        );

        let stale_run = aborted_run(Some(workspace));
        runtime.supervisor.initialize(&stale_run).unwrap();
        let stale_stop = root_pending_stop(operation, workspace, stale_run.supervisor_run_id);
        let mut state = RuntimeState::default();
        state.starts.insert(
            operation.to_string(),
            start_reservation(stale_run.supervisor_run_id),
        );
        runtime.save_state(&state).unwrap();
        assert!(
            runtime
                .acknowledge_pending_worker_stops(&[stale_stop])
                .unwrap_err()
                .to_string()
                .contains("acknowledgement is stale")
        );

        let caller_temp = tempfile::tempdir().unwrap();
        let caller_runtime = SupervisorRuntime::new(caller_temp.path());
        let mut caller_run = aborted_run(Some(workspace));
        let mut root = task(caller_run.supervisor_run_id, "root", None);
        root.state = TaskState::Cancelled;
        caller_run.tasks.insert(root.task_id.clone(), root);
        caller_runtime.supervisor.initialize(&caller_run).unwrap();
        let caller_operation = OperationId::new();
        let caller_stop =
            root_pending_stop(caller_operation, workspace, caller_run.supervisor_run_id);
        let mut caller_state = RuntimeState::default();
        caller_state.starts.insert(
            OperationId::new().to_string(),
            caller_start_reservation(caller_run.supervisor_run_id, workspace, caller_operation),
        );
        caller_runtime.save_state(&caller_state).unwrap();
        caller_runtime
            .acknowledge_pending_worker_stops(std::slice::from_ref(&caller_stop))
            .unwrap();

        let ambiguous_temp = tempfile::tempdir().unwrap();
        let ambiguous = SupervisorRuntime::new(ambiguous_temp.path());
        ambiguous.supervisor.initialize(&caller_run).unwrap();
        let mut ambiguous_state = RuntimeState::default();
        for _ in 0..2 {
            ambiguous_state.starts.insert(
                OperationId::new().to_string(),
                caller_start_reservation(caller_run.supervisor_run_id, workspace, caller_operation),
            );
        }
        ambiguous.save_state(&ambiguous_state).unwrap();
        assert!(
            ambiguous
                .acknowledge_pending_worker_stops(std::slice::from_ref(&caller_stop))
                .unwrap_err()
                .to_string()
                .contains("ambiguous reservations")
        );

        let moved_temp = tempfile::tempdir().unwrap();
        let moved = SupervisorRuntime::new(moved_temp.path());
        let mut moved_state = RuntimeState::default();
        moved_state.starts.insert(
            OperationId::new().to_string(),
            caller_start_reservation(SupervisorRunId::new(), workspace, caller_operation),
        );
        moved.save_state(&moved_state).unwrap();
        assert!(
            moved
                .acknowledge_pending_worker_stops(&[caller_stop])
                .unwrap_err()
                .to_string()
                .contains("changed run ownership")
        );
    }

    #[test]
    fn expired_workspace_control_operations_are_refused_before_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let run = runtime
            .start_for_workspace(
                "caller",
                workspace,
                "start",
                "root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        let operation = OperationId::new();
        let mut state = RuntimeState::default();
        state.expired_controls.insert(&operation.to_string());
        runtime.save_state(&state).unwrap();
        assert!(
            runtime
                .control_for_workspace(
                    workspace,
                    operation,
                    &SupervisorWorkspaceCommand::Cancel {
                        supervisor_run_id: run.supervisor_run_id,
                        reason: "operator cancelled".into(),
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("outside the retained replay window")
        );

        let recyclable_temp = tempfile::tempdir().unwrap();
        let recyclable = SupervisorRuntime::new(recyclable_temp.path());
        let recyclable_run = recyclable
            .start_for_workspace(
                "caller",
                workspace,
                "recyclable",
                "root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        let mut state = RuntimeState::default();
        for _ in 0..MAX_CONTROL_RESERVATIONS {
            state.controls.insert(
                OperationId::new().to_string(),
                ControlReservation {
                    semantic_digest: format!("sha256:{}", "0".repeat(64)),
                    supervisor_run_id: SupervisorRunId::new(),
                    reserved_at: now(),
                },
            );
        }
        recyclable.save_state(&state).unwrap();
        assert_eq!(
            recyclable
                .control_for_workspace(
                    workspace,
                    OperationId::new(),
                    &SupervisorWorkspaceCommand::Cancel {
                        supervisor_run_id: recyclable_run.supervisor_run_id,
                        reason: "operator cancelled".into(),
                    },
                    now(),
                )
                .unwrap()
                .state,
            SupervisorRunState::Cancelled
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One matrix covers ordering plus each corrupt durable provenance fence.
    fn worker_stop_obligations_sort_exact_workers_and_reject_corrupt_provenance() {
        let workspace = WorkspaceId::new();
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let mut running = SupervisorRun::new(
            "caller".into(),
            "running".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        running.workspace_id = Some(workspace);
        runtime.supervisor.initialize(&running).unwrap();
        for _ in 0..2 {
            let mut run = aborted_run(Some(workspace));
            let task_id = TaskId::new("worker").unwrap();
            let dispatch = OperationId::new();
            let mut worker = task(run.supervisor_run_id, "worker", None);
            worker.assigned_dispatch_run = Some(dispatch);
            run.tasks.insert(task_id.clone(), worker);
            run.provenance.insert(
                task_id.clone(),
                provenance(run.supervisor_run_id, &task_id, None, dispatch),
            );
            runtime.supervisor.initialize(&run).unwrap();
        }
        assert_eq!(runtime.worker_stop_obligations().unwrap().len(), 2);

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let mut missing_task = aborted_run(Some(workspace));
        let missing = TaskId::new("missing").unwrap();
        missing_task.provenance.insert(
            missing.clone(),
            provenance(
                missing_task.supervisor_run_id,
                &missing,
                None,
                OperationId::new(),
            ),
        );
        runtime.supervisor.initialize(&missing_task).unwrap();
        assert!(
            runtime
                .worker_stop_obligations()
                .unwrap_err()
                .to_string()
                .contains("provenance task is missing")
        );

        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let mut stale = aborted_run(Some(workspace));
        let task_id = TaskId::new("worker").unwrap();
        let dispatch = OperationId::new();
        let mut worker = task(stale.supervisor_run_id, "worker", None);
        worker.assigned_dispatch_run = Some(dispatch);
        stale.tasks.insert(task_id.clone(), worker);
        let mut stale_provenance = provenance(stale.supervisor_run_id, &task_id, None, dispatch);
        stale_provenance.supervisor_run_id = SupervisorRunId::new();
        stale.provenance.insert(task_id, stale_provenance);
        runtime.supervisor.initialize(&stale).unwrap();
        assert!(
            runtime
                .worker_stop_obligations()
                .unwrap_err()
                .to_string()
                .contains("provenance fence is stale")
        );
    }

    #[test]
    fn unfinished_workspace_detection_is_scoped_and_terminal_safe() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let workspace = WorkspaceId::new();
        let run = runtime
            .start_for_workspace(
                "caller",
                workspace,
                "start",
                "root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        assert!(runtime.has_unfinished_workspace(workspace).unwrap());
        assert!(
            !runtime
                .has_unfinished_workspace(WorkspaceId::new())
                .unwrap()
        );
        runtime
            .control_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: run.supervisor_run_id,
                    reason: "operator cancelled".into(),
                },
                now(),
            )
            .unwrap();
        assert!(!runtime.has_unfinished_workspace(workspace).unwrap());
    }

    #[test]
    fn start_rejects_an_unresolvable_initial_dag() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let error = runtime
            .start(
                "caller",
                "operation",
                "root".into(),
                vec![InitialTask {
                    task_id: "child".into(),
                    parent_task_id: None,
                    dependencies: vec!["missing".into()],
                    instruction: "child".into(),
                    required_artifact_contract: NO_ARTIFACT_CONTRACT,
                }],
                Some("strict".into()),
                now(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("missing dependency or cycle"));
        let parsed: InitialTask = serde_json::from_value(serde_json::json!({
            "task_id": "default-contract",
            "instruction": "body"
        }))
        .unwrap();
        assert_eq!(parsed.required_artifact_contract, NO_ARTIFACT_CONTRACT);
    }

    #[test]
    fn workspace_listing_exposes_only_explicitly_scoped_runs() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = SupervisorRuntime::new(temp.path());
        let first = WorkspaceId::new();
        let second = WorkspaceId::new();
        let visible = runtime
            .start_for_workspace(
                "caller-a",
                first,
                "scoped-a",
                "root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        runtime
            .start_for_workspace(
                "caller-b",
                second,
                "scoped-b",
                "root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap();
        runtime
            .start("legacy", "unscoped", "root".into(), Vec::new(), None, now())
            .unwrap();

        let listed = runtime.list_workspace(first).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].supervisor_run_id, visible.supervisor_run_id);
        assert_eq!(
            runtime
                .get_for_workspace(first, visible.supervisor_run_id)
                .unwrap(),
            Some(visible)
        );
        assert_eq!(
            runtime
                .get_for_workspace(second, listed[0].supervisor_run_id)
                .unwrap(),
            None
        );
        assert_eq!(
            runtime
                .get_for_workspace(first, SupervisorRunId::new())
                .unwrap(),
            None
        );
        assert!(
            runtime
                .list_workspace(WorkspaceId::new())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn control_helpers_validate_and_project_both_commands() {
        let run = SupervisorRun::new(
            "caller".into(),
            "task".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        let cancel = SupervisorWorkspaceCommand::Cancel {
            supervisor_run_id: run.supervisor_run_id,
            reason: "operator cancelled".into(),
        };
        validate_control_command(&cancel).unwrap();
        let invalid = SupervisorWorkspaceCommand::Cancel {
            supervisor_run_id: run.supervisor_run_id,
            reason: "line\nbreak".into(),
        };
        assert!(validate_control_command(&invalid).is_err());
        let cancel_digest = control_semantic_digest(&cancel).unwrap();
        assert!(cancel_digest.starts_with("sha256:"));
        let event = control_event(
            &run,
            OperationId::new(),
            cancel_digest.clone(),
            &cancel,
            now(),
        );
        assert_eq!(event.source, SupervisorEventSource::Cancel);
        assert!(matches!(
            event.kind,
            SupervisorEventKind::Cancel { task_id: None, ref reason }
                if reason == "operator cancelled"
        ));

        let escalation_id = OperationId::new();
        let resolve = SupervisorWorkspaceCommand::ResolveEscalation {
            supervisor_run_id: run.supervisor_run_id,
            escalation_id,
            decision: EscalationDecision::Fail,
        };
        validate_control_command(&resolve).unwrap();
        let resolve_digest = control_semantic_digest(&resolve).unwrap();
        assert_ne!(resolve_digest, cancel_digest);
        let event = control_event(&run, OperationId::new(), resolve_digest, &resolve, now());
        assert_eq!(event.source, SupervisorEventSource::Admission);
        assert!(matches!(
            event.kind,
            SupervisorEventKind::ResolveEscalation {
                escalation_id: actual,
                decision: EscalationDecision::Fail,
            } if actual == escalation_id
        ));
    }
}
