//! Durable daemon-owned managed-session runtime.
//!
//! The reducer and store in `usagi-core` deliberately have no process or git
//! dependency. This usecase durably reserves an operation before invoking
//! injected Git and filesystem ports, then applies the exact completion fence
//! captured from the reservation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::{Value, json};
use usagi_core::domain::agent::{CallerRef, ProviderResumeReason};
use usagi_core::domain::id::{
    AgentId, CompletionFence, DaemonGeneration, OperationId, SessionId, WorkspaceId, WorktreeId,
};
use usagi_core::domain::role::{EffectiveRoleCatalog, RoleId, RoleScope};
use usagi_core::domain::session_lifecycle::{
    AgentPhase, DeletePlan, Failure, FailureStage, LifecycleEvent, OperationJournal,
    OperationStatus, SetupPlan, WorkspaceLifecycleState, validate_session_name,
};
use usagi_core::infrastructure::git::{GitRunner, delete_branch};
use usagi_core::infrastructure::gitignore::migrate_usagi_ignore_rules;
use usagi_core::infrastructure::ipc::ErrorCode;
use usagi_core::infrastructure::ipc::SessionAction;
use usagi_core::infrastructure::paths::{SESSIONS_DIR, STATE_DIR, project_data_dir};
use usagi_core::infrastructure::persistence::json_file;
use usagi_core::infrastructure::runtime_model::WorkspaceSessionConfig;
use usagi_core::infrastructure::session_snapshot::{
    SessionListItem, SessionListSnapshot, SessionRuntimeObservation, SessionStatusItem,
    SessionStatusSnapshot, SessionWorktreeStatus,
};
use usagi_core::infrastructure::store::issue::AmbiguousIssueNumber;
use usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore;

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
    OrphanRecoveryBlocked(String),
    SessionWorkspaceCreationFailed { name: String, detail: String },
    DurableFailure(String),
    UnknownSession,
    ScopeUnavailable,
    PermissionDenied,
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
            Self::OrphanRecoveryBlocked(message)
            | Self::InvalidRole(message)
            | Self::DurableFailure(message)
            | Self::AgentFailure { message, .. }
            | Self::Delivery(message) => message.clone(),
            Self::AmbiguousIssue(error) => error.to_string(),
            Self::Delegation(failure) => failure.message.clone(),
            Self::UnknownSession => "session was not found".into(),
            Self::ScopeUnavailable => "session scope is not available".into(),
            Self::PermissionDenied => "caller did not create the target session".into(),
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
    /// Lists direct physical entries below the canonical session container.
    /// Invalid or non-UTF-8 names may be returned and are rejected by the
    /// usecase before any lifecycle adoption.
    ///
    /// # Errors
    ///
    /// Returns an error when the container cannot be enumerated safely.
    fn session_entries(&self, _container: &Path) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
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
        base_ref: Option<&str>,
    ) -> anyhow::Result<()>;
    /// Runs one configured setup command with `session_root` as its cwd.
    ///
    /// # Errors
    ///
    /// Returns an error when the shell cannot start or the command exits unsuccessfully.
    fn run_setup_command(&self, session_root: &Path, command: &str) -> anyhow::Result<()>;
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
    Pending(Box<SessionCreateInFlight>),
}

/// Outcome after the worktree effect has been durably recorded.
enum SessionCreateCompletion {
    Done(SessionReply),
    Initializing(SessionInitializeInFlight),
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
    base_ref: Option<String>,
    setup_commands: Vec<String>,
    io: Arc<dyn SessionWorktreeIo + Send + Sync>,
}

/// A durable `Initializing` session whose setup effect runs without the shared lock.
struct SessionInitializeInFlight {
    operation_id: OperationId,
    fence: CompletionFence,
    name: String,
    destination: PathBuf,
    commands: Vec<String>,
    io: Arc<dyn SessionWorktreeIo + Send + Sync>,
}

/// Live Git diagnosis for a physical session entry without a lifecycle owner.
/// Only booleans/counts and a validated local `usagi/` branch reach clients;
/// status filenames and raw Git stderr never cross the daemon boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanDiagnosis {
    branch: Option<String>,
    dirty: Option<bool>,
    unmerged_commits: Option<u64>,
    path_present: bool,
    linked_worktree: bool,
}

impl OrphanDiagnosis {
    fn safe_to_remove(&self) -> bool {
        !self.path_present
            || (self.linked_worktree
                && self.branch.is_some()
                && self.dirty == Some(false)
                && self.unmerged_commits == Some(0))
    }

