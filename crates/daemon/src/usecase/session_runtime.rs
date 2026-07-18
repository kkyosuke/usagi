//! Durable daemon-owned managed-session runtime.
//!
//! The reducer and store in `usagi-core` deliberately have no process or git
//! dependency.  This adapter is their only daemon-side effect owner: it
//! durably reserves an operation before invoking git, then applies the exact
//! completion fence captured from the reservation.

#![coverage(off)] // daemon runtime integration boundary; exercised by fake-Git tests.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

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
use usagi_core::infrastructure::paths::{SESSIONS_DIR, STATE_DIR};
use usagi_core::infrastructure::persistence::json_file;
use usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore;
use usagi_core::infrastructure::store::state::WorkspaceStateStore;
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
    IdempotencyConflict,
    SessionBranchExists(String),
    SessionWorkspaceExists(String),
    SessionWorkspaceCreationFailed { name: String, detail: String },
    UnknownSession,
    ScopeUnavailable,
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
    /// Returns the repository root durably trusted by this daemon's session store.
    #[must_use]
    pub fn repository_root(&self) -> &Path {
        &self.repo_root
    }

    /// # Errors
    ///
    /// Returns an error when the lifecycle state cannot be loaded or initialized.
    pub fn open(
        candidate_repo_root: PathBuf,
        state_dir: &Path,
        generation: DaemonGeneration,
        git: G,
    ) -> Result<Self, SessionRuntimeError> {
        let store = DaemonLifecycleStore::new(state_dir);
        let repo_root = if let Some((repository_root, _)) = store
            .load_with_workspace()
            .map_err(|_| SessionRuntimeError::Storage)?
        {
            repository_root
        } else {
            let legacy_lifecycle = candidate_repo_root
                .join(STATE_DIR)
                .join("lifecycle-state.json");
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
        let mut runtime = Self {
            repo_root,
            generation,
            store,
            git,
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
        Ok(snapshot(&state))
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
        let semantic_key = semantic_key(SessionAction::Create, &name);
        if let Some(existing) = before
            .operations
            .iter()
            .find(|op| op.operation_id == operation_id)
        {
            if existing.semantic_key != semantic_key {
                return Err(SessionRuntimeError::IdempotencyConflict);
            }
            return Ok(SessionReply {
                operation_id: operation_id.to_string(),
                revision: existing.progress_revision,
                body: self.snapshot()?,
            });
        }
        // A failed or otherwise retained lifecycle record still owns the
        // session name. Report that concrete conflict before asking the
        // reducer to reserve it, rather than collapsing the reducer's
        // `DuplicateSessionName` into a generic rejection.
        if before.sessions.iter().any(|session| session.name == name) {
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
        let fence = fence(&reserved, session, operation_id);
        let path = self
            .repo_root
            .join(STATE_DIR)
            .join(SESSIONS_DIR)
            .join(&name);
        match build_session_tree(&self.git, &self.repo_root, &path, &format!("usagi/{name}")) {
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
            Err(error) => {
                let error = error.to_string();
                let branch_exists = error.contains("branch") && error.contains("already exists");
                let workspace_exists = !branch_exists && error.contains("already exists");
                let detail = worktree_failure_detail(&error);
                let _ = self.store.apply(
                    self.generation,
                    LifecycleEvent::Failed {
                        fence,
                        failure: Failure {
                            stage: FailureStage::Create,
                            summary: if branch_exists {
                                "session branch already exists".into()
                            } else if workspace_exists {
                                "session workspace already exists".into()
                            } else {
                                "worktree creation failed".into()
                            },
                        },
                    },
                    Utc::now(),
                );
                Err(if branch_exists {
                    SessionRuntimeError::SessionBranchExists(name)
                } else if workspace_exists {
                    SessionRuntimeError::SessionWorkspaceExists(name)
                } else {
                    SessionRuntimeError::SessionWorkspaceCreationFailed { name, detail }
                })
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
            return Ok(SessionReply {
                operation_id: operation_id.to_string(),
                revision: existing.progress_revision,
                body: snapshot(&before),
            });
        }
        let session = before
            .sessions
            .iter()
            .find(|session| session.name == name)
            .ok_or(SessionRuntimeError::UnknownSession)?;
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
        match remove_session_tree(&self.git, &path, force) {
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
        let candidates = validated_legacy_sessions(&self.repo_root, &state, &self.git)?;
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
                "sessions": snapshot(&recovered)["sessions"].clone(),
                "workspace_id": recovered.workspace_id,
            }),
        })
    }

    fn reconcile(&mut self) -> Result<(), SessionRuntimeError> {
        let state = self.state()?;
        for session in state.sessions.into_iter().filter(|session| {
            matches!(
                session.lifecycle,
                usagi_core::domain::session_lifecycle::SessionLifecycle::Creating
                    | usagi_core::domain::session_lifecycle::SessionLifecycle::Initializing
                    | usagi_core::domain::session_lifecycle::SessionLifecycle::Deleting
            )
        }) {
            let Some(operation_id) = session.operation_id else {
                continue;
            };
            let failure_stage = if session.lifecycle
                == usagi_core::domain::session_lifecycle::SessionLifecycle::Deleting
            {
                FailureStage::Delete
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
}

/// Adopt repository-local records only while creating the first shared daemon
/// state.  We validate the complete legacy set before writing `sessions.json`;
/// a malformed, duplicate, missing, or differently-bound record leaves no
/// partial v2 state for a later start to guess from.
fn adopt_legacy_workspace_sessions<G: GitRunner>(
    repository_root: &Path,
    git: &G,
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
fn validated_legacy_sessions<G: GitRunner>(
    repository_root: &Path,
    v2: &WorkspaceLifecycleState,
    git: &G,
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

fn validated_legacy_sessions_without_v2<G: GitRunner>(
    repository_root: &Path,
    git: &G,
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
    // A reservation is durable so a crashed daemon can reconcile it and replay
    // its operation safely, but it is not a usable session.  Publishing failed
    // (or otherwise non-available) reservations lets a failed create become a
    // selectable sidebar row.  Keep lifecycle recovery state durable while
    // projecting only checkouts that were actually created to clients.
    let sessions = state
        .sessions
        .iter()
        .filter(|session| {
            session.lifecycle == usagi_core::domain::session_lifecycle::SessionLifecycle::Available
        })
        .collect::<Vec<_>>();
    json!({"workspace_id": state.workspace_id, "revision": state.state_revision, "sessions": sessions})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
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
    fn runtime(git: FakeGit) -> (TempDir, SessionRuntime<FakeGit>) {
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
        assert!(
            runtime.snapshot().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            runtime.state().unwrap().sessions[0]
                .failure
                .as_ref()
                .unwrap()
                .summary,
            "session branch already exists"
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
        assert!(
            runtime.snapshot().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            runtime.state().unwrap().sessions[0]
                .failure
                .as_ref()
                .unwrap()
                .summary,
            "session workspace already exists"
        );
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
        let restarted = SessionRuntime::open(
            tmp.path().to_path_buf(),
            &tmp.path().join("daemon"),
            DaemonGeneration::new(),
            FakeGit::ok(),
        )
        .unwrap();
        let snapshot = restarted.snapshot().unwrap();
        assert_eq!(snapshot["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(
            restarted.state().unwrap().sessions[1]
                .failure
                .as_ref()
                .unwrap()
                .summary,
            "interrupted; explicit recovery required"
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
        let legacy_dir = repository.join(STATE_DIR);
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
        assert!(
            migrated.snapshot().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
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
}
