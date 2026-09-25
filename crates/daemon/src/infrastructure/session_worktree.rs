//! Real Git and filesystem adapters for daemon-owned session worktrees.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use usagi_core::infrastructure::git::{
    GitOutput, GitRunner, add_worktree, confined_git_command, delete_branch, remove_worktree,
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

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=orphan_session_entries_are_sorted_and_direct
    fn session_entries(&self, container: &Path) -> anyhow::Result<Vec<String>> {
        let mut names = match std::fs::read_dir(container) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        names.sort();
        Ok(names)
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_runtime_fake_fs_contract
    fn build_session_tree(
        &self,
        git: &dyn GitRunner,
        workspace_root: &Path,
        destination: &Path,
        branch: &str,
        base_ref: Option<&str>,
    ) -> anyhow::Result<()> {
        if self.is_repo_root(workspace_root) {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            return add_worktree(git, workspace_root, destination, branch, base_ref);
        }
        std::fs::create_dir_all(destination)?;
        let mut created = Vec::new();
        let result = mirror_directory(
            self,
            git,
            workspace_root,
            destination,
            branch,
            base_ref,
            &mut created,
        );
        if let Err(error) = result {
            let mut cleanup = Vec::new();
            for (repository, worktree) in created.into_iter().rev() {
                if let Err(error) = remove_worktree(git, &repository, &worktree, true) {
                    cleanup.push(error.to_string());
                }
                if let Err(error) = delete_branch(git, &repository, branch, true) {
                    cleanup.push(error.to_string());
                }
            }
            if let Err(remove_error) = remove_tree(destination)
                && remove_error.kind() != std::io::ErrorKind::NotFound
            {
                cleanup.push(remove_error.to_string());
            }
            if cleanup.is_empty() {
                return Err(error);
            }
            return Err(anyhow::anyhow!(
                "{error}; compensation failed: {}",
                cleanup.join("; ")
            ));
        }
        Ok(())
    }

    fn run_setup_command(&self, session_root: &Path, command: &str) -> anyhow::Result<()> {
        let status = Command::new("/bin/sh")
            .args(["-lc", command])
            .current_dir(session_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("setup command exited with {status}"))
        }
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
        match remove_tree(session_root) {
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
    base_ref: Option<&str>,
    created: &mut Vec<(PathBuf, PathBuf)>,
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
                add_worktree(git, &source, &target, branch, base_ref)?;
                created.push((source, target));
            } else {
                std::fs::create_dir_all(&target)?;
                mirror_directory(io, git, &source, &target, branch, base_ref, created)?;
            }
        } else {
            std::fs::copy(source, target)?;
        }
    }
    Ok(())
}

/// Remove `root` and everything under it.
///
/// A directory without the owner-write bit refuses the unlink of its own
/// children, so `remove_dir_all` alone fails with `PermissionDenied` and the
/// session can never be torn down. A session tree picks such a directory up from
/// whatever ran inside it, which the removal has to cope with rather than
/// diagnose.
///
/// The repair therefore runs on that one error, and the removal is retried only
/// when a mode actually changed: a `PermissionDenied` that no mode in the tree
/// explains — the container above it is unwritable, say — keeps its original
/// error rather than paying for a second traversal. That signal is a lower
/// bound, not a proof. Widening a deep directory while a shallow one refuses the
/// chmod still reports a change, so that attempt spends one more removal; the
/// next one sees the deep directory already open and skips the retry. The walk
/// itself is bounded by the tree being deleted, and widening permissions there
/// costs nothing, since the tree is on its way out.
fn remove_tree(root: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(root) {
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            if grant_owner_access(root) {
                std::fs::remove_dir_all(root)
            } else {
                Err(error)
            }
        }
        result => result,
    }
}

