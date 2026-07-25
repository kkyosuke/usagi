use std::error::Error as _;
use std::sync::Arc;
use std::thread;

use usagi_core::domain::id::DaemonGeneration;

use super::*;

fn gate(role: GenerationRole) -> AdmissionGate {
    AdmissionGate::new(DaemonGeneration::new(), role)
}

#[test]
fn classification_is_a_closed_table_over_role_request_and_owner() {
    use GenerationRole::{Active, Draining, Retired, Standby};
    use RequestClass::{Control, Inventory, Read, Spawn, TerminalIo};
    use ResourceOwner::{OtherGeneration, SelfGeneration, Unscoped};

    let expected = |role, request, owner| -> Result<Option<LeaseClass>, AdmissionRefusal> {
        match (role, request, owner) {
            (_, _, OtherGeneration) => Err(AdmissionRefusal::NotOwner),
            (Retired, _, _) => Err(AdmissionRefusal::Retired),
            (_, Read | Inventory, _) => Ok(None),
            (Standby, _, _) | (Draining, Control | Spawn, _) => Err(AdmissionRefusal::NotActive),
            (_, TerminalIo, SelfGeneration) => Ok(Some(LeaseClass::OwnerTerminal)),
            (_, TerminalIo, Unscoped) => Err(AdmissionRefusal::NotOwner),
            (Active, Control | Spawn, _) => Ok(Some(LeaseClass::ActiveControl)),
        }
    };

    for role in [Standby, Active, Draining, Retired] {
        for request in [Control, Spawn, TerminalIo, Read, Inventory] {
            for owner in [SelfGeneration, OtherGeneration, Unscoped] {
                assert_eq!(
                    classify(role, request, owner),
                    expected(role, request, owner),
                    "{role:?} / {request:?} / {owner:?}"
                );
            }
        }
    }
}

#[test]
fn an_active_generation_admits_control_and_owner_work_under_separate_leases() {
    let gate = gate(GenerationRole::Active);
    assert_eq!(gate.role(), GenerationRole::Active);
    assert_eq!(gate.revision(), 1);
    assert!(gate.is_open(LeaseClass::ActiveControl));
    assert!(gate.is_open(LeaseClass::OwnerTerminal));

    let control = gate
        .admit(RequestClass::Spawn, ResourceOwner::Unscoped)
        .unwrap()
        .unwrap();
    let terminal = gate
        .admit(RequestClass::TerminalIo, ResourceOwner::SelfGeneration)
        .unwrap()
        .unwrap();
    assert_eq!(control.class(), LeaseClass::ActiveControl);
    assert_eq!(terminal.class(), LeaseClass::OwnerTerminal);
    assert_eq!(control.generation(), gate.generation());
    assert_eq!(control.revision(), 1);
    control.revalidate().unwrap();
    assert_eq!(gate.outstanding(LeaseClass::ActiveControl), 1);
    assert_eq!(gate.outstanding(LeaseClass::OwnerTerminal), 1);

    // A read needs no lease, so it can never hold a handoff barrier open.
    assert!(
        gate.admit(RequestClass::Read, ResourceOwner::Unscoped)
            .unwrap()
            .is_none()
    );
    drop(control);
    drop(terminal);
    assert_eq!(gate.outstanding(LeaseClass::ActiveControl), 0);
    assert_eq!(gate.outstanding(LeaseClass::OwnerTerminal), 0);
}

#[test]
fn a_connection_admitted_while_active_cannot_spawn_after_the_role_changes() {
    let gate = gate(GenerationRole::Active);
    // The request that arrives before the barrier succeeds.
    gate.admit(RequestClass::Spawn, ResourceOwner::Unscoped)
        .unwrap()
        .unwrap();

    gate.close(LeaseClass::ActiveControl);
    gate.await_drain(LeaseClass::ActiveControl).unwrap();
    assert_eq!(gate.enter_draining().unwrap(), 2);

    // The same still-open connection now gets a typed refusal, not an effect.
    assert_eq!(
        gate.admit(RequestClass::Spawn, ResourceOwner::Unscoped)
            .unwrap_err(),
        AdmissionRefusal::NotActive
    );
    assert_eq!(
        gate.admit(RequestClass::Control, ResourceOwner::SelfGeneration)
            .unwrap_err(),
        AdmissionRefusal::NotActive
    );
    // Its owned terminals keep working, and other owners never did.
    assert!(
        gate.admit(RequestClass::TerminalIo, ResourceOwner::SelfGeneration)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        gate.admit(RequestClass::TerminalIo, ResourceOwner::OtherGeneration)
            .unwrap_err(),
        AdmissionRefusal::NotOwner
    );
}

