//! Session PR inventory persistence port.

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::pr_inventory::PrInventory;
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
        let owned =
            crate::infrastructure::store::pr_inventory::PrInventoryStoreSnapshot { sessions };
        assert_eq!(
            std::fs::read_to_string(store.path()).unwrap(),
            format!("{}\n", serde_json::to_string_pretty(&owned).unwrap()),
            "borrowed writes preserve the owned persistence envelope"
        );
    }
}
