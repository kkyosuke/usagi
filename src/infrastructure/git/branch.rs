//! Branch operations: detecting the default branch, resolving base refs,
//! listing candidate branches, and ahead/behind/upstream queries.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::domain::settings::BranchSource;

use super::command::{git_capture, git_command, ref_names, remotes, rev_exists};

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

/// The first local branch nested under `name/` in `repo`, if any.
///
/// Git stores branches as files under `.git/refs/heads/`, so a branch named
/// `<name>` cannot coexist with branches under `<name>/` — creating
/// `refs/heads/<name>` (a file) clashes with the existing `refs/heads/<name>/…`
/// (a directory). In that state `git worktree add -b <name>` fails with a
/// cryptic `cannot lock ref` error, so callers check this first to refuse the
/// name with a clear message. An exact existing branch `<name>` is *not* a
/// conflict here (git reports that on its own); only nested branches are.
/// Returns `None` when no nested branch exists (or `repo` has none).
pub fn branch_namespace_conflict(repo: &Path, name: &str) -> Option<String> {
    // `for-each-ref refs/heads/<name>` matches the exact ref and everything
    // nested under it; the nested entries (short name `!= name`) are the clash.
    ref_names(repo, &format!("refs/heads/{name}"), 2)
        .into_iter()
        .find(|branch| branch != name)
}

/// The short names of every **local** branch in `repo` (e.g. `main`,
/// `test/foo`), in ref order. Empty when `repo` is not a git repository or has
/// no branches yet.
///
/// Unlike [`list_branches`] this excludes remote-tracking branches: only local
/// refs constrain what `git worktree add -b <name>` can create, so this is the
/// set a new session name is validated against (see
/// [`branch_namespace_conflict`]).
pub fn local_branches(repo: &Path) -> Vec<String> {
    ref_names(repo, "refs/heads", 2)
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
    // Prefer the remote integration branch; fall back to the local one when no
    // remote ref exists. Attempting the remote range directly (and falling back
    // on failure) avoids a separate `rev-parse --verify` existence check — one
    // `git` process instead of two in the common, remote-tracked case.
    count_ahead_behind(repo, &format!("origin/{into}"), branch)
        .or_else(|| count_ahead_behind(repo, into, branch))
}

/// Count `(ahead, behind)` of `branch` relative to `target`, or `None` when the
/// range cannot be resolved (e.g. `target` does not exist).
fn count_ahead_behind(repo: &Path, target: &str, branch: &str) -> Option<(usize, usize)> {
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
