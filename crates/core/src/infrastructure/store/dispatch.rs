//! Durable registry and inboxes for daemon-owned agent dispatch.
//!
//! The registry is one atomically replaced JSON document. Each caller inbox is
//! a locked, atomically replaced JSONL file so a crash cannot expose a partial
//! delivery and concurrent daemon commands cannot lose one another's updates.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::agent::{
    Agent, AgentProfileId, AgentStatus, CallerRef, DispatchBinding, DispatchRun, InboxMessage,
    ModelSelector, RunStatus,
};
use crate::domain::id::{AgentId, OperationId, SessionId};
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};

const REGISTRY_FILE: &str = "dispatch.json";
const INBOX_DIR: &str = "inbox";

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

/// The hard ceiling on one caller's inbox, unread messages included.
///
/// Read messages are dropped first and this bound is never reached in ordinary
/// use. It exists because "never drop an unread message" alone is not a bound: a
/// caller that never reads its inbox would otherwise grow it without limit.
const INBOX_HARD_LIMIT: usize = 4096;
/// Reserved inbox segment for a workspace-root caller. A `SessionId` is always a
/// lowercase UUID, so this non-UUID literal can never collide with one.
const ROOT_INBOX_SEGMENT: &str = "workspace-root";

/// Maps an optional owning session to its durable inbox directory segment.
/// `None` is the workspace root; `Some` is the session's UUID.
fn session_segment(session_id: Option<SessionId>) -> String {
    session_id.map_or_else(|| ROOT_INBOX_SEGMENT.to_owned(), |id| id.as_str())
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
}

/// File-backed durable dispatch state rooted at the daemon state directory.
pub struct DispatchStore {
    dir: PathBuf,
}

