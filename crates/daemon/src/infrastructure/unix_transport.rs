//! Secure Unix-domain transport adapter.
//!
//! This module is deliberately outside `usagi-core`: core defines a byte-stream
//! port and framing, while this adapter owns filesystem and peer-credential
//! policy.  Every discovery and accept path validates ownership and refuses
//! symlinks; permission bits are defence in depth, not authentication.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use usagi_core::infrastructure::ipc::DaemonGeneration;

const DIR_MODE: u32 = 0o700;
const SOCKET_MODE: u32 = 0o600;
const LOCATOR_LOCK: &str = "current.lock";
const PRIVATE_FILE_FLAGS: i32 = libc::O_NOFOLLOW | libc::O_CLOEXEC;
const LOCATOR_TEMP_PREFIX: &str = ".current.json.tmp.";

/// Combined with the process ID, this makes every locator write use its own
/// temp pathname. A stale pathname is skipped rather than reclaimed because it
/// may belong to a still-running writer.
static LOCATOR_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindStage {
    SetTemporaryPermissions,
    RenameEndpoint,
    VerifyEndpoint,
    SetNonblocking,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocatorWriteStage {
    Write,
    Sync,
    Rename,
    ParentSync,
}

#[cfg(test)]
struct LocatorWriteFailpoint {
    target: PathBuf,
    stage: LocatorWriteStage,
}

#[cfg(test)]
thread_local! {
    static LOCATOR_WRITE_FAILPOINT: std::cell::RefCell<Option<LocatorWriteFailpoint>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
fn fail_next_locator_write(target: &Path, stage: LocatorWriteStage) {
    LOCATOR_WRITE_FAILPOINT.with(|failpoint| {
        *failpoint.borrow_mut() = Some(LocatorWriteFailpoint {
            target: target.to_path_buf(),
            stage,
        });
    });
}

#[cfg(test)]
fn take_locator_write_failpoint(target: &Path, stage: LocatorWriteStage) -> bool {
    LOCATOR_WRITE_FAILPOINT.with(|failpoint| {
        let matches = failpoint
            .borrow()
            .as_ref()
            .is_some_and(|failpoint| failpoint.target == target && failpoint.stage == stage);
        if matches {
            failpoint.borrow_mut().take();
        }
        matches
    })
}

/// The atomically-published endpoint a client is allowed to connect to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointLocator {
    pub generation: DaemonGeneration,
    pub endpoint: String,
    pub state: EndpointState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointState {
    Active,
    Draining,
}

/// A listener paired with its generation locator.
pub struct SecureUnixListener {
    listener: UnixListener,
    locator: EndpointLocator,
    daemon: PathBuf,
    socket: PathBuf,
    retired: bool,
}

impl SecureUnixListener {
    /// Creates `<data>/daemon` and a private generation directory, binds a
    /// temporary socket then atomically renames it to `sock`, and publishes the
    /// active locator only after all checks succeed.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe existing path, a duplicate generation, or
    /// a filesystem/socket failure.
    #[coverage(off)]
    pub fn bind(data_dir: &Path, generation: DaemonGeneration) -> io::Result<Self> {
        Self::bind_with(data_dir, generation, |_| Ok(()))
    }

    #[coverage(off)]
    fn bind_with(
        data_dir: &Path,
        generation: DaemonGeneration,
        mut before: impl FnMut(BindStage) -> io::Result<()>,
    ) -> io::Result<Self> {
        let daemon = data_dir.join("daemon");
        ensure_private_dir(&daemon)?;
        let generations = daemon.join("generations");
        ensure_private_dir(&generations)?;
        let generation_dir = generations.join(&generation.0);
        ensure_private_dir(&generation_dir)?;
        let socket = generation_dir.join("sock");
        if socket.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "generation endpoint already exists",
            ));
        }
        let temporary = generation_dir.join(".sock.bind");
        if temporary.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "temporary endpoint exists",
            ));
        }
        let result = (|| {
            let listener = UnixListener::bind(&temporary)?;
            before(BindStage::SetTemporaryPermissions)?;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(SOCKET_MODE))?;
            before(BindStage::RenameEndpoint)?;
            fs::rename(&temporary, &socket)?;
            before(BindStage::VerifyEndpoint)?;
            verify_private(&socket, SOCKET_MODE, false)?;
            let locator = EndpointLocator {
                generation,
                endpoint: relative_endpoint(&daemon, &socket)?,
                state: EndpointState::Active,
            };
            before(BindStage::SetNonblocking)?;
            listener.set_nonblocking(true)?;
            write_locator(&daemon, &locator)?;
            Ok((listener, locator))
        })();
        let (listener, locator) = match result {
            Ok(published) => published,
            Err(error) => {
                // `Self` does not exist yet, so its Drop cannot retire files
                // created before locator publication. Cover every ordinary
                // error after bind, whether the endpoint is still temporary or
                // has already been renamed to its generation path.
                let temporary_cleanup = remove_file_if_present(&temporary);
                let socket_cleanup = remove_file_if_present(&socket);
                if let Err(cleanup) = temporary_cleanup.and(socket_cleanup) {
                    return Err(io::Error::new(
                        cleanup.kind(),
                        format!("{error}; endpoint rollback failed: {cleanup}"),
                    ));
                }
                return Err(error);
            }
        };
        Ok(Self {
            listener,
            locator,
            daemon,
            socket,
            retired: false,
        })
    }

    #[must_use]
    pub fn locator(&self) -> &EndpointLocator {
        &self.locator
    }

    /// Accept exactly one already-authenticated peer. `WouldBlock` is passed to
    /// the event loop; credential failures close before a protocol byte is read.
    ///
    /// # Errors
    ///
    /// Returns `WouldBlock` when no peer is pending and `PermissionDenied` for
    /// a peer whose OS credential does not match the daemon UID.
    #[coverage(off)]
    pub fn accept(&self) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept()?;
        if peer_uid(&stream)? != effective_uid() {
            drop(stream);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Unix peer uid does not match daemon uid",
            ));
        }
        stream.set_nonblocking(true)?;
        Ok(stream)
    }

    /// Retires this listener's generation endpoint. The locator is removed only
    /// when it still names this exact generation and relative endpoint; the
    /// generation-specific socket is always this listener's to reclaim.
    /// Locator publication and conditional removal share an exclusive lock, so
    /// a stale owner cannot win a compare/unlink race against a replacement.
    ///
    /// # Errors
    ///
    /// Returns an error when the locator cannot be inspected/removed safely or
    /// this generation's socket cannot be unlinked.
    #[coverage(off)]
    pub fn retire(&mut self) -> io::Result<()> {
        if self.retired {
            return Ok(());
        }

        let locator_result = (|| {
            let _lock = lock_locator(&self.daemon)?;
            match read_locator(&self.daemon) {
                Ok(current) if owns_endpoint(&current, &self.locator) => {
                    remove_file_if_present(&self.daemon.join("current.json"))
                }
                Ok(_) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        })();
        let socket_result = remove_file_if_present(&self.socket);

        locator_result?;
        socket_result?;
        self.retired = true;
        Ok(())
    }
}

