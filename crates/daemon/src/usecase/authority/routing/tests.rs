use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::{
    ClientWorkspace, DaemonGeneration as WireGeneration, ProtocolRange,
};

use super::*;
use crate::usecase::authority::fixture::{build, hello};
use crate::usecase::authority::registry::REGISTRY_SCHEMA;

/// One connection identity per name, stable across the calls in a test.
fn connection(name: &str) -> ConnectionId {
    static NAMES: [&str; 3] = ["tui", "mcp", "old"];
    let index = NAMES.iter().position(|entry| *entry == name).unwrap();
    ConnectionId::parse(&format!("00000000-0000-4000-8000-00000000000{index}")).unwrap()
}

fn client(capabilities: Vec<String>) -> ClientHello {
    ClientHello {
        client_id: usagi_core::infrastructure::ipc::ClientId(
            usagi_core::domain::id::ClientId::new().as_str(),
        ),
        connection_nonce: "nonce".into(),
        expected_daemon_generation: None,
        supported_protocols: vec![ProtocolRange {
            generation: 1,
            min_revision: 1,
            max_revision: 2,
        }],
        capabilities,
        required_capabilities: Vec::new(),
        build: build("current"),
        workspace: Some(ClientWorkspace::Unbound),
    }
}

fn routing_client() -> ClientHello {
    client(vec![OWNER_GENERATION_ROUTING_CAPABILITY.to_owned()])
}

fn legacy_client() -> ClientHello {
    client(vec!["request.correlation.v1".to_owned()])
}

fn successor() -> ServerHello {
    hello(DaemonGeneration::new(), &build("next"))
}

fn document(revision: u64) -> RegistryDocument {
    RegistryDocument {
        revision,
        ..RegistryDocument::default()
    }
}

#[test]
fn a_rollover_is_admitted_when_every_participant_can_address_a_draining_generation() {
    let ledger = RoutingLedger::new();
    ledger.admit(connection("tui"), &routing_client());
    ledger.admit(connection("mcp"), &routing_client());
    assert_eq!(ledger.connections(), 2);
    assert_eq!(ledger.unsupported(), 0);
    assert!(admit_rollover(&ledger, &document(7), 7, &successor()).is_ok());
}

#[test]
fn one_client_without_the_routing_capability_refuses_the_whole_rollover() {
    let ledger = RoutingLedger::new();
    ledger.admit(connection("tui"), &routing_client());
    ledger.admit(connection("old"), &legacy_client());
    assert_eq!(
        admit_rollover(&ledger, &document(1), 1, &successor()).err(),
        Some(RolloverRefusal::ClientRoutingUnsupported { connections: 1 })
    );

    // The refusal is a property of the *connections*, not of the client
    // identity: the old connection going away lifts it.
    ledger.disconnect(&connection("old"));
    assert_eq!(ledger.connections(), 1);
    assert!(admit_rollover(&ledger, &document(1), 1, &successor()).is_ok());

    // A reconnect with a newer build replaces its own answer rather than
    // accumulating a second entry.
    ledger.admit(connection("old"), &legacy_client());
    ledger.admit(connection("old"), &routing_client());
    assert_eq!(ledger.connections(), 2);
    assert!(admit_rollover(&ledger, &document(1), 1, &successor()).is_ok());
}

#[test]
fn a_successor_that_does_not_route_by_owner_generation_is_refused() {
    let ledger = RoutingLedger::new();
    let mut legacy = successor();
    legacy
        .capabilities
        .retain(|capability| capability != OWNER_GENERATION_ROUTING_CAPABILITY);
    assert_eq!(
        admit_rollover(&ledger, &document(1), 1, &legacy).err(),
        Some(RolloverRefusal::SuccessorRoutingUnsupported)
    );
    assert!(admit_rollover(&ledger, &document(1), 1, &successor()).is_ok());
}

#[test]
fn a_connection_that_finishes_hello_during_handoff_waits_for_the_routing_freeze() {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    let ledger = Arc::new(RoutingLedger::new());
    let freeze = admit_rollover(&ledger, &document(1), 1, &successor()).unwrap();
    assert_eq!(
        admit_rollover(&ledger, &document(1), 1, &successor()).err(),
        Some(RolloverRefusal::RoutingAdmissionBusy)
    );
    let (entered, entered_rx) = mpsc::channel();
    let (finished, finished_rx) = mpsc::channel();
    let admitting = Arc::clone(&ledger);
    let worker = std::thread::spawn(move || {
        entered.send(()).unwrap();
        admitting.admit(connection("old"), &legacy_client());
        finished.send(()).unwrap();
    });

    entered_rx.recv().unwrap();
    assert!(finished_rx.recv_timeout(Duration::from_millis(20)).is_err());
    assert_eq!(ledger.connections(), 0);
    drop(freeze);
    finished_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    worker.join().unwrap();
    assert_eq!(ledger.unsupported(), 1);
}

#[test]
fn a_registry_this_build_did_not_write_or_that_moved_is_refused() {
    let ledger = RoutingLedger::new();
    let mut unknown = document(1);
    unknown.schema = "usagi-generation-registry-v99".into();
    assert_eq!(
        admit_rollover(&ledger, &unknown, 1, &successor()).err(),
        Some(RolloverRefusal::RegistrySchemaUnsupported)
    );
    assert_eq!(document(1).schema, REGISTRY_SCHEMA);

    assert_eq!(
        admit_rollover(&ledger, &document(9), 7, &successor()).err(),
        Some(RolloverRefusal::RegistryRevisionMismatch {
            planned: 7,
            observed: 9,
        })
    );
}

#[test]
fn every_refusal_names_itself() {
    for refusal in [
        RolloverRefusal::ClientRoutingUnsupported { connections: 2 },
        RolloverRefusal::SuccessorRoutingUnsupported,
        RolloverRefusal::RegistrySchemaUnsupported,
        RolloverRefusal::RegistryRevisionMismatch {
            planned: 1,
            observed: 2,
        },
        RolloverRefusal::RoutingAdmissionBusy,
        RolloverRefusal::McpAuthorityRetained { credentials: 1 },
        RolloverRefusal::McpAuthorityUnavailable,
    ] {
        assert!(!refusal.to_string().is_empty());
    }
    assert_eq!(
        ParticipantRouting::of(&routing_client()),
        ParticipantRouting {
            supports_owner_routing: true
        }
    );
    assert_eq!(RoutingLedger::default().unsupported(), 0);
    let _ = WireGeneration("unused".into());
}
