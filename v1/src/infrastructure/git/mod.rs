//! Git operations used to build and maintain a repository's workspace state.
//!
//! All operations shell out to the system `git` binary (rather than linking a
//! git library) so the user's existing git configuration is respected and the
//! crate stays dependency-light. Most are read-only inspection; the worktree
//! lifecycle ([`worktree`]) and the fetch/merge in [`merge`] mutate state.
//!
//! The work is split by concern: [`command`] holds the shared shell-out
//! helpers, [`repo`] the repository-level operations (clone, dirtiness, repo
//! detection), [`worktree`] the worktree lifecycle, [`branch`] the branch
//! and base-ref queries, and [`merge`] the fetch/merge that brings a branch up
//! to date. The functions are re-exported here so callers use the flat
//! `git::<fn>` path regardless of which submodule defines them.

mod branch;
mod command;
mod merge;
mod repo;
mod worktree;

pub use branch::{
    ahead_behind, ahead_behind_against, branch_exists, branch_namespace_conflict, default_branch,
    delete_branch, diff_stat, diff_stat_against, diff_text, file_exists_at_rev, integration_base,
    list_branches, local_branches, resolve_base_ref, resolve_commit, IntegrationBase,
};
pub use merge::{fetch, merge, MergeStatus};
pub use repo::{clone, has_origin, is_repository, short_hash};
pub use worktree::{
    add_worktree, ensure_all_excluded, ensure_excluded, git_common_dir, init_submodules,
    list_worktrees, primary_worktree, prune_worktrees, remove_worktree, worktree_status,
    WorktreeInfo, WorktreeStatus,
};

/// A `git -C <repo>` command with repo-scoping env vars stripped, for tests.
///
/// Shared so every test that shells out to git is isolated from an inherited
/// `GIT_DIR` (e.g. when the suite runs inside a git hook).
#[cfg(test)]
pub(crate) fn test_command(repo: &std::path::Path) -> std::process::Command {
    command::git_command(repo)
}

#[cfg(test)]
mod tests;