impl Drop for SecureUnixListener {
    #[coverage(off)]
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

/// Looks up and verifies the current locator before connecting.  A caller that
/// holds an older generation can inspect its record separately; this function
/// is intentionally restricted to the active locator.
///
/// # Errors
///
/// Returns an error if the locator or endpoint is unsafe, invalid, draining, or
/// cannot be connected.
#[coverage(off)]
pub fn connect_current(data_dir: &Path) -> io::Result<UnixStream> {
    let daemon = data_dir.join("daemon");
    ensure_private_dir(&daemon)?;
    let locator = read_locator(&daemon)?;
    if locator.state != EndpointState::Active {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "daemon endpoint is draining",
        ));
    }
    let endpoint = checked_endpoint(&daemon, &locator)?;
    UnixStream::connect(endpoint)
}

/// Reads a previously atomically-published current locator.
///
/// # Errors
///
/// Returns an error if the locator is absent, unsafe, or malformed.
#[coverage(off)]
pub fn read_locator(daemon: &Path) -> io::Result<EndpointLocator> {
    let path = daemon.join("current.json");
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(PRIVATE_FILE_FLAGS | libc::O_NONBLOCK)
        .open(path)?;
    verify_open_private_file(&file, SOCKET_MODE)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[coverage(off)]
fn write_locator(daemon: &Path, locator: &EndpointLocator) -> io::Result<()> {
    let _lock = lock_locator(daemon)?;
    let target = daemon.join("current.json");
    let bytes = serde_json::to_vec(locator).expect("endpoint locator serializes");
    let (temporary, mut file) = loop {
        let temporary = unique_locator_temp_path(daemon);
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(SOCKET_MODE)
            .custom_flags(PRIVATE_FILE_FLAGS)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    };

    // Once create_new succeeds this writer owns the pathname. Every error
    // before rename removes only that pathname; a hard crash can leave it
    // behind, but later writers use distinct names and therefore recover.
    let result = (|| {
        make_open_file_private(&file, SOCKET_MODE)?;

        #[cfg(test)]
        if take_locator_write_failpoint(&target, LocatorWriteStage::Write) {
            file.write_all(&bytes[..bytes.len() / 2])?;
            return Err(io::Error::other("injected locator write failure"));
        }
        file.write_all(&bytes)?;

        #[cfg(test)]
        if take_locator_write_failpoint(&target, LocatorWriteStage::Sync) {
            return Err(io::Error::other("injected locator sync failure"));
        }
        file.sync_all()?;

        #[cfg(test)]
        if take_locator_write_failpoint(&target, LocatorWriteStage::Rename) {
            return Err(io::Error::other("injected locator rename failure"));
        }
        fs::rename(&temporary, &target)?;

        // A successful rename is the commit point. Directory fsync improves
        // power-loss durability where supported, but failure after commit must
        // not report an ambiguous failed publication to the caller.
        #[cfg(test)]
        let parent_sync_result =
            if take_locator_write_failpoint(&target, LocatorWriteStage::ParentSync) {
                Err(io::Error::other("injected locator parent sync failure"))
            } else {
                fs::File::open(daemon).and_then(|parent| parent.sync_all())
            };
        #[cfg(not(test))]
        let parent_sync_result = fs::File::open(daemon).and_then(|parent| parent.sync_all());
        let _ = parent_sync_result;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(cleanup) = remove_file_if_present(&temporary) {
            return Err(io::Error::new(
                cleanup.kind(),
                format!("{error}; locator temp rollback failed: {cleanup}"),
            ));
        }
        return Err(error);
    }
    Ok(())
}

fn unique_locator_temp_path(daemon: &Path) -> PathBuf {
    daemon.join(format!(
        "{LOCATOR_TEMP_PREFIX}{}.{}",
        std::process::id(),
        LOCATOR_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn owns_endpoint(current: &EndpointLocator, owner: &EndpointLocator) -> bool {
    current.generation == owner.generation && current.endpoint == owner.endpoint
}

#[coverage(off)]
fn lock_locator(daemon: &Path) -> io::Result<fs::File> {
    let path = daemon.join(LOCATOR_LOCK);
    let (file, created) = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(SOCKET_MODE)
        .custom_flags(PRIVATE_FILE_FLAGS)
        .open(&path)
    {
        Ok(file) => (file, true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            verify_private(&path, SOCKET_MODE, false)?;
            (
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(PRIVATE_FILE_FLAGS)
                    .open(&path)?,
                false,
            )
        }
        Err(error) => return Err(error),
    };
    if created {
        make_open_file_private(&file, SOCKET_MODE)?;
    } else {
        verify_open_private_file(&file, SOCKET_MODE)?;
    }
    verify_private(&path, SOCKET_MODE, false)?;
    FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn make_open_file_private(file: &fs::File, mode: u32) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != effective_uid() || metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe daemon endpoint file ownership or type",
        ));
    }
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    verify_open_private_file(file, mode)
}

fn verify_open_private_file(file: &fs::File, mode: u32) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o777 != mode
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe daemon endpoint file ownership or mode",
        ));
    }
    Ok(())
}

