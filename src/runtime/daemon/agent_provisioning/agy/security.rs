//! Host-side preparation of AGY's minimal persistent conversation state.

use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
};

use usagi_daemon::usecase::agy::AgyProvisionFailure;

use super::super::validate_owned_directory;

/// Creates the conversation database anchors AGY needs, without replacing user
/// content, and returns only those canonical paths as writable. The parent
/// `~/.gemini` tree stays outside the grant, so absent and future customization
/// paths are protected without relying on a blacklist.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agy_grants_only_conversation_state_in_the_shipping_sandbox
pub(super) fn prepare_agy_writable_paths(
    home: Option<&Path>,
) -> Result<Vec<PathBuf>, AgyProvisionFailure> {
    let Some(home) = home else {
        return Ok(Vec::new());
    };
    validate_owned_directory(home).map_err(|_| AgyProvisionFailure::MaterializationFailed)?;
    let home = home
        .canonicalize()
        .map_err(|_| AgyProvisionFailure::MaterializationFailed)?;

    let gemini = home.join(".gemini");
    let state = gemini.join("antigravity-cli");
    let conversations = state.join("conversations");
    // Create and validate each ancestor before descending so an existing
    // symlink cannot redirect the first write outside the trusted home.
    for directory in [&gemini, &state, &conversations] {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(AgyProvisionFailure::MaterializationFailed),
        }
        validate_owned_directory(directory)
            .map_err(|_| AgyProvisionFailure::MaterializationFailed)?;
    }

    let summaries = state.join("conversation_summaries.db");
    let summaries_shm = state.join("conversation_summaries.db-shm");
    let summaries_wal = state.join("conversation_summaries.db-wal");
    for path in [&summaries, &summaries_shm, &summaries_wal] {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        match options.open(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(AgyProvisionFailure::MaterializationFailed),
        }
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| AgyProvisionFailure::MaterializationFailed)?;
        if !metadata.file_type().is_file() || path.canonicalize().ok().as_deref() != Some(path) {
            return Err(AgyProvisionFailure::MaterializationFailed);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.uid() != unsafe { libc::geteuid() } {
                return Err(AgyProvisionFailure::MaterializationFailed);
            }
        }
    }

    [&conversations, &summaries, &summaries_shm, &summaries_wal]
        .into_iter()
        .map(|path| {
            path.canonicalize()
                .map_err(|_| AgyProvisionFailure::MaterializationFailed)
        })
        .collect()
}
