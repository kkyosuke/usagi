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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Registry {
    agents: Vec<Agent>,
    runs: Vec<DispatchRun>,
    bindings: Vec<DispatchBinding>,
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

    #[must_use]
    pub fn inbox_path(&self, caller: &CallerRef) -> PathBuf {
        self.dir
            .join(INBOX_DIR)
            .join(caller.session_id.as_str())
            .join(format!("{}.jsonl", caller.agent_id.as_str()))
    }

    /// Upserts an agent by its never-reused incarnation ID.
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
    pub fn upsert_agent_by_runtime_model(
        &self,
        session_id: SessionId,
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

    pub fn agent(&self, agent_id: AgentId) -> Result<Option<Agent>> {
        Ok(self
            .load_registry()?
            .agents
            .into_iter()
            .find(|agent| agent.agent_id == agent_id))
    }

    pub fn agents(&self) -> Result<Vec<Agent>> {
        Ok(self.load_registry()?.agents)
    }

    /// Adds or replaces a run by `run_id`.
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
    pub fn transition_run(
        &self,
        run_id: OperationId,
        status: RunStatus,
        ended_at: Option<DateTime<Utc>>,
    ) -> Result<Option<DispatchRun>> {
        self.mutate_registry(|registry| {
            let Some(run) = registry.runs.iter_mut().find(|run| run.run_id == run_id) else {
                return None;
            };
            run.status = status;
            run.ended_at = ended_at;
            Some(run.clone())
        })
    }

    /// Transitions an agent's durable availability and current run reference.
    pub fn transition_agent(
        &self,
        agent_id: AgentId,
        status: AgentStatus,
        current_run: Option<OperationId>,
    ) -> Result<Option<Agent>> {
        self.mutate_registry(|registry| {
            let Some(agent) = registry
                .agents
                .iter_mut()
                .find(|agent| agent.agent_id == agent_id)
            else {
                return None;
            };
            agent.status = status;
            agent.current_run = current_run;
            Some(agent.clone())
        })
    }

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

    pub fn binding(&self, run_id: OperationId) -> Result<Option<DispatchBinding>> {
        Ok(self
            .load_registry()?
            .bindings
            .into_iter()
            .find(|binding| binding.run_id == run_id))
    }

    /// Appends a report to the caller's durable inbox.
    pub fn append_inbox(&self, caller: &CallerRef, message: InboxMessage) -> Result<()> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let path = self.inbox_path(caller);
        let mut messages = Self::read_inbox(&path)?;
        messages.push(message);
        self.write_inbox(&path, &messages)
    }

    pub fn inbox(&self, caller: &CallerRef) -> Result<Vec<InboxMessage>> {
        Self::read_inbox(&self.inbox_path(caller))
    }

    pub fn unread_inbox(&self, caller: &CallerRef) -> Result<Vec<InboxMessage>> {
        Ok(self
            .inbox(caller)?
            .into_iter()
            .filter(|message| !message.read)
            .collect())
    }

    /// Marks all messages for `run_id` read and returns whether anything changed.
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
            self.write_inbox(&path, &messages)?;
        }
        Ok(changed)
    }

    fn mutate_registry<T>(&self, mutate: impl FnOnce(&mut Registry) -> T) -> Result<T> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut registry = self.load_registry()?;
        let result = mutate(&mut registry);
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

    fn write_inbox(&self, path: &Path, messages: &[InboxMessage]) -> Result<()> {
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
                session_id: session,
                agent_id: agent,
            },
        )
    }
    fn agent(session_id: SessionId, agent_id: AgentId) -> Agent {
        Agent {
            agent_id,
            session_id,
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
        let reused = store
            .upsert_agent_by_runtime_model(session, first.runtime.clone(), first.model.clone())
            .unwrap();
        assert_eq!(reused.agent_id, agent_id);
        let created = store
            .upsert_agent_by_runtime_model(
                session,
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
                session_id: session,
                agent_id,
            },
        };
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
    fn inbox_is_jsonl_durable_and_filters_then_marks_unread_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(tmp.path());
        let (session, agent_id, caller) = ids();
        let run_id = OperationId::new();
        let worker = WorkerRef {
            session_id: session,
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
            session_id: session,
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
                    .unwrap()
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
    }
}