impl DispatchStore {
    #[must_use]
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().into(),
        }
    }

    #[must_use]
    pub fn registry_path(&self) -> PathBuf {
        self.dir.join(REGISTRY_FILE)
    }

    /// Replaces the next-launch prompt for a session. A single slot prevents a
    /// caller retry from creating an unbounded duplicate queue.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn queue_prompt(
        &self,
        session_id: Option<SessionId>,
        prompt: String,
        queued_at: DateTime<Utc>,
    ) -> Result<QueuedPrompt> {
        self.mutate_registry(|registry| {
            let queued = QueuedPrompt {
                session_id,
                prompt,
                queued_at,
            };
            if let Some(existing) = registry
                .prompts
                .iter_mut()
                .find(|item| item.session_id == session_id)
            {
                *existing = queued.clone();
            } else {
                registry.prompts.push(queued.clone());
            }
            queued
        })
    }

    /// Reads, without consuming, the prompt waiting for a session launch.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be read.
    pub fn queued_prompt(&self, session_id: Option<SessionId>) -> Result<Option<QueuedPrompt>> {
        Ok(self
            .load_registry()?
            .prompts
            .into_iter()
            .find(|item| item.session_id == session_id))
    }

    /// Removes a prompt only after its matching Agent launch succeeded.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn consume_prompt(&self, session_id: Option<SessionId>) -> Result<Option<QueuedPrompt>> {
        self.mutate_registry(|registry| {
            registry
                .prompts
                .iter()
                .position(|item| item.session_id == session_id)
                .map(|index| registry.prompts.remove(index))
        })
    }

    #[must_use]
    pub fn inbox_path(&self, caller: &CallerRef) -> PathBuf {
        self.dir
            .join(INBOX_DIR)
            .join(session_segment(caller.session_id))
            .join(format!("{}.jsonl", caller.agent_id.as_str()))
    }

    /// Upserts an agent by its never-reused incarnation ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn upsert_agent(&self, agent: Agent) -> Result<Agent> {
        self.mutate_registry(|registry| {
            if let Some(existing) = registry
                .agents
                .iter_mut()
                .find(|item| item.agent_id == agent.agent_id)
            {
                *existing = agent.clone();
            } else {
                registry.agents.push(agent.clone());
            }
            agent
        })
    }

    /// Reuses the agent for this session/runtime/model tuple or creates an idle one.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn upsert_agent_by_runtime_model(
        &self,
        session_id: Option<SessionId>,
        runtime: AgentProfileId,
        model: ModelSelector,
    ) -> Result<Agent> {
        self.mutate_registry(|registry| {
            if let Some(agent) = registry.agents.iter().find(|agent| {
                agent.session_id == session_id && agent.runtime == runtime && agent.model == model
            }) {
                return agent.clone();
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
            agent
        })
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
        let reconciled = registry.reconcile_incomplete_admissions();
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

    /// # Errors
    ///
    /// Returns an error when the registry cannot be locked, read, or written.
    pub fn upsert_binding(&self, binding: DispatchBinding) -> Result<DispatchBinding> {
        self.mutate_registry(|registry| {
            if let Some(existing) = registry
                .bindings
                .iter_mut()
                .find(|item| item.run_id == binding.run_id)
            {
                *existing = binding.clone();
            } else {
                registry.bindings.push(binding.clone());
            }
            binding
        })
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

    /// Appends a report to the caller's durable inbox.
    ///
    /// # Errors
    ///
    /// Returns an error when the inbox cannot be locked, read, or written.
    pub fn append_inbox(&self, caller: &CallerRef, message: InboxMessage) -> Result<()> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let path = self.inbox_path(caller);
        let mut messages = Self::read_inbox(&path)?;
        messages.push(message);
        // Bounding here is what turns an append from O(history) into O(bound):
        // the whole file is rewritten each time, so without it N deliveries cost
        // O(N²) and a long-lived caller's inbox never stops getting slower.
        retain_bounded_inbox(&mut messages);
        Self::write_inbox(&path, &messages)
    }

    /// # Errors
    ///
    /// Returns an error when the inbox cannot be read.
    pub fn inbox(&self, caller: &CallerRef) -> Result<Vec<InboxMessage>> {
        Self::read_inbox(&self.inbox_path(caller))
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
        let path = self.inbox_path(caller);
        let mut messages = Self::read_inbox(&path)?;
        let mut changed = false;
        for message in &mut messages {
            if message.run_id == run_id && !message.read {
                message.read = true;
                changed = true;
            }
        }
        if changed {
            // Marking read is what makes a message evictable, so the bound is
            // applied on this write too and not only when one is appended.
            retain_bounded_inbox(&mut messages);
            Self::write_inbox(&path, &messages)?;
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

    fn read_inbox(path: &Path) -> Result<Vec<InboxMessage>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context(format!("failed to read {}", path.display())),
        };
        text.lines()
            .map(|line| {
                serde_json::from_str(line).context("failed to parse dispatch inbox message")
            })
            .collect()
    }

    fn write_inbox(path: &Path, messages: &[InboxMessage]) -> Result<()> {
        let parent = path.parent().expect("inbox path has a parent");
        fs::create_dir_all(parent).context(format!("failed to create {}", parent.display()))?;
        let mut text = messages
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        json_file::write_text_atomic(path, &text)
    }
}

/// Bound one caller's inbox with the shipped limits.
fn retain_bounded_inbox(messages: &mut Vec<InboxMessage>) {
    retain_inbox_within(messages, INBOX_READ_RETENTION, INBOX_HARD_LIMIT);
}

/// Bound one caller's inbox, dropping read messages before unread ones.
///
/// An unread message is a report its caller has not seen, so it outranks every
/// read one however old. "Never drop unread" is not by itself a bound, though: a
/// caller that never reads would grow its inbox forever. Past `hard_limit` the
/// oldest unread messages go too, and the drop is recorded so the loss is
/// inspectable rather than silent.
///
/// The limits are parameters so the shipped ones can stay at values a real
/// caller never reaches while the policy itself is still exercised directly.
/// Driving `hard_limit` through the store would mean appending thousands of
/// messages, each of which rewrites the whole file.
fn retain_inbox_within(messages: &mut Vec<InboxMessage>, read_retention: usize, hard_limit: usize) {
    let read_count = messages.iter().filter(|message| message.read).count();
    let mut over_read = read_count.saturating_sub(read_retention);
    if over_read > 0 {
        messages.retain(|message| {
            if over_read > 0 && message.read {
                over_read -= 1;
                return false;
            }
            true
        });
    }
    let over_hard = messages.len().saturating_sub(hard_limit);
    if over_hard == 0 {
        return;
    }
    crate::infrastructure::error_log::ErrorLog::record(&format!(
        "dispatch inbox reached its {hard_limit} message ceiling; \
         dropping the {over_hard} oldest unread message(s)"
    ));
    messages.drain(..over_hard);
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
        let (session, agent_id, caller) = ids();
        let first = agent(session, agent_id);
        assert_eq!(store.upsert_agent(first.clone()).unwrap(), first);
        let replacement = Agent {
            status: AgentStatus::Exited,
            ..first.clone()
        };
        assert_eq!(
            store.upsert_agent(replacement.clone()).unwrap(),
            replacement
        );
        let reused = store
            .upsert_agent_by_runtime_model(
                Some(session),
                first.runtime.clone(),
                first.model.clone(),
            )
            .unwrap();
        assert_eq!(reused.agent_id, agent_id);
        let created = store
            .upsert_agent_by_runtime_model(
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
    fn prompt_queue_replaces_peeks_and_consumes_per_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let session = SessionId::new();
        store
            .queue_prompt(Some(session), "first".into(), now())
            .unwrap();
        store
            .queue_prompt(Some(session), "second".into(), now())
            .unwrap();
        store.queue_prompt(None, "root".into(), now()).unwrap();
        assert_eq!(
            store.queued_prompt(Some(session)).unwrap().unwrap().prompt,
            "second"
        );
        assert_eq!(
            store.consume_prompt(Some(session)).unwrap().unwrap().prompt,
            "second"
        );
        assert!(store.queued_prompt(Some(session)).unwrap().is_none());
        assert_eq!(store.consume_prompt(None).unwrap().unwrap().prompt, "root");
        assert!(store.consume_prompt(None).unwrap().is_none());
    }

    #[test]
    fn admission_reservation_is_atomic_secret_free_and_reconciles_incomplete_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let operation = OperationId::new();
        let mut worker = agent(session, agent_id);
        worker.status = AgentStatus::Starting;
        worker.current_run = Some(operation);
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
    fn missing_and_invalid_inboxes_are_handled() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (_, _, caller) = ids();
        assert!(store.inbox(&caller).unwrap().is_empty());
        fs::create_dir_all(store.inbox_path(&caller).parent().unwrap()).unwrap();
        fs::write(store.inbox_path(&caller), "broken\n").unwrap();
        assert!(store.inbox(&caller).is_err());
        fs::remove_file(store.inbox_path(&caller)).unwrap();
        fs::create_dir(store.inbox_path(&caller)).unwrap();
        assert!(store.inbox(&caller).is_err());
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
        let root_agent = store
            .upsert_agent_by_runtime_model(None, runtime.clone(), model.clone())
            .unwrap();
        assert_eq!(root_agent.session_id, None);
        assert_eq!(
            store
                .upsert_agent_by_runtime_model(None, runtime, model)
                .unwrap()
                .agent_id,
            root_agent.agent_id
        );
    }

    #[test]
    fn malformed_registry_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        fs::write(store.registry_path(), "broken").unwrap();
        assert!(store.agents().is_err());
    }
    /// Every registry mutation replaces the whole document, so a history that is
    /// never dropped makes each dispatch cost more than the last — O(N²) over
    /// the life of a daemon meant to run for weeks.
    #[test]
    fn finished_runs_are_bounded_while_live_ones_are_never_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, _) = ids();
        store.upsert_agent(agent(session, agent_id)).unwrap();

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
        let reserved = OperationId::new();
        store
            .reserve_admission(
                agent(session, agent_id),
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

    /// An append rewrites the whole inbox file, so an unbounded inbox costs
    /// O(N²) over N deliveries. An unread report is not history, though: it is a
    /// result its caller has not seen.
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
    }

    /// "Never drop unread" is not a bound on its own: a caller that never reads
    /// would grow its inbox forever. The ceiling is the backstop, and what it
    /// drops is recorded rather than lost silently.
    #[test]
    fn the_inbox_ceiling_drops_the_oldest_unread_only_after_every_read_one() {
        let (session, agent_id, _) = ids();
        let worker = WorkerRef {
            session_id: Some(session),
            agent_id,
        };
        let mut messages: Vec<InboxMessage> = (0..10)
            .map(|index| {
                let mut item = message(OperationId::new(), worker.clone());
                item.read = index % 2 == 0;
                item.summary = format!("m{index}");
                item
            })
            .collect();
        let oldest_unread = messages
            .iter()
            .find(|item| !item.read)
            .map(|item| item.summary.clone())
            .unwrap();

        // Exactly at the ceiling nothing is dropped: the bound is "no more
        // than", not "fewer than".
        let mut at_ceiling = messages.clone();
        let ceiling = at_ceiling.len();
        retain_inbox_within(&mut at_ceiling, 100, ceiling);
        assert_eq!(at_ceiling.len(), 10);

        // Read messages go first, and only what is still over the ceiling comes
        // out of the unread ones.
        retain_inbox_within(&mut messages, 2, 4);
        assert_eq!(messages.len(), 4);
        assert!(
            messages.iter().filter(|item| item.read).count() <= 2,
            "read messages were kept past their retention"
        );
        assert!(
            !messages.iter().any(|item| item.summary == oldest_unread),
            "the ceiling dropped a newer unread message before the oldest"
        );
        // What survives is the newest of what was there.
        assert_eq!(messages.last().unwrap().summary, "m9");
    }
}
