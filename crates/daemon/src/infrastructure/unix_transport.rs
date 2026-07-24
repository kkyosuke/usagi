//! Secure Unix-domain transport adapter.
//!
//! This module is deliberately outside `usagi-core`: core defines a byte-stream
//! port and framing, while this adapter owns filesystem and peer-credential
//! policy.  Every discovery and accept path validates ownership and refuses
//! symlinks; permission bits are defence in depth, not authentication.

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use usagi_core::infrastructure::ipc::DaemonGeneration;

const DIR_MODE: u32 = 0o700;
const SOCKET_MODE: u32 = 0o600;
const LOCATOR_LOCK: &str = "current.lock";
const PRIVATE_FILE_FLAGS: i32 = libc::O_NOFOLLOW | libc::O_CLOEXEC;
const LOCATOR_TEMP_PREFIX: &str = ".current.json.tmp.";
#[cfg(test)]
const LOCATOR_HARDLINK_ALIAS: &str = ".current.json.injected-hardlink";
#[cfg(test)]
const PRIVATE_DIR_CRASH_PATH: &str = "USAGI_PRIVATE_DIR_CRASH_PATH";
#[cfg(test)]
const LOCATOR_LOCK_CRASH_PATH: &str = "USAGI_LOCATOR_LOCK_CRASH_PATH";
#[cfg(test)]
const PRIVATE_CHAIN_CRASH_TARGET: &str = "USAGI_PRIVATE_CHAIN_CRASH_TARGET";

/// Combined with the process ID, this makes every locator write use its own
/// temp pathname. A stale pathname is skipped rather than reclaimed because it
/// may belong to a still-running writer.
static LOCATOR_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindStage {
    CaptureIdentity,
    SetTemporaryPermissions,
    RenameEndpoint,
    VerifyEndpoint,
    SetNonblocking,
    VerifyPublication,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocatorWriteStage {
    CollideTemporaryCreate,
    HardlinkTemporaryAfterCreate,
    Write,
    Sync,
    Rename,
    HardlinkBeforeFinalVerify,
    FinalVerify,
    ParentSync,
}

#[cfg(test)]
struct LocatorWriteFailpoint {
    target: PathBuf,
    stage: LocatorWriteStage,
}

#[cfg(test)]
struct LocatorReplacementFailpoint {
    target: PathBuf,
    bytes: Vec<u8>,
}

#[cfg(test)]
struct LocatorLockBarrier {
    locked: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
}

#[cfg(test)]
struct LocatorLockSetupBarrier {
    ready: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
}

#[cfg(test)]
struct PathBarrier {
    path: PathBuf,
    ready: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
}

#[cfg(test)]
thread_local! {
    static LOCATOR_WRITE_FAILPOINT: std::cell::RefCell<Option<LocatorWriteFailpoint>> = const {
        std::cell::RefCell::new(None)
    };
    static LOCATOR_REPLACEMENT_FAILPOINT: std::cell::RefCell<Option<LocatorReplacementFailpoint>> =
        const { std::cell::RefCell::new(None) };
    static LOCATOR_LOCK_BARRIER: std::cell::RefCell<Option<LocatorLockBarrier>> =
        const { std::cell::RefCell::new(None) };
    static LOCATOR_LOCK_SETUP_BARRIER: std::cell::RefCell<Option<LocatorLockSetupBarrier>> =
        const { std::cell::RefCell::new(None) };
    static LOCATOR_LOCK_OPEN_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static CONNECT_ENDPOINT_BARRIER: std::cell::RefCell<Option<PathBarrier>> =
        const { std::cell::RefCell::new(None) };
    static PRIVATE_CHAIN_ANCHOR_BARRIER: std::cell::RefCell<Option<PathBarrier>> =
        const { std::cell::RefCell::new(None) };
    static GENERATION_ROOT_BARRIER: std::cell::RefCell<Option<PathBarrier>> =
        const { std::cell::RefCell::new(None) };
    static GENERATION_SCAN_BARRIER: std::cell::RefCell<Option<PathBarrier>> =
        const { std::cell::RefCell::new(None) };
    static GENERATION_UNLINK_BARRIER: std::cell::RefCell<Option<PathBarrier>> =
        const { std::cell::RefCell::new(None) };
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

#[cfg(test)]
fn replace_locator_after_rename(target: &Path, replacement: &EndpointLocator) {
    LOCATOR_REPLACEMENT_FAILPOINT.with(|failpoint| {
        *failpoint.borrow_mut() = Some(LocatorReplacementFailpoint {
            target: target.to_path_buf(),
            bytes: serde_json::to_vec(replacement).unwrap(),
        });
    });
}

#[cfg(test)]
fn take_locator_replacement(target: &Path) -> Option<Vec<u8>> {
    LOCATOR_REPLACEMENT_FAILPOINT.with(|failpoint| {
        let matches = failpoint
            .borrow()
            .as_ref()
            .is_some_and(|failpoint| failpoint.target == target);
        matches.then(|| failpoint.borrow_mut().take().unwrap().bytes)
    })
}

#[cfg(test)]
fn pause_next_locator_lock_before_verify(
    locked: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
) {
    LOCATOR_LOCK_BARRIER.with(|barrier| {
        *barrier.borrow_mut() = Some(LocatorLockBarrier { locked, resume });
    });
}

#[cfg(test)]
fn pause_locator_lock_before_verify() {
    LOCATOR_LOCK_BARRIER.with(|barrier| {
        if let Some(barrier) = barrier.borrow_mut().take() {
            barrier.locked.wait();
            barrier.resume.wait();
        }
    });
}

#[cfg(test)]
fn pause_next_locator_lock_after_setup(
    ready: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
) {
    LOCATOR_LOCK_SETUP_BARRIER.with(|barrier| {
        *barrier.borrow_mut() = Some(LocatorLockSetupBarrier { ready, resume });
    });
}

#[cfg(test)]
fn pause_locator_lock_after_setup() {
    LOCATOR_LOCK_SETUP_BARRIER.with(|barrier| {
        if let Some(barrier) = barrier.borrow_mut().take() {
            barrier.ready.wait();
            barrier.resume.wait();
        }
    });
}

#[cfg(test)]
fn fail_next_locator_lock_open() {
    LOCATOR_LOCK_OPEN_ERROR.set(true);
}

#[cfg(test)]
fn take_locator_lock_open_error() -> bool {
    LOCATOR_LOCK_OPEN_ERROR.replace(false)
}

#[cfg(test)]
fn pause_next_connect_endpoint(
    path: &Path,
    ready: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
) {
    CONNECT_ENDPOINT_BARRIER.with(|barrier| {
        *barrier.borrow_mut() = Some(PathBarrier {
            path: path.to_path_buf(),
            ready,
            resume,
        });
    });
}

#[cfg(test)]
fn pause_connect_endpoint(path: &Path) {
    CONNECT_ENDPOINT_BARRIER.with(|barrier| {
        let matches = barrier
            .borrow()
            .as_ref()
            .is_some_and(|barrier| barrier.path == path);
        if matches {
            let barrier = barrier.borrow_mut().take().unwrap();
            barrier.ready.wait();
            barrier.resume.wait();
        }
    });
}

#[cfg(test)]
fn pause_next_private_chain_anchor_recheck(
    path: &Path,
    ready: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
) {
    PRIVATE_CHAIN_ANCHOR_BARRIER.with(|barrier| {
        *barrier.borrow_mut() = Some(PathBarrier {
            path: path.to_path_buf(),
            ready,
            resume,
        });
    });
}

#[cfg(test)]
fn pause_private_chain_anchor_recheck(path: &Path) {
    PRIVATE_CHAIN_ANCHOR_BARRIER.with(|barrier| {
        let matches = barrier
            .borrow()
            .as_ref()
            .is_some_and(|barrier| barrier.path == path);
        if matches {
            let barrier = barrier.borrow_mut().take().unwrap();
            barrier.ready.wait();
            barrier.resume.wait();
        }
    });
}

#[cfg(test)]
fn pause_next_generation_root_recheck(
    path: &Path,
    ready: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
) {
    GENERATION_ROOT_BARRIER.with(|barrier| {
        *barrier.borrow_mut() = Some(PathBarrier {
            path: path.to_path_buf(),
            ready,
            resume,
        });
    });
}

#[cfg(test)]
fn pause_generation_root_recheck(path: &Path) {
    GENERATION_ROOT_BARRIER.with(|barrier| {
        let matches = barrier
            .borrow()
            .as_ref()
            .is_some_and(|barrier| barrier.path == path);
        if matches {
            let barrier = barrier.borrow_mut().take().unwrap();
            barrier.ready.wait();
            barrier.resume.wait();
        }
    });
}

#[cfg(test)]
fn pause_next_generation_scan(
    path: &Path,
    ready: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
) {
    GENERATION_SCAN_BARRIER.with(|barrier| {
        *barrier.borrow_mut() = Some(PathBarrier {
            path: path.to_path_buf(),
            ready,
            resume,
        });
    });
}

#[cfg(test)]
fn pause_generation_scan(path: &Path) {
    GENERATION_SCAN_BARRIER.with(|barrier| {
        let matches = barrier
            .borrow()
            .as_ref()
            .is_some_and(|barrier| barrier.path == path);
        if matches {
            let barrier = barrier.borrow_mut().take().unwrap();
            barrier.ready.wait();
            barrier.resume.wait();
        }
    });
}

#[cfg(test)]
fn pause_next_generation_unlink(
    path: &Path,
    ready: std::sync::Arc<std::sync::Barrier>,
    resume: std::sync::Arc<std::sync::Barrier>,
) {
    GENERATION_UNLINK_BARRIER.with(|barrier| {
        *barrier.borrow_mut() = Some(PathBarrier {
            path: path.to_path_buf(),
            ready,
            resume,
        });
    });
}

#[cfg(test)]
fn pause_generation_unlink(path: &Path) {
    GENERATION_UNLINK_BARRIER.with(|barrier| {
        let matches = barrier
            .borrow()
            .as_ref()
            .is_some_and(|barrier| barrier.path == path);
        if matches {
            let barrier = barrier.borrow_mut().take().unwrap();
            barrier.ready.wait();
            barrier.resume.wait();
        }
    });
}

#[cfg(test)]
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=locator_lock_creation_crash_under_restrictive_umask_is_repaired_on_reopen
fn crash_if_requested(variable: &str, path: &Path, status: i32) {
    if std::env::var_os(variable).is_some_and(|requested| requested == path.as_os_str()) {
        // SAFETY: this is a test-only failpoint that deliberately models a
        // process crash without running destructors after filesystem creation.
        unsafe { libc::_exit(status) };
    }
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
    cleanup: EndpointCleanup,
    retired: bool,
}

/// Retryable ownership proof for one published generation endpoint.
///
/// The token is deliberately independent from the live listener fd. Startup
/// errors and an accept-loop panic can drop or lose that fd, while the daemon
/// owner must still be able to prove that its exact socket and locator were
/// retired before clearing the lifecycle record.
#[derive(Clone)]
pub struct EndpointCleanup {
    locator: EndpointLocator,
    daemon: PathBuf,
    #[cfg_attr(not(test), allow(dead_code))]
    socket: PathBuf,
    socket_identity: SocketIdentity,
    generation_dir: PathBuf,
    generation_directory: Arc<fs::File>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    dev: u64,
    ino: u64,
    uid: u32,
    nlink: u64,
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
        let generation_directory = Arc::new(lock_setup_directory(&generation_dir, true)?);
        verify_locked_generation_directory(&generation_dir, &generation_directory)?;
        let socket = generation_dir.join("sock");
        if socket_stat_at(&generation_directory, "sock")?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "generation endpoint already exists",
            ));
        }
        let temporary = generation_dir.join(".sock.bind");
        if socket_stat_at(&generation_directory, ".sock.bind")?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "temporary endpoint exists",
            ));
        }
        let mut socket_identity = None;
        let mut bind_attempted = false;
        let mut renamed = false;
        let result = (|| {
            verify_locked_generation_directory(&generation_dir, &generation_directory)?;
            bind_attempted = true;
            let listener = UnixListener::bind(&temporary)?;
            verify_locked_generation_directory(&generation_dir, &generation_directory)?;
            before(BindStage::CaptureIdentity)?;
            let identity = capture_owned_socket_identity_at(&generation_directory, ".sock.bind")?;
            socket_identity = Some(identity);
            before(BindStage::SetTemporaryPermissions)?;
            verify_locked_generation_directory(&generation_dir, &generation_directory)?;
            verify_owned_socket_identity_at(&generation_directory, ".sock.bind", identity, false)?;
            set_socket_permissions_at(&generation_directory, ".sock.bind", SOCKET_MODE)?;
            verify_owned_socket_identity_at(&generation_directory, ".sock.bind", identity, true)?;
            before(BindStage::RenameEndpoint)?;
            verify_locked_generation_directory(&generation_dir, &generation_directory)?;
            verify_owned_socket_identity_at(&generation_directory, ".sock.bind", identity, true)?;
            rename_socket_noreplace_at(&generation_directory, ".sock.bind", "sock")?;
            renamed = true;
            before(BindStage::VerifyEndpoint)?;
            verify_locked_generation_directory(&generation_dir, &generation_directory)?;
            verify_owned_socket_identity_at(&generation_directory, "sock", identity, true)?;
            let locator = EndpointLocator {
                generation,
                endpoint: relative_endpoint(&daemon, &socket)?,
                state: EndpointState::Active,
            };
            before(BindStage::SetNonblocking)?;
            listener.set_nonblocking(true)?;
            verify_locked_generation_directory(&generation_dir, &generation_directory)?;
            verify_owned_socket_identity_at(&generation_directory, "sock", identity, true)?;
            // Publication takes current.lock. Release the generation setup
            // fence first so stale cleanup never observes the reverse order.
            FileExt::unlock(generation_directory.as_ref())?;
            write_locator(&daemon, &locator, || {
                before(BindStage::VerifyPublication)?;
                verify_open_directory(&generation_dir, &generation_directory, true)?;
                verify_owned_socket_identity_at(&generation_directory, "sock", identity, true)
            })?;
            Ok((listener, locator, identity))
        })();
        let (listener, locator, socket_identity) = match result {
            Ok(published) => published,
            Err(error) => {
                // `Self` does not exist yet, so its Drop cannot retire files
                // created before locator publication. Cover every ordinary
                // error after bind, whether the endpoint is still temporary or
                // has already been renamed to its generation path.
                return Err(rollback_bound_endpoint_error(
                    error,
                    &generation_dir,
                    &generation_directory,
                    socket_identity,
                    bind_attempted,
                    renamed,
                ));
            }
        };
        Ok(Self {
            listener,
            cleanup: EndpointCleanup {
                locator,
                daemon,
                socket,
                socket_identity,
                generation_dir,
                generation_directory,
            },
            retired: false,
        })
    }

    #[must_use]
    pub fn locator(&self) -> &EndpointLocator {
        &self.cleanup.locator
    }

    /// Returns an independently retained cleanup capability for this exact
    /// generation. It remains usable if the listener-owning worker unwinds.
    #[must_use]
    pub fn cleanup_handle(&self) -> EndpointCleanup {
        self.cleanup.clone()
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
        self.cleanup.retire()?;
        self.retired = true;
        Ok(())
    }
}

