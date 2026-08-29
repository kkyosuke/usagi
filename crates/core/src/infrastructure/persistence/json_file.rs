//! Shared helpers for the files usagi persists under its data directories.
//!
//! Every store treats a missing file as "no data yet" and writes through a temp
//! file + rename so a crash never leaves a half-written file behind. The temp
//! file's contents are flushed with `sync_all` (fsync) before the rename, so its
//! data survives a power loss / hard crash rather than only a process crash;
//! after the rename the parent directory is fsynced best-effort so the rename
//! itself is durable where the platform supports it (directory fsync is a no-op
//! or errors on some platforms such as Windows and is intentionally ignored).
//! JSON stores (`storage`) use [`read`] / [`write_atomic`]; the markdown stores
//! (`issue_store`, `memory_store`) use [`write_text_atomic`] for their
//! hand-rolled text. All share one per-writer-unique temp-name scheme so two
//! processes writing the same path never clobber each other.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

const MAX_TEMP_CREATE_ATTEMPTS: usize = 128;

#[cfg(test)]
thread_local! {
    static FORCED_TMP_PATHS: std::cell::RefCell<std::collections::VecDeque<PathBuf>> =
        const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    static TEMP_AFTER_CREATE: std::cell::Cell<Option<TempAfterCreate>> = const {
        std::cell::Cell::new(None)
    };
    static CLEANUP_BEFORE_REMOVE: std::cell::Cell<Option<CleanupBeforeRemove>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum TempAfterCreate {
    AddHardlink,
    ReplaceWithSymlink,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum CleanupBeforeRemove {
    Remove,
    ReplaceWithDirectory,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AtomicWriteStage {
    Write,
    Rename,
}

#[cfg(test)]
struct AtomicWriteFailpoint {
    path: PathBuf,
    stage: AtomicWriteStage,
}

#[cfg(test)]
thread_local! {
    static ATOMIC_WRITE_FAILPOINT: std::cell::RefCell<Option<AtomicWriteFailpoint>> = const {
        std::cell::RefCell::new(None)
    };
}

/// Fail the next matching atomic write in this test thread.
#[cfg(test)]
pub(crate) fn fail_next_atomic_write(path: &Path, stage: AtomicWriteStage) {
    ATOMIC_WRITE_FAILPOINT.with(|failpoint| {
        *failpoint.borrow_mut() = Some(AtomicWriteFailpoint {
            path: path.to_path_buf(),
            stage,
        });
    });
}

#[cfg(test)]
fn take_atomic_write_failpoint(path: &Path, stage: AtomicWriteStage) -> bool {
    ATOMIC_WRITE_FAILPOINT.with(|failpoint| {
        let matches = failpoint
            .borrow()
            .as_ref()
            .is_some_and(|failpoint| failpoint.path == path && failpoint.stage == stage);
        if matches {
            failpoint.borrow_mut().take();
        }
        matches
    })
}

#[cfg(test)]
fn force_tmp_paths(paths: impl IntoIterator<Item = PathBuf>) {
    FORCED_TMP_PATHS.with(|forced| forced.borrow_mut().extend(paths));
}

#[cfg(test)]
fn tamper_next_temp_after_create(action: TempAfterCreate) {
    TEMP_AFTER_CREATE.with(|pending| pending.set(Some(action)));
}

#[cfg(all(test, unix))]
fn tamper_temp_after_create(temp: &OwnedTemp) -> std::io::Result<()> {
    TEMP_AFTER_CREATE.with(|pending| match pending.take() {
        Some(TempAfterCreate::AddHardlink) => {
            fs::hard_link(&temp.path, temp.path.with_extension("alias"))
        }
        action @ Some(TempAfterCreate::ReplaceWithSymlink) => {
            pending.set(action);
            Ok(())
        }
        None => Ok(()),
    })
}

#[cfg(all(test, unix))]
fn tamper_temp_before_path_verify(temp: &OwnedTemp) -> std::io::Result<()> {
    use std::os::unix::fs::symlink;

    TEMP_AFTER_CREATE.with(|pending| match pending.take() {
        Some(TempAfterCreate::ReplaceWithSymlink) => {
            fs::remove_file(&temp.path)?;
            symlink(temp.path.with_extension("sentinel"), &temp.path)
        }
        Some(TempAfterCreate::AddHardlink) | None => Ok(()),
    })
}

/// A cryptographically random per-write temp path in the target directory.
fn unique_tmp_path(path: &Path) -> PathBuf {
    #[cfg(test)]
    if let Some(forced) = FORCED_TMP_PATHS.with(|paths| paths.borrow_mut().pop_front()) {
        return forced;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    PathBuf::from(tmp)
}

struct OwnedTemp {
    path: PathBuf,
    file: fs::File,
    identity: TempIdentity,
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct TempIdentity {
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
#[derive(Clone, Copy)]
struct TempIdentity;

fn create_owned_temp(path: &Path) -> Result<OwnedTemp> {
    for _ in 0..MAX_TEMP_CREATE_ATTEMPTS {
        let candidate = unique_tmp_path(path);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        configure_secure_temp_options(&mut options);
        match options.open(&candidate) {
            Ok(file) => {
                let identity = identify_open_temp(&file)
                    .context(format!("unsafe atomic temp {}", candidate.display()))?;
                let temp = OwnedTemp {
                    path: candidate,
                    file,
                    identity,
                };
                #[cfg(all(test, unix))]
                tamper_temp_after_create(&temp)?;
                let secured = make_temp_private(&temp.file)
                    .context(format!("failed to secure {}", temp.path.display()))
                    .and_then(|()| {
                        verify_open_temp(&temp.file)
                            .context(format!("unsafe atomic temp {}", temp.path.display()))
                    })
                    .and_then(|()| {
                        #[cfg(all(test, unix))]
                        tamper_temp_before_path_verify(&temp)?;
                        verify_temp_path(&temp.path, &temp.file, temp.identity)
                            .context(format!("unsafe atomic temp path {}", temp.path.display()))
                    });
                return match secured {
                    Ok(()) => Ok(temp),
                    Err(error) => cleanup_error(error, cleanup_owned_temp(&temp)),
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).context(format!("failed to create {}", candidate.display()));
            }
        }
    }
    anyhow::bail!(
        "failed to create a collision-free atomic temp for {} after {MAX_TEMP_CREATE_ATTEMPTS} attempts",
        path.display()
    )
}

#[cfg(unix)]
fn configure_secure_temp_options(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn configure_secure_temp_options(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn make_temp_private(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn make_temp_private(_file: &fs::File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn identify_open_temp(file: &fs::File) -> std::io::Result<TempIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "atomic temp is not an owner regular file",
        ));
    }
    Ok(TempIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn identify_open_temp(file: &fs::File) -> std::io::Result<TempIdentity> {
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "atomic temp is not a regular file",
        ));
    }
    Ok(TempIdentity)
}

#[cfg(unix)]
fn verify_open_temp(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "atomic temp is not a private singly-linked owner file",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_open_temp(file: &fs::File) -> std::io::Result<()> {
    identify_open_temp(file).map(|_| ())
}

fn verify_temp_path(path: &Path, file: &fs::File, identity: TempIdentity) -> std::io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "atomic temp path is not a regular file",
        ));
    }
    verify_path_identity(&path_metadata, file, identity)
}

#[cfg(unix)]
fn verify_path_identity(
    path_metadata: &fs::Metadata,
    file: &fs::File,
    identity: TempIdentity,
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let opened = file.metadata()?;
    if path_metadata.dev() != identity.device
        || path_metadata.ino() != identity.inode
        || opened.dev() != identity.device
        || opened.ino() != identity.inode
        || path_metadata.uid() != unsafe { libc::geteuid() }
        || path_metadata.nlink() != 1
        || opened.nlink() != 1
        || path_metadata.mode() & 0o777 != 0o600
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "atomic temp path no longer names the owned inode",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_path_identity(
    _path_metadata: &fs::Metadata,
    file: &fs::File,
    _identity: TempIdentity,
) -> std::io::Result<()> {
    if file.metadata()?.is_file() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "atomic temp is no longer a regular file",
        ))
    }
}

