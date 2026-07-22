//! The daemon record store: persistence for [`DaemonRecord`] behind an injected
//! file seam.
//!
//! [`DaemonRecordStore`] owns the JSON (de)serialization of the daemon lifecycle
//! record; where and how the bytes live is the [`RecordFile`] seam's concern.
//! The real filesystem implementation — reading, writing, and conditionally
//! removing
//! `<data-dir>/daemon/daemon.json` — is real IO and is bound at the synthesis
//! root, so this layer stays pure and fully testable through an in-memory fake.
//! Resolving `<data-dir>` into a concrete path is likewise a caller concern and
//! is not decided here.
//!
//! A missing file means no daemon has registered: [`DaemonRecordStore::load`]
//! returns `None` rather than erroring, which is what the daemon (guarding
//! single-instance startup) and clients (locating a daemon to connect to) act on
//! together with the record's liveness.

use std::io;

use crate::domain::daemon::DaemonRecord;

/// The file seam the store reads and writes through.
///
/// The real filesystem implementation (reading/writing the JSON file) is real IO
/// and is bound at the synthesis root; tests inject an in-memory fake. `read`
/// yields `None` when the file does not exist. [`write`](RecordFile::write) and
/// [`remove_if`](RecordFile::remove_if) must be serialized by one stable lock:
/// replacing `daemon.json` must never race a previous owner's conditional
/// cleanup after that owner has inspected an older record. Durable adapters
/// must also publish a complete replacement atomically rather than truncating
/// the live record in place.
pub trait RecordFile {
    /// Read the file's contents, or `None` when it does not exist.
    ///
    /// # Errors
    /// Returns an error when the file exists but cannot be read.
    fn read(&self) -> io::Result<Option<String>>;
    /// Replace the file's contents, creating it when absent.
    ///
    /// # Errors
    /// Returns an error when the contents cannot be written.
    fn write(&self, contents: &str) -> io::Result<()>;
    /// Remove the file only when its contents still equal `expected`.
    ///
    /// The comparison and removal must be one transaction relative to every
    /// [`write`](RecordFile::write) and other conditional removal. Returns
    /// `true` only when this call removed the expected contents; an absent or
    /// replaced file returns `false`.
    ///
    /// # Errors
    /// Returns an error when the file cannot be inspected or removed.
    fn remove_if(&self, expected: &str) -> io::Result<bool>;
}

/// Probes whether a process is alive — the liveness half of classifying a daemon
/// record.
///
/// Pairs with [`classify`](crate::domain::daemon::classify): the store supplies
/// the record and this probe supplies the `alive` flag. The real implementation
/// (signal 0 on Unix) is real IO bound at the synthesis root; tests inject a
/// fake so the surrounding logic stays pure.
pub trait LivenessProbe {
    /// Whether the process with `pid` is currently alive.
    fn is_alive(&self, pid: u32) -> bool;
}

/// Requests a process to terminate — the effecting half of `stop`.
///
/// The real implementation (SIGTERM on Unix) is real IO bound at the synthesis
/// root; tests inject a fake. Only the recorded daemon pid is ever passed, after
/// [`classify`](crate::domain::daemon::classify) has confirmed it is alive.
pub trait Terminator {
    /// Ask the process `pid` to terminate.
    ///
    /// # Errors
    /// Returns an error when the termination request cannot be delivered.
    fn terminate(&self, pid: u32) -> io::Result<()>;
}

/// Prepares for and then blocks a running `serve` until the daemon is asked to
/// shut down.
///
/// The real implementation waits for SIGINT / SIGTERM; it is real IO bound at
/// the synthesis root, so the `serve` loop stays testable through a fake that
/// returns immediately. Preparation happens before endpoint publication so
/// shutdown delivery is installed before any worker starts. Returning `Ok` from
/// [`wait`](ShutdownSignal::wait) means "shut down now"; the caller then
/// quiesces and retires its endpoint before exiting.
pub trait ShutdownSignal {
    /// Prepare shutdown delivery before the daemon publishes or spawns workers.
    ///
    /// # Errors
    /// Returns an error when shutdown delivery cannot be prepared safely.
    fn prepare(&self) -> io::Result<()>;

    /// Block until the daemon should stop.
    ///
    /// # Errors
    /// Returns an error when waiting for the shutdown signal fails.
    fn wait(&self) -> io::Result<()>;
}

/// Recovers any stale endpoint left by a previous owner, then publishes the
/// daemon's externally connectable endpoint after it has become the registered
/// single process owner.
///
/// [`crate::usecase::serve`] acquires the instance lock, snapshots the previous
/// lifecycle record, and calls
/// [`recover_stale_endpoint`](DaemonReady::recover_stale_endpoint) before
/// replacing that record or calling [`publish`](DaemonReady::publish). The
/// recovery must be idempotent and generation-fenced: it may remove artifacts
/// owned by the previous inactive daemon, but must leave a replacement
/// generation untouched. On shutdown `serve` calls
/// [`quiesce`](DaemonReady::quiesce) before clearing the record, then
/// [`retire`](DaemonReady::retire) while it still holds the instance lock.
/// Implementations must not expose a new endpoint before `publish`.
pub trait DaemonReady {
    /// Retire stale endpoint artifacts before this process registers itself.
    ///
    /// This is called while the instance lock is held, including when no
    /// previous lifecycle record exists. Successful return proves that startup
    /// may proceed without inheriting an endpoint from an inactive owner.
    ///
    /// # Errors
    /// Returns an error when stale endpoint ownership cannot be proved or its
    /// artifacts cannot be retired safely.
    fn recover_stale_endpoint(&self) -> io::Result<()>;

