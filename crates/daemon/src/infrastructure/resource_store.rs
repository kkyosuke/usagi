//! Filesystem adapters for the owner shards and the global allocator.
//!
//! ```text
//! <data-dir>/daemon/allocations.json          the global allocator document
//! <data-dir>/daemon/allocations.lock          serializes its read-modify-write
//! <data-dir>/daemon/shards/<generation>.json  one owner generation's shard
//! <data-dir>/daemon/shards/<generation>.lock  serializes that shard's swap
//! ```
//!
//! Every compare-and-swap is byte exact and holds one cross-process lock across
//! both the comparison and the replacement, which is what the whole-snapshot
//! stores could not do: a writer that read older bytes loses instead of erasing
//! the newer document. The shard files are separate objects on purpose — a
//! draining owner and a new active owner never write the same path at all.
//!
//! The same directory holds the one-way migration of the stores this replaces:
//!
//! ```text
//! <data-dir>/daemon/runtime-migration.json    what was adopted, and from where
//! <data-dir>/daemon/agents.json.migrated      the retired legacy Agent store
//! <data-dir>/daemon/terminals.json.migrated   the retired legacy terminal store
//! ```
//!
//! A legacy store is renamed rather than deleted, so the bytes stay inspectable
//! while no build reads them again.

use std::io;
use std::path::{Path, PathBuf};

use usagi_core::domain::id::DaemonGeneration;

use crate::infrastructure::unix_transport::{
    ensure_private_dir, lock_private_node, read_private_bytes_if_present, write_private_file,
};
use crate::usecase::resources::CasFile;
use crate::usecase::resources::durable::{LegacySnapshots, ShardArchive};

const ALLOCATOR_FILE: &str = "allocations.json";
const ALLOCATOR_LOCK: &str = "allocations.lock";
const ALLOCATOR_TEMP_PREFIX: &str = ".allocations.json.tmp.";
const SHARD_DIR: &str = "shards";
const MIGRATION_FILE: &str = "runtime-migration.json";
const MIGRATION_TEMP_PREFIX: &str = ".runtime-migration.json.tmp.";
const MIGRATION_LOCK: &str = "runtime-migration.lock";
const LEGACY_STORES: [&str; 2] = ["agents.json", "terminals.json"];
const MIGRATED_SUFFIX: &str = ".migrated";

/// The global allocator document in a daemon data directory.
pub struct AllocatorFile {
    daemon: PathBuf,
}

impl AllocatorFile {
    /// Bind the allocator inside `<data_dir>/daemon`, creating the private
    /// directory when it does not exist yet.
    ///
    /// # Errors
    /// Returns an error when the private daemon directory cannot be created or
    /// verified.
    pub fn new(data_dir: &Path) -> io::Result<Self> {
        let daemon = data_dir.join("daemon");
        ensure_private_dir(&daemon)?;
        Ok(Self { daemon })
    }
}

