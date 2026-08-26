//! Durable registry and inboxes for daemon-owned agent dispatch.
//!
//! The legacy-compatible dispatch registry and its workspace ownership sidecar
//! are atomically replaced JSON documents under one cross-process lock. Each
//! caller inbox is an fsynced sequence journal with a derived offset index and
//! an atomic ACK watermark. The same lock serializes append, ACK, migration and
//! compaction so concurrent daemon commands cannot lose one another's updates.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::agent::{
    Agent, AgentProfileId, AgentStatus, CallerRef, DispatchBinding, DispatchRun, InboxMessage,
    ModelSelector, RunStatus,
};
use crate::domain::id::{AgentId, OperationId, SessionId, WorkspaceId};
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};

const REGISTRY_FILE: &str = "dispatch.json";
const WORKSPACE_REGISTRY_FILE: &str = "dispatch-workspaces.json";
const INBOX_DIR: &str = "inbox";
const INBOX_INDEX_SUFFIX: &str = ".index.json";
const INBOX_ACK_SUFFIX: &str = ".ack.json";

/// How many finished dispatch runs the registry keeps.
///
/// Every mutation replaces this whole document, so history that is never dropped
/// makes each dispatch cost more than the last: N dispatches cost O(N²) in
/// bytes read, parsed and written, forever, on a daemon that is meant to run for
/// weeks. Finished runs are kept only so a duplicate report or a reconnecting
/// caller can still find the run it is talking about; past that window the run
/// is history, and history belongs in the inbox that already recorded it.
const RUN_RETENTION: usize = 256;

/// How many already-read messages one caller's inbox keeps.
const INBOX_READ_RETENTION: usize = 256;

/// The hard ceiling on one caller's inbox, unread messages included. Read
/// messages are compacted first; if every slot is unacknowledged, append is
/// rejected without evicting or mutating an existing report.
const INBOX_HARD_LIMIT: usize = 4096;
/// Maximum messages returned by one public inbox page.
pub const INBOX_PAGE_MAX: usize = 100;
/// Reserved inbox segment for a workspace-root caller. A `SessionId` is always a
/// lowercase UUID, so this non-UUID literal can never collide with one.
const ROOT_INBOX_SEGMENT: &str = "workspace-root";

/// Maps an optional owning session to its durable inbox directory segment.
/// `None` is the workspace root; `Some` is the session's UUID.
fn session_segment(session_id: Option<SessionId>) -> String {
    session_id.map_or_else(|| ROOT_INBOX_SEGMENT.to_owned(), |id| id.as_str())
}

/// Stable position of the next inbox message to inspect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxCursor {
    pub next_sequence: u64,
}

