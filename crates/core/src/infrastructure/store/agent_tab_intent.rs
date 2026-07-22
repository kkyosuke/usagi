//! User-local, workspace-scoped persistence for TUI Agent-tab intent.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::domain::agent_tab_intent::{
    AGENT_TAB_INTENT_SCHEMA, AgentTabIntent, AgentTabIntentMutation, AgentTabProjection,
};
use crate::domain::id::WorkspaceId;
use crate::infrastructure::paths::data_dir;
use crate::infrastructure::persistence::json_file;
use crate::infrastructure::persistence::store_lock::StoreLock;

const STORE_DIR: &str = "tui/agent-tabs";
const INTENT_FILE: &str = "intent.json";

/// How an intent load reached its safe result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTabIntentLoadStatus {
    Loaded,
    Missing,
    /// Corrupt, future-schema, or wrong-workspace state was ignored. The file is
    /// left in place for diagnosis and a later valid mutation atomically
    /// replaces it.
    IgnoredInvalid,
}

/// Safe load outcome. Startup never needs to fail because one resume file is
/// corrupt or from a future schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTabIntentLoad {
    pub intent: AgentTabIntent,
    pub status: AgentTabIntentLoadStatus,
}

/// Result of one locked stable-key mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTabIntentCommit {
    pub intent: AgentTabIntent,
    pub projection: Option<AgentTabProjection>,
    /// The caller's revision was stale. The mutation was still merged into the
    /// latest state under the lock rather than replacing it.
    pub cas_conflict: bool,
}

/// Atomic file store rooted below the selected user data directory.
#[derive(Debug, Clone)]
pub struct AgentTabIntentStore {
    root: PathBuf,
}

impl AgentTabIntentStore {
    #[must_use]
    pub fn new(data_root: impl Into<PathBuf>) -> Self {
        Self {
            root: data_root.into().join(STORE_DIR),
        }
    }

    /// Resolve the selected user-local data directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the user's data directory cannot be resolved.
    pub fn open_default() -> Result<Self> {
        Ok(Self::new(data_dir()?))
    }

    #[must_use]
    pub fn workspace_dir(&self, workspace: WorkspaceId) -> PathBuf {
        self.root.join(workspace.as_str())
    }

    #[must_use]
    pub fn path(&self, workspace: WorkspaceId) -> PathBuf {
        self.workspace_dir(workspace).join(INTENT_FILE)
    }

    /// Read one workspace intent. Missing and invalid state safely become an
    /// empty intent; ordinary filesystem errors are reported to the adapter.
    ///
    /// # Errors
    ///
    /// Returns an error when an existing file cannot be read.
    pub fn load(&self, workspace: WorkspaceId) -> Result<AgentTabIntentLoad> {
        self.load_unlocked(workspace)
    }

    /// Apply one mutation while holding the workspace file lock. A stale
    /// expected revision is reported but does not lose the mutation: stable keys
    /// are applied to the newly-read latest state.
    ///
    /// # Errors
    ///
    /// Returns an error when locking, reading, serializing, or atomically writing
    /// the state fails.
    pub fn mutate(
        &self,
        workspace: WorkspaceId,
        expected_revision: u64,
        mutation: AgentTabIntentMutation,
    ) -> Result<AgentTabIntentCommit> {
        let dir = self.workspace_dir(workspace);
        let _guard = StoreLock::acquire(&dir)?;
        let loaded = self.load_unlocked(workspace)?;
        let mut intent = loaded.intent;
        let cas_conflict = intent.revision != expected_revision;
        let before = intent.clone();
        let projection = intent.apply(mutation);
        if intent != before {
            intent.revision = intent.revision.saturating_add(1);
            json_file::write_atomic(&dir, &self.path(workspace), &intent)?;
        }
        Ok(AgentTabIntentCommit {
            intent,
            projection,
            cas_conflict,
        })
    }

