//! Filesystem identity checks shared by daemon admission and Agent provisioning.

use std::path::Path;

/// The path is not an absolute, canonical, caller-owned directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InvalidOwnedDirectory;

/// Accepts only an absolute non-root directory owned by this daemon user, with
/// no untrusted symlink component. macOS system firmlinks are OS-managed aliases
/// and therefore remain admissible.
pub(super) fn validate_owned_directory(path: &Path) -> Result<(), InvalidOwnedDirectory> {
    validate_owned_path(path, false)
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
