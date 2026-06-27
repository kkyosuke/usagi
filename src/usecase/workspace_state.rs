//! Refresh and persist a workspace's session state.
//!
//! A workspace is described by its sessions (recorded by `usecase::session`).
//! This module re-reads the git status of every session's per-repository
//! worktree, derives each [`BranchStatus`], and writes the result to
//! `<repo>/.usagi/state.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::Utc;

use crate::domain::workspace_state::{
    BranchStatus, DiffStat, SessionRecord, WorkspaceState, WorktreeState,
};
use crate::infrastructure::error_log::ErrorLog;
use crate::infrastructure::git;
use crate::infrastructure::workspace_store::WorkspaceStore;

/// Refresh the saved state for the repository containing `cwd`, persist it, and
/// return it. Every recorded session worktree's git status is recomputed; a
/// workspace with no sessions yields an empty (but saved) state.
pub fn sync(cwd: &Path) -> Result<WorkspaceState> {
    let root = git::primary_worktree(cwd)?;
    let store = WorkspaceStore::new(&root);

    // Read which worktrees to inspect, then run the expensive git inspection
    // (several subprocesses per worktree) **without holding the store lock**: it
    // reads nothing from the store, so holding the lock across it would block a
    // concurrent session create/remove for the whole git fan-out — long enough on
    // a large repo to risk the lock-acquire timeout. The lock is taken only for
    // the cheap load → scatter → save below.
    let recorded = store.load()?.unwrap_or_default();
    let paths: Vec<PathBuf> = recorded
        .sessions
        .iter()
        .flat_map(|s| s.worktrees.iter().map(|wt| wt.path.clone()))
        .collect();
    // The shared helper resolves each repository's default branch once and
    // refreshes the worktrees in parallel; index the results by path so they can
    // be matched onto whatever the locked re-load below holds.
    let refreshed: HashMap<PathBuf, WorktreeState> = paths
        .iter()
        .cloned()
        .zip(inspect_worktrees(&paths))
        .collect();

    // Now take the lock and re-load: a session may have been created or removed
    // while we were inspecting, so apply the refreshed states onto the *current*
    // on-disk state, matched by worktree path. A worktree added concurrently (not
    // in `refreshed`) keeps its recorded state and is picked up by the next sync.
    let _lock = store.lock()?;
    let mut state = store.load()?.unwrap_or_default();
    for session in &mut state.sessions {
        for wt in &mut session.worktrees {
            if let Some(updated) = refreshed.get(&wt.path) {
                *wt = updated.clone();
            }
        }
    }
    state.updated_at = Utc::now();
    store.save(&state)?;
    Ok(state)
}

/// Inspect a list of worktree paths into [`WorktreeState`]s, resolving each
/// worktree's repository default branch at most once.
///
/// The default branch is a per-repository property shared by every worktree of
/// that repository, so it is resolved once per repository (keyed by the
/// worktree's primary worktree) and reused — rather than shelling out for it
/// inside every [`inspect_worktree`]. The worktrees are then inspected in
/// parallel so a multi-repo, multi-session workspace is not bottlenecked on a
/// long sequence of git subprocesses. Used by both [`sync`] and session
/// recording so the two never drift into separate implementations.
pub fn inspect_worktrees(paths: &[PathBuf]) -> Vec<WorktreeState> {
    use rayon::prelude::*;

    // Map each worktree to its repository in parallel, then resolve each
    // distinct repository's default branch once.
    let repos: Vec<PathBuf> = paths
        .par_iter()
        .map(|path| git::primary_worktree(path).unwrap_or_else(|_| path.clone()))
        .collect();
    let mut defaults: HashMap<&Path, String> = HashMap::new();
    for repo in &repos {
        defaults
            .entry(repo.as_path())
            .or_insert_with(|| git::default_branch(repo));
    }

    paths
        .par_iter()
        .zip(repos.par_iter())
        .map(|(path, repo)| inspect_worktree(path, &defaults[repo.as_path()]))
        .collect()
}

/// Load the persisted state for the repository containing `cwd`, if any.
pub fn load(cwd: &Path) -> Result<Option<WorkspaceState>> {
    let root = git::primary_worktree(cwd)?;
    WorkspaceStore::new(root).load()
}

