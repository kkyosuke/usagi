//! Shared helpers for shelling out to the system `git` binary.
//!
//! These build repo-scoped commands and capture their output; the
//! domain-specific operations live in the sibling `repo`, `worktree`, and
//! `branch` modules.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Git environment variables that scope a command to a specific repository.
///
/// When usagi runs from inside a git hook these are set in the environment and
/// would override `-C <repo>`, making git operate on the hook's repository
/// instead of the one we asked about. Clearing them keeps `-C <repo>`
/// authoritative.
pub(super) const REPO_SCOPING_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_NAMESPACE",
];

/// Build a `git -C <repo>` command with repo-scoping env vars stripped and the
/// locale forced to `C`.
///
/// `LC_ALL=C` pins git's human-readable messages to English. Some callers
/// (notably [`super::worktree`]) branch on git's stderr text — e.g. treating
/// "is not a working tree" as a no-op — and a localized git (`LC_ALL=ja_JP`,
/// …) would otherwise emit translated messages those matches would miss. The
/// machine-readable output we parse (`--porcelain`, `--format=…`) is already
/// locale-independent, so this only stabilizes the prose.
pub(super) fn git_command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo);
    command.env("LC_ALL", "C");
    for var in REPO_SCOPING_ENV {
        command.env_remove(var);
    }
    command
}

/// Run `git` with `args` inside `repo` and return trimmed stdout.
///
/// Returns `Ok(None)` when git exits non-zero (e.g. the queried ref does not
/// exist), and an error only when the process itself could not be run.
pub(super) fn git_capture(repo: &Path, args: &[&str]) -> Result<Option<String>> {
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

/// The branch names under `refspec`, with the leading `lstrip` path components
/// removed (so `refs/heads/feature/x` with `lstrip=2` yields `feature/x`).
/// Empty when `repo` is not a git repository or has no matching refs.
pub(super) fn ref_names(repo: &Path, refspec: &str, lstrip: u32) -> Vec<String> {
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

/// The full ref names (e.g. `refs/heads/main`, `refs/remotes/origin/main`)
/// matching any of `refspecs`, fetched in a single `git for-each-ref`. Unlike
/// [`ref_names`] the names are returned in full (no `lstrip`) so a caller mixing
/// ref namespaces can strip each kind itself; empty when `repo` is not a git
/// repository or nothing matches.
pub(super) fn full_ref_names(repo: &Path, refspecs: &[&str]) -> Vec<String> {
    let mut args = vec!["for-each-ref", "--format=%(refname)"];
    args.extend_from_slice(refspecs);
    git_capture(repo, &args)
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

/// Return `true` if `rev` resolves to a commit in the repository.
pub(super) fn rev_exists(repo: &Path, rev: &str) -> bool {
    git_capture(repo, &["rev-parse", "--verify", "--quiet", rev])
        .ok()
        .flatten()
        .is_some()
}