impl EndpointCleanup {
    /// Removes this exact generation socket first and only then removes a
    /// locator that still names it. A missing locator is therefore successful
    /// cleanup proof only after the owned socket is known to be absent.
    ///
    /// # Errors
    ///
    /// Returns an error without removing the locator when the owned socket
    /// cannot be verified or removed, or when the locator cannot be inspected
    /// safely. The token remains retryable after an error.
    pub fn retire(&self) -> io::Result<()> {
        let _lock = lock_locator(&self.daemon)?;
        verify_open_directory(&self.generation_dir, &self.generation_directory, true)?;
        remove_owned_socket_at_if_present(
            &self.generation_directory,
            "sock",
            Some(self.socket_identity),
            true,
        )?;
        verify_open_directory(&self.generation_dir, &self.generation_directory, true)?;
        match read_locator(&self.daemon) {
            Ok(current) if owns_endpoint(&current, &self.locator) => {
                remove_file_if_present(&self.daemon.join("current.json"))
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for SecureUnixListener {
    #[coverage(off)]
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

fn rollback_bound_endpoint_error(
    error: io::Error,
    generation_dir: &Path,
    generation_directory: &fs::File,
    socket_identity: Option<SocketIdentity>,
    bind_attempted: bool,
    renamed: bool,
) -> io::Error {
    let directory_verification = verify_open_directory(generation_dir, generation_directory, true);
    let temporary_cleanup = if bind_attempted && !renamed {
        remove_owned_socket_at_if_present(
            generation_directory,
            ".sock.bind",
            socket_identity,
            false,
        )
    } else {
        Ok(())
    };
    let socket_cleanup = if renamed {
        socket_identity.map_or(Ok(()), |identity| {
            remove_owned_socket_at_if_present(generation_directory, "sock", Some(identity), false)
        })
    } else {
        Ok(())
    };
    let mut failures = Vec::new();
    let mut failure_kind = None;
    for (label, result) in [
        ("generation directory verification", directory_verification),
        ("temporary socket cleanup", temporary_cleanup),
        ("renamed socket cleanup", socket_cleanup),
    ] {
        if let Err(failure) = result {
            failure_kind.get_or_insert(failure.kind());
            failures.push(format!("{label} failed: {failure}"));
        }
    }
    if let Some(kind) = failure_kind {
        io::Error::new(
            kind,
            format!("{error}; endpoint rollback failed: {}", failures.join("; ")),
        )
    } else {
        error
    }
}

fn verify_locked_generation_directory(
    generation_dir: &Path,
    generation_directory: &fs::File,
) -> io::Result<()> {
    verify_open_directory(generation_dir, generation_directory, true)
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
    // Connecting is a read-only operation. If a polling client created this
    // directory, a concurrent daemon startup could observe the umask-derived
    // mode between mkdir and chmod and fail before publishing its socket.
    verify_private(&daemon, DIR_MODE, true)?;
    let locator = read_locator(&daemon)?;
    if locator.state != EndpointState::Active {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "daemon endpoint is draining",
        ));
    }
    let endpoint =
        checked_endpoint(&daemon, &locator).map_err(classify_published_endpoint_error)?;
    #[cfg(test)]
    pause_connect_endpoint(&endpoint);
    UnixStream::connect(endpoint).map_err(classify_published_endpoint_error)
}

fn classify_published_endpoint_error(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::NotFound {
        io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("published daemon endpoint is unavailable: {error}"),
        )
    } else {
        error
    }
}

/// Retires the endpoint named by a stale locator after the caller has excluded
/// every live or replacement daemon with the instance lock.
///
/// The socket is removed before `current.json`, so locator absence is a cleanup
/// commit only when no unidentified generation socket remains. The locator is
/// read and retired while holding `current.lock`; a cooperative replacement
/// therefore cannot be mistaken for the stale generation.
///
/// # Errors
///
/// Returns an error when the locator or socket is unsafe, socket removal fails,
/// or an absent locator is accompanied by a generation socket whose ownership
/// cannot be inferred. The caller must retain the daemon record on error.
pub fn retire_stale_current(data_dir: &Path) -> io::Result<()> {
    let daemon = data_dir.join("daemon");
    ensure_private_dir(&daemon)?;
    let _lock = lock_locator(&daemon)?;
    let locator = match read_locator(&daemon) {
        Ok(locator) => locator,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return remove_recoverable_generation_sockets(&daemon);
        }
        Err(error) => return Err(error),
    };
    endpoint_path(&daemon, &locator)?;
    remove_recoverable_generation_sockets(&daemon)?;

    // `current.lock` excludes every cooperative publish/retire writer. Keep an
    // exact comparison anyway so this function remains fail-closed if its
    // locking contract is accidentally weakened later.
    remove_locator_if_unchanged(&daemon, &locator)
}

fn remove_locator_if_unchanged(daemon: &Path, locator: &EndpointLocator) -> io::Result<()> {
    match read_locator(daemon) {
        Ok(current) if owns_endpoint(&current, locator) => {
            remove_file_if_present(&daemon.join("current.json"))
        }
        Ok(_) => Err(io::Error::other(
            "daemon locator changed during stale endpoint recovery",
        )),
        Err(error) => Err(error),
    }
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

#[cfg(test)]
fn write_locator_unverified(daemon: &Path, locator: &EndpointLocator) -> io::Result<()> {
    write_locator(daemon, locator, || Ok(()))
}

#[coverage(off)]
fn write_locator(
    daemon: &Path,
    locator: &EndpointLocator,
    mut verify_endpoint: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let _lock = lock_locator(daemon)?;
    verify_endpoint()?;
    let target = daemon.join("current.json");
    let bytes = serde_json::to_vec(locator).expect("endpoint locator serializes");
    let old_bytes = read_private_bytes_if_present(&target)?;

    // Keep a private byte-for-byte rollback image rather than hard-linking the
    // old locator. A hard link would transiently violate the nlink == 1 reader
    // invariant and make a still-valid locator undiscoverable during publish.
    let rollback = if let Some(old_bytes) = old_bytes.as_deref() {
        let (path, mut file) = create_private_locator_temp(daemon)?;
        let prepared = file
            .write_all(old_bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| verify_open_private_file(&file, SOCKET_MODE));
        if let Err(error) = prepared {
            return Err(cleanup_owned_temp_error(
                &path,
                error,
                "locator rollback temp",
            ));
        }
        Some(path)
    } else {
        None
    };
    let (temporary, mut file) = match create_private_locator_temp(daemon) {
        Ok(created) => created,
        Err(error) => {
            return Err(cleanup_optional_owned_temp_error(
                rollback.as_deref(),
                error,
                "locator rollback temp",
            ));
        }
    };

    // Once create_new succeeds this writer owns the pathname. Every error
    // before rename removes only its private paths. Errors after rename restore
    // the exact old bytes (or the old absence) before this function returns.
    let mut renamed = false;
    let result = (|| {
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
        // Recheck immediately before publication so a hard link created after
        // the initial fd validation cannot become the live locator.
        verify_open_private_file(&file, SOCKET_MODE)?;

        #[cfg(test)]
        if take_locator_write_failpoint(&target, LocatorWriteStage::Rename) {
            return Err(io::Error::other("injected locator rename failure"));
        }
        verify_endpoint()?;
        fs::rename(&temporary, &target)?;
        renamed = true;
        verify_endpoint()?;

        #[cfg(test)]
        if let Some(replacement) = take_locator_replacement(&target) {
            let (replacement_path, mut replacement_file) = create_private_locator_temp(daemon)?;
            replacement_file.write_all(&replacement)?;
            replacement_file.sync_all()?;
            verify_open_private_file(&replacement_file, SOCKET_MODE)?;
            fs::rename(&replacement_path, &target)?;
            verify_published_file(&target, &replacement_file)?;
            return Err(io::Error::other(
                "injected locator replacement after rename",
            ));
        }

        #[cfg(test)]
        if take_locator_write_failpoint(&target, LocatorWriteStage::HardlinkBeforeFinalVerify) {
            fs::hard_link(&target, daemon.join(LOCATOR_HARDLINK_ALIAS))?;
        }

        #[cfg(test)]
        if take_locator_write_failpoint(&target, LocatorWriteStage::FinalVerify) {
            return Err(io::Error::other(
                "injected locator final verification failure",
            ));
        }
        verify_published_file(&target, &file)?;
        verify_endpoint()?;

        if let Some(rollback) = rollback.as_deref() {
            remove_file_if_present(rollback)?;
        }

        // Final verification is the commit point. Directory fsync improves
        // power-loss durability where supported, but failure after that commit
        // must not report an ambiguous failed publication to the caller.
        sync_locator_parent_best_effort(daemon, &target);
        Ok(())
    })();
    if let Err(error) = result {
        return rollback_locator_write_error(
            &target,
            &file,
            rollback.as_deref(),
            old_bytes.as_deref(),
            &temporary,
            renamed,
            error,
        );
    }
    Ok(())
}

fn sync_locator_parent_best_effort(daemon: &Path, target: &Path) {
    #[cfg(not(test))]
    let _ = target;
    #[cfg(test)]
    let result = if take_locator_write_failpoint(target, LocatorWriteStage::ParentSync) {
        Err(io::Error::other("injected locator parent sync failure"))
    } else {
        fs::File::open(daemon).and_then(|parent| parent.sync_all())
    };
    #[cfg(not(test))]
    let result = fs::File::open(daemon).and_then(|parent| parent.sync_all());
    let _ = result;
}

fn rollback_locator_write_error(
    target: &Path,
    published: &fs::File,
    rollback: Option<&Path>,
    old_bytes: Option<&[u8]>,
    temporary: &Path,
    renamed: bool,
    error: io::Error,
) -> io::Result<()> {
    let restore = if renamed {
        restore_previous_locator(target, published, rollback, old_bytes)
    } else {
        Ok(())
    };
    let temporary_cleanup = remove_file_if_present(temporary);
    let rollback_cleanup = rollback.map_or(Ok(()), remove_file_if_present);
    let mut failures = Vec::new();
    let mut failure_kind = None;
    for (label, result) in [
        ("previous locator restore", restore),
        ("locator temp cleanup", temporary_cleanup),
        ("locator rollback temp cleanup", rollback_cleanup),
    ] {
        if let Err(failure) = result {
            failure_kind.get_or_insert(failure.kind());
            failures.push(format!("{label} failed: {failure}"));
        }
    }
    if let Some(kind) = failure_kind {
        Err(io::Error::new(
            kind,
            format!("{error}; {}", failures.join("; ")),
        ))
    } else {
        Err(error)
    }
}

fn create_private_locator_temp(daemon: &Path) -> io::Result<(PathBuf, fs::File)> {
    loop {
        let path = unique_locator_temp_path(daemon);
        #[cfg(test)]
        if take_locator_write_failpoint(
            &daemon.join("current.json"),
            LocatorWriteStage::CollideTemporaryCreate,
        ) {
            fs::write(&path, b"pre-existing collision")?;
        }
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(SOCKET_MODE)
            .custom_flags(PRIVATE_FILE_FLAGS)
            .open(&path)
        {
            Ok(file) => {
                #[cfg(test)]
                if take_locator_write_failpoint(
                    &daemon.join("current.json"),
                    LocatorWriteStage::HardlinkTemporaryAfterCreate,
                ) {
                    fs::hard_link(&path, daemon.join(LOCATOR_HARDLINK_ALIAS))?;
                }
                match make_open_file_private(&file, SOCKET_MODE) {
                    Ok(()) => return Ok((path, file)),
                    Err(error) => {
                        return Err(cleanup_owned_temp_error(&path, error, "locator temp"));
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn read_private_bytes_if_present(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(PRIVATE_FILE_FLAGS | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    verify_open_private_file(&file, SOCKET_MODE)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

fn verify_published_file(path: &Path, expected: &fs::File) -> io::Result<()> {
    let actual = OpenOptions::new()
        .read(true)
        .custom_flags(PRIVATE_FILE_FLAGS | libc::O_NONBLOCK)
        .open(path)?;
    verify_open_private_file(&actual, SOCKET_MODE)?;
    let expected = expected.metadata()?;
    let actual = actual.metadata()?;
    if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "published locator does not name the prepared inode",
        ));
    }
    Ok(())
}

fn restore_previous_locator(
    target: &Path,
    published: &fs::File,
    rollback: Option<&Path>,
    old_bytes: Option<&[u8]>,
) -> io::Result<()> {
    match (rollback, old_bytes) {
        (Some(rollback), Some(old_bytes)) => {
            if published_path_state(target, published)? == PublishedPathState::Replaced {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing to overwrite a replaced locator during rollback",
                ));
            }
            fs::rename(rollback, target)?;
            verify_restored_locator(read_private_bytes_if_present(target)?, old_bytes)?;
            sync_parent_best_effort(target);
            Ok(())
        }
        (None, None) => match published_path_state(target, published)? {
            PublishedPathState::Absent => Ok(()),
            PublishedPathState::Owned => {
                fs::remove_file(target)?;
                sync_parent_best_effort(target);
                Ok(())
            }
            PublishedPathState::Replaced => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to remove a replaced locator during rollback",
            )),
        },
        _ => Err(io::Error::other(
            "incomplete previous locator rollback state",
        )),
    }
}

fn verify_restored_locator(restored: Option<Vec<u8>>, old_bytes: &[u8]) -> io::Result<()> {
    let Some(restored) = restored else {
        return Err(io::Error::other(
            "restored locator disappeared before verification",
        ));
    };
    if restored != old_bytes {
        return Err(io::Error::other(
            "restored locator bytes do not match the previous publication",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublishedPathState {
    Absent,
    Owned,
    Replaced,
}

fn published_path_state(target: &Path, published: &fs::File) -> io::Result<PublishedPathState> {
    let metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PublishedPathState::Absent);
        }
        Err(error) => return Err(error),
    };
    let published = published.metadata()?;
    if !metadata.file_type().is_symlink()
        && metadata.dev() == published.dev()
        && metadata.ino() == published.ino()
    {
        Ok(PublishedPathState::Owned)
    } else {
        Ok(PublishedPathState::Replaced)
    }
}

fn sync_parent_best_effort(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(directory) = fs::File::open(parent)
    {
        let _ = directory.sync_all();
    }
}

fn cleanup_owned_temp_error(path: &Path, error: io::Error, label: &str) -> io::Error {
    match remove_file_if_present(path) {
        Ok(()) => error,
        Err(cleanup) => io::Error::new(
            cleanup.kind(),
            format!("{error}; {label} cleanup failed: {cleanup}"),
        ),
    }
}

fn cleanup_optional_owned_temp_error(
    path: Option<&Path>,
    error: io::Error,
    label: &str,
) -> io::Error {
    match path {
        Some(path) => cleanup_owned_temp_error(path, error, label),
        None => error,
    }
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
    // The directory fd is a bootstrap lock for creating or repairing the lock
    // file itself.
    let directory = lock_setup_directory(daemon, true)?;
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(SOCKET_MODE)
        .custom_flags(PRIVATE_FILE_FLAGS)
        .open(&path)
    {
        Ok(file) => {
            #[cfg(test)]
            crash_if_requested(LOCATOR_LOCK_CRASH_PATH, &path, 86);
            make_open_file_private(&file, SOCKET_MODE)?;
            file
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            open_recoverable_locator_lock(&path)?
        }
        Err(error) => return Err(error),
    };
    // Never wait for the inode lock while holding the setup-directory lock.
    // Stale retirement holds the inode lock before repairing generation
    // directories, so retaining both in the opposite order would deadlock.
    drop(directory);
    #[cfg(test)]
    pause_locator_lock_after_setup();
    FileExt::lock_exclusive(&file)?;
    #[cfg(test)]
    pause_locator_lock_before_verify();
    verify_locked_private_file(&path, &file, SOCKET_MODE)?;
    Ok(file)
}

fn open_recoverable_locator_lock(path: &Path) -> io::Result<fs::File> {
    let before = fs::symlink_metadata(path)?;
    verify_recoverable_lock_metadata(&before)?;
    let open = || {
        #[cfg(test)]
        if take_locator_lock_open_error() {
            return Err(io::Error::other("injected locator lock open failure"));
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(PRIVATE_FILE_FLAGS)
            .open(path)
    };
    let file = match open() {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            // A restrictive umask can remove either owner access bit before
            // the creating process reaches fchmod. The trusted-directory lock
            // excludes cooperative replacement while this pathname repair is
            // needed to obtain an fd on portable Unix.
            fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE))?;
            open()?
        }
        Err(error) => return Err(error),
    };
    let opened = file.metadata()?;
    verify_same_inode(&before, &opened, "daemon locator lock")?;
    make_open_file_private(&file, SOCKET_MODE)?;
    Ok(file)
}

fn verify_recoverable_lock_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    // open(2) was requested with 0600, so a crash before fchmod can only have
    // removed bits from 0600. Refuse broader modes rather than normalizing an
    // unrelated pre-existing file.
    let permissions = metadata.mode() & 0o7777;
    if !metadata.is_file()
        || metadata.uid() != effective_uid()
        || metadata.nlink() != 1
        || permissions & !SOCKET_MODE != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe daemon locator lock crash residue",
        ));
    }
    Ok(())
}

