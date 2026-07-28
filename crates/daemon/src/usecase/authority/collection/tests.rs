use std::collections::VecDeque;
use std::io;
use std::sync::Mutex;
use std::sync::mpsc::channel;

use super::*;
use crate::usecase::authority::fixture::{build, process, registry};
use crate::usecase::authority::registry::GenerationEntry;
use crate::usecase::authority::workers::ConnectionShutdown;

struct Observation {
    answers: Mutex<VecDeque<Result<Option<CollectionBlocker>, ResourceFailure>>>,
}

impl Observation {
    fn new(
        answers: impl IntoIterator<Item = Result<Option<CollectionBlocker>, ResourceFailure>>,
    ) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().collect()),
        }
    }

    fn blocker(blocker: CollectionBlocker) -> Self {
        Self::new([Ok(Some(blocker))])
    }

    fn remaining(&self) -> usize {
        self.answers.lock().unwrap().len()
    }
}

impl DrainObservation for Observation {
    fn blocker(&self) -> Result<Option<CollectionBlocker>, ResourceFailure> {
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .expect("collection made an unexpected durable observation")
    }
}

fn generation(role: GenerationRole) -> (GenerationRegistry, AdmissionGate, DaemonGeneration) {
    let (registry, _) = registry(2);
    let generation = DaemonGeneration::new();
    registry
        .update(|document| {
            document.generations.push(GenerationEntry {
                generation,
                role,
                endpoint: "generations/owner/sock".into(),
                process: process(41),
                expected_build: build("old"),
                verified_build: Some(build("old")),
                revision: 1,
            });
            if role == GenerationRole::Active {
                document.current = Some(generation);
            }
            Ok(())
        })
        .unwrap();
    (registry, AdmissionGate::new(generation, role), generation)
}

#[test]
fn every_runtime_claim_blocks_collection_by_itself() {
    for blocker in [
        CollectionBlocker::LiveResource,
        CollectionBlocker::InFlightCommand,
        CollectionBlocker::UnackedOutbox,
        CollectionBlocker::CapacityClaim,
    ] {
        let (registry, gate, owner) = generation(GenerationRole::Draining);
        let observation = Observation::blocker(blocker);
        let workers = ClientWorkers::new();
        assert!(matches!(
            collect_if_drained(&registry, &gate, &workers, owner, &observation).unwrap(),
            Collection::Pending(observed) if observed == blocker
        ));
        // The cheap pre-fence observation found a live claim, so owner IO stays
        // open and the draining process keeps serving its PTYs.
        drop(gate.acquire(LeaseClass::OwnerTerminal).unwrap());
        assert_eq!(gate.role(), GenerationRole::Draining);
        assert_eq!(observation.remaining(), 0);
    }
}

#[test]
fn an_active_or_already_retired_generation_is_never_observed_or_collected() {
    for role in [GenerationRole::Active, GenerationRole::Retired] {
        let (registry, gate, owner) = generation(role);
        let observation = Observation::new([]);
        assert!(matches!(
            collect_if_drained(&registry, &gate, &ClientWorkers::new(), owner, &observation)
                .unwrap(),
            Collection::NotDraining
        ));
        assert_eq!(observation.remaining(), 0);
        assert_eq!(gate.role(), role);
    }
}

#[test]
fn the_final_observation_happens_after_owner_leases_are_closed_and_drained() {
    let (registry, gate, owner) = generation(GenerationRole::Draining);
    let observation = Observation::new([Ok(None), Ok(Some(CollectionBlocker::UnackedOutbox))]);
    let lease = gate.acquire(LeaseClass::OwnerTerminal).unwrap();
    let releaser = std::thread::spawn(move || drop(lease));

    assert!(matches!(
        collect_if_drained(&registry, &gate, &ClientWorkers::new(), owner, &observation).unwrap(),
        Collection::Pending(CollectionBlocker::UnackedOutbox)
    ));
    releaser.join().unwrap();
    assert!(matches!(
        gate.acquire(LeaseClass::OwnerTerminal),
        Err(crate::usecase::authority::admission::AdmissionRefusal::Closed)
    ));
    assert_eq!(gate.role(), GenerationRole::Draining);
    assert_eq!(observation.remaining(), 0);
}

struct FakeConnection(Mutex<Option<std::sync::mpsc::Sender<()>>>);

impl ConnectionShutdown for FakeConnection {
    fn shutdown(&self) -> io::Result<()> {
        drop(self.0.lock().unwrap().take());
        Ok(())
    }
}

#[test]
fn a_fully_drained_generation_joins_workers_and_records_retirement() {
    let (registry, gate, owner) = generation(GenerationRole::Draining);
    let observation = Observation::new([Ok(None), Ok(None)]);
    let workers = ClientWorkers::new();
    let (sender, receiver) = channel();
    workers.register(
        Box::new(FakeConnection(Mutex::new(Some(sender)))),
        std::thread::spawn(move || assert!(receiver.recv().is_err())),
    );

    let Collection::Collected(report) =
        collect_if_drained(&registry, &gate, &workers, owner, &observation).unwrap()
    else {
        panic!("the fully drained generation was not collected");
    };
    assert!(report.is_clean());
    assert_eq!(report.joined, 1);
    assert_eq!(gate.role(), GenerationRole::Retired);
    assert_eq!(
        registry.load().unwrap().document().role(owner),
        Some(GenerationRole::Retired)
    );
    assert_eq!(observation.remaining(), 0);
}

#[test]
fn an_unreadable_runtime_and_a_registry_failure_both_fail_closed() {
    let (owner_registry, gate, owner) = generation(GenerationRole::Draining);
    let unreadable = Observation::new([Err(io::Error::other("unreadable").into())]);
    let failure = collect_if_drained(
        &owner_registry,
        &gate,
        &ClientWorkers::new(),
        owner,
        &unreadable,
    )
    .unwrap_err();
    assert!(matches!(failure, CollectionFailure::Runtime(_)));
    assert!(failure.to_string().contains("runtime observation failed"));
    assert_eq!(gate.role(), GenerationRole::Draining);

    let (empty, _) = registry(2);
    let unknown = DaemonGeneration::new();
    let gate = AdmissionGate::new(unknown, GenerationRole::Draining);
    let drained = Observation::new([Ok(None), Ok(None)]);
    let failure =
        collect_if_drained(&empty, &gate, &ClientWorkers::new(), unknown, &drained).unwrap_err();
    assert!(matches!(failure, CollectionFailure::Authority(_)));
    assert!(failure.to_string().contains("not registered"));
    // The process-local gate retired, but the durable registry did not guess an
    // entry into existence. The process must keep failing closed.
    assert_eq!(gate.role(), GenerationRole::Retired);
}
