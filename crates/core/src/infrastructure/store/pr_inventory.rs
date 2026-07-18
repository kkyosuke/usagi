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
        Ok(json_file::read(&self.path())?.unwrap_or_default())
    }
    /// # Errors
    ///
    /// Returns an error when the atomic snapshot write fails.
    pub fn save(&self, snapshot: &PrInventoryStoreSnapshot) -> Result<()> {
        json_file::write_atomic(Path::new(&self.dir), &self.path(), snapshot)
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
        let bad = PrInventoryStore::new(temp.path().join("file"));
        std::fs::write(temp.path().join("file"), "x").unwrap();
        assert!(bad.save(&PrInventoryStoreSnapshot::default()).is_err());
    }
}
