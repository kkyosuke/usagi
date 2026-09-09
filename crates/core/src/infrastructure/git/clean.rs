//! Git-backed inventory adapter for the pure cleanup planner.

use std::path::Path;

use anyhow::Result;

use super::{GitRunner, list_worktrees};
use crate::domain::workspace_layout::{SESSIONS_DIR, STATE_DIR};
use crate::usecase::clean::{ObservedBranch, ObservedWorktree, RepositoryInventory};

/// Observe the managed worktree and branch namespace for one repository.
///
/// Both the standalone CLI and the running daemon use this adapter, so cleanup
/// classification cannot drift between the offline and daemon-owned surfaces.
///
/// # Errors
///
/// Returns the first Git observation failure. A path outside a Git worktree is
/// represented as `Ok(None)` rather than an empty, cleanup-authorizing
/// inventory.
pub fn observe_repository(git: &dyn GitRunner, root: &Path) -> Result<Option<RepositoryInventory>> {
    let probe = git.run(root, &["rev-parse", "--is-inside-work-tree"])?;
    if !probe.success || probe.stdout.trim() != "true" {
        return Ok(None);
    }
    let expected_parent = root.join(STATE_DIR).join(SESSIONS_DIR);
    let mut worktrees = Vec::new();
    for worktree in list_worktrees(git, root)? {
        if worktree.path.parent() != Some(expected_parent.as_path()) {
            continue;
        }
        let status = git.run(&worktree.path, &["status", "--porcelain"])?;
        worktrees.push(ObservedWorktree {
            path: worktree.path,
            dirty: !status.success || !status.stdout.trim().is_empty() || worktree.branch.is_none(),
            branch: worktree.branch,
        });
    }
    let refs = git.run(
        root,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/usagi/",
        ],
    )?;
    if !refs.success {
        anyhow::bail!("git branch inventory failed: {}", refs.stderr.trim());
    }
    let mut branches = Vec::new();
    for name in refs
        .stdout
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let merged = git
            .run(root, &["merge-base", "--is-ancestor", name, "HEAD"])?
            .success;
        branches.push(ObservedBranch {
            name: name.to_owned(),
            merged,
        });
    }
    Ok(Some(RepositoryInventory {
        root: root.to_path_buf(),
        worktrees,
        branches,
    }))
}
