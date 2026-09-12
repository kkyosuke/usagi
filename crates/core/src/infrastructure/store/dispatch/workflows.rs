//! Daemon-owned workflow intent and progress journal, separate from Agent inboxes.
use super::DispatchStore;
use crate::domain::id::{OperationId, SessionId, WorkspaceId};
use crate::domain::workflow::WorkflowRun;
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

// Leave space for the response envelope inside the 1 MiB IPC frame budget.
const MAX_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRecord {
    #[serde(default)]
    pub agents: crate::domain::workflow::WorkflowAgents,
    pub version: u32,
    pub operation: OperationId,
    pub goal: String,
    pub run: Option<WorkflowRun>,
    pub initial_notified: bool,
    #[serde(default)]
    pub preferences_saved: bool,
    pub cursor: Option<OperationId>,
    #[serde(default)]
    pub start_error: Option<String>,
    #[serde(default)]
    pub suspended_phase: Option<crate::domain::workflow::Phase>,
    #[serde(default)]
    pub implementation_operation: Option<OperationId>,
    /// Daemon-proven exact resume chains rooted in the workflow launch or handoff.
    #[serde(default)]
    pub authorized_operations: Vec<(crate::domain::id::AgentId, OperationId)>,
}

impl DispatchStore {
    /// Read the last successfully launched choices in this workspace.
    /// # Errors
    /// Returns malformed or unreadable preferences instead of silently replacing them.
    pub fn workflow_agents(
        &self,
        workspace: WorkspaceId,
    ) -> Result<crate::domain::workflow::WorkflowAgents> {
        Ok(json_file::read_bounded(
            &self
                .dir
                .join("workflows")
                .join(workspace.as_str())
                .join("defaults.json"),
            MAX_BYTES,
        )?
        .unwrap_or_default())
    }

