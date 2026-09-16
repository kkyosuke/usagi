//! resume の振る舞いを固定するテスト。

use super::*;

#[test]
fn saturated_capacity_selection_compares_every_completed_resume_candidate() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let generation = DaemonGeneration::new();
    let mut agent = AgentRuntime::new(
        generation,
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
    let mut three_slots = RuntimeCoordinator::new(3, 64 * 1024, 64);
    three_slots.activate_generation(generation).unwrap();
    agent.coordinator = three_slots;

    for _ in 0..3 {
        let admission = agent
            .launch(
                &OperationId::new().to_string(),
                &AgentLaunchIntent {
                    workspace,
                    session: Some(session),
                    profile: None,
                },
                &FakeScope(Ok(scope())),
            )
            .unwrap();
        let runtime = agent
            .coordinator
            .runtime_for_terminal(&admission.terminal)
            .unwrap();
        agent
            .reported_phases
            .insert(runtime.agent_runtime_id, AgentPhase::Ended);
    }

    let mut operations = [OperationId::new(), OperationId::new(), OperationId::new()];
    operations.sort();
    let mut snapshot = agent.coordinator.snapshot();
    // Runtime records are keyed independently of operation age. Arrange
    // their ages so iteration first replaces the candidate, then retains it.
    snapshot.records[0].operation.operation_id = operations[2];
    snapshot.records[1].operation.operation_id = operations[0];
    snapshot.records[2].operation.operation_id = operations[1];
    let oldest = snapshot.records[1].runtime.clone();
    agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 3, 64 * 1024, 64).unwrap();

    assert_eq!(agent.sleep_one_for_capacity(), Ok(true));
    let records = agent.coordinator.snapshot().records;
    assert_eq!(agent.concurrency().in_use, 2);
    assert!(records.iter().any(|record| {
        record.runtime == oldest && record.state == crate::usecase::runtime::RuntimeState::Sleeping
    }));
    assert_eq!(
        records
            .iter()
            .filter(|record| record.state == crate::usecase::runtime::RuntimeState::Running)
            .count(),
        2
    );
}

#[test]
fn saturated_launch_refuses_when_no_completed_resume_source_is_safe() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let generation = DaemonGeneration::new();
    let mut agent = AgentRuntime::new(
        generation,
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
    let mut one_slot = RuntimeCoordinator::new(1, 64 * 1024, 64);
    one_slot.activate_generation(generation).unwrap();
    agent.coordinator = one_slot;
    let first = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: None,
            },
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();

    assert_eq!(
        agent
            .launch(
                &OperationId::new().to_string(),
                &AgentLaunchIntent {
                    workspace,
                    session: Some(session),
                    profile: None,
                },
                &FakeScope(Ok(resolved)),
            )
            .unwrap_err()
            .code,
        ErrorCode::ResourceExhausted
    );
    assert_eq!(agent.concurrency().in_use, 1);
    assert_eq!(
        agent
            .coordinator
            .runtime_for_terminal(&first.terminal)
            .unwrap()
            .terminal,
        first.terminal
    );
}

#[test]
fn manual_sleep_requires_an_idle_exact_resume_source_and_retains_the_session() {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
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
    assert_eq!(
        agent.sleep_session(session).unwrap_err().code,
        ErrorCode::Unavailable
    );

    let admission = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: None,
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    let runtime = agent
        .coordinator
        .runtime_for_terminal(&admission.terminal)
        .unwrap();
    assert_eq!(
        agent.sleep_session(session).unwrap_err().code,
        ErrorCode::Busy
    );

    agent
        .reported_phases
        .insert(runtime.agent_runtime_id, AgentPhase::Ready);
    let mut snapshot = agent.coordinator.snapshot();
    let resume = snapshot.records[0].provider_resume.take().unwrap();
    agent.coordinator = RuntimeCoordinator::hydrate(snapshot, 16, 64 * 1024, 64).unwrap();
    assert_eq!(
        agent.sleep_session(session).unwrap_err().code,
        ErrorCode::Busy
    );
    agent
        .coordinator
        .write_provider_resume(
            &runtime,
            resume,
            ProviderResumeWrite::Attach,
            &mut *agent.store,
        )
        .unwrap();

    assert_eq!(agent.sleep_session(session), Ok(1));
    assert_eq!(agent.session_phase(session), AgentPhase::Sleeping);
    assert_eq!(agent.concurrency().in_use, 0);
    assert!(agent.mcp_callers.is_empty());
    assert!(
        !agent
            .reported_phases
            .contains_key(&runtime.agent_runtime_id)
    );
    assert_eq!(
        agent.sleep_session(session).unwrap_err().code,
        ErrorCode::Unavailable
    );
}

