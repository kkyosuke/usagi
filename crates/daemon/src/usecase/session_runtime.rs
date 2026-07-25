//! Durable daemon-owned managed-session runtime.
//!
//! The reducer and store in `usagi-core` deliberately have no process or git
//! dependency.  This adapter is their only daemon-side effect owner: it
//! durably reserves an operation before invoking git, then applies the exact
//! completion fence captured from the reservation.

#![coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=session_runtime_fake_git_contract

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::{Value, json};
use usagi_core::domain::id::{
    CompletionFence, DaemonGeneration, OperationId, SessionId, WorkspaceId, WorktreeId,
};
use usagi_core::domain::session_lifecycle::{
    DeletePlan, Failure, FailureStage, LifecycleEvent, OperationJournal, OperationStatus,
    WorkspaceLifecycleState,
};
use usagi_core::infrastructure::git::list_worktrees;
use usagi_core::infrastructure::git::{GitOutput, GitRunner, add_worktree, remove_worktree};
use usagi_core::infrastructure::gitignore::migrate_usagi_ignore_rules;
use usagi_core::infrastructure::ipc::ErrorCode;
use usagi_core::infrastructure::paths::{SESSIONS_DIR, STATE_DIR, project_data_dir};
use usagi_core::infrastructure::persistence::json_file;
use usagi_core::infrastructure::store::issue::AmbiguousIssueNumber;
use usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore;
use usagi_core::infrastructure::store::state::WorkspaceStateStore;
use usagi_core::usecase::client::SessionAction;

use crate::usecase::session_teardown::{
    PendingTeardown, TeardownEffect, TeardownJournal, TeardownSignal,
};

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
    IdempotencyConflict,
    SessionBranchExists(String),
    SessionWorkspaceExists(String),
    SessionWorkspaceCreationFailed { name: String, detail: String },
    DurableFailure(String),
    UnknownSession,
    ScopeUnavailable,
    AgentFailure { code: ErrorCode, message: String },
    Delivery(String),
    AmbiguousIssue(AmbiguousIssueNumber),
    Rejected,
    Storage,
}

impl SessionRuntimeError {
    #[must_use]
    pub fn safe_message(&self) -> String {
        match self {
            Self::InvalidRequest => "invalid session request".into(),
            Self::InvalidOperation => "invalid operation identity".into(),
            Self::DuplicateOperation => {
                "operation identity conflicts with an existing request".into()
            }
            Self::IdempotencyConflict => "operation id was reused with a different request".into(),
            Self::SessionBranchExists(name) => format!(
                "cannot create session \"{name}\": branch usagi/{name} already exists; choose a different name or remove the stale branch"
            ),
            Self::SessionWorkspaceExists(name) => format!(
                "cannot create session \"{name}\": workspace already exists; choose a different name or remove the stale workspace"
            ),
            Self::SessionWorkspaceCreationFailed { name, detail } => {
                format!("cannot create session \"{name}\": {detail}")
            }
            Self::DurableFailure(message)
            | Self::AgentFailure { message, .. }
            | Self::Delivery(message) => message.clone(),
            Self::AmbiguousIssue(error) => error.to_string(),
            Self::UnknownSession => "session was not found".into(),
            Self::ScopeUnavailable => "session scope is not available".into(),
            Self::Rejected => {
                "could not create the session worktree; see the daemon log for details".into()
            }
            Self::Storage => "daemon could not persist session lifecycle state".into(),
        }
    }
}

/// A daemon-resolved checkout scope.  Consumers must retain this full stable
/// identity; the daemon never resolves a client supplied name or path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionScope {
    pub workspace_id: WorkspaceId,
    pub session_id: SessionId,
    pub worktree_id: WorktreeId,
    pub path: PathBuf,
}

/// Real git seam kept here so the daemon crate owns the worktree effect while
/// unit tests inject a deterministic runner.
pub struct SystemGit;
impl GitRunner for SystemGit {
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
pub struct SessionRuntime {
    repo_root: PathBuf,
    root_worktree_id: WorktreeId,
    generation: DaemonGeneration,
    store: DaemonLifecycleStore,
    git: Box<dyn GitRunner + Send>,
}

/// Outcome of [`SessionRuntime::begin_create`]: either an outcome fully resolved
/// under the lock (idempotent replay) or a pending worktree build to run with
/// the lock released.
enum SessionCreateStep {
    Done(SessionReply),
    Pending(SessionCreateInFlight),
}

/// The reserved-but-not-yet-built state of a create, carried across the lock
/// release so [`SessionRuntime::execute_create`] can build the worktree without
/// the shared session lock held.
struct SessionCreateInFlight {
    operation_id: OperationId,
    fence: CompletionFence,
    name: String,
    workspace_root: PathBuf,
    destination: PathBuf,
    branch: String,
}

/// Outcome of [`SessionRuntime::begin_remove`].
///
/// Both variants are a complete reply the caller can return right away; they
/// differ only in whether this request is the one that admitted a new teardown.
enum SessionRemoveStep {
    /// Fully resolved under the lock: an idempotent replay of a finished
    /// operation, or a removal that is already in flight.
    Settled(SessionReply),
    /// The session is now durably `Deleting`. The reply is the acceptance; the
    /// teardown worker owns the worktree effect from here.
    Accepted {
        reply: SessionReply,
        pending: PendingTeardown,
    },
}

/// Creates a session while holding the shared session lock only for the fast
/// durable transitions. The heavy Git worktree build runs with the lock
/// released so concurrent reads (session list, terminal poll, user-decision
/// list) stay responsive during a create — the daemon no longer freezes the TUI
/// for the duration of `git worktree add`.
///
/// # Errors
///
/// Returns a typed safe error when the request cannot be admitted or completed.
pub fn perform_create(
    runtime: &Mutex<SessionRuntime>,
    git: &dyn GitRunner,
    operation_id: &str,
    payload: &Value,
) -> Result<SessionReply, SessionRuntimeError> {
    let step = runtime
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?
        .begin_create(operation_id, payload)?;
    match step {
        SessionCreateStep::Done(reply) => Ok(reply),
        SessionCreateStep::Pending(in_flight) => {
            let result = SessionRuntime::execute_create(git, &in_flight);
            runtime
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .finish_create(in_flight, result)
        }
    }
}

/// Admits a removal and answers immediately.
///
/// Only the fast durable transition (validation, `Deleting`, the durable
/// `DeletePlan`) runs here, under the shared session lock. The unbounded
/// worktree teardown is left to the daemon's teardown worker, which this
/// function wakes: a session holding a multi-gigabyte `target/` would otherwise
/// hold the requesting connection past every client attempt deadline (TUI 2 s /
/// CLI 10 s / MCP 30 s) and block the other requests queued on that connection.
///
/// # Errors
///
/// Returns a typed safe error when the request cannot be admitted.
pub fn perform_remove(
    runtime: &Mutex<SessionRuntime>,
    teardown: &TeardownSignal,
    operation_id: &str,
    payload: &Value,
) -> Result<SessionReply, SessionRuntimeError> {
    let step = runtime
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?
        .begin_remove(operation_id, payload)?;
    match step {
        SessionRemoveStep::Settled(reply) => Ok(reply),
        SessionRemoveStep::Accepted { reply, .. } => {
            // The admitted teardown is not handed over here: the worker derives
            // it from the durable state this admission just wrote, which is what
            // makes a crashed daemon resume it. Waking the worker only avoids
            // waiting for its next tick.
            teardown.notify();
            Ok(reply)
        }
    }
}

/// The teardown journal backed by the daemon's shared session runtime.
///
/// Both halves take the shared session lock only for a fast durable read or
/// write, so the worker never holds it across the worktree effect.
pub struct SharedSessionTeardown {
    runtime: Arc<Mutex<SessionRuntime>>,
}

impl SharedSessionTeardown {
    #[must_use]
    pub const fn new(runtime: Arc<Mutex<SessionRuntime>>) -> Self {
        Self { runtime }
    }
}

impl TeardownJournal for SharedSessionTeardown {
    fn pending(&self) -> Vec<PendingTeardown> {
        self.runtime
            .lock()
            .ok()
            .and_then(|runtime| runtime.pending_teardowns().ok())
            .unwrap_or_default()
    }

    fn finish(
        &self,
        teardown: &PendingTeardown,
        outcome: Result<(), String>,
    ) -> Result<(), String> {
        match self
            .runtime
            .lock()
            .map_err(|_| "session lifecycle owner is unavailable".to_owned())?
            .finish_teardown(teardown, outcome)
        {
            // A recorded teardown failure *is* a successful finalization: the
            // durable row now carries the reason and is no longer pending. Only
            // a persistence error means the outcome could not be recorded, and
            // only that must leave the teardown for the next drain.
            Ok(_) | Err(SessionRuntimeError::DurableFailure(_)) => Ok(()),
            Err(error) => Err(error.safe_message()),
        }
    }
}

/// The real worktree teardown: nested linked worktrees are removed with Git
/// before the session tree itself. `NotFound` counts as success, so a resumed
/// teardown can safely re-run over a partially removed tree.
pub struct WorktreeTeardown<G: GitRunner> {
    git: G,
}

impl<G: GitRunner> WorktreeTeardown<G> {
    #[must_use]
    pub const fn new(git: G) -> Self {
        Self { git }
    }
}

impl<G: GitRunner> TeardownEffect for WorktreeTeardown<G> {
    fn tear_down(&self, teardown: &PendingTeardown) -> Result<(), String> {
        remove_session_tree(&self.git, &teardown.session_root, teardown.force)
            .map_err(|error| error.to_string())
    }
}

impl SessionRuntime {
    /// Returns the repository root durably trusted by this daemon's session store.
    #[must_use]
    pub fn repository_root(&self) -> &Path {
        &self.repo_root
    }

    /// Returns the durable workspace-root checkout identity. It is a real,
    /// persisted incarnation (never derived from a name or path), so a
    /// workspace-root terminal/agent is fenced exactly like a session one.
    #[must_use]
    pub fn root_worktree_id(&self) -> WorktreeId {
        self.root_worktree_id
    }

    /// Resolves the trusted workspace-root scope. The client never supplies the
    /// path: the workspace and root-worktree identities are verified against the
    /// daemon's durable state, and the returned path is always the trusted
    /// repository root.
    ///
    /// # Errors
    ///
    /// Returns [`SessionRuntimeError::ScopeUnavailable`] when the workspace or
    /// root-worktree identity is not this daemon's.
    pub fn resolve_root_scope(
        &self,
        workspace_id: WorkspaceId,
        worktree_id: WorktreeId,
    ) -> Result<PathBuf, SessionRuntimeError> {
        let state = self.state()?;
        if state.workspace_id != workspace_id || worktree_id != self.root_worktree_id {
            return Err(SessionRuntimeError::ScopeUnavailable);
        }
        Ok(self.repo_root.clone())
    }

