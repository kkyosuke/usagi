//! Bring a workspace's default branch up to date with its remote and propagate
//! that update into each session's worktrees, skipping anything that would
//! conflict.
//!
//! `usagi update` has two halves:
//!
//! 1. **Refresh the root.** Every source repository ([`session::source_repos`])
//!    is fetched from `origin` and its default branch (e.g. `main`) is
//!    fast-forwarded to the remote — only when it is checked out, clean, and can
//!    fast-forward, so the command never rewrites or merges into the root.
//! 2. **Distribute to sessions.** Each recorded session worktree on its
//!    `usagi/<name>` branch has the freshly updated default branch merged in,
//!    *but only where it merges cleanly*: a worktree with uncommitted changes is
//!    left alone, and a merge that would conflict is aborted so the worktree is
//!    untouched.
//!
//! The git side-effects live in [`crate::infrastructure::git`]; this module is
//! the policy that decides, per repository and per worktree, which of the
//! [`DefaultOutcome`] / [`WorktreeOutcome`] cases applies. The conflict-safety
//! guarantee is [`git::merge`]'s: a non-fast-forward merge that conflicts aborts
//! itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::infrastructure::git::{self, MergeStatus};
use crate::usecase::session;

/// What happened to one repository's default branch during the root refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultOutcome {
    /// `git fetch` failed (e.g. no `origin` remote); carries the reason.
    FetchFailed(String),
    /// The default branch is not checked out in the repository's primary
    /// worktree, so it was left untouched. Carries the branch that *is* checked
    /// out (`None` for a detached HEAD).
    NotCheckedOut(Option<String>),
    /// The primary worktree has uncommitted changes, so it was left untouched.
    Dirty,
    /// Already even with (or ahead of) the remote: nothing to pull.
    UpToDate,
    /// The local default branch has commits the remote lacks, so it cannot
    /// fast-forward; left untouched rather than diverge further with a merge.
    Diverged,
    /// The default branch was fast-forwarded to the remote, by `behind` commits.
    Updated { behind: usize },
    /// (dry-run) The default branch would be fast-forwarded by `behind` commits.
    WouldUpdate { behind: usize },
}

/// What happened to one session worktree when the default branch was
/// distributed into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeOutcome {
    /// The worktree's repository could not be fetched, so it was not touched.
    FetchFailed,
    /// A detached HEAD — no branch to merge into; skipped.
    Detached,
    /// The worktree has uncommitted changes; skipped so they are not disturbed.
    Dirty,
    /// Already contains the default branch's commits.
    UpToDate,
    /// Merging the default branch would conflict; skipped, worktree untouched.
    Conflict,
    /// The default branch was merged in, bringing in `behind` commits.
    Updated { behind: usize },
    /// (dry-run) The default branch would be merged in, by `behind` commits.
    WouldUpdate { behind: usize },
}

/// One repository's default-branch refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoUpdate {
    /// The repository's primary worktree path.
    pub repo: PathBuf,
    /// The repository's default branch (e.g. `main`).
    pub branch: String,
    pub outcome: DefaultOutcome,
}

/// One session worktree's propagation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeUpdate {
    /// The session worktree's path.
    pub worktree: PathBuf,
    /// The branch checked out there (`usagi/<name>`), or `None` if detached.
    pub branch: Option<String>,
    pub outcome: WorktreeOutcome,
}

/// One session's propagation across all of its worktrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUpdate {
    pub name: String,
    pub worktrees: Vec<WorktreeUpdate>,
}

/// The full report of an `update` run, ready for the presentation layer to
/// render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    /// `true` when nothing local was changed (fetch still ran to compute the
    /// preview).
    pub dry_run: bool,
    /// The root refresh, one entry per source repository.
    pub repos: Vec<RepoUpdate>,
    /// The distribution to sessions, one entry per recorded session.
    pub sessions: Vec<SessionUpdate>,
}

