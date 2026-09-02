//! Atomic durable storage for daemon-owned session PR inventories.

use crate::{
    domain::{id::SessionId, pr_inventory::PrInventory},
    infrastructure::persistence::json_file,
};
use anyhow::Result;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Hard ceiling for the complete durable inventory document.
pub const PR_INVENTORY_SNAPSHOT_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct PrInventoryStoreSnapshot {
    pub sessions: BTreeMap<SessionId, PrInventory>,
}

pub struct PrInventoryStore {
    dir: PathBuf,
}
impl PrInventoryStore {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.join("pr-inventory.json")
    }
    /// # Errors
    ///
    /// Returns an error when the snapshot cannot be read or deserialized.
    pub fn load(&self) -> Result<PrInventoryStoreSnapshot> {
        Ok(
            json_file::read_bounded(&self.path(), PR_INVENTORY_SNAPSHOT_MAX_BYTES)?
                .unwrap_or_default(),
        )
    }
    /// # Errors
    ///
    /// Returns an error when the atomic snapshot write fails.
    pub fn save(&self, snapshot: &PrInventoryStoreSnapshot) -> Result<()> {
        json_file::write_atomic_bounded(
            Path::new(&self.dir),
            &self.path(),
            snapshot,
            PR_INVENTORY_SNAPSHOT_MAX_BYTES,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trips_and_keeps_existing_file_when_write_fails() {
        let temp = tempfile::tempdir().unwrap();
        let store = PrInventoryStore::new(temp.path());
        assert_eq!(store.load().unwrap(), PrInventoryStoreSnapshot::default());
        store.save(&PrInventoryStoreSnapshot::default()).unwrap();
        assert_eq!(store.load().unwrap(), PrInventoryStoreSnapshot::default());

        let original = std::fs::read(store.path()).unwrap();
        assert!(
            json_file::write_atomic_bounded(
                temp.path(),
                &store.path(),
                &PrInventoryStoreSnapshot::default(),
                1,
            )
            .is_err()
        );
        assert_eq!(std::fs::read(store.path()).unwrap(), original);

        std::fs::write(store.path(), "{").unwrap();
        assert!(store.load().is_err());
        std::fs::write(
            store.path(),
            vec![b' '; PR_INVENTORY_SNAPSHOT_MAX_BYTES + 1],
        )
        .unwrap();
        assert!(store.load().is_err());

        let bad = PrInventoryStore::new(temp.path().join("file"));
        std::fs::write(temp.path().join("file"), "x").unwrap();
        assert!(bad.load().is_err());
        assert!(bad.save(&PrInventoryStoreSnapshot::default()).is_err());

        let directory = PrInventoryStore::new(temp.path().join("directory"));
        std::fs::create_dir_all(directory.path()).unwrap();
        assert!(directory.load().is_err());
    }
}
