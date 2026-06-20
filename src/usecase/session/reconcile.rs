//! Reconcile the on-disk session tree with the sessions recorded in
//! `state.json`, force-removing strays left by interrupted creates, crashes, or
//! hand-edited state.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::tree;
use crate::infrastructure::git;
use crate::infrastructure::workspace_store::WorkspaceStore;

/// Reconcile the on-disk session tree under `.usagi/sessions/` with the sessions
/// recorded in `state.json`. Every *directory* there that has no matching record
/// is a stray — left by an interrupted create, a hand-edited `state.json`, or a
/// crash — and is force-removed: its per-repository git worktrees are
/// unregistered, the session branch is dropped, and any copied files are
/// deleted, regardless of uncommitted changes. Loose files are left untouched.
///
/// Called at the start of [`create`](super::create) and [`remove`](super::remove)
/// so the tree never drifts from the recorded state. Returns the stray
/// directories that were removed.
pub fn reconcile(workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let sessions_base = workspace_root.join(".usagi").join("sessions");
    if !sessions_base.is_dir() {
        return Ok(Vec::new());
    }

    let store = WorkspaceStore::new(workspace_root);
    let recorded: HashSet<String> = store
        .load()?
        .map(|state| state.sessions.into_iter().map(|s| s.name).collect())
        .unwrap_or_default();

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

    let repo_worktrees = list_repo_worktrees(workspace_root)?;
    let mut removed = Vec::new();
    for (stray, name) in strays {
        // A stray is untracked and possibly dirty: force it out unconditionally.
        discard_session(&stray, &name, &repo_worktrees, true)?;
        removed.push(stray);
    }

    Ok(removed)
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
/// With `force`, a dirty worktree is discarded; without it git refuses one. Git
/// steps are best-effort (a session may be partly torn down already, or never
/// fully built); only deleting the directory itself is allowed to fail the call.
///
/// Shared by [`reconcile`] (pruning strays, always forced) and
/// [`remove`](super::remove) so the teardown procedure lives in one place.
/// `repo_worktrees` is each source repository paired with its worktrees, from
/// [`list_repo_worktrees`].
pub(super) fn discard_session(
    root: &Path,
    branch: &str,
    repo_worktrees: &[(PathBuf, Vec<git::WorktreeInfo>)],
    force: bool,
) -> Result<()> {
    for (repo, worktrees) in repo_worktrees {
        for wt in worktrees {
            if wt.branch.as_deref() == Some(branch) {
                let _ = git::remove_worktree(repo, &wt.path, force);
            }
        }
        let _ = git::delete_branch(repo, branch);
    }
    if root.exists() {
        fs::remove_dir_all(root).context(format!("failed to remove {}", root.display()))?;
    }
    Ok(())
}
