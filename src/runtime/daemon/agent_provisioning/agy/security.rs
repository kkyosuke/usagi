//! Host-side preparation of AGY's minimal persistent conversation state.

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

use usagi_daemon::usecase::agy::AgyProvisionFailure;

use super::super::validate_owned_directory;

/// Mode of the `run_command` helper AGY regenerates as a shell script and
/// executes from `PATH`.
const AGENTAPI_MODE: u32 = 0o700;
/// Mode of the SQLite files AGY only reads and writes.
const STATE_FILE_MODE: u32 = 0o600;

/// Creates the conversation, subagent artifact, conversation lock, crash
/// report, and `run_command` helper anchors AGY needs, without replacing user
/// content, and returns only those canonical paths as writable. The parent
/// `~/.gemini` tree and `bin/` itself stay outside the grant, so absent and
/// future customization paths are protected without relying on a blacklist.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agy_writable_paths_keep_the_agentapi_helper_executable
pub(in crate::runtime::daemon) fn prepare_agy_writable_paths(
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
    let brain = state.join("brain");
    let presence = state.join("presence");
    let crashes = state.join("crashes");
    let bin = state.join("bin");
    // Create and validate each ancestor before descending so an existing
    // symlink cannot redirect the first write outside the trusted home.
    for directory in [
        &gemini,
        &state,
        &conversations,
        &brain,
        &presence,
        &crashes,
        &bin,
    ] {
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
    let agentapi = bin.join("agentapi");
    for (path, mode) in [
        (&summaries, STATE_FILE_MODE),
        (&summaries_shm, STATE_FILE_MODE),
        (&summaries_wal, STATE_FILE_MODE),
        (&agentapi, AGENTAPI_MODE),
    ] {
        let file = open_owned_state_file(path, mode)?;
        // AGY rewrites the helper in place with `os.WriteFile(…, 0755)`, which
        // never changes the mode of an existing file. An anchor created without
        // the owner execute bit would therefore stay unexecutable and every
        // `run_command` would fail with `Permission denied`.
        #[cfg(unix)]
        if mode == AGENTAPI_MODE {
            use std::os::unix::fs::PermissionsExt as _;
            let current = file
                .metadata()
                .map_err(|_| AgyProvisionFailure::MaterializationFailed)?
                .permissions()
                .mode();
            if let Some(repaired) = agentapi_repair_mode(current) {
                file.set_permissions(std::fs::Permissions::from_mode(repaired))
                    .map_err(|_| AgyProvisionFailure::MaterializationFailed)?;
            }
        }
    }

    [
        &conversations,
        &brain,
        &presence,
        &crashes,
        &summaries,
        &summaries_shm,
        &summaries_wal,
        &agentapi,
    ]
    .into_iter()
    .map(|path| {
        path.canonicalize()
            .map_err(|_| AgyProvisionFailure::MaterializationFailed)
    })
    .collect()
}

/// Creates `path` with `mode` unless it already exists, then returns a handle
/// to it after checking that it is this user's regular file at its canonical
/// location. The handle is opened without following symlinks, so the checks
/// and any later permission repair apply to the same inode.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agy_writable_paths_keep_the_agentapi_helper_executable
fn open_owned_state_file(path: &Path, mode: u32) -> Result<File, AgyProvisionFailure> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(mode)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    match options.open(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(AgyProvisionFailure::MaterializationFailed),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| AgyProvisionFailure::MaterializationFailed)?;
    let metadata = file
        .metadata()
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
    Ok(file)
}

/// Returns the mode that restores the owner read, write, and execute bits of
/// an existing `run_command` helper, or `None` when it already has them.
/// Group and other bits are kept as AGY left them; set-id and sticky bits are
/// dropped so a repair never grants more than an executable script needs.
pub(in crate::runtime::daemon) fn agentapi_repair_mode(current: u32) -> Option<u32> {
    (current & AGENTAPI_MODE != AGENTAPI_MODE).then_some((current & 0o777) | AGENTAPI_MODE)
}
