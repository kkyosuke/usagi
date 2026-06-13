//! Read-only git inspection used to build a repository's workspace state.
//!
//! All operations shell out to the system `git` binary (rather than linking a
//! git library) so the user's existing git configuration is respected and the
//! crate stays dependency-light. Everything here is read-only.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

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
        .with_context(|| format!("failed to run `git {}`", args.join(" ")))?;
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
    list_worktrees(repo)?
        .into_iter()
        .next()
        .map(|w| w.path)
        .ok_or_else(|| anyhow!("no worktrees found in {}", repo.display()))
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

    if worktrees.is_empty() {
        bail!("{} is not a git repository", repo.display());
    }
    Ok(worktrees)
}

/// Determine the repository's default branch (e.g. `main`).
///
/// Prefers the remote's HEAD (`origin/HEAD`); falls back to the current branch
/// of the primary worktree, then to `main`.
pub fn default_branch(repo: &Path) -> String {
    if let Ok(Some(symref)) = git_capture(repo, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        if let Some(name) = symref.rsplit('/').next() {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    if let Ok(Some(branch)) = git_capture(repo, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        if branch != "HEAD" {
            return branch;
        }
    }
    "main".to_string()
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