fn cleanup_owned_temp(temp: &OwnedTemp) -> Result<()> {
    match verify_temp_path_identity(&temp.path, &temp.file, temp.identity) {
        Ok(()) => {
            #[cfg(test)]
            tamper_before_cleanup_remove(&temp.path)?;
            match fs::remove_file(&temp.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error).context(format!(
                    "failed to clean owned atomic temp {}",
                    temp.path.display()
                )),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context(format!(
            "refusing to clean replaced atomic temp {}",
            temp.path.display()
        )),
    }
}

#[cfg(test)]
fn tamper_next_cleanup_before_remove(action: CleanupBeforeRemove) {
    CLEANUP_BEFORE_REMOVE.with(|pending| pending.set(Some(action)));
}

#[cfg(test)]
fn tamper_before_cleanup_remove(path: &Path) -> std::io::Result<()> {
    CLEANUP_BEFORE_REMOVE.with(|pending| match pending.take() {
        Some(CleanupBeforeRemove::Remove) => fs::remove_file(path),
        Some(CleanupBeforeRemove::ReplaceWithDirectory) => {
            fs::remove_file(path)?;
            fs::create_dir(path)
        }
        None => Ok(()),
    })
}

fn cleanup_error<T>(error: anyhow::Error, cleanup: Result<()>) -> Result<T> {
    match cleanup {
        Ok(()) => Err(error),
        Err(cleanup) => Err(error.context(format!("atomic temp cleanup failed: {cleanup:#}"))),
    }
}

fn verify_temp_path_identity(
    path: &Path,
    file: &fs::File,
    identity: TempIdentity,
) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "atomic temp path no longer names a regular file",
        ));
    }
    verify_basic_path_identity(&metadata, file, identity)
}

