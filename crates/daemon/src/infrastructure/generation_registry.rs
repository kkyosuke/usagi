//! The filesystem adapter for the durable generation registry.
//!
//! `<data-dir>/daemon/generations.json` holds the registry document and
//! `<data-dir>/daemon/generations.lock` serializes every read-modify-write
//! against it across processes. The compare-and-swap is byte exact: a writer
//! that read older bytes loses, which is what keeps two daemon processes from
//! producing a lost update on the same document.
//!
//! It is deliberately a different object from `current.json`. Clients discover
//! an endpoint by reading one small locator; only the daemons read the
//! registry. Keeping the two consistent across a crash is the handoff
//! protocol's job ([`crate::usecase::authority::handoff`]), not this adapter's.

use std::io;
use std::path::{Path, PathBuf};

use crate::infrastructure::unix_transport::{
    ensure_private_dir, lock_private_node, read_private_bytes_if_present, write_private_file,
};
use crate::usecase::authority::handoff::{LocatorObservation, PublishedLocator};
use crate::usecase::authority::registry::RegistryFile;
use crate::usecase::authority::rollover::CurrentLocator;

const REGISTRY_FILE: &str = "generations.json";
const REGISTRY_LOCK: &str = "generations.lock";
const REGISTRY_TEMP_PREFIX: &str = ".generations.json.tmp.";

/// The durable registry document in a daemon data directory.
pub struct GenerationRegistryFile {
    daemon: PathBuf,
}

impl GenerationRegistryFile {
    /// Bind the registry inside `<data_dir>/daemon`, creating the private
    /// directory when it does not exist yet.
    ///
    /// # Errors
    ///
    /// Returns an error when the private daemon directory cannot be created or
    /// verified.
    pub fn new(data_dir: &Path) -> io::Result<Self> {
        let daemon = data_dir.join("daemon");
        ensure_private_dir(&daemon)?;
        Ok(Self { daemon })
    }
}

impl RegistryFile for GenerationRegistryFile {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=generation_registry_store
    fn read(&self) -> io::Result<Option<String>> {
        let _lock = lock_private_node(&self.daemon, REGISTRY_LOCK)?;
        read_bytes(&self.daemon)
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=generation_registry_store
    fn compare_and_write(&self, expected: Option<&str>, contents: &str) -> io::Result<bool> {
        // Comparison and replacement share one lock, so a writer that read the
        // same bytes concurrently cannot also win this swap.
        let _lock = lock_private_node(&self.daemon, REGISTRY_LOCK)?;
        if read_bytes(&self.daemon)?.as_deref() != expected {
            return Ok(false);
        }
        write_private_file(
            &self.daemon,
            REGISTRY_FILE,
            REGISTRY_TEMP_PREFIX,
            contents.as_bytes(),
        )?;
        Ok(true)
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=generation_registry_store
fn read_bytes(daemon: &Path) -> io::Result<Option<String>> {
    let Some(bytes) = read_private_bytes_if_present(&daemon.join(REGISTRY_FILE))? else {
        return Ok(None);
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// The published `current.json` in a daemon data directory, as the handoff
/// protocol's locator port.
pub struct CurrentLocatorFile {
    data_dir: PathBuf,
}

impl CurrentLocatorFile {
    /// Bind the current locator of `data_dir`.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
        }
    }
}

impl CurrentLocator for CurrentLocatorFile {
    /// An absent locator is an observation; anything unreadable or unsafe is
    /// reported as [`LocatorObservation::Unreadable`] rather than guessed at,
    /// so recovery fails closed instead of adopting a malformed endpoint.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=generation_registry_store
    fn read(&self) -> io::Result<LocatorObservation> {
        let daemon = self.data_dir.join("daemon");
        match crate::infrastructure::unix_transport::read_locator(&daemon) {
            Ok(locator) => usagi_core::domain::id::DaemonGeneration::parse(&locator.generation.0)
                .map_or(Ok(LocatorObservation::Unreadable), |generation| {
                    Ok(LocatorObservation::Published(PublishedLocator {
                        generation,
                        endpoint: locator.endpoint,
                    }))
                }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(LocatorObservation::Absent),
            Err(_) => Ok(LocatorObservation::Unreadable),
        }
    }

    /// A recovering process publishes on behalf of a generation it does not
    /// own, so the endpoint is re-verified as that generation's own safe socket
    /// before and across the write.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=generation_registry_store
    fn publish(&self, locator: &PublishedLocator) -> io::Result<()> {
        crate::infrastructure::unix_transport::publish_recovered_locator(
            &self.data_dir,
            &usagi_core::infrastructure::ipc::DaemonGeneration(locator.generation.as_str()),
            &locator.endpoint,
        )
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=generation_registry_store
    fn retire(&self) -> io::Result<()> {
        crate::infrastructure::unix_transport::retire_stale_current(&self.data_dir)
    }
}