#[test]
fn readiness_ticket_is_revalidated_before_launch_effects() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let intent = intent(Some("claude"));
    let operation = OperationId::new().to_string();
    let ticket = runtime
        .prepare_launch_readiness(&operation, &intent)
        .unwrap()
        .unwrap();

    let stale = AgentReadinessPreflight {
        profile: ticket.profile.clone(),
        profile_revision: ticket.profile_revision + 1,
        generation: ticket.generation,
    };
    let error = runtime
        .launch_after_readiness(
            &operation,
            &intent,
            &FakeScope(Ok(ResolvedAgentScope {
                worktree_id: WorktreeId::new(),
                working_directory: PathBuf::from("/worktree"),
            })),
            Some(&stale),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::RevisionConflict);
    assert!(runtime.coordinator.snapshot().records.is_empty());

    std::fs::remove_file(fixture.path().join("claude")).unwrap();
    let error = runtime
        .launch_after_readiness(
            &operation,
            &intent,
            &FakeScope(Ok(ResolvedAgentScope {
                worktree_id: WorktreeId::new(),
                working_directory: PathBuf::from("/worktree"),
            })),
            Some(&ticket),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(runtime.coordinator.snapshot().records.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)] // One table-like sweep covers every secret-free preflight refusal/replay branch.