    /// # Errors
    ///
    /// Returns an error when the lifecycle state cannot be loaded or initialized.
    pub fn open<G: GitRunner + Send + 'static>(
        candidate_repo_root: PathBuf,
        state_dir: &Path,
        generation: DaemonGeneration,
        git: G,
    ) -> Result<Self, SessionRuntimeError> {
        let store = DaemonLifecycleStore::new(state_dir);
        let repo_root = if let Some((repository_root, mut state)) = store
            .load_with_workspace()
            .map_err(|_| SessionRuntimeError::Storage)?
        {
            let revision = state.state_revision;
            if state.repair_legacy_failed_outcomes(Utc::now()) != 0 {
                store
                    .replace_if_revision(revision, &state)
                    .map_err(|_| SessionRuntimeError::Storage)?;
            }
            repository_root
        } else {
            let legacy_lifecycle =
                project_data_dir(&candidate_repo_root).join("lifecycle-state.json");
            let state = if let Some(state) =
                json_file::read(&legacy_lifecycle).map_err(|_| SessionRuntimeError::Storage)?
            {
                state
            } else {
                adopt_legacy_workspace_sessions(&candidate_repo_root, &git)?
                    .unwrap_or_else(|| WorkspaceLifecycleState::new(WorkspaceId::new(), Utc::now()))
            };
            store
                .initialize(&state, &candidate_repo_root)
                .map_err(|_| SessionRuntimeError::Storage)?;
            // The migrated state is already durable in `sessions.json`; from now
            // on the `Some(..)` branch wins and the legacy file is never read
            // again. Removing it is best-effort cleanup, so a failure here must
            // not fail daemon startup over an otherwise-ignored stale file.
            let _ = std::fs::remove_file(&legacy_lifecycle);
            candidate_repo_root
        };
        let root_worktree_id = store
            .ensure_root_worktree_id()
            .map_err(|_| SessionRuntimeError::Storage)?;
        let mut runtime = Self {
            repo_root,
            root_worktree_id,
            generation,
            store,
            git: Box::new(git),
        };
        if is_repo_root(&runtime.repo_root) {
            migrate_usagi_ignore_rules(&runtime.repo_root)
                .map_err(|_| SessionRuntimeError::Storage)?;
        }
        runtime.reconcile()?;
        Ok(runtime)
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
            SessionAction::RecoverLegacy => self.recover_legacy(operation_id, payload),
            SessionAction::List | SessionAction::Overview => {
                let state = self.state()?;
                Ok(SessionReply {
                    operation_id: operation_id.to_owned(),
                    revision: state.state_revision,
                    body: snapshot(&state, self.root_worktree_id),
                })
            }
            SessionAction::Status => self.status(operation_id),
            SessionAction::Setup
            | SessionAction::ResumeAgent
            | SessionAction::Prompt
            | SessionAction::Complete
            | SessionAction::Pr
            | SessionAction::NoteGet
            | SessionAction::NoteUpdate
            | SessionAction::TodoList
            | SessionAction::TodoAdd
            | SessionAction::TodoUpdate
            | SessionAction::TodoRemove
            | SessionAction::DecisionList
            | SessionAction::DecisionLog
            | SessionAction::DelegateIssue
            | SessionAction::DelegateBrief => Err(SessionRuntimeError::InvalidRequest),
        }
    }

    fn status(&self, operation_id: &str) -> Result<SessionReply, SessionRuntimeError> {
        let state = self.state()?;
        let base = self
            .git
            .run(&self.repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .map_err(|_| SessionRuntimeError::Storage)?;
        if !base.success {
            return Err(SessionRuntimeError::Storage);
        }
        let base = base.stdout.trim();
        let sessions = state
            .sessions
            .iter()
            .filter(|session| {
                session.lifecycle
                    == usagi_core::domain::session_lifecycle::SessionLifecycle::Available
            })
            .map(|session| {
                let root = self
                    .repo_root
                    .join(STATE_DIR)
                    .join(SESSIONS_DIR)
                    .join(&session.name);
                let porcelain = self
                    .git
                    .run(&root, &["status", "--porcelain"])
                    .map_err(|_| SessionRuntimeError::Storage)?;
                let branch = self
                    .git
                    .run(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
                    .map_err(|_| SessionRuntimeError::Storage)?;
                let merged = self
                    .git
                    .run(&root, &["merge-base", "--is-ancestor", "HEAD", base])
                    .map_err(|_| SessionRuntimeError::Storage)?;
                if !porcelain.success || !branch.success {
                    return Err(SessionRuntimeError::Storage);
                }
                let dirty = !porcelain.stdout.trim().is_empty();
                let merged = merged.success;
                let status = if dirty {
                    "dirty"
                } else if merged {
                    "synced"
                } else {
                    "local"
                };
                Ok(json!({
                    "name": session.name,
                    "session_id": session.session_id,
                    "lifecycle": session.lifecycle,
                    "agent_phase": "none",
                    "worktrees": [{
                        "path": root,
                        "branch": branch.stdout.trim(),
                        "status": status,
                        "dirty": dirty,
                        "merged": merged,
                    }],
                }))
            })
            .collect::<Result<Vec<_>, SessionRuntimeError>>()?;
        Ok(SessionReply {
            operation_id: operation_id.to_owned(),
            revision: state.state_revision,
            body: json!({"workspace_id": state.workspace_id, "revision": state.state_revision, "sessions": sessions}),
        })
    }

    /// Resolves an available session by its public name to its stable identity.
    ///
    /// # Errors
    ///
    /// Returns a safe storage or unknown-session error.
    pub fn session_id(&self, name: &str) -> Result<SessionId, SessionRuntimeError> {
        self.state()?
            .sessions
            .into_iter()
            .find(|session| {
                session.name == name
                    && session.lifecycle
                        == usagi_core::domain::session_lifecycle::SessionLifecycle::Available
            })
            .map(|session| session.session_id)
            .ok_or(SessionRuntimeError::UnknownSession)
    }

    /// Resolves an available stable session identity to its trusted worktree.
    ///
    /// # Errors
    ///
    /// Returns a safe storage or unknown-session error.
    pub fn session_scope_by_id(
        &self,
        session_id: SessionId,
    ) -> Result<SessionScope, SessionRuntimeError> {
        let state = self.state()?;
        let session = state
            .sessions
            .into_iter()
            .find(|session| {
                session.session_id == session_id
                    && session.lifecycle
                        == usagi_core::domain::session_lifecycle::SessionLifecycle::Available
            })
            .ok_or(SessionRuntimeError::UnknownSession)?;
        Ok(SessionScope {
            workspace_id: state.workspace_id,
            session_id,
            worktree_id: session.worktree_id,
            path: self
                .repo_root
                .join(STATE_DIR)
                .join(SESSIONS_DIR)
                .join(session.name),
        })
    }

    /// # Errors
    ///
    /// Returns an error when the durable lifecycle state cannot be read.
    pub fn snapshot(&self) -> Result<Value, SessionRuntimeError> {
        let state = self.state()?;
        Ok(snapshot(&state, self.root_worktree_id))
    }

    /// Resolves only an available, fully fenced managed session to a path.
    /// Name-only and path-only lookup deliberately do not exist at this port.
    ///
    /// # Errors
    ///
    /// Returns [`SessionRuntimeError::ScopeUnavailable`] when the supplied
    /// stable identity is not the current available managed session.
    pub fn resolve_scope(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        worktree_id: WorktreeId,
    ) -> Result<SessionScope, SessionRuntimeError> {
        let state = self.state()?;
        if state.workspace_id != workspace_id {
            return Err(SessionRuntimeError::ScopeUnavailable);
        }
        let session = state
            .sessions
            .iter()
            .find(|candidate| {
                candidate.session_id == session_id
                    && candidate.worktree_id == worktree_id
                    && candidate.lifecycle
                        == usagi_core::domain::session_lifecycle::SessionLifecycle::Available
            })
            .ok_or(SessionRuntimeError::ScopeUnavailable)?;
        Ok(SessionScope {
            workspace_id,
            session_id,
            worktree_id,
            path: self
                .repo_root
                .join(STATE_DIR)
                .join(SESSIONS_DIR)
                .join(&session.name),
        })
    }

    fn create(
        &mut self,
        operation_id: &str,
        payload: &Value,
    ) -> Result<SessionReply, SessionRuntimeError> {
        match self.begin_create(operation_id, payload)? {
            SessionCreateStep::Done(reply) => Ok(reply),
            SessionCreateStep::Pending(in_flight) => {
                let result = Self::execute_create(self.git.as_ref(), &in_flight);
                self.finish_create(in_flight, result)
            }
        }
    }

    /// Validates the request, reserves the create operation, and computes the
    /// worktree build plan. Runs under the shared session lock; the heavy Git
    /// build is deferred to [`Self::execute_create`] so the lock can be released
    /// (see [`perform_create`]).
    fn begin_create(
        &mut self,
        operation_id: &str,
        payload: &Value,
    ) -> Result<SessionCreateStep, SessionRuntimeError> {
        let name = session_name(payload)?;
        let operation_id =
            OperationId::parse(operation_id).map_err(|_| SessionRuntimeError::InvalidOperation)?;
        let before = self.state()?;
        let semantic_key = semantic_key(SessionAction::Create, &name);
        if let Some(existing) = before
            .operations
            .iter()
            .find(|op| op.operation_id == operation_id)
        {
            if existing.semantic_key != semantic_key {
                return Err(SessionRuntimeError::IdempotencyConflict);
            }
            return self.replay(&before, existing).map(SessionCreateStep::Done);
        }
        // A failed or otherwise retained lifecycle record still owns the
        // session name. Report that concrete conflict before asking the
        // reducer to reserve it, rather than collapsing the reducer's
        // `DuplicateSessionName` into a generic rejection.
        if before.sessions.iter().any(|session| session.name == name) {
            return Err(SessionRuntimeError::SessionWorkspaceExists(name));
        }
        let path = self
            .repo_root
            .join(STATE_DIR)
            .join(SESSIONS_DIR)
            .join(&name);
        // Do not reserve a lifecycle operation or invoke Git when a previous,
        // untracked session directory still occupies the destination.  The
        // durable snapshot cannot represent such a stale path, so the client
        // cannot catch it from its displayed session names alone.  Use
        // `symlink_metadata` so even a dangling symlink is treated as occupied.
        if fs::symlink_metadata(&path).is_ok() {
            return Err(SessionRuntimeError::SessionWorkspaceExists(name));
        }
        let operation = journal(operation_id, self.generation, semantic_key);
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
        let fence = fence(&reserved, session, operation_id).ok_or(SessionRuntimeError::Rejected)?;
        Ok(SessionCreateStep::Pending(SessionCreateInFlight {
            operation_id,
            fence,
            branch: format!("usagi/{name}"),
            name,
            workspace_root: self.repo_root.clone(),
            destination: path,
        }))
    }

    /// Builds the reserved session's worktree. Pure Git/filesystem work that
    /// runs with the shared session lock released.
    fn execute_create(
        git: &dyn GitRunner,
        in_flight: &SessionCreateInFlight,
    ) -> anyhow::Result<()> {
        build_session_tree(
            git,
            &in_flight.workspace_root,
            &in_flight.destination,
            &in_flight.branch,
        )
    }

    /// Records the durable outcome of a create whose worktree build already ran.
    /// Runs under the shared session lock.
    fn finish_create(
        &mut self,
        in_flight: SessionCreateInFlight,
        result: anyhow::Result<()>,
    ) -> Result<SessionReply, SessionRuntimeError> {
        let SessionCreateInFlight {
            operation_id,
            fence,
            name,
            ..
        } = in_flight;
        match result {
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
                    body: snapshot(&completed, self.root_worktree_id),
                })
            }
            Err(error) => {
                let error = error.to_string();
                let branch_exists = error.contains("branch") && error.contains("already exists");
                let workspace_exists = !branch_exists && error.contains("already exists");
                let detail = worktree_failure_detail(&error);
                let failure = if branch_exists {
                    SessionRuntimeError::SessionBranchExists(name.clone())
                } else if workspace_exists {
                    SessionRuntimeError::SessionWorkspaceExists(name.clone())
                } else {
                    SessionRuntimeError::SessionWorkspaceCreationFailed {
                        name: name.clone(),
                        detail,
                    }
                };
                let _ = self.store.apply(
                    self.generation,
                    LifecycleEvent::Failed {
                        fence,
                        failure: Failure {
                            stage: FailureStage::Create,
                            summary: failure.safe_message(),
                        },
                    },
                    Utc::now(),
                );
                Err(failure)
            }
        }
    }

    /// Removes a session synchronously: admit, tear down, finalize, all on this
    /// thread. The IPC path uses [`perform_remove`] plus the teardown worker
    /// instead, so no client connection waits for the worktree effect.
    fn remove(
        &mut self,
        operation_id: &str,
        payload: &Value,
    ) -> Result<SessionReply, SessionRuntimeError> {
        match self.begin_remove(operation_id, payload)? {
            SessionRemoveStep::Settled(reply) => Ok(reply),
            SessionRemoveStep::Accepted { pending, .. } => {
                let outcome =
                    remove_session_tree(self.git.as_ref(), &pending.session_root, pending.force)
                        .map_err(|error| error.to_string());
                self.finish_teardown(&pending, outcome)
            }
        }
    }

    /// Validates the request and marks the session `Deleting` with a durable
    /// delete plan. Runs under the shared session lock and performs no worktree
    /// effect, so the caller can answer as soon as it returns.
    fn begin_remove(
        &mut self,
        operation_id: &str,
        payload: &Value,
    ) -> Result<SessionRemoveStep, SessionRuntimeError> {
        let name = session_name(payload)?;
        let force = force(payload)?;
        let operation_id =
            OperationId::parse(operation_id).map_err(|_| SessionRuntimeError::InvalidOperation)?;
        let before = self.state()?;
        let semantic_key = semantic_key(SessionAction::Remove, &name);
        if let Some(existing) = before
            .operations
            .iter()
            .find(|op| op.operation_id == operation_id)
        {
            if existing.semantic_key != semantic_key {
                return Err(SessionRuntimeError::IdempotencyConflict);
            }
            return self
                .replay(&before, existing)
                .map(SessionRemoveStep::Settled);
        }
        let session = before
            .sessions
            .iter()
            .find(|session| session.name == name)
            .ok_or(SessionRuntimeError::UnknownSession)?;
        // A removal already in flight owns the worktree effect. Reporting its
        // operation instead of admitting a second one is what keeps a repeated
        // request (an impatient client, a retry with a fresh operation ID) from
        // running the teardown twice.
        if let Some(in_progress) = session.operation_id.filter(|_| {
            session.lifecycle == usagi_core::domain::session_lifecycle::SessionLifecycle::Deleting
        }) {
            return Ok(SessionRemoveStep::Settled(SessionReply {
                operation_id: in_progress.to_string(),
                revision: before.state_revision,
                body: snapshot(&before, self.root_worktree_id),
            }));
        }
        let session_id = session.session_id;
        let operation = journal(operation_id, self.generation, semantic_key);
        let removing = self
            .store
            .apply(
                self.generation,
                LifecycleEvent::BeginRemove {
                    session_id,
                    operation,
                    delete_plan: DeletePlan {
                        targets: vec![name.clone()],
                        force,
                    },
                },
                Utc::now(),
            )
            .map_err(|_| SessionRuntimeError::Rejected)?;
        // The teardown carries the stable identity, not a fence captured here:
        // the completion fence is recomputed when the worker finalizes, because
        // this revision is routinely stale by then (see `finish_teardown`).
        Ok(SessionRemoveStep::Accepted {
            reply: SessionReply {
                operation_id: operation_id.to_string(),
                revision: removing.state_revision,
                body: snapshot(&removing, self.root_worktree_id),
            },
            pending: PendingTeardown {
                session_id,
                operation_id,
                session_root: self.session_root(&name),
                name,
                force,
            },
        })
    }

    /// Every unfinished teardown, derived from durable state: a `Deleting`
    /// record that carries both its admitting operation and its delete plan.
    ///
    /// This derivation is the whole queue. A daemon that died mid-teardown
    /// resumes from it on the next start, and there is no separate file that
    /// could disagree with the lifecycle state.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable lifecycle state cannot be read.
    pub fn pending_teardowns(&self) -> Result<Vec<PendingTeardown>, SessionRuntimeError> {
        let state = self.state()?;
        Ok(state
            .sessions
            .iter()
            .filter(|session| {
                session.lifecycle
                    == usagi_core::domain::session_lifecycle::SessionLifecycle::Deleting
            })
            .filter_map(|session| {
                let plan = session.delete_plan.as_ref()?;
                Some(PendingTeardown {
                    session_id: session.session_id,
                    operation_id: session.operation_id?,
                    session_root: self.session_root(&session.name),
                    name: session.name.clone(),
                    force: plan.force,
                })
            })
            .collect())
    }

    /// Records the durable outcome of a teardown whose worktree effect already
    /// ran. Runs under the shared session lock, and only briefly.
    ///
    /// The completion fence is recomputed from the state observed here rather
    /// than captured at admission: the teardown runs concurrently with other
    /// lifecycle work, so the revision it was admitted at is routinely stale by
    /// the time it finishes. Identity is still fenced by the session
    /// incarnation, its attempt, and the admitting operation, so a record that a
    /// later attempt replaced is never completed by an older teardown.
    ///
    /// # Errors
    ///
    /// Returns [`SessionRuntimeError::DurableFailure`] carrying the safe failure
    /// summary when the teardown failed, or [`SessionRuntimeError::Storage`]
    /// when the outcome cannot be persisted.
    pub fn finish_teardown(
        &mut self,
        pending: &PendingTeardown,
        outcome: Result<(), String>,
    ) -> Result<SessionReply, SessionRuntimeError> {
        let state = self.state()?;
        let Some(fence) = state
            .sessions
            .iter()
            .find(|session| {
                session.session_id == pending.session_id
                    && session.operation_id == Some(pending.operation_id)
                    && session.lifecycle
                        == usagi_core::domain::session_lifecycle::SessionLifecycle::Deleting
            })
            .and_then(|session| fence(&state, session, pending.operation_id))
        else {
            // The teardown is no longer the record's live operation: a restart
            // already finalized it, or the record moved on. Report the current
            // durable truth instead of writing a stale outcome.
            return Ok(SessionReply {
                operation_id: pending.operation_id.to_string(),
                revision: state.state_revision,
                body: snapshot(&state, self.root_worktree_id),
            });
        };
        match outcome {
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
                    operation_id: pending.operation_id.to_string(),
                    revision: completed.state_revision,
                    body: snapshot(&completed, self.root_worktree_id),
                })
            }
            Err(error) => {
                // Keep the actionable reason: without it a `Failed` row only
                // says the removal failed, and the operator cannot tell a busy
                // worktree from a permission problem without the daemon log.
                let failure = SessionRuntimeError::DurableFailure(format!(
                    "could not remove the session worktree \"{}\": {}",
                    pending.name,
                    worktree_failure_detail(&error)
                ));
                let _ = self.store.apply(
                    self.generation,
                    LifecycleEvent::Failed {
                        fence,
                        failure: Failure {
                            stage: FailureStage::Delete,
                            summary: failure.safe_message(),
                        },
                    },
                    Utc::now(),
                );
                Err(failure)
            }
        }
    }

    fn session_root(&self, name: &str) -> PathBuf {
        self.repo_root.join(STATE_DIR).join(SESSIONS_DIR).join(name)
    }

    fn replay(
        &self,
        state: &WorkspaceLifecycleState,
        operation: &OperationJournal,
    ) -> Result<SessionReply, SessionRuntimeError> {
        if operation.status != OperationStatus::Succeeded {
            let summary = state
                .sessions
                .iter()
                .find(|session| session.operation_id == Some(operation.operation_id))
                .and_then(|session| session.failure.as_ref())
                .map_or_else(
                    || "session operation did not complete; explicit recovery required".into(),
                    |failure| failure.summary.clone(),
                );
            return Err(SessionRuntimeError::DurableFailure(summary));
        }
        Ok(SessionReply {
            operation_id: operation.operation_id.to_string(),
            revision: operation.progress_revision,
            body: snapshot(state, self.root_worktree_id),
        })
    }

    fn state(&self) -> Result<WorkspaceLifecycleState, SessionRuntimeError> {
        self.store
            .load()
            .map_err(|_| SessionRuntimeError::Storage)?
            .ok_or(SessionRuntimeError::Storage)
    }

    /// Plans or commits an operator-requested recovery.  Unlike startup
    /// adoption, this can extend an existing v2 state, but only after every
    /// legacy record and every collision has been checked.
    fn recover_legacy(
        &mut self,
        operation_id: &str,
        payload: &Value,
    ) -> Result<SessionReply, SessionRuntimeError> {
        OperationId::parse(operation_id).map_err(|_| SessionRuntimeError::InvalidOperation)?;
        let apply = match payload.get("apply") {
            Some(value) => value.as_bool().ok_or(SessionRuntimeError::InvalidRequest)?,
            None => false,
        };
        let state = self.state()?;
        let candidates = validated_legacy_sessions(&self.repo_root, &state, self.git.as_ref())?;
        let names = candidates
            .iter()
            .map(|record| record.name.clone())
            .collect::<Vec<_>>();
        if !apply {
            return Ok(SessionReply {
                operation_id: operation_id.to_owned(),
                revision: state.state_revision,
                body: json!({
                    "mode": "dry_run",
                    "revision": state.state_revision,
                    "candidates": names,
                    "would_adopt": candidates.len(),
                }),
            });
        }
        let mut recovered = state.clone();
        let now = Utc::now();
        recovered
            .sessions
            .extend(candidates.into_iter().map(|record| {
                usagi_core::domain::session_lifecycle::ManagedSession::adopt_available(
                    record.name,
                    record.created_at,
                )
            }));
        // This is a daemon-owned durable mutation despite having no reducer
        // event.  A new revision fences a concurrent lifecycle command.
        recovered.state_revision += 1;
        recovered.updated_at = now;
        self.store
            .replace_if_revision(state.state_revision, &recovered)
            .map_err(|_| SessionRuntimeError::Storage)?;
        Ok(SessionReply {
            operation_id: operation_id.to_owned(),
            revision: recovered.state_revision,
            body: json!({
                "mode": "applied",
                "revision": recovered.state_revision,
                "adopted": recovered.sessions.iter().filter(|session| names.contains(&session.name)).map(|session| json!({
                    "name": session.name,
                    "session_id": session.session_id,
                    "worktree_id": session.worktree_id,
                })).collect::<Vec<_>>(),
                "sessions": snapshot(&recovered, self.root_worktree_id)["sessions"].clone(),
                "workspace_id": recovered.workspace_id,
            }),
        })
    }

    /// Reconciles work an earlier daemon left unfinished.
    ///
    /// An interrupted create cannot be resumed: its worktree effect is not
    /// reversible and its completion cannot be proven, so it becomes a safe
    /// failure awaiting explicit recovery. An interrupted **delete** is
    /// different — the teardown is idempotent (a missing tree counts as
    /// removed) and its delete plan is durable — so it is left `Deleting` and
    /// resumed by the teardown worker, which derives it from exactly that
    /// state. Failing it here instead is what used to leave a half-removed
    /// worktree behind a record that kept owning the session name.
    fn reconcile(&mut self) -> Result<(), SessionRuntimeError> {
        let state = self.state()?;
        for session in state.sessions.into_iter().filter(|session| {
            matches!(
                session.lifecycle,
                usagi_core::domain::session_lifecycle::SessionLifecycle::Creating
                    | usagi_core::domain::session_lifecycle::SessionLifecycle::Initializing
            )
        }) {
            let Some(operation_id) = session.operation_id else {
                continue;
            };
            self.store
                .apply(
                    self.generation,
                    LifecycleEvent::ReconcileInterrupted {
                        session_id: session.session_id,
                        operation_id,
                        stage: FailureStage::Create,
                    },
                    Utc::now(),
                )
                .map_err(|_| SessionRuntimeError::Storage)?;
        }
        Ok(())
    }
}

