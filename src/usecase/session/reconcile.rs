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

    let repos = tree::source_repos(workspace_root);
    // List each repository's worktrees once up front rather than per stray:
    // pruning a stray only removes the worktree on that stray's unique branch
    // (the directory name), so a single listing stays valid across strays.
    let repo_worktrees: Vec<(PathBuf, Vec<git::WorktreeInfo>)> = repos
        .into_iter()
        .map(|repo| {
            let worktrees = git::list_worktrees(&repo)?;
            Ok((repo, worktrees))
        })
        .collect::<Result<_>>()?;
    let mut removed = Vec::new();

    for entry in fs::read_dir(&sessions_base).into_iter().flatten().flatten() {
        let stray = entry.path();
        if !stray.is_dir() {
            continue;
        }
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if recorded.contains(name.as_ref()) {
            continue;
        }
        prune_stray(&stray, &name, &repo_worktrees)?;
        removed.push(stray);
    }

    Ok(removed)
}

/// Force-remove one stray session directory `stray` whose session branch is
/// `branch`: unregister any worktree on that branch from each source repository,
/// drop the now-orphaned branch, then delete whatever files remain. Git steps
/// are best-effort (a stray may be partly torn down already); only deleting the
/// directory itself is allowed to fail the call.
///
/// `repo_worktrees` is each source repository paired with its worktrees, listed
/// once by the caller and shared across strays.
fn prune_stray(
    stray: &Path,
    branch: &str,
    repo_worktrees: &[(PathBuf, Vec<git::WorktreeInfo>)],
) -> Result<()> {
    for (repo, worktrees) in repo_worktrees {
        for wt in worktrees {
            if wt.branch.as_deref() == Some(branch) {
                // Untracked and possibly dirty: force the worktree out.
                let _ = git::remove_worktree(repo, &wt.path, true);
            }
        }
        let _ = git::delete_branch(repo, branch);
    }
    if stray.exists() {
        fs::remove_dir_all(stray).context(format!("failed to remove {}", stray.display()))?;
    }
    Ok(())
}
