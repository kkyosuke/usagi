//! The inventory is complete, and a draining process cannot whole-save.

use super::{
    SHARED_WRITERS, SharedWriter, WriteMode, WriteVerdict, fenced_writers, shared_write_verdict,
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