#[coverage(off)]
fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn checked_endpoint(daemon: &Path, locator: &EndpointLocator) -> io::Result<PathBuf> {
    let endpoint = daemon.join(&locator.endpoint);
    let expected = daemon
        .join("generations")
        .join(&locator.generation.0)
        .join("sock");
    if endpoint != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "locator endpoint is outside generation directory",
        ));
    }
    verify_private(&endpoint, SOCKET_MODE, false)?;
    Ok(endpoint)
}

fn relative_endpoint(daemon: &Path, endpoint: &Path) -> io::Result<String> {
    endpoint
        .strip_prefix(daemon)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "endpoint outside daemon directory",
            )
        })
        .map(|path| path.to_string_lossy().into_owned())
}

/// Creates a private endpoint directory or verifies that an existing one is
/// owned by the current user and has mode `0700`.
///
/// # Errors
///
/// Returns an error when `path` cannot be created or is not a private
/// directory owned by the current user.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => verify_private(path, DIR_MODE, true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(DIR_MODE))?;
            verify_private(path, DIR_MODE, true)
        }
        Err(error) => Err(error),
    }
}

fn verify_private(path: &Path, mode: u32, directory: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o777 != mode
        || (directory && !metadata.is_dir())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe daemon endpoint ownership or mode",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[coverage(off)]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut credential = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut size = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
        .expect("ucred size fits socklen_t");
    // SAFETY: credential is initialized and sized for SO_PEERCRED; fd belongs
    // to the live Unix stream for the duration of this call.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credential).cast(),
            &raw mut size,
        )
    };
    if result == 0 && size as usize == std::mem::size_of::<libc::ucred>() {
        Ok(credential.uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
#[coverage(off)]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: getpeereid writes the supplied uid/gid pointers for this socket.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
    if result == 0 {
        Ok(uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[coverage(off)]
fn peer_uid(_stream: &UnixStream) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer credentials unavailable",
    ))
}

#[coverage(off)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    const UMASK_TEST_CHILD: &str = "USAGI_LOCATOR_UMASK_TEST_CHILD";

    fn generation() -> DaemonGeneration {
        DaemonGeneration(
            usagi_core::domain::id::DaemonGeneration::new()
                .as_str()
                .clone(),
        )
    }

    fn locator() -> EndpointLocator {
        let generation = generation();
        EndpointLocator {
            endpoint: format!("generations/{}/sock", generation.0),
            generation,
            state: EndpointState::Active,
        }
    }

    fn locator_temp_names(daemon: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(daemon)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(LOCATOR_TEMP_PREFIX))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn binds_private_endpoint_publishes_locator_and_authenticates_same_uid() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let generation = generation();
        let listener = SecureUnixListener::bind(temp.path(), generation.clone()).unwrap();
        let daemon = temp.path().join("daemon");
        assert_eq!(fs::metadata(&daemon).unwrap().mode() & 0o777, DIR_MODE);
        let locator = read_locator(&daemon).unwrap();
        assert_eq!(locator.generation, generation);
        assert_eq!(listener.locator(), &locator);
        assert_eq!(
            fs::metadata(checked_endpoint(&daemon, &locator).unwrap())
                .unwrap()
                .mode()
                & 0o777,
            SOCKET_MODE
        );
        let client = connect_current(temp.path()).unwrap();
        let accepted = listener.accept().unwrap();
        drop((client, accepted));
    }

    #[test]
    fn retirement_is_generation_fenced_and_removes_only_the_owned_endpoint() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let mut old = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let old_socket = old.socket.clone();
        let old_locator = old.locator().clone();

        let mut replacement = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let replacement_socket = replacement.socket.clone();
        let replacement_locator = replacement.locator().clone();
        assert!(!owns_endpoint(&replacement_locator, &old_locator));

        old.retire().unwrap();
        // Retiring an already-retired owner is an idempotent no-op.
        old.retire().unwrap();
        assert!(!old_socket.exists());
        assert_eq!(
            read_locator(&temp.path().join("daemon")).unwrap(),
            replacement_locator
        );
        assert!(replacement_socket.exists());
        let client = connect_current(temp.path()).unwrap();
        let accepted = replacement.accept().unwrap();
        drop((client, accepted));

        replacement.retire().unwrap();
        assert!(!replacement_socket.exists());
        assert_eq!(
            connect_current(temp.path()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn publication_failure_removes_its_unpublished_generation_endpoint() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let lock = daemon.join(LOCATOR_LOCK);
        fs::write(&lock, []).unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
        let generation = generation();
        let socket = daemon.join("generations").join(&generation.0).join("sock");

        assert!(SecureUnixListener::bind(temp.path(), generation).is_err());

        assert!(!socket.exists());
        assert!(!daemon.join("current.json").exists());
        assert!(locator_temp_names(&daemon).is_empty());
    }

    #[test]
    fn every_post_bind_failure_rolls_back_temporary_and_renamed_endpoints() {
        for failure in [
            BindStage::SetTemporaryPermissions,
            BindStage::RenameEndpoint,
            BindStage::VerifyEndpoint,
            BindStage::SetNonblocking,
        ] {
            let temp = TempDir::new_in("/tmp").unwrap();
            let generation = generation();
            let generation_dir = temp.path().join("daemon/generations").join(&generation.0);

            let result = SecureUnixListener::bind_with(temp.path(), generation, |stage| {
                if stage == failure {
                    Err(io::Error::other(format!(
                        "injected failure before {stage:?}"
                    )))
                } else {
                    Ok(())
                }
            });
            let error = result
                .err()
                .expect("injected post-bind failure unexpectedly succeeded");

            assert!(error.to_string().contains("injected failure"));
            assert!(!generation_dir.join(".sock.bind").exists());
            assert!(!generation_dir.join("sock").exists());
            assert!(!temp.path().join("daemon/current.json").exists());
        }
    }

    #[test]
    fn pre_existing_orphan_locator_temp_does_not_block_publication() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let orphan = daemon.join(".current.json.tmp");
        fs::write(&orphan, b"orphan").unwrap();

        let listener = SecureUnixListener::bind(temp.path(), generation()).unwrap();

        assert_eq!(read_locator(&daemon).unwrap(), *listener.locator());
        assert_eq!(fs::read(orphan).unwrap(), b"orphan");
        assert!(locator_temp_names(&daemon).is_empty());
        let client = connect_current(temp.path()).unwrap();
        let accepted = listener.accept().unwrap();
        drop((client, accepted));
    }

    #[test]
    fn locator_failures_preserve_old_publication_cleanup_temp_and_allow_retry() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let target = daemon.join("current.json");
        let mut current = locator();
        write_locator(&daemon, &current).unwrap();

        for stage in [
            LocatorWriteStage::Write,
            LocatorWriteStage::Sync,
            LocatorWriteStage::Rename,
        ] {
            let old_bytes = fs::read(&target).unwrap();
            let replacement = locator();
            fail_next_locator_write(&target, stage);

            let error = write_locator(&daemon, &replacement).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Other, "stage: {stage:?}");
            assert_eq!(fs::read(&target).unwrap(), old_bytes, "stage: {stage:?}");
            assert_eq!(read_locator(&daemon).unwrap(), current, "stage: {stage:?}");
            assert!(locator_temp_names(&daemon).is_empty(), "stage: {stage:?}");

            write_locator(&daemon, &replacement).unwrap();
            assert_eq!(read_locator(&daemon).unwrap(), replacement);
            assert!(
                locator_temp_names(&daemon).is_empty(),
                "retry after: {stage:?}"
            );
            current = replacement;
        }
    }

    #[test]
    fn parent_sync_failure_is_best_effort_after_locator_commit() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let target = daemon.join("current.json");
        let locator = locator();
        fail_next_locator_write(&target, LocatorWriteStage::ParentSync);

        write_locator(&daemon, &locator).unwrap();

        assert_eq!(read_locator(&daemon).unwrap(), locator);
        assert!(locator_temp_names(&daemon).is_empty());
    }

    #[test]
    fn restrictive_umask_still_publishes_an_exact_private_regular_locator() {
        if std::env::var_os(UMASK_TEST_CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "infrastructure::unix_transport::tests::restrictive_umask_still_publishes_an_exact_private_regular_locator",
                    "--nocapture",
                ])
                .env(UMASK_TEST_CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let temp = TempDir::new_in("/tmp").unwrap();
        // SAFETY: this exact-test child has no sibling tests or application
        // threads, and the original process-global umask is restored below.
        let original_umask = unsafe { libc::umask(0o777) };
        let result = (|| {
            let listener = SecureUnixListener::bind(temp.path(), generation())?;
            let daemon = temp.path().join("daemon");
            let path = daemon.join("current.json");
            let metadata = fs::metadata(&path)?;
            assert!(metadata.is_file());
            assert_eq!(metadata.uid(), effective_uid());
            assert_eq!(metadata.mode() & 0o777, SOCKET_MODE);
            assert_eq!(read_locator(&daemon)?, *listener.locator());
            assert!(locator_temp_names(&daemon).is_empty());
            Ok::<_, io::Error>(())
        })();
        // SAFETY: restores the value returned by the paired umask call above.
        unsafe { libc::umask(original_umask) };
        result.unwrap();
    }

    #[test]
    fn concurrent_locator_writers_publish_whole_json_without_temp_collisions() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let first = locator();
        let second = locator();
        let barrier = Arc::new(Barrier::new(3));

        std::thread::scope(|scope| {
            let first_writer = {
                let barrier = Arc::clone(&barrier);
                let daemon = daemon.clone();
                let first = first.clone();
                scope.spawn(move || {
                    barrier.wait();
                    write_locator(&daemon, &first)
                })
            };
            let second_writer = {
                let barrier = Arc::clone(&barrier);
                let daemon = daemon.clone();
                let second = second.clone();
                scope.spawn(move || {
                    barrier.wait();
                    write_locator(&daemon, &second)
                })
            };
            barrier.wait();
            first_writer.join().unwrap().unwrap();
            second_writer.join().unwrap().unwrap();
        });

        let published = read_locator(&daemon).unwrap();
        assert!(published == first || published == second);
        assert!(locator_temp_names(&daemon).is_empty());
    }

    #[test]
    fn read_locator_rejects_symlinks_and_non_regular_files() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let path = daemon.join("current.json");
        let target = daemon.join("target.json");
        fs::write(&target, serde_json::to_vec(&locator()).unwrap()).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(read_locator(&daemon).is_err());

        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        assert_eq!(
            read_locator(&daemon).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn rejects_symlinked_daemon_directory_and_unsafe_locator() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, temp.path().join("daemon")).unwrap();
        assert!(SecureUnixListener::bind(temp.path(), generation()).is_err());

        let clean = TempDir::new_in("/tmp").unwrap();
        let listener = SecureUnixListener::bind(clean.path(), generation()).unwrap();
        let locator_path = clean.path().join("daemon/current.json");
        fs::set_permissions(&locator_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(connect_current(clean.path()).is_err());
        drop(listener);

        let outside = clean.path().join("outside.sock");
        assert_eq!(
            relative_endpoint(&clean.path().join("daemon"), &outside)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let unsafe_locator = EndpointLocator {
            generation: generation(),
            endpoint: "outside.sock".into(),
            state: EndpointState::Active,
        };
        assert_eq!(
            checked_endpoint(&clean.path().join("daemon"), &unsafe_locator)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let file = clean.path().join("not-a-directory");
        fs::write(&file, []).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(DIR_MODE)).unwrap();
        assert_eq!(
            ensure_private_dir(&file).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(verify_open_private_file(&fs::File::open(&file).unwrap(), SOCKET_MODE).is_err());
        assert!(
            make_open_file_private(&fs::File::open(clean.path()).unwrap(), SOCKET_MODE).is_err()
        );
        let invalid = PathBuf::from(std::ffi::OsString::from_vec(b"bad\0path".to_vec()));
        assert_eq!(
            ensure_private_dir(&invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