/// One bounded inbox query result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxPage {
    pub messages: Vec<InboxMessage>,
    pub next_cursor: InboxCursor,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InboxRecord {
    sequence: u64,
    #[serde(flatten)]
    message: InboxMessage,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct InboxIndexEntry {
    sequence: u64,
    offset: u64,
    created_at: DateTime<Utc>,
    read: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct InboxIndex {
    journal_len: u64,
    valid_len: u64,
    entries: Vec<InboxIndexEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct InboxAck {
    next_sequence: u64,
}

impl Default for InboxAck {
    fn default() -> Self {
        Self { next_sequence: 1 }
    }
}

fn next_inbox_sequence(entries: &[InboxIndexEntry]) -> Result<u64> {
    entries.last().map_or(Ok(1), |entry| {
        entry
            .sequence
            .checked_add(1)
            .context("inbox sequence exhausted")
    })
}

#[derive(Deserialize)]
#[serde(untagged)]
enum InboxLine {
    Record(InboxRecord),
    Legacy(InboxMessage),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Registry {
    agents: Vec<Agent>,
    runs: Vec<DispatchRun>,
    bindings: Vec<DispatchBinding>,
    #[serde(default)]
    prompts: Vec<QueuedPrompt>,
    #[serde(default)]
    admissions: Vec<AgentAdmissionReservation>,
}

/// Workspace-scoped additions kept outside `dispatch.json`.
///
/// A draining predecessor is allowed to update the legacy whole-snapshot
/// registry during a planned rollover. Keeping new fields in a sidecar means
/// that predecessor cannot erase fields it does not understand when it
/// serializes its older schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceRegistry {
    agent_workspaces: BTreeMap<AgentId, WorkspaceId>,
    prompts: Vec<WorkspacePrompt>,
    #[serde(default)]
    lineages: Vec<SessionLineage>,
    #[serde(default)]
    delegation_reservations: Vec<DelegationReservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionLineage {
    workspace: WorkspaceId,
    session: SessionId,
    parent: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DelegationReservation {
    workspace: WorkspaceId,
    operation: OperationId,
    parent: Option<SessionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationReservationOutcome {
    Reserved,
    AlreadyAdmitted,
    LimitReached,
    InProgress,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspacePrompt {
    workspace_id: WorkspaceId,
    session_id: Option<SessionId>,
    prompt: String,
    queued_at: DateTime<Utc>,
    #[serde(default)]
    caller: Option<CallerRef>,
    #[serde(default)]
    operation_id: Option<OperationId>,
}

impl WorkspacePrompt {
    fn into_legacy_shape(self) -> QueuedPrompt {
        QueuedPrompt {
            session_id: self.session_id,
            prompt: self.prompt,
            queued_at: self.queued_at,
            caller: self.caller,
        }
    }
}

fn record_lineage(
    registry: &mut WorkspaceRegistry,
    workspace_id: WorkspaceId,
    session_id: Option<SessionId>,
    parent_session_id: Option<SessionId>,
) -> Result<()> {
    let Some(session_id) = session_id.filter(|session| Some(*session) != parent_session_id) else {
        return Ok(());
    };
    if let Some(existing) = registry
        .lineages
        .iter()
        .find(|lineage| lineage.workspace == workspace_id && lineage.session == session_id)
    {
        if existing.parent != parent_session_id {
            anyhow::bail!("session delegation parent cannot be reassigned");
        }
        return Ok(());
    }
    registry.lineages.push(SessionLineage {
        workspace: workspace_id,
        session: session_id,
        parent: parent_session_id,
    });
    Ok(())
}

/// Durable, secret-free proof that an Agent operation was prepared before its
/// one permitted spawn attempt.  The opaque credential value is deliberately
/// absent; only its daemon-minted ephemeral provenance is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAdmissionReservation {
    pub operation_id: OperationId,
    pub semantic_key: String,
    pub credential_provenance: CredentialProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialProvenance {
    DaemonMintedEphemeral,
}

impl Registry {
    fn reserve_admission(
        &mut self,
        agent: Agent,
        run: DispatchRun,
        binding: DispatchBinding,
        admission: AgentAdmissionReservation,
    ) -> AgentAdmissionReservation {
        if let Some(existing) = self
            .admissions
            .iter()
            .find(|item| item.operation_id == admission.operation_id)
        {
            return existing.clone();
        }
        if let Some(existing) = self
            .agents
            .iter_mut()
            .find(|item| item.agent_id == agent.agent_id)
        {
            *existing = agent;
        } else {
            self.agents.push(agent);
        }
        self.runs.push(run);
        self.bindings.push(binding);
        self.admissions.push(admission.clone());
        admission
    }

    fn commit_admission(&mut self, operation_id: OperationId) -> bool {
        let Some(run) = self
            .runs
            .iter_mut()
            .find(|run| run.run_id == operation_id && run.status == RunStatus::Preparing)
        else {
            return false;
        };
        run.status = RunStatus::Running;
        if let Some(agent) = self
            .agents
            .iter_mut()
            .find(|agent| agent.agent_id == run.agent_id)
        {
            agent.status = AgentStatus::Running;
        }
        true
    }

    fn fail_admission(&mut self, operation_id: OperationId) -> bool {
        let Some(run) = self.runs.iter_mut().find(|run| run.run_id == operation_id) else {
            return false;
        };
        run.status = RunStatus::Failed;
        run.ended_at = Some(Utc::now());
        if let Some(agent) = self
            .agents
            .iter_mut()
            .find(|agent| agent.agent_id == run.agent_id)
        {
            agent.status = AgentStatus::Failed;
            agent.current_run = None;
        }
        true
    }

    /// Drop the finished runs past [`RUN_RETENTION`], oldest first, along with
    /// the bindings and admissions that existed only for them.
    ///
    /// Nothing live is ever dropped. A `Preparing` or `Running` run is an
    /// operation in flight, its binding is how a report finds its caller, and its
    /// admission is the proof that the one permitted spawn was prepared — so all
    /// three are kept regardless of age, and the bound applies to what is left.
    ///
    /// Agents are deliberately not bounded here. They are not history: an
    /// `Exited` agent is the record
    /// [`upsert_agent_by_runtime_model`](DispatchStore::upsert_agent_by_runtime_model)
    /// reuses so a relaunch keeps its identity, and dropping it would mint a new
    /// `AgentId` on every restart. Their count is bounded by the sessions,
    /// runtimes and models in play rather than by how many dispatches have run.
    fn retain_bounded(&mut self) {
        let terminal_run = |run: &DispatchRun| {
            matches!(
                run.status,
                RunStatus::Completed | RunStatus::Failed | RunStatus::NoReport
            )
        };
        let terminal_count = self.runs.iter().filter(|run| terminal_run(run)).count();
        let mut over = terminal_count.saturating_sub(RUN_RETENTION);
        if over == 0 {
            return;
        }
        // `runs` is append-ordered, so dropping from the front drops the runs
        // least likely to be asked about again.
        let mut dropped: Vec<OperationId> = Vec::with_capacity(over);
        self.runs.retain(|run| {
            if over > 0 && terminal_run(run) {
                over -= 1;
                dropped.push(run.run_id);
                return false;
            }
            true
        });
        // Only what this pass orphaned is removed. A binding recorded without a
        // run of its own was never this function's to delete — retention must
        // not turn into a consistency rule it was not asked to enforce.
        self.bindings
            .retain(|binding| !dropped.contains(&binding.run_id));
        self.admissions
            .retain(|admission| !dropped.contains(&admission.operation_id));
    }

    fn reconcile_incomplete_admissions(&mut self) -> usize {
        let mut reconciled = 0;
        for admission in &self.admissions {
            let Some(run) = self
                .runs
                .iter_mut()
                .find(|run| run.run_id == admission.operation_id)
            else {
                continue;
            };
            if !matches!(run.status, RunStatus::Preparing | RunStatus::Running) {
                continue;
            }
            run.status = RunStatus::Failed;
            run.ended_at = Some(Utc::now());
            if let Some(agent) = self
                .agents
                .iter_mut()
                .find(|agent| agent.agent_id == run.agent_id)
            {
                agent.status = AgentStatus::Failed;
                agent.current_run = None;
            }
            reconciled += 1;
        }
        reconciled
    }
}

/// One prompt waiting for the next Agent launch in a durable session scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedPrompt {
    pub session_id: Option<SessionId>,
    pub prompt: String,
    pub queued_at: DateTime<Utc>,
    /// Authenticated parent retained for a later ordinary Agent launch.
    #[serde(default)]
    pub caller: Option<CallerRef>,
}

/// File-backed durable dispatch state rooted at the daemon state directory.
pub struct DispatchStore {
    dir: PathBuf,
    #[cfg(test)]
    inbox_bytes_read: AtomicU64,
}

impl DispatchStore {
    #[must_use]
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().into(),
            #[cfg(test)]
            inbox_bytes_read: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn registry_path(&self) -> PathBuf {
        self.dir.join(REGISTRY_FILE)
    }

    fn workspace_registry_path(&self) -> PathBuf {
        self.dir.join(WORKSPACE_REGISTRY_FILE)
    }

    /// Replaces the next-launch prompt for a session. A single slot prevents a
    /// caller retry from creating an unbounded duplicate queue.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn queue_prompt(
        &self,
        workspace_id: WorkspaceId,
        session_id: Option<SessionId>,
        prompt: String,
        queued_at: DateTime<Utc>,
    ) -> Result<QueuedPrompt> {
        self.queue_prompt_for(workspace_id, session_id, prompt, queued_at, None, None)
    }

    /// Queues a next-launch prompt together with its authenticated parent.
    ///
    /// This is the delayed-launch equivalent of an immediate dispatch binding:
    /// the eventual worker must report to the delegator, not to itself.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn queue_delegated_prompt(
        &self,
        workspace_id: WorkspaceId,
        session_id: Option<SessionId>,
        prompt: String,
        queued_at: DateTime<Utc>,
        caller: CallerRef,
        operation_id: OperationId,
    ) -> Result<QueuedPrompt> {
        self.queue_prompt_for(
            workspace_id,
            session_id,
            prompt,
            queued_at,
            Some(caller),
            Some(operation_id),
        )
    }

    fn queue_prompt_for(
        &self,
        workspace_id: WorkspaceId,
        session_id: Option<SessionId>,
        prompt: String,
        queued_at: DateTime<Utc>,
        caller: Option<CallerRef>,
        operation_id: Option<OperationId>,
    ) -> Result<QueuedPrompt> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut workspace_registry = self.load_workspace_registry()?;
        let mut legacy_registry = session_id
            .is_some()
            .then(|| self.load_registry())
            .transpose()?;
        let queued = WorkspacePrompt {
            workspace_id,
            session_id,
            prompt,
            queued_at,
            caller,
            operation_id,
        };
        if let Some(caller) = &queued.caller {
            if workspace_registry.agent_workspaces.get(&caller.agent_id) != Some(&workspace_id) {
                anyhow::bail!("delegation caller does not belong to the workspace");
            }
            record_lineage(
                &mut workspace_registry,
                workspace_id,
                queued.session_id,
                caller.session_id,
            )?;
        }
        let existing = workspace_registry
            .prompts
            .iter()
            .position(|item| item.workspace_id == workspace_id && item.session_id == session_id);
        if let Some(index) = existing {
            workspace_registry.prompts[index] = queued.clone();
        } else {
            workspace_registry.prompts.push(queued.clone());
        }
        json_file::write_atomic(
            &self.dir,
            &self.workspace_registry_path(),
            &workspace_registry,
        )?;

        // A session UUID is resolved from the fenced workspace before this
        // method is called, so a new prompt may safely supersede its legacy
        // queue slot. Root legacy prompts have no provable workspace and remain
        // quarantined rather than being guessed or silently reassigned.
        if let Some(registry) = legacy_registry.as_mut()
            && let Some(index) = registry
                .prompts
                .iter()
                .position(|item| item.session_id == session_id)
        {
            registry.prompts.remove(index);
            json_file::write_atomic(&self.dir, &self.registry_path(), registry)?;
        }
        Ok(queued.into_legacy_shape())
    }

    /// Reads, without consuming, the prompt waiting for a session launch.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    pub fn queued_prompt(
        &self,
        workspace_id: WorkspaceId,
        session_id: Option<SessionId>,
    ) -> Result<Option<QueuedPrompt>> {
        if let Some(prompt) = self
            .load_workspace_registry()?
            .prompts
            .into_iter()
            .find(|item| item.workspace_id == workspace_id && item.session_id == session_id)
        {
            return Ok(Some(prompt.into_legacy_shape()));
        }
        // Session IDs come from the exact workspace lifecycle and therefore
        // prove which workspace owns a legacy session-scoped prompt. A legacy
        // root slot (`None`) is ambiguous and is deliberately not delivered.
        if session_id.is_some() {
            return Ok(self
                .load_registry()?
                .prompts
                .into_iter()
                .find(|item| item.session_id == session_id));
        }
        Ok(None)
    }

    /// Removes a prompt only after its matching Agent launch succeeded.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn consume_prompt(
        &self,
        workspace_id: WorkspaceId,
        session_id: Option<SessionId>,
    ) -> Result<Option<QueuedPrompt>> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut workspace_registry = self.load_workspace_registry()?;
        if let Some(index) = workspace_registry
            .prompts
            .iter()
            .position(|item| item.workspace_id == workspace_id && item.session_id == session_id)
        {
            let prompt = workspace_registry.prompts.remove(index);
            // Remove a legacy predecessor's same-session slot first. If either
            // write fails, the new scoped prompt remains available for an
            // idempotent retry instead of revealing the superseded prompt on a
            // later launch.
            if session_id.is_some() {
                let mut registry = self.load_registry()?;
                if let Some(index) = registry
                    .prompts
                    .iter()
                    .position(|item| item.session_id == session_id)
                {
                    registry.prompts.remove(index);
                    json_file::write_atomic(&self.dir, &self.registry_path(), &registry)?;
                }
            }
            json_file::write_atomic(
                &self.dir,
                &self.workspace_registry_path(),
                &workspace_registry,
            )?;
            return Ok(Some(prompt.into_legacy_shape()));
        }
        if session_id.is_some() {
            let mut registry = self.load_registry()?;
            if let Some(index) = registry
                .prompts
                .iter()
                .position(|item| item.session_id == session_id)
            {
                let prompt = registry.prompts.remove(index);
                json_file::write_atomic(&self.dir, &self.registry_path(), &registry)?;
                return Ok(Some(prompt));
            }
        }
        Ok(None)
    }

    #[must_use]
    pub fn inbox_path(&self, caller: &CallerRef) -> PathBuf {
        self.dir
            .join(INBOX_DIR)
            .join(session_segment(caller.session_id))
            .join(format!("{}.jsonl", caller.agent_id.as_str()))
    }

    fn inbox_index_path(&self, caller: &CallerRef) -> PathBuf {
        let path = self.inbox_path(caller);
        let mut value = path.into_os_string();
        value.push(INBOX_INDEX_SUFFIX);
        PathBuf::from(value)
    }

    fn inbox_ack_path(&self, caller: &CallerRef) -> PathBuf {
        let path = self.inbox_path(caller);
        let mut value = path.into_os_string();
        value.push(INBOX_ACK_SUFFIX);
        PathBuf::from(value)
    }

    /// Upserts an agent by its never-reused incarnation ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn upsert_agent(&self, workspace_id: WorkspaceId, agent: Agent) -> Result<Agent> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut workspace_registry = self.load_workspace_registry()?;
        if workspace_registry
            .agent_workspaces
            .get(&agent.agent_id)
            .is_some_and(|owner| *owner != workspace_id)
        {
            anyhow::bail!("agent workspace ownership cannot be reassigned");
        }
        let mut registry = self.load_registry()?;
        if let Some(existing) = registry
            .agents
            .iter_mut()
            .find(|item| item.agent_id == agent.agent_id)
        {
            *existing = agent.clone();
        } else {
            registry.agents.push(agent.clone());
        }
        workspace_registry
            .agent_workspaces
            .insert(agent.agent_id, workspace_id);
        self.write_workspace_registry(&workspace_registry)?;
        registry.retain_bounded();
        json_file::write_atomic(&self.dir, &self.registry_path(), &registry)?;
        Ok(agent)
    }

    /// Reuses the agent for this session/runtime/model tuple or creates an idle one.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn upsert_agent_by_runtime_model(
        &self,
        workspace_id: WorkspaceId,
        session_id: Option<SessionId>,
        runtime: AgentProfileId,
        model: ModelSelector,
    ) -> Result<Agent> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut registry = self.load_registry()?;
        let mut workspace_registry = self.load_workspace_registry()?;
        let matches = |agent: &Agent| {
            agent.session_id == session_id && agent.runtime == runtime && agent.model == model
        };
        let existing = registry
            .agents
            .iter()
            .position(|agent| {
                matches(agent)
                    && workspace_registry.agent_workspaces.get(&agent.agent_id)
                        == Some(&workspace_id)
            })
            .or_else(|| {
                // A session ID was resolved through the fenced workspace
                // lifecycle, so it proves ownership of its old Agent. Root
                // Agents have no such proof and receive a fresh identity.
                session_id?;
                registry.agents.iter().position(|agent| {
                    matches(agent)
                        && !workspace_registry
                            .agent_workspaces
                            .contains_key(&agent.agent_id)
                })
            });
        if let Some(index) = existing {
            let agent = registry.agents[index].clone();
            workspace_registry
                .agent_workspaces
                .insert(agent.agent_id, workspace_id);
            self.write_workspace_registry(&workspace_registry)?;
            return Ok(agent);
        }
        let agent = Agent {
            agent_id: AgentId::new(),
            session_id,
            runtime,
            model,
            status: AgentStatus::Idle,
            current_run: None,
        };
        registry.agents.push(agent.clone());
        workspace_registry
            .agent_workspaces
            .insert(agent.agent_id, workspace_id);
        // Publish ownership first. A crash before the legacy registry write
        // can leave only an inert mapping to a nonexistent Agent; the
        // inverse order could publish an unowned Agent that a later
        // workspace might incorrectly adopt.
        self.write_workspace_registry(&workspace_registry)?;
        registry.retain_bounded();
        json_file::write_atomic(&self.dir, &self.registry_path(), &registry)?;
        Ok(agent)
    }

    /// Reads an Agent only when it belongs to `workspace_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    pub fn agent_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    ) -> Result<Option<Agent>> {
        let workspace_registry = self.load_workspace_registry()?;
        if workspace_registry.agent_workspaces.get(&agent_id) != Some(&workspace_id) {
            return Ok(None);
        }
        let registry = self.load_registry()?;
        Ok(registry
            .agents
            .into_iter()
            .find(|agent| agent.agent_id == agent_id))
    }

    /// Every Agent owned by `workspace_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    pub fn agents_in_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Agent>> {
        let workspace_registry = self.load_workspace_registry()?;
        let registry = self.load_registry()?;
        Ok(registry
            .agents
            .into_iter()
            .filter(|agent| {
                workspace_registry.agent_workspaces.get(&agent.agent_id) == Some(&workspace_id)
            })
            .collect())
    }

    /// Resolves the workspace ownership sidecar for one daemon-owned agent.
    ///
    /// # Errors
    ///
    /// Returns an error when the ownership sidecar cannot be read.
    pub fn workspace_for_agent(&self, agent_id: AgentId) -> Result<Option<WorkspaceId>> {
        Ok(self
            .load_workspace_registry()?
            .agent_workspaces
            .get(&agent_id)
            .copied())
    }

    fn persist_binding_lineage(
        &self,
        registry: &mut WorkspaceRegistry,
        binding: &DispatchBinding,
    ) -> Result<()> {
        let worker_id = binding.worker.agent_id;
        let Some(workspace_id) = registry.agent_workspaces.get(&worker_id).copied() else {
            return Ok(());
        };
        record_lineage(
            registry,
            workspace_id,
            binding.worker.session_id,
            binding.caller.session_id,
        )?;
        self.write_workspace_registry(registry)
    }

    /// Atomically reserves one delegation concurrency slot until the caller
    /// publishes a queued prompt or active admission.
    ///
    /// # Errors
    ///
    /// Returns an error when durable state cannot be read or written, or the
    /// caller has no authenticated workspace ownership.
    pub fn reserve_delegation(
        &self,
        caller: &CallerRef,
        operation_id: OperationId,
        max_concurrency: usize,
    ) -> Result<DelegationReservationOutcome> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut workspace_registry = self.load_workspace_registry()?;
        let workspace_id = workspace_registry
            .agent_workspaces
            .get(&caller.agent_id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("caller workspace ownership is unavailable"))?;
        let registry = self.load_registry()?;
        if registry.bindings.iter().any(|binding| {
            binding.run_id == operation_id
                && binding.caller.session_id == caller.session_id
                && workspace_registry
                    .agent_workspaces
                    .get(&binding.worker.agent_id)
                    == Some(&workspace_id)
        }) || workspace_registry.prompts.iter().any(|prompt| {
            prompt.operation_id == Some(operation_id)
                && prompt.workspace_id == workspace_id
                && prompt
                    .caller
                    .as_ref()
                    .is_some_and(|parent| parent.session_id == caller.session_id)
        }) {
            return Ok(DelegationReservationOutcome::AlreadyAdmitted);
        }
        if workspace_registry
            .delegation_reservations
            .iter()
            .any(|reservation| reservation.operation == operation_id)
        {
            return Ok(DelegationReservationOutcome::InProgress);
        }
        let active = registry
            .bindings
            .iter()
            .filter(|binding| {
                binding.caller.session_id == caller.session_id
                    && binding.worker.session_id != caller.session_id
                    && workspace_registry
                        .agent_workspaces
                        .get(&binding.worker.agent_id)
                        == Some(&workspace_id)
            })
            .filter(|binding| {
                registry.runs.iter().any(|run| {
                    run.run_id == binding.run_id
                        && matches!(run.status, RunStatus::Preparing | RunStatus::Running)
                })
            })
            .count();
        let queued = workspace_registry
            .prompts
            .iter()
            .filter(|prompt| {
                prompt.workspace_id == workspace_id
                    && prompt.session_id != caller.session_id
                    && prompt
                        .caller
                        .as_ref()
                        .is_some_and(|parent| parent.session_id == caller.session_id)
            })
            .count();
        let reserved = workspace_registry
            .delegation_reservations
            .iter()
            .filter(|reservation| {
                reservation.workspace == workspace_id && reservation.parent == caller.session_id
            })
            .count();
        if active.saturating_add(queued).saturating_add(reserved) >= max_concurrency {
            return Ok(DelegationReservationOutcome::LimitReached);
        }
        workspace_registry
            .delegation_reservations
            .push(DelegationReservation {
                workspace: workspace_id,
                operation: operation_id,
                parent: caller.session_id,
            });
        self.write_workspace_registry(&workspace_registry)?;
        Ok(DelegationReservationOutcome::Reserved)
    }

    /// Releases a transient delegation slot after publication or failure.
    ///
    /// # Errors
    ///
    /// Returns an error when durable state cannot be read or written.
    pub fn release_delegation(&self, operation_id: OperationId) -> Result<bool> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut workspace_registry = self.load_workspace_registry()?;
        let before = workspace_registry.delegation_reservations.len();
        workspace_registry
            .delegation_reservations
            .retain(|reservation| reservation.operation != operation_id);
        let changed = workspace_registry.delegation_reservations.len() != before;
        if changed {
            self.write_workspace_registry(&workspace_registry)?;
        }
        Ok(changed)
    }

    /// Returns immutable session parentage retained independently of run
    /// history.
    ///
    /// # Errors
    ///
    /// Returns an error when durable state cannot be read.
    pub fn session_parent(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
    ) -> Result<Option<SessionId>> {
        Ok(self
            .load_workspace_registry()?
            .lineages
            .into_iter()
            .find(|lineage| lineage.workspace == workspace_id && lineage.session == session_id)
            .and_then(|lineage| lineage.parent))
    }

    /// Resolves the absolute delegation depth of an organization member from
    /// durable session parentage. Runtime/model replacement does not reset it.
    ///
    /// # Errors
    ///
    /// Returns an error when either durable registry cannot be read or the
    /// caller no longer has workspace ownership.
    pub fn delegation_depth(&self, caller: &CallerRef) -> Result<usize> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let workspace_registry = self.load_workspace_registry()?;
        let workspace_id = workspace_registry
            .agent_workspaces
            .get(&caller.agent_id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("caller workspace ownership is unavailable"))?;
        let mut depth = 0usize;
        let mut cursor = caller.session_id;
        let mut seen = BTreeSet::new();
        while let Some(session_id) = cursor
            && seen.insert(session_id)
        {
            let Some(parent) = workspace_registry
                .lineages
                .iter()
                .find(|lineage| lineage.workspace == workspace_id && lineage.session == session_id)
            else {
                break;
            };
            depth = depth.saturating_add(1);
            cursor = parent.parent;
        }
        Ok(depth)
    }

    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    pub fn agent(&self, agent_id: AgentId) -> Result<Option<Agent>> {
        Ok(self
            .load_registry()?
            .agents
            .into_iter()
            .find(|agent| agent.agent_id == agent_id))
    }

    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    pub fn agents(&self) -> Result<Vec<Agent>> {
        Ok(self.load_registry()?.agents)
    }

    /// Returns every durable dispatch run for daemon-side reconciliation.
    /// Callers must still use the run ID and binding fence before acting.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    pub fn runs(&self) -> Result<Vec<DispatchRun>> {
        Ok(self.load_registry()?.runs)
    }

    /// Reads one run by its durable operation identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    pub fn run(&self, operation_id: OperationId) -> Result<Option<DispatchRun>> {
        Ok(self
            .load_registry()?
            .runs
            .into_iter()
            .find(|run| run.run_id == operation_id))
    }

    /// Reads the durable admission fence for one operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    pub fn admission(
        &self,
        operation_id: OperationId,
    ) -> Result<Option<AgentAdmissionReservation>> {
        Ok(self
            .load_registry()?
            .admissions
            .into_iter()
            .find(|admission| admission.operation_id == operation_id))
    }

    /// Atomically reserves every dispatch-side fact required to authorize one
    /// spawn.  Retrying an existing reservation never rewrites its provenance.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn reserve_admission(
        &self,
        agent: Agent,
        run: DispatchRun,
        binding: DispatchBinding,
        admission: AgentAdmissionReservation,
    ) -> Result<AgentAdmissionReservation> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut registry = self.load_registry()?;
        let mut workspace_registry = self.load_workspace_registry()?;
        let workspace_id = workspace_registry
            .agent_workspaces
            .get(&binding.worker.agent_id)
            .copied()
            .context("worker workspace ownership is unavailable")?;
        let child = binding.worker.session_id;
        let parent = binding.caller.session_id;
        record_lineage(&mut workspace_registry, workspace_id, child, parent)?;
        self.write_workspace_registry(&workspace_registry)?;
        let reservation = registry.reserve_admission(agent, run, binding, admission);
        json_file::write_atomic(&self.dir, &self.registry_path(), &registry)?;
        Ok(reservation)
    }

    /// Atomically publishes a prepared admission as live only after the PTY
    /// spawn and runtime commit both succeeded.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn commit_admission(&self, operation_id: OperationId) -> Result<bool> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut registry = self.load_registry()?;
        let committed = registry.commit_admission(operation_id);
        json_file::write_atomic(&self.dir, &self.registry_path(), &registry)?;
        Ok(committed)
    }

    /// Records the safe terminal result of a compensated or interrupted
    /// admission. This is best-effort after a store failure; the still-durable
    /// `Preparing` state is also reconciled fail-closed on restart.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn fail_admission(&self, operation_id: OperationId) -> Result<bool> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut registry = self.load_registry()?;
        let failed = registry.fail_admission(operation_id);
        json_file::write_atomic(&self.dir, &self.registry_path(), &registry)?;
        Ok(failed)
    }

    /// Fails every run which was still non-terminal when the daemon lost its
    /// in-memory credential and PTY ownership.  Such an admission is never a
    /// reason to spawn a replacement after restart.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn reconcile_incomplete_admissions(&self) -> Result<usize> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut registry = self.load_registry()?;
        let mut workspace_registry = self.load_workspace_registry()?;
        let reconciled = registry.reconcile_incomplete_admissions();
        // These guards represent code executing in the previous daemon
        // process. No credential or worker survives restart, so retaining one
        // would leak a concurrency slot forever.
        if !workspace_registry.delegation_reservations.is_empty() {
            workspace_registry.delegation_reservations.clear();
            self.write_workspace_registry(&workspace_registry)?;
        }
        json_file::write_atomic(&self.dir, &self.registry_path(), &registry)?;
        Ok(reconciled)
    }

    /// Adds or replaces a run by `run_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn upsert_run(&self, run: DispatchRun) -> Result<DispatchRun> {
        self.mutate_registry(|registry| {
            if let Some(existing) = registry
                .runs
                .iter_mut()
                .find(|item| item.run_id == run.run_id)
            {
                *existing = run.clone();
            } else {
                registry.runs.push(run.clone());
            }
            run
        })
    }

    /// Transitions a run and records its completion timestamp when supplied.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn transition_run(
        &self,
        run_id: OperationId,
        status: RunStatus,
        ended_at: Option<DateTime<Utc>>,
    ) -> Result<Option<DispatchRun>> {
        self.mutate_registry(|registry| {
            let run = registry.runs.iter_mut().find(|run| run.run_id == run_id)?;
            run.status = status;
            run.ended_at = ended_at;
            Some(run.clone())
        })
    }

    /// Transitions an agent's durable availability and current run reference.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn transition_agent(
        &self,
        agent_id: AgentId,
        status: AgentStatus,
        current_run: Option<OperationId>,
    ) -> Result<Option<Agent>> {
        self.mutate_registry(|registry| {
            let agent = registry
                .agents
                .iter_mut()
                .find(|agent| agent.agent_id == agent_id)?;
            agent.status = status;
            agent.current_run = current_run;
            Some(agent.clone())
        })
    }

    /// Atomically converges one reported run and its Agent without overwriting
    /// a newer run that has already reused the same Agent identity.
    ///
    /// The run is always reconciled by its exact operation ID. The Agent is
    /// released only while `current_run` still points at that same operation;
    /// a successor admission is therefore a fence, not state for an older
    /// report retry to clear.
    ///
    /// Returns whether the durable registry changed.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn reconcile_report_outcome(
        &self,
        run_id: OperationId,
        agent_id: AgentId,
        run_status: RunStatus,
        agent_status: AgentStatus,
        ended_at: DateTime<Utc>,
    ) -> Result<bool> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut registry = self.load_registry()?;
        let mut changed = false;
        if let Some(run) = registry.runs.iter_mut().find(|run| run.run_id == run_id)
            && run.status != run_status
        {
            run.status = run_status;
            run.ended_at = Some(ended_at);
            changed = true;
        }
        if let Some(agent) = registry
            .agents
            .iter_mut()
            .find(|agent| agent.agent_id == agent_id && agent.current_run == Some(run_id))
        {
            agent.status = agent_status;
            agent.current_run = None;
            changed = true;
        }
        if changed {
            registry.retain_bounded();
            json_file::write_atomic(&self.dir, &self.registry_path(), &registry)?;
        }
        Ok(changed)
    }

    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn upsert_binding(&self, binding: DispatchBinding) -> Result<DispatchBinding> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut registry = self.load_registry()?;
        let mut workspace_registry = self.load_workspace_registry()?;
        self.persist_binding_lineage(&mut workspace_registry, &binding)?;
        if let Some(existing) = registry
            .bindings
            .iter_mut()
            .find(|item| item.run_id == binding.run_id)
        {
            *existing = binding.clone();
        } else {
            registry.bindings.push(binding.clone());
        }
        registry.retain_bounded();
        json_file::write_atomic(&self.dir, &self.registry_path(), &registry)?;
        Ok(binding)
    }

    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    pub fn binding(&self, run_id: OperationId) -> Result<Option<DispatchBinding>> {
        Ok(self
            .load_registry()?
            .bindings
            .into_iter()
            .find(|binding| binding.run_id == run_id))
    }

    /// Returns the retained caller-to-worker lineage used for organization
    /// policy and read-only projections.
    ///
    /// # Errors
    ///
    /// Returns an error when the dispatch registry cannot be read.
    pub fn bindings(&self) -> Result<Vec<DispatchBinding>> {
        Ok(self.load_registry()?.bindings)
    }

    /// Appends a report to the caller's durable inbox.
    ///
    /// # Errors
    ///
    /// Returns an error when the inbox cannot be locked, read, or written.
    #[allow(clippy::too_many_lines)]
    pub fn append_inbox(&self, caller: &CallerRef, mut message: InboxMessage) -> Result<()> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let path = self.inbox_path(caller);
        let mut index = self.inbox_index(caller)?;
        let ack = self.read_inbox_ack(caller)?;
        Self::validate_inbox_ack(ack, &index)?;
        let read_count = index
            .entries
            .iter()
            .filter(|entry| entry.read || entry.sequence < ack.next_sequence)
            .count();
        if read_count > INBOX_READ_RETENTION || index.entries.len() >= INBOX_HARD_LIMIT {
            index = self.compact_inbox(caller, &index, ack)?;
        }
        if index.entries.len() >= INBOX_HARD_LIMIT {
            anyhow::bail!("dispatch inbox capacity is exhausted by unacknowledged messages");
        }

        let parent = path.parent().context("dispatch inbox path has no parent")?;
        fs::create_dir_all(parent).context(format!("failed to create {}", parent.display()))?;
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut offset = file.metadata()?.len();
        if index.valid_len < offset {
            file.set_len(index.valid_len)?;
            offset = index.valid_len;
            index.journal_len = offset;
        }
        let sequence = next_inbox_sequence(&index.entries)?;
        message.read = false;
        let record = InboxRecord { sequence, message };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_all()?;
        let journal_len = offset + u64::try_from(bytes.len())?;
        index.entries.push(InboxIndexEntry {
            sequence,
            offset,
            created_at: record.message.created_at,
            read: false,
        });
        index.journal_len = journal_len;
        index.valid_len = journal_len;
        self.write_inbox_index(caller, &index)
    }

    /// Returns a stable, bounded page without acknowledging it.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid/expired cursor or unreadable state.
    pub fn inbox_page(
        &self,
        caller: &CallerRef,
        cursor: Option<InboxCursor>,
        limit: usize,
        unread_only: bool,
        since: Option<DateTime<Utc>>,
    ) -> Result<InboxPage> {
        if !(1..=INBOX_PAGE_MAX).contains(&limit) {
            anyhow::bail!("dispatch inbox page limit must be 1..={INBOX_PAGE_MAX}");
        }
        if cursor.is_some_and(|value| value.next_sequence == 0) {
            anyhow::bail!("dispatch inbox cursor sequence must be positive");
        }
        let _lock = StoreLock::acquire(&self.dir)?;
        let index = self.inbox_index(caller)?;
        let ack = self.read_inbox_ack(caller)?;
        Self::validate_inbox_ack(ack, &index)?;
        let end = next_inbox_sequence(&index.entries)?;
        if cursor.is_some_and(|value| value.next_sequence > end) {
            anyhow::bail!("dispatch inbox cursor is outside the retained sequence range");
        }
        let Some(first) = index.entries.first() else {
            let next_sequence = cursor.map_or(ack.next_sequence, |value| value.next_sequence);
            return Ok(InboxPage {
                messages: Vec::new(),
                next_cursor: InboxCursor { next_sequence },
                has_more: false,
            });
        };
        if cursor.is_some_and(|value| value.next_sequence < first.sequence) {
            anyhow::bail!(
                "dispatch inbox cursor expired: earliest retained sequence is {}",
                first.sequence
            );
        }
        let mut start = cursor.map_or(first.sequence, |value| value.next_sequence);
        if unread_only {
            start = start.max(ack.next_sequence);
        }
        if let Some(since) = since {
            let since_sequence = index
                .entries
                .iter()
                .find(|entry| entry.created_at > since)
                .map_or(end, |entry| entry.sequence);
            start = start.max(since_sequence);
        }
        let selected = index
            .entries
            .iter()
            .filter(|entry| entry.sequence >= start)
            .filter(|entry| since.is_none_or(|value| entry.created_at > value))
            .filter(|entry| !unread_only || (!entry.read && entry.sequence >= ack.next_sequence))
            .take(limit + 1)
            .copied()
            .collect::<Vec<_>>();
        let has_more = selected.len() > limit;
        let page_entries = &selected[..selected.len().min(limit)];
        let mut records = self.read_inbox_records(caller, page_entries)?;
        for record in &mut records {
            record.message.read = record.message.read || record.sequence < ack.next_sequence;
        }
        let next_sequence = if has_more {
            records
                .last()
                .context("dispatch inbox page cursor has no returned record")?
                .sequence
                + 1
        } else {
            end.max(start)
        };
        Ok(InboxPage {
            messages: records.into_iter().map(|record| record.message).collect(),
            next_cursor: InboxCursor { next_sequence },
            has_more,
        })
    }

    /// Advances the caller's durable ACK watermark. Repeating the same or an
    /// older ACK is effect-free.
    ///
    /// # Errors
    ///
    /// Returns an error when the cursor is outside the published inbox range or
    /// the ACK state cannot be persisted.
    pub fn ack_inbox(&self, caller: &CallerRef, cursor: InboxCursor) -> Result<InboxCursor> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let index = self.inbox_index(caller)?;
        let end = next_inbox_sequence(&index.entries)?;
        if cursor.next_sequence == 0 || cursor.next_sequence > end {
            anyhow::bail!("dispatch inbox ACK cursor is outside the published sequence range");
        }
        let mut ack = self.read_inbox_ack(caller)?;
        Self::validate_inbox_ack(ack, &index)?;
        if cursor.next_sequence > ack.next_sequence {
            ack.next_sequence = cursor.next_sequence;
            let path = self.inbox_ack_path(caller);
            let parent = path
                .parent()
                .context("dispatch inbox ACK path has no parent")?;
            json_file::write_atomic(parent, &path, &ack)?;
        }
        Ok(InboxCursor {
            next_sequence: ack.next_sequence,
        })
    }

    /// Compatibility projection for internal exact-run recovery. Public callers
    /// use [`Self::inbox_page`] so response work is bounded by a page.
    ///
    /// # Errors
    ///
    /// Returns an error when the inbox cannot be read.
    pub fn inbox(&self, caller: &CallerRef) -> Result<Vec<InboxMessage>> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let index = self.inbox_index(caller)?;
        let ack = self.read_inbox_ack(caller)?;
        Self::validate_inbox_ack(ack, &index)?;
        let mut records = self.read_inbox_records(caller, &index.entries)?;
        for record in &mut records {
            record.message.read = record.message.read || record.sequence < ack.next_sequence;
        }
        Ok(records.into_iter().map(|record| record.message).collect())
    }

    /// # Errors
    ///
    /// Returns an error when the inbox cannot be read.
    pub fn unread_inbox(&self, caller: &CallerRef) -> Result<Vec<InboxMessage>> {
        Ok(self
            .inbox(caller)?
            .into_iter()
            .filter(|message| !message.read)
            .collect())
    }

    /// Marks all messages for `run_id` read and returns whether anything changed.
    ///
    /// # Errors
    ///
    /// Returns an error when the inbox cannot be locked, read, or written.
    pub fn mark_inbox_read(&self, caller: &CallerRef, run_id: OperationId) -> Result<bool> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let index = self.inbox_index(caller)?;
        let mut records = self.read_inbox_records(caller, &index.entries)?;
        let mut changed = false;
        for record in &mut records {
            if record.message.run_id == run_id && !record.message.read {
                record.message.read = true;
                changed = true;
            }
        }
        if changed {
            let mut remove = records
                .iter()
                .filter(|record| record.message.read)
                .count()
                .saturating_sub(INBOX_READ_RETENTION);
            records.retain(|record| {
                if record.message.read && remove > 0 {
                    remove -= 1;
                    false
                } else {
                    true
                }
            });
            self.write_inbox_records(caller, &records)?;
        }
        Ok(changed)
    }

    fn mutate_registry<T>(&self, mutate: impl FnOnce(&mut Registry) -> T) -> Result<T> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut registry = self.load_registry()?;
        let result = mutate(&mut registry);
        // Bounding on every write makes the bound a property of the document
        // rather than of a maintenance tick that may never run on a daemon that
        // is restarted often.
        registry.retain_bounded();
        json_file::write_atomic(&self.dir, &self.registry_path(), &registry)?;
        Ok(result)
    }

    fn load_registry(&self) -> Result<Registry> {
        Ok(json_file::read(&self.registry_path())?.unwrap_or_default())
    }

    fn load_workspace_registry(&self) -> Result<WorkspaceRegistry> {
        Ok(json_file::read(&self.workspace_registry_path())?.unwrap_or_default())
    }

    fn write_workspace_registry(&self, registry: &WorkspaceRegistry) -> Result<()> {
        json_file::write_atomic(&self.dir, &self.workspace_registry_path(), registry)
    }

    fn read_inbox_ack(&self, caller: &CallerRef) -> Result<InboxAck> {
        Ok(json_file::read(&self.inbox_ack_path(caller))?.unwrap_or_default())
    }

    fn validate_inbox_ack(ack: InboxAck, index: &InboxIndex) -> Result<()> {
        let end = next_inbox_sequence(&index.entries)?;
        if ack.next_sequence == 0 || ack.next_sequence > end {
            anyhow::bail!("dispatch inbox ACK state is outside the published sequence range");
        }
        Ok(())
    }

    fn inbox_index(&self, caller: &CallerRef) -> Result<InboxIndex> {
        let journal_len = match fs::metadata(self.inbox_path(caller)) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(InboxIndex::default());
            }
            Err(error) => return Err(error).context("failed to inspect dispatch inbox"),
        };
        if let Ok(Some(index)) = json_file::read::<InboxIndex>(&self.inbox_index_path(caller))
            && index.journal_len == journal_len
            && index.valid_len <= index.journal_len
            && index.entries.first().is_none_or(|entry| entry.offset == 0)
            && index.entries.first().is_none_or(|entry| entry.sequence > 0)
            && index
                .entries
                .last()
                .is_none_or(|entry| entry.offset < index.valid_len)
            && index
                .entries
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence && pair[0].offset < pair[1].offset)
        {
            return Ok(index);
        }
        self.rebuild_inbox_index(caller)
    }

    #[allow(clippy::too_many_lines)]
    fn rebuild_inbox_index(&self, caller: &CallerRef) -> Result<InboxIndex> {
        let path = self.inbox_path(caller);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(InboxIndex::default());
            }
            Err(error) => return Err(error).context(format!("failed to read {}", path.display())),
        };
        let journal_len = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut index = InboxIndex {
            journal_len,
            ..InboxIndex::default()
        };
        let mut records = Vec::new();
        let mut migrated = false;
        loop {
            let offset = index.valid_len;
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 {
                break;
            }
            #[cfg(test)]
            self.inbox_bytes_read
                .fetch_add(bytes as u64, Ordering::Relaxed);
            // A terminating LF is the journal commit marker. A writer crash can
            // leave a complete-looking JSON value at EOF, but it was not
            // durably published and must be truncated by the next append.
            if !line.ends_with('\n') {
                break;
            }
            let record = match serde_json::from_str::<InboxLine>(line.trim_end_matches('\n')) {
                Ok(InboxLine::Record(record)) => record,
                Ok(InboxLine::Legacy(message)) => {
                    migrated = true;
                    InboxRecord {
                        sequence: next_inbox_sequence(&index.entries)?,
                        message,
                    }
                }
                Err(error) => return Err(error).context("failed to parse dispatch inbox message"),
            };
            // Compaction removes acknowledged/read records without renumbering:
            // cursors already handed to callers must remain stable. The retained
            // journal can therefore start above one or contain gaps, but sequence
            // identity must stay positive and strictly increasing.
            if record.sequence == 0
                || index
                    .entries
                    .last()
                    .is_some_and(|entry| record.sequence <= entry.sequence)
            {
                anyhow::bail!("dispatch inbox sequence is not strictly increasing");
            }
            if records.len() >= INBOX_HARD_LIMIT {
                anyhow::bail!("dispatch inbox exceeds its hard limit");
            }
            index.entries.push(InboxIndexEntry {
                sequence: record.sequence,
                offset,
                created_at: record.message.created_at,
                read: record.message.read,
            });
            index.valid_len += u64::try_from(bytes)?;
            records.push(record);
        }
        if migrated {
            return self.write_inbox_records(caller, &records);
        }
        self.write_inbox_index(caller, &index)?;
        Ok(index)
    }

    fn write_inbox_index(&self, caller: &CallerRef, index: &InboxIndex) -> Result<()> {
        json_file::write_atomic_cache(&self.dir, &self.inbox_index_path(caller), index)
    }

    fn read_inbox_records(
        &self,
        caller: &CallerRef,
        entries: &[InboxIndexEntry],
    ) -> Result<Vec<InboxRecord>> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let path = self.inbox_path(caller);
        let mut file =
            fs::File::open(&path).context(format!("failed to read {}", path.display()))?;
        let mut records = Vec::new();
        for entry in entries {
            file.seek(SeekFrom::Start(entry.offset))?;
            let mut reader = BufReader::new(&file);
            let mut line = String::new();
            let bytes = reader.read_line(&mut line)?;
            if bytes == 0 || !line.ends_with('\n') {
                anyhow::bail!("dispatch inbox index points beyond its journal");
            }
            #[cfg(test)]
            self.inbox_bytes_read
                .fetch_add(bytes as u64, Ordering::Relaxed);
            let record: InboxRecord = serde_json::from_str(line.trim_end_matches('\n'))
                .context("failed to parse indexed dispatch inbox message")?;
            if record.sequence != entry.sequence {
                anyhow::bail!("dispatch inbox index does not match its journal");
            }
            records.push(record);
        }
        Ok(records)
    }

    fn write_inbox_records(
        &self,
        caller: &CallerRef,
        records: &[InboxRecord],
    ) -> Result<InboxIndex> {
        let path = self.inbox_path(caller);
        let parent = path.parent().context("dispatch inbox path has no parent")?;
        fs::create_dir_all(parent).context(format!("failed to create {}", parent.display()))?;
        let mut offset = 0_u64;
        let mut text = String::new();
        let mut entries = Vec::with_capacity(records.len());
        for record in records {
            entries.push(InboxIndexEntry {
                sequence: record.sequence,
                offset,
                created_at: record.message.created_at,
                read: record.message.read,
            });
            let line = serde_json::to_string(record)?;
            offset += u64::try_from(line.len() + 1)?;
            text.push_str(&line);
            text.push('\n');
        }
        // The index is a disposable cache. Remove it before replacing the
        // journal so a crash cannot leave a same-length stale index that later
        // causes committed records to be hidden or truncated.
        match fs::remove_file(self.inbox_index_path(caller)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("failed to retire dispatch inbox index"),
        }
        json_file::write_text_atomic(&path, &text)?;
        let index = InboxIndex {
            journal_len: offset,
            valid_len: offset,
            entries,
        };
        self.write_inbox_index(caller, &index)?;
        Ok(index)
    }

    fn compact_inbox(
        &self,
        caller: &CallerRef,
        index: &InboxIndex,
        ack: InboxAck,
    ) -> Result<InboxIndex> {
        let mut records = self.read_inbox_records(caller, &index.entries)?;
        let read_count = records
            .iter()
            .filter(|record| record.message.read || record.sequence < ack.next_sequence)
            .count();
        let retention_excess = read_count.saturating_sub(INBOX_READ_RETENTION);
        let capacity_excess = records
            .len()
            .saturating_add(1)
            .saturating_sub(INBOX_HARD_LIMIT)
            .min(read_count);
        let mut remove = retention_excess.max(capacity_excess);
        if remove == 0 {
            return Ok(index.clone());
        }
        records.retain_mut(|record| {
            let read = record.message.read || record.sequence < ack.next_sequence;
            record.message.read = read;
            if read && remove > 0 {
                remove -= 1;
                false
            } else {
                true
            }
        });
        self.write_inbox_records(caller, &records)
    }
}