/// The sessions recorded in `<root>/.usagi/state.json` for the home screen's
/// immediate, git-free first paint, plus a notice when the state could not be
/// read.
///
/// The screen opens from this recorded state without touching git — syncing
/// every worktree's status on entry would block the first paint — and re-syncs
/// in the background afterwards (see the caller and [`sync`]). Read by the raw
/// `root` (a non-git multi-repo root has no `primary_worktree`), so no git is
/// touched here. This keeps the "recorded sessions, else empty-with-notice"
/// load policy in the usecase rather than the presentation layer.
pub fn recorded_sessions_for_display(root: &Path) -> (Vec<SessionRecord>, Option<String>) {
    match WorkspaceStore::new(root).load() {
        Ok(Some(state)) => (state.sessions, None),
        Ok(None) => (Vec::new(), None),
        Err(e) => (Vec::new(), Some(format!("Failed to load sessions: {e}"))),
    }
}

/// The sessions recorded straight in `<root>/.usagi/state.json` (read by `root`,
/// no git refresh), or `None` when none are saved or the file cannot be read.
///
/// Used for the home screen's post-terminal refresh, where `None` means "leave
/// the list as it is" — distinct from [`recorded_sessions_for_display`], which
/// surfaces a load error as a notice on first entry.
pub fn recorded_sessions(root: &Path) -> Option<Vec<SessionRecord>> {
    match WorkspaceStore::new(root).load() {
        Ok(state) => state.map(|s| s.sessions),
        // Unlike `recorded_sessions_for_display`, this refresh path has no notice
        // channel to surface a load failure on, so without recording it the error
        // would vanish entirely. Route it to the daily log before falling back to
        // "leave the list as it is".
        Err(e) => {
            ErrorLog::record(&format!(
                "failed to load recorded sessions from {}: {e}",
                root.display()
            ));
            None
        }
    }
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
    let diff = measure_diff(path, status.branch.as_deref(), default);
    WorktreeState {
        branch: status.branch,
        path: path.to_path_buf(),
        head: git::short_hash(&status.head),
        primary: false,
        upstream: status.upstream,
        status: classification,
        diff,
        updated_at: Utc::now(),
    }
}

/// Measure the worktree's cumulative diff against the default branch for the
/// sidebar `+N -M` badge, or `None` when there is nothing to show.
///
/// Only a real branch other than the default is measured — the default branch
/// itself and a detached HEAD report `None`, mirroring [`classify`]'s commit
/// counts. An empty diff (a session even with the default) also collapses to
/// `None`, so the badge and the persisted state only carry an actual diff.
fn measure_diff(repo: &Path, branch: Option<&str>, default: &str) -> Option<DiffStat> {
    match branch {
        Some(branch) if branch != default => {
            let (added, removed) = git::diff_stat(repo, default)?;
            let stat = DiffStat { added, removed };
            (!stat.is_empty()).then_some(stat)
        }
        _ => None,
    }
}

