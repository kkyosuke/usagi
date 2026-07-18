//! Incremental projection of committed PTY output into durable PR inventories.

use std::collections::{BTreeMap, VecDeque};
use usagi_core::{
    domain::{
        id::{SessionId, TerminalId},
        pr_inventory::extract,
    },
    usecase::pr_inventory::PrInventoryPort,
};

/// Parses only bytes supplied after the terminal journal has committed them.
pub struct OutputPrProjector<P> {
    store: P,
    tails: BTreeMap<TerminalId, VecDeque<u8>>,
}
impl<P: PrInventoryPort> OutputPrProjector<P> {
    #[must_use]
    pub fn new(store: P) -> Self {
        Self {
            store,
            tails: BTreeMap::new(),
        }
    }
    /// Projects a committed terminal segment. Root terminals have no session inventory.
    ///
    /// # Errors
    ///
    /// Returns the durable inventory port's read or write error.
    pub fn observe_committed(
        &mut self,
        terminal: TerminalId,
        session: Option<SessionId>,
        bytes: &[u8],
    ) -> Result<bool, P::Error> {
        let Some(session) = session else {
            return Ok(false);
        };
        let tail = self.tails.entry(terminal).or_default();
        let mut combined: Vec<u8> = tail.iter().copied().collect();
        combined.extend_from_slice(bytes);
        let identities = extract(&combined);
        tail.extend(bytes.iter().copied());
        while tail.len() > 4096 {
            tail.pop_front();
        }
        let mut sessions = self.store.load()?;
        let changed = sessions.entry(session).or_default().discover(identities);
        if changed {
            self.store.save(&sessions)?;
        }
        Ok(changed)
    }
    #[must_use]
    pub fn into_store(self) -> P {
        self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::BTreeMap};
    use usagi_core::{domain::pr_inventory::PrState, usecase::pr_inventory::PrInventoryPort};
    #[derive(Default)]
    struct Store(RefCell<BTreeMap<SessionId, usagi_core::domain::pr_inventory::PrInventory>>);
    impl PrInventoryPort for Store {
        type Error = ();
        fn load(
            &self,
        ) -> Result<BTreeMap<SessionId, usagi_core::domain::pr_inventory::PrInventory>, ()>
        {
            Ok(self.0.borrow().clone())
        }
        fn save(
            &self,
            value: &BTreeMap<SessionId, usagi_core::domain::pr_inventory::PrInventory>,
        ) -> Result<(), ()> {
            *self.0.borrow_mut() = value.clone();
            Ok(())
        }
    }
    #[test]
    fn joins_split_chunks_and_deduplicates_replay() {
        let session = SessionId::new();
        let terminal = TerminalId::new();
        let mut projector = OutputPrProjector::new(Store::default());
        assert!(
            !projector
                .observe_committed(terminal, Some(session), b"https://github.com/o/r/p")
                .unwrap()
        );
        assert!(
            projector
                .observe_committed(terminal, Some(session), b"ull/42\n")
                .unwrap()
        );
        assert!(
            !projector
                .observe_committed(terminal, Some(session), b"https://github.com/o/r/pull/42\n")
                .unwrap()
        );
        let store = projector.into_store();
        assert_eq!(store.0.borrow()[&session].entries.len(), 1);
    }
    #[test]
    fn separates_sessions_and_keeps_user_tombstone() {
        let a = SessionId::new();
        let b = SessionId::new();
        let mut projector = OutputPrProjector::new(Store::default());
        let terminal = TerminalId::new();
        projector
            .observe_committed(terminal, Some(a), b"https://github.com/o/r/pull/1\n")
            .unwrap();
        let id = projector.store.0.borrow()[&a]
            .entries
            .keys()
            .next()
            .unwrap()
            .clone();
        projector
            .store
            .0
            .borrow_mut()
            .get_mut(&a)
            .unwrap()
            .set_user_state(&id, PrState::Dismissed, true);
        projector
            .observe_committed(terminal, Some(a), b"https://github.com/o/r/pull/1\n")
            .unwrap();
        projector
            .observe_committed(
                TerminalId::new(),
                Some(b),
                b"https://github.com/o/r/pull/1\n",
            )
            .unwrap();
        assert_eq!(
            projector.store.0.borrow()[&a].entries[&id].state,
            PrState::Dismissed
        );
        assert_eq!(projector.store.0.borrow()[&b].entries.len(), 1);
    }
    #[test]
    fn ignores_root_output_and_bounds_the_terminal_tail() {
        let mut projector = OutputPrProjector::new(Store::default());
        let terminal = TerminalId::new();
        assert!(
            !projector
                .observe_committed(terminal, None, b"https://github.com/o/r/pull/1\n")
                .unwrap()
        );
        let session = SessionId::new();
        projector
            .observe_committed(terminal, Some(session), &vec![b'x'; 4097])
            .unwrap();
        assert_eq!(projector.tails[&terminal].len(), 4096);
    }
}