fn verify_locked_private_file(path: &Path, file: &fs::File, mode: u32) -> io::Result<()> {
    verify_open_private_file(file, mode)?;
    verify_close_on_exec(file)?;
    let path_metadata = fs::symlink_metadata(path)?;
    let opened = file.metadata()?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || path_metadata.uid() != effective_uid()
        || path_metadata.nlink() != 1
        || path_metadata.mode() & 0o7777 != mode
        || path_metadata.dev() != opened.dev()
        || path_metadata.ino() != opened.ino()
    {
        return Err(unsafe_path_replacement("daemon locator lock"));
    }
    Ok(())
}

fn verify_close_on_exec(file: &fs::File) -> io::Result<()> {
    verify_close_on_exec_flags(descriptor_flags(file.as_raw_fd())?)
}

fn descriptor_flags(fd: i32) -> io::Result<i32> {
    // SAFETY: F_GETFD has no memory-safety preconditions; an invalid descriptor
    // is reported through errno.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags)
    }
}

fn verify_close_on_exec_flags(flags: i32) -> io::Result<()> {
    if flags & libc::FD_CLOEXEC != 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "daemon lock fd is inherited across exec",
        ))
    }
}

fn unsafe_path_replacement(label: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{label} pathname does not name its locked inode"),
    )
}

fn verify_same_inode(
    expected: &fs::Metadata,
    actual: &fs::Metadata,
    label: &str,
) -> io::Result<()> {
    if expected.dev() == actual.dev() && expected.ino() == actual.ino() {
        Ok(())
    } else {
        Err(unsafe_path_replacement(label))
    }
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
        || metadata.mode() & 0o7777 != mode
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
    let endpoint = endpoint_path(daemon, locator)?;
    verify_private(&endpoint, SOCKET_MODE, false)?;
    Ok(endpoint)
}

fn endpoint_path(daemon: &Path, locator: &EndpointLocator) -> io::Result<PathBuf> {
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
    Ok(endpoint)
}

#[cfg(test)]
fn remove_owned_socket_if_present(path: &Path) -> io::Result<()> {
    remove_unidentified_owned_socket_if_present(path, true)
}

#[cfg(test)]
fn remove_unidentified_owned_socket_if_present(
    path: &Path,
    exact_private_mode: bool,
) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    verify_owned_socket_metadata(&metadata, exact_private_mode)?;
    fs::remove_file(path)
}

// libc's stat field widths differ between supported Unix targets.
#[allow(
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::unnecessary_cast
)]
fn socket_identity_from_stat(metadata: &libc::stat) -> SocketIdentity {
    SocketIdentity {
        // Unix device IDs are non-negative. libc exposes dev_t as signed on
        // macOS and unsigned on Linux, while MetadataExt normalizes it to u64.
        dev: metadata.st_dev as u64,
        ino: metadata.st_ino,
        uid: metadata.st_uid,
        nlink: metadata.st_nlink as u64,
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=bind_rejects_replaced_socket_nodes_without_unlinking_the_replacement
fn socket_stat_at(directory: &fs::File, name: &str) -> io::Result<Option<libc::stat>> {
    let name = CString::new(name).expect("fixed socket basename contains no NUL");
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `metadata` points to writable storage for one stat structure,
    // the directory fd is live, and `name` is NUL-terminated for this call.
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        // SAFETY: successful fstatat initialized the complete structure.
        Ok(Some(unsafe { metadata.assume_init() }))
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

// libc's mode_t width differs between supported Unix targets.
#[allow(clippy::cast_lossless, clippy::unnecessary_cast)]
fn verify_owned_socket_stat(
    metadata: &libc::stat,
    expected: Option<SocketIdentity>,
    exact_private_mode: bool,
) -> io::Result<()> {
    let mode = metadata.st_mode as u32;
    let permissions = mode & 0o7777;
    let identity = socket_identity_from_stat(metadata);
    if mode & libc::S_IFMT as u32 != libc::S_IFSOCK as u32
        || identity.uid != effective_uid()
        || identity.nlink != 1
        || (exact_private_mode && permissions != SOCKET_MODE)
        || (!exact_private_mode && permissions & 0o7000 != 0)
        || expected.is_some_and(|expected| expected != identity)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe, replaced, or multiply-linked daemon socket",
        ));
    }
    Ok(())
}

fn capture_owned_socket_identity_at(
    directory: &fs::File,
    name: &str,
) -> io::Result<SocketIdentity> {
    let Some(metadata) = socket_stat_at(directory, name)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "bound socket node disappeared",
        ));
    };
    verify_owned_socket_stat(&metadata, None, false)?;
    Ok(socket_identity_from_stat(&metadata))
}

fn verify_owned_socket_identity_at(
    directory: &fs::File,
    name: &str,
    expected: SocketIdentity,
    exact_private_mode: bool,
) -> io::Result<()> {
    let Some(metadata) = socket_stat_at(directory, name)? else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "owned socket node disappeared",
        ));
    };
    verify_owned_socket_stat(&metadata, Some(expected), exact_private_mode)
}