    /// Remember a successful start for subsequent sessions and daemon restarts.
    /// # Errors
    /// Returns persistence failures.
    pub fn remember_workflow_agents(
        &self,
        workspace: WorkspaceId,
        agents: crate::domain::workflow::WorkflowAgents,
    ) -> Result<()> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let directory = self.dir.join("workflows").join(workspace.as_str());
        json_file::write_atomic(&directory, &directory.join("defaults.json"), &agents)
    }

    fn workflow_path(&self, workspace: WorkspaceId, session: SessionId) -> std::path::PathBuf {
        self.dir
            .join("workflows")
            .join(workspace.as_str())
            .join(format!("{}.json", session.as_str()))
    }

    /// Save defaults once per successful launch, so retrying an old run cannot
    /// overwrite the choices of a newer workflow.
    /// # Errors
    /// Returns missing intent and persistence failures, retaining retryability.
    pub fn remember_workflow_start(
        &self,
        workspace: WorkspaceId,
        session: SessionId,
    ) -> Result<()> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut record = self
            .workflow(workspace, session)?
            .context("workflow intent is missing")?;
        if record.preferences_saved {
            return Ok(());
        }
        ensure!(record.run.is_some(), "workflow has not launched");
        let directory = self.dir.join("workflows").join(workspace.as_str());
        json_file::write_atomic(&directory, &directory.join("defaults.json"), &record.agents)?;
        record.preferences_saved = true;
        self.save_workflow(workspace, session, record)
    }

    /// Read the bounded record for one authority-checked session.
    /// # Errors
    /// Rejects unreadable, oversized or unsupported records.
    pub fn workflow(
        &self,
        workspace: WorkspaceId,
        session: SessionId,
    ) -> Result<Option<WorkflowRecord>> {
        let value: Option<WorkflowRecord> =
            json_file::read_bounded(&self.workflow_path(workspace, session), MAX_BYTES)?;
        ensure!(
            value.as_ref().is_none_or(|record| record.version == 1),
            "unsupported workflow version"
        );
        Ok(value)
    }

    /// Enumerate every session that holds a durable workflow record.
    ///
    /// The resident workflow lane sweeps this list every tick, so enumeration is
    /// best-effort by design: a missing root, an unreadable entry, a file where a
    /// workspace directory belongs, and names that are not typed identities
    /// (including `defaults.json`) all mean "nothing to advance here" rather than
    /// an error that would stop every other run. A transient read failure costs
    /// one tick, because the next sweep enumerates again.
    #[must_use]
    pub fn workflow_sessions(&self) -> Vec<(WorkspaceId, SessionId)> {
        let root = self.dir.join("workflows");
        let mut found = Vec::new();
        for workspace_entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
            let Ok(workspace) = WorkspaceId::parse(&workspace_entry.file_name().to_string_lossy())
            else {
                continue;
            };
            for session_entry in std::fs::read_dir(workspace_entry.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                let name = session_entry.file_name().to_string_lossy().into_owned();
                let Some(identity) = name.strip_suffix(".json") else {
                    continue;
                };
                if let Ok(session) = SessionId::parse(identity) {
                    found.push((workspace, session));
                }
            }
        }
        found.sort_by(|left, right| {
            (left.0.as_str(), left.1.as_str()).cmp(&(right.0.as_str(), right.1.as_str()))
        });
        found
    }

    /// Atomically validate and replace a bounded workflow record.
    /// # Errors
    /// Returns storage, capacity and caller validation errors without partial writes.
    pub fn update_workflow<T>(
        &self,
        workspace: WorkspaceId,
        session: SessionId,
        change: impl FnOnce(&mut Option<WorkflowRecord>) -> Result<T>,
    ) -> Result<T> {
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut record = self.workflow(workspace, session)?;
        let result = change(&mut record)?;
        self.save_workflow(
            workspace,
            session,
            record.context("workflow update must retain a record")?,
        )?;
        Ok(result)
    }

    fn save_workflow(
        &self,
        workspace: WorkspaceId,
        session: SessionId,
        mut value: WorkflowRecord,
    ) -> Result<()> {
        while serde_json::to_vec_pretty(&value)?.len() >= MAX_BYTES {
            let history = &mut value
                .run
                .as_mut()
                .context("workflow capacity exhausted")?
                .history;
            ensure!(!history.is_empty(), "workflow capacity exhausted");
            history.remove(0);
        }
        ensure!(
            serde_json::to_vec_pretty(&value)?.len() < MAX_BYTES,
            "workflow capacity exhausted"
        );
        let path = self.workflow_path(workspace, session);
        json_file::write_atomic(
            path.parent().context("missing workflow parent")?,
            &path,
            &value,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workflow_sessions_lists_records_and_skips_everything_that_is_not_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        assert!(store.workflow_sessions().is_empty());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        store
            .update_workflow(workspace, session, |value| {
                *value = Some(WorkflowRecord {
                    agents: crate::domain::workflow::WorkflowAgents::default(),
                    version: 1,
                    operation: OperationId::new(),
                    goal: "task".into(),
                    run: None,
                    initial_notified: false,
                    preferences_saved: false,
                    cursor: None,
                    start_error: None,
                    suspended_phase: None,
                    implementation_operation: None,
                    authorized_operations: Vec::new(),
                });
                Ok(())
            })
            .unwrap();
        store
            .remember_workflow_agents(
                workspace,
                crate::domain::workflow::WorkflowAgents::default(),
            )
            .unwrap();
        let workspace_dir = dir.path().join("workflows").join(workspace.as_str());
        std::fs::write(workspace_dir.join("notes.txt"), "unrelated").unwrap();
        std::fs::write(workspace_dir.join("not-an-identity.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.path().join("workflows").join("not-a-workspace")).unwrap();
        // A *file* named like a workspace is the case that used to abort the whole
        // sweep: reading it as a directory fails with something other than NotFound.
        std::fs::write(
            dir.path()
                .join("workflows")
                .join(WorkspaceId::new().as_str()),
            "not a directory",
        )
        .unwrap();
        // `defaults.json`, unrelated files, a non-identity directory and an
        // unreadable entry are all skipped: the sweep must not stop on someone
        // else's file.
        assert_eq!(store.workflow_sessions(), vec![(workspace, session)]);
    }

    #[test]
    fn workflow_choices_survive_restart_and_are_workspace_scoped() {
        use crate::domain::{settings::DefaultModel, workflow::WorkflowAgents};
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let agents = WorkflowAgents {
            planner: DefaultModel::Agy,
            implementer: DefaultModel::Claude,
            reviewer: DefaultModel::OpenAi,
        };
        assert_eq!(
            store.workflow_agents(workspace).unwrap(),
            WorkflowAgents::default()
        );
        store.remember_workflow_agents(workspace, agents).unwrap();
        let reopened = DispatchStore::new(dir.path());
        assert_eq!(reopened.workflow_agents(workspace).unwrap(), agents);
        assert_eq!(
            reopened.workflow_agents(WorkspaceId::new()).unwrap(),
            WorkflowAgents::default()
        );
        let path = dir
            .path()
            .join("workflows")
            .join(workspace.as_str())
            .join("defaults.json");
        std::fs::write(&path, "invalid").unwrap();
        assert!(reopened.workflow_agents(workspace).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            reopened
                .remember_workflow_agents(workspace, agents)
                .is_err()
        );
    }

    #[test]
    fn workflow_write_failure_is_not_reported_as_a_successful_update() {
        for fail in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let store = DispatchStore::new(directory.path());
            let workspace = WorkspaceId::new();
            let session = SessionId::new();
            let path = store.workflow_path(workspace, session);
            let parent = path.parent().unwrap();
            std::fs::create_dir_all(parent).unwrap();
            let result = store.update_workflow(workspace, session, |record| {
                *record = Some(WorkflowRecord {
                    agents: crate::domain::workflow::WorkflowAgents::default(),
                    version: 1,
                    operation: OperationId::new(),
                    goal: "task".into(),
                    run: None,
                    initial_notified: false,
                    preferences_saved: false,
                    cursor: None,
                    start_error: None,
                    suspended_phase: None,
                    implementation_operation: None,
                    authorized_operations: Vec::new(),
                });
                if fail {
                    std::fs::rename(parent, directory.path().join("saved-parent"))?;
                    std::fs::write(parent, "not a directory")?;
                }
                Ok(())
            });
            assert_eq!(result.is_err(), fail);
        }
    }
    #[test]
    #[allow(clippy::too_many_lines)] // One capacity fixture checks retention and all overflow paths.
    fn workflow_capacity_keeps_instructions_and_bounds_the_wire_snapshot() {
        use crate::domain::{
            id::AgentId,
            workflow::{Phase, WorkflowHistoryEntry, WorkflowRun},
        };
        let directory = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(directory.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let operation = OperationId::new();
        for oversized in [true, false] {
            let result = store.update_workflow(workspace, session, |value| {
                *value = Some(WorkflowRecord {
                    agents: crate::domain::workflow::WorkflowAgents::default(),
                    version: 1,
                    operation,
                    goal: if oversized {
                        "x".repeat(MAX_BYTES)
                    } else {
                        "Task".into()
                    },
                    run: None,
                    initial_notified: false,
                    preferences_saved: false,
                    cursor: None,
                    start_error: None,
                    suspended_phase: None,
                    implementation_operation: None,
                    authorized_operations: Vec::new(),
                });
                Ok(())
            });
            assert_eq!(result.is_err(), oversized);
        }
        let run = WorkflowRun {
            agents: crate::domain::workflow::WorkflowAgents::default(),
            id: operation,
            session,
            goal: "Task".into(),
            implementer: AgentId::new(),
            reviewer: None,
            phase: Phase::Implementing,
            revision_limit: 3,
            revisions: 0,
            review: None,
            waiting_reason: None,
            instructions: Vec::new(),
            history: vec![WorkflowHistoryEntry {
                id: OperationId::new(),
                actor: "Codex".into(),
                body: "x".repeat(MAX_BYTES),
            }],
        };
        store
            .update_workflow(workspace, session, |value| {
                *value = Some(WorkflowRecord {
                    agents: crate::domain::workflow::WorkflowAgents::default(),
                    version: 1,
                    operation,
                    goal: "Task".into(),
                    run: Some(run),
                    initial_notified: false,
                    preferences_saved: false,
                    cursor: None,
                    start_error: None,
                    suspended_phase: None,
                    implementation_operation: None,
                    authorized_operations: Vec::new(),
                });
                Ok(())
            })
            .unwrap();
        assert!(
            store
                .workflow(workspace, session)
                .unwrap()
                .unwrap()
                .run
                .unwrap()
                .history
                .is_empty()
        );
        for oversized in [true, false] {
            let result = store.update_workflow(workspace, session, |value| {
                value.as_mut().unwrap().goal = if oversized {
                    "x".repeat(MAX_BYTES)
                } else {
                    "Task".into()
                };
                Ok(())
            });
            assert_eq!(result.is_err(), oversized);
        }
        assert_eq!(
            store.workflow(workspace, session).unwrap().unwrap().goal,
            "Task"
        );
        store
            .update_workflow(workspace, session, |value| {
                value.as_mut().unwrap().version = 2;
                Ok(())
            })
            .unwrap();
        assert!(store.workflow(workspace, session).is_err());
    }
    #[test]
    fn workflow_is_durable_scoped_and_conflicts_preserve_original() {
        let dir = tempfile::tempdir().unwrap();
        let store = DispatchStore::new(dir.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        assert!(store.workflow(workspace, session).unwrap().is_none());
        let operation = OperationId::new();
        store
            .update_workflow(workspace, session, |record| {
                *record = Some(WorkflowRecord {
                    agents: crate::domain::workflow::WorkflowAgents::default(),
                    version: 1,
                    operation,
                    goal: "test".into(),
                    run: None,
                    initial_notified: false,
                    preferences_saved: false,
                    cursor: None,
                    start_error: None,
                    suspended_phase: None,
                    implementation_operation: None,
                    authorized_operations: Vec::new(),
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store
                .workflow(workspace, session)
                .unwrap()
                .unwrap()
                .operation,
            operation
        );
        assert!(
            store
                .workflow(WorkspaceId::new(), session)
                .unwrap()
                .is_none()
        );
        for conflict in [true, false] {
            let result = store.update_workflow(workspace, session, |record| -> Result<()> {
                if conflict {
                    record.as_mut().unwrap().goal.clear();
                    anyhow::bail!("conflict");
                }
                Ok(())
            });
            assert_eq!(result.is_err(), conflict);
        }
        assert_eq!(
            store.workflow(workspace, session).unwrap().unwrap().goal,
            "test"
        );
        for candidate in [SessionId::new(), session] {
            assert_eq!(
                store
                    .update_workflow(workspace, candidate, |_| Ok(()))
                    .is_err(),
                candidate != session
            );
        }
    }
}
