//! Durable daemon-owned managed-session runtime.
//!
//! The reducer and store in `usagi-core` deliberately have no process or git
//! dependency.  This adapter is their only daemon-side effect owner: it
//! durably reserves an operation before invoking git, then applies the exact
//! completion fence captured from the reservation.

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::{Value, json};
use usagi_core::domain::id::{CompletionFence, DaemonGeneration, OperationId, WorkspaceId};
use usagi_core::domain::session_lifecycle::{
    DeletePlan, Failure, FailureStage, LifecycleEvent, OperationJournal, OperationStatus,
    WorkspaceLifecycleState,
};
use usagi_core::infrastructure::git::{GitOutput, GitRunner, add_worktree, remove_worktree};
use usagi_core::infrastructure::paths::{SESSIONS_DIR, STATE_DIR};
use usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore;
use usagi_core::usecase::client::SessionAction;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionReply {
    pub operation_id: String,
    pub revision: u64,
    pub body: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRuntimeError {
    InvalidRequest,
    InvalidOperation,
    DuplicateOperation,
    UnknownSession,
    Rejected,
    Storage,
}

impl SessionRuntimeError {
    #[must_use]
    pub const fn safe_message(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid session request",
            Self::InvalidOperation => "invalid operation identity",
            Self::DuplicateOperation => "operation identity conflicts with an existing request",
            Self::UnknownSession => "session was not found",
            Self::Rejected => "session lifecycle rejected the request",
            Self::Storage => "daemon could not persist session lifecycle state",
        }
    }
}