    fn load_unlocked(&self, workspace: WorkspaceId) -> Result<AgentTabIntentLoad> {
        let path = self.path(workspace);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AgentTabIntentLoad {
                    intent: AgentTabIntent::empty(workspace),
                    status: AgentTabIntentLoadStatus::Missing,
                });
            }
            Err(error) => {
                return Err(error).context(format!("failed to read {}", path.display()));
            }
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Ok(invalid(workspace));
        };
        if value.get("schema").and_then(serde_json::Value::as_u64)
            != Some(u64::from(AGENT_TAB_INTENT_SCHEMA))
        {
            return Ok(invalid(workspace));
        }
        let Ok(intent) = serde_json::from_value::<AgentTabIntent>(value) else {
            return Ok(invalid(workspace));
        };
        if intent.workspace_id != workspace || intent.schema != AGENT_TAB_INTENT_SCHEMA {
            return Ok(invalid(workspace));
        }
        Ok(AgentTabIntentLoad {
            intent,
            status: AgentTabIntentLoadStatus::Loaded,
        })
    }
}

fn invalid(workspace: WorkspaceId) -> AgentTabIntentLoad {
    AgentTabIntentLoad {
        intent: AgentTabIntent::empty(workspace),
        status: AgentTabIntentLoadStatus::IgnoredInvalid,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::domain::agent_tab_intent::{AgentTabSlotIntent, AgentTabTargetIntent};
    use crate::domain::id::{
        AgentContinuationRef, DaemonGeneration, SessionId, TerminalId, TerminalRef, WorktreeId,
    };
    use crate::infrastructure::persistence::json_file::{AtomicWriteStage, fail_next_atomic_write};

    fn terminal(workspace: WorkspaceId, session: SessionId) -> TerminalRef {
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: WorktreeId::new(),
        }
    }

    #[test]
    fn missing_corrupt_future_and_wrong_workspace_are_safe_empty_state() {
        let temp = tempfile::tempdir().unwrap();
        let store = AgentTabIntentStore::new(temp.path());
        let workspace = WorkspaceId::new();
        assert_eq!(
            store.load(workspace).unwrap().status,
            AgentTabIntentLoadStatus::Missing
        );
        fs::create_dir_all(store.workspace_dir(workspace)).unwrap();
        fs::write(store.path(workspace), "{broken").unwrap();
        assert_eq!(
            store.load(workspace).unwrap().status,
            AgentTabIntentLoadStatus::IgnoredInvalid
        );
        fs::write(
            store.path(workspace),
            format!(r#"{{"schema":{AGENT_TAB_INTENT_SCHEMA},"workspace_id":false}}"#),
        )
        .unwrap();
        assert_eq!(
            store.load(workspace).unwrap().status,
            AgentTabIntentLoadStatus::IgnoredInvalid
        );
        fs::write(store.path(workspace), r#"{"schema":999}"#).unwrap();
        assert_eq!(
            store.load(workspace).unwrap().status,
            AgentTabIntentLoadStatus::IgnoredInvalid
        );
        let mut wrong = AgentTabIntent::empty(WorkspaceId::new());
        wrong.revision = 7;
        json_file::write_atomic(
            &store.workspace_dir(workspace),
            &store.path(workspace),
            &wrong,
        )
        .unwrap();
        assert_eq!(
            store.load(workspace).unwrap().status,
            AgentTabIntentLoadStatus::IgnoredInvalid
        );
    }

    #[test]
    fn atomic_mutation_round_trips_and_failed_replace_keeps_last_valid_state() {
        let temp = tempfile::tempdir().unwrap();
        let store = AgentTabIntentStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let continuation = AgentContinuationRef::new();
        let first = terminal(workspace, session);
        let committed = store
            .mutate(
                workspace,
                0,
                AgentTabIntentMutation::Upsert {
                    session_id: Some(session),
                    continuation,
                    terminal: first.clone(),
                    select: true,
                },
            )
            .unwrap();
        assert_eq!(committed.intent.revision, 1);
        assert_eq!(store.load(workspace).unwrap().intent, committed.intent);

        fail_next_atomic_write(&store.path(workspace), AtomicWriteStage::Rename);
        assert!(
            store
                .mutate(
                    workspace,
                    1,
                    AgentTabIntentMutation::Dismiss { continuation },
                )
                .is_err()
        );
        assert_eq!(store.load(workspace).unwrap().intent, committed.intent);
    }

    #[test]
    fn stale_writers_merge_dismissal_and_reorder_by_stable_key() {
        let temp = tempfile::tempdir().unwrap();
        let store = AgentTabIntentStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let first = AgentContinuationRef::new();
        let second = AgentContinuationRef::new();
        let first_terminal = terminal(workspace, session);
        let second_terminal = terminal(workspace, session);
        for (continuation, terminal) in [
            (first, first_terminal.clone()),
            (second, second_terminal.clone()),
        ] {
            let revision = store.load(workspace).unwrap().intent.revision;
            store
                .mutate(
                    workspace,
                    revision,
                    AgentTabIntentMutation::Upsert {
                        session_id: Some(session),
                        continuation,
                        terminal,
                        select: false,
                    },
                )
                .unwrap();
        }
        let stale_revision = 0;
        store
            .mutate(
                workspace,
                store.load(workspace).unwrap().intent.revision,
                AgentTabIntentMutation::Dismiss {
                    continuation: first,
                },
            )
            .unwrap();
        let reordered = store
            .mutate(
                workspace,
                stale_revision,
                AgentTabIntentMutation::Reorder {
                    session_id: Some(session),
                    continuations: vec![second, first],
                },
            )
            .unwrap();
        assert!(reordered.cas_conflict);
        assert!(reordered.intent.dismissed.contains(&first));
        assert_eq!(
            reordered.intent.targets[0]
                .tabs
                .iter()
                .map(|slot| slot.continuation)
                .collect::<Vec<_>>(),
            [second, first]
        );

        // Derives and schema fields remain exercised by a representative value.
        let value = AgentTabIntent {
            schema: AGENT_TAB_INTENT_SCHEMA,
            workspace_id: workspace,
            revision: reordered.intent.revision,
            targets: vec![AgentTabTargetIntent {
                session_id: Some(session),
                tabs: vec![AgentTabSlotIntent {
                    continuation: second,
                    terminal: second_terminal,
                }],
                selected: Some(second),
            }],
            dismissed: BTreeSet::from([first]),
        };
        assert!(format!("{value:?}").contains("revision"));
    }

    #[test]
    fn concurrent_clients_union_close_intent_under_the_file_lock() {
        let temp = tempfile::tempdir().unwrap();
        let store = AgentTabIntentStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let first = AgentContinuationRef::new();
        let second = AgentContinuationRef::new();
        for continuation in [first, second] {
            let revision = store.load(workspace).unwrap().intent.revision;
            store
                .mutate(
                    workspace,
                    revision,
                    AgentTabIntentMutation::Upsert {
                        session_id: Some(session),
                        continuation,
                        terminal: terminal(workspace, session),
                        select: false,
                    },
                )
                .unwrap();
        }
        let expected = store.load(workspace).unwrap().intent.revision;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut handles = Vec::new();
        for continuation in [first, second] {
            let store = store.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                store
                    .mutate(
                        workspace,
                        expected,
                        AgentTabIntentMutation::Dismiss { continuation },
                    )
                    .unwrap()
            }));
        }
        barrier.wait();
        let commits = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert!(commits.iter().any(|commit| commit.cas_conflict));
        assert_eq!(
            store.load(workspace).unwrap().intent.dismissed,
            BTreeSet::from([first, second])
        );
    }

    #[test]
    fn path_is_user_local_and_workspace_scoped() {
        let store = AgentTabIntentStore::new("/data/local");
        let workspace = WorkspaceId::new();
        assert_eq!(
            store.path(workspace),
            std::path::Path::new("/data/local")
                .join(STORE_DIR)
                .join(workspace.as_str())
                .join(INTENT_FILE)
        );
    }

    #[test]
    fn non_file_intent_path_reports_a_safe_read_error() {
        let temp = tempfile::tempdir().unwrap();
        let store = AgentTabIntentStore::new(temp.path());
        let workspace = WorkspaceId::new();
        fs::create_dir_all(store.path(workspace)).unwrap();
        assert!(store.load(workspace).is_err());
    }
}
