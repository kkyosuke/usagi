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
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=session_git_policy_anchors_authority_to_the_registered_worktree
pub(super) fn ensure_owned_subdirectories(
    base: &Path,
    relatives: &[&str],
) -> Result<(), InvalidOwnedDirectory> {
    validate_owned_directory(base)?;
    for relative in relatives {
        let directory = base.join(relative);
        if std::fs::symlink_metadata(&directory)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            std::fs::create_dir(&directory).map_err(|_| InvalidOwnedDirectory)?;
        }
        validate_owned_directory(&directory)?;
    }
    Ok(())
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=claude_sandbox_e2e
pub(super) fn read_git_indirection(
    path: &Path,
    prefix: Option<&str>,
    relative_to: &Path,
) -> Result<PathBuf, ()> {
    let metadata = std::fs::metadata(path).map_err(|_| ())?;
    if metadata.len() > 16 * 1024 {
        return Err(());
    }
    let value = std::fs::read_to_string(path).map_err(|_| ())?;
    let value = value.trim();
    let value = match prefix {
        Some(prefix) => value
            .strip_prefix(prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        None => (!value.is_empty()).then_some(value),
    }
    .ok_or(())?;
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        relative_to.join(path)
    };
    path.canonicalize().map_err(|_| ())
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