/// Adopt repository-local records only while creating the first shared daemon
/// state.  We validate the complete legacy set before writing `sessions.json`;
/// a malformed, duplicate, missing, or differently-bound record leaves no
/// partial v2 state for a later start to guess from.
fn adopt_legacy_workspace_sessions(
    repository_root: &Path,
    git: &dyn GitRunner,
) -> Result<Option<WorkspaceLifecycleState>, SessionRuntimeError> {
    let sessions = validated_legacy_sessions_without_v2(repository_root, git)
        .map_err(|_| SessionRuntimeError::Storage)?;
    if sessions.is_empty() {
        return Ok(None);
    }
    let mut adopted = WorkspaceLifecycleState::new(WorkspaceId::new(), Utc::now());
    for record in sessions {
        adopted.sessions.push(
            usagi_core::domain::session_lifecycle::ManagedSession::adopt_available(
                record.name,
                record.created_at,
            ),
        );
    }
    Ok(Some(adopted))
}

/// Reads and validates the complete legacy set.  The returned records are
/// deliberately only used to mint lifecycle identities; UI metadata stays in
/// `state.json` and is never rewritten by recovery.
fn validated_legacy_sessions(
    repository_root: &Path,
    v2: &WorkspaceLifecycleState,
    git: &dyn GitRunner,
) -> Result<Vec<usagi_core::domain::session::SessionRecord>, SessionRuntimeError> {
    let records = validated_legacy_sessions_without_v2(repository_root, git)?;
    if records.iter().any(|record| {
        v2.sessions
            .iter()
            .any(|session| session.name == record.name)
    }) {
        return Err(SessionRuntimeError::Rejected);
    }
    Ok(records)
}