#[cfg(unix)]
fn verify_basic_path_identity(
    path_metadata: &fs::Metadata,
    file: &fs::File,
    identity: TempIdentity,
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let opened = file.metadata()?;
    if path_metadata.dev() == identity.device
        && path_metadata.ino() == identity.inode
        && opened.dev() == identity.device
        && opened.ino() == identity.inode
    {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "atomic temp path no longer names the owned inode",
        ))
    }
}

#[cfg(not(unix))]
fn verify_basic_path_identity(
    _path_metadata: &fs::Metadata,
    file: &fs::File,
    _identity: TempIdentity,
) -> std::io::Result<()> {
    verify_open_temp(file)
}

/// Write `bytes` to a unique temp file and rename it onto `path`. When `durable`,
/// the temp file's contents are fsynced before the rename and the parent
/// directory is fsynced after it, so the write survives a power loss; otherwise
/// only the atomic rename is guaranteed (a crash never exposes a half-written
/// file, but the latest bytes may not have reached disk yet).
///
/// The non-durable mode is for rebuildable derived *caches* (`index.json`): they
/// are never relied on for correctness — a stale or missing cache self-heals from
/// the source-of-truth markdown on the next read — so paying an fsync on every
/// write to make them power-loss durable is wasted IO in the store lock's hot path.
/// Source-of-truth files (JSON state, memory/issue markdown) stay durable.
fn write_atomically(path: &Path, bytes: &[u8], durable: bool) -> Result<()> {
    let mut temp = create_owned_temp(path)?;
    // Clean up the temp file on any failure after it is created. The write
    // (write_all / sync_all) or the rename can fail — rename especially
    // (EXDEV/cross-device, ENOSPC, EACCES) — and without this each failed write
    // leaves an orphaned `*.tmp.<pid>.<random>` behind, so a recurring failure litters
    // the data dir without bound. The rename is still atomic, so a failed write
    // never replaces the existing good file; this only removes the dead temp.
    let result = write_atomically_inner(&mut temp, path, bytes, durable);
    if let Err(error) = result {
        return cleanup_error(error, cleanup_owned_temp(&temp));
    }
    Ok(())
}