impl CasFile for AllocatorFile {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
    fn read(&self) -> io::Result<Option<String>> {
        let _lock = lock_private_node(&self.daemon, ALLOCATOR_LOCK)?;
        read_document(&self.daemon.join(ALLOCATOR_FILE))
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
    fn compare_and_write(&self, expected: Option<&str>, contents: &str) -> io::Result<bool> {
        let _lock = lock_private_node(&self.daemon, ALLOCATOR_LOCK)?;
        if read_document(&self.daemon.join(ALLOCATOR_FILE))?.as_deref() != expected {
            return Ok(false);
        }
        write_private_file(
            &self.daemon,
            ALLOCATOR_FILE,
            ALLOCATOR_TEMP_PREFIX,
            contents.as_bytes(),
        )?;
        Ok(true)
    }
}

/// One owner generation's shard document.
pub struct OwnerShardFile {
    shards: PathBuf,
    file: String,
    lock: String,
    temp_prefix: String,
}

impl OwnerShardFile {
    /// Bind `generation`'s shard inside `<data_dir>/daemon/shards`, creating the
    /// private directory when it does not exist yet.
    ///
    /// # Errors
    /// Returns an error when the private shard directory cannot be created or
    /// verified.
    pub fn new(data_dir: &Path, generation: DaemonGeneration) -> io::Result<Self> {
        let daemon = data_dir.join("daemon");
        ensure_private_dir(&daemon)?;
        let shards = daemon.join(SHARD_DIR);
        ensure_private_dir(&shards)?;
        let name = generation.as_str();
        Ok(Self {
            shards,
            file: format!("{name}.json"),
            lock: format!("{name}.lock"),
            temp_prefix: format!(".{name}.json.tmp."),
        })
    }
}

impl CasFile for OwnerShardFile {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
    fn read(&self) -> io::Result<Option<String>> {
        let _lock = lock_private_node(&self.shards, &self.lock)?;
        read_document(&self.shards.join(&self.file))
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
    fn compare_and_write(&self, expected: Option<&str>, contents: &str) -> io::Result<bool> {
        let _lock = lock_private_node(&self.shards, &self.lock)?;
        if read_document(&self.shards.join(&self.file))?.as_deref() != expected {
            return Ok(false);
        }
        write_private_file(
            &self.shards,
            &self.file,
            &self.temp_prefix,
            contents.as_bytes(),
        )?;
        Ok(true)
    }
}

/// The shard directory, the legacy stores, and the one-way migration marker.
///
/// Every method here is byte level: which generation may be collected, and what a
/// legacy record becomes, are decided in
/// [`crate::usecase::resources::durable`] and never here.
pub struct ShardArchiveFiles {
    data_dir: PathBuf,
    daemon: PathBuf,
    shards: PathBuf,
}

impl ShardArchiveFiles {
    /// Bind the archive inside `<data_dir>/daemon`, creating the private
    /// directories when they do not exist yet.
    ///
    /// # Errors
    /// Returns an error when a private directory cannot be created or verified.
    pub fn new(data_dir: &Path) -> io::Result<Self> {
        let daemon = data_dir.join("daemon");
        ensure_private_dir(&daemon)?;
        let shards = daemon.join(SHARD_DIR);
        ensure_private_dir(&shards)?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            daemon,
            shards,
        })
    }
}

impl ShardArchive for ShardArchiveFiles {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_shard_state
    fn documents(&self) -> io::Result<Vec<String>> {
        let mut names: Vec<PathBuf> = std::fs::read_dir(&self.shards)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        names.sort();
        let mut documents = Vec::new();
        for path in names {
            // A shard removed by a concurrent collection is simply not there any
            // more, which is not a failure of this read.
            if let Some(contents) = read_document(&path)? {
                documents.push(contents);
            }
        }
        Ok(documents)
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_shard_state
    fn shard(&self, owner: DaemonGeneration) -> io::Result<Box<dyn CasFile + Send>> {
        Ok(Box::new(OwnerShardFile::new(&self.data_dir, owner)?))
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_shard_state
    fn collect(&self, owner: DaemonGeneration) -> io::Result<()> {
        let name = owner.as_str();
        let _lock = lock_private_node(&self.shards, &format!("{name}.lock"))?;
        remove_if_present(&self.shards.join(format!("{name}.json")))
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_shard_state
    fn legacy(&self) -> io::Result<LegacySnapshots> {
        let _lock = lock_private_node(&self.daemon, MIGRATION_LOCK)?;
        Ok(LegacySnapshots {
            agents: read_legacy(&self.daemon.join(LEGACY_STORES[0]))?,
            terminals: read_legacy(&self.daemon.join(LEGACY_STORES[1]))?,
        })
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_shard_state
    fn seal_legacy(&self, marker: &str) -> io::Result<()> {
        let _lock = lock_private_node(&self.daemon, MIGRATION_LOCK)?;
        // The marker is durable before the stores are retired: a crash between
        // the two leaves a marker plus readable legacy bytes, which the next pass
        // adopts again idempotently.
        write_private_file(
            &self.daemon,
            MIGRATION_FILE,
            MIGRATION_TEMP_PREFIX,
            marker.as_bytes(),
        )?;
        for name in LEGACY_STORES {
            let legacy = self.daemon.join(name);
            if legacy.exists() {
                std::fs::rename(
                    &legacy,
                    self.daemon.join(format!("{name}{MIGRATED_SUFFIX}")),
                )?;
            }
        }
        Ok(())
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_shard_state
fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Read a legacy store as the build that wrote it left it.
///
/// The shards and the allocator are held to the daemon's private file mode, but a
/// legacy store was written by an older build that did not harden it. Refusing to
/// read it because of its mode would make every existing installation fail to
/// start, so migration reads the bytes plainly and hardens what it writes instead.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_shard_state
fn read_legacy(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
fn read_document(path: &Path) -> io::Result<Option<String>> {
    let Some(bytes) = read_private_bytes_if_present(path)? else {
        return Ok(None);
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
