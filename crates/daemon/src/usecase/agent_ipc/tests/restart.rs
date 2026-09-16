//! restart の振る舞いを固定するテスト。

use super::*;

#[test]
#[allow(clippy::too_many_lines)] // One end-to-end usecase scenario proves stop, lost response, migration, and replay together.
fn doctor_restarts_only_outdated_idle_integration_and_migrates_exact_resume() {
    let workspace = WorkspaceId::new();
    let resolved = scope();
    let mut agent = AgentRuntime::new(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            terminate_success: true,
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    let admission = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    let runtime_id = agent
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap()
        .agent_runtime_id;
    let mcp_caller = agent
        .mcp_callers
        .values_mut()
        .next()
        .expect("launch registers one MCP caller");
    mcp_caller.child = Some(McpChildLease {
        pid: 9001,
        process_start_identity: "process-9001".into(),
        connection: Some(ConnectionId::new()),
    });
    let mut snapshot = agent.coordinator.snapshot();
    snapshot.records[0].launch.plan.profile_revision = 1;
    snapshot.records[0]
        .provider_resume
        .as_mut()
        .unwrap()
        .adapter_revision = 1;
    agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    agent
        .reported_phases
        .insert(runtime_id, AgentPhase::Waiting);
    let current_revision = crate::usecase::claude::PROFILE_REVISION;
    let expected = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: current_revision,
    }];

    let diagnosis = agent.diagnose_integrations(workspace, &expected).unwrap();
    assert_eq!(diagnosis.outdated.len(), 1);
    assert_eq!(diagnosis.outdated[0].actual_revision, 1);
    assert_eq!(diagnosis.outdated[0].expected_revision, current_revision);
    assert_eq!(diagnosis.outdated[0].phase, AgentPhase::Waiting);
    assert!(diagnosis.outdated[0].resume_available);
    assert_eq!(diagnosis.outdated_mcp_children, 1);
    assert_eq!(diagnosis.provisioned_mcp_callers, Some(1));
    let newcomer = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    let newcomer_runtime = agent
        .coordinator
        .runtime_for_terminal(&newcomer.terminal)
        .unwrap();
    let mut snapshot = agent.coordinator.snapshot();
    let newcomer_record = snapshot
        .records
        .iter_mut()
        .find(|record| record.runtime == newcomer_runtime)
        .unwrap();
    newcomer_record.launch.plan.profile_revision = 1;
    newcomer_record
        .provider_resume
        .as_mut()
        .unwrap()
        .adapter_revision = 1;
    agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    agent
        .reported_phases
        .insert(newcomer_runtime.agent_runtime_id, AgentPhase::Waiting);
    let (interrupted, stopped) = agent
        .interrupt_outdated_agents(
            workspace,
            &expected,
            &diagnosis
                .outdated
                .iter()
                .map(|item| item.runtime.clone())
                .collect::<Vec<_>>(),
            false,
        )
        .unwrap();
    assert_eq!(interrupted, 1);
    assert_eq!(stopped.outdated, diagnosis.outdated);
    assert_eq!(stopped.outdated_mcp_children, 1);
    assert_eq!(
        stopped.provisioned_mcp_callers,
        Some(2),
        "the diagnosis taken at interruption includes a newly minted, unclaimed credential"
    );
    assert_eq!(
        agent
            .coordinator
            .runtime_for_terminal(&newcomer.terminal)
            .unwrap(),
        newcomer_runtime.clone(),
        "an Agent that became outdated after diagnosis is not part of the confirmed selection"
    );
    assert_eq!(agent.mcp_callers.len(), 1);
    assert_eq!(
        agent
            .diagnose_integrations(workspace, &expected)
            .unwrap()
            .outdated
            .len(),
        2,
        "the stopped source and the unselected newcomer both remain diagnosable"
    );
    assert_eq!(
        agent
            .interrupt_outdated_agents(
                workspace,
                &expected,
                &diagnosis
                    .outdated
                    .iter()
                    .map(|item| item.runtime.clone())
                    .collect::<Vec<_>>(),
                false,
            )
            .unwrap()
            .0,
        0
    );
    let target = agent.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    assert_eq!(
        agent
            .resume_with_current_integration(
                &OperationId::new().to_string(),
                &target,
                current_revision + 1,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    let repair_operation = OperationId::new().to_string();
    let replacement = agent
        .resume_with_current_integration(
            &repair_operation,
            &target,
            current_revision,
            &FakeScope(Ok(resolved)),
        )
        .unwrap();
    assert!(
        agent
            .prepare_current_integration_resume_readiness(
                &repair_operation,
                &target,
                current_revision,
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        agent
            .resume_with_current_integration(
                &repair_operation,
                &target,
                current_revision,
                &FakeScope(Ok(scope())),
            )
            .unwrap(),
        replacement
    );
    assert_ne!(replacement.terminal, admission.terminal);
    let after = agent.diagnose_integrations(workspace, &expected).unwrap();
    assert_eq!(after.outdated.len(), 1);
    assert_eq!(after.outdated[0].runtime, newcomer_runtime);
    assert_eq!(
        agent
            .resume_with_current_integration(
                &repair_operation,
                &target,
                current_revision + 1,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
}

#[test]
fn daemon_restart_plan_revalidates_every_live_agent_before_interrupting() {
    let workspace = WorkspaceId::new();
    let mut agent = runtime();
    agent.pty = Box::new(Pty {
        terminate_success: true,
        ..Pty::default()
    });
    let admission = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let expected = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: crate::usecase::claude::PROFILE_REVISION,
    }];
    assert_eq!(
        agent
            .plan_daemon_restart_agents(&expected, false)
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );
    let credential = agent.mcp_callers.keys().next().cloned().unwrap();
    agent
        .report_agent_phase(&credential, AgentPhase::Waiting)
        .unwrap();
    let plan = agent.plan_daemon_restart_agents(&expected, false).unwrap();
    assert_eq!(plan.agents.len(), 1);
    assert_eq!(plan.agents[0].runtime.terminal, admission.terminal);
    assert_eq!(plan.agents[0].phase, AgentPhase::Waiting);
    assert!(
        !agent
            .daemon_restart_restore_needed(&plan.agents[0].runtime)
            .unwrap()
    );

    let mut stale = plan.agents[0].runtime.clone();
    stale.agent_runtime_id = AgentRuntimeId::new();
    assert_eq!(
        agent
            .interrupt_agents_for_daemon_restart(&expected, &[stale], true,)
            .unwrap_err()
            .error
            .code,
        ErrorCode::StaleTarget
    );
    assert_eq!(agent.provisioned_mcp_callers(), 1);

    let current = agent.plan_daemon_restart_agents(&expected, true).unwrap();
    let stopped = agent
        .interrupt_agents_for_daemon_restart(
            &expected,
            &current
                .agents
                .iter()
                .map(|item| item.runtime.clone())
                .collect::<Vec<_>>(),
            true,
        )
        .unwrap();
    assert_eq!(stopped, current);
    assert!(
        agent
            .daemon_restart_restore_needed(&stopped.agents[0].runtime)
            .unwrap()
    );
    assert_eq!(agent.provisioned_mcp_callers(), 0);
    assert!(
        agent
            .inventory(workspace)
            .runtimes
            .iter()
            .all(|item| item.state == AgentRuntimeInventoryState::Exited)
    );
}

#[test]
fn daemon_restart_plan_refuses_every_unprovable_agent_authority() {
    let expected = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: crate::usecase::claude::PROFILE_REVISION,
    }];
    let workspace = WorkspaceId::new();

    let mut incomplete = runtime();
    incomplete
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    assert_eq!(
        incomplete
            .plan_daemon_restart_agents(&[], true)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let mut snapshot = incomplete.coordinator.snapshot();
    snapshot.records[0].provider_resume = None;
    incomplete.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    assert_eq!(
        incomplete
            .plan_daemon_restart_agents(&expected, true)
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );

    let mut unmatched = runtime();
    unmatched
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let credential = unmatched.mcp_callers.keys().next().cloned().unwrap();
    unmatched
        .mcp_callers
        .get_mut(&credential)
        .unwrap()
        .runtime
        .agent_runtime_id = AgentRuntimeId::new();
    assert_eq!(
        unmatched
            .plan_daemon_restart_agents(&expected, true)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );

    let mut historical = runtime();
    let exited = historical
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: None,
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    historical.exit(&exited.terminal, 0).unwrap();
    assert!(
        historical
            .plan_daemon_restart_agents(&expected, true)
            .unwrap()
            .agents
            .is_empty()
    );
}

#[test]
fn daemon_restart_plan_refuses_agents_from_multiple_workspaces_before_stopping_any() {
    let first_workspace = WorkspaceId::new();
    let second_workspace = WorkspaceId::new();
    let mut agent = runtime();
    for workspace in [first_workspace, second_workspace] {
        agent
            .launch(
                &OperationId::new().to_string(),
                &AgentLaunchIntent {
                    workspace,
                    session: None,
                    profile: Some(AgentProfileId::new("claude").unwrap()),
                },
                &FakeScope(Ok(scope())),
            )
            .unwrap();
    }
    for credential in agent.mcp_callers.keys().cloned().collect::<Vec<_>>() {
        agent
            .report_agent_phase(&credential, AgentPhase::Waiting)
            .unwrap();
    }
    let expected = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: crate::usecase::claude::PROFILE_REVISION,
    }];

    let refusal = agent
        .plan_daemon_restart_agents(&expected, false)
        .unwrap_err();

    assert_eq!(refusal.code, ErrorCode::Busy);
    assert!(refusal.message.contains("multiple workspaces"));
    assert_eq!(agent.provisioned_mcp_callers(), 2);
    assert!(
        agent
            .coordinator
            .snapshot()
            .records
            .iter()
            .all(|record| { record.state == crate::usecase::runtime::RuntimeState::Running })
    );
}