fn validated_legacy_sessions_without_v2(
    repository_root: &Path,
    git: &dyn GitRunner,
) -> Result<Vec<usagi_core::domain::session::SessionRecord>, SessionRuntimeError> {
    let Some(legacy) = WorkspaceStateStore::new(repository_root)
        .load()
        .map_err(|_| SessionRuntimeError::Storage)?
    else {
        return Ok(vec![]);
    };
    if legacy.sessions.is_empty() {
        return Ok(vec![]);
    }
    let expected_parent = repository_root.join(STATE_DIR).join(SESSIONS_DIR);
    let worktrees =
        list_worktrees(git, repository_root).map_err(|_| SessionRuntimeError::Storage)?;
    let mut names = std::collections::BTreeSet::new();
    let mut records = Vec::with_capacity(legacy.sessions.len());
    for record in legacy.sessions {
        let expected = expected_parent.join(&record.name);
        let expected_branch = format!("usagi/{}", record.name);
        if !valid_legacy_name(&record.name)
            || !names.insert(record.name.clone())
            || !is_linked_worktree(&expected)
            || canonical_path(&record.root) != canonical_path(&expected)
            || !worktrees.iter().any(|worktree| {
                canonical_path(&worktree.path) == canonical_path(&expected)
                    && worktree.branch.as_deref() == Some(expected_branch.as_str())
            })
        {
            return Err(SessionRuntimeError::Rejected);
        }
        records.push(record);
    }
    Ok(records)
}

fn valid_legacy_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn canonical_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Mirror the v1 session layout: a repository at the workspace root becomes a
/// worktree at the session root; otherwise every repository found below the
/// workspace is checked out at the matching relative path and plain entries are
/// copied. Usagi metadata and Git internals never enter the mirror.
fn build_session_tree(
    git: &dyn GitRunner,
    workspace_root: &Path,
    destination: &Path,
    branch: &str,
) -> anyhow::Result<()> {
    if is_repo_root(workspace_root) {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        return add_worktree(git, workspace_root, destination, branch, None);
    }
    fs::create_dir_all(destination)?;
    mirror_directory(git, workspace_root, destination, branch)
}

fn mirror_directory(
    git: &dyn GitRunner,
    source: &Path,
    destination: &Path,
    branch: &str,
) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        if skipped_entry(&name) {
            continue;
        }
        let source = entry.path();
        let target = destination.join(&name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            // A `.git` file denotes an existing linked worktree. It is neither
            // a source repository nor a directory to recurse into.
            if is_linked_worktree(&source) {
                continue;
            }
            if is_repo_root(&source) {
                add_worktree(git, &source, &target, branch, None)?;
            } else {
                fs::create_dir_all(&target)?;
                mirror_directory(git, &source, &target, branch)?;
            }
        } else {
            fs::copy(source, target)?;
        }
    }
    Ok(())
}

