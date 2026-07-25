use usagi_core::domain::daemon::DaemonRecord;
use usagi_core::domain::id::DaemonGeneration;

use super::*;
use crate::usecase::authority::handoff::PublishedLocator;
use crate::usecase::authority::registry::{REGISTRY_SCHEMA, RegistryError};

fn locator(generation: DaemonGeneration) -> LocatorObservation {
    LocatorObservation::Published(PublishedLocator {
        generation,
        endpoint: format!("generations/{}/sock", generation.as_str()),
    })
}

#[test]
fn an_exact_owner_with_a_readable_locator_becomes_the_single_active_generation() {
    let generation = DaemonGeneration::new();
    let record = DaemonRecord::identified(4321, "start-4321");
    let document = migrate_legacy(
        Some(&record),
        DaemonProcessObservation::Exact,
        &locator(generation),
        99,
    )
    .unwrap();

    assert_eq!(document.schema, REGISTRY_SCHEMA);
    assert_eq!(document.current, Some(generation));
    let entry = document.entry(generation).unwrap();
    assert_eq!(entry.role, GenerationRole::Active);
    assert_eq!(entry.process.pid, 4321);
    assert_eq!(entry.process.start_identity, "start-4321");
    assert_eq!(entry.process.process_group, 99);
    // A legacy owner never declared an artifact, so it may be handed off from
    // but can never be the target of a handoff.
    assert!(!entry.expected_build.is_known());
    assert!(!entry.is_build_verified());
    document.validate(2).unwrap();

    let mut adopted = document;
    assert_eq!(
        crate::usecase::authority::handoff::begin_handoff(
            &mut adopted,
            &crate::usecase::authority::fixture::operation("a"),
            None,
            generation,
        ),
        Err(RegistryError::InvalidTransition)
    );
}

#[test]
fn adoption_refuses_every_owner_it_cannot_prove() {
    let generation = DaemonGeneration::new();
    let identified = DaemonRecord::identified(4321, "start-4321");
    let cases: [(
        Option<&DaemonRecord>,
        DaemonProcessObservation,
        LocatorObservation,
        MigrationRefusal,
    ); 7] = [
        (
            None,
            DaemonProcessObservation::Exact,
            locator(generation),
            MigrationRefusal::NoRecord,
        ),
        (
            Some(&DaemonRecord::new(4321)),
            DaemonProcessObservation::Exact,
            locator(generation),
            MigrationRefusal::NoProcessIdentity,
        ),
        (
            Some(&identified),
            DaemonProcessObservation::Gone,
            locator(generation),
            MigrationRefusal::StaleOwner,
        ),
        (
            Some(&identified),
            DaemonProcessObservation::IdentityMismatch,
            locator(generation),
            MigrationRefusal::UnverifiedOwner,
        ),
        (
            Some(&identified),
            DaemonProcessObservation::Unknown,
            locator(generation),
            MigrationRefusal::UnverifiedOwner,
        ),
        (
            Some(&identified),
            DaemonProcessObservation::Exact,
            LocatorObservation::Absent,
            MigrationRefusal::MissingLocator,
        ),
        (
            Some(&identified),
            DaemonProcessObservation::Exact,
            LocatorObservation::Unreadable,
            MigrationRefusal::UnreadableLocator,
        ),
    ];

    for (record, observation, locator, expected) in cases {
        assert_eq!(
            migrate_legacy(record, observation, &locator, 99),
            Err(expected),
            "{observation:?} / {locator:?}"
        );
    }
}

#[test]
fn an_empty_identity_string_is_as_unprovable_as_a_missing_one() {
    let record = DaemonRecord {
        pid: 4321,
        process_start_identity: Some(String::new()),
        started_at: chrono::Utc::now(),
    };
    assert_eq!(
        migrate_legacy(
            Some(&record),
            DaemonProcessObservation::Exact,
            &locator(DaemonGeneration::new()),
            99,
        ),
        Err(MigrationRefusal::NoProcessIdentity)
    );
}