fn remove_owned_socket_at_if_present(
    directory: &fs::File,
    name: &str,
    expected: Option<SocketIdentity>,
    exact_private_mode: bool,
) -> io::Result<()> {
    let Some(metadata) = socket_stat_at(directory, name)? else {
        return Ok(());
    };
    verify_owned_socket_stat(&metadata, expected, exact_private_mode)?;
    unlink_socket_at(directory, name)
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=retirement_is_generation_fenced_and_removes_only_the_owned_endpoint
fn unlink_socket_at(directory: &fs::File, name: &str) -> io::Result<()> {
    let name = CString::new(name).expect("fixed socket basename contains no NUL");
    // SAFETY: the directory fd is live and `name` is a valid NUL-terminated
    // relative basename. Flags zero request unlink of a non-directory entry.
    let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[allow(clippy::cast_possible_truncation)]
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=restrictive_umask_still_publishes_an_exact_private_regular_locator
fn set_socket_permissions_at(directory: &fs::File, name: &str, mode: u32) -> io::Result<()> {
    let name = CString::new(name).expect("fixed socket basename contains no NUL");
    // Only ordinary Unix permission bits are accepted by callers, so this is
    // lossless even where mode_t is narrower than u32.
    let mode = mode as libc::mode_t;
    // SAFETY: the directory fd is live, `name` is NUL-terminated, and mode is
    // restricted to ordinary Unix permission bits.
    let result = unsafe { libc::fchmodat(directory.as_raw_fd(), name.as_ptr(), mode, 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn rename_socket_noreplace_at(directory: &fs::File, from: &str, to: &str) -> io::Result<()> {
    let from = CString::new(from).expect("fixed socket basename contains no NUL");
    let to = CString::new(to).expect("fixed socket basename contains no NUL");
    // SAFETY: both names are valid relative basenames and the same live
    // directory fd supplies the source and destination namespace. The
    // platform-specific exclusive flag makes destination non-replacement
    // atomic with the rename.
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let result = unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
fn verify_owned_socket_metadata(
    metadata: &fs::Metadata,
    exact_private_mode: bool,
) -> io::Result<()> {
    let permissions = metadata.mode() & 0o7777;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_socket()
        || metadata.uid() != effective_uid()
        || metadata.nlink() != 1
        || (exact_private_mode && permissions != SOCKET_MODE)
        || (!exact_private_mode && permissions & 0o7000 != 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe, replaced, or multiply-linked daemon socket",
        ));
    }
    Ok(())
}

fn remove_recoverable_generation_sockets(daemon: &Path) -> io::Result<()> {
    let generations = daemon.join("generations");
    match fs::symlink_metadata(&generations) {
        Ok(_) => ensure_private_dir(&generations)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let root_guard = lock_setup_directory(&generations, true)?;
    let entries = fs::read_dir(&generations)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    verify_open_directory(&generations, &root_guard, true)?;
    for generation in entries {
        verify_open_directory(&generations, &root_guard, true)?;
        #[cfg(test)]
        pause_generation_root_recheck(&generation);
        verify_open_directory(&generations, &root_guard, true)?;
        // The parent flock is already held, so repair the entry directly
        // instead of recursively trying to acquire the same setup lock.
        create_or_repair_private_directory(&generation)?;
        verify_open_directory(&generations, &root_guard, true)?;
        let child_guard = lock_setup_directory(&generation, true)?;
        verify_open_directory(&generations, &root_guard, true)?;
        #[cfg(test)]
        pause_generation_scan(&generation);
        verify_open_directory(&generation, &child_guard, true)?;
        let mut sockets = Vec::new();
        for name in ["sock", ".sock.bind"] {
            verify_open_directory(&generation, &child_guard, true)?;
            if let Some(metadata) = socket_stat_at(&child_guard, name)? {
                // `.sock.bind` may still have a restrictive-umask mode when
                // chmod itself failed. Its type, owner, single link, and
                // private parent directory are sufficient recovery proof.
                let exact_private_mode = name == "sock";
                verify_owned_socket_stat(&metadata, None, exact_private_mode)?;
                sockets.push((
                    name,
                    socket_identity_from_stat(&metadata),
                    exact_private_mode,
                ));
            }
        }
        for (name, identity, exact_private_mode) in sockets {
            #[cfg(test)]
            pause_generation_unlink(&generation.join(name));
            verify_open_directory(&generations, &root_guard, true)?;
            verify_open_directory(&generation, &child_guard, true)?;
            remove_owned_socket_at_if_present(
                &child_guard,
                name,
                Some(identity),
                exact_private_mode,
            )?;
            verify_open_directory(&generations, &root_guard, true)?;
            verify_open_directory(&generation, &child_guard, true)?;
        }
    }
    verify_open_directory(&generations, &root_guard, true)
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
    if path.as_os_str().as_bytes().contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private directory path contains NUL",
        ));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    repair_crashed_private_parent(parent)?;
    let parent_directory = lock_setup_directory(parent, false)?;
    create_or_repair_private_directory(path)?;
    verify_open_directory(parent, &parent_directory, false)
}

/// Creates every missing component of a private data-directory chain.
///
/// Traversal starts at the deepest existing directory that is either owned by
/// the effective user and not group/world-writable, or is an exact root-owned
/// sticky temporary directory. Every newly managed component is requested as
/// `0700`; crash residues whose mode is a subset of `0700` are repaired before
/// traversal continues.
///
/// # Errors
///
/// Returns an error for an unsafe anchor/component, a symlinked managed
/// component, or a filesystem failure.
pub fn ensure_private_dir_all(path: &Path) -> io::Result<()> {
    if path.as_os_str().as_bytes().contains(&0)
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private directory chain contains an unsafe component",
        ));
    }
    let requested = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    verify_private_chain_prefixes(&requested)?;
    let (mut current, components, mut anchor) = private_chain_anchor(&requested)?;
    for component in components.into_iter().rev() {
        current.push(component);
        match anchor {
            PrivateChainAnchor::Owned => ensure_private_dir(&current)?,
            PrivateChainAnchor::RootSticky => {
                create_or_repair_private_directory(&current)?;
            }
        }
        anchor = PrivateChainAnchor::Owned;
    }
    verify_private(&requested, DIR_MODE, true)?;
    verify_private_chain_prefixes(&requested)
}

fn verify_private_chain_prefixes(path: &Path) -> io::Result<()> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        match fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                // macOS exposes `/tmp` as a root-owned symlink to the exact
                // root-owned 01777 directory `/private/tmp`. Preserve that
                // trusted system anchor while rejecting every user-controlled
                // intermediate redirect.
                verify_private_chain_anchor(&prefix, &metadata)?;
            }
            Ok(metadata) => {
                let permissions = metadata.mode() & 0o7777;
                let trusted_owner =
                    metadata.uid() == effective_uid() && permissions & (0o7000 | 0o022) == 0;
                let trusted_root = metadata.uid() == 0
                    && (permissions == 0o1777 || permissions & (0o7000 | 0o022) == 0);
                if !metadata.is_dir() || (!trusted_owner && !trusted_root) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "private directory chain contains an unsafe parent",
                    ));
                }
            }
            Err(error) => {
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) {
                    break;
                }
                return Err(error);
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum PrivateChainAnchor {
    Owned,
    RootSticky,
}

fn private_chain_anchor(
    requested: &Path,
) -> io::Result<(PathBuf, Vec<std::ffi::OsString>, PrivateChainAnchor)> {
    let mut cursor = requested.to_path_buf();
    let mut components = Vec::new();
    loop {
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                let permissions = metadata.mode() & 0o7777;
                if !metadata.file_type().is_symlink()
                    && metadata.is_dir()
                    && metadata.uid() == effective_uid()
                    && permissions != DIR_MODE
                    && permissions & !DIR_MODE == 0
                {
                    push_private_chain_component(&mut cursor, &mut components)?;
                    continue;
                }
                let (canonical, anchor) = verify_private_chain_anchor(&cursor, &metadata)?;
                return Ok((canonical, components, anchor));
            }
            Err(error) => {
                if !matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) {
                    return Err(error);
                }
                push_private_chain_component(&mut cursor, &mut components)?;
            }
        }
    }
}

fn push_private_chain_component(
    cursor: &mut PathBuf,
    components: &mut Vec<std::ffi::OsString>,
) -> io::Result<()> {
    let component = cursor.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private directory chain has no trusted anchor",
        )
    })?;
    components.push(component.to_os_string());
    cursor.pop();
    Ok(())
}

fn verify_private_chain_anchor(
    path: &Path,
    path_metadata: &fs::Metadata,
) -> io::Result<(PathBuf, PrivateChainAnchor)> {
    let canonical = fs::canonicalize(path)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | PRIVATE_FILE_FLAGS)
        .open(&canonical)?;
    let metadata = directory.metadata()?;
    let permissions = metadata.mode() & 0o7777;
    let anchor = if metadata.is_dir()
        && metadata.uid() == effective_uid()
        && permissions & (0o7000 | 0o022) == 0
        && permissions & 0o100 != 0
        && !path_metadata.file_type().is_symlink()
    {
        PrivateChainAnchor::Owned
    } else if metadata.is_dir()
        && metadata.uid() == 0
        && permissions == 0o1777
        && (!path_metadata.file_type().is_symlink() || path_metadata.uid() == 0)
    {
        PrivateChainAnchor::RootSticky
    } else {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private directory chain has an unsafe anchor",
        ));
    };
    verify_close_on_exec(&directory)?;
    #[cfg(test)]
    pause_private_chain_anchor_recheck(&canonical);
    let canonical_metadata = fs::symlink_metadata(&canonical)?;
    for observed in (!path_metadata.file_type().is_symlink())
        .then_some(path_metadata)
        .into_iter()
        .chain(std::iter::once(&canonical_metadata))
    {
        verify_same_inode(observed, &metadata, "private directory chain anchor")?;
    }
    Ok((canonical, anchor))
}

fn create_or_repair_private_directory(path: &Path) -> io::Result<()> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(DIR_MODE);
            match builder.create(path) {
                Ok(()) => {}
                // A non-cooperating same-UID creator may ignore the advisory
                // parent lock. Re-inspection below still requires one exact,
                // owned directory inode.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            #[cfg(test)]
            crash_if_requested(PRIVATE_DIR_CRASH_PATH, path, 87);
            fs::symlink_metadata(path)?
        }
        Err(error) => return Err(error),
    };
    repair_private_directory(path, &before)
}

fn repair_private_directory(path: &Path, before: &fs::Metadata) -> io::Result<()> {
    let permissions = before.mode() & 0o7777;
    if before.file_type().is_symlink()
        || !before.is_dir()
        || before.uid() != effective_uid()
        || permissions & !DIR_MODE != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe private directory crash residue",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(DIR_MODE))?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | PRIVATE_FILE_FLAGS)
        .open(path)?;
    let opened = directory.metadata()?;
    verify_same_inode(before, &opened, "private directory")?;
    directory.set_permissions(fs::Permissions::from_mode(DIR_MODE))?;
    verify_open_directory(path, &directory, true)
}

fn repair_crashed_private_parent(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let permissions = metadata.mode() & 0o7777;
    if !metadata.file_type().is_symlink()
        && metadata.is_dir()
        && metadata.uid() == effective_uid()
        && permissions != DIR_MODE
        && permissions & !DIR_MODE == 0
    {
        ensure_private_dir(path)?;
    }
    Ok(())
}

fn lock_setup_directory(path: &Path, exact_private_mode: bool) -> io::Result<fs::File> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | PRIVATE_FILE_FLAGS)
        .open(path)?;
    verify_open_directory(path, &directory, exact_private_mode)?;
    FileExt::lock_exclusive(&directory)?;
    verify_open_directory(path, &directory, exact_private_mode)?;
    Ok(directory)
}

fn verify_open_directory(
    path: &Path,
    directory: &fs::File,
    exact_private_mode: bool,
) -> io::Result<()> {
    let opened = directory.metadata()?;
    let path_metadata = fs::symlink_metadata(path)?;
    let permissions = opened.mode() & 0o7777;
    if !opened.is_dir()
        || opened.uid() != effective_uid()
        || (exact_private_mode && permissions != DIR_MODE)
        || (!exact_private_mode && permissions & (0o7000 | 0o022) != 0)
        || path_metadata.file_type().is_symlink()
        || !path_metadata.is_dir()
        || path_metadata.uid() != effective_uid()
        || path_metadata.mode() & 0o7777 != permissions
        || path_metadata.dev() != opened.dev()
        || path_metadata.ino() != opened.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe or replaced private directory",
        ));
    }
    verify_close_on_exec(directory)
}

fn verify_private(path: &Path, mode: u32, directory: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o7777 != mode
        || (directory && !metadata.is_dir())
        || (!directory && (!metadata.file_type().is_socket() || metadata.nlink() != 1))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe daemon endpoint ownership, type, mode, or link count",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[coverage(off)]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    linux_peer_credentials(stream).map(|credential| credential.uid)
}

#[cfg(target_os = "linux")]
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=unix_peer_pid_contract
fn linux_peer_credentials(stream: &UnixStream) -> io::Result<libc::ucred> {
    use std::os::fd::AsRawFd;
    let mut credential = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut size = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
        .map_err(|_| io::Error::other("ucred size does not fit socklen_t"))?;
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
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if size as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned an invalid credential size",
        ));
    }
    Ok(credential)
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

/// Returns the OS-authenticated PID of an established Unix-stream peer.
///
/// # Errors
///
/// Returns an error when the platform cannot provide a valid positive PID.
#[cfg(target_os = "linux")]
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=unix_peer_pid_contract
pub fn peer_pid(stream: &UnixStream) -> io::Result<u32> {
    validated_peer_pid(linux_peer_credentials(stream)?.pid)
}

/// Returns the OS-authenticated PID of an established Unix-stream peer.
///
/// # Errors
///
/// Returns an error when the platform cannot provide a valid positive PID.
#[cfg(target_os = "macos")]
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=unix_peer_pid_contract
pub fn peer_pid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut size = libc::socklen_t::try_from(std::mem::size_of::<libc::pid_t>())
        .map_err(|_| io::Error::other("pid_t size does not fit socklen_t"))?;
    // SAFETY: `pid` is initialized writable storage sized by `size`; the fd
    // belongs to the established stream for this call.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &raw mut size,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if size as usize != std::mem::size_of::<libc::pid_t>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LOCAL_PEERPID returned an invalid PID size",
        ));
    }
    validated_peer_pid(pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[coverage(off)]
pub fn peer_pid(_stream: &UnixStream) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer PID credentials unavailable",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn validated_peer_pid(pid: libc::pid_t) -> io::Result<u32> {
    let pid = u32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "peer PID is negative"))?;
    if !(2..=i32::MAX.cast_unsigned()).contains(&pid) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer PID is not a single-process target",
        ));
    }
    Ok(pid)
}

