//! Git authority a managed session receives inside the OS sandbox.
//!
//! The grant is the narrow metadata set a linked worktree needs for a normal
//! `git add` / `git commit`, resolved per repository of the workspace.

use std::path::{Path, PathBuf};

use usagi_core::domain::workspace_layout::session_temporary_branch_directory;
use usagi_daemon::infrastructure::session_worktree::SystemSessionWorktreeIo;
use usagi_daemon::usecase::session_runtime::SessionWorktreeIo as _;

use super::super::secure_path::{ensure_owned_subdirectories, validate_owned_directory};
use super::{read_git_indirection, session_git_common_dir};

/// Git authority granted to one linked session worktree.
pub(in crate::runtime::daemon) struct SessionGitPolicy {
    pub(in crate::runtime::daemon) writable_roots: Vec<PathBuf>,
    pub(in crate::runtime::daemon) read_only_roots: Vec<PathBuf>,
}

/// Resolve the narrow Git metadata set a linked session worktree needs for a
/// normal `git add` / `git commit` transaction, keeping its `.git` file and
/// worktree-admin indirection pointers read-only.
///
/// A Multi-Repo workspace is not a repository itself: its session root is a
/// plain directory mirroring the workspace, with one linked worktree per child
/// repository. Each of those worktrees receives the same narrow grant, anchored
/// to the child repository the daemon enumerates below the workspace root.
pub(in crate::runtime::daemon) fn session_git_policy(
    workspace_root: &Path,
    worktree: &Path,
) -> Result<Option<SessionGitPolicy>, ()> {
    let marker = worktree.join(".git");
    let marker_metadata = match std::fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match std::fs::symlink_metadata(workspace_root.join(".git")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    multi_repo_session_git_policy(workspace_root, worktree)
                }
                _ => Err(()),
            };
        }
        Err(_) => return Err(()),
    };
    if !marker_metadata.file_type().is_file() {
        let workspace_root = workspace_root.canonicalize().map_err(|_| ())?;
        let worktree = worktree.canonicalize().map_err(|_| ())?;
        return if marker_metadata.file_type().is_dir() && worktree == workspace_root {
            Ok(None)
        } else {
            Err(())
        };
    }
    let marker = marker.canonicalize().map_err(|_| ())?;
    linked_worktree_git_policy(workspace_root, worktree, &marker, worktree).map(Some)
}

/// Union the per-repository grants of a Multi-Repo session.
///
/// The repository set comes from the workspace root, which the session cannot
/// write, never from the agent-writable session tree. A child repository with
/// no worktree marker in the session (cloned after the session was created)
/// contributes no authority, and neither does a standalone repository planted
/// there. A linked-worktree marker must sit at its symlink-free path inside the
/// session and pass the same validation as a single-repository session, or the
/// launch is refused.
fn multi_repo_session_git_policy(
    workspace_root: &Path,
    session_root: &Path,
) -> Result<Option<SessionGitPolicy>, ()> {
    let io = SystemSessionWorktreeIo;
    let repositories = io.workspace_repositories(workspace_root).map_err(|_| ())?;
    let mut policy = SessionGitPolicy {
        writable_roots: Vec::new(),
        read_only_roots: Vec::new(),
    };
    let session_root = session_root.canonicalize().map_err(|_| ())?;
    for relative in repositories
        .iter()
        .filter(|relative| !relative.as_os_str().is_empty())
    {
        let worktree = session_root.join(relative);
        match std::fs::symlink_metadata(worktree.join(".git")) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            // A standalone repository inside the writable session tree needs no
            // grant outside it.
            Ok(metadata) if metadata.file_type().is_dir() => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            _ => return Err(()),
        }
        // Every component below the session root is agent-writable. A symlink
        // there would alias the marker of another linked worktree of the same
        // repository, whose backlink check then passes for the wrong worktree.
        if worktree.canonicalize().map_err(|_| ())? != worktree {
            return Err(());
        }
        // The backlink is compared with this daemon-derived path, not with a
        // re-resolved one, so a component swapped after the check above still
        // cannot name another worktree's admin directory.
        let marker = worktree.join(".git");
        let nested = linked_worktree_git_policy(
            &workspace_root.join(relative),
            &worktree,
            &marker,
            &session_root,
        )?;
        policy.writable_roots.extend(nested.writable_roots);
        policy.read_only_roots.extend(nested.read_only_roots);
    }
    Ok((!policy.writable_roots.is_empty()).then_some(policy))
}

/// The grant for one linked worktree whose `.git` marker is a regular file.
/// `expected_marker` is the canonical marker path the admin backlink must name.
fn linked_worktree_git_policy(
    workspace_root: &Path,
    worktree: &Path,
    expected_marker: &Path,
    session_root: &Path,
) -> Result<SessionGitPolicy, ()> {
    let marker = worktree.join(".git");
    let git_dir = read_git_indirection(&marker, Some("gitdir:"), worktree)?;
    let common = session_git_common_dir(worktree)?.ok_or(())?;
    let expected_common = session_git_common_dir(workspace_root)?.ok_or(())?;
    if common != expected_common {
        return Err(());
    }
    let worktrees = common.join("worktrees").canonicalize().map_err(|_| ())?;
    if git_dir.parent() != Some(worktrees.as_path()) {
        return Err(());
    }
    let backlink = read_git_indirection(&git_dir.join("gitdir"), None, &git_dir)?;
    if backlink != expected_marker {
        return Err(());
    }
    let subdirs = ["refs/heads/usagi", "logs/refs/heads/usagi"];
    ensure_owned_subdirectories(&common, &subdirs).map_err(|_| ())?;
    let read_only_roots = vec![backlink, git_dir.join("commondir"), git_dir.join("gitdir")];
    let mut writable_roots = vec![
        git_dir,
        common.join("objects"),
        common.join("refs/heads/usagi"),
        common.join("logs/refs/heads/usagi"),
    ];
    // Multi-repo worktrees use their parent session's name, independent of
    // the child repository name or the branch currently checked out.
    let session = session_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(())?;
    let temporary = session_temporary_branch_directory(session);
    let heads = format!("refs/heads/{temporary}");
    let logs = format!("logs/refs/heads/{temporary}");
    // A pre-existing ref may occupy the prefix. Git cannot create its children
    // in that case, but the Agent can still work on the normal session branch.
    if ensure_owned_subdirectories(&common, &[&heads, &logs]).is_ok() {
        writable_roots.extend([common.join(heads), common.join(logs)]);
    }
    for root in &writable_roots {
        validate_owned_directory(root).map_err(|_| ())?;
    }
    Ok(SessionGitPolicy {
        writable_roots,
        read_only_roots,
    })
}
