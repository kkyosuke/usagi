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

    // The default branch is a per-repository property shared by every worktree
    // of that repository, so resolve it once here rather than re-deriving it
    // (another `git` process) inside every `inspect_worktree`. A git-repository
    // workspace gives each session a single worktree on this repository, so its
    // default applies to them all.
    let default = git::default_branch(&root);

    // Each `inspect_worktree` still shells out to git; refresh the worktrees in
    // parallel so a multi-session workspace is not bottlenecked on a long
    // sequence of git subprocesses.
    use rayon::prelude::*;
    for session in &mut state.sessions {
        session
            .worktrees
            .par_iter_mut()
            .for_each(|wt| *wt = inspect_worktree(&wt.path, &default));
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

/// Build the [`WorktreeState`] of a single worktree at `path`, classifying its
/// branch against `default` — the default branch of the worktree's repository,
/// resolved once by the caller (a workspace may span repositories with differing
/// defaults, so the caller passes the one that applies here).
///
/// The branch, HEAD, upstream, and dirtiness are read in a single git call
/// ([`git::worktree_status`]); a `None` (not a git worktree) yields an empty,
/// branch-less state.
pub fn inspect_worktree(path: &Path, default: &str) -> WorktreeState {
    let status = git::worktree_status(path).unwrap_or(git::WorktreeStatus {
        head: String::new(),
        branch: None,
        upstream: None,
        dirty: false,
    });
    let classification = classify(
        path,
        status.branch.as_deref(),
        default,
        status.upstream.is_some(),
        status.dirty,
    );
    WorktreeState {
        branch: status.branch,
        path: path.to_path_buf(),
        head: git::short_hash(&status.head),
        primary: false,
        upstream: status.upstream,
        status: classification,
        updated_at: Utc::now(),
    }
}

/// Derive a branch's lifecycle status from its working tree, its commits
/// relative to the default branch, and whether it has an upstream.
///
/// The order of checks:
///
/// 1. **dirty** — an uncommitted change in the working tree wins regardless of
///    commit topology: there is work here that has not been committed.
/// 2. Otherwise, by commits *ahead of* the default branch (commits of its own):
///    - **ahead > 0** → `pushed` if it has an upstream, else `local`.
///    - **ahead == 0** → `synced` if the default has moved past it (behind > 0),
///      else `new` (even with the default: freshly cut, no work yet).
///
/// A branch equal to the default branch (or a detached HEAD) is never compared
/// against itself, so its ahead/behind counts are not consulted; it falls
/// through to `local` / `pushed` by its upstream state. The default is resolved
/// against the remote (`origin/<default>`) first inside [`git::ahead_behind`],
/// so the status reflects what has landed on the remote integration branch even
/// before a local fetch.
fn classify(
    repo: &Path,
    branch: Option<&str>,
    default: &str,
    has_upstream: bool,
    dirty: bool,
) -> BranchStatus {
    if dirty {
        return BranchStatus::Dirty;
    }
    // Only a real branch other than the default is measured against the default;
    // the default branch and a detached HEAD skip the ahead/behind read.
    let counts = match branch {
        Some(branch) if branch != default => git::ahead_behind(repo, branch, default),
        _ => None,
    };
    if let Some((ahead, behind)) = counts {
        if ahead == 0 {
            return if behind > 0 {
                BranchStatus::Synced
            } else {
                BranchStatus::New
            };
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

        let wt = inspect_worktree(dir.path(), "main");
        assert_eq!(wt.branch.as_deref(), Some("main"));
        // The worktree is on the repo's own default branch (clean, no upstream)
        // → local; the default is never measured against itself.
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
            display_name: None,
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
    fn classify_reports_new_for_a_freshly_cut_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // A branch cut from main with no commits of its own, and main has not
        // moved past it: even with the default → new (nothing done yet), NOT
        // synced. This is the freshly created session case.
        git(dir.path())
            .args(["branch", "feature"])
            .status()
            .unwrap();
        assert_eq!(
            classify(dir.path(), Some("feature"), "main", false, false),
            BranchStatus::New
        );
    }

    #[test]
    fn classify_reports_synced_when_the_default_moved_past_the_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // feature is cut from main, then main gains a commit feature does not
        // have: feature is behind with nothing of its own ahead → synced.
        git(dir.path())
            .args(["branch", "feature"])
            .status()
            .unwrap();
        std::fs::write(dir.path().join("on-main"), "y").unwrap();
        git(dir.path()).args(["add", "."]).status().unwrap();
        git(dir.path())
            .args(["commit", "-q", "-m", "main moves on"])
            .status()
            .unwrap();
        assert_eq!(
            classify(dir.path(), Some("feature"), "main", false, false),
            BranchStatus::Synced
        );
    }

    #[test]
    fn classify_reports_dirty_when_the_tree_has_uncommitted_changes() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        git(dir.path())
            .args(["branch", "feature"])
            .status()
            .unwrap();
        // Dirty wins over every commit-topology state, even a pushed upstream.
        assert_eq!(
            classify(dir.path(), Some("feature"), "main", true, true),
            BranchStatus::Dirty
        );
    }

    #[test]
    fn classify_reports_local_and_pushed_for_a_branch_with_its_own_commits() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // A branch with a commit ahead of main has un-merged work of its own.
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

        // No upstream → local; with an upstream → pushed.
        assert_eq!(
            classify(dir.path(), Some("feature"), "main", false, false),
            BranchStatus::Local
        );
        assert_eq!(
            classify(dir.path(), Some("feature"), "main", true, false),
            BranchStatus::Pushed
        );
    }

    #[test]
    fn classify_handles_detached_head_and_the_default_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // Detached HEAD (branch = None): ahead/behind is not consulted, so it is
        // only ever local/pushed by upstream state.
        assert_eq!(
            classify(dir.path(), None, "main", false, false),
            BranchStatus::Local
        );
        // The default branch is never measured against itself, so it cannot read
        // new/synced — only local/pushed.
        assert_eq!(
            classify(dir.path(), Some("main"), "main", false, false),
            BranchStatus::Local
        );
    }
}
