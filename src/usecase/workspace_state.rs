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
    AheadBehind, BranchStatus, DiffStat, SessionRecord, WorkspaceState, WorktreeState,
};
use crate::infrastructure::error_log::ErrorLog;
use crate::infrastructure::git;
use crate::infrastructure::pr_link_store;
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
        // The git refresh above never sets a PR, so fold the harvested PR links
        // onto the session and persist them into state.json below.
        fold_pr_links(session);
    }
    state.updated_at = Utc::now();
    store.save(&state)?;
    Ok(state)
}

/// Inspect a list of worktree paths into [`WorktreeState`]s, resolving each
/// worktree's repository integration base at most once.
///
/// The integration base — the default branch name and the ref sessions are
/// measured against ([`git::IntegrationBase`]) — is a per-repository property
/// shared by every worktree of that repository, so it is resolved once per
/// repository (keyed by the worktree's git common dir) and reused, rather than
/// shelling out for it inside every [`inspect_worktree`]. Resolving it here also
/// picks `origin/<default>` versus the local `<default>` once, so a remote-less
/// repository's worktrees skip the speculative `origin/<default>` probe that
/// would otherwise miss on each of them. The worktrees are then inspected in
/// parallel so a multi-repo, multi-session workspace is not bottlenecked on a
/// long sequence of git subprocesses. Used by both [`sync`] and session recording
/// so the two never drift into separate implementations.
pub fn inspect_worktrees(paths: &[PathBuf]) -> Vec<WorktreeState> {
    use rayon::prelude::*;

    // Identify each worktree's repository by its shared git common directory — one
    // cheap `rev-parse` per path. This used to call `primary_worktree`, which shells
    // out to `git worktree list` and parses *every* worktree of the repository, so N
    // session worktrees of one repo cost O(N²) porcelain output just to group them.
    let repo_keys: Vec<PathBuf> = paths
        .par_iter()
        .map(|path| git::git_common_dir(path).unwrap_or_else(|| path.clone()))
        .collect();
    // Resolve each distinct repository's integration base once, keyed by its common
    // dir. `integration_base` works from any worktree of the repository, so it is
    // run against the first path that maps to each key.
    let mut bases: HashMap<&Path, git::IntegrationBase> = HashMap::new();
    for (key, path) in repo_keys.iter().zip(paths.iter()) {
        bases
            .entry(key.as_path())
            .or_insert_with(|| git::integration_base(path));
    }

    paths
        .par_iter()
        .zip(repo_keys.par_iter())
        .map(|(path, key)| inspect_worktree(path, &bases[key.as_path()]))
        .collect()
}

/// Load the persisted state for the repository containing `cwd`, if any.
pub fn load(cwd: &Path) -> Result<Option<WorkspaceState>> {
    let root = git::primary_worktree(cwd)?;
    WorkspaceStore::new(root).load()
}

/// Merge a session's harvested PR links onto its first worktree so every path that
/// builds the sidebar surfaces the same `#<number>` badges.
///
/// The PRs printed in a session's live terminal panes are harvested out-of-band
/// into the [`pr_link_store`], keyed by the session root (the dir the agent/shell
/// runs in). Merging that store onto `wt.pr` here (deduped by URL) — rather than
/// waiting for a slow re-sync to fold it in — is what lets **every** read path show
/// the badges immediately: [`sync`] persists the result, and the git-free
/// [`recorded_sessions`] / [`recorded_sessions_for_display`] surface it on first
/// paint and on every mtime-driven refresh, so a session's badge reads the same in
/// 選択 (Overview) as when its pane is attached. The merge is **additive**: an empty
/// store leaves the caller's own `wt.pr` (state.json's persisted badges) untouched
/// rather than wiping it — so a reader that trusts state.json (e.g. the workspace
/// overview's PR count) keeps working. In [`sync`] the preceding git refresh has
/// already reset every `wt.pr` to empty, so there the merge reproduces the old
/// "store wins" result and persists it. The aggregate lands on the first worktree
/// because the sidebar's per-session row ([`session_row`]) folds every worktree's
/// PRs into one badge.
///
/// [`session_row`]: crate::presentation::tui::home::state
fn fold_pr_links(session: &mut SessionRecord) {
    let stored = pr_link_store::get(&session.root);
    if let Some(first) = session.worktrees.first_mut() {
        for pr in stored {
            if !first.pr.iter().any(|p| p.url == pr.url) {
                first.pr.push(pr);
            }
        }
    }
}

