//! Repository-level operations: cloning, dirtiness, repo detection, and hashes.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::command::{git_capture, REPO_SCOPING_ENV};

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

/// Shorten a full commit hash to its 7-character abbreviation.
pub fn short_hash(head: &str) -> String {
    head.chars().take(7).collect()
}
