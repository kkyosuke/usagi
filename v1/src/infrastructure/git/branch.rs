//! Branch operations: detecting the default branch, resolving base refs,
//! listing candidate branches, and ahead/behind/upstream queries.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::domain::settings::BranchSource;

use super::command::{full_ref_names, git_capture, git_command, ref_names, rev_exists};

/// Force-delete `branch` in `repo` (`git branch -D`). Used when discarding a
/// session, so the branch is removed regardless of merge status.
pub fn delete_branch(repo: &Path, branch: &str) -> Result<()> {
    // `--` separates options from the branch operand so a name beginning with
    // `-` can never be parsed as a `git branch` option.
    let output = git_command(repo)
        .args(["branch", "-D", "--", branch])
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

/// Whether the exact local branch exists. Teardown checks this before deletion
/// so retrying a partially removed multi-repository session treats an already
/// deleted branch as success without masking real `git branch -D` failures.
pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    rev_exists(repo, &format!("refs/heads/{branch}"))
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

/// A repository's default branch and the ref its sessions are measured against.
///
/// Resolved once per repository (see [`integration_base`]) and shared by every
/// worktree of it: `default` is the default branch's short name — used to ask
/// "is this worktree on the default branch?", which suppresses its ahead/behind
/// and diff — while `base` is the ref those counts and the `+N -M` diff are
/// actually taken against.
pub struct IntegrationBase {
    /// The default branch's short name (e.g. `main`).
    pub default: String,
    /// The ref sessions are measured against: `origin/<default>` when the
    /// repository publishes a remote default branch, else the local `<default>`.
    pub base: String,
}

/// Resolve `repo`'s [`IntegrationBase`]: the default branch name and the ref its
/// sessions' ahead/behind and diffs are measured against, in one place per
/// repository so every worktree reuses the decision.
///
/// When the remote publishes a default branch (`origin/HEAD` resolves), sessions
/// are measured against `origin/<default>`, so the status tracks what has landed
/// on the integration branch even before a local fetch — the same preference
/// [`ahead_behind`] and [`diff_stat`] apply per call. Without a remote default (a
/// local-only repo, or one whose `origin/HEAD` is unset) there is no
/// `origin/<default>` to prefer, so the local `<default>` — the primary
/// worktree's current branch, else `main` — is used directly. Resolving this once
/// lets [`crate::usecase::workspace_state`] skip the speculative `origin/<default>`
/// probe that would otherwise miss (and cost an extra git process) on every
/// worktree of a remote-less repository.
pub fn integration_base(repo: &Path) -> IntegrationBase {
    match remote_default_branch(repo) {
        // `origin/HEAD` points at `origin/<name>`, so that ref is known to exist;
        // measure against it directly without a fallback probe.
        Some(name) => IntegrationBase {
            base: format!("origin/{name}"),
            default: name,
        },
        // No remote default: `origin/<default>` cannot exist, so use the local
        // branch for both the name and the base rather than probing a ref that is
        // guaranteed to miss.
        None => {
            let default = current_branch(repo).unwrap_or_else(|| "main".to_string());
            IntegrationBase {
                base: default.clone(),
                default,
            }
        }
    }
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

/// Resolve `revision` to its immutable commit id.
///
/// The `^{commit}` suffix rejects trees, blobs, and other non-commit objects so
/// callers can safely use the returned id as a worktree base.
pub fn resolve_commit(repo: &Path, revision: &str) -> Option<String> {
    git_capture(
        repo,
        &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
    )
    .ok()
    .flatten()
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

    // One `for-each-ref` over both ref namespaces, with each kind stripped to its
    // bare branch name in Rust, replaces the previous `git remote` + per-remote
    // `for-each-ref` fan-out: a single git process regardless of remote count.
    full_ref_names(repo, &["refs/heads", "refs/remotes"])
        .iter()
        .filter_map(|refname| branch_short_name(refname))
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect()
}

/// The bare branch name a candidate ref contributes to [`list_branches`], or
/// `None` for a ref that names no branch.
///
/// A local `refs/heads/<name>` keeps any embedded slashes; a remote-tracking
/// `refs/remotes/<remote>/<name>` drops the `<remote>` component (again keeping
/// slashes in `<name>`), and the `<remote>/HEAD` pseudo-ref is skipped.
fn branch_short_name(refname: &str) -> Option<String> {
    if let Some(name) = refname.strip_prefix("refs/heads/") {
        return (!name.is_empty()).then(|| name.to_string());
    }
    let (_remote, name) = refname.strip_prefix("refs/remotes/")?.split_once('/')?;
    (name != "HEAD" && !name.is_empty()).then(|| name.to_string())
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

/// Count `(ahead, behind)` of `branch` against an already-resolved integration
/// `base` ref (an [`IntegrationBase::base`]).
///
/// Unlike [`ahead_behind`], `base` is used verbatim — the caller resolved
/// `origin/<default>` versus the local `<default>` once for the whole repository
/// (see [`integration_base`]), so this neither prepends `origin/` nor falls back,
/// sparing the speculative probe on every worktree of a remote-less repository.
pub fn ahead_behind_against(repo: &Path, branch: &str, base: &str) -> Option<(usize, usize)> {
    count_ahead_behind(repo, base, branch)
}

/// The total added / removed line counts of the worktree's cumulative diff
/// against the default branch, as `(added, removed)`, or `None` when it cannot
/// be computed (e.g. an unrelated history or a ref that does not resolve).
///
/// The diff is taken from the working tree to the **merge-base** with the
/// default branch, so it measures only what this session changed — commits the
/// default branch gained afterwards do not inflate the count — and counts both
/// committed and uncommitted work (the right side is the working tree). `into`
/// is resolved against the remote (`origin/<into>`) first, like
/// [`ahead_behind`], so the badge tracks the integration branch even before a
/// local fetch updates it.
pub fn diff_stat(repo: &Path, into: &str) -> Option<(usize, usize)> {
    // `git diff --merge-base <base>` is equivalent to diffing the working tree
    // against `$(git merge-base <base> HEAD)`, so it preserves the previous
    // behaviour (including uncommitted work on the right-hand side) while folding
    // the old `merge-base` + `diff` pair into one git process in the common case.
    diff_stat_against(repo, &format!("origin/{into}")).or_else(|| diff_stat_against(repo, into))
}

/// The `(added, removed)` line counts of the worktree's cumulative diff against
/// an already-resolved `base` ref (an [`IntegrationBase::base`]).
///
/// Unlike [`diff_stat`], `base` is used verbatim — the caller resolved
/// `origin/<default>` versus the local `<default>` once for the repository (see
/// [`integration_base`]), so this skips the fallback probe [`diff_stat`] makes
/// per call.
pub fn diff_stat_against(repo: &Path, base: &str) -> Option<(usize, usize)> {
    let output = git_capture(repo, &["diff", "--numstat", "--merge-base", base])
        .ok()
        .flatten()?;
    Some(sum_numstat(&output))
}

/// The full unified-diff text of what this session changed against `into` — the
/// same cumulative, merge-base diff [`diff_stat`] measures (committed work plus
/// the uncommitted working tree on the right-hand side), but the whole patch
/// rather than just the line counts, for a scrollable diff view.
///
/// `into` is resolved against the remote (`origin/<into>`) first, like
/// [`diff_stat`], falling back to the local branch. Returns `None` only when
/// neither base ref resolves (e.g. an unknown default, or a repo with no commits);
/// a session that changed nothing yields `Some("")` (the base resolved, the diff
/// is empty), so the caller can tell "no changes" apart from "no base".
pub fn diff_text(repo: &Path, into: &str) -> Option<String> {
    diff_text_against(repo, &format!("origin/{into}")).or_else(|| diff_text_against(repo, into))
}

fn diff_text_against(repo: &Path, base: &str) -> Option<String> {
    // `git diff --merge-base <base>` diffs the working tree against
    // `$(git merge-base <base> HEAD)`, matching [`diff_stat`]'s range so the
    // rendered patch and the sidebar `+N -M` badge always describe the same diff.
    git_capture(repo, &["diff", "--merge-base", base])
        .ok()
        .flatten()
}

/// Sum the added / removed columns of `git diff --numstat` output as
/// `(added, removed)`. Each line is `<added>\t<removed>\t<path>`; a binary file
/// reports `-` for both counts, so that row parses as neither and contributes
/// nothing.
fn sum_numstat(output: &str) -> (usize, usize) {
    output.lines().fold((0, 0), |(added, removed), line| {
        let mut cols = line.split('\t');
        match (
            cols.next().and_then(|c| c.parse::<usize>().ok()),
            cols.next().and_then(|c| c.parse::<usize>().ok()),
        ) {
            (Some(a), Some(d)) => (added + a, removed + d),
            _ => (added, removed),
        }
    })
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

/// Return `true` if `path` (relative to the repository root) exists at `rev`.
pub fn file_exists_at_rev(repo: &Path, rev: &str, path: &str) -> bool {
    let rev_path = format!("{rev}:{path}");
    git_capture(repo, &["cat-file", "-e", &rev_path])
        .ok()
        .flatten()
        .is_some()
}