/// The body of [`write_atomically`]: create-write(-sync) the temp file, rename it
/// onto `path`, then (when durable) fsync the parent. Split out so the caller can
/// remove the temp file on any error this returns.
fn write_atomically_inner(
    temp: &mut OwnedTemp,
    path: &Path,
    bytes: &[u8],
    durable: bool,
) -> Result<()> {
    #[cfg(test)]
    if take_atomic_write_failpoint(path, AtomicWriteStage::Write) {
        anyhow::bail!("injected atomic write failure for {}", path.display());
    }
    verify_temp_path(&temp.path, &temp.file, temp.identity)
        .context(format!("unsafe atomic temp path {}", temp.path.display()))?;
    temp.file
        .write_all(bytes)
        .context(format!("failed to write {}", temp.path.display()))?;
    if durable {
        temp.file
            .sync_all()
            .context(format!("failed to flush {}", temp.path.display()))?;
    }
    verify_temp_path(&temp.path, &temp.file, temp.identity)
        .context(format!("unsafe atomic temp path {}", temp.path.display()))?;
    #[cfg(test)]
    if take_atomic_write_failpoint(path, AtomicWriteStage::Rename) {
        anyhow::bail!("injected atomic rename failure for {}", path.display());
    }
    fs::rename(&temp.path, path).context(format!("failed to replace {}", path.display()))?;
    if durable {
        fsync_parent_dir(path);
    }
    Ok(())
}

/// Best-effort fsync of `path`'s parent directory so a preceding rename's
/// directory entry is durable.
///
/// Directory fsync is what makes a rename survive power loss, but it is a no-op
/// or returns an error on some platforms (e.g. Windows) and may fail if the
/// parent cannot be opened. Such failures must not fail an otherwise-successful
/// write, so every error here is intentionally swallowed.
fn fsync_parent_dir(path: &Path) {
    let Some(parent) = path.parent() else { return };
    let Ok(dir) = fs::File::open(parent) else {
        return;
    };
    let _ = dir.sync_all();
}

/// Read and deserialize the JSON file at `path`, returning `None` if it does
/// not exist.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read, or when its
/// contents are not valid JSON for `T`.
pub fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context(format!("failed to read {}", path.display())),
    };
    let value =
        serde_json::from_str(&text).context(format!("failed to parse {}", path.display()))?;
    Ok(Some(value))
}

/// Serialize `value` to pretty JSON and write it durably and atomically to `path`
/// (temp file + fsync + rename), creating `dir` (the directory that contains
/// `path`) first. For source-of-truth JSON; a rebuildable cache uses
/// [`write_atomic_cache`].
///
/// # Errors
///
/// Returns an error when `dir` cannot be created, `value` cannot be serialized,
/// or the temp file cannot be written or renamed onto `path`.
pub fn write_atomic<T: Serialize>(dir: &Path, path: &Path, value: &T) -> Result<()> {
    write_json(dir, path, value, true)
}

/// Like [`write_atomic`] but for a rebuildable derived cache: the write is atomic
/// (temp file + rename) but not fsynced, so it does not pay the durability cost of
/// a source-of-truth file. A power loss may lose the latest cache bytes; the cache
/// self-heals from the markdown source of truth on the next read.
///
/// # Errors
///
/// Returns an error when `dir` cannot be created, `value` cannot be serialized,
/// or the temp file cannot be written or renamed onto `path`.
pub fn write_atomic_cache<T: Serialize>(dir: &Path, path: &Path, value: &T) -> Result<()> {
    write_json(dir, path, value, false)
}

fn write_json<T: Serialize>(dir: &Path, path: &Path, value: &T, durable: bool) -> Result<()> {
    fs::create_dir_all(dir).context(format!("failed to create {}", dir.display()))?;
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write_atomically(path, text.as_bytes(), durable)
}

/// Write `text` to `path` durably and atomically (per-writer-unique temp file +
/// fsync + rename) so a crash never leaves a half-written file behind and two
/// processes writing the same path never clobber each other's temp. Unlike
/// [`write_atomic`], `text` is written verbatim and the parent directory is
/// assumed to exist already — the markdown stores create it when they set up their
/// data dir.
///
/// # Errors
///
/// Returns an error when the temp file cannot be written or renamed onto `path`.
pub fn write_text_atomic(path: &Path, text: &str) -> Result<()> {
    write_atomically(path, text.as_bytes(), true)
}

