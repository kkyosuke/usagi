//! Read-only git inspection used to build a repository's workspace state.
//!
//! All operations shell out to the system `git` binary (rather than linking a
//! git library) so the user's existing git configuration is respected and the
//! crate stays dependency-light. Everything here is read-only.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::domain::settings::BranchSource;

/// A worktree as reported by `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// Branch short name (e.g. `main`), or `None` for a detached HEAD.
    pub branch: Option<String>,
    /// Full commit hash currently checked out.
    pub head: String,
}

/// Git environment variables that scope a command to a specific repository.
///
/// When usagi runs from inside a git hook these are set in the environment and
/// would override `-C <repo>`, making git operate on the hook's repository
/// instead of the one we asked about. Clearing them keeps `-C <repo>`
/// authoritative.
const REPO_SCOPING_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_NAMESPACE",
];

/// Build a `git -C <repo>` command with repo-scoping env vars stripped.
fn git_command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo);
    for var in REPO_SCOPING_ENV {
        command.env_remove(var);
    }
    command
}

/// Run `git` with `args` inside `repo` and return trimmed stdout.
///
/// Returns `Ok(None)` when git exits non-zero (e.g. the queried ref does not
/// exist), and an error only when the process itself could not be run.
fn git_capture(repo: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = git_command(repo)
        .args(args)
        .output()
        .context(format!("failed to run `git {}`", args.join(" ")))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

/// Clone `url` into `dest`, optionally checking out `branch` after cloning.
///
/// Output is captured rather than inherited so it does not disturb an active
/// TUI; on failure the captured stderr is surfaced in the error. Repo-scoping
/// env vars are stripped so an inherited `GIT_DIR` (e.g. when usagi runs from a
/// git hook) cannot redirect the clone.
pub fn clone(url: &str, dest: &Path, branch: Option<&str>) -> Result<()> {
    let mut command = Command::new("git");
    for var in REPO_SCOPING_ENV {
        command.env_remove(var);
    }
    command.arg("clone");
    if let Some(branch) = branch {
        command.args(["--branch", branch]);
    }
    command.arg(url).arg(dest);
    // Anchor the command to the destination's parent so it never depends on the
    // process's inherited working directory (which a concurrent test — or a
    // caller running from a since-removed directory — may have invalidated).
    // `dest` is passed absolute, so the clone target is unaffected.
    if let Some(parent) = dest.parent() {
        command.current_dir(parent);
    }

    let output = command.output().context("failed to run `git clone`")?;
    if !output.status.success() {
        bail!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Create a worktree at `dest` checking out a new branch `branch` in `repo`.
///
/// Runs `git -C <repo> worktree add -b <branch> <dest> [<base>]`. When `base` is
/// given the new branch starts from that ref (e.g. `main` or `origin/main`);
/// otherwise it starts from the repository's current `HEAD`. Fails if `branch`
/// already exists or `dest` is not empty. Output is captured (not inherited) so
/// it never disturbs an active TUI; on failure the captured stderr is surfaced.
/// Repo-scoping env vars are stripped so an inherited `GIT_DIR` cannot redirect
/// the operation to another repository.
pub fn add_worktree(repo: &Path, dest: &Path, branch: &str, base: Option<&str>) -> Result<()> {
    let mut command = git_command(repo);
    command.args(["worktree", "add", "-b", branch]).arg(dest);
    if let Some(base) = base {
        command.arg(base);
    }
    let output = command
        .output()
        .context("failed to run `git worktree add`")?;
    if !output.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The checked-out branch (`None` if detached) and full HEAD commit at the
/// worktree `path`, or `None` if it is not a git worktree.
pub fn worktree_head(path: &Path) -> Option<(Option<String>, String)> {
    let head = git_capture(path, &["rev-parse", "HEAD"]).ok().flatten()?;
    let branch = git_capture(path, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .flatten()
        .filter(|b| b != "HEAD");
    Some((branch, head))
}

/// Return `true` if the worktree at `path` has uncommitted changes — modified,
/// staged, or untracked files (anything `git status --porcelain` reports).
///
/// A git failure (e.g. not a worktree) is treated as "no changes" so it never
/// blocks a removal on its own.
pub fn has_uncommitted_changes(path: &Path) -> bool {
    git_capture(path, &["status", "--porcelain"])
        .ok()
        .flatten()
        .map(|out| !out.is_empty())
        .unwrap_or(false)
}

/// Remove the worktree at `worktree` from `repo`. With `force`, discard
/// uncommitted changes; without it, git refuses a dirty worktree.
pub fn remove_worktree(repo: &Path, worktree: &Path, force: bool) -> Result<()> {
    let mut command = git_command(repo);
    command.args(["worktree", "remove"]);
    if force {
        command.arg("--force");
    }
    let output = command
        .arg(worktree)
        .output()
        .context("failed to run `git worktree remove`")?;
    if !output.status.success() {
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Force-delete `branch` in `repo` (`git branch -D`). Used when discarding a
/// session, so the branch is removed regardless of merge status.
pub fn delete_branch(repo: &Path, branch: &str) -> Result<()> {
    let output = git_command(repo)
        .args(["branch", "-D", branch])
        .output()
        .context("failed to run `git branch -D`")?;
    if !output.status.success() {
        bail!(
            "git branch -D failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Return `true` if `path` is inside a git working tree.
///
/// Used to decide whether an existing directory registered as a workspace can
/// have its worktree state synced; a plain directory simply skips the sync.
pub fn is_repository(path: &Path) -> bool {
    git_capture(path, &["rev-parse", "--is-inside-work-tree"])
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
}

/// Resolve the absolute path of the repository's primary (main) worktree.
///
/// This is the directory under which `.usagi/` should live, regardless of which
/// worktree the command was invoked from.
pub fn primary_worktree(repo: &Path) -> Result<PathBuf> {
    let worktrees = list_worktrees(repo)?;
    Ok(worktrees
        .into_iter()
        .next()
        .expect("a successful `git worktree list` always yields the current worktree")
        .path)
}

/// List all worktrees of the repository, primary first.
pub fn list_worktrees(repo: &Path) -> Result<Vec<WorktreeInfo>> {
    let stdout = git_capture(repo, &["worktree", "list", "--porcelain"])?
        .ok_or_else(|| anyhow!("{} is not a git repository", repo.display()))?;

    let mut worktrees = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut head = String::new();
    let mut branch: Option<String> = None;

    for line in stdout.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            head = h.to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if line.is_empty() {
            // Blank line terminates one worktree record.
            if let Some(path) = path.take() {
                worktrees.push(WorktreeInfo {
                    path,
                    branch: branch.take(),
                    head: std::mem::take(&mut head),
                });
            }
        }
    }
    // The porcelain output may not end with a trailing blank line.
    if let Some(path) = path.take() {
        worktrees.push(WorktreeInfo { path, branch, head });
    }

    Ok(worktrees)
}

/// Determine the repository's default branch (e.g. `main`).
///
/// Prefers the remote's HEAD (`origin/HEAD`); falls back to the current branch
/// of the primary worktree, then to `main`.
pub fn default_branch(repo: &Path) -> String {
    remote_default_branch(repo)
        .or_else(|| current_branch(repo))
        .unwrap_or_else(|| "main".to_string())
}

/// Resolve the base ref a new session worktree should branch from, honouring the
/// project's [`BranchSource`] and chosen branch.
///
/// `branch` names the branch to cut from; `None` uses the repository's detected
/// default branch (e.g. `main`). The source then selects which form of that
/// branch is used:
///
/// - [`BranchSource::Remote`] prefers `origin/<branch>`, falling back to the
///   local `<branch>` when no remote ref exists.
/// - [`BranchSource::Local`] uses the local `<branch>`.
///
/// Returns `None` when the chosen ref does not exist (e.g. a brand-new repo with
/// no commits, or a branch name that no longer resolves), so the caller branches
/// from the current `HEAD` instead.
pub fn resolve_base_ref(repo: &Path, source: BranchSource, branch: Option<&str>) -> Option<String> {
    let default = branch
        .map(str::to_string)
        .unwrap_or_else(|| default_branch(repo));
    let local = rev_exists(repo, &default).then(|| default.clone());
    match source {
        BranchSource::Remote => {
            let remote = format!("origin/{default}");
            rev_exists(repo, &remote).then_some(remote).or(local)
        }
        BranchSource::Local => local,
    }
}

/// List the candidate base branches in `repo`: the short names of every local
/// and remote-tracking branch, with the remote prefix stripped, de-duplicated
/// and sorted. The `<remote>/HEAD` pseudo-refs are skipped. Returns an empty
/// list when `repo` is not a git repository (or has no branches yet).
///
/// These are offered in the config screen so a project can branch new sessions
/// off a specific branch rather than the detected default.
pub fn list_branches(repo: &Path) -> Vec<String> {
    use std::collections::BTreeSet;

    // Local branches: `lstrip=2` drops `refs/heads/`, leaving the bare name.
    let mut names: BTreeSet<String> = ref_names(repo, "refs/heads", 2).into_iter().collect();

    // Remote-tracking branches: `lstrip=3` drops `refs/remotes/<remote>/`, so a
    // branch name keeps any embedded slashes. The `HEAD` pseudo-ref is skipped.
    for remote in remotes(repo) {
        for name in ref_names(repo, &format!("refs/remotes/{remote}"), 3) {
            if name != "HEAD" {
                names.insert(name);
            }
        }
    }

    names.into_iter().collect()
}

/// The branch names under `refspec`, with the leading `lstrip` path components
/// removed (so `refs/heads/feature/x` with `lstrip=2` yields `feature/x`).
/// Empty when `repo` is not a git repository or has no matching refs.
fn ref_names(repo: &Path, refspec: &str, lstrip: u32) -> Vec<String> {
    let format = format!("--format=%(refname:lstrip={lstrip})");
    git_capture(repo, &["for-each-ref", &format, refspec])
        .ok()
        .flatten()
        .map(|out| {
            out.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The configured remote names of `repo` (e.g. `["origin"]`), or empty when
/// there are none.
fn remotes(repo: &Path) -> Vec<String> {
    git_capture(repo, &["remote"])
        .ok()
        .flatten()
        .map(|out| {
            out.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The branch the remote's `HEAD` points at (e.g. `main`), if a remote exists.
fn remote_default_branch(repo: &Path) -> Option<String> {
    let symref = git_capture(repo, &["symbolic-ref", "refs/remotes/origin/HEAD"])
        .ok()
        .flatten()?;
    symref
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// The currently checked-out branch, or `None` for a detached HEAD.
fn current_branch(repo: &Path) -> Option<String> {
    let branch = git_capture(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .flatten()?;
    (branch != "HEAD").then_some(branch)
}

/// Return the upstream tracking branch of `branch` (e.g. `origin/feature`).
pub fn upstream_of(repo: &Path, branch: &str) -> Option<String> {
    git_capture(
        repo,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &format!("{branch}@{{upstream}}"),
        ],
    )
    .ok()
    .flatten()
    .filter(|s| !s.is_empty())
}

/// Count how many commits `branch` is **ahead of** and **behind** its
/// integration target, as `(ahead, behind)`.
///
/// - `ahead` — commits on `branch` that are not on the target (its own,
///   un-merged work). `0` means everything it carries is already on the target.
/// - `behind` — commits on the target that are not on `branch` (the target has
///   moved past it). `0` means it is even with (or ahead of) the target.
///
/// `into` is resolved against the remote default branch first (`origin/<into>`),
/// then the local branch, so the answer reflects what has landed on the
/// integration branch even before a local fetch updates it. Returns `None` when
/// the counts cannot be computed (e.g. an unrelated history or a ref that does
/// not resolve).
pub fn ahead_behind(repo: &Path, branch: &str, into: &str) -> Option<(usize, usize)> {
    let target = if rev_exists(repo, &format!("origin/{into}")) {
        format!("origin/{into}")
    } else {
        into.to_string()
    };

    // `--left-right --count A...B` prints "<left>\t<right>": commits reachable
    // from the target but not the branch (behind), then from the branch but not
    // the target (ahead).
    let range = format!("{target}...{branch}");
    let output = git_capture(repo, &["rev-list", "--left-right", "--count", &range])
        .ok()
        .flatten()?;
    let mut counts = output.split_whitespace();
    let behind = counts.next()?.parse().ok()?;
    let ahead = counts.next()?.parse().ok()?;
    Some((ahead, behind))
}

/// Return `true` if `rev` resolves to a commit in the repository.
fn rev_exists(repo: &Path, rev: &str) -> bool {
    git_capture(repo, &["rev-parse", "--verify", "--quiet", rev])
        .ok()
        .flatten()
        .is_some()
}

/// Shorten a full commit hash to its 7-character abbreviation.
pub fn short_hash(head: &str) -> String {
    head.chars().take(7).collect()
}

/// A `git -C <repo>` command with repo-scoping env vars stripped, for tests.
///
/// Shared so every test that shells out to git is isolated from an inherited
/// `GIT_DIR` (e.g. when the suite runs inside a git hook).
#[cfg(test)]
pub(crate) fn test_command(repo: &Path) -> Command {
    git_command(repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a git command in `dir`, asserting success.
    fn run(dir: &Path, args: &[&str]) {
        assert!(
            test_command(dir).args(args).status().unwrap().success(),
            "git {args:?} failed"
        );
    }

    /// A repo on `main` with one commit and no remote.
    fn init_repo(dir: &Path) {
        run(dir, &["init", "-q", "-b", "main"]);
        run(dir, &["config", "user.email", "t@e.com"]);
        run(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("f"), "x").unwrap();
        run(dir, &["add", "."]);
        run(dir, &["commit", "-q", "-m", "init"]);
    }

    /// A repo with a remote, so `origin/*` refs and an upstream exist.
    ///
    /// Built without `git clone` so the result does not depend on the host's
    /// `init.defaultBranch` (which differs between developer machines and CI):
    /// the work repo is created explicitly on `main`, then pushed with `-u` to
    /// a bare remote to establish the upstream and `origin/main` ref.
    fn repo_with_remote() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("remote.git");
        let work = tmp.path().join("work");

        run(
            tmp.path(),
            &["init", "-q", "--bare", bare.to_str().unwrap()],
        );

        std::fs::create_dir_all(&work).unwrap();
        init_repo(&work);
        run(&work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        // `-u` records origin/main as the upstream and creates the remote ref.
        run(&work, &["push", "-q", "-u", "origin", "main"]);
        // Point refs/remotes/origin/HEAD at origin/main explicitly.
        run(&work, &["remote", "set-head", "origin", "main"]);
        (tmp, work)
    }

    #[test]
    fn lists_worktrees_with_primary_first() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());

        let worktrees = list_worktrees(dir.path()).unwrap();
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0].branch.as_deref(), Some("main"));
        assert!(!worktrees[0].head.is_empty());
        assert_eq!(primary_worktree(dir.path()).unwrap(), worktrees[0].path);
    }

    #[test]
    fn errors_when_not_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_worktrees(dir.path()).is_err());
        assert!(primary_worktree(dir.path()).is_err());
    }

    #[test]
    fn lists_multiple_worktrees_including_a_detached_one() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let feature = dir.path().join("feature");
        let detached = dir.path().join("detached");
        run(
            dir.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                feature.to_str().unwrap(),
            ],
        );
        // A detached worktree emits a `detached` line (no `branch`), exercising
        // the parser's fall-through and yielding `branch: None`.
        run(
            dir.path(),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                detached.to_str().unwrap(),
            ],
        );

        let worktrees = list_worktrees(dir.path()).unwrap();
        let branches: Vec<_> = worktrees
            .iter()
            .filter_map(|w| w.branch.as_deref())
            .collect();
        assert_eq!(worktrees.len(), 3);
        assert!(branches.contains(&"main"));
        assert!(branches.contains(&"feature"));
        // Exactly one worktree (the detached one) has no branch.
        assert_eq!(worktrees.iter().filter(|w| w.branch.is_none()).count(), 1);
    }

    #[test]
    fn default_branch_prefers_remote_head() {
        let (_tmp, work) = repo_with_remote();
        assert_eq!(default_branch(&work), "main");
    }

    #[test]
    fn default_branch_falls_back_without_remote() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        // No origin/HEAD: falls back to the checked-out branch.
        assert_eq!(default_branch(dir.path()), "main");
    }

    #[test]
    fn default_branch_falls_back_to_main_when_detached() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        run(dir.path(), &["checkout", "-q", "--detach"]);
        // Detached HEAD and no remote: the hard-coded fallback applies.
        assert_eq!(default_branch(dir.path()), "main");
    }

    #[test]
    fn upstream_is_some_when_tracking_and_none_otherwise() {
        let (_tmp, work) = repo_with_remote();
        assert_eq!(upstream_of(&work, "main").as_deref(), Some("origin/main"));

        let local = tempfile::tempdir().unwrap();
        init_repo(local.path());
        assert_eq!(upstream_of(local.path(), "main"), None);
    }

    #[test]
    fn ahead_behind_counts_against_local_and_remote() {
        let (_tmp, work) = repo_with_remote();
        // origin/main exists, so the remote ref is used as the target. main is
        // even with itself: nothing ahead, nothing behind.
        assert_eq!(ahead_behind(&work, "main", "main"), Some((0, 0)));

        let local = tempfile::tempdir().unwrap();
        init_repo(local.path());
        run(local.path(), &["branch", "feature"]);
        // No origin/main: the local branch is used. A freshly cut branch carries
        // no commits of its own and the default has not moved → (0, 0).
        assert_eq!(ahead_behind(local.path(), "feature", "main"), Some((0, 0)));

        // A commit on feature puts it one ahead of main, still zero behind.
        run(local.path(), &["checkout", "-q", "feature"]);
        std::fs::write(local.path().join("g"), "y").unwrap();
        run(local.path(), &["add", "."]);
        run(local.path(), &["commit", "-q", "-m", "ahead"]);
        assert_eq!(ahead_behind(local.path(), "feature", "main"), Some((1, 0)));

        // Advancing main past feature's base (a separate commit on main) makes
        // feature one behind as well as one ahead.
        run(local.path(), &["checkout", "-q", "main"]);
        std::fs::write(local.path().join("h"), "z").unwrap();
        run(local.path(), &["add", "."]);
        run(local.path(), &["commit", "-q", "-m", "main ahead"]);
        assert_eq!(ahead_behind(local.path(), "feature", "main"), Some((1, 1)));
    }

    #[test]
    fn clone_copies_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        init_repo(&src);

        let dest = tmp.path().join("dest");
        clone(src.to_str().unwrap(), &dest, None).unwrap();

        assert!(dest.join(".git").is_dir());
        assert!(dest.join("f").is_file());
    }

    #[test]
    fn clone_checks_out_the_requested_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        init_repo(&src);
        run(&src, &["branch", "feature"]);

        let dest = tmp.path().join("dest");
        clone(src.to_str().unwrap(), &dest, Some("feature")).unwrap();

        let head = git_capture(&dest, &["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap()
            .unwrap();
        assert_eq!(head, "feature");
    }

    #[test]
    fn clone_fails_for_a_missing_source() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        let dest = tmp.path().join("dest");

        let err = clone(missing.to_str().unwrap(), &dest, None).unwrap_err();
        assert!(err.to_string().contains("git clone failed"));
    }

    #[test]
    fn short_hash_takes_first_seven_chars() {
        assert_eq!(short_hash("0123456789abcdef"), "0123456");
        assert_eq!(short_hash("abc"), "abc");
    }

    #[test]
    fn add_worktree_creates_a_new_branch_checkout() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let dest = dir.path().join("wt");

        add_worktree(dir.path(), &dest, "feature", None).unwrap();

        // The new worktree exists and is checked out on the new branch.
        assert!(dest.join("f").is_file());
        let head = git_capture(&dest, &["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap()
            .unwrap();
        assert_eq!(head, "feature");
        // It is registered as a worktree of the repo (compare canonical paths,
        // since git resolves symlinks like macOS's /var -> /private/var).
        let canonical = dest.canonicalize().unwrap();
        let worktrees = list_worktrees(dir.path()).unwrap();
        assert!(worktrees.iter().any(|w| w
            .path
            .canonicalize()
            .map(|p| p == canonical)
            .unwrap_or(false)));
    }

    #[test]
    fn worktree_head_reports_branch_and_commit() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let (branch, head) = worktree_head(dir.path()).unwrap();
        assert_eq!(branch.as_deref(), Some("main"));
        assert_eq!(head.len(), 40);
        // Detached HEAD reports no branch.
        run(dir.path(), &["checkout", "-q", "--detach"]);
        assert_eq!(worktree_head(dir.path()).unwrap().0, None);
        // A non-repo path yields nothing.
        let plain = tempfile::tempdir().unwrap();
        assert!(worktree_head(plain.path()).is_none());
    }

    #[test]
    fn has_uncommitted_changes_detects_a_dirty_tree() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        assert!(!has_uncommitted_changes(dir.path()));
        // An untracked file makes the tree dirty.
        std::fs::write(dir.path().join("new"), "y").unwrap();
        assert!(has_uncommitted_changes(dir.path()));
        // A non-repo path reports clean rather than erroring.
        let plain = tempfile::tempdir().unwrap();
        assert!(!has_uncommitted_changes(plain.path()));
    }

    #[test]
    fn remove_worktree_deletes_a_clean_one_and_needs_force_when_dirty() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let clean = dir.path().join("clean");
        add_worktree(dir.path(), &clean, "clean", None).unwrap();
        remove_worktree(dir.path(), &clean, false).unwrap();
        assert!(!clean.exists());

        // A dirty worktree cannot be removed without force.
        let dirty = dir.path().join("dirty");
        add_worktree(dir.path(), &dirty, "dirty", None).unwrap();
        std::fs::write(dirty.join("scratch"), "z").unwrap();
        assert!(remove_worktree(dir.path(), &dirty, false).is_err());
        // ...but force discards it.
        remove_worktree(dir.path(), &dirty, true).unwrap();
        assert!(!dirty.exists());
    }

    #[test]
    fn delete_branch_removes_a_branch_and_errors_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        run(dir.path(), &["branch", "doomed"]);
        delete_branch(dir.path(), "doomed").unwrap();
        // Deleting it again fails (it is gone).
        assert!(delete_branch(dir.path(), "doomed").is_err());
    }

    #[test]
    fn add_worktree_fails_for_an_existing_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        run(dir.path(), &["branch", "feature"]);

        // `-b feature` cannot create a branch that already exists.
        let err = add_worktree(dir.path(), &dir.path().join("wt"), "feature", None).unwrap_err();
        assert!(err.to_string().contains("git worktree add failed"));
    }

    #[test]
    fn add_worktree_branches_from_the_given_base() {
        // A repo with two commits: the session branch is cut from the *first*
        // commit (tagged `base`), proving the base ref is honoured rather than
        // the current HEAD.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let base = git_capture(dir.path(), &["rev-parse", "HEAD"])
            .unwrap()
            .unwrap();
        run(dir.path(), &["branch", "base"]);
        std::fs::write(dir.path().join("f"), "second").unwrap();
        run(dir.path(), &["commit", "-aqm", "second"]);

        let dest = dir.path().join("wt");
        add_worktree(dir.path(), &dest, "feature", Some("base")).unwrap();

        let head = git_capture(&dest, &["rev-parse", "HEAD"]).unwrap().unwrap();
        assert_eq!(head, base);
    }

    #[test]
    fn resolve_base_ref_prefers_remote_then_falls_back_to_local() {
        let (_tmp, work) = repo_with_remote();
        // With a remote, Remote resolves to origin/<default>...
        assert_eq!(
            resolve_base_ref(&work, BranchSource::Remote, None).as_deref(),
            Some("origin/main")
        );
        // ...while Local stays on the local branch.
        assert_eq!(
            resolve_base_ref(&work, BranchSource::Local, None).as_deref(),
            Some("main")
        );

        // Without a remote, Remote falls back to the local default branch.
        let local = tempfile::tempdir().unwrap();
        init_repo(local.path());
        assert_eq!(
            resolve_base_ref(local.path(), BranchSource::Remote, None).as_deref(),
            Some("main")
        );
        assert_eq!(
            resolve_base_ref(local.path(), BranchSource::Local, None).as_deref(),
            Some("main")
        );
    }

    #[test]
    fn resolve_base_ref_honours_an_explicit_branch() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        run(dir.path(), &["branch", "develop"]);

        // A named branch overrides the detected default, in both forms.
        assert_eq!(
            resolve_base_ref(dir.path(), BranchSource::Local, Some("develop")).as_deref(),
            Some("develop")
        );
        // No origin/develop exists, so Remote falls back to the local branch.
        assert_eq!(
            resolve_base_ref(dir.path(), BranchSource::Remote, Some("develop")).as_deref(),
            Some("develop")
        );
        // A branch that does not resolve yields None (caller falls back to HEAD).
        assert_eq!(
            resolve_base_ref(dir.path(), BranchSource::Local, Some("ghost")),
            None
        );
    }

    #[test]
    fn resolve_base_ref_is_none_without_the_default_branch() {
        // A fresh repo with no commits has no `main` ref, so there is nothing to
        // branch from and the caller should fall back to HEAD.
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"]);
        assert_eq!(
            resolve_base_ref(dir.path(), BranchSource::Local, None),
            None
        );
        assert_eq!(
            resolve_base_ref(dir.path(), BranchSource::Remote, None),
            None
        );
    }

    #[test]
    fn list_branches_returns_local_and_remote_names_deduped() {
        // A repo with a remote: local `main` plus a local `develop`, and the
        // remote-tracking `origin/main`. The duplicate `main` collapses and the
        // remote prefix is stripped, leaving a sorted, unique list.
        let (_tmp, work) = repo_with_remote();
        run(&work, &["branch", "develop"]);

        assert_eq!(list_branches(&work), vec!["develop", "main"]);

        // A branch that lives only on the remote still surfaces (prefix stripped).
        run(&work, &["branch", "feature/x"]);
        run(&work, &["push", "-q", "origin", "feature/x"]);
        run(&work, &["branch", "-D", "feature/x"]);
        assert_eq!(list_branches(&work), vec!["develop", "feature/x", "main"]);
    }

    #[test]
    fn list_branches_is_empty_for_a_non_repo() {
        let plain = tempfile::tempdir().unwrap();
        assert!(list_branches(plain.path()).is_empty());
    }

    #[test]
    fn is_repository_detects_git_and_plain_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        assert!(is_repository(&repo));

        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(!is_repository(&plain));
    }
}