#[test]
fn a_lease_taken_under_the_old_role_can_tell_that_authority_moved() {
    let gate = gate(GenerationRole::Active);
    let terminal = gate.acquire(LeaseClass::OwnerTerminal).unwrap();
    gate.close(LeaseClass::ActiveControl);
    gate.await_drain(LeaseClass::ActiveControl).unwrap();
    gate.enter_draining().unwrap();
    assert_eq!(terminal.revalidate(), Err(AdmissionRefusal::StaleRevision));
    let fresh = gate.acquire(LeaseClass::OwnerTerminal).unwrap();
    fresh.revalidate().unwrap();
    assert_eq!(fresh.revision(), 2);
}

#[test]
fn the_drain_barrier_waits_for_work_that_is_already_running() {
    let gate = Arc::new(gate(GenerationRole::Active));
    let lease = gate.acquire(LeaseClass::ActiveControl).unwrap();
    gate.close(LeaseClass::ActiveControl);

    let waiter = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            gate.await_drain(LeaseClass::ActiveControl).unwrap();
            // The barrier may only return once the outstanding work is gone.
            assert_eq!(gate.outstanding(LeaseClass::ActiveControl), 0);
            gate.enter_draining().unwrap()
        })
    };

    // Until the in-flight effect releases its lease the handoff cannot commit.
    assert_eq!(gate.enter_draining(), Err(AdmissionRefusal::Closed));
    drop(lease);
    assert_eq!(waiter.join().unwrap(), 2);
    assert_eq!(gate.role(), GenerationRole::Draining);
}

#[test]
fn a_background_producer_stops_before_the_barrier_waits_on_it() {
    let gate = Arc::new(gate(GenerationRole::Active));
    let (ticked, observe_tick) = std::sync::mpsc::channel();
    let (release, await_release) = std::sync::mpsc::channel();
    let worker = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            // A supervisor / decision / PR refresh producer: it holds a control
            // lease for each tick and stops issuing when the class closes.
            let mut ticks = 0;
            while let Ok(lease) = gate.acquire(LeaseClass::ActiveControl) {
                ticks += 1;
                ticked.send(()).unwrap();
                await_release.recv().unwrap();
                drop(lease);
            }
            ticks
        })
    };

    observe_tick.recv().unwrap();
    gate.close(LeaseClass::ActiveControl);
    release.send(()).unwrap();
    gate.await_drain(LeaseClass::ActiveControl).unwrap();
    gate.enter_draining().unwrap();
    assert_eq!(gate.outstanding(LeaseClass::ActiveControl), 0);
    assert_eq!(worker.join().unwrap(), 1);
}

#[test]
fn a_barrier_cannot_be_waited_on_while_the_class_still_issues_leases() {
    let gate = gate(GenerationRole::Active);
    assert_eq!(
        gate.await_drain(LeaseClass::ActiveControl),
        Err(AdmissionRefusal::StillOpen)
    );
    assert_eq!(gate.enter_draining(), Err(AdmissionRefusal::StillOpen));
    assert_eq!(gate.enter_retired(), Err(AdmissionRefusal::StillOpen));
}

#[test]
fn retirement_requires_both_classes_closed_and_drained() {
    let gate = gate(GenerationRole::Active);
    gate.close(LeaseClass::ActiveControl);
    gate.await_drain(LeaseClass::ActiveControl).unwrap();
    gate.enter_draining().unwrap();

    let owned = gate.acquire(LeaseClass::OwnerTerminal).unwrap();
    assert_eq!(gate.enter_retired(), Err(AdmissionRefusal::StillOpen));
    gate.close(LeaseClass::OwnerTerminal);
    // Collection may not start while an owner terminal operation is running.
    assert_eq!(gate.enter_retired(), Err(AdmissionRefusal::Closed));
    assert_eq!(
        gate.acquire(LeaseClass::OwnerTerminal).unwrap_err(),
        AdmissionRefusal::Closed
    );
    drop(owned);
    gate.await_drain(LeaseClass::OwnerTerminal).unwrap();
    assert_eq!(gate.enter_retired().unwrap(), 3);
    assert_eq!(gate.role(), GenerationRole::Retired);
    assert_eq!(
        gate.acquire(LeaseClass::OwnerTerminal).unwrap_err(),
        AdmissionRefusal::Retired
    );
    assert_eq!(
        gate.admit(RequestClass::Read, ResourceOwner::Unscoped)
            .unwrap_err(),
        AdmissionRefusal::Retired
    );
}