/// The on-disk format version stamped onto every versioned store file
/// (`storage`'s `workspaces.json` and the issue/memory `index.json` derived
/// caches). Bumped only on an incompatible on-disk format change; the single
/// source of truth for the envelope's `version` field so no store carries its
/// own copy.
pub const FILE_FORMAT_VERSION: u32 = 1;

/// The on-disk envelope shared by every versioned store file: a `version` plus
/// the flattened payload (`{ "version": N, <payload…> }`). The write side
/// borrows the payload (so callers never clone it into an owned wrapper); the
/// read side exposes the `version` to the strict reader while the compatibility
/// reader continues to ignore it.
#[derive(Serialize)]
struct VersionedRef<'a, T: ?Sized> {
    version: u32,
    #[serde(flatten)]
    inner: &'a T,
}

#[derive(Deserialize)]
struct Versioned<T> {
    #[serde(default)]
    version: u32,
    #[serde(flatten)]
    inner: T,
}

/// Read the payload from a versioned JSON file — the `{ "version": N, <payload…> }`
/// envelope the stores write — returning `None` when the file does not exist.
/// The envelope's `version` is accepted and ignored.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
pub fn read_versioned<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    Ok(read::<Versioned<T>>(path)?.map(|v| v.inner))
}

/// Read a versioned payload only when its envelope version is supported by this
/// build. This is for durable user-authored state that must not be interpreted
/// and later overwritten after a newer usagi has changed its schema.
///
/// Files without a version keep the legacy value `0` and remain readable.
///
/// # Errors
///
/// Returns an error when the file cannot be read or parsed, or when its version
/// is newer than [`FILE_FORMAT_VERSION`].
pub fn read_supported_version<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let Some(versioned) = read::<Versioned<T>>(path)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        versioned.version <= FILE_FORMAT_VERSION,
        "unsupported file format version {} in {}",
        versioned.version,
        path.display()
    );
    Ok(Some(versioned.inner))
}

