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
//! together with the record's exact process-owner observation.

use std::io;

use crate::domain::daemon::{DaemonProcessObservation, DaemonRecord};

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

/// Reads the OS process-start identity used to fence daemon ownership.
///
/// Production binds this to the platform process table. The returned token is
/// opaque to domain/usecase code: it is persisted and later compared for exact
/// equality. Implementations must not derive it from wall-clock registration
/// time or PID alone.
pub trait ProcessIdentitySource {
    /// Read the process-start identity for `pid`.
    ///
    /// # Errors
    /// Returns an error when the process does not exist, identity cannot be
    /// observed, or the platform cannot provide a safe identity.
    fn process_start_identity(&self, pid: u32) -> io::Result<String>;
}

/// Observes whether a daemon record still names the exact OS process that
/// registered it.
///
/// Pairs with [`classify`](crate::domain::daemon::classify): the store supplies
/// the record and this probe supplies an exact/gone/mismatch/unknown outcome.
/// The real implementation reads an OS process-start identity and never treats
/// PID liveness alone as ownership authority; tests inject a fake so the
/// surrounding logic stays pure.
pub trait LivenessProbe {
    /// Observe the exact process recorded as daemon owner.
    fn observe(&self, record: &DaemonRecord) -> DaemonProcessObservation;
}

/// Requests a process to terminate — the effecting half of `stop`.
///
/// The real implementation re-observes the recorded OS process-start identity
/// immediately before SIGTERM and refuses an unknown or mismatched owner. Tests
/// inject a fake.
pub trait Terminator {
    /// Ask the exact process represented by `record` to terminate.
    ///
    /// # Errors
    /// Returns an error when ownership cannot be revalidated or the termination
    /// request cannot be delivered.
    fn terminate(&self, record: &DaemonRecord) -> io::Result<()>;
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

    /// The failure the launched daemon recorded for itself, if it recorded one.
    ///
    /// A launched daemon is detached with its stderr discarded, exactly as in
    /// production, so a start that never registers looks the same from here
    /// whatever went wrong — a socket path over the platform limit, a data
    /// directory with the wrong mode, a workspace another daemon owns. The
    /// daemon writes the real reason to its error log, and this is how the
    /// waiting `start` reads it back instead of reporting only that a deadline
    /// passed.
    ///
    /// Implementations return the most recent entry. The caller compares the
    /// value from before the launch with the value after, so a stale entry from
    /// an earlier failure is not reported as this one's cause.
    fn recorded_failure(&self) -> Option<String> {
        None
    }

    /// Where a reader can find the full log behind [`Self::recorded_failure`].
    fn failure_log_hint(&self) -> Option<String> {
        None
    }
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
/// record + process observation cannot do race-free.
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

/// What a [`WorkspaceFence`] acquisition found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceFenceOutcome {
    /// This process now owns the workspace for its lifetime.
    Acquired,
    /// Another live daemon owns the workspace. `owner` is its pid when the
    /// holder published one; it is absent when the hint cannot be read, which
    /// still refuses the start.
    Held {
        /// The canonical workspace root that is already owned.
        workspace: String,
        /// The owning daemon's pid, when its hint is readable.
        owner: Option<u32>,
    },
}

/// The workspace-scoped single-daemon guard held by a running `serve`.
///
/// [`InstanceLock`] excludes a second daemon per *data directory*, but a
/// daemon's authority is a *workspace*: the git worktrees, `usagi/<name>`
/// branches, and session names under `<workspace>/.usagi`. Because the data
/// directory is selected by `$USAGI_HOME` and the runtime mode, two daemons that
/// disagree about either one take different instance locks and then both write
/// the same worktrees from independent lifecycle state. This fence keys on the
/// workspace instead, so no spelling of the environment can produce a second
/// owner.
///
/// It is acquired **before** [`InstanceLock`] and held for the process's
/// lifetime; the OS releases it on death, so a crashed owner does not lock the
/// workspace out. Real IO is bound at the synthesis root.
pub trait WorkspaceFence {
    /// Try to become the single daemon owning this workspace.
    ///
    /// # Errors
    /// Returns an error when the fence node cannot be created, verified, or
    /// locked.
    fn acquire(&self) -> io::Result<WorkspaceFenceOutcome>;
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
    /// Registration is the other boundary a PID crosses into durable state (the
    /// first is deserialization), so a record whose PID cannot name a process is
    /// refused here rather than written for a later reader to act on.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::InvalidInput`] when `record` names a PID outside
    /// [`is_record_pid`](crate::domain::daemon::is_record_pid), or the
    /// [`RecordFile`] write error.
    ///
    /// # Panics
    /// Panics only if serializing a `DaemonRecord` to JSON fails, which cannot
    /// happen for its scalar fields.
    pub fn save(&self, record: &DaemonRecord) -> io::Result<()> {
        if !crate::domain::daemon::is_record_pid(record.pid) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                crate::domain::daemon::InvalidRecordPid(record.pid),
            ));
        }
        // Serializing a DaemonRecord's scalar fields cannot fail.
        let json = serde_json::to_string(record).expect("DaemonRecord serializes to JSON");
        self.file.write(&json)
    }

    /// Remove `expected` only if it is still the persisted daemon incarnation.
    ///
    /// Equality covers the full serialized [`DaemonRecord`] (PID, OS
    /// process-start identity, and registration timestamp). A replacement
    /// record is therefore preserved even when an older stop or owner cleanup
    /// resumes late.
    ///
    /// # Errors
    /// Returns the [`RecordFile`] conditional-remove error.
    ///
    /// # Panics
    /// Panics only if serializing a `DaemonRecord` to JSON fails, which cannot
    /// happen for its scalar fields.
    pub fn clear_if(&self, expected: &DaemonRecord) -> io::Result<bool> {
        // Serializing a DaemonRecord's scalar fields cannot fail.
        let json = serde_json::to_string(expected).expect("DaemonRecord serializes to JSON");
        self.file.remove_if(&json)
    }
}

#[cfg(test)]
mod tests;
