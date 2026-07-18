//! Session PR inventory snapshot and persistence port.

use crate::domain::{id::SessionId, pr_inventory::PrInventory};
use std::collections::BTreeMap;

/// Durable boundary used by the daemon projection.
pub trait PrInventoryPort {
    type Error;
    /// # Errors
    ///
    /// Returns the port-specific read error.
    fn load(&self) -> Result<BTreeMap<SessionId, PrInventory>, Self::Error>;
    /// # Errors
    ///
    /// Returns the port-specific write error.
    fn save(&self, sessions: &BTreeMap<SessionId, PrInventory>) -> Result<(), Self::Error>;
}

/// A revisioned snapshot for one stable session identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrInventorySnapshot {
    pub session_id: SessionId,
    pub revision: u64,
    pub inventory: PrInventory,
}

/// # Errors
///
/// Returns an error when the inventory snapshot cannot be loaded.
pub fn snapshot<P: PrInventoryPort>(
    port: &P,
    session_id: SessionId,
) -> Result<PrInventorySnapshot, P::Error> {
    let inventory = port.load()?.remove(&session_id).unwrap_or_default();
    Ok(PrInventorySnapshot {
        session_id,
        revision: inventory.revision,
        inventory,
    })
}

impl PrInventoryPort for crate::infrastructure::store::pr_inventory::PrInventoryStore {
    type Error = anyhow::Error;
    fn load(&self) -> Result<BTreeMap<SessionId, PrInventory>, Self::Error> {
        Ok(self.load()?.sessions)
    }
    fn save(&self, sessions: &BTreeMap<SessionId, PrInventory>) -> Result<(), Self::Error> {
        self.save(
            &crate::infrastructure::store::pr_inventory::PrInventoryStoreSnapshot {
                sessions: sessions.clone(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::pr_inventory::{PrInventory, canonicalize};
    use std::{cell::RefCell, collections::BTreeMap};
    #[derive(Default)]
    struct Memory(RefCell<BTreeMap<SessionId, PrInventory>>);
    impl PrInventoryPort for Memory {
        type Error = ();
        fn load(&self) -> Result<BTreeMap<SessionId, PrInventory>, ()> {
            Ok(self.0.borrow().clone())
        }
        fn save(&self, sessions: &BTreeMap<SessionId, PrInventory>) -> Result<(), ()> {
            *self.0.borrow_mut() = sessions.clone();
            Ok(())
        }
    }
    #[test]
    fn snapshot_defaults_and_returns_the_saved_revision() {
        let store = Memory::default();
        let session = SessionId::new();
        assert_eq!(snapshot(&store, session).unwrap().revision, 0);
        let mut inventory = PrInventory::default();
        inventory.discover([canonicalize("https://github.com/o/r/pull/1").unwrap()]);
        let mut sessions = BTreeMap::new();
        sessions.insert(session, inventory);
        store.save(&sessions).unwrap();
        assert_eq!(snapshot(&store, session).unwrap().revision, 1);
    }
    #[test]
    fn file_port_saves_and_loads_session_inventory() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            crate::infrastructure::store::pr_inventory::PrInventoryStore::new(directory.path());
        let session = SessionId::new();
        let mut sessions = BTreeMap::new();
        sessions.insert(session, PrInventory::default());
        PrInventoryPort::save(&store, &sessions).unwrap();
        assert_eq!(PrInventoryPort::load(&store).unwrap(), sessions);
    }
}