/// Serialize `payload` and write it atomically to `path` as a versioned JSON
/// file, stamping the current [`FILE_FORMAT_VERSION`]. The payload is serialized
/// by reference, so the caller never clones it into an owned envelope struct.
///
/// # Errors
///
/// Returns an error when `dir` cannot be created, `payload` cannot be serialized,
/// or the temp file cannot be written or renamed onto `path`.
pub fn write_versioned<T: Serialize + ?Sized>(dir: &Path, path: &Path, payload: &T) -> Result<()> {
    write_atomic(
        dir,
        path,
        &VersionedRef {
            version: FILE_FORMAT_VERSION,
            inner: payload,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_text_atomic_writes_and_replaces_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        write_text_atomic(&path, "hello\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");

        // A second write replaces the file in place and leaves no temp behind.
        write_text_atomic(&path, "world\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "world\n");
        let leftover: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp files left behind: {leftover:?}");
    }

    #[test]
    fn write_atomic_round_trips_json_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let value = vec!["a".to_string(), "b".to_string()];
        write_atomic(dir.path(), &path, &value).unwrap();

        let read_back: Option<Vec<String>> = read(&path).unwrap();
        assert_eq!(read_back, Some(value.clone()));
        // Pretty JSON plus a trailing newline reaches disk after the fsync.
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n'));

        // A second write replaces in place and leaves no temp behind.
        let value2 = vec!["c".to_string()];
        write_atomic(dir.path(), &path, &value2).unwrap();
        let read_back2: Option<Vec<String>> = read(&path).unwrap();
        assert_eq!(read_back2, Some(value2));
        let leftover: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp files left behind: {leftover:?}");
    }

    #[test]
    fn write_removes_the_temp_file_when_the_rename_fails() {
        let dir = tempfile::tempdir().unwrap();
        // The target path is an existing, non-empty directory, so the final
        // rename(temp, path) fails *after* the temp file has been created and
        // synced — exercising the failure-cleanup path.
        let path = dir.path().join("target");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("child"), "x").unwrap();

        assert!(write_text_atomic(&path, "data").is_err());

        // The dead temp file was removed rather than orphaned in the data dir.
        let leftover: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp files left behind: {leftover:?}");
    }

    #[test]
    fn regular_temp_collision_is_retried_without_truncating_the_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let collision = dir.path().join("forced.tmp");
        fs::write(&collision, "do not truncate").unwrap();
        force_tmp_paths([collision.clone()]);

        write_text_atomic(&path, "published").unwrap();

        assert_eq!(fs::read_to_string(&collision).unwrap(), "do not truncate");
        assert_eq!(fs::read_to_string(&path).unwrap(), "published");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_temp_collision_is_not_followed_and_external_target_is_unchanged() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let sentinel = dir.path().join("sentinel");
        let collision = dir.path().join("forced.tmp");
        fs::write(&sentinel, "sentinel").unwrap();
        symlink(&sentinel, &collision).unwrap();
        force_tmp_paths([collision.clone()]);

        write_text_atomic(&path, "published").unwrap();

        assert!(
            fs::symlink_metadata(&collision)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "sentinel");
        assert_eq!(fs::read_to_string(&path).unwrap(), "published");
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_temp_collision_is_not_opened_or_unlinked() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let sentinel = dir.path().join("sentinel");
        let collision = dir.path().join("forced.tmp");
        fs::write(&sentinel, "sentinel").unwrap();
        fs::hard_link(&sentinel, &collision).unwrap();
        force_tmp_paths([collision.clone()]);

        write_text_atomic(&path, "published").unwrap();

        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "sentinel");
        assert_eq!(fs::read_to_string(&collision).unwrap(), "sentinel");
        assert_eq!(fs::metadata(&sentinel).unwrap().nlink(), 2);
    }

    #[test]
    fn temp_collision_retry_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let collision = dir.path().join("forced.tmp");
        fs::write(&collision, "collision").unwrap();
        force_tmp_paths(std::iter::repeat_n(
            collision.clone(),
            MAX_TEMP_CREATE_ATTEMPTS,
        ));

        let error = create_owned_temp(&path)
            .err()
            .expect("collisions must exhaust the bounded retry");

        assert!(error.to_string().contains("after 128 attempts"));
        assert_eq!(fs::read_to_string(collision).unwrap(), "collision");
    }

    #[test]
    fn temp_create_reports_a_missing_parent_without_creating_any_node() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("note.md");

        let error = create_owned_temp(&path)
            .err()
            .expect("a missing parent must fail temp creation");

        assert!(error.to_string().contains("failed to create"));
        assert!(!dir.path().join("missing").exists());
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_refuses_a_replacement_for_the_owned_temp_path() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let temp = create_owned_temp(&path).unwrap();
        fs::remove_file(&temp.path).unwrap();
        let sentinel = dir.path().join("sentinel");
        fs::write(&sentinel, "sentinel").unwrap();
        symlink(&sentinel, &temp.path).unwrap();

        let error = cleanup_owned_temp(&temp).unwrap_err();

        assert!(error.to_string().contains("refusing to clean replaced"));
        assert!(
            fs::symlink_metadata(&temp.path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "sentinel");

        let regular_temp = create_owned_temp(&path).unwrap();
        fs::remove_file(&regular_temp.path).unwrap();
        fs::write(&regular_temp.path, "replacement").unwrap();
        let error = cleanup_owned_temp(&regular_temp).unwrap_err();
        assert!(error.to_string().contains("refusing to clean replaced"));
        assert_eq!(
            fs::read_to_string(&regular_temp.path).unwrap(),
            "replacement"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_and_path_validators_reject_non_owned_shapes_and_identities() {
        let dir = tempfile::tempdir().unwrap();
        let directory = fs::File::open(dir.path()).unwrap();
        assert_eq!(
            identify_open_temp(&directory).err().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );

        let path = dir.path().join("note.md");
        let temp = create_owned_temp(&path).unwrap();
        let alias = dir.path().join("alias");
        fs::hard_link(&temp.path, &alias).unwrap();
        assert_eq!(
            verify_open_temp(&temp.file).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        fs::remove_file(alias).unwrap();

        fs::remove_file(&temp.path).unwrap();
        std::os::unix::fs::symlink("sentinel", &temp.path).unwrap();
        assert_eq!(
            verify_temp_path(&temp.path, &temp.file, temp.identity)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        fs::remove_file(&temp.path).unwrap();
        fs::write(&temp.path, "replacement").unwrap();
        assert_eq!(
            verify_temp_path(&temp.path, &temp.file, temp.identity)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_temp_detected_during_creation_is_conditionally_cleaned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let candidate = dir.path().join("forced.tmp");
        force_tmp_paths([candidate.clone()]);
        tamper_next_temp_after_create(TempAfterCreate::AddHardlink);

        let hardlink_error = create_owned_temp(&path).err().unwrap();

        assert!(hardlink_error.to_string().contains("unsafe atomic temp"));
        assert!(!candidate.exists());
        assert!(candidate.with_extension("alias").exists());

        let candidate = dir.path().join("forced-symlink.tmp");
        force_tmp_paths([candidate.clone()]);
        tamper_next_temp_after_create(TempAfterCreate::ReplaceWithSymlink);

        let replacement_error = create_owned_temp(&path).err().unwrap();

        assert!(
            replacement_error
                .to_string()
                .contains("atomic temp cleanup failed")
        );
        assert!(
            fs::symlink_metadata(candidate)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_unlinks_only_the_owned_name_after_a_hardlink_is_added() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let temp = create_owned_temp(&path).unwrap();
        let alias = dir.path().join("alias");
        fs::hard_link(&temp.path, &alias).unwrap();

        cleanup_owned_temp(&temp).unwrap();

        assert!(!temp.path.exists());
        assert!(alias.exists());
        assert_eq!(fs::metadata(alias).unwrap().nlink(), 1);
    }

    #[test]
    fn cleanup_accepts_an_already_absent_owned_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let temp = create_owned_temp(&path).unwrap();
        fs::remove_file(&temp.path).unwrap();

        cleanup_owned_temp(&temp).unwrap();
    }

    #[test]
    fn cleanup_handles_disappearance_and_reports_a_non_file_replacement_at_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let temp = create_owned_temp(&path).unwrap();
        tamper_next_cleanup_before_remove(CleanupBeforeRemove::Remove);
        cleanup_owned_temp(&temp).unwrap();

        let temp = create_owned_temp(&path).unwrap();
        tamper_next_cleanup_before_remove(CleanupBeforeRemove::ReplaceWithDirectory);
        let error = cleanup_owned_temp(&temp).unwrap_err();
        assert!(error.to_string().contains("failed to clean owned"));
        assert!(temp.path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn published_temp_is_private_owned_regular_and_singly_linked() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        write_text_atomic(&path, "published").unwrap();

        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(metadata.mode() & 0o777, 0o600);
    }

    #[test]
    fn concurrent_writers_publish_only_complete_documents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut writers = Vec::new();
        for writer in 0..8 {
            let path = path.clone();
            let barrier = barrier.clone();
            writers.push(std::thread::spawn(move || {
                let document = format!("writer-{writer}:{}", "x".repeat(16_384));
                barrier.wait();
                write_text_atomic(&path, &document).unwrap();
                document
            }));
        }
        let documents: Vec<_> = writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .collect();

        let published = fs::read_to_string(path).unwrap();
        assert!(documents.contains(&published));
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| {
                let name = entry.unwrap().file_name();
                name.to_string_lossy().contains(".tmp.").then_some(name)
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn atomic_replace_never_exposes_a_partial_document() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        let first = format!("first:{}", "a".repeat(64 * 1024));
        let second = format!("second:{}", "b".repeat(64 * 1024));
        write_atomically(&path, first.as_bytes(), false).unwrap();
        let finished = std::sync::Arc::new(AtomicBool::new(false));
        let writer_path = path.clone();
        let writer_finished = finished.clone();
        let writer_first = first.clone();
        let writer_second = second.clone();
        let writer = std::thread::spawn(move || {
            for round in 0..32 {
                let document = if round % 2 == 0 {
                    &writer_second
                } else {
                    &writer_first
                };
                write_atomically(&writer_path, document.as_bytes(), false).unwrap();
            }
            writer_finished.store(true, Ordering::Release);
        });

        while !finished.load(Ordering::Acquire) {
            let observed = fs::read_to_string(&path).unwrap();
            assert!(observed == first || observed == second);
        }
        writer.join().unwrap();
    }

    #[test]
    fn write_and_rename_failpoints_preserve_target_and_cleanup_owned_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        fs::write(&path, "original").unwrap();

        for stage in [AtomicWriteStage::Write, AtomicWriteStage::Rename] {
            fail_next_atomic_write(&path, stage);
            assert!(write_text_atomic(&path, "replacement").is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), "original");
        }
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| {
                let name = entry.unwrap().file_name();
                name.to_string_lossy().contains(".tmp.").then_some(name)
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn write_atomic_cache_round_trips_json_without_fsync() {
        // The cache variant skips the temp-file fsync and the parent-dir fsync
        // (the non-durable branch of `write_atomically`) but still writes atomically
        // through a temp file + rename, so it round-trips and leaves no temp behind.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let value = vec!["a".to_string(), "b".to_string()];
        write_atomic_cache(dir.path(), &path, &value).unwrap();

        let read_back: Option<Vec<String>> = read(&path).unwrap();
        assert_eq!(read_back, Some(value));
        assert!(fs::read_to_string(&path).unwrap().ends_with('\n'));

        // A second cache write replaces in place and leaves no temp behind.
        let value2 = vec!["c".to_string()];
        write_atomic_cache(dir.path(), &path, &value2).unwrap();
        let read_back2: Option<Vec<String>> = read(&path).unwrap();
        assert_eq!(read_back2, Some(value2));
        let leftover: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp files left behind: {leftover:?}");
    }

    #[test]
    fn write_atomic_creates_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested").join("data");
        let path = nested.join("index.json");
        write_atomic(&nested, &path, &"value".to_string()).unwrap();
        let read_back: Option<String> = read(&path).unwrap();
        assert_eq!(read_back, Some("value".to_string()));
    }

    #[test]
    fn read_versioned_round_trips_through_the_envelope() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug)]
        struct Payload {
            items: Vec<String>,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        let payload = Payload {
            items: vec!["x".to_string()],
        };
        write_versioned(dir.path(), &path, &payload).unwrap();

        // The envelope carries the format version alongside the flattened payload.
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 1"));
        assert!(text.contains("\"items\""));

        let back: Option<Payload> = read_versioned(&path).unwrap();
        assert_eq!(back, Some(payload));
        // A missing versioned file reads as `None`.
        let missing: Option<Payload> = read_versioned(&dir.path().join("nope.json")).unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn fsync_parent_dir_succeeds_for_a_real_directory() {
        // The directory exists and opens, so the best-effort sync runs without
        // panicking and the function returns normally.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        fs::write(&path, "x").unwrap();
        fsync_parent_dir(&path);
    }

    #[test]
    fn fsync_parent_dir_swallows_an_unopenable_parent() {
        // `Path::new("bare.md").parent()` is `Some("")`, and opening "" fails;
        // the error is swallowed rather than propagated, so this must not panic.
        fsync_parent_dir(Path::new("bare.md"));
    }

    #[test]
    fn fsync_parent_dir_is_a_noop_without_a_parent() {
        // `Path::new("").parent()` is `None`, exercising the early return.
        fsync_parent_dir(Path::new(""));
    }

    #[test]
    fn unique_tmp_path_differs_per_call_and_keeps_target_name() {
        let path = Path::new("/data/index.json");
        let a = unique_tmp_path(path);
        let b = unique_tmp_path(path);
        assert_ne!(a, b, "two calls must yield distinct temp names");
        for tmp in [&a, &b] {
            let name = tmp.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("index.json.tmp."), "unexpected: {name}");
        }
    }
}
