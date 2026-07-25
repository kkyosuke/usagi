use std::error::Error as _;
use std::sync::Arc;
use std::sync::mpsc::channel;

use usagi_core::domain::id::DaemonGeneration;

use super::*;
use crate::usecase::authority::admission::{AdmissionGate, RequestClass, ResourceOwner};
use crate::usecase::authority::fixture::{
    MemoryLocator, MemoryRegistryFile, build, operation, process, registry,
};
use crate::usecase::authority::registry::{
    GenerationEntry, HandoffPhase, RegistryError, RegistrySnapshot,
};
use crate::usecase::authority::workers::{ClientWorkers, ConnectionShutdown};

const STEPS: [HandoffStep; 10] = [
    HandoffStep::BeforeIntent,
    HandoffStep::AfterIntent,
    HandoffStep::BeforeBarrier,
    HandoffStep::AfterBarrier,
    HandoffStep::BeforeRegistryCommit,
    HandoffStep::AfterRegistryCommit,
    HandoffStep::BeforeLocatorPublish,
    HandoffStep::AfterLocatorPublish,
    HandoffStep::BeforeComplete,
    HandoffStep::AfterComplete,
];

struct World {
    store: GenerationRegistry<Arc<MemoryRegistryFile>>,
    file: Arc<MemoryRegistryFile>,
    locator: MemoryLocator,
    gate: AdmissionGate,
    old: DaemonGeneration,
    next: DaemonGeneration,
}

impl World {
    fn document(&self) -> RegistrySnapshot {
        self.store.load().unwrap()
    }

    fn old_locator(&self) -> PublishedLocator {
        PublishedLocator {
            generation: self.old,
            endpoint: "generations/old/sock".into(),
        }
    }

    fn next_locator(&self) -> PublishedLocator {
        PublishedLocator {
            generation: self.next,
            endpoint: "generations/next/sock".into(),
        }
    }
}

fn world() -> World {
    let (store, file) = registry(2);
    let old = DaemonGeneration::new();
    let next = DaemonGeneration::new();
    store
        .update(|document| {
            document.generations.push(GenerationEntry {
                generation: old,
                role: GenerationRole::Active,
                endpoint: "generations/old/sock".into(),
                process: process(1),
                expected_build: build("old"),
                verified_build: None,
                revision: 1,
            });
            document.current = Some(old);
            Ok(())
        })
        .unwrap();
    store
        .update(|document| {
            document.register_standby(2, next, "generations/next/sock", process(2), build("next"))
        })
        .unwrap();
    store
        .update(|document| document.verify_standby_build(next, &build("next")))
        .unwrap();
    let locator = MemoryLocator::naming(PublishedLocator {
        generation: old,
        endpoint: "generations/old/sock".into(),
    });
    let gate = AdmissionGate::new(old, GenerationRole::Active);
    World {
        store,
        file,
        locator,
        gate,
        old,
        next,
    }
}

fn both_alive(process: &ProcessIdentity) -> ProcessObservation {
    ProcessObservation::VerifiedAlive(process.clone())
}

fn none_alive(_: &ProcessIdentity) -> ProcessObservation {
    ProcessObservation::Gone
}

#[test]
fn a_rollover_publishes_the_successor_only_after_the_registry_commit() {
    let world = world();
    let op = operation("a");
    let mut observed_at_publish = None;

    let outcome = execute_rollover_with(
        &world.store,
        &world.locator,
        Some(&world.gate),
        &op,
        Some(world.old),
        world.next,
        &mut |step| {
            if step == HandoffStep::BeforeLocatorPublish {
                observed_at_publish = Some(world.store.load().unwrap().to_document());
            }
            Ok(())
        },
    )
    .unwrap();

    assert_eq!(outcome, RolloverOutcome::Advanced);
    // The registry already named the successor before the locator moved, so a
    // client that follows the locator can only ever land on a committed
    // authority.
    let at_publish = observed_at_publish.unwrap();
    assert_eq!(at_publish.current, Some(world.next));
    assert_eq!(at_publish.handoff.unwrap().phase, HandoffPhase::Committed);

    assert_eq!(world.locator.publishes(), vec![world.next_locator()]);
    let document = world.document();
    assert_eq!(document.document().current, Some(world.next));
    assert_eq!(
        document.document().role(world.old),
        Some(GenerationRole::Draining)
    );
    assert!(document.document().handoff.is_none());
    assert_eq!(document.document().completed_operation, Some(op));
    assert_eq!(world.gate.role(), GenerationRole::Draining);
    // The old generation can no longer spawn, but still serves its terminals.
    assert_eq!(
        world
            .gate
            .admit(RequestClass::Spawn, ResourceOwner::Unscoped)
            .unwrap_err()
            .to_string(),
        "generation does not hold control authority"
    );
    assert!(
        world
            .gate
            .admit(RequestClass::TerminalIo, ResourceOwner::SelfGeneration)
            .unwrap()
            .is_some()
    );
}

