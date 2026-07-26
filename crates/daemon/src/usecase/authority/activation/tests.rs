use std::error::Error as _;
use std::sync::Arc;

use usagi_core::domain::id::DaemonGeneration;

use super::*;
use crate::usecase::authority::fixture::{
    MemoryLocator, MemoryRegistryFile, build, operation, process, registry, unknown_build,
};
use crate::usecase::authority::handoff::RecoveryRefusal;
use crate::usecase::authority::registry::{
    GenerationEntry, HandoffPhase, HandoffRecord, REGISTRY_SCHEMA, RegistryDocument, RegistryError,
};
use crate::usecase::generation::GenerationRole;

struct World {
    store: GenerationRegistry,
    file: Arc<MemoryRegistryFile>,
    locator: MemoryLocator,
    generation: DaemonGeneration,
}

impl World {
    fn document(&self) -> RegistryDocument {
        self.store.load().unwrap().to_document()
    }
}

const ENDPOINT: &str = "generations/self/sock";

/// This process's own identity, as `serve` observes it before claiming.
fn own_process() -> ProcessIdentity {
    process(SELF_PID)
}

const SELF_PID: u32 = 9001;

fn world() -> World {
    let (store, file) = registry(2);
    World {
        store,
        file,
        locator: MemoryLocator::default(),
        generation: DaemonGeneration::new(),
    }
}

/// Every observation this suite makes: the process it is asked about is alive
/// exactly when it is in `alive`.
fn observer(alive: Vec<ProcessIdentity>) -> impl FnMut(&ProcessIdentity) -> ProcessObservation {
    move |process| {
        if alive.contains(process) {
            ProcessObservation::VerifiedAlive(process.clone())
        } else {
            ProcessObservation::Gone
        }
    }
}

fn claim(world: &World, alive: Vec<ProcessIdentity>) -> Result<AuthorityClaimed, ClaimFailure> {
    claim_as(world, alive, &build("self"))
}

fn claim_as(
    world: &World,
    alive: Vec<ProcessIdentity>,
    artifact: &BuildIdentity,
) -> Result<AuthorityClaimed, ClaimFailure> {
    let process = own_process();
    claim_authority(
        &world.store,
        &world.locator,
        &AuthorityClaim {
            generation: world.generation,
            endpoint: ENDPOINT,
            process: &process,
            build: artifact,
        },
        &mut observer(alive),
    )
}

/// A leftover registry naming `generation` as the live active authority.
fn active_entry(generation: DaemonGeneration, pid: u32) -> GenerationEntry {
    GenerationEntry {
        generation,
        role: GenerationRole::Active,
        endpoint: format!("generations/{pid}/sock"),
        process: process(pid),
        expected_build: build("previous"),
        verified_build: Some(build("previous")),
        revision: 1,
    }
}

fn seed(file: &Arc<MemoryRegistryFile>, document: &RegistryDocument) {
    file.set_contents(Some(&serde_json::to_string(document).unwrap()));
}

#[test]
fn a_fresh_start_becomes_the_single_active_generation_and_publishes_current() {
    let world = world();

    let claimed = claim(&world, Vec::new()).unwrap();

    assert_eq!(claimed.recovery, RecoveryOutcome::Consistent);
    let document = world.document();
    assert_eq!(document.schema, REGISTRY_SCHEMA);
    assert_eq!(document.current, Some(world.generation));
    let entry = document.entry(world.generation).unwrap();
    assert_eq!(entry.role, GenerationRole::Active);
    assert_eq!(entry.endpoint, ENDPOINT);
    assert_eq!(entry.process, own_process());
    // Its own artifact is verified by construction: the process that registered
    // is the process serving, so no second peer is needed to confirm it.
    assert!(entry.is_build_verified());
    assert_eq!(
        world.locator.publishes(),
        vec![PublishedLocator {
            generation: world.generation,
            endpoint: ENDPOINT.to_owned(),
        }]
    );
}

#[test]
fn the_registry_names_the_generation_before_the_locator_does() {
    let world = world();
    world.locator.fail_publish(true);

    let failure = claim(&world, Vec::new()).unwrap_err();

    assert!(matches!(failure, ClaimFailure::Locator(_)));
    // W1 happened, W2 did not: the entry exists and nothing is discoverable.
    assert_eq!(world.document().current, Some(world.generation));
    assert!(world.locator.publishes().is_empty());
}

#[test]
fn a_claim_left_half_written_by_a_crash_is_retired_by_the_next_start() {
    let crashed = world();
    crashed.locator.fail_publish(true);
    claim(&crashed, Vec::new()).unwrap_err();

    // The next process reads the same registry and cannot prove the recorded
    // authority is alive, so it fails the leftover closed before claiming.
    let next = World {
        store: GenerationRegistry::new(Arc::clone(&crashed.file), 2),
        file: Arc::clone(&crashed.file),
        locator: crashed.locator,
        generation: DaemonGeneration::new(),
    };
    next.locator.fail_publish(false);

    let claimed = claim(&next, Vec::new()).unwrap();

    assert_eq!(
        claimed.recovery,
        RecoveryOutcome::FailedClosed(RecoveryRefusal::ActiveGone)
    );
    let document = next.document();
    assert_eq!(document.current, Some(next.generation));
    // The crashed generation's record is gone rather than retained as retired:
    // one restart must not cost one entry forever.
    assert_eq!(document.generations.len(), 1);
    assert_eq!(document.entry(crashed.generation), None);
}

