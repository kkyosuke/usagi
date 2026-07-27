//! The inventory is complete, and a draining process cannot whole-save.

use std::cell::RefCell;
use std::collections::BTreeMap;

use usagi_core::domain::id::SessionId;
use usagi_core::domain::pr_inventory::PrInventory;
use usagi_core::usecase::pr_inventory::PrInventoryPort;

use super::{
    FencedError, FencedPrInventory, SHARED_WRITERS, SharedWriter, WriteMode, WriteVerdict,
    fenced_writers, shared_write_verdict,
};
use crate::usecase::generation::GenerationRole;

#[test]
fn only_the_active_generation_whole_saves_a_shared_document() {
    for writer in SHARED_WRITERS {
        assert_eq!(
            shared_write_verdict(writer, GenerationRole::Active),
            WriteVerdict::Allowed
        );
        for role in [GenerationRole::Standby, GenerationRole::Retired] {
            assert_eq!(
                shared_write_verdict(writer, role),
                WriteVerdict::Refused,
                "a standby is read-only and a retired generation writes nothing"
            );
        }
    }
}

#[test]
fn a_draining_generation_defers_its_only_deferrable_writer_and_is_refused_the_rest() {
    assert_eq!(
        shared_write_verdict(SharedWriter::PrInventory, GenerationRole::Draining),
        WriteVerdict::DeferToOutbox
    );
    for writer in [
        SharedWriter::SupervisorState,
        SharedWriter::SessionLifecycle,
    ] {
        assert_eq!(
            shared_write_verdict(writer, GenerationRole::Draining),
            WriteVerdict::Refused
        );
    }
    for writer in [
        SharedWriter::DispatchRegistry,
        SharedWriter::CompletionInbox,
    ] {
        assert_eq!(
            shared_write_verdict(writer, GenerationRole::Draining),
            WriteVerdict::Allowed,
            "an append under a cross-process lock cannot lose an update"
        );
    }
}

#[test]
fn the_inventory_names_every_writer_that_needs_the_fence() {
    assert_eq!(
        fenced_writers(),
        vec![
            SharedWriter::PrInventory,
            SharedWriter::SupervisorState,
            SharedWriter::SessionLifecycle
        ]
    );
    assert_eq!(
        SHARED_WRITERS
            .into_iter()
            .filter(|writer| writer.mode() == WriteMode::AppendOnly)
            .count(),
        2
    );
    assert!(SharedWriter::PrInventory.is_deferrable());
    assert!(!SharedWriter::SupervisorState.is_deferrable());
    assert_eq!(SharedWriter::CompletionInbox.mode(), WriteMode::AppendOnly);
}

/// A port that counts the writes it was actually asked to perform.
#[derive(Default)]
struct CountingPort {
    saves: RefCell<usize>,
    fail: bool,
}

impl PrInventoryPort for CountingPort {
    type Error = String;

    fn load(&self) -> Result<BTreeMap<SessionId, PrInventory>, Self::Error> {
        if self.fail {
            return Err("read failed".to_owned());
        }
        Ok(BTreeMap::new())
    }

    fn save(&self, _sessions: &BTreeMap<SessionId, PrInventory>) -> Result<(), Self::Error> {
        *self.saves.borrow_mut() += 1;
        if self.fail {
            return Err("write failed".to_owned());
        }
        Ok(())
    }
}

#[test]
fn only_the_active_generation_reaches_the_inventory_document() {
    let active = FencedPrInventory::new(CountingPort::default(), GenerationRole::Active);
    assert!(active.writable());
    active.save(&BTreeMap::new()).unwrap();
    assert!(active.load().unwrap().is_empty());
    assert_eq!(*active.port.saves.borrow(), 1);

    for role in [
        GenerationRole::Draining,
        GenerationRole::Standby,
        GenerationRole::Retired,
    ] {
        let fenced = FencedPrInventory::new(CountingPort::default(), role);
        assert!(!fenced.writable());
        let refusal = fenced.save(&BTreeMap::new()).unwrap_err();
        match refusal {
            FencedError::Refused(refusal) => {
                assert_eq!(refusal.writer, SharedWriter::PrInventory);
                assert_eq!(refusal.role, role);
                assert_eq!(
                    FencedError::<String>::Refused(refusal).to_string(),
                    refusal.to_string()
                );
                assert!(refusal.to_string().contains("PrInventory"));
            }
            FencedError::Port(error) => panic!("expected a fence refusal, got {error}"),
        }
        // The refusal never reaches the document, so no update can be lost.
        assert_eq!(*fenced.port.saves.borrow(), 0);
        // Observing is open to every role: a read cannot lose an update.
        assert!(fenced.load().unwrap().is_empty());
    }
}

#[test]
fn the_ports_own_failure_is_reported_as_itself() {
    let failing = FencedPrInventory::new(
        CountingPort {
            saves: RefCell::new(0),
            fail: true,
        },
        GenerationRole::Active,
    );
    assert_eq!(
        failing.save(&BTreeMap::new()).unwrap_err().to_string(),
        "write failed"
    );
    assert_eq!(failing.load().unwrap_err().to_string(), "read failed");
}