/// Give the owner read, write and traverse permission on `root` and on every
/// directory beneath it, reporting whether any mode actually changed.
///
/// The chmod precedes the descent, so a directory that denied its own listing is
/// readable by the time the walk reaches its children.
///
/// Only directories matter, because a read-only *file* is unlinked through its
/// parent, so anything else ends the walk. Reading the metadata without
/// following links is what keeps a symlink out: it is never reported as a
/// directory, so the walk stops at the link rather than reaching through it into
/// a tree that is not being deleted. Every effect is best-effort — the retry in
/// [`remove_tree`] is what reports whether the removal actually became possible.
fn grant_owner_access(root: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(root) else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    let mut granted = false;
    let mode = metadata.permissions().mode();
    if mode & 0o700 != 0o700 {
        let mut permissions = metadata.permissions();
        permissions.set_mode(mode | 0o700);
        granted = std::fs::set_permissions(root, permissions).is_ok();
    }
    // Flattened rather than matched: a directory that cannot be listed
    // contributes nothing, exactly as an empty one does, and giving that case a
    // branch of its own would leave a line no test can reach — the chmod above
    // is what would have to fail for the listing to, and the owner's own chmod
    // does not.
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        granted |= grant_owner_access(&entry.path());
    }
    granted
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Leaves a narrowed directory usable again when the test ends, so an
    /// assertion that fails partway cannot strand a temporary tree that the
    /// runner is then unable to remove.
    struct UnlockOnDrop(PathBuf);

    impl Drop for UnlockOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn a_write_denying_directory_no_longer_strands_the_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("session");
        std::fs::create_dir_all(root.join("locked").join("nested")).unwrap();
        std::fs::write(root.join("locked").join("nested").join("deep.txt"), b"x").unwrap();
        std::fs::write(root.join("locked").join("held.txt"), b"x").unwrap();
        // Deepest first: a parent that already denies writes would refuse the
        // chmod of its own children.
        let _unlock = [
            UnlockOnDrop(root.join("locked")),
            UnlockOnDrop(root.join("locked").join("nested")),
        ];
        std::fs::set_permissions(
            root.join("locked").join("nested"),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o500))
            .unwrap();

        // This is the behaviour that stranded sessions: a directory without the
        // owner-write bit refuses the unlink of its own children, so the bare
        // removal fails and the teardown worker retries it forever. (Root would
        // not be refused, but the gates run as an unprivileged user.)
        assert_eq!(
            std::fs::remove_dir_all(&root).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );

        remove_tree(&root).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn the_permission_walk_stops_at_anything_that_is_not_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let _unlock = UnlockOnDrop(outside.clone());
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o500)).unwrap();

        // A symlink to a directory is not itself a directory when its metadata is
        // read without following it, so the walk stops at the link instead of
        // rewriting a tree that is not being deleted.
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(!grant_owner_access(&link));
        assert_eq!(mode(&outside), 0o500);

        // A read-only file is unlinked through its parent, and a path that is not
        // there at all has nothing to grant.
        let file = tmp.path().join("file.txt");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(!grant_owner_access(&file));
        assert!(!grant_owner_access(&tmp.path().join("absent")));
        assert_eq!(mode(&file), 0o400);

        // A directory the owner can already use fully has nothing to grant, so a
        // `PermissionDenied` the walk cannot explain keeps its original error
        // rather than retrying the removal.
        let open = tmp.path().join("open");
        std::fs::create_dir_all(open.join("child")).unwrap();
        // Set explicitly rather than trusting the runner's umask to leave every
        // owner bit on.
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!grant_owner_access(&open));
    }

    #[test]
    fn removal_reports_every_other_outcome_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(plain.join("child")).unwrap();
        remove_tree(&plain).unwrap();
        assert!(!plain.exists());

        assert_eq!(
            remove_tree(&tmp.path().join("missing")).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );

        // A `PermissionDenied` the repair cannot explain keeps its original
        // error: here the tree itself is fully usable and the refusal comes from
        // the container above it, which is not the teardown's to widen.
        let container = tmp.path().join("locked-container");
        let inside = container.join("session");
        std::fs::create_dir_all(inside.join("child")).unwrap();
        let _unlock = UnlockOnDrop(container.clone());
        std::fs::set_permissions(&container, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(
            remove_tree(&inside).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(mode(&container), 0o500);
    }

    #[test]
    fn orphan_session_entries_are_sorted_and_direct() {
        let tmp = tempfile::tempdir().unwrap();
        let container = tmp.path().join("sessions");
        std::fs::create_dir_all(container.join("zeta").join("nested")).unwrap();
        std::fs::create_dir_all(container.join("alpha")).unwrap();
        std::fs::write(container.join("marker"), b"not a worktree").unwrap();

        assert_eq!(
            SystemSessionWorktreeIo.session_entries(&container).unwrap(),
            ["alpha", "marker", "zeta"]
        );
        assert!(
            SystemSessionWorktreeIo
                .session_entries(&tmp.path().join("missing"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn setup_commands_run_in_the_session_root_and_report_failure() {
        let session = tempfile::tempdir().unwrap();
        SystemSessionWorktreeIo
            .run_setup_command(session.path(), "printf ready > setup-marker")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(session.path().join("setup-marker")).unwrap(),
            "ready"
        );
        assert!(
            SystemSessionWorktreeIo
                .run_setup_command(session.path(), "exit 7")
                .unwrap_err()
                .to_string()
                .contains("exit status: 7")
        );
    }
}