#[test]
fn retired_leftovers_are_discarded_so_the_document_stays_bounded() {
    let world = world();
    let mut seeded = RegistryDocument::default();
    for pid in 1..=5 {
        let mut entry = active_entry(DaemonGeneration::new(), pid);
        entry.role = GenerationRole::Retired;
        seeded.generations.push(entry);
    }
    seed(&world.file, &seeded);

    claim(&world, Vec::new()).unwrap();

    assert_eq!(world.document().generations.len(), 1);
}

#[test]
fn a_live_active_generation_is_never_displaced_by_a_claim() {
    let world = world();
    let holder = DaemonGeneration::new();
    let mut seeded = RegistryDocument::default();
    seeded.generations.push(active_entry(holder, 4242));
    seeded.current = Some(holder);
    seed(&world.file, &seeded);

    let failure = claim(&world, vec![process(4242)]).unwrap_err();

    assert!(matches!(
        failure,
        ClaimFailure::Registry(failure)
            if failure.refusal() == Some(RegistryError::AuthorityRetained)
    ));
    let document = world.document();
    assert_eq!(document.current, Some(holder));
    assert_eq!(document.entry(world.generation), None);
    // Recovery repaired the live holder's own locator; this process's endpoint
    // was never published, so no client can have been sent to it.
    assert!(
        world
            .locator
            .publishes()
            .iter()
            .all(|published| published.generation == holder)
    );
}

#[test]
fn an_in_flight_handoff_is_left_to_the_handoff_protocol() {
    let world = world();
    let holder = DaemonGeneration::new();
    let mut seeded = RegistryDocument::default();
    let mut entry = active_entry(holder, 4242);
    entry.role = GenerationRole::Standby;
    seeded.generations.push(entry);
    seeded.handoff = Some(HandoffRecord {
        operation: operation("in-flight"),
        from: None,
        to: holder,
        endpoint: "generations/4242/sock".into(),
        phase: HandoffPhase::Preparing,
    });
    seed(&world.file, &seeded);

    // Recovery aborts an intent that never became observable, which leaves the
    // standby retained — and a retained generation is the handoff's business.
    let failure = claim(&world, vec![process(4242)]).unwrap_err();

    assert!(matches!(
        failure,
        ClaimFailure::Registry(failure)
            if failure.refusal() == Some(RegistryError::AuthorityRetained)
    ));
    assert_eq!(world.document().entry(world.generation), None);
}

#[test]
fn an_unreadable_registry_refuses_the_claim_without_writing_anything() {
    let world = world();
    world.file.fail_read(true);

    let failure = claim(&world, Vec::new()).unwrap_err();

    assert!(matches!(failure, ClaimFailure::Recovery(_)));
    assert!(failure.source().is_none());
    assert!(failure.to_string().contains("generation recovery failed"));
    assert_eq!(world.file.writes(), 0);
    assert!(world.locator.publishes().is_empty());
}

#[test]
fn a_registry_that_cannot_be_written_refuses_the_claim() {
    let world = world();
    world.file.fail_write(true);

    let failure = claim(&world, Vec::new()).unwrap_err();

    assert!(matches!(failure, ClaimFailure::Registry(_)));
    assert!(failure.to_string().contains("generation registry store"));
    assert!(world.locator.publishes().is_empty());
}

#[test]
fn a_build_that_cannot_name_itself_still_serves_but_is_no_rollover_successor() {
    let world = world();
    claim_as(&world, Vec::new(), &unknown_build()).unwrap();

    let document = world.document();
    let entry = document.entry(world.generation).unwrap();
    assert_eq!(entry.role, GenerationRole::Active);
    assert_eq!(entry.verified_build, None);
    assert!(!entry.is_build_verified());
}

#[test]
fn repeating_a_claim_writes_nothing_at_all() {
    let world = world();
    claim(&world, Vec::new()).unwrap();
    let writes = world.file.writes();

    claim(&world, vec![own_process()]).unwrap();

    assert_eq!(world.file.writes(), writes);
    assert_eq!(world.document().generations.len(), 1);
}

#[test]
fn releasing_gives_up_the_authority_and_is_idempotent() {
    let world = world();
    claim(&world, Vec::new()).unwrap();

    release_authority(&world.store, world.generation).unwrap();
    let after = world.document();
    assert_eq!(after.current, None);
    assert_eq!(after.role(world.generation), Some(GenerationRole::Retired));

    let writes = world.file.writes();
    release_authority(&world.store, world.generation).unwrap();
    assert_eq!(world.file.writes(), writes);
}

#[test]
fn releasing_a_generation_that_never_claimed_is_not_a_failure() {
    let world = world();

    release_authority(&world.store, world.generation).unwrap();

    assert_eq!(world.file.writes(), 0);
}

#[test]
fn a_release_that_cannot_be_written_is_reported() {
    let world = world();
    claim(&world, Vec::new()).unwrap();
    world.file.fail_write(true);

    let failure = release_authority(&world.store, world.generation).unwrap_err();

    assert!(failure.refusal().is_none());
}

#[test]
fn a_claim_failure_converts_to_an_io_error_for_the_serve_state_machine() {
    let world = world();
    world.locator.fail_publish(true);
    let failure = claim(&world, Vec::new()).unwrap_err();
    let message = failure.to_string();

    let error: io::Error = failure.into();

    assert_eq!(error.to_string(), message);
    assert!(message.contains("current locator failed"));
}
