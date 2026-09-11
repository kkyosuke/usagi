//! Host-side preparation of AGY customization paths protected by the outer sandbox.

use std::{
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
};

use usagi_daemon::usecase::agy::AgyProvisionFailure;

use super::super::validate_owned_directory;

/// Creates missing customization anchors without replacing user content, then
/// returns canonical paths that the launcher must make read-only. AGY keeps the
/// rest of `antigravity-cli` writable for conversations and authentication.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agy_global_customizations_are_read_only_in_the_shipping_sandbox
pub(super) fn prepare_agy_read_only_paths(
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
    let config = gemini.join("config");
    let state = gemini.join("antigravity-cli");
    let plugins = state.join("plugins");
    for directory in [&gemini, &config, &state, &plugins] {
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

    let settings = state.join("settings.json");
    let import_manifest = state.join("import_manifest.json");
    for path in [&settings, &import_manifest] {
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
            Ok(mut file) => file
                .write_all(b"{}\n")
                .map_err(|_| AgyProvisionFailure::MaterializationFailed)?,
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

    [&config, &plugins, &settings, &import_manifest]
        .into_iter()
        .map(|path| {
            path.canonicalize()
                .map_err(|_| AgyProvisionFailure::MaterializationFailed)
        })
        .collect()
}