    /// Publish the endpoint for an already registered daemon.
    ///
    /// # Errors
    /// Returns an error when the endpoint cannot be made available.
    fn publish(&self) -> io::Result<()>;

    /// Stop accepting new work and join the endpoint-serving worker without
    /// removing the published generation locator yet.
    ///
    /// # Errors
    /// Returns an error when the serving worker cannot be stopped and joined.
    fn quiesce(&self) -> io::Result<()>;

    /// Remove the quiesced endpoint if this owner still owns the published
    /// generation. A stale owner must leave a replacement locator untouched.
    ///
    /// # Errors
    /// Returns an error when the owned endpoint cannot be retired safely.
    fn retire(&self) -> io::Result<()>;
}

/// Spawns a detached daemon process — the effecting half of `start`.
///
/// The real implementation launches `usagi daemon serve` as a detached child
/// that survives the parent; it is real IO bound at the synthesis root. The
/// launched `serve` registers its own pid, so `start` learns the pid by reading
/// the record afterwards rather than from `launch`.
pub trait DaemonLauncher {
    /// Spawn a detached `usagi daemon serve` process.
    ///
    /// # Errors
    /// Returns an error when the process cannot be spawned.
    fn launch(&self) -> io::Result<()>;
}

/// Pauses between daemon lifecycle polls: `start` waits for a freshly launched
/// daemon to record itself, while `stop` waits for the signalled owner to retire
/// its endpoint and clear its exact record.
///
/// The real implementation sleeps a short interval; tests inject a no-op so the
/// poll loop runs instantly.
pub trait Sleeper {
    /// Sleep for one lifecycle poll interval.
    fn sleep(&self);
}

/// The authoritative single-instance guard held by a running `serve`.
///
/// The real implementation takes an exclusive advisory lock (`flock`-style, via
/// `fs2`, following [`super::persistence::store_lock`]'s style) on a per-daemon
/// lock file and holds it for the process's lifetime; it is real IO bound at the
/// synthesis root. Because the OS releases an `flock` when the holding process
/// dies, this guards against multiple daemons even across crashes — something the
/// record + liveness check cannot do race-free.
///
/// [`acquire`](InstanceLock::acquire) waits briefly for a departing holder before
/// giving up, so a `restart` hands the lock from the exiting daemon to the new
/// one without a race, while a genuine second daemon is refused.
pub trait InstanceLock {
    /// Try to become the single running daemon, waiting briefly for a departing
    /// holder. Returns `true` when the lock is now held by this process, or
    /// `false` when another daemon still holds it.
    ///
    /// # Errors
    /// Returns an error when the lock file cannot be opened or locked.
    fn acquire(&self) -> io::Result<bool>;
}

/// Persists a [`DaemonRecord`] as JSON through a [`RecordFile`].
pub struct DaemonRecordStore<F> {
    file: F,
}

impl<F: RecordFile> DaemonRecordStore<F> {
    /// Build a store over the given file seam.
    pub fn new(file: F) -> Self {
        Self { file }
    }

    /// Load the persisted record, or `None` when the file is absent.
    ///
    /// # Errors
    /// Returns the [`RecordFile`] read error, or [`io::ErrorKind::InvalidData`]
    /// when the stored bytes are not a valid `DaemonRecord`, so callers handle
    /// malformed data uniformly with read failures.
    pub fn load(&self) -> io::Result<Option<DaemonRecord>> {
        match self.file.read()? {
            None => Ok(None),
            Some(contents) => serde_json::from_str(&contents)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        }
    }

    /// Persist `record`, overwriting any existing record.
    ///
    /// # Errors
    /// Returns the [`RecordFile`] write error.
    ///
    /// # Panics
    /// Panics only if serializing a `DaemonRecord` to JSON fails, which cannot
    /// happen for its fields (a `u32` and a timestamp).
    pub fn save(&self, record: &DaemonRecord) -> io::Result<()> {
        // Serializing a DaemonRecord (a u32 and a timestamp) cannot fail.
        let json = serde_json::to_string(record).expect("DaemonRecord serializes to JSON");
        self.file.write(&json)
    }

    /// Remove `expected` only if it is still the persisted daemon incarnation.
    ///
    /// Equality covers the full serialized [`DaemonRecord`] (`pid` and
    /// `started_at`). A replacement record is therefore preserved even when an
    /// older stop or owner cleanup resumes late.
    ///
    /// # Errors
    /// Returns the [`RecordFile`] conditional-remove error.
    ///
    /// # Panics
    /// Panics only if serializing a `DaemonRecord` to JSON fails, which cannot
    /// happen for its fields (a `u32` and a timestamp).
    pub fn clear_if(&self, expected: &DaemonRecord) -> io::Result<bool> {
        // Serializing a DaemonRecord (a u32 and a timestamp) cannot fail.
        let json = serde_json::to_string(expected).expect("DaemonRecord serializes to JSON");
        self.file.remove_if(&json)
    }
}

#[cfg(test)]
mod tests;
