//! Read-only git inspection used to build a repository's workspace state.
//!
//! All operations shell out to the system `git` binary (rather than linking a
//! git library) so the user's existing git configuration is respected and the
//! crate stays dependency-light. Everything here is read-only.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};

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

/// Return `true` if `branch` has been merged into `into` (an ancestor check).
///
/// `into` is resolved against the remote default branch first
/// (`origin/<into>`), then the local branch, so the answer reflects what has
/// landed on the integration branch even before a local fetch updates it.
pub fn is_merged(repo: &Path, branch: &str, into: &str) -> bool {
    let target = if rev_exists(repo, &format!("origin/{into}")) {
        format!("origin/{into}")
    } else {
        into.to_string()
    };

    git_command(repo)
        .args(["merge-base", "--is-ancestor", branch, &target])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
    fn is_merged_detects_ancestor_against_local_and_remote() {
        let (_tmp, work) = repo_with_remote();
        // origin/main exists, so the remote ref is used as the target.
        assert!(is_merged(&work, "main", "main"));

        let local = tempfile::tempdir().unwrap();
        init_repo(local.path());
        run(local.path(), &["branch", "feature"]);
        // No origin/main: the local branch is used. An unrelated, ahead branch
        // is not an ancestor.
        assert!(is_merged(local.path(), "feature", "main"));
        run(local.path(), &["checkout", "-q", "feature"]);
        std::fs::write(local.path().join("g"), "y").unwrap();
        run(local.path(), &["add", "."]);
        run(local.path(), &["commit", "-q", "-m", "ahead"]);
        assert!(!is_merged(local.path(), "feature", "main"));
    }

    #[test]
    fn short_hash_takes_first_seven_chars() {
        assert_eq!(short_hash("0123456789abcdef"), "0123456");
        assert_eq!(short_hash("abc"), "abc");
    }
}
