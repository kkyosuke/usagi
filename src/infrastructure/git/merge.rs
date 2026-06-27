//! Mutating git operations for bringing a branch up to date: fetching from the
//! remote and merging one ref into a worktree.
//!
//! These back `usecase::update`, which refreshes a workspace's default branch
//! from `origin` and propagates it into each session's worktrees. Unlike the
//! read-only inspection in the sibling modules, these change repository state —
//! but [`merge`] is conflict-safe: a merge that would conflict is aborted so the
//! worktree is left exactly as it was.

use std::path::Path;

use anyhow::{bail, Context, Result};

use super::command::{git_capture, git_command};

/// The outcome of a [`merge`] attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStatus {
    /// The worktree already contained everything in the target; HEAD unchanged.
    AlreadyUpToDate,
    /// HEAD advanced — either a fast-forward or a new merge commit.
    Updated,
    /// The merge hit conflicts and was aborted; the worktree is restored to its
    /// pre-merge state (only possible without `ff_only`).
    Conflict,
    /// A `ff_only` merge could not fast-forward because the histories diverged;
    /// the worktree is untouched.
    NotFastForward,
}

/// Fetch `origin` in `repo` (`git fetch origin`), updating its remote-tracking
/// refs so a later [`merge`] of `origin/<branch>` sees the latest commits.
///
/// Output is captured (not inherited) so it never disturbs an active TUI; on
/// failure — e.g. the repository has no `origin` remote, or the network is
/// unreachable — the captured stderr is surfaced. Repo-scoping env vars are
/// stripped so an inherited `GIT_DIR` cannot redirect the fetch to another
/// repository.
pub fn fetch(repo: &Path) -> Result<()> {
    let output = git_command(repo)
        .args(["fetch", "origin"])
        .output()
        .context("failed to run `git fetch origin`")?;
    if !output.status.success() {
        bail!(
            "git fetch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Merge `target` (e.g. `origin/main`) into the branch checked out at
/// `worktree`, returning what happened.
///
/// With `ff_only` the merge only fast-forwards: if the branch has commits the
/// target lacks (the histories diverged) it makes no commit and reports
/// [`MergeStatus::NotFastForward`], leaving the worktree untouched — used to keep
/// the default branch a strict mirror of the remote rather than creating a merge
/// commit on it.
///
/// Without `ff_only` a real merge is allowed (a merge commit when the branch has
/// its own work). If that merge conflicts, it is **aborted** so the worktree is
/// restored to its pre-merge HEAD and [`MergeStatus::Conflict`] is returned — the
/// caller can then skip that worktree without leaving it half-merged. The caller
/// is expected to merge into a clean worktree (a dirty tree is skipped upstream),
/// so a non-`ff_only` failure is a content conflict, not a refusal to clobber
/// local changes.
///
/// Whether HEAD actually advanced is decided by comparing it before and after
/// (rather than scraping git's prose), so the result is locale-independent.
pub fn merge(worktree: &Path, target: &str, ff_only: bool) -> Result<MergeStatus> {
    let before = head(worktree);

    let mut command = git_command(worktree);
    command.args(["merge", "--no-edit"]);
    if ff_only {
        command.arg("--ff-only");
    }
    // `--` ends option parsing so a target beginning with `-` is taken as the
    // ref operand, matching the other git wrappers in this crate.
    command.arg("--").arg(target);
    let output = command.output().context("failed to run `git merge`")?;

    if output.status.success() {
        let after = head(worktree);
        return Ok(if after == before {
            MergeStatus::AlreadyUpToDate
        } else {
            MergeStatus::Updated
        });
    }

    if ff_only {
        // `git merge --ff-only` that cannot fast-forward makes no commit and
        // leaves nothing to abort.
        return Ok(MergeStatus::NotFastForward);
    }

    // A failed real merge left conflict state behind; abort it so the worktree
    // returns to `before`. Best-effort: if there is nothing to abort, the error
    // is irrelevant — the worktree is already where we want it.
    let _ = git_command(worktree).args(["merge", "--abort"]).output();
    Ok(MergeStatus::Conflict)
}

/// The worktree's current HEAD commit, or `None` when it cannot be read (e.g. an
/// unborn branch with no commits yet). Compared before/after a merge to decide
/// whether it advanced.
fn head(worktree: &Path) -> Option<String> {
    git_capture(worktree, &["rev-parse", "HEAD"]).ok().flatten()
}
