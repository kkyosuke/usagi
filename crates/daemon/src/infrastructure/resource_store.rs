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
use crate::usecase::resources::shard::OwnerShard;
use crate::usecase::runtime_shard::{RETIRED_LEGACY_SUFFIX, ShardSource};

const ALLOCATOR_FILE: &str = "allocations.json";
const ALLOCATOR_LOCK: &str = "allocations.lock";
const ALLOCATOR_TEMP_PREFIX: &str = ".allocations.json.tmp.";
const SHARD_DIR: &str = "shards";
const SHARD_EXTENSION: &str = "json";

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

/// The retained shards of one daemon data directory.
///
/// Enumerating them is what makes "every generation's state" readable without a
/// registry: the documents *are* the inventory. A file this build cannot name a
/// generation for is skipped rather than guessed at, so a stray file in the
/// directory can never be adopted as somebody's runtime state.
pub struct ShardDirectory {
    data_dir: PathBuf,
}

impl ShardDirectory {
    /// Bind the shard directory of `data_dir`.
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
        }
    }
}

impl ShardSource for ShardDirectory {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
    fn generations(&self) -> io::Result<Vec<DaemonGeneration>> {
        let shards = self.data_dir.join("daemon").join(SHARD_DIR);
        match std::fs::symlink_metadata(&shards) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        }
        ensure_private_dir(&shards)?;
        let mut generations: Vec<DaemonGeneration> = std::fs::read_dir(&shards)?
            .collect::<io::Result<Vec<_>>>()?
            .iter()
            .filter_map(|entry| shard_generation(&entry.path()))
            .collect();
        generations.sort_by_key(|generation| generation.as_str().clone());
        Ok(generations)
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
    fn open(&self, generation: DaemonGeneration) -> io::Result<OwnerShard> {
        Ok(OwnerShard::new(
            OwnerShardFile::new(&self.data_dir, generation)?,
            generation,
        ))
    }
}

/// The generation a shard path names, if it names one at all.
fn shard_generation(path: &Path) -> Option<DaemonGeneration> {
    if path.extension()?.to_str()? != SHARD_EXTENSION {
        return None;
    }
    DaemonGeneration::parse(path.file_stem()?.to_str()?).ok()
}

/// Move a legacy whole-snapshot store aside once its records live in shards.
///
/// The bytes are kept, not deleted: they are the only copy of what the previous
/// build believed, and an operator inspecting a migration needs them. Renaming is
/// also what makes the migration one-way — an older build reading `agents.json`
/// finds nothing rather than state that has since moved on.
///
/// # Errors
/// Returns the rename error. An absent legacy document is already retired and is
/// not an error.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=runtime_resources
pub fn retire_legacy(path: &Path) -> io::Result<bool> {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let retired = path.with_file_name(format!("{name}{RETIRED_LEGACY_SUFFIX}"));
    std::fs::rename(path, retired)?;
    Ok(true)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_shard_document_names_a_generation() {
        let generation = DaemonGeneration::new();
        let named = PathBuf::from(format!("/tmp/shards/{}.json", generation.as_str()));
        assert_eq!(shard_generation(&named), Some(generation));
        // Neither a lock, a writer's temporary file, an extensionless entry, nor a
        // name this build cannot parse is somebody's runtime state.
        for stray in [
            format!("/tmp/shards/{}.lock", generation.as_str()),
            format!("/tmp/shards/.{}.json.tmp.7", generation.as_str()),
            format!("/tmp/shards/{}", generation.as_str()),
            "/tmp/shards/not-a-generation.json".to_owned(),
        ] {
            assert_eq!(shard_generation(&PathBuf::from(stray)), None);
        }
    }
}