impl Clone for DispatchStore {
    fn clone(&self) -> Self {
        Self::new(&self.dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{InboxKind, StructuredResult, WorkerRef};
    use chrono::TimeZone;
    use std::sync::Arc;
    use std::thread;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap()
    }
    fn ids() -> (SessionId, AgentId, CallerRef) {
        let session = SessionId::new();
        let agent = AgentId::new();
        (
            session,
            agent,
            CallerRef {
                session_id: Some(session),
                agent_id: agent,
            },
        )
    }

    #[test]
    fn clone_preserves_the_durable_root_without_sharing_test_observation_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        store.inbox_bytes_read.store(7, Ordering::Relaxed);

        let cloned = store.clone();

        assert_eq!(cloned.dir, store.dir);
        assert_eq!(cloned.inbox_bytes_read.load(Ordering::Relaxed), 0);
    }
    fn agent(session_id: SessionId, agent_id: AgentId) -> Agent {
        Agent {
            agent_id,
            session_id: Some(session_id),
            runtime: AgentProfileId::new("codex").unwrap(),
            model: ModelSelector::new("gpt-5").unwrap(),
            status: AgentStatus::Idle,
            current_run: None,
        }
    }
    fn message(run_id: OperationId, worker: WorkerRef) -> InboxMessage {
        InboxMessage {
            run_id,
            from: worker,
            kind: InboxKind::Completed,
            summary: "done".into(),
            result: Some(StructuredResult {
                pr: Some("#321".into()),
                commits: vec!["abc".into()],
                changed_files: vec!["file".into()],
                verification: Some("test".into()),
            }),
            created_at: now(),
            read: false,
        }
    }