/// Real git seam kept here so the daemon crate owns the worktree effect while
/// unit tests inject a deterministic runner.
pub struct SystemGit;
impl GitRunner for SystemGit {
    #[coverage(off)]
    fn run(&self, repo: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()?;
        Ok(GitOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// One daemon process's session writer.  Callers serialize it across IPC
/// connections; the store also locks every reducer mutation for crash safety.
pub struct SessionRuntime<G> {
    repo_root: PathBuf,
    generation: DaemonGeneration,
    store: DaemonLifecycleStore,
    git: G,
}

impl<G: GitRunner> SessionRuntime<G> {
    /// # Errors
    ///
    /// Returns an error when the lifecycle state cannot be loaded or initialized.
    pub fn open(
        repo_root: PathBuf,
        generation: DaemonGeneration,
        git: G,
    ) -> Result<Self, SessionRuntimeError> {
        let store = DaemonLifecycleStore::new(&repo_root);
        if store
            .load()
            .map_err(|_| SessionRuntimeError::Storage)?
            .is_none()
        {
            store
                .initialize(&WorkspaceLifecycleState::new(
                    WorkspaceId::new(),
                    Utc::now(),
                ))
                .map_err(|_| SessionRuntimeError::Storage)?;
        }
        Ok(Self {
            repo_root,
            generation,
            store,
            git,
        })
    }

    /// # Errors
    ///
    /// Returns a typed safe error when the request cannot be admitted or completed.
    #[allow(clippy::single_match_else)]
    pub fn handle(
        &mut self,
        action: SessionAction,
        operation_id: &str,
        payload: &Value,
    ) -> Result<SessionReply, SessionRuntimeError> {
        match action {
            SessionAction::Create => self.create(operation_id, payload),
            SessionAction::Remove => self.remove(operation_id, payload),
            SessionAction::List | SessionAction::Overview => {
                let state = self.state()?;
                Ok(SessionReply {
                    operation_id: operation_id.to_owned(),
                    revision: state.state_revision,
                    body: snapshot(&state),
                })
            }
            SessionAction::Setup | SessionAction::Prompt => {
                Err(SessionRuntimeError::InvalidRequest)
            }
        }
    }

    /// # Errors
    ///
    /// Returns an error when the durable lifecycle state cannot be read.
    pub fn snapshot(&self) -> Result<Value, SessionRuntimeError> {
        let state = self.state()?;
        Ok(
            json!({"workspace_id": state.workspace_id, "revision": state.state_revision, "sessions": state.sessions}),
        )
    }

    #[allow(clippy::single_match_else)]
    fn create(
        &mut self,
        operation_id: &str,
        payload: &Value,
    ) -> Result<SessionReply, SessionRuntimeError> {
        let name = session_name(payload)?;
        let operation_id =
            OperationId::parse(operation_id).map_err(|_| SessionRuntimeError::InvalidOperation)?;
        let before = self.state()?;
        if let Some(existing) = before
            .operations
            .iter()
            .find(|op| op.operation_id == operation_id)
        {
            return Ok(SessionReply {
                operation_id: operation_id.to_string(),
                revision: existing.progress_revision,
                body: self.snapshot()?,
            });
        }
        let operation = journal(operation_id, self.generation);
        let reserved = self
            .store
            .apply(
                self.generation,
                LifecycleEvent::ReserveCreate {
                    name: name.clone(),
                    operation,
                },
                Utc::now(),
            )
            .map_err(|_| SessionRuntimeError::Rejected)?;
        let session = reserved
            .sessions
            .last()
            .ok_or(SessionRuntimeError::Rejected)?;
        let fence = fence(&reserved, session, operation_id);
        let path = self
            .repo_root
            .join(STATE_DIR)
            .join(SESSIONS_DIR)
            .join(&name);
        match add_worktree(
            &self.git,
            &self.repo_root,
            &path,
            &format!("usagi/{name}"),
            None,
        ) {
            Ok(()) => {
                let completed = self
                    .store
                    .apply(
                        self.generation,
                        LifecycleEvent::CreateCompleted {
                            fence,
                            setup_plan: None,
                        },
                        Utc::now(),
                    )
                    .map_err(|_| SessionRuntimeError::Storage)?;
                Ok(SessionReply {
                    operation_id: operation_id.to_string(),
                    revision: completed.state_revision,
                    body: snapshot(&completed),
                })
            }
            Err(_) => {
                let _ = self.store.apply(
                    self.generation,
                    LifecycleEvent::Failed {
                        fence,
                        failure: Failure {
                            stage: FailureStage::Create,
                            summary: "worktree creation failed".into(),
                        },
                    },
                    Utc::now(),
                );
                Err(SessionRuntimeError::Rejected)
            }
        }
    }

    #[allow(clippy::single_match_else)]
    fn remove(
        &mut self,
        operation_id: &str,
        payload: &Value,
    ) -> Result<SessionReply, SessionRuntimeError> {
        let name = session_name(payload)?;
        let operation_id =
            OperationId::parse(operation_id).map_err(|_| SessionRuntimeError::InvalidOperation)?;
        let before = self.state()?;
        if before
            .operations
            .iter()
            .any(|op| op.operation_id == operation_id)
        {
            return Err(SessionRuntimeError::DuplicateOperation);
        }
        let session = before
            .sessions
            .iter()
            .find(|session| session.name == name)
            .ok_or(SessionRuntimeError::UnknownSession)?;
        let session_id = session.session_id;
        let operation = journal(operation_id, self.generation);
        let removing = self
            .store
            .apply(
                self.generation,
                LifecycleEvent::BeginRemove {
                    session_id,
                    operation,
                    delete_plan: DeletePlan {
                        targets: vec![name.clone()],
                        force: false,
                    },
                },
                Utc::now(),
            )
            .map_err(|_| SessionRuntimeError::Rejected)?;
        let session = removing
            .sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .ok_or(SessionRuntimeError::Rejected)?;
        let fence = fence(&removing, session, operation_id);
        let path = self
            .repo_root
            .join(STATE_DIR)
            .join(SESSIONS_DIR)
            .join(&name);
        match remove_worktree(&self.git, &self.repo_root, &path, false) {
            Ok(()) => {
                let completed = self
                    .store
                    .apply(
                        self.generation,
                        LifecycleEvent::Completed { fence },
                        Utc::now(),
                    )
                    .map_err(|_| SessionRuntimeError::Storage)?;
                Ok(SessionReply {
                    operation_id: operation_id.to_string(),
                    revision: completed.state_revision,
                    body: snapshot(&completed),
                })
            }
            Err(_) => {
                let _ = self.store.apply(
                    self.generation,
                    LifecycleEvent::Failed {
                        fence,
                        failure: Failure {
                            stage: FailureStage::Delete,
                            summary: "worktree removal failed".into(),
                        },
                    },
                    Utc::now(),
                );
                Err(SessionRuntimeError::Rejected)
            }
        }
    }

    fn state(&self) -> Result<WorkspaceLifecycleState, SessionRuntimeError> {
        self.store
            .load()
            .map_err(|_| SessionRuntimeError::Storage)?
            .ok_or(SessionRuntimeError::Storage)
    }
}

fn session_name(payload: &Value) -> Result<String, SessionRuntimeError> {
    let name = payload
        .get("name")
        .or_else(|| payload.get("label"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        })
        .ok_or(SessionRuntimeError::InvalidRequest)?;
    Ok(name.to_owned())
}

fn journal(operation_id: OperationId, generation: DaemonGeneration) -> OperationJournal {
    OperationJournal {
        operation_id,
        owner_daemon_generation: generation,
        status: OperationStatus::Accepted,
        execution_attempt: 1,
        progress_revision: 0,
    }
}

fn fence(
    state: &WorkspaceLifecycleState,
    session: &usagi_core::domain::session_lifecycle::ManagedSession,
    operation_id: OperationId,
) -> CompletionFence {
    CompletionFence {
        workspace_id: state.workspace_id,
        session_id: session.session_id,
        operation_id,
        owner_daemon_generation: state
            .operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .map(|operation| operation.owner_daemon_generation)
            .expect("reserved operation exists"),
        execution_attempt: 1,
        lifecycle_attempt: session.attempt,
        expected_revision: state.state_revision,
    }
}

fn snapshot(state: &WorkspaceLifecycleState) -> Value {
    json!({"workspace_id": state.workspace_id, "revision": state.state_revision, "sessions": state.sessions})
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct FakeGit(bool);
    impl FakeGit {
        fn ok() -> Self {
            Self(true)
        }
        fn fail() -> Self {
            Self(false)
        }
    }
    impl GitRunner for FakeGit {
        fn run(&self, _: &Path, _: &[&str]) -> anyhow::Result<GitOutput> {
            Ok(GitOutput {
                success: self.0,
                stdout: String::new(),
                stderr: "no".into(),
            })
        }
    }
    fn runtime(git: FakeGit) -> (TempDir, SessionRuntime<FakeGit>) {
        let tmp = tempfile::tempdir().unwrap();
        let runtime =
            SessionRuntime::open(tmp.path().to_path_buf(), DaemonGeneration::new(), git).unwrap();
        (tmp, runtime)
    }
    fn operation() -> String {
        OperationId::new().to_string()
    }
    #[test]
    fn create_lists_overview_and_removes_a_durable_session() {
        let (_tmp, mut runtime) = runtime(FakeGit::ok());
        let created = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        assert_eq!(created.body["sessions"].as_array().unwrap().len(), 1);
        let list = runtime
            .handle(SessionAction::List, "read", &json!({}))
            .unwrap();
        assert_eq!(list.revision, created.revision);
        let overview = runtime
            .handle(SessionAction::Overview, "read", &json!({}))
            .unwrap();
        assert_eq!(overview.body, list.body);
        let removed = runtime
            .handle(SessionAction::Remove, &operation(), &json!({"name":"one"}))
            .unwrap();
        assert!(removed.body["sessions"].as_array().unwrap().is_empty());
    }
    #[test]
    fn rejects_invalid_requests_duplicates_missing_sessions_and_git_failures() {
        let (_tmp, mut runtime) = runtime(FakeGit::fail());
        assert_eq!(
            runtime
                .handle(SessionAction::Create, "bad", &json!({"name":"one"}))
                .unwrap_err(),
            SessionRuntimeError::InvalidOperation
        );
        assert_eq!(
            runtime
                .handle(
                    SessionAction::Create,
                    &operation(),
                    &json!({"name":"../bad"})
                )
                .unwrap_err(),
            SessionRuntimeError::InvalidRequest
        );
        assert_eq!(
            runtime
                .handle(SessionAction::Remove, &operation(), &json!({"name":"none"}))
                .unwrap_err(),
            SessionRuntimeError::UnknownSession
        );
        assert_eq!(
            runtime
                .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
                .unwrap_err(),
            SessionRuntimeError::Rejected
        );
        assert_eq!(
            runtime
                .handle(SessionAction::Setup, &operation(), &json!({}))
                .unwrap_err(),
            SessionRuntimeError::InvalidRequest
        );
    }
    #[test]
    fn same_create_operation_returns_its_existing_snapshot() {
        let (_tmp, mut runtime) = runtime(FakeGit::ok());
        let operation = operation();
        runtime
            .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
            .unwrap();
        assert!(
            runtime
                .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
                .is_ok()
        );
        assert!(
            runtime
                .handle(SessionAction::Create, &operation, &json!({"name":"two"}))
                .is_ok()
        );
    }
}
