//! Refresh and persist a workspace's session state.
//!
//! A workspace is described by its sessions (recorded by `usecase::session`).
//! This module re-reads the git status of every session's per-repository
//! worktree, derives each [`BranchStatus`], and writes the result to
//! `<repo>/.usagi/state.json`.

use std::path::Path;

use anyhow::Result;
use chrono::Utc;

use crate::domain::workspace_state::{BranchStatus, WorkspaceState, WorktreeState};
use crate::infrastructure::git;
use crate::infrastructure::workspace_store::WorkspaceStore;

/// Refresh the saved state for the repository containing `cwd`, persist it, and
/// return it. Every recorded session worktree's git status is recomputed; a
/// workspace with no sessions yields an empty (but saved) state.
pub fn sync(cwd: &Path) -> Result<WorkspaceState> {
    let root = git::primary_worktree(cwd)?;
    let store = WorkspaceStore::new(&root);
    let mut state = store.load()?.unwrap_or_default();

    for session in &mut state.sessions {
        for wt in &mut session.worktrees {
            *wt = inspect_worktree(&wt.path);
        }
    }
    state.updated_at = Utc::now();
    store.save(&state)?;
    Ok(state)
}

/// Load the persisted state for the repository containing `cwd`, if any.
pub fn load(cwd: &Path) -> Result<Option<WorkspaceState>> {
    let root = git::primary_worktree(cwd)?;
    WorkspaceStore::new(root).load()
}

/// Build the [`WorktreeState`] of a single worktree at `path`. Its branch is
/// classified against **its own repository's** default branch (resolved from the
/// worktree), since a workspace may span repositories with differing defaults.
pub fn inspect_worktree(path: &Path) -> WorktreeState {
    let (branch, head) = git::worktree_head(path).unwrap_or((None, String::new()));
    let default = git::default_branch(path);
    let upstream = branch.as_deref().and_then(|b| git::upstream_of(path, b));
    let status = classify(path, branch.as_deref(), &default, upstream.is_some());
    WorktreeState {
        branch,
        path: path.to_path_buf(),
        head: git::short_hash(&head),
        primary: false,
        upstream,
        status,
        updated_at: Utc::now(),
    }
}

/// Derive a branch's lifecycle status.
///
/// `synced` (up to date) takes priority over `pushed`, which takes priority over
/// `local`. A branch is `synced` when it has **no commits of its own** beyond the
/// default branch — every commit it carries is already on the integration branch
/// (it is an ancestor), so there is nothing un-merged. A branch equal to the
/// default branch is never compared against itself, so it is only ever `local`
/// or `pushed`.
fn classify(repo: &Path, branch: Option<&str>, default: &str, has_upstream: bool) -> BranchStatus {
    if let Some(branch) = branch {
        if branch != default && git::is_merged(repo, branch, default) {
            return BranchStatus::UpToDate;
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
    use crate::infrastructure::git::test_command as git;

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
    fn inspect_worktree_reports_branch_and_local_status() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());

        let wt = inspect_worktree(dir.path());
        assert_eq!(wt.branch.as_deref(), Some("main"));
        // The worktree is on the repo's own default branch → not "merged".
        assert_eq!(wt.status, BranchStatus::Local);
        assert_eq!(wt.upstream, None);
        assert_eq!(wt.head.len(), 7);
        assert!(!wt.primary);
    }

    #[test]
    fn sync_writes_an_empty_state_for_a_repo_without_sessions() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());

        let state = sync(dir.path()).unwrap();
        assert!(state.sessions.is_empty());
        assert!(dir.path().join(".usagi/state.json").exists());
        assert_eq!(load(dir.path()).unwrap().as_ref(), Some(&state));
    }

    #[test]
    fn sync_refreshes_recorded_session_worktrees() {
        use crate::domain::workspace_state::{SessionRecord, WorktreeState};
        use crate::infrastructure::workspace_store::WorkspaceStore;

        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // A real worktree on a feature branch stands in for a session worktree.
        let wt_path = dir.path().join(".usagi/sessions/wip");
        git(dir.path())
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "wip",
                wt_path.to_str().unwrap(),
            ])
            .status()
            .unwrap();

        // Seed a session whose recorded worktree has stale, empty git fields.
        let store = WorkspaceStore::new(dir.path());
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "wip".to_string(),
            root: wt_path.clone(),
            worktrees: vec![WorktreeState {
                branch: None,
                path: wt_path.clone(),
                head: String::new(),
                primary: false,
                upstream: None,
                status: BranchStatus::Local,
                updated_at: Utc::now(),
            }],
            created_at: Utc::now(),
        });
        store.save(&state).unwrap();

        // sync re-reads the worktree's git status from disk.
        let synced = sync(dir.path()).unwrap();
        assert_eq!(synced.sessions.len(), 1);
        let wt = &synced.sessions[0].worktrees[0];
        assert_eq!(wt.branch.as_deref(), Some("wip"));
        assert!(!wt.head.is_empty());
    }

    #[test]
    fn classify_reports_synced_for_an_ancestor_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // A branch with no commits of its own is an ancestor of main, so it has
        // nothing un-merged → synced (up to date).
        git(dir.path())
            .args(["branch", "feature"])
            .status()
            .unwrap();
        assert_eq!(
            classify(dir.path(), Some("feature"), "main", false),
            BranchStatus::UpToDate
        );
    }

    #[test]
    fn classify_reports_pushed_for_unmerged_tracked_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // A branch with a commit ahead of main is not merged.
        git(dir.path())
            .args(["checkout", "-q", "-b", "feature"])
            .status()
            .unwrap();
        std::fs::write(dir.path().join("ahead"), "y").unwrap();
        git(dir.path()).args(["add", "."]).status().unwrap();
        git(dir.path())
            .args(["commit", "-q", "-m", "ahead"])
            .status()
            .unwrap();

        // has_upstream = true, not merged → pushed.
        assert_eq!(
            classify(dir.path(), Some("feature"), "main", true),
            BranchStatus::Pushed
        );
    }

    #[test]
    fn classify_handles_detached_head_and_the_default_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // Detached HEAD (branch = None): never merged, only local/pushed.
        assert_eq!(
            classify(dir.path(), None, "main", false),
            BranchStatus::Local
        );
        // The default branch is never classified as merged against itself.
        assert_eq!(
            classify(dir.path(), Some("main"), "main", false),
            BranchStatus::Local
        );
    }
}