    #[test]
    fn registry_upserts_and_transitions_dispatch_entities() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let workspace = WorkspaceId::new();
        let (session, agent_id, caller) = ids();
        let first = agent(session, agent_id);
        assert_eq!(store.upsert_agent(workspace, first.clone()).unwrap(), first);
        let replacement = Agent {
            status: AgentStatus::Exited,
            ..first.clone()
        };
        assert_eq!(
            store.upsert_agent(workspace, replacement.clone()).unwrap(),
            replacement
        );
        let reused = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(session),
                first.runtime.clone(),
                first.model.clone(),
            )
            .unwrap();
        assert_eq!(reused.agent_id, agent_id);
        let created = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(session),
                AgentProfileId::new("claude").unwrap(),
                first.model.clone(),
            )
            .unwrap();
        assert_ne!(created.agent_id, agent_id);
        let run = DispatchRun {
            run_id: OperationId::new(),
            agent_id,
            prompt: "work".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        };
        store.upsert_run(run.clone()).unwrap();
        let replaced_run = DispatchRun {
            prompt: "updated work".into(),
            ..run.clone()
        };
        assert_eq!(
            store.upsert_run(replaced_run.clone()).unwrap(),
            replaced_run
        );
        assert_eq!(
            store
                .transition_run(run.run_id, RunStatus::Completed, Some(now()))
                .unwrap()
                .unwrap()
                .status,
            RunStatus::Completed
        );
        assert!(
            store
                .transition_run(OperationId::new(), RunStatus::Failed, None)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .transition_agent(agent_id, AgentStatus::Running, Some(run.run_id))
                .unwrap()
                .unwrap()
                .current_run,
            Some(run.run_id)
        );
        assert!(
            store
                .transition_agent(AgentId::new(), AgentStatus::Failed, None)
                .unwrap()
                .is_none()
        );
        let binding = DispatchBinding {
            run_id: run.run_id,
            caller,
            worker: WorkerRef {
                session_id: Some(session),
                agent_id,
            },
        };
        assert_eq!(store.upsert_binding(binding.clone()).unwrap(), binding);
        assert_eq!(store.upsert_binding(binding.clone()).unwrap(), binding);
        assert_eq!(store.binding(run.run_id).unwrap(), Some(binding));
        assert_eq!(
            store.agent(agent_id).unwrap().unwrap().status,
            AgentStatus::Running
        );
        assert_eq!(store.agents().unwrap().len(), 2);
        assert!(store.registry_path().is_file());
    }

    #[test]
    fn legacy_unowned_binding_does_not_invent_workspace_lineage() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (_, _, caller) = ids();
        let binding = DispatchBinding {
            run_id: OperationId::new(),
            caller,
            worker: WorkerRef {
                session_id: Some(SessionId::new()),
                agent_id: AgentId::new(),
            },
        };

        assert_eq!(store.upsert_binding(binding.clone()).unwrap(), binding);
        assert_eq!(store.binding(binding.run_id).unwrap(), Some(binding));
        assert!(store.load_workspace_registry().unwrap().lineages.is_empty());
    }

    #[test]
    fn agent_workspace_ownership_cannot_be_reassigned() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, _) = ids();
        let agent = agent(session, agent_id);
        store
            .upsert_agent(WorkspaceId::new(), agent.clone())
            .unwrap();
        assert!(
            store
                .upsert_agent(WorkspaceId::new(), agent)
                .unwrap_err()
                .to_string()
                .contains("ownership cannot be reassigned")
        );
    }

    #[test]
    fn report_reconciliation_is_atomic_and_preserves_a_successor_run() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, _) = ids();
        let first_run = OperationId::new();
        let second_run = OperationId::new();
        let workspace = WorkspaceId::new();
        let mut worker = agent(session, agent_id);
        worker.status = AgentStatus::Running;
        worker.current_run = Some(first_run);
        store.upsert_agent(workspace, worker).unwrap();
        store
            .upsert_run(DispatchRun {
                run_id: first_run,
                agent_id,
                prompt: "first".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();

        assert!(
            store
                .reconcile_report_outcome(
                    first_run,
                    agent_id,
                    RunStatus::Completed,
                    AgentStatus::Idle,
                    now(),
                )
                .unwrap()
        );
        let first = store.run(first_run).unwrap().unwrap();
        assert_eq!(first.status, RunStatus::Completed);
        assert_eq!(first.ended_at, Some(now()));
        assert_eq!(store.agent(agent_id).unwrap().unwrap().current_run, None);
        assert!(
            !store
                .reconcile_report_outcome(
                    first_run,
                    agent_id,
                    RunStatus::Completed,
                    AgentStatus::Idle,
                    now(),
                )
                .unwrap(),
            "an already converged retry must not rewrite the registry"
        );

        store
            .transition_agent(agent_id, AgentStatus::Running, Some(second_run))
            .unwrap();
        assert!(
            !store
                .reconcile_report_outcome(
                    first_run,
                    agent_id,
                    RunStatus::Completed,
                    AgentStatus::Idle,
                    now(),
                )
                .unwrap(),
            "the successor run fences its Agent state from the old report"
        );
        let preserved = store.agent(agent_id).unwrap().unwrap();
        assert_eq!(preserved.status, AgentStatus::Running);
        assert_eq!(preserved.current_run, Some(second_run));

        store
            .transition_run(first_run, RunStatus::Running, None)
            .unwrap();
        assert!(
            store
                .reconcile_report_outcome(
                    first_run,
                    agent_id,
                    RunStatus::Completed,
                    AgentStatus::Idle,
                    now(),
                )
                .unwrap(),
            "the old run still converges without clearing its successor"
        );
        let preserved = store.agent(agent_id).unwrap().unwrap();
        assert_eq!(preserved.status, AgentStatus::Running);
        assert_eq!(preserved.current_run, Some(second_run));
    }

    #[test]
    fn prompt_queue_replaces_peeks_and_consumes_per_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let workspace = WorkspaceId::new();
        let other_workspace = WorkspaceId::new();
        let session = SessionId::new();
        store
            .queue_prompt(workspace, Some(session), "first".into(), now())
            .unwrap();
        store
            .queue_prompt(workspace, Some(session), "second".into(), now())
            .unwrap();
        store
            .queue_prompt(workspace, None, "root".into(), now())
            .unwrap();
        store
            .queue_prompt(other_workspace, None, "other root".into(), now())
            .unwrap();
        assert_eq!(
            store
                .queued_prompt(workspace, Some(session))
                .unwrap()
                .unwrap()
                .prompt,
            "second"
        );
        assert_eq!(
            store
                .consume_prompt(workspace, Some(session))
                .unwrap()
                .unwrap()
                .prompt,
            "second"
        );
        assert!(
            store
                .queued_prompt(workspace, Some(session))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .consume_prompt(workspace, None)
                .unwrap()
                .unwrap()
                .prompt,
            "root"
        );
        assert_eq!(
            store
                .consume_prompt(other_workspace, None)
                .unwrap()
                .unwrap()
                .prompt,
            "other root"
        );
        assert!(store.consume_prompt(workspace, None).unwrap().is_none());
    }

    #[test]
    fn delegated_prompt_retains_authenticated_parent_until_consumed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let parent_session = SessionId::new();
        let parent = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(parent_session),
                AgentProfileId::new("codex").unwrap(),
                ModelSelector::new("parent").unwrap(),
            )
            .unwrap();
        let caller = CallerRef {
            session_id: Some(parent_session),
            agent_id: parent.agent_id,
        };
        store
            .queue_delegated_prompt(
                workspace,
                Some(session),
                "delegated work".into(),
                now(),
                caller.clone(),
                OperationId::new(),
            )
            .unwrap();

        assert_eq!(
            store
                .queued_prompt(workspace, Some(session))
                .unwrap()
                .unwrap()
                .caller,
            Some(caller.clone())
        );
        assert_eq!(
            store
                .consume_prompt(workspace, Some(session))
                .unwrap()
                .unwrap()
                .caller,
            Some(caller)
        );
    }

    #[test]
    fn delegation_depth_survives_manager_runtime_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let workspace = WorkspaceId::new();
        let manager_session = SessionId::new();
        let original_manager = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(manager_session),
                AgentProfileId::new("codex").unwrap(),
                ModelSelector::new("first").unwrap(),
            )
            .unwrap();
        let replacement_manager = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(manager_session),
                AgentProfileId::new("claude").unwrap(),
                ModelSelector::new("second").unwrap(),
            )
            .unwrap();
        let director = store
            .upsert_agent_by_runtime_model(
                workspace,
                None,
                AgentProfileId::new("codex").unwrap(),
                ModelSelector::new("director").unwrap(),
            )
            .unwrap();
        let manager_run = OperationId::new();
        store
            .upsert_binding(DispatchBinding {
                run_id: manager_run,
                caller: CallerRef {
                    session_id: None,
                    agent_id: director.agent_id,
                },
                worker: WorkerRef {
                    session_id: Some(manager_session),
                    agent_id: original_manager.agent_id,
                },
            })
            .unwrap();
        assert_eq!(
            store
                .delegation_depth(&CallerRef {
                    session_id: Some(manager_session),
                    agent_id: replacement_manager.agent_id,
                })
                .unwrap(),
            1,
            "a replacement Manager retains its session's parent depth"
        );
        let isolated = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(SessionId::new()),
                AgentProfileId::new("codex").unwrap(),
                ModelSelector::new("isolated").unwrap(),
            )
            .unwrap();
        assert_eq!(
            store
                .delegation_depth(&CallerRef {
                    session_id: isolated.session_id,
                    agent_id: isolated.agent_id,
                })
                .unwrap(),
            0
        );
        let unknown = CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: AgentId::new(),
        };
        assert!(store.delegation_depth(&unknown).is_err());
    }

    #[test]
    fn delegation_reservation_closes_the_concurrency_check_to_publish_gap() {
        use std::sync::{Arc, Barrier};

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(DispatchStore::new(tmp.path()));
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let manager = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(session),
                AgentProfileId::new("codex").unwrap(),
                ModelSelector::new("manager").unwrap(),
            )
            .unwrap();
        let caller = CallerRef {
            session_id: Some(session),
            agent_id: manager.agent_id,
        };
        let barrier = Arc::new(Barrier::new(3));
        let operations = [OperationId::new(), OperationId::new()];
        let workers = operations
            .into_iter()
            .map(|operation_id| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let caller = caller.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.reserve_delegation(&caller, operation_id, 1).unwrap()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DelegationReservationOutcome::Reserved)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == DelegationReservationOutcome::LimitReached)
                .count(),
            1
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One graph covers every reservation and immutable-lineage boundary.
    fn delegation_reservation_replay_and_lineage_fail_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let workspace = WorkspaceId::new();
        let parent_session = SessionId::new();
        let parent = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(parent_session),
                AgentProfileId::new("codex").unwrap(),
                ModelSelector::new("parent").unwrap(),
            )
            .unwrap();
        let caller = CallerRef {
            session_id: Some(parent_session),
            agent_id: parent.agent_id,
        };
        assert!(
            store
                .reserve_delegation(
                    &CallerRef {
                        session_id: None,
                        agent_id: AgentId::new(),
                    },
                    OperationId::new(),
                    1,
                )
                .is_err()
        );

        let reserved = OperationId::new();
        assert_eq!(
            store.reserve_delegation(&caller, reserved, 1).unwrap(),
            DelegationReservationOutcome::Reserved
        );
        assert_eq!(
            store.reserve_delegation(&caller, reserved, 1).unwrap(),
            DelegationReservationOutcome::InProgress
        );
        assert!(store.release_delegation(reserved).unwrap());
        assert!(!store.release_delegation(reserved).unwrap());

        let child_session = SessionId::new();
        let child = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(child_session),
                AgentProfileId::new("codex").unwrap(),
                ModelSelector::new("child").unwrap(),
            )
            .unwrap();
        let active_run = OperationId::new();
        store
            .upsert_run(DispatchRun {
                run_id: active_run,
                agent_id: child.agent_id,
                prompt: "active".into(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        let binding = DispatchBinding {
            run_id: active_run,
            caller: caller.clone(),
            worker: WorkerRef {
                session_id: Some(child_session),
                agent_id: child.agent_id,
            },
        };
        store.upsert_binding(binding.clone()).unwrap();
        assert_eq!(store.bindings().unwrap(), vec![binding]);
        assert_eq!(
            store.reserve_delegation(&caller, active_run, 2).unwrap(),
            DelegationReservationOutcome::AlreadyAdmitted
        );
        assert_eq!(
            store
                .reserve_delegation(&caller, OperationId::new(), 1)
                .unwrap(),
            DelegationReservationOutcome::LimitReached
        );

        let queued_operation = OperationId::new();
        let queued_session = SessionId::new();
        store
            .queue_delegated_prompt(
                workspace,
                Some(queued_session),
                "queued".into(),
                now(),
                caller.clone(),
                queued_operation,
            )
            .unwrap();
        assert_eq!(
            store
                .reserve_delegation(&caller, queued_operation, 3)
                .unwrap(),
            DelegationReservationOutcome::AlreadyAdmitted
        );
        assert!(
            store
                .queue_delegated_prompt(
                    WorkspaceId::new(),
                    Some(SessionId::new()),
                    "wrong workspace".into(),
                    now(),
                    caller.clone(),
                    OperationId::new(),
                )
                .is_err()
        );

        let other_parent_session = SessionId::new();
        let other_parent = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(other_parent_session),
                AgentProfileId::new("claude").unwrap(),
                ModelSelector::new("other-parent").unwrap(),
            )
            .unwrap();
        let conflicting = CallerRef {
            session_id: Some(other_parent_session),
            agent_id: other_parent.agent_id,
        };
        assert!(
            store
                .queue_delegated_prompt(
                    workspace,
                    Some(queued_session),
                    "reparent".into(),
                    now(),
                    conflicting.clone(),
                    OperationId::new(),
                )
                .is_err()
        );
        assert!(
            store
                .upsert_binding(DispatchBinding {
                    run_id: OperationId::new(),
                    caller: conflicting,
                    worker: WorkerRef {
                        session_id: Some(child_session),
                        agent_id: child.agent_id,
                    },
                })
                .is_err()
        );

        let unowned_agent = AgentId::new();
        assert!(
            store
                .reserve_admission(
                    agent(SessionId::new(), unowned_agent),
                    DispatchRun {
                        run_id: OperationId::new(),
                        agent_id: unowned_agent,
                        prompt: "unowned".into(),
                        started_at: now(),
                        ended_at: None,
                        status: RunStatus::Preparing,
                    },
                    DispatchBinding {
                        run_id: OperationId::new(),
                        caller: caller.clone(),
                        worker: WorkerRef {
                            session_id: Some(SessionId::new()),
                            agent_id: unowned_agent,
                        },
                    },
                    AgentAdmissionReservation {
                        operation_id: OperationId::new(),
                        semantic_key: "unowned".into(),
                        credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
                    },
                )
                .is_err()
        );

        let admitted_agent_id = AgentId::new();
        let admitted_session = SessionId::new();
        let admitted_operation = OperationId::new();
        let mut ownership = store.load_workspace_registry().unwrap();
        ownership
            .agent_workspaces
            .insert(admitted_agent_id, workspace);
        store.write_workspace_registry(&ownership).unwrap();
        let admitted = agent(admitted_session, admitted_agent_id);
        store
            .reserve_admission(
                admitted,
                DispatchRun {
                    run_id: admitted_operation,
                    agent_id: admitted_agent_id,
                    prompt: "admitted".into(),
                    started_at: now(),
                    ended_at: None,
                    status: RunStatus::Preparing,
                },
                DispatchBinding {
                    run_id: admitted_operation,
                    caller: caller.clone(),
                    worker: WorkerRef {
                        session_id: Some(admitted_session),
                        agent_id: admitted_agent_id,
                    },
                },
                AgentAdmissionReservation {
                    operation_id: admitted_operation,
                    semantic_key: "admitted".into(),
                    credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
                },
            )
            .unwrap();
        assert!(store.agent(admitted_agent_id).unwrap().is_some());

        let conflicting_operation = OperationId::new();
        assert!(
            store
                .reserve_admission(
                    agent(admitted_session, admitted_agent_id),
                    DispatchRun {
                        run_id: conflicting_operation,
                        agent_id: admitted_agent_id,
                        prompt: "reparent admitted worker".into(),
                        started_at: now(),
                        ended_at: None,
                        status: RunStatus::Preparing,
                    },
                    DispatchBinding {
                        run_id: conflicting_operation,
                        caller: CallerRef {
                            session_id: Some(other_parent_session),
                            agent_id: other_parent.agent_id,
                        },
                        worker: WorkerRef {
                            session_id: Some(admitted_session),
                            agent_id: admitted_agent_id,
                        },
                    },
                    AgentAdmissionReservation {
                        operation_id: conflicting_operation,
                        semantic_key: "conflicting-parent".into(),
                        credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
                    },
                )
                .is_err()
        );
        assert!(store.run(conflicting_operation).unwrap().is_none());
        assert!(store.admission(conflicting_operation).unwrap().is_none());

        assert_eq!(
            store
                .reserve_delegation(&caller, OperationId::new(), 10)
                .unwrap(),
            DelegationReservationOutcome::Reserved
        );
        store.reconcile_incomplete_admissions().unwrap();
        assert!(
            store
                .reserve_delegation(&caller, OperationId::new(), 10)
                .is_ok()
        );
    }

    #[test]
    fn session_lineage_survives_dispatch_run_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let workspace = WorkspaceId::new();
        let parent_session = SessionId::new();
        let child_session = SessionId::new();
        let parent = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(parent_session),
                AgentProfileId::new("codex").unwrap(),
                ModelSelector::new("parent").unwrap(),
            )
            .unwrap();
        let child = store
            .upsert_agent_by_runtime_model(
                workspace,
                Some(child_session),
                AgentProfileId::new("codex").unwrap(),
                ModelSelector::new("child").unwrap(),
            )
            .unwrap();
        let lineage_run = OperationId::new();
        store
            .upsert_run(DispatchRun {
                run_id: lineage_run,
                agent_id: child.agent_id,
                prompt: "lineage".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
        store
            .upsert_binding(DispatchBinding {
                run_id: lineage_run,
                caller: CallerRef {
                    session_id: Some(parent_session),
                    agent_id: parent.agent_id,
                },
                worker: WorkerRef {
                    session_id: Some(child_session),
                    agent_id: child.agent_id,
                },
            })
            .unwrap();
        for _ in 0..=RUN_RETENTION {
            store
                .upsert_run(DispatchRun {
                    run_id: OperationId::new(),
                    agent_id: child.agent_id,
                    prompt: "history".into(),
                    started_at: now(),
                    ended_at: Some(now()),
                    status: RunStatus::Completed,
                })
                .unwrap();
        }
        assert!(store.binding(lineage_run).unwrap().is_none());
        assert_eq!(
            store.session_parent(workspace, child_session).unwrap(),
            Some(parent_session)
        );
        assert_eq!(
            store
                .delegation_depth(&CallerRef {
                    session_id: Some(child_session),
                    agent_id: child.agent_id,
                })
                .unwrap(),
            1
        );
    }

    #[test]
    fn legacy_session_prompt_is_delivered_but_ambiguous_root_prompt_is_quarantined() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let mut legacy = Registry::default();
        legacy.prompts.push(QueuedPrompt {
            session_id: Some(session),
            prompt: "session work".into(),
            queued_at: now(),
            caller: None,
        });
        legacy.prompts.push(QueuedPrompt {
            session_id: None,
            prompt: "unknown root work".into(),
            queued_at: now(),
            caller: None,
        });
        json_file::write_atomic(tmp.path(), &store.registry_path(), &legacy).unwrap();

        assert_eq!(
            store
                .queued_prompt(workspace, Some(session))
                .unwrap()
                .unwrap()
                .prompt,
            "session work"
        );
        assert_eq!(
            store
                .consume_prompt(workspace, Some(session))
                .unwrap()
                .unwrap()
                .prompt,
            "session work"
        );
        assert!(store.queued_prompt(workspace, None).unwrap().is_none());
        assert_eq!(store.load_registry().unwrap().prompts.len(), 1);

        let replacement_session = SessionId::new();
        store
            .mutate_registry(|registry| {
                registry.prompts.push(QueuedPrompt {
                    session_id: Some(replacement_session),
                    prompt: "superseded legacy work".into(),
                    queued_at: now(),
                    caller: None,
                });
            })
            .unwrap();
        store
            .queue_prompt(
                workspace,
                Some(replacement_session),
                "new scoped work".into(),
                now(),
            )
            .unwrap();
        assert_eq!(store.load_registry().unwrap().prompts.len(), 1);

        // Emulate an old predecessor restoring its same-session slot after the
        // scoped prompt was queued. Consumption removes both copies so a later
        // launch cannot reveal the superseded prompt.
        store
            .mutate_registry(|registry| {
                registry.prompts.push(QueuedPrompt {
                    session_id: Some(replacement_session),
                    prompt: "late legacy work".into(),
                    queued_at: now(),
                    caller: None,
                });
            })
            .unwrap();
        assert_eq!(
            store
                .consume_prompt(workspace, Some(replacement_session))
                .unwrap()
                .unwrap()
                .prompt,
            "new scoped work"
        );
        assert_eq!(store.load_registry().unwrap().prompts.len(), 1);
        assert!(
            store
                .consume_prompt(workspace, Some(SessionId::new()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn scoped_prompt_write_failures_are_reported_without_consuming_the_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let workspace = WorkspaceId::new();
        json_file::fail_next_atomic_write(
            &store.workspace_registry_path(),
            json_file::AtomicWriteStage::Write,
        );
        assert!(
            store
                .queue_prompt(workspace, None, "not queued".into(), now())
                .is_err()
        );
        assert!(store.queued_prompt(workspace, None).unwrap().is_none());

        store
            .queue_prompt(workspace, None, "still queued".into(), now())
            .unwrap();
        json_file::fail_next_atomic_write(
            &store.workspace_registry_path(),
            json_file::AtomicWriteStage::Rename,
        );
        assert!(store.consume_prompt(workspace, None).is_err());
        assert_eq!(
            store
                .queued_prompt(workspace, None)
                .unwrap()
                .unwrap()
                .prompt,
            "still queued"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One reservation lifecycle keeps prepare, commit, and restart reconciliation together.
    fn admission_reservation_is_atomic_secret_free_and_reconciles_incomplete_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let operation = OperationId::new();
        let mut worker = agent(session, agent_id);
        worker.status = AgentStatus::Starting;
        worker.current_run = Some(operation);
        store
            .upsert_agent(WorkspaceId::new(), worker.clone())
            .unwrap();
        let run = DispatchRun {
            run_id: operation,
            agent_id,
            prompt: "work".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Preparing,
        };
        let binding = DispatchBinding {
            run_id: operation,
            caller,
            worker: WorkerRef {
                session_id: Some(session),
                agent_id,
            },
        };
        let reservation = AgentAdmissionReservation {
            operation_id: operation,
            semantic_key: "intent".into(),
            credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
        };

        assert_eq!(
            store
                .reserve_admission(worker, run, binding.clone(), reservation.clone())
                .unwrap(),
            reservation
        );
        let existing_agent = store.agent(agent_id).unwrap().unwrap();
        let existing_run = store.run(operation).unwrap().unwrap();
        assert_eq!(
            store
                .reserve_admission(
                    existing_agent,
                    existing_run,
                    binding.clone(),
                    store.admission(operation).unwrap().unwrap(),
                )
                .unwrap()
                .operation_id,
            operation
        );
        let different_operation = OperationId::new();
        let mut replacement = agent(session, agent_id);
        replacement.status = AgentStatus::Starting;
        replacement.current_run = Some(different_operation);
        let mut different_binding = binding.clone();
        different_binding.run_id = different_operation;
        let different_reservation = AgentAdmissionReservation {
            operation_id: different_operation,
            semantic_key: "different-intent".into(),
            credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
        };
        assert_eq!(
            store
                .reserve_admission(
                    replacement,
                    DispatchRun {
                        run_id: different_operation,
                        agent_id,
                        prompt: "different work".into(),
                        started_at: now(),
                        ended_at: None,
                        status: RunStatus::Preparing,
                    },
                    different_binding,
                    different_reservation.clone(),
                )
                .unwrap(),
            different_reservation
        );
        assert_eq!(store.admission(operation).unwrap(), Some(reservation));
        assert_eq!(store.binding(operation).unwrap(), Some(binding));
        let serialized = fs::read_to_string(store.registry_path()).unwrap();
        assert!(serialized.contains("daemon_minted_ephemeral"));
        assert!(!serialized.contains("USAGI_MCP_CALLER_CREDENTIAL"));

        assert!(store.commit_admission(operation).unwrap());
        assert!(!store.commit_admission(operation).unwrap());
        assert_eq!(store.runs().unwrap()[0].status, RunStatus::Running);
        assert_eq!(store.reconcile_incomplete_admissions().unwrap(), 2);
        assert_eq!(store.runs().unwrap()[0].status, RunStatus::Failed);
        assert_eq!(
            store.agent(agent_id).unwrap().unwrap().status,
            AgentStatus::Failed
        );
        assert_eq!(store.reconcile_incomplete_admissions().unwrap(), 0);
        assert!(store.fail_admission(operation).unwrap());
        assert!(!store.fail_admission(OperationId::new()).unwrap());

        store
            .mutate_registry(|registry| registry.runs.clear())
            .unwrap();
        assert_eq!(store.reconcile_incomplete_admissions().unwrap(), 0);
    }

    #[test]
    fn inbox_is_jsonl_durable_and_filters_then_marks_unread_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let run_id = OperationId::new();
        let worker = WorkerRef {
            session_id: Some(session),
            agent_id,
        };
        store
            .append_inbox(&caller, message(run_id, worker.clone()))
            .unwrap();
        let other = OperationId::new();
        store.append_inbox(&caller, message(other, worker)).unwrap();
        assert_eq!(store.unread_inbox(&caller).unwrap().len(), 2);
        assert!(store.mark_inbox_read(&caller, run_id).unwrap());
        assert!(!store.mark_inbox_read(&caller, run_id).unwrap());
        assert_eq!(store.unread_inbox(&caller).unwrap().len(), 1);
        assert!(store.inbox_path(&caller).is_file());
        let text = fs::read_to_string(store.inbox_path(&caller)).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn inbox_pages_and_explicit_ack_converge_without_response_loss() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let worker = WorkerRef {
            session_id: Some(session),
            agent_id,
        };
        assert!(
            store
                .inbox_page(&caller, None, 1, true, None)
                .unwrap()
                .messages
                .is_empty()
        );
        for limit in [0, INBOX_PAGE_MAX + 1] {
            assert!(store.inbox_page(&caller, None, limit, false, None).is_err());
        }
        let run_ids = (0..3).map(|_| OperationId::new()).collect::<Vec<_>>();
        for run_id in &run_ids {
            store
                .append_inbox(&caller, message(*run_id, worker.clone()))
                .unwrap();
        }

        let first = store.inbox_page(&caller, None, 2, true, None).unwrap();
        assert_eq!(
            first
                .messages
                .iter()
                .map(|item| item.run_id)
                .collect::<Vec<_>>(),
            run_ids[..2]
        );
        assert!(first.has_more);
        assert_eq!(first.next_cursor.next_sequence, 3);
        assert!(
            store
                .inbox_page(&caller, None, 2, false, Some(now()))
                .unwrap()
                .messages
                .is_empty()
        );
        assert_eq!(
            store.inbox_page(&caller, None, 2, true, None).unwrap(),
            first,
            "a lost response without ACK must replay the same page"
        );

        assert_eq!(
            store.ack_inbox(&caller, first.next_cursor).unwrap(),
            first.next_cursor
        );
        assert_eq!(
            store.ack_inbox(&caller, first.next_cursor).unwrap(),
            first.next_cursor
        );
        let reopened = DispatchStore::new(tmp.path());
        let unread = reopened.inbox_page(&caller, None, 2, true, None).unwrap();
        assert_eq!(unread.messages.len(), 1);
        assert_eq!(unread.messages[0].run_id, run_ids[2]);
        assert_eq!(unread.next_cursor.next_sequence, 4);
        reopened.ack_inbox(&caller, unread.next_cursor).unwrap();
        assert!(
            reopened
                .inbox_page(&caller, None, 2, true, None)
                .unwrap()
                .messages
                .is_empty()
        );
        assert!(
            reopened
                .inbox_page(
                    &caller,
                    Some(InboxCursor { next_sequence: 0 }),
                    1,
                    false,
                    None,
                )
                .is_err()
        );
        assert!(
            reopened
                .ack_inbox(&caller, InboxCursor { next_sequence: 5 })
                .is_err()
        );
        assert!(
            reopened
                .inbox_page(
                    &caller,
                    Some(InboxCursor { next_sequence: 5 }),
                    1,
                    false,
                    None,
                )
                .unwrap_err()
                .to_string()
                .contains("outside the retained")
        );
    }

    #[test]
    fn indexed_inbox_pages_read_only_page_records_and_legacy_files_migrate() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let worker = WorkerRef {
            session_id: Some(session),
            agent_id,
        };
        let records = (1..=u64::try_from(INBOX_HARD_LIMIT).unwrap())
            .map(|sequence| InboxRecord {
                sequence,
                message: message(OperationId::new(), worker.clone()),
            })
            .collect::<Vec<_>>();
        let index = store.write_inbox_records(&caller, &records).unwrap();

        store.inbox_bytes_read.store(0, Ordering::Relaxed);
        let page = store
            .inbox_page(
                &caller,
                Some(InboxCursor {
                    next_sequence: u64::try_from(INBOX_HARD_LIMIT - 99).unwrap(),
                }),
                INBOX_PAGE_MAX,
                false,
                None,
            )
            .unwrap();
        assert_eq!(page.messages.len(), INBOX_PAGE_MAX);
        assert!(!page.has_more);
        assert!(store.inbox_bytes_read.load(Ordering::Relaxed) * 20 < index.journal_len);
        fs::write(store.inbox_index_path(&caller), "{broken").unwrap();
        assert_eq!(
            store
                .inbox_page(
                    &caller,
                    Some(InboxCursor {
                        next_sequence: u64::try_from(INBOX_HARD_LIMIT).unwrap(),
                    }),
                    1,
                    false,
                    None,
                )
                .unwrap()
                .messages
                .len(),
            1
        );
        assert_eq!(
            store.inbox_index(&caller).unwrap().entries.len(),
            INBOX_HARD_LIMIT
        );

        let legacy_caller = CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: AgentId::new(),
        };
        let legacy = [
            message(OperationId::new(), worker.clone()),
            message(OperationId::new(), worker),
        ];
        let text = legacy
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n")
            + "\n";
        let path = store.inbox_path(&legacy_caller);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        assert_eq!(
            store
                .inbox_page(&legacy_caller, None, 10, false, None)
                .unwrap()
                .messages,
            legacy
        );
        assert!(fs::read_to_string(path).unwrap().contains("\"sequence\":1"));
    }

    #[test]
    fn locked_mutations_do_not_lose_concurrent_inbox_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(DispatchStore::new(tmp.path()));
        let (session, agent_id, caller) = ids();
        let worker = WorkerRef {
            session_id: Some(session),
            agent_id,
        };
        let mut handles = Vec::new();
        for _ in 0..2 {
            let store = Arc::clone(&store);
            let caller = caller.clone();
            let worker = worker.clone();
            handles.push(thread::spawn(move || {
                store
                    .append_inbox(&caller, message(OperationId::new(), worker))
                    .unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(store.inbox(&caller).unwrap().len(), 2);
    }

    #[test]
    fn missing_torn_and_invalid_inboxes_are_handled() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let worker = WorkerRef {
            session_id: Some(session),
            agent_id,
        };
        assert!(store.inbox(&caller).unwrap().is_empty());
        fs::create_dir_all(store.inbox_path(&caller).parent().unwrap()).unwrap();
        fs::write(store.inbox_path(&caller), "broken\nalso-broken\n").unwrap();
        assert!(store.inbox(&caller).is_err());
        fs::write(store.inbox_path(&caller), "{final-torn").unwrap();
        assert!(store.inbox(&caller).unwrap().is_empty());
        let complete_but_uncommitted = serde_json::to_string(&InboxRecord {
            sequence: 1,
            message: message(OperationId::new(), worker.clone()),
        })
        .unwrap();
        fs::write(store.inbox_path(&caller), complete_but_uncommitted).unwrap();
        assert!(store.inbox(&caller).unwrap().is_empty());
        store
            .append_inbox(&caller, message(OperationId::new(), worker))
            .unwrap();
        assert_eq!(store.inbox(&caller).unwrap().len(), 1);
        assert!(
            fs::read_to_string(store.inbox_path(&caller))
                .unwrap()
                .ends_with('\n')
        );
        fs::remove_file(store.inbox_path(&caller)).unwrap();
        fs::create_dir(store.inbox_path(&caller)).unwrap();
        assert!(store.inbox(&caller).is_err());
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::too_many_lines)]
    fn inbox_journal_and_derived_index_corruption_fail_closed() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let worker = WorkerRef {
            session_id: Some(session),
            agent_id,
        };
        let path = store.inbox_path(&caller);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        assert!(
            store
                .rebuild_inbox_index(&caller)
                .unwrap()
                .entries
                .is_empty()
        );
        fs::create_dir(&path).unwrap();
        assert!(store.rebuild_inbox_index(&caller).is_err());
        fs::remove_dir(&path).unwrap();
        symlink(&path, &path).unwrap();
        assert!(store.rebuild_inbox_index(&caller).is_err());
        assert!(store.inbox_index(&caller).is_err());
        fs::remove_file(&path).unwrap();

        let invalid = InboxRecord {
            sequence: 0,
            message: message(OperationId::new(), worker.clone()),
        };
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&invalid).unwrap()),
        )
        .unwrap();
        assert!(
            store
                .rebuild_inbox_index(&caller)
                .unwrap_err()
                .to_string()
                .contains("strictly increasing")
        );

        let records = (1..=u64::try_from(INBOX_HARD_LIMIT + 1).unwrap())
            .map(|sequence| InboxRecord {
                sequence,
                message: message(OperationId::new(), worker.clone()),
            })
            .collect::<Vec<_>>();
        let text = records
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n")
            + "\n";
        fs::write(&path, text).unwrap();
        assert!(
            store
                .rebuild_inbox_index(&caller)
                .unwrap_err()
                .to_string()
                .contains("hard limit")
        );

        let record = InboxRecord {
            sequence: 7,
            message: message(OperationId::new(), worker),
        };
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&record).unwrap()),
        )
        .unwrap();
        let matching = InboxIndexEntry {
            sequence: 7,
            offset: 0,
            created_at: record.message.created_at,
            read: false,
        };
        let missing_path = tmp.path().join("missing");
        let missing_store = DispatchStore::new(&missing_path);
        assert!(
            missing_store
                .read_inbox_records(&caller, &[matching])
                .is_err()
        );
        let beyond = InboxIndexEntry {
            offset: fs::metadata(&path).unwrap().len(),
            ..matching
        };
        assert!(store.read_inbox_records(&caller, &[beyond]).is_err());
        let mismatched = InboxIndexEntry {
            sequence: 8,
            ..matching
        };
        assert!(store.read_inbox_records(&caller, &[mismatched]).is_err());

        let index_path = store.inbox_index_path(&caller);
        fs::write(&index_path, b"stale-index").unwrap();
        fs::remove_file(&index_path).unwrap();
        fs::create_dir(&index_path).unwrap();
        assert!(store.write_inbox_records(&caller, &[record]).is_err());
    }

    #[test]
    fn corrupt_ack_state_fails_closed_without_hiding_unread_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let worker = WorkerRef {
            session_id: Some(session),
            agent_id,
        };
        store
            .append_inbox(&caller, message(OperationId::new(), worker.clone()))
            .unwrap();
        let path = store.inbox_ack_path(&caller);
        fs::write(&path, r#"{"next_sequence":99}"#).unwrap();

        assert!(store.inbox_page(&caller, None, 1, true, None).is_err());
        assert!(store.inbox(&caller).is_err());
        assert!(
            store
                .append_inbox(&caller, message(OperationId::new(), worker))
                .is_err()
        );
        assert_eq!(store.inbox_index(&caller).unwrap().entries.len(), 1);
    }

    #[test]
    fn workspace_root_caller_and_worker_use_a_reserved_inbox_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let agent_id = AgentId::new();
        let root_caller = CallerRef {
            session_id: None,
            agent_id,
        };
        let run_id = OperationId::new();
        let worker = WorkerRef {
            session_id: None,
            agent_id,
        };
        store
            .append_inbox(&root_caller, message(run_id, worker))
            .unwrap();
        let path = store.inbox_path(&root_caller);
        assert!(path.parent().unwrap().ends_with(super::ROOT_INBOX_SEGMENT));
        assert_eq!(store.inbox(&root_caller).unwrap().len(), 1);

        // A root agent is a distinct incarnation from any session agent with the
        // same runtime/model, and is reused on the next resolve.
        let runtime = AgentProfileId::new("codex").unwrap();
        let model = ModelSelector::new("gpt-5").unwrap();
        let workspace = WorkspaceId::new();
        let other_workspace = WorkspaceId::new();
        let root_agent = store
            .upsert_agent_by_runtime_model(workspace, None, runtime.clone(), model.clone())
            .unwrap();
        assert_eq!(root_agent.session_id, None);
        assert_eq!(
            store
                .upsert_agent_by_runtime_model(workspace, None, runtime.clone(), model.clone())
                .unwrap()
                .agent_id,
            root_agent.agent_id
        );
        let other_root = store
            .upsert_agent_by_runtime_model(other_workspace, None, runtime, model)
            .unwrap();
        assert_ne!(other_root.agent_id, root_agent.agent_id);
        assert_eq!(
            store
                .agents_in_workspace(workspace)
                .unwrap()
                .into_iter()
                .map(|agent| agent.agent_id)
                .collect::<Vec<_>>(),
            vec![root_agent.agent_id]
        );
        assert!(
            store
                .agent_in_workspace(workspace, other_root.agent_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn legacy_root_agent_is_not_claimed_but_session_ownership_can_be_proved() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let runtime = AgentProfileId::new("codex").unwrap();
        let model = ModelSelector::new("gpt-5").unwrap();
        let legacy_root = Agent {
            agent_id: AgentId::new(),
            session_id: None,
            runtime: runtime.clone(),
            model: model.clone(),
            status: AgentStatus::Exited,
            current_run: None,
        };
        let legacy_session = Agent {
            agent_id: AgentId::new(),
            session_id: Some(session),
            ..legacy_root.clone()
        };
        let legacy = Registry {
            agents: vec![legacy_root.clone(), legacy_session.clone()],
            ..Registry::default()
        };
        json_file::write_atomic(tmp.path(), &store.registry_path(), &legacy).unwrap();

        let root = store
            .upsert_agent_by_runtime_model(workspace, None, runtime.clone(), model.clone())
            .unwrap();
        assert_ne!(root.agent_id, legacy_root.agent_id);
        assert!(
            store
                .agent_in_workspace(workspace, legacy_root.agent_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .upsert_agent_by_runtime_model(workspace, Some(session), runtime, model)
                .unwrap()
                .agent_id,
            legacy_session.agent_id
        );
    }

    #[test]
    fn legacy_whole_snapshot_writes_cannot_erase_workspace_sidecar_state() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let workspace = WorkspaceId::new();
        let runtime = AgentProfileId::new("codex").unwrap();
        let model = ModelSelector::new("gpt-5").unwrap();
        let agent = store
            .upsert_agent_by_runtime_model(workspace, None, runtime, model)
            .unwrap();
        store
            .queue_prompt(workspace, None, "after rollover".into(), now())
            .unwrap();

        let legacy_document = fs::read_to_string(store.registry_path()).unwrap();
        assert!(!legacy_document.contains("agent_workspaces"));
        assert!(!legacy_document.contains("workspace_id"));
        // `transition_agent` rewrites the complete legacy-shaped document, as
        // a draining predecessor does when one of its Agents exits.
        store
            .transition_agent(agent.agent_id, AgentStatus::Exited, None)
            .unwrap();

        assert_eq!(
            store
                .agent_in_workspace(workspace, agent.agent_id)
                .unwrap()
                .unwrap()
                .status,
            AgentStatus::Exited
        );
        assert_eq!(
            store
                .queued_prompt(workspace, None)
                .unwrap()
                .unwrap()
                .prompt,
            "after rollover"
        );
    }

    #[test]
    fn malformed_registry_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        fs::write(store.registry_path(), "broken").unwrap();
        assert!(store.agents().is_err());
        fs::remove_file(store.registry_path()).unwrap();
        fs::write(store.workspace_registry_path(), "broken").unwrap();
        assert!(store.agents_in_workspace(WorkspaceId::new()).is_err());
    }
    /// Every registry mutation replaces the whole document, so a history that is
    /// never dropped makes each dispatch cost more than the last — O(N²) over
    /// the life of a daemon meant to run for weeks.
    #[test]
    fn finished_runs_are_bounded_while_live_ones_are_never_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, _) = ids();
        store
            .upsert_agent(WorkspaceId::new(), agent(session, agent_id))
            .unwrap();

        // Two operations still in flight, recorded before the flood.
        let mut live = Vec::new();
        for status in [RunStatus::Preparing, RunStatus::Running] {
            let run = DispatchRun {
                run_id: OperationId::new(),
                agent_id,
                prompt: "live".into(),
                started_at: now(),
                ended_at: None,
                status,
            };
            live.push(run.run_id);
            store.upsert_run(run).unwrap();
        }

        for _ in 0..(RUN_RETENTION + 40) {
            store
                .upsert_run(DispatchRun {
                    run_id: OperationId::new(),
                    agent_id,
                    prompt: "done".into(),
                    started_at: now(),
                    ended_at: Some(now()),
                    status: RunStatus::Completed,
                })
                .unwrap();
        }

        let registry = store.load_registry().unwrap();
        let finished = registry
            .runs
            .iter()
            .filter(|run| run.status == RunStatus::Completed)
            .count();
        assert!(
            finished <= RUN_RETENTION,
            "run history grew past its bound: {finished}"
        );
        for run_id in live {
            assert!(
                store.run(run_id).unwrap().is_some(),
                "an in-flight run was dropped as history"
            );
        }
        // The agent itself is not history: it is the record a relaunch reuses.
        assert!(store.agent(agent_id).unwrap().is_some());
    }

    /// A binding is how a report finds its caller and an admission is the proof
    /// that a spawn was prepared. Neither may outlive the run it belongs to, and
    /// neither may be dropped while that run is still live.
    #[test]
    fn bindings_and_admissions_follow_the_runs_they_belong_to() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let worker = WorkerRef {
            session_id: Some(session),
            agent_id,
        };
        let owned_worker = agent(session, agent_id);
        store
            .upsert_agent(WorkspaceId::new(), owned_worker.clone())
            .unwrap();
        let reserved = OperationId::new();
        store
            .reserve_admission(
                owned_worker,
                DispatchRun {
                    run_id: reserved,
                    agent_id,
                    prompt: "reserved".into(),
                    started_at: now(),
                    ended_at: None,
                    status: RunStatus::Preparing,
                },
                DispatchBinding {
                    run_id: reserved,
                    caller: caller.clone(),
                    worker,
                },
                AgentAdmissionReservation {
                    operation_id: reserved,
                    semantic_key: "key".into(),
                    credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
                },
            )
            .unwrap();

        for _ in 0..(RUN_RETENTION + 40) {
            store
                .upsert_run(DispatchRun {
                    run_id: OperationId::new(),
                    agent_id,
                    prompt: "done".into(),
                    started_at: now(),
                    ended_at: Some(now()),
                    status: RunStatus::Completed,
                })
                .unwrap();
        }

        let registry = store.load_registry().unwrap();
        assert!(
            registry.bindings.iter().any(|b| b.run_id == reserved),
            "the binding of a live run was dropped"
        );
        assert!(
            registry
                .admissions
                .iter()
                .any(|a| a.operation_id == reserved),
            "the admission of a live run was dropped"
        );
        // Nothing is left pointing at a run this retention removed.
        let kept: Vec<_> = registry.runs.iter().map(|run| run.run_id).collect();
        assert!(registry.bindings.iter().all(|b| kept.contains(&b.run_id)));
        assert!(
            registry
                .admissions
                .iter()
                .all(|a| kept.contains(&a.operation_id))
        );
    }

    /// An unread report is not history, so acknowledged/read records are the
    /// only records eligible for bounded compaction.
    #[test]
    fn a_read_inbox_is_bounded_and_unread_reports_outrank_every_read_one() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let worker = WorkerRef {
            session_id: Some(session),
            agent_id,
        };

        let awaited = OperationId::new();
        store
            .append_inbox(&caller, message(awaited, worker.clone()))
            .unwrap();

        for _ in 0..(INBOX_READ_RETENTION + 40) {
            let run_id = OperationId::new();
            store
                .append_inbox(&caller, message(run_id, worker.clone()))
                .unwrap();
            store.mark_inbox_read(&caller, run_id).unwrap();
        }

        let inbox = store.inbox(&caller).unwrap();
        let read = inbox.iter().filter(|item| item.read).count();
        assert!(
            read <= INBOX_READ_RETENTION,
            "read history grew past its bound: {read}"
        );
        assert!(
            inbox
                .iter()
                .any(|item| item.run_id == awaited && !item.read),
            "an unread report was dropped to make room for read ones"
        );
        assert!(inbox.len() <= INBOX_HARD_LIMIT);

        // Read retention can leave a stable-sequence gap after an unread record.
        // Losing the disposable index must not make that authoritative journal
        // impossible to rebuild on restart.
        fs::remove_file(store.inbox_index_path(&caller)).unwrap();
        let reopened = DispatchStore::new(tmp.path());
        assert!(
            reopened
                .inbox(&caller)
                .unwrap()
                .iter()
                .any(|item| item.run_id == awaited && !item.read)
        );
    }

    #[test]
    fn the_inbox_ceiling_backpressures_unread_and_recycles_acknowledged_history() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let worker = WorkerRef {
            session_id: Some(session),
            agent_id,
        };
        let records = (1..=u64::try_from(INBOX_HARD_LIMIT).unwrap())
            .map(|sequence| InboxRecord {
                sequence,
                message: message(OperationId::new(), worker.clone()),
            })
            .collect::<Vec<_>>();
        store.write_inbox_records(&caller, &records).unwrap();
        let before = fs::read(store.inbox_path(&caller)).unwrap();

        assert!(
            store
                .append_inbox(&caller, message(OperationId::new(), worker.clone()))
                .unwrap_err()
                .to_string()
                .contains("unacknowledged")
        );
        assert_eq!(fs::read(store.inbox_path(&caller)).unwrap(), before);
        assert_eq!(
            store.inbox_index(&caller).unwrap().entries.len(),
            INBOX_HARD_LIMIT
        );

        store
            .ack_inbox(&caller, InboxCursor { next_sequence: 2 })
            .unwrap();
        store
            .append_inbox(&caller, message(OperationId::new(), worker))
            .unwrap();
        let index = store.inbox_index(&caller).unwrap();
        assert_eq!(index.entries.len(), INBOX_HARD_LIMIT);
        assert_eq!(index.entries.first().unwrap().sequence, 2);
        assert!(
            store
                .inbox_page(
                    &caller,
                    Some(InboxCursor { next_sequence: 1 }),
                    1,
                    false,
                    None,
                )
                .unwrap_err()
                .to_string()
                .contains("cursor expired")
        );

        // Prefix compaction also advances the first retained sequence. The
        // index is derived state, so deleting it must still allow exact rebuild.
        fs::remove_file(store.inbox_index_path(&caller)).unwrap();
        let reopened = DispatchStore::new(tmp.path());
        let rebuilt = reopened.inbox_index(&caller).unwrap();
        assert_eq!(rebuilt.entries.len(), INBOX_HARD_LIMIT);
        assert_eq!(rebuilt.entries.first().unwrap().sequence, 2);
        assert_eq!(rebuilt.entries.last().unwrap().sequence, 4097);
    }
}
