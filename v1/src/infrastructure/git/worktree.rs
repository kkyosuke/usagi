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

/// Ensure `pattern` is listed in the worktree's local git exclude file
/// (`$GIT_DIR/info/exclude`).
///
/// usagi symlinks its shipped skills into each worktree at `.claude/skills`
/// (see [`crate::infrastructure::skills`]). That symlink is untracked, so in a
/// project that does not already ignore `.claude/` it would show up in `git
/// status` and mark the session dirty — blocking `remove` / `finish` and
/// flagging the session in the TUI. Excluding it here keeps usagi's own artifact
/// invisible to git. The exclude file is local to the repository and never
/// committed or pushed, so the user's tracked `.gitignore` is left untouched.
///
/// Idempotent: the pattern is appended only when absent. The path is resolved
/// through git (`rev-parse --git-path`) so it lands in the right place for a
/// linked worktree, whose `info/exclude` lives in the shared common dir.
pub fn ensure_excluded(worktree: &Path, pattern: &str) -> Result<()> {
    ensure_all_excluded(worktree, std::slice::from_ref(&pattern))
}

/// Append every pattern in `patterns` (absent ones only) to the worktree's
/// `info/exclude`, in one pass. See [`ensure_excluded`] for what this hides and
/// why; this batches several patterns so the exclude path is resolved through git
/// once and the file is read and rewritten once, instead of paying a
/// `rev-parse --git-path` plus a read/write per pattern (a session excludes one
/// pattern per shipped skill in every worktree it builds). Idempotent: a pattern
/// already present is left as-is, and the file is only rewritten when something
/// was actually added.
pub fn ensure_all_excluded(worktree: &Path, patterns: &[&str]) -> Result<()> {
    if patterns.is_empty() {
        return Ok(());
    }
    let path = exclude_path(worktree)?;

    // Preserve any existing content (git seeds `info/exclude` with comments) and
    // append each missing pattern on its own line. `content` grows as we go, so a
    // duplicate later in `patterns` sees the copy appended earlier in this loop.
    let mut content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut changed = false;
    for pattern in patterns {
        if content.lines().any(|line| line.trim() == *pattern) {
            continue;
        }
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(pattern);
        content.push('\n');
        changed = true;
    }
    if changed {
        std::fs::write(&path, &content).context(format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

/// Resolve the worktree's `info/exclude` path through git (`rev-parse
/// --git-path`) so it lands in the right place for a linked worktree, whose
/// `info/exclude` lives in the shared common dir.
fn exclude_path(worktree: &Path) -> Result<PathBuf> {
    let args = [
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "info/exclude",
    ];
    match git_capture(worktree, &args)? {
        Some(raw) => Ok(PathBuf::from(raw)),
        None => bail!("{} is not a git worktree", worktree.display()),
    }
}

/// The absolute path of the repository's git **common directory** — the `.git`
/// store shared by every worktree of the repository — or `None` when `path` is
/// not inside a git repository.
///
/// A single cheap `rev-parse`, unlike [`primary_worktree`], which shells out to
/// `git worktree list` and parses every worktree of the repository. Callers that
/// only need to know *which repository* a path belongs to (e.g. to resolve a
/// per-repository property once across many worktrees) use this to avoid an
/// O(worktrees) scan per path.
pub fn git_common_dir(path: &Path) -> Option<PathBuf> {
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    git_capture(path, &args).ok().flatten().map(PathBuf::from)
}

/// Remove the worktree at `worktree` from `repo`. With `force`, discard
/// uncommitted changes; without it, git refuses a dirty worktree.
pub fn remove_worktree(repo: &Path, worktree: &Path, force: bool) -> Result<()> {
    let stderr = match run_worktree_remove(repo, worktree, force)? {
        Ok(()) => return Ok(()),
        Err(stderr) => stderr,
    };

    // A path git does not recognise as a worktree is already in the desired
    // end state: a session whose worktree was never built, or a repeated
    // removal after a partial earlier one. Treat it as a no-op so callers
    // can finish cleaning up the rest of the session instead of aborting.
    if stderr.contains("is not a working tree") {
        return Ok(());
    }

    // git flatly refuses to remove a worktree that contains submodules unless
    // `--force` is given — *regardless of whether it is clean* (`fatal: working
    // trees containing submodules cannot be moved or removed`). That refusal is
    // structural, not a dirtiness guard, so when it is the only obstacle we may
    // retry forced (a forced call no longer matches this branch, so the retry
    // resolves through the success return or the `bail!` below).
    //
    // But `--force` *would* discard uncommitted submodule work, and the caller's
    // dirty gate cannot be trusted to have caught it: that gate uses plain
    // `git status`, which honours the user's `submodule.<name>.ignore` /
    // `diff.ignoreSubmodules` config and so can report a worktree clean while a
    // submodule holds uncommitted changes. So before escalating, re-verify
    // cleanliness in a way that ignores that config; only force when the worktree
    // (and every submodule) is provably clean, otherwise refuse so the work is
    // never silently destroyed.
    if !force && stderr.contains("containing submodules") {
        if worktree_clean_ignoring_submodule_config(worktree) {
            return remove_worktree(repo, worktree, true);
        }
        bail!(
            "refusing to remove worktree {}: it or one of its submodules has \
             uncommitted changes that a forced removal would discard",
            worktree.display()
        );
    }

    bail!("git worktree remove failed: {}", stderr.trim());
}

/// Whether the worktree at `path` — and every submodule under it — has **no**
/// uncommitted change, checked so the answer does not depend on the user's
/// `submodule.<name>.ignore` / `diff.ignoreSubmodules` config.
///
/// `git status --porcelain --ignore-submodules=none` forces submodule changes to
/// be reported regardless of that config, so empty output means provably clean.
/// Used as the safety gate before a destructive `--force` worktree removal: a
/// status that cannot be read at all is treated as *not* provably clean, so the
/// caller refuses to force rather than risk discarding work.
fn worktree_clean_ignoring_submodule_config(path: &Path) -> bool {
    git_capture(path, &["status", "--porcelain", "--ignore-submodules=none"])
        .ok()
        .flatten()
        .map(|out| out.is_empty())
        .unwrap_or(false)
}

/// Run a single `git worktree remove [--force] <worktree>`, returning `Ok(())`
/// on success or the captured stderr on failure so the caller can decide
/// whether the failure is recoverable.
fn run_worktree_remove(
    repo: &Path,
    worktree: &Path,
    force: bool,
) -> Result<std::result::Result<(), String>> {
    let mut command = git_command(repo);
    command.args(["worktree", "remove"]);
    if force {
        command.arg("--force");
    }
    let output = command
        .arg(worktree)
        .output()
        .context("failed to run `git worktree remove`")?;
    if output.status.success() {
        Ok(Ok(()))
    } else {
        Ok(Err(String::from_utf8_lossy(&output.stderr).into_owned()))
    }
}

/// Resolve the absolute path of the repository's primary (main) worktree.
///
/// This is the directory under which `.usagi/` should live, regardless of which
/// worktree the command was invoked from.
pub fn primary_worktree(repo: &Path) -> Result<PathBuf> {
    primary_of(list_worktrees(repo)?, repo)
}

/// The primary (first) worktree's path, or an error when the list is empty.
///
/// `git worktree list` on a real repository always yields at least the current
/// worktree, so this is expected to be infallible in practice. But a future
/// porcelain format change, locale noise, or a wrapper that returns success with
/// no `worktree` lines would make [`list_worktrees`] yield an empty list.
/// Surfacing that as an error keeps it from panicking the process — this runs on
/// the hot status-sync path (`workspace_state::sync` / `load`), where a panic
/// would take the whole TUI down.
pub(super) fn primary_of(worktrees: Vec<WorktreeInfo>, repo: &Path) -> Result<PathBuf> {
    worktrees.into_iter().next().map(|w| w.path).ok_or_else(|| {
        anyhow!(
            "git worktree list returned no worktrees for {}",
            repo.display()
        )
    })
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