#[coverage(off)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_pid_validation_rejects_non_process_targets() {
        for invalid in [-1, 0, 1] {
            assert!(validated_peer_pid(invalid).is_err());
        }
        assert_eq!(validated_peer_pid(2).unwrap(), 2);
    }
    use std::os::unix::ffi::OsStringExt;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    const UMASK_TEST_CHILD: &str = "USAGI_LOCATOR_UMASK_TEST_CHILD";

    #[derive(Clone, Copy)]
    enum CrashChild {
        LocatorLock,
        PrivateDirectory,
        PrivateChain,
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=private_directory_chain_recovers_an_inaccessible_intermediate_component
    fn run_crash_child_if_requested(child: CrashChild) {
        let (variable, operation): (&str, fn(&Path) -> io::Result<()>) = match child {
            CrashChild::LocatorLock => (LOCATOR_LOCK_CRASH_PATH, |path| {
                lock_locator(path.parent().unwrap()).map(drop)
            }),
            CrashChild::PrivateDirectory => (PRIVATE_DIR_CRASH_PATH, ensure_private_dir),
            CrashChild::PrivateChain => (PRIVATE_CHAIN_CRASH_TARGET, ensure_private_dir_all),
        };
        let Some(path) = std::env::var_os(variable) else {
            return;
        };
        // SAFETY: an exact-test child has no sibling application threads and
        // deliberately exits at the requested filesystem creation failpoint.
        unsafe { libc::umask(0o777) };
        let _unexpected = operation(&PathBuf::from(path));
        panic!("filesystem creation failpoint did not exit");
    }

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

    fn replace_with_private_socket(path: &Path) -> UnixListener {
        fs::remove_file(path).unwrap();
        let replacement = UnixListener::bind(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        replacement
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
        let old_socket = old.cleanup.socket.clone();
        let old_locator = old.locator().clone();

        let mut replacement = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let replacement_cleanup = replacement.cleanup_handle();
        let replacement_socket = replacement.cleanup.socket.clone();
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
        replacement_cleanup.retire().unwrap();
        assert_eq!(
            connect_current(temp.path()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn retirement_rejects_a_replaced_generation_directory_and_remains_retryable() {
        use std::mem::ManuallyDrop;

        let temp = TempDir::new_in("/tmp").unwrap();
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(temp.path(), generation()).unwrap());
        let cleanup = listener.cleanup_handle();
        let generation_dir = cleanup.socket.parent().unwrap().to_path_buf();
        let displaced = cleanup.daemon.join("displaced-owned-generation");
        let displaced_socket = displaced.join("sock");
        let current = cleanup.daemon.join("current.json");

        fs::rename(&generation_dir, &displaced).unwrap();
        ensure_private_dir(&generation_dir).unwrap();

        assert_eq!(
            cleanup.retire().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(displaced_socket.exists());
        assert!(current.exists());
        assert!(!generation_dir.join("sock").exists());

        fs::remove_dir(&generation_dir).unwrap();
        fs::rename(&displaced, &generation_dir).unwrap();
        cleanup.retire().unwrap();
        assert!(!generation_dir.join("sock").exists());
        assert!(!current.exists());
        // SAFETY: the listener was not moved or dropped; cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn connecting_before_startup_does_not_create_the_daemon_directory() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");

        assert_eq!(
            connect_current(temp.path()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(!daemon.exists());
    }

    #[test]
    fn locator_absence_remains_not_found_but_published_endpoint_absence_is_unavailable() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();

        assert_eq!(
            connect_current(temp.path()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        let published = locator();
        write_locator_unverified(&daemon, &published).unwrap();
        assert_eq!(
            connect_current(temp.path()).unwrap_err().kind(),
            io::ErrorKind::ConnectionRefused
        );
        assert_eq!(read_locator(&daemon).unwrap(), published);
    }

    #[test]
    fn endpoint_disappearance_after_validation_is_classified_as_unavailable() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let listener = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let socket = listener.cleanup.socket.clone();
        let current = listener.cleanup.daemon.join("current.json");
        let ready = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker = {
            let data_dir = temp.path().to_path_buf();
            let socket = socket.clone();
            let ready = Arc::clone(&ready);
            let resume = Arc::clone(&resume);
            std::thread::spawn(move || {
                pause_next_connect_endpoint(&socket, ready, resume);
                connect_current(&data_dir)
            })
        };

        ready.wait();
        fs::remove_file(&socket).unwrap();
        resume.wait();

        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::ConnectionRefused
        );
        assert!(current.exists());
        drop(listener);
        assert!(!current.exists());
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
            BindStage::CaptureIdentity,
            BindStage::SetTemporaryPermissions,
            BindStage::RenameEndpoint,
            BindStage::VerifyEndpoint,
            BindStage::SetNonblocking,
            BindStage::VerifyPublication,
        ] {
            let temp = TempDir::new_in("/tmp").unwrap();
            let old = SecureUnixListener::bind(temp.path(), generation()).unwrap();
            let daemon = temp.path().join("daemon");
            let target = daemon.join("current.json");
            let old_bytes = fs::read(&target).unwrap();
            let old_locator = old.locator().clone();
            let generation = generation();
            let generation_dir = temp.path().join("daemon/generations").join(&generation.0);

            let result = SecureUnixListener::bind_with(temp.path(), generation.clone(), |stage| {
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
            assert_eq!(fs::read(&target).unwrap(), old_bytes);
            assert_eq!(read_locator(&daemon).unwrap(), old_locator);
            let client = connect_current(temp.path()).unwrap();
            let accepted = old.accept().unwrap();
            drop((client, accepted));

            let retry = SecureUnixListener::bind(temp.path(), generation).unwrap();
            assert_eq!(read_locator(&daemon).unwrap(), *retry.locator());
        }
    }

    #[test]
    fn bind_failure_after_temporary_socket_disappears_preserves_old_locator() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let old = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let daemon = temp.path().join("daemon");
        let current = daemon.join("current.json");
        let old_bytes = fs::read(&current).unwrap();
        let replacement_generation = generation();
        let replacement_dir = daemon.join("generations").join(&replacement_generation.0);
        let temporary = replacement_dir.join(".sock.bind");

        let error =
            SecureUnixListener::bind_with(temp.path(), replacement_generation.clone(), |stage| {
                assert_eq!(stage, BindStage::CaptureIdentity);
                fs::remove_file(&temporary)?;
                Ok(())
            })
            .err()
            .expect("a disappeared temporary socket must fail publication");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!temporary.exists());
        assert!(!replacement_dir.join("sock").exists());
        assert_eq!(fs::read(&current).unwrap(), old_bytes);
        let client = connect_current(temp.path()).unwrap();
        let accepted = old.accept().unwrap();
        drop((client, accepted));

        let retry = SecureUnixListener::bind(temp.path(), replacement_generation).unwrap();
        assert_eq!(read_locator(&daemon).unwrap(), *retry.locator());
    }

    #[test]
    fn bind_rejects_replaced_socket_nodes_without_unlinking_the_replacement() {
        for replacement_stage in [
            BindStage::RenameEndpoint,
            BindStage::VerifyEndpoint,
            BindStage::VerifyPublication,
        ] {
            let temp = TempDir::new_in("/tmp").unwrap();
            let old = SecureUnixListener::bind(temp.path(), generation()).unwrap();
            let daemon = temp.path().join("daemon");
            let current = daemon.join("current.json");
            let old_bytes = fs::read(&current).unwrap();
            let replacement_generation = generation();
            let generation_dir = daemon.join("generations").join(&replacement_generation.0);
            let replacement_path = if replacement_stage == BindStage::RenameEndpoint {
                generation_dir.join(".sock.bind")
            } else {
                generation_dir.join("sock")
            };
            let mut replacement_listener = None;

            let error =
                SecureUnixListener::bind_with(temp.path(), replacement_generation, |stage| {
                    if stage == replacement_stage {
                        replacement_listener = Some(replace_with_private_socket(&replacement_path));
                    }
                    Ok(())
                })
                .err()
                .expect("a replaced socket pathname must fail closed");

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(replacement_listener.is_some());
            assert!(replacement_path.exists());
            assert_eq!(fs::read(&current).unwrap(), old_bytes);
            let client = connect_current(temp.path()).unwrap();
            let accepted = old.accept().unwrap();
            drop((client, accepted));

            drop(replacement_listener);
            fs::remove_file(replacement_path).unwrap();
        }
    }

    #[test]
    fn bind_preserves_a_dangling_destination_symlink_before_and_during_rename() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let old = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let daemon = temp.path().join("daemon");
        let current = daemon.join("current.json");
        let old_bytes = fs::read(&current).unwrap();
        let dangling_target = temp.path().join("missing-socket-target");

        let existing_generation = generation();
        let existing_dir = daemon.join("generations").join(&existing_generation.0);
        ensure_private_dir(&existing_dir).unwrap();
        let existing_socket = existing_dir.join("sock");
        std::os::unix::fs::symlink(&dangling_target, &existing_socket).unwrap();
        assert_eq!(
            SecureUnixListener::bind(temp.path(), existing_generation)
                .err()
                .expect("a pre-existing dangling endpoint must be rejected")
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read_link(&existing_socket).unwrap(), dangling_target);
        fs::remove_file(&existing_socket).unwrap();

        let racing_generation = generation();
        let racing_dir = daemon.join("generations").join(&racing_generation.0);
        let racing_socket = racing_dir.join("sock");
        let error = SecureUnixListener::bind_with(temp.path(), racing_generation, |stage| {
            if stage == BindStage::RenameEndpoint {
                std::os::unix::fs::symlink(&dangling_target, &racing_socket)?;
            }
            Ok(())
        })
        .err()
        .expect("an endpoint inserted before rename must be preserved");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_link(&racing_socket).unwrap(), dangling_target);
        assert!(!racing_dir.join(".sock.bind").exists());
        assert_eq!(fs::read(&current).unwrap(), old_bytes);
        let client = connect_current(temp.path()).unwrap();
        let accepted = old.accept().unwrap();
        drop((client, accepted));
        fs::remove_file(racing_socket).unwrap();
    }

    #[test]
    fn bind_rollback_uses_the_retained_generation_directory_after_path_replacement() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let old = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let daemon = temp.path().join("daemon");
        let current = daemon.join("current.json");
        let old_bytes = fs::read(&current).unwrap();
        let replacement_generation = generation();
        let generation_dir = daemon.join("generations").join(&replacement_generation.0);
        let displaced = daemon.join("displaced-bind-generation");
        let outside_generation = temp.path().join("outside-bind-generation");
        ensure_private_dir(&outside_generation).unwrap();
        let outside_socket = outside_generation.join("sock");
        let outside_listener = UnixListener::bind(&outside_socket).unwrap();
        fs::set_permissions(&outside_socket, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();

        let error = SecureUnixListener::bind_with(temp.path(), replacement_generation, |stage| {
            if stage == BindStage::VerifyPublication {
                fs::rename(&generation_dir, &displaced)?;
                std::os::unix::fs::symlink(&outside_generation, &generation_dir)?;
            }
            Ok(())
        })
        .err()
        .expect("a replaced generation pathname must fail closed");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(outside_socket.exists());
        assert!(!displaced.join(".sock.bind").exists());
        assert!(!displaced.join("sock").exists());
        assert_eq!(fs::read(&current).unwrap(), old_bytes);
        let client = connect_current(temp.path()).unwrap();
        let accepted = old.accept().unwrap();
        drop((client, accepted));

        fs::remove_file(&generation_dir).unwrap();
        fs::rename(&displaced, &generation_dir).unwrap();
        drop(outside_listener);
        fs::remove_file(outside_socket).unwrap();
    }

    #[test]
    fn stale_cleanup_cannot_leave_a_missing_socket_published_by_a_waiting_bind() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let old = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let data_dir = temp.path().to_path_buf();
        let stale_locked = Arc::new(Barrier::new(2));
        let stale_resume = Arc::new(Barrier::new(2));
        let stale = {
            let data_dir = data_dir.clone();
            let locked = Arc::clone(&stale_locked);
            let resume = Arc::clone(&stale_resume);
            std::thread::spawn(move || {
                pause_next_locator_lock_before_verify(locked, resume);
                retire_stale_current(&data_dir)
            })
        };
        stale_locked.wait();

        let replacement_generation = generation();
        let replacement_socket = data_dir
            .join("daemon/generations")
            .join(&replacement_generation.0)
            .join("sock");
        let endpoint_ready = Arc::new(Barrier::new(2));
        let endpoint_resume = Arc::new(Barrier::new(2));
        let publisher = {
            let data_dir = data_dir.clone();
            let ready = Arc::clone(&endpoint_ready);
            let resume = Arc::clone(&endpoint_resume);
            std::thread::spawn(move || {
                SecureUnixListener::bind_with(&data_dir, replacement_generation, |stage| {
                    if stage == BindStage::SetNonblocking {
                        ready.wait();
                        resume.wait();
                    }
                    Ok(())
                })
            })
        };
        endpoint_ready.wait();
        endpoint_resume.wait();
        stale_resume.wait();

        stale.join().unwrap().unwrap();
        assert!(publisher.join().unwrap().is_err());
        assert!(!replacement_socket.exists());
        assert!(!data_dir.join("daemon/current.json").exists());
        drop(old);
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
    fn unique_locator_temps_skip_collisions_and_reject_hardlinks() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let current = daemon.join("current.json");

        fail_next_locator_write(&current, LocatorWriteStage::CollideTemporaryCreate);
        let first = locator();
        write_locator_unverified(&daemon, &first).unwrap();
        let collisions = locator_temp_names(&daemon);
        assert_eq!(collisions.len(), 1);
        let collision = daemon.join(&collisions[0]);
        assert_eq!(fs::read(&collision).unwrap(), b"pre-existing collision");
        fs::remove_file(collision).unwrap();

        fs::remove_file(&current).unwrap();
        fail_next_locator_write(&current, LocatorWriteStage::HardlinkTemporaryAfterCreate);
        assert_eq!(
            write_locator_unverified(&daemon, &locator())
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!current.exists());
        assert!(locator_temp_names(&daemon).is_empty());
        let hardlink = daemon.join(LOCATOR_HARDLINK_ALIAS);
        assert!(hardlink.exists());
        assert_eq!(fs::metadata(&hardlink).unwrap().nlink(), 1);
        fs::remove_file(hardlink).unwrap();

        assert_eq!(
            create_private_locator_temp(Path::new("invalid\0daemon"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn locator_failures_preserve_old_publication_cleanup_temp_and_allow_retry() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let target = daemon.join("current.json");
        let mut current = locator();
        write_locator_unverified(&daemon, &current).unwrap();

        for stage in [
            LocatorWriteStage::Write,
            LocatorWriteStage::Sync,
            LocatorWriteStage::Rename,
            LocatorWriteStage::FinalVerify,
        ] {
            let old_bytes = fs::read(&target).unwrap();
            let replacement = locator();
            fail_next_locator_write(&target, stage);

            let error = write_locator_unverified(&daemon, &replacement).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Other, "stage: {stage:?}");
            assert_eq!(fs::read(&target).unwrap(), old_bytes, "stage: {stage:?}");
            assert_eq!(read_locator(&daemon).unwrap(), current, "stage: {stage:?}");
            assert!(locator_temp_names(&daemon).is_empty(), "stage: {stage:?}");

            write_locator_unverified(&daemon, &replacement).unwrap();
            assert_eq!(read_locator(&daemon).unwrap(), replacement);
            assert!(
                locator_temp_names(&daemon).is_empty(),
                "retry after: {stage:?}"
            );
            current = replacement;
        }
    }

    #[test]
    fn first_locator_final_verify_failure_restores_absence_and_allows_retry() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let target = daemon.join("current.json");
        let locator = locator();
        fail_next_locator_write(&target, LocatorWriteStage::FinalVerify);

        let error = write_locator_unverified(&daemon, &locator).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(!target.exists());
        assert!(locator_temp_names(&daemon).is_empty());

        write_locator_unverified(&daemon, &locator).unwrap();
        assert_eq!(read_locator(&daemon).unwrap(), locator);
        assert!(locator_temp_names(&daemon).is_empty());
    }

    #[test]
    fn locator_temp_cleanup_helpers_preserve_primary_errors_and_report_cleanup_failure() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let owned = temp.path().join("owned.tmp");
        fs::write(&owned, b"owned").unwrap();
        let error = cleanup_owned_temp_error(&owned, io::Error::other("primary"), "owned temp");
        assert_eq!(error.to_string(), "primary");
        assert!(!owned.exists());

        let optional = temp.path().join("optional.tmp");
        fs::write(&optional, b"owned").unwrap();
        let error = cleanup_optional_owned_temp_error(
            Some(&optional),
            io::Error::other("optional primary"),
            "optional temp",
        );
        assert_eq!(error.to_string(), "optional primary");
        assert!(!optional.exists());

        let error =
            cleanup_optional_owned_temp_error(None, io::Error::other("no temp"), "optional temp");
        assert_eq!(error.to_string(), "no temp");

        let directory = temp.path().join("not-removable-as-file");
        fs::create_dir(&directory).unwrap();
        let error = cleanup_owned_temp_error(&directory, io::Error::other("primary"), "owned temp");
        assert!(error.to_string().contains("owned temp cleanup failed"));
        assert!(directory.is_dir());
    }

    #[test]
    fn locator_verification_and_rollback_reject_mismatched_inodes_and_state() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let expected_path = temp.path().join("expected");
        let actual_path = temp.path().join("actual");
        fs::write(&expected_path, b"expected").unwrap();
        fs::write(&actual_path, b"actual").unwrap();
        fs::set_permissions(&expected_path, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        fs::set_permissions(&actual_path, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        let expected = fs::File::open(&expected_path).unwrap();
        let expected_metadata = fs::metadata(&expected_path).unwrap();
        let actual_metadata = fs::metadata(&actual_path).unwrap();
        let invalid_path = PathBuf::from(std::ffi::OsString::from_vec(b"invalid\0path".to_vec()));

        verify_same_inode(&expected_metadata, &expected_metadata, "test inode").unwrap();
        assert_eq!(
            verify_same_inode(&expected_metadata, &actual_metadata, "test inode")
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            published_path_state(&invalid_path, &expected)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        sync_parent_best_effort(Path::new(""));

        assert_eq!(
            verify_published_file(&actual_path, &expected)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            restore_previous_locator(&actual_path, &expected, None, Some(b"old"))
                .unwrap_err()
                .to_string(),
            "incomplete previous locator rollback state"
        );
        let replaced_rollback = temp.path().join("replaced-rollback");
        fs::write(&replaced_rollback, b"old").unwrap();
        fs::set_permissions(&replaced_rollback, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        assert!(
            restore_previous_locator(
                &actual_path,
                &expected,
                Some(&replaced_rollback),
                Some(b"old"),
            )
            .unwrap_err()
            .to_string()
            .contains("refusing to overwrite a replaced locator")
        );
        assert!(
            restore_previous_locator(&actual_path, &expected, None, None)
                .unwrap_err()
                .to_string()
                .contains("refusing to remove a replaced locator")
        );
        assert_eq!(
            verify_restored_locator(None, b"old")
                .unwrap_err()
                .to_string(),
            "restored locator disappeared before verification"
        );
        assert_eq!(
            verify_restored_locator(Some(b"different".to_vec()), b"old")
                .unwrap_err()
                .to_string(),
            "restored locator bytes do not match the previous publication"
        );
        verify_restored_locator(Some(b"old".to_vec()), b"old").unwrap();

        fs::remove_file(&actual_path).unwrap();
        let rollback = temp.path().join("rollback");
        fs::write(&rollback, b"old").unwrap();
        fs::set_permissions(&rollback, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        restore_previous_locator(&actual_path, &expected, Some(&rollback), Some(b"old")).unwrap();
        assert_eq!(fs::read(&actual_path).unwrap(), b"old");
        fs::remove_file(&actual_path).unwrap();
        restore_previous_locator(&actual_path, &expected, None, None).unwrap();
        assert!(!actual_path.exists());
    }

    #[test]
    fn bind_locator_failures_preserve_old_connection_and_cleanup_owned_artifacts() {
        for stage in [
            LocatorWriteStage::Write,
            LocatorWriteStage::Sync,
            LocatorWriteStage::Rename,
            LocatorWriteStage::FinalVerify,
        ] {
            let temp = TempDir::new_in("/tmp").unwrap();
            let old = SecureUnixListener::bind(temp.path(), generation()).unwrap();
            let daemon = temp.path().join("daemon");
            let target = daemon.join("current.json");
            let old_bytes = fs::read(&target).unwrap();
            let old_locator = old.locator().clone();
            let replacement_generation = generation();
            let replacement_dir = daemon.join("generations").join(&replacement_generation.0);
            fail_next_locator_write(&target, stage);

            let error = SecureUnixListener::bind(temp.path(), replacement_generation.clone())
                .err()
                .expect("injected locator publication failure unexpectedly succeeded");

            assert!(error.to_string().contains("injected locator"));
            assert_eq!(fs::read(&target).unwrap(), old_bytes, "stage: {stage:?}");
            assert_eq!(
                read_locator(&daemon).unwrap(),
                old_locator,
                "stage: {stage:?}"
            );
            assert!(!replacement_dir.join(".sock.bind").exists());
            assert!(!replacement_dir.join("sock").exists());
            assert!(locator_temp_names(&daemon).is_empty(), "stage: {stage:?}");
            let client = connect_current(temp.path()).unwrap();
            let accepted = old.accept().unwrap();
            drop((client, accepted));

            let retry = SecureUnixListener::bind(temp.path(), replacement_generation).unwrap();
            assert_eq!(read_locator(&daemon).unwrap(), *retry.locator());
        }
    }

    #[test]
    fn final_verification_preserves_a_replacement_locator_and_cleans_failed_generation() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let replacement = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let daemon = temp.path().join("daemon");
        let target = daemon.join("current.json");
        let replacement_locator = replacement.locator().clone();
        let replacement_socket = daemon.join(&replacement_locator.endpoint);
        let attempted_generation = generation();
        let attempted_dir = daemon.join("generations").join(&attempted_generation.0);
        replace_locator_after_rename(&target, &replacement_locator);

        let error = SecureUnixListener::bind(temp.path(), attempted_generation.clone())
            .err()
            .expect("replacement failpoint unexpectedly published");

        assert!(
            error
                .to_string()
                .contains("refusing to overwrite a replaced locator")
        );
        assert_eq!(read_locator(&daemon).unwrap(), replacement_locator);
        assert!(replacement_socket.exists());
        assert!(!attempted_dir.join(".sock.bind").exists());
        assert!(!attempted_dir.join("sock").exists());
        assert!(locator_temp_names(&daemon).is_empty());
        let client = connect_current(temp.path()).unwrap();
        let accepted = replacement.accept().unwrap();
        drop((client, accepted));

        let retry = SecureUnixListener::bind(temp.path(), attempted_generation).unwrap();
        assert_eq!(read_locator(&daemon).unwrap(), *retry.locator());
    }

    #[test]
    fn final_verification_rejects_a_hardlinked_publish_and_restores_old_locator() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let old = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let daemon = temp.path().join("daemon");
        let target = daemon.join("current.json");
        let old_locator = old.locator().clone();
        let attempted_generation = generation();
        let attempted_dir = daemon.join("generations").join(&attempted_generation.0);
        fail_next_locator_write(&target, LocatorWriteStage::HardlinkBeforeFinalVerify);

        let error = SecureUnixListener::bind(temp.path(), attempted_generation.clone())
            .err()
            .expect("hardlinked publication unexpectedly succeeded");

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(read_locator(&daemon).unwrap(), old_locator);
        assert!(!attempted_dir.join(".sock.bind").exists());
        assert!(!attempted_dir.join("sock").exists());
        assert!(locator_temp_names(&daemon).is_empty());
        let client = connect_current(temp.path()).unwrap();
        let accepted = old.accept().unwrap();
        drop((client, accepted));

        fs::remove_file(daemon.join(LOCATOR_HARDLINK_ALIAS)).unwrap();
        let retry = SecureUnixListener::bind(temp.path(), attempted_generation).unwrap();
        assert_eq!(read_locator(&daemon).unwrap(), *retry.locator());
    }

    #[test]
    fn parent_sync_failure_is_best_effort_after_locator_commit() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let target = daemon.join("current.json");
        let locator = locator();
        fail_next_locator_write(&target, LocatorWriteStage::ParentSync);

        write_locator_unverified(&daemon, &locator).unwrap();

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
            assert_eq!(metadata.nlink(), 1);
            assert_eq!(read_locator(&daemon)?, *listener.locator());
            assert!(locator_temp_names(&daemon).is_empty());
            Ok::<_, io::Error>(())
        })();
        // SAFETY: restores the value returned by the paired umask call above.
        unsafe { libc::umask(original_umask) };
        result.unwrap();
    }

    #[test]
    fn locator_lock_creation_crash_under_restrictive_umask_is_repaired_on_reopen() {
        run_crash_child_if_requested(CrashChild::LocatorLock);

        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let lock_path = daemon.join(LOCATOR_LOCK);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "infrastructure::unix_transport::tests::locator_lock_creation_crash_under_restrictive_umask_is_repaired_on_reopen",
                "--nocapture",
            ])
            .env(LOCATOR_LOCK_CRASH_PATH, &lock_path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86));
        let residue = fs::symlink_metadata(&lock_path).unwrap();
        assert!(residue.is_file());
        assert_eq!(residue.uid(), effective_uid());
        assert_eq!(residue.mode() & 0o777, 0);
        assert_eq!(residue.nlink(), 1);

        let repaired = lock_locator(&daemon).unwrap();
        verify_locked_private_file(&lock_path, &repaired, SOCKET_MODE).unwrap();
        assert!(descriptor_flags(-1).is_err());
        assert_eq!(
            verify_close_on_exec_flags(0).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        verify_close_on_exec_flags(libc::FD_CLOEXEC).unwrap();
        assert_eq!(
            fs::metadata(&lock_path).unwrap().mode() & 0o777,
            SOCKET_MODE
        );
        drop(repaired);
        fail_next_locator_lock_open();
        assert_eq!(
            open_recoverable_locator_lock(&lock_path)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Other
        );
        let reopened = lock_locator(&daemon).unwrap();
        verify_locked_private_file(&lock_path, &reopened, SOCKET_MODE).unwrap();
    }

    #[test]
    fn private_directory_creation_crash_is_repaired_and_first_boot_is_serialized() {
        run_crash_child_if_requested(CrashChild::PrivateDirectory);

        let temp = TempDir::new_in("/tmp").unwrap();
        let crashed = temp.path().join("crashed");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "infrastructure::unix_transport::tests::private_directory_creation_crash_is_repaired_and_first_boot_is_serialized",
                "--nocapture",
            ])
            .env(PRIVATE_DIR_CRASH_PATH, &crashed)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(87));
        let residue = fs::symlink_metadata(&crashed).unwrap();
        assert!(residue.is_dir());
        assert_eq!(residue.uid(), effective_uid());
        assert_eq!(residue.mode() & 0o777, 0);
        let nested = crashed.join("daemon");
        ensure_private_dir(&nested).unwrap();
        assert_eq!(fs::metadata(&crashed).unwrap().mode() & 0o777, DIR_MODE);
        assert_eq!(fs::metadata(&nested).unwrap().mode() & 0o777, DIR_MODE);

        let simultaneous = temp.path().join("simultaneous");
        let barrier = Arc::new(Barrier::new(3));
        std::thread::scope(|scope| {
            let first = {
                let barrier = Arc::clone(&barrier);
                let simultaneous = simultaneous.clone();
                scope.spawn(move || {
                    barrier.wait();
                    ensure_private_dir(&simultaneous)
                })
            };
            let second = {
                let barrier = Arc::clone(&barrier);
                let simultaneous = simultaneous.clone();
                scope.spawn(move || {
                    barrier.wait();
                    ensure_private_dir(&simultaneous)
                })
            };
            barrier.wait();
            first.join().unwrap().unwrap();
            second.join().unwrap().unwrap();
        });
        assert_eq!(
            fs::metadata(&simultaneous).unwrap().mode() & 0o777,
            DIR_MODE
        );
    }

    #[test]
    fn private_directory_chain_recovers_an_inaccessible_intermediate_component() {
        run_crash_child_if_requested(CrashChild::PrivateChain);

        let temp = TempDir::new_in("/tmp").unwrap();
        let intermediate = temp.path().join("channel");
        let canonical_intermediate = fs::canonicalize(temp.path()).unwrap().join("channel");
        let target = intermediate.join("local/daemon");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "infrastructure::unix_transport::tests::private_directory_chain_recovers_an_inaccessible_intermediate_component",
                "--nocapture",
            ])
            .env(PRIVATE_CHAIN_CRASH_TARGET, &target)
            .env(PRIVATE_DIR_CRASH_PATH, &canonical_intermediate)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(87));
        assert_eq!(fs::metadata(&intermediate).unwrap().mode() & 0o777, 0);
        assert!(!intermediate.join("local").exists());

        ensure_private_dir_all(&target).unwrap();
        for path in [
            intermediate.clone(),
            intermediate.join("local"),
            target.clone(),
        ] {
            let metadata = fs::metadata(path).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.uid(), effective_uid());
            assert_eq!(metadata.mode() & 0o777, DIR_MODE);
        }
    }

    #[test]
    fn private_directory_chain_can_start_at_the_root_owned_sticky_tmp_anchor() {
        let path = Path::new("/tmp").join(format!(
            ".usagi-private-chain-{}-{}",
            std::process::id(),
            generation().0
        ));
        assert!(!path.exists());

        ensure_private_dir_all(&path).unwrap();

        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.uid(), effective_uid());
        assert_eq!(metadata.mode() & 0o777, DIR_MODE);
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn private_directory_chain_rejects_an_anchor_replaced_after_open() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let anchor = temp.path().join("anchor");
        ensure_private_dir(&anchor).unwrap();
        let canonical = fs::canonicalize(&anchor).unwrap();
        let displaced = temp.path().join("displaced");
        let ready = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker = {
            let anchor = anchor.clone();
            let canonical = canonical.clone();
            let ready = Arc::clone(&ready);
            let resume = Arc::clone(&resume);
            std::thread::spawn(move || {
                pause_next_private_chain_anchor_recheck(&canonical, ready, resume);
                ensure_private_dir_all(&anchor)
            })
        };

        ready.wait();
        fs::rename(&anchor, &displaced).unwrap();
        ensure_private_dir(&anchor).unwrap();
        resume.wait();

        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_ne!(
            fs::metadata(anchor).unwrap().ino(),
            fs::metadata(displaced).unwrap().ino()
        );
    }

    #[test]
    fn private_directory_chain_rejects_an_existing_intermediate_symlink() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let managed = temp.path().join("managed");
        let outside = temp.path().join("outside");
        ensure_private_dir(&managed).unwrap();
        ensure_private_dir(&outside).unwrap();
        let existing = outside.join("existing");
        ensure_private_dir(&existing).unwrap();
        let redirect = managed.join("redirect");
        std::os::unix::fs::symlink(&outside, &redirect).unwrap();

        assert_eq!(
            ensure_private_dir_all(&redirect.join("existing"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs::symlink_metadata(&redirect).unwrap().uid(),
            effective_uid()
        );
        assert_eq!(fs::metadata(&existing).unwrap().mode() & 0o7777, DIR_MODE);

        let file = managed.join("file");
        fs::write(&file, []).unwrap();
        assert_eq!(
            ensure_private_dir_all(&file.join("child"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!file.join("child").exists());
    }

    #[test]
    fn locator_lock_rejects_path_replacement_after_open_and_hardlinks() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let path = daemon.join(LOCATOR_LOCK);
        drop(lock_locator(&daemon).unwrap());

        let locked = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let writer = {
            let daemon = daemon.clone();
            let locked_for_writer = Arc::clone(&locked);
            let resume_for_writer = Arc::clone(&resume);
            std::thread::spawn(move || {
                pause_next_locator_lock_before_verify(locked_for_writer, resume_for_writer);
                lock_locator(&daemon)
            })
        };
        locked.wait();
        let displaced = daemon.join("displaced.lock");
        fs::rename(&path, &displaced).unwrap();
        let replacement = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(SOCKET_MODE)
            .custom_flags(PRIVATE_FILE_FLAGS)
            .open(&path)
            .unwrap();
        make_open_file_private(&replacement, SOCKET_MODE).unwrap();
        assert_ne!(
            fs::metadata(&path).unwrap().ino(),
            fs::metadata(&displaced).unwrap().ino()
        );
        resume.wait();
        let error = writer.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("locked inode"));
        drop(replacement);
        drop(lock_locator(&daemon).unwrap());

        fs::remove_file(displaced).unwrap();
        let alias = daemon.join("current.lock.alias");
        fs::hard_link(&path, &alias).unwrap();
        assert_eq!(
            lock_locator(&daemon).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::remove_file(alias).unwrap();
        drop(lock_locator(&daemon).unwrap());

        fs::remove_file(&path).unwrap();
        let target = daemon.join("lock-target");
        fs::write(&target, b"outside").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(lock_locator(&daemon).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"outside");
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, SOCKET_MODE);
    }

    #[test]
    fn locator_lock_releases_setup_directory_before_waiting_on_current() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let held = lock_locator(&daemon).unwrap();
        let setup_ready = Arc::new(Barrier::new(2));
        let setup_resume = Arc::new(Barrier::new(2));
        let writer = {
            let daemon = daemon.clone();
            let setup_ready_for_writer = Arc::clone(&setup_ready);
            let setup_resume_for_writer = Arc::clone(&setup_resume);
            std::thread::spawn(move || {
                pause_next_locator_lock_after_setup(
                    setup_ready_for_writer,
                    setup_resume_for_writer,
                );
                lock_locator(&daemon)
            })
        };
        setup_ready.wait();

        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | PRIVATE_FILE_FLAGS)
            .open(&daemon)
            .unwrap();
        let setup_lock_result = FileExt::try_lock_exclusive(&directory);
        if setup_lock_result.is_ok() {
            FileExt::unlock(&directory).unwrap();
        }
        setup_resume.wait();
        drop(held);
        drop(writer.join().unwrap().unwrap());

        setup_lock_result.expect("current.lock waiter retained the daemon-directory setup lock");
        let generations = daemon.join("generations");
        ensure_private_dir(&generations).unwrap();
        assert_eq!(fs::metadata(generations).unwrap().mode() & 0o777, DIR_MODE);
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
                    write_locator_unverified(&daemon, &first)
                })
            };
            let second_writer = {
                let barrier = Arc::clone(&barrier);
                let daemon = daemon.clone();
                let second = second.clone();
                scope.spawn(move || {
                    barrier.wait();
                    write_locator_unverified(&daemon, &second)
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
        assert!(write_locator_unverified(&daemon, &locator()).is_err());
        assert!(locator_temp_names(&daemon).is_empty());

        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        assert_eq!(
            read_locator(&daemon).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn private_path_verification_rejects_special_mode_bits() {
        use std::mem::ManuallyDrop;

        let temp = TempDir::new_in("/tmp").unwrap();
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(temp.path(), generation()).unwrap());
        let cleanup = listener.cleanup_handle();
        let daemon = temp.path().join("daemon");
        let current = daemon.join("current.json");
        fs::set_permissions(&current, fs::Permissions::from_mode(0o1600)).unwrap();
        assert_eq!(
            read_locator(&daemon).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::metadata(&current).unwrap().mode() & 0o7777, 0o1600);
        fs::set_permissions(&current, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();

        let socket = cleanup.socket.clone();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o1600)).unwrap();
        assert_eq!(
            remove_owned_socket_if_present(&socket).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::metadata(&socket).unwrap().mode() & 0o7777, 0o1600);
        fs::set_permissions(&socket, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();

        let socket_alias = daemon.join("socket.alias");
        fs::hard_link(&socket, &socket_alias).unwrap();
        assert_eq!(
            cleanup.retire().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(socket.exists());
        assert!(current.exists());
        fs::remove_file(socket_alias).unwrap();

        let special_directory = temp.path().join("special-directory");
        fs::create_dir(&special_directory).unwrap();
        fs::set_permissions(&special_directory, fs::Permissions::from_mode(0o1700)).unwrap();
        assert_eq!(
            ensure_private_dir(&special_directory).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            verify_private(&special_directory, DIR_MODE, true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs::metadata(&special_directory).unwrap().mode() & 0o7777,
            0o1700
        );

        cleanup.retire().unwrap();
        // SAFETY: the listener was not moved or dropped; cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn hardlinked_locator_is_rejected_and_retirement_remains_retryable() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let mut listener = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let daemon = temp.path().join("daemon");
        let current = daemon.join("current.json");
        let alias = daemon.join("current.alias");
        let socket = listener.cleanup.socket.clone();
        fs::hard_link(&current, &alias).unwrap();
        assert_eq!(fs::metadata(&current).unwrap().nlink(), 2);

        assert_eq!(
            read_locator(&daemon).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            listener.retire().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(
            !socket.exists(),
            "socket must be removed before locator commit"
        );
        assert!(
            current.exists(),
            "unsafe locator remains as a failure fence"
        );

        fs::remove_file(alias).unwrap();
        listener.retire().unwrap();
        assert!(!current.exists());
    }

    #[test]
    fn endpoint_cleanup_rejects_an_unsafe_socket_node_and_remains_retryable() {
        use std::mem::ManuallyDrop;

        let temp = TempDir::new_in("/tmp").unwrap();
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(temp.path(), generation()).unwrap());
        let cleanup = listener.cleanup_handle();
        let daemon = temp.path().join("daemon");
        let socket = cleanup.socket.clone();
        fs::remove_file(&socket).unwrap();
        fs::write(&socket, b"not a socket").unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();

        assert_eq!(
            connect_current(temp.path()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            cleanup.retire().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::read(&socket).unwrap(), b"not a socket");
        assert!(daemon.join("current.json").exists());

        fs::remove_file(&socket).unwrap();
        cleanup.retire().unwrap();
        assert!(!daemon.join("current.json").exists());
        assert_eq!(
            remove_owned_socket_if_present(Path::new("invalid\0socket"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        // SAFETY: the listener was not moved or dropped; cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn endpoint_cleanup_preserves_a_replacement_socket_and_locator_fence() {
        use std::mem::ManuallyDrop;

        let temp = TempDir::new_in("/tmp").unwrap();
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(temp.path(), generation()).unwrap());
        let cleanup = listener.cleanup_handle();
        let current = cleanup.daemon.join("current.json");
        let replacement = replace_with_private_socket(&cleanup.socket);

        assert_eq!(
            cleanup.retire().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(cleanup.socket.exists());
        assert!(
            current.exists(),
            "locator remains as a cleanup failure fence"
        );

        drop(replacement);
        fs::remove_file(&cleanup.socket).unwrap();
        cleanup.retire().unwrap();
        assert!(!current.exists());
        // SAFETY: the listener was not moved or dropped; cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn stale_recovery_accepts_missing_locator_and_generations_as_proven_absence() {
        let temp = TempDir::new_in("/tmp").unwrap();

        retire_stale_current(temp.path()).unwrap();

        let daemon = temp.path().join("daemon");
        assert!(!daemon.join("current.json").exists());
        assert!(!daemon.join("generations").exists());
    }

    #[test]
    fn stale_recovery_keeps_invalid_or_changed_locators_as_failure_fences() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let current_path = daemon.join("current.json");
        fs::write(&current_path, b"not json").unwrap();
        fs::set_permissions(&current_path, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        assert_eq!(
            retire_stale_current(temp.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(current_path.exists());

        fs::remove_file(&current_path).unwrap();
        let current = locator();
        write_locator_unverified(&daemon, &current).unwrap();
        let different = locator();
        assert_eq!(
            remove_locator_if_unchanged(&daemon, &different)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Other
        );
        assert_eq!(read_locator(&daemon).unwrap(), current);

        fs::set_permissions(&current_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            remove_locator_if_unchanged(&daemon, &current)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(current_path.exists());
    }

    #[test]
    fn stale_recovery_removes_current_socket_and_safe_bind_orphans() {
        use std::mem::ManuallyDrop;

        let temp = TempDir::new_in("/tmp").unwrap();
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(temp.path(), generation()).unwrap());
        let daemon = temp.path().join("daemon");
        let socket = listener.cleanup.socket.clone();
        let orphan_generation = daemon.join("generations").join(generation().0);
        ensure_private_dir(&orphan_generation).unwrap();
        let orphan = orphan_generation.join(".sock.bind");
        let orphan_listener = UnixListener::bind(&orphan).unwrap();
        drop(orphan_listener);
        fs::set_permissions(&orphan, fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(&orphan_generation, fs::Permissions::from_mode(0o000)).unwrap();
        let generations = daemon.join("generations");
        fs::set_permissions(&generations, fs::Permissions::from_mode(0o000)).unwrap();

        retire_stale_current(temp.path()).unwrap();

        assert!(!daemon.join("current.json").exists());
        assert!(!socket.exists());
        assert!(!orphan.exists());
        assert_eq!(fs::metadata(&generations).unwrap().mode() & 0o777, DIR_MODE);
        assert_eq!(
            fs::metadata(&orphan_generation).unwrap().mode() & 0o777,
            DIR_MODE
        );
        // SAFETY: the value has not otherwise been dropped or moved. Its Drop
        // observes already-retired filesystem state and only closes the fd.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn stale_recovery_preserves_locator_when_generation_scan_becomes_inaccessible() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let listener = SecureUnixListener::bind(temp.path(), generation()).unwrap();
        let daemon = temp.path().join("daemon");
        let current = daemon.join("current.json");
        let socket = listener.cleanup.socket.clone();
        let generation_dir = socket.parent().unwrap().to_path_buf();
        let ready = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker = {
            let data_dir = temp.path().to_path_buf();
            let generation_dir = generation_dir.clone();
            let ready = Arc::clone(&ready);
            let resume = Arc::clone(&resume);
            std::thread::spawn(move || {
                pause_next_generation_scan(&generation_dir, ready, resume);
                retire_stale_current(&data_dir)
            })
        };

        ready.wait();
        fs::set_permissions(&generation_dir, fs::Permissions::from_mode(0o000)).unwrap();
        resume.wait();
        let error = worker.join().unwrap().unwrap_err();
        fs::set_permissions(&generation_dir, fs::Permissions::from_mode(DIR_MODE)).unwrap();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(socket.exists());
        assert!(current.exists());
        drop(listener);
    }

    #[test]
    fn stale_recovery_preserves_a_socket_replaced_after_scan() {
        use std::mem::ManuallyDrop;

        let temp = TempDir::new_in("/tmp").unwrap();
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(temp.path(), generation()).unwrap());
        let cleanup = listener.cleanup_handle();
        let socket = cleanup.socket.clone();
        let current = cleanup.daemon.join("current.json");
        let ready = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker = {
            let data_dir = temp.path().to_path_buf();
            let socket = socket.clone();
            let ready = Arc::clone(&ready);
            let resume = Arc::clone(&resume);
            std::thread::spawn(move || {
                pause_next_generation_unlink(&socket, ready, resume);
                retire_stale_current(&data_dir)
            })
        };

        ready.wait();
        let replacement = replace_with_private_socket(&socket);
        resume.wait();

        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(socket.exists());
        assert!(current.exists());
        drop(replacement);
        fs::remove_file(&socket).unwrap();
        cleanup.retire().unwrap();
        // SAFETY: the listener was not moved or dropped; cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn stale_recovery_rejects_a_generation_directory_replaced_by_a_symlink() {
        use std::mem::ManuallyDrop;

        let temp = TempDir::new_in("/tmp").unwrap();
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(temp.path(), generation()).unwrap());
        let cleanup = listener.cleanup_handle();
        let socket = cleanup.socket.clone();
        let generation_dir = socket.parent().unwrap().to_path_buf();
        let current = cleanup.daemon.join("current.json");
        let outside_generation = temp.path().join("outside-generation");
        ensure_private_dir(&outside_generation).unwrap();
        let outside_socket = outside_generation.join("sock");
        let outside_listener = UnixListener::bind(&outside_socket).unwrap();
        fs::set_permissions(&outside_socket, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        let displaced = cleanup.daemon.join("displaced-generation");
        let ready = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker = {
            let data_dir = temp.path().to_path_buf();
            let generation_dir = generation_dir.clone();
            let ready = Arc::clone(&ready);
            let resume = Arc::clone(&resume);
            std::thread::spawn(move || {
                pause_next_generation_scan(&generation_dir, ready, resume);
                retire_stale_current(&data_dir)
            })
        };

        ready.wait();
        fs::rename(&generation_dir, &displaced).unwrap();
        std::os::unix::fs::symlink(&outside_generation, &generation_dir).unwrap();
        resume.wait();

        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(outside_socket.exists());
        assert!(current.exists());

        fs::remove_file(&generation_dir).unwrap();
        fs::rename(&displaced, &generation_dir).unwrap();
        drop(outside_listener);
        fs::remove_file(outside_socket).unwrap();
        cleanup.retire().unwrap();
        // SAFETY: the listener was not moved or dropped; cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn stale_recovery_rejects_a_replaced_generations_root_before_child_open() {
        use std::mem::ManuallyDrop;

        let temp = TempDir::new_in("/tmp").unwrap();
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(temp.path(), generation()).unwrap());
        let cleanup = listener.cleanup_handle();
        let generation_dir = cleanup.socket.parent().unwrap().to_path_buf();
        let generation_name = generation_dir.file_name().unwrap();
        let generations = generation_dir.parent().unwrap().to_path_buf();
        let displaced_generations = cleanup.daemon.join("displaced-generations-root");
        let displaced_socket = displaced_generations.join(generation_name).join("sock");
        let outside_generations = temp.path().join("outside-generations-root");
        ensure_private_dir(&outside_generations).unwrap();
        let outside_generation = outside_generations.join(generation_name);
        ensure_private_dir(&outside_generation).unwrap();
        let outside_socket = outside_generation.join("sock");
        let outside_listener = UnixListener::bind(&outside_socket).unwrap();
        fs::set_permissions(&outside_socket, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        let current = cleanup.daemon.join("current.json");
        let ready = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker = {
            let data_dir = temp.path().to_path_buf();
            let generation_dir = generation_dir.clone();
            let ready = Arc::clone(&ready);
            let resume = Arc::clone(&resume);
            std::thread::spawn(move || {
                pause_next_generation_root_recheck(&generation_dir, ready, resume);
                retire_stale_current(&data_dir)
            })
        };

        ready.wait();
        fs::rename(&generations, &displaced_generations).unwrap();
        std::os::unix::fs::symlink(&outside_generations, &generations).unwrap();
        resume.wait();

        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(outside_socket.exists());
        assert!(displaced_socket.exists());
        assert!(current.exists());

        fs::remove_file(&generations).unwrap();
        fs::rename(&displaced_generations, &generations).unwrap();
        drop(outside_listener);
        fs::remove_file(outside_socket).unwrap();
        cleanup.retire().unwrap();
        // SAFETY: the listener was not moved or dropped; cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn absent_locator_recovery_rejects_an_unsafe_generation_node() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let generations = daemon.join("generations");
        ensure_private_dir(&generations).unwrap();
        let generation = generations.join(generation().0);
        ensure_private_dir(&generation).unwrap();
        let unsafe_socket = generation.join("sock");
        fs::write(&unsafe_socket, b"not a socket").unwrap();
        fs::set_permissions(&unsafe_socket, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();

        assert_eq!(
            retire_stale_current(temp.path()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::read(&unsafe_socket).unwrap(), b"not a socket");
    }

    #[test]
    fn stale_recovery_rejects_a_symlinked_generations_root_without_touching_outside_socket() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let outside = temp.path().join("outside-generations");
        ensure_private_dir(&outside).unwrap();
        let outside_generation = outside.join(generation().0);
        ensure_private_dir(&outside_generation).unwrap();
        let outside_socket = outside_generation.join("sock");
        let listener = UnixListener::bind(&outside_socket).unwrap();
        fs::set_permissions(&outside_socket, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        std::os::unix::fs::symlink(&outside, daemon.join("generations")).unwrap();

        assert_eq!(
            retire_stale_current(temp.path()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(outside_socket.exists());
        drop(listener);
    }

    #[test]
    fn stale_recovery_rejects_a_broad_generations_directory_without_normalizing_it() {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let generations = daemon.join("generations");
        fs::create_dir(&generations).unwrap();
        fs::set_permissions(&generations, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            retire_stale_current(temp.path()).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::metadata(&generations).unwrap().mode() & 0o777, 0o755);
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
    }

    #[test]
    fn private_directory_chain_rejects_unsafe_anchors_and_inputs() {
        let clean = TempDir::new_in("/tmp").unwrap();
        let broad = clean.path().join("broad-directory");
        fs::create_dir(&broad).unwrap();
        fs::set_permissions(&broad, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            ensure_private_dir(&broad).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::metadata(&broad).unwrap().mode() & 0o777, 0o755);
        let broad_fd = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | PRIVATE_FILE_FLAGS)
            .open(&broad)
            .unwrap();
        assert_eq!(
            verify_open_directory(&broad, &broad_fd, true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let unsafe_anchor = clean.path().join("unsafe-anchor");
        fs::create_dir(&unsafe_anchor).unwrap();
        let existing_child = unsafe_anchor.join("existing");
        fs::create_dir(&existing_child).unwrap();
        fs::set_permissions(&existing_child, fs::Permissions::from_mode(DIR_MODE)).unwrap();
        fs::set_permissions(&unsafe_anchor, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            ensure_private_dir_all(&unsafe_anchor.join("child"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!unsafe_anchor.join("child").exists());
        assert_eq!(
            ensure_private_dir_all(&existing_child).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(fs::metadata(&unsafe_anchor).unwrap().mode() & 0o7777, 0o777);

        let outside_chain = clean.path().join("outside-chain");
        ensure_private_dir(&outside_chain).unwrap();
        let chain_link = clean.path().join("chain-link");
        std::os::unix::fs::symlink(&outside_chain, &chain_link).unwrap();
        assert_eq!(
            ensure_private_dir_all(&chain_link.join("child"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(!outside_chain.join("child").exists());
        assert_eq!(
            ensure_private_dir_all(Path::new("../unsafe-chain"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let _ = ensure_private_dir_all(Path::new(""));
        assert!(ensure_private_dir(Path::new("")).is_err());
        assert_eq!(
            push_private_chain_component(&mut PathBuf::from("/"), &mut Vec::new())
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let invalid = PathBuf::from(std::ffi::OsString::from_vec(b"bad\0path".to_vec()));
        assert_eq!(
            ensure_private_dir(&invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            create_or_repair_private_directory(&invalid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            remove_recoverable_generation_sockets(&invalid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            private_chain_anchor(&invalid).err().unwrap().kind(),
            io::ErrorKind::InvalidInput
        );
        let overlong = clean.path().join("x".repeat(1024));
        let error = verify_private_chain_prefixes(&overlong).unwrap_err();
        assert!(!matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn private_cleanup_helpers_propagate_permission_errors() {
        let clean = TempDir::new_in("/tmp").unwrap();
        let unwritable = clean.path().join("unwritable");
        ensure_private_dir(&unwritable).unwrap();
        fs::set_permissions(&unwritable, fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(
            create_or_repair_private_directory(&unwritable.join("child"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(&unwritable, fs::Permissions::from_mode(DIR_MODE)).unwrap();

        let socket_parent = clean.path().join("socket-parent");
        ensure_private_dir(&socket_parent).unwrap();
        let protected_socket = socket_parent.join("sock");
        let protected_listener = UnixListener::bind(&protected_socket).unwrap();
        fs::set_permissions(&protected_socket, fs::Permissions::from_mode(SOCKET_MODE)).unwrap();
        fs::set_permissions(&socket_parent, fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(
            remove_owned_socket_if_present(&protected_socket)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(&socket_parent, fs::Permissions::from_mode(DIR_MODE)).unwrap();
        drop(protected_listener);
        fs::remove_file(protected_socket).unwrap();
    }
}
