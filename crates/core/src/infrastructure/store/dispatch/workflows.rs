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
    pub version: u32,
    pub operation: OperationId,
    pub goal: String,
    pub run: Option<WorkflowRun>,
    pub initial_notified: bool,
    pub cursor: Option<OperationId>,
}

impl DispatchStore {
    fn workflow_path(&self, workspace: WorkspaceId, session: SessionId) -> std::path::PathBuf {
        self.dir
            .join("workflows")
            .join(workspace.as_str())
            .join(format!("{}.json", session.as_str()))
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
        let mut value = record.context("workflow update must retain a record")?;
        while serde_json::to_vec_pretty(&value)?.len() >= MAX_BYTES {
            let history = &mut value.run.as_mut().context("workflow capacity exhausted")?.history;
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
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workflow_capacity_keeps_instructions_and_bounds_the_wire_snapshot() {
        use crate::domain::{workflow::{Phase,WorkflowRun,WorkflowHistoryEntry},id::AgentId};
        let directory=tempfile::tempdir().unwrap();let store=DispatchStore::new(directory.path());let workspace=WorkspaceId::new();let session=SessionId::new();let operation=OperationId::new();
        assert!(store.update_workflow(workspace,session,|value| { *value=Some(WorkflowRecord {version:1,operation,goal:"x".repeat(MAX_BYTES),run:None,initial_notified:false,cursor:None});Ok(()) }).is_err());
        let run=WorkflowRun {id:operation,session,goal:"Task".into(),implementer:AgentId::new(),reviewer:None,phase:Phase::Implementing,revision_limit:3,revisions:0,review:None,waiting_reason:None,instructions:Vec::new(),history:vec![WorkflowHistoryEntry {id:OperationId::new(),actor:"Codex".into(),body:"x".repeat(MAX_BYTES)}]};
        store.update_workflow(workspace,session,|value| { *value=Some(WorkflowRecord {version:1,operation,goal:"Task".into(),run:Some(run),initial_notified:false,cursor:None});Ok(()) }).unwrap();
        assert!(store.workflow(workspace,session).unwrap().unwrap().run.unwrap().history.is_empty());
        assert!(store.update_workflow(workspace,session,|value| {value.as_mut().unwrap().goal="x".repeat(MAX_BYTES);Ok(())}).is_err());
        assert_eq!(store.workflow(workspace,session).unwrap().unwrap().goal,"Task");
        store.update_workflow(workspace,session,|value| {value.as_mut().unwrap().version=2;Ok(())}).unwrap();
        assert!(store.workflow(workspace,session).is_err());
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
                    version: 1,
                    operation,
                    goal: "test".into(),
                    run: None,
                    initial_notified: false,
                    cursor: None,
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
        assert!(
            store
                .update_workflow(workspace, session, |record| -> Result<()> {
                    record.as_mut().unwrap().goal.clear();
                    anyhow::bail!("conflict")
                })
                .is_err()
        );
        assert_eq!(
            store.workflow(workspace, session).unwrap().unwrap().goal,
            "test"
        );
        assert!(
            store
                .update_workflow(workspace, SessionId::new(), |_| Ok(()))
                .is_err()
        );
    }
}
