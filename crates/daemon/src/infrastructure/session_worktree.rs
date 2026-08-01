//! Real Git and filesystem adapters for daemon-owned session worktrees.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use usagi_core::infrastructure::git::{
    GitOutput, GitRunner, add_worktree, confined_git_command, remove_worktree,
};
use usagi_core::infrastructure::paths::STATE_DIR;

use crate::usecase::session_runtime::SessionWorktreeIo;

/// Executes Git commands for the daemon composition root.
pub struct SystemGit;

impl GitRunner for SystemGit {
    /// Every session Git effect — create, the nested worktrees of a mirrored
    /// tree, remove — reaches the binary here, so confining the environment once
    /// at this seam scopes all of them. The daemon inherits the environment of
    /// whoever started it, and an inherited `GIT_DIR` (or any of the rest of the
    /// namespace) outranks the `-C <repo>` this passes.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_runtime_fake_git_contract,git_environment_confinement
    fn run(&self, repo: &Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        let output = confined_git_command(repo).args(args).output()?;
        Ok(GitOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Filesystem boundary used by the daemon composition root.
pub struct SystemSessionWorktreeIo;

impl SessionWorktreeIo for SystemSessionWorktreeIo {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_runtime_fake_fs_contract
    fn remove_file_best_effort(&self, path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_runtime_fake_fs_contract
    fn path_occupied(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_runtime_fake_fs_contract
    fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
        std::fs::canonicalize(path).ok()
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_runtime_fake_fs_contract
    fn is_repo_root(&self, path: &Path) -> bool {
        path.join(".git").exists()
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_runtime_fake_fs_contract
    fn is_linked_worktree(&self, path: &Path) -> bool {
        path.join(".git").is_file()
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_runtime_fake_fs_contract
    fn build_session_tree(
        &self,
        git: &dyn GitRunner,
        workspace_root: &Path,
        destination: &Path,
        branch: &str,
    ) -> anyhow::Result<()> {
        if self.is_repo_root(workspace_root) {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            return add_worktree(git, workspace_root, destination, branch, None);
        }
        std::fs::create_dir_all(destination)?;
        mirror_directory(self, git, workspace_root, destination, branch)
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_runtime_fake_fs_contract
    fn remove_session_tree(
        &self,
        git: &dyn GitRunner,
        session_root: &Path,
        force: bool,
    ) -> anyhow::Result<()> {
        let mut worktrees = Vec::new();
        collect_session_worktrees(self, session_root, &mut worktrees)?;
        worktrees.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for worktree in worktrees {
            remove_worktree(git, &worktree, &worktree, force)?;
        }
        match std::fs::remove_dir_all(session_root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_runtime_fake_fs_contract
fn mirror_directory(
    io: &SystemSessionWorktreeIo,
    git: &dyn GitRunner,
    source: &Path,
    destination: &Path,
    branch: &str,
) -> anyhow::Result<()> {
    let mut entries = std::fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        if skipped_entry(&name) {
            continue;
        }
        let source = entry.path();
        let target = destination.join(&name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if io.is_linked_worktree(&source) {
                continue;
            }
            if io.is_repo_root(&source) {
                add_worktree(git, &source, &target, branch, None)?;
            } else {
                std::fs::create_dir_all(&target)?;
                mirror_directory(io, git, &source, &target, branch)?;
            }
        } else {
            std::fs::copy(source, target)?;
        }
    }
    Ok(())
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_runtime_fake_fs_contract
fn collect_session_worktrees(
    io: &SystemSessionWorktreeIo,
    directory: &Path,
    worktrees: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    if io.is_linked_worktree(directory) {
        worktrees.push(directory.into());
        return Ok(());
    }
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            collect_session_worktrees(io, &entry.path(), worktrees)?;
        }
    }
    Ok(())
}

fn skipped_entry(name: &OsStr) -> bool {
    name == OsStr::new(".git") || name == OsStr::new(STATE_DIR)
}
