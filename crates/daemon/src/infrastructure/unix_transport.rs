//! Secure Unix-domain transport adapter.
//!
//! This module is deliberately outside `usagi-core`: core defines a byte-stream
//! port and framing, while this adapter owns filesystem and peer-credential
//! policy.  Every discovery and accept path validates ownership and refuses
//! symlinks; permission bits are defence in depth, not authentication.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use usagi_core::infrastructure::ipc::DaemonGeneration;

const DIR_MODE: u32 = 0o700;
const SOCKET_MODE: u32 = 0o600;

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
    socket: PathBuf,
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
    pub fn bind(data_dir: &Path, generation: DaemonGeneration) -> io::Result<Self> {
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
        let listener = UnixListener::bind(&temporary)?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(SOCKET_MODE))?;
        fs::rename(&temporary, &socket)?;
        verify_private(&socket, SOCKET_MODE, false)?;
        let locator = EndpointLocator {
            generation,
            endpoint: relative_endpoint(&daemon, &socket)?,
            state: EndpointState::Active,
        };
        write_locator(&daemon, &locator)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            locator,
            socket,
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
}

impl Drop for SecureUnixListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
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
pub fn read_locator(daemon: &Path) -> io::Result<EndpointLocator> {
    let path = daemon.join("current.json");
    verify_private(&path, SOCKET_MODE, false)?;
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_locator(daemon: &Path, locator: &EndpointLocator) -> io::Result<()> {
    let temporary = daemon.join(".current.json.tmp");
    let bytes = serde_json::to_vec(locator).expect("endpoint locator serializes");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(SOCKET_MODE)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(temporary, daemon.join("current.json"))
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

fn ensure_private_dir(path: &Path) -> io::Result<()> {
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
fn peer_uid(_stream: &UnixStream) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer credentials unavailable",
    ))
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn generation() -> DaemonGeneration {
        DaemonGeneration(
            usagi_core::domain::id::DaemonGeneration::new()
                .as_str()
                .clone(),
        )
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
    }
}