fn readiness_preparation_covers_replay_conflict_and_safe_refusals() {
    let mut runtime = runtime();
    let launch = intent(None);
    assert_eq!(
        runtime
            .prepare_launch_readiness(&OperationId::new().to_string(), &launch)
            .unwrap()
            .unwrap()
            .product(),
        "claude"
    );
    assert_eq!(
        runtime
            .prepare_launch_readiness("invalid", &launch)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let unknown = intent(Some("unknown"));
    assert_eq!(
        runtime
            .prepare_launch_readiness(&OperationId::new().to_string(), &unknown)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let launch_operation = OperationId::new().to_string();
    runtime.operations.insert(
        launch_operation.clone(),
        AgentOperation::new(
            Some(&semantic_key(&launch)),
            Err(ProtocolError::new(ErrorCode::Unavailable, "fixture")),
            Utc::now(),
        ),
    );
    assert!(
        runtime
            .prepare_launch_readiness(&launch_operation, &launch)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        runtime
            .prepare_launch_readiness(&launch_operation, &intent(Some("claude")))
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );

    let stale_target = AgentResumeTarget {
        continuation: AgentContinuationRef::new(),
        source: AgentResumeSourceId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
        runtime_id: AgentRuntimeId::new(),
        adapter_revision: 1,
    };
    assert_eq!(
        runtime
            .prepare_resume_readiness("invalid", &stale_target)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .prepare_resume_readiness(&OperationId::new().to_string(), &stale_target)
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );
    let resume_operation = OperationId::new().to_string();
    runtime.operations.insert(
        resume_operation.clone(),
        AgentOperation::new(
            Some(&resume_semantic_key(&stale_target)),
            Err(ProtocolError::new(ErrorCode::Unavailable, "fixture")),
            Utc::now(),
        ),
    );
    assert!(
        runtime
            .prepare_resume_readiness(&resume_operation, &stale_target)
            .unwrap()
            .is_none()
    );
    let mut conflicting = stale_target;
    conflicting.source = AgentResumeSourceId::new();
    assert_eq!(
        runtime
            .prepare_resume_readiness(&resume_operation, &conflicting)
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );

    let dispatch_operation = OperationId::new().to_string();
    let dispatch = DispatchIntent {
        workspace: WorkspaceId::new(),
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
    assert_eq!(
        runtime
            .prepare_dispatch_readiness("invalid", &dispatch)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert!(
        runtime
            .prepare_dispatch_readiness(&dispatch_operation, &dispatch)
            .unwrap()
            .is_some()
    );
    let existing = runtime
        .dispatch
        .upsert_agent_by_runtime_model(
            dispatch.workspace,
            None,
            AgentProfileId::new("claude").unwrap(),
            ModelSelector::new("test").unwrap(),
        )
        .unwrap();
    let existing_dispatch = DispatchIntent {
        agent: DispatchAgentIntent::Existing {
            agent_id: existing.agent_id,
        },
        ..dispatch.clone()
    };
    assert!(
        runtime
            .prepare_dispatch_readiness(&OperationId::new().to_string(), &existing_dispatch,)
            .unwrap()
            .is_some()
    );
    runtime.operations.insert(
        dispatch_operation.clone(),
        AgentOperation::new(
            None,
            Err(ProtocolError::new(ErrorCode::Unavailable, "fixture")),
            Utc::now(),
        ),
    );
    assert!(
        runtime
            .prepare_dispatch_readiness(&dispatch_operation, &dispatch)
            .unwrap()
            .is_none()
    );
    let missing = DispatchIntent {
        agent: DispatchAgentIntent::Existing {
            agent_id: AgentId::new(),
        },
        ..dispatch
    };
    assert_eq!(
        runtime
            .prepare_dispatch_readiness(&OperationId::new().to_string(), &missing)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One end-to-end test keeps capture, exit, resume, replay, and live rejection visibly ordered.
fn structured_codex_identity_enables_one_explicit_new_runtime_resume() {
    let mut runtime = codex_runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let launch_intent = AgentLaunchIntent {
        workspace,
        session: Some(session),
        profile: Some(AgentProfileId::new("codex").unwrap()),
    };
    let initial_operation = OperationId::new();
    let first = runtime
        .launch(
            &initial_operation.to_string(),
            &launch_intent,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    assert_eq!(
        runtime.session_resume_status(session),
        (false, ProviderResumeReason::LiveOrOwnershipUnknown)
    );
    let first_runtime = runtime
        .coordinator
        .runtime_for_terminal(&first.terminal)
        .unwrap();
    let credential = runtime.mcp_callers.keys().next().cloned().unwrap();
    let native_id = ProviderSessionId::new("structured-codex-session").unwrap();
    assert_eq!(
        runtime
            .capture_codex_session(
                "unknown-credential",
                ProviderSessionId::new("ignored-session").unwrap(),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        runtime
            .capture_structured_provider_session(
                &first_runtime,
                ProviderKind::Claude,
                native_id.clone(),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    runtime
        .capture_codex_session(&credential, native_id.clone())
        .unwrap();
    assert_eq!(
        runtime
            .capture_codex_session(
                &credential,
                ProviderSessionId::new("different-session").unwrap(),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    let captured = runtime.coordinator.snapshot();
    assert_eq!(
        captured.records[0]
            .provider_resume
            .as_ref()
            .unwrap()
            .provenance,
        ProviderCaptureProvenance::ProviderStructured
    );
    assert!(
        !serde_json::to_string(&captured.records[0].launch)
            .unwrap()
            .contains(native_id.expose_sensitive())
    );

    runtime.exit(&first.terminal, 0).unwrap();
    let target = runtime
        .inventory(workspace)
        .resumable
        .into_iter()
        .find_map(|item| item.target)
        .unwrap();
    let mut wrong_capture_policy = runtime.coordinator.snapshot().records[0].clone();
    wrong_capture_policy
        .provider_resume
        .as_mut()
        .unwrap()
        .provenance = ProviderCaptureProvenance::DaemonIssued;
    assert_eq!(
        runtime.resume_source_availability(
            &wrong_capture_policy,
            std::slice::from_ref(&wrong_capture_policy),
        ),
        (false, ProviderResumeReason::IncompatibleProviderMetadata)
    );
    assert_eq!(
        runtime
            .capture_codex_session(&credential, ProviderSessionId::new("late-session").unwrap(),)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        runtime
            .resume_exact(
                &initial_operation.to_string(),
                &target,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    assert_eq!(
        runtime
            .resume_exact(
                "not-an-operation-id",
                &target,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .admit_resume_exact(
                &initial_operation.to_string(),
                &target,
                &resume_semantic_key(&target),
                &FakeScope(Ok(resolved.clone())),
                None,
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(
        runtime.session_resume_status(session),
        (true, ProviderResumeReason::ExplicitResumeAvailable)
    );
    let mut ambiguous_snapshot = runtime.coordinator.snapshot();
    let mut ambiguous_record = ambiguous_snapshot.records[0].clone();
    let mut ambiguous_ownership = ambiguous_snapshot
        .generation
        .terminals
        .iter()
        .find(|ownership| {
            ownership
                .terminal
                .fences(&ambiguous_record.runtime.terminal)
        })
        .unwrap()
        .clone();
    ambiguous_record.runtime.agent_runtime_id = AgentRuntimeId::new();
    ambiguous_record.continuation = Some(AgentContinuationRef::new());
    ambiguous_record.resume_source = Some(usagi_core::domain::id::AgentResumeSourceId::new());
    let ambiguous_terminal_id = TerminalId::new();
    ambiguous_record.runtime.terminal.terminal_id = ambiguous_terminal_id;
    ambiguous_ownership.terminal.terminal_id = ambiguous_terminal_id;
    ambiguous_record.operation.operation_id = OperationId::new();
    ambiguous_record.semantic_key = Some("ambiguous-resume-source".into());
    ambiguous_record
        .provider_resume
        .as_mut()
        .unwrap()
        .native_session_id = ProviderSessionId::new("other-codex-session").unwrap();
    ambiguous_snapshot.records.push(ambiguous_record);
    ambiguous_snapshot
        .generation
        .terminals
        .push(ambiguous_ownership);
    let original_coordinator = std::mem::replace(
        &mut runtime.coordinator,
        RuntimeCoordinator::hydrate(ambiguous_snapshot, 16, 64 * 1024, 64).unwrap(),
    );
    assert_eq!(
        runtime.session_resume_status(session),
        (false, ProviderResumeReason::AmbiguousProviderMetadata)
    );
    runtime.coordinator = original_coordinator;

    let original_registry = std::mem::replace(&mut runtime.registry, AdapterRegistry::new());
    assert_eq!(
        runtime.session_resume_status(session),
        (false, ProviderResumeReason::IncompatibleProviderMetadata)
    );
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    runtime.registry = original_registry;

    let inner = CodexAdapter::new(FakeAgentCodexProvisioner);
    let mut profile = inner.profile().clone();
    profile.capabilities.remove(&AgentCapability::Resume);
    let mut incompatible_registry = AdapterRegistry::new();
    incompatible_registry
        .register(
            profile.clone(),
            Box::new(ProfileOverrideAdapter { profile, inner }),
        )
        .unwrap();
    let original_registry = std::mem::replace(&mut runtime.registry, incompatible_registry);
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    runtime.registry = original_registry;

    assert_eq!(
        runtime
            .resume_exact(
                "not-an-operation-id",
                &target,
                &FakeScope(Ok(resolved.clone()))
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );
    let operation = OperationId::new().to_string();
    let resumed = runtime
        .resume_exact(&operation, &target, &FakeScope(Ok(resolved.clone())))
        .unwrap();
    assert_ne!(resumed.terminal, first.terminal);
    assert_eq!(resumed.continuation, Some(target.continuation));
    assert_eq!(
        resumed.resume_relation.as_ref().unwrap().source,
        target.source
    );
    assert_eq!(
        runtime
            .resume_exact(&operation, &target, &FakeScope(Ok(resolved.clone())))
            .unwrap()
            .terminal,
        resumed.terminal
    );
    let mut conflicting = target.clone();
    conflicting.runtime_id = AgentRuntimeId::new();
    assert_eq!(
        runtime
            .resume_exact(&operation, &conflicting, &FakeScope(Ok(resolved.clone())))
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    let double_click = runtime
        .resume_exact(
            &OperationId::new().to_string(),
            &target,
            &FakeScope(Ok(resolved.clone())),
        )
        .unwrap();
    assert_eq!(double_click.terminal, resumed.terminal);
    assert_eq!(double_click.continuation, resumed.continuation);
    assert_eq!(double_click.resume_relation, resumed.resume_relation);
    // The workspace-wide question a retirement asks: this workspace has a
    // running Agent, another workspace does not.
    assert!(runtime.has_running_agent(workspace));
    assert!(!runtime.has_running_agent(WorkspaceId::new()));
    let inventory = runtime.inventory(workspace);
    assert_eq!(inventory.runtimes.len(), 2);
    assert!(
        inventory
            .runtimes
            .iter()
            .all(|item| item.continuation == target.continuation)
    );
    assert_eq!(
        inventory.resumable[0].reason,
        ProviderResumeReason::SourceAlreadySuperseded
    );
    assert_eq!(runtime.coordinator.snapshot().records.len(), 2);

    let mut live_replacement = runtime.coordinator.snapshot();
    for record in &mut live_replacement.records {
        if record.runtime.agent_runtime_id == target.runtime_id {
            record.superseded_by = None;
        }
        if record.resumed_from == Some(target.source) {
            record.resumed_from = None;
        }
    }
    runtime.coordinator = RuntimeCoordinator::hydrate(live_replacement, 16, 64 * 1024, 64).unwrap();
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(resolved)),
            )
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
}

#[test]
fn codex_without_structured_identity_fails_closed_for_resume() {
    let mut runtime = codex_runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let first = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("codex").unwrap()),
            },
            &FakeScope(Ok(resolved)),
        )
        .unwrap();
    runtime.exit(&first.terminal, 0).unwrap();
    assert_eq!(
        runtime.session_resume_status(session),
        (false, ProviderResumeReason::ProviderMetadataUnavailable)
    );
    let item = &runtime.inventory(workspace).resumable[0];
    assert!(item.target.is_some());
    assert!(!item.available);
    assert_eq!(
        item.reason,
        ProviderResumeReason::ProviderMetadataUnavailable
    );
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                item.target.as_ref().unwrap(),
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
}

#[test]
fn exact_resume_rejects_every_public_fence_before_spawn() {
    let mut runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let launched = runtime
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
    let live_runtime = runtime
        .coordinator
        .runtime_for_terminal(&launched.terminal)
        .unwrap();
    let live_target =
        resume_target(runtime.coordinator.record_for(&live_runtime).unwrap()).unwrap();
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &live_target,
                &FakeScope(Ok(resolved.clone())),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    runtime.exit(&launched.terminal, 0).unwrap();
    let target = runtime.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();

    let mut stale_targets = Vec::new();
    let mut stale = target.clone();
    stale.continuation = AgentContinuationRef::new();
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.source = usagi_core::domain::id::AgentResumeSourceId::new();
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.workspace_id = WorkspaceId::new();
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.session_id = None;
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.worktree_id = WorktreeId::new();
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.runtime_id = AgentRuntimeId::new();
    stale_targets.push(stale);
    let mut stale = target.clone();
    stale.adapter_revision += 1;
    stale_targets.push(stale);
    for stale in stale_targets {
        assert_eq!(
            runtime
                .resume_exact(
                    &OperationId::new().to_string(),
                    &stale,
                    &FakeScope(Ok(resolved.clone())),
                )
                .unwrap_err()
                .code,
            ErrorCode::StaleTarget
        );
    }
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Err(ScopeResolveError::Unavailable)),
            )
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(scope())),
            )
            .unwrap_err()
            .code,
        ErrorCode::StaleTarget
    );
    assert_eq!(runtime.coordinator.snapshot().records.len(), 1);
}

#[test]
fn exact_resume_spawn_failure_removes_only_its_ephemeral_credential() {
    let mut runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = scope();
    let launched = runtime
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
    runtime.exit(&launched.terminal, 0).unwrap();
    let target = runtime.inventory(workspace).resumable[0]
        .target
        .clone()
        .unwrap();
    let callers_before = runtime.mcp_callers.len();
    pty_mut(&mut runtime).spawn = Some(SpawnFailure::Definite);
    assert_eq!(
        runtime
            .resume_exact(
                &OperationId::new().to_string(),
                &target,
                &FakeScope(Ok(resolved)),
            )
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(runtime.mcp_callers.len(), callers_before);
}

#[test]
fn schema_v3_runtime_without_public_lineage_loads_as_resume_unavailable() {
    let mut runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let launched = runtime
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: Some(AgentProfileId::new("claude").unwrap()),
            },
            &FakeScope(Ok(scope())),
        )
        .unwrap();
    runtime.exit(&launched.terminal, 0).unwrap();
    let mut legacy = runtime.coordinator.snapshot();
    legacy.schema_version = 3;
    let mut partial_lineage = legacy.records[0].clone();
    partial_lineage.continuation = Some(AgentContinuationRef::new());
    partial_lineage.resume_source = None;
    assert!(resume_target(&partial_lineage).is_none());
    legacy.records[0].continuation = None;
    legacy.records[0].resume_source = None;
    runtime.coordinator = RuntimeCoordinator::hydrate(legacy, 16, 64 * 1024, 64).unwrap();

    let inventory = runtime.inventory(workspace);
    assert!(inventory.runtimes.is_empty());
    assert_eq!(inventory.resumable.len(), 1);
    assert!(inventory.resumable[0].target.is_none());
    assert!(!inventory.resumable[0].available);
    assert_eq!(
        inventory.resumable[0].reason,
        ProviderResumeReason::ProviderMetadataUnavailable
    );
}

#[test]
fn session_workflow_launch_rechecks_readiness_and_embeds_exact_prompt() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let intent = intent(None);
    let operation = OperationId::new().to_string();
    let prompt = "workflow task";
    let scope = FakeScope(Ok(scope()));
    for invalid in ["", "\0", " "] {
        assert!(
            runtime
                .prepare_workflow_readiness(&operation, &intent, invalid)
                .is_err()
        );
    }
    assert!(
        runtime
            .prepare_workflow_readiness("invalid", &intent, prompt)
            .is_err()
    );
    let preflight = runtime
        .prepare_workflow_readiness(&operation, &intent, prompt)
        .unwrap();
    assert!(
        runtime
            .launch_workflow_after_readiness(&operation, &intent, prompt, &scope, None)
            .is_err()
    );
    let first = runtime
        .launch_workflow_after_readiness(&operation, &intent, prompt, &scope, preflight.as_ref())
        .unwrap();
    let replay = runtime
        .launch_workflow_after_readiness(&operation, &intent, prompt, &scope, None)
        .unwrap();
    assert_eq!(first, replay);
    assert!(
        runtime
            .prepare_workflow_readiness(&operation, &intent, "changed")
            .is_err()
    );
    assert_eq!(
        runtime.coordinator.snapshot().records[0]
            .launch
            .request
            .initial_prompt
            .as_deref(),
        Some(prompt)
    );
    let other = OperationId::new().to_string();
    let preflight = runtime
        .prepare_workflow_readiness(&other, &intent, prompt)
        .unwrap();
    assert!(
        runtime
            .launch_workflow_after_readiness(&other, &intent, prompt, &scope, preflight.as_ref())
            .is_err()
    );
}

#[test]
fn goal_readiness_defaults_profile_and_rejects_semantic_conflict() {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("claude"), "fixture").unwrap();
    let mut runtime = runtime_with_fixture(FixtureLocator(fixture.path().to_path_buf()));
    let operation = OperationId::new().to_string();
    let mut intent = AgentGoalIntent {
        workspace: WorkspaceId::new(),
        profile: None,
        goal: "use the default profile".into(),
    };
    assert_eq!(
        runtime.goal_worker_profile(&intent).unwrap().as_str(),
        "claude"
    );
    let mut explicit = intent.clone();
    explicit.profile = Some(AgentProfileId::new("claude").unwrap());
    assert_eq!(
        runtime.goal_worker_profile(&explicit).unwrap().as_str(),
        "claude"
    );
    let mut invalid = intent.clone();
    invalid.goal = " ".into();
    assert_eq!(
        runtime.goal_worker_profile(&invalid).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    let readiness = runtime
        .prepare_goal_launch_readiness(&operation, &intent)
        .unwrap()
        .unwrap();
    assert_eq!(readiness.product(), "claude");
    runtime
        .launch_goal_after_readiness(
            &operation,
            &intent,
            &FakeScope(Ok(scope())),
            Some(&readiness),
        )
        .unwrap();

    intent.goal = "a different goal".into();
    assert_eq!(
        runtime
            .prepare_goal_launch_readiness(&operation, &intent)
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
}

#[test]
fn agent_resume_reports_exit_for_parity_with_the_generic_terminal() {
    // Regression: an Agent's `Resume` must carry the hosting terminal's
    // `exited` flag (like the generic terminal Resume), so a TUI client's
    // per-frame poll observes the exit and drops the pane tab instead of
    // leaving it stranded until an incidental resync.
    let mut runtime = runtime();
    let fake_scope = FakeScope(Ok(scope()));
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);
    let terminal = runtime
        .launch(&operation, &launch_intent, &fake_scope)
        .unwrap()
        .terminal;
    runtime.output(&terminal, b"working\n".to_vec()).unwrap();

    let connection = ConnectionId::new();
    let client = ClientId::new();
    let live = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resume,
        TerminalRequest::Resume {
            terminal: terminal.clone(),
            after_offset: 0,
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(live["exited"], false);

    runtime.exit(&terminal, 0).unwrap();
    assert!(runtime.exit(&terminal, 0).is_err());
    assert_eq!(
        pty(&runtime).released.as_slice(),
        std::slice::from_ref(&terminal)
    );
    let late_resize = runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resize,
        TerminalRequest::Resize {
            terminal: terminal.clone(),
            geometry: TerminalGeometry { cols: 80, rows: 24 },
        },
        SnapshotWire::RawTail,
    );
    assert!(matches!(
        late_resize,
        TerminalOutcome::Handled(Err(ProtocolError {
            code: ErrorCode::StaleTarget,
            ..
        }))
    ));
    let exited = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resume,
        TerminalRequest::Resume {
            terminal: terminal.clone(),
            after_offset: 8,
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(exited["exited"], true);
}

#[test]
#[allow(clippy::too_many_lines)] // Preserve the full launch/resume/exit identity scenario.
fn same_model_launches_and_exact_resume_preserve_each_peer_identity() {
    let mut runtime = runtime();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let resolved = FakeScope(Ok(scope()));
    let launch = AgentLaunchIntent {
        workspace,
        session: Some(session),
        profile: Some(AgentProfileId::new("claude").unwrap()),
    };
    let first_operation = OperationId::new();
    let first = runtime
        .launch(&first_operation.to_string(), &launch, &resolved)
        .unwrap();
    let first_agent = runtime
        .dispatch
        .binding(first_operation)
        .unwrap()
        .unwrap()
        .worker
        .agent_id;
    let second_operation = OperationId::new();
    let second = runtime
        .launch(&second_operation.to_string(), &launch, &resolved)
        .unwrap();
    let second_agent = runtime
        .dispatch
        .binding(second_operation)
        .unwrap()
        .unwrap()
        .worker
        .agent_id;
    assert_ne!(first_agent, second_agent);
    assert_eq!(
        runtime
            .dispatch
            .agent(first_agent)
            .unwrap()
            .unwrap()
            .current_run,
        Some(first_operation)
    );
    runtime.exit(&second.terminal, 0).unwrap();
    let target = runtime
        .inventory(workspace)
        .resumable
        .into_iter()
        .find_map(|item| {
            item.target
                .filter(|target| target.runtime_id == second.runtime.agent_runtime_id)
        })
        .unwrap();
    let resumed_operation = OperationId::new();
    let resumed = runtime
        .resume_exact(&resumed_operation.to_string(), &target, &resolved)
        .unwrap();
    assert_eq!(
        runtime
            .dispatch
            .binding(resumed_operation)
            .unwrap()
            .unwrap()
            .worker
            .agent_id,
        second_agent
    );
    assert_eq!(
        runtime
            .dispatch
            .agent(first_agent)
            .unwrap()
            .unwrap()
            .current_run,
        Some(first_operation)
    );
    runtime
        .notify_peer(workspace, session, first_agent)
        .unwrap();
    assert_eq!(pty(&runtime).selected.as_ref(), Some(&first.terminal));
    runtime
        .notify_peer(workspace, session, second_agent)
        .unwrap();
    assert_eq!(pty(&runtime).selected.as_ref(), Some(&resumed.terminal));
    assert_eq!(
        runtime.workflow_operation_lineage(second_operation),
        vec![second_operation, resumed_operation]
    );
    assert_eq!(
        runtime.workflow_live_operation(second_operation),
        Some(resumed_operation)
    );
    assert_eq!(
        runtime.workflow_operation_lineage(first_operation),
        vec![first_operation]
    );
    assert!(
        runtime
            .workflow_operation_lineage(OperationId::new())
            .is_empty()
    );
    runtime.exit(&resumed.terminal, 0).unwrap();
    assert_eq!(
        runtime.workflow_operation_lineage(second_operation),
        vec![second_operation, resumed_operation]
    );
    assert_eq!(runtime.workflow_live_operation(second_operation), None);
    assert_eq!(runtime.workflow_live_operation(OperationId::new()), None);
    let snapshot = runtime.coordinator.snapshot();
    for replacement in [AgentRuntimeId::new(), resumed.runtime.agent_runtime_id] {
        let mut broken = snapshot.clone();
        broken
            .records
            .iter_mut()
            .find(|record| record.operation.operation_id == resumed_operation)
            .unwrap()
            .superseded_by = Some(replacement);
        // Missing/cyclic replacement data cannot invent another admitted
        // operation or loop forever, even before hydration rejects it.
        assert_eq!(
            AgentRuntime::workflow_lineage(&broken, second_operation),
            vec![second_operation, resumed_operation]
        );
    }
}