/// Run `usagi update` for the workspace containing `cwd`.
///
/// Resolves the workspace root (handling being run from inside a session
/// worktree), refreshes every source repository's default branch from `origin`,
/// then distributes the update into each recorded session's worktrees. With
/// `dry_run`, `origin` is still fetched (so the preview is accurate) but no local
/// branch or worktree is changed.
pub fn run(cwd: &Path, dry_run: bool) -> Result<UpdateReport> {
    let root = resolve_root(cwd);

    // 1. Refresh each source repository's default branch from the remote.
    let mut repos = Vec::new();
    for repo in session::source_repos(&root)
        .into_iter()
        .filter(|repo| git::has_origin(repo))
    {
        let branch = git::default_branch(&repo);
        let outcome = match git::fetch(&repo) {
            Err(e) => DefaultOutcome::FetchFailed(format!("{e}")),
            Ok(()) => update_default(&repo, &branch, dry_run)?,
        };
        repos.push(RepoUpdate {
            repo,
            branch,
            outcome,
        });
    }

    // 2. Distribute the updated default branch into each session worktree.
    //    A worktree's repository (and its fetch result / default branch) is
    //    resolved from the worktree itself, memoized so a multi-repo or
    //    multi-session workspace fetches each repository at most once here.
    let mut fetched: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut sessions = Vec::new();
    for record in session::list(&root)? {
        let mut worktrees = Vec::new();
        for wt in &record.worktrees {
            let repo = git::primary_worktree(&wt.path).unwrap_or_else(|_| wt.path.clone());
            // Local-only repositories are not update targets. In particular,
            // recursively scanning a broad workspace (such as the user's home)
            // commonly reaches editor/plugin scratch repositories without an
            // `origin`; do not turn those into noisy fetch failures.
            if git::is_repository(&repo) && !git::has_origin(&repo) {
                continue;
            }
            let outcome = match ensure_fetched(&mut fetched, &repo) {
                Some(_) => WorktreeOutcome::FetchFailed,
                None => {
                    let default = git::default_branch(&repo);
                    propagate(&wt.path, &repo, &default, dry_run)?
                }
            };
            worktrees.push(WorktreeUpdate {
                worktree: wt.path.clone(),
                branch: wt.branch.clone(),
                outcome,
            });
        }
        sessions.push(SessionUpdate {
            name: record.name,
            worktrees,
        });
    }

    Ok(UpdateReport {
        dry_run,
        repos,
        sessions,
    })
}

/// Resolve the workspace root to operate on from a working directory.
///
/// When run from inside a session worktree (`.usagi/sessions/<name>/…`), the
/// command still targets the whole workspace, so the path is stripped back to
/// the workspace root. A root that is itself a git repository (the common
/// single-repo workspace, or any subdirectory of one) is normalized to its
/// primary worktree; a non-git, multi-repo root is used as-is so the recursive
/// repository walk can find the repositories under it.
fn resolve_root(cwd: &Path) -> PathBuf {
    let stripped = session::workspace_root(cwd);
    if git::is_repository(&stripped) {
        git::primary_worktree(&stripped).unwrap_or(stripped)
    } else {
        stripped
    }
}

/// Fetch `repo` once, memoizing the result in `cache`: `None` means the fetch
/// succeeded, `Some(error)` records why it failed. Returns the cached entry.
fn ensure_fetched<'a>(
    cache: &'a mut HashMap<PathBuf, Option<String>>,
    repo: &Path,
) -> &'a Option<String> {
    cache
        .entry(repo.to_path_buf())
        .or_insert_with(|| git::fetch(repo).err().map(|e| format!("{e}")))
}

