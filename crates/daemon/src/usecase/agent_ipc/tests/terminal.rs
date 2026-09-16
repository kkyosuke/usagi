//! terminal の振る舞いを固定するテスト。

use super::*;

#[test]
fn runtime_operation_join_is_exact_and_durable_ownership_is_unique() {
    let mut agent = runtime();
    let first_operation = OperationId::new();
    let first = agent
        .launch(
            &first_operation.to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    assert_eq!(
        agent.runtime_for_operation(first_operation),
        agent.coordinator.runtime_for_terminal(&first.terminal)
    );
    assert_eq!(agent.runtime_for_operation(OperationId::new()), None);

    let second_operation = OperationId::new();
    agent
        .launch(
            &second_operation.to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let mut duplicate = agent.coordinator.snapshot();
    for record in &mut duplicate.records {
        record.operation.operation_id = first_operation;
    }
    assert_eq!(
        RuntimeCoordinator::hydrate(duplicate, 16, 64 * 1024, 64).unwrap_err(),
        crate::usecase::runtime::RuntimeSnapshotError::DuplicateOperation
    );
}

#[test]
fn agent_output_answers_cursor_position_queries_through_the_owned_pty() {
    let mut agent = runtime();
    let admission = agent
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();

    agent
        .output(&admission.terminal, b"warning\r\n> \x1b[6n".to_vec())
        .unwrap();

    assert_eq!(pty(&agent).writes, b"\x1b[2;3R");
    assert_eq!(pty(&agent).selected, Some(admission.terminal));
}

/// The Agent runtime is the authority a metrics observer reads through: the
/// level it publishes is `AGENT_RUNTIME_LIMIT` wide and moves with the
/// admissions it grants, so nobody has to count runtimes or restate the limit.
#[test]
fn a_bound_gauge_reports_this_owners_agent_concurrency() {
    use crate::usecase::metrics::AgentConcurrencyGauge;

    let mut runtime = runtime();
    let gauge = AgentConcurrencyGauge::default();
    runtime.bind_concurrency_gauge(gauge.clone());
    assert_eq!(
        gauge.observe(),
        Some(usagi_core::infrastructure::ipc::AgentConcurrency {
            in_use: 0,
            limit: u32::try_from(AGENT_RUNTIME_LIMIT).unwrap(),
        })
    );

    runtime
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();

    assert_eq!(gauge.observe(), Some(runtime.concurrency()));
    assert_eq!(gauge.observe().unwrap().in_use, 1);
    assert!(!gauge.observe().unwrap().is_saturated());
}

#[test]
fn retained_resources_lists_live_agent_terminals() {
    let mut runtime = runtime();
    let admission = runtime
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();

    assert_eq!(
        runtime.retained_resources(),
        std::iter::once(admission.terminal.terminal_id.as_str()).collect()
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One ordered scenario keeps launch, both snapshot revisions, input, detach, reattach and exit visibly sequential.
fn end_to_end_launch_output_attach_input_detach_reattach_and_exit() {
    let mut runtime = runtime();
    let fake_scope = FakeScope(Ok(scope()));
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let admission = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap();
    assert_eq!(admission.operation_id, operation);
    assert_eq!(admission.revision, 1);
    assert_eq!(admission.terminal.session_id, launch_intent.session);
    let terminal = admission.terminal;

    // Daemon-owned PTY output is journaled before it is replayable.
    runtime.output(&terminal, b"ready\n".to_vec()).unwrap();

    let connection = ConnectionId::new();
    let client = ClientId::new();
    let attached = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Attach,
        TerminalRequest::Attach {
            terminal: terminal.clone(),
            geometry: None,
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(attached["snapshot"]["replay"], json!(b"ready\n".to_vec()));
    let subscription = attached["subscription"].as_u64().unwrap();

    // The same Agent terminal serves a revision 2 connection its semantic
    // screen instead of the raw tail (#534): Agent and generic terminals
    // share one snapshot contract.
    let checkpointed = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Attach,
        TerminalRequest::Attach {
            terminal: terminal.clone(),
            geometry: None,
        },
        SnapshotWire::ScreenCheckpoint,
    ));
    let screen = &checkpointed["snapshot"]["screen"];
    assert!(checkpointed["snapshot"]["replay"].is_null());
    assert_eq!(
        checkpointed["snapshot"]["base_offset"],
        checkpointed["snapshot"]["output_offset"]
    );
    assert_eq!(
        screen["schema_version"].as_u64(),
        Some(u64::from(usagi_core::usecase::vt_screen::SCHEMA_VERSION))
    );
    // The screen is the authority for what the PTY printed.
    let restored = usagi_core::usecase::vt_screen::VtScreen::from_checkpoint(
        &serde_json::from_value(screen.clone()).unwrap(),
    )
    .unwrap();
    assert_eq!(restored.cells()[0].trim_end(), "ready");

    handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resize,
        TerminalRequest::Resize {
            terminal: terminal.clone(),
            geometry: TerminalGeometry { cols: 43, rows: 17 },
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(
        pty(&runtime).resized,
        vec![(terminal.clone(), Geometry { cols: 43, rows: 17 })]
    );

    let input_operation = OperationId::new();
    let ack = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Input,
        TerminalRequest::Input {
            terminal: terminal.clone(),
            subscription,
            input_seq: 0,
            input_operation: Some(input_operation),
            bytes: b"go\n".to_vec(),
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(ack["ack"], "Written");

    // The Agent owner shares the durable input operation ledger, so a client
    // whose acknowledgement was lost resolves the same final here too (#519),
    // and an identity it never issued is a typed unknown.
    let resolve = |runtime: &mut AgentRuntime, operation| {
        handled(runtime.handle_terminal(
            connection,
            client,
            RequestId::new(),
            TerminalAction::InputOutcome,
            TerminalRequest::InputOutcome {
                terminal: terminal.clone(),
                input_operation: operation,
            },
            SnapshotWire::RawTail,
        ))
    };
    let resolved = resolve(&mut runtime, input_operation);
    assert_eq!(resolved["outcome"], "final");
    assert_eq!(resolved["ack"], "Written");
    assert_eq!(
        resolve(&mut runtime, OperationId::new())["outcome"],
        "unknown"
    );
    // Reusing that identity for different bytes conflicts without writing.
    let conflict = handled_result(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Input,
        TerminalRequest::Input {
            terminal: terminal.clone(),
            subscription,
            input_seq: 1,
            input_operation: Some(input_operation),
            bytes: b"rm -rf\n".to_vec(),
        },
        SnapshotWire::RawTail,
    ))
    .unwrap_err();
    assert_eq!(conflict.code, ErrorCode::IdempotencyConflict);

    handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Detach,
        TerminalRequest::Detach {
            terminal: terminal.clone(),
            subscription,
        },
        SnapshotWire::RawTail,
    ));
    // A coalesced live-set sweep drops only subscriptions; the process/PTY
    // stay alive.
    runtime.retain_live_connections(&BTreeSet::new());

    let reattached = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Attach,
        TerminalRequest::Attach {
            terminal: terminal.clone(),
            geometry: None,
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(reattached["snapshot"]["output_offset"], 6);

    runtime.exit(&terminal, 0).unwrap();
    let final_replay = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap();
    assert_eq!(final_replay.terminal, terminal);
    assert!(final_replay.completed);
    let resync = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resync,
        TerminalRequest::Resync {
            terminal: terminal.clone(),
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(resync["exited"], 0);
    assert_eq!(pty(&runtime).selected.as_ref(), Some(&terminal));
    assert_eq!(pty(&runtime).writes, b"go\n");
}

#[test]
fn terminal_requests_for_unknown_refs_are_not_owned_and_output_is_stale_safe() {
    let mut runtime = runtime();
    let foreign = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    assert!(matches!(
        runtime.handle_terminal(
            ConnectionId::new(),
            ClientId::new(),
            RequestId::new(),
            TerminalAction::Attach,
            TerminalRequest::Attach {
                terminal: foreign.clone(),
                geometry: None,
            },
            SnapshotWire::RawTail,
        ),
        TerminalOutcome::NotOwned
    ));
    // Launch/Inventory never address an agent terminal.
    assert!(matches!(
        runtime.handle_terminal(
            ConnectionId::new(),
            ClientId::new(),
            RequestId::new(),
            TerminalAction::Inventory,
            TerminalRequest::Inventory {
                scope: usagi_core::domain::terminal_launch::TerminalLaunchScope {
                    workspace_id: WorkspaceId::new(),
                    session_id: None,
                    worktree_id: WorktreeId::new(),
                },
            },
            SnapshotWire::RawTail,
        ),
        TerminalOutcome::NotOwned
    ));
    assert_eq!(
        runtime.output(&foreign, b"x".to_vec()).unwrap_err().code,
        ErrorCode::StaleTarget
    );
    assert_eq!(
        runtime.exit(&foreign, 0).unwrap_err().code,
        ErrorCode::StaleTarget
    );
}

#[test]
fn agent_resize_rejects_each_forged_terminal_ref_field_before_pty_effect() {
    let mut runtime = runtime();
    let terminal = runtime
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap()
        .terminal;
    let mut forged = Vec::new();
    let mut reference = terminal.clone();
    reference.daemon_generation = DaemonGeneration::new();
    forged.push(reference);
    let mut reference = terminal.clone();
    reference.terminal_id = TerminalId::new();
    forged.push(reference);
    let mut reference = terminal.clone();
    reference.workspace_id = WorkspaceId::new();
    forged.push(reference);
    let mut reference = terminal.clone();
    reference.session_id = Some(SessionId::new());
    forged.push(reference);
    let mut reference = terminal;
    reference.worktree_id = WorktreeId::new();
    forged.push(reference);

    for terminal in forged {
        assert!(matches!(
            runtime.handle_terminal(
                ConnectionId::new(),
                ClientId::new(),
                RequestId::new(),
                TerminalAction::Resize,
                TerminalRequest::Resize {
                    terminal,
                    geometry: TerminalGeometry {
                        cols: 100,
                        rows: 40
                    },
                },
                SnapshotWire::RawTail,
            ),
            TerminalOutcome::NotOwned
        ));
    }
    assert!(pty(&runtime).resized.is_empty());
}

#[test]
fn shared_owner_routes_agent_terminals_to_agent_and_others_to_generic() {
    let mut agent = runtime();
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let admission = agent
        .launch(&operation, &launch_intent, &FakeScope(Ok(scope())))
        .unwrap();
    let terminal = admission.terminal;
    agent.output(&terminal, b"hi\n".to_vec()).unwrap();

    let mut owner = SharedTerminalOwner::new(agent, FakeGeneric::default());
    let connection = ConnectionId::new();
    let client = ClientId::new();
    // Agent terminal → agent owner.
    let attached = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Attach,
            serde_json::to_value(TerminalRequest::Attach {
                terminal: terminal.clone(),
                geometry: None,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap();
    assert_eq!(attached["snapshot"]["replay"], json!(b"hi\n".to_vec()));

    // A generic Launch (no agent terminal) → generic owner.
    let generic = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Launch,
            serde_json::to_value(TerminalRequest::Launch {
                intent: usagi_core::infrastructure::ipc::TerminalLaunchIntent {
                    request: usagi_core::domain::terminal_launch::TerminalLaunchRequest {
                        profile_id: usagi_core::domain::terminal_launch::TerminalProfileId::new(
                            "login-shell",
                        )
                        .unwrap(),
                        scope: usagi_core::domain::terminal_launch::TerminalLaunchScope {
                            workspace_id: WorkspaceId::new(),
                            session_id: Some(SessionId::new()),
                            worktree_id: WorktreeId::new(),
                        },
                    },
                    geometry: usagi_core::infrastructure::ipc::TerminalGeometry {
                        cols: 80,
                        rows: 24,
                    },
                    launch_operation: None,
                },
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap();
    assert_eq!(generic["terminals"], json!([]));

    // Malformed payload is rejected before either usecase owner runs.
    owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Attach,
            json!({ "operation": "bogus" }),
            SnapshotWire::RawTail,
        )
        .unwrap_err();

    TerminalOwner::disconnect(&mut owner, connection);
    assert_eq!(owner.generic.requests, 1);
    assert_eq!(owner.generic.disconnects, 1);
}

#[test]
fn shared_owner_inventory_merges_agent_and_generic_and_rejects_invalid_scope() {
    use usagi_core::domain::terminal_launch::{
        TerminalInventoryEntry, TerminalKind, TerminalLaunchScope,
    };

    let mut agent = runtime();
    let operation = OperationId::new().to_string();
    let admission = agent
        .launch(&operation, &intent(None), &FakeScope(Ok(scope())))
        .unwrap();
    let agent_terminal = admission.terminal;
    // Query with the launched Agent's exact scope so it is in scope.
    let inventory_scope = TerminalLaunchScope {
        workspace_id: agent_terminal.workspace_id,
        session_id: agent_terminal.session_id,
        worktree_id: agent_terminal.worktree_id,
    };
    // A generic terminal the generic owner reports for the same scope.
    let generic_terminal = TerminalRef {
        daemon_generation: agent_terminal.daemon_generation,
        terminal_id: TerminalId::new(),
        workspace_id: agent_terminal.workspace_id,
        session_id: agent_terminal.session_id,
        worktree_id: agent_terminal.worktree_id,
    };
    let generic = FakeGeneric {
        inventory: vec![TerminalInventoryEntry {
            terminal: generic_terminal.clone(),
            kind: TerminalKind::Terminal,
            live: true,
        }],
        ..FakeGeneric::default()
    };
    let mut owner = SharedTerminalOwner::new(agent, generic);
    let connection = ConnectionId::new();
    let client = ClientId::new();
    // The shared owner handles Inventory through `request`; when used as a
    // nested generic owner its trait-level default inventory is empty.
    assert!(TerminalOwner::inventory(&owner, &inventory_scope).is_empty());

    let reply = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Inventory,
            serde_json::to_value(TerminalRequest::Inventory {
                scope: inventory_scope,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap();
    let entries: Vec<TerminalInventoryEntry> =
        serde_json::from_value(reply["terminals"].clone()).unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|entry| {
        entry.kind == TerminalKind::Terminal
            && entry.terminal.fences(&generic_terminal)
            && entry.live
    }));
    assert!(entries.iter().any(|entry| {
        entry.kind == TerminalKind::Agent && entry.terminal.fences(&agent_terminal) && entry.live
    }));

    // A payload that is not a valid inventory request is a safe rejection,
    // never a generic-owner fallback.
    let error = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Inventory,
            json!({ "operation": "bogus" }),
            SnapshotWire::RawTail,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers merge, stamping, and CAS.
fn shared_owner_completed_inventory_merges_and_stamps_visibility() {
    use usagi_core::domain::terminal_launch::{TerminalKind, TerminalLaunchScope};
    use usagi_core::domain::terminal_visibility::{
        CompletedTerminalEntry, TerminalVisibility, TerminalVisibilityState,
    };

    let mut agent = runtime();
    let operation = OperationId::new().to_string();
    let admission = agent
        .launch(&operation, &intent(None), &FakeScope(Ok(scope())))
        .unwrap();
    let agent_terminal = admission.terminal;
    // Exit the Agent so it becomes an exited tombstone, not a live runtime.
    agent.exit(&agent_terminal, 0).unwrap();
    let query_scope = TerminalLaunchScope {
        workspace_id: agent_terminal.workspace_id,
        session_id: agent_terminal.session_id,
        worktree_id: agent_terminal.worktree_id,
    };
    let generic_terminal = TerminalRef {
        daemon_generation: agent_terminal.daemon_generation,
        terminal_id: TerminalId::new(),
        workspace_id: agent_terminal.workspace_id,
        session_id: agent_terminal.session_id,
        worktree_id: agent_terminal.worktree_id,
    };
    let generic = FakeGeneric {
        completed: vec![CompletedTerminalEntry {
            terminal: generic_terminal.clone(),
            kind: TerminalKind::Terminal,
            exit_status: 3,
            base_offset: 0,
            final_output_offset: 12,
            visibility: TerminalVisibility::unobserved(),
        }],
        ..FakeGeneric::default()
    };
    let mut owner = SharedTerminalOwner::new(agent, generic);
    let connection = ConnectionId::new();
    let client = ClientId::new();
    let query = |owner: &mut SharedTerminalOwner<_, _>| -> Vec<CompletedTerminalEntry> {
        let reply = owner
            .request(
                connection,
                client,
                RequestId::new(),
                TerminalAction::CompletedInventory,
                serde_json::to_value(TerminalRequest::CompletedInventory {
                    scope: query_scope.clone(),
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap();
        serde_json::from_value(reply["entries"].clone()).unwrap()
    };

    let entries = query(&mut owner);
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| { entry.visibility.state == TerminalVisibilityState::Unobserved })
    );
    let agent_entry = entries
        .iter()
        .find(|entry| entry.kind == TerminalKind::Agent)
        .unwrap();
    assert!(agent_entry.terminal.fences(&agent_terminal));

    // Observe the generic tombstone and re-query: only that exact ref rises.
    let observed = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Observe,
            serde_json::to_value(TerminalRequest::Observe {
                terminal: generic_terminal.clone(),
                expected_revision: 0,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap();
    assert_eq!(observed["applied"], serde_json::json!(true));
    assert_eq!(observed["conflict"], serde_json::json!(false));

    let entries = query(&mut owner);
    let generic_entry = entries
        .iter()
        .find(|entry| entry.terminal.fences(&generic_terminal))
        .unwrap();
    assert_eq!(
        generic_entry.visibility.state,
        TerminalVisibilityState::Observed
    );
    assert_eq!(generic_entry.exit_status, 3);
    // The Agent tombstone's independent visibility is untouched.
    let agent_entry = entries
        .iter()
        .find(|entry| entry.kind == TerminalKind::Agent)
        .unwrap();
    assert_eq!(
        agent_entry.visibility.state,
        TerminalVisibilityState::Unobserved
    );

    // An invalid completed-inventory payload is a safe rejection.
    let error = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::CompletedInventory,
            json!({ "operation": "bogus" }),
            SnapshotWire::RawTail,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[test]
#[allow(clippy::too_many_lines)] // One fixture covers observe/dismiss CAS, conflict, and rejection.
fn shared_owner_observe_and_dismiss_are_cas_and_do_not_touch_the_process() {
    use usagi_core::domain::terminal_visibility::{TerminalVisibility, TerminalVisibilityState};

    let mut owner = SharedTerminalOwner::new(runtime(), FakeGeneric::default());
    let connection = ConnectionId::new();
    let client = ClientId::new();
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    let visibility = |value: &Value| -> TerminalVisibility {
        serde_json::from_value(value["visibility"].clone()).unwrap()
    };
    let send = |owner: &mut SharedTerminalOwner<_, _>,
                action: TerminalAction,
                request: TerminalRequest|
     -> Value {
        owner
            .request(
                connection,
                client,
                RequestId::new(),
                action,
                serde_json::to_value(request).unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap()
    };

    let observed = send(
        &mut owner,
        TerminalAction::Observe,
        TerminalRequest::Observe {
            terminal: terminal.clone(),
            expected_revision: 0,
        },
    );
    assert_eq!(
        visibility(&observed).state,
        TerminalVisibilityState::Observed
    );
    assert_eq!(visibility(&observed).revision, 1);

    // A stale dismiss conflicts and returns the authoritative snapshot.
    let conflict = send(
        &mut owner,
        TerminalAction::Dismiss,
        TerminalRequest::Dismiss {
            terminal: terminal.clone(),
            expected_revision: 0,
        },
    );
    assert_eq!(conflict["applied"], serde_json::json!(false));
    assert_eq!(conflict["conflict"], serde_json::json!(true));
    assert_eq!(
        visibility(&conflict).state,
        TerminalVisibilityState::Observed
    );

    // Merging to the authoritative revision succeeds.
    let dismissed = send(
        &mut owner,
        TerminalAction::Dismiss,
        TerminalRequest::Dismiss {
            terminal: terminal.clone(),
            expected_revision: 1,
        },
    );
    assert_eq!(dismissed["applied"], serde_json::json!(true));
    assert_eq!(
        visibility(&dismissed).state,
        TerminalVisibilityState::Dismissed
    );

    // A stale observe never lowers the dismissed state (idempotent no-op).
    let idempotent = send(
        &mut owner,
        TerminalAction::Observe,
        TerminalRequest::Observe {
            terminal,
            expected_revision: 0,
        },
    );
    assert_eq!(idempotent["applied"], serde_json::json!(false));
    assert_eq!(idempotent["conflict"], serde_json::json!(false));
    assert_eq!(
        visibility(&idempotent).state,
        TerminalVisibilityState::Dismissed
    );

    // A well-formed but non-visibility payload under a visibility action is
    // a safe rejection, never routed to a terminal handler.
    let mismatch = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Observe,
            serde_json::to_value(TerminalRequest::Attach {
                terminal: TerminalRef {
                    daemon_generation: DaemonGeneration::new(),
                    terminal_id: TerminalId::new(),
                    workspace_id: WorkspaceId::new(),
                    session_id: None,
                    worktree_id: WorktreeId::new(),
                },
                geometry: None,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap_err();
    assert_eq!(mismatch.code, ErrorCode::InvalidArgument);

    // A malformed visibility payload is a safe rejection.
    let error = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Observe,
            json!({ "operation": "bogus" }),
            SnapshotWire::RawTail,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}

#[test]
fn trimmed_agent_output_maps_to_a_resync_protocol_error() {
    let error = map_runtime_error(RuntimeError::Terminal(RegistryError::ResyncRequired));

    assert_eq!(error.code, ErrorCode::ResyncRequired);
}
