//! The daemon record store: persistence for [`DaemonRecord`] behind an injected
//! file seam.
//!
//! [`DaemonRecordStore`] owns the JSON (de)serialization of the daemon lifecycle
//! record; where and how the bytes live is the [`RecordFile`] seam's concern.
//! The real filesystem implementation — reading and writing
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
/// yields `None` when the file does not exist, and `remove` succeeds even when
/// it is already absent, so the store can treat "no daemon registered" as a
/// normal state rather than an error.
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
    /// Remove the file, succeeding even when it does not exist.
    ///
    /// # Errors
    /// Returns an error when an existing file cannot be removed.
    fn remove(&self) -> io::Result<()>;
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

/// Blocks a running `serve` until the daemon is asked to shut down.
///
/// The real implementation waits for SIGINT / SIGTERM; it is real IO bound at
/// the synthesis root, so the `serve` loop stays testable through a fake that
/// returns immediately. Returning `Ok` means "shut down now"; the caller then
/// clears its record and exits.
pub trait ShutdownSignal {
    /// Block until the daemon should stop.
    ///
    /// # Errors
    /// Returns an error when waiting for the shutdown signal fails.
    fn wait(&self) -> io::Result<()>;
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

    /// Remove the persisted record on stop or stale reclaim; a no-op when absent.
    ///
    /// # Errors
    /// Returns the [`RecordFile`] remove error.
    pub fn clear(&self) -> io::Result<()> {
        self.file.remove()
    }
}

#[cfg(test)]
mod tests;