/// Refresh `repo`'s default branch from `origin/<branch>` (already fetched).
///
/// The default branch is only fast-forwarded when it is the branch checked out
/// in the primary worktree, that worktree is clean, and the remote is strictly
/// ahead — otherwise the repository is reported untouched with the reason. In
/// `dry_run` mode the same decision is made from the commit counts without
/// running the merge.
fn update_default(repo: &Path, branch: &str, dry_run: bool) -> Result<DefaultOutcome> {
    // A non-default or detached HEAD, or a dirty tree, blocks a safe
    // fast-forward of the default branch — report it rather than touch the repo.
    // A path that is not a git worktree reports as a branch-less, clean tree,
    // handled by the "not checked out" branch below.
    let status = git::worktree_status(repo).unwrap_or(git::WorktreeStatus {
        head: String::new(),
        branch: None,
        upstream: None,
        dirty: false,
    });
    if status.branch.as_deref() != Some(branch) {
        return Ok(DefaultOutcome::NotCheckedOut(status.branch));
    }
    if status.dirty {
        return Ok(DefaultOutcome::Dirty);
    }

    // `ahead`/`behind` of the local default branch against `origin/<branch>`:
    // `behind == 0` means nothing to pull; any `ahead` means the local branch
    // has commits the remote lacks, so it cannot fast-forward.
    let (ahead, behind) = git::ahead_behind(repo, branch, branch).unwrap_or((0, 0));
    if dry_run {
        return Ok(if behind == 0 {
            DefaultOutcome::UpToDate
        } else if ahead > 0 {
            DefaultOutcome::Diverged
        } else {
            DefaultOutcome::WouldUpdate { behind }
        });
    }

    let target = format!("origin/{branch}");
    Ok(match git::merge(repo, &target, true)? {
        MergeStatus::Updated => DefaultOutcome::Updated { behind },
        MergeStatus::AlreadyUpToDate => DefaultOutcome::UpToDate,
        MergeStatus::NotFastForward | MergeStatus::Conflict => DefaultOutcome::Diverged,
    })
}

