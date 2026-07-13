//! Durable persistence boundary for daemon-owned lifecycle state.
//!
//! Clients never receive this store: they submit commands to the daemon, which
//! serializes reducer application under this store's lock.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};

use crate::domain::id::DaemonGeneration;
use crate::domain::session_lifecycle::{LifecycleEvent, WorkspaceLifecycleState, reduce};
use crate::infrastructure::paths::STATE_DIR;
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};

const FILE: &str = "lifecycle-state.json";

/// A state store intended solely for the daemon command handler.
pub struct DaemonLifecycleStore {
    dir: PathBuf,
}

impl DaemonLifecycleStore {
    #[must_use]
    #[coverage(off)]
    pub fn new(repo_root: &Path) -> Self {
        Self {
            dir: repo_root.join(STATE_DIR),
        }
    }
    #[must_use]
    #[coverage(off)]
    pub fn state_path(&self) -> PathBuf {
        self.dir.join(FILE)
    }
    /// # Errors
    /// Returns an error when the durable state cannot be read.
    #[coverage(off)]
    pub fn load(&self) -> Result<Option<WorkspaceLifecycleState>> {
        json_file::read(&self.state_path())
    }
    /// Atomically applies an event after checking that the daemon owns any newly accepted operation.
    ///
    /// # Errors
    /// Returns an error for an ownership mismatch, reducer rejection, lock failure, or failed write.
    #[coverage(off)]
    pub fn apply(
        &self,
        daemon: DaemonGeneration,
        event: LifecycleEvent,
        now: DateTime<Utc>,
    ) -> Result<WorkspaceLifecycleState> {
        if let LifecycleEvent::ReserveCreate { operation, .. }
        | LifecycleEvent::BeginRemove { operation, .. } = &event
            && operation.owner_daemon_generation != daemon
        {
            bail!("operation is owned by another daemon generation");
        }
        let _lock = StoreLock::acquire(&self.dir)?;
        let mut state = self
            .load()?
            .ok_or_else(|| anyhow::anyhow!("lifecycle state has not been initialized"))?;
        reduce(&mut state, event, now).map_err(anyhow::Error::msg)?;
        json_file::write_atomic(&self.dir, &self.state_path(), &state)?;
        Ok(state)
    }
    /// # Errors
    /// Returns an error when the state cannot be durably initialized.
    #[coverage(off)]
    pub fn initialize(&self, state: &WorkspaceLifecycleState) -> Result<()> {
        state.validate().map_err(anyhow::Error::msg)?;
        json_file::write_atomic(&self.dir, &self.state_path(), state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::{OperationId, WorkspaceId};
    use crate::domain::session_lifecycle::{OperationJournal, OperationStatus};
    use chrono::TimeZone;
    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 12, 0, 0, 0).unwrap()
    }
    #[test]
    fn daemon_owner_and_revisions_are_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DaemonLifecycleStore::new(tmp.path());
        let daemon = DaemonGeneration::new();
        store
            .initialize(&WorkspaceLifecycleState::new(WorkspaceId::new(), now()))
            .unwrap();
        let operation = OperationJournal {
            operation_id: OperationId::new(),
            owner_daemon_generation: daemon,
            status: OperationStatus::Accepted,
            execution_attempt: 1,
            progress_revision: 0,
            semantic_key: "create:one".into(),
        };
        let saved = store
            .apply(
                daemon,
                LifecycleEvent::ReserveCreate {
                    name: "one".into(),
                    operation,
                },
                now(),
            )
            .unwrap();
        assert_eq!(saved.state_revision, 1);
        assert_eq!(store.load().unwrap().unwrap(), saved);
    }
    #[test]
    fn another_daemon_cannot_accept_an_operation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DaemonLifecycleStore::new(tmp.path());
        let daemon = DaemonGeneration::new();
        store
            .initialize(&WorkspaceLifecycleState::new(WorkspaceId::new(), now()))
            .unwrap();
        let operation = OperationJournal {
            operation_id: OperationId::new(),
            owner_daemon_generation: DaemonGeneration::new(),
            status: OperationStatus::Accepted,
            execution_attempt: 1,
            progress_revision: 0,
            semantic_key: "create:one".into(),
        };
        assert!(
            store
                .apply(
                    daemon,
                    LifecycleEvent::ReserveCreate {
                        name: "one".into(),
                        operation
                    },
                    now()
                )
                .is_err()
        );
    }

    #[test]
    fn apply_rejects_an_uninitialized_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DaemonLifecycleStore::new(tmp.path());
        let daemon = DaemonGeneration::new();
        let operation = OperationJournal {
            operation_id: OperationId::new(),
            owner_daemon_generation: daemon,
            status: OperationStatus::Accepted,
            execution_attempt: 1,
            progress_revision: 0,
            semantic_key: "create:one".into(),
        };
        assert!(
            store
                .apply(
                    daemon,
                    LifecycleEvent::ReserveCreate {
                        name: "one".into(),
                        operation,
                    },
                    now(),
                )
                .is_err()
        );
    }
}
