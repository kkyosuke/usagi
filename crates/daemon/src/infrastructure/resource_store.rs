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

use std::io;
use std::path::{Path, PathBuf};

use usagi_core::domain::id::DaemonGeneration;

use crate::infrastructure::unix_transport::{
    ensure_private_dir, lock_private_node, read_private_bytes_if_present, write_private_file,
};
use crate::usecase::resources::CasFile;

const ALLOCATOR_FILE: &str = "allocations.json";
const ALLOCATOR_LOCK: &str = "allocations.lock";
const ALLOCATOR_TEMP_PREFIX: &str = ".allocations.json.tmp.";
const SHARD_DIR: &str = "shards";

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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
fn read_document(path: &Path) -> io::Result<Option<String>> {
    let Some(bytes) = read_private_bytes_if_present(path)? else {
        return Ok(None);
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