/// Merge `origin/<default>` into the session worktree at `worktree` (on a
/// repository whose primary worktree is `repo`), skipping it when it cannot be
/// done cleanly.
///
/// A detached HEAD or a dirty tree is skipped untouched. Otherwise the commit
/// counts decide whether there is anything to bring in, and — outside `dry_run` —
/// the actual merge brings it in or, on conflict, aborts and reports the skip.
fn propagate(
    worktree: &Path,
    repo: &Path,
    default: &str,
    dry_run: bool,
) -> Result<WorktreeOutcome> {
    // A path that is not (or no longer) a git worktree reports as a detached,
    // branch-less, clean tree — handled the same as a detached HEAD below.
    let status = git::worktree_status(worktree).unwrap_or(git::WorktreeStatus {
        head: String::new(),
        branch: None,
        upstream: None,
        dirty: false,
    });
    if status.dirty {
        return Ok(WorktreeOutcome::Dirty);
    }
    let Some(branch) = status.branch else {
        return Ok(WorktreeOutcome::Detached);
    };

    // How many commits the default branch is ahead of this worktree's branch:
    // `0` means it already carries them.
    let (_, behind) = git::ahead_behind(repo, &branch, default).unwrap_or((0, 0));
    if dry_run {
        return Ok(if behind == 0 {
            WorktreeOutcome::UpToDate
        } else {
            WorktreeOutcome::WouldUpdate { behind }
        });
    }

    let target = format!("origin/{default}");
    Ok(match git::merge(worktree, &target, false)? {
        MergeStatus::Updated => WorktreeOutcome::Updated { behind },
        MergeStatus::AlreadyUpToDate => WorktreeOutcome::UpToDate,
        // A non-fast-forward merge never reports `NotFastForward`; pairing it
        // here keeps the match exhaustive without an unreachable arm.
        MergeStatus::Conflict | MergeStatus::NotFastForward => WorktreeOutcome::Conflict,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::git::test_command as git_cmd;
    use std::path::Path;

    fn run(dir: &Path, args: &[&str]) {
        assert!(
            git_cmd(dir).args(args).status().unwrap().success(),
            "git {args:?} failed"
        );
    }

    /// A repo on `main` with one commit, no remote.
    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        run(dir, &["init", "-q", "-b", "main"]);
        run(dir, &["config", "user.email", "t@e.com"]);
        run(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("f"), "base\n").unwrap();
        run(dir, &["add", "."]);
        run(dir, &["commit", "-q", "-m", "init"]);
    }

    /// A workspace whose single repo (the root) tracks a bare `origin`, with the
    /// remote one commit ahead of the local `main`. Returns `(tempdir, root)`.
    fn workspace_behind_remote() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("remote.git");
        let root = tmp.path().join("work");
        // `-b main` pins the bare repo's HEAD to `main` so the throwaway clone
        // in [`push_remote`] checks out `main` regardless of the host's
        // `init.defaultBranch` (`master` on CI), where it would otherwise commit
        // onto `master` and fail to `git push origin main`.
        run(
            tmp.path(),
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        init_repo(&root);
        // `usagi init` gitignores the local `.usagi/` state; mirror that via the
        // shared `info/exclude` so the sessions this helper's callers create do
        // not leave the root worktree looking dirty.
        std::fs::write(root.join(".git/info/exclude"), ".usagi/\n").unwrap();
        run(&root, &["remote", "add", "origin", bare.to_str().unwrap()]);
        run(&root, &["push", "-q", "-u", "origin", "main"]);
        run(&root, &["remote", "set-head", "origin", "main"]);
        // Advance the remote one commit via a throwaway clone, then reset the
        // local main back so it is strictly behind origin/main.
        push_remote(&bare, "remote.txt", "hi");
        (tmp, root)
    }

    /// Push a commit to `bare`'s main from a throwaway clone.
    fn push_remote(bare: &Path, file: &str, contents: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let clone = tmp.path().join("pusher");
        run(
            tmp.path(),
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        run(&clone, &["config", "user.email", "t@e.com"]);
        run(&clone, &["config", "user.name", "t"]);
        std::fs::write(clone.join(file), contents).unwrap();
        run(&clone, &["add", "."]);
        run(&clone, &["commit", "-q", "-m", "remote work"]);
        run(&clone, &["push", "-q", "origin", "main"]);
    }

    fn default_outcome(report: &UpdateReport) -> &DefaultOutcome {
        &report.repos[0].outcome
    }

    fn session_outcome<'a>(report: &'a UpdateReport, name: &str) -> &'a WorktreeOutcome {
        &report
            .sessions
            .iter()
            .find(|s| s.name == name)
            .unwrap()
            .worktrees[0]
            .outcome
    }

    #[test]
    fn fast_forwards_the_default_branch_and_reports_up_to_date_next_time() {
        let (_tmp, root) = workspace_behind_remote();

        let report = run_update(&root, false);
        assert_eq!(report.repos.len(), 1);
        assert_eq!(report.repos[0].branch, "main");
        assert_eq!(
            *default_outcome(&report),
            DefaultOutcome::Updated { behind: 1 }
        );
        // No sessions recorded yet.
        assert!(report.sessions.is_empty());

        // A second run has nothing to pull.
        let report = run_update(&root, false);
        assert_eq!(*default_outcome(&report), DefaultOutcome::UpToDate);
    }

    #[test]
    fn dry_run_reports_would_update_without_changing_the_branch() {
        let (_tmp, root) = workspace_behind_remote();
        let before = head_commit(&root);

        let report = run_update(&root, true);
        assert!(report.dry_run);
        assert_eq!(
            *default_outcome(&report),
            DefaultOutcome::WouldUpdate { behind: 1 }
        );
        // The local branch is untouched.
        assert_eq!(head_commit(&root), before);
    }

    #[test]
    fn reports_diverged_when_local_main_has_its_own_commits() {
        let (_tmp, root) = workspace_behind_remote();
        // A local commit on main the remote lacks: the histories diverge, so a
        // fast-forward is impossible.
        std::fs::write(root.join("local.txt"), "local\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-q", "-m", "local"]);

        // Real run refuses to diverge.
        let report = run_update(&root, false);
        assert_eq!(*default_outcome(&report), DefaultOutcome::Diverged);
        // Dry run reports the same.
        let report = run_update(&root, true);
        assert_eq!(*default_outcome(&report), DefaultOutcome::Diverged);
    }

    #[test]
    fn reports_not_checked_out_and_dirty_for_the_default_branch() {
        let (_tmp, root) = workspace_behind_remote();

        // The primary worktree on a different branch: the default is not checked
        // out, so it is left untouched.
        run(&root, &["checkout", "-q", "-b", "side"]);
        let report = run_update(&root, false);
        assert_eq!(
            *default_outcome(&report),
            DefaultOutcome::NotCheckedOut(Some("side".to_string()))
        );

        // Back on main but with uncommitted changes: skipped as dirty.
        run(&root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("f"), "uncommitted\n").unwrap();
        let report = run_update(&root, false);
        assert_eq!(*default_outcome(&report), DefaultOutcome::Dirty);
    }

    #[test]
    fn skips_a_repo_without_an_origin_remote() {
        // Local-only repositories (including editor/plugin scratch repos) are
        // outside update's contract and must not be rendered as fetch errors.
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        session::create(root.path(), "feat").unwrap();

        let report = run_update(root.path(), false);
        assert!(report.repos.is_empty());
        assert_eq!(report.sessions.len(), 1);
        assert!(report.sessions[0].worktrees.is_empty());
    }

    #[test]
    fn distributes_the_default_branch_into_a_clean_session_and_skips_a_dirty_one() {
        let (_tmp, root) = workspace_behind_remote();
        // Two sessions cut before the remote moves on again.
        session::create(&root, "clean").unwrap();
        session::create(&root, "dirty").unwrap();
        // The remote gains another commit so the sessions are behind it.
        push_remote(&root.join("..").join("remote.git"), "remote2.txt", "more\n");

        // Make the "dirty" session's worktree dirty.
        let dirty_wt = root.join(".usagi/sessions/dirty");
        std::fs::write(dirty_wt.join("scratch.txt"), "wip").unwrap();

        let report = run_update(&root, false);
        // The clean session merged the new default-branch commits in.
        assert!(matches!(
            session_outcome(&report, "clean"),
            WorktreeOutcome::Updated { .. }
        ));
        // The dirty session was skipped, untouched.
        assert_eq!(*session_outcome(&report, "dirty"), WorktreeOutcome::Dirty);

        // Re-running leaves the now up-to-date clean session reporting so.
        let report = run_update(&root, false);
        assert_eq!(
            *session_outcome(&report, "clean"),
            WorktreeOutcome::UpToDate
        );
    }

    #[test]
    fn skips_a_session_whose_merge_would_conflict() {
        let (_tmp, root) = workspace_behind_remote();
        // Pull the first remote commit into main first so the session cuts from
        // an up-to-date base.
        run_update(&root, false);
        session::create(&root, "feat").unwrap();
        let wt = root.join(".usagi/sessions/feat");

        // The session commits an edit to `f`; the remote then edits the same
        // line differently and main fast-forwards to it — so merging the default
        // into the session conflicts.
        std::fs::write(wt.join("f"), "session edit\n").unwrap();
        run(&wt, &["add", "."]);
        run(&wt, &["commit", "-q", "-m", "session edit"]);
        push_remote(&root.join("..").join("remote.git"), "f", "remote edit\n");

        let report = run_update(&root, false);
        assert_eq!(
            *default_outcome(&report),
            DefaultOutcome::Updated { behind: 1 }
        );
        assert_eq!(*session_outcome(&report, "feat"), WorktreeOutcome::Conflict);
        // The session worktree is untouched (its own edit survives, no merge in
        // progress).
        assert_eq!(
            std::fs::read_to_string(wt.join("f")).unwrap(),
            "session edit\n"
        );
    }

    #[test]
    fn dry_run_previews_a_session_that_would_update() {
        let (_tmp, root) = workspace_behind_remote();
        run_update(&root, false); // bring main up to date first
        session::create(&root, "feat").unwrap();
        push_remote(&root.join("..").join("remote.git"), "remote2.txt", "more\n");

        let report = run_update(&root, true);
        assert!(matches!(
            session_outcome(&report, "feat"),
            WorktreeOutcome::WouldUpdate { .. }
        ));
    }

    #[test]
    fn resolves_the_workspace_root_from_inside_a_session_worktree() {
        let (_tmp, root) = workspace_behind_remote();
        session::create(&root, "feat").unwrap();
        // Running from inside the session worktree still targets the whole
        // workspace: the default branch is fast-forwarded and the session seen.
        let wt = root.join(".usagi/sessions/feat");
        let report = run_update(&wt, false);
        assert_eq!(
            *default_outcome(&report),
            DefaultOutcome::Updated { behind: 1 }
        );
        assert!(report.sessions.iter().any(|s| s.name == "feat"));
    }

    #[test]
    fn handles_a_non_git_multi_repo_root() {
        // A non-git root can contain both a remotely managed repository and a
        // local-only scratch repository. Only the former is an update target.
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("app");
        let scratch = root.path().join("scratch");
        init_repo(&app);
        init_repo(&scratch);
        run(
            &app,
            &["remote", "add", "origin", "/definitely/missing/repo.git"],
        );

        let report = run_update(root.path(), false);
        assert_eq!(report.repos.len(), 1);
        assert_eq!(report.repos[0].repo, app);
        assert!(matches!(
            report.repos[0].outcome,
            DefaultOutcome::FetchFailed(_)
        ));
    }

    // --- helpers reused across the tests above -----------------------------

    /// Run the usecase against `cwd`, asserting it succeeds.
    fn run_update(cwd: &Path, dry_run: bool) -> UpdateReport {
        super::run(cwd, dry_run).unwrap()
    }

    /// The full HEAD commit at `dir`.
    fn head_commit(dir: &Path) -> String {
        let out = git_cmd(dir).args(["rev-parse", "HEAD"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn dry_run_reports_up_to_date_when_already_even_with_the_remote() {
        let (_tmp, root) = workspace_behind_remote();
        // Bring main up to date first, then a dry run finds nothing to pull.
        run_update(&root, false);
        let report = run_update(&root, true);
        assert_eq!(*default_outcome(&report), DefaultOutcome::UpToDate);
    }

    #[test]
    fn dry_run_reports_a_session_already_up_to_date() {
        let (_tmp, root) = workspace_behind_remote();
        run_update(&root, false); // bring main up to date
        session::create(&root, "feat").unwrap();
        // No further remote movement: the fresh session already carries the
        // default branch, so a dry run reports it up to date.
        let report = run_update(&root, true);
        assert_eq!(*session_outcome(&report, "feat"), WorktreeOutcome::UpToDate);
    }

    #[test]
    fn skips_a_session_worktree_on_a_detached_head() {
        let (_tmp, root) = workspace_behind_remote();
        run_update(&root, false);
        session::create(&root, "feat").unwrap();
        // Detach the session worktree's HEAD: there is no branch to merge into.
        let wt = root.join(".usagi/sessions/feat");
        run(&wt, &["checkout", "-q", "--detach"]);

        let report = run_update(&root, false);
        assert_eq!(*session_outcome(&report, "feat"), WorktreeOutcome::Detached);
    }

    #[test]
    fn reports_fetch_failure_for_a_ghost_session_worktree_path() {
        use crate::domain::workspace_state::{
            BranchStatus, SessionRecord, WorkspaceState, WorktreeState,
        };
        use crate::infrastructure::workspace_store::WorkspaceStore;

        // A non-git workspace root, plus a recorded session whose worktree path
        // is a plain directory under no git repository: resolving its repository
        // fails (falling back to the path itself), and fetching that
        // non-repository fails — so it is reported as a fetch failure. The
        // non-git root also means there are no source repositories to refresh.
        let root = tempfile::tempdir().unwrap();
        let ghost = root.path().join(".usagi/sessions/ghost");
        std::fs::create_dir_all(&ghost).unwrap();

        let store = WorkspaceStore::new(root.path());
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            todos: Vec::new(),
            decisions: Vec::new(),
            name: "ghost".to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: ghost.clone(),
            worktrees: vec![WorktreeState {
                branch: Some("usagi/ghost".to_string()),
                path: ghost.clone(),
                head: String::new(),
                primary: false,
                upstream: None,
                status: BranchStatus::Local,
                diff: None,
                ahead_behind: None,
                pr: Vec::new(),
                updated_at: chrono::Utc::now(),
            }],
            worktree_provenance: Vec::new(),
            created_at: chrono::Utc::now(),
            last_active: None,
        });
        store.save(&state).unwrap();

        let report = run_update(root.path(), false);
        assert_eq!(
            *session_outcome(&report, "ghost"),
            WorktreeOutcome::FetchFailed
        );
    }
}