/// Gather the git facts a branch's lifecycle status is derived from — its
/// commits relative to the default branch — and hand them to
/// [`BranchStatus::derive`], which holds the (pure) classification rules.
///
/// Only a real branch other than the default is measured against the default;
/// the default branch and a detached HEAD skip the ahead/behind read. The
/// default is resolved against the remote (`origin/<default>`) first inside
/// [`git::ahead_behind`], so the status reflects what has landed on the remote
/// integration branch even before a local fetch.
fn classify(
    repo: &Path,
    branch: Option<&str>,
    default: &str,
    has_upstream: bool,
    dirty: bool,
) -> BranchStatus {
    let counts = match branch {
        Some(branch) if branch != default => git::ahead_behind(repo, branch, default),
        _ => None,
    };
    BranchStatus::derive(dirty, counts, has_upstream)
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
        // The default branch is never measured against itself, so no badge.
        assert_eq!(wt.diff, None);
    }

    #[test]
    fn inspect_worktree_measures_a_feature_branch_diff_and_skips_a_clean_one() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let run = |args: &[&str]| {
            assert!(git(dir.path()).args(args).status().unwrap().success());
        };
        // A feature branch with a committed two-line file, then an uncommitted
        // third line appended to it: +3 against the default overall.
        run(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("new.txt"), "a\nb\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "work"]);
        std::fs::write(dir.path().join("new.txt"), "a\nb\nc\n").unwrap();

        let wt = inspect_worktree(dir.path(), "main");
        assert_eq!(
            wt.diff,
            Some(DiffStat {
                added: 3,
                removed: 0
            })
        );

        // A branch even with the default carries no diff, so the badge collapses
        // to `None` rather than a `+0 -0`. Force past the uncommitted edit above.
        run(&["checkout", "-q", "-f", "main"]);
        run(&["checkout", "-q", "-b", "untouched"]);
        let clean = inspect_worktree(dir.path(), "main");
        assert_eq!(clean.diff, None);
    }

    #[test]
    fn inspect_worktrees_inspects_each_path_and_tolerates_non_git_paths() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // A real linked worktree on a feature branch alongside the repo itself.
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
        // A path that is not a git worktree falls back to itself as the repo and
        // yields a branch-less state rather than erroring.
        let plain = tempfile::tempdir().unwrap();

        let states = inspect_worktrees(&[
            dir.path().to_path_buf(),
            wt_path.clone(),
            plain.path().to_path_buf(),
        ]);

        assert_eq!(states.len(), 3);
        assert_eq!(states[0].branch.as_deref(), Some("main"));
        assert_eq!(states[1].branch.as_deref(), Some("wip"));
        assert_eq!(states[2].branch, None);
        assert!(states[2].head.is_empty());
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
            note: None,
            root: wt_path.clone(),
            worktrees: vec![WorktreeState {
                branch: None,
                path: wt_path.clone(),
                head: String::new(),
                primary: false,
                upstream: None,
                status: BranchStatus::Local,
                diff: None,
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

    /// A `SessionRecord` with no worktrees, enough to seed `state.json`.
    fn session(name: &str) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from(name),
            worktrees: Vec::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn recorded_sessions_reads_saved_state_and_is_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        // No state.json yet → None.
        assert!(recorded_sessions(dir.path()).is_none());

        // Saved state (no git needed) → its sessions, read by the raw root.
        let store = WorkspaceStore::new(dir.path());
        let mut state = WorkspaceState::new();
        state.sessions.push(session("wip"));
        store.save(&state).unwrap();
        let got = recorded_sessions(dir.path()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "wip");
    }

    #[test]
    fn recorded_sessions_is_none_when_state_is_unreadable() {
        // The load failure is recorded to `<data dir>/logs/`, so pin the data
        // directory to a temp home to keep the test hermetic.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let dir = tempfile::tempdir().unwrap();
        // A directory where state.json should be makes the load error → None.
        std::fs::create_dir_all(dir.path().join(".usagi/state.json")).unwrap();
        assert!(recorded_sessions(dir.path()).is_none());

        // The load failure is recorded rather than silently dropped.
        let entry = std::fs::read_dir(home.path().join("logs"))
            .expect("logs dir exists")
            .next()
            .expect("a log file was written")
            .expect("readable entry");
        assert!(std::fs::read_to_string(entry.path())
            .unwrap()
            .contains("failed to load recorded sessions"));

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn recorded_sessions_for_display_returns_saved_sessions_without_git() {
        let dir = tempfile::tempdir().unwrap();
        // Saved state on a non-git root is returned verbatim (no sync, no notice).
        let store = WorkspaceStore::new(dir.path());
        let mut state = WorkspaceState::new();
        state.sessions.push(session("wip"));
        store.save(&state).unwrap();

        let (sessions, notice) = recorded_sessions_for_display(dir.path());
        assert_eq!(sessions.len(), 1);
        assert!(notice.is_none());
    }

    #[test]
    fn recorded_sessions_for_display_is_empty_without_saved_state() {
        let dir = tempfile::tempdir().unwrap();
        let (sessions, notice) = recorded_sessions_for_display(dir.path());
        assert!(sessions.is_empty());
        assert!(notice.is_none());
    }

    #[test]
    fn recorded_sessions_for_display_reports_a_notice_when_state_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where state.json should be makes the load error → a notice.
        std::fs::create_dir_all(dir.path().join(".usagi/state.json")).unwrap();
        let (sessions, notice) = recorded_sessions_for_display(dir.path());
        assert!(sessions.is_empty());
        assert!(notice.unwrap().contains("Failed to load sessions"));
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