#[test]
fn the_first_activation_needs_no_predecessor_and_no_barrier() {
    let (store, _) = registry(2);
    let locator = MemoryLocator::default();
    let generation = DaemonGeneration::new();
    store
        .update(|document| {
            document.register_standby(
                2,
                generation,
                "generations/first/sock",
                process(7),
                build("next"),
            )
        })
        .unwrap();
    store
        .update(|document| document.verify_standby_build(generation, &build("next")))
        .unwrap();

    let op = operation("first");
    execute_rollover(&store, &locator, None, &op, None, generation).unwrap();
    assert_eq!(store.load().unwrap().document().current, Some(generation));
    assert_eq!(
        locator.publishes(),
        vec![PublishedLocator {
            generation,
            endpoint: "generations/first/sock".into(),
        }]
    );
}

#[test]
fn a_crash_at_any_boundary_recovers_to_exactly_one_authority() {
    for step in STEPS {
        let world = world();
        let op = operation("a");
        let crashed = execute_rollover_with(
            &world.store,
            &world.locator,
            Some(&world.gate),
            &op,
            Some(world.old),
            world.next,
            &mut |reached| {
                if reached == step {
                    Err(io::Error::other("process killed"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(crashed.is_err(), "{step:?}");

        // A fresh process reconciles both durable objects.
        let outcome = recover(&world.store, &world.locator, &mut both_alive).unwrap();
        let document = world.document();
        let document = document.document();
        document.validate(2).unwrap();
        assert!(document.handoff.is_none(), "{step:?}");

        let committed = matches!(
            step,
            HandoffStep::AfterRegistryCommit
                | HandoffStep::BeforeLocatorPublish
                | HandoffStep::AfterLocatorPublish
                | HandoffStep::BeforeComplete
                | HandoffStep::AfterComplete
        );
        let (expected_current, expected_locator) = if committed {
            (world.next, world.next_locator())
        } else {
            (world.old, world.old_locator())
        };
        assert_eq!(document.current, Some(expected_current), "{step:?}");
        assert_eq!(
            world.locator.read().unwrap(),
            LocatorObservation::Published(expected_locator),
            "{step:?}"
        );
        match (step, &outcome) {
            (HandoffStep::BeforeIntent | HandoffStep::AfterComplete, outcome) => {
                assert_eq!(*outcome, RecoveryOutcome::Consistent, "{step:?}");
            }
            (_, outcome) if committed => {
                assert_eq!(*outcome, RecoveryOutcome::RolledForward(op), "{step:?}");
            }
            (_, outcome) => assert_eq!(*outcome, RecoveryOutcome::AbortedIntent(op), "{step:?}"),
        }

        // Recovery is idempotent: replaying it changes nothing.
        assert_eq!(
            recover(&world.store, &world.locator, &mut both_alive).unwrap(),
            RecoveryOutcome::Consistent,
            "{step:?}"
        );
    }
}

#[test]
fn a_crash_that_takes_both_processes_converges_on_no_authority() {
    for step in STEPS {
        let world = world();
        let op = operation("a");
        let _ = execute_rollover_with(
            &world.store,
            &world.locator,
            Some(&world.gate),
            &op,
            Some(world.old),
            world.next,
            &mut |reached| {
                if reached == step {
                    Err(io::Error::other("process killed"))
                } else {
                    Ok(())
                }
            },
        );

        let outcome = recover(&world.store, &world.locator, &mut none_alive).unwrap();
        assert!(
            matches!(outcome, RecoveryOutcome::FailedClosed(_)),
            "{step:?}: {outcome:?}"
        );
        let document = world.document();
        let document = document.document();
        document.validate(2).unwrap();
        assert_eq!(document.current, None, "{step:?}");
        assert_eq!(document.retained(), 0, "{step:?}");
        assert_eq!(
            world.locator.read().unwrap(),
            LocatorObservation::Absent,
            "{step:?}"
        );
    }
}

#[test]
fn a_failed_registry_commit_reopens_the_barrier_and_keeps_the_old_authority() {
    let world = world();
    let op = operation("a");
    let failure = execute_rollover_with(
        &world.store,
        &world.locator,
        Some(&world.gate),
        &op,
        Some(world.old),
        world.next,
        &mut |step| {
            if step == HandoffStep::BeforeRegistryCommit {
                Err(io::Error::other("registry unavailable"))
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();
    assert!(failure.to_string().contains("registry unavailable"));

    // Nothing outside this process saw the barrier, so control work resumes.
    assert_eq!(world.gate.role(), GenerationRole::Active);
    assert!(
        world
            .gate
            .admit(RequestClass::Spawn, ResourceOwner::Unscoped)
            .unwrap()
            .is_some()
    );
    assert_eq!(world.document().document().current, Some(world.old));
    assert_eq!(
        world.locator.read().unwrap(),
        LocatorObservation::Published(world.old_locator())
    );

    // The same operation can be retried and completes.
    execute_rollover(
        &world.store,
        &world.locator,
        Some(&world.gate),
        &op,
        Some(world.old),
        world.next,
    )
    .unwrap();
    assert_eq!(world.document().document().current, Some(world.next));
}

#[test]
fn a_failed_locator_publish_leaves_a_committed_handoff_that_rolls_forward() {
    let world = world();
    let op = operation("a");
    world.locator.fail_publish(true);
    let failure = execute_rollover(
        &world.store,
        &world.locator,
        Some(&world.gate),
        &op,
        Some(world.old),
        world.next,
    )
    .unwrap_err();
    assert!(failure.to_string().contains("current locator failed"));
    assert!(failure.source().is_none());

    // The commit is durable, so the old authority is never restored.
    assert_eq!(world.document().document().current, Some(world.next));
    assert_eq!(world.gate.role(), GenerationRole::Draining);
    assert_eq!(
        world.gate.abort_draining().unwrap_err().to_string(),
        "generation does not hold control authority"
    );

    world.locator.fail_publish(false);
    assert_eq!(
        recover(&world.store, &world.locator, &mut both_alive).unwrap(),
        RecoveryOutcome::RolledForward(op)
    );
    assert_eq!(world.locator.publishes(), vec![world.next_locator()]);
}

#[test]
fn a_lost_ack_replays_into_the_same_result_without_a_second_effect() {
    let world = world();
    let op = operation("a");
    execute_rollover(
        &world.store,
        &world.locator,
        Some(&world.gate),
        &op,
        Some(world.old),
        world.next,
    )
    .unwrap();
    let writes = world.file.writes();

    // The client never saw the reply and retries the same operation.
    assert_eq!(
        execute_rollover(
            &world.store,
            &world.locator,
            Some(&world.gate),
            &op,
            Some(world.old),
            world.next,
        )
        .unwrap(),
        RolloverOutcome::AlreadyCompleted
    );
    assert_eq!(world.file.writes(), writes);
    assert_eq!(world.locator.publishes().len(), 1);
    assert_eq!(world.document().document().retained(), 2);
}

#[test]
fn a_concurrent_rollover_for_a_different_operation_is_refused() {
    let world = world();
    let first = operation("a");
    let second = operation("b");
    execute_rollover_with(
        &world.store,
        &world.locator,
        Some(&world.gate),
        &first,
        Some(world.old),
        world.next,
        &mut |step| {
            if step == HandoffStep::AfterIntent {
                Err(io::Error::other("process paused"))
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();

    let failure = execute_rollover(
        &world.store,
        &world.locator,
        Some(&world.gate),
        &second,
        Some(world.old),
        world.next,
    )
    .unwrap_err();
    assert!(matches!(
        &failure,
        HandoffFailure::Registry(registry)
            if registry.refusal() == Some(RegistryError::HandoffInProgress)
    ));
    assert_eq!(world.document().document().current, Some(world.old));
    // The generation limit still holds: no third process was admitted.
    assert_eq!(world.document().document().retained(), 2);
}

#[test]
fn the_handoff_waits_for_an_effect_that_is_already_running() {
    let world = Arc::new(world());
    let op = operation("a");
    let lease = world
        .gate
        .admit(RequestClass::Spawn, ResourceOwner::Unscoped)
        .unwrap()
        .unwrap();
    let (committed, observe_commit) = channel();

    let handoff = {
        let world = Arc::clone(&world);
        let op = op.clone();
        std::thread::spawn(move || {
            let outcome = execute_rollover(
                &world.store,
                &world.locator,
                Some(&world.gate),
                &op,
                Some(world.old),
                world.next,
            )
            .unwrap();
            committed.send(()).unwrap();
            outcome
        })
    };

    // While the spawn holds its lease the authority cannot move.
    assert_eq!(
        observe_commit.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    );
    assert_eq!(world.document().document().current, Some(world.old));
    lease.revalidate().unwrap();
    drop(lease);

    assert_eq!(handoff.join().unwrap(), RolloverOutcome::Advanced);
    observe_commit.recv().unwrap();
    assert_eq!(world.document().document().current, Some(world.next));
}

struct FakeConnection(std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>);

impl ConnectionShutdown for FakeConnection {
    fn shutdown(&self) -> io::Result<()> {
        drop(self.0.lock().unwrap().take());
        Ok(())
    }
}

#[test]
fn collection_joins_every_client_worker_before_recording_retirement() {
    let world = world();
    let op = operation("a");
    execute_rollover(
        &world.store,
        &world.locator,
        Some(&world.gate),
        &op,
        Some(world.old),
        world.next,
    )
    .unwrap();

    let workers = ClientWorkers::new();
    let (sender, receiver) = channel();
    let joined = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = {
        let joined = Arc::clone(&joined);
        std::thread::spawn(move || {
            assert!(receiver.recv().is_err());
            joined.store(true, std::sync::atomic::Ordering::Release);
        })
    };
    workers.register(
        Box::new(FakeConnection(std::sync::Mutex::new(Some(sender)))),
        handle,
    );

    // The draining owner keeps serving its terminals until the last owned
    // operation finishes; collection waits on that barrier before retiring.
    let owned = world.gate.acquire(LeaseClass::OwnerTerminal).unwrap();
    assert_eq!(world.gate.outstanding(LeaseClass::OwnerTerminal), 1);
    drop(owned);
    let barrier = collect_retired(&world.store, &world.gate, &workers, world.old).unwrap();

    assert!(barrier.is_clean());
    assert_eq!(barrier.joined, 1);
    assert!(joined.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(world.gate.role(), GenerationRole::Retired);
    assert_eq!(
        world.document().document().role(world.old),
        Some(GenerationRole::Retired)
    );
    // The freed slot lets a further generation be admitted.
    assert_eq!(world.document().document().retained(), 1);
}

#[test]
fn recovery_surfaces_a_durable_failure_instead_of_guessing() {
    let world = world();
    world.locator.fail_read(true);
    assert!(matches!(
        recover(&world.store, &world.locator, &mut both_alive),
        Err(HandoffFailure::Locator(_))
    ));

    world.locator.fail_read(false);
    world.locator.fail_retire(true);
    assert!(matches!(
        recover(&world.store, &world.locator, &mut none_alive),
        Err(HandoffFailure::Locator(_))
    ));
    // Nothing was committed, so the next attempt sees the same state.
    assert_eq!(world.document().document().current, Some(world.old));

    world.locator.fail_retire(false);
    world.file.fail_read(true);
    assert!(matches!(
        recover(&world.store, &world.locator, &mut both_alive),
        Err(HandoffFailure::Registry(_))
    ));
    world.file.fail_read(false);

    // A locator that has to be republished, but cannot be.
    world.locator.retire().unwrap();
    world.locator.fail_publish(true);
    assert!(matches!(
        recover(&world.store, &world.locator, &mut both_alive),
        Err(HandoffFailure::Locator(_))
    ));

    // The repair is written to the locator before the registry, so a registry
    // failure leaves the plan replayable rather than half applied.
    world.locator.fail_publish(false);
    world.file.fail_write(true);
    assert!(matches!(
        recover(&world.store, &world.locator, &mut none_alive),
        Err(HandoffFailure::Registry(_))
    ));
    world.file.fail_write(false);
    assert_eq!(world.document().document().current, Some(world.old));
}

#[test]
fn an_unreadable_locator_is_replaced_by_the_live_active_endpoint() {
    let world = world();
    world.locator.make_unreadable();
    assert_eq!(
        recover(&world.store, &world.locator, &mut both_alive).unwrap(),
        RecoveryOutcome::RepairedCurrent
    );
    assert_eq!(world.locator.publishes(), vec![world.old_locator()]);
    assert_eq!(world.document().document().current, Some(world.old));
}

#[test]
fn a_rollover_onto_an_unknown_successor_is_refused_before_any_write() {
    let world = world();
    let op = operation("a");
    let failure = execute_rollover(
        &world.store,
        &world.locator,
        Some(&world.gate),
        &op,
        Some(world.old),
        DaemonGeneration::new(),
    )
    .unwrap_err();
    assert!(matches!(
        &failure,
        HandoffFailure::Registry(registry)
            if registry.refusal() == Some(RegistryError::UnknownGeneration)
    ));
    assert_eq!(world.gate.role(), GenerationRole::Active);
    assert!(world.locator.publishes().is_empty());
}

#[test]
fn failures_are_reported_by_their_source_rather_than_flattened() {
    let admission: HandoffFailure =
        crate::usecase::authority::admission::AdmissionRefusal::Retired.into();
    assert_eq!(admission.to_string(), "generation is retired");
    let registry: HandoffFailure = RegistryError::Corrupt.into();
    assert_eq!(registry.to_string(), RegistryError::Corrupt.to_string());
    let locator: HandoffFailure = io::Error::other("unlink").into();
    assert!(locator.to_string().contains("unlink"));
    assert_eq!(format!("{:?}", HandoffStep::BeforeIntent), "BeforeIntent");
}
