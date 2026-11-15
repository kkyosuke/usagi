//! Workspace-side alias that keeps the launch directory first in AGY's workspace order.

use std::{fmt::Write as _, path::PathBuf};

use super::super::validate_owned_directory;

/// AGY lists its workspace directories sorted by path and uses the first one
/// as the `Command Working Directory` for the model and the default cwd of
/// `run_command`, regardless of the process cwd or the `--add-dir` order.
/// The integration is therefore added through an alias that must sort after
/// the launch's working directory; otherwise the read-only integration would
/// become the directory every command runs in.
pub(in crate::runtime::daemon) fn agy_arguments_for_integration(
    working_directory: &std::path::Path,
    alias: &std::path::Path,
    integration: &std::path::Path,
) -> Result<(Vec<String>, Option<PathBuf>), ()> {
    if alias.as_os_str().as_encoded_bytes() <= working_directory.as_os_str().as_encoded_bytes() {
        return Err(());
    }
    let alias = alias.to_str().ok_or(())?;
    // The sandbox carves the integration out by this path, which must be UTF-8.
    integration.to_str().ok_or(())?;
    Ok((
        vec!["--add-dir".to_owned(), alias.to_owned()],
        Some(integration.to_path_buf()),
    ))
}

/// File name of the alias, under the workspace's `.usagi/`, that points at one
/// integration root. `workspace-` sorts after `sessions/`, so every session
/// worktree and the workspace root itself precede the alias in AGY's order.
/// The digest separates the MCP and prompt-only variants and the integrations
/// of different data homes (runtime modes) that serve the same workspace.
#[must_use]
pub(in crate::runtime::daemon) fn agy_workspace_alias_name(
    integration: &std::path::Path,
) -> String {
    use sha2::{Digest as _, Sha256};

    let digest = Sha256::digest(integration.as_os_str().as_encoded_bytes());
    let mut name = "workspace-agy-".to_owned();
    for byte in &digest[..8] {
        write!(name, "{byte:02x}").expect("writing to a String cannot fail");
    }
    name
}

/// Points `<workspace>/.usagi/<alias>` at the canonical integration root.
/// `.usagi/` is ignored by Git and outside every Agent's writable roots (the
/// sandbox protects the workspace root except the session's own worktree), so
/// the alias can be read through but not replaced by the Agent. An existing
/// entry that is not a symlink is never replaced, and a stale symlink is
/// swapped atomically so concurrent launches always see a complete alias.
/// The workspace root must be absolute.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agy_workspace_alias_keeps_the_worktree_as_the_command_directory
pub(in crate::runtime::daemon) fn publish_agy_workspace_alias(
    workspace_root: &std::path::Path,
    integration: &std::path::Path,
) -> Result<PathBuf, ()> {
    if !workspace_root.is_absolute() {
        return Err(());
    }
    let metadata = workspace_root
        .canonicalize()
        .map_err(|_| ())?
        .join(".usagi");
    // AGY compares the paths as spelled, and the working directory is
    // spelled from the same workspace root, so the returned alias keeps that
    // spelling (for example `/var` rather than `/private/var` on macOS).
    let spelled = workspace_root.join(".usagi");
    match std::fs::create_dir(&metadata) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(()),
    }
    validate_owned_directory(&metadata).map_err(|_| ())?;
    let name = agy_workspace_alias_name(integration);
    let alias = metadata.join(&name);
    match std::fs::read_link(&alias) {
        Ok(target) if target == integration => return Ok(spelled.join(&name)),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(()),
    }
    let staging = metadata.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        usagi_core::domain::id::OperationId::new()
    ));
    std::os::unix::fs::symlink(integration, &staging).map_err(|_| ())?;
    if std::fs::rename(&staging, &alias).is_err() {
        let _ = std::fs::remove_file(&staging);
        return Err(());
    }
    Ok(spelled.join(&name))
}
