//! The filesystem adapter for the durable generation registry.
//!
//! `<data-dir>/daemon/generations.json` holds the registry document and
//! `<data-dir>/daemon/generations.lock` serializes every read-modify-write
//! against it across processes. The compare-and-swap is byte exact: a writer
//! that read older bytes loses, which is what keeps two daemon processes from
//! producing a lost update on the same document.
//!
//! It is deliberately a different object from `current.json`. The locator names
//! the one endpoint a client connects to for control work; the registry names
//! every retained generation, which a client needs only to reach a terminal an
//! older generation still owns ([`TrustedGenerationDirectory`], #508). Writing
//! it stays a daemon-only privilege, and keeping the two consistent across a
//! crash is the handoff protocol's job
//! ([`crate::usecase::authority::handoff`]), not this adapter's.

use std::io;
use std::path::{Path, PathBuf};

use usagi_core::infrastructure::ipc::GenerationRole as WireRole;
use usagi_core::usecase::owner_routing::{
    DirectoryError, GenerationDirectory, TrustedEndpoint, TrustedEndpoints,
};

use crate::infrastructure::unix_transport::{
    ensure_private_dir, lock_private_node, read_private_bytes_if_present, write_private_file,
};
use crate::usecase::authority::handoff::{LocatorObservation, PublishedLocator};
use crate::usecase::authority::registry::{REGISTRY_SCHEMA, RegistryDocument, RegistryFile};
use crate::usecase::authority::rollover::CurrentLocator;
use crate::usecase::generation::GenerationRole;

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

/// Read the durable registry document without creating anything in `data_dir`.
///
/// A reader must never become a second writer, so this neither ensures the
/// private daemon directory nor takes the registry lock node. `Ok(None)` means
/// no daemon in this data directory has ever registered a generation, which is
/// the state a build that cannot roll over stays in.
///
/// # Errors
///
/// Returns [`DirectoryError::Unreadable`] when the bytes cannot be read and
/// [`DirectoryError::Corrupt`] when they are not a registry document. The
/// schema itself is *not* judged here: naming an unsupported schema is a useful
/// answer for a caller that reports why a rollover cannot start
/// ([`crate::usecase::replacement::seamless_refusal`]).
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=generation_registry_store
pub fn read_registry_document(data_dir: &Path) -> Result<Option<RegistryDocument>, DirectoryError> {
    let daemon = data_dir.join("daemon");
    let Some(contents) =
        read_bytes(&daemon).map_err(|error| DirectoryError::Unreadable(error.to_string()))?
    else {
        return Ok(None);
    };
    serde_json::from_str(&contents)
        .map(Some)
        .map_err(|_| DirectoryError::Corrupt("registry document does not parse"))
}

/// The client's read-only view of the same durable records.
///
/// A client resolves the endpoint of a generation it holds a `TerminalRef` for,
/// so it needs more than the one-entry locator — but it must not become a second
/// writer of the registry either. This adapter therefore reads and never
/// creates: no daemon directory, no lock node, no document.
///
/// Two readings are possible, and both are trusted because the daemon wrote
/// them:
///
/// * `generations.json` present — every retained generation, with the role and
///   endpoint the registry holds. Standby and retired generations are dropped:
///   a standby is private until it is activated, and a retired generation is
///   exactly the verified absence that lets a client collect its tabs.
/// * `generations.json` absent — a daemon that has never rolled over. The
///   published `current.json` is then the whole authority, which is precisely
///   today's single-generation behaviour.
pub struct TrustedGenerationDirectory {
    data_dir: PathBuf,
}

impl TrustedGenerationDirectory {
    /// Bind the directory of `data_dir` without creating anything in it.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// The registry document, or `None` when this daemon never rolled over.
    fn registry(&self) -> Result<Option<RegistryDocument>, DirectoryError> {
        let document = read_registry_document(&self.data_dir)?;
        if document
            .as_ref()
            .is_some_and(|document| document.schema != REGISTRY_SCHEMA)
        {
            return Err(DirectoryError::Corrupt("registry schema is not supported"));
        }
        Ok(document)
    }

    /// The published locator, or `None` when no daemon is published.
    fn locator(&self) -> Result<Option<PublishedLocator>, DirectoryError> {
        match CurrentLocatorFile::new(&self.data_dir).read() {
            Ok(LocatorObservation::Published(locator)) => Ok(Some(locator)),
            Ok(LocatorObservation::Absent) => Ok(None),
            // An untrustworthy locator and a failed inspection are the same
            // answer: no endpoint may be taken from here.
            Ok(LocatorObservation::Unreadable) | Err(_) => Err(DirectoryError::Unreadable(
                "current locator cannot be trusted".into(),
            )),
        }
    }
}

/// Which registry roles a client may address.
fn addressable(role: GenerationRole) -> Option<WireRole> {
    match role {
        GenerationRole::Active => Some(WireRole::Active),
        GenerationRole::Draining => Some(WireRole::Draining),
        GenerationRole::Standby | GenerationRole::Retired => None,
    }
}

impl GenerationDirectory for TrustedGenerationDirectory {
    fn snapshot(&self) -> Result<TrustedEndpoints, DirectoryError> {
        let Some(document) = self.registry()? else {
            let Some(locator) = self.locator()? else {
                return Ok(TrustedEndpoints::default());
            };
            return TrustedEndpoints::build(
                Some(locator.generation),
                vec![TrustedEndpoint {
                    generation: locator.generation,
                    role: WireRole::Active,
                    endpoint: locator.endpoint,
                }],
            );
        };
        let entries = document
            .generations
            .iter()
            .filter_map(|entry| {
                addressable(entry.role).map(|role| TrustedEndpoint {
                    generation: entry.generation,
                    role,
                    endpoint: entry.endpoint.clone(),
                })
            })
            .collect();
        TrustedEndpoints::build(document.current, entries)
    }
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
