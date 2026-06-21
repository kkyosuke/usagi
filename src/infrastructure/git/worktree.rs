//! Worktree operations: creating, listing, inspecting, and removing worktrees.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use super::command::{git_capture, git_command};

/// A worktree as reported by `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// Branch short name (e.g. `main`), or `None` for a detached HEAD.
    pub branch: Option<String>,
    /// Full commit hash currently checked out.
    pub head: String,
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
    // `branch` is consumed as the value of `-b`, but `--` before the positional
    // operands keeps a leading-`-` path or base from being parsed as an option.
    let mut command = git_command(repo);
    command
        .args(["worktree", "add", "-b", branch, "--"])
        .arg(dest);
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

/// Prune worktree registrations whose working directory no longer exists.
///
/// Runs `git -C <repo> worktree prune`. When a session directory under
/// `.usagi/sessions/` is deleted out-of-band — a crash, a manual `rm`, or a
/// teardown that removed the directory but not the registration (e.g. a worktree
/// left on an unexpected branch) — git keeps a dangling "prunable" registration
/// at that path. A later `git worktree add` at the same path then fails with
/// `'<path>' is a missing but already registered worktree`. Pruning first clears
/// those stale registrations so a fresh session can reuse the path. Output is
/// captured (not inherited) so it never disturbs an active TUI; on failure the
/// captured stderr is surfaced. Repo-scoping env vars are stripped for the same
/// reason as [`add_worktree`].
pub fn prune_worktrees(repo: &Path) -> Result<()> {
    let output = git_command(repo)
        .args(["worktree", "prune"])
        .output()
        .context("failed to run `git worktree prune`")?;
    if !output.status.success() {
        bail!(
            "git worktree prune failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Initialize and check out the git submodules of the worktree at `worktree`.
///
/// Runs `git -C <worktree> submodule update --init --recursive` — the same
/// operation `git clone --recursive` performs — so a freshly created session
/// worktree in a repository with submodules has them checked out and ready to
/// work in. A repository without submodules has no tracked `.gitmodules`, so the
/// work is skipped entirely and no git process is spawned. Output is captured
/// (not inherited) so it never disturbs an active TUI; on failure the captured
/// stderr is surfaced. Repo-scoping env vars are stripped for the same reason as
/// [`add_worktree`].
pub fn init_submodules(worktree: &Path) -> Result<()> {
    // No `.gitmodules` means no submodules: skip the subprocess entirely.
    if !worktree.join(".gitmodules").exists() {
        return Ok(());
    }
    let output = git_command(worktree)
        .args(["submodule", "update", "--init", "--recursive"])
        .output()
        .context("failed to run `git submodule update --init --recursive`")?;
    if !output.status.success() {
        bail!(
            "git submodule update failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The branch, HEAD, upstream, and dirtiness of a worktree, read together in a
/// single `git status` invocation (see [`worktree_status`]).
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeStatus {
    /// Full HEAD commit hash; empty for an unborn branch (no commits yet).
    pub head: String,
    /// Checked-out branch short name (e.g. `main`), or `None` for a detached
    /// HEAD.
    pub branch: Option<String>,
    /// Upstream tracking branch (e.g. `origin/feature`), or `None` when the
    /// branch tracks nothing.
    pub upstream: Option<String>,
    /// `true` when the working tree has uncommitted changes — modified, staged,
    /// or untracked files (anything `git status` reports as an entry).
    pub dirty: bool,
}

/// Read the branch, HEAD, upstream, and dirtiness of the worktree at `path` in
/// **one** `git status --porcelain=v2 --branch` call, or `None` if it is not a
/// git worktree.
///
/// A workspace refresh inspects every session worktree, so collapsing what used
/// to be four separate `git` invocations (`rev-parse HEAD`, `rev-parse
/// --abbrev-ref HEAD`, the upstream lookup, and `status --porcelain`) into a
/// single process is the dominant saving on a multi-session workspace.
///
/// The porcelain v2 header lines carry everything needed:
///
/// - `# branch.oid <sha>` — the HEAD commit (`(initial)` on an unborn branch).
/// - `# branch.head <name>` — the branch (`(detached)` for a detached HEAD).
/// - `# branch.upstream <ref>` — present only when the branch tracks an upstream.
///
/// Any non-header, non-empty line is a changed/untracked entry, so its presence
/// marks the tree dirty (matching `git status --porcelain`, which also counts
/// untracked files).
pub fn worktree_status(path: &Path) -> Option<WorktreeStatus> {
    let output = git_capture(path, &["status", "--porcelain=v2", "--branch"])
        .ok()
        .flatten()?;

    let mut status = WorktreeStatus {
        head: String::new(),
        branch: None,
        upstream: None,
        dirty: false,
    };
    for line in output.lines() {
        if let Some(oid) = line.strip_prefix("# branch.oid ") {
            // "(initial)" marks an unborn branch with no commits yet.
            if oid != "(initial)" {
                status.head = oid.to_string();
            }
        } else if let Some(head) = line.strip_prefix("# branch.head ") {
            // "(detached)" marks a detached HEAD: no branch.
            if head != "(detached)" {
                status.branch = Some(head.to_string());
            }
        } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
            status.upstream = Some(upstream.to_string());
        } else if !line.starts_with('#') && !line.is_empty() {
            // A changed, unmerged, or untracked entry: the tree is dirty.
            status.dirty = true;
        }
    }
    Some(status)
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        // A path git does not recognise as a worktree is already in the desired
        // end state: a session whose worktree was never built, or a repeated
        // removal after a partial earlier one. Treat it as a no-op so callers
        // can finish cleaning up the rest of the session instead of aborting.
        if stderr.contains("is not a working tree") {
            return Ok(());
        }
        bail!("git worktree remove failed: {}", stderr.trim());
    }
    Ok(())
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
