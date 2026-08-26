//! Durable daemon-owned managed-session runtime.
//!
//! The reducer and store in `usagi-core` deliberately have no process or git
//! dependency. This usecase durably reserves an operation before invoking
//! injected Git and filesystem ports, then applies the exact completion fence
//! captured from the reservation.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::{Value, json};
use usagi_core::domain::id::{
    CompletionFence, DaemonGeneration, OperationId, SessionId, WorkspaceId, WorktreeId,
};
use usagi_core::domain::role::{EffectiveRoleCatalog, RoleId, RoleScope};
use usagi_core::domain::session_lifecycle::{
    DeletePlan, Failure, FailureStage, LifecycleEvent, OperationJournal, OperationStatus,
    WorkspaceLifecycleState, validate_session_name,
};
use usagi_core::infrastructure::git::{GitRunner, delete_branch};
use usagi_core::infrastructure::gitignore::migrate_usagi_ignore_rules;
use usagi_core::infrastructure::ipc::ErrorCode;
use usagi_core::infrastructure::paths::{SESSIONS_DIR, STATE_DIR, project_data_dir};
use usagi_core::infrastructure::persistence::json_file;
use usagi_core::infrastructure::store::issue::AmbiguousIssueNumber;
use usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore;
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
    RoleConflict(Option<RoleId>, Option<RoleId>),
    InvalidRole(String),
    SessionBranchExists(String),
    SessionWorkspaceExists(String),
    SessionWorkspaceCreationFailed { name: String, detail: String },
    DurableFailure(String),
    UnknownSession,
    ScopeUnavailable,
    AgentFailure { code: ErrorCode, message: String },
    Delivery(String),
    AmbiguousIssue(AmbiguousIssueNumber),
    Delegation(DelegationFailure),
    Rejected,
    Storage,
}

/// The safe, structured outcome of a delegation whose dispatch did not succeed.
///
/// A delegation creates a session and then dispatches into it, so its failure is
/// never just a message: the caller has to know whether the session it asked for
/// exists, which run identity to reconcile it against, and whether the daemon
/// already rolled it back. A bare error would leave the caller unable to tell a
/// clean rejection from a worker that may be running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationFailure {
    pub code: ErrorCode,
    pub message: String,
    pub session_id: SessionId,
    pub run_operation_id: String,
    pub reconcile: DelegationReconcile,
}

/// What the daemon did with the session a failed delegation had already created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationReconcile {
    /// The dispatch definitively did not start, and the session is rolled back
    /// by a durable teardown the daemon resumes across a restart.
    Compensated,
    /// The rollback could not be recorded, so the session is still present and
    /// has to be removed explicitly.
    CompensationFailed,
    /// The spawn outcome is unknown. The session is deliberately kept: tearing
    /// it down could delete the worktree of a worker that is in fact running.
    Retained,
}

impl DelegationReconcile {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compensated => "compensated",
            Self::CompensationFailed => "compensation_failed",
            Self::Retained => "retained",
        }
    }

    /// Whether the delegation left durable state the caller still owns.
    #[must_use]
    pub const fn left_side_effect(self) -> bool {
        !matches!(self, Self::Compensated)
    }
}

impl DelegationFailure {
    /// The safe machine-readable identity a caller needs to reconcile this
    /// delegation. It carries identities and states only, never worker output.
    #[must_use]
    pub fn details(&self) -> Value {
        json!({
            "session_id": self.session_id,
            "run_operation_id": self.run_operation_id,
            "reconcile": self.reconcile.as_str(),
        })
    }
}

/// Why a session is being created, which is what the durable create journal
/// records.
///
/// A delegated create is one step of a composite operation whose dispatch may
/// still be missing, so a restart has to be able to tell it from a plain
/// `session_create` that is complete on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateOrigin {
    /// `session_create`: the create is the whole operation.
    Direct,
    /// `session_delegate_brief`: the create is a step whose dispatch follows.
    Delegated,
}

impl CreateOrigin {
    const fn semantic_action(self) -> SessionAction {
        match self {
            Self::Direct => SessionAction::Create,
            Self::Delegated => SessionAction::DelegateBrief,
        }
    }
}

/// Why a session is being removed, which decides how much of its create is
/// undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveKind {
    /// `session_remove`: the worktree goes and the branch stays, because the
    /// branch holds the session's work.
    Requested,
    /// The compensation of a delegated create whose dispatch never started. It
    /// undoes the create completely, branch included: nothing was ever committed
    /// on that branch, and leaving it would make a retry under the same session
    /// name fail with a branch conflict.
    Compensating,
}

/// A completed delegated create whose session is still available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedCreate {
    pub session_id: SessionId,
    pub name: String,
    pub operation_id: OperationId,
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
            Self::RoleConflict(existing, requested) => format!(
                "session role conflict: existing={}, requested={}",
                existing.as_ref().map_or("<legacy>", RoleId::as_str),
                requested.as_ref().map_or("<legacy>", RoleId::as_str)
            ),
            Self::SessionBranchExists(name) => format!(
                "cannot create session \"{name}\": branch usagi/{name} already exists; choose a different name or remove the stale branch"
            ),
            Self::SessionWorkspaceExists(name) => format!(
                "cannot create session \"{name}\": workspace already exists; choose a different name or remove the stale workspace"
            ),
            Self::SessionWorkspaceCreationFailed { name, detail } => {
                format!("cannot create session \"{name}\": {detail}")
            }
            Self::InvalidRole(message)
            | Self::DurableFailure(message)
            | Self::AgentFailure { message, .. }
            | Self::Delivery(message) => message.clone(),
            Self::AmbiguousIssue(error) => error.to_string(),
            Self::Delegation(failure) => failure.message.clone(),
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

/// Filesystem/worktree effects required by the session lifecycle usecase.
///
/// The production implementation lives in `infrastructure`; unit tests inject
/// deterministic fakes so validation, reconciliation, parsing, and error
/// mapping remain measurable without touching the host filesystem.
pub trait SessionWorktreeIo {
    fn remove_file_best_effort(&self, path: &Path);
    fn path_occupied(&self, path: &Path) -> bool;
    fn canonical_path(&self, path: &Path) -> Option<PathBuf>;
    fn is_repo_root(&self, path: &Path) -> bool;
    fn is_linked_worktree(&self, path: &Path) -> bool;
    /// Builds the complete session worktree layout.
    ///
    /// # Errors
    ///
    /// Returns an error when a Git or filesystem effect fails.
    fn build_session_tree(
        &self,
        git: &dyn GitRunner,
        workspace_root: &Path,
        destination: &Path,
        branch: &str,
    ) -> anyhow::Result<()>;
    /// Removes nested linked worktrees and the containing session tree.
    ///
    /// # Errors
    ///
    /// Returns an error when a Git or filesystem effect fails.
    fn remove_session_tree(
        &self,
        git: &dyn GitRunner,
        session_root: &Path,
        force: bool,
    ) -> anyhow::Result<()>;
}

/// One daemon process's session writer.  Callers serialize it across IPC
/// connections; the store also locks every reducer mutation for crash safety.
pub struct SessionRuntime {
    repo_root: PathBuf,
    data_home: PathBuf,
    root_worktree_id: WorktreeId,
    generation: DaemonGeneration,
    store: DaemonLifecycleStore,
    git: Box<dyn GitRunner + Send>,
    io: Arc<dyn SessionWorktreeIo + Send + Sync>,
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
    io: Arc<dyn SessionWorktreeIo + Send + Sync>,
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
    perform_create_from(runtime, git, CreateOrigin::Direct, operation_id, payload)
}

/// Creates a session as one step of a composite operation, recording the
/// delegated origin in the durable create journal.
///
/// The origin is what makes the composite operation recoverable: a daemon that
/// died between this create and its dispatch leaves a session no caller owns,
/// and only a journal that says "this create was delegated" lets the next start
/// tell it from a plain `session_create`.
///
/// # Errors
///
/// Returns a typed safe error when the request cannot be admitted or completed.
pub fn perform_delegated_create(
    runtime: &Mutex<SessionRuntime>,
    git: &dyn GitRunner,
    operation_id: &str,
    payload: &Value,
) -> Result<SessionReply, SessionRuntimeError> {
    perform_create_from(runtime, git, CreateOrigin::Delegated, operation_id, payload)
}

fn perform_create_from(
    runtime: &Mutex<SessionRuntime>,
    git: &dyn GitRunner,
    origin: CreateOrigin,
    operation_id: &str,
    payload: &Value,
) -> Result<SessionReply, SessionRuntimeError> {
    let step = runtime
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?
        .begin_create(origin, operation_id, payload)?;
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
    perform_remove_with_merged_head(runtime, teardown, operation_id, payload, None)
}

/// Admits a requested removal with an optional provider-verified merged PR head.
/// The durable teardown rechecks this OID against the branch after removing the
/// worktree, so commits added after the PR remain protected.
///
/// # Errors
///
/// Returns a typed safe error when the removal cannot be admitted or persisted.
pub fn perform_remove_with_merged_head(
    runtime: &Mutex<SessionRuntime>,
    teardown: &TeardownSignal,
    operation_id: &str,
    payload: &Value,
    merged_head_oid: Option<String>,
) -> Result<SessionReply, SessionRuntimeError> {
    perform_remove_as(
        runtime,
        teardown,
        RemoveKind::Requested,
        operation_id,
        payload,
        merged_head_oid,
    )
}

/// Undoes a delegated create completely: the worktree and the branch it made.
///
/// The removal is forced and deletes the branch. A requested removal also
/// deletes its branch, but uses Git's safe mode so unmerged work is preserved.
/// Compensation is safe because it is reached exclusively for a session whose
/// dispatch definitively never started, so nothing on the branch is anybody's
/// work. Undoing the branch too is what lets the caller retry the same session
/// name instead of hitting a branch conflict.
///
/// # Errors
///
/// Returns a typed safe error when the compensation cannot be admitted.
pub fn perform_compensating_remove(
    runtime: &Mutex<SessionRuntime>,
    teardown: &TeardownSignal,
    operation_id: &str,
    name: &str,
) -> Result<SessionReply, SessionRuntimeError> {
    perform_remove_as(
        runtime,
        teardown,
        RemoveKind::Compensating,
        operation_id,
        &json!({"name": name}),
        None,
    )
}