#[test]
fn a_standby_gate_issues_no_lease_at_all() {
    let gate = gate(GenerationRole::Standby);
    assert!(!gate.is_open(LeaseClass::ActiveControl));
    assert!(!gate.is_open(LeaseClass::OwnerTerminal));
    assert_eq!(
        gate.acquire(LeaseClass::ActiveControl).unwrap_err(),
        AdmissionRefusal::NotActive
    );
    assert_eq!(
        gate.acquire(LeaseClass::OwnerTerminal).unwrap_err(),
        AdmissionRefusal::NotActive
    );
    assert_eq!(gate.enter_draining(), Err(AdmissionRefusal::NotActive));
    // A readiness probe is a read: it is admissible and takes nothing.
    assert!(
        gate.admit(RequestClass::Read, ResourceOwner::Unscoped)
            .unwrap()
            .is_none()
    );
    // Closing is idempotent even for a class that never opened.
    gate.close(LeaseClass::ActiveControl);
    gate.await_drain(LeaseClass::ActiveControl).unwrap();
}

#[test]
fn a_standby_only_admits_control_work_after_the_registry_named_it_active() {
    let gate = gate(GenerationRole::Standby);
    assert_eq!(
        gate.acquire(LeaseClass::ActiveControl).unwrap_err(),
        AdmissionRefusal::NotActive
    );
    assert_eq!(gate.activate().unwrap(), 2);
    assert_eq!(gate.role(), GenerationRole::Active);
    assert!(
        gate.admit(RequestClass::Spawn, ResourceOwner::Unscoped)
            .unwrap()
            .is_some()
    );
    // Authority only ever moves forward: activation is not a way back.
    assert_eq!(gate.activate().unwrap_err(), AdmissionRefusal::NotActive);
    assert_eq!(
        AdmissionGate::new(DaemonGeneration::new(), GenerationRole::Draining)
            .activate()
            .unwrap_err(),
        AdmissionRefusal::NotActive
    );
}

#[test]
fn a_pre_commit_barrier_is_reopened_but_a_durable_one_is_not() {
    let gate = gate(GenerationRole::Active);
    gate.close(LeaseClass::ActiveControl);
    gate.await_drain(LeaseClass::ActiveControl).unwrap();
    gate.enter_draining().unwrap();
    assert_eq!(gate.abort_draining().unwrap(), 3);
    assert_eq!(gate.role(), GenerationRole::Active);
    assert!(
        gate.admit(RequestClass::Spawn, ResourceOwner::Unscoped)
            .unwrap()
            .is_some()
    );
    // A barrier that was never closed by this process cannot be reopened.
    assert_eq!(
        gate.abort_draining().unwrap_err(),
        AdmissionRefusal::NotActive
    );

    gate.close(LeaseClass::ActiveControl);
    gate.await_drain(LeaseClass::ActiveControl).unwrap();
    gate.enter_draining().unwrap();
    gate.confirm_draining();
    assert_eq!(
        gate.abort_draining().unwrap_err(),
        AdmissionRefusal::NotActive
    );
    assert_eq!(gate.role(), GenerationRole::Draining);
}

#[test]
fn a_draining_gate_never_reopens_control_work() {
    let gate = gate(GenerationRole::Draining);
    assert!(!gate.is_open(LeaseClass::ActiveControl));
    assert!(gate.is_open(LeaseClass::OwnerTerminal));
    assert_eq!(
        gate.acquire(LeaseClass::ActiveControl).unwrap_err(),
        AdmissionRefusal::NotActive
    );
    gate.acquire(LeaseClass::OwnerTerminal).unwrap();
    assert_eq!(gate.enter_draining(), Err(AdmissionRefusal::NotActive));
    assert!(format!("{gate:?}").contains("AdmissionGate"));
}

#[test]
fn every_refusal_reads_as_a_safety_outcome() {
    for refusal in [
        AdmissionRefusal::NotActive,
        AdmissionRefusal::Closed,
        AdmissionRefusal::NotOwner,
        AdmissionRefusal::Retired,
        AdmissionRefusal::StaleRevision,
        AdmissionRefusal::StillOpen,
    ] {
        assert!(!refusal.to_string().is_empty());
        assert!(refusal.source().is_none());
    }
}
