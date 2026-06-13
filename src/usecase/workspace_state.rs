//! Build and persist a repository's workspace state.
//!
//! This inspects the git repository containing a given directory, derives the
//! [`BranchStatus`] of every worktree, and writes the result to
//! `<repo>/.usagi/state.json`.

use std::path::Path;

use anyhow::Result;
use chrono::Utc;

use crate::domain::workspace_state::{BranchStatus, WorkspaceState, WorktreeState};
use crate::infrastructure::git;
use crate::infrastructure::workspace_store::WorkspaceStore;

/// Inspect the repository containing `cwd`, persist its state, and return it.
pub fn sync(cwd: &Path) -> Result<WorkspaceState> {
    let state = inspect(cwd)?;
    let root = git::primary_worktree(cwd)?;
    WorkspaceStore::new(root).save(&state)?;
    Ok(state)
}

/// Load the persisted state for the repository containing `cwd`, if any.
pub fn load(cwd: &Path) -> Result<Option<WorkspaceState>> {
    let root = git::primary_worktree(cwd)?;
    WorkspaceStore::new(root).load()
}

/// Build the current workspace state from git without persisting it.
pub fn inspect(cwd: &Path) -> Result<WorkspaceState> {
    let default = git::default_branch(cwd);
    let worktrees = git::list_worktrees(cwd)?;
    let now = Utc::now();

    let states = worktrees
        .into_iter()
        .enumerate()
        .map(|(idx, wt)| {
            let primary = idx == 0;
            let upstream = wt.branch.as_deref().and_then(|b| git::upstream_of(cwd, b));
            let status = classify(
                cwd,
                wt.branch.as_deref(),
                &default,
                upstream.is_some(),
                primary,
            );
            WorktreeState {
                branch: wt.branch,
                path: wt.path,
                head: git::short_hash(&wt.head),
                primary,
                upstream,
                status,
                updated_at: now,
            }
        })
        .collect();

    Ok(WorkspaceState {
        default_branch: default,
        worktrees: states,
        updated_at: now,
    })
}

/// Derive a branch's lifecycle status.
///
/// `merged` takes priority over `pushed`, which takes priority over `local`.
/// The primary worktree is never reported as `merged` against itself — it is
/// the integration branch, so it is only ever `local` or `pushed`.
fn classify(
    repo: &Path,
    branch: Option<&str>,
    default: &str,
    has_upstream: bool,
    primary: bool,
) -> BranchStatus {
    if let Some(branch) = branch {
        if !primary && branch != default && git::is_merged(repo, branch, default) {
            return BranchStatus::Merged;
        }
    }
    if has_upstream {
        BranchStatus::Pushed
    } else {
        BranchStatus::Local
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Build a git command isolated from any inherited repo-scoping env vars
    /// (so these tests still pass when run from inside a git hook).
    fn git(dir: &Path) -> Command {
        let mut command = Command::new("git");
        command.arg("-C").arg(dir);
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_COMMON_DIR",
            "GIT_PREFIX",
            "GIT_NAMESPACE",
        ] {
            command.env_remove(var);
        }
        command
    }

    /// Initialise a throwaway git repo with one commit on `main`.
    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let ok = git(dir).args(args).status().unwrap().success();
            assert!(ok, "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(dir.join("README.md"), "hi").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    #[test]
    fn inspect_reports_local_default_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());

        let state = inspect(dir.path()).unwrap();
        assert_eq!(state.default_branch, "main");
        assert_eq!(state.worktrees.len(), 1);

        let primary = &state.worktrees[0];
        assert!(primary.primary);
        assert_eq!(primary.branch.as_deref(), Some("main"));
        // No remote configured, so the default branch is purely local.
        assert_eq!(primary.status, BranchStatus::Local);
        assert_eq!(primary.upstream, None);
    }

    #[test]
    fn sync_writes_state_file_to_primary_worktree() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());

        let state = sync(dir.path()).unwrap();
        assert_eq!(state.worktrees.len(), 1);

        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.as_ref(), Some(&state));
        assert!(dir.path().join(".usagi/state.json").exists());
    }

    #[test]
    fn merged_branch_is_reported_as_merged() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // Create a branch with no new commits: its tip is an ancestor of main,
        // so it counts as merged.
        git(dir.path())
            .args(["branch", "feature"])
            .status()
            .unwrap();

        let state = inspect(dir.path()).unwrap();
        // Primary (main) worktree only; check the classifier directly for the
        // sibling branch since it isn't checked out as a worktree.
        let status = classify(dir.path(), Some("feature"), "main", false, false);
        assert_eq!(status, BranchStatus::Merged);
        assert_eq!(state.default_branch, "main");
    }
}