/// Remove every linked worktree in a mirrored session before removing copied
/// directories and files. Children are removed first so Git never sees a
/// parent directory that still contains a registered nested worktree.
fn remove_session_tree(
    git: &dyn GitRunner,
    session_root: &Path,
    force: bool,
) -> anyhow::Result<()> {
    let mut worktrees = Vec::new();
    collect_session_worktrees(session_root, &mut worktrees)?;
    worktrees.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for worktree in worktrees {
        remove_worktree(git, &worktree, &worktree, force)?;
    }
    match fs::remove_dir_all(session_root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn collect_session_worktrees(directory: &Path, worktrees: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    if is_linked_worktree(directory) {
        worktrees.push(directory.into());
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            collect_session_worktrees(&entry.path(), worktrees)?;
        }
    }
    Ok(())
}

fn is_repo_root(path: &Path) -> bool {
    path.join(".git").exists()
}

fn is_linked_worktree(path: &Path) -> bool {
    path.join(".git").is_file()
}

fn skipped_entry(name: &OsStr) -> bool {
    name == OsStr::new(".git") || name == OsStr::new(STATE_DIR)
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

/// Parse the optional destructive-removal flag without coercing malformed JSON.
/// The request schema exposes it as a boolean, so accepting another type here
/// would make a caller believe a dirty worktree was force-removed when it was not.
fn force(payload: &Value) -> Result<bool, SessionRuntimeError> {
    match payload.get("force") {
        Some(value) => value.as_bool().ok_or(SessionRuntimeError::InvalidRequest),
        None => Ok(false),
    }
}

/// Keep the actionable Git reason on one bounded display line. Session names
/// are validated before Git is invoked, and the command has no user-supplied
/// argv or environment, so this only carries the worktree command's own
/// diagnostic into the safe UI notice.
fn worktree_failure_detail(error: &str) -> String {
    let detail = error
        .strip_prefix("git worktree add failed:")
        .unwrap_or(error)
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Git rejected workspace creation")
        .trim();
    let detail = detail
        .chars()
        .filter(|ch| !ch.is_control())
        .take(160)
        .collect::<String>();
    if detail.is_empty() {
        "Git rejected workspace creation".into()
    } else {
        detail
    }
}

fn journal(
    operation_id: OperationId,
    generation: DaemonGeneration,
    semantic_key: String,
) -> OperationJournal {
    OperationJournal {
        operation_id,
        owner_daemon_generation: generation,
        status: OperationStatus::Accepted,
        execution_attempt: 1,
        progress_revision: 0,
        semantic_key,
    }
}

fn semantic_key(action: SessionAction, name: &str) -> String {
    format!("{action:?}:{name}").to_ascii_lowercase()
}

/// The completion fence for one session operation, taken from the journal entry
/// rather than from this daemon's own generation: a teardown resumed after a
/// restart must complete the operation its predecessor journaled.
fn fence(
    state: &WorkspaceLifecycleState,
    session: &usagi_core::domain::session_lifecycle::ManagedSession,
    operation_id: OperationId,
) -> Option<CompletionFence> {
    let operation = state
        .operations
        .iter()
        .find(|operation| operation.operation_id == operation_id)?;
    Some(CompletionFence {
        workspace_id: state.workspace_id,
        session_id: Some(session.session_id),
        operation_id,
        owner_daemon_generation: operation.owner_daemon_generation,
        execution_attempt: operation.execution_attempt,
        lifecycle_attempt: session.attempt,
        expected_revision: state.state_revision,
    })
}

fn snapshot(state: &WorkspaceLifecycleState, root_worktree_id: WorktreeId) -> Value {
    // Project every durable session record, not only `Available` ones. A failed
    // create is durable so a crashed daemon can reconcile and replay it safely,
    // and it keeps owning the session name — so hiding it from the list left the
    // name blocked with no way for a client to see or remove it. Each row
    // carries its `lifecycle` (and `failure` when present), so clients derive
    // per-row capabilities (a `Failed` row is not usable but is removable) from
    // the lifecycle without widening the wire surface. Scope resolution stays
    // `Available`-only (see `resolve_scope`), so listing a session never makes an
    // unusable one attachable.
    json!({
        "workspace_id": state.workspace_id,
        "root_worktree_id": root_worktree_id,
        "revision": state.state_revision,
        "sessions": state.sessions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::session_teardown::drain_pending_teardowns;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use usagi_core::domain::note::Scratchpad;
    use usagi_core::domain::session_lifecycle::{ManagedSession, SessionLifecycle};

    struct FakeGit(bool);
    impl FakeGit {
        fn ok() -> Self {
            Self(true)
        }
        fn fail() -> Self {
            Self(false)
        }
    }

    struct BranchExistsGit;
    impl GitRunner for BranchExistsGit {
        fn run(&self, _: &Path, _: &[&str]) -> anyhow::Result<GitOutput> {
            Ok(GitOutput {
                success: false,
                stdout: String::new(),
                stderr: "fatal: a branch named 'usagi/one' already exists".into(),
            })
        }
    }

    struct WorkspaceExistsGit;
    impl GitRunner for WorkspaceExistsGit {
        fn run(&self, _: &Path, _: &[&str]) -> anyhow::Result<GitOutput> {
            Ok(GitOutput {
                success: false,
                stdout: String::new(),
                stderr: "fatal: '/repo/.usagi/sessions/one' already exists".into(),
            })
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

    struct WorktreeListingGit {
        porcelain: String,
    }
    impl GitRunner for WorktreeListingGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            assert_eq!(args, ["worktree", "list", "--porcelain"]);
            Ok(GitOutput {
                success: true,
                stdout: self.porcelain.clone(),
                stderr: String::new(),
            })
        }
    }

    struct CountingGit {
        calls: Arc<AtomicUsize>,
    }

    struct OutcomeGit {
        succeeds: bool,
        calls: Arc<AtomicUsize>,
    }

    type GitCall = (PathBuf, Vec<String>);
    type RecordingCalls = Arc<Mutex<Vec<GitCall>>>;

    struct RecordingGit {
        calls: RecordingCalls,
    }
    impl RecordingGit {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }
    impl GitRunner for RecordingGit {
        fn run(&self, repo: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            self.calls.lock().unwrap().push((
                repo.into(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            ));
            Ok(GitOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }
    impl GitRunner for CountingGit {
        fn run(&self, _: &Path, _: &[&str]) -> anyhow::Result<GitOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(GitOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }
    impl GitRunner for OutcomeGit {
        fn run(&self, _: &Path, _: &[&str]) -> anyhow::Result<GitOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(GitOutput {
                success: self.succeeds,
                stdout: String::new(),
                stderr: "injected effect failure".into(),
            })
        }
    }
    /// A teardown that always refuses, standing in for a worktree Git will not
    /// remove (dirty, busy, or permission-denied).
    struct FailingTeardown;
    impl TeardownEffect for FailingTeardown {
        fn tear_down(&self, _: &PendingTeardown) -> Result<(), String> {
            Err("fatal: 'one' contains modified or untracked files".into())
        }
    }

    fn runtime(git: FakeGit) -> (TempDir, SessionRuntime) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let runtime = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            git,
        )
        .unwrap();
        (tmp, runtime)
    }
    fn operation() -> String {
        OperationId::new().to_string()
    }

    fn legacy_record(name: &str, root: PathBuf) -> usagi_core::domain::session::SessionRecord {
        usagi_core::domain::session::SessionRecord {
            name: name.into(),
            display_name: Some("preserved label".into()),
            origin: usagi_core::domain::session::SessionOrigin::Mcp,
            started_from: Some("parent".into()),
            root,
            created_at: Utc::now(),
            last_active: None,
            notes: Scratchpad::default(),
            prs: Vec::new(),
        }
    }

    #[test]
    fn adopts_valid_legacy_sessions_once_and_preserves_stable_ids_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = tmp.path().join("repository");
        let worktree = repository.join(STATE_DIR).join(SESSIONS_DIR).join("legacy");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir(repository.join(".git")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: /safe/worktree").unwrap();
        WorkspaceStateStore::new(&repository)
            .save(&usagi_core::domain::workspace_state::WorkspaceState {
                sessions: vec![legacy_record("legacy", worktree.clone())],
                root_notes: Scratchpad::default(),
                updated_at: Utc::now(),
            })
            .unwrap();
        let porcelain = format!(
            "worktree {}\nHEAD abc\nbranch refs/heads/usagi/legacy\n\n",
            worktree.display()
        );
        let state_dir = tmp.path().join("daemon");
        let first = SessionRuntime::open(
            repository.clone(),
            &state_dir,
            DaemonGeneration::new(),
            WorktreeListingGit { porcelain },
        )
        .unwrap();
        let session = first.state().unwrap().sessions[0].clone();
        assert_eq!(session.lifecycle, SessionLifecycle::Available);
        assert_eq!(first.snapshot().unwrap()["sessions"][0]["name"], "legacy");
        drop(first);

        let restarted = SessionRuntime::open(
            tmp.path().join("wrong-candidate"),
            &state_dir,
            DaemonGeneration::new(),
            FakeGit::ok(),
        )
        .unwrap();
        let restored = restarted.state().unwrap().sessions[0].clone();
        assert_eq!(restored.session_id, session.session_id);
        assert_eq!(restored.worktree_id, session.worktree_id);
    }

    #[test]
    fn refuses_invalid_legacy_records_without_creating_shared_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = tmp.path().join("repository");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir(repository.join(".git")).unwrap();
        WorkspaceStateStore::new(&repository)
            .save(&usagi_core::domain::workspace_state::WorkspaceState {
                sessions: vec![legacy_record("missing", repository.join("elsewhere"))],
                root_notes: Scratchpad::default(),
                updated_at: Utc::now(),
            })
            .unwrap();
        let state_dir = tmp.path().join("daemon");

        let result = SessionRuntime::open(
            repository,
            &state_dir,
            DaemonGeneration::new(),
            FakeGit::ok(),
        );
        assert!(matches!(result, Err(SessionRuntimeError::Storage)));
        assert!(!state_dir.join("sessions.json").exists());
    }

    #[test]
    fn explicit_recovery_dry_runs_then_atomically_adopts_without_replacing_failed_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = tmp.path().join("repository");
        let worktree = repository.join(STATE_DIR).join(SESSIONS_DIR).join("legacy");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir(repository.join(".git")).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: /safe/worktree").unwrap();
        WorkspaceStateStore::new(&repository)
            .save(&usagi_core::domain::workspace_state::WorkspaceState {
                sessions: vec![legacy_record("legacy", worktree.clone())],
                root_notes: Scratchpad::default(),
                updated_at: Utc::now(),
            })
            .unwrap();
        let state_dir = tmp.path().join("daemon");
        let mut existing = WorkspaceLifecycleState::new(WorkspaceId::new(), Utc::now());
        let mut failed =
            ManagedSession::new_creating("test-1".into(), OperationId::new(), Utc::now());
        failed.lifecycle = SessionLifecycle::Failed;
        existing.sessions.push(failed.clone());
        DaemonLifecycleStore::new(&state_dir)
            .initialize(&existing, &repository)
            .unwrap();
        let porcelain = format!(
            "worktree {}\nHEAD abc\nbranch refs/heads/usagi/legacy\n\n",
            worktree.display()
        );
        let mut runtime = SessionRuntime::open(
            repository.clone(),
            &state_dir,
            DaemonGeneration::new(),
            WorktreeListingGit { porcelain },
        )
        .unwrap();
        let before = std::fs::read(state_dir.join("sessions.json")).unwrap();
        let preview = runtime
            .handle(SessionAction::RecoverLegacy, &operation(), &json!({}))
            .unwrap();
        assert_eq!(preview.body["mode"], "dry_run");
        assert_eq!(
            std::fs::read(state_dir.join("sessions.json")).unwrap(),
            before
        );

        let applied = runtime
            .handle(
                SessionAction::RecoverLegacy,
                &operation(),
                &json!({"apply": true}),
            )
            .unwrap();
        assert_eq!(applied.body["mode"], "applied");
        let state = runtime.state().unwrap();
        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.sessions[0], failed);
        let adopted = state.sessions[1].clone();
        drop(runtime);
        let restarted = SessionRuntime::open(
            tmp.path().join("wrong-root"),
            &state_dir,
            DaemonGeneration::new(),
            FakeGit::ok(),
        )
        .unwrap();
        assert_eq!(
            restarted.state().unwrap().sessions[1].session_id,
            adopted.session_id
        );
        assert_eq!(
            restarted.state().unwrap().sessions[1].worktree_id,
            adopted.worktree_id
        );
    }

    #[test]
    fn explicit_recovery_rejects_a_same_name_without_writing_v2_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = tmp.path().join("repository");
        let worktree = repository.join(STATE_DIR).join(SESSIONS_DIR).join("same");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir(repository.join(".git")).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: /safe/worktree").unwrap();
        WorkspaceStateStore::new(&repository)
            .save(&usagi_core::domain::workspace_state::WorkspaceState {
                sessions: vec![legacy_record("same", worktree.clone())],
                root_notes: Scratchpad::default(),
                updated_at: Utc::now(),
            })
            .unwrap();
        let state_dir = tmp.path().join("daemon");
        let mut existing = WorkspaceLifecycleState::new(WorkspaceId::new(), Utc::now());
        existing
            .sessions
            .push(ManagedSession::adopt_available("same".into(), Utc::now()));
        DaemonLifecycleStore::new(&state_dir)
            .initialize(&existing, &repository)
            .unwrap();
        let porcelain = format!(
            "worktree {}\nHEAD abc\nbranch refs/heads/usagi/same\n\n",
            worktree.display()
        );
        let mut runtime = SessionRuntime::open(
            repository,
            &state_dir,
            DaemonGeneration::new(),
            WorktreeListingGit { porcelain },
        )
        .unwrap();
        let before = std::fs::read(state_dir.join("sessions.json")).unwrap();
        assert!(matches!(
            runtime.handle(
                SessionAction::RecoverLegacy,
                &operation(),
                &json!({"apply": true})
            ),
            Err(SessionRuntimeError::Rejected)
        ));
        assert_eq!(
            std::fs::read(state_dir.join("sessions.json")).unwrap(),
            before
        );
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
    fn creates_a_single_character_session_name() {
        let (_tmp, mut runtime) = runtime(FakeGit::ok());

        let created = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"a"}))
            .unwrap();

        assert_eq!(created.body["sessions"][0]["name"], "a");
        assert_eq!(created.body["sessions"][0]["lifecycle"], "available");
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
            SessionRuntimeError::SessionWorkspaceCreationFailed {
                name: "one".into(),
                detail: "no".into(),
            }
        );
        assert_eq!(
            runtime
                .handle(SessionAction::Setup, &operation(), &json!({}))
                .unwrap_err(),
            SessionRuntimeError::InvalidRequest
        );
    }

    #[test]
    fn ambiguous_issue_error_preserves_number_and_sorted_exact_paths() {
        let files = vec![
            PathBuf::from("/repo/.usagi/issues/001-first.md"),
            PathBuf::from("/repo/.usagi/issues/001-second.md"),
        ];
        let error = SessionRuntimeError::AmbiguousIssue(AmbiguousIssueNumber {
            number: 1,
            files: files.clone(),
        });

        assert_eq!(
            error,
            SessionRuntimeError::AmbiguousIssue(AmbiguousIssueNumber { number: 1, files })
        );
        let message = error.safe_message();
        assert!(message.contains("issue #1 is ambiguous"));
        assert!(message.contains("/repo/.usagi/issues/001-first.md"));
        assert!(message.contains("/repo/.usagi/issues/001-second.md"));
    }

    #[test]
    fn reports_a_reusable_session_name_when_its_branch_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let mut runtime = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            BranchExistsGit,
        )
        .unwrap();

        let error = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap_err();

        assert_eq!(
            error,
            SessionRuntimeError::SessionBranchExists("one".into())
        );
        assert_eq!(
            error.safe_message(),
            "cannot create session \"one\": branch usagi/one already exists; choose a different name or remove the stale branch"
        );
        // The failed reservation is projected so the client can see and remove
        // the name it still owns.
        let listed = runtime.snapshot().unwrap();
        let sessions = listed["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["name"], "one");
        assert_eq!(sessions[0]["lifecycle"], "failed");
        assert_eq!(sessions[0]["failure"]["summary"], error.safe_message());
        assert_eq!(
            runtime.state().unwrap().sessions[0]
                .failure
                .as_ref()
                .unwrap()
                .summary,
            error.safe_message()
        );
    }

    #[test]
    fn reports_a_reusable_session_name_when_its_workspace_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let mut runtime = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            WorkspaceExistsGit,
        )
        .unwrap();

        let error = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap_err();

        assert_eq!(
            error,
            SessionRuntimeError::SessionWorkspaceExists("one".into())
        );
        assert_eq!(
            error.safe_message(),
            "cannot create session \"one\": workspace already exists; choose a different name or remove the stale workspace"
        );
        // The failed reservation is projected so the client can see and remove
        // the name it still owns.
        let listed = runtime.snapshot().unwrap();
        let sessions = listed["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["name"], "one");
        assert_eq!(sessions[0]["lifecycle"], "failed");
        assert_eq!(sessions[0]["failure"]["summary"], error.safe_message());
        assert_eq!(
            runtime.state().unwrap().sessions[0]
                .failure
                .as_ref()
                .unwrap()
                .summary,
            error.safe_message()
        );
    }

    #[test]
    fn lists_a_failed_session_but_refuses_to_resolve_it_then_removes_it_to_free_the_name() {
        let (_tmp, mut runtime) = runtime(FakeGit::ok());
        runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        // Force the created session into the Failed lifecycle a real create
        // failure would leave behind: the name stays owned, but the row is not a
        // usable checkout.
        let mut state = runtime.state().unwrap();
        let revision = state.state_revision;
        state.sessions[0].lifecycle = SessionLifecycle::Failed;
        state.sessions[0].failure = Some(Failure {
            stage: FailureStage::Create,
            summary: "create failed".into(),
        });
        runtime.store.replace_if_revision(revision, &state).unwrap();

        // The failed row is projected with its lifecycle and failure summary.
        let listed = runtime.snapshot().unwrap();
        assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(listed["sessions"][0]["name"], "one");
        assert_eq!(listed["sessions"][0]["lifecycle"], "failed");
        assert_eq!(listed["sessions"][0]["failure"]["summary"], "create failed");

        // Scope resolution still refuses it: attach targets only Available.
        let workspace = state.workspace_id;
        let session_id = state.sessions[0].session_id;
        let worktree_id = state.sessions[0].worktree_id;
        assert_eq!(
            runtime
                .resolve_scope(workspace, session_id, worktree_id)
                .unwrap_err(),
            SessionRuntimeError::ScopeUnavailable
        );

        // Removing the failed row succeeds even though no worktree was created,
        // frees the name, and a same-name create then succeeds.
        let removed = runtime
            .handle(SessionAction::Remove, &operation(), &json!({"name":"one"}))
            .unwrap();
        assert!(removed.body["sessions"].as_array().unwrap().is_empty());
        let recreated = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        assert_eq!(recreated.body["sessions"][0]["name"], "one");
        assert_eq!(recreated.body["sessions"][0]["lifecycle"], "available");
    }

    #[test]
    fn rejects_a_stale_workspace_before_reserving_or_invoking_git() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let stale = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("test");
        std::fs::create_dir_all(&stale).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut runtime = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            CountingGit {
                calls: Arc::clone(&calls),
            },
        )
        .unwrap();

        let error = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"test"}))
            .unwrap_err();

        assert_eq!(
            error,
            SessionRuntimeError::SessionWorkspaceExists("test".into())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(runtime.state().unwrap().sessions.is_empty());
        assert!(runtime.state().unwrap().operations.is_empty());
    }

    #[test]
    fn remove_forwards_force_to_the_worktree_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            RecordingGit {
                calls: Arc::clone(&calls),
            },
        )
        .unwrap();
        runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        std::fs::write(
            tmp.path().join(".usagi/sessions/one/.git"),
            "gitdir: /fixture",
        )
        .unwrap();
        runtime
            .handle(
                SessionAction::Remove,
                &operation(),
                &json!({"name":"one", "force":true}),
            )
            .unwrap();

        assert_eq!(
            calls.lock().unwrap()[0].1[..3],
            ["worktree", "remove", "--force"]
        );
    }

    #[test]
    fn remove_rejects_a_non_boolean_force_flag() {
        let (_tmp, mut runtime) = runtime(FakeGit::ok());
        runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();

        assert_eq!(
            runtime
                .handle(
                    SessionAction::Remove,
                    &operation(),
                    &json!({"name":"one", "force":"yes"}),
                )
                .unwrap_err(),
            SessionRuntimeError::InvalidRequest
        );
    }

    #[test]
    fn reports_an_existing_lifecycle_session_before_reserving_another_create() {
        let (_tmp, mut runtime) = runtime(FakeGit::ok());
        runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();

        let error = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap_err();

        assert_eq!(
            error,
            SessionRuntimeError::SessionWorkspaceExists("one".into())
        );
    }

    #[test]
    fn worktree_failure_detail_is_single_line_bounded_and_nonempty() {
        assert_eq!(
            worktree_failure_detail("git worktree add failed: fatal: first\nsecond"),
            "fatal: first"
        );
        assert_eq!(
            worktree_failure_detail("\n\t"),
            "Git rejected workspace creation"
        );
        assert_eq!(
            worktree_failure_detail(&"x".repeat(200)).chars().count(),
            160
        );
    }
    #[test]
    fn operation_id_is_idempotent_only_for_the_same_semantic_request() {
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
        assert_eq!(
            runtime
                .handle(SessionAction::Create, &operation, &json!({"name":"two"}))
                .unwrap_err(),
            SessionRuntimeError::IdempotencyConflict
        );
    }

    #[test]
    fn replaying_a_successful_create_after_daemon_restart_does_not_create_twice() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let operation = operation();
        let first_calls = Arc::new(AtomicUsize::new(0));
        let mut first = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            CountingGit {
                calls: Arc::clone(&first_calls),
            },
        )
        .unwrap();

        let created = first
            .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
            .unwrap();
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        drop(first);

        let replay_calls = Arc::new(AtomicUsize::new(0));
        let mut restarted = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            CountingGit {
                calls: Arc::clone(&replay_calls),
            },
        )
        .unwrap();
        let replayed = restarted
            .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
            .unwrap();

        assert_eq!(replayed.body, created.body);
        assert_eq!(replay_calls.load(Ordering::SeqCst), 0);
        assert_eq!(replayed.body["sessions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn failed_create_replays_the_same_failure_without_repeating_the_effect() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let state_dir = tmp.path().join("daemon");
        let operation = operation();
        let first_calls = Arc::new(AtomicUsize::new(0));
        let mut first = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &state_dir,
            DaemonGeneration::new(),
            OutcomeGit {
                succeeds: false,
                calls: Arc::clone(&first_calls),
            },
        )
        .unwrap();

        let failed = first
            .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
            .unwrap_err();
        let replayed = first
            .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
            .unwrap_err();
        assert_eq!(replayed.safe_message(), failed.safe_message());
        assert_eq!(
            first
                .handle(SessionAction::Create, &operation, &json!({"name":"two"}))
                .unwrap_err(),
            SessionRuntimeError::IdempotencyConflict
        );
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            first.state().unwrap().operations[0].status,
            OperationStatus::Failed
        );
        drop(first);

        let restart_calls = Arc::new(AtomicUsize::new(0));
        let mut restarted = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &state_dir,
            DaemonGeneration::new(),
            OutcomeGit {
                succeeds: true,
                calls: Arc::clone(&restart_calls),
            },
        )
        .unwrap();
        let reopened = restarted
            .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
            .unwrap_err();
        assert_eq!(reopened.safe_message(), failed.safe_message());
        assert_eq!(restart_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn failed_remove_replays_the_same_failure_without_repeating_the_effect() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("daemon");
        let create_calls = Arc::new(AtomicUsize::new(0));
        let mut creator = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &state_dir,
            DaemonGeneration::new(),
            OutcomeGit {
                succeeds: true,
                calls: Arc::clone(&create_calls),
            },
        )
        .unwrap();
        creator
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
        std::fs::create_dir_all(&session_root).unwrap();
        std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
        drop(creator);

        let operation = operation();
        let first_calls = Arc::new(AtomicUsize::new(0));
        let mut first = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &state_dir,
            DaemonGeneration::new(),
            OutcomeGit {
                succeeds: false,
                calls: Arc::clone(&first_calls),
            },
        )
        .unwrap();
        let failed = first
            .handle(SessionAction::Remove, &operation, &json!({"name":"one"}))
            .unwrap_err();
        let replayed = first
            .handle(SessionAction::Remove, &operation, &json!({"name":"one"}))
            .unwrap_err();
        assert_eq!(replayed.safe_message(), failed.safe_message());
        assert_eq!(
            first
                .handle(SessionAction::Remove, &operation, &json!({"name":"two"}))
                .unwrap_err(),
            SessionRuntimeError::IdempotencyConflict
        );
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            first.state().unwrap().operations[1].status,
            OperationStatus::Failed
        );
        drop(first);

        let restart_calls = Arc::new(AtomicUsize::new(0));
        let mut restarted = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &state_dir,
            DaemonGeneration::new(),
            OutcomeGit {
                succeeds: true,
                calls: Arc::clone(&restart_calls),
            },
        )
        .unwrap();
        let reopened = restarted
            .handle(SessionAction::Remove, &operation, &json!({"name":"one"}))
            .unwrap_err();
        assert_eq!(reopened.safe_message(), failed.safe_message());
        assert_eq!(restart_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn resolver_requires_complete_available_scope_and_restart_reconciles_interrupted_work() {
        let (tmp, mut runtime) = runtime(FakeGit::ok());
        let created = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        let session = created.body["sessions"][0].clone();
        let workspace = serde_json::from_value(created.body["workspace_id"].clone()).unwrap();
        let session_id = serde_json::from_value(session["session_id"].clone()).unwrap();
        let worktree_id = serde_json::from_value(session["worktree_id"].clone()).unwrap();
        assert!(
            runtime
                .resolve_scope(workspace, session_id, worktree_id)
                .is_ok()
        );
        assert_eq!(
            runtime
                .resolve_scope(WorkspaceId::new(), session_id, worktree_id)
                .unwrap_err(),
            SessionRuntimeError::ScopeUnavailable
        );

        let operation = OperationId::new();
        runtime
            .store
            .apply(
                runtime.generation,
                LifecycleEvent::ReserveCreate {
                    name: "interrupted".into(),
                    operation: journal(
                        operation,
                        runtime.generation,
                        semantic_key(SessionAction::Create, "interrupted"),
                    ),
                },
                Utc::now(),
            )
            .unwrap();
        let mut restarted = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            FakeGit::ok(),
        )
        .unwrap();
        // Both the completed `Available` session and the interrupted work,
        // reconciled to `Failed` on restart, are projected to the client.
        let snapshot = restarted.snapshot().unwrap();
        let listed = snapshot["sessions"].as_array().unwrap();
        assert_eq!(listed.len(), 2);
        let interrupted = listed
            .iter()
            .find(|session| session["name"] == "interrupted")
            .unwrap();
        assert_eq!(interrupted["lifecycle"], "failed");
        assert_eq!(
            interrupted["failure"]["summary"],
            "interrupted; explicit recovery required"
        );
        assert_eq!(
            restarted.state().unwrap().sessions[1]
                .failure
                .as_ref()
                .unwrap()
                .summary,
            "interrupted; explicit recovery required"
        );
        assert_eq!(
            restarted.state().unwrap().operations[1].status,
            OperationStatus::Failed
        );
        assert_eq!(
            restarted
                .handle(
                    SessionAction::Create,
                    &operation.to_string(),
                    &json!({"name":"interrupted"})
                )
                .unwrap_err()
                .safe_message(),
            "interrupted; explicit recovery required"
        );
    }

    #[test]
    fn open_repairs_a_legacy_failed_session_and_replays_failure() {
        let (tmp, mut runtime) = runtime(FakeGit::ok());
        let operation = operation();
        runtime
            .handle(SessionAction::Create, &operation, &json!({"name":"legacy"}))
            .unwrap();
        let mut legacy = runtime.state().unwrap();
        let revision = legacy.state_revision;
        legacy.sessions[0].lifecycle = SessionLifecycle::Failed;
        legacy.sessions[0].failure = Some(Failure {
            stage: FailureStage::Create,
            summary: "legacy create failed".into(),
        });
        legacy.sessions[0].operation_id = None;
        legacy.operations[0].status = OperationStatus::Succeeded;
        runtime
            .store
            .replace_if_revision(revision, &legacy)
            .unwrap();
        drop(runtime);

        let mut reopened = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            FakeGit::ok(),
        )
        .unwrap();
        let repaired = reopened.state().unwrap();
        assert_eq!(repaired.operations[0].status, OperationStatus::Failed);
        assert_eq!(
            repaired.sessions[0].operation_id,
            Some(repaired.operations[0].operation_id)
        );
        assert_eq!(
            reopened
                .handle(SessionAction::Create, &operation, &json!({"name":"legacy"}))
                .unwrap_err()
                .safe_message(),
            "legacy create failed"
        );
    }

    #[test]
    fn restart_from_another_directory_uses_the_shared_session_state_and_root() {
        let tmp = tempfile::tempdir().unwrap();
        let original_root = tmp.path().join("original");
        let another_directory = tmp.path().join("another");
        let state_dir = tmp.path().join("shared-daemon");
        std::fs::create_dir_all(&original_root).unwrap();
        std::fs::create_dir_all(&another_directory).unwrap();

        let mut first = SessionRuntime::open(
            original_root.clone(),
            &state_dir,
            DaemonGeneration::new(),
            FakeGit::ok(),
        )
        .unwrap();
        first
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        drop(first);

        let restarted = SessionRuntime::open(
            another_directory,
            &state_dir,
            DaemonGeneration::new(),
            FakeGit::ok(),
        )
        .unwrap();

        assert_eq!(restarted.repository_root(), original_root);
        assert_eq!(
            restarted.snapshot().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn first_shared_start_migrates_legacy_repository_session_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = tmp.path().join("repository");
        let legacy_dir = project_data_dir(&repository);
        let state_dir = tmp.path().join("shared-daemon");
        let mut legacy = WorkspaceLifecycleState::new(WorkspaceId::new(), Utc::now());
        legacy.sessions.push(ManagedSession::new_creating(
            "legacy".into(),
            OperationId::new(),
            Utc::now(),
        ));
        json_file::write_atomic(
            &legacy_dir,
            &legacy_dir.join("lifecycle-state.json"),
            &legacy,
        )
        .unwrap();

        let migrated = SessionRuntime::open(
            repository.clone(),
            &state_dir,
            DaemonGeneration::new(),
            FakeGit::ok(),
        )
        .unwrap();

        assert_eq!(migrated.repo_root, repository);
        assert_eq!(migrated.state().unwrap().sessions[0].name, "legacy");
        // The interrupted `Creating` reservation is reconciled to `Failed` on
        // open and then projected, so the migrated name is visible and removable.
        let listed = migrated.snapshot().unwrap();
        let sessions = listed["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["name"], "legacy");
        assert_eq!(sessions[0]["lifecycle"], "failed");
        assert!(state_dir.join("sessions.json").is_file());
        assert!(!legacy_dir.join("lifecycle-state.json").exists());
    }

    #[test]
    fn create_recursively_mirrors_plain_entries_and_adds_a_worktree_per_nested_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let destination = workspace.join(".usagi/sessions/feature");
        let nested_repo = workspace.join("services/api");
        std::fs::create_dir_all(nested_repo.join(".git")).unwrap();
        std::fs::create_dir_all(workspace.join("docs")).unwrap();
        std::fs::write(workspace.join("README.md"), "read me").unwrap();
        std::fs::write(workspace.join("docs/guide.md"), "guide").unwrap();

        let git = RecordingGit::new();
        build_session_tree(&git, &workspace, &destination, "usagi/feature").unwrap();

        assert_eq!(
            std::fs::read_to_string(destination.join("README.md")).unwrap(),
            "read me"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("docs/guide.md")).unwrap(),
            "guide"
        );
        assert_eq!(
            git.calls.lock().unwrap().as_slice(),
            &[(
                nested_repo,
                vec![
                    "worktree".into(),
                    "add".into(),
                    "-b".into(),
                    "usagi/feature".into(),
                    "--".into(),
                    destination
                        .join("services/api")
                        .to_string_lossy()
                        .into_owned(),
                ],
            )]
        );
    }

    #[test]
    fn opening_a_repository_migrates_v1_usagi_ignore_rules() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(
            tmp.path().join(".gitignore"),
            "target\n.usagi/*\n!.usagi/issues/\n.usagi/issues/index.json\n",
        )
        .unwrap();

        let _runtime = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            FakeGit::ok(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".usagi/.gitignore")).unwrap(),
            usagi_core::infrastructure::gitignore::USAGI_GITIGNORE
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
            "target\n"
        );
    }

    /// A Git runner that records whether the shared session lock was free at the
    /// moment Git ran. `perform_create`/`perform_remove` must release the lock
    /// before invoking Git, so a same-thread `try_lock` succeeds here.
    struct LockProbeGit {
        runtime: std::sync::Weak<Mutex<SessionRuntime>>,
        observed_unlocked: Arc<std::sync::atomic::AtomicBool>,
    }
    impl GitRunner for LockProbeGit {
        fn run(&self, _: &Path, _: &[&str]) -> anyhow::Result<GitOutput> {
            if let Some(runtime) = self.runtime.upgrade()
                && runtime.try_lock().is_ok()
            {
                self.observed_unlocked
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(GitOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    /// A Git runner that poisons the shared session lock while it runs, so the
    /// `finish_*` re-lock inside `perform_*` observes a poisoned lock.
    struct PoisoningGit {
        runtime: std::sync::Weak<Mutex<SessionRuntime>>,
    }
    impl GitRunner for PoisoningGit {
        fn run(&self, _: &Path, _: &[&str]) -> anyhow::Result<GitOutput> {
            if let Some(runtime) = self.runtime.upgrade() {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _guard = runtime.lock().unwrap();
                    panic!("poison the session lock mid Git effect");
                }));
            }
            Ok(GitOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn poison_lock(runtime: &Arc<Mutex<SessionRuntime>>) {
        let clone = Arc::clone(runtime);
        let _ = std::thread::spawn(move || {
            let _guard = clone.lock().unwrap();
            panic!("poison the session lock before begin");
        })
        .join();
    }

    #[test]
    fn perform_create_releases_the_session_lock_while_building_the_worktree() {
        let (_tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        let observed_unlocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let git = LockProbeGit {
            runtime: Arc::downgrade(&runtime),
            observed_unlocked: Arc::clone(&observed_unlocked),
        };
        let reply = perform_create(&runtime, &git, &operation(), &json!({"name":"one"})).unwrap();
        assert!(
            observed_unlocked.load(std::sync::atomic::Ordering::SeqCst),
            "the session lock must be released while `git worktree add` runs"
        );
        assert_eq!(reply.body["sessions"][0]["name"], "one");
    }

    #[test]
    fn perform_remove_accepts_without_touching_the_worktree_and_hands_it_to_the_worker() {
        let (tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        // Materialize a linked worktree so a teardown would have to invoke Git.
        let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
        std::fs::create_dir_all(&session_root).unwrap();
        std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
        let signal = TeardownSignal::new();

        let reply =
            perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();

        // The reply is the acceptance: the row is `deleting`, the tree is still
        // there, and the worker was woken.
        assert_eq!(reply.body["sessions"][0]["name"], "one");
        assert_eq!(reply.body["sessions"][0]["lifecycle"], "deleting");
        assert!(session_root.exists());
        assert!(signal.wait(std::time::Duration::from_millis(1)));

        // The pending teardown is derived from that durable state alone.
        let pending = runtime.lock().unwrap().pending_teardowns().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "one");
        assert_eq!(pending[0].session_root, session_root);

        // Draining it removes the tree and retires the record.
        let calls = Arc::new(AtomicUsize::new(0));
        let reports = drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&runtime)),
            &WorktreeTeardown::new(CountingGit {
                calls: Arc::clone(&calls),
            }),
            &|| false,
        );
        assert_eq!(reports[0].effect_error, None);
        assert_eq!(reports[0].finalize_error, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!session_root.exists());
        assert!(
            runtime.lock().unwrap().snapshot().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            runtime
                .lock()
                .unwrap()
                .pending_teardowns()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_second_remove_of_a_deleting_session_returns_the_operation_already_in_flight() {
        let (_tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        let signal = TeardownSignal::new();
        let accepted =
            perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();

        // A retry with a fresh operation ID must not admit a second teardown.
        let again =
            perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();

        assert_eq!(again.operation_id, accepted.operation_id);
        assert_eq!(again.revision, accepted.revision);
        assert_eq!(
            runtime.lock().unwrap().pending_teardowns().unwrap().len(),
            1
        );
        assert_eq!(runtime.lock().unwrap().state().unwrap().operations.len(), 2);
    }

    #[test]
    fn a_teardown_failure_records_the_reason_on_a_failed_row_and_frees_the_name_after_removal() {
        let (_tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        let signal = TeardownSignal::new();
        perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();

        let reports = drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&runtime)),
            &FailingTeardown,
            &|| false,
        );

        assert!(reports[0].effect_error.is_some());
        assert_eq!(reports[0].finalize_error, None);
        let listed = runtime.lock().unwrap().snapshot().unwrap();
        assert_eq!(listed["sessions"][0]["lifecycle"], "failed");
        let summary = listed["sessions"][0]["failure"]["summary"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(summary.contains("could not remove the session worktree \"one\""));
        assert!(summary.contains("contains modified or untracked files"));
        // The failed row still owns the name; removing it frees it again.
        assert!(
            runtime
                .lock()
                .unwrap()
                .pending_teardowns()
                .unwrap()
                .is_empty()
        );
        perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();
        drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&runtime)),
            &WorktreeTeardown::new(FakeGit::ok()),
            &|| false,
        );
        assert!(
            perform_create(
                &runtime,
                &FakeGit::ok(),
                &operation(),
                &json!({"name":"one"})
            )
            .is_ok()
        );
    }

    #[test]
    fn an_interrupted_teardown_is_resumed_after_restart_instead_of_failing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let state_dir = tmp.path().join("daemon");
        let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
        let first = Arc::new(Mutex::new(
            SessionRuntime::open(
                tmp.path().to_path_buf(),
                &state_dir,
                DaemonGeneration::new(),
                FakeGit::ok(),
            )
            .unwrap(),
        ));
        perform_create(&first, &FakeGit::ok(), &operation(), &json!({"name":"one"})).unwrap();
        std::fs::create_dir_all(&session_root).unwrap();
        std::fs::write(session_root.join("file"), "work").unwrap();
        let signal = TeardownSignal::new();
        perform_remove(&first, &signal, &operation(), &json!({"name":"one"})).unwrap();
        // The daemon dies here: the record stays `Deleting` with its durable
        // delete plan, and the worktree is still on disk.
        drop(first);

        let restarted = Arc::new(Mutex::new(
            SessionRuntime::open(
                tmp.path().to_path_buf(),
                &state_dir,
                DaemonGeneration::new(),
                FakeGit::ok(),
            )
            .unwrap(),
        ));

        // Restart does not fail the interrupted delete: it is pending again.
        let listed = restarted.lock().unwrap().snapshot().unwrap();
        assert_eq!(listed["sessions"][0]["lifecycle"], "deleting");
        let pending = restarted.lock().unwrap().pending_teardowns().unwrap();
        assert_eq!(pending.len(), 1);

        // The new daemon's worker completes the operation the previous
        // generation journaled.
        drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&restarted)),
            &WorktreeTeardown::new(FakeGit::ok()),
            &|| false,
        );
        assert!(!session_root.exists());
        assert!(
            restarted.lock().unwrap().snapshot().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn finalizing_a_teardown_twice_reports_durable_truth_without_a_stale_write() {
        let (_tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        let signal = TeardownSignal::new();
        perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();
        let pending = runtime.lock().unwrap().pending_teardowns().unwrap()[0].clone();

        let completed = runtime
            .lock()
            .unwrap()
            .finish_teardown(&pending, Ok(()))
            .unwrap();
        // The record is gone, so a duplicate finalization is a no-op that
        // reports the current state rather than writing a stale outcome.
        let repeated = runtime
            .lock()
            .unwrap()
            .finish_teardown(&pending, Ok(()))
            .unwrap();

        assert_eq!(repeated.revision, completed.revision);
        assert!(repeated.body["sessions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn the_shared_teardown_journal_reports_an_unavailable_session_owner() {
        let (_tmp, rt) = runtime(FakeGit::ok());
        let shared = Arc::new(Mutex::new(rt));
        perform_create(
            &shared,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        let signal = TeardownSignal::new();
        perform_remove(&shared, &signal, &operation(), &json!({"name":"one"})).unwrap();
        let journal = SharedSessionTeardown::new(Arc::clone(&shared));
        let pending = journal.pending();
        assert_eq!(pending.len(), 1);
        poison_lock(&shared);

        // A poisoned session lock leaves the record `Deleting`, so the next
        // drain retries it instead of losing the teardown.
        assert_eq!(
            journal.finish(&pending[0], Ok(())),
            Err("session lifecycle owner is unavailable".into())
        );
        assert!(journal.pending().is_empty(), "the poisoned read is empty");
    }

    #[test]
    fn perform_create_and_remove_replay_a_completed_operation_under_the_lock() {
        let (_tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        let create_op = operation();
        let created =
            perform_create(&runtime, &FakeGit::ok(), &create_op, &json!({"name":"one"})).unwrap();
        let replayed_create =
            perform_create(&runtime, &FakeGit::ok(), &create_op, &json!({"name":"one"})).unwrap();
        assert_eq!(created.body, replayed_create.body);

        let signal = TeardownSignal::new();
        let remove_op = operation();
        perform_remove(&runtime, &signal, &remove_op, &json!({"name":"one"})).unwrap();
        drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&runtime)),
            &WorktreeTeardown::new(FakeGit::ok()),
            &|| false,
        );
        let replayed_remove =
            perform_remove(&runtime, &signal, &remove_op, &json!({"name":"one"})).unwrap();
        assert!(replayed_remove.body.get("sessions").is_some());
    }

    #[test]
    fn perform_create_maps_a_poisoned_session_lock_to_storage() {
        // Poisoned before begin: the first re-lock fails.
        let (_tmp, rt) = runtime(FakeGit::ok());
        let shared = Arc::new(Mutex::new(rt));
        poison_lock(&shared);
        assert!(matches!(
            perform_create(
                &shared,
                &FakeGit::ok(),
                &operation(),
                &json!({"name":"one"})
            ),
            Err(SessionRuntimeError::Storage)
        ));

        // Poisoned mid-build: begin succeeds, the finish re-lock fails.
        let (_tmp, rt) = runtime(FakeGit::ok());
        let shared = Arc::new(Mutex::new(rt));
        let git = PoisoningGit {
            runtime: Arc::downgrade(&shared),
        };
        assert!(matches!(
            perform_create(&shared, &git, &operation(), &json!({"name":"one"})),
            Err(SessionRuntimeError::Storage)
        ));
    }

    #[test]
    fn perform_remove_maps_a_poisoned_session_lock_to_storage() {
        // The admission is the only lock this path takes, so a poisoned session
        // lock is the one way it fails without reaching the reducer.
        let (_tmp, rt) = runtime(FakeGit::ok());
        let shared = Arc::new(Mutex::new(rt));
        poison_lock(&shared);

        assert!(matches!(
            perform_remove(
                &shared,
                &TeardownSignal::new(),
                &operation(),
                &json!({"name":"one"})
            ),
            Err(SessionRuntimeError::Storage)
        ));
    }
}
