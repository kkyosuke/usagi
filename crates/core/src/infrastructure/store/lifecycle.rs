//! Durable persistence boundary for daemon-owned lifecycle state.
//!
//! Clients never receive this store: they submit commands to the daemon, which
//! serializes reducer application under this store's lock.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::id::DaemonGeneration;
use crate::domain::session_lifecycle::{LifecycleEvent, WorkspaceLifecycleState, reduce};
use crate::infrastructure::persistence::{json_file, store_lock::StoreLock};

const STATE_FILE: &str = "sessions.json";

/// The complete durable state owned by one shared daemon.
///
/// Keeping the repository binding and lifecycle state in one envelope makes
/// their relationship atomic: a crash cannot publish one without the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedLifecycle {
    repository_root: PathBuf,
    state: WorkspaceLifecycleState,
}

/// A state store intended solely for the daemon command handler.
pub struct DaemonLifecycleStore {
    dir: PathBuf,
}

impl DaemonLifecycleStore {
    #[must_use]
    #[coverage(off)]
    pub fn new(dir: &Path) -> Self {
        Self { dir: dir.into() }
    }
    #[must_use]
    #[coverage(off)]
    pub fn state_path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }
    /// # Errors
    /// Returns an error when the durable state cannot be read.
    #[coverage(off)]
    pub fn load(&self) -> Result<Option<WorkspaceLifecycleState>> {
        Ok(self.load_persisted()?.map(|persisted| persisted.state))
    }
    /// # Errors
    /// Returns an error when the shared lifecycle record cannot be read.
    #[coverage(off)]
    pub fn load_with_workspace(&self) -> Result<Option<(PathBuf, WorkspaceLifecycleState)>> {
        Ok(self
            .load_persisted()?
            .map(|persisted| (persisted.repository_root, persisted.state)))
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
        let mut persisted = self
            .load_persisted()?
            .ok_or_else(|| anyhow::anyhow!("lifecycle state has not been initialized"))?;
        reduce(&mut persisted.state, event, now).map_err(anyhow::Error::msg)?;
        json_file::write_atomic(&self.dir, &self.state_path(), &persisted)?;
        Ok(persisted.state)
    }
    /// # Errors
    /// Returns an error when the state cannot be durably initialized.
    #[coverage(off)]
    pub fn initialize(
        &self,
        state: &WorkspaceLifecycleState,
        repository_root: &Path,
    ) -> Result<()> {
        state.validate().map_err(anyhow::Error::msg)?;
        let _lock = StoreLock::acquire(&self.dir)?;
        json_file::write_atomic(
            &self.dir,
            &self.state_path(),
            &PersistedLifecycle {
                repository_root: repository_root.into(),
                state: state.clone(),
            },
        )
    }

    #[coverage(off)]
    fn load_persisted(&self) -> Result<Option<PersistedLifecycle>> {
        json_file::read(&self.state_path())
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
            .initialize(
                &WorkspaceLifecycleState::new(WorkspaceId::new(), now()),
                tmp.path(),
            )
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
            .initialize(
                &WorkspaceLifecycleState::new(WorkspaceId::new(), now()),
                tmp.path(),
            )
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

    #[test]
    fn persists_workspace_and_session_state_in_one_shared_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DaemonLifecycleStore::new(tmp.path());
        let root = Path::new("/tmp/repository");
        store
            .initialize(
                &WorkspaceLifecycleState::new(WorkspaceId::new(), now()),
                root,
            )
            .unwrap();

        assert_eq!(
            store.load_with_workspace().unwrap().map(|(root, _)| root),
            Some(root.into())
        );
        assert_eq!(store.state_path(), tmp.path().join(STATE_FILE));
    }
}