#[test]
fn daemon_restart_interruption_reports_only_the_partial_stop_for_rollback() {
    let workspace = WorkspaceId::new();
    let mut agent = runtime();
    agent.pty = Box::new(Pty {
        terminate_success: true,
        terminate_fail_at: Some(2),
        spawn_counter: Some(Arc::new(AtomicU32::new(100))),
        ..Pty::default()
    });
    for workspace in [workspace, workspace] {
        agent
            .launch(
                &OperationId::new().to_string(),
                &AgentLaunchIntent {
                    workspace,
                    session: None,
                    profile: Some(AgentProfileId::new("claude").unwrap()),
                },
                &FakeScope(Ok(scope())),
            )
            .unwrap();
    }
    for credential in agent.mcp_callers.keys().cloned().collect::<Vec<_>>() {
        agent
            .report_agent_phase(&credential, AgentPhase::Waiting)
            .unwrap();
    }
    let expected = [AgentIntegrationRevision {
        profile_id: AgentProfileId::new("claude").unwrap(),
        revision: crate::usecase::claude::PROFILE_REVISION,
    }];
    let plan = agent.plan_daemon_restart_agents(&expected, false).unwrap();
    let failure = agent
        .interrupt_agents_for_daemon_restart(
            &expected,
            &plan
                .agents
                .iter()
                .map(|item| item.runtime.clone())
                .collect::<Vec<_>>(),
            false,
        )
        .unwrap_err();

    assert_eq!(failure.error.code, ErrorCode::OwnershipUnknown);
    assert_eq!(failure.interrupted.agents.len(), 1);
    assert_eq!(agent.provisioned_mcp_callers(), 1);
    let snapshot = agent.coordinator.snapshot();
    assert_eq!(
        snapshot
            .records
            .iter()
            .filter(|record| record.state == crate::usecase::runtime::RuntimeState::Exited)
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .records
            .iter()
            .filter(|record| {
                record.state
                    == crate::usecase::runtime::RuntimeState::ReconcileRequired(
                        crate::usecase::runtime::ReconcileState::OrphanRunning,
                    )
            })
            .count(),
        1
    );
    assert_eq!(
        agent
            .plan_daemon_restart_agents(&expected, true)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
fn restart_resume_supersedes_the_interrupted_runtime_without_leaking_capacity() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let mut first = restart_runtime();
    let initial = first
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    let initial_runtime = first
        .coordinator
        .runtime_for_terminal(&initial.terminal)
        .unwrap();
    let continuation = initial.continuation.unwrap();
    let (reconciled, interrupted) = first
        .coordinator
        .snapshot()
        .reconcile_after_daemon_restart();
    assert_eq!(interrupted, 1);

    let mut second = hydrate_restart_runtime(reconciled);
    // Restart preserves the dispatch journal as well as runtime state;
    // exact resume must not guess an Agent from its provider/model tuple.
    second.dispatch = first.dispatch.clone();
    assert_eq!(second.session_phase(session), AgentPhase::Interrupted);
    assert_eq!(second.coordinator.occupied_slots(), 1);

    let target = second.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    assert_eq!(target.continuation, continuation);
    let resume_operation = OperationId::new().to_string();
    let resumed = second
        .resume_exact(&resume_operation, &target, &FakeScope(Ok(resolved.clone())))
        .unwrap();
    assert_ne!(resumed.terminal, initial.terminal);
    assert_eq!(resumed.continuation, Some(continuation));
    assert_eq!(
        resumed.resume_relation.as_ref().unwrap().source,
        target.source
    );
    assert_eq!(second.coordinator.occupied_slots(), 1);
    let superseded = second.coordinator.record_for(&initial_runtime).unwrap();
    assert_eq!(
        superseded.state,
        crate::usecase::runtime::RuntimeState::Reclaimed
    );
    assert_eq!(
        superseded
            .provider_resume
            .as_ref()
            .unwrap()
            .last_known_status,
        ProviderResumeStatus::Exited
    );
    assert_eq!(
        superseded.superseded_by,
        resumed
            .resume_relation
            .as_ref()
            .map(|relation| relation.replacement_runtime)
    );
    assert!(matches!(
        second.coordinator.retention().lookup(&initial.terminal),
        usagi_core::domain::terminal_retention::FinalLookup::Retained(_)
    ));
    second.coordinator.snapshot().validate_ownership().unwrap();

    let (reconciled_again, interrupted_again) = second
        .coordinator
        .snapshot()
        .reconcile_after_daemon_restart();
    assert_eq!(interrupted_again, 1);
    let mut third = hydrate_restart_runtime(reconciled_again);
    third.dispatch = second.dispatch.clone();
    let replay = third
        .resume_exact(&resume_operation, &target, &FakeScope(Ok(resolved.clone())))
        .unwrap();
    assert_eq!(replay.terminal, resumed.terminal);
    assert_eq!(replay.continuation, Some(continuation));
    assert_eq!(replay.resume_relation, resumed.resume_relation);
    let double_click = third
        .resume_exact(
            &OperationId::new().to_string(),
            &target,
            &FakeScope(Ok(resolved)),
        )
        .unwrap();
    assert_eq!(double_click.terminal, resumed.terminal);
    assert_eq!(double_click.resume_relation, resumed.resume_relation);
    assert_eq!(third.coordinator.snapshot().records.len(), 2);

    assert_eq!(third.session_phase(session), AgentPhase::Interrupted);
}

#[test]
fn restart_reconciles_prepared_admission_without_spawning_a_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let dispatch_dir = dir.path().join("dispatch");
    let spawns = Arc::new(AtomicU32::new(0));
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let mut first = AgentRuntime::with_dispatch(
        DaemonGeneration::new(),
        claude_registry(),
        Store {
            saves: 0,
            fail_after: Some(0),
            ..Store::default()
        },
        Journal::default(),
        Pty {
            spawn_counter: Some(Arc::clone(&spawns)),
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(&dispatch_dir),
    );
    assert_eq!(
        first
            .launch(&operation, &launch_intent, &FakeScope(Ok(scope())),)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 0);
    drop(first);

    let mut second = AgentRuntime::hydrate_with_dispatch_and_locator(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            spawn_counter: Some(Arc::clone(&spawns)),
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(&dispatch_dir),
        PathExecutableLocator,
        RuntimeStoreSnapshot::default(),
    )
    .unwrap();
    assert_eq!(
        second
            .launch(&operation, &launch_intent, &FakeScope(Ok(scope())),)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    let mut conflict = launch_intent;
    conflict.workspace = WorkspaceId::new();
    let mut third = AgentRuntime::with_dispatch(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(&dispatch_dir),
    );
    assert_eq!(
        third
            .launch(&operation, &conflict, &FakeScope(Ok(scope())))
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 0);
    assert_eq!(
        second
            .dispatch
            .run(OperationId::parse(&operation).unwrap())
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Failed
    );
}

#[test]
fn workflow_restart_replays_interrupted_admission_without_spawning_a_replacement() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut first = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let intent = intent(None);
    let operation = OperationId::new().to_string();
    let prompt = "workflow immutable goal";
    let ticket = first
        .prepare_workflow_readiness(&operation, &intent, prompt)
        .unwrap();
    first
        .launch_workflow_after_readiness(
            &operation,
            &intent,
            prompt,
            &FakeScope(Ok(scope())),
            ticket.as_ref(),
        )
        .unwrap();
    let dispatch = first.dispatch.clone();
    let (snapshot, count) = first
        .coordinator
        .snapshot()
        .reconcile_after_daemon_restart();
    assert_eq!(count, 1);
    let spawns = Arc::new(AtomicU32::new(0));
    let mut restored = AgentRuntime::hydrate_with_dispatch_and_locator(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty {
            spawn_counter: Some(Arc::clone(&spawns)),
            ..Pty::default()
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        dispatch,
        FixtureLocator(fixture.path().to_path_buf()),
        snapshot,
    )
    .unwrap();
    assert!(
        restored
            .prepare_workflow_readiness(&operation, &intent, prompt)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        restored
            .launch_workflow_after_readiness(
                &operation,
                &intent,
                prompt,
                &FakeScope(Ok(scope())),
                None
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        restored
            .prepare_workflow_readiness(&operation, &intent, "different goal")
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 0);
}

#[test]
fn peer_plan_is_stable_before_admission_and_across_runtime_restart() {
    let runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let caller = CallerRef {
        session_id: Some(session),
        agent_id: AgentId::new(),
    };
    let operation = OperationId::new();
    let selected = DispatchAgentIntent::New {
        runtime: AgentProfileId::new("claude").unwrap(),
        model: ModelSelector::new("default").unwrap(),
    };
    let worker = runtime
        .plan_peer_worker(&operation.to_string(), workspace, &caller, &selected)
        .unwrap();
    assert_eq!(
        runtime
            .plan_peer_worker(&operation.to_string(), workspace, &caller, &selected)
            .unwrap()
            .agent_id,
        worker.agent_id
    );
    let restarted = self::runtime();
    assert_eq!(
        restarted
            .plan_peer_worker(&operation.to_string(), workspace, &caller, &selected)
            .unwrap()
            .agent_id,
        worker.agent_id
    );
    assert_ne!(
        peer_worker_id(OperationId::new(), workspace, session),
        worker.agent_id
    );
    assert_ne!(
        peer_worker_id(operation, WorkspaceId::new(), session),
        worker.agent_id
    );
    assert_ne!(
        peer_worker_id(operation, workspace, SessionId::new()),
        worker.agent_id
    );
}