    fn summary(&self, name: &str) -> String {
        if !self.path_present {
            return format!(
                "orphan session \"{name}\" no longer has a worktree; cleanup can remove its stale lifecycle row"
            );
        }
        let branch = self.branch.as_deref().unwrap_or("unknown");
        let dirty = self
            .dirty
            .map_or_else(|| "unknown".to_owned(), |dirty| dirty.to_string());
        let unmerged = self
            .unmerged_commits
            .map_or_else(|| "unknown".to_owned(), |commits| commits.to_string());
        let guidance = if !self.linked_worktree {
            "cleanup blocked: entry is not a registered Git worktree; inspect it manually"
        } else if self.dirty != Some(false) {
            "cleanup blocked: commit or stash local changes first"
        } else if self.unmerged_commits != Some(0) {
            "cleanup blocked: preserve the branch and open/merge a PR first"
        } else if self.branch.is_none() {
            "cleanup blocked: the checked-out branch is detached or outside the usagi/ namespace"
        } else {
            "safe cleanup is available"
        };
        format!(
            "orphan session \"{name}\": branch={branch}, dirty={dirty}, unmerged_commits={unmerged}; {guidance}"
        )
    }
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
        pending: Box<PendingTeardown>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemoveOptions {
    force: bool,
    force_delete_branch: bool,
    orphan: OrphanRemoveIntent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrphanRemoveIntent {
    Protect,
    Discard,
    Purge,
}

impl OrphanRemoveIntent {
    const fn discards(self) -> bool {
        !matches!(self, Self::Protect)
    }
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
            let completion = runtime
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .finish_create(*in_flight, result)?;
            match completion {
                SessionCreateCompletion::Done(reply) => Ok(reply),
                SessionCreateCompletion::Initializing(in_flight) => {
                    let result = SessionRuntime::execute_initialize(&in_flight);
                    runtime
                        .lock()
                        .map_err(|_| SessionRuntimeError::Storage)?
                        .finish_initialize(in_flight, result)
                }
            }
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
    let conventional = session_branch(&teardown.name);
    let mut branches = teardown.branch_name.iter().cloned().collect::<Vec<_>>();
    if !branches.contains(&conventional) {
        branches.push(conventional);
    }
    for branch in branches {
        let branch_ref = format!("refs/heads/{branch}");
        let squash_merged = teardown.merged_head_oid.as_deref().is_some_and(|expected| {
            git.run(
                &teardown.repository_root,
                &["rev-parse", "--verify", &branch_ref],
            )
            .is_ok_and(|output| output.success && output.stdout.trim() == expected)
        });
        delete_branch(
            git,
            &teardown.repository_root,
            &branch,
            teardown.force_delete_branch || squash_merged,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
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

    /// Number of durable managed sessions retained by this workspace.
    ///
    /// # Errors
    ///
    /// Returns [`SessionRuntimeError::Storage`] when the lifecycle document
    /// cannot be read.
    pub fn session_count(&self) -> Result<usize, SessionRuntimeError> {
        Ok(self.state()?.sessions.len())
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
        runtime.reconcile_orphan_worktrees()?;
        Ok(runtime)
    }

    /// # Errors
    ///
    /// Returns a typed safe error when the request cannot be admitted or completed.
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
            SessionAction::Clean
            | SessionAction::Sleep
            | SessionAction::Setup
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
            | SessionAction::DelegateBrief
            | SessionAction::WorkflowStart
            | SessionAction::WorkflowStatus
            | SessionAction::WorkflowInstruct
            | SessionAction::WorkflowFinish => Err(SessionRuntimeError::InvalidRequest),
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
                Ok(SessionStatusItem {
                    name: session.name.clone(),
                    session_id: session.session_id,
                    role_id: session.role_id.clone(),
                    role_summary: session.role_id.as_ref().and_then(|id| {
                        catalog
                            .as_ref()?
                            .roles
                            .get(id)
                            .map(|role| role.summary.clone())
                    }),
                    lifecycle: session.lifecycle,
                    parent_session_id: session.parent_session_id,
                    worktrees: vec![SessionWorktreeStatus {
                        path: root,
                        branch: branch.stdout.trim().to_owned(),
                        status: status.to_owned(),
                        dirty,
                        merged,
                    }],
                    runtime: unobserved_runtime(&session.name),
                })
            })
            .collect::<Result<Vec<_>, SessionRuntimeError>>()?;
        let body = serde_json::to_value(SessionStatusSnapshot {
            workspace_id: state.workspace_id,
            revision: state.state_revision,
            sessions,
        })
        .map_err(|_| SessionRuntimeError::Storage)?;
        Ok(SessionReply {
            operation_id: operation_id.to_owned(),
            revision: state.state_revision,
            body,
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

    /// Returns the sessions created by one exact authenticated Agent caller.
    /// Legacy and human-created sessions have no creator Agent and therefore
    /// never become implicitly owned by an Agent after an upgrade.
    ///
    /// # Errors
    ///
    /// Returns a storage error when lifecycle state cannot be read.
    pub fn created_session_ids(
        &self,
        caller: &CallerRef,
    ) -> Result<BTreeSet<SessionId>, SessionRuntimeError> {
        Ok(self
            .state()?
            .sessions
            .into_iter()
            .filter(|session| {
                session.parent_session_id == caller.session_id
                    && session.creator_agent_id == Some(caller.agent_id)
            })
            .map(|session| session.session_id)
            .collect())
    }

    /// Proves that one available named session was created by the exact
    /// authenticated Agent.
    ///
    /// # Errors
    ///
    /// Returns `UnknownSession` for an absent name and `PermissionDenied` when
    /// the durable creator does not match.
    pub fn created_session_id(
        &self,
        name: &str,
        caller: &CallerRef,
    ) -> Result<SessionId, SessionRuntimeError> {
        let session = self
            .state()?
            .sessions
            .into_iter()
            .find(|session| session.name == name)
            .ok_or(SessionRuntimeError::UnknownSession)?;
        if session.parent_session_id == caller.session_id
            && session.creator_agent_id == Some(caller.agent_id)
        {
            if session.lifecycle
                == usagi_core::domain::session_lifecycle::SessionLifecycle::Available
            {
                Ok(session.session_id)
            } else {
                Err(SessionRuntimeError::UnknownSession)
            }
        } else {
            Err(SessionRuntimeError::PermissionDenied)
        }
    }

    /// Proves ownership of a named durable lifecycle record regardless of its
    /// state. Removal uses this form because a creator must be able to clean up
    /// its own failed record without making that record usable by other tools.
    ///
    /// # Errors
    ///
    /// Returns `UnknownSession` for an absent name and `PermissionDenied` when
    /// the durable creator does not match.
    pub fn created_session_record_id(
        &self,
        name: &str,
        caller: &CallerRef,
    ) -> Result<SessionId, SessionRuntimeError> {
        let session = self
            .state()?
            .sessions
            .into_iter()
            .find(|session| session.name == name)
            .ok_or(SessionRuntimeError::UnknownSession)?;
        if session.parent_session_id == caller.session_id
            && session.creator_agent_id == Some(caller.agent_id)
        {
            Ok(session.session_id)
        } else {
            Err(SessionRuntimeError::PermissionDenied)
        }
    }

    /// Resolves the initialization failure created by one exact delegated operation.
    ///
    /// Composite delegation uses this after create returns an error: only a
    /// record whose operation, origin, name, creator, and terminal state all
    /// match may be compensated. A pre-effect role or idempotency error must
    /// never remove an older session that merely has the requested name.
    ///
    /// # Errors
    ///
    /// Returns a storage or invalid-operation error when the durable state or
    /// operation identity cannot be read.
    pub fn failed_delegated_initialize_id(
        &self,
        operation_id: &str,
        name: &str,
        caller: &CallerRef,
    ) -> Result<Option<SessionId>, SessionRuntimeError> {
        let operation_id =
            OperationId::parse(operation_id).map_err(|_| SessionRuntimeError::InvalidOperation)?;
        let state = self.state()?;
        let delegated_key = semantic_key(SessionAction::DelegateBrief, name);
        let failed_operation = state.operations.iter().any(|operation| {
            operation.operation_id == operation_id
                && operation.status == OperationStatus::Failed
                && names_session_operation(&operation.semantic_key, &delegated_key)
        });
        if !failed_operation {
            return Ok(None);
        }
        Ok(state
            .sessions
            .into_iter()
            .find(|session| {
                session.name == name
                    && session.operation_id == Some(operation_id)
                    && session.lifecycle
                        == usagi_core::domain::session_lifecycle::SessionLifecycle::Failed
                    && session
                        .failure
                        .as_ref()
                        .is_some_and(|failure| failure.stage == FailureStage::Initialize)
                    && session.parent_session_id == caller.session_id
                    && session.creator_agent_id == Some(caller.agent_id)
            })
            .map(|session| session.session_id))
    }

    /// Allows a new name, or proves that an existing record belongs to the
    /// exact Agent. This is the pre-effect guard for create-or-reuse tools.
    ///
    /// # Errors
    ///
    /// Returns `PermissionDenied` when another creator already owns the name.
    pub fn authorize_create_or_reuse(
        &self,
        name: &str,
        caller: &CallerRef,
    ) -> Result<(), SessionRuntimeError> {
        let Some(session) = self
            .state()?
            .sessions
            .into_iter()
            .find(|session| session.name == name)
        else {
            return Ok(());
        };
        if session.parent_session_id == caller.session_id
            && session.creator_agent_id == Some(caller.agent_id)
        {
            Ok(())
        } else {
            Err(SessionRuntimeError::PermissionDenied)
        }
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
                match self.finish_create(*in_flight, result)? {
                    SessionCreateCompletion::Done(reply) => Ok(reply),
                    SessionCreateCompletion::Initializing(in_flight) => {
                        let result = Self::execute_initialize(&in_flight);
                        self.finish_initialize(in_flight, result)
                    }
                }
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
        let requested_role = requested_role(payload)?;
        let parent_session_id = parent_session_id(payload)?;
        let creator_agent_id = creator_agent_id(payload)?;
        // Re-read both catalog layers at the daemon admission boundary; the
        // registered repository root is authoritative for workspace policy.
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
        // This check shares the create reservation's lifecycle lock. The
        // presentation-layer preflight gives an early effect-free refusal,
        // while this comparison closes the race where another Agent reserves
        // the same name between preflight and reservation.
        authorize_existing_create_record(existing_session, parent_session_id, creator_agent_id)?;
        if let Some(summary) = orphan_failure_summary(existing_session) {
            return Err(SessionRuntimeError::OrphanRecoveryBlocked(summary));
        }
        let role_id = resolve_create_role(&catalog, existing_session, requested_role.as_ref())?;
        let base_ref = session_base_ref(payload)?;
        let setup_commands = WorkspaceSessionConfig::read(&self.repo_root)
            .setup_commands()
            .to_vec();
        let semantic_key = create_semantic_key(
            origin,
            &name,
            role_id.as_ref(),
            parent_session_id,
            creator_agent_id,
            base_ref.as_deref(),
        );
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
            return Err(self.adopt_orphan_conflict(&name)?);
        }
        let operation = journal(operation_id, self.generation, semantic_key);
        let reserved = self
            .store
            .apply(
                self.generation,
                LifecycleEvent::ReserveCreate {
                    name: name.clone(),
                    role_id,
                    parent_session_id,
                    creator_agent_id,
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
        Ok(SessionCreateStep::Pending(Box::new(
            SessionCreateInFlight {
                operation_id,
                fence,
                branch: session_branch(&name),
                name,
                workspace_root: self.repo_root.clone(),
                destination: path,
                base_ref,
                setup_commands,
                io: Arc::clone(&self.io),
            },
        )))
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
            in_flight.base_ref.as_deref(),
        )
    }

    /// Records the durable outcome of a create whose worktree build already ran.
    /// Runs under the shared session lock.
    fn finish_create(
        &mut self,
        in_flight: SessionCreateInFlight,
        result: anyhow::Result<()>,
    ) -> Result<SessionCreateCompletion, SessionRuntimeError> {
        let SessionCreateInFlight {
            operation_id,
            fence: create_fence,
            name,
            destination,
            setup_commands,
            io,
            ..
        } = in_flight;
        // Other sessions may have advanced the workspace-wide revision while
        // this create's Git effect ran without the shared lock. Rebuild the
        // revision component from the current state while retaining every
        // stable identity in the admitted fence.
        let create_fence = self.refresh_session_fence(
            &create_fence,
            usagi_core::domain::session_lifecycle::SessionLifecycle::Creating,
        )?;
        match result {
            Ok(()) => {
                let setup_plan = (!setup_commands.is_empty()).then(|| SetupPlan {
                    commands: setup_commands.clone(),
                });
                let completed = self
                    .store
                    .apply(
                        self.generation,
                        LifecycleEvent::CreateCompleted {
                            fence: create_fence,
                            setup_plan,
                        },
                        Utc::now(),
                    )
                    .map_err(|_| SessionRuntimeError::Storage)?;
                if setup_commands.is_empty() {
                    return Ok(SessionCreateCompletion::Done(SessionReply {
                        operation_id: operation_id.to_string(),
                        revision: completed.state_revision,
                        body: snapshot(&completed, self.root_worktree_id),
                    }));
                }
                let session = completed
                    .sessions
                    .iter()
                    .find(|session| session.name == name)
                    .ok_or(SessionRuntimeError::Storage)?;
                let fence =
                    fence(&completed, session, operation_id).ok_or(SessionRuntimeError::Storage)?;
                Ok(SessionCreateCompletion::Initializing(
                    SessionInitializeInFlight {
                        operation_id,
                        fence,
                        name,
                        destination,
                        commands: setup_commands,
                        io,
                    },
                ))
            }
            Err(error) => {
                let error = error.to_string();
                let branch_exists = error.contains("branch") && error.contains("already exists");
                let workspace_exists = !branch_exists && error.contains("already exists");
                let detail = worktree_failure_detail(&error);
                let failure = if branch_exists {
                    SessionRuntimeError::SessionBranchExists(name)
                } else if workspace_exists {
                    SessionRuntimeError::SessionWorkspaceExists(name)
                } else {
                    SessionRuntimeError::SessionWorkspaceCreationFailed { name, detail }
                };
                let _ = self.store.apply(
                    self.generation,
                    LifecycleEvent::Failed {
                        fence: create_fence,
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

    /// Executes every configured command in order, retaining the first failed index.
    fn execute_initialize(in_flight: &SessionInitializeInFlight) -> Result<(), usize> {
        let mut first_failure = None;
        for (index, command) in in_flight.commands.iter().enumerate() {
            if in_flight
                .io
                .run_setup_command(&in_flight.destination, command)
                .is_err()
            {
                first_failure.get_or_insert(index);
            }
        }
        first_failure.map_or(Ok(()), Err)
    }

    /// Records the fenced terminal outcome of configured session initialization.
    fn finish_initialize(
        &mut self,
        in_flight: SessionInitializeInFlight,
        result: Result<(), usize>,
    ) -> Result<SessionReply, SessionRuntimeError> {
        let SessionInitializeInFlight {
            operation_id,
            fence,
            name,
            ..
        } = in_flight;
        // Setup also runs without the shared lock and can outlive unrelated
        // lifecycle mutations. Refresh only the workspace revision; all
        // operation/session incarnation fields remain fenced to this worker.
        let fence = self.refresh_session_fence(
            &fence,
            usagi_core::domain::session_lifecycle::SessionLifecycle::Initializing,
        )?;
        match result {
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
                    body: snapshot(&completed, self.root_worktree_id),
                })
            }
            Err(index) => {
                let summary = format!(
                    "cannot initialize session \"{name}\": setup command {} failed",
                    index + 1
                );
                let failure = SessionRuntimeError::DurableFailure(summary.clone());
                let _ = self.store.apply(
                    self.generation,
                    LifecycleEvent::Failed {
                        fence,
                        failure: Failure {
                            stage: FailureStage::Initialize,
                            summary,
                        },
                    },
                    Utc::now(),
                );
                Err(failure)
            }
        }
    }

    fn refresh_session_fence(
        &self,
        admitted: &CompletionFence,
        expected_lifecycle: usagi_core::domain::session_lifecycle::SessionLifecycle,
    ) -> Result<CompletionFence, SessionRuntimeError> {
        let state = self.state()?;
        if state.workspace_id != admitted.workspace_id {
            return Err(SessionRuntimeError::Storage);
        }
        let session_id = admitted.session_id.ok_or(SessionRuntimeError::Storage)?;
        let session = state
            .sessions
            .iter()
            .find(|session| {
                session.session_id == session_id
                    && session.operation_id == Some(admitted.operation_id)
                    && session.attempt == admitted.lifecycle_attempt
                    && session.lifecycle == expected_lifecycle
            })
            .ok_or(SessionRuntimeError::Storage)?;
        let refreshed = fence(&state, session, admitted.operation_id)
            .filter(|refreshed| {
                refreshed.owner_daemon_generation == admitted.owner_daemon_generation
                    && refreshed.execution_attempt == admitted.execution_attempt
            })
            .ok_or(SessionRuntimeError::Storage)?;
        Ok(refreshed)
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
        let expected_parent_session_id = parent_session_id(payload)?;
        let expected_creator_agent_id = creator_agent_id(payload)?;
        // A compensation is not a client request: it forces the removal and
        // branch deletion, and neither is negotiable through a payload. A
        // requested removal uses Git's safe `-d` mode unless the client pairs
        // `force_delete_branch` with `force`, which is what the TUI's forced
        // removals send.
        let options = remove_options(kind, payload)?;
        let operation_id =
            OperationId::parse(operation_id).map_err(|_| SessionRuntimeError::InvalidOperation)?;
        let before = self.state()?;
        let existing_session = before.sessions.iter().find(|session| session.name == name);
        authorize_existing_remove_record(
            existing_session,
            expected_parent_session_id,
            expected_creator_agent_id,
        )?;
        let semantic_key =
            remove_semantic_key(kind, &name, options.force, options.force_delete_branch);
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
                options.force,
                options.force_delete_branch,
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
        let session = existing_session.ok_or(SessionRuntimeError::UnknownSession)?;
        let integrity_orphan = session
            .failure
            .as_ref()
            .is_some_and(|failure| failure.stage == FailureStage::Integrity);
        if options.orphan == OrphanRemoveIntent::Purge && !integrity_orphan {
            return Err(SessionRuntimeError::InvalidRequest);
        }
        // A removal already in flight owns the worktree effect. Reporting its
        // operation instead of admitting a second one is what keeps a repeated
        // request (an impatient client, a retry with a fresh operation ID) from
        // running the teardown twice.
        if let Some(in_progress) = in_progress_remove(session, &before, self.root_worktree_id) {
            return Ok(in_progress);
        }
        let orphan_branch =
            self.orphan_branch_for_remove(session, &name, options.orphan.discards())?;
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
                        force: options.force,
                        delete_branch,
                        branch_name: orphan_branch.clone(),
                        force_delete_branch: options.force_delete_branch,
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
            pending: Box::new(PendingTeardown {
                session_id,
                operation_id,
                repository_root: self.repo_root.clone(),
                data_home: self.data_home.clone(),
                session_container: self.session_container(),
                session_root: self.session_root(&name),
                name,
                force: options.force,
                delete_branch,
                branch_name: orphan_branch,
                force_delete_branch: options.force_delete_branch,
                merged_head_oid,
            }),
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
                    branch_name: plan.branch_name.clone(),
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
            let failure_stage = if session.lifecycle
                == usagi_core::domain::session_lifecycle::SessionLifecycle::Initializing
            {
                FailureStage::Initialize
            } else {
                FailureStage::Create
            };
            self.store
                .apply(
                    self.generation,
                    LifecycleEvent::ReconcileInterrupted {
                        session_id: session.session_id,
                        operation_id,
                        stage: failure_stage,
                    },
                    Utc::now(),
                )
                .map_err(|_| SessionRuntimeError::Storage)?;
        }
        Ok(())
    }

    /// Adopts physical session entries which have no durable lifecycle owner.
    ///
    /// Adoption is deliberately fail-closed: the row is `Failed`, carries only
    /// safe Git metadata, and never resolves as an Agent scope. A later remove
    /// re-runs the diagnosis so cleanup cannot race newly-created work.
    fn reconcile_orphan_worktrees(&mut self) -> Result<(), SessionRuntimeError> {
        let known = self
            .state()?
            .sessions
            .into_iter()
            .map(|session| session.name)
            .collect::<std::collections::BTreeSet<_>>();
        let entries = self
            .io
            .session_entries(&self.session_container())
            .map_err(|_| SessionRuntimeError::Storage)?;
        for name in entries {
            if known.contains(&name) || validate_session_name(&name).is_err() {
                continue;
            }
            let diagnosis = self.inspect_orphan(&name);
            self.store
                .apply(
                    self.generation,
                    LifecycleEvent::AdoptOrphan {
                        name: name.clone(),
                        failure: Failure {
                            stage: FailureStage::Integrity,
                            summary: diagnosis.summary(&name),
                        },
                    },
                    Utc::now(),
                )
                .map_err(|_| SessionRuntimeError::Storage)?;
        }
        Ok(())
    }

    fn adopt_orphan_conflict(
        &mut self,
        name: &str,
    ) -> Result<SessionRuntimeError, SessionRuntimeError> {
        let diagnosis = self.inspect_orphan(name);
        let summary = diagnosis.summary(name);
        self.store
            .apply(
                self.generation,
                LifecycleEvent::AdoptOrphan {
                    name: name.to_owned(),
                    failure: Failure {
                        stage: FailureStage::Integrity,
                        summary: summary.clone(),
                    },
                },
                Utc::now(),
            )
            .map_err(|_| SessionRuntimeError::Storage)?;
        Ok(SessionRuntimeError::OrphanRecoveryBlocked(summary))
    }

    fn orphan_branch_for_remove(
        &self,
        session: &usagi_core::domain::session_lifecycle::ManagedSession,
        name: &str,
        discard: bool,
    ) -> Result<Option<String>, SessionRuntimeError> {
        if !session
            .failure
            .as_ref()
            .is_some_and(|failure| failure.stage == FailureStage::Integrity)
        {
            return Ok(None);
        }
        let diagnosis = self.inspect_orphan(name);
        if !discard && !diagnosis.safe_to_remove() {
            return Err(SessionRuntimeError::OrphanRecoveryBlocked(
                diagnosis.summary(name),
            ));
        }
        Ok(diagnosis.branch)
    }

    fn inspect_orphan(&self, name: &str) -> OrphanDiagnosis {
        let path = self.session_root(name);
        if !self.io.path_occupied(&path) {
            return OrphanDiagnosis {
                branch: None,
                dirty: None,
                unmerged_commits: None,
                path_present: false,
                linked_worktree: false,
            };
        }
        if !self.io.is_linked_worktree(&path) {
            return OrphanDiagnosis {
                branch: None,
                dirty: None,
                unmerged_commits: None,
                path_present: true,
                linked_worktree: false,
            };
        }
        let branch = self
            .git
            .run(&path, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok()
            .filter(|output| output.success)
            .map(|output| output.stdout.trim().to_owned())
            .filter(|branch| valid_orphan_branch(branch));
        let dirty = self
            .git
            .run(&path, &["status", "--porcelain"])
            .ok()
            .filter(|output| output.success)
            .map(|output| !output.stdout.trim().is_empty());
        let base = self
            .git
            .run(&self.repo_root, &["rev-parse", "HEAD"])
            .ok()
            .filter(|output| output.success)
            .map(|output| output.stdout.trim().to_owned());
        let unmerged_commits = base.and_then(|base| {
            let range = format!("{base}..HEAD");
            self.git
                .run(&path, &["rev-list", "--count", &range])
                .ok()
                .filter(|output| output.success)
                .and_then(|output| output.stdout.trim().parse().ok())
        });
        OrphanDiagnosis {
            branch,
            dirty,
            unmerged_commits,
            path_present: true,
            linked_worktree: true,
        }
    }
}

fn orphan_failure_summary(
    session: Option<&usagi_core::domain::session_lifecycle::ManagedSession>,
) -> Option<String> {
    session
        .and_then(|session| session.failure.as_ref())
        .filter(|failure| failure.stage == FailureStage::Integrity)
        .map(|failure| failure.summary.clone())
}

fn valid_orphan_branch(branch: &str) -> bool {
    branch.starts_with("usagi/")
        && branch.len() > "usagi/".len()
        && !branch.chars().any(char::is_control)
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

fn requested_role(payload: &Value) -> Result<Option<RoleId>, SessionRuntimeError> {
    payload
        .get("role")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| SessionRuntimeError::InvalidRequest)
}

fn parent_session_id(payload: &Value) -> Result<Option<SessionId>, SessionRuntimeError> {
    payload
        .get("parent_session_id")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| SessionRuntimeError::InvalidRequest)
}

fn creator_agent_id(payload: &Value) -> Result<Option<AgentId>, SessionRuntimeError> {
    payload
        .get("creator_agent_id")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| SessionRuntimeError::InvalidRequest)
}

fn authorize_existing_create_record(
    session: Option<&usagi_core::domain::session_lifecycle::ManagedSession>,
    parent_session_id: Option<SessionId>,
    creator_agent_id: Option<AgentId>,
) -> Result<(), SessionRuntimeError> {
    if let (Some(session), Some(creator_agent_id)) = (session, creator_agent_id)
        && (session.parent_session_id != parent_session_id
            || session.creator_agent_id != Some(creator_agent_id))
    {
        return Err(SessionRuntimeError::PermissionDenied);
    }
    Ok(())
}

fn resolve_create_role(
    catalog: &EffectiveRoleCatalog,
    existing: Option<&usagi_core::domain::session_lifecycle::ManagedSession>,
    requested: Option<&RoleId>,
) -> Result<Option<RoleId>, SessionRuntimeError> {
    if let Some(existing) = existing {
        let selected = requested.or(existing.role_id.as_ref());
        return selected
            .map(|role| {
                catalog
                    .resolve(Some(role), RoleScope::Session)
                    .map_err(|error| SessionRuntimeError::InvalidRole(error.to_string()))
            })
            .transpose()
            .map(Option::flatten);
    }
    catalog
        .resolve(requested, RoleScope::Session)
        .map_err(|error| SessionRuntimeError::InvalidRole(error.to_string()))
}

fn authorize_existing_remove_record(
    session: Option<&usagi_core::domain::session_lifecycle::ManagedSession>,
    parent_session_id: Option<SessionId>,
    creator_agent_id: Option<AgentId>,
) -> Result<(), SessionRuntimeError> {
    let Some(creator_agent_id) = creator_agent_id else {
        return Ok(());
    };
    let session = session.ok_or(SessionRuntimeError::UnknownSession)?;
    if session.parent_session_id != parent_session_id
        || session.creator_agent_id != Some(creator_agent_id)
    {
        return Err(SessionRuntimeError::PermissionDenied);
    }
    Ok(())
}

fn in_progress_remove(
    session: &usagi_core::domain::session_lifecycle::ManagedSession,
    state: &WorkspaceLifecycleState,
    root_worktree_id: WorktreeId,
) -> Option<SessionRemoveStep> {
    let operation_id = session.operation_id.filter(|_| {
        session.lifecycle == usagi_core::domain::session_lifecycle::SessionLifecycle::Deleting
    })?;
    Some(SessionRemoveStep::Settled(SessionReply {
        operation_id: operation_id.to_string(),
        revision: state.state_revision,
        body: snapshot(state, root_worktree_id),
    }))
}

fn session_base_ref(payload: &Value) -> Result<Option<String>, SessionRuntimeError> {
    let Some(value) = payload.get("base_ref").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let refname = value
        .as_str()
        .filter(|refname| {
            (refname.starts_with("refs/heads/") || refname.starts_with("refs/remotes/"))
                && !refname.ends_with('/')
                && !refname.ends_with('.')
                && !refname.contains("..")
                && !refname.contains("@{")
                && !refname.chars().any(|character| {
                    character.is_control()
                        || character.is_whitespace()
                        || "~^:?*[\\{}".contains(character)
                })
        })
        .ok_or(SessionRuntimeError::InvalidRequest)?;
    Ok(Some(refname.to_owned()))
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

/// Parse the explicit permission to discard a diagnosed integrity orphan. This is
/// intentionally separate from ordinary worktree `force`: an integrity row can
/// represent unregistered files or unmerged commits that need a stronger,
/// target-specific acknowledgement.
fn purge_orphan(payload: &Value) -> Result<bool, SessionRuntimeError> {
    match payload.get("purge_orphan") {
        Some(value) => value.as_bool().ok_or(SessionRuntimeError::InvalidRequest),
        None => Ok(false),
    }
}

fn remove_options(kind: RemoveKind, payload: &Value) -> Result<RemoveOptions, SessionRuntimeError> {
    let compensating = kind == RemoveKind::Compensating;
    let force = compensating || force(payload)?;
    let purge_orphan = purge_orphan(payload)?;
    let requested_force_delete_branch = force_delete_branch(payload)? || purge_orphan;
    if requested_force_delete_branch && !force {
        return Err(SessionRuntimeError::InvalidRequest);
    }
    Ok(RemoveOptions {
        force,
        force_delete_branch: compensating || requested_force_delete_branch,
        orphan: if purge_orphan {
            OrphanRemoveIntent::Purge
        } else if compensating || requested_force_delete_branch {
            OrphanRemoveIntent::Discard
        } else {
            OrphanRemoveIntent::Protect
        },
    })
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
fn create_semantic_key(
    origin: CreateOrigin,
    name: &str,
    role_id: Option<&RoleId>,
    parent_session_id: Option<SessionId>,
    creator_agent_id: Option<AgentId>,
    base_ref: Option<&str>,
) -> String {
    let action = semantic_key(origin.semantic_action(), name);
    let action = role_id.map_or_else(
        || action.clone(),
        |role_id| format!("{action}:{}", role_id.as_str()),
    );
    let action =
        parent_session_id.map_or(action.clone(), |parent| format!("{action}:parent={parent}"));
    let action = creator_agent_id.map_or(action.clone(), |creator| {
        format!("{action}:creator={creator}")
    });
    base_ref.map_or(action.clone(), |base_ref| {
        format!("{action}:base={base_ref}")
    })
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

fn unobserved_runtime(session_name: &str) -> SessionRuntimeObservation {
    SessionRuntimeObservation {
        agent_phase: AgentPhase::Absent,
        agent_resumable: false,
        agent_resume_reason: ProviderResumeReason::ProviderMetadataUnavailable,
        agent_status: None,
        parent_session_name: None,
        organization_depth: 1,
        organization_path: vec!["Director".to_owned(), session_name.to_owned()],
    }
}

fn projected_snapshot(
    state: &WorkspaceLifecycleState,
    root_worktree_id: WorktreeId,
    data_home: &Path,
    repo_root: &Path,
) -> Value {
    let catalog =
        usagi_core::infrastructure::role_catalog::load_effective(data_home, repo_root).ok();
    let sessions = state
        .sessions
        .iter()
        .cloned()
        .map(|mut session| {
            let role_summary = session.role_id.as_ref().and_then(|id| {
                catalog
                    .as_ref()?
                    .roles
                    .get(id)
                    .map(|role| role.summary.clone())
            });
            // Setup command bodies are durable recovery state, not client
            // observation data. Keep them out of typed list projections just
            // as mutation snapshots do below.
            session.setup_plan = None;
            SessionListItem {
                role_summary,
                session: session.into(),
                runtime: None.into(),
            }
        })
        .collect();
    serde_json::to_value(SessionListSnapshot {
        workspace_id: state.workspace_id,
        root_worktree_id,
        revision: state.state_revision,
        sessions,
    })
    .expect("typed lifecycle snapshot is serializable")
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
    let mut sessions = json!(state.sessions);
    for session in sessions
        .as_array_mut()
        .expect("managed sessions serialize as an array")
    {
        let session = session
            .as_object_mut()
            .expect("managed sessions serialize as objects");
        session.remove("creator_agent_id");
        // Setup command bodies belong to the trusted root configuration and
        // durable recovery state. Session-scoped callers may list lifecycle
        // rows, but never need the command text to act on their capabilities.
        session.remove("setup_plan");
    }
    json!({
        "workspace_id": state.workspace_id,
        "root_worktree_id": root_worktree_id,
        "revision": state.state_revision,
        "sessions": sessions,
    })
}

#[cfg(test)]
mod tests;
