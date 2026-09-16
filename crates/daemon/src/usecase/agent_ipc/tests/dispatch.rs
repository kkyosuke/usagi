//! dispatch の振る舞いを固定するテスト。

use super::*;

#[test]
fn daemon_dispatch_store_requires_ownership_without_reparenting() {
    let directory = tempfile::tempdir().unwrap();
    let store = DispatchStore::new(directory.path());
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let worker = Agent {
        agent_id: AgentId::new(),
        session_id: Some(session),
        runtime: AgentProfileId::new("claude").unwrap(),
        model: ModelSelector::new("test").unwrap(),
        status: AgentStatus::Starting,
        current_run: None,
    };
    let admission = |operation, parent| {
        (
            worker.clone(),
            DispatchRun {
                run_id: operation,
                agent_id: worker.agent_id,
                prompt: "dispatch ownership".into(),
                started_at: Utc::now(),
                ended_at: None,
                status: RunStatus::Preparing,
            },
            DispatchBinding {
                run_id: operation,
                caller: CallerRef {
                    session_id: Some(parent),
                    agent_id: AgentId::new(),
                },
                worker: WorkerRef {
                    session_id: Some(session),
                    agent_id: worker.agent_id,
                },
            },
            AgentAdmissionReservation {
                operation_id: operation,
                semantic_key: "dispatch-ownership".into(),
                credential_provenance: DispatchCredentialProvenance::DaemonMintedEphemeral,
            },
        )
    };

    let missing = OperationId::new();
    let (agent, run, binding, reservation) = admission(missing, SessionId::new());
    assert!(
        store
            .reserve_admission(agent, run, binding, reservation)
            .is_err()
    );
    assert!(store.run(missing).unwrap().is_none());

    store.upsert_agent(workspace, worker.clone()).unwrap();
    let initial_parent = SessionId::new();
    store
        .record_session_parent(workspace, session, Some(initial_parent))
        .unwrap();
    let admitted = OperationId::new();
    let (agent, run, binding, reservation) = admission(admitted, SessionId::new());
    store
        .reserve_admission(agent, run, binding, reservation)
        .unwrap();

    let conflicting = OperationId::new();
    let (agent, run, binding, reservation) = admission(conflicting, SessionId::new());
    store
        .reserve_admission(agent, run, binding, reservation)
        .unwrap();
    assert!(store.run(conflicting).unwrap().is_some());
    assert!(store.admission(conflicting).unwrap().is_some());
    assert_eq!(
        store.session_parent(workspace, session).unwrap(),
        Some(initial_parent)
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn readiness_admission_wrappers_cover_launch_exact_and_dispatch() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let launch = AgentLaunchIntent {
        workspace,
        session: Some(session),
        profile: Some(AgentProfileId::new("claude").unwrap()),
    };
    let operation = OperationId::new().to_string();
    let ticket = runtime
        .prepare_launch_readiness(&operation, &launch)
        .unwrap()
        .unwrap();
    assert_eq!(ticket.product(), "claude");
    let admitted = runtime
        .launch_after_readiness(
            &operation,
            &launch,
            &FakeScope(Ok(resolved.clone())),
            Some(&ticket),
        )
        .unwrap();
    assert!(
        runtime
            .launch_after_readiness(&operation, &launch, &FakeScope(Ok(resolved.clone())), None,)
            .is_ok(),
        "a concurrent completed admission replays without a ticket"
    );
    runtime.exit(&admitted.terminal, 0).unwrap();
    let target = runtime.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    let resume_operation = OperationId::new().to_string();
    let resume_ticket = runtime
        .prepare_resume_readiness(&resume_operation, &target)
        .unwrap()
        .unwrap();
    let resumed = runtime
        .resume_exact_after_readiness(
            &resume_operation,
            &target,
            &FakeScope(Ok(resolved.clone())),
            Some(&resume_ticket),
        )
        .unwrap();
    runtime.exit(&resumed.terminal, 0).unwrap();
    let repair_target = runtime.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    let repair_operation = OperationId::new().to_string();
    let repair_ticket = runtime
        .prepare_current_integration_resume_readiness(&repair_operation, &repair_target, 2)
        .unwrap()
        .unwrap();
    runtime
        .resume_with_current_integration_after_readiness(
            &repair_operation,
            &repair_target,
            2,
            &FakeScope(Ok(resolved.clone())),
            Some(&repair_ticket),
        )
        .unwrap();
    assert_eq!(
        runtime
            .resume_with_current_integration_after_readiness(
                "invalid",
                &repair_target,
                2,
                &FakeScope(Ok(resolved.clone())),
                None,
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let worktree = tempfile::tempdir().unwrap();
    let mut dispatch_runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let dispatch = DispatchIntent {
        workspace,
        session_name: "worker".into(),
        caller: CallerRef {
            session_id: None,
            agent_id: AgentId::new(),
        },
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: ModelSelector::new("test").unwrap(),
        },
        prompt: "work".into(),
    };
    let dispatch_operation = OperationId::new().to_string();
    let dispatch_ticket = dispatch_runtime
        .prepare_dispatch_readiness(&dispatch_operation, &dispatch)
        .unwrap()
        .unwrap();
    dispatch_runtime
        .dispatch_after_readiness(
            &dispatch_operation,
            &dispatch,
            session,
            &FakeScope(Ok(configured_scope(worktree.path()))),
            Some(&dispatch_ticket),
        )
        .unwrap();

    let planned_session = SessionId::new();
    let planned_operation = OperationId::new().to_string();
    let planned_ticket = dispatch_runtime
        .prepare_dispatch_readiness(&planned_operation, &dispatch)
        .unwrap()
        .unwrap();
    let planned = dispatch_runtime
        .plan_dispatch_worker(workspace, planned_session, &dispatch.agent)
        .unwrap();
    dispatch_runtime
        .dispatch_planned_after_readiness(
            &planned_operation,
            &dispatch,
            planned_session,
            &FakeScope(Ok(configured_scope(worktree.path()))),
            Some(&planned_ticket),
            &planned,
        )
        .unwrap();

    let mut wrong_session = planned.clone();
    wrong_session.session_id = Some(SessionId::new());
    let wrong_session_operation = OperationId::new().to_string();
    let wrong_session_ticket = dispatch_runtime
        .prepare_dispatch_readiness(&wrong_session_operation, &dispatch)
        .unwrap()
        .unwrap();
    assert_eq!(
        dispatch_runtime
            .dispatch_planned_after_readiness(
                &wrong_session_operation,
                &dispatch,
                planned_session,
                &FakeScope(Ok(configured_scope(worktree.path()))),
                Some(&wrong_session_ticket),
                &wrong_session,
            )
            .unwrap_err()
            .code,
        ErrorCode::RevisionConflict
    );
    let existing_selection = DispatchIntent {
        agent: DispatchAgentIntent::Existing {
            agent_id: planned.agent_id,
        },
        ..dispatch
    };
    let mut wrong_agent = planned.clone();
    wrong_agent.agent_id = AgentId::new();
    let wrong_agent_operation = OperationId::new().to_string();
    let wrong_agent_ticket = dispatch_runtime
        .prepare_dispatch_readiness(&wrong_agent_operation, &existing_selection)
        .unwrap()
        .unwrap();
    assert_eq!(
        dispatch_runtime
            .dispatch_planned_after_readiness(
                &wrong_agent_operation,
                &existing_selection,
                planned_session,
                &FakeScope(Ok(configured_scope(worktree.path()))),
                Some(&wrong_agent_ticket),
                &wrong_agent,
            )
            .unwrap_err()
            .code,
        ErrorCode::RevisionConflict
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One restart scenario keeps the two runtime instances and shared file visibly ordered.
fn restart_hydrates_file_snapshot_before_dispatch_admission_and_preserves_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let snapshot_path = dir.path().join("agents.json");
    let dispatch_dir = dir.path().join("dispatch");
    let executable_dir = tempfile::tempdir().unwrap();
    std::fs::write(executable_dir.path().join("claude"), "fixture").unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let resolved = configured_scope(worktree.path());
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let caller = CallerRef {
        session_id: Some(SessionId::new()),
        agent_id: usagi_core::domain::id::AgentId::new(),
    };
    let dispatch_intent = |prompt: &str| DispatchIntent {
        workspace,
        session_name: "worker".into(),
        caller: caller.clone(),
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: usagi_core::domain::agent::ModelSelector::new("test").unwrap(),
        },
        prompt: prompt.into(),
    };
    let spawns = Arc::new(AtomicU32::new(0));
    let make_fresh = || {
        AgentRuntime::with_dispatch_and_locator(
            DaemonGeneration::new(),
            claude_registry(),
            Store {
                snapshot_path: Some(snapshot_path.clone()),
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
            FixtureLocator(executable_dir.path().to_path_buf()),
        )
    };
    let mut first = make_fresh();
    let successful = OperationId::new().to_string();
    let success_terminal = first
        .dispatch(
            &successful,
            &dispatch_intent("success"),
            session,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap()
        .terminal;
    first.exit(&success_terminal, 0).unwrap();
    let unsuccessful = OperationId::new().to_string();
    let failed_terminal = first
        .dispatch(
            &unsuccessful,
            &dispatch_intent("failure"),
            session,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap()
        .terminal;
    first.exit(&failed_terminal, 17).unwrap();
    let interrupted = OperationId::new().to_string();
    first
        .dispatch(
            &interrupted,
            &dispatch_intent("pending"),
            session,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    let old_credential = first.mcp_callers.keys().next().unwrap().clone();
    assert_eq!(spawns.load(Ordering::SeqCst), 3);
    drop(first);

    let loaded: RuntimeStoreSnapshot =
        serde_json::from_slice(&std::fs::read(&snapshot_path).unwrap()).unwrap();
    loaded.validate_schema().unwrap();
    loaded.validate_ownership().unwrap();
    let interrupted_record = loaded
        .records
        .iter()
        .find(|record| record.operation.operation_id.to_string() == interrupted)
        .unwrap()
        .clone();
    let (reconciled, count) = loaded.reconcile_after_daemon_restart();
    assert_eq!(count, 1);
    let reconciled_interrupted = reconciled
        .records
        .iter()
        .find(|record| record.operation.operation_id.to_string() == interrupted)
        .unwrap();
    assert_eq!(
        reconciled_interrupted
            .provider_resume
            .as_ref()
            .unwrap()
            .last_known_status,
        ProviderResumeStatus::Interrupted
    );
    assert_eq!(
        reconciled_interrupted
            .provider_resume
            .as_ref()
            .unwrap()
            .last_known_phase,
        Some(ProviderResumePhase::Interrupted)
    );
    assert!(reconciled.generation.current.is_none());
    assert!(
        reconciled
            .generation
            .records
            .iter()
            .all(|record| { record.role == crate::usecase::generation::GenerationRole::Retired })
    );
    Store {
        snapshot_path: Some(snapshot_path.clone()),
        ..Store::default()
    }
    .save(reconciled.clone())
    .unwrap();
    let mut second = AgentRuntime::hydrate_with_dispatch_and_locator(
        DaemonGeneration::new(),
        claude_registry(),
        Store {
            snapshot_path: Some(snapshot_path.clone()),
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
        FixtureLocator(executable_dir.path().to_path_buf()),
        reconciled,
    )
    .unwrap();

    // Replay is resolved before current admission checks; the executable
    // disappearing after restart cannot turn a durable final into a new
    // launch failure (or authorize a replacement spawn).
    std::fs::remove_file(executable_dir.path().join("claude")).unwrap();
    let replay = second
        .dispatch(
            &successful,
            &dispatch_intent("success"),
            session,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    assert!(replay.completed);
    assert_eq!(replay.terminal, success_terminal);
    assert_eq!(
        second
            .dispatch(
                &unsuccessful,
                &dispatch_intent("failure"),
                session,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(
        second
            .dispatch(
                &interrupted,
                &dispatch_intent("pending"),
                session,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        second
            .dispatch(
                &successful,
                &dispatch_intent("different"),
                session,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 3);
    assert!(second.mcp_caller(&old_credential).is_none());
    assert_eq!(
        second
            .output(&interrupted_record.runtime.terminal, b"late".to_vec())
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        second
            .exit(&interrupted_record.runtime.terminal, 0)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    let inbox_before = second.dispatch.inbox(&caller).unwrap();
    second
        .report(
            &interrupted_record.runtime,
            &interrupted_record.operation,
            InboxKind::Completed,
            "late completion".into(),
            None,
        )
        .unwrap();
    assert_eq!(second.dispatch.inbox(&caller).unwrap(), inbox_before);
    let inventory =
        second
            .coordinator
            .inventory(&usagi_core::domain::terminal_launch::TerminalLaunchScope {
                workspace_id: workspace,
                session_id: Some(session),
                worktree_id: resolved.worktree_id,
            });
    assert!(inventory.iter().all(|entry| !entry.live));

    second
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(resolved)),
        )
        .unwrap();
    assert_eq!(spawns.load(Ordering::SeqCst), 4);
    let saved: RuntimeStoreSnapshot =
        serde_json::from_slice(&std::fs::read(snapshot_path).unwrap()).unwrap();
    saved.validate_ownership().unwrap();
    assert_eq!(saved.records.len(), 4);
    assert!(saved.generation.current.is_some());
    assert_eq!(
        saved
            .generation
            .records
            .iter()
            .filter(|record| { record.role == crate::usecase::generation::GenerationRole::Active })
            .count(),
        1
    );
    assert!(saved.records.iter().any(|record| {
        record.operation.operation_id.to_string() == successful
            && record.outcome == crate::usecase::runtime::DurableOperationOutcome::Completed
    }));
}

#[test]
fn missing_dispatch_binding_is_a_safe_noop_for_report_and_observer_exit() {
    let mut runtime = runtime();
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let terminal = runtime
        .launch(&operation, &launch_intent, &FakeScope(Ok(scope())))
        .unwrap()
        .terminal;
    let runtime_ref = runtime.coordinator.runtime_for_terminal(&terminal).unwrap();
    let fence = runtime
        .coordinator
        .record_for(&runtime_ref)
        .unwrap()
        .operation
        .clone();
    runtime.dispatch = DispatchStore::new(tempfile::tempdir().unwrap().keep());
    runtime.mcp_callers.insert(
        "missing-binding".into(),
        McpCaller {
            runtime: runtime_ref.clone(),
            operation: fence.operation_id,
            child: None,
        },
    );
    assert_eq!(
        runtime
            .report_from_mcp(
                "missing-binding",
                None,
                InboxKind::Completed,
                "missing binding".into(),
                None,
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    runtime
        .report(
            &runtime_ref,
            &fence,
            InboxKind::Completed,
            "missing binding".into(),
            None,
        )
        .unwrap();
    runtime.exit(&terminal, 0).unwrap();
}

#[test]
fn delegated_dispatch_requires_the_authenticated_callers_runtime() {
    let runtime = runtime();
    let workspace = WorkspaceId::new();
    let caller_agent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            Some(SessionId::new()),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("manager").unwrap(),
        )
        .unwrap();
    let same_runtime_agent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            Some(SessionId::new()),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("worker").unwrap(),
        )
        .unwrap();
    let other_runtime_agent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            Some(SessionId::new()),
            AgentProfileId::new("codex").unwrap(),
            ModelSelector::new("worker").unwrap(),
        )
        .unwrap();
    let caller = CallerRef {
        session_id: caller_agent.session_id,
        agent_id: caller_agent.agent_id,
    };

    for selected in [
        DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: ModelSelector::new("different-model-is-allowed").unwrap(),
        },
        DispatchAgentIntent::Existing {
            agent_id: same_runtime_agent.agent_id,
        },
    ] {
        runtime
            .require_same_dispatch_runtime(workspace, &caller, &selected)
            .unwrap();
    }
    for selected in [
        DispatchAgentIntent::New {
            runtime: AgentProfileId::new("codex").unwrap(),
            model: ModelSelector::new("worker").unwrap(),
        },
        DispatchAgentIntent::Existing {
            agent_id: other_runtime_agent.agent_id,
        },
    ] {
        assert_eq!(
            runtime
                .require_same_dispatch_runtime(workspace, &caller, &selected)
                .unwrap_err()
                .code,
            ErrorCode::PermissionDenied
        );
    }
    assert_eq!(
        runtime
            .require_same_dispatch_runtime(
                workspace,
                &CallerRef {
                    session_id: None,
                    agent_id: usagi_core::domain::id::AgentId::new(),
                },
                &DispatchAgentIntent::New {
                    runtime: AgentProfileId::new("claude").unwrap(),
                    model: ModelSelector::new("worker").unwrap(),
                },
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One dispatch lifetime exercises claim, reconnect, PID reuse, replay, and exit invalidation.
fn dispatch_launches_once_persists_binding_and_synthesizes_no_report_on_exit() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let caller = CallerRef {
        session_id: Some(SessionId::new()),
        agent_id: usagi_core::domain::id::AgentId::new(),
    };
    let operation = OperationId::new().to_string();
    let dispatch = DispatchIntent {
        workspace,
        session_name: "worker".into(),
        caller: caller.clone(),
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: usagi_core::domain::agent::ModelSelector::new("test").unwrap(),
        },
        prompt: "finish the task".into(),
    };
    let admission = runtime
        .dispatch(
            &operation,
            &dispatch,
            session,
            &FakeScope(Ok(configured_scope(worktree.path()))),
        )
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();
    let durable_snapshot = serde_json::to_string(&runtime.coordinator.snapshot()).unwrap();
    assert!(durable_snapshot.contains("daemon_minted_ephemeral"));
    assert!(!durable_snapshot.contains(&credential));
    assert_eq!(
        runtime.mcp_caller(&credential),
        Some(OperationId::parse(&operation).unwrap())
    );
    let first_connection = ConnectionId::new();
    let second_connection = ConnectionId::new();
    assert!(!runtime.authenticate_mcp_child_connection(
        &credential,
        9001,
        "start-a",
        first_connection
    ));
    assert!(
        runtime
            .claim_mcp_child(9001, "start-a", 9998, 4321, first_connection, &|_, _| true)
            .is_err()
    );
    let ambiguous = runtime.mcp_callers[&credential].clone();
    runtime
        .mcp_callers
        .insert("ambiguous-runtime".into(), ambiguous);
    assert_eq!(
        runtime
            .claim_mcp_child(9001, "start-a", 4321, 4321, first_connection, &|_, _| true)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    runtime.mcp_callers.remove("ambiguous-runtime");
    assert_eq!(
        runtime
            .claim_mcp_child(9001, "start-a", 4321, 4321, first_connection, &|_, _| true)
            .unwrap(),
        credential
    );
    assert!(runtime.authenticate_mcp_child_connection(
        &credential,
        9001,
        "start-a",
        second_connection
    ));
    assert!(!runtime.authenticate_mcp_child_connection(
        &credential,
        9002,
        "start-a",
        second_connection
    ));
    assert!(!runtime.authenticate_mcp_child_connection(
        &credential,
        9001,
        "reused-pid",
        second_connection
    ));
    assert!(
        runtime
            .claim_mcp_child(9002, "start-b", 4321, 4321, second_connection, &|_, _| true)
            .is_err()
    );
    assert!(
        runtime
            .claim_mcp_child(9001, "start-a", 4321, 9999, second_connection, &|_, _| true)
            .is_err()
    );
    runtime.release_mcp_connection(first_connection);
    assert_eq!(
        runtime.mcp_callers[&credential]
            .child
            .as_ref()
            .and_then(|child| child.connection),
        Some(second_connection)
    );
    runtime.retain_live_mcp_connections(&BTreeSet::from([second_connection]));
    assert_eq!(
        runtime.mcp_callers[&credential]
            .child
            .as_ref()
            .and_then(|child| child.connection),
        Some(second_connection)
    );
    runtime.retain_live_mcp_connections(&BTreeSet::new());
    assert_eq!(
        runtime.mcp_callers[&credential]
            .child
            .as_ref()
            .and_then(|child| child.connection),
        None
    );
    let reconnect = ConnectionId::new();
    assert!(runtime.authenticate_mcp_child_connection(&credential, 9001, "start-a", reconnect));
    runtime.release_mcp_connection(reconnect);
    assert!(
        runtime
            .claim_mcp_child(9003, "start-c", 4321, 4321, reconnect, &|_, _| true)
            .is_err(),
        "a live exact-process claim must not be reassigned after disconnect"
    );
    assert!(!runtime.authenticate_mcp_child_connection(&credential, 9003, "start-c", reconnect));
    assert_eq!(
        runtime
            .claim_mcp_child(9003, "start-c", 4321, 4321, reconnect, &|pid, identity| {
                assert_eq!((pid, identity), (9001, "start-a"));
                false
            })
            .unwrap(),
        credential,
        "a replacement MCP process may claim only after exact death proof"
    );
    assert!(runtime.authenticate_mcp_child_connection(&credential, 9003, "start-c", reconnect));
    assert_eq!(runtime.mcp_caller("forged"), None);
    let run_id = OperationId::parse(&operation).unwrap();
    assert_eq!(
        runtime
            .dispatch_store()
            .binding(run_id)
            .unwrap()
            .unwrap()
            .caller,
        caller
    );
    assert_eq!(runtime.dispatch_store().inbox(&caller).unwrap(), Vec::new());
    assert_eq!(
        runtime
            .dispatch(
                &operation,
                &dispatch,
                session,
                &FakeScope(Ok(configured_scope(worktree.path())))
            )
            .unwrap(),
        admission
    );
    runtime.exit(&admission.terminal, 0).unwrap();
    assert_eq!(runtime.mcp_caller(&credential), None);
    assert!(!runtime.authenticate_mcp_child_connection(&credential, 9003, "start-c", reconnect));
    let inbox = runtime.dispatch_store().inbox(&caller).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, InboxKind::NoReport);
}

#[test]
#[allow(clippy::too_many_lines)] // Related fence and completion branches share one admitted fixture.
fn completed_dispatch_does_not_receive_no_report_and_wrong_fence_is_noop() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let worktree = tempfile::tempdir().unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let parent_session = SessionId::new();
    let parent = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            workspace,
            Some(parent_session),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("manager").unwrap(),
        )
        .unwrap();
    let caller = CallerRef {
        session_id: Some(parent_session),
        agent_id: parent.agent_id,
    };
    let operation = OperationId::new().to_string();
    let dispatch = DispatchIntent {
        workspace,
        session_name: "worker".into(),
        caller: caller.clone(),
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: usagi_core::domain::agent::ModelSelector::new("test").unwrap(),
        },
        prompt: "finish".into(),
    };
    let admission = runtime
        .dispatch(
            &operation,
            &dispatch,
            session,
            &FakeScope(Ok(configured_scope(worktree.path()))),
        )
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();
    assert_eq!(
        runtime
            .report_from_mcp("forged", None, InboxKind::Completed, "ignored".into(), None)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        runtime.mcp_dispatch_caller(&credential).unwrap().session_id,
        Some(session)
    );
    let authenticated = runtime.mcp_dispatch_context(&credential).unwrap();
    assert_eq!(authenticated.workspace_id, workspace);
    assert_eq!(authenticated.run_id.to_string(), operation);
    assert_eq!(authenticated.caller.session_id, Some(session));
    assert_eq!(
        authenticated.terminal_scope,
        TerminalLaunchScope {
            workspace_id: admission.terminal.workspace_id,
            session_id: admission.terminal.session_id,
            worktree_id: admission.terminal.worktree_id,
        }
    );
    assert!(runtime.mcp_dispatch_caller("forged").is_none());
    let runtime_ref = runtime
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap();
    let fence = runtime
        .coordinator
        .record_for(&runtime_ref)
        .unwrap()
        .operation
        .clone();
    let mut wrong = fence.clone();
    wrong.owner_daemon_generation = DaemonGeneration::new();
    runtime
        .report(
            &runtime_ref,
            &wrong,
            InboxKind::Completed,
            "wrong".into(),
            None,
        )
        .unwrap();
    assert!(runtime.dispatch_store().inbox(&caller).unwrap().is_empty());
    assert_eq!(
        runtime
            .report_from_mcp(
                &credential,
                Some(OperationId::new()),
                InboxKind::Completed,
                "wrong run".into(),
                None,
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    let result = usagi_core::domain::agent::StructuredResult {
        pr: Some("https://github.com/o/r/pull/1".into()),
        commits: vec!["abc".into()],
        ..Default::default()
    };
    let delivery = runtime
        .report_from_mcp(
            &credential,
            None,
            InboxKind::Completed,
            "done".into(),
            Some(result.clone()),
        )
        .unwrap();
    assert_eq!(delivery.delivered_to, caller);
    assert_eq!(delivery.worker.session_id, Some(session));
    assert!(delivery.accepted);
    let wake = runtime
        .dispatch_store()
        .queued_prompt(workspace, Some(parent_session))
        .unwrap()
        .expect("a stopped manager must receive a durable wake prompt");
    assert!(wake.prompt.contains("A child report is ready"));
    assert!(wake.prompt.contains("done"));
    assert_eq!(
        delivery
            .committed
            .as_ref()
            .and_then(|message| message.result.as_ref()),
        Some(&result)
    );
    let completed_run = OperationId::parse(&operation).unwrap();
    let completed_binding = runtime
        .dispatch_store()
        .binding(completed_run)
        .unwrap()
        .unwrap();
    let completed_at = runtime
        .dispatch_store()
        .run(completed_run)
        .unwrap()
        .unwrap()
        .ended_at;
    runtime
        .reconcile_report_status(&completed_binding, InboxKind::Completed)
        .unwrap();
    runtime
        .reconcile_report_status(&completed_binding, InboxKind::NoReport)
        .unwrap();
    assert_eq!(
        runtime
            .dispatch_store()
            .run(completed_run)
            .unwrap()
            .unwrap()
            .ended_at,
        completed_at,
        "an already converged retry must preserve its completion time"
    );
    let replacement = usagi_core::domain::agent::StructuredResult {
        pr: Some("https://github.com/o/r/pull/2".into()),
        ..Default::default()
    };
    // Model a crash or storage failure after the inbox append committed but
    // before either registry transition became durable.
    runtime
        .dispatch_store()
        .transition_run(completed_run, RunStatus::Running, None)
        .unwrap();
    runtime
        .dispatch_store()
        .transition_agent(
            completed_binding.worker.agent_id,
            AgentStatus::Running,
            Some(completed_run),
        )
        .unwrap();
    let duplicate = runtime
        .report_from_mcp(
            &credential,
            None,
            InboxKind::Failed,
            "conflicting retry".into(),
            Some(replacement),
        )
        .unwrap();
    assert!(!duplicate.accepted);
    assert_eq!(
        duplicate
            .committed
            .as_ref()
            .and_then(|message| message.result.as_ref()),
        Some(&result),
        "a retry must expose only the first committed artifact"
    );
    assert_eq!(
        runtime
            .dispatch_store()
            .run(completed_run)
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Completed,
        "the committed outcome repairs a partially persisted run"
    );
    assert_eq!(
        runtime
            .dispatch_store()
            .agent(completed_binding.worker.agent_id)
            .unwrap()
            .unwrap()
            .status,
        AgentStatus::Idle,
        "the retry payload cannot reverse the committed outcome"
    );

    let successor_operation = OperationId::new();
    assert_eq!(
        runtime
            .dispatch(
                &successor_operation.to_string(),
                &dispatch,
                session,
                &FakeScope(Ok(configured_scope(worktree.path()))),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable,
        "public dispatch cannot replace a still-live Agent"
    );
    // Retain the late-completion regression for overlaps persisted by old
    // daemons, which allowed a successor before the predecessor PTY exited.
    let worker = runtime
        .dispatch
        .agent(completed_binding.worker.agent_id)
        .unwrap()
        .unwrap();
    let successor = runtime
        .admit_dispatch(
            successor_operation,
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(worker.runtime.clone()),
            },
            &dispatch.prompt,
            &worker,
            &caller,
            &usagi_core::infrastructure::ipc::agent_dispatch_semantic_key(
                &dispatch.session_name,
                worker.agent_id,
                &dispatch.prompt,
            ),
            &FakeScope(Ok(configured_scope(worktree.path()))),
        )
        .unwrap();
    runtime.remember_operation(
        &successor_operation.to_string(),
        None,
        Ok(successor.clone()),
    );
    let successor_binding = runtime
        .dispatch_store()
        .binding(successor_operation)
        .unwrap()
        .unwrap();
    assert_eq!(
        successor_binding.worker.agent_id, completed_binding.worker.agent_id,
        "the runtime/model selector reuses the same stable Agent"
    );
    let late_duplicate = runtime
        .report_from_mcp(
            &credential,
            None,
            InboxKind::Completed,
            "late duplicate".into(),
            None,
        )
        .unwrap();
    assert!(!late_duplicate.accepted);
    let preserved = runtime
        .dispatch_store()
        .agent(successor_binding.worker.agent_id)
        .unwrap()
        .unwrap();
    assert_eq!(preserved.status, AgentStatus::Running);
    assert_eq!(preserved.current_run, Some(successor_operation));

    runtime.exit(&admission.terminal, 0).unwrap();
    let inbox = runtime.dispatch_store().inbox(&caller).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, InboxKind::Completed);
    assert_eq!(inbox[0].result, Some(result));
    runtime.exit(&successor.terminal, 0).unwrap();

    let failed_operation = OperationId::new();
    let failed = runtime
        .dispatch(
            &failed_operation.to_string(),
            &dispatch,
            session,
            &FakeScope(Ok(configured_scope(worktree.path()))),
        )
        .unwrap();
    let failed_credential = runtime
        .mcp_callers
        .iter()
        .find(|(_, provenance)| provenance.operation == failed_operation)
        .map(|(credential, _)| credential.clone())
        .unwrap();
    runtime
        .report_from_mcp(
            &failed_credential,
            None,
            InboxKind::Failed,
            "failed".into(),
            None,
        )
        .unwrap();
    let binding = runtime
        .dispatch_store()
        .binding(failed_operation)
        .unwrap()
        .unwrap();
    runtime
        .dispatch_store()
        .transition_run(failed_operation, RunStatus::Running, None)
        .unwrap();
    runtime
        .dispatch_store()
        .transition_agent(
            binding.worker.agent_id,
            AgentStatus::Running,
            Some(failed_operation),
        )
        .unwrap();
    let duplicate = runtime
        .report_from_mcp(
            &failed_credential,
            None,
            InboxKind::Completed,
            "conflicting retry".into(),
            None,
        )
        .unwrap();
    assert!(!duplicate.accepted);
    assert_eq!(duplicate.committed.unwrap().kind, InboxKind::Failed);
    assert_eq!(
        runtime
            .dispatch_store()
            .run(failed_operation)
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Failed
    );
    assert_eq!(
        runtime
            .dispatch_store()
            .agent(binding.worker.agent_id)
            .unwrap()
            .unwrap()
            .status,
        AgentStatus::Failed
    );
    runtime.exit(&failed.terminal, 1).unwrap();
}

#[test]
fn dispatch_revalidates_current_allowlist_and_fixture_executable_before_spawn() {
    let fixture = tempfile::tempdir().unwrap();
    let executable = fixture.path().join("claude");
    std::fs::write(&executable, "fixture").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let session_worktree = tempfile::tempdir().unwrap();
    let root_scope = configured_scope(workspace.path());
    let session_scope = configured_scope(session_worktree.path());
    std::fs::write(
        session_worktree.path().join(".usagi/config.toml"),
        "[agents.claude]\nmodels = [\"session-only\"]\n",
    )
    .unwrap();
    let scope = RootAndSessionScope {
        root: root_scope,
        session: session_scope,
    };
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let session = SessionId::new();
    let dispatch = |model: &str| DispatchIntent {
        workspace: WorkspaceId::new(),
        session_name: "worker".into(),
        caller: CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: usagi_core::domain::id::AgentId::new(),
        },
        agent: DispatchAgentIntent::New {
            runtime: AgentProfileId::new("claude").unwrap(),
            model: usagi_core::domain::agent::ModelSelector::new(model).unwrap(),
        },
        prompt: "finish".into(),
    };
    let accepted = runtime
        .dispatch(
            &OperationId::new().to_string(),
            &dispatch("test"),
            session,
            &scope,
        )
        .unwrap();
    assert_eq!(accepted.terminal.session_id, Some(session));
    assert_eq!(runtime.coordinator.occupied_slots(), 1);

    std::fs::remove_file(&executable).unwrap();
    let unavailable = runtime
        .dispatch(
            &OperationId::new().to_string(),
            &dispatch("test"),
            session,
            &scope,
        )
        .unwrap_err();
    assert_eq!(unavailable.code, ErrorCode::Unavailable);
    assert_eq!(runtime.coordinator.occupied_slots(), 1);

    std::fs::write(&executable, "fixture").unwrap();
    std::fs::write(
        workspace.path().join(".usagi/config.toml"),
        "[agents.claude]\nmodels = [\"other\"]\n",
    )
    .unwrap();
    let rejected = runtime
        .dispatch(
            &OperationId::new().to_string(),
            &dispatch("test"),
            session,
            &scope,
        )
        .unwrap_err();
    assert_eq!(rejected.code, ErrorCode::InvalidArgument);
    assert_eq!(runtime.coordinator.occupied_slots(), 1);
}

/// A delegation has to build a worktree before it can dispatch into it, so
/// every refusal that needs no side effect belongs before the create. The
/// preflight raises the same refusals `dispatch` does, without touching the
/// dispatch store or the coordinator (#611).
#[test]
fn the_dispatch_preflight_refuses_before_anything_is_created() {
    let fixture = tempfile::tempdir().unwrap();
    let executable = fixture.path().join("claude");
    std::fs::write(&executable, "fixture").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let scope = configured_scope(workspace.path());
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let claude = AgentProfileId::new("claude").unwrap();
    let allowed = ModelSelector::new("test").unwrap();
    let operation = OperationId::new().to_string();
    let preflight = |runtime: &AgentRuntime, operation: &str, prompt: &str, model: &str| {
        runtime.preflight_dispatch(
            operation,
            prompt,
            &claude,
            &ModelSelector::new(model).unwrap(),
            workspace.path(),
        )
    };

    preflight(&runtime, &operation, "finish", "test").unwrap();
    assert_eq!(
        preflight(&runtime, "not-canonical", "finish", "test")
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        preflight(&runtime, &operation, "", "test")
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    // An unknown model is refused with the same message `dispatch` uses.
    let rejected = preflight(&runtime, &operation, "finish", "other").unwrap_err();
    assert_eq!(rejected.code, ErrorCode::InvalidArgument);
    assert!(rejected.message.contains("not allowed"));
    // Nothing above reserved anything: no agent, no run, no occupied slot.
    assert!(runtime.dispatch_store().agents().unwrap().is_empty());
    assert!(runtime.dispatch_store().runs().unwrap().is_empty());
    assert_eq!(runtime.coordinator.occupied_slots(), 0);

    std::fs::remove_file(&executable).unwrap();
    assert_eq!(
        preflight(&runtime, &operation, "finish", "test")
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    std::fs::write(&executable, "fixture").unwrap();

    // An operation that already owns a durable admission must not create a
    // second session for it: the spawn's outcome is not this daemon's to
    // decide again.
    let session = SessionId::new();
    let admitted = OperationId::new();
    runtime
        .dispatch(
            &admitted.to_string(),
            &DispatchIntent {
                workspace: WorkspaceId::new(),
                session_name: "worker".into(),
                caller: CallerRef {
                    session_id: Some(SessionId::new()),
                    agent_id: usagi_core::domain::id::AgentId::new(),
                },
                agent: DispatchAgentIntent::New {
                    runtime: claude.clone(),
                    model: allowed.clone(),
                },
                prompt: "finish".into(),
            },
            session,
            &FakeScope(Ok(scope)),
        )
        .unwrap();
    // This daemon already answered that operation, so a retry replays through
    // `dispatch` and the preflight deliberately admits it.
    preflight(&runtime, &admitted.to_string(), "finish", "test").unwrap();
    // A restart loses the in-memory outcome, and only the durable run is
    // left: that is the reservation the preflight must refuse to redo.
    runtime.operations.clear();
    let incomplete = preflight(&runtime, &admitted.to_string(), "finish", "test").unwrap_err();
    assert_eq!(incomplete.code, ErrorCode::OwnershipUnknown);
    assert!(incomplete.message.contains("cannot be spawned again"));
}

#[test]
fn sakana_dispatch_preflight_checks_the_executable_it_launches() {
    let fixture = tempfile::tempdir().unwrap();
    // Fugu runs the Claude CLI, so that is the executable whose absence
    // makes this runtime unavailable.
    let executable = fixture.path().join("claude");
    std::fs::write(&executable, "fixture").unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join(".usagi")).unwrap();
    std::fs::write(
        workspace.path().join(".usagi/config.toml"),
        "[agents.sakana-ai]\nmodels = [\"fixture\"]\n",
    )
    .unwrap();
    let runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let operation = OperationId::new().to_string();
    let sakana = AgentProfileId::new("sakana-ai").unwrap();
    let model = ModelSelector::new("fixture").unwrap();

    runtime
        .preflight_dispatch(
            &operation,
            "inspect argv",
            &sakana,
            &model,
            workspace.path(),
        )
        .unwrap();
    std::fs::remove_file(executable).unwrap();
    assert_eq!(
        runtime
            .preflight_dispatch(
                &operation,
                "inspect argv",
                &sakana,
                &model,
                workspace.path()
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(runtime_executable("unknown-runtime"), "unknown-runtime");
}

#[test]
fn dispatch_rejects_invalid_unknown_and_foreign_requests_before_spawn() {
    let mut runtime = runtime();
    let session = SessionId::new();
    let caller = CallerRef {
        session_id: Some(SessionId::new()),
        agent_id: usagi_core::domain::id::AgentId::new(),
    };
    let unknown = DispatchIntent {
        workspace: WorkspaceId::new(),
        session_name: "worker".into(),
        caller: caller.clone(),
        agent: DispatchAgentIntent::Existing {
            agent_id: usagi_core::domain::id::AgentId::new(),
        },
        prompt: "work".into(),
    };
    assert_eq!(
        runtime
            .dispatch("invalid", &unknown, session, &FakeScope(Ok(scope())))
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let mut empty = unknown.clone();
    empty.prompt.clear();
    assert_eq!(
        runtime
            .dispatch(
                &OperationId::new().to_string(),
                &empty,
                session,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .dispatch(
                &OperationId::new().to_string(),
                &unknown,
                session,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );

    let foreign_session = SessionId::new();
    let foreign = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            unknown.workspace,
            Some(foreign_session),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("test").unwrap(),
        )
        .unwrap();
    let foreign_intent = DispatchIntent {
        agent: DispatchAgentIntent::Existing {
            agent_id: foreign.agent_id,
        },
        ..unknown
    };
    assert_eq!(
        runtime
            .dispatch(
                &OperationId::new().to_string(),
                &foreign_intent,
                session,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert!(runtime.coordinator.snapshot().records.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)] // The durable admission states intentionally share one replay setup.
fn dispatch_replays_prepared_conflicting_and_legacy_admissions_without_respawn() {
    let temp = tempfile::tempdir().unwrap();
    let dispatch_dir = temp.path().join("dispatch");
    let session = SessionId::new();
    let workspace = WorkspaceId::new();
    let durable = DispatchStore::new(&dispatch_dir);
    let worker = durable
        .upsert_agent_by_runtime_model(
            workspace,
            Some(session),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("test").unwrap(),
        )
        .unwrap();
    let intent = DispatchIntent {
        workspace,
        session_name: "worker".into(),
        caller: CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: usagi_core::domain::id::AgentId::new(),
        },
        agent: DispatchAgentIntent::Existing {
            agent_id: worker.agent_id,
        },
        prompt: "work".into(),
    };
    let operation = OperationId::new();
    let make_runtime = |store| {
        AgentRuntime::with_dispatch(
            DaemonGeneration::new(),
            claude_registry(),
            store,
            Journal::default(),
            Pty::default(),
            AgentProfileId::new("claude").unwrap(),
            Geometry { cols: 80, rows: 24 },
            DispatchStore::new(&dispatch_dir),
        )
    };
    let mut first = make_runtime(Store {
        saves: 0,
        fail_after: Some(0),
        ..Store::default()
    });
    assert_eq!(
        first
            .dispatch(
                &operation.to_string(),
                &intent,
                session,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    for (candidate, code) in [
        (intent.clone(), ErrorCode::OwnershipUnknown),
        (
            DispatchIntent {
                prompt: "different".into(),
                ..intent.clone()
            },
            ErrorCode::IdempotencyConflict,
        ),
    ] {
        assert_eq!(
            make_runtime(Store::default())
                .dispatch(
                    &operation.to_string(),
                    &candidate,
                    session,
                    &FakeScope(Ok(scope())),
                )
                .unwrap_err()
                .code,
            code
        );
    }

    let legacy_dir = temp.path().join("legacy");
    let legacy = DispatchStore::new(&legacy_dir);
    let worker = legacy
        .upsert_agent_by_runtime_model(
            workspace,
            Some(session),
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("test").unwrap(),
        )
        .unwrap();
    let legacy_operation = OperationId::new();
    legacy
        .upsert_run(DispatchRun {
            run_id: legacy_operation,
            agent_id: worker.agent_id,
            prompt: "legacy".into(),
            started_at: Utc::now(),
            ended_at: None,
            status: RunStatus::Preparing,
        })
        .unwrap();
    let mut runtime = AgentRuntime::with_dispatch(
        DaemonGeneration::new(),
        claude_registry(),
        Store::default(),
        Journal::default(),
        Pty::default(),
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        legacy,
    );
    assert_eq!(
        runtime
            .dispatch(
                &legacy_operation.to_string(),
                &DispatchIntent {
                    agent: DispatchAgentIntent::Existing {
                        agent_id: worker.agent_id,
                    },
                    ..intent
                },
                session,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
fn clean_never_selects_a_live_runtime_even_if_its_dispatch_run_is_failed() {
    let operation = OperationId::new();
    let mut runtime = runtime();
    let admission = runtime
        .launch(
            &operation.to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    runtime.dispatch.fail_admission(operation).unwrap();

    assert!(runtime.failed_reservation_ids().unwrap().is_empty());
    assert_eq!(runtime.clean_failed_reservations().unwrap(), 0);
    assert_eq!(
        runtime
            .coordinator
            .record_for(
                &runtime
                    .coordinator
                    .runtime_for_terminal(&admission.terminal)
                    .unwrap()
            )
            .unwrap()
            .state,
        crate::usecase::runtime::RuntimeState::Running
    );
}

#[test]
fn clean_repairs_a_failed_dispatch_reservation_and_hides_ghost_ready() {
    let operation = OperationId::new();
    let mut runtime = runtime();
    let admission = runtime
        .launch(
            &operation.to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let session = admission.terminal.session_id.unwrap();
    runtime.dispatch.fail_admission(operation).unwrap();
    let mut snapshot = runtime.coordinator.snapshot();
    snapshot.records[0].state = crate::usecase::runtime::RuntimeState::ReconcileRequired(
        crate::usecase::runtime::ReconcileState::IdentityUnknown,
    );
    snapshot.records[0].process = None;
    snapshot.generation.terminals[0].process = None;
    snapshot.generation.terminals[0].state =
        crate::usecase::generation::TerminalState::IdentityUnknown;
    runtime.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 1024, 2).unwrap();

    assert_eq!(runtime.failed_reservation_ids().unwrap().len(), 1);
    assert_eq!(runtime.session_phase(session), AgentPhase::Exited);
    assert_eq!(
        runtime.inventory(admission.terminal.workspace_id).runtimes[0].state,
        AgentRuntimeInventoryState::Unavailable
    );
    assert_eq!(runtime.clean_failed_reservations().unwrap(), 1);
    assert!(runtime.failed_reservation_ids().unwrap().is_empty());
    assert_eq!(
        runtime.coordinator.snapshot().records[0].state,
        crate::usecase::runtime::RuntimeState::SpawnFailed
    );
    assert_eq!(runtime.close_session(session).unwrap(), 1);
    assert!(runtime.coordinator.snapshot().records.is_empty());
}

#[test]
fn agent_dispatch_refuses_non_terminal_typed_requests() {
    let mut runtime = runtime();
    let admission = runtime
        .launch(
            &OperationId::new().to_string(),
            &intent(None),
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let runtime_ref = runtime
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap();
    let error = runtime
        .dispatch_terminal(
            TerminalRequestContext {
                connection: ConnectionId::new(),
                client: ClientId::new(),
                request: RequestId::new(),
            },
            TerminalRequest::Inventory {
                scope: usagi_core::domain::terminal_launch::TerminalLaunchScope {
                    workspace_id: WorkspaceId::new(),
                    session_id: None,
                    worktree_id: WorktreeId::new(),
                },
            },
            &runtime_ref,
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgument);
}
