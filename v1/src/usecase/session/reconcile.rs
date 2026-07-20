//! Reconcile the on-disk session tree with the sessions recorded in
//! `state.json`, quarantining strays left by interrupted creates, crashes, or
//! hand-edited state until their ownership can be reviewed explicitly.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::tree;
use crate::domain::workspace_state::{PendingSessionRemoval, SessionRemovalPhase, WorkspaceState};
use crate::infrastructure::git;
use crate::infrastructure::repo_paths::{SESSIONS_DIR, STATE_DIR};
use crate::infrastructure::workspace_store::WorkspaceStore;

/// Reconcile the on-disk session tree under `.usagi/sessions/` with the sessions
/// recorded in `state.json`. Every *directory* there that has no matching record
/// is a stray — left by an interrupted create, a hand-edited `state.json`, or a
/// crash — and is durably quarantined as an orphaned pending removal. Reconcile
/// never force-deletes it because the missing record means ownership cannot be
/// established safely. Loose files are left untouched.
///
/// Returns the stray directories newly quarantined by this pass.
///
/// This is the public, self-locking entry point: it acquires the workspace
/// store lock for the duration of the scan-and-quarantine so it never races a
/// concurrent writer. [`create`](super::create) and [`remove`](super::remove)
/// already hold that lock across their whole operation and call
/// [`reconcile_locked`] directly instead, so the load-and-destroy here cannot
/// delete a worktree another process has built but not yet recorded.
pub fn reconcile(workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let store = WorkspaceStore::new(workspace_root);
    let _lock = store.lock()?;
    reconcile_locked(workspace_root)
}

/// Reconcile assuming the caller already holds the workspace store lock (see
/// [`WorkspaceStore::lock`]). [`create`](super::create) and
/// [`remove`](super::remove) hold the lock across reconcile → build/teardown →
/// record so the whole sequence is serialised against other usagi processes;
/// they call this directly to avoid re-acquiring the non-reentrant lock.
pub(super) fn reconcile_locked(workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let sessions_base = workspace_root.join(STATE_DIR).join(SESSIONS_DIR);
    if !sessions_base.is_dir() {
        return Ok(Vec::new());
    }

    let store = WorkspaceStore::new(workspace_root);
    let mut state = store.load()?.unwrap_or_else(WorkspaceState::new);
    let recorded: HashSet<String> = state
        .sessions
        .iter()
        .map(|session| session.name.clone())
        .chain(
            state
                .pending_removals
                .iter()
                .map(|pending| pending.name.clone()),
        )
        .collect();

    // Cheap pre-check before the expensive rescan: match the recorded session
    // names against the directory names directly under `.usagi/sessions/`. When
    // every on-disk session directory is recorded there are no strays, so skip
    // the full `source_repos` walk and the per-repository `list_worktrees`
    // entirely — the common case on every `create`/`remove`.
    let strays: Vec<(PathBuf, String)> = fs::read_dir(&sessions_base)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            (
                entry.path(),
                entry.file_name().to_string_lossy().into_owned(),
            )
        })
        .filter(|(_, name)| !recorded.contains(name))
        .collect();
    if strays.is_empty() {
        return Ok(Vec::new());
    }

    let mut quarantined = Vec::new();
    for (stray, name) in strays {
        state.pending_removals.push(PendingSessionRemoval {
            name,
            root: stray.clone(),
            worktrees: Vec::new(),
            phase: SessionRemovalPhase::Orphaned,
        });
        quarantined.push(stray);
    }
    state.updated_at = chrono::Utc::now();
    store.save(&state)?;
    Ok(quarantined)
}

/// Each source repository under `workspace_root` paired with its worktrees,
/// listed once. Sharing a single listing across every stray is sound because
/// destroying a session only removes the worktree on that session's unique
/// branch (the directory name), so the listing stays valid across sessions.
pub(super) fn list_repo_worktrees(
    workspace_root: &Path,
) -> Result<Vec<(PathBuf, Vec<git::WorktreeInfo>)>> {
    tree::source_repos(workspace_root)
        .into_iter()
        .map(|repo| {
            let worktrees = git::list_worktrees(&repo)?;
            Ok((repo, worktrees))
        })
        .collect()
}

/// Physically destroy one session whose directory is `root` and whose branch is
/// `branch`: unregister any worktree on that branch from each source repository,
/// drop the now-orphaned branch, then delete whatever files remain under `root`.
/// With `force`, a dirty worktree may be discarded. Infrastructure failures
/// (including locked worktrees) still abort before the directory is deleted so
/// the durable caller can retain context and retry. Already-absent components
/// remain successful, making partial teardown idempotent.
///
/// Used by [`remove`](super::remove); reconcile quarantines unowned strays and
/// therefore never calls this destructive primitive.
/// `repo_worktrees` is each source repository paired with its worktrees, from
/// [`list_repo_worktrees`].
pub(super) fn discard_session(
    root: &Path,
    branch: &str,
    repo_worktrees: &[(PathBuf, Vec<git::WorktreeInfo>)],
    force: bool,
) -> Result<()> {
    // git reports worktree paths canonicalized (e.g. `/private/var/…` on macOS),
    // so compare against the session root in canonical form too; fall back to the
    // raw path when a directory no longer exists to be resolved.
    let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let root_canon = canon(root);
    for (repo, worktrees) in repo_worktrees {
        for wt in worktrees {
            // A worktree belongs to this session when it is on the session branch
            // *or* when it physically lives under the session root. The latter
            // catches a worktree left on an unexpected branch (e.g. one created
            // directly with `git worktree add -b other` at the session path): the
            // branch-only match used to skip it, so the directory was deleted but
            // its git registration stayed behind, dangling at the session path and
            // blocking any later session of the same name from being created.
            if wt.branch.as_deref() == Some(branch) || canon(&wt.path).starts_with(&root_canon) {
                git::remove_worktree(repo, &wt.path, force)?;
            }
        }
    }

    // Delete the session tree *before* pruning and dropping the branch below.
    // This ordering is what keeps the name reusable: a worktree whose directory
    // vanished out-of-band (a crash, a manual `rm`, an external cleanup) — or one
    // a forced/locked `worktree remove` above failed to unregister — leaves a
    // registration that still holds the session branch checked out, which makes
    // `git branch -D` refuse. Removing the directory first turns that into a
    // prunable registration, so the prune clears it and the branch is no longer
    // checked out anywhere; only then can it actually be deleted.
    if root.exists() {
        fs::remove_dir_all(root).context(format!("failed to remove {}", root.display()))?;
    }

    for (repo, _) in repo_worktrees {
        git::prune_worktrees(repo)?;
        if git::branch_exists(repo, branch) {
            git::delete_branch(repo, branch)?;
        }
    }
    Ok(())
}