fn perform_remove_as(
    runtime: &Mutex<SessionRuntime>,
    teardown: &TeardownSignal,
    kind: RemoveKind,
    operation_id: &str,
    payload: &Value,
    merged_head_oid: Option<String>,
) -> Result<SessionReply, SessionRuntimeError> {
    let step = runtime
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?
        .begin_remove(kind, operation_id, payload, merged_head_oid)?;
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
pub struct WorktreeTeardown<G: GitRunner, I: SessionWorktreeIo> {
    git: G,
    io: I,
}

impl<G: GitRunner, I: SessionWorktreeIo> WorktreeTeardown<G, I> {
    #[must_use]
    pub const fn new(git: G, io: I) -> Self {
        Self { git, io }
    }
}

impl<G: GitRunner, I: SessionWorktreeIo> TeardownEffect for WorktreeTeardown<G, I> {
    fn tear_down(&self, teardown: &PendingTeardown) -> Result<(), String> {
        validate_teardown_target(&self.io, teardown)?;
        self.io
            .remove_session_tree(&self.git, &teardown.session_root, teardown.force)
            .map_err(|error| error.to_string())?;
        delete_teardown_branch(&self.git, teardown)
    }
}

/// Deletes a teardown branch outside the generic effect implementation so every
/// `WorktreeTeardown<G, I>` instantiation shares one coverage region.
fn delete_teardown_branch(git: &dyn GitRunner, teardown: &PendingTeardown) -> Result<(), String> {
    if !teardown.delete_branch {
        return Ok(());
    }
    // Only after the worktree is gone: git refuses to delete a branch that a
    // worktree still has checked out.
    let squash_merged = teardown.merged_head_oid.as_deref().is_some_and(|expected| {
        git.run(
            &teardown.repository_root,
            &["rev-parse", "--verify", &session_branch_ref(&teardown.name)],
        )
        .is_ok_and(|output| output.success && output.stdout.trim() == expected)
    });
    delete_branch(
        git,
        &teardown.repository_root,
        &session_branch(&teardown.name),
        teardown.force_delete_branch || squash_merged,
    )
    .map_err(|error| error.to_string())
}

impl SessionRuntime {
    /// Returns the repository root durably trusted by this daemon's session store.
    #[must_use]
    pub fn repository_root(&self) -> &Path {
        &self.repo_root
    }

    /// Loads the current effective role policy at an admission boundary.
    ///
    /// # Errors
    ///
    /// Returns an invalid-role error when either catalog layer cannot be parsed.
    pub fn effective_role_catalog(&self) -> Result<EffectiveRoleCatalog, SessionRuntimeError> {
        usagi_core::infrastructure::role_catalog::load_effective(&self.data_home, &self.repo_root)
            .map_err(|_| {
                SessionRuntimeError::InvalidRole("effective role catalog is invalid".into())
            })
    }

    /// Returns the durable workspace-root checkout identity. It is a real,
    /// persisted incarnation (never derived from a name or path), so a
    /// workspace-root terminal/agent is fenced exactly like a session one.
    #[must_use]
    pub fn root_worktree_id(&self) -> WorktreeId {
        self.root_worktree_id
    }

    /// Whether this workspace still has durable work only its owner can finish.
    ///
    /// A session mid-creation or mid-teardown, and an operation this daemon
    /// accepted but has not settled, both outlive the client that asked for
    /// them. Giving the workspace back while either is open would leave the work
    /// to a daemon that never accepted it.
    ///
    /// # Errors
    ///
    /// Returns [`SessionRuntimeError::Storage`] when the durable state cannot be
    /// read.
    pub fn has_unfinished_work(&self) -> Result<bool, SessionRuntimeError> {
        let state = self.state()?;
        Ok(state.sessions.iter().any(|session| {
            !matches!(
                session.lifecycle,
                usagi_core::domain::session_lifecycle::SessionLifecycle::Available
            )
        }) || state
            .operations
            .iter()
            .any(|operation| !operation.status.terminal()))
    }

    /// Returns the durable workspace identity this runtime bound.
    ///
    /// A daemon that owns several workspaces routes a fenced request to the
    /// runtime whose identity the request names, so the identity has to be
    /// readable without going through a scope resolution first.
    ///
    /// # Errors
    ///
    /// Returns [`SessionRuntimeError::Storage`] when the durable state cannot be
    /// read.
    pub fn workspace_id(&self) -> Result<WorkspaceId, SessionRuntimeError> {
        Ok(self.state()?.workspace_id)
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

    /// The workspace root a later [`Self::open`] on `state_dir` will bind.
    ///
    /// The daemon must fence the workspace it is about to own *before* it opens
    /// the runtime and publishes an endpoint, so the fence cannot read the root
    /// off an opened runtime. This applies the same rule `open` does — a durable
    /// `repository_root` wins over the startup candidate — so the fenced
    /// workspace and the owned workspace are always the same one.
    ///
    /// # Errors
    ///
    /// Returns [`SessionRuntimeError::Storage`] when the durable lifecycle state
    /// cannot be read.
    pub fn bound_workspace_root(
        state_dir: &Path,
        candidate_repo_root: PathBuf,
    ) -> Result<PathBuf, SessionRuntimeError> {
        Ok(DaemonLifecycleStore::new(state_dir)
            .load_with_workspace()
            .map_err(|_| SessionRuntimeError::Storage)?
            .map_or(candidate_repo_root, |(repository_root, _)| repository_root))
    }

    /// # Errors
    ///
    /// Returns an error when the lifecycle state cannot be loaded or initialized.
    ///
    /// The root this binds is the one [`Self::bound_workspace_root`] predicts for
    /// the same `state_dir`; a fixture test pins the two together.
    ///
    /// The data home is taken to be the parent of `state_dir`, which holds while
    /// the state directory is `<data-dir>/daemon`. A workspace state subtree sits
    /// deeper than that, so a daemon that owns several workspaces opens each one
    /// through [`Self::open_at`] with the data home spelled out.
    pub fn open<G: GitRunner + Send + 'static, I: SessionWorktreeIo + Send + Sync + 'static>(
        candidate_repo_root: PathBuf,
        state_dir: &Path,
        generation: DaemonGeneration,
        git: G,
        io: I,
    ) -> Result<Self, SessionRuntimeError> {
        let data_home = state_dir
            .parent()
            .ok_or(SessionRuntimeError::Storage)?
            .to_path_buf();
        Self::open_at(
            candidate_repo_root,
            state_dir,
            &data_home,
            generation,
            git,
            io,
        )
    }

    /// Open the runtime whose lifecycle document lives in `state_dir`, reading
    /// the role catalog and teardown guards from `data_home`.
    ///
    /// The two are separate because the workspace's state subtree is not a child
    /// of the data home: `<data-dir>/daemon/w/<digest>` holds the document while
    /// `<data-dir>` still holds the settings, catalogs, and stores every
    /// workspace shares.
    ///
    /// # Errors
    ///
    /// Returns an error when the lifecycle state cannot be loaded or initialized.
    pub fn open_at<G: GitRunner + Send + 'static, I: SessionWorktreeIo + Send + Sync + 'static>(
        candidate_repo_root: PathBuf,
        state_dir: &Path,
        data_home: &Path,
        generation: DaemonGeneration,
        git: G,
        io: I,
    ) -> Result<Self, SessionRuntimeError> {
        let store = DaemonLifecycleStore::new(state_dir);
        let data_home = data_home.to_path_buf();
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
            let state = json_file::read(&legacy_lifecycle)
                .map_err(|_| SessionRuntimeError::Storage)?
                .unwrap_or_else(|| WorkspaceLifecycleState::new(WorkspaceId::new(), Utc::now()));
            store
                .initialize(&state, &candidate_repo_root)
                .map_err(|_| SessionRuntimeError::Storage)?;
            // The migrated state is already durable in `sessions.json`; from now
            // on the `Some(..)` branch wins and the legacy file is never read
            // again. Removing it is best-effort cleanup, so a failure here must
            // not fail daemon startup over an otherwise-ignored stale file.
            io.remove_file_best_effort(&legacy_lifecycle);
            candidate_repo_root
        };
        let root_worktree_id = store
            .ensure_root_worktree_id()
            .map_err(|_| SessionRuntimeError::Storage)?;
        let mut runtime = Self {
            repo_root,
            data_home,
            root_worktree_id,
            generation,
            store,
            git: Box::new(git),
            io: Arc::new(io),
        };
        if runtime.io.is_repo_root(&runtime.repo_root) {
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
            SessionAction::List | SessionAction::Overview => {
                let state = self.state()?;
                Ok(SessionReply {
                    operation_id: operation_id.to_owned(),
                    revision: state.state_revision,
                    body: projected_snapshot(
                        &state,
                        self.root_worktree_id,
                        &self.data_home,
                        &self.repo_root,
                    ),
                })
            }
            SessionAction::Status => self.status(operation_id),
            SessionAction::Setup
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
        let catalog = usagi_core::infrastructure::role_catalog::load_effective(
            &self.data_home,
            &self.repo_root,
        )
        .ok();
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
                    "role_id": session.role_id,
                    "role_summary": session.role_id.as_ref().and_then(|id| catalog.as_ref()?.roles.get(id).map(|role| role.summary.clone())),
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
        for session in self.state()?.sessions {
            if session.name == name
                && session.lifecycle
                    == usagi_core::domain::session_lifecycle::SessionLifecycle::Available
            {
                return Ok(session.session_id);
            }
        }
        Err(SessionRuntimeError::UnknownSession)
    }

    /// Resolves the stable identity and current branch HEAD used to authorize a
    /// squash-merged removal. Failed rows remain resolvable so a retry can
    /// finish a teardown whose first safe branch deletion was refused.
    ///
    /// # Errors
    ///
    /// Returns a storage error when lifecycle state or Git cannot be read, or
    /// an unknown-session error when no durable row has that name.
    pub fn removal_identity(
        &self,
        name: &str,
    ) -> Result<(SessionId, Option<String>), SessionRuntimeError> {
        let session_id = self
            .state()?
            .sessions
            .into_iter()
            .find(|session| session.name == name)
            .map(|session| session.session_id)
            .ok_or(SessionRuntimeError::UnknownSession)?;
        let branch = session_branch_ref(name);
        let head = self
            .git
            .run(&self.repo_root, &["rev-parse", "--verify", &branch])
            .map_err(|_| SessionRuntimeError::Storage)?;
        Ok((
            session_id,
            head.success.then(|| head.stdout.trim().to_owned()),
        ))
    }

    /// Stable role assignment for a managed session incarnation.
    ///
    /// # Errors
    ///
    /// Returns a storage error when lifecycle state cannot be loaded, or
    /// [`SessionRuntimeError::UnknownSession`] when the incarnation is absent.
    pub fn session_role(
        &self,
        session_id: SessionId,
    ) -> Result<Option<RoleId>, SessionRuntimeError> {
        self.state()?
            .sessions
            .into_iter()
            .find(|session| session.session_id == session_id)
            .map(|session| session.role_id)
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
        for session in state.sessions {
            if session.session_id == session_id
                && session.lifecycle
                    == usagi_core::domain::session_lifecycle::SessionLifecycle::Available
            {
                return Ok(SessionScope {
                    workspace_id: state.workspace_id,
                    session_id,
                    worktree_id: session.worktree_id,
                    path: self
                        .repo_root
                        .join(STATE_DIR)
                        .join(SESSIONS_DIR)
                        .join(session.name),
                });
            }
        }
        Err(SessionRuntimeError::UnknownSession)
    }

    /// # Errors
    ///
    /// Returns an error when the durable lifecycle state cannot be read.
    pub fn snapshot(&self) -> Result<Value, SessionRuntimeError> {
        let state = self.state()?;
        Ok(projected_snapshot(
            &state,
            self.root_worktree_id,
            &self.data_home,
            &self.repo_root,
        ))
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
        match self.begin_create(CreateOrigin::Direct, operation_id, payload)? {
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
        origin: CreateOrigin,
        operation_id: &str,
        payload: &Value,
    ) -> Result<SessionCreateStep, SessionRuntimeError> {
        let name = session_name(payload)?;
        let requested_role = payload
            .get("role")
            .filter(|value| !value.is_null())
            .cloned()
            .map(serde_json::from_value::<RoleId>)
            .transpose()
            .map_err(|_| SessionRuntimeError::InvalidRequest)?;
        // Re-read both catalog layers at the daemon admission boundary. The
        // registered repository root, never the target session worktree, is
        // authoritative for workspace policy.
        let catalog = usagi_core::infrastructure::role_catalog::load_effective(
            &self.data_home,
            &self.repo_root,
        )
        .map_err(|_| {
            SessionRuntimeError::InvalidRole("effective role catalog is invalid".into())
        })?;
        let operation_id =
            OperationId::parse(operation_id).map_err(|_| SessionRuntimeError::InvalidOperation)?;
        let before = self.state()?;
        let existing_session = before.sessions.iter().find(|session| session.name == name);
        let role_id = if let Some(existing) = existing_session {
            if let Some(requested) = requested_role.as_ref() {
                catalog
                    .resolve(Some(requested), RoleScope::Session)
                    .map_err(|error| SessionRuntimeError::InvalidRole(error.to_string()))?
            } else if let Some(assigned) = existing.role_id.as_ref() {
                catalog
                    .resolve(Some(assigned), RoleScope::Session)
                    .map_err(|error| SessionRuntimeError::InvalidRole(error.to_string()))?
            } else {
                None
            }
        } else {
            catalog
                .resolve(requested_role.as_ref(), RoleScope::Session)
                .map_err(|error| SessionRuntimeError::InvalidRole(error.to_string()))?
        };
        let semantic_key = create_semantic_key(origin, &name, role_id.as_ref());
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
        if let Some(existing) = existing_session {
            if existing.role_id != role_id {
                return Err(SessionRuntimeError::RoleConflict(
                    existing.role_id.clone(),
                    role_id,
                ));
            }
            return Ok(SessionCreateStep::Done(SessionReply {
                operation_id: operation_id.to_string(),
                revision: before.state_revision,
                body: snapshot(&before, self.root_worktree_id),
            }));
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
        if self.io.path_occupied(&path) {
            return Err(SessionRuntimeError::SessionWorkspaceExists(name));
        }
        let operation = journal(operation_id, self.generation, semantic_key);
        let reserved = self
            .store
            .apply(
                self.generation,
                LifecycleEvent::ReserveCreate {
                    name: name.clone(),
                    role_id,
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
            branch: session_branch(&name),
            name,
            workspace_root: self.repo_root.clone(),
            destination: path,
            io: Arc::clone(&self.io),
        }))
    }

    /// Builds the reserved session's worktree. Pure Git/filesystem work that
    /// runs with the shared session lock released.
    fn execute_create(
        git: &dyn GitRunner,
        in_flight: &SessionCreateInFlight,
    ) -> anyhow::Result<()> {
        in_flight.io.build_session_tree(
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
        match self.begin_remove(RemoveKind::Requested, operation_id, payload, None)? {
            SessionRemoveStep::Settled(reply) => Ok(reply),
            SessionRemoveStep::Accepted { pending, .. } => {
                let outcome = match self.io.remove_session_tree(
                    self.git.as_ref(),
                    &pending.session_root,
                    pending.force,
                ) {
                    Ok(()) => {
                        // Every newly accepted removal carries branch deletion.
                        // Legacy branch-preserving plans can only be replayed as
                        // `Settled`, so they never reach this effect path.
                        delete_teardown_branch(self.git.as_ref(), &pending)
                            .map_err(anyhow::Error::msg)
                    }
                    Err(error) => Err(error),
                }
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
        kind: RemoveKind,
        operation_id: &str,
        payload: &Value,
        merged_head_oid: Option<String>,
    ) -> Result<SessionRemoveStep, SessionRuntimeError> {
        let name = session_name(payload)?;
        // A compensation is not a client request: it forces the removal and
        // branch deletion, and neither is negotiable through a payload. A
        // requested removal uses Git's safe `-d` mode unless the client pairs
        // `force_delete_branch` with `force`, which is what the TUI's forced
        // removals send.
        let compensating = kind == RemoveKind::Compensating;
        let force = compensating || force(payload)?;
        let requested_force_delete_branch = force_delete_branch(payload)?;
        if requested_force_delete_branch && !force {
            return Err(SessionRuntimeError::InvalidRequest);
        }
        let force_delete_branch = compensating || requested_force_delete_branch;
        let operation_id =
            OperationId::parse(operation_id).map_err(|_| SessionRuntimeError::InvalidOperation)?;
        let before = self.state()?;
        let semantic_key = remove_semantic_key(kind, &name, force, force_delete_branch);
        if let Some(existing) = before
            .operations
            .iter()
            .find(|op| op.operation_id == operation_id)
        {
            if !remove_operation_matches(
                &before,
                existing,
                kind,
                &name,
                force,
                force_delete_branch,
                &semantic_key,
            ) {
                return Err(SessionRuntimeError::IdempotencyConflict);
            }
            if existing.status == OperationStatus::Accepted {
                return Ok(SessionRemoveStep::Settled(SessionReply {
                    operation_id: existing.operation_id.to_string(),
                    revision: before.state_revision,
                    body: snapshot(&before, self.root_worktree_id),
                }));
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
        let delete_branch = true;
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
                        delete_branch,
                        force_delete_branch,
                        merged_head_oid: merged_head_oid.clone(),
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
                repository_root: self.repo_root.clone(),
                data_home: self.data_home.clone(),
                session_container: self.session_container(),
                session_root: self.session_root(&name),
                name,
                force,
                delete_branch,
                force_delete_branch,
                merged_head_oid,
            },
        })
    }

    /// Every available session whose current incarnation came from a delegated
    /// create.
    ///
    /// This is the recovery half of the delegation saga. The dispatch that such
    /// a create exists for lives in the dispatch store, so this side only
    /// reports the identities; the composition root asks the dispatch ledger
    /// whether each one's run ever became durable and compensates the ones with
    /// nothing behind them. A successful delegation stays in this list — its run
    /// is what makes it not an orphan, not its absence here.
    ///
    /// A completed create releases the session's operation identity (the record
    /// no longer has an operation in flight), so the link back to the journal is
    /// the session name. Only the *last* operation journaled for that name counts:
    /// a name that was delegated, compensated, and then created plainly belongs
    /// to the plain create, and reading the stale delegated entry would have this
    /// roll back a session the user asked for.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable lifecycle state cannot be read.
    pub fn delegated_sessions(&self) -> Result<Vec<DelegatedCreate>, SessionRuntimeError> {
        let state = self.state()?;
        Ok(state
            .sessions
            .iter()
            .filter(|session| {
                session.lifecycle
                    == usagi_core::domain::session_lifecycle::SessionLifecycle::Available
            })
            .filter_map(|session| {
                let delegated = semantic_key(SessionAction::DelegateBrief, &session.name);
                let owning = [
                    delegated.clone(),
                    semantic_key(SessionAction::Create, &session.name),
                    semantic_key(SessionAction::Remove, &session.name),
                ];
                let operation = state.operations.iter().rev().find(|operation| {
                    owning
                        .iter()
                        .any(|key| names_session_operation(&operation.semantic_key, key))
                })?;
                (names_session_operation(&operation.semantic_key, &delegated)
                    && operation.status == OperationStatus::Succeeded)
                    .then(|| DelegatedCreate {
                        session_id: session.session_id,
                        name: session.name.clone(),
                        operation_id: operation.operation_id,
                    })
            })
            .collect())
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
                    repository_root: self.repo_root.clone(),
                    data_home: self.data_home.clone(),
                    session_container: self.session_container(),
                    session_root: self.session_root(&session.name),
                    name: session.name.clone(),
                    force: plan.force,
                    delete_branch: plan.delete_branch,
                    force_delete_branch: plan.force_delete_branch,
                    merged_head_oid: plan.merged_head_oid.clone(),
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
        self.session_container().join(name)
    }

    fn session_container(&self) -> PathBuf {
        self.repo_root.join(STATE_DIR).join(SESSIONS_DIR)
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

fn session_name(payload: &Value) -> Result<String, SessionRuntimeError> {
    let name = payload
        .get("name")
        .or_else(|| payload.get("label"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| validate_session_name(name).is_ok())
        .ok_or(SessionRuntimeError::InvalidRequest)?;
    Ok(name.to_owned())
}

fn validate_teardown_target(
    io: &dyn SessionWorktreeIo,
    teardown: &PendingTeardown,
) -> Result<(), String> {
    validate_session_name(&teardown.name)
        .map_err(|_| "refusing teardown outside the managed session container".to_owned())?;
    let expected_container = teardown.repository_root.join(STATE_DIR).join(SESSIONS_DIR);
    let expected_target = expected_container.join(&teardown.name);
    if teardown.session_container != expected_container
        || teardown.session_root != expected_target
        || teardown.session_root.parent() != Some(teardown.session_container.as_path())
    {
        return Err("refusing teardown outside the managed session container".into());
    }

    let canonical_repository = io
        .canonical_path(&teardown.repository_root)
        .ok_or_else(|| "could not resolve the managed repository root".to_owned())?;
    let canonical_data_home = io
        .canonical_path(&teardown.data_home)
        .ok_or_else(|| "could not resolve the daemon data home".to_owned())?;
    let canonical_container = io
        .canonical_path(&teardown.session_container)
        .ok_or_else(|| "could not resolve the managed session container".to_owned())?;
    if canonical_container != canonical_repository.join(STATE_DIR).join(SESSIONS_DIR) {
        return Err("refusing teardown through a symlinked session ancestor".into());
    }
    if protected_teardown_target(
        &canonical_container,
        &canonical_repository,
        &canonical_data_home,
    ) {
        return Err("refusing teardown of a protected filesystem root".into());
    }

    if io.path_occupied(&teardown.session_root) {
        let canonical_target = io
            .canonical_path(&teardown.session_root)
            .ok_or_else(|| "could not resolve the managed session target".to_owned())?;
        if canonical_target != canonical_container.join(&teardown.name)
            || protected_teardown_target(
                &canonical_target,
                &canonical_repository,
                &canonical_data_home,
            )
        {
            return Err("refusing teardown outside the managed session container".into());
        }
    }
    Ok(())
}

fn protected_teardown_target(target: &Path, repository: &Path, data_home: &Path) -> bool {
    let filesystem_root = target.ancestors().last();
    target == repository || target == data_home || filesystem_root == Some(target)
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

/// Parse the separately confirmed permission to discard an unmerged branch.
/// It is independent from worktree force so legacy `--force` callers retain
/// their existing branch-preserving behavior.
fn force_delete_branch(payload: &Value) -> Result<bool, SessionRuntimeError> {
    match payload.get("force_delete_branch") {
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

/// The branch one session's worktree is checked out on. It is derived from the
/// name in exactly one place so create, legacy adoption, and the compensating
/// branch deletion can never disagree about which branch belongs to a session.
fn session_branch(name: &str) -> String {
    format!("usagi/{name}")
}

/// The fully qualified ref for one session branch. OID comparisons must not use
/// the short branch name: Git may resolve an identically named tag first.
fn session_branch_ref(name: &str) -> String {
    format!("refs/heads/{}", session_branch(name))
}

fn semantic_key(action: SessionAction, name: &str) -> String {
    format!("{action:?}:{name}").to_ascii_lowercase()
}

/// The journaled identity of one create: its origin, the session name, and the
/// role it was admitted for.
///
/// The origin is part of the key because a delegated create is one step of a
/// composite operation whose dispatch may still be missing, and the recovery pass
/// has to tell it from a plain `session_create` that is complete on its own.
/// A direct create without a role keeps the `create:<name>` form earlier daemons
/// wrote, so existing journals replay unchanged.
fn create_semantic_key(origin: CreateOrigin, name: &str, role_id: Option<&RoleId>) -> String {
    let action = semantic_key(origin.semantic_action(), name);
    role_id.map_or_else(
        || action.clone(),
        |role_id| format!("{action}:{}", role_id.as_str()),
    )
}

/// The journaled identity of one removal: its origin, session name, and request
/// options.
///
/// A compensation always deletes the branch, so the origin is durable intent
/// rather than implementation metadata. A requested removal's branch choice is
/// derived from the session lifecycle and captured in its `DeletePlan`. `force`
/// is spelled out even when false so opposite destructive intents cannot share
/// an operation id.
fn remove_semantic_key(
    kind: RemoveKind,
    name: &str,
    force: bool,
    force_delete_branch: bool,
) -> String {
    let action = semantic_key(SessionAction::Remove, name);
    let origin = match kind {
        RemoveKind::Requested => "requested",
        RemoveKind::Compensating => "compensating",
    };
    format!("{action}:origin={origin}:force={force}:force_delete_branch={force_delete_branch}")
}

/// Whether an existing journal proves it represents this removal intent.
///
/// Current journals compare their complete canonical key. Earlier keys omitted
/// either the branch-force choice or every option. They are replay-compatible
/// only while the session still carries the matching operation and `DeletePlan`,
/// which independently prove all effecting choices. Once that evidence is gone
/// (notably after success), guessing would correlate an unknown old intent with
/// a new request, so reuse fails closed.
fn remove_operation_matches(
    state: &WorkspaceLifecycleState,
    operation: &OperationJournal,
    kind: RemoveKind,
    name: &str,
    force: bool,
    force_delete_branch: bool,
    requested_key: &str,
) -> bool {
    if operation.semantic_key == requested_key {
        return true;
    }
    let action = semantic_key(SessionAction::Remove, name);
    let origin = match kind {
        RemoveKind::Requested => "requested",
        RemoveKind::Compensating => "compensating",
    };
    let previous_canonical_key = format!("{action}:origin={origin}:force={force}");
    if operation.semantic_key != action && operation.semantic_key != previous_canonical_key {
        return false;
    }
    for session in &state.sessions {
        if session.name != name || session.operation_id != Some(operation.operation_id) {
            continue;
        }
        let Some(plan) = session.delete_plan.as_ref() else {
            return false;
        };
        let branch_delete_matches = match kind {
            RemoveKind::Compensating => {
                plan.delete_branch && plan.force_delete_branch && force_delete_branch
            }
            RemoveKind::Requested => {
                plan.force_delete_branch == force_delete_branch
                    && (!force_delete_branch || plan.delete_branch)
            }
        };
        return plan.force == force && branch_delete_matches;
    }
    false
}

/// Whether one journaled semantic key names this action and session.
///
/// Create and remove keys may carry intent fields after the action and name, so
/// those first two components are a prefix rather than the whole key. Session
/// names cannot contain `:`, which is what makes the separator unambiguous.
fn names_session_operation(semantic_key: &str, action_and_name: &str) -> bool {
    semantic_key == action_and_name
        || semantic_key
            .strip_prefix(action_and_name)
            .is_some_and(|role| role.starts_with(':'))
}

fn projected_snapshot(
    state: &WorkspaceLifecycleState,
    root_worktree_id: WorktreeId,
    data_home: &Path,
    repo_root: &Path,
) -> Value {
    let mut value = snapshot(state, root_worktree_id);
    let catalog =
        usagi_core::infrastructure::role_catalog::load_effective(data_home, repo_root).ok();
    project_role_summaries(&mut value, catalog.as_ref());
    value
}

/// Applies current catalog display metadata without changing lifecycle truth.
fn project_role_summaries(value: &mut Value, catalog: Option<&EffectiveRoleCatalog>) {
    let items = value["sessions"]
        .as_array_mut()
        .expect("lifecycle snapshot always contains a sessions array");
    for item in items {
        let role_id = item
            .get("role_id")
            .cloned()
            .and_then(|value| serde_json::from_value::<RoleId>(value).ok());
        let summary = role_id
            .as_ref()
            .and_then(|id| catalog?.roles.get(id).map(|role| role.summary.clone()));
        item["role_summary"] = json!(summary);
    }
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
    use crate::infrastructure::session_worktree::SystemSessionWorktreeIo;
    use crate::usecase::session_teardown::drain_pending_teardowns;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use usagi_core::domain::session_lifecycle::{ManagedSession, SessionLifecycle};
    use usagi_core::infrastructure::git::GitOutput;

    struct FakeGit(bool);
    impl FakeGit {
        fn ok() -> Self {
            Self(true)
        }
        fn fail() -> Self {
            Self(false)
        }
    }

    struct FakeSessionWorktreeIo {
        occupied: bool,
        build_calls: Arc<AtomicUsize>,
    }

    struct FailingSessionWorktreeIo;

    struct ConfinementIo {
        canonical: std::collections::BTreeMap<PathBuf, Option<PathBuf>>,
        occupied: bool,
        remove_calls: Arc<AtomicUsize>,
    }

    impl ConfinementIo {
        fn new(remove_calls: Arc<AtomicUsize>) -> Self {
            Self {
                canonical: std::collections::BTreeMap::new(),
                occupied: false,
                remove_calls,
            }
        }
    }

    impl SessionWorktreeIo for ConfinementIo {
        fn remove_file_best_effort(&self, _: &Path) {}
        fn path_occupied(&self, _: &Path) -> bool {
            self.occupied
        }
        fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
            self.canonical
                .get(path)
                .cloned()
                .unwrap_or_else(|| Some(path.into()))
        }
        fn is_repo_root(&self, _: &Path) -> bool {
            false
        }
        fn is_linked_worktree(&self, _: &Path) -> bool {
            false
        }
        fn build_session_tree(
            &self,
            _: &dyn GitRunner,
            _: &Path,
            _: &Path,
            _: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove_session_tree(&self, _: &dyn GitRunner, _: &Path, _: bool) -> anyhow::Result<()> {
            self.remove_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl SessionWorktreeIo for FailingSessionWorktreeIo {
        fn remove_file_best_effort(&self, _: &Path) {}
        fn path_occupied(&self, _: &Path) -> bool {
            false
        }
        fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
            Some(path.into())
        }
        fn is_repo_root(&self, _: &Path) -> bool {
            false
        }
        fn is_linked_worktree(&self, _: &Path) -> bool {
            false
        }
        fn build_session_tree(
            &self,
            _: &dyn GitRunner,
            _: &Path,
            _: &Path,
            _: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove_session_tree(&self, _: &dyn GitRunner, _: &Path, _: bool) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("injected remove failure"))
        }
    }

    impl SessionWorktreeIo for FakeSessionWorktreeIo {
        fn remove_file_best_effort(&self, _: &Path) {}

        fn path_occupied(&self, _: &Path) -> bool {
            self.occupied
        }

        fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
            Some(path.to_path_buf())
        }

        fn is_repo_root(&self, _: &Path) -> bool {
            false
        }

        fn is_linked_worktree(&self, _: &Path) -> bool {
            true
        }

        fn build_session_tree(
            &self,
            git: &dyn GitRunner,
            workspace_root: &Path,
            _: &Path,
            _: &str,
        ) -> anyhow::Result<()> {
            self.build_calls.fetch_add(1, Ordering::SeqCst);
            let output = git.run(workspace_root, &["worktree", "add"])?;
            if output.success {
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "git worktree add failed: {}",
                    output.stderr
                ))
            }
        }

        fn remove_session_tree(&self, _: &dyn GitRunner, _: &Path, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct BranchExistsGit;
    fn checkout_validation_output(args: &[&str]) -> Option<GitOutput> {
        if matches!(args, ["rev-parse", "--verify", expression] if expression.ends_with("^{commit}"))
        {
            return Some(GitOutput {
                success: true,
                stdout: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                stderr: String::new(),
            });
        }
        (args.first() == Some(&"ls-tree")).then(|| GitOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    impl GitRunner for BranchExistsGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            if let Some(output) = checkout_validation_output(args) {
                return Ok(output);
            }
            Ok(GitOutput {
                success: false,
                stdout: String::new(),
                stderr: "fatal: a branch named 'usagi/one' already exists".into(),
            })
        }
    }

    struct WorkspaceExistsGit;
    impl GitRunner for WorkspaceExistsGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            if let Some(output) = checkout_validation_output(args) {
                return Ok(output);
            }
            Ok(GitOutput {
                success: false,
                stdout: String::new(),
                stderr: "fatal: '/repo/.usagi/sessions/one' already exists".into(),
            })
        }
    }
    impl GitRunner for FakeGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            if let Some(output) = checkout_validation_output(args) {
                return Ok(output);
            }
            Ok(GitOutput {
                success: self.0,
                stdout: String::new(),
                stderr: "no".into(),
            })
        }
    }

    enum ScriptedGitResult {
        Output {
            success: bool,
            stdout: &'static str,
            stderr: &'static str,
        },
        Error,
    }

    struct ScriptedGit {
        results: Mutex<std::collections::VecDeque<ScriptedGitResult>>,
    }

    impl ScriptedGit {
        fn new(results: impl IntoIterator<Item = ScriptedGitResult>) -> Self {
            Self {
                results: Mutex::new(results.into_iter().collect()),
            }
        }
    }

    impl GitRunner for ScriptedGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            if let Some(output) = checkout_validation_output(args) {
                return Ok(output);
            }
            match self.results.lock().unwrap().pop_front().unwrap() {
                ScriptedGitResult::Output {
                    success,
                    stdout,
                    stderr,
                } => Ok(GitOutput {
                    success,
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                }),
                ScriptedGitResult::Error => Err(anyhow::anyhow!("injected Git IO failure")),
            }
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
            Ok(checkout_validation_output(args).unwrap_or(GitOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            }))
        }
    }
    impl GitRunner for CountingGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(output) = checkout_validation_output(args) {
                return Ok(output);
            }
            Ok(GitOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }
    impl GitRunner for OutcomeGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(output) = checkout_validation_output(args) {
                return Ok(output);
            }
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
            SystemSessionWorktreeIo,
        )
        .unwrap();
        (tmp, runtime)
    }
    fn operation() -> String {
        OperationId::new().to_string()
    }

    fn confined_teardown() -> PendingTeardown {
        PendingTeardown {
            session_id: SessionId::new(),
            operation_id: OperationId::new(),
            name: "one".into(),
            repository_root: PathBuf::from("/repo"),
            data_home: PathBuf::from("/data"),
            session_container: PathBuf::from("/repo/.usagi/sessions"),
            session_root: PathBuf::from("/repo/.usagi/sessions/one"),
            force: false,
            delete_branch: false,
            force_delete_branch: false,
            merged_head_oid: None,
        }
    }

    #[test]
    fn a_branch_preserving_teardown_skips_git_branch_deletion() {
        assert_eq!(
            delete_teardown_branch(&FakeGit::ok(), &confined_teardown()),
            Ok(())
        );
    }

    #[test]
    fn an_exact_merged_pr_head_force_deletes_only_that_squash_merged_branch() {
        struct RecordingGit {
            head: String,
            calls: Arc<Mutex<Vec<Vec<String>>>>,
        }
        impl GitRunner for RecordingGit {
            fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
                self.calls
                    .lock()
                    .unwrap()
                    .push(args.iter().map(|arg| (*arg).to_owned()).collect());
                Ok(GitOutput {
                    success: true,
                    stdout: if args.first() == Some(&"rev-parse") {
                        self.head.clone()
                    } else {
                        String::new()
                    },
                    stderr: String::new(),
                })
            }
        }

        for (head, expected_flag) in [("a".repeat(40), "-D"), ("b".repeat(40), "-d")] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut teardown = confined_teardown();
            teardown.delete_branch = true;
            teardown.merged_head_oid = Some("a".repeat(40));
            delete_teardown_branch(
                &RecordingGit {
                    head,
                    calls: Arc::clone(&calls),
                },
                &teardown,
            )
            .unwrap();
            assert!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|args| { args == &["branch", expected_flag, "--", "usagi/one"] })
            );
            assert_eq!(
                calls.lock().unwrap()[0],
                ["rev-parse", "--verify", "refs/heads/usagi/one"]
            );
        }
    }

    #[test]
    fn merged_pr_head_is_durable_across_teardown_worker_handoff() {
        let (tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        let signal = TeardownSignal::new();
        let head = "a".repeat(40);
        perform_remove_with_merged_head(
            &runtime,
            &signal,
            &operation(),
            &json!({"name":"one"}),
            Some(head.clone()),
        )
        .unwrap();

        let pending = runtime.lock().unwrap().pending_teardowns().unwrap();
        assert_eq!(pending[0].merged_head_oid.as_deref(), Some(head.as_str()));
        let reopened = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            FakeGit::ok(),
            SystemSessionWorktreeIo,
        )
        .unwrap();
        let state = reopened.state().unwrap();
        assert_eq!(
            state.sessions[0]
                .delete_plan
                .as_ref()
                .unwrap()
                .merged_head_oid
                .as_deref(),
            Some(head.as_str())
        );
    }

    #[test]
    fn removal_identity_treats_a_missing_branch_as_absent_and_rejects_unknown_names() {
        let (_tmp, rt) = runtime(FakeGit::fail());
        let runtime = Arc::new(Mutex::new(rt));
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        let runtime = runtime.lock().unwrap();
        let session_id = runtime.session_id("one").unwrap();
        assert_eq!(runtime.removal_identity("one").unwrap(), (session_id, None));
        assert_eq!(
            runtime.removal_identity("missing"),
            Err(SessionRuntimeError::UnknownSession)
        );
    }

    #[test]
    fn session_runtime_fake_git_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut runtime = SessionRuntime::open(
            tmp.path().join("repository"),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            FakeGit::fail(),
            FakeSessionWorktreeIo {
                occupied: false,
                build_calls: Arc::clone(&calls),
            },
        )
        .unwrap();

        let error = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name": "one"}))
            .unwrap_err();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            error,
            SessionRuntimeError::SessionWorkspaceCreationFailed {
                name: "one".into(),
                detail: "no".into(),
            }
        );
    }

    #[test]
    fn session_runtime_fake_fs_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut runtime = SessionRuntime::open(
            tmp.path().join("repository"),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            FakeGit::ok(),
            FakeSessionWorktreeIo {
                occupied: true,
                build_calls: Arc::clone(&calls),
            },
        )
        .unwrap();

        assert_eq!(
            runtime.handle(
                SessionAction::Create,
                &operation(),
                &json!({"name": "occupied"}),
            ),
            Err(SessionRuntimeError::SessionWorkspaceExists(
                "occupied".into()
            ))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn create_lists_overview_and_removes_a_durable_session() {
        let (_tmp, mut runtime) = runtime(FakeGit::ok());
        // An empty workspace has nothing only its owner can finish, so it may be
        // given back; a session mid-teardown is exactly such work.
        assert!(!runtime.has_unfinished_work().unwrap());
        let created = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        assert_eq!(created.body["sessions"].as_array().unwrap().len(), 1);
        assert!(!runtime.has_unfinished_work().unwrap());
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
    fn role_errors_preserve_their_derived_value_contract() {
        let errors = [
            SessionRuntimeError::RoleConflict(
                Some(RoleId::new("coder").unwrap()),
                Some(RoleId::new("reviewer").unwrap()),
            ),
            SessionRuntimeError::InvalidRole("invalid role".into()),
        ];

        for error in errors {
            let cloned = error.clone();
            assert_eq!(cloned, error);
            assert_eq!(format!("{cloned:?}"), format!("{error:?}"));
            assert!(!error.safe_message().is_empty());
        }
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
            SystemSessionWorktreeIo,
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
            SystemSessionWorktreeIo,
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
    fn synchronous_failed_session_removal_records_a_branch_deletion_failure() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let mut runtime = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            ScriptedGit::new([
                ScriptedGitResult::Output {
                    success: true,
                    stdout: "",
                    stderr: "",
                },
                ScriptedGitResult::Output {
                    success: false,
                    stdout: "",
                    stderr: "branch is locked",
                },
            ]),
            SystemSessionWorktreeIo,
        )
        .unwrap();
        runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        let mut state = runtime.state().unwrap();
        let revision = state.state_revision;
        state.sessions[0].lifecycle = SessionLifecycle::Failed;
        state.sessions[0].failure = Some(Failure {
            stage: FailureStage::Create,
            summary: "create failed".into(),
        });
        runtime.store.replace_if_revision(revision, &state).unwrap();

        let error = runtime
            .handle(SessionAction::Remove, &operation(), &json!({"name":"one"}))
            .unwrap_err();

        assert!(
            matches!(&error, SessionRuntimeError::DurableFailure(summary) if summary.contains("git branch delete failed: branch is locked")),
            "{error:?}"
        );
        let failed = &runtime.state().unwrap().sessions[0];
        assert_eq!(failed.lifecycle, SessionLifecycle::Failed);
        assert_eq!(failed.failure.as_ref().unwrap().stage, FailureStage::Delete);
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
            SystemSessionWorktreeIo,
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
            SystemSessionWorktreeIo,
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
    fn existing_session_create_is_idempotent_for_the_same_legacy_role() {
        let (tmp, mut runtime) = runtime(FakeGit::ok());
        runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();

        let reply = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();

        assert_eq!(reply.body["sessions"].as_array().unwrap().len(), 1);
        std::fs::write(
            tmp.path().join(".usagi/roles.toml"),
            r#"version = 1
[roles.coder]
summary = "Implement"
scopes = ["session"]
instructions = "code"
"#,
        )
        .unwrap();
        assert!(matches!(
            runtime.handle(
                SessionAction::Create,
                &operation(),
                &json!({"name":"one", "role":"coder"}),
            ),
            Err(SessionRuntimeError::RoleConflict(None, Some(_)))
        ));
        assert!(matches!(
            runtime.session_role(SessionId::new()),
            Err(SessionRuntimeError::UnknownSession)
        ));
    }

    #[test]
    fn catalog_default_assignment_is_stable_and_conflicting_role_is_rejected() {
        let (tmp, mut runtime) = runtime(FakeGit::ok());
        std::fs::write(
            tmp.path().join(".usagi/roles.toml"),
            r#"version = 1
[defaults]
session = "coder"
[roles.coder]
summary = "Implement"
scopes = ["session"]
instructions = "code"
[roles.reviewer]
summary = "Review"
scopes = ["session"]
instructions = "review"
"#,
        )
        .unwrap();

        let created = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        assert_eq!(created.body["sessions"][0]["role_id"], "coder");
        let replay = runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        assert_eq!(replay.body["sessions"].as_array().unwrap().len(), 1);
        assert!(matches!(
            runtime.handle(
                SessionAction::Create,
                &operation(),
                &json!({"name":"one", "role":"reviewer"}),
            ),
            Err(SessionRuntimeError::RoleConflict(..))
        ));
        assert!(matches!(
            runtime.handle(
                SessionAction::Create,
                &operation(),
                &json!({"name":"one", "role":"missing"}),
            ),
            Err(SessionRuntimeError::InvalidRole(_))
        ));
        let id = runtime.session_id("one").unwrap();
        assert_eq!(runtime.session_role(id).unwrap().unwrap().as_str(), "coder");

        std::fs::write(
            tmp.path().join(".usagi/roles.toml"),
            r#"version = 1
[defaults]
session = "coder"
[roles.coder]
summary = "Changed summary"
scopes = ["session"]
instructions = "changed"
"#,
        )
        .unwrap();
        let snapshot = runtime.snapshot().unwrap();
        assert_eq!(snapshot["sessions"][0]["role_id"], "coder");
        assert_eq!(snapshot["sessions"][0]["role_summary"], "Changed summary");
        let status = runtime
            .handle(SessionAction::Status, &operation(), &json!({}))
            .unwrap();
        assert_eq!(status.body["sessions"][0]["role_id"], "coder");
        assert_eq!(
            status.body["sessions"][0]["role_summary"],
            "Changed summary"
        );

        assert_eq!(
            runtime
                .handle(
                    SessionAction::Create,
                    &operation(),
                    &json!({"name":"invalid", "role":"Bad"}),
                )
                .unwrap_err(),
            SessionRuntimeError::InvalidRequest
        );

        std::fs::write(
            tmp.path().join(".usagi/roles.toml"),
            r#"version = 1
[roles.reviewer]
summary = "Review"
scopes = ["session"]
instructions = "review"
"#,
        )
        .unwrap();
        assert!(matches!(
            runtime.handle(SessionAction::Create, &operation(), &json!({"name":"one"})),
            Err(SessionRuntimeError::InvalidRole(_))
        ));
    }

    #[test]
    fn malformed_catalog_fails_create_before_git_effect() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".usagi")).unwrap();
        std::fs::write(tmp.path().join(".usagi/roles.toml"), "version = 99\n").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut runtime = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            FakeGit::ok(),
            FakeSessionWorktreeIo {
                occupied: false,
                build_calls: Arc::clone(&calls),
            },
        )
        .unwrap();
        assert!(matches!(
            runtime.handle(SessionAction::Create, &operation(), &json!({"name":"one"})),
            Err(SessionRuntimeError::InvalidRole(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        std::fs::write(
            tmp.path().join(".usagi/roles.toml"),
            r#"version = 1
[roles.director]
summary = "Direct"
scopes = ["root"]
instructions = "direct"
"#,
        )
        .unwrap();
        assert!(matches!(
            runtime.handle(
                SessionAction::Create,
                &operation(),
                &json!({"name":"one", "role":"director"}),
            ),
            Err(SessionRuntimeError::InvalidRole(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn effective_role_catalog_rejects_a_malformed_catalog() {
        let (tmp, runtime) = runtime(FakeGit::ok());
        std::fs::write(tmp.path().join(".usagi/roles.toml"), "version = 99\n").unwrap();

        assert!(matches!(
            runtime.effective_role_catalog(),
            Err(SessionRuntimeError::InvalidRole(message))
                if message == "effective role catalog is invalid"
        ));
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
            SystemSessionWorktreeIo,
        )
        .unwrap();

        let created = first
            .handle(SessionAction::Create, &operation, &json!({"name":"one"}))
            .unwrap();
        // Resolve + attribute scan + add. The replay below performs none of
        // them.
        assert_eq!(first_calls.load(Ordering::SeqCst), 3);
        drop(first);

        let replay_calls = Arc::new(AtomicUsize::new(0));
        let mut restarted = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            CountingGit {
                calls: Arc::clone(&replay_calls),
            },
            SystemSessionWorktreeIo,
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
            SystemSessionWorktreeIo,
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
        // Resolve + attribute scan + failed add + exact partial-registration
        // probe. The replay above performs none of them.
        assert_eq!(first_calls.load(Ordering::SeqCst), 4);
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
            SystemSessionWorktreeIo,
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
            SystemSessionWorktreeIo,
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
            SystemSessionWorktreeIo,
        )
        .unwrap();
        let failed = first
            .handle(
                SessionAction::Remove,
                &operation,
                &json!({"name":"one", "force":true}),
            )
            .unwrap_err();
        let replayed = first
            .handle(
                SessionAction::Remove,
                &operation,
                &json!({"name":"one", "force":true}),
            )
            .unwrap_err();
        assert_eq!(replayed.safe_message(), failed.safe_message());
        assert_eq!(
            first
                .handle(
                    SessionAction::Remove,
                    &operation,
                    &json!({"name":"one", "force":false}),
                )
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
            SystemSessionWorktreeIo,
        )
        .unwrap();
        let reopened = restarted
            .handle(
                SessionAction::Remove,
                &operation,
                &json!({"name":"one", "force":true}),
            )
            .unwrap_err();
        assert_eq!(reopened.safe_message(), failed.safe_message());
        assert_eq!(
            restarted
                .handle(
                    SessionAction::Remove,
                    &operation,
                    &json!({"name":"one", "force":false}),
                )
                .unwrap_err(),
            SessionRuntimeError::IdempotencyConflict
        );
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
                    role_id: None,
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
            SystemSessionWorktreeIo,
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
            SystemSessionWorktreeIo,
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
            SystemSessionWorktreeIo,
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
            SystemSessionWorktreeIo,
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
            SystemSessionWorktreeIo,
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
        SystemSessionWorktreeIo
            .build_session_tree(&git, &workspace, &destination, "usagi/feature")
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(destination.join("README.md")).unwrap(),
            "read me"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("docs/guide.md")).unwrap(),
            "guide"
        );
        let calls = git.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls.last(),
            Some(&(
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
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                ],
            ))
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
            SystemSessionWorktreeIo,
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

    #[test]
    fn bound_workspace_root_predicts_the_root_open_binds() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("daemon");
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        // Fresh state: the prediction is the startup candidate, and `open` binds
        // exactly that.
        assert_eq!(
            SessionRuntime::bound_workspace_root(&state_dir, first.clone()).unwrap(),
            first
        );
        let runtime = SessionRuntime::open(
            first.clone(),
            &state_dir,
            DaemonGeneration::new(),
            FakeGit::ok(),
            SystemSessionWorktreeIo,
        )
        .unwrap();
        assert_eq!(runtime.repository_root(), first);
        drop(runtime);

        // Durable state: the stored root wins over a different candidate for the
        // prediction and for `open` alike, so the fence cannot key a workspace
        // the runtime will not own.
        assert_eq!(
            SessionRuntime::bound_workspace_root(&state_dir, second.clone()).unwrap(),
            first
        );
        let reopened = SessionRuntime::open(
            second,
            &state_dir,
            DaemonGeneration::new(),
            FakeGit::ok(),
            SystemSessionWorktreeIo,
        )
        .unwrap();
        assert_eq!(reopened.repository_root(), first);
    }

    #[test]
    fn bound_workspace_root_reports_unreadable_state() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("daemon");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("sessions.json"), "not json").unwrap();
        assert_eq!(
            SessionRuntime::bound_workspace_root(&state_dir, tmp.path().to_path_buf()),
            Err(SessionRuntimeError::Storage)
        );
    }

    #[test]
    fn session_id_reports_unreadable_state() {
        let (tmp, mut runtime) = runtime(FakeGit::ok());
        runtime
            .handle(SessionAction::Create, &operation(), &json!({"name": "one"}))
            .unwrap();
        let session_id = runtime.session_id("one").unwrap();
        assert!(runtime.session_scope_by_id(session_id).is_ok());
        assert_eq!(
            runtime.session_id("missing"),
            Err(SessionRuntimeError::UnknownSession)
        );
        std::fs::write(tmp.path().join("daemon/sessions.json"), "not json").unwrap();

        assert_eq!(runtime.session_id("one"), Err(SessionRuntimeError::Storage));
    }

    #[test]
    fn pending_teardowns_skip_incomplete_delete_records() {
        let (_tmp, runtime) = runtime(FakeGit::ok());
        let mut missing_plan =
            ManagedSession::new_creating("missing-plan".into(), OperationId::new(), Utc::now());
        missing_plan.lifecycle = SessionLifecycle::Deleting;
        let mut missing_operation = ManagedSession::new_creating(
            "missing-operation".into(),
            OperationId::new(),
            Utc::now(),
        );
        missing_operation.lifecycle = SessionLifecycle::Deleting;
        missing_operation.delete_plan = Some(DeletePlan {
            targets: vec!["missing-operation".into()],
            force: false,
            delete_branch: false,
            force_delete_branch: false,
            merged_head_oid: None,
        });
        missing_operation.operation_id = None;

        let mut state = runtime.state().unwrap();
        let revision = state.state_revision;
        state.state_revision += 1;
        state.sessions = vec![missing_plan, missing_operation];
        runtime.store.replace_if_revision(revision, &state).unwrap();

        assert!(runtime.pending_teardowns().unwrap().is_empty());
    }

    /// A Git runner that records whether the shared session lock was free at the
    /// moment Git ran. `perform_create`/`perform_remove` must release the lock
    /// before invoking Git, so a same-thread `try_lock` succeeds here.
    struct LockProbeGit {
        runtime: std::sync::Weak<Mutex<SessionRuntime>>,
        observed_unlocked: Arc<std::sync::atomic::AtomicBool>,
    }
    impl GitRunner for LockProbeGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            let runtime = self.runtime.upgrade().expect("runtime remains alive");
            self.observed_unlocked.store(
                runtime.try_lock().is_ok(),
                std::sync::atomic::Ordering::SeqCst,
            );
            Ok(checkout_validation_output(args).unwrap_or(GitOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            }))
        }
    }

    /// A Git runner that poisons the shared session lock while it runs, so the
    /// `finish_*` re-lock inside `perform_*` observes a poisoned lock.
    struct PoisoningGit {
        runtime: std::sync::Weak<Mutex<SessionRuntime>>,
    }
    impl GitRunner for PoisoningGit {
        fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
            if let Some(output) = checkout_validation_output(args) {
                return Ok(output);
            }
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

    /// A delegated create journals its origin, which is the only durable trace
    /// that a session belongs to a composite operation whose dispatch may never
    /// have happened. A plain `session_create` is complete on its own and is never
    /// a recovery candidate — with or without a role (#611).
    #[test]
    fn only_delegated_creates_are_reported_for_recovery() {
        let (tmp, rt) = runtime(FakeGit::ok());
        std::fs::write(
            tmp.path().join(".usagi/roles.toml"),
            r#"version = 1
[roles.coder]
summary = "Implement"
scopes = ["session"]
instructions = "code"
"#,
        )
        .unwrap();
        let runtime = Arc::new(Mutex::new(rt));
        let delegate = |operation: &str, payload| {
            perform_delegated_create(&runtime, &FakeGit::ok(), operation, &payload).unwrap();
        };
        let create = |payload| {
            perform_create(&runtime, &FakeGit::ok(), &operation(), &payload).unwrap();
        };

        let delegated = operation();
        delegate(&delegated, json!({"name":"triage"}));
        create(json!({"name":"plain"}));
        // A role-bearing create journals its role in the same key, so the recovery
        // pass must recognise the action and name as a prefix rather than by
        // whole-key equality — in both directions.
        let with_role = operation();
        delegate(&with_role, json!({"name":"triage-coder", "role":"coder"}));
        create(json!({"name":"plain-coder", "role":"coder"}));

        let candidates = runtime.lock().unwrap().delegated_sessions().unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>(),
            ["triage", "triage-coder"]
        );
        assert_eq!(candidates[0].operation_id.to_string(), delegated);
        assert_eq!(candidates[1].operation_id.to_string(), with_role);

        // A retry under the same operation replays the create instead of making a
        // second session.
        delegate(&delegated, json!({"name":"triage"}));
        assert_eq!(
            runtime.lock().unwrap().snapshot().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
    }

    /// A session the journal does not explain is not a delegation.
    ///
    /// Legacy adoption produces exactly that: an available session with no create
    /// operation at all. Recovery must leave it alone rather than read the absence
    /// of a journal entry as "nothing dispatched" and roll it back (#611).
    #[test]
    fn a_session_without_an_owning_operation_is_not_a_delegation_candidate() {
        let (_tmp, rt) = runtime(FakeGit::ok());
        let mut state = rt.state().unwrap();
        let revision = state.state_revision;
        state.state_revision += 1;
        state.sessions.push(ManagedSession::adopt_available(
            "adopted".into(),
            Utc::now(),
        ));
        rt.store.replace_if_revision(revision, &state).unwrap();

        assert_eq!(
            rt.state().unwrap().sessions[0].lifecycle,
            SessionLifecycle::Available
        );
        assert!(rt.delegated_sessions().unwrap().is_empty());
    }

    /// Compensating a delegated create undoes the branch too, so the same session
    /// name can be delegated again (#611).
    #[test]
    fn compensating_a_delegated_create_undoes_the_branch_and_frees_the_name() {
        let (tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_delegated_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"triage"}),
        )
        .unwrap();

        let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("triage");
        std::fs::create_dir_all(&session_root).unwrap();
        std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
        let signal = TeardownSignal::new();
        perform_compensating_remove(&runtime, &signal, &operation(), "triage").unwrap();

        // The durable plan says: force, and take the branch with it.
        let pending = runtime.lock().unwrap().pending_teardowns().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].force);
        assert!(pending[0].delete_branch);
        assert!(pending[0].force_delete_branch);

        let git = RecordingGit::new();
        let calls = Arc::clone(&git.calls);
        let reports = drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&runtime)),
            &WorktreeTeardown::new(git, SystemSessionWorktreeIo),
            &|| false,
        );
        assert_eq!(reports[0].effect_error, None);
        assert!(!session_root.exists());
        let calls = calls.lock().unwrap().clone();
        assert_eq!(
            calls.last().unwrap().1,
            vec!["branch", "-D", "--", "usagi/triage"]
        );
        // The branch is deleted from the repository root, never from the tree that
        // was just removed.
        assert_eq!(calls.last().unwrap().0, tmp.path());
        // The compensated session is gone, so it is no longer a recovery candidate.
        let candidates = || runtime.lock().unwrap().delegated_sessions().unwrap().len();
        assert_eq!(candidates(), 0);

        // The name is free again, and a plain create that reuses it belongs to the
        // user: the stale delegated journal entry must not make it a candidate.
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"triage"}),
        )
        .unwrap();
        assert_eq!(candidates(), 0);
    }

    /// Removing an available session safely deletes a fully merged branch.
    #[test]
    fn removing_an_available_session_safely_deletes_its_branch() {
        let (tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
        std::fs::create_dir_all(&session_root).unwrap();
        std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
        let signal = TeardownSignal::new();
        perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();
        let pending = runtime.lock().unwrap().pending_teardowns().unwrap();
        assert!(pending[0].delete_branch);
        assert!(!pending[0].force_delete_branch);

        let git = RecordingGit::new();
        let calls = Arc::clone(&git.calls);
        drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&runtime)),
            &WorktreeTeardown::new(git, SystemSessionWorktreeIo),
            &|| false,
        );
        assert!(calls.lock().unwrap().iter().any(|(repo, args)| {
            repo == tmp.path() && args == &["branch", "-d", "--", "usagi/one"]
        }));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One recovery scenario proves safe failure, confirmed retry, and final cleanup.
    fn removing_a_session_with_unmerged_commits_keeps_the_branch_and_failed_name() {
        struct UnmergedBranchGit {
            calls: Arc<Mutex<Vec<Vec<String>>>>,
        }
        impl GitRunner for UnmergedBranchGit {
            fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
                self.calls
                    .lock()
                    .unwrap()
                    .push(args.iter().map(|arg| (*arg).to_owned()).collect());
                Ok(if args.get(..2) == Some(["branch", "-d"].as_slice()) {
                    GitOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: "error: the branch 'usagi/one' is not fully merged".into(),
                    }
                } else {
                    GitOutput {
                        success: true,
                        stdout: String::new(),
                        stderr: String::new(),
                    }
                })
            }
        }

        let (tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
        std::fs::create_dir_all(&session_root).unwrap();
        std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
        let signal = TeardownSignal::new();
        perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let reports = drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&runtime)),
            &WorktreeTeardown::new(
                UnmergedBranchGit {
                    calls: Arc::clone(&calls),
                },
                SystemSessionWorktreeIo,
            ),
            &|| false,
        );

        assert!(
            reports[0]
                .effect_error
                .as_deref()
                .is_some_and(|error| error.contains("not fully merged"))
        );
        let state = runtime.lock().unwrap().state().unwrap();
        assert_eq!(state.sessions[0].name, "one");
        assert_eq!(state.sessions[0].lifecycle, SessionLifecycle::Failed);
        assert!(
            state.sessions[0]
                .failure
                .as_ref()
                .is_some_and(|failure| failure.summary.contains("not fully merged"))
        );
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|args| { args == &["branch", "-d", "--", "usagi/one"] })
        );
        drop(state);

        perform_remove(
            &runtime,
            &signal,
            &operation(),
            &json!({
                "name":"one",
                "force":true,
                "force_delete_branch":true,
            }),
        )
        .unwrap();
        let reports = drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&runtime)),
            &WorktreeTeardown::new(
                UnmergedBranchGit {
                    calls: Arc::clone(&calls),
                },
                SystemSessionWorktreeIo,
            ),
            &|| false,
        );

        assert_eq!(reports[0].effect_error, None);
        assert!(runtime.lock().unwrap().state().unwrap().sessions.is_empty());
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|args| { args == &["branch", "-D", "--", "usagi/one"] })
        );
    }

    #[test]
    fn forced_branch_delete_requires_worktree_force() {
        let (_tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();

        let error = perform_remove(
            &runtime,
            &TeardownSignal::new(),
            &operation(),
            &json!({"name":"one", "force_delete_branch":true}),
        )
        .unwrap_err();

        assert_eq!(error, SessionRuntimeError::InvalidRequest);
        assert_eq!(
            runtime.lock().unwrap().state().unwrap().sessions[0].lifecycle,
            SessionLifecycle::Available
        );
    }

    #[test]
    fn removing_a_failed_session_deletes_its_branch() {
        let (tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        {
            let runtime = runtime.lock().unwrap();
            let mut state = runtime.state().unwrap();
            let revision = state.state_revision;
            state.sessions[0].lifecycle = SessionLifecycle::Failed;
            state.sessions[0].failure = Some(Failure {
                stage: FailureStage::Create,
                summary: "create failed".into(),
            });
            runtime.store.replace_if_revision(revision, &state).unwrap();
        }
        let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
        std::fs::create_dir_all(&session_root).unwrap();
        std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();

        let signal = TeardownSignal::new();
        perform_remove(&runtime, &signal, &operation(), &json!({"name":"one"})).unwrap();
        let pending = runtime.lock().unwrap().pending_teardowns().unwrap();
        assert!(pending[0].delete_branch);
        assert!(!pending[0].force_delete_branch);

        let git = RecordingGit::new();
        let calls = Arc::clone(&git.calls);
        let reports = drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&runtime)),
            &WorktreeTeardown::new(git, SystemSessionWorktreeIo),
            &|| false,
        );

        assert_eq!(reports[0].effect_error, None);
        assert!(!session_root.exists());
        assert!(runtime.lock().unwrap().state().unwrap().sessions.is_empty());
        assert!(calls.lock().unwrap().iter().any(|(repo, args)| {
            repo == tmp.path() && args == &["branch", "-d", "--", "usagi/one"]
        }));
    }

    /// The branch deletion is part of the compensation, so its failure is a
    /// teardown failure: the record stays diagnosable rather than being reported
    /// as a clean rollback.
    #[test]
    fn a_failed_branch_deletion_fails_the_compensating_teardown() {
        /// Succeeds at removing the worktree and refuses to delete the branch.
        struct BranchLockedGit;
        impl GitRunner for BranchLockedGit {
            fn run(&self, _: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
                Ok(GitOutput {
                    success: args[0] != "branch",
                    stdout: String::new(),
                    stderr: "error: cannot delete branch 'usagi/one' used by worktree".into(),
                })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
        std::fs::create_dir_all(&session_root).unwrap();
        let data_home = tmp.path().join("daemon");
        std::fs::create_dir_all(&data_home).unwrap();
        let error = WorktreeTeardown::new(BranchLockedGit, SystemSessionWorktreeIo)
            .tear_down(&PendingTeardown {
                delete_branch: true,
                repository_root: tmp.path().to_path_buf(),
                data_home,
                session_container: tmp.path().join(STATE_DIR).join(SESSIONS_DIR),
                session_root,
                ..confined_teardown()
            })
            .unwrap_err();
        assert!(error.contains("git branch delete failed"), "{error}");
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

        // A row in a transient state is work only this daemon can finish, so the
        // workspace it belongs to must not be given back while it is there.
        assert!(runtime.lock().unwrap().has_unfinished_work().unwrap());

        // The pending teardown is derived from that durable state alone.
        let pending = runtime.lock().unwrap().pending_teardowns().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "one");
        assert_eq!(pending[0].session_root, session_root);

        // Draining it removes the tree and retires the record.
        let calls = Arc::new(AtomicUsize::new(0));
        let reports = drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&runtime)),
            &WorktreeTeardown::new(
                CountingGit {
                    calls: Arc::clone(&calls),
                },
                SystemSessionWorktreeIo,
            ),
            &|| false,
        );
        assert_eq!(reports[0].effect_error, None);
        assert_eq!(reports[0].finalize_error, None);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
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

    /// Losing the accepted response does not widen the operation identity. The
    /// exact force intent replays while the opposite intent conflicts, in both
    /// directions, and the one admitted plan remains the only worktree effect.
    #[test]
    #[allow(clippy::too_many_lines)] // One scenario crosses accepted/restart/succeeded boundaries.
    fn remove_force_is_part_of_the_durable_identity_before_and_after_restart() {
        for (first_force, conflicting_force) in [(false, true), (true, false)] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::create_dir(tmp.path().join(".git")).unwrap();
            let state_dir = tmp.path().join("daemon");
            let runtime = Arc::new(Mutex::new(
                SessionRuntime::open(
                    tmp.path().to_path_buf(),
                    &state_dir,
                    DaemonGeneration::new(),
                    FakeGit::ok(),
                    SystemSessionWorktreeIo,
                )
                .unwrap(),
            ));
            perform_create(
                &runtime,
                &FakeGit::ok(),
                &operation(),
                &json!({"name":"one"}),
            )
            .unwrap();
            let session_root = tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("one");
            std::fs::create_dir_all(&session_root).unwrap();
            std::fs::write(session_root.join(".git"), "gitdir: /fixture").unwrap();
            let operation = operation();
            let signal = TeardownSignal::new();
            let request = json!({"name":"one", "force":first_force});

            // Model response loss by discarding the first accepted reply.
            perform_remove(&runtime, &signal, &operation, &request).unwrap();
            let replayed = perform_remove(&runtime, &signal, &operation, &request).unwrap();
            assert_eq!(replayed.operation_id, operation);
            assert_eq!(replayed.body["sessions"][0]["lifecycle"], "deleting");
            assert_eq!(
                perform_remove(
                    &runtime,
                    &signal,
                    &operation,
                    &json!({"name":"one", "force":conflicting_force}),
                ),
                Err(SessionRuntimeError::IdempotencyConflict)
            );
            assert_eq!(
                runtime.lock().unwrap().pending_teardowns().unwrap().len(),
                1
            );

            // An accepted operation remains replayable after daemon restart;
            // the durable plan is still the only queued effect.
            drop(runtime);
            let runtime = Arc::new(Mutex::new(
                SessionRuntime::open(
                    tmp.path().to_path_buf(),
                    &state_dir,
                    DaemonGeneration::new(),
                    FakeGit::ok(),
                    SystemSessionWorktreeIo,
                )
                .unwrap(),
            ));
            let after_accepted_restart =
                perform_remove(&runtime, &signal, &operation, &request).unwrap();
            assert_eq!(after_accepted_restart.operation_id, operation);
            assert_eq!(
                runtime.lock().unwrap().pending_teardowns().unwrap().len(),
                1
            );
            assert_eq!(
                perform_remove(
                    &runtime,
                    &signal,
                    &operation,
                    &json!({"name":"one", "force":conflicting_force}),
                ),
                Err(SessionRuntimeError::IdempotencyConflict)
            );

            let calls = Arc::new(AtomicUsize::new(0));
            drain_pending_teardowns(
                &SharedSessionTeardown::new(Arc::clone(&runtime)),
                &WorktreeTeardown::new(
                    CountingGit {
                        calls: Arc::clone(&calls),
                    },
                    SystemSessionWorktreeIo,
                ),
                &|| false,
            );
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            let succeeded = perform_remove(&runtime, &signal, &operation, &request).unwrap();
            assert_eq!(succeeded.operation_id, operation);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            drop(runtime);

            // A terminal successful outcome also survives restart without a
            // replacement worktree effect.
            let restarted = Arc::new(Mutex::new(
                SessionRuntime::open(
                    tmp.path().to_path_buf(),
                    &state_dir,
                    DaemonGeneration::new(),
                    CountingGit {
                        calls: Arc::clone(&calls),
                    },
                    SystemSessionWorktreeIo,
                )
                .unwrap(),
            ));
            let after_restart = perform_remove(&restarted, &signal, &operation, &request).unwrap();
            assert_eq!(after_restart.operation_id, operation);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(
                perform_remove(
                    &restarted,
                    &signal,
                    &operation,
                    &json!({"name":"one", "force":conflicting_force}),
                ),
                Err(SessionRuntimeError::IdempotencyConflict)
            );
        }
    }

    #[test]
    fn legacy_remove_keys_replay_only_while_the_delete_plan_proves_the_intent() {
        let (tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"one"}),
        )
        .unwrap();
        let operation = operation();
        let signal = TeardownSignal::new();
        let request = json!({"name":"one", "force":true});
        perform_remove(&runtime, &signal, &operation, &request).unwrap();

        // Simulate a snapshot written before remove keys carried force/origin.
        {
            let runtime = runtime.lock().unwrap();
            let mut legacy = runtime.state().unwrap();
            let revision = legacy.state_revision;
            legacy.operations.last_mut().unwrap().semantic_key =
                semantic_key(SessionAction::Remove, "one");
            runtime
                .store
                .replace_if_revision(revision, &legacy)
                .unwrap();
        }
        assert!(perform_remove(&runtime, &signal, &operation, &request).is_ok());
        assert_eq!(
            perform_remove(
                &runtime,
                &signal,
                &operation,
                &json!({"name":"one", "force":false}),
            ),
            Err(SessionRuntimeError::IdempotencyConflict)
        );

        // The retained plan proves the same intent across restart too.
        let state_dir = tmp.path().join("daemon");
        drop(runtime);
        let restarted = Arc::new(Mutex::new(
            SessionRuntime::open(
                tmp.path().to_path_buf(),
                &state_dir,
                DaemonGeneration::new(),
                FakeGit::ok(),
                SystemSessionWorktreeIo,
            )
            .unwrap(),
        ));
        assert!(perform_remove(&restarted, &signal, &operation, &request).is_ok());

        drain_pending_teardowns(
            &SharedSessionTeardown::new(Arc::clone(&restarted)),
            &WorktreeTeardown::new(FakeGit::ok(), SystemSessionWorktreeIo),
            &|| false,
        );
        // Success retires the session and its plan. The legacy key can no longer
        // prove either force value, so both guesses fail closed.
        for force in [false, true] {
            assert_eq!(
                perform_remove(
                    &restarted,
                    &signal,
                    &operation,
                    &json!({"name":"one", "force":force}),
                ),
                Err(SessionRuntimeError::IdempotencyConflict)
            );
        }
    }

    #[test]
    fn compensating_and_requested_removes_are_distinct_durable_intents() {
        let (_tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_delegated_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"triage"}),
        )
        .unwrap();
        let operation = operation();
        let signal = TeardownSignal::new();
        perform_compensating_remove(&runtime, &signal, &operation, "triage").unwrap();

        assert_eq!(
            perform_remove(
                &runtime,
                &signal,
                &operation,
                &json!({"name":"triage", "force":true}),
            ),
            Err(SessionRuntimeError::IdempotencyConflict)
        );
        let state = runtime.lock().unwrap().state().unwrap();
        assert_eq!(state.operations.len(), 2);
        assert_eq!(
            state.operations[1].semantic_key,
            "remove:triage:origin=compensating:force=true:force_delete_branch=true"
        );
        assert!(
            state.sessions[0]
                .delete_plan
                .as_ref()
                .unwrap()
                .delete_branch
        );
    }

    #[test]
    fn legacy_compensation_replay_requires_a_matching_session_and_forced_branch_delete_flags() {
        let (_tmp, rt) = runtime(FakeGit::ok());
        let runtime = Arc::new(Mutex::new(rt));
        perform_delegated_create(
            &runtime,
            &FakeGit::ok(),
            &operation(),
            &json!({"name":"triage"}),
        )
        .unwrap();
        let operation_id = operation();
        let signal = TeardownSignal::new();
        perform_compensating_remove(&runtime, &signal, &operation_id, "triage").unwrap();

        let mut state = runtime.lock().unwrap().state().unwrap();
        let mut legacy_operation = state.operations.last().unwrap().clone();
        legacy_operation.semantic_key = semantic_key(SessionAction::Remove, "triage");
        let requested_key = remove_semantic_key(RemoveKind::Compensating, "triage", true, true);
        assert!(remove_operation_matches(
            &state,
            &legacy_operation,
            RemoveKind::Compensating,
            "triage",
            true,
            true,
            &requested_key,
        ));

        let mut wrong_name_operation = legacy_operation.clone();
        wrong_name_operation.semantic_key = semantic_key(SessionAction::Remove, "missing");
        assert!(!remove_operation_matches(
            &state,
            &wrong_name_operation,
            RemoveKind::Compensating,
            "missing",
            true,
            true,
            &remove_semantic_key(RemoveKind::Compensating, "missing", true, true),
        ));
        let mut wrong_id_operation = legacy_operation.clone();
        wrong_id_operation.operation_id = OperationId::new();
        assert!(!remove_operation_matches(
            &state,
            &wrong_id_operation,
            RemoveKind::Compensating,
            "triage",
            true,
            true,
            &requested_key,
        ));

        let saved_plan = state.sessions[0].delete_plan.take();
        assert!(!remove_operation_matches(
            &state,
            &legacy_operation,
            RemoveKind::Compensating,
            "triage",
            true,
            true,
            &requested_key,
        ));
        state.sessions[0].delete_plan = saved_plan;

        let plan = state.sessions[0].delete_plan.as_mut().unwrap();
        plan.delete_branch = false;
        assert!(!remove_operation_matches(
            &state,
            &legacy_operation,
            RemoveKind::Compensating,
            "triage",
            true,
            true,
            &requested_key,
        ));
        let plan = state.sessions[0].delete_plan.as_mut().unwrap();
        plan.delete_branch = true;
        plan.force_delete_branch = false;
        assert!(!remove_operation_matches(
            &state,
            &legacy_operation,
            RemoveKind::Compensating,
            "triage",
            true,
            true,
            &requested_key,
        ));
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
            &WorktreeTeardown::new(FakeGit::ok(), SystemSessionWorktreeIo),
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
                SystemSessionWorktreeIo,
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
                SystemSessionWorktreeIo,
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
            &WorktreeTeardown::new(FakeGit::ok(), SystemSessionWorktreeIo),
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
    fn restart_rejects_persisted_path_names_without_touching_a_sentinel() {
        for name in [
            "/tmp/victim",
            "../victim",
            "nested/victim",
            "nested\\victim",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::create_dir(tmp.path().join(".git")).unwrap();
            let state_dir = tmp.path().join("daemon");
            let mut runtime = SessionRuntime::open(
                tmp.path().to_path_buf(),
                &state_dir,
                DaemonGeneration::new(),
                FakeGit::ok(),
                FakeSessionWorktreeIo {
                    occupied: false,
                    build_calls: Arc::new(AtomicUsize::new(0)),
                },
            )
            .unwrap();
            runtime
                .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
                .unwrap();
            let state_path = state_dir.join("sessions.json");
            let mut document: Value =
                serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
            document["state"]["sessions"][0]["name"] = Value::String(name.into());
            std::fs::write(&state_path, serde_json::to_vec(&document).unwrap()).unwrap();
            let sentinel = tmp.path().join("victim/sentinel");
            std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
            std::fs::write(&sentinel, "keep").unwrap();
            drop(runtime);

            let git_calls = Arc::new(AtomicUsize::new(0));
            let reopened = SessionRuntime::open(
                tmp.path().to_path_buf(),
                &state_dir,
                DaemonGeneration::new(),
                CountingGit {
                    calls: Arc::clone(&git_calls),
                },
                SystemSessionWorktreeIo,
            );

            assert!(matches!(reopened, Err(SessionRuntimeError::Storage)));
            assert!(sentinel.exists(), "malicious persisted name was {name}");
            assert_eq!(git_calls.load(Ordering::SeqCst), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn real_teardown_rejects_a_symlinked_session_ancestor_with_zero_effect() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let repository = tmp.path().join("repository");
        let data_home = tmp.path().join("data");
        let victim_session = tmp.path().join("victim/one");
        std::fs::create_dir_all(repository.join(STATE_DIR)).unwrap();
        std::fs::create_dir_all(&data_home).unwrap();
        std::fs::create_dir_all(&victim_session).unwrap();
        let sentinel = victim_session.join("sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        let container = repository.join(STATE_DIR).join(SESSIONS_DIR);
        symlink(tmp.path().join("victim"), &container).unwrap();
        let git_calls = Arc::new(AtomicUsize::new(0));
        let teardown = PendingTeardown {
            session_id: SessionId::new(),
            operation_id: OperationId::new(),
            name: "one".into(),
            repository_root: repository,
            data_home,
            session_container: container.clone(),
            session_root: container.join("one"),
            force: true,
            delete_branch: false,
            force_delete_branch: false,
            merged_head_oid: None,
        };

        let result = WorktreeTeardown::new(
            CountingGit {
                calls: Arc::clone(&git_calls),
            },
            SystemSessionWorktreeIo,
        )
        .tear_down(&teardown);

        assert!(result.unwrap_err().contains("symlinked session ancestor"));
        assert!(sentinel.exists());
        assert_eq!(git_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn protected_roots_are_never_valid_teardown_targets() {
        let repository = Path::new("/repository");
        let data_home = Path::new("/data");
        assert!(protected_teardown_target(repository, repository, data_home));
        assert!(protected_teardown_target(data_home, repository, data_home));
        assert!(protected_teardown_target(
            Path::new("/"),
            repository,
            data_home
        ));
        assert!(!protected_teardown_target(
            Path::new("/repository/.usagi/sessions/one"),
            repository,
            data_home,
        ));
    }

    #[test]
    fn teardown_confinement_errors_have_zero_git_and_filesystem_effects() {
        let git_calls = Arc::new(AtomicUsize::new(0));
        let remove_calls = Arc::new(AtomicUsize::new(0));

        let mut invalid_name = confined_teardown();
        invalid_name.name = "../victim".into();
        let mut mismatched_shape = confined_teardown();
        mismatched_shape.session_root = PathBuf::from("/repo");

        let cases = [
            (invalid_name, ConfinementIo::new(Arc::clone(&remove_calls))),
            (
                mismatched_shape,
                ConfinementIo::new(Arc::clone(&remove_calls)),
            ),
            {
                let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
                io.canonical.insert(PathBuf::from("/repo"), None);
                (confined_teardown(), io)
            },
            {
                let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
                io.canonical.insert(PathBuf::from("/data"), None);
                (confined_teardown(), io)
            },
            {
                let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
                io.canonical
                    .insert(PathBuf::from("/repo/.usagi/sessions"), None);
                (confined_teardown(), io)
            },
            {
                let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
                io.canonical.insert(
                    PathBuf::from("/repo/.usagi/sessions"),
                    Some(PathBuf::from("/escape")),
                );
                (confined_teardown(), io)
            },
            {
                let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
                io.canonical.insert(
                    PathBuf::from("/data"),
                    Some(PathBuf::from("/repo/.usagi/sessions")),
                );
                (confined_teardown(), io)
            },
            {
                let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
                io.occupied = true;
                io.canonical
                    .insert(PathBuf::from("/repo/.usagi/sessions/one"), None);
                (confined_teardown(), io)
            },
            {
                let mut io = ConfinementIo::new(Arc::clone(&remove_calls));
                io.occupied = true;
                io.canonical.insert(
                    PathBuf::from("/repo/.usagi/sessions/one"),
                    Some(PathBuf::from("/victim")),
                );
                (confined_teardown(), io)
            },
        ];

        for (teardown, io) in cases {
            assert!(
                WorktreeTeardown::new(
                    CountingGit {
                        calls: Arc::clone(&git_calls),
                    },
                    io,
                )
                .tear_down(&teardown)
                .is_err()
            );
        }
        assert_eq!(git_calls.load(Ordering::SeqCst), 0);
        assert_eq!(remove_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn teardown_confinement_preserves_absent_target_idempotency() {
        let remove_calls = Arc::new(AtomicUsize::new(0));
        let io_contract = ConfinementIo::new(Arc::clone(&remove_calls));
        io_contract.remove_file_best_effort(Path::new("/unused"));
        assert!(!io_contract.is_repo_root(Path::new("/unused")));
        assert!(!io_contract.is_linked_worktree(Path::new("/unused")));
        io_contract
            .build_session_tree(
                &FakeGit::ok(),
                Path::new("/source"),
                Path::new("/destination"),
                "branch",
            )
            .unwrap();
        WorktreeTeardown::new(FakeGit::ok(), io_contract)
            .tear_down(&confined_teardown())
            .unwrap();
        assert_eq!(remove_calls.load(Ordering::SeqCst), 1);

        let mut occupied = ConfinementIo::new(Arc::clone(&remove_calls));
        occupied.occupied = true;
        WorktreeTeardown::new(FakeGit::ok(), occupied)
            .tear_down(&confined_teardown())
            .unwrap();
        assert_eq!(remove_calls.load(Ordering::SeqCst), 2);
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
            &WorktreeTeardown::new(FakeGit::ok(), SystemSessionWorktreeIo),
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

    #[test]
    #[allow(clippy::too_many_lines)]
    fn production_logic_coverage_contract() {
        let path = Path::new("/fake");
        let failing_io = FailingSessionWorktreeIo;
        failing_io.remove_file_best_effort(path);
        assert!(!failing_io.path_occupied(path));
        assert_eq!(failing_io.canonical_path(path), Some(path.into()));
        assert!(!failing_io.is_repo_root(path));
        assert!(!failing_io.is_linked_worktree(path));
        failing_io
            .build_session_tree(&FakeGit::ok(), path, path, "branch")
            .unwrap();
        let fake_io = FakeSessionWorktreeIo {
            occupied: false,
            build_calls: Arc::new(AtomicUsize::new(0)),
        };
        assert_eq!(fake_io.canonical_path(path), Some(path.into()));
        assert!(fake_io.is_linked_worktree(path));
        fake_io
            .remove_session_tree(&FakeGit::ok(), path, false)
            .unwrap();
        PoisoningGit {
            runtime: std::sync::Weak::new(),
        }
        .run(path, &[])
        .unwrap();

        let messages = [
            (
                SessionRuntimeError::InvalidRequest,
                "invalid session request",
            ),
            (
                SessionRuntimeError::InvalidOperation,
                "invalid operation identity",
            ),
            (
                SessionRuntimeError::DuplicateOperation,
                "operation identity conflicts with an existing request",
            ),
            (
                SessionRuntimeError::IdempotencyConflict,
                "operation id was reused with a different request",
            ),
            (
                SessionRuntimeError::AgentFailure {
                    code: ErrorCode::Internal,
                    message: "agent".into(),
                },
                "agent",
            ),
            (
                SessionRuntimeError::ScopeUnavailable,
                "session scope is not available",
            ),
            (SessionRuntimeError::UnknownSession, "session was not found"),
            (
                SessionRuntimeError::Rejected,
                "could not create the session worktree; see the daemon log for details",
            ),
            (
                SessionRuntimeError::Storage,
                "daemon could not persist session lifecycle state",
            ),
            // A delegation failure reports the dispatch refusal it wraps; the
            // reconcile state travels in `details`, not in the message.
            (
                SessionRuntimeError::Delegation(DelegationFailure {
                    code: ErrorCode::Unavailable,
                    message: "dispatch runtime executable is unavailable".into(),
                    session_id: SessionId::new(),
                    run_operation_id: OperationId::new().to_string(),
                    reconcile: DelegationReconcile::Compensated,
                }),
                "dispatch runtime executable is unavailable",
            ),
        ];
        for (error, expected) in messages {
            assert_eq!(error.safe_message(), expected);
        }
        assert_eq!(
            worktree_failure_detail("\u{1}"),
            "Git rejected workspace creation"
        );
        assert_eq!(
            session_name(&json!({"name": ""})),
            Err(SessionRuntimeError::InvalidRequest)
        );
        assert_eq!(session_name(&json!({"label": "alias"})), Ok("alias".into()));
        assert_eq!(
            WorktreeTeardown::new(FakeGit::ok(), FailingSessionWorktreeIo).tear_down(
                &PendingTeardown {
                    session_id: SessionId::new(),
                    operation_id: OperationId::new(),
                    name: "one".into(),
                    repository_root: PathBuf::from("/repo"),
                    data_home: PathBuf::from("/data"),
                    session_container: PathBuf::from("/repo/.usagi/sessions"),
                    session_root: PathBuf::from("/repo/.usagi/sessions/one"),
                    force: false,
                    delete_branch: false,
                    force_delete_branch: false,
                    merged_head_oid: None,
                }
            ),
            Err("injected remove failure".into())
        );

        let (tmp, mut runtime) = runtime(FakeGit::ok());
        let state = runtime.state().unwrap();
        // A daemon holding several workspaces routes by this identity, so it is
        // readable without resolving a scope first.
        assert_eq!(runtime.workspace_id().unwrap(), state.workspace_id);
        assert_eq!(
            runtime.resolve_root_scope(WorkspaceId::new(), runtime.root_worktree_id()),
            Err(SessionRuntimeError::ScopeUnavailable)
        );
        let create = runtime
            .handle(
                SessionAction::Create,
                &operation(),
                &json!({"name":"scope"}),
            )
            .unwrap();
        let session_id = create.body["sessions"][0]["session_id"]
            .as_str()
            .and_then(|value| SessionId::parse(value).ok())
            .unwrap();
        let scope = runtime.session_scope_by_id(session_id).unwrap();
        assert_eq!(
            scope.path,
            tmp.path().join(STATE_DIR).join(SESSIONS_DIR).join("scope")
        );
        assert_eq!(
            runtime.session_scope_by_id(SessionId::new()),
            Err(SessionRuntimeError::UnknownSession)
        );
        assert_eq!(
            runtime
                .resolve_root_scope(state.workspace_id, runtime.root_worktree_id())
                .unwrap(),
            tmp.path()
        );

        let remove_operation = operation();
        runtime
            .handle(
                SessionAction::Remove,
                &remove_operation,
                &json!({"name":"scope"}),
            )
            .unwrap();
        runtime
            .handle(
                SessionAction::Remove,
                &remove_operation,
                &json!({"name":"scope"}),
            )
            .unwrap();

        let _ = runtime
            .begin_create(
                CreateOrigin::Direct,
                &operation(),
                &json!({"name":"unfinished"}),
            )
            .unwrap();
        let current = runtime.state().unwrap();
        let journal = current.operations.last().unwrap();
        assert_eq!(
            runtime.replay(&current, journal),
            Err(SessionRuntimeError::DurableFailure(
                "session operation did not complete; explicit recovery required".into()
            ))
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn status_projects_each_git_state_and_failure() {
        use ScriptedGitResult::Output;

        let tmp = tempfile::tempdir().unwrap();
        let git = ScriptedGit::new([
            Output {
                success: true,
                stdout: "",
                stderr: "",
            },
            Output {
                success: true,
                stdout: "",
                stderr: "",
            },
            Output {
                success: true,
                stdout: "",
                stderr: "",
            },
            Output {
                success: true,
                stdout: "main\n",
                stderr: "",
            },
            Output {
                success: true,
                stdout: " M file\n",
                stderr: "",
            },
            Output {
                success: true,
                stdout: "usagi/dirty\n",
                stderr: "",
            },
            Output {
                success: false,
                stdout: "",
                stderr: "",
            },
            Output {
                success: true,
                stdout: "",
                stderr: "",
            },
            Output {
                success: true,
                stdout: "usagi/synced\n",
                stderr: "",
            },
            Output {
                success: true,
                stdout: "",
                stderr: "",
            },
            Output {
                success: true,
                stdout: "",
                stderr: "",
            },
            Output {
                success: true,
                stdout: "usagi/local\n",
                stderr: "",
            },
            Output {
                success: false,
                stdout: "",
                stderr: "",
            },
        ]);
        let mut runtime = SessionRuntime::open(
            tmp.path().join("repository"),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            git,
            FakeSessionWorktreeIo {
                occupied: false,
                build_calls: Arc::new(AtomicUsize::new(0)),
            },
        )
        .unwrap();
        for name in ["dirty", "synced", "local"] {
            runtime
                .handle(SessionAction::Create, &operation(), &json!({"name": name}))
                .unwrap();
        }
        let reply = runtime
            .handle(SessionAction::Status, &operation(), &json!({}))
            .unwrap();
        assert_eq!(reply.body["sessions"][0]["worktrees"][0]["status"], "dirty");
        assert_eq!(
            reply.body["sessions"][1]["worktrees"][0]["status"],
            "synced"
        );
        assert_eq!(reply.body["sessions"][2]["worktrees"][0]["status"], "local");

        let tmp = tempfile::tempdir().unwrap();
        let mut runtime = SessionRuntime::open(
            tmp.path().join("repository"),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            ScriptedGit::new([Output {
                success: false,
                stdout: "",
                stderr: "",
            }]),
            FakeSessionWorktreeIo {
                occupied: false,
                build_calls: Arc::new(AtomicUsize::new(0)),
            },
        )
        .unwrap();
        assert_eq!(
            runtime.handle(SessionAction::Status, &operation(), &json!({})),
            Err(SessionRuntimeError::Storage)
        );

        let tmp = tempfile::tempdir().unwrap();
        let mut runtime = SessionRuntime::open(
            tmp.path().join("repository"),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            ScriptedGit::new([
                Output {
                    success: true,
                    stdout: "",
                    stderr: "",
                },
                Output {
                    success: true,
                    stdout: "main\n",
                    stderr: "",
                },
                Output {
                    success: false,
                    stdout: "",
                    stderr: "",
                },
                Output {
                    success: true,
                    stdout: "usagi/one\n",
                    stderr: "",
                },
                Output {
                    success: false,
                    stdout: "",
                    stderr: "",
                },
            ]),
            FakeSessionWorktreeIo {
                occupied: false,
                build_calls: Arc::new(AtomicUsize::new(0)),
            },
        )
        .unwrap();
        runtime
            .handle(SessionAction::Create, &operation(), &json!({"name":"one"}))
            .unwrap();
        assert_eq!(
            runtime.handle(SessionAction::Status, &operation(), &json!({})),
            Err(SessionRuntimeError::Storage)
        );

        let tmp = tempfile::tempdir().unwrap();
        let mut runtime = SessionRuntime::open(
            tmp.path().join("repository"),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            ScriptedGit::new([ScriptedGitResult::Error, ScriptedGitResult::Error]),
            FakeSessionWorktreeIo {
                occupied: false,
                build_calls: Arc::new(AtomicUsize::new(0)),
            },
        )
        .unwrap();
        assert!(matches!(
            runtime.handle(
                SessionAction::Create,
                &operation(),
                &json!({"name":"error"})
            ),
            Err(SessionRuntimeError::SessionWorkspaceCreationFailed { .. })
        ));
        assert_eq!(
            runtime.handle(SessionAction::Status, &operation(), &json!({})),
            Err(SessionRuntimeError::Storage)
        );
    }

    #[test]
    fn unowned_reconcile_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let repository = tmp.path().join("repository");
        let state_dir = tmp.path().join("daemon");
        let mut runtime = SessionRuntime::open(
            repository.clone(),
            &state_dir,
            DaemonGeneration::new(),
            FakeGit::ok(),
            SystemSessionWorktreeIo,
        )
        .unwrap();
        let mut state = runtime.state().unwrap();
        let mut unowned =
            ManagedSession::new_creating("unowned".into(), OperationId::new(), Utc::now());
        unowned.operation_id = None;
        state.sessions.push(unowned);
        let revision = state.state_revision;
        state.state_revision += 1;
        runtime.store.replace_if_revision(revision, &state).unwrap();
        runtime.reconcile().unwrap();
    }

    #[test]
    fn shared_teardown_reports_storage_failure() {
        let (tmp, runtime) = runtime(FakeGit::ok());
        let shared = Arc::new(Mutex::new(runtime));
        std::fs::write(tmp.path().join("daemon/sessions.json"), "not json").unwrap();
        let pending = PendingTeardown {
            session_id: SessionId::new(),
            operation_id: OperationId::new(),
            name: "missing".into(),
            repository_root: tmp.path().into(),
            data_home: tmp.path().into(),
            session_container: tmp.path().join(STATE_DIR).join(SESSIONS_DIR),
            session_root: tmp.path().join("missing"),
            force: false,
            delete_branch: false,
            force_delete_branch: false,
            merged_head_oid: None,
        };
        assert_eq!(
            SharedSessionTeardown::new(shared).finish(&pending, Ok(())),
            Err("daemon could not persist session lifecycle state".into())
        );
    }
}
