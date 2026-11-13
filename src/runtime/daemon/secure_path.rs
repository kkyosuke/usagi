//! Filesystem identity checks shared by daemon admission and Agent provisioning.

use std::path::{Path, PathBuf};

/// The path is not an absolute, canonical, caller-owned directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InvalidOwnedDirectory;

/// Accepts only an absolute non-root directory owned by this daemon user, with
/// no untrusted symlink component. macOS system firmlinks are OS-managed aliases
/// and therefore remain admissible.
pub(super) fn validate_owned_directory(path: &Path) -> Result<(), InvalidOwnedDirectory> {
    validate_owned_path(path, false)
}

/// Creates missing child directories one component at a time and verifies that
/// the base directory and every child remain caller-owned, non-symlink
/// directories.
pub(super) fn ensure_owned_subdirectories(
    base: &Path,
    relatives: &[&str],
) -> Result<(), InvalidOwnedDirectory> {
    validate_owned_directory(base)?;
    for relative in relatives {
        let mut directory = base.to_path_buf();
        for component in Path::new(relative).components() {
            directory.push(component);
            let _ = std::fs::create_dir(&directory);
            validate_owned_directory(&directory)?;
        }
    }
    Ok(())
}

/// Drops group and other permission bits from existing directories owned by this
/// user. Absent paths, symlinks, and foreign owners are left unchanged so the
/// caller's exact private-chain check still rejects them.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agent_memory_root_tightens_a_chain_left_by_an_earlier_release
pub(super) fn tighten_owned_directories(paths: &[PathBuf]) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    for path in paths {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 != 0
        {
            std::fs::set_permissions(
                path,
                std::fs::Permissions::from_mode(metadata.mode() & 0o700),
            )?;
        }
    }
    Ok(())
}

/// Applies the same identity checks to a directory or, when requested, a
/// regular file used as an exact sandbox bind.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=claude_sandbox_e2e
pub(super) fn validate_owned_path(
    path: &Path,
    allow_file: bool,
) -> Result<(), InvalidOwnedDirectory> {
    if !path.is_absolute() || path == Path::new("/") {
        return Err(InvalidOwnedDirectory);
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| InvalidOwnedDirectory)?;
    if !(metadata.file_type().is_dir() || allow_file && metadata.file_type().is_file())
        || path_has_symlink_component(path)
    {
        return Err(InvalidOwnedDirectory);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(InvalidOwnedDirectory);
        }
    }
    Ok(())
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=claude_sandbox_e2e
fn path_has_symlink_component(path: &Path) -> bool {
    path.ancestors().any(|component| {
        std::fs::symlink_metadata(component).map_or(true, |metadata| {
            metadata.file_type().is_symlink() && !is_macos_system_firmlink(component)
        })
    })
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=root_policy_accepts_the_per_user_cache_root
fn is_macos_system_firmlink(path: &Path) -> bool {
    cfg!(target_os = "macos") && matches!(path.to_str(), Some("/var" | "/tmp" | "/etc"))
}

/// Bootstrap `$TMPDIR` resolution for the outer agent sandbox.
///
/// A sticky system temp directory (`/tmp`, `/var/tmp`, or macOS `/private/tmp`,
/// `/private/var/tmp`) or a directory not owned by the current user is not a
/// private per-user temp directory; ignore it so the launcher falls back to the
/// standard `/tmp` + `/var/tmp` grant instead of failing the policy gate.
pub(super) fn resolve_sandbox_tmpdir(raw: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = PathBuf::from(raw?).canonicalize().ok()?;
    if matches!(
        path.to_str(),
        Some("/tmp" | "/var/tmp" | "/private/tmp" | "/private/var/tmp")
    ) {
        return None;
    }
    validate_owned_directory(&path).ok().map(|()| path)
}

/// Whether the lexical file family beginning at `prefix` intersects `path`'s
/// subtree. Non-UTF-8 paths cannot be represented in the launcher argv and are
/// therefore treated as an overlap (fail closed).
pub(super) fn lexical_prefix_overlaps_path(prefix: &Path, path: &Path) -> bool {
    let Some((prefix_text, path_text)) = prefix.to_str().zip(path.to_str()) else {
        return true;
    };
    path_text.starts_with(prefix_text) || prefix.starts_with(path)
}

pub(super) fn workspace_repository_relatives(workspace_root: &Path) -> Result<Vec<PathBuf>, ()> {
    use super::SystemSessionWorktreeIo;
    use usagi_daemon::usecase::session_runtime::SessionWorktreeIo as _;
    if !workspace_root.is_dir() {
        return Err(());
    }
    if std::fs::symlink_metadata(workspace_root.join(".git")).is_ok() {
        return Ok(vec![PathBuf::new()]);
    }
    let relatives = SystemSessionWorktreeIo
        .workspace_repositories(workspace_root)
        .unwrap_or_default();
    Ok(relatives
        .into_iter()
        .filter(|relative| !relative.as_os_str().is_empty())
        .collect())
}

/// Git authority granted to one linked session worktree.
pub(super) struct SessionGitPolicy {
    pub(super) writable_roots: Vec<PathBuf>,
    pub(super) read_only_roots: Vec<PathBuf>,
}

/// Resolve the narrow Git metadata set a linked session worktree needs for a
/// normal `git add` / `git commit` transaction, keeping its `.git` file and
/// worktree-admin indirection pointers read-only.
pub(super) fn session_git_policy(
    workspace_root: &Path,
    worktree: &Path,
) -> Result<Option<SessionGitPolicy>, ()> {
    if std::fs::symlink_metadata(worktree.join(".git")).is_ok()
        || std::fs::symlink_metadata(workspace_root.join(".git")).is_ok()
    {
        return single_repo_session_git_policy(workspace_root, worktree);
    }
    let relatives = workspace_repository_relatives(workspace_root)?;
    if relatives.is_empty() {
        return single_repo_session_git_policy(workspace_root, worktree);
    }
    let mut writable_roots = Vec::new();
    let mut read_only_roots = Vec::new();
    for relative in relatives {
        if let Some(policy) = single_repo_session_git_policy(
            &workspace_root.join(&relative),
            &worktree.join(&relative),
        )? {
            writable_roots.extend(policy.writable_roots);
            read_only_roots.extend(policy.read_only_roots);
        }
    }
    Ok((!writable_roots.is_empty()).then_some(SessionGitPolicy {
        writable_roots,
        read_only_roots,
    }))
}

fn single_repo_session_git_policy(
    workspace_root: &Path,
    worktree: &Path,
) -> Result<Option<SessionGitPolicy>, ()> {
    use super::agent_provisioning::{read_git_indirection, session_git_common_dir};
    let marker = worktree.join(".git");
    let marker_metadata = match std::fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match std::fs::symlink_metadata(workspace_root.join(".git")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
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
    if backlink != marker.canonicalize().map_err(|_| ())? {
        return Err(());
    }
    let subdirs = ["refs/heads/usagi", "logs/refs/heads/usagi"];
    ensure_owned_subdirectories(&common, &subdirs).map_err(|_| ())?;
    let read_only_roots = vec![backlink, git_dir.join("commondir"), git_dir.join("gitdir")];
    let writable_roots = vec![
        git_dir,
        common.join("objects"),
        common.join("refs/heads/usagi"),
        common.join("logs/refs/heads/usagi"),
    ];
    for root in &writable_roots {
        validate_owned_directory(root).map_err(|_| ())?;
    }
    Ok(Some(SessionGitPolicy {
        writable_roots,
        read_only_roots,
    }))
}