/// Re-derive every session's `#<number>` PR badges from the [`pr_link_store`],
/// upgrading a possibly-stale session list to the store's **current** contents
/// just before it is displayed.
///
/// A background [`sync`] (or any other producer) folds the store into its
/// session list at the moment it *reads* the store, then hands the list to the
/// TUI to apply on a later frame. If a PR is harvested into the store in the gap
/// between that read and the apply, the handed-over list is stale and would
/// clobber a badge the live watcher already surfaced — so the freshly detected
/// PR flickers on and then vanishes until the next full re-sync. Re-folding at
/// apply time closes that gap: because [`fold_pr_links`] is **additive** (it only
/// merges in URLs not already present, never dropping any), and the store is the
/// monotonic superset of every PR ever seen for a session, re-folding can only
/// restore missing badges — never lose one. Callers on the presentation refresh
/// path use this so a stale list never drops a live PR badge.
pub fn refold_pr_links(sessions: &mut [SessionRecord]) {
    sessions.iter_mut().for_each(fold_pr_links);
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
/// load policy in the usecase rather than the presentation layer. Each session's
/// `#<number>` PR badges are re-derived from the [`pr_link_store`] ([`fold_pr_links`]
/// — a cheap file read, not git), so the badges show on the very first paint.
pub fn recorded_sessions_for_display(root: &Path) -> (Vec<SessionRecord>, Option<String>) {
    match WorkspaceStore::new(root).load() {
        Ok(Some(mut state)) => {
            state.sessions.iter_mut().for_each(fold_pr_links);
            (state.sessions, None)
        }
        Ok(None) => (Vec::new(), None),
        Err(e) => (Vec::new(), Some(format!("Failed to load sessions: {e}"))),
    }
}

/// The sessions recorded straight in `<root>/.usagi/state.json` (read by `root`,
/// no git refresh), or `None` when none are saved or the file cannot be read.
///
/// Used for the home screen's post-terminal refresh, where `None` means "leave
/// the list as it is" — distinct from [`recorded_sessions_for_display`], which
/// surfaces a load error as a notice on first entry. Like that path, it re-derives
/// each session's `#<number>` PR badges from the [`pr_link_store`]
/// ([`fold_pr_links`]), so a mtime-driven refresh keeps the badges rather than
/// wiping them back to whatever `state.json` last persisted.
pub fn recorded_sessions(root: &Path) -> Option<Vec<SessionRecord>> {
    match WorkspaceStore::new(root).load() {
        Ok(state) => state.map(|s| {
            let mut sessions = s.sessions;
            sessions.iter_mut().for_each(fold_pr_links);
            sessions
        }),
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

/// The note recorded for the workspace **root** (the `⌂ root` row) straight from
/// `<root>/.usagi/state.json`, read by `root` with no git refresh, or `None` when
/// none has been written or the file cannot be read.
///
/// Companion to [`recorded_sessions_for_display`] for the home screen's first
/// paint: the root row's memo, like a session's, is loaded from the recorded
/// state without touching git. A read failure simply yields `None` (no root note)
/// — the screen surfaces a state load error through the sessions path's notice.
pub fn recorded_root_note(root: &Path) -> Option<String> {
    match WorkspaceStore::new(root).load() {
        Ok(state) => state.and_then(|s| s.root_note),
        Err(_) => None,
    }
}

/// Build the [`WorktreeState`] of a single worktree at `path`, classifying its
/// branch against `base` — the integration base of the worktree's repository,
/// resolved once by the caller (a workspace may span repositories with differing
/// defaults, so the caller passes the one that applies here).
///
/// The branch, HEAD, upstream, and dirtiness are read in a single git call
/// ([`git::worktree_status`]); a `None` (not a git worktree) yields an empty,
/// branch-less state.
pub fn inspect_worktree(path: &Path, base: &git::IntegrationBase) -> WorktreeState {
    let status = git::worktree_status(path).unwrap_or(git::WorktreeStatus {
        head: String::new(),
        branch: None,
        upstream: None,
        dirty: false,
    });
    // The ahead/behind commit counts feed both the lifecycle status and the
    // `↑N ↓M` marker, so they are read once here and shared.
    let counts = branch_counts(path, status.branch.as_deref(), base);
    let classification = BranchStatus::derive(status.dirty, counts, status.upstream.is_some());
    let ahead_behind = counts.and_then(|(ahead, behind)| {
        let ab = AheadBehind { ahead, behind };
        (!ab.is_empty()).then_some(ab)
    });
    let diff = measure_diff(path, status.branch.as_deref(), base);
    WorktreeState {
        branch: status.branch,
        path: path.to_path_buf(),
        head: git::short_hash(&status.head),
        primary: false,
        upstream: status.upstream,
        status: classification,
        diff,
        ahead_behind,
        // The git inspection never sets PRs — they are harvested from live
        // terminal output and folded in by [`sync`] from the PR-link store.
        pr: Vec::new(),
        updated_at: Utc::now(),
    }
}

/// Measure the worktree's cumulative diff against the integration base for the
/// sidebar `+N -M` badge, or `None` when there is nothing to show.
///
/// Only a real branch other than the default is measured — the default branch
/// itself and a detached HEAD report `None`, mirroring [`branch_counts`]'s commit
/// counts. An empty diff (a session even with the default) also collapses to
/// `None`, so the badge and the persisted state only carry an actual diff. The
/// base ref (`origin/<default>` or local `<default>`) was resolved once per
/// repository by [`inspect_worktrees`], so this measures against it directly.
fn measure_diff(
    repo: &Path,
    branch: Option<&str>,
    base: &git::IntegrationBase,
) -> Option<DiffStat> {
    match branch {
        Some(branch) if branch != base.default => {
            let (added, removed) = git::diff_stat_against(repo, &base.base)?;
            let stat = DiffStat { added, removed };
            (!stat.is_empty()).then_some(stat)
        }
        _ => None,
    }
}

/// The branch's ahead/behind commit counts against the integration base — the git
/// facts the lifecycle status ([`BranchStatus::derive`]) and the `↑N ↓M` marker are
/// both built from, read once per worktree and shared.
///
/// Only a real branch other than the default is measured; the default branch and a
/// detached HEAD report `None` (their ahead/behind is not meaningful). The base ref
/// (`origin/<default>` when the repository has a remote default, else local
/// `<default>`) was resolved once per repository by [`inspect_worktrees`], so the
/// counts reflect what has landed on the remote integration branch even before a
/// local fetch, without re-probing the remote for each worktree.
fn branch_counts(
    repo: &Path,
    branch: Option<&str>,
    base: &git::IntegrationBase,
) -> Option<(usize, usize)> {
    match branch {
        Some(branch) if branch != base.default => {
            git::ahead_behind_against(repo, branch, &base.base)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::git::test_command as git;

    /// A local (remote-less) integration base named `name`, as the test repos
    /// below have — `origin/<name>` does not exist, so both the default name and
    /// the base ref are the local branch.
    fn base(name: &str) -> crate::infrastructure::git::IntegrationBase {
        crate::infrastructure::git::IntegrationBase {
            default: name.to_string(),
            base: name.to_string(),
        }
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
    fn inspect_worktree_reports_branch_and_local_status() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());

        let wt = inspect_worktree(dir.path(), &base("main"));
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

        let wt = inspect_worktree(dir.path(), &base("main"));
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
        let clean = inspect_worktree(dir.path(), &base("main"));
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
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: wt_path.clone(),
            worktrees: vec![WorktreeState {
                branch: None,
                path: wt_path.clone(),
                head: String::new(),
                primary: false,
                upstream: None,
                status: BranchStatus::Local,
                diff: None,
                ahead_behind: None,
                pr: Vec::new(),
                updated_at: Utc::now(),
            }],
            created_at: Utc::now(),
            last_active: None,
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
    fn sync_folds_in_the_recorded_pr_link() {
        use crate::domain::workspace_state::PrLink;
        use crate::infrastructure::workspace_store::WorkspaceStore;

        // The PR-link store lives under the data dir, so pin it to a temp home.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
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

        // Seed a session whose worktree has no PR recorded in state.json yet.
        let store = WorkspaceStore::new(dir.path());
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "wip".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: wt_path.clone(),
            worktrees: vec![inspect_worktree(&wt_path, &base("main"))],
            created_at: Utc::now(),
            last_active: None,
        });
        store.save(&state).unwrap();

        // The agent "printed" two PRs, harvested into the PR-link store out-of-band.
        let prs = vec![
            PrLink {
                number: 412,
                url: "https://github.com/KKyosuke/usagi/pull/412".to_string(),
            },
            PrLink {
                number: 98,
                url: "https://github.com/KKyosuke/other/pull/98".to_string(),
            },
        ];
        pr_link_store::add(&wt_path, &prs).unwrap();

        // sync folds them onto the worktree and persists them.
        let synced = sync(dir.path()).unwrap();
        assert_eq!(synced.sessions[0].worktrees[0].pr, prs);
        // They are durable: a fresh load (no sync) still carries the PRs.
        let reloaded = store.load().unwrap().unwrap();
        assert_eq!(reloaded.sessions[0].worktrees[0].pr, prs);

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    /// A `SessionRecord` with no worktrees, enough to seed `state.json`.
    fn session(name: &str) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: PathBuf::from(name),
            worktrees: Vec::new(),
            created_at: Utc::now(),
            last_active: None,
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
    fn recorded_root_note_reads_the_saved_root_note_and_is_none_when_absent_or_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        // No state.json yet → None.
        assert!(recorded_root_note(dir.path()).is_none());

        // Saved state without a root note → None.
        let store = WorkspaceStore::new(dir.path());
        let state = WorkspaceState::new();
        store.save(&state).unwrap();
        assert!(recorded_root_note(dir.path()).is_none());

        // Saved state with a root note → that note, read by the raw root.
        let mut state = WorkspaceState::new();
        state.root_note = Some("root memo".to_string());
        store.save(&state).unwrap();
        assert_eq!(recorded_root_note(dir.path()).as_deref(), Some("root memo"));

        // An unreadable state.json falls back to None (no note).
        let bad = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(bad.path().join(".usagi/state.json")).unwrap();
        assert!(recorded_root_note(bad.path()).is_none());
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
    fn recorded_read_paths_merge_in_the_pr_links_from_the_store() {
        use crate::domain::workspace_state::{BranchStatus, PrLink, WorktreeState};
        use crate::infrastructure::workspace_store::WorkspaceStore;

        // The PR-link store lives under the data dir, so pin it to a temp home.
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".usagi/sessions/wip");
        let second = dir.path().join(".usagi/sessions/wip-lib");
        // One PR already persisted in state.json, one only in the store.
        let known = PrLink {
            number: 7,
            url: "https://github.com/KKyosuke/usagi/pull/7".to_string(),
        };
        let fresh = PrLink {
            number: 8,
            url: "https://github.com/KKyosuke/usagi/pull/8".to_string(),
        };
        let wt = |path: &Path, pr: Vec<PrLink>| WorktreeState {
            branch: Some("wip".to_string()),
            path: path.to_path_buf(),
            head: "abc123".to_string(),
            primary: false,
            upstream: None,
            status: BranchStatus::default(),
            diff: None,
            ahead_behind: None,
            pr,
            updated_at: Utc::now(),
        };

        let store = WorkspaceStore::new(dir.path());
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "wip".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: root.clone(),
            worktrees: vec![wt(&root, vec![known.clone()]), wt(&second, Vec::new())],
            created_at: Utc::now(),
            last_active: None,
        });
        store.save(&state).unwrap();

        // The store carries the already-known PR again plus a freshly scanned one.
        pr_link_store::add(&root, &[known.clone(), fresh.clone()]).unwrap();

        // Both git-free read paths merge the store onto the first worktree — the
        // fresh PR appears, the known one is not duplicated, later worktrees stay
        // empty — so the sidebar shows the badge without waiting for a sync.
        let recorded = recorded_sessions(dir.path()).unwrap();
        assert_eq!(
            recorded[0].worktrees[0].pr,
            vec![known.clone(), fresh.clone()]
        );
        assert!(recorded[0].worktrees[1].pr.is_empty());
        let (display, notice) = recorded_sessions_for_display(dir.path());
        assert_eq!(display[0].worktrees[0].pr, vec![known, fresh]);
        assert!(display[0].worktrees[1].pr.is_empty());
        assert!(notice.is_none());

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    /// Re-derive a branch's lifecycle status the way [`inspect_worktree`] does:
    /// read its ahead/behind via [`branch_counts`] (a real git call) and hand the
    /// counts to the pure [`BranchStatus::derive`]. Keeps the integration coverage
    /// the old `classify` had after it was split into fetch + derive.
    fn classify(
        repo: &Path,
        branch: Option<&str>,
        default: &str,
        up: bool,
        dirty: bool,
    ) -> BranchStatus {
        BranchStatus::derive(dirty, branch_counts(repo, branch, &base(default)), up)
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
        // The freshly-cut branch is even with the default → no `↑↓` marker.
        assert_eq!(
            branch_counts(dir.path(), Some("feature"), &base("main")),
            Some((0, 0))
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
        // Behind by the one commit main gained, ahead by none.
        assert_eq!(
            branch_counts(dir.path(), Some("feature"), &base("main")),
            Some((0, 1))
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
        // Neither the detached HEAD nor the default branch yields ahead/behind.
        assert_eq!(branch_counts(dir.path(), None, &base("main")), None);
        assert_eq!(branch_counts(dir.path(), Some("main"), &base("main")), None);
    }

    #[test]
    fn inspect_worktree_records_ahead_behind_for_a_diverged_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // feature gains a commit (ahead 1); main then gains one feature lacks
        // (behind 1).
        git(dir.path())
            .args(["checkout", "-q", "-b", "feature"])
            .status()
            .unwrap();
        std::fs::write(dir.path().join("f"), "y").unwrap();
        git(dir.path()).args(["add", "."]).status().unwrap();
        git(dir.path())
            .args(["commit", "-q", "-m", "feature work"])
            .status()
            .unwrap();
        git(dir.path())
            .args(["checkout", "-q", "main"])
            .status()
            .unwrap();
        std::fs::write(dir.path().join("m"), "z").unwrap();
        git(dir.path()).args(["add", "."]).status().unwrap();
        git(dir.path())
            .args(["commit", "-q", "-m", "main work"])
            .status()
            .unwrap();
        git(dir.path())
            .args(["checkout", "-q", "feature"])
            .status()
            .unwrap();

        let wt = inspect_worktree(dir.path(), &base("main"));
        assert_eq!(
            wt.ahead_behind,
            Some(AheadBehind {
                ahead: 1,
                behind: 1
            })
        );

        // The default branch carries no `↑↓` marker (not measured against itself).
        let main = inspect_worktree(dir.path(), &base("feature"));
        // Checked out on feature, so inspecting with default "feature" reads the
        // current branch as the default → no counts.
        assert_eq!(main.ahead_behind, None);
    }
}
