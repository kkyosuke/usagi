//! The compare-and-swap seam every durable object in this module shares.

use usagi_core::domain::id::OperationId;

use super::allocator::{ALLOCATOR_SCHEMA, AllocatorDocument, ExpiryClass, OperationTombstone};
use super::fixture::{FileFault, MemoryFile, SharedBytes};
use super::{CasDocument, CasStore, ResourceError, ResourceFailure};

fn store(bytes: &SharedBytes) -> CasStore<MemoryFile, AllocatorDocument> {
    CasStore::new(MemoryFile::new(bytes))
}

#[test]
fn absent_document_loads_as_the_caller_supplied_empty_one() {
    let bytes = SharedBytes::default();
    let snapshot = store(&bytes).load(AllocatorDocument::default).unwrap();
    assert_eq!(snapshot.document().revision, 0);
    assert!(snapshot.observed().is_none());
    assert_eq!(snapshot.to_document(), AllocatorDocument::default());
    assert!(bytes.get().is_none(), "loading never writes");
}

#[test]
fn unreadable_corrupt_and_unknown_schema_bytes_all_fail_closed() {
    let bytes = SharedBytes::default();
    let failing = CasStore::<MemoryFile, AllocatorDocument>::new(MemoryFile::faulty(
        &bytes,
        FileFault::ReadFails,
    ));
    let failure = failing.load(AllocatorDocument::default).unwrap_err();
    assert!(failure.refusal().is_none());
    assert!(format!("{failure}").contains("store failed"));

    bytes.set("{not json");
    assert_eq!(
        store(&bytes)
            .load(AllocatorDocument::default)
            .unwrap_err()
            .refusal(),
        Some(ResourceError::Corrupt)
    );

    let foreign = AllocatorDocument {
        schema: "other".to_owned(),
        ..AllocatorDocument::default()
    };
    bytes.set(&serde_json::to_string(&foreign).unwrap());
    assert_eq!(
        store(&bytes)
            .load(AllocatorDocument::default)
            .unwrap_err()
            .refusal(),
        Some(ResourceError::UnknownSchema)
    );
}

#[test]
fn a_second_writer_loses_the_swap_instead_of_overwriting_it() {
    let bytes = SharedBytes::default();
    let first = store(&bytes);
    let second = store(&bytes);
    let read_by_first = first.load(AllocatorDocument::default).unwrap();
    let read_by_second = second.load(AllocatorDocument::default).unwrap();

    let mut next = read_by_first.to_document();
    next.bump();
    let committed = first.commit(&read_by_first, next).unwrap();
    assert_eq!(committed.document().revision, 1);

    let mut stale = read_by_second.to_document();
    stale.bump();
    assert_eq!(
        second.commit(&read_by_second, stale).unwrap_err().refusal(),
        Some(ResourceError::StaleRevision)
    );
    assert_eq!(bytes.get().as_deref(), committed.observed());
}

#[test]
fn commit_requires_exactly_one_revision_step_and_a_valid_document() {
    let bytes = SharedBytes::default();
    let store = store(&bytes);
    let snapshot = store.load(AllocatorDocument::default).unwrap();

    let unchanged = snapshot.to_document();
    assert_eq!(
        store.commit(&snapshot, unchanged).unwrap_err().refusal(),
        Some(ResourceError::StaleRevision)
    );

    let mut invalid = snapshot.to_document();
    invalid.schema = "other".to_owned();
    invalid.bump();
    assert_eq!(
        store.commit(&snapshot, invalid).unwrap_err().refusal(),
        Some(ResourceError::UnknownSchema)
    );
    assert!(bytes.get().is_none(), "a refusal writes nothing");
}

#[test]
fn a_write_failure_and_a_lost_race_are_reported_differently() {
    let bytes = SharedBytes::default();
    let failing = CasStore::<MemoryFile, AllocatorDocument>::new(MemoryFile::faulty(
        &bytes,
        FileFault::WriteFails,
    ));
    let snapshot = failing.load(AllocatorDocument::default).unwrap();
    let mut next = snapshot.to_document();
    next.bump();
    assert!(
        failing
            .commit(&snapshot, next)
            .unwrap_err()
            .refusal()
            .is_none()
    );

    let racing = CasStore::<MemoryFile, AllocatorDocument>::new(MemoryFile::faulty(
        &bytes,
        FileFault::AlwaysStale,
    ));
    let snapshot = racing.load(AllocatorDocument::default).unwrap();
    let mut next = snapshot.to_document();
    next.bump();
    assert_eq!(
        racing.commit(&snapshot, next).unwrap_err().refusal(),
        Some(ResourceError::StaleRevision)
    );
}

#[test]
fn a_converged_update_writes_nothing_and_a_refused_one_commits_nothing() {
    let bytes = SharedBytes::default();
    let store = store(&bytes);
    let (value, snapshot) = store
        .update(AllocatorDocument::default, |_| Ok("unchanged"))
        .unwrap();
    assert_eq!(value, "unchanged");
    assert_eq!(snapshot.document().revision, 0);
    assert!(bytes.get().is_none());

    let error = store
        .update(AllocatorDocument::default, |_| {
            Err::<(), _>(ResourceError::WrongState)
        })
        .unwrap_err();
    assert_eq!(error.refusal(), Some(ResourceError::WrongState));
    assert!(bytes.get().is_none());

    let ((), snapshot) = store
        .update(AllocatorDocument::default, |document| {
            assert_eq!(document.schema, ALLOCATOR_SCHEMA);
            document.tombstones.push(OperationTombstone {
                operation: OperationId::new(),
                digest: "d".to_owned(),
                class: ExpiryClass::Failed,
                cutoff: 1,
            });
            Ok(())
        })
        .unwrap();
    assert_eq!(snapshot.document().revision, 1);
    assert!(bytes.get().is_some());
}

#[test]
fn every_refusal_and_failure_renders_a_distinct_message() {
    let refusals = [
        ResourceError::UnknownSchema,
        ResourceError::Corrupt,
        ResourceError::StaleRevision,
        ResourceError::ForeignOwner,
        ResourceError::CapacityExhausted,
        ResourceError::OperationConflict,
        ResourceError::OperationExpired,
        ResourceError::RetentionBackpressure,
        ResourceError::UnknownOperation,
        ResourceError::UnknownResource,
        ResourceError::DuplicateResource,
        ResourceError::WrongOwner,
        ResourceError::WrongState,
        ResourceError::OwnershipUnknown,
        ResourceError::IdentityUnverifiable,
        ResourceError::SealedElsewhere,
        ResourceError::NotCollectable,
    ];
    let mut rendered: Vec<String> = refusals
        .iter()
        .map(|refusal| {
            let failure = ResourceFailure::from(*refusal);
            assert_eq!(failure.refusal(), Some(*refusal));
            assert_eq!(format!("{failure}"), format!("{refusal}"));
            format!("{refusal}")
        })
        .collect();
    rendered.sort();
    let total = rendered.len();
    rendered.dedup();
    assert_eq!(rendered.len(), total);

    let io = ResourceFailure::from(std::io::Error::other("broken"));
    assert!(io.refusal().is_none());
    assert!(format!("{io}").contains("broken"));
    assert!(std::error::Error::source(&io).is_none());
    assert!(std::error::Error::source(&ResourceError::Corrupt).is_none());
}
