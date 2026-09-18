//! daemon の振る舞いを固定するテスト。

#![coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=module_unit_contract

use super::*;
use std::cell::Cell;
use std::process::Command;
use std::sync::atomic::AtomicUsize;

use usagi_daemon::usecase::generic_terminal::TerminalStoreSnapshot;
use usagi_daemon::usecase::runtime::{RuntimeStore, RuntimeStoreSnapshot};

use usagi_core::domain::{
    agent::{AgentResumeTarget, DaemonRestartAgent},
    id::{
        AgentRuntimeId, AgentRuntimeRef, ClientId, ConnectionId, DaemonGeneration, RequestId,
        SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
    },
    terminal_launch::{TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId},
};
use usagi_core::infrastructure::ipc::{
    TerminalAction, TerminalGeometry, TerminalLaunchIntent, TerminalRequest,
};
use usagi_daemon::presentation::ipc::encode_terminal_response;
use usagi_daemon::usecase::terminal::SnapshotWire;
use usagi_daemon::usecase::terminal_ipc::{
    ResolvedTerminalScope, TerminalScopeResolveError, TerminalScopeResolver,
};
use usagi_daemon::usecase::terminal_owner::{TerminalOwner, TerminalRequestContext};

fn protocol_response(code: ErrorCode) -> Envelope {
    Envelope {
        protocol: usagi_core::infrastructure::ipc::ProtocolVersion {
            generation: 1,
            revision: 0,
        },
        daemon_generation: usagi_core::infrastructure::ipc::DaemonGeneration(
            "generation".to_owned(),
        ),
        kind: EnvelopeKind::Response {
            request_id: usagi_core::infrastructure::ipc::RequestId("request".to_owned()),
            outcome: ResponseOutcome::Error(usagi_core::infrastructure::ipc::ProtocolError::new(
                code,
                "safe reason",
            )),
            body: serde_json::Value::Null,
        },
    }
}

#[test]
fn daemon_error_log_policy_records_abnormal_responses_only() {
    for (code, expected) in [
        (ErrorCode::ProtocolMismatch, true),
        (ErrorCode::CapabilityMissing, true),
        (ErrorCode::GenerationMismatch, true),
        (ErrorCode::Unauthenticated, true),
        (ErrorCode::PermissionDenied, true),
        (ErrorCode::InvalidArgument, false),
        (ErrorCode::NotFound, false),
        (ErrorCode::StaleTarget, false),
        (ErrorCode::GenerationRolledOver, false),
        (ErrorCode::RevisionConflict, false),
        (ErrorCode::IdempotencyConflict, false),
        (ErrorCode::IdempotencyExpired, false),
        (ErrorCode::SequenceGap, false),
        (ErrorCode::ResourceExhausted, true),
        (ErrorCode::Backpressure, true),
        (ErrorCode::Busy, false),
        (ErrorCode::DeadlineExceeded, true),
        (ErrorCode::Cancelled, false),
        (ErrorCode::OwnershipUnknown, true),
        (ErrorCode::Unavailable, true),
        (ErrorCode::Internal, true),
        (ErrorCode::ResyncRequired, false),
    ] {
        assert_eq!(
            unexpected_daemon_response_entry("agent", &protocol_response(code)).is_some(),
            expected,
            "{code:?}"
        );
    }

    let entry =
        unexpected_daemon_response_entry("agent", &protocol_response(ErrorCode::Unavailable))
            .unwrap();
    assert!(entry.contains("surface=agent request=request code=Unavailable"));
    assert!(entry.contains("error_id=protocol"));
    assert!(entry.contains("message=safe reason"));

    let mut ok = protocol_response(ErrorCode::Unavailable);
    let EnvelopeKind::Response { outcome, .. } = &mut ok.kind else {
        unreachable!();
    };
    *outcome = ResponseOutcome::Ok;
    assert!(unexpected_daemon_response_entry("agent", &ok).is_none());
    ok.kind = EnvelopeKind::Request {
        request_id: usagi_core::infrastructure::ipc::RequestId("request".to_owned()),
        timeout_ms: None,
        body: serde_json::Value::Null,
    };
    assert!(unexpected_daemon_response_entry("agent", &ok).is_none());
}

#[test]
fn daemon_request_surface_never_copies_untrusted_kind_text() {
    for (kind, expected) in [
        ("mcp_child_claim", "mcp_child_claim"),
        ("rollover", "rollover"),
        ("tenant", "tenant"),
        ("session", "session"),
        ("agent", "agent"),
        ("agent_goal", "agent"),
        ("agent_inventory", "agent"),
        ("agent_workspace_observation", "agent"),
        ("diagnose_agents", "agent"),
        ("plan_daemon_restart_agents", "agent"),
        ("restart_agents", "agent"),
        ("resume_agent", "agent"),
        ("resume_agent_with_current_integration", "agent"),
        ("codex_session_capture", "codex_session_capture"),
        ("agent_phase_report", "agent_phase_report"),
        ("dispatch", "dispatch"),
        ("metrics", "metrics"),
        ("pr", "pr"),
        ("pr_batch", "pr"),
        ("pr_dismiss", "pr"),
        ("dispatch_tool", "dispatch_tool"),
        ("supervisor_tool", "supervisor_tool"),
        ("supervisor_snapshot", "supervisor_snapshot"),
        ("supervisor_control", "supervisor_control"),
        ("user_decision", "user_decision"),
        ("terminal", "terminal"),
        ("attacker\nforged log", "generic"),
    ] {
        assert_eq!(
            daemon_request_surface(&serde_json::json!({"kind": kind})),
            expected
        );
    }
    assert_eq!(daemon_request_surface(&serde_json::Value::Null), "unknown");
    assert_eq!(safe_log_token("request\nforged/値"), "request_forged____");
    assert_eq!(safe_log_token(&"x".repeat(129)).len(), 128);
}

#[test]
fn daemon_connection_logging_ignores_only_normal_peer_disconnects() {
    for kind in [
        std::io::ErrorKind::BrokenPipe,
        std::io::ErrorKind::ConnectionAborted,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::Interrupted,
        std::io::ErrorKind::NotConnected,
        std::io::ErrorKind::UnexpectedEof,
    ] {
        assert!(expected_client_disconnect(kind), "{kind:?}");
    }
    for kind in [
        std::io::ErrorKind::InvalidData,
        std::io::ErrorKind::OutOfMemory,
        std::io::ErrorKind::PermissionDenied,
        std::io::ErrorKind::TimedOut,
    ] {
        assert!(!expected_client_disconnect(kind), "{kind:?}");
    }
}

#[test]
fn daemon_pty_failure_entry_contains_identity_and_reason_without_launch_data() {
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    let entry = daemon_pty_failure_entry(
        "Agent",
        "spawn",
        &terminal,
        "PTY child spawn failed: executable not found",
    );
    assert!(entry.contains("daemon Agent PTY failed: action=spawn"));
    assert!(entry.contains(&format!("session={}", terminal.session_id.unwrap())));
    assert!(entry.contains("error=PTY child spawn failed: executable not found"));

    let root = TerminalRef {
        session_id: None,
        ..terminal
    };
    assert!(
        daemon_pty_failure_entry("terminal", "observe-child", &root, "missing pid")
            .contains("session=workspace-root")
    );
}

#[test]
fn terminal_capacity_uses_the_global_setting_and_falls_back_safely() {
    let temporary = tempfile::tempdir().unwrap();
    assert_eq!(
        terminal_capacity_limit(temporary.path()),
        GENERIC_TERMINAL_LIMIT
    );

    let configured = usagi_core::domain::settings::Settings {
        terminal_max_concurrent: usagi_core::domain::settings::TerminalConcurrencyLimit::new(128)
            .unwrap(),
        ..usagi_core::domain::settings::Settings::default()
    };
    Storage::new(temporary.path())
        .save_settings(&configured)
        .unwrap();
    assert_eq!(terminal_capacity_limit(temporary.path()), 128);

    std::fs::write(
        temporary.path().join("settings.json"),
        br#"{"terminal_max_concurrent":0}"#,
    )
    .unwrap();
    assert_eq!(
        terminal_capacity_limit(temporary.path()),
        GENERIC_TERMINAL_LIMIT
    );
}

#[derive(Default)]
struct SupervisorAgentStore;

impl RuntimeStore for SupervisorAgentStore {
    fn save(&mut self, _: RuntimeStoreSnapshot) -> Result<(), ()> {
        Ok(())
    }
}

#[derive(Default)]
struct SupervisorAgentJournal;

impl OutputJournal for SupervisorAgentJournal {
    fn append(&mut self, _: &Output) -> Result<(), ()> {
        Ok(())
    }
}

#[derive(Default)]
struct SupervisorAgentPty;

impl PtySpawner for SupervisorAgentPty {
    fn spawn(
        &mut self,
        _: &DurableLaunchSnapshot,
        _: &SpawnProvision,
        _: &TerminalRef,
    ) -> Result<ProcessIdentity, SpawnFailure> {
        Ok(ProcessIdentity {
            pid: 4_321,
            start_identity: "supervisor-agent-fixture".into(),
            process_group: 4_321,
        })
    }

    fn terminate_reap(&mut self, _: &TerminalRef) -> Result<(), TerminateReapError> {
        Ok(())
    }
}

impl PtyWriter for SupervisorAgentPty {
    fn write_all(&mut self, _: &[u8]) -> Result<(), PtyWriteError> {
        Ok(())
    }
}

fn empty_supervisor_agent(dispatch: DispatchStore) -> SharedAgentRuntime {
    Arc::new(SharedAgentState {
        owner: Mutex::new(AgentRuntime::with_dispatch(
            DaemonGeneration::new(),
            AdapterRegistry::new(),
            SupervisorAgentStore,
            SupervisorAgentJournal,
            SupervisorAgentPty,
            AgentProfileId::new("claude").unwrap(),
            Geometry { cols: 80, rows: 24 },
            dispatch,
        )),
        readiness: Arc::new(SystemAgentReadiness::default()),
    })
}

fn persist_supervisor_dispatch(
    dispatch: &DispatchStore,
    workspace: WorkspaceId,
    operation: usagi_core::domain::id::OperationId,
    agent_id: usagi_core::domain::id::AgentId,
    worker: &AgentRuntimeRef,
    semantic_key: String,
) {
    use usagi_core::domain::agent::{
        Agent, AgentStatus, CallerRef, DispatchBinding, DispatchRun, ModelSelector, RunStatus,
        WorkerRef,
    };
    use usagi_core::infrastructure::store::dispatch::{
        AgentAdmissionReservation, CredentialProvenance,
    };

    dispatch
        .reserve_admission_for_workspace(
            workspace,
            Agent {
                agent_id,
                session_id: worker.session_id,
                runtime: AgentProfileId::new("claude").unwrap(),
                model: ModelSelector::new("test").unwrap(),
                status: AgentStatus::Running,
                current_run: Some(operation),
            },
            DispatchRun {
                run_id: operation,
                agent_id,
                prompt: String::new(),
                started_at: chrono::Utc::now(),
                ended_at: None,
                status: RunStatus::Running,
            },
            DispatchBinding {
                run_id: operation,
                caller: CallerRef {
                    session_id: worker.session_id,
                    agent_id,
                },
                worker: WorkerRef {
                    session_id: worker.session_id,
                    agent_id,
                },
            },
            AgentAdmissionReservation {
                operation_id: operation,
                semantic_key,
                credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
            },
        )
        .unwrap();
}

fn supervisor_admission(
    operation: usagi_core::domain::id::OperationId,
    worker: &AgentRuntimeRef,
) -> AgentAdmission {
    AgentAdmission {
        operation_id: operation.to_string(),
        revision: 1,
        runtime: worker.clone(),
        terminal: worker.terminal.clone(),
        continuation: None,
        resume_relation: None,
        completed: false,
        semantic_digest: None,
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One table-like fixture exercises every independent admission identity fence.
fn promotion_admission_matching_rejects_each_collision_fence() {
    use usagi_core::domain::id::{AgentId, OperationId};

    let temp = tempfile::tempdir().unwrap();
    let dispatch = DispatchStore::new(temp.path());
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let operation = OperationId::new();
    let agent_id = AgentId::new();
    let worker = AgentRuntimeRef::new(
        AgentRuntimeId::new(),
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: WorktreeId::new(),
        },
        Some(session),
    )
    .unwrap();
    let semantic = "promotion-worker";
    let digest = usagi_core::infrastructure::ipc::agent_operation_digest(semantic);
    persist_supervisor_dispatch(
        &dispatch,
        workspace,
        operation,
        agent_id,
        &worker,
        semantic.into(),
    );
    let shared = empty_supervisor_agent(dispatch);
    let runtime = shared.owner.lock().unwrap();

    assert!(
        promotion_admission_matches(
            &runtime,
            &operation.to_string(),
            &worker,
            workspace,
            true,
            Some(session),
            Some(worker.agent_runtime_id),
            Some(agent_id),
            Some(&AgentProfileId::new("claude").unwrap()),
            Some(&digest),
        )
        .unwrap()
    );
    assert!(
        promotion_admission_matches(
            &runtime,
            &operation.to_string(),
            &worker,
            workspace,
            true,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
    );
    for mismatch in [
        promotion_admission_matches(
            &runtime,
            &operation.to_string(),
            &worker,
            WorkspaceId::new(),
            true,
            Some(session),
            None,
            None,
            None,
            None,
        ),
        promotion_admission_matches(
            &runtime,
            &operation.to_string(),
            &worker,
            workspace,
            false,
            None,
            None,
            None,
            None,
            None,
        ),
        promotion_admission_matches(
            &runtime,
            &operation.to_string(),
            &worker,
            workspace,
            true,
            Some(session),
            Some(AgentRuntimeId::new()),
            None,
            None,
            None,
        ),
        promotion_admission_matches(
            &runtime,
            &operation.to_string(),
            &worker,
            workspace,
            true,
            Some(session),
            None,
            Some(AgentId::new()),
            None,
            None,
        ),
        promotion_admission_matches(
            &runtime,
            &operation.to_string(),
            &worker,
            workspace,
            true,
            Some(session),
            None,
            None,
            Some(&AgentProfileId::new("codex").unwrap()),
            None,
        ),
        promotion_admission_matches(
            &runtime,
            &operation.to_string(),
            &worker,
            workspace,
            true,
            Some(session),
            None,
            None,
            None,
            Some("wrong-digest"),
        ),
    ] {
        assert!(!mismatch.unwrap());
    }
    assert!(
        promotion_admission_matches(
            &runtime,
            "invalid",
            &worker,
            workspace,
            true,
            Some(session),
            None,
            None,
            None,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("operation is invalid")
    );
    assert!(
        promotion_admission_matches(
            &runtime,
            &OperationId::new().to_string(),
            &worker,
            workspace,
            true,
            Some(session),
            None,
            None,
            None,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("dispatch is unavailable")
    );
    drop(runtime);

    let missing_agent_root = tempfile::tempdir().unwrap();
    let missing_agent_dispatch = DispatchStore::new(missing_agent_root.path());
    let missing_agent_operation = OperationId::new();
    missing_agent_dispatch
        .upsert_run(usagi_core::domain::agent::DispatchRun {
            run_id: missing_agent_operation,
            agent_id: AgentId::new(),
            prompt: String::new(),
            started_at: chrono::Utc::now(),
            ended_at: None,
            status: usagi_core::domain::agent::RunStatus::Running,
        })
        .unwrap();
    let missing_agent = empty_supervisor_agent(missing_agent_dispatch);
    assert!(
        promotion_admission_matches(
            &missing_agent.owner.lock().unwrap(),
            &missing_agent_operation.to_string(),
            &worker,
            workspace,
            true,
            Some(session),
            None,
            None,
            None,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("Agent is unavailable")
    );

    let missing_admission_root = tempfile::tempdir().unwrap();
    let missing_admission_dispatch = DispatchStore::new(missing_admission_root.path());
    let missing_admission_operation = OperationId::new();
    let missing_admission_agent = AgentId::new();
    missing_admission_dispatch
        .upsert_agent(
            workspace,
            usagi_core::domain::agent::Agent {
                agent_id: missing_admission_agent,
                session_id: Some(session),
                runtime: AgentProfileId::new("claude").unwrap(),
                model: usagi_core::domain::agent::ModelSelector::new("test").unwrap(),
                status: usagi_core::domain::agent::AgentStatus::Running,
                current_run: Some(missing_admission_operation),
            },
        )
        .unwrap();
    missing_admission_dispatch
        .upsert_run(usagi_core::domain::agent::DispatchRun {
            run_id: missing_admission_operation,
            agent_id: missing_admission_agent,
            prompt: String::new(),
            started_at: chrono::Utc::now(),
            ended_at: None,
            status: usagi_core::domain::agent::RunStatus::Running,
        })
        .unwrap();
    let missing_admission = empty_supervisor_agent(missing_admission_dispatch);
    assert!(
        promotion_admission_matches(
            &missing_admission.owner.lock().unwrap(),
            &missing_admission_operation.to_string(),
            &worker,
            workspace,
            true,
            Some(session),
            None,
            Some(missing_admission_agent),
            None,
            Some(&digest),
        )
        .unwrap_err()
        .to_string()
        .contains("admission is unavailable")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Caller, Goal, and delegated reservations must share the same live/startup boundary.
fn supervisor_promotion_reconciliation_distinguishes_live_and_startup_absence() {
    use usagi_core::domain::{
        agent::{Agent, AgentStatus, ModelSelector},
        id::{AgentId, OperationId},
        pr_inventory::GitHubRepository,
        supervisor::SupervisorRunState,
    };
    use usagi_daemon::usecase::supervisor_runtime::GoalSpecification;

    let temp = tempfile::tempdir().unwrap();
    let dispatch = DispatchStore::new(temp.path());
    let supervisor = Arc::new(Mutex::new(SupervisorRuntime::new(temp.path())));
    let workspace = WorkspaceId::new();
    let operation = OperationId::new();
    let started = supervisor
        .lock()
        .unwrap()
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &operation.to_string(),
            GoalSpecification::new(
                "finish".into(),
                GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
            ),
            None,
            chrono::Utc::now(),
        )
        .unwrap();

    let caller_operation = OperationId::new();
    let caller_worker = AgentRuntimeRef::new(
        AgentRuntimeId::new(),
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: None,
            worktree_id: WorktreeId::new(),
        },
        None,
    )
    .unwrap();
    persist_supervisor_dispatch(
        &dispatch,
        workspace,
        caller_operation,
        AgentId::new(),
        &caller_worker,
        "caller-promotion".into(),
    );
    let caller_start = OperationId::new().to_string();
    let caller_started = supervisor
        .lock()
        .unwrap()
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &caller_start,
            "caller root".into(),
            None,
            caller_operation,
            &caller_worker,
            chrono::Utc::now(),
        )
        .unwrap();

    let parent_operation = OperationId::new();
    let parent_worker = AgentRuntimeRef::new(
        AgentRuntimeId::new(),
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: None,
            worktree_id: WorktreeId::new(),
        },
        None,
    )
    .unwrap();
    persist_supervisor_dispatch(
        &dispatch,
        workspace,
        parent_operation,
        AgentId::new(),
        &parent_worker,
        "parent-promotion".into(),
    );
    let parent_started = supervisor
        .lock()
        .unwrap()
        .start_for_workspace_root_dispatch(
            "goal",
            workspace,
            &parent_operation.to_string(),
            GoalSpecification::new(
                "parent".into(),
                GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
            ),
            None,
            &parent_worker,
            chrono::Utc::now(),
        )
        .unwrap();
    let child_operation = OperationId::new();
    let child_session = SessionId::new();
    let planned_child = Agent {
        agent_id: AgentId::new(),
        session_id: Some(child_session),
        runtime: AgentProfileId::new("claude").unwrap(),
        model: ModelSelector::new("test").unwrap(),
        status: AgentStatus::Idle,
        current_run: None,
    };
    supervisor
        .lock()
        .unwrap()
        .reserve_delegated_dispatch_for_session(
            parent_operation,
            &child_operation.to_string(),
            "child",
            child_session,
            &planned_child,
            "child-session",
            chrono::Utc::now(),
        )
        .unwrap()
        .unwrap();
    let agent = empty_supervisor_agent(dispatch);

    {
        let runtime = supervisor.lock().unwrap();
        assert_eq!(runtime.pending_caller_promotions().unwrap().len(), 1);
        assert_eq!(runtime.pending_goal_promotions().unwrap().len(), 1);
        assert_eq!(runtime.pending_delegated_promotions().unwrap().len(), 1);
    }

    assert_eq!(
        reconcile_pending_supervisor_promotions(&supervisor, &agent).unwrap(),
        0
    );
    assert_eq!(
        supervisor
            .lock()
            .unwrap()
            .get_for_workspace(workspace, started.supervisor_run_id)
            .unwrap()
            .unwrap()
            .state,
        SupervisorRunState::Running
    );
    assert_eq!(
        reconcile_startup_supervisor_promotions(&supervisor, &agent).unwrap(),
        3
    );
    assert_eq!(
        supervisor
            .lock()
            .unwrap()
            .get_for_workspace(workspace, started.supervisor_run_id)
            .unwrap()
            .unwrap()
            .state,
        SupervisorRunState::Failed
    );
    assert_eq!(
        supervisor
            .lock()
            .unwrap()
            .get_for_workspace(workspace, caller_started.supervisor_run_id)
            .unwrap()
            .unwrap()
            .state,
        SupervisorRunState::Failed
    );
    let runtime = supervisor.lock().unwrap();
    assert!(runtime.pending_delegated_promotions().unwrap().is_empty());
    assert_eq!(
        runtime
            .get_for_workspace(workspace, parent_started.supervisor_run_id)
            .unwrap()
            .unwrap()
            .state,
        SupervisorRunState::Running
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One matrix proves every promotion kind binds and rejects an identity collision.
fn supervisor_promotion_outcomes_bind_or_close_every_reservation_kind() {
    use usagi_core::domain::{
        agent::{Agent, AgentStatus, ModelSelector},
        id::{AgentId, OperationId},
        pr_inventory::GitHubRepository,
    };
    use usagi_daemon::usecase::supervisor_runtime::GoalSpecification;

    let temp = tempfile::tempdir().unwrap();
    let dispatch = DispatchStore::new(temp.path());
    let supervisor = Arc::new(Mutex::new(SupervisorRuntime::new(temp.path())));
    let agent = empty_supervisor_agent(dispatch.clone());
    let workspace = WorkspaceId::new();
    let now = chrono::Utc::now();
    let root_worker = |workspace_id| {
        AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id,
                session_id: None,
                worktree_id: WorktreeId::new(),
            },
            None,
        )
        .unwrap()
    };
    let session_worker = |workspace_id| {
        let session = SessionId::new();
        AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id,
                session_id: Some(session),
                worktree_id: WorktreeId::new(),
            },
            Some(session),
        )
        .unwrap()
    };

    let goal_operation = OperationId::new();
    let goal_worker = root_worker(workspace);
    let goal_agent = AgentId::new();
    let goal_semantic = "goal-success";
    let goal_digest = usagi_core::infrastructure::ipc::agent_operation_digest(goal_semantic);
    persist_supervisor_dispatch(
        &dispatch,
        workspace,
        goal_operation,
        goal_agent,
        &goal_worker,
        goal_semantic.into(),
    );
    supervisor
        .lock()
        .unwrap()
        .reserve_goal_for_workspace_with_profile(
            "goal",
            workspace,
            &goal_operation.to_string(),
            GoalSpecification::new(
                "goal success".into(),
                GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
            ),
            AgentProfileId::new("claude").unwrap(),
            goal_digest.clone(),
            None,
            now,
        )
        .unwrap();
    let goal_candidate = PendingPromotionCandidate {
        kind: PendingPromotionKind::Goal,
        operation_id: goal_operation.to_string(),
        workspace_id: workspace,
        requires_worker_session: false,
        worker_session_id: None,
        worker_runtime_id: None,
        worker_agent_id: None,
        worker_profile_id: Some(AgentProfileId::new("claude").unwrap()),
        worker_semantic_digest: Some(goal_digest),
    };
    assert!(
        reconcile_supervisor_promotion_outcome(
            &supervisor,
            &agent,
            &goal_candidate,
            false,
            now,
            Some(Ok(supervisor_admission(goal_operation, &goal_worker))),
        )
        .unwrap()
    );

    let caller_operation = OperationId::new();
    let caller_worker = root_worker(workspace);
    let caller_agent = AgentId::new();
    let caller_semantic = "caller-success";
    let caller_digest = usagi_core::infrastructure::ipc::agent_operation_digest(caller_semantic);
    persist_supervisor_dispatch(
        &dispatch,
        workspace,
        caller_operation,
        caller_agent,
        &caller_worker,
        caller_semantic.into(),
    );
    let caller_start = OperationId::new().to_string();
    supervisor
        .lock()
        .unwrap()
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &caller_start,
            "caller success".into(),
            None,
            caller_operation,
            &caller_worker,
            now,
        )
        .unwrap();
    let caller_candidate = PendingPromotionCandidate {
        kind: PendingPromotionKind::Caller {
            start_operation_id: caller_start,
        },
        operation_id: caller_operation.to_string(),
        workspace_id: workspace,
        requires_worker_session: false,
        worker_session_id: None,
        worker_runtime_id: Some(caller_worker.agent_runtime_id),
        worker_agent_id: Some(caller_agent),
        worker_profile_id: Some(AgentProfileId::new("claude").unwrap()),
        worker_semantic_digest: Some(caller_digest),
    };
    assert!(
        reconcile_supervisor_promotion_outcome(
            &supervisor,
            &agent,
            &caller_candidate,
            false,
            now,
            Some(Ok(supervisor_admission(caller_operation, &caller_worker,))),
        )
        .unwrap()
    );

    let child_operation = OperationId::new();
    let child_worker = session_worker(workspace);
    let planned_child = Agent {
        agent_id: AgentId::new(),
        session_id: child_worker.session_id,
        runtime: AgentProfileId::new("claude").unwrap(),
        model: ModelSelector::new("test").unwrap(),
        status: AgentStatus::Idle,
        current_run: None,
    };
    let reserved_child = supervisor
        .lock()
        .unwrap()
        .reserve_delegated_dispatch_for_session(
            goal_operation,
            &child_operation.to_string(),
            "child success",
            child_worker.session_id.unwrap(),
            &planned_child,
            "child-session",
            now,
        )
        .unwrap()
        .unwrap();
    let child_semantic = usagi_core::infrastructure::ipc::agent_dispatch_semantic_key(
        "child-session",
        planned_child.agent_id,
        &reserved_child.prompt,
    );
    let child_digest = usagi_core::infrastructure::ipc::agent_operation_digest(&child_semantic);
    persist_supervisor_dispatch(
        &dispatch,
        workspace,
        child_operation,
        planned_child.agent_id,
        &child_worker,
        child_semantic,
    );
    let child_candidate = PendingPromotionCandidate {
        kind: PendingPromotionKind::Delegated,
        operation_id: child_operation.to_string(),
        workspace_id: workspace,
        requires_worker_session: true,
        worker_session_id: child_worker.session_id,
        worker_runtime_id: None,
        worker_agent_id: Some(planned_child.agent_id),
        worker_profile_id: Some(planned_child.runtime),
        worker_semantic_digest: Some(child_digest),
    };
    assert!(
        reconcile_supervisor_promotion_outcome(
            &supervisor,
            &agent,
            &child_candidate,
            false,
            now,
            Some(Ok(supervisor_admission(child_operation, &child_worker))),
        )
        .unwrap()
    );

    let wrong_worker = root_worker(WorkspaceId::new());
    for candidate in [&goal_candidate, &caller_candidate, &child_candidate] {
        let collision = supervisor_admission(
            OperationId::parse(&candidate.operation_id).unwrap(),
            &wrong_worker,
        );
        assert!(
            reconcile_supervisor_promotion_outcome(
                &supervisor,
                &agent,
                candidate,
                false,
                now,
                Some(Ok(collision)),
            )
            .unwrap()
        );
    }

    let invalid_candidate = PendingPromotionCandidate {
        operation_id: "invalid".into(),
        ..goal_candidate
    };
    assert!(
        reconcile_supervisor_promotion_outcome(
            &supervisor,
            &agent,
            &invalid_candidate,
            false,
            now,
            Some(Ok(supervisor_admission(goal_operation, &goal_worker))),
        )
        .is_err()
    );

    let unreserved_operation = OperationId::new();
    let unreserved_worker = session_worker(workspace);
    let unreserved_agent = AgentId::new();
    let unreserved_semantic = "unreserved-child";
    let unreserved_digest =
        usagi_core::infrastructure::ipc::agent_operation_digest(unreserved_semantic);
    persist_supervisor_dispatch(
        &dispatch,
        workspace,
        unreserved_operation,
        unreserved_agent,
        &unreserved_worker,
        unreserved_semantic.into(),
    );
    let unreserved_candidate = PendingPromotionCandidate {
        kind: PendingPromotionKind::Delegated,
        operation_id: unreserved_operation.to_string(),
        workspace_id: workspace,
        requires_worker_session: true,
        worker_session_id: unreserved_worker.session_id,
        worker_runtime_id: None,
        worker_agent_id: Some(unreserved_agent),
        worker_profile_id: Some(AgentProfileId::new("claude").unwrap()),
        worker_semantic_digest: Some(unreserved_digest),
    };
    assert!(
        reconcile_supervisor_promotion_outcome(
            &supervisor,
            &agent,
            &unreserved_candidate,
            false,
            now,
            Some(Ok(supervisor_admission(
                unreserved_operation,
                &unreserved_worker,
            ))),
        )
        .unwrap_err()
        .to_string()
        .contains("reservation does not exist")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Failure outcomes share one matrix so every reservation kind remains explicit.
fn supervisor_promotion_failures_preserve_unknown_ownership_and_close_definite_errors() {
    use usagi_core::domain::{
        agent::{Agent, AgentStatus, ModelSelector},
        id::{AgentId, OperationId},
        pr_inventory::GitHubRepository,
    };
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    use usagi_daemon::usecase::supervisor_runtime::GoalSpecification;

    let temp = tempfile::tempdir().unwrap();
    let dispatch = DispatchStore::new(temp.path());
    let supervisor = Arc::new(Mutex::new(SupervisorRuntime::new(temp.path())));
    let agent = empty_supervisor_agent(dispatch.clone());
    let workspace = WorkspaceId::new();
    let now = chrono::Utc::now();
    let root_worker = |workspace_id| {
        AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            TerminalRef {
                daemon_generation: DaemonGeneration::new(),
                terminal_id: TerminalId::new(),
                workspace_id,
                session_id: None,
                worktree_id: WorktreeId::new(),
            },
            None,
        )
        .unwrap()
    };

    let unknown_goal = OperationId::new();
    supervisor
        .lock()
        .unwrap()
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &unknown_goal.to_string(),
            GoalSpecification::new(
                "unknown".into(),
                GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
            ),
            None,
            now,
        )
        .unwrap();
    let unknown_candidate = PendingPromotionCandidate {
        kind: PendingPromotionKind::Goal,
        operation_id: unknown_goal.to_string(),
        workspace_id: workspace,
        requires_worker_session: false,
        worker_session_id: None,
        worker_runtime_id: None,
        worker_agent_id: None,
        worker_profile_id: None,
        worker_semantic_digest: None,
    };
    let unknown = || {
        Some(Err(ProtocolError::new(
            ErrorCode::OwnershipUnknown,
            "ownership is unknown",
        )))
    };
    assert!(
        !reconcile_supervisor_promotion_outcome(
            &supervisor,
            &agent,
            &unknown_candidate,
            false,
            now,
            unknown(),
        )
        .unwrap()
    );
    assert!(
        reconcile_supervisor_promotion_outcome(
            &supervisor,
            &agent,
            &unknown_candidate,
            true,
            now,
            unknown(),
        )
        .unwrap()
    );

    let failed_goal = OperationId::new();
    supervisor
        .lock()
        .unwrap()
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &failed_goal.to_string(),
            GoalSpecification::new(
                "failed".into(),
                GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
            ),
            None,
            now,
        )
        .unwrap();
    let failed_goal_candidate = PendingPromotionCandidate {
        operation_id: failed_goal.to_string(),
        ..unknown_candidate
    };
    assert!(
        reconcile_supervisor_promotion_outcome(
            &supervisor,
            &agent,
            &failed_goal_candidate,
            false,
            now,
            Some(Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "definite failure",
            ))),
        )
        .unwrap()
    );

    let caller_operation = OperationId::new();
    let caller_worker = root_worker(workspace);
    persist_supervisor_dispatch(
        &dispatch,
        workspace,
        caller_operation,
        AgentId::new(),
        &caller_worker,
        "caller-failure".into(),
    );
    let caller_start = OperationId::new().to_string();
    supervisor
        .lock()
        .unwrap()
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &caller_start,
            "caller failure".into(),
            None,
            caller_operation,
            &caller_worker,
            now,
        )
        .unwrap();
    let caller_candidate = PendingPromotionCandidate {
        kind: PendingPromotionKind::Caller {
            start_operation_id: caller_start,
        },
        operation_id: caller_operation.to_string(),
        workspace_id: workspace,
        requires_worker_session: false,
        worker_session_id: None,
        worker_runtime_id: Some(caller_worker.agent_runtime_id),
        worker_agent_id: None,
        worker_profile_id: None,
        worker_semantic_digest: None,
    };
    assert!(
        reconcile_supervisor_promotion_outcome(
            &supervisor,
            &agent,
            &caller_candidate,
            false,
            now,
            Some(Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "definite failure",
            ))),
        )
        .unwrap()
    );

    let parent_operation = OperationId::new();
    let parent_worker = root_worker(workspace);
    persist_supervisor_dispatch(
        &dispatch,
        workspace,
        parent_operation,
        AgentId::new(),
        &parent_worker,
        "parent".into(),
    );
    supervisor
        .lock()
        .unwrap()
        .start_for_workspace_root_dispatch(
            "goal",
            workspace,
            &parent_operation.to_string(),
            GoalSpecification::new(
                "parent".into(),
                GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
            ),
            None,
            &parent_worker,
            now,
        )
        .unwrap();
    let child_operation = OperationId::new();
    let child_session = SessionId::new();
    let planned_child = Agent {
        agent_id: AgentId::new(),
        session_id: Some(child_session),
        runtime: AgentProfileId::new("claude").unwrap(),
        model: ModelSelector::new("test").unwrap(),
        status: AgentStatus::Idle,
        current_run: None,
    };
    supervisor
        .lock()
        .unwrap()
        .reserve_delegated_dispatch_for_session(
            parent_operation,
            &child_operation.to_string(),
            "child failure",
            child_session,
            &planned_child,
            "child-session",
            now,
        )
        .unwrap()
        .unwrap();
    let child_candidate = PendingPromotionCandidate {
        kind: PendingPromotionKind::Delegated,
        operation_id: child_operation.to_string(),
        workspace_id: workspace,
        requires_worker_session: true,
        worker_session_id: Some(child_session),
        worker_runtime_id: None,
        worker_agent_id: Some(planned_child.agent_id),
        worker_profile_id: Some(planned_child.runtime),
        worker_semantic_digest: None,
    };
    assert!(
        reconcile_supervisor_promotion_outcome(
            &supervisor,
            &agent,
            &child_candidate,
            false,
            now,
            Some(Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "definite failure",
            ))),
        )
        .unwrap()
    );

    let mut reconciled = 0;
    let mut first_failure = None;
    record_supervisor_promotion_result(&mut reconciled, &mut first_failure, "accepted", Ok(true));
    record_supervisor_promotion_result(&mut reconciled, &mut first_failure, "pending", Ok(false));
    record_supervisor_promotion_result(
        &mut reconciled,
        &mut first_failure,
        "first",
        Err(anyhow::anyhow!("first failure")),
    );
    record_supervisor_promotion_result(
        &mut reconciled,
        &mut first_failure,
        "second",
        Err(anyhow::anyhow!("second failure")),
    );
    assert_eq!(reconciled, 1);
    assert!(
        finish_supervisor_promotion_reconciliation(reconciled, first_failure)
            .unwrap_err()
            .to_string()
            .contains("first failure")
    );
    assert_eq!(
        finish_supervisor_promotion_reconciliation(2, None).unwrap(),
        2
    );
}

#[test]
fn supervisor_promotion_lock_failures_are_fail_closed() {
    use usagi_core::domain::id::OperationId;

    let startup_root = tempfile::tempdir().unwrap();
    let startup_supervisor = Arc::new(Mutex::new(SupervisorRuntime::new(startup_root.path())));
    let startup_agent = empty_supervisor_agent(DispatchStore::new(startup_root.path()));
    assert_eq!(
        reconcile_startup_supervisor_workers(&startup_supervisor, &startup_agent).unwrap(),
        0
    );

    let supervisor_root = tempfile::tempdir().unwrap();
    let supervisor = Arc::new(Mutex::new(SupervisorRuntime::new(supervisor_root.path())));
    let poisoned_supervisor = Arc::clone(&supervisor);
    assert!(
        std::thread::spawn(move || {
            let _guard = poisoned_supervisor.lock().unwrap();
            panic!("poison supervisor");
        })
        .join()
        .is_err()
    );
    assert!(lock_supervisor_runtime(&supervisor).is_err());

    let agent_root = tempfile::tempdir().unwrap();
    let agent = empty_supervisor_agent(DispatchStore::new(agent_root.path()));
    let poisoned_agent = Arc::clone(&agent);
    assert!(
        std::thread::spawn(move || {
            let _guard = poisoned_agent.owner.lock().unwrap();
            panic!("poison Agent");
        })
        .join()
        .is_err()
    );
    assert!(lock_agent_runtime(&agent).is_err());
    let candidate = PendingPromotionCandidate {
        kind: PendingPromotionKind::Goal,
        operation_id: OperationId::new().to_string(),
        workspace_id: WorkspaceId::new(),
        requires_worker_session: false,
        worker_session_id: None,
        worker_runtime_id: None,
        worker_agent_id: None,
        worker_profile_id: None,
        worker_semantic_digest: None,
    };
    let healthy_supervisor_root = tempfile::tempdir().unwrap();
    let healthy_supervisor = Arc::new(Mutex::new(SupervisorRuntime::new(
        healthy_supervisor_root.path(),
    )));
    assert!(
        reconcile_supervisor_promotion(
            &healthy_supervisor,
            &agent,
            &candidate,
            false,
            chrono::Utc::now(),
        )
        .is_err()
    );
    assert!(reconcile_supervisor_promotions(&supervisor, &agent, false).is_err());
}

#[test]
fn supervisor_control_errors_map_every_refusal_class() {
    use usagi_core::infrastructure::ipc::ErrorCode;

    for (message, code) in [
        ("capacity is exhausted", ErrorCode::ResourceExhausted),
        (
            "operation conflicts with its reservation",
            ErrorCode::IdempotencyConflict,
        ),
        (
            "operation conflicts with its semantic payload",
            ErrorCode::IdempotencyConflict,
        ),
        (
            "operation is outside the retained window",
            ErrorCode::IdempotencyExpired,
        ),
        (
            "run does not belong to workspace",
            ErrorCode::OwnershipUnknown,
        ),
        (
            "invalid supervisor cancellation reason",
            ErrorCode::InvalidArgument,
        ),
        ("InvalidTransition", ErrorCode::InvalidArgument),
        ("ProvenanceMismatch", ErrorCode::InvalidArgument),
        ("TerminalRun", ErrorCode::InvalidArgument),
        ("durable store failed", ErrorCode::Unavailable),
    ] {
        assert_eq!(
            supervisor_control_error(anyhow::anyhow!(message)).code,
            code
        );
    }
}

#[test]
fn supervised_delegation_refuses_fence_changes_during_session_creation() {
    use usagi_core::infrastructure::ipc::ErrorCode;

    for fence in [None, Some("run-a/task-root/1")] {
        require_stable_supervisor_fence(fence.as_ref(), fence.as_ref()).unwrap();
    }
    for changed in [
        (None, Some("run-a/task-root/1")),
        (Some("run-a/task-root/1"), None),
        (Some("run-a/task-root/1"), Some("run-b/task-root/1")),
    ] {
        assert_eq!(
            require_stable_supervisor_fence(changed.0.as_ref(), changed.1.as_ref())
                .unwrap_err()
                .code,
            ErrorCode::RevisionConflict
        );
    }
    for (fence, reservation_present) in [(None, false), (Some("fence"), true)] {
        require_supervisor_reservation_presence(fence.as_ref(), reservation_present).unwrap();
    }
    for (fence, reservation_present) in [(None, true), (Some("fence"), false)] {
        assert_eq!(
            require_supervisor_reservation_presence(fence.as_ref(), reservation_present)
                .unwrap_err()
                .code,
            ErrorCode::RevisionConflict
        );
    }
}

#[test]
fn generic_escalation_resume_does_not_require_an_agent_prompt() {
    use usagi_core::domain::{
        id::OperationId,
        supervisor::{EscalationDecision, SupervisorRunState, SupervisorWorkspaceCommand},
    };

    let temp = tempfile::tempdir().unwrap();
    let dispatch = DispatchStore::new(temp.path());
    let supervisor = Arc::new(Mutex::new(SupervisorRuntime::new(temp.path())));
    let agent = empty_supervisor_agent(dispatch);
    let workspace = WorkspaceId::new();
    let started = supervisor
        .lock()
        .unwrap()
        .start_for_workspace(
            "caller",
            workspace,
            &OperationId::new().to_string(),
            "work requiring later dispatch".into(),
            Vec::new(),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
    supervisor
        .lock()
        .unwrap()
        .tick(
            started.supervisor_run_id,
            chrono::Utc::now(),
            &mut AgentDecisionWaker { agent: &agent },
        )
        .unwrap();
    let escalated = supervisor
        .lock()
        .unwrap()
        .get_for_workspace(workspace, started.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert_eq!(escalated.state, SupervisorRunState::Escalated);
    let command = SupervisorWorkspaceCommand::ResolveEscalation {
        supervisor_run_id: started.supervisor_run_id,
        escalation_id: escalated.escalation.unwrap().escalation_id,
        decision: EscalationDecision::Resume,
    };

    prompt_supervisor_retry(&supervisor, &agent, workspace, &command).unwrap();
    assert_eq!(
        supervisor
            .lock()
            .unwrap()
            .control_for_workspace(workspace, OperationId::new(), &command, chrono::Utc::now(),)
            .unwrap()
            .state,
        SupervisorRunState::Running
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One reconciliation fixture joins bound and unbound run obligations.
fn supervisor_worker_reconciliation_joins_bound_and_unbound_runs_exactly() {
    use usagi_core::domain::{
        id::{AgentId, OperationId},
        pr_inventory::GitHubRepository,
        supervisor::{SupervisorRunState, SupervisorWorkspaceCommand},
    };
    use usagi_daemon::usecase::supervisor_runtime::GoalSpecification;

    let temp = tempfile::tempdir().unwrap();
    let dispatch = DispatchStore::new(temp.path());
    let supervisor = Arc::new(Mutex::new(SupervisorRuntime::new(temp.path())));
    let workspace = WorkspaceId::new();
    let dispatch_run = OperationId::new();
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: workspace,
        session_id: None,
        worktree_id: WorktreeId::new(),
    };
    let worker = AgentRuntimeRef::new(AgentRuntimeId::new(), terminal, None).unwrap();
    persist_supervisor_dispatch(
        &dispatch,
        workspace,
        dispatch_run,
        AgentId::new(),
        &worker,
        "root-reconciliation".into(),
    );
    let goal = || {
        GoalSpecification::new(
            "finish the goal".into(),
            GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
        )
    };
    let bound = supervisor
        .lock()
        .unwrap()
        .start_for_workspace_root_dispatch(
            "goal",
            workspace,
            &dispatch_run.to_string(),
            goal(),
            None,
            &worker,
            chrono::Utc::now(),
        )
        .unwrap();
    supervisor
        .lock()
        .unwrap()
        .control_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: bound.supervisor_run_id,
                reason: "operator cancelled".into(),
            },
            chrono::Utc::now(),
        )
        .unwrap();

    let unbound_operation = OperationId::new();
    let unbound = supervisor
        .lock()
        .unwrap()
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &unbound_operation.to_string(),
            goal(),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
    supervisor
        .lock()
        .unwrap()
        .control_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: unbound.supervisor_run_id,
                reason: "operator cancelled".into(),
            },
            chrono::Utc::now(),
        )
        .unwrap();

    let agent = empty_supervisor_agent(dispatch);
    assert_eq!(
        reconcile_supervisor_run_workers(&supervisor, &agent, bound.supervisor_run_id).unwrap(),
        0
    );
    assert_eq!(
        reconcile_supervisor_run_workers(&supervisor, &agent, unbound.supervisor_run_id).unwrap(),
        0
    );
    let pending = supervisor
        .lock()
        .unwrap()
        .pending_worker_stops_for_run(unbound.supervisor_run_id)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].operation_id(), unbound_operation);
    assert_eq!(
        reconcile_supervisor_run_workers(&supervisor, &agent, unbound.supervisor_run_id).unwrap(),
        0
    );
    assert_eq!(
        reconcile_aborted_supervisor_workers(&supervisor, &agent).unwrap(),
        0
    );
    assert_eq!(
        supervisor
            .lock()
            .unwrap()
            .get_for_workspace(workspace, bound.supervisor_run_id)
            .unwrap()
            .unwrap()
            .state,
        SupervisorRunState::Cancelled
    );
}
#[test]
fn supervisor_control_errors_distinguish_refusal_from_unknown_effect() {
    use usagi_core::infrastructure::ipc::{RetryMode, SideEffect};

    let refused = supervisor_control_error(anyhow::anyhow!("InvalidTransition"));
    assert_eq!(refused.side_effect, SideEffect::None);
    assert_eq!(refused.retry_mode, RetryMode::Never);

    let unknown = supervisor_control_error(anyhow::anyhow!("durable store failed"));
    assert_eq!(unknown.side_effect, SideEffect::PartialOrUnknown);
    assert_eq!(unknown.retry_mode, RetryMode::SameOperation);
    let post_commit = supervisor_control_unconfirmed("worker stop failed");
    assert_eq!(post_commit.side_effect, SideEffect::PartialOrUnknown);
    assert_eq!(post_commit.retry_mode, RetryMode::SameOperation);
}

#[cfg(unix)]
#[test]
fn readiness_timeout_coalesces_and_reaps_the_exact_child() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = tempfile::tempdir().unwrap();
    let program = fixture.path().join("codex");
    let pid_file = fixture.path().join("pid");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\necho $$ >> '{}'\ntrap '' TERM\nwhile :; do :; done\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let script = program.to_string_lossy().into_owned();
    let readiness = Arc::new(SystemAgentReadiness {
        state: Mutex::new(ReadinessState::default()),
        completed: Condvar::new(),
        terminate_grace: Duration::from_millis(50),
        ..SystemAgentReadiness::default()
    });
    let bounds = ReadinessBounds {
        timeout: Duration::from_millis(150),
        output_limit: 16 * 1024,
    };
    let first = {
        let readiness = Arc::clone(&readiness);
        let script = script.clone();
        std::thread::spawn(move || {
            readiness.ready_command("codex", "/bin/sh", &[&script], bounds, &[])
        })
    };
    let started = Instant::now();
    while !pid_file.is_file() && started.elapsed() < Duration::from_secs(1) {
        std::thread::yield_now();
    }
    assert!(pid_file.is_file(), "fixture readiness child started");
    let second = {
        let readiness = Arc::clone(&readiness);
        std::thread::spawn(move || {
            readiness.ready_command("codex", "/bin/sh", &[&script], bounds, &[])
        })
    };
    assert_eq!(first.join().unwrap(), AgentReadiness::Unavailable);
    assert_eq!(second.join().unwrap(), AgentReadiness::Unavailable);

    let pids = std::fs::read_to_string(pid_file).unwrap();
    let pids = pids.lines().collect::<Vec<_>>();
    assert_eq!(pids.len(), 1, "concurrent callers share one provider child");
    let pid = pids[0].parse::<libc::pid_t>().unwrap();
    // SAFETY: signal 0 only observes whether the fixture PID remains.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "timed-out readiness child was reaped"
    );
}

#[test]
fn a_gateway_provider_is_made_of_environment_and_fails_closed_without_it() {
    let home = PathBuf::from("/home/dev");
    let user = BTreeMap::from([
        ("SAKANA_API_KEY".to_owned(), "fish-secret".to_owned()),
        ("UNRELATED".to_owned(), "value".to_owned()),
    ]);
    let environment = provider_gateway_environment(DefaultModel::SakanaAi, Some(&home), &user)
        .expect("a configured key and a home are all this provider needs");
    let pairs = environment
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    // The endpoint and every model slot come from the vocabulary, so a
    // launch cannot lose one and quietly run as Anthropic Claude.
    assert_eq!(
        pairs.get("ANTHROPIC_BASE_URL").map(String::as_str),
        Some("https://api.sakana.ai")
    );
    assert_eq!(
        pairs
            .get("ANTHROPIC_DEFAULT_OPUS_MODEL")
            .map(String::as_str),
        Some("fugu-max[1m]")
    );
    // The CLI is pointed at this provider's own state, not the home the
    // Claude profile uses.
    assert_eq!(
        pairs.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        Some("/home/dev/.claude-sakana")
    );
    // The key is stored under the product's name and delivered under
    // Claude's, so the Claude profile never receives it.
    assert_eq!(
        pairs.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str),
        Some("fish-secret")
    );
    assert!(!pairs.contains_key("SAKANA_API_KEY"));
    assert!(!pairs.contains_key("UNRELATED"));

    // Without a home there is no isolated config directory to name, and the
    // CLI would fall back to the Claude profile's `~/.claude`.
    assert_eq!(
        provider_gateway_environment(DefaultModel::SakanaAi, None, &user),
        Err(())
    );
    // A missing key is not a provisioning failure: readiness refuses the
    // launch first, with a reason the user can act on. The rest of the
    // gateway is still assembled.
    let without_key =
        provider_gateway_environment(DefaultModel::SakanaAi, Some(&home), &BTreeMap::new())
            .expect("a missing credential does not fail provisioning");
    assert!(
        !without_key
            .iter()
            .any(|(name, _)| name.as_str() == "ANTHROPIC_AUTH_TOKEN")
    );
    assert_eq!(without_key.len(), environment.len() - 1);
    // A product that is its own CLI carries no gateway at all.
    for agent in [
        DefaultModel::Claude,
        DefaultModel::OpenAi,
        DefaultModel::Agy,
    ] {
        assert_eq!(
            provider_gateway_environment(agent, Some(&home), &user),
            Ok(Vec::new()),
            "{agent:?}"
        );
    }
}

#[test]
fn readiness_bounds_come_from_the_probed_product_not_a_shared_constant() {
    let agy = ReadinessBounds::for_probe(
        DefaultModel::readiness_command_for("agy").expect("Antigravity is a modelled product"),
    );
    let codex = ReadinessBounds::for_probe(
        DefaultModel::readiness_command_for("codex").expect("Codex is a modelled product"),
    );
    // The root carries whatever the vocabulary declares for that product
    // instead of re-imposing a budget of its own.
    assert_eq!(agy.timeout, DefaultModel::Agy.readiness_command().timeout());
    assert_eq!(
        agy.output_limit,
        DefaultModel::Agy.readiness_command().output_limit()
    );
    // `agy models` starts a language server and logs while it works, so its
    // budget is the larger one. A root that kept one shared constant — the
    // regression that reported an installed, authenticated CLI as
    // unavailable — would answer identically for both products here.
    assert!(agy.timeout > codex.timeout, "{agy:?} vs {codex:?}");
    assert!(
        agy.output_limit > codex.output_limit,
        "{agy:?} vs {codex:?}"
    );
}

#[cfg(unix)]
#[test]
fn readiness_probe_is_bounded_by_its_own_products_budget_not_a_shared_one() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = tempfile::tempdir().unwrap();
    let program = fixture.path().join("slow-status");
    // A status command that answers after a delay and prints while it works
    // stands in for `agy models`, which starts a language server and logs
    // for the whole probe.
    std::fs::write(
        &program,
        "#!/bin/sh\nawk 'BEGIN { while (i++ < 400) print \"log line\" }' >&2\nsleep 0.3\necho ready\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let program = program.to_string_lossy().into_owned();
    let grace = Duration::from_millis(50);

    // The product's own budget admits it.
    assert_eq!(
        bounded_readiness_command(
            &program,
            &[],
            &[],
            ReadinessBounds {
                timeout: Duration::from_secs(10),
                output_limit: 256 * 1024,
            },
            grace,
        ),
        AgentReadiness::Ready
    );
    // A budget sized for a credential read terminates the same healthy CLI
    // and reports it unavailable.
    assert_eq!(
        bounded_readiness_command(
            &program,
            &[],
            &[],
            ReadinessBounds {
                timeout: Duration::from_millis(50),
                output_limit: 256 * 1024,
            },
            grace,
        ),
        AgentReadiness::Unavailable
    );
    // So does a capture bound smaller than what a chatty probe prints.
    assert_eq!(
        bounded_readiness_command(
            &program,
            &[],
            &[],
            ReadinessBounds {
                timeout: Duration::from_secs(10),
                output_limit: 64,
            },
            grace,
        ),
        AgentReadiness::Unavailable
    );
}

#[test]
fn readiness_is_distinct_from_install_and_rejects_unauthenticated_status() {
    assert_eq!(
        readiness_from_observation(&ChildObservation::EmptyOutput),
        AgentReadiness::Ready
    );
    assert_eq!(
        readiness_from_observation(&ChildObservation::ExitFailure),
        AgentReadiness::Unavailable
    );
    assert_eq!(
        readiness_from_observation(&ChildObservation::TimedOut),
        AgentReadiness::Unavailable
    );
    assert_eq!(
        readiness_from_observation(&ChildObservation::OutputTooLarge),
        AgentReadiness::Unavailable
    );
}

fn request_terminal_json(
    owner: &mut dyn TerminalOwner,
    connection: ConnectionId,
    client: ClientId,
    request_id: RequestId,
    _action: TerminalAction,
    payload: serde_json::Value,
    wire: SnapshotWire,
) -> Result<serde_json::Value, usagi_core::infrastructure::ipc::ProtocolError> {
    let request = serde_json::from_value(payload).unwrap();
    owner
        .handle(
            TerminalRequestContext {
                connection,
                client,
                request: request_id,
            },
            request,
        )
        .map(|response| encode_terminal_response(response, wire))
}

fn daemon_test_info() -> AppInfo {
    AppInfo {
        name: "usagi",
        version: "0.1.0",
    }
}

/// An instance lock fixture that was never acquired. These tests drive the
/// publication and retirement seams directly; custody supervision starts
/// only from the production `publish` path, which owns a real acquired lock.
/// The fixture is leaked so it can satisfy `IpcReady`'s borrow without every
/// call site threading an extra binding through its scope.
fn unacquired_instance_lock(data_dir: &Path) -> &'static FileInstanceLock {
    Box::leak(Box::new(FileInstanceLock {
        path: data_dir.join("daemon/daemon.lock"),
        held: RefCell::new(None),
    }))
}

/// A workspace fence fixture that is already owned, so `serve` tests reach
/// the publication and retirement seams under test. The real fence's own
/// acquire / refuse / owner-hint behaviour has dedicated tests.
struct AcquiredWorkspaceFence;

impl WorkspaceFence for AcquiredWorkspaceFence {
    fn acquire(&self) -> std::io::Result<WorkspaceFenceOutcome> {
        Ok(WorkspaceFenceOutcome::Acquired)
    }
}

fn fresh_ipc_ready<'a>(data_dir: &'a Path, _info: &'a AppInfo) -> IpcReady<'a> {
    IpcReady {
        data_dir,
        // These tests never reach the real publication path, so the workspace
        // root only has to be a resolved directory.
        workspace_root: data_dir,
        instance_lock: unacquired_instance_lock(data_dir),
        build: BuildIdentity {
            version: "test".to_owned(),
            commit: "test".to_owned(),
            target: "test".to_owned(),
            artifact: "test-artifact".to_owned(),
        },
        shutdown: Arc::new(ShutdownRequest::new()),
        published: AtomicBool::new(false),
        publication_attempted: AtomicBool::new(false),
        worker: RefCell::new(None),
        listener: RefCell::new(None),
        cleanup: RefCell::new(None),
    }
}

fn ipc_generation() -> usagi_core::infrastructure::ipc::DaemonGeneration {
    usagi_core::infrastructure::ipc::DaemonGeneration(
        usagi_core::domain::id::DaemonGeneration::new().as_str(),
    )
}

struct SupersededCleanup;

impl usagi_daemon::usecase::stop::StaleDaemonCleanup for SupersededCleanup {
    fn cleanup_if(
        &self,
        _store: &dyn usagi_daemon::usecase::serve::DaemonRecordPort,
        _expected: &usagi_core::domain::daemon::DaemonRecord,
    ) -> std::io::Result<StaleCleanup> {
        Ok(StaleCleanup::Superseded)
    }
}

fn replace_private_lock_after_flock(path: &Path) -> std::thread::JoinHandle<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut replacement = path.as_os_str().to_owned();
    replacement.push(".replacement");
    let replacement = PathBuf::from(replacement);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&replacement)
        .unwrap();
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .unwrap();
    drop(file);

    let acquired = Arc::new(std::sync::Barrier::new(2));
    let replaced = Arc::new(std::sync::Barrier::new(2));
    install_private_lock_after_flock_barrier(path, Arc::clone(&acquired), Arc::clone(&replaced));
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        acquired.wait();
        std::fs::rename(replacement, path).unwrap();
        replaced.wait();
    })
}

fn assert_private_lock_descriptor(file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = file.metadata().unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(metadata.nlink(), 1);
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    let descriptor_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(descriptor_flags, -1);
    assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
}

struct ImmediateTestShutdown;

impl ShutdownSignal for ImmediateTestShutdown {
    fn prepare(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn wait(&self) -> std::io::Result<()> {
        Ok(())
    }
}

struct RecoveryOnlyReady<'a, 'b> {
    ready: &'a IpcReady<'b>,
    publishes: &'a Cell<u8>,
}

impl DaemonReady for RecoveryOnlyReady<'_, '_> {
    fn recover_stale_endpoint(&self) -> std::io::Result<()> {
        self.ready.recover_stale_endpoint()
    }

    fn publish(&self) -> std::io::Result<()> {
        self.publishes.set(self.publishes.get() + 1);
        Ok(())
    }

    fn quiesce(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn retire(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A generation authority that takes no authority at all.
///
/// The pre-registration recovery cases below never reach a bound endpoint, so
/// there is nothing for a real authority to claim; this keeps those cases
/// about the record and endpoint fence they are testing.
struct NoGenerationAuthority;

impl GenerationAuthority for NoGenerationAuthority {
    fn claim(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn release(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A [`ProcessIdentitySource`] that returns a fixed identity for any pid, so
/// `serve` tests can register a record without observing a real OS process.
struct FixedIdentitySource(&'static str);

impl ProcessIdentitySource for FixedIdentitySource {
    fn process_start_identity(&self, _pid: u32) -> std::io::Result<String> {
        Ok(self.0.to_string())
    }
}

#[test]
fn daemon_process_identity_observation_fences_pid_reuse_and_legacy_records() {
    let pid = std::process::id();
    let identity = ExactProcessControl.process_start_identity(pid).unwrap();
    assert!(!identity.is_empty());
    let exact = DaemonRecord::identified(pid, identity.clone());
    assert_eq!(
        ExactProcessControl.observe(&exact),
        DaemonProcessObservation::Exact
    );

    let mismatch = DaemonRecord::identified(pid, format!("{identity}-other"));
    assert_eq!(
        ExactProcessControl.observe(&mismatch),
        DaemonProcessObservation::IdentityMismatch
    );
    assert_eq!(
        ExactProcessControl.observe(&DaemonRecord::new(pid)),
        DaemonProcessObservation::Unknown
    );
    let absent = DaemonRecord::identified(2_000_000_000, "not-present");
    assert_eq!(
        ExactProcessControl.observe(&absent),
        DaemonProcessObservation::Gone
    );
}

#[test]
fn forged_same_uid_endpoint_cannot_echo_another_process_record() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let generation = ipc_generation();
    let listener = SecureUnixListener::bind(data, generation.clone()).unwrap();
    let mut recorded = std::process::Command::new("sleep")
        .arg("5")
        .spawn()
        .unwrap();
    let record = DaemonRecord::identified(
        recorded.id(),
        process_start_identity(recorded.id()).unwrap(),
    );
    let store = DaemonRecordStore::new(FsRecordFile {
        path: data.join("daemon/daemon.json"),
    });
    store.save(&record).unwrap();
    let protocol = usagi_daemon::presentation::ipc::server_protocol(
        generation,
        "forged".into(),
        current_build(),
        record,
        paths::wire_workspace_root(data),
    );
    let server = std::thread::spawn(move || {
        let mut stream = loop {
            match listener.accept() {
                Ok(stream) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::yield_now();
                }
                Err(error) => panic!("forged endpoint accept failed: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        let mut writer = stream.try_clone().unwrap();
        usagi_daemon::presentation::ipc::handshake(&mut stream, &mut writer, &protocol)
            .unwrap()
            .unwrap();
    });

    let clock = SystemClock::new();
    let error = connect_client(
        data,
        ClientPolicy::cli(),
        current_build(),
        ClientWorkspace::Bound {
            root: paths::wire_workspace_root(data),
        },
        |stream| deadline_transport(clock, stream, ClientPolicy::cli().timeout_ms),
    )
    .err()
    .expect("forged endpoint must be rejected");
    assert!(error.to_string().contains("endpoint owner"));
    server.join().unwrap();
    recorded.kill().unwrap();
    recorded.wait().unwrap();
}

#[test]
fn daemon_shutdown_signals_only_the_exact_child_incarnation() {
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let identity = ExactProcessControl
        .process_start_identity(child.id())
        .unwrap();
    let exact = DaemonRecord::identified(child.id(), identity.clone());
    let mismatch = DaemonRecord::identified(child.id(), format!("{identity}-reused"));

    let error = SigtermTerminator.terminate(&mismatch).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(child.try_wait().unwrap().is_none());

    SigtermTerminator.terminate(&exact).unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
}

#[test]
fn delayed_record_clear_cannot_remove_a_concurrent_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("daemon").join("daemon.json");
    let old_store = DaemonRecordStore::new(FsRecordFile { path: path.clone() });
    let old = usagi_core::domain::daemon::DaemonRecord::new(4242);
    let replacement = usagi_core::domain::daemon::DaemonRecord {
        pid: old.pid,
        process_start_identity: old.process_start_identity.clone(),
        started_at: old.started_at + chrono::Duration::nanoseconds(1),
    };
    old_store.save(&old).unwrap();
    let delayed_expected = old_store.load().unwrap().unwrap();

    let saved = Arc::new(std::sync::Barrier::new(2));
    let saved_by_replacement = Arc::clone(&saved);
    let replacement_for_thread = replacement.clone();
    let replacement_thread = std::thread::spawn(move || {
        let store = DaemonRecordStore::new(FsRecordFile { path });
        store.save(&replacement_for_thread).unwrap();
        saved_by_replacement.wait();
    });
    saved.wait();

    assert!(!old_store.clear_if(&delayed_expected).unwrap());
    assert_eq!(old_store.load().unwrap(), Some(replacement));
    replacement_thread.join().unwrap();
}

#[test]
fn failed_atomic_record_save_preserves_old_record_and_removes_temporary() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let daemon = directory.path().join("daemon");
    let path = daemon.join("daemon.json");
    let store = DaemonRecordStore::new(FsRecordFile { path: path.clone() });
    let old = usagi_core::domain::daemon::DaemonRecord::new(4242);
    let replacement = usagi_core::domain::daemon::DaemonRecord::new(4343);
    store.save(&old).unwrap();

    fail_record_write_before_rename(&path);
    assert!(store.save(&replacement).is_err());

    assert_eq!(store.load().unwrap(), Some(old));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        std::fs::read_dir(&daemon).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("daemon.json.tmp.")
        }),
        "failed save left a daemon record temporary behind"
    );
}

#[test]
fn lifecycle_private_files_override_a_restrictive_umask() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    const FIXTURE: &str = "USAGI_TEST_RESTRICTIVE_DAEMON_UMASK";
    if std::env::var_os(FIXTURE).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::daemon::tests::lifecycle_private_files_override_a_restrictive_umask",
                "--nocapture",
            ])
            .env(FIXTURE, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }

    // This branch runs in its own test subprocess, so changing the process
    // umask cannot perturb parallel tests or unrelated persistence stores.
    let directory = tempfile::Builder::new()
        .prefix("umask-")
        .tempdir_in("/tmp")
        .unwrap();
    let previous_umask = unsafe { libc::umask(0o777) };
    let data = directory.path().join("data");
    ensure_private_dir(&data).unwrap();
    let daemon = data.join("daemon");
    let first_bootstrap = acquire_bootstrap_lock(&data).unwrap();
    let bootstrap_metadata = first_bootstrap.metadata().unwrap();
    assert!(bootstrap_metadata.is_file());
    assert_eq!(bootstrap_metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(bootstrap_metadata.nlink(), 1);
    assert_eq!(bootstrap_metadata.mode() & 0o777, 0o600);
    let descriptor_flags = unsafe { libc::fcntl(first_bootstrap.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(descriptor_flags, -1);
    assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
    drop(first_bootstrap);
    // Reopening after the creating fd closes is the regression boundary:
    // the former code left a mode-000 node under umask 0777.
    let bootstrap = acquire_bootstrap_lock(&data).unwrap();
    let lifecycle = acquire_lifecycle_lock_io_within(&data, PrivateLockWait::LIFECYCLE).unwrap();
    let reopened_flags = unsafe { libc::fcntl(bootstrap.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(reopened_flags, -1);
    assert_ne!(reopened_flags & libc::FD_CLOEXEC, 0);
    let path = daemon.join("daemon.json");
    let store = DaemonRecordStore::new(FsRecordFile { path });
    store
        .save(&usagi_core::domain::daemon::DaemonRecord::new(4242))
        .unwrap();

    let instance = FileInstanceLock {
        path: daemon.join("daemon.lock"),
        held: RefCell::new(None),
    };
    assert!(instance.acquire().unwrap());
    let listener = SecureUnixListener::bind(
        &data,
        usagi_core::infrastructure::ipc::DaemonGeneration(
            usagi_core::domain::id::DaemonGeneration::new().as_str(),
        ),
    )
    .unwrap();

    for private_file in [
        "daemon.json",
        "daemon.lock",
        "record.lock",
        "bootstrap.lock",
        "lifecycle.lock",
        "current.json",
        "current.lock",
    ] {
        assert_eq!(
            std::fs::metadata(daemon.join(private_file))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "{private_file} did not override umask 0777"
        );
    }
    drop((listener, instance, lifecycle, bootstrap));
    unsafe {
        libc::umask(previous_umask);
    }
}

#[test]
fn all_lifecycle_locks_recover_a_crash_after_restrictive_umask_creation() {
    use std::os::unix::fs::PermissionsExt;

    const FIXTURE: &str = "USAGI_TEST_PRIVATE_LOCK_CREATE_CRASH";
    if std::env::var_os(FIXTURE).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::daemon::tests::all_lifecycle_locks_recover_a_crash_after_restrictive_umask_creation",
                "--nocapture",
            ])
            .env(FIXTURE, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }

    // Isolate the process-global umask, then stop each lock immediately
    // after create_new. The next API call must recover that durable mode-000
    // residue and leave the same exact private invariant on every fd.
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let record_daemon = directory.path().join("record-daemon");
    let instance_daemon = directory.path().join("instance-daemon");
    let bootstrap_data = directory.path().join("bootstrap-data");
    ensure_private_dir(&record_daemon).unwrap();
    ensure_private_dir(&instance_daemon).unwrap();
    ensure_private_dir(&bootstrap_data).unwrap();
    let previous_umask = unsafe { libc::umask(0o777) };

    let record_path = record_daemon.join("daemon.json");
    let record_lock = record_daemon.join("record.lock");
    let store = DaemonRecordStore::new(FsRecordFile { path: record_path });
    fail_private_lock_after_create(&record_lock);
    assert!(store.load().is_err());
    assert_eq!(
        std::fs::metadata(&record_lock)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0
    );
    assert_eq!(store.load().unwrap(), None);
    let record_descriptor = lock_private_exclusive(
        &record_lock,
        "daemon record lock",
        PrivateLockModePolicy::CrashResidue,
        PrivateLockWait::RECORD,
    )
    .unwrap();
    assert_private_lock_descriptor(&record_descriptor);
    drop(record_descriptor);

    let instance_path = instance_daemon.join("daemon.lock");
    let failed_instance = FileInstanceLock {
        path: instance_path.clone(),
        held: RefCell::new(None),
    };
    fail_private_lock_after_create(&instance_path);
    assert!(failed_instance.acquire().is_err());
    assert_eq!(
        std::fs::metadata(&instance_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0
    );
    let instance = FileInstanceLock {
        path: instance_path,
        held: RefCell::new(None),
    };
    assert!(instance.acquire().unwrap());
    assert_private_lock_descriptor(instance.held.borrow().as_ref().unwrap());

    let bootstrap_path = bootstrap_data.join("daemon/bootstrap.lock");
    fail_private_lock_after_create(&bootstrap_path);
    assert!(acquire_bootstrap_lock(&bootstrap_data).is_err());
    assert_eq!(
        std::fs::metadata(&bootstrap_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0
    );
    let bootstrap = acquire_bootstrap_lock(&bootstrap_data).unwrap();
    assert_private_lock_descriptor(&bootstrap);

    let lifecycle_path = bootstrap_data.join("daemon/lifecycle.lock");
    fail_private_lock_after_create(&lifecycle_path);
    assert!(acquire_lifecycle_lock_io_within(&bootstrap_data, PrivateLockWait::LIFECYCLE).is_err());
    assert_eq!(
        std::fs::metadata(&lifecycle_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0
    );
    let lifecycle =
        acquire_lifecycle_lock_io_within(&bootstrap_data, PrivateLockWait::LIFECYCLE).unwrap();
    assert_private_lock_descriptor(&lifecycle);

    drop((lifecycle, bootstrap, instance));
    unsafe {
        libc::umask(previous_umask);
    }
}

/// The bootstrap section is bounded, and its contention is a distinct answer.
///
/// The section is entered on a machine-wide data directory by every surface,
/// including the TUI's render thread, and it is held across one
/// `connect_or_start` — a cold start, in the worst case. A blocking `flock`
/// there means any other usagi process (MCP server, CLI, rollover), or a
/// holder that was killed while wedged, stalls the UI without limit. So a
/// holder that outlasts the wait yields `BootstrapContended`, which tells the
/// surface to retry rather than that the daemon is absent.
#[test]
fn a_contended_bootstrap_section_returns_bounded_typed_contention() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data_dir = directory.path().join("data");
    // Enter and leave once, so the uncontended path is the one that creates
    // the lock node and the `daemon/` directory chain.
    drop(acquire_bootstrap_lock(&data_dir).unwrap());

    // A second open file description on the same node: `flock` conflicts
    // across descriptions, so this is exactly what another process holding
    // the section looks like.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(data_dir.join("daemon").join("bootstrap.lock"))
        .unwrap();
    FileExt::lock_exclusive(&held).unwrap();

    let wait = PrivateLockWait {
        limit: Duration::from_millis(120),
        poll: Duration::from_millis(10),
    };
    let started = Instant::now();
    let error = acquire_bootstrap_lock_within(&data_dir, wait)
        .expect_err("a held bootstrap section must not be entered");
    let elapsed = started.elapsed();

    assert_eq!(error, ClientError::BootstrapContended);
    assert_eq!(
        error.side_effect(),
        usagi_core::infrastructure::ipc::SideEffect::None
    );
    assert!(
        elapsed >= wait.limit,
        "the wait was actually spent: {elapsed:?}"
    );
    assert!(
        elapsed < wait.limit * 10,
        "the wait is bounded, not blocking: {elapsed:?}"
    );

    // Once the holder leaves, the same section is entered normally.
    FileExt::unlock(&held).unwrap();
    drop(held);
    let entered = acquire_bootstrap_lock_within(&data_dir, wait).unwrap();
    assert_private_lock_descriptor(&entered);
}

/// The wait must outlast one honest cold start, or a client that legitimately
/// waits for a peer's `daemon start` would report contention instead of using
/// the daemon that peer is about to publish.
#[test]
fn the_bootstrap_wait_outlasts_one_cold_start() {
    assert!(PrivateLockWait::BOOTSTRAP.limit > bootstrap::READINESS_CEILING);
    assert!(PrivateLockWait::BOOTSTRAP.poll < PrivateLockWait::BOOTSTRAP.limit);
    assert!(PrivateLockWait::RECORD.limit < PrivateLockWait::BOOTSTRAP.limit);
    assert!(PrivateLockWait::LIFECYCLE.limit > ROLLOVER_STARTUP_WINDOW * 2);
    assert!(PrivateLockWait::LIFECYCLE.poll < PrivateLockWait::LIFECYCLE.limit);
}

/// Every daemon socket this composition root builds carries an armed
/// end-to-end deadline, so no surface can be handed an unbounded stream: the
/// only client type is [`LaneClient`], and its transport fails closed once
/// the budget is spent.
#[test]
fn the_lane_transport_bounds_reads_and_writes_by_construction() {
    let (client_socket, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut lane = deadline_transport(SystemClock::new(), client_socket, 40);

    // The peer is alive and simply never answers, which is the shape a hung
    // daemon has: without the armed deadline this read would never return.
    let started = Instant::now();
    let mut byte = [0_u8; 1];
    let error = lane.read(&mut byte).unwrap_err();
    let elapsed = started.elapsed();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ));
    assert!(elapsed < Duration::from_secs(2), "bounded: {elapsed:?}");

    // The budget is spent, so the next call fails without touching the OS;
    // re-arming is what gives the next request its own budget.
    assert_eq!(
        lane.read(&mut byte).unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    usagi_core::infrastructure::client::RearmableStream::rearm(&mut lane, 40);
    assert!(lane.write(b"x").is_ok());
    drop(peer);
}

#[test]
fn record_lock_rejects_a_path_replacement_after_flock() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let daemon = directory.path().join("daemon");
    ensure_private_dir(&daemon).unwrap();
    let record_lock = daemon.join("record.lock");
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    let replacement = replace_private_lock_after_flock(&record_lock);

    let error = store.load().unwrap_err();
    replacement.join().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("daemon record lock"));
    assert_eq!(store.load().unwrap(), None);
}

#[test]
fn instance_lock_rejects_a_path_replacement_after_flock() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let daemon = directory.path().join("daemon");
    ensure_private_dir(&daemon).unwrap();
    let path = daemon.join("daemon.lock");
    let instance = FileInstanceLock {
        path: path.clone(),
        held: RefCell::new(None),
    };
    let replacement = replace_private_lock_after_flock(&path);

    let error = instance.acquire().unwrap_err();
    replacement.join().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("daemon instance lock"));
    assert!(instance.held.borrow().is_none());
    let retry = FileInstanceLock {
        path,
        held: RefCell::new(None),
    };
    assert!(retry.acquire().unwrap());
}

/// Build a fence for `workspace` as `pid` would see it.
fn workspace_fence(workspace: &Path, pid: u32) -> FileWorkspaceFence {
    let workspace = paths::canonical_workspace_root(workspace).unwrap();
    FileWorkspaceFence {
        path: paths::workspace_fence_path(&workspace),
        workspace,
        pid,
        patience: WORKSPACE_FENCE_PATIENCE,
        held: RefCell::new(None),
    }
}

#[test]
fn workspace_fence_refuses_a_second_owner_and_names_its_pid() {
    let workspace = tempfile::tempdir_in("/tmp").unwrap();
    let owner = workspace_fence(workspace.path(), 4242);
    assert_eq!(owner.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);

    // A second daemon over the same workspace is refused and can name the
    // live owner, which is the only cross-data-directory discovery it has.
    let second = workspace_fence(workspace.path(), 5252);
    assert_eq!(
        second.acquire().unwrap(),
        WorkspaceFenceOutcome::Held {
            workspace: paths::canonical_workspace_root(workspace.path())
                .unwrap()
                .display()
                .to_string(),
            owner: Some(4242),
        }
    );
    assert!(second.held.borrow().is_none());

    // The fence node lives in a daemon-private directory beside — not inside
    // — the runtime-mode children, and the OS releases it with the owner.
    assert_eq!(
        owner.path,
        paths::canonical_workspace_root(workspace.path())
            .unwrap()
            .join(".usagi/daemon/daemon.lock")
    );
    drop(owner);
    let third = workspace_fence(workspace.path(), 6262);
    assert_eq!(third.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);
}

#[test]
fn a_home_workspace_reuses_its_fence_as_the_instance_lock() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    ensure_private_dir_all(&home.path().join(".usagi/daemon")).unwrap();
    let workspace = workspace_fence(home.path(), 4242);
    let instance_path = home.path().join(".usagi/daemon/daemon.lock");
    assert!(lock_paths_alias(&instance_path, &instance_path));
    let instance = process_instance_lock(instance_path.clone(), &workspace);

    assert!(matches!(
        &instance,
        ProcessInstanceLock::WorkspaceAlias { .. }
    ));
    assert_eq!(
        instance.acquire().unwrap_err().to_string(),
        "daemon workspace fence must be acquired before its aliased instance lock"
    );
    assert_eq!(
        workspace.acquire().unwrap(),
        WorkspaceFenceOutcome::Acquired
    );
    assert!(instance.acquire().unwrap());
    assert!(instance.locked_inode().is_some());

    let alias = home.path().join("daemon-alias");
    std::os::unix::fs::symlink(home.path().join(".usagi/daemon"), &alias).unwrap();
    assert!(lock_paths_alias(&alias.join("daemon.lock"), &instance_path));
    let replaced_alias = process_instance_lock(alias.join("daemon.lock"), &workspace);
    assert!(matches!(
        &replaced_alias,
        ProcessInstanceLock::WorkspaceAlias { .. }
    ));
    std::fs::remove_file(&alias).unwrap();
    assert!(replaced_alias.acquire().is_err());
    assert!(!lock_paths_alias(
        &home.path().join(".usagi/daemon/other.lock"),
        &instance_path
    ));
    assert!(!lock_paths_alias(
        &home.path().join("missing/daemon.lock"),
        &instance_path
    ));

    let independent_path = home.path().join(".usagi/local/daemon/daemon.lock");
    ensure_private_dir_all(independent_path.parent().unwrap()).unwrap();
    let independent = process_instance_lock(independent_path, &workspace);
    assert!(matches!(&independent, ProcessInstanceLock::Independent(_)));
    assert!(independent.acquire().unwrap());
    assert!(independent.locked_inode().is_some());
    if let ProcessInstanceLock::Independent(lock) = &independent {
        assert!(InstanceLockCustody::locked_inode(lock).is_some());
    }

    // The shared descriptor still excludes another process description; it
    // only prevents this process from contending with itself.
    let mut second = workspace_fence(home.path(), 5252);
    second.patience = Duration::ZERO;
    assert_eq!(
        second.acquire().unwrap(),
        WorkspaceFenceOutcome::Held {
            workspace: paths::canonical_workspace_root(home.path())
                .unwrap()
                .display()
                .to_string(),
            owner: Some(4242),
        }
    );
}

#[test]
fn workspace_fence_narrows_the_exact_legacy_owner_mode() {
    use std::os::unix::fs::PermissionsExt;

    let workspace = tempfile::tempdir_in("/tmp").unwrap();
    let fence = workspace_fence(workspace.path(), 4242);
    std::fs::create_dir_all(fence.workspace.join(paths::STATE_DIR)).unwrap();
    ensure_private_dir(fence.path.parent().unwrap()).unwrap();
    std::fs::write(&fence.path, []).unwrap();
    std::fs::set_permissions(&fence.path, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(fence.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);
    assert_eq!(
        std::fs::metadata(&fence.path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn workspace_fence_refuses_through_a_symlinked_or_relative_spelling() {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let owner = workspace_fence(&workspace, 4242);
    assert_eq!(owner.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);

    let link = root.path().join("link");
    std::os::unix::fs::symlink(&workspace, &link).unwrap();
    for spelling in [
        link,
        workspace.join("."),
        workspace.join("..").join("workspace"),
    ] {
        let refused = workspace_fence(&spelling, 5252);
        assert!(
            matches!(
                refused.acquire().unwrap(),
                WorkspaceFenceOutcome::Held {
                    owner: Some(4242),
                    ..
                }
            ),
            "{} escaped the workspace fence",
            spelling.display()
        );
    }
}

#[test]
fn workspace_fence_refuses_when_the_owner_hint_is_unreadable() {
    let workspace = tempfile::tempdir_in("/tmp").unwrap();
    let owner = workspace_fence(workspace.path(), 4242);
    assert_eq!(owner.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);

    // A holder killed between `flock` and publishing its hint leaves an empty
    // node. The refusal must stand; only the diagnostic pid is lost.
    std::fs::write(&owner.path, "").unwrap();
    let refused = workspace_fence(workspace.path(), 5252);
    assert_eq!(
        refused.acquire().unwrap(),
        WorkspaceFenceOutcome::Held {
            workspace: paths::canonical_workspace_root(workspace.path())
                .unwrap()
                .display()
                .to_string(),
            owner: None,
        }
    );

    // So does a garbled or over-long line.
    std::fs::write(&owner.path, "x".repeat(128)).unwrap();
    assert!(matches!(
        workspace_fence(workspace.path(), 5252).acquire().unwrap(),
        WorkspaceFenceOutcome::Held { owner: None, .. }
    ));
}

#[test]
fn workspace_fence_rejects_a_path_replacement_after_flock() {
    let workspace = tempfile::tempdir_in("/tmp").unwrap();
    let fence = workspace_fence(workspace.path(), 4242);
    // Create the parent chain first: the replacement thread races the
    // pathname, not the directory setup.
    std::fs::create_dir_all(fence.workspace.join(paths::STATE_DIR)).unwrap();
    ensure_private_dir(fence.path.parent().unwrap()).unwrap();
    let replacement = replace_private_lock_after_flock(&fence.path);

    let error = fence.acquire().unwrap_err();
    replacement.join().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(error.to_string().contains("daemon workspace fence"));
    assert!(fence.held.borrow().is_none());
}

#[test]
fn bound_workspace_root_canonicalizes_and_fails_on_an_unresolvable_root() {
    let workspace = tempfile::tempdir_in("/tmp").unwrap();
    let daemon = workspace.path().join("data/daemon");
    ensure_private_dir_all(&daemon).unwrap();

    // With no durable state the bound root is the (canonicalized) startup
    // directory, which is what the session runtime would adopt.
    assert_eq!(
        bound_workspace_root(&daemon, &workspace.path().join(".")).unwrap(),
        paths::canonical_workspace_root(workspace.path()).unwrap()
    );
    assert_eq!(
        standby_workspace_state_dir(&daemon, workspace.path())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound,
        "a standby never initializes the first durable workspace"
    );

    // A startup directory that no longer resolves is a startup failure, not a
    // fence that silently keys some other path.
    let error = bound_workspace_root(&daemon, &workspace.path().join("absent")).unwrap_err();
    assert!(error.to_string().contains("workspace root"), "{error}");

    // Unreadable durable state fails the same way, rather than falling back
    // to a candidate the runtime would not adopt. Here the unreadable
    // document is the workspace's own, inside its state subtree.
    let canonical = paths::canonical_workspace_root(workspace.path()).unwrap();
    let state_dir = workspace_state::resolve(&daemon, &canonical)
        .unwrap()
        .dir()
        .to_path_buf();
    std::fs::write(state_dir.join("sessions.json"), "not json").unwrap();
    assert!(
        bound_workspace_root(&daemon, workspace.path())
            .unwrap_err()
            .to_string()
            .contains("Storage")
    );
}

/// A CLI or MCP client is as entitled to open a workspace as the TUI is.
/// Refusing here is what forced an operator to open every new repository in
/// the TUI once before their CLI would work in it.
#[test]
fn a_bound_client_adopts_the_repository_it_is_running_inside() {
    use usagi_core::infrastructure::ipc::WorkspaceResolver;

    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let data = temporary.path().join("data");
    let held = temporary.path().join("held");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&held).unwrap();
    let daemon_dir = data.join("daemon");
    let held_root = paths::canonical_workspace_root(&held).unwrap();
    let tenants = Arc::new(TenantRegistry::new(
        daemon_dir.clone(),
        FileWorkspaceFences {
            pid: std::process::id(),
        },
        SystemTenantOpener {
            data_home: data,
            generation: usagi_core::domain::id::DaemonGeneration::new(),
        },
        DEFAULT_TENANT_LIMIT,
    ));
    tenants.adopt_initial(&held_root).unwrap();
    let resolver = TenantWorkspaces {
        tenants: Arc::clone(&tenants),
        daemon_dir,
        initial: held_root.clone(),
    };
    let wire = |root: &Path| paths::wire_workspace_root(root);

    // Standing in a plain directory opens nothing.
    let outside = tempfile::tempdir_in("/tmp").unwrap();
    let refusal = resolver
        .resolve(Some(&ClientWorkspace::Bound {
            root: wire(&paths::canonical_workspace_root(outside.path()).unwrap()),
        }))
        .unwrap_err();
    assert!(usagi_core::infrastructure::ipc::is_workspace_mismatch(
        &refusal
    ));
    assert!(
        refusal.message.contains("run this from a repository root"),
        "the refusal gives the caller no next step: {refusal:?}"
    );
    assert!(
        refusal.message.contains("usagi open"),
        "the refusal omits the way to open a directory that is not a repository: {refusal:?}"
    );
    // The refusal names what this daemon really holds, not the root it just
    // refused — naming that one is what made the message contradict itself.
    assert!(refusal.message.contains(&wire(&held_root)), "{refusal:?}");
    assert_eq!(tenants.adopted().len(), 1);

    // Standing *at* a repository this daemon has never seen opens it. That
    // is the whole of what a bound declaration may open.
    let project = temporary.path().join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(project.join("crates/core")).unwrap();
    std::fs::create_dir_all(project.join(".usagi/sessions/worker/.git")).unwrap();
    let project_root = paths::canonical_workspace_root(&project).unwrap();

    // Below it, nothing is opened. A dotfiles repository at `$HOME` is an
    // ordinary setup, so searching upwards would let `usagi session create`
    // in any plain directory under it fence `$HOME` and open a branch in the
    // caller's dotfiles. Standing at a repository says which workspace is
    // meant; standing anywhere underneath one does not.
    for below in [
        project_root.join("crates/core"),
        project_root.join(".usagi/sessions/worker"),
    ] {
        let refusal = resolver
            .resolve(Some(&ClientWorkspace::Bound { root: wire(&below) }))
            .unwrap_err();
        assert!(
            usagi_core::infrastructure::ipc::is_workspace_mismatch(&refusal),
            "{below:?} opened a workspace from below its root"
        );
    }
    assert_eq!(tenants.adopted().len(), 1);

    assert_eq!(
        resolver
            .resolve(Some(&ClientWorkspace::Bound {
                root: wire(&project_root),
            }))
            .unwrap(),
        wire(&project_root)
    );
    assert_eq!(tenants.adopted().len(), 2);

    // Once it is open, everything below it resolves to it again — that is
    // ancestor matching, which this narrowing does not touch.
    for below in [
        project_root.join("crates/core"),
        project_root.join(".usagi/sessions/worker"),
    ] {
        assert_eq!(
            resolver
                .resolve(Some(&ClientWorkspace::Bound { root: wire(&below) }))
                .unwrap(),
            wire(&project_root),
            "{below:?} did not resolve to the workspace that owns it"
        );
    }
    assert_eq!(tenants.adopted().len(), 2);
}

/// The real activity observer over a fixture data directory.
fn daemon_activity(
    data: &Path,
    root: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    tenants: &Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
) -> DaemonWorkspaceActivity {
    let children = Arc::new(SpawnedChildren::default());
    let metrics = Arc::new(TerminalPipelineMetrics::default());
    DaemonWorkspaceActivity {
        terminal: new_terminal_runtime(
            data,
            generation,
            root.to_path_buf(),
            DaemonPty::new(
                Arc::clone(&metrics),
                Arc::clone(&children),
                Arc::new(ShutdownRequest::new()),
            )
            .0,
            Arc::clone(tenants) as Workspaces,
            Arc::new(UserEnvironment::new(data.to_path_buf(), OpCli)),
            usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention::new(),
            &children,
            false,
            GENERIC_TERMINAL_LIMIT,
        )
        .unwrap(),
        agent: open_agent_runtime(
            data,
            generation,
            Arc::clone(tenants) as Workspaces,
            AgentPty::new(
                terminal_environment(),
                metrics,
                Arc::clone(&children),
                Arc::new(ShutdownRequest::new()),
            )
            .0,
            std::env::current_exe().unwrap(),
            Arc::new(UserEnvironment::new(data.to_path_buf(), OpCli)),
            usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention::new(),
            AgentConcurrencyGauge::default(),
            &children,
            RuntimeHydration::Empty,
            GENERIC_TERMINAL_LIMIT,
        )
        .unwrap(),
        supervisor: Arc::new(Mutex::new(SupervisorRuntime::new(&data.join("daemon")))),
    }
}

/// The daemon-wide registries are keyed by session alone, so what they may
/// keep is every session this *data directory* knows — not the sessions of
/// the workspaces held right now. A workspace given back by retirement still
/// owns its sessions, and pruning against a set that lost them would delete
/// the user's own PR records for a workspace that is merely closed.
#[test]
fn a_closed_workspace_still_counts_as_owning_its_sessions() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let data = temporary.path().join("data");
    let workspace = temporary.path().join("workspace");
    for directory in [&data, &workspace] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let daemon_dir = data.join("daemon");
    ensure_private_dir_all(&daemon_dir).unwrap();
    let root = paths::canonical_workspace_root(&workspace).unwrap();
    let generation = usagi_core::domain::id::DaemonGeneration::new();
    let tenants = Arc::new(TenantRegistry::new(
        daemon_dir.clone(),
        FileWorkspaceFences {
            pid: std::process::id(),
        },
        SystemTenantOpener {
            data_home: data,
            generation,
        },
        DEFAULT_TENANT_LIMIT,
    ));

    // Nothing opened yet: no session is known, and the empty answer is a
    // fact rather than a read failure.
    assert_eq!(
        known_sessions(&daemon_dir),
        Some(std::collections::BTreeSet::new())
    );

    let tenant = tenants.adopt_initial(&root).unwrap();
    let session = {
        let mut runtime = tenant.runtime().lock().unwrap();
        let created = runtime
            .handle(
                usagi_core::infrastructure::ipc::SessionAction::Create,
                &usagi_core::domain::id::OperationId::new().to_string(),
                &serde_json::json!({"name": "kept"}),
            )
            .unwrap();
        serde_json::from_value::<SessionId>(created.body["sessions"][0]["session_id"].clone())
            .unwrap()
    };
    assert!(known_sessions(&daemon_dir).unwrap().contains(&session));

    // Giving the workspace back does not un-own its sessions: the lifecycle
    // document is still there, and it is the authority.
    drop(tenant);
    assert!(tenants.retire(&root));
    assert!(tenants.adopted().is_empty());
    assert!(known_sessions(&daemon_dir).unwrap().contains(&session));

    // A subtree that cannot be read is not "no sessions": pruning on a
    // partial view is exactly the deletion this guards against.
    std::fs::write(
        daemon_dir
            .join(paths::WORKSPACE_STATE_DIR)
            .join(paths::workspace_state_digest(&root))
            .join("sessions.json"),
        "not json",
    )
    .unwrap();
    assert_eq!(known_sessions(&daemon_dir), None);
}

/// A workspace with nothing left to do is given back, and one with work is
/// not. The observation fails closed on every side: a runtime that cannot be
/// read keeps its workspace, because keeping one costs a fence while
/// releasing a working one hands its worktrees to a second owner.
#[test]
fn an_idle_workspace_is_released_and_a_working_one_is_kept() {
    use usagi_daemon::usecase::tenant::WorkspaceActivity;

    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let data = temporary.path().join("data");
    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    for directory in [&data, &first, &second] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let daemon_dir = data.join("daemon");
    ensure_private_dir_all(&daemon_dir).unwrap();
    let first_root = paths::canonical_workspace_root(&first).unwrap();
    let second_root = paths::canonical_workspace_root(&second).unwrap();
    let generation = usagi_core::domain::id::DaemonGeneration::new();
    let tenants = Arc::new(TenantRegistry::new(
        daemon_dir,
        FileWorkspaceFences {
            pid: std::process::id(),
        },
        SystemTenantOpener {
            data_home: data.clone(),
            generation,
        },
        DEFAULT_TENANT_LIMIT,
    ));
    let initial = tenants.adopt_initial(&first_root).unwrap();
    let adopted = tenants.adopt(&second_root).unwrap();

    // A fresh workspace has no runtime and no unfinished lifecycle work, so
    // the real observer reports it idle; a session mid-creation does not.
    let activity = daemon_activity(&data, &first_root, generation, &tenants);
    assert!(!activity.has_work(adopted.workspace_id(), adopted.runtime()));
    let goal_operation = usagi_core::domain::id::OperationId::new().to_string();
    activity
        .supervisor
        .lock()
        .unwrap()
        .reserve_goal_for_workspace(
            "goal",
            adopted.workspace_id(),
            &goal_operation,
            usagi_daemon::usecase::supervisor_runtime::GoalSpecification::new(
                "finish".into(),
                usagi_core::domain::pr_inventory::GitHubRepository::from_name_with_owner(
                    "acme/repo",
                )
                .unwrap(),
            ),
            None,
            chrono::Utc::now(),
        )
        .unwrap();
    assert!(activity.has_work(adopted.workspace_id(), adopted.runtime()));
    activity
        .supervisor
        .lock()
        .unwrap()
        .fail_reserved_goal(
            &goal_operation,
            "fixture complete".into(),
            chrono::Utc::now(),
        )
        .unwrap();
    assert!(!activity.has_work(adopted.workspace_id(), adopted.runtime()));

    // A handle held outside the registry keeps the workspace whatever the
    // observation says, so the sweep only sees it once the handle is gone.
    let now = chrono::Utc::now();
    let idle_for = chrono::Duration::zero();
    assert!(tenants.retire_idle(&activity, now, idle_for).is_empty());
    drop(adopted);

    // The worker gives it back and leaves the startup workspace alone.
    let shutdown = Arc::new(ShutdownRequest::new());
    spawn_tenant_retire_worker(
        Arc::clone(&tenants),
        activity,
        Arc::clone(&shutdown),
        Duration::from_millis(5),
        Duration::ZERO,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while tenants.adopted().len() > 1 {
        assert!(
            Instant::now() < deadline,
            "the idle workspace was not released"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        tenants
            .adopted()
            .iter()
            .map(|tenant| tenant.root().to_path_buf())
            .collect::<Vec<_>>(),
        vec![initial.root().to_path_buf()]
    );
    shutdown.request();
}

/// An observation that cannot be made keeps the workspace.
#[test]
fn an_unreadable_runtime_keeps_its_workspace() {
    use usagi_daemon::usecase::tenant::WorkspaceActivity;

    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let data = temporary.path().join("data");
    let workspace = temporary.path().join("workspace");
    for directory in [&data, &workspace] {
        std::fs::create_dir_all(directory).unwrap();
    }
    ensure_private_dir_all(&data.join("daemon")).unwrap();
    let generation = usagi_core::domain::id::DaemonGeneration::new();
    let root = paths::canonical_workspace_root(&workspace).unwrap();
    let tenants = Arc::new(TenantRegistry::new(
        data.join("daemon"),
        FileWorkspaceFences {
            pid: std::process::id(),
        },
        SystemTenantOpener {
            data_home: data.clone(),
            generation,
        },
        DEFAULT_TENANT_LIMIT,
    ));
    let tenant = tenants.adopt_initial(&root).unwrap();
    let activity = daemon_activity(&data, &root, generation, &tenants);
    assert!(!activity.has_work(tenant.workspace_id(), tenant.runtime()));

    // A lifecycle runtime whose lock is poisoned cannot be read, so the
    // workspace is kept rather than released on an unknown state.
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = tenant.runtime().lock().unwrap();
        panic!("a reader panicked while holding the lifecycle runtime");
    }));
    assert!(poisoned.is_err());
    assert!(activity.has_work(tenant.workspace_id(), tenant.runtime()));
}

#[test]
#[allow(clippy::too_many_lines)] // Every declaration and refusal in one flow.
fn the_handshake_resolves_a_selected_workspace_by_adopting_it() {
    use usagi_core::infrastructure::ipc::WorkspaceResolver;

    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let data = temporary.path().join("data");
    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    for directory in [&data, &first, &second] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let daemon_dir = data.join("daemon");
    let first_root = paths::canonical_workspace_root(&first).unwrap();
    let second_root = paths::canonical_workspace_root(&second).unwrap();
    let generation = usagi_core::domain::id::DaemonGeneration::new();
    let tenants = Arc::new(TenantRegistry::new(
        daemon_dir.clone(),
        FileWorkspaceFences {
            pid: std::process::id(),
        },
        SystemTenantOpener {
            data_home: data,
            generation,
        },
        DEFAULT_TENANT_LIMIT,
    ));
    let initial = tenants.adopt_initial(&first_root).unwrap();
    let workspaces: Workspaces = tenants.clone();
    let resolver = TenantWorkspaces {
        tenants: Arc::clone(&tenants),
        daemon_dir,
        initial: first_root.clone(),
    };
    let wire = |root: &Path| paths::wire_workspace_root(root);

    // A client that names no workspace is answered with the one this process
    // started in: it reads no workspace state either way.
    for declared in [None, Some(ClientWorkspace::Unbound)] {
        assert_eq!(
            resolver.resolve(declared.as_ref()).unwrap(),
            wire(&first_root)
        );
    }

    // Selecting a workspace this daemon has never seen adopts it, and the
    // second selection is the same tenant rather than a second adoption.
    let selected = ClientWorkspace::Selected {
        root: wire(&second_root),
    };
    assert_eq!(
        resolver.resolve(Some(&selected)).unwrap(),
        wire(&second_root)
    );
    assert_eq!(
        resolver.resolve(Some(&selected)).unwrap(),
        wire(&second_root)
    );
    assert_eq!(tenants.adopted().len(), 2);

    // A bound client resolves to the workspace containing it, including from
    // a path that no longer exists — a worktree its own teardown removed.
    for candidate in [
        second_root.clone(),
        second_root.join(".usagi/sessions/gone"),
    ] {
        let bound = ClientWorkspace::Bound {
            root: wire(&candidate),
        };
        assert_eq!(resolver.resolve(Some(&bound)).unwrap(), wire(&second_root));
    }

    // A selected root that does not resolve on this machine is refused, and
    // nothing is adopted for it.
    let refusal = resolver
        .resolve(Some(&ClientWorkspace::Selected {
            root: wire(&temporary.path().join("absent")),
        }))
        .unwrap_err();
    assert!(usagi_core::infrastructure::ipc::is_workspace_mismatch(
        &refusal
    ));
    assert_eq!(tenants.adopted().len(), 2);

    // A workspace this data directory has opened before keeps answering for
    // the clients inside it, even once it has been given back: its state
    // subtree records the root, so the resolution adopts it again. Without
    // this, a workspace that idled out of tenancy would refuse the very CLI
    // and MCP clients running in it.
    assert!(tenants.retire(&second_root));
    let inside = ClientWorkspace::Bound {
        root: wire(&second_root.join("nested")),
    };
    assert_eq!(resolver.resolve(Some(&inside)).unwrap(), wire(&second_root));
    assert!(
        tenants.tenant(&second_root).is_some(),
        "resolving a known workspace adopts it again"
    );

    // The connection binds the workspace its handshake settled on.
    for (declared, expected) in [
        (None, first_root.clone()),
        (Some(ClientWorkspace::Unbound), first_root),
        (Some(selected.clone()), second_root.clone()),
        (
            Some(ClientWorkspace::Bound {
                root: wire(&second_root.join("nested")),
            }),
            second_root.clone(),
        ),
    ] {
        let bound = connection_workspace(&workspaces, &initial, declared.as_ref())
            .expect("the handshake resolved this workspace");
        assert_eq!(bound.tenant.root(), expected);
    }

    // A workspace retired between the handshake and the lookup closes the
    // connection instead of serving another workspace's state.
    assert!(tenants.retire(&second_root));
    assert!(connection_workspace(&workspaces, &initial, Some(&selected)).is_none());
}

#[test]
fn the_fence_factory_owns_one_workspace_per_root() {
    let first = tempfile::tempdir_in("/tmp").unwrap();
    let second = tempfile::tempdir_in("/tmp").unwrap();
    let fences = FileWorkspaceFences { pid: 4242 };

    // Each root gets its own fence node, so owning one workspace never
    // implies owning another.
    let held = fences.fence_for(first.path());
    assert_eq!(held.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);
    assert_eq!(
        fences.fence_for(second.path()).acquire().unwrap(),
        WorkspaceFenceOutcome::Acquired
    );

    // A second owner of the same root is refused and names the holder, which
    // is what lets one workspace be refused without disturbing the rest.
    let contender = std::thread::spawn({
        let root = first.path().to_path_buf();
        move || FileWorkspaceFences { pid: 5252 }.fence_for(&root).acquire()
    })
    .join()
    .unwrap()
    .unwrap();
    assert_eq!(
        contender,
        WorkspaceFenceOutcome::Held {
            workspace: first.path().display().to_string(),
            owner: Some(4242),
        }
    );
}

#[test]
fn workspace_state_resolution_reports_every_failure_it_can_meet() {
    let workspace = tempfile::tempdir_in("/tmp").unwrap();
    let canonical = paths::canonical_workspace_root(workspace.path()).unwrap();
    let daemon = workspace.path().join("data/daemon");
    ensure_private_dir_all(&daemon).unwrap();

    // A legacy document that cannot be parsed names the workspace this
    // daemon would otherwise adopt, so the start fails instead of adopting
    // the startup directory in its place.
    let legacy = daemon.join("sessions.json");
    std::fs::write(&legacy, "not json").unwrap();
    let error = bound_workspace_root(&daemon, workspace.path()).unwrap_err();
    assert!(error.to_string().contains("sessions.json"), "{error}");
    std::fs::remove_file(&legacy).unwrap();

    // A container that cannot be enumerated is reported rather than read as
    // "no workspace has been adopted", which would adopt a second subtree
    // for a workspace that already owns one.
    let container = daemon.join(paths::WORKSPACE_STATE_DIR);
    std::fs::write(&container, "").unwrap();
    for error in [
        bound_workspace_root(&daemon, workspace.path()).unwrap_err(),
        standby_workspace_state_dir(&daemon, &canonical).unwrap_err(),
    ] {
        assert!(error.to_string().contains("could not"), "{error}");
    }
}

#[test]
fn bound_workspace_root_migrates_a_legacy_document_and_prefers_the_adopted_owner() {
    let workspace = tempfile::tempdir_in("/tmp").unwrap();
    let canonical = paths::canonical_workspace_root(workspace.path()).unwrap();
    let daemon = workspace.path().join("data/daemon");
    ensure_private_dir_all(&daemon).unwrap();

    // A data directory written before workspace subtrees existed keeps its
    // lifecycle document beside the locator. The first resolution moves it
    // into the subtree of the workspace it names, and binds that workspace.
    let legacy = daemon.join("sessions.json");
    std::fs::write(
        &legacy,
        format!(
            r#"{{"repository_root":{:?},"state":{{"format":"usagi-workspace-lifecycle","version":{{"major":2,"minor":0}},"workspace_id":"543166c9-3923-4086-b3c9-05a69a66550c","state_revision":0,"sessions":[],"operations":[],"updated_at":"2026-08-20T23:22:45.487133Z"}}}}"#,
            canonical.to_str().unwrap()
        ),
    )
    .unwrap();

    let subdirectory = workspace.path().join("nested/deeper");
    std::fs::create_dir_all(&subdirectory).unwrap();
    assert_eq!(
        bound_workspace_root(&daemon, &subdirectory).unwrap(),
        canonical
    );
    assert!(!legacy.exists());
    let state_dir = workspace_state::resolve(&daemon, &canonical)
        .unwrap()
        .dir()
        .to_path_buf();
    assert!(state_dir.join("sessions.json").is_file());

    // A subdirectory of an adopted workspace resolves to the workspace, so a
    // daemon started there fences what it will actually own rather than
    // adopting the subdirectory as a second workspace.
    assert_eq!(
        standby_workspace_state_dir(&daemon, &canonical).unwrap(),
        state_dir
    );
    let unadopted = tempfile::tempdir_in("/tmp").unwrap();
    assert_eq!(
        standby_workspace_state_dir(
            &daemon,
            &paths::canonical_workspace_root(unadopted.path()).unwrap(),
        )
        .unwrap(),
        state_dir,
        "a machine-wide restart falls back to durable state instead of its cwd"
    );
}

#[test]
fn standby_workspace_selection_skips_partial_adoptions_and_rejects_corruption() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let daemon = temporary.path().join("data/daemon");
    ensure_private_dir_all(&daemon).unwrap();

    // `resolve` publishes root.json before the tenant opener initializes
    // sessions.json. A failed open can leave exactly this partial subtree,
    // and it must not shadow an initialized workspace during replacement.
    let partial_root = temporary.path().join("a-partial");
    let initialized_root = temporary.path().join("z-initialized");
    std::fs::create_dir_all(&partial_root).unwrap();
    std::fs::create_dir_all(&initialized_root).unwrap();
    let partial_root = paths::canonical_workspace_root(&partial_root).unwrap();
    let initialized_root = paths::canonical_workspace_root(&initialized_root).unwrap();
    workspace_state::resolve(&daemon, &partial_root).unwrap();
    let initialized = workspace_state::resolve(&daemon, &initialized_root).unwrap();
    drop(
        open_session_runtime(
            initialized_root,
            initialized.dir(),
            temporary.path(),
            usagi_core::domain::id::DaemonGeneration::new(),
        )
        .unwrap(),
    );
    assert_eq!(
        standby_workspace_state_dir(&daemon, &partial_root).unwrap(),
        initialized.dir(),
        "an absent sessions.json is skipped even when the cwd names that subtree"
    );

    // A present node with the wrong type is corruption, not another miss
    // that may be hidden by selecting a different workspace.
    let corrupt_root = temporary.path().join("0-corrupt");
    std::fs::create_dir_all(&corrupt_root).unwrap();
    let corrupt_root = paths::canonical_workspace_root(&corrupt_root).unwrap();
    let corrupt = workspace_state::resolve(&daemon, &corrupt_root).unwrap();
    std::fs::create_dir(corrupt.dir().join("sessions.json")).unwrap();
    assert_eq!(
        standby_workspace_state_dir(&daemon, temporary.path())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );

    // Metadata failures other than absence also remain errors. A plain file
    // used as the state directory produces NotADirectory on every Unix host.
    let not_directory = temporary.path().join("not-directory");
    std::fs::write(&not_directory, "not a directory").unwrap();
    assert_ne!(
        lifecycle_state_initialized(&not_directory)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
}

fn daemon_restart_plan(workspace: WorkspaceId) -> DaemonRestartAgentPlan {
    let session = SessionId::new();
    let runtime_id = AgentRuntimeId::new();
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: WorktreeId::new(),
    };
    DaemonRestartAgentPlan {
        agents: vec![DaemonRestartAgent {
            runtime: AgentRuntimeRef::new(runtime_id, terminal.clone(), Some(session)).unwrap(),
            target: AgentResumeTarget {
                continuation: usagi_core::domain::id::AgentContinuationRef::new(),
                source: usagi_core::domain::id::AgentResumeSourceId::new(),
                workspace_id: workspace,
                session_id: Some(session),
                worktree_id: terminal.worktree_id,
                runtime_id,
                adapter_revision: 1,
            },
            profile_id: AgentProfileId::new("codex").unwrap(),
            expected_revision: 2,
            phase: usagi_core::domain::session_lifecycle::AgentPhase::Waiting,
        }],
    }
}

#[test]
fn pending_daemon_agent_restart_round_trips_and_clears_only_its_operation() {
    let workspace = tempfile::tempdir_in("/tmp").unwrap();
    let root = paths::canonical_workspace_root(workspace.path()).unwrap();
    let data = workspace.path().join("data");
    ensure_private_dir_all(&data.join("daemon")).unwrap();
    let operation = OperationId("restart-operation".to_owned());
    let pending = PendingDaemonAgentRestart::new(
        &operation,
        DaemonGeneration::new(),
        root,
        &daemon_restart_plan(WorkspaceId::new()),
    );

    write_pending_daemon_agent_restart(&data, &pending).unwrap();
    assert_eq!(
        read_pending_daemon_agent_restart(&data).unwrap(),
        Some(pending)
    );
    assert!(!clear_pending_daemon_agent_restart(&data, "other-operation").unwrap());
    assert!(clear_pending_daemon_agent_restart(&data, &operation.0).unwrap());
    assert!(!clear_pending_daemon_agent_restart(&data, &operation.0).unwrap());
}

#[test]
fn planned_agent_workspace_is_resolved_by_durable_identity() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let root = temporary.path().join("workspace");
    std::fs::create_dir_all(&root).unwrap();
    let root = paths::canonical_workspace_root(&root).unwrap();
    let data = temporary.path().join("data");
    let daemon = data.join("daemon");
    ensure_private_dir_all(&daemon).unwrap();
    let workspace = WorkspaceId::new();
    let state = workspace_state::resolve(&daemon, &root).unwrap();
    usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore::new(state.dir())
        .initialize(
            &usagi_core::domain::session_lifecycle::WorkspaceLifecycleState::new(
                workspace,
                chrono::Utc::now(),
            ),
            &root,
        )
        .unwrap();

    assert_eq!(
        planned_agent_workspace_root(&data, &daemon_restart_plan(workspace)).unwrap(),
        Some(root)
    );
    assert!(
        planned_agent_workspace_root(&data, &daemon_restart_plan(WorkspaceId::new()))
            .unwrap_err()
            .to_string()
            .contains("not present")
    );
    assert_eq!(
        planned_agent_workspace_root(&data, &DaemonRestartAgentPlan { agents: Vec::new() })
            .unwrap(),
        None
    );
}

#[test]
fn bootstrap_lock_rejects_a_path_replacement_after_flock() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path().join("data");
    ensure_private_dir(&data).unwrap();
    let daemon = data.join("daemon");
    ensure_private_dir(&daemon).unwrap();
    let path = daemon.join("bootstrap.lock");
    let replacement = replace_private_lock_after_flock(&path);

    let error = acquire_bootstrap_lock(&data).unwrap_err();
    replacement.join().unwrap();
    assert!(error.to_string().contains("bootstrap lock"));
    let retry = acquire_bootstrap_lock(&data).unwrap();
    assert_private_lock_descriptor(&retry);
}

#[test]
fn record_and_instance_locks_reject_broad_modes_and_hardlinks_without_chmod() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let daemon = directory.path().join("daemon");
    ensure_private_dir(&daemon).unwrap();

    let broad_record_lock = daemon.join("record.lock");
    std::fs::write(&broad_record_lock, []).unwrap();
    std::fs::set_permissions(&broad_record_lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    assert!(store.load().is_err());
    assert_eq!(
        std::fs::metadata(&broad_record_lock)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
    std::fs::remove_file(&broad_record_lock).unwrap();

    let record_target = daemon.join("record-target");
    std::fs::write(&record_target, b"preserve").unwrap();
    std::fs::set_permissions(&record_target, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::hard_link(&record_target, daemon.join("record.lock")).unwrap();
    assert!(store.load().is_err());
    assert_eq!(std::fs::read(&record_target).unwrap(), b"preserve");
    assert_eq!(
        std::fs::metadata(&record_target)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );

    let broad_instance_lock = daemon.join("daemon.lock");
    std::fs::write(&broad_instance_lock, []).unwrap();
    std::fs::set_permissions(&broad_instance_lock, std::fs::Permissions::from_mode(0o640)).unwrap();
    let broad_instance = FileInstanceLock {
        path: broad_instance_lock.clone(),
        held: RefCell::new(None),
    };
    assert!(broad_instance.acquire().is_err());
    assert_eq!(
        std::fs::metadata(&broad_instance_lock)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );

    // Pre-identity daemon builds created their singleton lock through the
    // process umask. The exact owner/single-link 0644 residue is narrowed
    // through the validated descriptor; no other broad mode is accepted.
    std::fs::set_permissions(&broad_instance_lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(broad_instance.acquire().unwrap());
    assert_eq!(
        std::fs::metadata(&broad_instance_lock)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(broad_instance);
    std::fs::remove_file(&broad_instance_lock).unwrap();

    let instance_target = daemon.join("instance-target");
    std::fs::write(&instance_target, b"preserve").unwrap();
    std::fs::set_permissions(&instance_target, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::hard_link(&instance_target, daemon.join("daemon.lock")).unwrap();
    let instance = FileInstanceLock {
        path: daemon.join("daemon.lock"),
        held: RefCell::new(None),
    };
    assert!(instance.acquire().is_err());
    assert_eq!(std::fs::read(&instance_target).unwrap(), b"preserve");
    assert_eq!(
        std::fs::metadata(instance_target)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
}

#[test]
fn bootstrap_lock_rejects_symlink_hardlink_and_non_regular_nodes() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path().join("data");
    ensure_private_dir(&data).unwrap();
    let daemon = data.join("daemon");
    ensure_private_dir(&daemon).unwrap();
    let lock = daemon.join("bootstrap.lock");
    let target = daemon.join("target");
    std::fs::write(&target, b"preserve").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

    std::os::unix::fs::symlink(&target, &lock).unwrap();
    assert!(acquire_bootstrap_lock(&data).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
    std::fs::remove_file(&lock).unwrap();

    std::fs::hard_link(&target, &lock).unwrap();
    assert_eq!(std::fs::metadata(&target).unwrap().nlink(), 2);
    assert!(acquire_bootstrap_lock(&data).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
    assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o600);
    std::fs::remove_file(&lock).unwrap();

    std::fs::create_dir(&lock).unwrap();
    assert!(acquire_bootstrap_lock(&data).is_err());
    std::fs::remove_dir(&lock).unwrap();

    // No broad mode other than the exact origin/main 0644 legacy state is
    // a valid umask residue or migration candidate.
    std::fs::write(&lock, []).unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(acquire_bootstrap_lock(&data).is_err());
    assert_eq!(std::fs::metadata(&lock).unwrap().mode() & 0o777, 0o666);

    // Use the sticky bit for the exact-mode boundary: Darwin strips set-id
    // bits when this test is built with coverage instrumentation.
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o1600)).unwrap();
    assert_eq!(std::fs::metadata(&lock).unwrap().mode() & 0o7777, 0o1600);
    assert!(acquire_bootstrap_lock(&data).is_err());
    assert_eq!(std::fs::metadata(&lock).unwrap().mode() & 0o7777, 0o1600);

    // origin/main created bootstrap.lock without an explicit mode, so the
    // exact historical 0644 owner file is a one-time migration exception.
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    let repaired = acquire_bootstrap_lock(&data).unwrap();
    assert_eq!(repaired.metadata().unwrap().mode() & 0o777, 0o600);
    drop(repaired);

    // A creator killed between create_new and fd-fchmod can leave the same
    // owner single-link inode at mode 000. Secure reopen repairs that
    // durable residue instead of permanently wedging every later client.
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();
    let repaired = acquire_bootstrap_lock(&data).unwrap();
    assert_eq!(repaired.metadata().unwrap().mode() & 0o777, 0o600);
    assert_eq!(repaired.metadata().unwrap().nlink(), 1);
}

#[test]
fn ipc_ready_retains_listener_cleanup_ownership_until_retry_succeeds() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let listener = SecureUnixListener::bind(data, ipc_generation()).unwrap();
    let daemon = data.join("daemon");
    let socket = daemon.join(&listener.locator().endpoint);
    let cleanup = listener.cleanup_handle();
    let lock = daemon.join("current.lock");
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    let ready = fresh_ipc_ready(data, &info);
    ready.publication_attempted.store(true, Ordering::Release);
    ready.published.store(true, Ordering::Release);
    *ready.listener.borrow_mut() = Some(listener);
    *ready.cleanup.borrow_mut() = Some(cleanup);

    assert!(ready.retire().is_err());
    assert!(ready.listener.borrow().is_some());
    assert!(ready.cleanup.borrow().is_some());
    assert!(socket.exists());
    assert!(daemon.join("current.json").exists());

    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    ready.retire().unwrap();
    assert!(ready.listener.borrow().is_none());
    assert!(ready.cleanup.borrow().is_none());
    assert!(!socket.exists());
    assert!(!daemon.join("current.json").exists());
}

#[test]
fn ipc_ready_retains_cleanup_token_when_accept_worker_panics() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let listener = SecureUnixListener::bind(data, ipc_generation()).unwrap();
    let daemon = data.join("daemon");
    let socket = daemon.join(&listener.locator().endpoint);
    let cleanup = listener.cleanup_handle();
    let lock = daemon.join("current.lock");
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    let worker = std::thread::spawn(move || -> SecureUnixListener {
        let _listener = listener;
        panic!("injected accept-loop panic")
    });
    let ready = fresh_ipc_ready(data, &info);
    ready.publication_attempted.store(true, Ordering::Release);
    ready.published.store(true, Ordering::Release);
    *ready.worker.borrow_mut() = Some(worker);
    *ready.cleanup.borrow_mut() = Some(cleanup);

    assert!(ready.quiesce().is_err());
    assert!(ready.worker.borrow().is_none());
    assert!(ready.listener.borrow().is_none());
    assert!(ready.retire().is_err());
    assert!(ready.cleanup.borrow().is_some());
    assert!(socket.exists());
    assert!(daemon.join("current.json").exists());

    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    ready.retire().unwrap();
    assert!(ready.cleanup.borrow().is_none());
    assert!(!socket.exists());
    assert!(!daemon.join("current.json").exists());
}

#[test]
fn abnormal_startup_after_bind_keeps_retryable_cleanup_ownership() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let daemon = data.join("daemon");
    let socket = RefCell::new(None);
    let ready = fresh_ipc_ready(data, &info);

    let unsafe_locator_lock = |daemon: &Path| -> std::io::Result<()> {
        let lock = daemon.join("current.lock");
        if !lock.exists() {
            std::fs::write(&lock, b"")?;
        }
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644))
    };

    let error = ready
        .publish_with(|listener, _generation| {
            *socket.borrow_mut() = Some(daemon.join(&listener.locator().endpoint));
            // Break the locator lock so the listener's own `Drop` cannot
            // reclaim this endpoint: the retained cleanup token has to remain
            // the only retry path.
            unsafe_locator_lock(&daemon)?;
            Err(std::io::Error::other("injected post-bind startup failure"))
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "injected post-bind startup failure");
    assert!(ready.cleanup.borrow().is_some());
    assert!(ready.publication_attempted.load(Ordering::Acquire));
    assert!(socket.borrow().as_ref().unwrap().exists());
    // Binding is not publishing: a startup that failed before the generation
    // authority ran leaves nothing for a client to discover.
    assert!(!daemon.join("current.json").exists());

    std::fs::set_permissions(
        daemon.join("current.lock"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    // Once published, an unreadable locator lock keeps retirement retryable
    // rather than letting the endpoint look cleanly reclaimed.
    ready.publish_current().unwrap();
    assert!(daemon.join("current.json").exists());
    unsafe_locator_lock(&daemon).unwrap();
    assert!(ready.retire().is_err());
    assert!(ready.cleanup.borrow().is_some());
    assert!(socket.borrow().as_ref().unwrap().exists());

    std::fs::set_permissions(
        daemon.join("current.lock"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    ready.retire().unwrap();
    assert!(ready.cleanup.borrow().is_none());
    assert!(!socket.borrow().as_ref().unwrap().exists());
    assert!(!daemon.join("current.json").exists());
}

/// A registry authority over a real data directory, bound to `ready`.
fn registry_authority<'a>(data_dir: &'a Path, ready: &'a IpcReady<'a>) -> RegistryAuthority<'a> {
    RegistryAuthority {
        data_dir,
        ready,
        build: current_build(),
        pid: std::process::id(),
        claimed: RefCell::new(None),
    }
}

/// The durable registry document, which must exist by the time this is read.
fn registry_document(
    data_dir: &Path,
) -> usagi_daemon::usecase::authority::registry::RegistryDocument {
    usagi_daemon::infrastructure::generation_registry::read_registry_document(data_dir)
        .unwrap()
        .expect("the daemon registered a generation")
}

#[test]
fn claiming_authority_registers_this_generation_and_then_publishes_current() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let listener = SecureUnixListener::bind_private(data, ipc_generation()).unwrap();
    let generation = listener.locator().generation.clone();
    let ready = fresh_ipc_ready(data, &info);
    *ready.cleanup.borrow_mut() = Some(listener.cleanup_handle());
    let authority = registry_authority(data, &ready);

    // A bound endpoint is not yet discoverable.
    assert!(read_locator(&data.join("daemon")).is_err());

    authority.claim().unwrap();

    let document = registry_document(data);
    assert_eq!(
        document.current.map(|current| current.as_str()),
        Some(generation.0.clone())
    );
    let entry = document.generations.first().unwrap();
    assert_eq!(
        entry.role,
        usagi_daemon::usecase::generation::GenerationRole::Active
    );
    assert_eq!(entry.endpoint, listener.locator().endpoint);
    assert_eq!(entry.process.pid, std::process::id());
    // Only now is the endpoint discoverable, and by exactly the generation
    // the registry named.
    assert_eq!(
        read_locator(&data.join("daemon")).unwrap().generation,
        generation
    );

    // A repeated claim converges instead of consuming a second slot.
    authority.claim().unwrap();
    assert_eq!(registry_document(data).generations.len(), 1);

    authority.release().unwrap();
    assert_eq!(registry_document(data).current, None);
    // Releasing an authority that is already given up is not a failure.
    authority.release().unwrap();
}

#[test]
fn claiming_authority_before_binding_is_refused_without_touching_the_registry() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let ready = fresh_ipc_ready(data, &info);
    let authority = registry_authority(data, &ready);

    assert_eq!(
        authority.claim().unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(
        usagi_daemon::infrastructure::generation_registry::read_registry_document(data),
        Ok(None)
    );
    // Nothing was claimed, so there is nothing to release either.
    authority.release().unwrap();
}

#[test]
fn a_non_canonical_bound_generation_is_refused_before_the_registry_is_written() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let listener = SecureUnixListener::bind_private(
        data,
        usagi_core::infrastructure::ipc::DaemonGeneration("not-a-generation".to_owned()),
    )
    .unwrap();
    let ready = fresh_ipc_ready(data, &info);
    *ready.cleanup.borrow_mut() = Some(listener.cleanup_handle());
    let authority = registry_authority(data, &ready);

    assert_eq!(
        authority.claim().unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        usagi_daemon::infrastructure::generation_registry::read_registry_document(data),
        Ok(None)
    );
}

#[test]
fn a_live_registered_authority_is_repaired_rather_than_displaced() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let daemon = data.join("daemon");

    // A generation whose recorded process is this very test binary: the
    // recovery below can therefore prove it alive.
    let holder_listener = SecureUnixListener::bind_private(data, ipc_generation()).unwrap();
    let holder = holder_listener.locator().clone();
    let holder_ready = fresh_ipc_ready(data, &info);
    *holder_ready.cleanup.borrow_mut() = Some(holder_listener.cleanup_handle());
    registry_authority(data, &holder_ready).claim().unwrap();
    // Drop only the published locator, leaving the holder's endpoint bound and
    // the registry as the only surviving statement of authority.
    std::fs::remove_file(daemon.join("current.json")).unwrap();
    assert!(read_locator(&daemon).is_err());

    let listener = SecureUnixListener::bind_private(data, ipc_generation()).unwrap();
    let ready = fresh_ipc_ready(data, &info);
    *ready.cleanup.borrow_mut() = Some(listener.cleanup_handle());

    let error = registry_authority(data, &ready).claim().unwrap_err();

    assert!(
        error.to_string().contains("still holds registry authority"),
        "{error}"
    );
    // Recovery republished the live holder's own locator — the foreign-owner
    // publication path — and this process's endpoint was never published.
    assert_eq!(read_locator(&daemon).unwrap().generation, holder.generation);
    assert_eq!(registry_document(data).generations.len(), 1);
}

#[test]
fn a_generation_process_is_only_verified_by_its_exact_recorded_identity() {
    let pid = std::process::id();
    let live = own_process_identity(pid).unwrap();
    assert_eq!(
        observe_generation_process(&live),
        ProcessObservation::VerifiedAlive(live.clone())
    );

    let reused = ProcessIdentity {
        start_identity: "another-incarnation".to_owned(),
        ..live
    };
    assert_eq!(
        observe_generation_process(&reused),
        ProcessObservation::Unknown
    );

    let legacy = ProcessIdentity {
        start_identity: String::new(),
        ..live
    };
    assert_eq!(
        observe_generation_process(&legacy),
        ProcessObservation::Unknown
    );

    // A PID far above the OS maximum names no process at all.
    let absent = ProcessIdentity {
        pid: 2_000_000_000,
        ..live
    };
    assert_eq!(
        observe_generation_process(&absent),
        ProcessObservation::Gone
    );
}

#[test]
fn forced_transition_stops_a_live_draining_generation_before_stale_cleanup() {
    use usagi_daemon::usecase::authority::registry::{GenerationEntry, RegistryFile};

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let mut draining = Command::new("sleep").arg("30").spawn().unwrap();
    let draining_process = ProcessIdentity {
        pid: draining.id(),
        start_identity: process_start_identity(draining.id()).unwrap(),
        process_group: process_group(draining.id()).unwrap(),
    };
    let mut retired = Command::new("sleep").arg("30").spawn().unwrap();
    let retired_process = ProcessIdentity {
        pid: retired.id(),
        start_identity: process_start_identity(retired.id()).unwrap(),
        process_group: process_group(retired.id()).unwrap(),
    };
    let entry = |role, process: ProcessIdentity| GenerationEntry {
        generation: DaemonGeneration::new(),
        role,
        endpoint: format!("generations/{}/daemon.sock", process.pid),
        process,
        expected_build: current_build(),
        verified_build: Some(current_build()),
        revision: 1,
    };
    let document = RegistryDocument {
        revision: 1,
        generations: vec![
            entry(GenerationRole::Draining, draining_process),
            entry(GenerationRole::Retired, retired_process),
        ],
        ..RegistryDocument::default()
    };
    assert!(
        GenerationRegistryFile::new(data)
            .unwrap()
            .compare_and_write(None, &serde_json::to_string(&document).unwrap())
            .unwrap()
    );
    let control = RegistryGenerationControl::production(data.to_path_buf());
    assert!(control.has_live().unwrap());

    // Reap concurrently: an unreaped fixture child remains visible as a
    // zombie, while a real daemon generation is reaped by its process owner.
    let draining_waiter = std::thread::spawn(move || draining.wait().unwrap());
    control.shutdown_all().unwrap();
    assert!(!draining_waiter.join().unwrap().success());
    assert!(!control.has_live().unwrap());
    assert!(
        retired.try_wait().unwrap().is_none(),
        "retired generations must never be signalled"
    );
    retired.kill().unwrap();
    retired.wait().unwrap();
}

/// `--force` is the operator's escape hatch, so it has to end with the
/// generation actually gone. A daemon draining Agent runtimes and PTYs can
/// outlast any fixed SIGTERM window, and one that never exits outlasts every
/// window — so the transition escalates rather than reporting a timeout
/// while leaving the process running and every later command refusing on it.
#[test]
fn a_forced_transition_escalates_to_sigkill_when_sigterm_is_ignored() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::process::ExitStatusExt as _;
    use usagi_daemon::usecase::authority::registry::{GenerationEntry, RegistryFile};

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let program = data.join("deaf-to-sigterm");
    // The marker is written *after* the trap is installed, so the test can
    // wait for the child to actually be deaf before signalling it. Spawning
    // and signalling straight away races the shell's own startup, and the
    // run where SIGTERM lands first would read as "escalation not needed".
    std::fs::write(
        &program,
        "#!/bin/sh\ntrap '' TERM\n: > \"$1\"\nwhile :; do sleep 1; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let deaf_marker = data.join("deaf.ready");
    let mut deaf = Command::new(&program).arg(&deaf_marker).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !deaf_marker.exists() {
        assert!(
            Instant::now() < deadline,
            "the fixture child never installed its SIGTERM trap"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let process = ProcessIdentity {
        pid: deaf.id(),
        start_identity: process_start_identity(deaf.id()).unwrap(),
        process_group: process_group(deaf.id()).unwrap(),
    };
    let document = RegistryDocument {
        revision: 1,
        generations: vec![GenerationEntry {
            generation: DaemonGeneration::new(),
            role: GenerationRole::Active,
            endpoint: format!("generations/{}/daemon.sock", process.pid),
            process,
            expected_build: current_build(),
            verified_build: Some(current_build()),
            revision: 1,
        }],
        ..RegistryDocument::default()
    };
    assert!(
        GenerationRegistryFile::new(data)
            .unwrap()
            .compare_and_write(None, &serde_json::to_string(&document).unwrap())
            .unwrap()
    );
    let control = RegistryGenerationControl {
        data_dir: data.to_path_buf(),
        // The escalation is what is under test, not the length of the
        // production grace, so this waits only long enough to prove SIGTERM
        // was given its turn first.
        term_grace: Duration::from_millis(300),
        kill_grace: Duration::from_secs(5),
    };
    assert!(control.has_live().unwrap());

    // Reap concurrently: a killed fixture child stays visible as a zombie
    // until someone waits on it, and the exact-identity probe would keep
    // reading that zombie as live.
    let waiter = std::thread::spawn(move || deaf.wait().unwrap());
    control.shutdown_all().unwrap();
    let status = waiter.join().unwrap();
    assert!(!status.success());
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "a generation that ignores SIGTERM has to be escalated, not reported as a timeout"
    );
    assert!(!control.has_live().unwrap());
}

#[test]
fn publishing_current_before_binding_is_refused() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let info = daemon_test_info();
    let ready = fresh_ipc_ready(directory.path(), &info);

    assert_eq!(
        ready.publish_current().unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(ready.bound_endpoint().is_none());
}

#[test]
fn stale_cleanup_keeps_record_until_endpoint_retry_proves_absence() {
    use std::mem::ManuallyDrop;
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let daemon = data.join("daemon");
    let mut listener = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let socket = daemon.join(&listener.locator().endpoint);
    let record_path = daemon.join("daemon.json");
    let store = DaemonRecordStore::new(FsRecordFile { path: record_path });
    let record = usagi_core::domain::daemon::DaemonRecord::new(4242);
    store.save(&record).unwrap();
    let ready = fresh_ipc_ready(data, &info);
    let lock = daemon.join("current.lock");
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert!(ready.cleanup_if(&store, &record).is_err());
    assert_eq!(store.load().unwrap(), Some(record.clone()));
    assert!(socket.exists());
    assert!(daemon.join("current.json").exists());

    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        ready.cleanup_if(&store, &record).unwrap(),
        StaleCleanup::Cleared
    );
    assert_eq!(store.load().unwrap(), None);
    assert!(!socket.exists());
    assert!(!daemon.join("current.json").exists());
    // SAFETY: the listener was not moved or dropped; cleanup is idempotent.
    unsafe { ManuallyDrop::drop(&mut listener) };
}

#[test]
fn unavailable_client_errors_preserve_the_safe_reason() {
    let expected = PathBuf::from("available");
    assert_eq!(client_result(Ok(expected.clone())), Ok(expected));
    assert!(matches!(
        client_result::<PathBuf>(Err(anyhow::anyhow!("data directory unavailable"))),
        Err(ClientError::Unavailable(message)) if message == "data directory unavailable"
    ));
}

#[test]
fn the_declared_workspace_prefers_the_opened_one_then_the_injected_root() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let canonical_root =
        paths::wire_workspace_root(paths::canonical_workspace_root(&workspace).unwrap());
    let canonical = ClientWorkspace::Bound {
        root: canonical_root,
    };

    // A daemon-provisioned child declares the trusted root the daemon
    // injected, not whatever directory the provider left it in.
    assert_eq!(
        declared_client_workspace(
            None,
            Some(workspace.clone().into_os_string()),
            Ok(directory.path().join("elsewhere")),
        ),
        canonical
    );

    // Every other surface declares its canonical working directory, so a
    // subdirectory spelling still resolves onto the one comparable root. An
    // empty injection is ignored rather than treated as a root.
    assert_eq!(
        declared_client_workspace(
            None,
            Some(std::ffi::OsString::new()),
            Ok(workspace.join(".").join("..").join("workspace")),
        ),
        canonical
    );

    // An unresolvable directory is declared exactly as spelled: the daemon
    // refuses it rather than this client assuming that it matches.
    let missing = workspace.join("absent");
    assert_eq!(
        declared_client_workspace(None, None, Ok(missing.clone())),
        ClientWorkspace::Bound {
            root: paths::wire_workspace_root(&missing),
        }
    );

    // With no working directory at all there is nothing to declare, and an
    // empty root is refused by every daemon.
    assert_eq!(
        declared_client_workspace(
            None,
            None,
            Err(std::io::Error::other("no working directory"))
        ),
        ClientWorkspace::Bound {
            root: String::new(),
        }
    );

    // An opened workspace outranks both: it is the workspace whose sessions
    // the surface is about to show, so the daemon must serve exactly it. The
    // injected root and the working directory would both be admitted here
    // (they are the trusted root and a directory below it), which is how the
    // title and the session list used to disagree.
    let opened = directory.path().join("other");
    std::fs::create_dir(&opened).unwrap();
    let opened_canonical = paths::canonical_workspace_root(&opened).unwrap();
    assert_eq!(
        declared_client_workspace(
            Some(opened_canonical.clone()),
            Some(workspace.clone().into_os_string()),
            Ok(workspace.join("crates")),
        ),
        ClientWorkspace::Selected {
            root: paths::wire_workspace_root(&opened_canonical),
        }
    );
}

#[test]
fn declaring_the_opened_workspace_selects_it_for_every_later_connection() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();

    let canonical = declare_opened_workspace(&workspace).unwrap();
    assert_eq!(
        canonical,
        paths::canonical_workspace_root(&workspace).unwrap()
    );
    assert_eq!(opened_workspace().as_deref(), Some(canonical.as_path()));
    assert_eq!(
        client_workspace(),
        ClientWorkspace::Selected {
            root: paths::wire_workspace_root(&canonical),
        }
    );

    // `usagi hop` opens several workspaces in one process, so the latest
    // selection replaces the previous one.
    let second = directory.path().join("second");
    std::fs::create_dir(&second).unwrap();
    let second_canonical = declare_opened_workspace(&second).unwrap();
    assert_eq!(
        client_workspace(),
        ClientWorkspace::Selected {
            root: paths::wire_workspace_root(&second_canonical),
        }
    );

    // A path that cannot be resolved is reported instead of being declared,
    // and it leaves the previous selection untouched.
    assert!(declare_opened_workspace(&directory.path().join("absent")).is_err());
    assert_eq!(
        opened_workspace().as_deref(),
        Some(second_canonical.as_path())
    );

    // A root with no wire spelling is reported before anything connects or
    // starts a daemon: no daemon can own it, because its own authority record
    // and the workspace registry are JSON.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        let name = std::ffi::OsString::from_vec(b"workspace-\xff".to_vec());
        let unnameable = directory.path().join(name);
        if std::fs::create_dir(&unnameable).is_ok() {
            let error = declare_opened_workspace(&unnameable).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("not valid UTF-8"), "{error}");
            assert_eq!(
                opened_workspace().as_deref(),
                Some(second_canonical.as_path())
            );
        }
    }

    *OPENED_WORKSPACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

#[test]
fn a_lifecycle_start_runs_in_the_workspace_being_opened() {
    let exe = PathBuf::from("/usr/bin/usagi");
    let start = lifecycle_command(&exe, &["daemon", "start"], None);
    // Without a selection the child inherits this process's directory, which
    // is what a plain `usagi daemon start` means.
    assert_eq!(start.get_current_dir(), None);
    assert_eq!(
        start.get_args().collect::<Vec<_>>(),
        vec!["daemon", "start"]
    );

    // A daemon takes authority over the workspace of its start-up directory,
    // so a client opening a workspace must start it there — otherwise the
    // fresh daemon would bind this process's directory and then refuse the
    // very connection that started it.
    let opened = PathBuf::from("/workspace/root");
    let restart = lifecycle_command(&exe, &["daemon", "restart"], Some(opened.clone()));
    assert_eq!(restart.get_current_dir(), Some(opened.as_path()));
    // Development consumes a build-mismatch trigger with a *planned*
    // replacement: the live-runtime guard decides between a cold transition
    // and a seamless rollover, so no `--force` override is passed and a
    // rebuild cannot kill another client's Agent.
    assert_eq!(
        restart.get_args().collect::<Vec<_>>(),
        vec!["daemon", "restart"]
    );
}

#[test]
fn a_lifecycle_failure_preserves_the_childs_diagnostic() {
    assert_eq!(
        lifecycle_failure_message(
            "start",
            b"error: workspace fence is already held\n",
            b"ignored fallback\n",
        ),
        "daemon start failed: error: workspace fence is already held"
    );
    assert_eq!(
        lifecycle_failure_message("restart", b"", b""),
        "daemon restart failed"
    );
    assert_eq!(
        lifecycle_failure_message("start", b"", b"refused on stdout\n"),
        "daemon start failed: refused on stdout"
    );
}

#[test]
fn a_reused_build_mismatch_is_recorded_once_per_daemon_artifact() {
    let running = test_build("a");
    let expected = test_build("b");
    let trigger = build_rollover_trigger(&running, &expected, "development", false).unwrap();

    let entry = reused_build_mismatch_record(&trigger, "live runtime preserved")
        .expect("the first observation of a mismatch is recorded");
    assert!(entry.contains(&running.artifact), "{entry}");
    assert!(entry.contains(&expected.artifact), "{entry}");
    assert!(entry.contains("live runtime preserved"), "{entry}");
    // Every bootstrapped lane observes the same standing mismatch, so the
    // trail stays one entry instead of one per connection.
    assert_eq!(
        reused_build_mismatch_record(&trigger, "live runtime preserved"),
        None
    );
}

#[test]
fn only_development_attempts_automatic_build_replacement() {
    assert!(should_attempt_automatic_replacement(
        paths::RuntimeMode::Development
    ));
    assert!(!should_attempt_automatic_replacement(
        paths::RuntimeMode::Production
    ));
    assert!(!should_attempt_automatic_replacement(
        paths::RuntimeMode::Local
    ));
}

/// A known artifact identity whose source digest is distinguished by `seed`.
fn test_build(seed: &str) -> BuildIdentity {
    usagi_core::infrastructure::ipc::build_identity(
        "2.0.0",
        "test",
        "test-target",
        "debug",
        &seed.repeat(64),
    )
}

#[test]
fn client_bootstrap_recovery_uses_the_instance_fence_not_a_raw_pid() {
    use std::mem::ManuallyDrop;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let daemon = data.join("daemon");
    let mut listener = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let socket = daemon.join(&listener.locator().endpoint);
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    // The PID is deliberately live (this test process), modelling PID
    // reuse. Acquiring daemon.lock proves that this live process is not the
    // daemon owner, and recovery never sends it a signal.
    let record = DaemonRecord::identified(std::process::id(), "reused-process");
    store.save(&record).unwrap();

    assert_eq!(
        recover_stale_client_endpoint(data).unwrap(),
        bootstrap::StaleRecovery::Recovered
    );
    assert_eq!(store.load().unwrap(), None);
    assert!(!socket.exists());
    assert!(!daemon.join("current.json").exists());
    assert!(
        ExactProcessControl
            .process_start_identity(std::process::id())
            .is_ok()
    );

    // SAFETY: recovery removed only filesystem artifacts; dropping closes
    // the still-owned listener fd and its cleanup is idempotent.
    unsafe { ManuallyDrop::drop(&mut listener) };
}

#[test]
fn client_bootstrap_recovers_a_socket_first_partial_retire_with_a_reused_live_pid() {
    use std::mem::ManuallyDrop;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let daemon = data.join("daemon");
    let mut listener = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let cleanup = listener.cleanup_handle();
    let socket = daemon.join(&listener.locator().endpoint);
    let current = daemon.join("current.json");
    let alias = daemon.join("current.alias");
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    // Model PID reuse: this process is alive, but it does not own the
    // daemon singleton. Recovery must use daemon.lock rather than the PID.
    let record = DaemonRecord::identified(std::process::id(), "reused-process");
    store.save(&record).unwrap();

    // A locator hardlink forces retirement to stop after its socket-first
    // step. Once the unsafe alias is repaired, the durable state is the
    // exact crash window: record + locator remain, while the socket is
    // absent. That endpoint absence must enter fenced recovery instead of
    // the raw `NotFound => start` path.
    std::fs::hard_link(&current, &alias).unwrap();
    assert_eq!(
        cleanup.retire().unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(!socket.exists());
    assert!(current.exists());
    assert_eq!(store.load().unwrap(), Some(record));
    std::fs::remove_file(alias).unwrap();
    assert_eq!(
        usagi_daemon::infrastructure::unix_transport::connect_current(data)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::ConnectionRefused
    );

    assert_eq!(
        recover_stale_client_endpoint(data).unwrap(),
        bootstrap::StaleRecovery::Recovered
    );
    assert_eq!(store.load().unwrap(), None);
    assert!(!current.exists());
    assert!(
        ExactProcessControl
            .process_start_identity(std::process::id())
            .is_ok()
    );

    // SAFETY: recovery removed only filesystem artifacts; dropping closes
    // the still-owned listener fd and its cleanup is idempotent.
    unsafe { ManuallyDrop::drop(&mut listener) };
}

#[test]
fn client_bootstrap_reclaims_an_unverified_owner_without_signalling() {
    use std::mem::ManuallyDrop;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let daemon = data.join("daemon");
    let mut listener = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let socket = daemon.join(&listener.locator().endpoint);
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    // A legacy record carries no signal identity. The live PID therefore
    // remains unverified, but daemon.lock proves this process does not own
    // the active role. Recovery reclaims the endpoint without ever
    // addressing that PID.
    let record = DaemonRecord::new(std::process::id());
    store.save(&record).unwrap();
    assert_eq!(
        ExactProcessControl.observe(&record),
        DaemonProcessObservation::Unknown
    );

    assert_eq!(
        recover_stale_client_endpoint(data).unwrap(),
        bootstrap::StaleRecovery::Recovered
    );
    assert_eq!(store.load().unwrap(), None);
    assert!(!socket.exists());
    assert!(!daemon.join("current.json").exists());
    assert!(
        ExactProcessControl
            .process_start_identity(std::process::id())
            .is_ok(),
        "recovery must not signal the unverified PID"
    );

    // SAFETY: the listener has not moved and still owns normal cleanup.
    unsafe { ManuallyDrop::drop(&mut listener) };
}

/// The instance lock this recovery holds excludes another *active* daemon,
/// not a standby — which holds no lock, so its live socket looks exactly like
/// a crashed generation's leftover on the filesystem. Sweeping it would leave
/// the registry naming a verified successor that nobody accepts on, which is
/// the same hazard the daemon-side sweep already guards against.
#[test]
fn client_bootstrap_recovery_preserves_a_live_standby_endpoint() {
    use std::mem::ManuallyDrop;
    use std::os::unix::fs::PermissionsExt;
    use usagi_daemon::usecase::authority::registry::{
        GenerationEntry, REGISTRY_SCHEMA, RegistryDocument,
    };
    use usagi_daemon::usecase::generation::GenerationRole;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let daemon = data.join("daemon");

    // The dead active's published endpoint, and a live standby's private one.
    let mut dead = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let dead_socket = daemon.join(&dead.locator().endpoint);
    let standby = SecureUnixListener::bind_private(data, ipc_generation()).unwrap();
    let standby_socket = daemon.join(&standby.locator().endpoint);
    assert!(dead_socket.exists() && standby_socket.exists());

    let active_generation =
        usagi_core::domain::id::DaemonGeneration::parse(&dead.locator().generation.0).unwrap();
    let standby_generation =
        usagi_core::domain::id::DaemonGeneration::parse(&standby.locator().generation.0).unwrap();
    // The standby's recorded process is this one, which the OS proves alive;
    // the active's is a PID that has been reused, which it cannot.
    let live = own_process_identity(std::process::id()).unwrap();
    let mut gone = live.clone();
    gone.start_identity = "gone".to_owned();
    let entry = |generation, role, endpoint: &str, process: ProcessIdentity| GenerationEntry {
        generation,
        role,
        endpoint: endpoint.to_owned(),
        process,
        expected_build: current_build(),
        verified_build: Some(current_build()),
        revision: 1,
    };
    let document = RegistryDocument {
        schema: REGISTRY_SCHEMA.to_owned(),
        revision: 1,
        current: Some(active_generation),
        generations: vec![
            entry(
                active_generation,
                GenerationRole::Active,
                &dead.locator().endpoint,
                gone,
            ),
            entry(
                standby_generation,
                GenerationRole::Standby,
                &standby.locator().endpoint,
                live,
            ),
        ],
        handoff: None,
        completed_operation: None,
    };
    // Written the way the daemon writes it: the private read this recovery
    // performs rejects a world-readable document.
    let registry = daemon.join("generations.json");
    std::fs::write(&registry, serde_json::to_string(&document).unwrap()).unwrap();
    std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o600)).unwrap();

    // A record whose identity no longer matches its PID is proved stale, which
    // is what admits this recovery at all.
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    store
        .save(&DaemonRecord::identified(std::process::id(), "gone"))
        .unwrap();

    assert_eq!(
        recover_stale_client_endpoint(data).unwrap(),
        bootstrap::StaleRecovery::Recovered
    );

    // The crashed generation's residue is reclaimed, and the live standby's
    // socket — which its own process is still accepting on — is not.
    assert!(!dead_socket.exists());
    assert!(
        standby_socket.exists(),
        "client recovery swept a live standby endpoint"
    );
    assert_eq!(store.load().unwrap(), None);

    drop(standby);
    // SAFETY: recovery removed only filesystem artifacts; dropping closes the
    // still-owned listener fd and its cleanup is idempotent.
    unsafe { ManuallyDrop::drop(&mut dead) };
}

#[test]
fn client_bootstrap_recovery_requires_an_exact_lifecycle_record() {
    use std::mem::ManuallyDrop;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let daemon = data.join("daemon");
    let mut listener = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let socket = daemon.join(&listener.locator().endpoint);

    assert_eq!(
        recover_stale_client_endpoint(data).unwrap(),
        bootstrap::StaleRecovery::NotProven
    );
    assert!(socket.exists());
    assert!(daemon.join("current.json").exists());

    // SAFETY: the listener has not moved and still owns normal cleanup.
    unsafe { ManuallyDrop::drop(&mut listener) };
}

#[test]
fn client_bootstrap_recovery_preserves_an_active_owner() {
    use std::mem::ManuallyDrop;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let daemon = data.join("daemon");
    let mut listener = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let locator = listener.locator().clone();
    let socket = daemon.join(&locator.endpoint);
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    let record = DaemonRecord::identified(4242, "gone-process");
    store.save(&record).unwrap();

    assert_eq!(
        recover_stale_client_endpoint_with(
            data,
            |_lock| Ok(false),
            || panic!("post-lock effects must not run for an active owner"),
        )
        .unwrap(),
        bootstrap::StaleRecovery::OwnerActive
    );
    assert_eq!(store.load().unwrap(), Some(record));
    assert_eq!(
        usagi_daemon::infrastructure::unix_transport::read_locator(&daemon).unwrap(),
        locator
    );
    assert!(socket.exists());

    // SAFETY: the listener has not moved and still owns normal cleanup.
    unsafe { ManuallyDrop::drop(&mut listener) };
}

#[test]
fn client_bootstrap_recovery_preserves_a_record_replaced_after_instance_lock() {
    use std::mem::ManuallyDrop;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let daemon = data.join("daemon");
    let mut listener = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let locator = listener.locator().clone();
    let socket = daemon.join(&locator.endpoint);
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    let old = usagi_core::domain::daemon::DaemonRecord::new(4242);
    let replacement = usagi_core::domain::daemon::DaemonRecord {
        pid: old.pid,
        process_start_identity: old.process_start_identity.clone(),
        started_at: old.started_at + chrono::Duration::nanoseconds(1),
    };
    store.save(&old).unwrap();

    assert_eq!(
        recover_stale_client_endpoint_with(data, InstanceLock::acquire, || {
            store.save(&replacement).unwrap();
        })
        .unwrap(),
        bootstrap::StaleRecovery::NotProven
    );
    assert_eq!(store.load().unwrap(), Some(replacement));
    assert_eq!(
        usagi_daemon::infrastructure::unix_transport::read_locator(&daemon).unwrap(),
        locator
    );
    assert!(socket.exists());

    // SAFETY: the listener has not moved and still owns normal cleanup.
    unsafe { ManuallyDrop::drop(&mut listener) };
}

#[test]
fn client_bootstrap_recovery_keeps_record_when_current_lock_is_unsafe() {
    use std::mem::ManuallyDrop;
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let daemon = data.join("daemon");
    let mut listener = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    let record = DaemonRecord::identified(4242, "gone-process");
    store.save(&record).unwrap();
    let current_lock = daemon.join("current.lock");
    std::fs::set_permissions(&current_lock, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert!(recover_stale_client_endpoint(data).is_err());
    assert_eq!(store.load().unwrap(), Some(record));
    assert!(daemon.join("current.json").exists());

    std::fs::set_permissions(&current_lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    // SAFETY: the listener has not moved and still owns normal cleanup.
    unsafe { ManuallyDrop::drop(&mut listener) };
}

#[test]
fn stale_retry_clears_record_only_after_socket_first_partial_retire_commits_locator() {
    use std::mem::ManuallyDrop;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let daemon = data.join("daemon");
    let mut listener = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let cleanup = listener.cleanup_handle();
    let socket = daemon.join(&listener.locator().endpoint);
    let current = daemon.join("current.json");
    let alias = daemon.join("current.alias");
    std::fs::hard_link(&current, &alias).unwrap();
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    let record = usagi_core::domain::daemon::DaemonRecord::new(4242);
    store.save(&record).unwrap();

    assert_eq!(
        cleanup.retire().unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(!socket.exists(), "owned socket is the first cleanup step");
    assert!(current.exists(), "unsafe locator remains the commit fence");
    assert_eq!(store.load().unwrap(), Some(record.clone()));

    std::fs::remove_file(alias).unwrap();
    let ready = fresh_ipc_ready(data, &info);
    assert_eq!(
        ready.cleanup_if(&store, &record).unwrap(),
        StaleCleanup::Cleared
    );
    assert_eq!(store.load().unwrap(), None);
    assert!(!current.exists());
    // SAFETY: the listener was not moved or dropped; cleanup is idempotent.
    unsafe { ManuallyDrop::drop(&mut listener) };
}

#[test]
fn serve_preserves_stale_record_until_real_pre_registration_recovery_succeeds() {
    use std::mem::ManuallyDrop;
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let daemon = data.join("daemon");
    let mut listener = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let socket = daemon.join(&listener.locator().endpoint);
    let current = daemon.join("current.json");
    let current_lock = daemon.join("current.lock");
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    let stale = usagi_core::domain::daemon::DaemonRecord::new(4242);
    store.save(&stale).unwrap();
    std::fs::set_permissions(&current_lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    let publishes = Cell::new(0);

    {
        let ready = fresh_ipc_ready(data, &info);
        let recovery = RecoveryOnlyReady {
            ready: &ready,
            publishes: &publishes,
        };
        let lock = FileInstanceLock {
            path: daemon.join("daemon.lock"),
            held: RefCell::new(None),
        };
        assert!(
            usagi_daemon::usecase::serve::serve(
                &mut Vec::new(),
                &store,
                &recovery,
                &NoGenerationAuthority,
                &ImmediateTestShutdown,
                &AcquiredWorkspaceFence,
                &lock,
                &FixedIdentitySource("test:7777"),
                7777,
                &info,
            )
            .is_err()
        );
    }
    assert_eq!(store.load().unwrap(), Some(stale));
    assert_eq!(publishes.get(), 0);
    assert!(socket.exists());
    assert!(current.exists());

    std::fs::set_permissions(&current_lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    {
        let ready = fresh_ipc_ready(data, &info);
        let recovery = RecoveryOnlyReady {
            ready: &ready,
            publishes: &publishes,
        };
        let lock = FileInstanceLock {
            path: daemon.join("daemon.lock"),
            held: RefCell::new(None),
        };
        usagi_daemon::usecase::serve::serve(
            &mut Vec::new(),
            &store,
            &recovery,
            &NoGenerationAuthority,
            &ImmediateTestShutdown,
            &AcquiredWorkspaceFence,
            &lock,
            &FixedIdentitySource("test:7777"),
            7777,
            &info,
        )
        .unwrap();
    }
    assert_eq!(publishes.get(), 1);
    assert_eq!(store.load().unwrap(), None);
    assert!(!socket.exists());
    assert!(!current.exists());
    // SAFETY: the listener was not moved or dropped; stale recovery already
    // removed its filesystem endpoint and Drop only closes the descriptor.
    unsafe { ManuallyDrop::drop(&mut listener) };
}

#[test]
fn stale_cleanup_preserves_a_saved_replacement_generation() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let daemon = data.join("daemon");
    let old_listener = SecureUnixListener::bind(data, ipc_generation()).unwrap();
    let replacement_listener = SecureUnixListener::bind(data, ipc_generation()).unwrap();
    let replacement_locator = replacement_listener.locator().clone();
    let replacement_socket = daemon.join(&replacement_locator.endpoint);
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    let old = usagi_core::domain::daemon::DaemonRecord::new(4242);
    let replacement = usagi_core::domain::daemon::DaemonRecord {
        pid: old.pid,
        process_start_identity: old.process_start_identity.clone(),
        started_at: old.started_at + chrono::Duration::nanoseconds(1),
    };
    store.save(&replacement).unwrap();
    let ready = fresh_ipc_ready(data, &info);

    assert_eq!(
        ready.cleanup_if(&store, &old).unwrap(),
        StaleCleanup::Superseded
    );
    assert_eq!(store.load().unwrap(), Some(replacement));
    assert_eq!(
        usagi_daemon::infrastructure::unix_transport::read_locator(&daemon).unwrap(),
        replacement_locator
    );
    assert!(replacement_socket.exists());
    let client = usagi_daemon::infrastructure::unix_transport::connect_current(data).unwrap();
    let accepted = replacement_listener.accept().unwrap();
    drop((client, accepted, old_listener, replacement_listener));
}

#[test]
fn production_stop_reclaims_a_reused_pid_in_socket_first_order_without_signalling() {
    use std::mem::ManuallyDrop;

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let data = directory.path();
    let info = daemon_test_info();
    let daemon = data.join("daemon");
    let mut listener = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
    let socket = daemon.join(&listener.locator().endpoint);
    let current = daemon.join("current.json");
    let socket_alias = socket.with_extension("alias");
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });

    // A live, unrelated process occupies the recorded pid, which is what the
    // OS leaves behind once a crashed owner's pid is handed out again. Only
    // the identity distinguishes it from the owner, so the record is
    // byte-identical to what the crashed daemon wrote apart from that field.
    let mut occupant = Command::new("sleep").arg("30").spawn().unwrap();
    let identity = ExactProcessControl
        .process_start_identity(occupant.id())
        .unwrap();
    let record = DaemonRecord::identified(occupant.id(), format!("{identity}-crashed-owner"));
    store.save(&record).unwrap();
    assert_eq!(
        ExactProcessControl.observe(&record),
        DaemonProcessObservation::IdentityMismatch
    );

    let stop = |ready: &IpcReady<'_>| {
        usagi_daemon::usecase::stop::stop(
            &store,
            &ExactProcessControl,
            &SigtermTerminator,
            &RealSleeper,
            ready,
            &info,
        )
    };

    // A second link to the socket makes its removal unsafe, so the reclaim
    // fails at its first step. The locator surviving that failure is what
    // pins the order: it is the commit fence, retired only after the socket,
    // so this crash point stays retryable through the retained record.
    std::fs::hard_link(&socket, &socket_alias).unwrap();
    let error = stop(&fresh_ipc_ready(data, &info)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        socket.exists(),
        "the socket step failed, so nothing committed"
    );
    assert!(current.exists(), "the locator commits after the socket");
    assert_eq!(store.load().unwrap(), Some(record));
    assert!(
        occupant.try_wait().unwrap().is_none(),
        "a failed reclaim must not signal the process holding the reused pid"
    );

    std::fs::remove_file(&socket_alias).unwrap();
    assert_eq!(
        stop(&fresh_ipc_ready(data, &info)).unwrap(),
        format!("{}: cleared stale daemon record", info.describe())
    );
    assert_eq!(store.load().unwrap(), None);
    assert!(!socket.exists());
    assert!(!current.exists());
    assert!(
        occupant.try_wait().unwrap().is_none(),
        "the reclaim completed with zero signals"
    );

    occupant.kill().unwrap();
    occupant.wait().unwrap();
    // SAFETY: reclaim removed only filesystem artifacts; dropping closes the
    // still-owned listener fd and its cleanup is idempotent.
    unsafe { ManuallyDrop::drop(&mut listener) };
}

#[test]
fn a_record_pid_that_cannot_name_a_process_reaches_no_signal_path() {
    // Neither boundary lets such a value become durable state, and the
    // terminator refuses it even if one were handed to it directly.
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let store = DaemonRecordStore::new(FsRecordFile {
        path: directory.path().join("daemon").join("daemon.json"),
    });
    for pid in [0, 1] {
        let record = DaemonRecord::identified(pid, "forged");
        assert_eq!(
            store.save(&record).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            SigtermTerminator.terminate(&record).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    assert_eq!(store.load().unwrap(), None);
}

#[test]
fn production_stop_preserves_a_superseded_stale_record() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let daemon = directory.path().join("daemon");
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    let record = usagi_core::domain::daemon::DaemonRecord::identified(2_000_000_000, "test:absent");
    store.save(&record).unwrap();

    let error = usagi_daemon::usecase::stop::stop(
        &store,
        &ExactProcessControl,
        &SigtermTerminator,
        &RealSleeper,
        &SupersededCleanup,
        &daemon_test_info(),
    )
    .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(store.load().unwrap(), Some(record));
}

#[test]
fn shutdown_signal_closes_admission_before_wait_consumes_it() {
    const FIXTURE: &str = "USAGI_TEST_EARLY_DAEMON_SHUTDOWN_FLAG";
    if std::env::var_os(FIXTURE).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::daemon::tests::shutdown_signal_closes_admission_before_wait_consumes_it",
                "--nocapture",
            ])
            .env(FIXTURE, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }

    let admission_closed = Arc::new(ShutdownRequest::new());
    let shutdown = SignalShutdown::new(Arc::clone(&admission_closed));
    shutdown.prepare().unwrap();
    signal_hook::low_level::raise(libc::SIGTERM).unwrap();

    assert!(admission_closed.is_requested());
}

#[test]
fn accept_worker_exit_wakes_shutdown_wait_without_an_os_signal() {
    const FIXTURE: &str = "USAGI_TEST_IPC_WORKER_EXIT_WAKE";
    if std::env::var_os(FIXTURE).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::daemon::tests::accept_worker_exit_wakes_shutdown_wait_without_an_os_signal",
                "--nocapture",
            ])
            .env(FIXTURE, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }

    let admission_closed = Arc::new(ShutdownRequest::new());
    let shutdown = SignalShutdown::new(Arc::clone(&admission_closed));
    shutdown.prepare().unwrap();
    let worker_flag = Arc::clone(&admission_closed);
    let worker = std::thread::spawn(move || {
        let _exit = ShutdownOnIpcWorkerExit {
            shutdown: worker_flag,
        };
        panic!("injected accept-worker panic");
    });

    shutdown.wait().unwrap();

    assert!(admission_closed.is_requested());
    assert!(worker.join().is_err());
}

#[test]
fn dropping_a_shutdown_pipe_joins_its_writer_before_closing_descriptors() {
    let shutdown = Arc::new(ShutdownRequest::new());
    let pipe = ShutdownPipe::mirroring(&shutdown).unwrap();
    assert!(!shutdown.is_requested());

    drop(pipe);

    assert!(shutdown.is_requested());
}

struct FixedRefreshClock {
    calls: Arc<AtomicUsize>,
    shutdown_after: Option<(usize, Arc<ShutdownRequest>)>,
}
/// A monotonic clock that stands still, for tests that are not about the
/// verification cache's own timing.
struct StoppedClock(u64);
impl MonotonicClock for StoppedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

/// A cache nothing else has warmed, so an existing test still exercises the real
/// `gh` read rather than a leftover answer from an earlier assertion.
fn fresh_verification_cache()
-> std::sync::Arc<std::sync::Mutex<usagi_daemon::usecase::workflow::VerificationCache>> {
    std::sync::Arc::default()
}

impl MonotonicClock for FixedRefreshClock {
    fn now_ms(&self) -> u64 {
        let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some((after, shutdown)) = &self.shutdown_after
            && call >= *after
        {
            shutdown.request();
        }
        0
    }
}

#[derive(Clone)]
struct CompositionGh {
    calls: Arc<AtomicUsize>,
    inventory: SharedPrInventory,
    unlocked_during_call: Arc<AtomicBool>,
}
impl GhProcessPort for CompositionGh {
    type Error = ();
    fn run(&mut self, _: &str, _: &[String], _: u64) -> Result<String, ()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.unlocked_during_call
            .store(self.inventory.try_lock().is_ok(), Ordering::Release);
        Ok("{\"title\":\"production\",\"state\":\"MERGED\",\"headRefOid\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}".into())
    }
}

fn run_gh_shell(script: String, timeout_ms: u64) -> std::io::Result<String> {
    GhProcess.run("/bin/sh", &["-c".into(), script], timeout_ms)
}

fn repeated_gh_output(bytes: usize, stderr: bool) -> String {
    const CHUNK_BYTES: usize = 4 * 1024;
    let chunk = "x".repeat(CHUNK_BYTES);
    let full_chunks = bytes / CHUNK_BYTES;
    let remainder = bytes % CHUNK_BYTES;
    format!(
        "{}chunk='{chunk}'; i=0; while [ \"$i\" -lt {full_chunks} ]; do printf '%s' \"$chunk\"; i=$((i + 1)); done; printf '%.*s' {remainder} \"$chunk\"",
        if stderr { "exec 1>&2; " } else { "" }
    )
}

#[test]
fn gh_process_normalizes_every_unsafe_observation_without_raw_output() {
    assert_eq!(
        gh_process_result(ChildObservation::Success("public-json".into())).unwrap(),
        "public-json"
    );
    let timeout = gh_process_result(ChildObservation::TimedOut).unwrap_err();
    assert_eq!(timeout.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(timeout.to_string(), "PR provider timed out");

    for observation in [
        ChildObservation::SpawnFailed,
        ChildObservation::ExitFailure,
        ChildObservation::OutputTooLarge,
        ChildObservation::InvalidOutput,
        ChildObservation::EmptyOutput,
        ChildObservation::ObservationFailed,
    ] {
        let error = gh_process_result(observation).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(error.to_string(), "PR provider failed");
    }
}

#[test]
fn gh_process_enforces_each_stream_limit_and_safe_failures() {
    let stdout = run_gh_shell(repeated_gh_output(PR_PROVIDER_OUTPUT_LIMIT, false), 5_000).unwrap();
    assert_eq!(stdout.len(), PR_PROVIDER_OUTPUT_LIMIT);
    assert!(
        run_gh_shell(
            repeated_gh_output(PR_PROVIDER_OUTPUT_LIMIT + 1, false),
            5_000
        )
        .is_err()
    );

    let stderr = run_gh_shell(repeated_gh_output(PR_PROVIDER_OUTPUT_LIMIT, true), 5_000).unwrap();
    assert_eq!(stderr.len(), PR_PROVIDER_OUTPUT_LIMIT);
    assert!(
        run_gh_shell(
            repeated_gh_output(PR_PROVIDER_OUTPUT_LIMIT + 1, true),
            5_000
        )
        .is_err()
    );

    assert!(run_gh_shell("printf '\\377'".into(), 5_000).is_err());
    assert!(run_gh_shell("printf secret; exit 7".into(), 5_000).is_err());

    let started = Instant::now();
    assert!(
        run_gh_shell(
            "trap '' TERM; while :; do printf oversized; done".into(),
            5_000
        )
        .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(1));

    let timeout = run_gh_shell("trap '' TERM; while :; do :; done".into(), 30).unwrap_err();
    assert_eq!(timeout.kind(), std::io::ErrorKind::TimedOut);
}

#[test]
fn gh_process_cleans_a_descendant_holding_its_capture_pipe() {
    let fixture = tempfile::tempdir().unwrap();
    let pid_file = fixture.path().join("descendant-pid");
    let argv = vec![
        "-c".into(),
        "(trap '' TERM; while :; do :; done) & descendant=$!; printf '%s' \"$descendant\" > \"$1\"; printf done"
            .into(),
        "usagi-gh-fixture".into(),
        pid_file.to_string_lossy().into_owned(),
    ];
    let started = Instant::now();
    assert_eq!(GhProcess.run("/bin/sh", &argv, 5_000).unwrap(), "done");
    assert!(started.elapsed() < Duration::from_secs(1));

    let pid = std::fs::read_to_string(pid_file)
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        // SAFETY: signal 0 observes only whether the fixture descendant remains.
        if unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture descendant {pid} was not cleaned up"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn structured_pr_report_enters_the_worker_session_inventory_idempotently() {
    let directory = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let inventory = Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
        PrInventoryStore::new(directory.path()),
        GenerationRole::Active,
    ))));

    project_reported_pr(&inventory, Some(session), Some("#1")).unwrap();
    project_reported_pr(&inventory, None, Some("https://github.com/o/r/pull/1")).unwrap();
    assert!(
        inventory
            .lock()
            .unwrap()
            .snapshot(session)
            .unwrap()
            .entries
            .is_empty()
    );

    project_reported_pr(
        &inventory,
        Some(session),
        Some("https://github.com/o/r/pull/1"),
    )
    .unwrap();
    project_reported_pr(
        &inventory,
        Some(session),
        Some("https://github.com/o/r/pull/1"),
    )
    .unwrap();

    let snapshot = inventory.lock().unwrap().snapshot(session).unwrap();
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(
        snapshot.entries[0].identity.as_url(),
        "https://github.com/o/r/pull/1"
    );
    assert!(snapshot.entries[0].auto_open);
}

#[test]
fn structured_pr_projection_can_retry_after_a_fenced_write_failure() {
    let directory = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let refused = Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
        PrInventoryStore::new(directory.path()),
        GenerationRole::Draining,
    ))));

    let error = project_reported_pr(
        &refused,
        Some(session),
        Some("https://github.com/o/r/pull/1"),
    )
    .unwrap_err();
    assert_eq!(
        error.code,
        usagi_core::infrastructure::ipc::ErrorCode::Unavailable
    );

    let active = Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
        PrInventoryStore::new(directory.path()),
        GenerationRole::Active,
    ))));
    project_reported_pr(
        &active,
        Some(session),
        Some("https://github.com/o/r/pull/1"),
    )
    .unwrap();
    assert_eq!(
        active
            .lock()
            .unwrap()
            .snapshot(session)
            .unwrap()
            .entries
            .len(),
        1
    );
}

#[test]
fn production_pr_worker_rebuilds_publishes_without_locking_and_honors_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let identity =
        usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/493").unwrap();
    let inventory = Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
        PrInventoryStore::new(directory.path()),
        GenerationRole::Active,
    ))));
    inventory
        .lock()
        .unwrap()
        .observe_committed(
            TerminalId::new(),
            Some(session),
            // The newline terminates the candidate. Without it the projector
            // carries the token into the next chunk instead of crediting a
            // token the output may not have finished writing.
            format!("{}\n", identity.as_url()).as_bytes(),
        )
        .unwrap();
    let shutdown = Arc::new(ShutdownRequest::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let unlocked = Arc::new(AtomicBool::new(false));
    let handle = spawn_pr_refresh_worker(
        Arc::clone(&inventory),
        None,
        Arc::clone(&shutdown),
        CompositionGh {
            calls: Arc::clone(&calls),
            inventory: Arc::clone(&inventory),
            unlocked_during_call: Arc::clone(&unlocked),
        },
        FixedRefreshClock {
            calls: Arc::new(AtomicUsize::new(0)),
            shutdown_after: Some((3, Arc::clone(&shutdown))),
        },
        Duration::from_millis(1),
    )
    .unwrap();
    handle.join().unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert!(unlocked.load(Ordering::Acquire));
    let snapshot = inventory.lock().unwrap().snapshot(session).unwrap();
    assert_eq!(snapshot.entries[0].title.as_deref(), Some("production"));

    let cancelled = Arc::new(ShutdownRequest::new());
    cancelled.request();
    let cancelled_calls = Arc::new(AtomicUsize::new(0));
    let handle = spawn_pr_refresh_worker(
        Arc::clone(&inventory),
        None,
        Arc::clone(&cancelled),
        CompositionGh {
            calls: Arc::clone(&cancelled_calls),
            inventory,
            unlocked_during_call: Arc::new(AtomicBool::new(false)),
        },
        FixedRefreshClock {
            calls: Arc::new(AtomicUsize::new(0)),
            shutdown_after: None,
        },
        Duration::from_millis(1),
    )
    .unwrap();
    handle.join().unwrap();
    assert_eq!(cancelled_calls.load(Ordering::Acquire), 0);
}

/// A teardown journal whose pending set drains as it is finalized, plus a
/// scripted effect failure, so the worker's logging arms are both exercised.
struct FakeTeardownJournal {
    pending: Arc<Mutex<Vec<usagi_daemon::usecase::session_teardown::PendingTeardown>>>,
    pending_calls: Arc<AtomicUsize>,
    finalize_error: Option<String>,
}
impl TeardownJournal for FakeTeardownJournal {
    fn pending(&self) -> Vec<usagi_daemon::usecase::session_teardown::PendingTeardown> {
        let pending = self.pending.lock().unwrap().clone();
        self.pending_calls.fetch_add(1, Ordering::AcqRel);
        pending
    }
    fn finish(
        &self,
        teardown: &usagi_daemon::usecase::session_teardown::PendingTeardown,
        _outcome: Result<(), String>,
    ) -> Result<(), String> {
        if let Some(error) = &self.finalize_error {
            return Err(error.clone());
        }
        self.pending
            .lock()
            .unwrap()
            .retain(|pending| pending.name != teardown.name);
        Ok(())
    }
}

struct FakeTeardownEffect {
    torn_down: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<ShutdownRequest>,
    shutdown_after: usize,
}
impl TeardownEffect for FakeTeardownEffect {
    fn tear_down(
        &self,
        teardown: &usagi_daemon::usecase::session_teardown::PendingTeardown,
    ) -> Result<(), String> {
        let mut torn_down = self.torn_down.lock().unwrap();
        torn_down.push(teardown.name.clone());
        if torn_down.len() == self.shutdown_after {
            self.shutdown.request();
        }
        Err("worktree is busy".into())
    }
}

#[test]
fn production_teardown_worker_drains_an_admitted_removal_and_honors_shutdown() {
    let pending = Arc::new(Mutex::new(Vec::new()));
    let pending_calls = Arc::new(AtomicUsize::new(0));
    let shutdown = Arc::new(ShutdownRequest::new());
    let torn_down = Arc::new(Mutex::new(Vec::new()));
    let signal = Arc::new(TeardownSignal::new());

    let handle = spawn_session_teardown_worker(
        FakeTeardownJournal {
            pending: Arc::clone(&pending),
            pending_calls: Arc::clone(&pending_calls),
            finalize_error: Some("session lifecycle owner is unavailable".into()),
        },
        FakeTeardownEffect {
            torn_down: Arc::clone(&torn_down),
            shutdown: Arc::clone(&shutdown),
            shutdown_after: 1,
        },
        Arc::clone(&signal),
        Arc::clone(&shutdown),
        Duration::from_millis(1),
    )
    .unwrap();

    while pending_calls.load(Ordering::Acquire) == 0 {
        std::thread::yield_now();
    }
    pending
        .lock()
        .unwrap()
        .push(usagi_daemon::usecase::session_teardown::PendingTeardown {
            session_id: SessionId::new(),
            operation_id: usagi_core::domain::id::OperationId::new(),
            name: "one".into(),
            repository_root: PathBuf::from("/repo"),
            data_home: PathBuf::from("/data"),
            session_container: PathBuf::from("/repo/.usagi/sessions"),
            session_root: PathBuf::from("/repo/.usagi/sessions/one"),
            force: false,
            delete_branch: false,
            branch_name: None,
            force_delete_branch: false,
            merged_head_oid: None,
        });
    signal.notify();
    handle.join().unwrap();

    assert_eq!(torn_down.lock().unwrap().as_slice(), ["one"]);
    assert_eq!(pending.lock().unwrap().len(), 1);
    assert_eq!(pending_calls.load(Ordering::Acquire), 2);

    // A worker started under shutdown takes no work at all.
    let already_stopped = Arc::new(ShutdownRequest::new());
    already_stopped.request();
    let untouched = Arc::new(Mutex::new(Vec::new()));
    spawn_session_teardown_worker(
        FakeTeardownJournal {
            pending: Arc::clone(&pending),
            pending_calls: Arc::new(AtomicUsize::new(0)),
            finalize_error: None,
        },
        FakeTeardownEffect {
            torn_down: Arc::clone(&untouched),
            shutdown: Arc::clone(&already_stopped),
            shutdown_after: 1,
        },
        signal,
        already_stopped,
        Duration::from_millis(1),
    )
    .unwrap()
    .join()
    .unwrap();
    assert!(untouched.lock().unwrap().is_empty());
}

#[test]
fn production_teardown_worker_does_not_reread_an_idle_journal_on_each_tick() {
    let pending_calls = Arc::new(AtomicUsize::new(0));
    let shutdown = Arc::new(ShutdownRequest::new());
    let handle = spawn_session_teardown_worker(
        FakeTeardownJournal {
            pending: Arc::new(Mutex::new(Vec::new())),
            pending_calls: Arc::clone(&pending_calls),
            finalize_error: None,
        },
        FakeTeardownEffect {
            torn_down: Arc::new(Mutex::new(Vec::new())),
            shutdown: Arc::clone(&shutdown),
            shutdown_after: 1,
        },
        Arc::new(TeardownSignal::new()),
        Arc::clone(&shutdown),
        Duration::from_millis(1),
    )
    .unwrap();

    while pending_calls.load(Ordering::Acquire) == 0 {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(10));
    assert_eq!(pending_calls.load(Ordering::Acquire), 1);

    shutdown.request();
    handle.join().unwrap();
}

#[test]
fn production_teardown_worker_retries_a_failed_finalization_on_the_tick() {
    let pending = Arc::new(Mutex::new(vec![
        usagi_daemon::usecase::session_teardown::PendingTeardown {
            session_id: SessionId::new(),
            operation_id: usagi_core::domain::id::OperationId::new(),
            name: "one".into(),
            repository_root: PathBuf::from("/repo"),
            data_home: PathBuf::from("/data"),
            session_container: PathBuf::from("/repo/.usagi/sessions"),
            session_root: PathBuf::from("/repo/.usagi/sessions/one"),
            force: false,
            delete_branch: false,
            branch_name: None,
            force_delete_branch: false,
            merged_head_oid: None,
        },
    ]));
    let pending_calls = Arc::new(AtomicUsize::new(0));
    let shutdown = Arc::new(ShutdownRequest::new());
    let torn_down = Arc::new(Mutex::new(Vec::new()));

    spawn_session_teardown_worker(
        FakeTeardownJournal {
            pending,
            pending_calls: Arc::clone(&pending_calls),
            finalize_error: Some("session lifecycle owner is unavailable".into()),
        },
        FakeTeardownEffect {
            torn_down: Arc::clone(&torn_down),
            shutdown: Arc::clone(&shutdown),
            shutdown_after: 2,
        },
        Arc::new(TeardownSignal::new()),
        shutdown,
        Duration::from_millis(1),
    )
    .unwrap()
    .join()
    .unwrap();

    assert_eq!(torn_down.lock().unwrap().as_slice(), ["one", "one"]);
    assert_eq!(pending_calls.load(Ordering::Acquire), 2);
}

#[test]
fn automatic_orphan_cleanup_ticks_until_shutdown() {
    let shutdown = Arc::new(ShutdownRequest::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let stop = Arc::clone(&shutdown);
    let worker = spawn_orphan_cleanup_worker(
        move || {
            if observed.fetch_add(1, Ordering::AcqRel) + 1 == 2 {
                stop.request();
            }
        },
        Arc::clone(&shutdown),
        Duration::from_millis(1),
    )
    .unwrap();

    worker.join().unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 2);

    let already_stopped = Arc::new(ShutdownRequest::new());
    already_stopped.request();
    let untouched = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&untouched);
    spawn_orphan_cleanup_worker(
        move || {
            observed.fetch_add(1, Ordering::AcqRel);
        },
        already_stopped,
        Duration::from_millis(1),
    )
    .unwrap()
    .join()
    .unwrap();
    assert_eq!(untouched.load(Ordering::Acquire), 0);
}

#[test]
fn automatic_orphan_cleanup_is_fenced_by_the_active_control_lease() {
    let gate = AdmissionGate::new(DaemonGeneration::new(), GenerationRole::Active);
    let lease = active_cleanup_lease(&gate).expect("active generation admits cleanup");
    assert_eq!(gate.outstanding(LeaseClass::ActiveControl), 1);
    drop(lease);
    assert_eq!(gate.outstanding(LeaseClass::ActiveControl), 0);

    gate.close(LeaseClass::ActiveControl);
    gate.await_drain(LeaseClass::ActiveControl).unwrap();
    assert!(active_cleanup_lease(&gate).is_none());
}

/// Prepares `<data>/daemon` with an acquired instance lock and a registered
/// owner record, exactly as `serve` leaves it before publishing, and returns
/// the production custody probe built from that state.
fn custody_fixture(data_dir: &Path) -> (FileInstanceLock, DaemonRecord, FsCustodyProbe) {
    let daemon_dir = data_dir.join("daemon");
    ensure_private_dir_all(&daemon_dir).unwrap();
    let lock = FileInstanceLock {
        path: daemon_dir.join("daemon.lock"),
        held: RefCell::new(None),
    };
    assert!(lock.acquire().unwrap());
    let record = FsRecordFile {
        path: daemon_dir.join("daemon.json"),
    };
    let owner = DaemonRecord::identified(std::process::id(), "custody:test");
    DaemonRecordStore::new(FsRecordFile {
        path: daemon_dir.join("daemon.json"),
    })
    .save(&owner)
    .unwrap();
    let probe = FsCustodyProbe {
        locked: lock.locked_inode(),
        lock_path: daemon_dir.join("daemon.lock"),
        record,
    };
    (lock, owner, probe)
}

fn custody_worker(
    probe: FsCustodyProbe,
    owner: DaemonRecord,
    data_dir: &Path,
    shutdown: &Arc<ShutdownRequest>,
) -> std::thread::JoinHandle<()> {
    spawn_custody_worker(
        probe,
        owner,
        data_dir.to_path_buf(),
        AdmissionGate::new(DaemonGeneration::new(), GenerationRole::Active),
        Arc::clone(shutdown),
        Duration::from_millis(5),
    )
    .unwrap()
}

fn wait_for_request(shutdown: &ShutdownRequest, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if shutdown.is_requested() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    shutdown.is_requested()
}

#[test]
fn production_custody_probe_observes_the_locked_inode_and_the_owner_record() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let (lock, owner, probe) = custody_fixture(home.path());
    let daemon_dir = home.path().join("daemon");

    assert_eq!(
        usagi_daemon::usecase::custody::evaluate(&probe, &owner).unwrap(),
        Custody::Held
    );

    // Replacing the pathname cannot forge the identity: it is read from the
    // descriptor this process locked, not from the path.
    let replacement = daemon_dir.join("replacement.lock");
    std::fs::write(&replacement, "").unwrap();
    std::fs::rename(&replacement, daemon_dir.join("daemon.lock")).unwrap();
    assert_eq!(
        usagi_daemon::usecase::custody::evaluate(&probe, &owner).unwrap(),
        Custody::Lost(usagi_daemon::usecase::custody::CustodyLoss::LockInodeReplaced)
    );
    drop(lock);

    // A malformed record is an undecidable observation, never a loss.
    std::fs::write(daemon_dir.join("daemon.json"), "not json").unwrap();
    std::fs::remove_file(daemon_dir.join("daemon.lock")).unwrap();
    std::fs::write(daemon_dir.join("daemon.lock"), "").unwrap();
    let unobserved = FsCustodyProbe {
        locked: None,
        lock_path: daemon_dir.join("daemon.lock"),
        record: FsRecordFile {
            path: daemon_dir.join("daemon.json"),
        },
    };
    assert!(usagi_daemon::usecase::custody::evaluate(&unobserved, &owner).is_err());
}

#[test]
fn production_custody_worker_requests_shutdown_when_the_lock_path_disappears() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let (lock, owner, probe) = custody_fixture(home.path());
    let shutdown = Arc::new(ShutdownRequest::new());
    let handle = custody_worker(probe, owner, home.path(), &shutdown);

    // A live daemon keeps serving across ticks.
    assert!(!wait_for_request(&shutdown, Duration::from_millis(50)));

    std::fs::remove_file(home.path().join("daemon/daemon.lock")).unwrap();
    assert!(wait_for_request(&shutdown, Duration::from_secs(5)));
    handle.join().unwrap();
    drop(lock);
}

#[test]
fn production_custody_worker_requests_shutdown_when_another_owner_takes_the_record() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let (lock, owner, probe) = custody_fixture(home.path());
    let shutdown = Arc::new(ShutdownRequest::new());
    let handle = custody_worker(probe, owner, home.path(), &shutdown);

    DaemonRecordStore::new(FsRecordFile {
        path: home.path().join("daemon/daemon.json"),
    })
    .save(&DaemonRecord::identified(4321, "custody:replacement"))
    .unwrap();
    assert!(wait_for_request(&shutdown, Duration::from_secs(5)));
    handle.join().unwrap();
    drop(lock);
}

#[test]
fn draining_custody_ignores_the_record_transferred_to_its_successor() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let (lock, owner, probe) = custody_fixture(home.path());
    let shutdown = Arc::new(ShutdownRequest::new());
    let gate = AdmissionGate::new(DaemonGeneration::new(), GenerationRole::Active);
    gate.close(LeaseClass::ActiveControl);
    gate.await_drain(LeaseClass::ActiveControl).unwrap();
    gate.enter_draining().unwrap();
    let handle = spawn_custody_worker(
        probe,
        owner,
        home.path().to_path_buf(),
        gate,
        Arc::clone(&shutdown),
        Duration::from_millis(5),
    )
    .unwrap();

    DaemonRecordStore::new(FsRecordFile {
        path: home.path().join("daemon/daemon.json"),
    })
    .save(&DaemonRecord::identified(4321, "custody:successor"))
    .unwrap();
    assert!(!wait_for_request(&shutdown, Duration::from_millis(50)));
    shutdown.request();
    handle.join().unwrap();
    drop(lock);
}

#[test]
fn production_custody_worker_stops_at_an_already_requested_shutdown() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let (lock, owner, probe) = custody_fixture(home.path());
    let shutdown = Arc::new(ShutdownRequest::new());
    shutdown.request();
    custody_worker(probe, owner, home.path(), &shutdown)
        .join()
        .unwrap();
    // The record and lock were left untouched by the supervisor itself.
    assert!(home.path().join("daemon/daemon.json").is_file());
    drop(lock);
}

#[test]
fn a_deleted_data_directory_makes_endpoint_and_record_cleanup_a_successful_no_op() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let data_dir = home.path().join("local");
    let (lock, owner, _) = custody_fixture(&data_dir);
    let record = FsRecordFile {
        path: data_dir.join("daemon/daemon.json"),
    };
    let contents = serde_json::to_string(&owner).unwrap();
    let info = daemon_test_info();
    let ready = fresh_ipc_ready(&data_dir, &info);

    drop(lock);
    std::fs::remove_dir_all(&data_dir).unwrap();

    // Neither step re-creates the released tree, and both succeed so the
    // daemon exits through its ordinary path rather than failing closed.
    DaemonReady::retire(&ready).unwrap();
    assert!(!RecordFile::remove_if(&record, &contents).unwrap());
    assert!(!data_dir.exists());
}

#[test]
fn accepted_stream_observes_peer_close_behind_buffered_data() {
    let (server, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let connection = AcceptedStream::new(server);

    std::io::Write::write_all(&mut peer, b"pipelined request").unwrap();
    drop(peer);

    // The close is observed through `poll`, so it lands when the kernel has
    // processed the peer's exit rather than when `drop` returns. A single
    // sample turns that ordinary scheduling delay into a failure on a loaded
    // machine, so the observation is driven until it lands.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !connection.peer_disconnected() {
        assert!(
            Instant::now() < deadline,
            "the peer close was never observed"
        );
        std::thread::yield_now();
    }
}

#[test]
fn decision_maintenance_never_writes_when_nothing_is_due_and_honors_shutdown() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let daemon = home.path().join("daemon");
    std::fs::create_dir_all(&daemon).unwrap();
    let decisions = Arc::new(UserDecisionStore::new(daemon.clone()));
    let store_path = decisions.path();
    let shutdown = Arc::new(ShutdownRequest::new());
    let stopper = Arc::clone(&shutdown);

    let handle = spawn_decision_maintenance(decisions, shutdown, Duration::from_millis(1)).unwrap();
    // Let several ticks run, then stop: the worker must observe the request
    // rather than needing its tick to be short.
    std::thread::sleep(Duration::from_millis(30));
    stopper.request();
    handle.join().unwrap();

    // An idle tick decides "nothing is due" from a lock-free read, so it must
    // not have created the durable document at all — no fsync, no store lock.
    assert!(
        !store_path.exists(),
        "an idle maintenance tick must not write the decision store"
    );
    assert!(
        !daemon
            .join(usagi_core::infrastructure::persistence::store_lock::LOCK_FILE_NAME)
            .exists(),
        "an idle maintenance tick must not take the store lock"
    );
}

#[test]
fn decision_maintenance_stops_at_an_already_requested_shutdown() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let daemon = home.path().join("daemon");
    std::fs::create_dir_all(&daemon).unwrap();
    let shutdown = Arc::new(ShutdownRequest::new());
    shutdown.request();
    spawn_decision_maintenance(
        Arc::new(UserDecisionStore::new(daemon)),
        shutdown,
        Duration::from_secs(30),
    )
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn the_workflow_lane_ticks_until_shutdown_and_never_sweeps_once_down() {
    let shutdown = Arc::new(ShutdownRequest::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let ticking = Arc::clone(&calls);
    let stopper = Arc::clone(&shutdown);
    let handle = spawn_workflow_lane(
        Box::new(move || {
            if ticking.fetch_add(1, Ordering::AcqRel) >= 1 {
                stopper.request();
            }
        }),
        Arc::clone(&shutdown),
        Duration::from_millis(1),
    )
    .unwrap();
    handle.join().unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 2);

    // A daemon already shutting down never sweeps.
    let cancelled = Arc::new(ShutdownRequest::new());
    cancelled.request();
    let skipped = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&skipped);
    spawn_workflow_lane(
        Box::new(move || {
            counter.fetch_add(1, Ordering::AcqRel);
        }),
        cancelled,
        Duration::from_millis(1),
    )
    .unwrap()
    .join()
    .unwrap();
    assert_eq!(skipped.load(Ordering::Acquire), 0);
}

#[test]
fn the_retention_collector_ticks_until_shutdown_and_stops_when_already_down() {
    let shutdown = Arc::new(ShutdownRequest::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let ticking = Arc::clone(&calls);
    let stopper = Arc::clone(&shutdown);
    let handle = spawn_retention_gc_worker(
        move || {
            if ticking.fetch_add(1, Ordering::AcqRel) >= 1 {
                stopper.request();
            }
        },
        Arc::clone(&shutdown),
        Duration::from_millis(1),
    )
    .unwrap();
    handle.join().unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 2);

    // A daemon already shutting down never collects.
    let cancelled = Arc::new(ShutdownRequest::new());
    cancelled.request();
    let skipped = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&skipped);
    let handle = spawn_retention_gc_worker(
        move || {
            counter.fetch_add(1, Ordering::AcqRel);
        },
        cancelled,
        Duration::from_millis(1),
    )
    .unwrap();
    handle.join().unwrap();
    assert_eq!(skipped.load(Ordering::Acquire), 0);
}

#[test]
fn a_panicked_background_worker_reports_danger_and_requests_shutdown() {
    use usagi_core::infrastructure::ipc::MetricsAction;
    use usagi_tui::usecase::application::daemon_health::{
        DaemonHealth, DaemonHealthTracker, HealthReason,
    };

    let shutdown = Arc::new(ShutdownRequest::new());
    let handle = spawn_retention_gc_worker(
        || panic!("injected retention worker panic"),
        Arc::clone(&shutdown),
        Duration::from_secs(30),
    )
    .unwrap();
    assert!(handle.join().is_err());
    assert!(shutdown.is_requested());

    let broker = Arc::new(Mutex::new(MetricsBroker::with_runtime_health(
        AgentConcurrencyGauge::default(),
        shutdown.background_worker_health(),
    )));
    let sampler = Arc::new(Mutex::new(ProcessResourceSampler { previous: None }));
    let pipeline = TerminalPipelineMetrics::default();
    let mut observer = None;
    let snapshot = metrics_response(
        &broker,
        &sampler,
        &pipeline,
        &mut observer,
        MetricsAction::Snapshot,
    );
    assert_eq!(snapshot.failed_background_workers, 1);

    let mut tracker = DaemonHealthTracker::default();
    tracker.observe(&snapshot);
    assert_eq!(
        tracker.evaluate(i64::try_from(snapshot.sampled_at_ms).unwrap()),
        DaemonHealth::Danger(HealthReason::BackgroundWorkerStopped)
    );
}

#[test]
fn every_critical_worker_unexpected_return_requests_shutdown_and_closes_its_source() {
    for worker in [
        BackgroundWorker::AgentObserver,
        BackgroundWorker::TerminalObserver,
        BackgroundWorker::PrProjection,
    ] {
        let shutdown = Arc::new(ShutdownRequest::new());
        let closed = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&closed);
        let handle = spawn_critical_worker(
            "injected-critical-worker",
            worker,
            Arc::clone(&shutdown),
            move || observed.store(true, Ordering::Release),
            |_| {},
        )
        .unwrap();

        handle.join().unwrap();
        assert!(
            shutdown.is_requested(),
            "{worker:?} did not stop the daemon"
        );
        assert!(
            closed.load(Ordering::Acquire),
            "{worker:?} source stayed open"
        );
        assert_eq!(shutdown.background_worker_health().failed_count(), 1);
    }
}

#[test]
fn every_critical_worker_panic_is_recorded_before_unwind_and_requests_shutdown() {
    for worker in [
        BackgroundWorker::AgentObserver,
        BackgroundWorker::TerminalObserver,
        BackgroundWorker::PrProjection,
    ] {
        let shutdown = Arc::new(ShutdownRequest::new());
        let closed = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&closed);
        let handle = spawn_critical_worker(
            "injected-critical-worker",
            worker,
            Arc::clone(&shutdown),
            move || observed.store(true, Ordering::Release),
            move |_| panic!("injected {worker:?} panic"),
        )
        .unwrap();

        assert!(handle.join().is_err());
        assert!(
            shutdown.is_requested(),
            "{worker:?} did not stop the daemon"
        );
        assert!(
            closed.load(Ordering::Acquire),
            "{worker:?} source stayed open"
        );
        assert_eq!(shutdown.background_worker_health().failed_count(), 1);
    }
}

#[test]
fn standby_client_panic_reaches_only_the_process_shutdown_domain() {
    let process = Arc::new(ShutdownRequest::new());
    let shutdown = StandbyShutdownDomains::new(Arc::clone(&process));
    let panic = panic::catch_unwind(AssertUnwindSafe({
        let shutdown = shutdown.clone();
        move || {
            let _guard = shutdown.process_panic_guard();
            panic!("injected standby client panic");
        }
    }));
    assert!(panic.is_err());
    assert!(process.is_requested());
    assert!(!shutdown.replacement.is_requested());
}

#[test]
fn a_clean_panic_guard_does_not_request_shutdown() {
    let completed = Arc::new(ShutdownRequest::new());
    {
        let _guard = ShutdownOnWorkerPanic {
            shutdown: Arc::clone(&completed),
        };
    }
    assert!(!completed.is_requested());
}

#[test]
fn standby_accept_exit_distinguishes_promotion_from_failure() {
    let unavailable_process = Arc::new(ShutdownRequest::new());
    let unavailable = StandbyShutdownDomains::new(Arc::clone(&unavailable_process));
    let error = unavailable
        .accept_lifetime(|_| Err(std::io::Error::other("injected wake failure")))
        .err()
        .unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert!(unavailable_process.is_requested());
    assert!(!unavailable.replacement.is_requested());

    let unexpected_process = Arc::new(ShutdownRequest::new());
    let unexpected = StandbyShutdownDomains::new(Arc::clone(&unexpected_process));
    {
        let mut lifetime = unexpected.accept_lifetime(ShutdownPipe::mirroring).unwrap();
        // The production loop reaches the same completion call after a poll
        // error, but an absent replacement request must keep it unexpected.
        lifetime.finish_planned();
    }
    assert!(unexpected_process.is_requested());

    let promoted_process = Arc::new(ShutdownRequest::new());
    let promoted = StandbyShutdownDomains::new(Arc::clone(&promoted_process));
    {
        let mut lifetime = promoted.accept_lifetime(ShutdownPipe::mirroring).unwrap();
        promoted.request_replacement();
        assert!(!lifetime.wake.wait_for_listener(-1));
        lifetime.finish_planned();
    }
    assert!(promoted.replacement.is_requested());
    assert!(!promoted_process.is_requested());
}

#[test]
fn an_unfinished_pty_watcher_requests_shutdown() {
    let shutdown = Arc::new(ShutdownRequest::new());
    {
        let _lifecycle = ShutdownOnUnexpectedWorkerExit::new(Arc::clone(&shutdown));
    }
    assert!(shutdown.is_requested());
}

#[test]
fn planned_critical_worker_shutdown_joins_without_a_health_failure() {
    let shutdown = Arc::new(ShutdownRequest::new());
    let handle = spawn_critical_worker(
        "planned-critical-worker",
        BackgroundWorker::AgentObserver,
        Arc::clone(&shutdown),
        || panic!("planned shutdown must not run failure cleanup"),
        ShutdownRequest::wait_until_requested,
    )
    .unwrap();

    shutdown.request();
    handle.join().unwrap();
    assert_eq!(shutdown.background_worker_health().failed_count(), 0);
}

#[test]
fn lifecycle_owner_closes_sources_and_joins_every_critical_worker() {
    let shutdown = Arc::new(ShutdownRequest::new());
    let health = shutdown.background_worker_health();
    let projection = Arc::new(PrProjectionQueue::new());
    let joined = Arc::new(AtomicUsize::new(0));
    let mut workers = DaemonBackgroundWorkers::new(shutdown, Arc::clone(&projection));

    for worker in [
        BackgroundWorker::AgentObserver,
        BackgroundWorker::TerminalObserver,
        BackgroundWorker::PrProjection,
    ] {
        let completed = Arc::clone(&joined);
        workers.push(
            spawn_critical_worker(
                "planned-critical-worker",
                worker,
                Arc::clone(&workers.shutdown),
                || panic!("planned shutdown must not run failure cleanup"),
                move |shutdown| {
                    shutdown.wait_until_requested();
                    completed.fetch_add(1, Ordering::AcqRel);
                },
            )
            .unwrap(),
        );
    }

    drop(workers);
    assert_eq!(joined.load(Ordering::Acquire), 3);
    assert_eq!(health.failed_count(), 0);
    assert_eq!(projection.recv(), None);
}

#[test]
fn the_draining_collector_retries_observations_and_never_outlives_shutdown() {
    let shutdown = Arc::new(ShutdownRequest::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let handle = spawn_draining_collection_worker(
        move || observed.fetch_add(1, Ordering::AcqRel) >= 1,
        Arc::clone(&shutdown),
        Duration::from_millis(1),
    )
    .unwrap();
    handle.join().unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 2);
    assert!(shutdown.is_requested());

    // Tests do not leave the product worker parked on a fixed sleep: an
    // already-observed shutdown makes it return without another collection
    // observation, and the handle is always joined.
    let skipped = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&skipped);
    let handle = spawn_draining_collection_worker(
        move || {
            counter.fetch_add(1, Ordering::AcqRel);
            false
        },
        shutdown,
        Duration::from_secs(30),
    )
    .unwrap();
    handle.join().unwrap();
    assert_eq!(skipped.load(Ordering::Acquire), 0);
}

fn session_test_hello() -> usagi_core::infrastructure::ipc::ServerHello {
    use usagi_core::infrastructure::ipc::{
        BuildIdentity, ConnectionId, DaemonGeneration, GenerationRole, ProtocolLimits,
        ProtocolVersion,
    };
    usagi_core::infrastructure::ipc::ServerHello {
        connection_nonce: "test".into(),
        connection_id: ConnectionId("connection".into()),
        daemon_generation: DaemonGeneration("generation".into()),
        generation_role: GenerationRole::Active,
        protocol: ProtocolVersion {
            generation: 1,
            revision: 0,
        },
        capabilities: vec![],
        build: BuildIdentity {
            version: "test".into(),
            commit: "test".into(),
            target: "test".into(),
            artifact: "test-artifact".into(),
        },
        limits: ProtocolLimits::default(),
        daemon_process: None,
    }
}

fn metrics_response(
    broker: &SharedMetricsBroker,
    sampler: &SharedProcessResourceSampler,
    pipeline: &TerminalPipelineMetrics,
    observer: &mut Option<MetricsObserver>,
    action: usagi_core::infrastructure::ipc::MetricsAction,
) -> usagi_core::infrastructure::ipc::DaemonMetrics {
    use usagi_core::infrastructure::ipc::DaemonRequest;
    use usagi_core::infrastructure::ipc::{EnvelopeKind, ResponseOutcome};

    let response = dispatch_metrics(
        broker,
        sampler,
        pipeline,
        observer,
        usagi_core::infrastructure::ipc::RequestId("metrics".into()),
        &serde_json::to_value(DaemonRequest::Metrics { action }).unwrap(),
        &session_test_hello(),
    );
    let EnvelopeKind::Response { outcome, body, .. } = response.kind else {
        panic!("metrics dispatch must produce a response")
    };
    assert_eq!(outcome, ResponseOutcome::Ok);
    serde_json::from_value(body).unwrap()
}

#[test]
fn production_snapshot_polling_does_not_drop_but_a_slow_observer_does() {
    use usagi_core::infrastructure::ipc::MetricsAction;

    let broker = Arc::new(Mutex::new(MetricsBroker::default()));
    let sampler = Arc::new(Mutex::new(ProcessResourceSampler { previous: None }));
    let pipeline = TerminalPipelineMetrics::default();
    let mut snapshot_client = None;
    for _ in 0..4 {
        let snapshot = metrics_response(
            &broker,
            &sampler,
            &pipeline,
            &mut snapshot_client,
            MetricsAction::Snapshot,
        );
        assert_eq!(snapshot.active_subscribers, 0);
        assert_eq!(snapshot.dropped_updates, 0);
    }

    let mut slow = None;
    assert_eq!(
        metrics_response(
            &broker,
            &sampler,
            &pipeline,
            &mut slow,
            MetricsAction::Subscribe,
        )
        .active_subscribers,
        1
    );
    metrics_response(
        &broker,
        &sampler,
        &pipeline,
        &mut snapshot_client,
        MetricsAction::Snapshot,
    );
    let dropped = metrics_response(
        &broker,
        &sampler,
        &pipeline,
        &mut snapshot_client,
        MetricsAction::Snapshot,
    );
    assert_eq!(dropped.dropped_updates, 1);

    let disconnected = slow.take().unwrap();
    broker
        .lock()
        .unwrap()
        .unsubscribe(disconnected.subscription());
    assert_eq!(broker.lock().unwrap().snapshot().active_subscribers, 0);

    let restarted = Arc::new(Mutex::new(MetricsBroker::default()));
    let restarted_sampler = Arc::new(Mutex::new(ProcessResourceSampler { previous: None }));
    let restarted_snapshot = metrics_response(
        &restarted,
        &restarted_sampler,
        &pipeline,
        &mut snapshot_client,
        MetricsAction::Snapshot,
    );
    assert_eq!(restarted_snapshot.active_subscribers, 0);
    assert_eq!(restarted_snapshot.dropped_updates, 0);
}

/// The metrics reply carries the Agent concurrency the daemon's own admission
/// authority published, and it reads that level **without** the Agent runtime
/// lock: a display-only tick may never wait behind a launch (#644).
#[test]
fn production_metrics_report_agent_concurrency_without_taking_the_agent_lock() {
    use usagi_core::infrastructure::ipc::{AgentConcurrency, MetricsAction};

    let gauge = AgentConcurrencyGauge::default();
    let broker = Arc::new(Mutex::new(MetricsBroker::with_agent_concurrency(
        gauge.clone(),
    )));
    let sampler = Arc::new(Mutex::new(ProcessResourceSampler { previous: None }));
    let pipeline = TerminalPipelineMetrics::default();
    let mut client = None;

    // Before any authority publishes, the reply says "unknown" rather than an
    // idle zero, and it declares the schema that carries the projection.
    let unknown = metrics_response(
        &broker,
        &sampler,
        &pipeline,
        &mut client,
        MetricsAction::Subscribe,
    );
    assert_eq!(unknown.agent_concurrency, None);
    assert_eq!(unknown.schema_version, 4);

    // What the authority publishes is what the reply reports, on the very next
    // request and without another sample being pushed.
    gauge.publish(2, AGENT_RUNTIME_LIMIT);
    let reported = metrics_response(
        &broker,
        &sampler,
        &pipeline,
        &mut client,
        MetricsAction::Snapshot,
    );
    assert_eq!(
        reported.agent_concurrency,
        Some(AgentConcurrency {
            in_use: 2,
            limit: u32::try_from(AGENT_RUNTIME_LIMIT).unwrap(),
        })
    );

    // The reply is produced while another thread holds the Agent runtime's
    // authority. `dispatch_metrics` has no access to that runtime by
    // construction; this bounds the regression that would give it one, so a
    // launch could no longer stall the mascot's metrics tick.
    let authority = Arc::new(Mutex::new(()));
    let held = Arc::clone(&authority);
    let (holding, holds) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let guard = held.lock().expect("fresh mutex");
        holding.send(()).expect("the test waits for the held lock");
        released.recv().expect("the test releases the lock");
        drop(guard);
    });
    holds.recv().expect("the authority is held");
    let (answered, answer) = mpsc::channel();
    let replying = std::thread::spawn(move || {
        let mut isolated = None;
        let reply = metrics_response(
            &broker,
            &sampler,
            &TerminalPipelineMetrics::default(),
            &mut isolated,
            MetricsAction::Snapshot,
        );
        let _ = answered.send(reply);
    });
    let reply = answer
        .recv_timeout(Duration::from_secs(10))
        .expect("a metrics reply never waits on the Agent authority");
    assert_eq!(
        reply.agent_concurrency,
        Some(AgentConcurrency {
            in_use: 2,
            limit: u32::try_from(AGENT_RUNTIME_LIMIT).unwrap(),
        })
    );
    replying.join().expect("the reply thread finished");
    release.send(()).expect("the holder is still waiting");
    holder.join().expect("the holder released the authority");
}

#[test]
fn failed_create_and_remove_replay_as_error_envelopes_without_success_hooks() {
    use usagi_core::infrastructure::ipc::SessionAction;
    use usagi_core::infrastructure::ipc::{EnvelopeKind, ErrorCode, ResponseOutcome};

    for action in [SessionAction::Create, SessionAction::Remove] {
        let response = session_response_envelope(
            action,
            Err(SessionRuntimeError::DurableFailure(
                "durable session failure".into(),
            )),
            usagi_core::infrastructure::ipc::RequestId("request".into()),
            &session_test_hello(),
        );
        let EnvelopeKind::Response { outcome, body, .. } = response.kind else {
            panic!("session dispatch must produce a response")
        };
        assert_eq!(body, serde_json::Value::Null);
        let ResponseOutcome::Error(error) = outcome else {
            panic!("failed session replay must not be accepted")
        };
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(error.message, "durable session failure");
        assert!(body.get("hook").is_none());
    }
}

#[test]
fn only_an_exact_merged_pr_head_authorizes_squash_branch_deletion() {
    use usagi_core::domain::pr_inventory::{PrInventory, PrState, canonicalize};

    let session = SessionId::new();
    let identity = canonicalize("https://github.com/o/r/pull/1").unwrap();
    let head = "a".repeat(40);
    let mut inventory = PrInventory::default();
    inventory.discover([identity.clone()]);
    inventory.entries.get_mut(&identity).unwrap().state = PrState::Merged;
    inventory.entries.get_mut(&identity).unwrap().head_oid = Some(head.clone());
    let snapshot = usagi_core::infrastructure::ipc::PrSnapshot::from((session, inventory.clone()));
    assert_eq!(
        exact_merged_pr_head(Some(snapshot), Some(head.clone())),
        Some(head.clone())
    );

    inventory.entries.get_mut(&identity).unwrap().state = PrState::Open;
    assert_eq!(
        exact_merged_pr_head(
            Some(usagi_core::infrastructure::ipc::PrSnapshot::from((
                session,
                inventory.clone()
            ))),
            Some(head.clone())
        ),
        None
    );
    inventory.entries.get_mut(&identity).unwrap().state = PrState::Merged;
    assert_eq!(
        exact_merged_pr_head(
            Some(usagi_core::infrastructure::ipc::PrSnapshot::from((
                session, inventory
            ))),
            Some("b".repeat(40))
        ),
        None
    );
    assert_eq!(
        exact_merged_pr_head(
            Some(usagi_core::infrastructure::ipc::PrSnapshot::from((
                session,
                PrInventory::default()
            ))),
            None
        ),
        None
    );
    assert_eq!(exact_merged_pr_head(None, Some(head)), None);
}

#[test]
fn unavailable_pr_inventory_falls_back_to_safe_branch_deletion() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("pr-inventory.json"), "not json").unwrap();
    let inventory = Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
        PrInventoryStore::new(directory.path()),
        GenerationRole::Active,
    ))));

    assert_eq!(
        best_effort_merged_pr_head(&inventory, SessionId::new(), Some("a".repeat(40))),
        None
    );
}

#[test]
fn product_mcp_arguments_start_usagi_mcp_from_the_daemon_binary() {
    let command = Path::new("/opt/usagi/bin/usagi");

    let codex = codex_integration_arguments(command).unwrap();
    assert_eq!(
        &codex[..12],
        [
            "-c",
            "mcp_servers.usagi.command = \"/opt/usagi/bin/usagi\"",
            "-c",
            "mcp_servers.usagi.args = [\"mcp\"]",
            "-c",
            "mcp_servers.usagi.env_vars = [\"USAGI_HOME\", \"USAGI_RUNTIME_MODE\", \"USAGI_WORKSPACE_ROOT\"]",
            "-c",
            "mcp_servers.usagi.required = true",
            "-c",
            "mcp_servers.usagi.default_tools_approval_mode = \"approve\"",
            "-c",
            "features.hooks = true",
        ]
    );
    assert_eq!(codex.len(), 24);
    for (event, phase) in AGENT_PHASE_HOOK_EVENTS {
        if matches!(event, "Notification" | "PermissionRequest") {
            assert!(
                !codex
                    .iter()
                    .any(|argument| argument.starts_with(&format!("hooks.{event}")))
            );
            continue;
        }
        let assignment = codex
            .iter()
            .find(|argument| argument.starts_with(&format!("hooks.{event} = ")))
            .unwrap_or_else(|| panic!("missing Codex {event} hook"));
        assert!(
            assignment.contains(&format!("agent-phase {}", phase.as_token())),
            "{assignment}"
        );
    }
    let session_start = codex
        .iter()
        .find(|argument| argument.starts_with("hooks.SessionStart = "))
        .unwrap();
    assert!(session_start.contains("agent-phase ready"));
    assert!(!session_start.contains("matcher"));
    assert!(!session_start.contains("codex-session-capture"));
    let session_end = codex
        .iter()
        .find(|argument| argument.starts_with("hooks.SessionEnd = "))
        .unwrap();
    assert!(session_end.contains("timeout = 3"));
    assert_eq!(
        claude_mcp_arguments(command).unwrap(),
        [
            "--mcp-config",
            r#"{"mcpServers":{"usagi":{"args":["mcp"],"command":"/opt/usagi/bin/usagi"}}}"#,
            "--allowedTools",
            "mcp__usagi",
        ]
    );
}

#[cfg(unix)]
#[test]
fn rendered_codex_hook_toml_parses_and_executes_every_wired_command() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = tempfile::tempdir().unwrap();
    let bin = fixture.path().join("hook bin");
    std::fs::create_dir(&bin).unwrap();
    let command = bin.join("usagi'fixture");
    let log = fixture.path().join("hook.log");
    std::fs::write(
        &command,
        "#!/bin/sh\npayload=$(cat)\nprintf '%s|%s\\n' \"$*\" \"$payload\" >> \"$USAGI_HOOK_TEST_LOG\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();

    let arguments = codex_integration_arguments(&command).unwrap();
    let (pairs, remainder) = arguments.as_chunks::<2>();
    assert!(remainder.is_empty());
    let assignments = pairs
        .iter()
        .filter_map(|pair| pair[1].strip_prefix("hooks.").map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(assignments.len(), 6);

    for assignment in assignments {
        let (event_and_key, groups) = assignment.split_once(" = ").unwrap();
        let event = event_and_key.trim();
        let document = toml::from_str::<toml::Value>(&format!("value = {groups}"))
            .unwrap_or_else(|error| panic!("invalid generated {event} TOML: {error}"));
        let groups = document["value"]
            .as_array()
            .expect("hook groups are an array");
        for group in groups {
            let hooks = group["hooks"].as_array().expect("group hooks are an array");
            for hook in hooks {
                let rendered = hook["command"].as_str().expect("command hook");
                let mut child = Command::new("/bin/sh")
                    .args(["-c", rendered])
                    .env("USAGI_HOOK_TEST_LOG", &log)
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                write!(
                    child.stdin.take().unwrap(),
                    "{{\"hook_event_name\":\"{event}\",\"source\":\"startup\",\"session_id\":\"fixture-session\"}}"
                )
                .unwrap();
                assert!(child.wait().unwrap().success(), "{event}: {rendered}");
            }
        }
    }

    let calls = std::fs::read_to_string(log).unwrap();
    for expected in [
        "agent-phase ready|",
        "agent-phase running|",
        "agent-phase waiting|",
        "agent-phase ended|",
        "agent-phase exited|",
    ] {
        assert!(calls.contains(expected), "missing {expected}: {calls}");
    }
    assert_eq!(calls.lines().count(), 6, "{calls}");
}

#[test]
fn removed_local_llm_setting_is_ignored_before_daemon_provisioning() {
    let base = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // Production selects the base itself, so its settings file is the base's.
    let data_home = paths::DataHome::new(base.path(), paths::RuntimeMode::Production);
    let storage = Storage::new(data_home.selected());
    let tools = configured_mcp_tools(&data_home, workspace.path()).unwrap();
    assert_eq!(
        tools.families,
        McpToolFamilies {
            issue: true,
            memory: true,
        }
    );

    std::fs::create_dir_all(storage.dir()).unwrap();
    std::fs::write(
        storage.dir().join("settings.json"),
        r#"{"local_llm":{"enabled":true,"model":"qwen2.5-coder:7b"}}"#,
    )
    .unwrap();
    let tools = configured_mcp_tools(&data_home, workspace.path()).unwrap();
    assert_eq!(
        tools.families,
        McpToolFamilies {
            issue: true,
            memory: true,
        }
    );
}

#[test]
fn tool_families_follow_the_registered_workspace_and_fail_closed_when_unreadable() {
    use usagi_core::domain::settings::LocalSettings;

    let base = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let data_home = paths::DataHome::new(base.path(), paths::RuntimeMode::Production);
    let store = WorkspaceSettingsStore::new(workspace.path());

    // The workspace layer decides, exactly as it does for `usagi mcp`.
    store
        .save(&LocalSettings {
            issue_enabled: Some(false),
            ..LocalSettings::default()
        })
        .unwrap();
    assert_eq!(
        configured_mcp_tools(&data_home, workspace.path())
            .unwrap()
            .families,
        McpToolFamilies {
            issue: false,
            memory: true,
        }
    );

    // A prompt that advertised tools the MCP server cannot register would be
    // worse than no launch, so an unreadable layer fails the provision.
    std::fs::write(store.path(), "{ not json").unwrap();
    assert!(configured_mcp_tools(&data_home, workspace.path()).is_err());
}

#[test]
fn system_prompt_arguments_follow_scope_once_and_stay_parseable() {
    use usagi_core::domain::agent::prompt::{launch_system_prompt, scope_prompt};

    for mode in [SandboxMode::Root, SandboxMode::Session] {
        let expected = scope_prompt(prompt_scope(mode));
        let claude = claude_system_prompt_arguments(mode, None, None);
        assert_eq!(claude, ["--append-system-prompt", expected]);
        assert_eq!(
            claude
                .iter()
                .filter(|argument| argument.as_str() == "--append-system-prompt")
                .count(),
            1
        );

        let codex = codex_system_prompt_arguments(mode, None, None);
        assert_eq!(codex[0], "-c");
        assert_eq!(
            codex
                .iter()
                .filter(|argument| argument.starts_with("developer_instructions="))
                .count(),
            1
        );
        let parsed: toml::Value = toml::from_str(&codex[1]).unwrap();
        assert_eq!(parsed["developer_instructions"].as_str(), Some(expected));
    }

    // A later resolve (including a resume replacement) regenerates from
    // its current scope instead of retaining the previous provision.
    assert_ne!(
        claude_system_prompt_arguments(SandboxMode::Root, None, None),
        claude_system_prompt_arguments(SandboxMode::Session, None, None)
    );
    assert_ne!(
        codex_system_prompt_arguments(SandboxMode::Root, None, None),
        codex_system_prompt_arguments(SandboxMode::Session, None, None)
    );

    // The families the injected server registers reach both products through
    // the one composition, so neither adapter can describe a different set.
    let families = McpToolFamilies {
        issue: false,
        memory: true,
    };
    let expected = launch_system_prompt(PromptScope::Session, Some(families), None);
    assert_eq!(
        claude_system_prompt_arguments(SandboxMode::Session, Some(families), None),
        ["--append-system-prompt", &expected]
    );
    let codex = codex_system_prompt_arguments(SandboxMode::Session, Some(families), None);
    let parsed: toml::Value = toml::from_str(&codex[1]).unwrap();
    assert_eq!(
        parsed["developer_instructions"].as_str(),
        Some(expected.as_str())
    );
}

#[test]
fn role_instruction_is_injected_once_for_claude_and_codex_without_entering_user_prompt() {
    let role = usagi_core::domain::role::RoleId::new("reviewer").unwrap();
    let instructions = "Review correctness and tests.";
    let claude =
        claude_system_prompt_arguments(SandboxMode::Session, None, Some((&role, instructions)));
    assert_eq!(claude[0], "--append-system-prompt");
    assert_eq!(claude[1].matches("<role id=\"reviewer\">").count(), 1);
    assert_eq!(claude[1].matches(instructions).count(), 1);

    let codex =
        codex_system_prompt_arguments(SandboxMode::Session, None, Some((&role, instructions)));
    let parsed: toml::Value = toml::from_str(&codex[1]).unwrap();
    let prompt = parsed["developer_instructions"].as_str().unwrap();
    assert_eq!(prompt.matches("<role id=\"reviewer\">").count(), 1);
    assert_eq!(prompt.matches(instructions).count(), 1);
    assert!(!codex.iter().any(|argument| argument == instructions));
}

#[test]
fn root_role_definition_is_resolved_from_the_current_catalog_at_each_launch() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    let data = temporary.path().join("data");
    std::fs::create_dir_all(workspace.join(".usagi")).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let data_home = paths::DataHome::new(&data, paths::RuntimeMode::Production);
    let sessions = open_session_runtime(
        workspace.clone(),
        &data.join("daemon"),
        &data,
        DaemonGeneration::new(),
    )
    .unwrap();
    let context = provision_context(None);
    let workspaces = one_workspace(
        &data.join("daemon"),
        &workspace,
        sessions,
        context.scope.workspace_id,
    );
    let catalog = |instructions: &str| {
        format!(
            r#"version = 1
[defaults]
root = "director"
[roles.director]
summary = "Direct"
scopes = ["root"]
instructions = "{instructions}"
"#
        )
    };

    std::fs::write(data.join("roles.toml"), catalog("first launch policy")).unwrap();
    let first = effective_role_instruction(&workspaces, &data_home, &workspace, &context)
        .unwrap()
        .unwrap();
    assert_eq!(first.0.as_str(), "director");
    assert_eq!(first.1, "first launch policy");

    std::fs::write(data.join("roles.toml"), catalog("next launch policy")).unwrap();
    let next = effective_role_instruction(&workspaces, &data_home, &workspace, &context)
        .unwrap()
        .unwrap();
    assert_eq!(next.1, "next launch policy");
}

#[test]
fn prompt_renderers_preserve_opaque_argv_and_escape_toml_controls() {
    let prompt = "don't reinterpret \"quotes\", C:\\work\nnext\tline\u{0000}\u{007f}";

    let claude = claude_prompt_arguments(prompt.to_owned());
    assert_eq!(claude, ["--append-system-prompt", prompt]);
    assert_eq!(claude.len(), 2);

    let codex = codex_developer_instructions_arguments(prompt);
    assert_eq!(codex[0], "-c");
    assert!(codex[1].contains(r#"\"quotes\""#));
    assert!(codex[1].contains(r"C:\\work\nnext\tline\u0000\u007F"));
    let parsed: toml::Value = toml::from_str(&codex[1]).unwrap();
    assert_eq!(parsed["developer_instructions"].as_str(), Some(prompt));
}

#[test]
fn pure_daemon_helpers_keep_their_decisions_measured() {
    assert!(matches!(transition_mode(false), TransitionMode::Planned));
    assert!(matches!(transition_mode(true), TransitionMode::Cold));

    let mut context = provision_context(None);
    assert_eq!(mcp_environment_allowlist(&context).len(), 3);
    context.inject_mcp = false;
    assert!(mcp_environment_allowlist(&context).is_empty());

    let text = "quote \" slash \\ backspace \u{0008} tab \t newline \n formfeed \u{000c} return \r null \u{0000}";
    let assignment = format!("value={}", toml_basic_string(text));
    let parsed: toml::Value = toml::from_str(&assignment).unwrap();
    assert_eq!(parsed["value"].as_str(), Some(text));

    let session = SessionId::new();
    let worktree = WorktreeId::new();
    let snapshot = serde_json::json!({
        "sessions": [{
            "session_id": session,
            "worktree_id": worktree,
            "lifecycle": "available"
        }]
    });
    assert_eq!(available_worktree(&snapshot, session), Some(worktree));
    assert_eq!(available_worktree(&snapshot, SessionId::new()), None);

    let payload = serde_json::json!({"value": "  present  ", "blank": " ", "number": 1});
    assert_eq!(
        dispatch::session::required_payload_string(&payload, "value").unwrap(),
        "present"
    );
    assert!(dispatch::session::required_payload_string(&payload, "missing").is_err());
    assert!(dispatch::session::required_payload_string(&payload, "blank").is_err());
    assert!(dispatch::session::required_payload_string(&payload, "number").is_err());

    for (mode, expected) in [
        (paths::RuntimeMode::Production, "production"),
        (paths::RuntimeMode::Development, "development"),
        (paths::RuntimeMode::Local, "local"),
    ] {
        assert_eq!(runtime_channel_for(mode), expected);
    }
    assert_eq!(
        runtime_channel(),
        runtime_channel_for(paths::runtime_mode())
    );
}

#[test]
fn integration_and_system_prompt_precede_resume_and_durable_prompt() {
    let mut codex_arguments =
        codex_integration_arguments(Path::new("/opt/usagi/bin/usagi")).unwrap();
    codex_arguments.extend(codex_system_prompt_arguments(
        SandboxMode::Session,
        None,
        None,
    ));
    codex_arguments.extend(["resume".to_owned(), "provider-session".to_owned()]);
    let codex = SpawnProvision::new([], codex_arguments);
    let (_, argv) = provisioned_agent_command(
        "codex",
        &["--".to_owned(), "user prompt".to_owned()],
        &codex,
    );
    let developer = argv
        .iter()
        .position(|argument| argument.starts_with("developer_instructions="))
        .unwrap();
    let resume = argv
        .iter()
        .position(|argument| argument == "resume")
        .unwrap();
    let separator = argv.iter().position(|argument| argument == "--").unwrap();
    assert!(developer < resume);
    assert!(resume < separator);
    assert_eq!(
        argv.iter()
            .filter(|argument| argument.starts_with("developer_instructions="))
            .count(),
        1
    );

    let prompt = usagi_core::domain::agent::prompt::scope_prompt(PromptScope::Session).to_owned();
    let mut claude = SpawnProvision::new(
        [],
        claude_system_prompt_arguments(SandboxMode::Session, None, None),
    );
    claude.set_sandbox_launcher(SandboxLauncher {
        program: "/opt/usagi/bin/usagi".to_owned(),
        prefix: vec!["claude-sandbox".to_owned(), "--".to_owned()],
    });
    claude.append_sensitive_arguments(["--resume".to_owned(), "provider-session".to_owned()]);
    let (program, argv) = provisioned_agent_command(
        "claude",
        &[
            "--model".to_owned(),
            "sonnet".to_owned(),
            "--".to_owned(),
            "user prompt".to_owned(),
        ],
        &claude,
    );
    assert_eq!(program, "/opt/usagi/bin/usagi");
    assert_eq!(
        argv,
        [
            "claude-sandbox",
            "--",
            "claude",
            "--append-system-prompt",
            prompt.as_str(),
            "--resume",
            "provider-session",
            "--model",
            "sonnet",
            "--",
            "user prompt",
        ]
    );
    assert_eq!(
        argv.iter()
            .filter(|argument| argument.as_str() == "--append-system-prompt")
            .count(),
        1
    );
}

#[test]
fn saved_environment_reaches_terminal_and_agent_with_workspace_precedence() {
    use usagi_core::domain::settings::{LocalSettings, Settings};
    use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;

    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    Storage::new(data.path().to_path_buf())
        .save_settings(&Settings {
            env: BTreeMap::from([
                ("GLOBAL_ONLY".to_owned(), "global".to_owned()),
                (
                    "OP_SERVICE_ACCOUNT_TOKEN".to_owned(),
                    "daemon-only".to_owned(),
                ),
                ("SHARED".to_owned(), "global".to_owned()),
            ]),
            ..Settings::default()
        })
        .unwrap();
    WorkspaceSettingsStore::new(workspace.path())
        .save(&LocalSettings {
            env: BTreeMap::from([
                ("SHARED".to_owned(), "workspace".to_owned()),
                ("WORKSPACE_ONLY".to_owned(), "workspace".to_owned()),
            ]),
            ..LocalSettings::default()
        })
        .unwrap();

    let configured = Arc::new(UserEnvironment::new(data.path().to_path_buf(), OpCli));
    let request = TerminalLaunchRequest {
        profile_id: TerminalProfileId::new("login-shell").unwrap(),
        scope: TerminalLaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        },
    };
    let terminal = TrustedLoginShell {
        workspaces: None,
        profile: LoginShellProfile::new(
            BTreeMap::from([("SHELL".to_owned(), "/bin/sh".to_owned())]),
            workspace.path().to_path_buf(),
        ),
        environment: Some(Arc::clone(&configured)),
        workspace_root: workspace.path().to_path_buf(),
    }
    .resolve(&request)
    .unwrap();
    let terminal_environment = terminal
        .environment
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(terminal_environment["GLOBAL_ONLY"], "global");
    assert_eq!(terminal_environment["SHARED"], "workspace");
    assert_eq!(terminal_environment["WORKSPACE_ONLY"], "workspace");
    assert!(!terminal_environment.contains_key("OP_SERVICE_ACCOUNT_TOKEN"));

    let user = configured_environment(Some(&configured), workspace.path()).unwrap();
    let agent = SpawnProvision::new(launch_environment(&user, Vec::new()), Vec::new())
        .compose_environment(&BTreeMap::new());
    assert_eq!(agent["GLOBAL_ONLY"], "global");
    assert_eq!(agent["SHARED"], "workspace");
    assert_eq!(agent["WORKSPACE_ONLY"], "workspace");
    assert!(!agent.contains_key("OP_SERVICE_ACCOUNT_TOKEN"));
}

/// The public environment both PTY owners start from names the account this
/// daemon runs as, because a keychain client in the child is indexed by it
/// (#735).
///
/// The expectation is the platform adapter's own answer, not this
/// composition re-run: what is under test here is that the composition
/// actually wires the resolved name in rather than leaving the inherited one
/// to win. That the adapter's answer is the real account is held separately
/// by `usagi-daemon`'s `terminal_user_environment` integration test, which
/// compares it against the OS.
#[test]
fn the_public_terminal_environment_names_the_user_the_daemon_runs_as() {
    let environment = terminal_environment();
    let resolved = usagi_daemon::infrastructure::os_user::effective_user_name();
    if let Some(user) = resolved.as_ref() {
        // The ordinary case on any machine with a passwd entry: the child
        // receives the resolved account, and an inherited name never wins.
        assert_eq!(environment.get("USER"), Some(user));
    } else {
        // A platform that cannot answer never invents a name. Which value
        // the fallback then picks is pinned by `terminal_profile`'s unit
        // tests, which do not need such a platform to run.
        assert!(
            environment
                .get("USER")
                .is_none_or(|value| !value.is_empty() && !value.contains('\0'))
        );
    }
    // The memoized accessor answers with the same name the adapter gives,
    // which is what every launch after the first one reads.
    assert_eq!(resolved_os_user(), resolved.as_deref());

    // Against an inherited name that is deliberately not this account, the
    // precedence is observable: dropping the resolved name from the
    // composition would export the sentinel instead.
    let sentinel = terminal_environment_from(|_| Some("inherited-sentinel".to_owned()));
    assert_eq!(
        sentinel.get("USER").map(String::as_str),
        resolved_os_user().or(Some("inherited-sentinel"))
    );
    assert!(!environment.contains_key("GH_TOKEN"));
    assert!(!environment.contains_key("OP_SERVICE_ACCOUNT_TOKEN"));
}

/// A registry holding exactly one workspace, for tests that exercise a
/// single runtime through the daemon's per-workspace resolution.
fn one_workspace(
    daemon_dir: &Path,
    root: &Path,
    runtime: SharedSessionRuntime,
    workspace_id: WorkspaceId,
) -> Workspaces {
    struct FixedOpener {
        runtime: SharedSessionRuntime,
        workspace_id: WorkspaceId,
    }
    impl TenantRuntimeOpener for FixedOpener {
        type Runtime = SharedSessionRuntime;
        fn open(&self, _: &Path, _: &Path) -> std::io::Result<OpenedTenant<SharedSessionRuntime>> {
            Ok(OpenedTenant {
                runtime: Arc::clone(&self.runtime),
                workspace_id: self.workspace_id,
            })
        }
    }
    let registry = Arc::new(TenantRegistry::new(
        daemon_dir.to_path_buf(),
        FileWorkspaceFences {
            pid: std::process::id(),
        },
        FixedOpener {
            runtime,
            workspace_id,
        },
        DEFAULT_TENANT_LIMIT,
    ));
    registry
        .adopt_initial(root)
        .expect("the fixture workspace is adopted");
    registry
}

/// A connection bound to one workspace, for tests that exercise a single
/// runtime through the per-connection resolution.
fn bound_to(
    daemon_dir: &Path,
    root: &Path,
    runtime: SharedSessionRuntime,
    workspace_id: WorkspaceId,
) -> ConnectionWorkspace {
    let workspaces = one_workspace(daemon_dir, root, runtime, workspace_id);
    let tenant = workspaces
        .workspace_at(root)
        .expect("the fixture workspace is adopted");
    ConnectionWorkspace { tenant, workspaces }
}

#[test]
fn non_git_tenant_has_no_orphan_git_candidates() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("plain-workspace");
    std::fs::create_dir(&root).unwrap();
    let sessions = Arc::new(Mutex::new(
        SessionRuntime::open(
            root.clone(),
            &temporary.path().join("session-daemon"),
            DaemonGeneration::new(),
            AlwaysSuccessfulGit,
            PermissiveSessionWorktreeIo,
        )
        .unwrap(),
    ));
    let workspace = serde_json::from_value(
        sessions.lock().unwrap().snapshot().unwrap()["workspace_id"].clone(),
    )
    .unwrap();
    let bound = bound_to(
        &temporary.path().join("tenants"),
        &root,
        sessions,
        workspace,
    );

    for apply in [false, true] {
        let result = clean_orphan_session_resources(&bound, None, apply, false).unwrap();
        assert!(result["candidates"].as_array().unwrap().is_empty());
        assert_eq!(result["removed"], 0);
        assert_eq!(result["protected"], 0);
    }
}

fn provision_context(session: Option<SessionId>) -> ProvisionContext {
    ProvisionContext {
        scope: usagi_core::domain::agent::LaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: session,
            worktree_id: WorktreeId::new(),
        },
        inject_mcp: true,
    }
}

/// Every consumer of the Agent child's data home must agree on the base the
/// daemon actually runs from, in all three runtime modes and under a custom
/// `$USAGI_HOME`. Before #608 the base was guessed as the selected
/// directory's `parent()`, which is correct only for `dev/` and `local/`:
/// production selects the base itself, so the guess handed the child — and
/// the sandbox — the directory *above* the data home.
#[test]
fn the_agent_child_data_home_follows_the_runtime_mode_in_every_channel() {
    use usagi_core::domain::settings::Settings;

    let context = provision_context(Some(SessionId::new()));
    for mode in [
        paths::RuntimeMode::Production,
        paths::RuntimeMode::Development,
        paths::RuntimeMode::Local,
    ] {
        // One custom `$USAGI_HOME` per mode, so a value read from the wrong
        // directory cannot be satisfied by another mode's leftovers.
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let base = home.path();
        let selected = paths::DataHome::new(base, mode).selected();

        // The daemon holds only its selected directory; this is the one
        // place the mode-neutral base is recovered from it.
        let data_home = paths::DataHome::from_selected(&selected, mode);
        assert_eq!(data_home.base(), base);
        assert_eq!(data_home.selected(), selected);

        // 1. Child env: the base plus the mode that re-selects `selected`.
        let environment = mcp_environment(&context, &data_home, Path::new("/repo")).unwrap();
        let value = |name: &str| {
            environment
                .iter()
                .find(|(variable, _)| variable.as_str() == name)
                .map_or_else(|| panic!("{name} is injected"), |(_, value)| value.clone())
        };
        let child_home = PathBuf::from(value(usagi_core::infrastructure::paths::DATA_DIR_ENV));
        assert_eq!(child_home, base);
        let child_mode = value(usagi_core::infrastructure::paths::RUNTIME_MODE_ENV);
        assert_eq!(child_mode, mode.as_env_value());
        // Re-applying the announced mode lands the child on the daemon's
        // own directory — the round trip the E2E pins end to end. The
        // artifact default is passed as the fallback so a dropped wire
        // spelling would resolve somewhere else instead of passing.
        assert_eq!(
            paths::DataHome::new(
                &child_home,
                paths::RuntimeMode::from_env_value(Some(&child_mode), paths::DEFAULT_RUNTIME_MODE)
            )
            .selected(),
            selected
        );

        // 2. Settings source: the selected directory the daemon writes.
        std::fs::create_dir_all(&selected).unwrap();
        Storage::new(&selected)
            .save_settings(&Settings {
                issue_enabled: false,
                ..Settings::default()
            })
            .unwrap();
        assert!(
            !configured_mcp_tools(&data_home, home.path())
                .unwrap()
                .families
                .issue
        );

        // 3. Root sandbox scope: daemon bootstrap is brokered out of process,
        // so neither the selected directory nor its mode-neutral base is writable.
        let roots =
            claude_writable_roots(SandboxMode::Root, Path::new("/repo/.usagi/sessions/work"));
        assert!(roots.is_empty(), "{roots:?}");
    }
}

#[test]
fn root_agent_writable_roots_include_only_provider_state() {
    std::fs::create_dir_all("target").unwrap();
    let fixture = tempfile::tempdir_in("target").unwrap();
    let home = fixture.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let roots = root_agent_writable_roots(Some(&home), DefaultModel::OpenAi).unwrap();
    assert_eq!(roots, [home.join(".codex").canonicalize().unwrap()]);
    assert!(!roots.contains(&fixture.path().canonicalize().unwrap()));

    // Two providers exec the same `claude`, and each still gets only its own
    // state: the grant is keyed by provider, never by that shared program.
    let claude = root_agent_writable_roots(Some(&home), DefaultModel::Claude).unwrap();
    let sakana = root_agent_writable_roots(Some(&home), DefaultModel::SakanaAi).unwrap();
    assert_eq!(claude, [home.join(".claude").canonicalize().unwrap()]);
    assert_eq!(
        sakana,
        [home.join(".claude-sakana").canonicalize().unwrap()]
    );
    assert_ne!(claude, sakana);

    let roots = root_agent_writable_roots(Some(&home), DefaultModel::Agy).unwrap();
    assert_eq!(
        roots,
        [home
            .join(".gemini/antigravity-cli/conversations")
            .canonicalize()
            .unwrap()]
    );
    assert!(!roots.contains(&fixture.path().canonicalize().unwrap()));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let hostile_home = fixture.path().join("hostile-home");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(&hostile_home).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, hostile_home.join(".gemini")).unwrap();
        assert_eq!(
            root_agent_writable_roots(Some(&hostile_home), DefaultModel::Agy),
            Err(ClaudeSandboxPolicyError::InvalidWritableRoot)
        );
        assert!(!outside.join("antigravity-cli").exists());
    }

    let roots = root_agent_writable_roots(None, DefaultModel::Claude).unwrap();
    assert!(roots.is_empty());
}

#[test]
fn agy_plugin_documents_are_scoped_and_shell_safe() {
    let command = "/Applications/Usagi's Tools/usagi";
    let (manifest, mcp, hooks) = agy_plugin_documents(command);

    assert_eq!(manifest, serde_json::json!({"name": "usagi-runtime"}));
    assert_eq!(mcp["mcpServers"]["usagi"]["command"], command);
    assert_eq!(
        mcp["mcpServers"]["usagi"]["args"],
        serde_json::json!(["mcp"])
    );
    assert_eq!(hooks.as_object().unwrap().len(), 1);
    let integration = &hooks["usagi-runtime"];
    let command = integration["PreInvocation"][0]["command"].as_str().unwrap();
    assert!(command.starts_with("'/Applications/Usagi'\"'\"'s Tools/usagi'"));
    assert!(command.ends_with("agent-phase running --hook-event PreInvocation"));
    assert_eq!(
        integration["PreToolUse"][0]["matcher"],
        serde_json::json!("*")
    );
    assert_eq!(
        integration["PostToolUse"][0]["hooks"][0]["command"],
        serde_json::json!(format!(
            "{} agent-phase waiting --hook-event PostToolUse",
            shell_quote("/Applications/Usagi's Tools/usagi")
        ))
    );
    assert_eq!(
        integration["Stop"][0]["command"],
        serde_json::json!(format!(
            "{} agent-phase ended --hook-event Stop",
            shell_quote("/Applications/Usagi's Tools/usagi")
        ))
    );

    std::fs::create_dir_all("target").unwrap();
    let fixture = tempfile::tempdir_in("target").unwrap();
    let data_home = paths::DataHome::new(fixture.path(), paths::RuntimeMode::Production);
    let workspace = WorkspaceId::new();
    let plugin_workspace =
        materialize_agy_plugin(&data_home, workspace, Path::new(command)).unwrap();
    let expected = fixture
        .path()
        .join("agent-integrations")
        .join(workspace.to_string())
        .join("agy")
        .canonicalize()
        .unwrap();
    assert_eq!(plugin_workspace, expected);
    let plugin = plugin_workspace.join(".agents/plugins/usagi-runtime");
    for document in ["plugin.json", "mcp_config.json", "hooks.json"] {
        assert!(plugin.join(document).is_file());
    }
    assert!(
        !fixture
            .path()
            .join(".gemini/config/plugins/usagi-runtime")
            .exists(),
        "managed launch material must not become a global AGY plugin"
    );

    let sandbox_home = fixture.path().join("home");
    std::fs::create_dir(&sandbox_home).unwrap();
    let writable = agent_writable_roots(
        SandboxMode::Root,
        Path::new("/workspace"),
        None,
        Some(&sandbox_home),
        DefaultModel::Agy,
        &data_home,
        workspace,
    )
    .unwrap();
    assert!(writable.iter().all(|root| {
        !plugin_workspace.starts_with(root) && !root.starts_with(&plugin_workspace)
    }));
}

#[test]
fn agy_plugin_arguments_materialize_only_outside_the_write_surface() {
    std::fs::create_dir_all("target").unwrap();
    let fixture = tempfile::tempdir_in("target").unwrap();
    let data_home = paths::DataHome::new(fixture.path(), paths::RuntimeMode::Production);
    let workspace = WorkspaceId::new();
    let policy = SandboxPolicyInputs {
        mode: SandboxMode::Root,
        agent: DefaultModel::Agy,
        workspace_root: Path::new("/workspace"),
        launch_roots: &[],
        tmpdir: None,
        home: None,
        cache_dir: None,
        backend: None,
        passthrough: false,
        read_only_roots: &[],
    };
    assert_eq!(
        agy_plugin_arguments(
            &data_home,
            workspace,
            Path::new("usagi"),
            false,
            false,
            &policy
        )
        .unwrap(),
        (Vec::new(), None)
    );
    let (arguments, isolated) = agy_plugin_arguments(
        &data_home,
        workspace,
        Path::new("usagi"),
        true,
        false,
        &policy,
    )
    .unwrap();
    let isolated = isolated.unwrap();
    assert_eq!(
        arguments,
        [
            "--add-dir".to_owned(),
            isolated.to_str().unwrap().to_owned()
        ]
    );

    let temporary = tempfile::tempdir().unwrap();
    let temporary_data = paths::DataHome::new(temporary.path(), paths::RuntimeMode::Production);
    let temporary_policy = SandboxPolicyInputs {
        tmpdir: Some(temporary.path()),
        ..policy
    };
    assert!(
        agy_plugin_arguments(
            &temporary_data,
            workspace,
            Path::new("usagi"),
            true,
            false,
            &temporary_policy,
        )
        .is_err()
    );
    assert!(!temporary.path().join("agent-integrations").exists());
}

#[test]
fn agy_plugin_arguments_reject_invalid_materialization_inputs() {
    std::fs::create_dir_all("target").unwrap();
    let fixture = tempfile::tempdir_in("target").unwrap();
    let data_home = paths::DataHome::new(fixture.path(), paths::RuntimeMode::Production);
    let workspace = WorkspaceId::new();
    let policy = SandboxPolicyInputs {
        mode: SandboxMode::Root,
        agent: DefaultModel::Agy,
        workspace_root: Path::new("/workspace"),
        launch_roots: &[],
        tmpdir: None,
        home: None,
        cache_dir: None,
        backend: None,
        passthrough: false,
        read_only_roots: &[],
    };
    let missing_data = paths::DataHome::new(
        fixture.path().join("missing-data"),
        paths::RuntimeMode::Production,
    );
    assert!(
        agy_plugin_arguments(
            &missing_data,
            workspace,
            Path::new("usagi"),
            true,
            true,
            &policy,
        )
        .is_err()
    );

    #[cfg(unix)]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

        let invalid_command = PathBuf::from(OsString::from_vec(vec![0xff]));
        assert!(agy_arguments_for_integration(&invalid_command).is_err());
        assert!(
            agy_plugin_arguments(
                &data_home,
                WorkspaceId::new(),
                &invalid_command,
                true,
                true,
                &policy,
            )
            .is_err()
        );

        #[cfg(target_os = "linux")]
        {
            let non_utf8_root = fixture
                .path()
                .join(OsString::from_vec(vec![b'n', b'o', b'n', b'-', 0xff]));
            std::fs::create_dir(&non_utf8_root).unwrap();
            let non_utf8_data =
                paths::DataHome::new(&non_utf8_root, paths::RuntimeMode::Production);
            assert!(
                agy_plugin_arguments(
                    &non_utf8_data,
                    WorkspaceId::new(),
                    Path::new("usagi"),
                    true,
                    true,
                    &policy,
                )
                .is_err()
            );
        }
    }
}

#[test]
fn agy_integration_rejects_every_effective_sandbox_write_surface() {
    let roots = [PathBuf::from("/repo/.usagi/sessions/agy")];
    let policy = SandboxPolicyInputs {
        mode: SandboxMode::Session,
        agent: DefaultModel::Agy,
        workspace_root: Path::new("/repo"),
        launch_roots: &roots,
        tmpdir: Some(Path::new("/custom/tmpdir")),
        home: Some(Path::new("/home/dev")),
        cache_dir: Some(Path::new("/private/var/folders/ab/cd/C")),
        backend: None,
        passthrough: false,
        read_only_roots: &[],
    };
    for target in [
        "/tmp/usagi/agent-integrations",
        "/var/tmp/usagi/agent-integrations",
        "/custom/tmpdir/agent-integrations",
        "/repo/.usagi/sessions/agy/private-plugin",
        "/repo/daemon-data/agent-integrations",
        "/home/dev/.gemini/antigravity-cli/conversations/private-plugin",
    ] {
        assert_eq!(
            validate_isolated_sandbox_root(&policy, Path::new(target)),
            Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor),
            "{target} must fail closed"
        );
    }
    assert_eq!(
        validate_isolated_sandbox_root(
            &policy,
            Path::new("/daemon/agent-integrations/workspace/agy")
        ),
        Ok(())
    );
    assert_eq!(
        validate_isolated_sandbox_root(&policy, Path::new("/home/dev/.gemini/private-plugin")),
        Ok(()),
        "AGY global customization is no longer part of the write surface"
    );
}

#[test]
fn agent_memory_root_is_shared_by_workspace_and_outside_the_checkout() {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all("target").unwrap();
    let fixture = tempfile::tempdir_in("target").unwrap();
    let data_home = paths::DataHome::new(fixture.path(), paths::RuntimeMode::Production);
    let workspace = WorkspaceId::new();

    let root = root_memory_store_root(&data_home, workspace).unwrap();

    assert_eq!(
        root,
        fixture
            .path()
            .join("agent-memory")
            .join(workspace.to_string())
            .canonicalize()
            .unwrap()
    );
    assert!(root.join(".usagi/memory").is_dir());
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn bootstrap_broker_accepts_only_ping_start_and_stop() {
    let launches = std::cell::Cell::new(0_u8);
    let count = |request| {
        handle_bootstrap_broker_request(
            request,
            || {
                launches.set(launches.get() + 1);
                Ok(())
            },
            || false,
        )
    };

    assert_eq!(count(BROKER_PING), BrokerOutcome::served(true));
    assert_eq!(launches.get(), 0);
    assert_eq!(count(BROKER_START), BrokerOutcome::served(true));
    assert_eq!(launches.get(), 1);
    // An unknown byte is refused without starting anything, and without
    // ending the broker: a stray peer must not be able to retire it.
    assert_eq!(count(b'Z'), BrokerOutcome::served(false));
    assert_eq!(launches.get(), 1);
    assert_eq!(
        handle_bootstrap_broker_request(
            BROKER_START,
            || Err(std::io::Error::other("launch refused")),
            || false,
        ),
        BrokerOutcome::served(false)
    );
    // Stop is the one request that ends the loop, and it starts no daemon.
    assert_eq!(count(BROKER_STOP), BrokerOutcome::RETIRE);
    assert_eq!(launches.get(), 1);

    // A daemon that came back between the decision to retire and this point
    // vetoes it: retiring would leave it with no broker to outlive it, which
    // is the one state the broker exists to prevent.
    assert_eq!(
        handle_bootstrap_broker_request(BROKER_STOP, || Ok(()), || true),
        BrokerOutcome::served(false)
    );
}

/// The broker exists so that a sandboxed client can cold-start a daemon it
/// cannot spawn itself. Retiring next to a live daemon would remove exactly
/// the helper that daemon's death is going to need.
#[test]
fn an_idle_broker_retires_only_once_no_daemon_is_left_to_outlive() {
    let timeout = Duration::from_secs(60);
    assert!(broker_may_retire(Duration::from_secs(60), timeout, false));
    assert!(broker_may_retire(Duration::from_secs(600), timeout, false));
    assert!(!broker_may_retire(Duration::from_secs(59), timeout, false));
    for idle in [Duration::ZERO, Duration::from_hours(24)] {
        assert!(
            !broker_may_retire(idle, timeout, true),
            "a live daemon must keep its broker"
        );
    }
}

/// A broker whose endpoint was removed underneath it can never be reached
/// again — not by a client, and not by the retirement request its own idle
/// watch sends. That state has to be told apart from a broker that is merely
/// idle, because only the first one has to leave without being asked.
#[test]
fn an_unreachable_broker_endpoint_is_not_mistaken_for_an_idle_one() {
    // `/tmp` keeps the path inside the platform limit for a socket name.
    let fixture = tempfile::tempdir_in("/tmp").unwrap();
    let socket = fixture.path().join("broker.sock");
    assert!(
        !broker_endpoint_present(&socket),
        "a path that was never bound is not an endpoint"
    );

    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    assert!(broker_endpoint_present(&socket));

    // A regular file at the same path is not the endpoint either: only a
    // socket can carry the retirement request.
    drop(listener);
    std::fs::remove_file(&socket).unwrap();
    std::fs::write(&socket, b"").unwrap();
    assert!(!broker_endpoint_present(&socket));

    std::fs::remove_file(&socket).unwrap();
    assert!(
        !broker_endpoint_present(&socket),
        "a removed endpoint leaves the broker unreachable"
    );
}

#[test]
fn bootstrap_broker_launches_only_serve_in_its_fixed_workspace() {
    let command = bootstrap_serve_command(Path::new("/opt/usagi"), Path::new("/repo"));
    assert_eq!(command.get_program(), "/opt/usagi");
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["daemon", "serve"]);
    assert_eq!(command.get_current_dir(), Some(Path::new("/repo")));
}

#[test]
fn bootstrap_broker_address_is_fenced_by_workspace_and_executable() {
    let data = Path::new("/data");
    let first = bootstrap_broker_address(data, Path::new("/repo-a"), Path::new("/bin/usagi"));
    assert_eq!(
        first,
        bootstrap_broker_address(data, Path::new("/repo-a"), Path::new("/bin/usagi"))
    );
    assert_ne!(
        first,
        bootstrap_broker_address(data, Path::new("/repo-b"), Path::new("/bin/usagi"))
    );
    assert_ne!(
        first,
        bootstrap_broker_address(data, Path::new("/repo-a"), Path::new("/opt/usagi"))
    );
    assert_eq!(first.socket.parent(), Some(Path::new("/data/daemon")));
    assert_eq!(first.lock.parent(), Some(Path::new("/data/daemon")));
    assert!(first.socket.to_string_lossy().ends_with(".sock"));
    assert!(first.lock.to_string_lossy().ends_with(".lock"));
}

#[test]
fn cold_start_uses_the_running_handshakes_implicit_workspace_rule() {
    use usagi_core::infrastructure::ipc::{
        ClientWorkspace, ErrorCode, SideEffect, is_workspace_mismatch,
    };

    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let daemon = directory.path().join("data/daemon");
    let plain = directory.path().join("plain");
    let repository = directory.path().join("repository");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::create_dir_all(repository.join(".git")).unwrap();
    let plain = plain.canonicalize().unwrap();
    let repository = repository.canonicalize().unwrap();
    let bound = |path: &Path| ClientWorkspace::Bound {
        root: paths::wire_workspace_root(path),
    };

    let refusal = cold_start_workspace(&daemon, &bound(&plain), None, None).unwrap_err();
    assert!(is_workspace_mismatch(&refusal));
    assert_eq!(refusal.code, ErrorCode::PermissionDenied);
    assert_eq!(refusal.side_effect, SideEffect::None);
    assert!(refusal.message.contains("repository root"));
    assert!(!plain.join(".usagi").exists());
    assert!(
        cold_start_workspace(
            &daemon,
            &ClientWorkspace::Bound {
                root: String::new(),
            },
            None,
            None,
        )
        .is_err()
    );

    assert_eq!(
        cold_start_workspace(&daemon, &bound(&repository), None, None).unwrap(),
        repository
    );
    let worktree = directory.path().join(".usagi/sessions/worker");
    std::fs::create_dir_all(worktree.join(".git")).unwrap();
    let worktree = worktree.canonicalize().unwrap();
    assert!(cold_start_workspace(&daemon, &bound(&worktree), None, None).is_err());

    let selected = ClientWorkspace::Selected {
        root: paths::wire_workspace_root(&plain),
    };
    assert_eq!(
        cold_start_workspace(&daemon, &selected, None, None).unwrap(),
        plain
    );
    assert_eq!(
        cold_start_workspace(&daemon, &ClientWorkspace::Unbound, Some(&plain), None).unwrap(),
        plain
    );
    let missing_opened = directory.path().join("missing-opened");
    let refusal = cold_start_workspace(
        &daemon,
        &ClientWorkspace::Unbound,
        Some(&missing_opened),
        None,
    )
    .unwrap_err();
    assert!(is_workspace_mismatch(&refusal));
    assert!(refusal.message.contains("does not resolve"));
    assert_eq!(
        cold_start_workspace(&daemon, &ClientWorkspace::Unbound, None, Some(&repository)).unwrap(),
        repository
    );
    assert!(cold_start_workspace(&daemon, &ClientWorkspace::Unbound, None, Some(&plain)).is_err());
    assert!(cold_start_workspace(&daemon, &ClientWorkspace::Unbound, None, None).is_err());

    workspace_state::resolve(&daemon, &plain).unwrap();
    let child = plain.join("nested");
    std::fs::create_dir(&child).unwrap();
    assert_eq!(
        cold_start_workspace(&daemon, &bound(&child), None, None).unwrap(),
        plain
    );
    let removed_child = plain.join("removed-session");
    assert_eq!(
        cold_start_workspace(&daemon, &bound(&removed_child), None, None).unwrap(),
        plain
    );
}

#[test]
fn ordinary_daemon_start_does_not_use_a_workspace_fixed_broker() {
    assert!(run_broker_lifecycle_command(&CliDaemonCommand::Start).is_none());
}

/// One broker serving a throwaway workspace, plus everything a test needs to
/// address it and to know it finished.
struct BrokerFixture {
    workspace_dir: tempfile::TempDir,
    _data_parent: tempfile::TempDir,
    address: BootstrapBrokerAddress,
    server: std::thread::JoinHandle<std::io::Result<()>>,
}

fn start_broker(idle: BrokerIdlePolicy) -> BrokerFixture {
    let workspace_dir = tempfile::tempdir_in("/tmp").unwrap();
    let workspace = workspace_dir.path().canonicalize().unwrap();
    let data_parent = tempfile::tempdir_in("/tmp").unwrap();
    let data = data_parent.path().join("data");
    let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    let address = bootstrap_broker_address(&data, &workspace, &exe);
    let (server_data, server_workspace, server_exe) = (data, workspace, exe);
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let result = serve_bootstrap_broker(&server_data, &server_workspace, &server_exe, idle);
        let _ = finished_tx.send(result.as_ref().err().map(ToString::to_string));
        result
    });
    for _ in 0..500 {
        if address.record.is_file()
            && std::os::unix::net::UnixStream::connect(&address.socket).is_ok()
        {
            return BrokerFixture {
                workspace_dir,
                _data_parent: data_parent,
                address,
                server,
            };
        }
        if let Ok(error) = finished_rx.try_recv() {
            panic!("broker failed before binding its socket: {error:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("broker socket never became ready");
}

/// A broker that never retires is a process the operator cannot end: nothing
/// else stops it, and one accumulates per workspace and per executable path.
#[test]
fn a_stop_request_retires_the_broker_and_removes_its_endpoint() {
    let fixture = start_broker(BrokerIdlePolicy {
        timeout: Duration::from_secs(3600),
        poll: Duration::from_secs(3600),
    });
    let record: BootstrapBrokerRecord = json_file::read(&fixture.address.record).unwrap().unwrap();
    assert_eq!(record.pid, std::process::id());
    assert_eq!(
        record.process_start_identity,
        process_start_identity(record.pid).unwrap()
    );

    // Retirement is acknowledged, so a caller learns the endpoint is going
    // rather than having to infer it from a closed connection.
    request_bootstrap_broker(&fixture.address, BROKER_STOP).unwrap();

    fixture.server.join().unwrap().unwrap();
    assert!(!fixture.address.socket.exists());
    assert!(!fixture.address.record.exists());
    let replacement_lock = FileInstanceLock {
        path: fixture.address.lock.clone(),
        held: RefCell::new(None),
    };
    assert!(
        replacement_lock.acquire().unwrap(),
        "a retired broker still held its instance lock"
    );
    assert!(
        std::os::unix::net::UnixStream::connect(&fixture.address.socket).is_err(),
        "a retired broker still answered"
    );
    fixture.workspace_dir.close().unwrap();
}

/// A condition-variable notification is not durable, so this regression
/// repeatedly puts `stop` before the waiter. The state predicate must make
/// every wait return immediately even though no later notification exists.
#[test]
fn broker_stop_before_wait_cannot_lose_shutdown() {
    for _ in 0..128 {
        let activity = Arc::new(BrokerActivity::started());
        activity.stop();
        let waiter = Arc::clone(&activity);
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _ = finished_tx.send(waiter.wait_for_poll(Duration::from_secs(3600)));
        });

        assert_eq!(
            finished_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            None
        );
        thread.join().unwrap();
    }
}

/// With no daemon to outlive and no request to serve, the broker is holding
/// a process open for a workspace nobody is using.
#[test]
fn an_unused_broker_retires_itself_once_nothing_needs_it() {
    let fixture = start_broker(BrokerIdlePolicy {
        timeout: Duration::ZERO,
        poll: Duration::from_millis(20),
    });

    // The idle watch reaches the broker through its own endpoint, so the
    // accept loop stays blocked until then and pays no polling latency.
    let started = Instant::now();
    fixture.server.join().unwrap().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the idle watch did not retire an unused broker"
    );
    assert!(!fixture.address.socket.exists());
    fixture.workspace_dir.close().unwrap();
}

#[test]
fn an_idle_broker_client_cannot_block_the_next_request_forever() {
    let fixture = start_broker(BrokerIdlePolicy {
        timeout: Duration::from_secs(3600),
        poll: Duration::from_secs(3600),
    });
    let idle = std::os::unix::net::UnixStream::connect(&fixture.address.socket).unwrap();
    drop(std::os::unix::net::UnixStream::connect(&fixture.address.socket).unwrap());

    let started = Instant::now();
    request_bootstrap_broker(&fixture.address, BROKER_PING).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "an idle peer blocked the broker beyond its IO deadline"
    );
    drop(idle);

    fixture.workspace_dir.close().unwrap();
    let _ = request_bootstrap_broker(&fixture.address, BROKER_PING);
    fixture.server.join().unwrap().unwrap();
    assert!(!fixture.address.socket.exists());
}

#[test]
fn root_git_environment_overrides_untrusted_process_launch_configuration() {
    let mut spawn = SpawnProvision::new([], Vec::new());
    insert_root_git_environment(&mut spawn);
    let environment = spawn.compose_environment(&BTreeMap::from([
        ("GIT_CONFIG_COUNT".to_owned(), "0".to_owned()),
        ("GIT_PAGER".to_owned(), "touch PWNED".to_owned()),
        ("GIT_EXTERNAL_DIFF".to_owned(), "touch".to_owned()),
    ]));
    assert_eq!(environment["GIT_CONFIG_COUNT"], "5");
    assert_eq!(environment["GIT_CONFIG_KEY_0"], "core.fsmonitor");
    assert_eq!(environment["GIT_CONFIG_VALUE_0"], "false");
    assert_eq!(environment["GIT_CONFIG_KEY_1"], "core.hooksPath");
    assert_eq!(environment["GIT_CONFIG_VALUE_1"], "/dev/null");
    assert_eq!(environment["GIT_PAGER"], "");
    assert_eq!(environment["GIT_EXTERNAL_DIFF"], "");
    assert_eq!(environment["GIT_OPTIONAL_LOCKS"], "0");
}

#[test]
fn root_codex_uses_the_outer_boundary_without_nesting_the_native_sandbox() {
    let mut spawn = SpawnProvision::new([], Vec::new());
    spawn.set_sandbox_launcher(SandboxLauncher {
        program: "/opt/usagi/bin/usagi".to_owned(),
        prefix: vec!["claude-sandbox".to_owned(), "--".to_owned()],
    });
    insert_root_git_environment(&mut spawn);

    assert!(spawn.sandbox_launcher().is_some());
    let (program, argv) = provisioned_agent_command(
        "codex",
        &[
            "--sandbox".to_owned(),
            "danger-full-access".to_owned(),
            "--ask-for-approval".to_owned(),
            "never".to_owned(),
        ],
        &spawn,
    );
    assert_eq!(program, "/opt/usagi/bin/usagi");
    assert_eq!(
        argv,
        [
            "claude-sandbox",
            "--",
            "codex",
            "--sandbox",
            "danger-full-access",
            "--ask-for-approval",
            "never"
        ]
    );

    let environment = spawn.compose_environment(&BTreeMap::new());
    assert_eq!(environment["GIT_CONFIG_NOSYSTEM"], "1");
    assert_eq!(environment["GIT_CONFIG_GLOBAL"], "/dev/null");
    assert_eq!(environment["GIT_OPTIONAL_LOCKS"], "0");
}

#[cfg(unix)]
#[test]
fn codex_arg0_preflight_repairs_only_owned_provider_temp_directory_modes() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let state = tempfile::tempdir().unwrap();
    let arg0 = state.path().join("tmp/arg0");
    let stale_dir = arg0.join("codex-arg0-stale");
    let unrelated = arg0.join("other-temp");
    let target = arg0.join("target");
    std::fs::create_dir_all(&stale_dir).unwrap();
    std::fs::create_dir(&unrelated).unwrap();
    std::fs::create_dir(&target).unwrap();
    symlink(&target, arg0.join("codex-arg0-alias")).unwrap();
    std::fs::set_permissions(&stale_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o000)).unwrap();

    assert_eq!(repair_codex_arg0_permissions(state.path()).unwrap(), 1);
    assert_eq!(
        std::fs::symlink_metadata(&stale_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::symlink_metadata(&unrelated)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o000
    );

    // Let TempDir clean up the intentionally untouched fixture.
    std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
#[test]
fn codex_arg0_preflight_has_a_hard_scan_bound() {
    let state = tempfile::tempdir().unwrap();
    let arg0 = state.path().join("tmp/arg0");
    std::fs::create_dir_all(arg0.join("codex-arg0-first")).unwrap();
    std::fs::create_dir(arg0.join("codex-arg0-second")).unwrap();

    assert_eq!(
        repair_codex_arg0_permissions_with_limit(state.path(), 1)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn root_git_common_dir_must_not_overlap_sandbox_writable_state() {
    std::fs::create_dir_all("target").unwrap();
    let safe = tempfile::tempdir_in("target").unwrap();
    assert_eq!(session_git_common_dir(safe.path()), Ok(None));
    std::fs::create_dir(safe.path().join(".git")).unwrap();
    assert_eq!(
        git_common_dir(safe.path()).unwrap(),
        safe.path().join(".git").canonicalize().unwrap()
    );
    assert_eq!(
        session_git_common_dir(safe.path()),
        Ok(Some(safe.path().join(".git").canonicalize().unwrap()))
    );
    assert!(
        validate_root_git_common_dir_policy(
            safe.path(),
            DefaultModel::Claude,
            Some(Path::new("/tmp")),
            None,
            None
        )
        .is_ok()
    );

    let linked = tempfile::tempdir_in("target").unwrap();
    let common = tempfile::tempdir_in("/tmp").unwrap();
    let git_dir = common.path().join("worktrees/linked");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
    std::fs::write(
        linked.path().join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .unwrap();
    assert_eq!(
        git_common_dir(linked.path()).unwrap(),
        common.path().canonicalize().unwrap()
    );
    assert_eq!(
        session_git_common_dir(linked.path()),
        Ok(Some(common.path().canonicalize().unwrap()))
    );
    assert!(
        validate_root_git_common_dir_policy(
            linked.path(),
            DefaultModel::Claude,
            Some(Path::new("/tmp")),
            None,
            None
        )
        .is_err()
    );

    let not_a_directory = tempfile::NamedTempFile::new_in("target").unwrap();
    assert_eq!(session_git_common_dir(not_a_directory.path()), Err(()));

    // The `$HOME` state root covered by this check is the launched agent's own
    // (`~/.codex` for Codex), so a Git common directory under it is refused for
    // that provider while every other provider's own root is unaffected —
    // including `sakana-ai`, which execs the same `claude` as Claude but is
    // checked against its own `~/.claude-sakana`.
    let home = tempfile::tempdir_in("target").unwrap();
    let state = home.path().join(".codex");
    std::fs::create_dir_all(state.join("worktrees/linked")).unwrap();
    let under_state = tempfile::tempdir_in("target").unwrap();
    std::fs::write(
        under_state.path().join(".git"),
        format!("gitdir: {}\n", state.join("worktrees/linked").display()),
    )
    .unwrap();
    std::fs::write(state.join("worktrees/linked/commondir"), "../..\n").unwrap();
    for (agent, allowed) in [
        (DefaultModel::OpenAi, false),
        (DefaultModel::Claude, true),
        (DefaultModel::SakanaAi, true),
        (DefaultModel::Agy, true),
    ] {
        assert_eq!(
            validate_root_git_common_dir_policy(
                under_state.path(),
                agent,
                None,
                Some(&home.path().canonicalize().unwrap()),
                None,
            )
            .is_ok(),
            allowed,
            "{agent:?} must {} a Git common directory under ~/.codex",
            if allowed { "accept" } else { "refuse" }
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One Git authority contract, including every fail-closed edge.
fn session_git_policy_anchors_authority_to_the_registered_worktree() {
    std::fs::create_dir_all("target").unwrap();
    let fixture = tempfile::tempdir_in("target").unwrap();
    let common = fixture.path().join("repo/.git");
    let git_dir = common.join("worktrees/session");
    let worktree = fixture.path().join("repo/.usagi/sessions/session");
    for path in [
        &git_dir,
        &common.join("objects"),
        &common.join("refs/heads/usagi"),
        &common.join("logs/refs/heads/usagi"),
        &worktree,
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .unwrap();
    std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
    std::fs::write(
        git_dir.join("gitdir"),
        format!("{}\n", worktree.join(".git").display()),
    )
    .unwrap();

    let policy = session_git_policy(&fixture.path().join("repo"), &worktree)
        .unwrap()
        .unwrap();
    assert_eq!(
        policy.writable_roots,
        [
            git_dir.canonicalize().unwrap(),
            common.join("objects").canonicalize().unwrap(),
            common.join("refs/heads/usagi").canonicalize().unwrap(),
            common.join("logs/refs/heads/usagi").canonicalize().unwrap(),
        ]
    );
    assert!(
        !policy
            .writable_roots
            .contains(&common.canonicalize().unwrap())
    );
    assert!(
        !policy
            .writable_roots
            .contains(&common.join("refs/heads/main"))
    );
    assert!(!policy.writable_roots.contains(&common.join("config")));

    let standalone = tempfile::tempdir_in("target").unwrap();
    std::fs::create_dir(standalone.path().join(".git")).unwrap();
    assert!(
        session_git_policy(standalone.path(), standalone.path())
            .unwrap()
            .is_none(),
        "a standalone workspace needs no grant outside its writable root"
    );
    assert!(
        session_git_policy(&fixture.path().join("repo"), standalone.path()).is_err(),
        "a foreign standalone repository is not session authority"
    );

    let plain_workspace = tempfile::tempdir_in("target").unwrap();
    let plain_session = plain_workspace.path().join(".usagi/sessions/plain");
    std::fs::create_dir_all(&plain_session).unwrap();
    assert!(
        session_git_policy(plain_workspace.path(), &plain_session)
            .unwrap()
            .is_none()
    );

    let missing_marker = fixture.path().join("repo/.usagi/sessions/missing");
    std::fs::create_dir_all(&missing_marker).unwrap();
    assert!(
        session_git_policy(&fixture.path().join("repo"), &missing_marker).is_err(),
        "a Git workspace cannot silently admit a session without its marker"
    );
    let not_a_directory = fixture.path().join("repo/not-a-directory");
    std::fs::write(&not_a_directory, "fixture").unwrap();
    assert!(
        session_git_policy(&fixture.path().join("repo"), &not_a_directory).is_err(),
        "metadata errors other than absence remain admission failures"
    );

    let indirect = common.join("indirect/session");
    std::fs::create_dir_all(&indirect).unwrap();
    std::fs::write(indirect.join("commondir"), "../..\n").unwrap();
    std::fs::write(
        indirect.join("gitdir"),
        format!("{}\n", worktree.join(".git").display()),
    )
    .unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", indirect.display()),
    )
    .unwrap();
    assert!(
        session_git_policy(&fixture.path().join("repo"), &worktree).is_err(),
        "the private admin directory must be a direct worktrees child"
    );

    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .unwrap();
    std::fs::write(
        git_dir.join("gitdir"),
        format!("{}\n", standalone.path().join(".git").display()),
    )
    .unwrap();
    assert!(
        session_git_policy(&fixture.path().join("repo"), &worktree).is_err(),
        "the private admin backlink must name the selected marker"
    );

    let foreign = fixture.path().join("foreign/.git/worktrees/session");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("commondir"), "../..\n").unwrap();
    std::fs::write(
        foreign.join("gitdir"),
        format!("{}\n", worktree.join(".git").display()),
    )
    .unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", foreign.display()),
    )
    .unwrap();
    assert!(session_git_policy(&fixture.path().join("repo"), &worktree).is_err());
}

#[test]
fn a_session_claude_is_confined_to_its_worktree_and_gets_the_guard_hook() {
    let usagi = Path::new("/opt/usagi/bin/usagi");
    let context = provision_context(Some(SessionId::new()));
    let mode = sandbox_mode(&context);
    assert_eq!(mode, SandboxMode::Session);

    let roots = claude_writable_roots(mode, Path::new("/repo/.usagi/sessions/work"));
    assert_eq!(roots, [PathBuf::from("/repo/.usagi/sessions/work")]);

    let launcher = claude_sandbox_launcher(
        usagi,
        mode,
        DefaultModel::Claude,
        Path::new("/repo"),
        &SandboxLauncherPaths::default(),
        &roots,
        &[],
    )
    .unwrap();
    assert_eq!(launcher.program, "/opt/usagi/bin/usagi");
    assert_eq!(
        launcher.prefix,
        [
            "claude-sandbox",
            "--mode",
            "session",
            // Which provider's `$HOME` state the launcher grants cannot be
            // read off the program: Claude and `sakana-ai` share `claude`.
            "--agent",
            "claude",
            "--protected-root",
            "/repo",
            "--writable-root",
            "/repo/.usagi/sessions/work",
            "--",
        ]
    );

    // A session launch carries the same universal policy paths a root
    // coordinator does. Withholding them does not confine the agent to its
    // worktree — it leaves Claude Code unable to create its fixed
    // `/tmp/claude-<uid>` scratchpad on every tool call, and restarts it
    // against an empty `~/.claude` (first-run flow, no settings, no
    // permission mode) on every launch.
    let universal = claude_sandbox_launcher(
        usagi,
        mode,
        DefaultModel::Claude,
        Path::new("/repo"),
        &SandboxLauncherPaths {
            backend: Some(Path::new("/usr/bin/sandbox-exec")),
            tmpdir: Some(Path::new("/tmp/user")),
            home: Some(Path::new("/home/dev")),
            cache_dir: Some(Path::new("/cache")),
        },
        &roots,
        &[],
    )
    .unwrap();
    assert_eq!(
        universal.prefix,
        [
            "claude-sandbox",
            "--mode",
            "session",
            "--agent",
            "claude",
            "--protected-root",
            "/repo",
            "--backend",
            "/usr/bin/sandbox-exec",
            "--tmpdir",
            "/tmp/user",
            "--cache-dir",
            "/cache",
            "--home",
            "/home/dev",
            "--writable-root",
            "/repo/.usagi/sessions/work",
            "--",
        ]
    );

    let arguments = claude_settings_arguments(usagi).unwrap();
    assert_eq!(arguments[0], "--settings");
    let settings: serde_json::Value = serde_json::from_str(&arguments[1]).unwrap();
    let pre_tool_use = settings["hooks"]["PreToolUse"][0]["hooks"]
        .as_array()
        .unwrap();
    assert_eq!(
        pre_tool_use[1]["args"],
        serde_json::json!(["guard-workspace"])
    );
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["hooks"][0]["args"],
        serde_json::json!(["agent-phase", "ready"])
    );
}

#[test]
fn root_policy_accepts_the_per_user_cache_root() {
    // macOS の Keychain 検索は per-user の MDS cache を更新する。root sandbox がここへ
    // 書けないと agent CLI は Keychain の credential を読めず、古い file 側 credential へ
    // fallback して 401 で起動できない。launcher へは cache root を渡し、writable にする
    // subpath（`<cache>/mds`）は core の純粋な決定部が決める。
    let usagi = Path::new("/opt/usagi/bin/usagi");
    let launcher = claude_sandbox_launcher(
        usagi,
        SandboxMode::Root,
        DefaultModel::Claude,
        Path::new("/repo"),
        &SandboxLauncherPaths {
            cache_dir: Some(Path::new("/private/var/folders/ab/cd/C")),
            ..SandboxLauncherPaths::default()
        },
        &[],
        &[],
    )
    .unwrap();
    assert!(
        launcher
            .prefix
            .windows(2)
            .any(|pair| pair[0] == "--cache-dir" && pair[1] == "/private/var/folders/ab/cd/C"),
        "{:?}",
        launcher.prefix
    );

    // daemon 側の gate も cache root を writable root と同じ規則で検証する。
    std::fs::create_dir_all("target").unwrap();
    let workspace = tempfile::tempdir_in("target").unwrap();
    std::fs::create_dir(workspace.path().join(".git")).unwrap();
    let cache = tempfile::tempdir_in("target").unwrap();
    let backend_dir = tempfile::tempdir().unwrap();
    let backend = backend_dir.path().join("backend");
    std::fs::write(&backend, "fixture").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let backend = backend.canonicalize().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let cache_root = cache.path().canonicalize().unwrap();
    let validate = |cache_dir: Option<&Path>| {
        validate_claude_sandbox_policy(&SandboxPolicyInputs {
            mode: SandboxMode::Root,
            agent: DefaultModel::Claude,
            workspace_root: &workspace_root,
            launch_roots: &[],
            tmpdir: None,
            home: None,
            cache_dir,
            backend: Some(&backend),
            passthrough: false,
            read_only_roots: &[],
        })
    };
    assert_eq!(validate(Some(&cache_root)), Ok(()));
    // 判定の対象は grant する `<cache>/mds` である。workspace がその中にある構成だけを
    // 拒否し、workspace の単なる兄弟（`<cache>/…`）は grant と重ならないので通す。
    let overlapping = tempfile::tempdir_in("target").unwrap();
    let overlapping_root = overlapping.path().canonicalize().unwrap();
    let nested_workspace = claude_sandbox::macos_mds_cache_root(&overlapping_root).join("repo");
    std::fs::create_dir_all(nested_workspace.join(".git")).unwrap();
    assert_eq!(
        validate_claude_sandbox_policy(&SandboxPolicyInputs {
            mode: SandboxMode::Root,
            agent: DefaultModel::Claude,
            workspace_root: &nested_workspace,
            launch_roots: &[],
            tmpdir: None,
            home: None,
            cache_dir: Some(&overlapping_root),
            backend: Some(&backend),
            passthrough: false,
            read_only_roots: &[],
        }),
        Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor)
    );
    // cache root 自体は実在しなければならない（grant の親を所有者ごと確かめる）。
    assert_eq!(
        validate(Some(&cache_root.join("missing"))),
        Err(ClaudeSandboxPolicyError::InvalidWritableRoot)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn the_macos_cache_root_resolves_to_an_owned_canonical_directory() {
    // bootstrap が実際に確定できることを実 platform で確かめる。ここが None のままだと
    // root sandbox は per-user MDS cache を許可できず、Keychain 検索が壊れる。
    let cache = resolve_sandbox_cache_dir().unwrap();
    assert!(cache.is_absolute() && cache.is_dir());
    assert_eq!(validate_owned_directory(&cache), Ok(()));
}

#[test]
fn session_sandbox_policy_rejects_root_workspace_ancestors_and_symlink_aliases() {
    let workspace = tempfile::tempdir().unwrap();
    let owned = tempfile::tempdir().unwrap();
    let backend_dir = tempfile::tempdir().unwrap();
    let backend = backend_dir.path().join("backend");
    std::fs::write(&backend, "fixture").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let backend = backend.canonicalize().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let owned_root = owned.path().canonicalize().unwrap();
    let validate = |roots: &[PathBuf], tmpdir: Option<&Path>| {
        validate_claude_sandbox_policy(&SandboxPolicyInputs {
            mode: SandboxMode::Session,
            agent: DefaultModel::Claude,
            workspace_root: &workspace_root,
            launch_roots: roots,
            tmpdir,
            home: None,
            cache_dir: None,
            backend: Some(&backend),
            passthrough: false,
            read_only_roots: &[],
        })
    };

    assert_eq!(
        validate(&[PathBuf::from("/")], None),
        Err(ClaudeSandboxPolicyError::InvalidWritableRoot)
    );
    assert_eq!(
        validate(std::slice::from_ref(&workspace_root), None),
        Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor)
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let alias = owned_root.with_extension("alias");
        symlink(&owned_root, &alias).unwrap();
        assert_eq!(
            validate(&[], Some(&alias)),
            Err(ClaudeSandboxPolicyError::InvalidWritableRoot)
        );
        std::fs::remove_file(alias).unwrap();
    }
}

#[test]
fn root_sandbox_policy_checks_the_state_root_of_the_agent_it_launches() {
    // The launcher grants the state directory of the CLI it execs, so the daemon
    // checks that same directory against the protected workspace. A workspace
    // living inside `~/.codex` is refused for Codex, accepted for a provider whose
    // state is elsewhere, and unaffected by a program usagi does not launch.
    let backend_dir = tempfile::tempdir_in("/tmp").unwrap();
    let backend = backend_dir.path().join("backend");
    std::fs::write(&backend, "fixture").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let backend = backend.canonicalize().unwrap();
    // The fixture home stays outside `/tmp`, which a root launch may write in its
    // own right: a Git common directory under it is refused for every provider.
    std::fs::create_dir_all("target").unwrap();
    let home = tempfile::tempdir_in("target").unwrap();
    let home = home.path().canonicalize().unwrap();
    let workspace_root = home.join(".codex/repo");
    std::fs::create_dir_all(workspace_root.join(".git")).unwrap();

    for (agent, expected) in [
        (
            DefaultModel::OpenAi,
            Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor),
        ),
        (DefaultModel::Claude, Ok(())),
        // `sakana-ai` execs the same `claude`, and is still judged against
        // its own `~/.claude-sakana` rather than that shared program.
        (DefaultModel::SakanaAi, Ok(())),
        (DefaultModel::Agy, Ok(())),
    ] {
        assert_eq!(
            validate_claude_sandbox_policy(&SandboxPolicyInputs {
                mode: SandboxMode::Root,
                agent,
                workspace_root: &workspace_root,
                launch_roots: &[],
                tmpdir: None,
                home: Some(&home),
                cache_dir: None,
                backend: Some(&backend),
                passthrough: false,
                read_only_roots: &[],
            }),
            expected,
            "{agent:?} state root against a workspace inside ~/.codex"
        );
    }

    // This gate exists to mirror the grant the launcher will actually hand
    // out, and that grant is keyed by provider. Both launches below name the
    // same `claude` program, so a gate that read the state root off the argv
    // would accept a workspace sitting inside the very directory it is about
    // to make writable — and refuse the harmless one.
    for (state, refused) in [
        (".claude", DefaultModel::Claude),
        (".claude-sakana", DefaultModel::SakanaAi),
    ] {
        let shared_workspace = home.join(state).join("repo");
        std::fs::create_dir_all(shared_workspace.join(".git")).unwrap();
        for agent in [DefaultModel::Claude, DefaultModel::SakanaAi] {
            assert_eq!(
                validate_claude_sandbox_policy(&SandboxPolicyInputs {
                    mode: SandboxMode::Root,
                    agent,
                    workspace_root: &shared_workspace,
                    launch_roots: &[],
                    tmpdir: None,
                    home: Some(&home),
                    cache_dir: None,
                    backend: Some(&backend),
                    passthrough: false,
                    read_only_roots: &[],
                }),
                if agent == refused {
                    Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor)
                } else {
                    Ok(())
                },
                "{agent:?} against a workspace inside ~/{state}"
            );
        }
    }

    let prefix_workspace = home.join(".claude.json-repository");
    std::fs::create_dir_all(prefix_workspace.join(".git")).unwrap();
    for mode in [SandboxMode::Session, SandboxMode::Root] {
        assert_eq!(
            validate_claude_sandbox_policy(&SandboxPolicyInputs {
                mode,
                agent: DefaultModel::Claude,
                workspace_root: &prefix_workspace,
                launch_roots: &[],
                tmpdir: None,
                home: Some(&home),
                cache_dir: None,
                backend: Some(&backend),
                passthrough: false,
                read_only_roots: &[],
            }),
            Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor),
            "the lexical ~/.claude.json* grant must not cover a repository"
        );
    }

    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let non_utf8 = PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));
        assert!(lexical_prefix_overlaps_path(&non_utf8, &workspace_root));
    }
}

#[test]
fn a_root_claude_keeps_the_repository_read_only_and_gets_the_guard_hook() {
    let usagi = Path::new("/opt/usagi/bin/usagi");
    let mode = sandbox_mode(&provision_context(None));
    assert_eq!(mode, SandboxMode::Root);

    // A root launch's cwd and daemon data stay read-only; bootstrap uses the broker.
    let roots = claude_writable_roots(mode, Path::new("/repo"));
    assert!(roots.is_empty());
    let launcher = claude_sandbox_launcher(
        usagi,
        mode,
        DefaultModel::Claude,
        Path::new("/repo"),
        &SandboxLauncherPaths::default(),
        &roots,
        &[],
    )
    .unwrap();
    assert_eq!(&launcher.prefix[..3], ["claude-sandbox", "--mode", "root"]);
    assert_eq!(launcher.prefix.last().unwrap(), "--");

    let arguments = claude_settings_arguments(usagi).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&arguments[1]).unwrap();
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][1]["args"],
        serde_json::json!(["guard-workspace"])
    );
    // Lifecycle phase reporting stays wired for a root coordinator.
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][0]["args"],
        serde_json::json!(["agent-phase", "running"])
    );
}

#[derive(Clone)]
struct TestTerminalScope {
    scope: TerminalLaunchScope,
    working_directory: PathBuf,
}

impl TerminalScopeResolver for TestTerminalScope {
    fn resolve_available_scope(
        &self,
        scope: &TerminalLaunchScope,
    ) -> Result<ResolvedTerminalScope, TerminalScopeResolveError> {
        (scope == &self.scope)
            .then(|| ResolvedTerminalScope {
                scope: self.scope.clone(),
                working_directory: self.working_directory.clone(),
            })
            .ok_or(TerminalScopeResolveError::Unavailable)
    }
}

#[derive(Default)]
struct TestTerminalStore;

impl TerminalStore for TestTerminalStore {
    fn save(&mut self, _: TerminalStoreSnapshot) -> Result<(), ()> {
        Ok(())
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RestartEffects {
    spawns: usize,
    selections: usize,
    resizes: usize,
    writes: usize,
}

struct RestartPty(Arc<Mutex<RestartEffects>>);

impl GenericPtySpawner for RestartPty {
    fn spawn(
        &mut self,
        _: &usagi_core::domain::terminal_launch::ResolvedTerminalLaunch,
        _: &TerminalRef,
        _: Geometry,
    ) -> Result<ProcessIdentity, SpawnFailure> {
        self.0.lock().unwrap().spawns += 1;
        Ok(ProcessIdentity {
            pid: 7,
            start_identity: "restart-test".to_owned(),
            process_group: 7,
        })
    }
}

impl PtyWriter for RestartPty {
    fn select_terminal(&mut self, _: &TerminalRef) {
        self.0.lock().unwrap().selections += 1;
    }

    fn resize(&mut self, _: &TerminalRef, _: Geometry) -> Result<(), PtyWriteError> {
        self.0.lock().unwrap().resizes += 1;
        Ok(())
    }

    fn write_all(&mut self, _: &[u8]) -> Result<(), PtyWriteError> {
        self.0.lock().unwrap().writes += 1;
        Ok(())
    }
}

#[test]
fn generic_pty_reports_child_exit_after_the_shell_exits() {
    let directory = tempfile::tempdir().unwrap();
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    let request = TerminalLaunchRequest {
        profile_id: TerminalProfileId::new("login-shell").unwrap(),
        scope: TerminalLaunchScope {
            workspace_id: terminal.workspace_id,
            session_id: terminal.session_id,
            worktree_id: terminal.worktree_id,
        },
    };
    let launch = TrustedLoginShell {
        workspaces: None,
        profile: LoginShellProfile::new(BTreeMap::new(), directory.path().to_path_buf()),
        environment: None,
        workspace_root: PathBuf::new(),
    }
    .resolve(&request)
    .unwrap();
    let metrics = Arc::new(TerminalPipelineMetrics::default());
    let shutdown = Arc::new(ShutdownRequest::new());
    let (mut pty, observations) = DaemonPty::new(
        metrics,
        Arc::new(SpawnedChildren::default()),
        Arc::clone(&shutdown),
    );

    pty.spawn(&launch, &terminal, Geometry { cols: 80, rows: 24 })
        .unwrap();
    pty.resize(&terminal, Geometry { cols: 91, rows: 37 })
        .unwrap();
    pty.select_terminal(&terminal);
    pty.write_all(b"exit\n").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match observations.recv_timeout(remaining).unwrap() {
            PtyObservation::Output(_, _) => {}
            PtyObservation::Exited(exited, status, _) => {
                assert_eq!(exited, terminal);
                assert_eq!(status, 0);
                break;
            }
            PtyObservation::Shutdown => panic!("unexpected observer shutdown"),
        }
    }
    assert!(!shutdown.is_requested());
}

#[test]
fn full_pty_observation_queue_backpressures_without_reordering() {
    let metrics = Arc::new(TerminalPipelineMetrics::default());
    let terminal = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    sender
        .send(PtyObservation::Output(terminal.clone(), vec![1]))
        .unwrap();
    let blocked_sender = sender;
    let blocked_metrics = Arc::clone(&metrics);
    let blocked_terminal = terminal.clone();
    let producer = std::thread::spawn(move || {
        send_pty_observation(
            &blocked_sender,
            PtyObservation::Output(blocked_terminal.clone(), vec![2; 7]),
            7,
            &blocked_metrics,
        )
        .unwrap();
        blocked_sender
            .send(PtyObservation::Exited(blocked_terminal, 0, None))
            .unwrap();
    });

    let deadline = Instant::now() + Duration::from_secs(1);
    while metrics.backpressured_bytes.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(metrics.backpressured_bytes.load(Ordering::Relaxed), 7);
    assert!(matches!(
        receiver.recv().unwrap(),
        PtyObservation::Output(_, bytes) if bytes == [1]
    ));
    assert!(matches!(
        receiver.recv().unwrap(),
        PtyObservation::Output(_, bytes) if bytes == [2; 7]
    ));
    assert!(matches!(
        receiver.recv().unwrap(),
        PtyObservation::Exited(actual, 0, None) if actual == terminal
    ));
    producer.join().unwrap();
}

/// A [`ChildProcessProbe`] the test writes the OS's answers into.
///
/// Pid reuse is the case this fix turns on, and it cannot be raced for
/// against a real kernel: here the test simply says that the same number now
/// answers as a different process. A pid with no answer is a process the
/// platform cannot see.
#[derive(Default)]
struct ScriptedProbe(Mutex<BTreeMap<u32, (String, u32)>>);

impl ScriptedProbe {
    fn answers(&self, pid: u32, start_identity: &str, process_group: u32) {
        self.0
            .lock()
            .unwrap()
            .insert(pid, (start_identity.to_owned(), process_group));
    }

    /// One lookup behind both reads, so a pid is either a whole process or
    /// no process at all — the platform never half-answers here.
    fn answer(&self, pid: u32) -> std::io::Result<(String, u32)> {
        self.0
            .lock()
            .unwrap()
            .get(&pid)
            .cloned()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
    }
}

impl ChildProcessProbe for ScriptedProbe {
    fn start_identity(&self, pid: u32) -> std::io::Result<String> {
        self.answer(pid).map(|(start_identity, _)| start_identity)
    }

    fn process_group(&self, pid: u32) -> std::io::Result<u32> {
        self.answer(pid).map(|(_, process_group)| process_group)
    }
}

#[test]
fn an_observed_child_stays_provable_until_its_release_is_dropped() {
    let children = Arc::new(SpawnedChildren::default());
    let probe = ScriptedProbe::default();
    probe.answers(4242, "start-a", 4242);

    let (identity, release) = children.observe(&probe, 4242, "daemon-owned-pty");
    assert_eq!(identity.start_identity, "start-a");
    let authority = ObservedChildren(Arc::clone(&children));
    // The durable store may only call a record `Running` while the child is
    // provable, so the proof has to outlive everything up to the exit commit.
    assert!(authority.verified(&identity).is_some());
    assert_eq!(children.0.lock().unwrap().len(), 1);

    drop(release);
    assert!(authority.verified(&identity).is_none());
    assert!(children.0.lock().unwrap().is_empty());
}

#[test]
fn releasing_an_exited_child_leaves_the_pid_its_successor_took() {
    const REUSED: u32 = 4243;
    let children = Arc::new(SpawnedChildren::default());
    let probe = ScriptedProbe::default();
    probe.answers(REUSED, "first-start", REUSED);
    let (first, first_release) = children.observe(&probe, REUSED, "daemon-owned-pty");

    // The kernel reaped the first child and handed the number to the next one.
    probe.answers(REUSED, "second-start", REUSED);
    let (second, second_release) = children.observe(&probe, REUSED, "daemon-owned-pty");

    let authority = ObservedChildren(Arc::clone(&children));
    drop(first_release);
    assert!(authority.verified(&first).is_none());
    assert!(
        authority.verified(&second).is_some(),
        "the live child lost the proof its namesake released"
    );
    assert_eq!(children.0.lock().unwrap().len(), 1);

    drop(second_release);
    assert!(authority.verified(&second).is_none());
    assert!(children.0.lock().unwrap().is_empty());
}

#[test]
fn a_long_run_of_short_lived_children_returns_the_registry_to_its_baseline() {
    const CHILDREN: u32 = 1024;
    let children = Arc::new(SpawnedChildren::default());
    let probe = ScriptedProbe::default();

    for pid in 1..=CHILDREN {
        probe.answers(pid, &format!("start-{pid}"), pid);
        let (_, release) = children.observe(&probe, pid, "daemon-owned-pty");
        assert!(release.is_some());
        drop(release);
        let observed = children.0.lock().unwrap().len();
        assert_eq!(observed, 0, "child {pid} left {observed} proof(s) behind");
    }
}

#[test]
fn a_child_the_platform_cannot_read_records_no_proof_and_needs_no_release() {
    let children = Arc::new(SpawnedChildren::default());

    let (identity, release) = children.observe(&ScriptedProbe::default(), 7, "daemon-owned-pty");

    // The unverifiable token stays visible so the record fails closed, but
    // nothing was recorded, so there is nothing to release either.
    assert_eq!(identity.start_identity, "daemon-owned-pty");
    assert_eq!(identity.process_group, 7);
    assert!(release.is_none());
    assert!(children.0.lock().unwrap().is_empty());
}

#[test]
#[allow(clippy::too_many_lines)] // One isolated process covers both real PTY transport owners.
fn exited_generic_and_agent_pty_transports_return_to_the_fd_baseline() {
    const TERMINALS_PER_OWNER: usize = 24;
    const FD_TOLERANCE: usize = 4;

    if std::env::var_os("USAGI_PTY_RECLAIM_TEST_HELPER").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::daemon::tests::exited_generic_and_agent_pty_transports_return_to_the_fd_baseline",
                "--nocapture",
            ])
            .env("USAGI_PTY_RECLAIM_TEST_HELPER", "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }

    let baseline = std::fs::read_dir("/dev/fd").unwrap().count();
    let metrics = Arc::new(TerminalPipelineMetrics::default());
    let children = Arc::new(SpawnedChildren::default());
    let shutdown = Arc::new(ShutdownRequest::new());
    let (mut generic, generic_observations) = DaemonPty::new(
        Arc::clone(&metrics),
        Arc::clone(&children),
        Arc::clone(&shutdown),
    );
    let (mut agent, agent_observations) =
        AgentPty::new(BTreeMap::new(), metrics, Arc::clone(&children), shutdown);
    let generation = DaemonGeneration::new();

    let generic_scope = TerminalLaunchScope {
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    let generic_request = TerminalLaunchRequest {
        profile_id: TerminalProfileId::new("login-shell").unwrap(),
        scope: generic_scope.clone(),
    };
    let generic_launch = usagi_core::domain::terminal_launch::ResolvedTerminalLaunch::new(
        usagi_core::domain::terminal_launch::DurableTerminalLaunchSnapshot::new(
            generic_request,
            1,
            "/bin/sh",
            vec![
                "-c".to_owned(),
                "printf generic-final; sleep 0.01".to_owned(),
            ],
            PathBuf::from("/"),
            [],
        )
        .unwrap(),
        BTreeMap::new(),
    )
    .unwrap();
    let generic_terminals = (0..TERMINALS_PER_OWNER)
        .map(|_| TerminalRef {
            daemon_generation: generation,
            terminal_id: TerminalId::new(),
            workspace_id: generic_scope.workspace_id,
            session_id: generic_scope.session_id,
            worktree_id: generic_scope.worktree_id,
        })
        .collect::<Vec<_>>();
    for terminal in &generic_terminals {
        generic
            .spawn(&generic_launch, terminal, Geometry { cols: 80, rows: 24 })
            .unwrap();
    }
    reclaim_generic_observations(&mut generic, &generic_observations, TERMINALS_PER_OWNER);
    assert!(generic.terminals.is_empty());

    let profile = AgentProfileId::new("codex").unwrap();
    let agent_scope = usagi_core::domain::agent::LaunchScope {
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    let agent_request = usagi_core::domain::agent::LaunchRequest {
        profile_id: profile.clone(),
        mode: usagi_core::domain::agent::LaunchMode::Interactive,
        model: None,
        resume: false,
        provider_resume: None,
        initial_prompt: None,
        scope: agent_scope.clone(),
        required_capabilities: BTreeSet::new(),
    };
    let plan = usagi_core::domain::agent::LaunchPlan::new(
        profile,
        1,
        "/bin/sh",
        vec!["-c".to_owned(), "printf agent-final; sleep 0.01".to_owned()],
        [],
        PathBuf::from("/"),
    )
    .unwrap();
    let agent_launch = DurableLaunchSnapshot::new(agent_request, plan);
    let agent_terminals = (0..TERMINALS_PER_OWNER)
        .map(|_| TerminalRef {
            daemon_generation: generation,
            terminal_id: TerminalId::new(),
            workspace_id: agent_scope.workspace_id,
            session_id: agent_scope.session_id,
            worktree_id: agent_scope.worktree_id,
        })
        .collect::<Vec<_>>();
    for terminal in &agent_terminals {
        agent
            .spawn(
                &agent_launch,
                &SpawnProvision::new([], Vec::new()),
                terminal,
            )
            .unwrap();
    }
    reclaim_agent_observations(&mut agent, &agent_observations, TERMINALS_PER_OWNER);
    assert!(agent.terminals.is_empty());
    // Both owners spawned real children through the real probe, so the
    // identity registry proves the leak is closed end to end and not only in
    // the unit tests' fake: every observation released exactly its own entry.
    assert!(children.0.lock().unwrap().is_empty());

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let current = std::fs::read_dir("/dev/fd").unwrap().count();
        if current <= baseline + FD_TOLERANCE {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "PTY FDs did not return near baseline"
        );
        std::thread::yield_now();
    }
}

fn reclaim_generic_observations(
    pty: &mut DaemonPty,
    observations: &Receiver<PtyObservation>,
    expected_exits: usize,
) {
    let mut output = BTreeSet::new();
    let mut exits = 0;
    while exits != expected_exits {
        match observations.recv_timeout(Duration::from_secs(5)).unwrap() {
            PtyObservation::Output(terminal, bytes) => {
                assert!(!bytes.is_empty());
                output.insert(terminal.terminal_id.as_str().clone());
            }
            PtyObservation::Exited(terminal, 0, release) => {
                assert!(output.contains(&terminal.terminal_id.as_str()));
                assert!(pty.release(&terminal));
                assert!(!pty.release(&terminal));
                // The observer's contract: the identity proof dies with the
                // observation that reported the exit.
                drop(release);
                exits += 1;
            }
            PtyObservation::Exited(_, status, _) => {
                panic!("unexpected exit status {status}")
            }
            PtyObservation::Shutdown => panic!("unexpected observer shutdown"),
        }
    }
}

fn reclaim_agent_observations(
    pty: &mut AgentPty,
    observations: &Receiver<AgentPtyObservation>,
    expected_exits: usize,
) {
    let mut output = BTreeSet::new();
    let mut exits = 0;
    while exits != expected_exits {
        match observations.recv_timeout(Duration::from_secs(5)).unwrap() {
            AgentPtyObservation::Output(terminal, bytes) => {
                assert!(!bytes.is_empty());
                output.insert(terminal.terminal_id.as_str().clone());
            }
            AgentPtyObservation::Exited(terminal, 0, release) => {
                assert!(output.contains(&terminal.terminal_id.as_str()));
                assert!(pty.release(&terminal));
                assert!(!pty.release(&terminal));
                drop(release);
                exits += 1;
            }
            AgentPtyObservation::Exited(_, status, _) => {
                panic!("unexpected exit status {status}");
            }
            AgentPtyObservation::Shutdown => panic!("unexpected observer shutdown"),
        }
    }
}

/// Waits for `condition`, failing the test rather than hanging if the
/// projection worker never applies the queued work.
fn await_projection(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "the projection worker did not apply queued work"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_projection_worker_owns_every_scan_and_durable_write() {
    let directory = tempfile::tempdir().unwrap();
    let projector = Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
        PrInventoryStore::new(directory.path()),
        GenerationRole::Active,
    ))));
    let projection = Arc::new(PrProjectionQueue::new());
    let shutdown = Arc::new(ShutdownRequest::new());
    let worker = start_pr_projection_worker(
        Arc::clone(&projector),
        Arc::clone(&projection),
        Arc::clone(&shutdown),
    )
    .unwrap();
    let session = SessionId::new();
    let terminal = TerminalId::new();

    // A terminated candidate is credited by the worker, not by the submitter.
    projection.submit_output(
        terminal,
        Some(session),
        b"opened https://github.com/o/r/pull/11\n".to_vec(),
    );
    await_projection(|| {
        projector
            .lock()
            .is_ok_and(|mut projector| !projector.snapshot(session).unwrap().entries.is_empty())
    });

    // A gap must discard the carry instead of joining across dropped bytes.
    projection.submit_output(
        terminal,
        Some(session),
        b" https://github.com/o/r/pu".to_vec(),
    );
    projection.submit_gap(terminal);
    projection.submit_output(terminal, Some(session), b"ll/12\n".to_vec());
    // A candidate the output never terminated is credited when the terminal
    // closes, and not before.
    projection.submit_output(
        terminal,
        Some(session),
        b" https://github.com/o/r/pull/13".to_vec(),
    );
    projection.submit_closed(terminal, Some(session));
    await_projection(|| {
        projector
            .lock()
            .is_ok_and(|mut projector| projector.snapshot(session).unwrap().entries.len() == 2)
    });
    let urls: Vec<String> = projector
        .lock()
        .unwrap()
        .snapshot(session)
        .unwrap()
        .entries
        .iter()
        .map(|entry| entry.identity.as_url().to_owned())
        .collect();
    assert_eq!(
        urls,
        [
            "https://github.com/o/r/pull/11",
            "https://github.com/o/r/pull/13"
        ],
        "pull/12 was split across a gap and must not be synthesized"
    );

    // Closing retires the worker: `recv` returns `None` once drained. The
    // accept worker's guard is what closes it in production, including on an
    // unwind, so the guard's drop is the path under test.
    shutdown.request();
    drop(ClosePrProjectionOnExit {
        projection: Arc::clone(&projection),
    });
    worker.join().unwrap();
    assert_eq!(projection.recv(), None);
}

#[test]
#[allow(clippy::too_many_lines)] // PTY-to-IPC exit observation is one integration scenario.
fn generic_terminal_exit_reaches_its_resume_response() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let worktree = WorktreeId::new();
    let scope = TerminalLaunchScope {
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: worktree,
    };
    let metrics = Arc::new(TerminalPipelineMetrics::default());
    let (pty, observations) = DaemonPty::new(
        metrics,
        Arc::new(SpawnedChildren::default()),
        Arc::new(ShutdownRequest::new()),
    );
    let observer_stop = pty.observations.clone();
    let runtime = Arc::new(Mutex::new(GenericTerminalRuntime::new(
        DaemonGeneration::new(),
        TrustedLoginShell {
            workspaces: None,
            profile: LoginShellProfile::new(BTreeMap::new(), directory.path().to_path_buf()),
            environment: None,
            workspace_root: PathBuf::new(),
        },
        TestTerminalStore,
        pty,
        TestTerminalScope {
            scope: scope.clone(),
            working_directory: directory.path().to_path_buf(),
        },
    )));
    let projection = Arc::new(PrProjectionQueue::new());
    let shutdown = Arc::new(ShutdownRequest::new());
    let observer = start_terminal_observer(
        Arc::downgrade(&runtime),
        observations,
        Arc::clone(&projection),
        Arc::clone(&shutdown),
    )
    .unwrap();
    let projector = start_pr_projection_worker(
        Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
            PrInventoryStore::new(directory.path()),
            GenerationRole::Active,
        )))),
        Arc::clone(&projection),
        Arc::clone(&shutdown),
    )
    .unwrap();
    let connection = ConnectionId::new();
    let client = ClientId::new();
    let launch = TerminalLaunchIntent {
        request: TerminalLaunchRequest {
            profile_id: TerminalProfileId::new("login-shell").unwrap(),
            scope,
        },
        geometry: TerminalGeometry { cols: 80, rows: 24 },
        launch_operation: None,
    };
    let terminal: TerminalRef = serde_json::from_value(
        request_terminal_json(
            &mut *runtime.lock().unwrap(),
            connection,
            client,
            RequestId::new(),
            TerminalAction::Launch,
            serde_json::to_value(TerminalRequest::Launch { intent: launch }).unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap()["terminal"]
            .clone(),
    )
    .unwrap();
    let subscription = request_terminal_json(
        &mut *runtime.lock().unwrap(),
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
    .unwrap()["subscription"]
        .as_u64()
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let racers = [
        (
            TerminalAction::Detach,
            TerminalRequest::Detach {
                terminal: terminal.clone(),
                subscription,
            },
        ),
        (
            TerminalAction::Resize,
            TerminalRequest::Resize {
                terminal: terminal.clone(),
                geometry: TerminalGeometry { cols: 81, rows: 25 },
            },
        ),
        (
            TerminalAction::Input,
            TerminalRequest::Input {
                terminal: terminal.clone(),
                subscription,
                input_seq: 0,
                input_operation: None,
                bytes: b"printf race\n".to_vec(),
            },
        ),
    ]
    .into_iter()
    .map(|(action, request)| {
        let runtime = Arc::clone(&runtime);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            request_terminal_json(
                &mut *runtime.lock().unwrap(),
                connection,
                client,
                RequestId::new(),
                action,
                serde_json::to_value(request).unwrap(),
                SnapshotWire::RawTail,
            )
        })
    })
    .collect::<Vec<_>>();
    for racer in racers {
        if let Err(error) = racer.join().unwrap() {
            assert_eq!(
                error.code,
                usagi_core::infrastructure::ipc::ErrorCode::StaleTarget
            );
        }
    }

    let exit_connection = ConnectionId::new();
    let exit_client = ClientId::new();
    let exit_subscription = request_terminal_json(
        &mut *runtime.lock().unwrap(),
        exit_connection,
        exit_client,
        RequestId::new(),
        TerminalAction::Attach,
        serde_json::to_value(TerminalRequest::Attach {
            terminal: terminal.clone(),
            geometry: None,
        })
        .unwrap(),
        SnapshotWire::RawTail,
    )
    .unwrap()["subscription"]
        .as_u64()
        .unwrap();
    request_terminal_json(
        &mut *runtime.lock().unwrap(),
        exit_connection,
        exit_client,
        RequestId::new(),
        TerminalAction::Input,
        serde_json::to_value(TerminalRequest::Input {
            terminal: terminal.clone(),
            subscription: exit_subscription,
            input_seq: 0,
            input_operation: None,
            bytes: b"exit\n".to_vec(),
        })
        .unwrap(),
        SnapshotWire::RawTail,
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = request_terminal_json(
            &mut *runtime.lock().unwrap(),
            connection,
            client,
            RequestId::new(),
            TerminalAction::Resume,
            serde_json::to_value(TerminalRequest::Resume {
                terminal: terminal.clone(),
                after_offset: 0,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap();
        if response["exited"] == true {
            break;
        }
        assert!(Instant::now() < deadline, "terminal exit was not observed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(runtime.lock().unwrap().exit(&terminal, 0).is_err());
    shutdown.request();
    observer_stop.send(PtyObservation::Shutdown).unwrap();
    projection.close();
    observer.join().unwrap();
    projector.join().unwrap();
}

#[test]
fn restart_from_another_directory_launches_terminals_at_the_restored_root() {
    let temporary = tempfile::tempdir().unwrap();
    let original_root = temporary.path().join("original-root");
    let restart_directory = temporary.path().join("restart-directory");
    let daemon_state = temporary.path().join("shared-daemon");
    std::fs::create_dir_all(&original_root).unwrap();
    std::fs::create_dir_all(&restart_directory).unwrap();

    let first = open_session_runtime(
        original_root.clone(),
        &daemon_state,
        temporary.path(),
        usagi_core::domain::id::DaemonGeneration::new(),
    )
    .unwrap();
    drop(first);
    let restored = open_session_runtime(
        restart_directory,
        &daemon_state,
        temporary.path(),
        usagi_core::domain::id::DaemonGeneration::new(),
    )
    .unwrap();

    let profile =
        LoginShellProfile::new(BTreeMap::new(), trusted_repository_root(&restored).unwrap());
    let launch = profile
        .resolve(&TerminalLaunchRequest {
            profile_id: TerminalProfileId::new("login-shell").unwrap(),
            scope: TerminalLaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: Some(SessionId::new()),
                worktree_id: WorktreeId::new(),
            },
        })
        .unwrap();

    assert_eq!(launch.snapshot.working_directory, original_root);
}

#[test]
fn root_composition_resolves_an_available_session_by_stable_id() {
    struct SuccessfulGit;
    impl usagi_core::infrastructure::git::GitRunner for SuccessfulGit {
        fn run(
            &self,
            _: &Path,
            _: &[&str],
        ) -> anyhow::Result<usagi_core::infrastructure::git::GitOutput> {
            Ok(usagi_core::infrastructure::git::GitOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    struct NoopSessionWorktreeIo;
    impl usagi_daemon::usecase::session_runtime::SessionWorktreeIo for NoopSessionWorktreeIo {
        fn remove_file_best_effort(&self, _: &Path) {}
        fn path_occupied(&self, _: &Path) -> bool {
            false
        }
        fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
            Some(path.to_path_buf())
        }
        fn is_repo_root(&self, _: &Path) -> bool {
            false
        }
        fn is_linked_worktree(&self, _: &Path) -> bool {
            true
        }
        fn build_session_tree(
            &self,
            _: &dyn usagi_core::infrastructure::git::GitRunner,
            _: &Path,
            _: &Path,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn run_setup_command(&self, _: &Path, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove_session_tree(
            &self,
            _: &dyn usagi_core::infrastructure::git::GitRunner,
            _: &Path,
            _: bool,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let temporary = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new(
        SessionRuntime::open(
            temporary.path().join("repository"),
            &temporary.path().join("daemon"),
            DaemonGeneration::new(),
            SuccessfulGit,
            NoopSessionWorktreeIo,
        )
        .unwrap(),
    ));
    perform_create(
        &runtime,
        &SuccessfulGit,
        &usagi_core::domain::id::OperationId::new().to_string(),
        &serde_json::json!({"name": "one"}),
    )
    .unwrap();

    let runtime = runtime.lock().unwrap();
    let session_id = runtime.session_id("one").unwrap();
    assert!(runtime.session_scope_by_id(session_id).is_ok());
}

#[test]
fn root_dispatch_admission_does_not_reparent_existing_session() {
    use usagi_core::domain::agent::{
        Agent, AgentStatus, CallerRef, DispatchBinding, DispatchRun, RunStatus, WorkerRef,
    };
    use usagi_core::domain::id::{AgentId, OperationId as DispatchOperationId};
    use usagi_core::infrastructure::store::dispatch::{
        AgentAdmissionReservation, CredentialProvenance,
    };

    let temporary = tempfile::tempdir().unwrap();
    let dispatch = DispatchStore::new(temporary.path());
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let agent_id = AgentId::new();
    let worker = Agent {
        agent_id,
        session_id: Some(session),
        runtime: AgentProfileId::new("claude").unwrap(),
        model: usagi_core::domain::agent::ModelSelector::new("test").unwrap(),
        status: AgentStatus::Starting,
        current_run: None,
    };
    dispatch.upsert_agent(workspace, worker.clone()).unwrap();
    let initial_parent = SessionId::new();
    dispatch
        .record_session_parent(workspace, session, Some(initial_parent))
        .unwrap();
    dispatch
        .upsert_binding(DispatchBinding {
            run_id: DispatchOperationId::new(),
            caller: CallerRef {
                session_id: Some(SessionId::new()),
                agent_id: AgentId::new(),
            },
            worker: WorkerRef {
                session_id: Some(session),
                agent_id,
            },
        })
        .unwrap();

    let conflicting = DispatchOperationId::new();
    dispatch
        .reserve_admission(
            worker,
            DispatchRun {
                run_id: conflicting,
                agent_id,
                prompt: "dispatch without reparenting".into(),
                started_at: chrono::Utc::now(),
                ended_at: None,
                status: RunStatus::Preparing,
            },
            DispatchBinding {
                run_id: conflicting,
                caller: CallerRef {
                    session_id: Some(SessionId::new()),
                    agent_id: AgentId::new(),
                },
                worker: WorkerRef {
                    session_id: Some(session),
                    agent_id,
                },
            },
            AgentAdmissionReservation {
                operation_id: conflicting,
                semantic_key: "dispatch-existing".into(),
                credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
            },
        )
        .unwrap();
    assert!(dispatch.run(conflicting).unwrap().is_some());
    assert!(dispatch.admission(conflicting).unwrap().is_some());
    assert_eq!(
        dispatch.session_parent(workspace, session).unwrap(),
        Some(initial_parent)
    );
}

/// A delegation builds its worktree before it can dispatch into it, so a
/// daemon that died in that window left a session no caller owns. The next
/// start rolls exactly those back — and leaves the ones whose dispatch did
/// reach the store, because that operation's outcome is the dispatch side's
/// to decide (#611).
#[test]
fn startup_compensates_only_delegated_creates_with_nothing_dispatched() {
    let temporary = tempfile::tempdir().unwrap();
    let sessions = Arc::new(Mutex::new(
        SessionRuntime::open(
            temporary.path().join("repository"),
            &temporary.path().join("daemon"),
            DaemonGeneration::new(),
            AlwaysSuccessfulGit,
            PermissiveSessionWorktreeIo,
        )
        .unwrap(),
    ));
    let dispatch = DispatchStore::new(temporary.path().join("dispatch"));
    let teardown = TeardownSignal::new();
    let delegate = |name: &str| {
        let operation = usagi_core::domain::id::OperationId::new();
        perform_delegated_create(
            &sessions,
            &AlwaysSuccessfulGit,
            &operation.to_string(),
            &serde_json::json!({"name": name}),
        )
        .unwrap();
        operation
    };

    let orphan = delegate("orphan");
    let dispatched = delegate("dispatched");
    // A plain `session_create` is complete on its own and is never a
    // compensation candidate.
    perform_create(
        &sessions,
        &AlwaysSuccessfulGit,
        &usagi_core::domain::id::OperationId::new().to_string(),
        &serde_json::json!({"name": "plain"}),
    )
    .unwrap();
    // The dispatched delegation reached the dispatch store, which now owns
    // that operation's outcome.
    let agent = usagi_core::domain::agent::Agent {
        agent_id: usagi_core::domain::id::AgentId::new(),
        session_id: None,
        runtime: usagi_core::domain::agent::AgentProfileId::new("claude").unwrap(),
        model: usagi_core::domain::agent::ModelSelector::new("test").unwrap(),
        status: usagi_core::domain::agent::AgentStatus::Idle,
        current_run: None,
    };
    dispatch
        .upsert_run(usagi_core::domain::agent::DispatchRun {
            run_id: dispatched,
            agent_id: agent.agent_id,
            prompt: "finish".into(),
            started_at: chrono::Utc::now(),
            ended_at: None,
            status: usagi_core::domain::agent::RunStatus::Running,
        })
        .unwrap();

    let bound = bound_to(
        &temporary.path().join("tenants"),
        &temporary.path().join("repository"),
        Arc::clone(&sessions),
        WorkspaceId::new(),
    );
    assert_eq!(
        reconcile_orphan_delegations(&bound, &dispatch, &teardown),
        1
    );

    let names = |lifecycle: &str| {
        sessions.lock().unwrap().snapshot().unwrap()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|session| session["lifecycle"] == lifecycle)
            .map(|session| session["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(names("deleting"), ["orphan"]);
    assert_eq!(names("available"), ["dispatched", "plain"]);
    // The teardown worker was woken for the admitted compensation, and the
    // durable plan takes the branch with the worktree.
    assert!(teardown.wait(std::time::Duration::from_millis(1)));
    let pending = sessions.lock().unwrap().pending_teardowns().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].force && pending[0].delete_branch);

    // A second start finds nothing new: the orphan is already `Deleting`, so
    // it is the teardown worker's, not another compensation's.
    assert_eq!(
        reconcile_orphan_delegations(&bound, &dispatch, &teardown),
        0
    );
    assert_ne!(orphan, dispatched);
}

#[test]
fn a_failed_delegated_setup_is_compensated_before_dispatch() {
    use usagi_core::domain::agent::CallerRef;
    use usagi_daemon::usecase::session_runtime::{DelegationReconcile, SessionRuntimeError};

    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    std::fs::create_dir_all(repository.join(".usagi")).unwrap();
    std::fs::write(
        repository.join(".usagi/config.toml"),
        "[session]\nsetup_commands = [\"fail\"]\n",
    )
    .unwrap();
    let sessions = Arc::new(Mutex::new(
        SessionRuntime::open(
            repository,
            &temporary.path().join("daemon"),
            DaemonGeneration::new(),
            AlwaysSuccessfulGit,
            FailingSetupSessionWorktreeIo,
        )
        .unwrap(),
    ));
    let caller = CallerRef {
        session_id: Some(SessionId::new()),
        agent_id: usagi_core::domain::id::AgentId::new(),
    };
    let operation_id = usagi_core::domain::id::OperationId::new().to_string();
    let create_error = perform_delegated_create(
        &sessions,
        &AlwaysSuccessfulGit,
        &operation_id,
        &serde_json::json!({
            "name": "setup-failed",
            "parent_session_id": caller.session_id,
            "creator_agent_id": caller.agent_id,
        }),
    )
    .unwrap_err();
    let teardown = TeardownSignal::new();

    let compensated = dispatch::session::compensate_failed_delegated_initialize(
        &sessions,
        &teardown,
        &caller,
        "setup-failed",
        &operation_id,
        create_error,
    );

    let SessionRuntimeError::Delegation(failure) = compensated else {
        panic!("the failed setup must report delegation reconciliation");
    };
    assert_eq!(failure.reconcile, DelegationReconcile::Compensated);
    assert_eq!(failure.run_operation_id, operation_id);
    let pending = sessions.lock().unwrap().pending_teardowns().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].name, "setup-failed");
    assert!(teardown.wait(std::time::Duration::from_millis(1)));

    // A pre-effect error with another operation ID must not compensate an
    // already-existing session that happens to have the requested name.
    std::fs::remove_file(
        sessions
            .lock()
            .unwrap()
            .repository_root()
            .join(".usagi/config.toml"),
    )
    .unwrap();
    perform_create(
        &sessions,
        &AlwaysSuccessfulGit,
        &usagi_core::domain::id::OperationId::new().to_string(),
        &serde_json::json!({
            "name": "existing",
            "parent_session_id": caller.session_id,
            "creator_agent_id": caller.agent_id,
        }),
    )
    .unwrap();
    let pre_effect = SessionRuntimeError::InvalidRole("refused".into());
    assert_eq!(
        dispatch::session::compensate_failed_delegated_initialize(
            &sessions,
            &teardown,
            &caller,
            "existing",
            &usagi_core::domain::id::OperationId::new().to_string(),
            pre_effect.clone(),
        ),
        pre_effect
    );
    assert!(sessions.lock().unwrap().session_id("existing").is_ok());
}

/// Whether a failed dispatch rolls its session back is decided by the failure,
/// not by the caller: an unknown spawn outcome must keep the worktree, because
/// a worker may be running in it (#611).
#[test]
fn an_unknown_spawn_outcome_keeps_the_delegated_session_and_a_definite_one_rolls_it_back() {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    use usagi_daemon::usecase::session_runtime::DelegationReconcile;

    let temporary = tempfile::tempdir().unwrap();
    let sessions = Arc::new(Mutex::new(
        SessionRuntime::open(
            temporary.path().join("repository"),
            &temporary.path().join("daemon"),
            DaemonGeneration::new(),
            AlwaysSuccessfulGit,
            PermissiveSessionWorktreeIo,
        )
        .unwrap(),
    ));
    // Bound before the test poisons the runtime lock: the compensation path
    // under test is the one that meets an unreadable workspace, not a
    // fixture that cannot be built.
    let bound = bound_to(
        &temporary.path().join("tenants"),
        &temporary.path().join("repository"),
        Arc::clone(&sessions),
        WorkspaceId::new(),
    );
    let teardown = TeardownSignal::new();
    let run = usagi_core::domain::id::OperationId::new().to_string();
    let delegate = |name: &str| {
        perform_delegated_create(
            &sessions,
            &AlwaysSuccessfulGit,
            &usagi_core::domain::id::OperationId::new().to_string(),
            &serde_json::json!({"name": name}),
        )
        .unwrap();
        sessions.lock().unwrap().session_id(name).unwrap()
    };
    let compensate = |name: &str, id, code| {
        dispatch::session::compensate_delegation(
            &sessions,
            &teardown,
            id,
            name,
            &run,
            ProtocolError::new(code, "refused"),
        )
    };

    // Unknown: the session stays available and the caller is told to
    // reconcile it.
    let retained_id = delegate("retained");
    let retained = compensate("retained", retained_id, ErrorCode::OwnershipUnknown);
    let SessionRuntimeError::Delegation(retained) = retained else {
        panic!("a failed delegation reports a delegation failure");
    };
    assert_eq!(retained.reconcile, DelegationReconcile::Retained);
    assert_eq!(retained.session_id, retained_id);
    assert_eq!(retained.run_operation_id, run);
    assert!(sessions.lock().unwrap().session_id("retained").is_ok());

    // Definite: the session is rolled back by a durable teardown.
    let rolled_back_id = delegate("rolled-back");
    let compensated = compensate("rolled-back", rolled_back_id, ErrorCode::Unavailable);
    let SessionRuntimeError::Delegation(compensated) = compensated else {
        panic!("a failed delegation reports a delegation failure");
    };
    assert_eq!(compensated.reconcile, DelegationReconcile::Compensated);
    assert_eq!(
        sessions.lock().unwrap().pending_teardowns().unwrap()[0].name,
        "rolled-back"
    );
    // A session the compensation cannot find is also nothing left behind: an
    // earlier attempt's teardown already removed it.
    let already_gone = compensate("never-created", rolled_back_id, ErrorCode::Unavailable);
    let SessionRuntimeError::Delegation(already_gone) = already_gone else {
        panic!("a failed delegation reports a delegation failure");
    };
    assert_eq!(already_gone.reconcile, DelegationReconcile::Compensated);

    // A rollback that cannot be admitted at all leaves the session present,
    // and says so rather than claiming a clean rejection.
    let poisoned = Arc::clone(&sessions);
    let _ = std::thread::spawn(move || {
        let _guard = poisoned.lock().unwrap();
        panic!("poison the session lock");
    })
    .join();
    let failed = compensate("rolled-back", rolled_back_id, ErrorCode::Unavailable);
    let SessionRuntimeError::Delegation(failed) = failed else {
        panic!("a failed delegation reports a delegation failure");
    };
    assert_eq!(failed.reconcile, DelegationReconcile::CompensationFailed);
    // The same poisoned lock makes the startup reconcile report nothing
    // rather than guessing at an empty candidate set.
    assert_eq!(
        reconcile_orphan_delegations(
            &bound,
            &DispatchStore::new(temporary.path().join("dispatch")),
            &teardown
        ),
        0
    );
}

/// A delegation that fails answers with structured state, not a sentence: the
/// caller has to tell a clean rejection from a session that is still there
/// because its worker's fate is unknown (#611).
#[test]
fn a_failed_delegation_reports_its_reconcile_state_on_the_wire() {
    use usagi_core::infrastructure::ipc::{ErrorCode, SideEffect};
    use usagi_daemon::usecase::session_runtime::{DelegationFailure, DelegationReconcile};

    let session_id = SessionId::new();
    let run = usagi_core::domain::id::OperationId::new().to_string();
    let envelope_for = |reconcile: DelegationReconcile, code: ErrorCode| {
        session_response_envelope(
            usagi_core::infrastructure::ipc::SessionAction::DelegateBrief,
            Err(SessionRuntimeError::Delegation(DelegationFailure {
                code,
                message: "dispatch runtime executable is unavailable".into(),
                session_id,
                run_operation_id: run.clone(),
                reconcile,
            })),
            usagi_core::infrastructure::ipc::RequestId("delegate".into()),
            &session_test_hello(),
        )
    };

    let compensated = envelope_for(DelegationReconcile::Compensated, ErrorCode::InvalidArgument);
    let error = response_error(&compensated);
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    // A rolled-back delegation left nothing behind.
    assert_eq!(error.side_effect, SideEffect::None);
    let details = error.details.unwrap();
    assert_eq!(details["reconcile"], "compensated");
    assert_eq!(details["run_operation_id"], run);
    assert_eq!(
        details["session_id"],
        serde_json::to_value(session_id).unwrap()
    );

    for (reconcile, token) in [
        (DelegationReconcile::Retained, "retained"),
        (
            DelegationReconcile::CompensationFailed,
            "compensation_failed",
        ),
    ] {
        let error = response_error(&envelope_for(reconcile, ErrorCode::OwnershipUnknown));
        assert_eq!(error.code, ErrorCode::OwnershipUnknown);
        // Something durable is still there, so the caller must reconcile it
        // rather than assume a clean rejection.
        assert_eq!(error.side_effect, SideEffect::PartialOrUnknown);
        assert_eq!(error.details.unwrap()["reconcile"], token);
    }
}

/// The protocol error one response envelope carries, or a failure naming what
/// it carried instead.
fn response_error(
    envelope: &usagi_core::infrastructure::ipc::Envelope,
) -> usagi_core::infrastructure::ipc::ProtocolError {
    match &envelope.kind {
        usagi_core::infrastructure::ipc::EnvelopeKind::Response {
            outcome: usagi_core::infrastructure::ipc::ResponseOutcome::Error(error),
            ..
        } => error.clone(),
        other => panic!("expected an error response, got {other:?}"),
    }
}

/// A Git runner that reports success for everything, for composition tests
/// whose subject is the durable lifecycle rather than Git.
struct AlwaysSuccessfulGit;
impl usagi_core::infrastructure::git::GitRunner for AlwaysSuccessfulGit {
    fn run(
        &self,
        _: &Path,
        _: &[&str],
    ) -> anyhow::Result<usagi_core::infrastructure::git::GitOutput> {
        Ok(usagi_core::infrastructure::git::GitOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

/// Worktree IO used to prove that a deterministic setup failure enters the
/// delegated-create compensation path before worker dispatch.
struct FailingSetupSessionWorktreeIo;
impl usagi_daemon::usecase::session_runtime::SessionWorktreeIo for FailingSetupSessionWorktreeIo {
    fn remove_file_best_effort(&self, _: &Path) {}
    fn path_occupied(&self, _: &Path) -> bool {
        false
    }
    fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
        Some(path.to_path_buf())
    }
    fn is_repo_root(&self, _: &Path) -> bool {
        false
    }
    fn is_linked_worktree(&self, _: &Path) -> bool {
        true
    }
    fn build_session_tree(
        &self,
        _: &dyn usagi_core::infrastructure::git::GitRunner,
        _: &Path,
        _: &Path,
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn run_setup_command(&self, _: &Path, _: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("injected setup failure"))
    }
    fn remove_session_tree(
        &self,
        _: &dyn usagi_core::infrastructure::git::GitRunner,
        _: &Path,
        _: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Worktree IO that accepts every managed path and performs no effect.
struct PermissiveSessionWorktreeIo;
impl usagi_daemon::usecase::session_runtime::SessionWorktreeIo for PermissiveSessionWorktreeIo {
    fn remove_file_best_effort(&self, _: &Path) {}
    fn path_occupied(&self, _: &Path) -> bool {
        false
    }
    fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
        Some(path.to_path_buf())
    }
    fn is_repo_root(&self, _: &Path) -> bool {
        false
    }
    fn is_linked_worktree(&self, _: &Path) -> bool {
        true
    }
    fn build_session_tree(
        &self,
        _: &dyn usagi_core::infrastructure::git::GitRunner,
        _: &Path,
        _: &Path,
        _: &str,
        _: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn run_setup_command(&self, _: &Path, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove_session_tree(
        &self,
        _: &dyn usagi_core::infrastructure::git::GitRunner,
        _: &Path,
        _: bool,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// One process's durable runtime state over a real data directory.
fn sharded_state(
    data_dir: &Path,
    generation: DaemonGeneration,
) -> usagi_daemon::usecase::resources::durable::ShardedRuntimeState {
    open_runtime_state(
        data_dir,
        generation,
        &Arc::new(SpawnedChildren::default()),
        GENERIC_TERMINAL_LIMIT,
    )
    .unwrap()
}

/// The shard document one generation wrote, as raw bytes.
fn shard_bytes(data_dir: &Path, generation: DaemonGeneration) -> Vec<u8> {
    std::fs::read(shard_path(data_dir, generation)).unwrap()
}

fn shard_path(data_dir: &Path, generation: DaemonGeneration) -> PathBuf {
    data_dir
        .join("daemon")
        .join("shards")
        .join(format!("{}.json", generation.as_str()))
}

/// One durable generic terminal record, reserved and owned by `generation`.
fn reserved_terminal_record(
    generation: DaemonGeneration,
) -> usagi_daemon::usecase::generic_terminal::DurableTerminalRecord {
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let worktree = WorktreeId::new();
    let scope = TerminalLaunchScope {
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: worktree,
    };
    let terminal = TerminalRef {
        daemon_generation: generation,
        terminal_id: TerminalId::new(),
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: worktree,
    };
    usagi_daemon::usecase::generic_terminal::DurableTerminalRecord {
        terminal,
        operation: usagi_core::domain::id::CompletionFence {
            workspace_id: workspace,
            session_id: Some(session),
            operation_id: usagi_core::domain::id::OperationId::new(),
            owner_daemon_generation: generation,
            execution_attempt: 1,
            lifecycle_attempt: 1,
            expected_revision: 1,
        },
        launch: usagi_core::domain::terminal_launch::DurableTerminalLaunchSnapshot::new(
            TerminalLaunchRequest {
                profile_id: TerminalProfileId::new("login-shell").unwrap(),
                scope,
            },
            1,
            "sh",
            Vec::new(),
            PathBuf::from("/tmp"),
            [],
        )
        .unwrap(),
        state: usagi_daemon::usecase::terminal::TerminalRuntimeState::Reserved,
        process: None,
        launch_digest: Some("digest".to_owned()),
    }
}

fn terminal_truth(generation: DaemonGeneration) -> TerminalStoreSnapshot {
    TerminalStoreSnapshot {
        records: vec![reserved_terminal_record(generation)],
        ..TerminalStoreSnapshot::default()
    }
}

#[test]
fn the_terminal_store_writes_this_generations_own_shard() {
    let dir = tempfile::tempdir().unwrap();
    let generation = DaemonGeneration::new();
    let mut store = ShardedTerminalStore::new(sharded_state(dir.path(), generation));

    store.save(terminal_truth(generation)).unwrap();

    // The shard is named after its only writer and carries the record itself.
    let document: serde_json::Value =
        serde_json::from_slice(&shard_bytes(dir.path(), generation)).unwrap();
    assert_eq!(document["owner"], generation.as_str());
    assert_eq!(
        document["schema"],
        usagi_daemon::usecase::resources::shard::SHARD_SCHEMA
    );
    let resources = document["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0]["kind"], "terminal");
    assert_eq!(resources[0]["state"], "reserved");
    assert!(resources[0]["payload"].is_string());
    // The capacity claim is durable before the reservation the spawn follows.
    assert!(dir.path().join("daemon").join("allocations.json").exists());
}

#[test]
#[allow(clippy::too_many_lines)] // Two daemon instances and every fenced effect form one restart contract.
fn generic_terminal_restart_hydrates_inventory_and_preserves_records() {
    let dir = tempfile::tempdir().unwrap();
    let first_generation = DaemonGeneration::new();
    let second_generation = DaemonGeneration::new();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let worktree = WorktreeId::new();
    let scope = TerminalLaunchScope {
        workspace_id: workspace,
        session_id: Some(session),
        worktree_id: worktree,
    };
    let request = TerminalLaunchRequest {
        profile_id: TerminalProfileId::new("login-shell").unwrap(),
        scope: scope.clone(),
    };
    let first_effects = Arc::new(Mutex::new(RestartEffects::default()));
    let mut first = GenericTerminalRuntime::new(
        first_generation,
        TrustedLoginShell {
            workspaces: None,
            profile: LoginShellProfile::new(BTreeMap::new(), dir.path().to_path_buf()),
            environment: None,
            workspace_root: PathBuf::new(),
        },
        ShardedTerminalStore::new(sharded_state(dir.path(), first_generation)),
        RestartPty(Arc::clone(&first_effects)),
        TestTerminalScope {
            scope: scope.clone(),
            working_directory: dir.path().to_path_buf(),
        },
    );
    let old_terminal: TerminalRef = serde_json::from_value(
        request_terminal_json(
            &mut first,
            ConnectionId::new(),
            ClientId::new(),
            RequestId::new(),
            TerminalAction::Launch,
            serde_json::to_value(TerminalRequest::Launch {
                intent: TerminalLaunchIntent {
                    request: request.clone(),
                    geometry: TerminalGeometry { cols: 80, rows: 24 },
                    launch_operation: None,
                },
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap()["terminal"]
            .clone(),
    )
    .unwrap();
    assert_eq!(first_effects.lock().unwrap().spawns, 1);
    drop(first);

    // The restarted process owns a new generation, so the old record reaches it
    // through the retained shard its dead owner wrote, never by rewriting it.
    let before_restart = sharded_state(dir.path(), second_generation)
        .hydrate()
        .unwrap();
    assert_eq!(before_restart.interrupted, 1);
    let old_record = before_restart.terminals.records[0].clone();
    let reconciled = before_restart.terminals;
    let second_effects = Arc::new(Mutex::new(RestartEffects::default()));
    let second_store = ShardedTerminalStore::new(sharded_state(dir.path(), second_generation));
    let mut second = GenericTerminalRuntime::from_snapshot(
        second_generation,
        TrustedLoginShell {
            workspaces: None,
            profile: LoginShellProfile::new(BTreeMap::new(), dir.path().to_path_buf()),
            environment: None,
            workspace_root: PathBuf::new(),
        },
        second_store,
        RestartPty(Arc::clone(&second_effects)),
        TestTerminalScope {
            scope: scope.clone(),
            working_directory: dir.path().to_path_buf(),
        },
        reconciled,
    )
    .unwrap();

    let inventory = TerminalOwner::inventory(&second, &scope);
    assert_eq!(inventory.len(), 1);
    assert!(inventory[0].terminal.fences(&old_terminal));
    assert!(!inventory[0].live);
    for (action, request) in [
        (
            TerminalAction::Attach,
            TerminalRequest::Attach {
                terminal: old_terminal.clone(),
                geometry: None,
            },
        ),
        (
            TerminalAction::Resize,
            TerminalRequest::Resize {
                terminal: old_terminal.clone(),
                geometry: TerminalGeometry {
                    cols: 100,
                    rows: 40,
                },
            },
        ),
        (
            TerminalAction::Input,
            TerminalRequest::Input {
                terminal: old_terminal.clone(),
                subscription: 1,
                input_seq: 0,
                input_operation: None,
                bytes: b"must-not-run".to_vec(),
            },
        ),
    ] {
        let error = request_terminal_json(
            &mut second,
            ConnectionId::new(),
            ClientId::new(),
            RequestId::new(),
            action,
            serde_json::to_value(request).unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap_err();
        assert_eq!(
            error.code,
            usagi_core::infrastructure::ipc::ErrorCode::OwnershipUnknown
        );
    }
    assert_eq!(*second_effects.lock().unwrap(), RestartEffects::default());

    let new_terminal: TerminalRef = serde_json::from_value(
        request_terminal_json(
            &mut second,
            ConnectionId::new(),
            ClientId::new(),
            RequestId::new(),
            TerminalAction::Launch,
            serde_json::to_value(TerminalRequest::Launch {
                intent: TerminalLaunchIntent {
                    request,
                    geometry: TerminalGeometry { cols: 80, rows: 24 },
                    launch_operation: None,
                },
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap()["terminal"]
            .clone(),
    )
    .unwrap();
    assert!(!new_terminal.fences(&old_terminal));
    assert_eq!(second_effects.lock().unwrap().spawns, 1);

    let after_launch = sharded_state(dir.path(), second_generation)
        .hydrate()
        .unwrap()
        .terminals;
    assert_eq!(after_launch.records.len(), 2);
    // The old owner's shard still holds its record exactly as it left it.
    assert!(!shard_bytes(dir.path(), first_generation).is_empty());
    let retained = after_launch
        .records
        .iter()
        .find(|record| record.terminal.fences(&old_terminal))
        .unwrap();
    assert_eq!(retained.terminal, old_record.terminal);
    assert_eq!(retained.operation, old_record.operation);
    assert_eq!(retained.launch, old_record.launch);
    assert_eq!(
        retained.state,
        usagi_daemon::usecase::terminal::TerminalRuntimeState::ReconcileRequired(
            usagi_daemon::usecase::terminal::TerminalReconcileState::IdentityUnknown,
        )
    );
}

#[test]
fn a_corrupt_or_unknown_shard_fails_closed_without_effect_or_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let generation = DaemonGeneration::new();
    // Write the shard the way a first start would.
    ShardedTerminalStore::new(sharded_state(dir.path(), generation))
        .save(terminal_truth(generation))
        .unwrap();
    let path = shard_path(dir.path(), generation);
    for bytes in [
        b"{broken".as_slice(),
        br#"{"schema":"usagi-owner-shard-v999"}"#.as_slice(),
    ] {
        std::fs::write(&path, bytes).unwrap();
        let preserved = std::fs::read(&path).unwrap();
        assert!(sharded_state(dir.path(), generation).hydrate().is_err());
        // Startup fails closed and leaves the last bytes for inspection.
        assert_eq!(std::fs::read(&path).unwrap(), preserved);
    }
}

#[test]
fn both_stores_share_one_shard_without_clobbering_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let generation = DaemonGeneration::new();
    let mut agents = ShardedAgentStore::new(sharded_state(dir.path(), generation));
    let mut terminals = ShardedTerminalStore::new(sharded_state(dir.path(), generation));

    // Two stores, one document, and every write a compare-and-swap: the Agent
    // save must not erase the terminal reservation or the other way round.
    terminals.save(terminal_truth(generation)).unwrap();
    agents.save(RuntimeStoreSnapshot::default()).unwrap();

    let document: serde_json::Value =
        serde_json::from_slice(&shard_bytes(dir.path(), generation)).unwrap();
    assert_eq!(document["owner"], generation.as_str());
    assert_eq!(document["resources"].as_array().unwrap().len(), 1);
    assert_eq!(document["resources"][0]["kind"], "terminal");
}

#[test]
fn a_legacy_store_this_build_cannot_read_is_never_sealed() {
    for bytes in [
        b"{not-json".as_slice(),
        br#"{"schema_version":999,"records":[]}"#.as_slice(),
    ] {
        let dir = tempfile::tempdir().unwrap();
        // The archive creates the private directories a daemon start needs.
        let state = sharded_state(dir.path(), DaemonGeneration::new());
        let daemon = dir.path().join("daemon");
        let legacy = daemon.join("agents.json");
        std::fs::write(&legacy, bytes).unwrap();
        let before = std::fs::read(&legacy).unwrap();

        assert!(state.hydrate().is_err());
        // The legacy bytes stay exactly where they are: nothing is migrated,
        // renamed, or marked, so a fix or a rollback is still possible.
        assert_eq!(std::fs::read(&legacy).unwrap(), before);
        assert!(!daemon.join("runtime-migration.json").exists());
    }
}

#[test]
fn a_legacy_store_is_migrated_once_and_retired_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let legacy_generation = DaemonGeneration::new();
    let state = sharded_state(dir.path(), DaemonGeneration::new());
    let daemon = dir.path().join("daemon");
    std::fs::write(
        daemon.join("terminals.json"),
        serde_json::to_vec(&terminal_truth(legacy_generation)).unwrap(),
    )
    .unwrap();

    let hydrated = state.hydrate().unwrap();
    let marker = hydrated.migration.unwrap().marker;
    assert_eq!(
        marker.schema,
        usagi_daemon::usecase::resources::durable::MIGRATION_SCHEMA
    );
    assert_eq!(marker.generations, vec![legacy_generation.as_str()]);
    // A legacy reservation cannot prove a child, so it is adopted as a
    // non-spawnable safe failure rather than as live runtime.
    assert_eq!(marker.unknown, 1);
    assert_eq!(hydrated.terminals.records.len(), 1);
    // The store is retired by rename, so its bytes stay inspectable while no
    // build reads them again — the migration is one way.
    assert!(!daemon.join("terminals.json").exists());
    assert!(daemon.join("terminals.json.migrated").exists());
    assert!(daemon.join("runtime-migration.json").exists());
    assert!(state.hydrate().unwrap().migration.is_none());
}

#[test]
fn a_record_that_leaves_the_owners_truth_is_fenced_and_still_counted() {
    let dir = tempfile::tempdir().unwrap();
    let generation = DaemonGeneration::new();
    let mut store = ShardedTerminalStore::new(sharded_state(dir.path(), generation));
    let truth = terminal_truth(generation);
    store.save(truth).unwrap();

    // A reserved record still owns a PTY reservation a cold transition would
    // destroy, so the lifecycle census counts it.
    let census = DurableResourceCensus {
        data_dir: dir.path().to_path_buf(),
    };
    assert_eq!(census.live().unwrap().terminals, 1);

    // Dropping it from the owner's truth cannot silently forget a live record:
    // it becomes unprovable and keeps its capacity instead.
    store.save(TerminalStoreSnapshot::default()).unwrap();
    let document: serde_json::Value =
        serde_json::from_slice(&shard_bytes(dir.path(), generation)).unwrap();
    assert_eq!(document["resources"][0]["state"], "ownership_unknown");
    assert_eq!(census.live().unwrap().terminals, 0);
}

#[test]
fn a_collection_pass_removes_the_shard_of_a_generation_nothing_retains() {
    let dir = tempfile::tempdir().unwrap();
    let old = DaemonGeneration::new();
    let record = reserved_terminal_record(old);
    let mut exited = record.clone();
    exited.state = usagi_daemon::usecase::terminal::TerminalRuntimeState::Exited;
    let mut store = ShardedTerminalStore::new(sharded_state(dir.path(), old));
    store
        .save(TerminalStoreSnapshot {
            records: vec![exited],
            ..TerminalStoreSnapshot::default()
        })
        .unwrap();
    assert!(shard_path(dir.path(), old).exists());

    let active = sharded_state(dir.path(), DaemonGeneration::new());
    let limits = shipping_retention_limits();
    let retained: BTreeSet<String> =
        std::iter::once(record.terminal.terminal_id.as_str()).collect();

    // While the active generation still answers for the record, its history
    // stays; once it does not, the whole document goes.
    assert_eq!(active.collect(&retained, &limits).unwrap().1, 0);
    assert!(shard_path(dir.path(), old).exists());
    assert_eq!(active.collect(&BTreeSet::new(), &limits).unwrap().1, 1);
    assert!(!shard_path(dir.path(), old).exists());
}

#[test]
fn a_failed_shard_write_is_a_refused_save_that_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let generation = DaemonGeneration::new();
    let mut store = ShardedTerminalStore::new(sharded_state(dir.path(), generation));
    store.save(terminal_truth(generation)).unwrap();
    let shards = dir.path().join("daemon").join("shards");
    let document = shard_path(dir.path(), generation);
    let preserved = std::fs::read(&document).unwrap();
    // An unwritable shard directory fails the swap after the document was read.
    let mut mode = std::fs::metadata(&shards).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o500);
    std::fs::set_permissions(&shards, mode).unwrap();

    let refused = store.save(terminal_truth(generation)).is_err();

    let mut mode = std::fs::metadata(&shards).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o700);
    std::fs::set_permissions(&shards, mode).unwrap();
    assert!(refused || std::fs::read(&document).unwrap() == preserved);
    assert_eq!(std::fs::read(&document).unwrap(), preserved);
    let leftovers: Vec<_> = std::fs::read_dir(&shards)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().contains(".tmp."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

// ---------------------------------------------------------------- fence
//
// The generation fence the shipping accept loop now serves every connection
// through (#559). These tests drive the real presentation connection loop
// with the real `GenerationFence`, so what they fix is the *wiring*: the
// pure decisions are covered in `usagi_daemon::usecase::authority`.

/// A terminal owner that records what actually reached it. The fence's whole
/// job is to decide what does, so "the owner never saw it" is the only
/// statement of effect zero worth making.
#[derive(Default)]
struct FenceWitness {
    seen: Vec<TerminalAction>,
}

impl usagi_daemon::usecase::terminal_owner::TerminalOwner for FenceWitness {
    fn handle(
        &mut self,
        _context: usagi_daemon::usecase::terminal_owner::TerminalRequestContext,
        request: TerminalRequest,
    ) -> Result<
        usagi_daemon::usecase::terminal_owner::TerminalResponse,
        usagi_core::infrastructure::ipc::ProtocolError,
    > {
        let action = match request {
            TerminalRequest::Launch { .. } => TerminalAction::Launch,
            TerminalRequest::Inventory { .. } => TerminalAction::Inventory,
            TerminalRequest::Attach { .. } => TerminalAction::Attach,
            TerminalRequest::Resume { .. } => TerminalAction::Resume,
            TerminalRequest::Resync { .. } => TerminalAction::Resync,
            TerminalRequest::Input { .. } => TerminalAction::Input,
            TerminalRequest::InputOutcome { .. } => TerminalAction::InputOutcome,
            TerminalRequest::Resize { .. } => TerminalAction::Resize,
            TerminalRequest::Detach { .. } => TerminalAction::Detach,
            TerminalRequest::CompletedInventory { .. } => TerminalAction::CompletedInventory,
            TerminalRequest::Observe { .. } => TerminalAction::Observe,
            TerminalRequest::Dismiss { .. } => TerminalAction::Dismiss,
        };
        self.seen.push(action);
        Ok(usagi_daemon::usecase::terminal_owner::TerminalResponse::Detached)
    }

    fn disconnect(&mut self, _connection: ConnectionId) {}
}

/// The client hello a routing-capable peer sends.
fn fence_client_hello(capabilities: Vec<String>) -> usagi_core::infrastructure::ipc::ClientHello {
    use usagi_core::infrastructure::ipc::{
        ClientHello, ProtocolRange, TERMINAL_CHECKPOINT_REVISION, TERMINAL_WIRE_GENERATION,
    };
    ClientHello {
        client_id: usagi_core::infrastructure::ipc::ClientId(ClientId::new().as_str()),
        connection_nonce: "fence".to_owned(),
        expected_daemon_generation: None,
        supported_protocols: vec![ProtocolRange {
            generation: TERMINAL_WIRE_GENERATION,
            min_revision: 0,
            max_revision: TERMINAL_CHECKPOINT_REVISION,
        }],
        capabilities,
        required_capabilities: Vec::new(),
        build: current_build(),
        workspace: Some(ClientWorkspace::Unbound),
    }
}

/// Serve `requests` to one connection through `fence` and report each
/// response's outcome alongside what reached the terminal owner.
///
/// The bytes are a real hello frame plus real request envelopes, so the fence
/// is exercised exactly where production puts it: inside
/// `handle_connection_with_terminal_and`, ahead of both the terminal path and
/// the dispatch closure.
fn serve_through_fence(
    fence: &GenerationFence,
    hello: &usagi_core::infrastructure::ipc::ClientHello,
    requests: &[serde_json::Value],
) -> (
    Vec<usagi_core::infrastructure::ipc::ResponseOutcome>,
    Vec<TerminalAction>,
) {
    use usagi_core::infrastructure::ipc::{
        Bootstrap, DEFAULT_MAX_FRAME_BYTES, Envelope, EnvelopeKind, RequestId as WireRequestId,
        read_json_frame, write_json_frame,
    };
    let generation = ipc_generation();
    let protocol = usagi_daemon::presentation::ipc::server_protocol(
        generation.clone(),
        generation.0,
        current_build(),
        DaemonRecord::new(std::process::id()),
        String::new(),
    );
    // The version and generation every envelope must target are the ones the
    // handshake will settle on, so they are read from the same negotiation the
    // connection loop performs rather than assumed. An envelope that named a
    // different pair would be answered by the generation-mismatch branch,
    // which never reaches the fence.
    let negotiated = usagi_core::infrastructure::ipc::negotiate(hello, &protocol)
        .expect("the fence fixture's client must be admissible");
    let mut inbound = Vec::new();
    write_json_frame(
        &mut inbound,
        &Bootstrap::ClientHello(hello.clone()),
        DEFAULT_MAX_FRAME_BYTES,
    )
    .unwrap();
    for body in requests {
        write_json_frame(
            &mut inbound,
            &Envelope {
                protocol: negotiated.protocol,
                daemon_generation: negotiated.daemon_generation.clone(),
                kind: EnvelopeKind::Request {
                    request_id: WireRequestId(RequestId::new().as_str().clone()),
                    timeout_ms: None,
                    body: body.clone(),
                },
            },
            DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap();
    }

    let mut reader = std::io::Cursor::new(inbound);
    let mut outbound = Vec::new();
    let mut owner = FenceWitness::default();
    usagi_daemon::presentation::ipc::handle_connection_with_terminal_and(
        &mut reader,
        &mut outbound,
        &protocol,
        fence,
        &mut owner,
        &mut |request_id, _body, hello, _connection, _client| Envelope {
            protocol: hello.protocol,
            daemon_generation: hello.daemon_generation.clone(),
            kind: EnvelopeKind::Response {
                request_id,
                outcome: usagi_core::infrastructure::ipc::ResponseOutcome::Ok,
                body: serde_json::json!({"dispatched": true}),
            },
        },
    )
    .unwrap();

    let mut replies = std::io::Cursor::new(outbound);
    // The server hello is the first frame out; the responses follow it.
    assert!(matches!(
        read_json_frame::<Bootstrap>(&mut replies, DEFAULT_MAX_FRAME_BYTES).unwrap(),
        Some(Bootstrap::ServerHello(_))
    ));
    let mut outcomes = Vec::new();
    while let Some(envelope) =
        read_json_frame::<Envelope>(&mut replies, DEFAULT_MAX_FRAME_BYTES).unwrap()
    {
        let EnvelopeKind::Response { outcome, .. } = envelope.kind else {
            panic!("daemon replied with something other than a response");
        };
        outcomes.push(outcome);
    }
    (outcomes, owner.seen)
}

fn serve_supervisor_request(
    runtime: &SharedSupervisorRuntime,
    generation: &usagi_core::infrastructure::ipc::DaemonGeneration,
    hello: &usagi_core::infrastructure::ipc::ClientHello,
    authenticated: Option<(
        &usagi_core::domain::agent::CallerRef,
        usagi_core::domain::id::OperationId,
        &AgentRuntimeRef,
    )>,
    workspace: WorkspaceId,
    body: serde_json::Value,
) -> (
    usagi_core::infrastructure::ipc::ResponseOutcome,
    serde_json::Value,
) {
    use usagi_core::infrastructure::ipc::{
        Bootstrap, DEFAULT_MAX_FRAME_BYTES, Envelope, EnvelopeKind, ErrorCode, ProtocolError,
        RequestId as WireRequestId, read_json_frame, write_json_frame,
    };
    let protocol = usagi_daemon::presentation::ipc::server_protocol(
        generation.clone(),
        generation.0.clone(),
        current_build(),
        DaemonRecord::new(std::process::id()),
        String::new(),
    );
    let negotiated = usagi_core::infrastructure::ipc::negotiate(hello, &protocol).unwrap();
    let mut inbound = Vec::new();
    write_json_frame(
        &mut inbound,
        &Bootstrap::ClientHello(hello.clone()),
        DEFAULT_MAX_FRAME_BYTES,
    )
    .unwrap();
    write_json_frame(
        &mut inbound,
        &Envelope {
            protocol: negotiated.protocol,
            daemon_generation: negotiated.daemon_generation,
            kind: EnvelopeKind::Request {
                request_id: WireRequestId(RequestId::new().as_str()),
                timeout_ms: None,
                body,
            },
        },
        DEFAULT_MAX_FRAME_BYTES,
    )
    .unwrap();

    let mut reader = std::io::Cursor::new(inbound);
    let mut outbound = Vec::new();
    let mut owner = FenceWitness::default();
    usagi_daemon::presentation::ipc::handle_connection_with_terminal_and(
        &mut reader,
        &mut outbound,
        &protocol,
        &usagi_daemon::presentation::ipc::UnfencedConnection,
        &mut owner,
        &mut |request_id, body, server, _connection, client| {
            let caller = authenticated.map_or_else(
                || {
                    Err(ProtocolError::new(
                        ErrorCode::OwnershipUnknown,
                        "supervisor caller provenance is unknown",
                    ))
                },
                |(caller, dispatch_run_id, agent_runtime)| {
                    Ok(AuthenticatedSupervisorCaller {
                        descriptor: supervisor_caller_descriptor(&client, caller),
                        workspace,
                        dispatch_run_id,
                        runtime: agent_runtime.clone(),
                    })
                },
            );
            dispatch_supervisor_tool(runtime, caller, request_id, &body, server)
        },
    )
    .unwrap();
    let mut replies = std::io::Cursor::new(outbound);
    assert!(matches!(
        read_json_frame::<Bootstrap>(&mut replies, DEFAULT_MAX_FRAME_BYTES).unwrap(),
        Some(Bootstrap::ServerHello(_))
    ));
    let reply = read_json_frame::<Envelope>(&mut replies, DEFAULT_MAX_FRAME_BYTES)
        .unwrap()
        .unwrap();
    let EnvelopeKind::Response { outcome, body, .. } = reply.kind else {
        panic!("supervisor dispatcher returned a non-response envelope");
    };
    (outcome, body)
}

fn supervisor_request(
    action: SupervisorToolAction,
    operation_id: &str,
    payload: serde_json::Value,
) -> serde_json::Value {
    serde_json::to_value(DaemonRequest::SupervisorTool {
        action,
        operation_id: operation_id.to_owned(),
        payload,
        caller_context: None,
    })
    .unwrap()
}

#[test]
#[allow(clippy::too_many_lines)] // One matrix keeps every authority transition on one durable run.
fn supervisor_authority_survives_reconnect_and_rollover_but_not_forgery_or_restart() {
    use usagi_core::domain::{
        agent::CallerRef,
        id::AgentId,
        supervisor::{
            EscalationDecision, SupervisorEvent, SupervisorEventKind, SupervisorEventSource,
            TaskState,
        },
    };
    use usagi_core::infrastructure::{
        ipc::{ErrorCode, ResponseOutcome},
        store::supervisor::SupervisorStore,
    };

    let temp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Mutex::new(SupervisorRuntime::new(temp.path())));
    let caller_session = SessionId::new();
    let caller = CallerRef {
        session_id: Some(caller_session),
        agent_id: AgentId::new(),
    };
    let workspace = WorkspaceId::new();
    let caller_dispatch_run = usagi_core::domain::id::OperationId::new();
    let caller_runtime = AgentRuntimeRef::new(
        AgentRuntimeId::new(),
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(caller_session),
            worktree_id: WorktreeId::new(),
        },
        Some(caller_session),
    )
    .unwrap();
    persist_supervisor_dispatch(
        &DispatchStore::new(temp.path()),
        workspace,
        caller_dispatch_run,
        caller.agent_id,
        &caller_runtime,
        "supervisor-tool-caller".into(),
    );
    let hello = fence_client_hello(Vec::new());
    let first_generation = ipc_generation();
    let start = supervisor_request(
        SupervisorToolAction::Start,
        "lost-response-operation",
        serde_json::json!({"root_task":"root"}),
    );

    // The first response is deliberately discarded. A new production
    // connection with the same handshake incarnation converges on its run.
    let _ = serve_supervisor_request(
        &runtime,
        &first_generation,
        &hello,
        Some((&caller, caller_dispatch_run, &caller_runtime)),
        workspace,
        start.clone(),
    );
    let (retry_outcome, retry_body) = serve_supervisor_request(
        &runtime,
        &first_generation,
        &hello,
        Some((&caller, caller_dispatch_run, &caller_runtime)),
        workspace,
        start,
    );
    assert_eq!(retry_outcome, ResponseOutcome::Ok);
    let run_id = retry_body["supervisor_run_id"].as_str().unwrap();

    let (duplicate_start, _) = serve_supervisor_request(
        &runtime,
        &first_generation,
        &hello,
        Some((&caller, caller_dispatch_run, &caller_runtime)),
        workspace,
        supervisor_request(
            SupervisorToolAction::Start,
            "second-live-operation",
            serde_json::json!({"root_task":"another root"}),
        ),
    );
    assert!(
        matches!(duplicate_start, ResponseOutcome::Error(error) if error.code == ErrorCode::RevisionConflict)
    );
    assert_eq!(
        runtime
            .lock()
            .unwrap()
            .list_workspace(workspace)
            .unwrap()
            .len(),
        1
    );

    // A generation rollover keeps the daemon-issued credential registry and
    // the process client incarnation, so every control surface remains owned.
    let rollover = ipc_generation();
    for (action, payload) in [
        (
            SupervisorToolAction::Get,
            serde_json::json!({"supervisor_run_id":run_id}),
        ),
        (SupervisorToolAction::List, serde_json::json!({})),
        (
            SupervisorToolAction::Events,
            serde_json::json!({"supervisor_run_id":run_id}),
        ),
    ] {
        let (outcome, _) = serve_supervisor_request(
            &runtime,
            &rollover,
            &hello,
            Some((&caller, caller_dispatch_run, &caller_runtime)),
            workspace,
            supervisor_request(action, "observe", payload),
        );
        assert_eq!(outcome, ResponseOutcome::Ok);
    }

    // Start binds the root to the exact authenticated Agent dispatch. Add an
    // explicit operator decision so the authorized resolve path stays in
    // this end-to-end authority matrix.
    let store = SupervisorStore::new(temp.path());
    let id = serde_json::from_value(retry_body["supervisor_run_id"].clone()).unwrap();
    let active = store.load(id).unwrap().unwrap();
    assert_eq!(
        active.tasks[&usagi_core::domain::supervisor::TaskId::new("root").unwrap()].state,
        TaskState::Dispatched
    );
    let escalation_event_id = usagi_core::domain::id::OperationId::new();
    let escalated = store
        .apply(
            id,
            active.state_revision,
            &SupervisorEvent {
                sequence: active.state_revision + 1,
                event_id: escalation_event_id,
                causation_id: None,
                correlation_id: None,
                observed_at: chrono::Utc::now(),
                payload_digest: "authority-test-escalation".into(),
                source: SupervisorEventSource::Admission,
                kind: SupervisorEventKind::Escalate {
                    task_id: None,
                    reason: "operator decision required".into(),
                    safe_evidence: "safe evidence".into(),
                    choices: vec!["resume".into()],
                },
            },
        )
        .unwrap();
    let actual_escalation = escalated.escalation.unwrap().escalation_id;
    let (resolved, _) = serve_supervisor_request(
        &runtime,
        &rollover,
        &hello,
        Some((&caller, caller_dispatch_run, &caller_runtime)),
        workspace,
        supervisor_request(
            SupervisorToolAction::ResolveEscalation,
            "resolve",
            serde_json::json!({
                "supervisor_run_id":run_id,
                "escalation_id":actual_escalation,
                "decision":EscalationDecision::Resume,
            }),
        ),
    );
    assert_eq!(resolved, ResponseOutcome::Ok);

    // A different incarnation or missing/expired capability cannot observe
    // or mutate the run. In particular a daemon restart loses the in-memory
    // credential registry even though the durable aggregate is reloaded.
    let foreign_hello = fence_client_hello(Vec::new());
    let foreign_scope = CallerRef {
        session_id: Some(SessionId::new()),
        agent_id: AgentId::new(),
    };
    for (candidate_hello, authenticated) in [
        (&foreign_hello, Some(&caller)),
        (&hello, Some(&foreign_scope)),
    ] {
        let before = store.load(id).unwrap().unwrap();
        for (action, operation, payload) in [
            (
                SupervisorToolAction::Start,
                "lost-response-operation",
                serde_json::json!({"root_task":"root"}),
            ),
            (
                SupervisorToolAction::Get,
                "foreign-get",
                serde_json::json!({"supervisor_run_id":run_id}),
            ),
            (
                SupervisorToolAction::Events,
                "foreign-events",
                serde_json::json!({"supervisor_run_id":run_id}),
            ),
            (
                SupervisorToolAction::Cancel,
                "foreign-cancel",
                serde_json::json!({"supervisor_run_id":run_id,"reason":"foreign"}),
            ),
            (
                SupervisorToolAction::ResolveEscalation,
                "foreign-resolve",
                serde_json::json!({
                    "supervisor_run_id":run_id,
                    "escalation_id":actual_escalation,
                    "decision":EscalationDecision::Cancel,
                }),
            ),
        ] {
            let (outcome, _) = serve_supervisor_request(
                &runtime,
                &rollover,
                candidate_hello,
                authenticated.map(|caller| (caller, caller_dispatch_run, &caller_runtime)),
                workspace,
                supervisor_request(action, operation, payload),
            );
            assert!(matches!(outcome, ResponseOutcome::Error(_)));
            assert_eq!(store.load(id).unwrap().unwrap(), before);
        }
        let (listed, body) = serve_supervisor_request(
            &runtime,
            &rollover,
            candidate_hello,
            authenticated.map(|caller| (caller, caller_dispatch_run, &caller_runtime)),
            workspace,
            supervisor_request(
                SupervisorToolAction::List,
                "foreign-list",
                serde_json::json!({}),
            ),
        );
        assert_eq!(listed, ResponseOutcome::Ok);
        assert_eq!(body["runs"].as_array().unwrap().len(), 0);
        assert_eq!(store.load(id).unwrap().unwrap(), before);
    }
    let before_unauthenticated = store.load(id).unwrap().unwrap();
    let (unauthenticated, _) = serve_supervisor_request(
        &runtime,
        &rollover,
        &hello,
        None,
        workspace,
        supervisor_request(
            SupervisorToolAction::Cancel,
            "missing-capability",
            serde_json::json!({"supervisor_run_id":run_id,"reason":"foreign"}),
        ),
    );
    assert!(
        matches!(unauthenticated, ResponseOutcome::Error(error) if error.code == ErrorCode::OwnershipUnknown)
    );
    assert_eq!(store.load(id).unwrap().unwrap(), before_unauthenticated);
    let restarted = Arc::new(Mutex::new(SupervisorRuntime::new(temp.path())));
    let (after_restart, _) = serve_supervisor_request(
        &restarted,
        &ipc_generation(),
        &hello,
        None,
        workspace,
        supervisor_request(
            SupervisorToolAction::Get,
            "restart",
            serde_json::json!({"supervisor_run_id":run_id}),
        ),
    );
    assert!(
        matches!(after_restart, ResponseOutcome::Error(error) if error.code == ErrorCode::OwnershipUnknown)
    );

    let (cancelled, _) = serve_supervisor_request(
        &runtime,
        &rollover,
        &hello,
        Some((&caller, caller_dispatch_run, &caller_runtime)),
        workspace,
        supervisor_request(
            SupervisorToolAction::Cancel,
            "cancel",
            serde_json::json!({"supervisor_run_id":run_id,"reason":"owner"}),
        ),
    );
    assert_eq!(cancelled, ResponseOutcome::Ok);
}

mod workflow_composition {
    use super::*;
    use usagi_core::domain::workflow::{Delivery, Recipient, WorkflowCommand, WorkflowSnapshot};
    use usagi_daemon::usecase::codex::{CodexProvision, CodexProvisionFailure};

    struct Ready(bool);
    impl AgentReadinessProbe for Ready {
        fn observe(&self, _: &str) -> AgentReadiness {
            if self.0 {
                AgentReadiness::Ready
            } else {
                AgentReadiness::Unavailable
            }
        }
    }
    struct Available;
    impl usagi_core::infrastructure::runtime_model::ExecutableLocator for Available {
        fn is_available(&self, _: &str) -> bool {
            true
        }
    }
    struct Provision(PathBuf);
    impl ClaudeProvisioner for Provision {
        fn provision(
            &mut self,
            _: &ProvisionContext,
        ) -> Result<ClaudeProvision, ClaudeProvisionFailure> {
            Ok(ClaudeProvision {
                working_directory: self.0.clone(),
                environment_allowlist: BTreeSet::new(),
                spawn: SpawnProvision::new(Vec::new(), Vec::new()),
            })
        }
    }
    impl CodexProvisioner for Provision {
        fn provision(
            &mut self,
            _: &ProvisionContext,
        ) -> Result<CodexProvision, CodexProvisionFailure> {
            Ok(CodexProvision {
                working_directory: self.0.clone(),
                environment_allowlist: BTreeSet::new(),
                spawn: SpawnProvision::new(Vec::new(), Vec::new()),
            })
        }
    }
    #[derive(Default)]
    struct Writes {
        selected: Option<TerminalRef>,
        entries: Vec<(TerminalRef, Vec<u8>)>,
    }
    struct Pty(Arc<Mutex<Writes>>);
    impl PtySpawner for Pty {
        fn spawn(
            &mut self,
            _: &DurableLaunchSnapshot,
            _: &SpawnProvision,
            _: &TerminalRef,
        ) -> Result<ProcessIdentity, SpawnFailure> {
            Ok(ProcessIdentity {
                pid: 4321,
                start_identity: "workflow-test".into(),
                process_group: 4321,
            })
        }
        fn terminate_reap(&mut self, _: &TerminalRef) -> Result<(), TerminateReapError> {
            Ok(())
        }
    }
    impl PtyWriter for Pty {
        fn select_terminal(&mut self, terminal: &TerminalRef) {
            self.0.lock().unwrap().selected = Some(terminal.clone());
        }
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
            let mut writes = self.0.lock().unwrap();
            let terminal = writes.selected.clone().unwrap();
            writes.entries.push((terminal, bytes.to_vec()));
            Ok(())
        }
    }
    /// Records what the lane announced, so a test can assert both the notice
    /// and that entering the same phase twice announces once.
    #[derive(Default)]
    struct RecordingNotifier(Mutex<Vec<(String, String)>>);

    impl workflow::AttentionNotifier for RecordingNotifier {
        fn notify(&self, title: &str, body: &str) {
            self.0.lock().unwrap().push((title.into(), body.into()));
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        bound: ConnectionWorkspace,
        agent: SharedAgentRuntime,
        inventory: SharedPrInventory,
        workspace: WorkspaceId,
        session: SessionId,
        writes: Arc<Mutex<Writes>>,
    }
    impl Fixture {
        fn new() -> Self {
            Self::with_readiness(true)
        }
        fn with_readiness(ready: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("repository");
            let sessions = Arc::new(Mutex::new(
                SessionRuntime::open(
                    root.clone(),
                    &directory.path().join("sessions"),
                    DaemonGeneration::new(),
                    AlwaysSuccessfulGit,
                    PermissiveSessionWorktreeIo,
                )
                .unwrap(),
            ));
            perform_create(
                &sessions,
                &AlwaysSuccessfulGit,
                &usagi_core::domain::id::OperationId::new().to_string(),
                &serde_json::json!({"name":"workflow"}),
            )
            .unwrap();
            let workspace = sessions.lock().unwrap().workspace_id().unwrap();
            let session = sessions.lock().unwrap().session_id("workflow").unwrap();
            let bound = bound_to(
                &directory.path().join("tenants"),
                &root,
                sessions,
                workspace,
            );
            let mut registry = AdapterRegistry::new();
            let adapter = ClaudeAdapter::new(Provision(root.clone()));
            registry
                .register(adapter.profile().clone(), Box::new(adapter))
                .unwrap();
            let adapter = CodexAdapter::new(Provision(root));
            registry
                .register(adapter.profile().clone(), Box::new(adapter))
                .unwrap();
            let writes = Arc::new(Mutex::new(Writes::default()));
            let owner = AgentRuntime::with_dispatch_and_locator(
                DaemonGeneration::new(),
                registry,
                SupervisorAgentStore,
                SupervisorAgentJournal,
                Pty(Arc::clone(&writes)),
                AgentProfileId::new("codex").unwrap(),
                Geometry { cols: 80, rows: 24 },
                DispatchStore::new(directory.path().join("dispatch")),
                Available,
            );
            let agent = Arc::new(SharedAgentState {
                owner: Mutex::new(owner),
                readiness: Arc::new(Ready(ready)),
            });
            let inventory = Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
                PrInventoryStore::new(directory.path().join("prs")),
                GenerationRole::Active,
            ))));
            Self {
                _directory: directory,
                bound,
                agent,
                inventory,
                workspace,
                session,
                writes,
            }
        }
        fn call(
            &self,
            request: DaemonRequest,
        ) -> Result<WorkflowSnapshot, usagi_core::infrastructure::ipc::ProtocolError> {
            let raw = serde_json::to_value(&request).unwrap();
            self.call_raw(request, &raw)
        }
        fn call_raw(
            &self,
            request: DaemonRequest,
            raw: &serde_json::Value,
        ) -> Result<WorkflowSnapshot, usagi_core::infrastructure::ipc::ProtocolError> {
            let cache = fresh_verification_cache();
            let response = workflow::dispatch(
                &workflow::WorkflowDispatchContext {
                    agent: &self.agent,
                    inventory: &self.inventory,
                    verification: workflow::Verification {
                        cache: &cache,
                        clock: &StoppedClock(0),
                    },
                    bound: &self.bound,
                },
                usagi_core::infrastructure::ipc::RequestId("workflow-test".into()),
                request,
                raw,
                &session_test_hello(),
            );
            match response.kind {
                EnvelopeKind::Response {
                    outcome: ResponseOutcome::Error(error),
                    ..
                } => Err(error),
                EnvelopeKind::Response {
                    outcome: ResponseOutcome::Ok,
                    body,
                    ..
                } => Ok(serde_json::from_value(body).unwrap()),
                _ => panic!("unexpected workflow envelope"),
            }
        }
        fn control(
            &self,
            operation: usagi_core::domain::id::OperationId,
            command: WorkflowCommand,
        ) -> Result<WorkflowSnapshot, usagi_core::infrastructure::ipc::ProtocolError> {
            self.call(DaemonRequest::WorkflowControl {
                workspace: self.workspace,
                session: self.session,
                operation_id: operation,
                command,
            })
        }
    }

    fn resume_workflow_participant(
        fixture: &Fixture,
        operation: usagi_core::domain::id::OperationId,
        provider: usagi_core::domain::agent::ProviderKind,
    ) -> usagi_core::domain::id::OperationId {
        use usagi_core::domain::agent::ProviderSessionId;
        let mut owner = fixture.agent.lock().unwrap();
        let runtime = owner.runtime_for_operation(operation).unwrap();
        owner
            .capture_structured_provider_session(
                &runtime,
                provider,
                ProviderSessionId::new(operation.to_string()).unwrap(),
            )
            .unwrap();
        owner.exit(&runtime.terminal, 0).unwrap();
        drop(owner);
        resume_stopped_workflow_participant(fixture, operation)
    }

    fn resume_stopped_workflow_participant(
        fixture: &Fixture,
        operation: usagi_core::domain::id::OperationId,
    ) -> usagi_core::domain::id::OperationId {
        let mut owner = fixture.agent.lock().unwrap();
        let runtime = owner.runtime_for_operation(operation).unwrap();
        let target = owner
            .inventory(fixture.workspace)
            .resumable
            .into_iter()
            .find_map(|item| {
                item.target
                    .filter(|target| target.runtime_id == runtime.agent_runtime_id)
            })
            .unwrap();
        let resumed = usagi_core::domain::id::OperationId::new();
        owner
            .resume_exact(
                &resumed.to_string(),
                &target,
                &fixture.bound.scope_resolver(),
            )
            .unwrap();
        resumed
    }

    #[test]
    fn workflow_launch_uses_selected_implementer_and_remembers_the_three_agents() {
        use usagi_core::domain::{
            settings::DefaultModel,
            workflow::{WorkflowAgents, WorkflowCommand},
        };
        let fixture = Fixture::new();
        let agents = WorkflowAgents {
            planner: DefaultModel::Agy,
            implementer: DefaultModel::Claude,
            reviewer: DefaultModel::OpenAi,
        };
        let result = fixture
            .control(
                usagi_core::domain::id::OperationId::new(),
                WorkflowCommand::Start {
                    goal: "Selected providers".into(),
                    agents,
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap();
        let run = result.run.unwrap();
        assert_eq!(run.agents, agents);
        let owner = fixture.agent.lock().unwrap();
        let store = owner.dispatch_store();
        let implementer = store
            .agents_in_workspace(fixture.workspace)
            .unwrap()
            .into_iter()
            .find(|entry| entry.agent_id == run.implementer)
            .unwrap();
        assert_eq!(implementer.runtime.as_str(), "claude");
        assert_eq!(
            store.workflow_defaults(fixture.workspace).unwrap().agents,
            agents
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One exact-resume sequence exercises both participants before the first observation.
    fn workflow_unobserved_exact_resumes_keep_requests_and_self_bound_reviewer_verdicts() {
        use usagi_core::domain::agent::{CallerRef, ModelSelector, ProviderKind};
        use usagi_core::domain::agent_message::{MessageKind, ReviewTarget, SendMessage};
        use usagi_core::domain::id::OperationId;
        use usagi_core::infrastructure::ipc::{DispatchAgentIntent, DispatchIntent};
        let fixture = Fixture::new();
        let operation = OperationId::new();
        let run = fixture
            .control(
                operation,
                WorkflowCommand::Start {
                    goal: "resume safely".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap()
            .run
            .unwrap();
        let caller = CallerRef {
            agent_id: run.implementer,
            session_id: Some(fixture.session),
        };
        let review_operation = OperationId::new();
        let store = fixture.agent.lock().unwrap().dispatch_store().clone();
        fixture
            .agent
            .lock()
            .unwrap()
            .dispatch(
                &review_operation.to_string(),
                &DispatchIntent {
                    workspace: fixture.workspace,
                    session_name: "workflow".into(),
                    caller: caller.clone(),
                    agent: DispatchAgentIntent::New {
                        runtime: AgentProfileId::new("claude").unwrap(),
                        model: ModelSelector::new("default").unwrap(),
                    },
                    prompt: "review only".into(),
                },
                fixture.session,
                &fixture.bound.scope_resolver(),
            )
            .unwrap();
        let reviewer = store
            .binding(review_operation)
            .unwrap()
            .unwrap()
            .worker
            .agent_id;
        // Neither stop nor resume is observed by the Workflow projection.
        let implementation = resume_workflow_participant(&fixture, operation, ProviderKind::Codex);
        let review_run =
            resume_workflow_participant(&fixture, review_operation, ProviderKind::Claude);
        assert_eq!(
            store.binding(review_run).unwrap().unwrap().caller.agent_id,
            reviewer
        );
        let request = OperationId::new();
        let target = ReviewTarget {
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
        };
        store
            .send_message(
                fixture.workspace,
                &caller,
                implementation,
                SendMessage {
                    message_id: request,
                    to_agent_id: reviewer,
                    kind: MessageKind::ReviewRequest,
                    body: "Review resumed implementation".into(),
                    in_reply_to: None,
                    review: Some(target.clone()),
                },
            )
            .unwrap();
        store
            .send_message(
                fixture.workspace,
                &CallerRef {
                    agent_id: reviewer,
                    session_id: Some(fixture.session),
                },
                review_run,
                SendMessage {
                    message_id: OperationId::new(),
                    to_agent_id: run.implementer,
                    kind: MessageKind::ChangesRequested,
                    body: "Fix resumed review finding".into(),
                    in_reply_to: Some(request),
                    review: Some(target),
                },
            )
            .unwrap();
        // A fast exit after publishing must not remove authenticated evidence.
        for finished in [implementation, review_run] {
            let mut owner = fixture.agent.lock().unwrap();
            let terminal = owner.runtime_for_operation(finished).unwrap().terminal;
            owner.exit(&terminal, 0).unwrap();
        }
        let snapshot = fixture
            .call(DaemonRequest::WorkflowSnapshot {
                workspace: fixture.workspace,
                session: fixture.session,
            })
            .unwrap()
            .run
            .unwrap();
        assert_eq!(snapshot.review.unwrap().request, request);
        assert_eq!(snapshot.revisions, 1);
        assert_eq!(snapshot.history.len(), 2);
        assert_eq!(snapshot.phase, usagi_core::domain::workflow::Phase::Waiting);
        // Recovery re-reads the durable cursor without replaying the already
        // accepted request and finding into another revision or history row.
        resume_stopped_workflow_participant(&fixture, implementation);
        let replay = fixture
            .call(DaemonRequest::WorkflowSnapshot {
                workspace: fixture.workspace,
                session: fixture.session,
            })
            .unwrap()
            .run
            .unwrap();
        assert_eq!(replay.revisions, 1);
        assert_eq!(replay.history.len(), 2);
        assert_eq!(replay.review.unwrap().request, request);
        assert_eq!(replay.phase, usagi_core::domain::workflow::Phase::Revising);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One observed reviewer interruption verifies exact recovery and queued delivery.
    fn workflow_observed_reviewer_stop_resumes_and_retries_queued_instructions() {
        use usagi_core::domain::agent::{
            CallerRef, ModelSelector, ProviderKind, ProviderSessionId,
        };
        use usagi_core::domain::id::OperationId;
        use usagi_core::domain::workflow::Phase;
        use usagi_core::infrastructure::ipc::{DispatchAgentIntent, DispatchIntent};
        let fixture = Fixture::new();
        let operation = OperationId::new();
        let run = fixture
            .control(
                operation,
                WorkflowCommand::Start {
                    goal: "review recovery".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap()
            .run
            .unwrap();
        let review_operation = OperationId::new();
        let store = fixture.agent.lock().unwrap().dispatch_store().clone();
        fixture
            .agent
            .lock()
            .unwrap()
            .dispatch(
                &review_operation.to_string(),
                &DispatchIntent {
                    workspace: fixture.workspace,
                    session_name: "workflow".into(),
                    caller: CallerRef {
                        agent_id: run.implementer,
                        session_id: Some(fixture.session),
                    },
                    agent: DispatchAgentIntent::New {
                        runtime: AgentProfileId::new("claude").unwrap(),
                        model: ModelSelector::new("default").unwrap(),
                    },
                    prompt: "review only".into(),
                },
                fixture.session,
                &fixture.bound.scope_resolver(),
            )
            .unwrap();
        let reviewer = store
            .binding(review_operation)
            .unwrap()
            .unwrap()
            .worker
            .agent_id;
        store
            .update_workflow(fixture.workspace, fixture.session, |record| {
                let run = record.as_mut().unwrap().run.as_mut().unwrap();
                run.reviewer = Some(reviewer);
                run.phase = Phase::Reviewing;
                Ok(())
            })
            .unwrap();
        let request = DaemonRequest::WorkflowSnapshot {
            workspace: fixture.workspace,
            session: fixture.session,
        };
        assert_eq!(
            fixture.call(request.clone()).unwrap().run.unwrap().phase,
            Phase::Reviewing
        );
        {
            let mut owner = fixture.agent.lock().unwrap();
            let runtime = owner.runtime_for_operation(review_operation).unwrap();
            owner
                .capture_structured_provider_session(
                    &runtime,
                    ProviderKind::Claude,
                    ProviderSessionId::new("observed-reviewer").unwrap(),
                )
                .unwrap();
            owner.exit(&runtime.terminal, 0).unwrap();
        }
        assert_eq!(
            fixture.call(request.clone()).unwrap().run.unwrap().phase,
            Phase::Waiting
        );
        let queued = fixture
            .control(
                OperationId::new(),
                WorkflowCommand::Instruct {
                    recipient: Recipient::Reviewer,
                    body: "Check recovery edge cases".into(),
                },
            )
            .unwrap()
            .run
            .unwrap();
        assert_eq!(queued.instructions[0].delivery, Delivery::Queued);
        assert!(fixture.writes.lock().unwrap().entries.is_empty());
        // A plain launch may reuse the mailbox identity, but is not a resume
        // of this Workflow's assigned conversation and must receive nothing.
        let unrelated_operation = OperationId::new();
        let unrelated = fixture
            .agent
            .lock()
            .unwrap()
            .launch(
                &unrelated_operation.to_string(),
                &usagi_core::infrastructure::ipc::AgentLaunchIntent {
                    workspace: fixture.workspace,
                    session: Some(fixture.session),
                    profile: Some(AgentProfileId::new("claude").unwrap()),
                },
                &fixture.bound.scope_resolver(),
            )
            .unwrap();
        assert_eq!(
            store
                .binding(unrelated_operation)
                .unwrap()
                .unwrap()
                .worker
                .agent_id,
            reviewer
        );
        assert_eq!(
            fixture
                .call(request.clone())
                .unwrap()
                .run
                .unwrap()
                .instructions[0]
                .delivery,
            Delivery::Queued
        );
        assert!(fixture.writes.lock().unwrap().entries.is_empty());
        fixture
            .agent
            .lock()
            .unwrap()
            .exit(&unrelated.terminal, 0)
            .unwrap();
        let resumed = resume_stopped_workflow_participant(&fixture, review_operation);
        let restored = fixture.call(request.clone()).unwrap().run.unwrap();
        assert_eq!(restored.phase, Phase::Reviewing);
        assert_eq!(restored.waiting_reason, None);
        assert_eq!(restored.instructions[0].delivery, Delivery::Notified);
        let terminal = fixture
            .agent
            .lock()
            .unwrap()
            .runtime_for_operation(resumed)
            .unwrap()
            .terminal;
        assert_eq!(fixture.writes.lock().unwrap().entries[0].0, terminal);
        // Simulate exact resume completing between the synchronized journal
        // observation and the later runtime-state reconciliation.
        store
            .update_workflow(fixture.workspace, fixture.session, |record| {
                let record = record.as_mut().unwrap();
                record.suspended_phase = Some(Phase::Reviewing);
                let run = record.run.as_mut().unwrap();
                run.phase = Phase::Waiting;
                run.waiting_reason = Some("interrupted".into());
                Ok(())
            })
            .unwrap();
        workflow::reconcile_runtime(&fixture.agent, fixture.workspace, fixture.session).unwrap();
        let reconciled = store
            .workflow(fixture.workspace, fixture.session)
            .unwrap()
            .unwrap();
        assert_eq!(reconciled.suspended_phase, None);
        assert_eq!(reconciled.run.unwrap().waiting_reason, None);
        // An explicit revision-limit wait is not an interrupted-runtime wait.
        store
            .update_workflow(fixture.workspace, fixture.session, |record| {
                record.as_mut().unwrap().run.as_mut().unwrap().phase = Phase::Waiting;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            fixture.call(request).unwrap().run.unwrap().phase,
            Phase::Waiting
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One replay verifies original binding fallback, reviewer reuse, and non-verdict messages.
    fn workflow_legacy_journal_replay_accepts_original_handoff_bindings_without_cached_lineage() {
        use usagi_core::domain::agent::{CallerRef, ModelSelector};
        use usagi_core::domain::agent_message::{MessageKind, ReviewTarget, SendMessage};
        use usagi_core::domain::id::OperationId;
        use usagi_core::infrastructure::ipc::{DispatchAgentIntent, DispatchIntent};
        let fixture = Fixture::new();
        let operation = OperationId::new();
        let run = fixture
            .control(
                operation,
                WorkflowCommand::Start {
                    goal: "legacy journal replay".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap()
            .run
            .unwrap();
        let caller = CallerRef {
            agent_id: run.implementer,
            session_id: Some(fixture.session),
        };
        let review_operation = OperationId::new();
        fixture
            .agent
            .lock()
            .unwrap()
            .dispatch(
                &review_operation.to_string(),
                &DispatchIntent {
                    workspace: fixture.workspace,
                    session_name: "workflow".into(),
                    caller: caller.clone(),
                    agent: DispatchAgentIntent::New {
                        runtime: AgentProfileId::new("claude").unwrap(),
                        model: ModelSelector::new("default").unwrap(),
                    },
                    prompt: "review only".into(),
                },
                fixture.session,
                &fixture.bound.scope_resolver(),
            )
            .unwrap();
        let store = fixture.agent.lock().unwrap().dispatch_store().clone();
        let reviewer = store
            .binding(review_operation)
            .unwrap()
            .unwrap()
            .worker
            .agent_id;
        store
            .update_workflow(fixture.workspace, fixture.session, |record| {
                record.as_mut().unwrap().authorized_operations.clear();
                Ok(())
            })
            .unwrap();
        let first = OperationId::new();
        let next = OperationId::new();
        let target = ReviewTarget {
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
        };
        for (from, from_run, message) in [
            (
                caller.clone(),
                operation,
                SendMessage {
                    message_id: first,
                    to_agent_id: reviewer,
                    kind: MessageKind::ReviewRequest,
                    body: "Review initial implementation".into(),
                    in_reply_to: None,
                    review: Some(target.clone()),
                },
            ),
            (
                CallerRef {
                    agent_id: reviewer,
                    session_id: Some(fixture.session),
                },
                review_operation,
                SendMessage {
                    message_id: OperationId::new(),
                    to_agent_id: run.implementer,
                    kind: MessageKind::ChangesRequested,
                    body: "Fix the finding".into(),
                    in_reply_to: Some(first),
                    review: Some(target.clone()),
                },
            ),
            (
                caller,
                operation,
                SendMessage {
                    message_id: next,
                    to_agent_id: reviewer,
                    kind: MessageKind::ReviewRequest,
                    body: "Review the correction".into(),
                    in_reply_to: None,
                    review: Some(target),
                },
            ),
            (
                CallerRef {
                    agent_id: reviewer,
                    session_id: Some(fixture.session),
                },
                review_operation,
                SendMessage {
                    message_id: OperationId::new(),
                    to_agent_id: run.implementer,
                    kind: MessageKind::Message,
                    body: "Review is still in progress, not a verdict".into(),
                    in_reply_to: None,
                    review: None,
                },
            ),
        ] {
            store
                .send_message(fixture.workspace, &from, from_run, message)
                .unwrap();
        }
        // Replay an older persisted record before runtime lineage caching:
        // exact original handoff bindings remain sufficient evidence.
        let replay =
            usagi_daemon::usecase::workflow::snapshot(&store, fixture.workspace, fixture.session)
                .unwrap()
                .run
                .unwrap();
        assert_eq!(replay.review.unwrap().request, next);
        assert_eq!(replay.reviewer, Some(reviewer));
        assert_eq!(replay.revisions, 1);
        // Four, not three: the reviewer's "still in progress" message moves no
        // phase, and the history now keeps those too. Exactly the three that
        // advanced the run are marked.
        assert_eq!(replay.history.len(), 4);
        assert_eq!(
            replay.history.iter().filter(|entry| entry.advanced).count(),
            3
        );
        let idle = replay
            .history
            .iter()
            .find(|entry| !entry.advanced)
            .expect("the non-advancing message is kept");
        assert!(idle.body.contains("still in progress"));
        assert!(idle.at.is_some());
        assert_eq!(replay.phase, usagi_core::domain::workflow::Phase::Reviewing);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One lane fixture covers delivery, the mid-sweep stop and both skip rules.
    fn the_resident_workflow_lane_advances_a_run_without_any_client_request() {
        use usagi_core::domain::id::{OperationId, SessionId};
        use usagi_core::domain::workflow::Recipient;
        let fixture = Fixture::new();
        fixture
            .control(
                OperationId::new(),
                WorkflowCommand::Start {
                    goal: "lane progress".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap();
        let store = fixture.agent.lock().unwrap().dispatch_store().clone();
        // Enqueue straight into the record: no Workflow request is made from
        // here on, which is exactly the situation the lane exists for (a
        // closed TUI, or a session the user is not looking at).
        let instruction = OperationId::new();
        store
            .update_workflow(fixture.workspace, fixture.session, |record| {
                record
                    .as_mut()
                    .unwrap()
                    .run
                    .as_mut()
                    .unwrap()
                    .enqueue(instruction, Recipient::Implementer, "Keep going".into())
                    .map_err(anyhow::Error::msg)
            })
            .unwrap();
        assert!(fixture.writes.lock().unwrap().entries.is_empty());

        let notices = RecordingNotifier::default();
        let advanced = workflow::sweep(
            &fixture.agent,
            &fixture.inventory,
            workflow::Verification {
                cache: &fresh_verification_cache(),
                clock: &StoppedClock(0),
            },
            &fixture.bound.scope_resolver(),
            &notices,
            &|| false,
        )
        .unwrap();

        assert_eq!(advanced, 1);
        assert_eq!(
            store
                .workflow(fixture.workspace, fixture.session)
                .unwrap()
                .unwrap()
                .run
                .unwrap()
                .instructions[0]
                .delivery,
            Delivery::Notified
        );
        assert!(
            fixture
                .writes
                .lock()
                .unwrap()
                .entries
                .iter()
                .any(|(_, bytes)| String::from_utf8_lossy(bytes).contains("Keep going"))
        );
        // A record whose session this daemon cannot resolve is skipped, not
        // fatal: the sweep still counts the run it could advance. The sweep
        // visits records in identity order, so this one is given an identity
        // that sorts after the fixture's: the assertions below then describe
        // one fixed order instead of whichever one the random identities
        // happened to produce.
        let unresolvable = std::iter::repeat_with(SessionId::new)
            .find(|candidate| candidate.as_str() > fixture.session.as_str())
            .expect("identities are unbounded");
        store
            .update_workflow(fixture.workspace, unresolvable, |record| {
                *record = store.workflow(fixture.workspace, fixture.session).unwrap();
                Ok(())
            })
            .unwrap();
        assert_eq!(
            workflow::sweep(
                &fixture.agent,
                &fixture.inventory,
                workflow::Verification {
                    cache: &fresh_verification_cache(),
                    clock: &StoppedClock(0),
                },
                &fixture.bound.scope_resolver(),
                &notices,
                &|| false,
            )
            .unwrap(),
            1
        );
        // A daemon that starts stopping mid-sweep leaves the rest for its
        // next start. The fixture's record sorts first, so the stop lands
        // between the two: the first is advanced, the second is never
        // visited.
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            workflow::sweep(
                &fixture.agent,
                &fixture.inventory,
                workflow::Verification {
                    cache: &fresh_verification_cache(),
                    clock: &StoppedClock(0),
                },
                &fixture.bound.scope_resolver(),
                &notices,
                &|| {
                    calls.set(calls.get() + 1);
                    calls.get() > 1
                },
            )
            .unwrap(),
            1
        );
        assert_eq!(calls.get(), 2);
        // A launch that never bound its Agent waits for the human. Its
        // session resolves like any other, so the sweep reaches it and skips
        // it on the record itself.
        perform_create(
            fixture.bound.sessions(),
            &AlwaysSuccessfulGit,
            &OperationId::new().to_string(),
            &serde_json::json!({"name":"pending"}),
        )
        .unwrap();
        let pending_session = fixture
            .bound
            .sessions()
            .lock()
            .unwrap()
            .session_id("pending")
            .unwrap();
        store
            .update_workflow(fixture.workspace, pending_session, |record| {
                *record = Some(
                    store
                        .workflow(fixture.workspace, fixture.session)
                        .unwrap()
                        .unwrap(),
                );
                record.as_mut().unwrap().run = None;
                Ok(())
            })
            .unwrap();
        // `PR ready` is still swept: another review can be requested from
        // there, and instructions enqueued there still have to be delivered.
        // Only the unattended PR re-verification is left out.
        let ready_instruction = OperationId::new();
        store
            .update_workflow(fixture.workspace, fixture.session, |record| {
                let run = record.as_mut().unwrap().run.as_mut().unwrap();
                run.phase = usagi_core::domain::workflow::Phase::Ready;
                run.enqueue(
                    ready_instruction,
                    Recipient::Implementer,
                    "One more thing".into(),
                )
                .map_err(anyhow::Error::msg)
            })
            .unwrap();
        assert_eq!(
            workflow::sweep(
                &fixture.agent,
                &fixture.inventory,
                workflow::Verification {
                    cache: &fresh_verification_cache(),
                    clock: &StoppedClock(0),
                },
                &fixture.bound.scope_resolver(),
                &notices,
                &|| false,
            )
            .unwrap(),
            1
        );
        let swept = store
            .workflow(fixture.workspace, fixture.session)
            .unwrap()
            .unwrap()
            .run
            .unwrap();
        assert_eq!(swept.phase, usagi_core::domain::workflow::Phase::Ready);
        assert_eq!(
            swept
                .instructions
                .iter()
                .find(|item| item.id == ready_instruction)
                .unwrap()
                .delivery,
            Delivery::Notified
        );
        // The count is the proof: a swept record is counted, and the
        // run-less one is not — so nothing rewrote it either.
        assert!(
            store
                .workflow(fixture.workspace, pending_session)
                .unwrap()
                .unwrap()
                .run
                .is_none()
        );
    }

    #[test]
    fn a_named_issue_becomes_the_goal_or_is_refused_by_number() {
        let fixture = Fixture::new();
        // The backlog entry is read from the repository root, where a merged
        // PR leaves it.
        let root = fixture
            .bound
            .sessions()
            .lock()
            .unwrap()
            .repository_root()
            .to_path_buf();
        let issues = root.join(".usagi/issues");
        std::fs::create_dir_all(&issues).unwrap();
        std::fs::write(
            issues.join("742-close-the-loop.md"),
            "---\nnumber: 742\ntitle: fix(daemon): close the loop\nstatus: todo\npriority: high\nlabels: []\ndependson: []\nrelated: []\ncreated_at: 2026-09-12T00:00:00+00:00\nupdated_at: 2026-09-12T00:00:00+00:00\n---\n\nreproduce and fix\n",
        )
        .unwrap();
        let goal = workflow::issue_goal(&fixture.bound, 742).unwrap();
        assert!(goal.contains("fix(daemon): close the loop"), "{goal}");
        assert!(goal.contains("reproduce and fix"), "{goal}");
        // An issue this workspace does not have is named in the refusal, so
        // the caller can tell it from an unavailable dependency.
        let refused = workflow::issue_goal(&fixture.bound, 743).unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidArgument);
        assert!(refused.message.contains("#743"), "{}", refused.message);
        // A backlog that answers ambiguously is not a goal: two files
        // claiming the same number leave the store unable to say which
        // issue this run would implement.
        std::fs::write(
            issues.join("742-duplicate.md"),
            "---\nnumber: 742\ntitle: fix(daemon): duplicate\nstatus: todo\npriority: high\nlabels: []\ndependson: []\nrelated: []\ncreated_at: 2026-09-12T00:00:00+00:00\nupdated_at: 2026-09-12T00:00:00+00:00\n---\n\nsecond claim\n",
        )
        .unwrap();
        assert_eq!(
            workflow::issue_goal(&fixture.bound, 742).unwrap_err().code,
            ErrorCode::Unavailable
        );
    }

    #[test]
    fn remembered_participants_seed_a_start_that_names_nobody() {
        use usagi_core::domain::settings::DefaultModel;
        use usagi_core::domain::workflow::WorkflowAgents;
        let fixture = Fixture::new();
        // Nothing launched yet: the product defaults stand in.
        assert_eq!(
            workflow::remembered_defaults(&fixture.agent, fixture.workspace).agents,
            WorkflowAgents::default()
        );
        let remembered = WorkflowAgents {
            planner: DefaultModel::Agy,
            implementer: DefaultModel::Claude,
            reviewer: DefaultModel::OpenAi,
        };
        fixture
            .agent
            .lock()
            .unwrap()
            .dispatch_store()
            .remember_workflow_defaults(
                fixture.workspace,
                usagi_core::domain::workflow::WorkflowDefaults {
                    agents: remembered,
                    ..Default::default()
                },
            )
            .unwrap();
        // A caller that names nobody gets what this workspace already works
        // with, which is the same seed the start form shows.
        assert_eq!(
            workflow::remembered_defaults(&fixture.agent, fixture.workspace).agents,
            remembered
        );
        assert_eq!(
            workflow::requested_agents(&serde_json::json!({}), remembered),
            Some(remembered)
        );
        // Another workspace keeps its own answer.
        assert_eq!(
            workflow::remembered_defaults(&fixture.agent, WorkspaceId::new()).agents,
            WorkflowAgents::default()
        );
    }

    #[test]
    fn the_lane_announces_each_call_for_a_human_once() {
        use usagi_core::domain::id::OperationId;
        use usagi_core::domain::workflow::Phase;
        let fixture = Fixture::new();
        fixture
            .control(
                OperationId::new(),
                WorkflowCommand::Start {
                    goal: "Add login\nwith tests".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap();
        let store = fixture.agent.lock().unwrap().dispatch_store().clone();
        let notices = RecordingNotifier::default();
        let sweep = || {
            workflow::sweep(
                &fixture.agent,
                &fixture.inventory,
                workflow::Verification {
                    cache: &fresh_verification_cache(),
                    clock: &StoppedClock(0),
                },
                &fixture.bound.scope_resolver(),
                &notices,
                &|| false,
            )
            .unwrap()
        };
        // An Agent's turn is nobody's business but the Agent's.
        assert_eq!(sweep(), 1);
        assert!(notices.0.lock().unwrap().is_empty());

        let waiting = |reason: &str| {
            let reason = reason.to_owned();
            store
                .update_workflow(fixture.workspace, fixture.session, move |record| {
                    let run = record.as_mut().unwrap().run.as_mut().unwrap();
                    run.phase = Phase::Waiting;
                    run.waiting_reason = Some(reason);
                    Ok(())
                })
                .unwrap();
        };
        waiting("Revision limit reached");
        assert_eq!(sweep(), 1);
        // Sitting in the same phase is not a new event.
        assert_eq!(sweep(), 1);
        let announced = notices.0.lock().unwrap().clone();
        assert_eq!(announced.len(), 1);
        assert_eq!(announced[0].0, "usagi: workflow needs you");
        assert_eq!(announced[0].1, "Add login\nRevision limit reached");

        // Recovering and waiting again is a new event.
        store
            .update_workflow(fixture.workspace, fixture.session, |record| {
                let run = record.as_mut().unwrap().run.as_mut().unwrap();
                run.phase = Phase::Implementing;
                run.waiting_reason = None;
                Ok(())
            })
            .unwrap();
        assert_eq!(sweep(), 1);
        waiting("Assigned Agent is stopped");
        assert_eq!(sweep(), 1);
        assert_eq!(notices.0.lock().unwrap().len(), 2);

        // A finished run names its PR.
        store
            .update_workflow(fixture.workspace, fixture.session, |record| {
                let run = record.as_mut().unwrap().run.as_mut().unwrap();
                run.phase = Phase::Ready;
                run.waiting_reason = None;
                run.pr_url = Some("https://github.com/o/r/pull/9".into());
                Ok(())
            })
            .unwrap();
        assert_eq!(sweep(), 1);
        let announced = notices.0.lock().unwrap().clone();
        assert_eq!(announced.len(), 3);
        assert_eq!(announced[2].0, "usagi: PR ready");
        assert!(announced[2].1.contains("https://github.com/o/r/pull/9"));
    }

    #[test]
    fn workflow_verification_inventory_failure_is_reported_without_external_io() {
        use usagi_core::domain::id::OperationId;
        use usagi_core::domain::workflow::{Phase, Review};
        let fixture = Fixture::new();
        fixture
            .control(
                OperationId::new(),
                WorkflowCommand::Start {
                    goal: "verification error".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap();
        let store = fixture.agent.lock().unwrap().dispatch_store().clone();
        store
            .update_workflow(fixture.workspace, fixture.session, |record| {
                let run = record.as_mut().unwrap().run.as_mut().unwrap();
                run.phase = Phase::Verifying;
                run.review = Some(Review {
                    request: OperationId::new(),
                    target: usagi_core::domain::agent_message::ReviewTarget {
                        base_sha: "a".repeat(40),
                        head_sha: "b".repeat(40),
                    },
                    approved: true,
                });
                Ok(())
            })
            .unwrap();
        let inventory = Arc::clone(&fixture.inventory);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = inventory.lock().unwrap();
            panic!("fixture inventory failure");
        }));
        let error = fixture
            .call(DaemonRequest::WorkflowSnapshot {
                workspace: fixture.workspace,
                session: fixture.session,
            })
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Unavailable);
        // An instruction is not a verification: a GitHub read that is
        // momentarily unavailable must not refuse the command.
        let instruction = OperationId::new();
        let accepted = fixture
            .control(
                instruction,
                WorkflowCommand::Instruct {
                    recipient: Recipient::Implementer,
                    body: "Keep going".into(),
                },
            )
            .unwrap()
            .run
            .unwrap();
        assert_eq!(accepted.instructions[0].id, instruction);
    }

    #[test]
    fn workflow_pending_readiness_failure_restores_the_original_start_operation() {
        let mut fixture = Fixture::with_readiness(false);
        let operation = usagi_core::domain::id::OperationId::new();
        assert!(
            fixture
                .control(
                    operation,
                    WorkflowCommand::Start {
                        goal: "recover me".into(),
                        agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                        revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                    }
                )
                .is_err()
        );
        let pending = fixture
            .call(DaemonRequest::WorkflowSnapshot {
                workspace: fixture.workspace,
                session: fixture.session,
            })
            .unwrap()
            .pending_start
            .unwrap();
        assert_eq!(pending.operation_id, operation);
        assert_eq!(pending.goal, "recover me");
        assert!(pending.error.is_some());
        Arc::get_mut(&mut fixture.agent).unwrap().readiness = Arc::new(Ready(true));
        let recovered = fixture
            .control(
                pending.operation_id,
                WorkflowCommand::Start {
                    goal: pending.goal,
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap();
        assert!(recovered.pending_start.is_none());
        assert_eq!(recovered.run.unwrap().id, operation);
    }

    /// The refusal reported from the pane: an Agent the person opened
    /// themselves already owns the session, so no start can be launched
    /// until they stop it. Resending cannot change that, so the admission
    /// is undone rather than left holding the session.
    /// A start that names a backlog issue, and the finish that ends it.
    /// Both are ordinary session commands: neither reaches the daemon only
    /// through the MCP tool, so both are held here rather than resting on
    /// the shipping-binary E2E.
    #[test]
    fn an_issue_backed_start_is_archived_with_its_reference_when_it_finishes() {
        let fixture = Fixture::new();
        let root = fixture
            .bound
            .sessions()
            .lock()
            .unwrap()
            .repository_root()
            .to_path_buf();
        let issues = root.join(".usagi/issues");
        std::fs::create_dir_all(&issues).unwrap();
        std::fs::write(
            issues.join("742-close-the-loop.md"),
            "---\nnumber: 742\ntitle: fix(daemon): close the loop\nstatus: todo\npriority: high\nlabels: []\ndependson: []\nrelated: []\ncreated_at: 2026-09-12T00:00:00+00:00\nupdated_at: 2026-09-12T00:00:00+00:00\n---\n\nreproduce and fix\n",
        )
        .unwrap();
        let goal = workflow::issue_goal(&fixture.bound, 742).unwrap();
        let operation = usagi_core::domain::id::OperationId::new();
        let started = workflow::control_workflow(
            &fixture.agent,
            &fixture.bound,
            fixture.workspace,
            fixture.session,
            operation,
            WorkflowCommand::Start {
                goal,
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
            },
            Some(742),
        )
        .unwrap();
        assert_eq!(started.run.unwrap().id, operation);
        // Finishing changes the stored record and nothing else: the run
        // leaves the live slot for the bounded archive, still carrying the
        // issue it was started to implement.
        let ended = workflow::control_workflow(
            &fixture.agent,
            &fixture.bound,
            fixture.workspace,
            fixture.session,
            usagi_core::domain::id::OperationId::new(),
            WorkflowCommand::Finish,
            None,
        )
        .unwrap();
        assert!(ended.run.is_none());
        assert_eq!(ended.finished.len(), 1);
        assert_eq!(ended.finished[0].id, operation);
        assert_eq!(ended.finished[0].issue, Some(742));
    }

    #[test]
    fn a_start_refused_outright_leaves_the_session_free_of_the_intent() {
        let fixture = Fixture::new();
        fixture
            .agent
            .lock()
            .unwrap()
            .launch(
                &usagi_core::domain::id::OperationId::new().to_string(),
                &usagi_core::infrastructure::ipc::AgentLaunchIntent {
                    workspace: fixture.workspace,
                    session: Some(fixture.session),
                    profile: Some(AgentProfileId::new("claude").unwrap()),
                },
                &fixture.bound.scope_resolver(),
            )
            .unwrap();
        let refused = fixture
            .control(
                usagi_core::domain::id::OperationId::new(),
                WorkflowCommand::Start {
                    goal: "start anyway".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap_err();
        assert_eq!(refused.code, ErrorCode::Busy);
        assert_eq!(
            refused.retry_mode,
            usagi_core::infrastructure::ipc::RetryMode::Never
        );
        // Nothing is left holding the session: no pending start for the
        // pane to keep retrying, no run, and the bounded archive did not
        // spend one of its slots on a start that never launched.
        let after = fixture
            .call(DaemonRequest::WorkflowSnapshot {
                workspace: fixture.workspace,
                session: fixture.session,
            })
            .unwrap();
        assert!(after.pending_start.is_none());
        assert!(after.run.is_none());
        assert!(after.finished.is_empty());
        // This is the wedge itself: the record left behind used to refuse
        // the next start as "session already has another workflow", hiding
        // the one reason the person could act on. It stays the honest one,
        // and that refusal is undone in turn.
        let again = fixture
            .control(
                usagi_core::domain::id::OperationId::new(),
                WorkflowCommand::Start {
                    goal: "start anyway".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap_err();
        assert_eq!(again.code, ErrorCode::Busy);
        let after = fixture
            .call(DaemonRequest::WorkflowSnapshot {
                workspace: fixture.workspace,
                session: fixture.session,
            })
            .unwrap();
        assert!(after.pending_start.is_none());
        assert!(after.finished.is_empty());
    }

    struct VerificationGit(String);
    impl usagi_core::infrastructure::git::GitRunner for VerificationGit {
        fn run(
            &self,
            _: &Path,
            args: &[&str],
        ) -> anyhow::Result<usagi_core::infrastructure::git::GitOutput> {
            Ok(usagi_core::infrastructure::git::GitOutput {
                success: true,
                stdout: if args[0] == "status" {
                    String::new()
                } else {
                    self.0.clone()
                },
                stderr: String::new(),
            })
        }
    }
    struct VerificationGh(String);
    impl GhProcessPort for VerificationGh {
        type Error = std::io::Error;
        fn run(&mut self, _: &str, _: &[String], _: u64) -> Result<String, Self::Error> {
            Ok(self.0.clone())
        }
    }

    /// Counts what reached GitHub, which is the whole point of the cache.
    #[derive(Default)]
    struct CountingGh {
        output: String,
        calls: usize,
    }
    impl GhProcessPort for CountingGh {
        type Error = std::io::Error;
        fn run(&mut self, _: &str, _: &[String], _: u64) -> Result<String, Self::Error> {
            self.calls += 1;
            Ok(self.output.clone())
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One cache lifecycle: warm, expire, back off, invalidate.
    fn repeated_verification_of_one_head_spends_a_single_github_read_per_window() {
        use usagi_core::domain::workflow::{Phase, Review};
        let fixture = Fixture::new();
        let operation = usagi_core::domain::id::OperationId::new();
        let mut run = fixture
            .control(
                operation,
                WorkflowCommand::Start {
                    goal: "verify me".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap()
            .run
            .unwrap();
        run.phase = Phase::Verifying;
        run.review = Some(Review {
            request: usagi_core::domain::id::OperationId::new(),
            target: usagi_core::domain::agent_message::ReviewTarget {
                base_sha: "b".repeat(40),
                head_sha: "a".repeat(40),
            },
            approved: true,
        });
        let store = fixture.agent.lock().unwrap().dispatch_store().clone();
        store
            .update_workflow(fixture.workspace, fixture.session, |record| {
                record.as_mut().unwrap().run = Some(run.clone());
                Ok(())
            })
            .unwrap();
        let url = "https://github.com/owner/repo/pull/1";
        // Checks that have not finished: the answer that used to be re-asked of
        // GitHub on every sweep and every snapshot the open tab requested.
        let output = serde_json::json!({"title":"Task","state":"OPEN","headRefOid":"a".repeat(40),"isDraft":false,"reviewDecision":"APPROVED","statusCheckRollup":[{"status":"IN_PROGRESS"}],"mergeable":"MERGEABLE"}).to_string();
        let identity = usagi_core::domain::pr_inventory::extract(url.as_bytes()).remove(0);
        let view = usagi_daemon::usecase::pr_inventory::parse_gh_pr_view(&output).unwrap();
        fixture
            .inventory
            .lock()
            .unwrap()
            .observe_reported(fixture.session, url)
            .unwrap();
        fixture
            .inventory
            .lock()
            .unwrap()
            .publish_success(&identity, &view)
            .unwrap();

        let cache = fresh_verification_cache();
        let mut gh = CountingGh {
            output: output.clone(),
            calls: 0,
        };
        let verify = |now_ms: u64, gh: &mut CountingGh| {
            workflow::verify_progress(
                &store,
                &fixture.inventory,
                &cache,
                now_ms,
                &fixture.bound.scope_resolver(),
                fixture.workspace,
                fixture.session,
                &run,
                &VerificationGit("a".repeat(40)),
                gh,
            )
            .unwrap();
        };

        // Ten passes inside one window cost exactly one GitHub read.
        for tick in 0..10 {
            verify(tick * 100, &mut gh);
        }
        assert_eq!(gh.calls, 1);

        // The run still reaches the same conclusion from the cached answer: the
        // local git probes still run on every pass, so the verdict is not stale.
        assert_eq!(
            store
                .workflow(fixture.workspace, fixture.session)
                .unwrap()
                .unwrap()
                .run
                .unwrap()
                .waiting_reason
                .as_deref(),
            Some("Waiting for successful PR checks")
        );

        // Past the window a second read happens, and because the answer was
        // "still waiting" the next window is longer than the first.
        verify(20_000, &mut gh);
        assert_eq!(gh.calls, 2);
        verify(45_000, &mut gh);
        assert_eq!(gh.calls, 2, "the backoff doubled the window");
        verify(50_001, &mut gh);
        assert_eq!(gh.calls, 3);

        // A new approved HEAD is different evidence and is never answered from
        // the previous one's cache. The inventory has to carry a PR for it, or
        // verification refuses locally before GitHub is consulted at all.
        let moved_output = serde_json::json!({"title":"Task","state":"OPEN","headRefOid":"c".repeat(40),"isDraft":false,"reviewDecision":"APPROVED","statusCheckRollup":[{"status":"IN_PROGRESS"}],"mergeable":"MERGEABLE"}).to_string();
        fixture
            .inventory
            .lock()
            .unwrap()
            .publish_success(
                &identity,
                &usagi_daemon::usecase::pr_inventory::parse_gh_pr_view(&moved_output).unwrap(),
            )
            .unwrap();
        gh.output = moved_output;
        let mut moved = run.clone();
        moved.review.as_mut().unwrap().target.head_sha = "c".repeat(40);
        workflow::verify_progress(
            &store,
            &fixture.inventory,
            &cache,
            50_002,
            &fixture.bound.scope_resolver(),
            fixture.workspace,
            fixture.session,
            &moved,
            &VerificationGit("c".repeat(40)),
            &mut gh,
        )
        .unwrap();
        assert_eq!(gh.calls, 4);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One publication scenario: the evidence, its staleness, and the demotion.
    fn workflow_pr_publication_uses_injected_independent_git_and_github_evidence() {
        use usagi_core::domain::workflow::{Phase, Review};
        let fixture = Fixture::new();
        let operation = usagi_core::domain::id::OperationId::new();
        let mut run = fixture
            .control(
                operation,
                WorkflowCommand::Start {
                    goal: "verify me".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                    revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                },
            )
            .unwrap()
            .run
            .unwrap();
        run.phase = Phase::Verifying;
        run.review = Some(Review {
            request: usagi_core::domain::id::OperationId::new(),
            target: usagi_core::domain::agent_message::ReviewTarget {
                base_sha: "b".repeat(40),
                head_sha: "a".repeat(40),
            },
            approved: true,
        });
        let store = fixture.agent.lock().unwrap().dispatch_store().clone();
        store
            .update_workflow(fixture.workspace, fixture.session, |record| {
                record.as_mut().unwrap().run = Some(run.clone());
                Ok(())
            })
            .unwrap();
        let url = "https://github.com/owner/repo/pull/1";
        let mut output = serde_json::json!({"title":"Task","state":"OPEN","headRefOid":"a".repeat(40),"isDraft":false,"reviewDecision":"APPROVED","statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}],"mergeable":"MERGEABLE"});
        let identity = usagi_core::domain::pr_inventory::extract(url.as_bytes()).remove(0);
        let view =
            usagi_daemon::usecase::pr_inventory::parse_gh_pr_view(&output.to_string()).unwrap();
        fixture
            .inventory
            .lock()
            .unwrap()
            .observe_reported(fixture.session, url)
            .unwrap();
        fixture
            .inventory
            .lock()
            .unwrap()
            .publish_success(&identity, &view)
            .unwrap();
        workflow::verify_progress(
            &store,
            &fixture.inventory,
            &fresh_verification_cache(),
            0,
            &fixture.bound.scope_resolver(),
            fixture.workspace,
            fixture.session,
            &run,
            &VerificationGit("a".repeat(40)),
            &mut VerificationGh(output.to_string()),
        )
        .unwrap();
        assert_eq!(
            store
                .workflow(fixture.workspace, fixture.session)
                .unwrap()
                .unwrap()
                .run
                .unwrap()
                .phase,
            Phase::Ready
        );
        output["isDraft"] = serde_json::json!(true);
        // Production re-reads the run before each verification, and the
        // publication only lands on the phase it was started from.
        let ready = store
            .workflow(fixture.workspace, fixture.session)
            .unwrap()
            .unwrap()
            .run
            .unwrap();
        workflow::verify_progress(
            &store,
            &fixture.inventory,
            &fresh_verification_cache(),
            0,
            &fixture.bound.scope_resolver(),
            fixture.workspace,
            fixture.session,
            &ready,
            &VerificationGit("a".repeat(40)),
            &mut VerificationGh(output.to_string()),
        )
        .unwrap();
        assert_eq!(
            store
                .workflow(fixture.workspace, fixture.session)
                .unwrap()
                .unwrap()
                .run
                .unwrap()
                .phase,
            Phase::Verifying
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One dispatch fixture tests replay and exact targeting with a concurrent sibling.
    fn workflow_dispatch_is_scoped_idempotent_and_delivers_only_to_its_exact_agent() {
        let fixture = Fixture::new();
        assert!(
            fixture
                .call(DaemonRequest::WorkflowSnapshot {
                    workspace: fixture.workspace,
                    session: fixture.session
                })
                .unwrap()
                .run
                .is_none()
        );
        assert_eq!(
            fixture
                .call(DaemonRequest::WorkflowSnapshot {
                    workspace: WorkspaceId::new(),
                    session: fixture.session
                })
                .unwrap_err()
                .code,
            ErrorCode::OwnershipUnknown
        );
        assert!(
            fixture
                .call(DaemonRequest::WorkflowSnapshot {
                    workspace: fixture.workspace,
                    session: SessionId::new()
                })
                .is_err()
        );
        assert_eq!(
            fixture
                .call(DaemonRequest::SupervisorSnapshot {
                    workspace: fixture.workspace
                })
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        let operation = usagi_core::domain::id::OperationId::new();
        assert_eq!(
            fixture
                .control(
                    operation,
                    WorkflowCommand::Start {
                        goal: " ".into(),
                        agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                        revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                    }
                )
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        let command = WorkflowCommand::Start {
            goal: "Implement feature".into(),
            agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
        };
        let first = fixture.control(operation, command.clone()).unwrap();
        assert_eq!(fixture.control(operation, command).unwrap(), first);
        assert_eq!(
            fixture
                .control(
                    operation,
                    WorkflowCommand::Start {
                        goal: "Changed".into(),
                        agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                        revision_limit: usagi_core::domain::workflow::DEFAULT_REVISION_LIMIT,
                    }
                )
                .unwrap_err()
                .code,
            ErrorCode::IdempotencyConflict
        );
        let run = first.run.unwrap();
        let credential = fixture
            .agent
            .lock()
            .unwrap()
            .hook_credential(5000, 4321, 4321)
            .unwrap()
            .to_owned();
        assert!(
            fixture
                .agent
                .lock()
                .unwrap()
                .mcp_dispatch_context(&credential)
                .is_some()
        );
        for credential in [credential, "invalid-credential".into()] {
            let request = DaemonRequest::WorkflowControl {
                workspace: fixture.workspace,
                session: fixture.session,
                operation_id: usagi_core::domain::id::OperationId::new(),
                command: WorkflowCommand::Instruct {
                    recipient: Recipient::Implementer,
                    body: "must not deliver".into(),
                },
            };
            let mut raw = serde_json::to_value(&request).unwrap();
            raw["caller_context"] = serde_json::json!({"credential":credential});
            assert_eq!(
                fixture.call_raw(request, &raw).unwrap_err().code,
                ErrorCode::PermissionDenied
            );
        }
        let sibling_operation = usagi_core::domain::id::OperationId::new().to_string();
        let sibling_intent = usagi_core::infrastructure::ipc::AgentLaunchIntent {
            workspace: fixture.workspace,
            session: Some(fixture.session),
            profile: Some(AgentProfileId::new("codex").unwrap()),
        };
        let ticket = fixture
            .agent
            .lock()
            .unwrap()
            .prepare_launch_readiness(&sibling_operation, &sibling_intent)
            .unwrap();
        run_agent_readiness(&fixture.agent, ticket.as_ref()).unwrap();
        let sibling = fixture
            .agent
            .lock()
            .unwrap()
            .launch_after_readiness(
                &sibling_operation,
                &sibling_intent,
                &fixture.bound.scope_resolver(),
                ticket.as_ref(),
            )
            .unwrap();
        let instruction = usagi_core::domain::id::OperationId::new();
        assert_eq!(
            fixture
                .control(
                    instruction,
                    WorkflowCommand::Instruct {
                        recipient: Recipient::Reviewer,
                        body: "No reviewer".into()
                    }
                )
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        let command = WorkflowCommand::Instruct {
            recipient: Recipient::Implementer,
            body: "Verify the edge cases".into(),
        };
        let delivered = fixture.control(instruction, command.clone()).unwrap();
        assert_eq!(
            delivered.run.unwrap().instructions[0].delivery,
            Delivery::Notified
        );
        fixture.control(instruction, command).unwrap();
        let writes = fixture.writes.lock().unwrap();
        assert_eq!(writes.entries.len(), 1);
        let expected = fixture
            .agent
            .lock()
            .unwrap()
            .runtime_for_operation(run.id)
            .unwrap()
            .terminal;
        assert_eq!(writes.entries[0].0, expected);
        assert_ne!(writes.entries[0].0, sibling.terminal);
        assert!(String::from_utf8_lossy(&writes.entries[0].1).contains("Verify the edge cases"));
        drop(writes);
        fixture.agent.lock().unwrap().exit(&expected, 0).unwrap();
        let stopped = fixture
            .call(DaemonRequest::WorkflowSnapshot {
                workspace: fixture.workspace,
                session: fixture.session,
            })
            .unwrap()
            .run
            .unwrap();
        assert_eq!(stopped.phase, usagi_core::domain::workflow::Phase::Waiting);
        assert!(
            stopped
                .waiting_reason
                .unwrap()
                .contains("stopped or interrupted")
        );
    }
}

#[test]
fn supervisor_snapshot_is_exactly_workspace_scoped() {
    use chrono::Utc;
    use usagi_core::domain::supervisor::SupervisorWorkspaceSnapshot;
    use usagi_core::infrastructure::ipc::{EnvelopeKind, ErrorCode, ResponseOutcome};
    use usagi_daemon::usecase::session_runtime::SessionRuntime;

    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    let sessions = Arc::new(Mutex::new(
        SessionRuntime::open(
            repository.clone(),
            &temporary.path().join("session-daemon"),
            DaemonGeneration::new(),
            AlwaysSuccessfulGit,
            PermissiveSessionWorktreeIo,
        )
        .unwrap(),
    ));
    let workspace: WorkspaceId = serde_json::from_value(
        sessions.lock().unwrap().snapshot().unwrap()["workspace_id"].clone(),
    )
    .unwrap();
    let bound = bound_to(
        &temporary.path().join("tenants"),
        &repository,
        sessions,
        workspace,
    );
    let runtime = Arc::new(Mutex::new(SupervisorRuntime::new(
        &temporary.path().join("supervisor"),
    )));
    let visible = runtime
        .lock()
        .unwrap()
        .start_for_workspace(
            "caller",
            workspace,
            "start",
            "root".into(),
            Vec::new(),
            None,
            Utc::now(),
        )
        .unwrap();
    let request_id = usagi_core::infrastructure::ipc::RequestId("snapshot".into());
    let request = serde_json::to_value(DaemonRequest::SupervisorSnapshot { workspace }).unwrap();
    let reply = dispatch_supervisor_snapshot(
        &runtime,
        &bound,
        request_id.clone(),
        &request,
        &session_test_hello(),
    );
    let EnvelopeKind::Response { outcome, body, .. } = reply.kind else {
        panic!("supervisor snapshot returned a non-response envelope");
    };
    assert_eq!(outcome, ResponseOutcome::Ok);
    let snapshot: SupervisorWorkspaceSnapshot = serde_json::from_value(body).unwrap();
    assert_eq!(snapshot.workspace_id, workspace);
    assert_eq!(snapshot.runs.len(), 1);
    assert_eq!(
        snapshot.runs[0].supervisor_run_id,
        visible.supervisor_run_id
    );
    assert!(snapshot.runs[0].provenance.is_empty());

    let foreign = serde_json::to_value(DaemonRequest::SupervisorSnapshot {
        workspace: WorkspaceId::new(),
    })
    .unwrap();
    let rejected = dispatch_supervisor_snapshot(
        &runtime,
        &bound,
        request_id,
        &foreign,
        &session_test_hello(),
    );
    let EnvelopeKind::Response { outcome, .. } = rejected.kind else {
        panic!("supervisor snapshot returned a non-response envelope");
    };
    assert!(
        matches!(outcome, ResponseOutcome::Error(error) if error.code == ErrorCode::OwnershipUnknown)
    );
}

#[test]
fn supervisor_delete_dispatch_returns_exact_receipt() {
    use chrono::Utc;
    use usagi_core::domain::id::OperationId;
    use usagi_core::domain::supervisor::{SupervisorRunDeletion, SupervisorWorkspaceCommand};
    use usagi_core::infrastructure::ipc::DaemonRequest;
    use usagi_core::infrastructure::ipc::{EnvelopeKind, ResponseOutcome};
    use usagi_daemon::usecase::session_runtime::SessionRuntime;

    let temporary = tempfile::tempdir().unwrap();
    let repository = temporary.path().join("repository");
    let sessions = Arc::new(Mutex::new(
        SessionRuntime::open(
            repository.clone(),
            &temporary.path().join("session-daemon"),
            DaemonGeneration::new(),
            AlwaysSuccessfulGit,
            PermissiveSessionWorktreeIo,
        )
        .unwrap(),
    ));
    let workspace: WorkspaceId = serde_json::from_value(
        sessions.lock().unwrap().snapshot().unwrap()["workspace_id"].clone(),
    )
    .unwrap();
    let bound = bound_to(
        &temporary.path().join("tenants"),
        &repository,
        sessions,
        workspace,
    );
    let runtime = Arc::new(Mutex::new(SupervisorRuntime::new(
        &temporary.path().join("supervisor"),
    )));
    let active = runtime
        .lock()
        .unwrap()
        .start_for_workspace(
            "caller",
            workspace,
            "start",
            "root".into(),
            Vec::new(),
            None,
            Utc::now(),
        )
        .unwrap();
    let cancelled = runtime
        .lock()
        .unwrap()
        .control_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: active.supervisor_run_id,
                reason: "operator cancelled".into(),
            },
            Utc::now(),
        )
        .unwrap();
    let operation_id = OperationId::new();
    let request = serde_json::to_value(DaemonRequest::SupervisorControl {
        workspace,
        operation_id,
        command: SupervisorWorkspaceCommand::Delete {
            supervisor_run_id: cancelled.supervisor_run_id,
            observed_state_revision: cancelled.state_revision,
        },
    })
    .unwrap();
    let agent = empty_supervisor_agent(DispatchStore::new(temporary.path().join("agent-dispatch")));
    let reply = dispatch_supervisor_control(
        &runtime,
        &agent,
        &bound,
        usagi_core::infrastructure::ipc::RequestId("delete".into()),
        &request,
        &session_test_hello(),
    );
    let EnvelopeKind::Response { outcome, body, .. } = reply.kind else {
        panic!("supervisor control returned a non-response envelope");
    };
    assert_eq!(outcome, ResponseOutcome::Ok);
    assert_eq!(
        serde_json::from_value::<SupervisorRunDeletion>(body).unwrap(),
        SupervisorRunDeletion {
            supervisor_run_id: cancelled.supervisor_run_id,
            state_revision: cancelled.state_revision,
        }
    );
    assert!(
        runtime
            .lock()
            .unwrap()
            .get_for_workspace(workspace, cancelled.supervisor_run_id)
            .unwrap()
            .is_none()
    );
}

struct GoalScope(
    Result<
        usagi_daemon::usecase::agent_ipc::ResolvedAgentScope,
        usagi_daemon::usecase::agent_ipc::ScopeResolveError,
    >,
);
impl usagi_daemon::usecase::agent_ipc::SessionScopeResolver for GoalScope {
    fn resolve_available_scope(
        &self,
        _: WorkspaceId,
        _: Option<SessionId>,
    ) -> Result<
        usagi_daemon::usecase::agent_ipc::ResolvedAgentScope,
        usagi_daemon::usecase::agent_ipc::ScopeResolveError,
    > {
        self.0.clone()
    }
}

fn goal_intent(workspace: WorkspaceId) -> usagi_core::infrastructure::ipc::AgentGoalIntent {
    usagi_core::infrastructure::ipc::AgentGoalIntent {
        workspace,
        profile: None,
        goal: "prepare the requested change for review".into(),
    }
}

#[test]
fn goal_repository_resolution_reuses_the_reservation_and_maps_scope_failure() {
    use usagi_core::domain::{id::OperationId, pr_inventory::GitHubRepository};
    use usagi_daemon::usecase::agent_ipc::ScopeResolveError;

    let temporary = tempfile::tempdir().unwrap();
    let workspace = WorkspaceId::new();
    let intent = goal_intent(workspace);
    let repository = GitHubRepository::from_name_with_owner("acme/repo").unwrap();
    let reserved_operation = OperationId::new().to_string();
    let reserved = Arc::new(Mutex::new(SupervisorRuntime::new(
        &temporary.path().join("reserved"),
    )));
    reserve_goal_supervisor_run(
        &reserved,
        &reserved_operation,
        &intent,
        repository.clone(),
        AgentProfileId::new("claude").unwrap(),
    )
    .unwrap();
    assert_eq!(
        resolve_goal_artifact_repository(
            &reserved,
            &GoalScope(Err(ScopeResolveError::Unavailable)),
            &reserved_operation,
            &intent,
        )
        .unwrap(),
        repository
    );

    let unresolved = Arc::new(Mutex::new(SupervisorRuntime::new(
        &temporary.path().join("unresolved"),
    )));
    let operation = OperationId::new().to_string();
    assert_eq!(
        resolve_goal_artifact_repository(
            &unresolved,
            &GoalScope(Err(ScopeResolveError::Unavailable)),
            &operation,
            &intent,
        )
        .unwrap_err()
        .message,
        "Goal workspace is unavailable"
    );
}

#[test]
fn goal_repository_resolution_validates_git_and_owner_health() {
    use usagi_core::domain::{id::OperationId, pr_inventory::GitHubRepository};
    use usagi_daemon::usecase::agent_ipc::ResolvedAgentScope;

    let temporary = tempfile::tempdir().unwrap();
    let workspace = WorkspaceId::new();
    let intent = goal_intent(workspace);
    let repository = GitHubRepository::from_name_with_owner("acme/repo").unwrap();
    let operation = OperationId::new().to_string();
    let unresolved = Arc::new(Mutex::new(SupervisorRuntime::new(
        &temporary.path().join("unresolved"),
    )));

    let non_repository = temporary.path().join("not-a-repository");
    std::fs::create_dir(&non_repository).unwrap();
    let unavailable_git = GoalScope(Ok(ResolvedAgentScope {
        worktree_id: WorktreeId::new(),
        working_directory: non_repository,
    }));
    assert_eq!(
        resolve_goal_artifact_repository(&unresolved, &unavailable_git, &operation, &intent)
            .unwrap_err()
            .message,
        "Goal workspace GitHub repository is unavailable"
    );

    let git_repository = temporary.path().join("repository");
    std::fs::create_dir(&git_repository).unwrap();
    assert!(
        Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(&git_repository)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&git_repository)
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/acme/repo.git",
            ])
            .status()
            .unwrap()
            .success()
    );
    let available_git = GoalScope(Ok(ResolvedAgentScope {
        worktree_id: WorktreeId::new(),
        working_directory: git_repository,
    }));
    assert_eq!(
        resolve_goal_artifact_repository(&unresolved, &available_git, &operation, &intent).unwrap(),
        repository
    );

    let poisoned = Arc::new(Mutex::new(SupervisorRuntime::new(
        &temporary.path().join("poisoned-resolution"),
    )));
    let poison_owner = Arc::clone(&poisoned);
    std::thread::spawn(move || {
        let _guard = poison_owner.lock().unwrap();
        panic!("poison goal repository resolver owner");
    })
    .join()
    .unwrap_err();
    assert_eq!(
        resolve_goal_artifact_repository(&poisoned, &available_git, &operation, &intent)
            .unwrap_err()
            .message,
        "supervisor runtime is unavailable"
    );
}

#[test]
fn goal_supervisor_promotion_maps_a_poisoned_owner_to_unavailable() {
    use usagi_core::domain::id::AgentId;
    use usagi_core::infrastructure::ipc::{AgentGoalIntent, agent_goal_semantic_key};
    use usagi_core::infrastructure::store::dispatch::DispatchStore;

    let temporary = tempfile::tempdir().unwrap();
    let workspace = WorkspaceId::new();
    let intent = AgentGoalIntent {
        workspace,
        profile: None,
        goal: "prepare the requested change for review".into(),
    };
    let operation = usagi_core::domain::id::OperationId::new().to_string();
    let worker = AgentRuntimeRef::new(
        AgentRuntimeId::new(),
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: None,
            worktree_id: WorktreeId::new(),
        },
        None,
    )
    .unwrap();
    let healthy = Arc::new(Mutex::new(SupervisorRuntime::new(temporary.path())));
    persist_supervisor_dispatch(
        &DispatchStore::new(temporary.path()),
        workspace,
        usagi_core::domain::id::OperationId::parse(&operation).unwrap(),
        AgentId::new(),
        &worker,
        agent_goal_semantic_key(&intent),
    );
    let started = start_goal_supervisor_run(&healthy, &operation, &intent, &worker).unwrap();
    assert_eq!(
        started.tasks[0].state,
        usagi_core::domain::supervisor::TaskState::Dispatched
    );
    assert_eq!(
        goal_supervisor_caller(workspace),
        format!("goal-composer:{workspace}")
    );

    let poisoned = Arc::new(Mutex::new(SupervisorRuntime::new(
        &temporary.path().join("poisoned"),
    )));
    let poison_owner = Arc::clone(&poisoned);
    std::thread::spawn(move || {
        let _guard = poison_owner.lock().unwrap();
        panic!("poison supervisor runtime for the unavailable-path fixture");
    })
    .join()
    .unwrap_err();
    let error = start_goal_supervisor_run(&poisoned, &operation, &intent, &worker).unwrap_err();
    assert_eq!(
        error.code,
        usagi_core::infrastructure::ipc::ErrorCode::Unavailable
    );
}

#[test]
fn supervisor_query_capacity_maps_to_typed_backpressure() {
    assert_eq!(
        supervisor_error(anyhow::anyhow!(
            "supervisor query response capacity is exhausted"
        ))
        .code,
        usagi_core::infrastructure::ipc::ErrorCode::ResourceExhausted
    );
    assert_eq!(
        supervisor_error(anyhow::anyhow!(
            "dispatch already belongs to another retained supervisor run"
        ))
        .code,
        usagi_core::infrastructure::ipc::ErrorCode::RevisionConflict
    );
    assert_eq!(
        supervisor_error(anyhow::anyhow!(
            "parent dispatch has stale supervisor ownership"
        ))
        .code,
        usagi_core::infrastructure::ipc::ErrorCode::RevisionConflict
    );
    assert_eq!(
        supervisor_error(anyhow::anyhow!(
            "parent dispatch has closed supervisor ownership"
        ))
        .code,
        usagi_core::infrastructure::ipc::ErrorCode::RevisionConflict
    );
    assert_eq!(
        supervisor_error(anyhow::anyhow!(
            "delegated dispatch operation is already in use"
        ))
        .code,
        usagi_core::infrastructure::ipc::ErrorCode::IdempotencyConflict
    );
}

fn fence_in(role: GenerationRole) -> GenerationFence {
    GenerationFence {
        gate: AdmissionGate::new(DaemonGeneration::new(), role),
        ledger: Arc::new(RoutingLedger::new()),
    }
}

fn session_request() -> serde_json::Value {
    serde_json::json!({"kind": "session", "action": "list", "operation_id": "op", "payload": null})
}

fn attach_request() -> serde_json::Value {
    let terminal = usagi_core::domain::id::TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: TerminalId::new(),
        workspace_id: WorkspaceId::new(),
        session_id: Some(SessionId::new()),
        worktree_id: WorktreeId::new(),
    };
    serde_json::to_value(DaemonRequest::Terminal {
        action: TerminalAction::Attach,
        payload: serde_json::to_value(TerminalRequest::Attach {
            terminal,
            geometry: None,
        })
        .unwrap(),
    })
    .unwrap()
}

/// The fence changes nothing for the one active generation this build runs:
/// every request a client sent before is still dispatched, and the terminal
/// owner still sees its own IO.
#[test]
fn an_active_generation_serves_every_request_through_its_fence_unchanged() {
    use usagi_core::infrastructure::ipc::ResponseOutcome;
    let fence = fence_in(GenerationRole::Active);
    let (outcomes, seen) = serve_through_fence(
        &fence,
        &fence_client_hello(vec![
            usagi_core::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY.to_owned(),
        ]),
        &[session_request(), attach_request()],
    );
    assert_eq!(outcomes.len(), 2);
    assert!(
        outcomes
            .iter()
            .all(|outcome| !matches!(outcome, ResponseOutcome::Error(_))),
        "{outcomes:?}"
    );
    assert_eq!(seen, vec![TerminalAction::Attach]);
    // Every lease the two requests took has been released, so a barrier
    // starting now would not wait on this connection.
    assert_eq!(fence.gate.outstanding(LeaseClass::ActiveControl), 0);
    assert_eq!(fence.gate.outstanding(LeaseClass::OwnerTerminal), 0);
}

/// The pair this fence exists for: once the role is `draining`, control is
/// refused with zero effect from the *next request onwards* — on a connection
/// that was admitted while the generation was still active — while IO on the
/// terminals it owns keeps being served.
#[test]
fn a_draining_generation_refuses_control_and_still_serves_its_own_terminals() {
    use usagi_core::infrastructure::ipc::{ErrorCode, ResponseOutcome};
    let fence = fence_in(GenerationRole::Active);
    fence.gate.close(LeaseClass::ActiveControl);
    fence.gate.await_drain(LeaseClass::ActiveControl).unwrap();
    fence.gate.enter_draining().unwrap();

    let (outcomes, seen) = serve_through_fence(
        &fence,
        &fence_client_hello(vec![
            usagi_core::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY.to_owned(),
        ]),
        &[session_request(), attach_request()],
    );
    match &outcomes[0] {
        ResponseOutcome::Error(error) => {
            assert_eq!(error.code, ErrorCode::GenerationRolledOver);
        }
        other => panic!("a draining generation admitted control work: {other:?}"),
    }
    assert!(
        !matches!(outcomes[1], ResponseOutcome::Error(_)),
        "{:?}",
        outcomes[1]
    );
    // Effect zero for the refused control request: the owner saw only the
    // terminal IO it owns.
    assert_eq!(seen, vec![TerminalAction::Attach]);
}

/// A retired generation admits nothing at all, terminal IO included, and the
/// terminal owner is never reached.
#[test]
fn a_retired_generation_admits_nothing_and_reaches_no_owner() {
    use usagi_core::infrastructure::ipc::{ErrorCode, ResponseOutcome};
    let fence = fence_in(GenerationRole::Active);
    fence.gate.close(LeaseClass::ActiveControl);
    fence.gate.close(LeaseClass::OwnerTerminal);
    fence.gate.enter_retired().unwrap();

    let (outcomes, seen) = serve_through_fence(
        &fence,
        &fence_client_hello(vec![
            usagi_core::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY.to_owned(),
        ]),
        &[session_request(), attach_request()],
    );
    assert_eq!(outcomes.len(), 2);
    for outcome in &outcomes {
        match outcome {
            ResponseOutcome::Error(error) => {
                assert_eq!(error.code, ErrorCode::GenerationRolledOver);
            }
            other => panic!("a retired generation admitted work: {other:?}"),
        }
    }
    assert!(seen.is_empty(), "{seen:?}");
}

/// The ledger half: a connection is recorded with the routing answer it
/// advertised, which is what decides whether a rollover may leave this
/// generation draining at all — a client that cannot address a draining owner
/// is counted as unsupported and blocks the rollover.
#[test]
fn the_fence_records_each_connections_routing_answer() {
    use usagi_daemon::presentation::ipc::ConnectionFence;
    let fence = fence_in(GenerationRole::Active);
    let routing = fence_client_hello(vec![
        usagi_core::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY.to_owned(),
    ]);
    let old_build = fence_client_hello(Vec::new());

    let (first, second) = (ConnectionId::new(), ConnectionId::new());
    fence.admitted(first, &routing).unwrap();
    fence.admitted(second, &old_build).unwrap();
    assert_eq!(fence.ledger.connections(), 2);
    assert_eq!(fence.ledger.unsupported(), 1);

    // The peer that could not address a draining owner has gone away, so it
    // stops blocking a rollover.
    fence.disconnected(second);
    assert_eq!(fence.ledger.connections(), 1);
    assert_eq!(fence.ledger.unsupported(), 0);
}

#[test]
fn a_connection_unblocked_after_commit_must_support_owner_routing() {
    use usagi_daemon::presentation::ipc::ConnectionFence;

    let fence = fence_in(GenerationRole::Active);
    fence.gate.close(LeaseClass::ActiveControl);
    fence.gate.await_drain(LeaseClass::ActiveControl).unwrap();
    fence.gate.enter_draining().unwrap();

    let refused = fence
        .admitted(ConnectionId::new(), &fence_client_hello(Vec::new()))
        .unwrap_err();
    assert_eq!(
        refused.code,
        usagi_core::infrastructure::ipc::ErrorCode::GenerationRolledOver
    );
    assert_eq!(fence.ledger.connections(), 0);

    fence
        .admitted(
            ConnectionId::new(),
            &fence_client_hello(vec![
                usagi_core::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY.to_owned(),
            ]),
        )
        .unwrap();
    assert_eq!(fence.ledger.connections(), 1);
}

/// The loop itself performs that pair: a connection is admitted before
/// handshake success and forgotten on every exit, so the ledger tracks live
/// connections rather than historical ones.
#[test]
fn serving_a_connection_admits_it_to_the_ledger_and_forgets_it_at_the_end() {
    let fence = fence_in(GenerationRole::Active);
    assert_eq!(fence.ledger.connections(), 0);
    serve_through_fence(
        &fence,
        &fence_client_hello(Vec::new()),
        &[session_request()],
    );
    assert_eq!(fence.ledger.connections(), 0);
}

/// A worker whose stream could not be duplicated is not retained: retirement
/// must never park on a thread it has no way to unblock.
#[test]
fn only_a_collectable_client_worker_is_retained() {
    let workers = ClientWorkers::new();
    // A refused worker's handle is consumed and dropped, so nothing can join
    // it afterwards — it is driven to completion *before* being handed over.
    // A worker thread still running writes coverage counters while the harness
    // dumps the profile, and that race reports lines other tests certainly
    // executed as unreached.
    let refused = std::thread::spawn(|| {});
    while !refused.is_finished() {
        std::thread::yield_now();
    }
    retain_client_worker(
        &workers,
        Err(std::io::Error::other("no descriptors")),
        refused,
    );
    assert_eq!(workers.outstanding(), 0);

    // The production-owned admission counter and worker set are injected
    // independently. Holding the permit inside the worker makes the two
    // observable lifetimes match the accept-loop contract without asking
    // the OS for a process-wide thread census.
    let pre_handshake = PreHandshakeAdmission::new(1);
    let permit = pre_handshake
        .try_admit()
        .expect("the incomplete handshake reserves the only permit");
    // Shaped exactly as the accept loop builds it: the retained half is a
    // duplicate of the *accepted* socket, and the worker parks on that same
    // socket armed with the retirement poll. `peer` stays open so nothing but
    // retirement can end the read.
    let (mut peer, accepted) = std::os::unix::net::UnixStream::pair().unwrap();
    let unblock = accepted.try_clone().map(AcceptedStream::new);
    let mut parked_stream = RetiringReader::new(
        accepted,
        unblock
            .as_ref()
            .expect("the accepted socket duplicates")
            .retirement(),
        Duration::from_millis(10),
    );
    let parked = std::thread::spawn(move || {
        let _permit = permit;
        // Parked exactly as a client worker is: blocked reading a frame that
        // never arrives.
        let mut byte = [0_u8; 1];
        let _ = parked_stream.read(&mut byte);
    });
    retain_client_worker(&workers, unblock, parked);
    assert_eq!(pre_handshake.in_flight(), 1);
    assert!(pre_handshake.try_admit().is_none());
    assert_eq!(workers.outstanding(), 1);

    // Retirement shuts the retained half down, which is what lets the join
    // return. A test that hung here would be reporting a real defect.
    let report = workers.retire();
    assert_eq!(report.joined, 1);
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(pre_handshake.in_flight(), 0);
    assert_eq!(workers.outstanding(), 0);
    let mut byte = [0_u8; 1];
    assert_eq!(
        peer.read(&mut byte).unwrap(),
        0,
        "retirement closes the socket"
    );
}

/// Builds a reader parked on a socketpair nothing ever writes to, plus the
/// peer that keeps it open.
fn parked_reader(retired: &Arc<AtomicBool>) -> (std::os::unix::net::UnixStream, RetiringReader) {
    let (peer, accepted) = std::os::unix::net::UnixStream::pair().unwrap();
    let reader = RetiringReader::new(accepted, Arc::clone(retired), Duration::from_millis(5));
    (peer, reader)
}

/// The defect this exists for: `shutdown(2)` can return `Ok` for a duplicate
/// of an `AF_UNIX` socket without returning a peer parked in an indefinite
/// `recv` — and once the socket is in that state a receive timeout is not
/// honoured either. The worker must therefore stop on the flag alone, with no
/// socket wakeup of any kind: nothing here is ever written, closed or shut
/// down.
#[test]
fn a_retired_reader_stops_without_any_socket_wakeup() {
    let retired = Arc::new(AtomicBool::new(true));
    let (_peer, mut reader) = parked_reader(&retired);

    let mut byte = [0_u8; 1];
    assert_eq!(
        reader.read(&mut byte).unwrap(),
        0,
        "retirement reads as end of stream"
    );
}

/// The readiness wait is a retirement backstop, not an idle policy. The
/// reader is driven until it has actually parked across several waits —
/// observed, not assumed — and only then is a frame written; it must still be
/// served.
#[test]
fn a_live_reader_crosses_its_waits_and_still_serves_the_next_frame() {
    let retired = Arc::new(AtomicBool::new(false));
    let (mut peer, mut reader) = parked_reader(&retired);
    let timeouts = reader.timeouts();

    let served = std::thread::spawn(move || {
        let mut byte = [0_u8; 1];
        let read = reader.read(&mut byte).unwrap();
        (read, byte[0])
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    while timeouts.load(Ordering::Acquire) < 3 {
        assert!(
            Instant::now() < deadline,
            "the reader never parked, so this run proves nothing about retrying"
        );
        std::thread::yield_now();
    }
    peer.write_all(b"f").unwrap();

    assert_eq!(served.join().unwrap(), (1, b'f'));
    assert!(!retired.load(Ordering::Acquire));
}

/// The retained half is what publishes the flag, so shutting it down is what
/// a parked worker observes — including through the ordinary socket wakeup.
#[test]
fn shutting_the_retained_half_down_stops_the_parked_reader() {
    let (_peer, accepted) = std::os::unix::net::UnixStream::pair().unwrap();
    let retained = AcceptedStream::new(accepted.try_clone().unwrap());
    let mut reader = RetiringReader::new(accepted, retained.retirement(), Duration::from_millis(5));
    retained.shutdown().unwrap();

    let mut byte = [0_u8; 1];
    assert_eq!(
        reader.read(&mut byte).unwrap(),
        0,
        "a retired worker stops on its own"
    );
}

/// A finished worker can stay in `ClientWorkers` until the next accept
/// triggers reaping. Its collection handle must not keep the accepted fd
/// open during that interval: a long-lived daemon otherwise accumulates one
/// descriptor for every historical short-lived client.
#[test]
fn worker_completion_closes_the_shared_retirement_descriptor_before_reaping() {
    let (mut peer, accepted) = std::os::unix::net::UnixStream::pair().unwrap();
    let retained = AcceptedStream::new(accepted);
    let completion = retained.clone();

    drop(ShutdownAcceptedStreamOnDrop(Some(completion)));

    let mut byte = [0_u8; 1];
    assert_eq!(peer.read(&mut byte).unwrap(), 0);
    assert!(retained.shutdown().is_ok(), "closing twice is idempotent");
}

#[test]
fn established_client_capacity_reaps_completion_but_refuses_live_workers() {
    let workers = ClientWorkers::new();
    let (mut peer, mut accepted) = std::os::unix::net::UnixStream::pair().unwrap();
    let retained = AcceptedStream::new(accepted.try_clone().unwrap());
    let completion = retained.clone();
    let worker = std::thread::spawn(move || {
        let _completion = ShutdownAcceptedStreamOnDrop(Some(completion));
        let mut byte = [0_u8; 1];
        let _ = accepted.read(&mut byte);
    });
    retain_client_worker(&workers, Ok(retained), worker);

    assert!(!client_connection_capacity_available(&workers, 1));
    peer.write_all(&[1]).unwrap();
    drop(peer);
    while workers.outstanding() != 0 && !client_connection_capacity_available(&workers, 1) {
        std::thread::yield_now();
    }
    assert!(client_connection_capacity_available(&workers, 1));
    assert_eq!(workers.outstanding(), 0);
}

#[test]
fn established_client_capacity_uses_the_process_descriptor_budget() {
    assert_eq!(client_connection_limit_from_nofile(32), 1);
    assert_eq!(client_connection_limit_from_nofile(256), 42);
    assert_eq!(client_connection_limit_from_nofile(2_560), 256);
    assert_eq!(client_connection_limit_from_nofile(u64::MAX), 256);
    assert_eq!(preferred_client_nofile_soft_limit(32, 256), 256);
    assert_eq!(
        preferred_client_nofile_soft_limit(256, 10_240),
        CLIENT_NOFILE_TARGET
    );
    assert_eq!(
        preferred_client_nofile_soft_limit(256, libc::RLIM_INFINITY),
        CLIENT_NOFILE_TARGET
    );
    assert_eq!(preferred_client_nofile_soft_limit(1_024, 10_240), 1_024);
}

#[test]
fn capacity_refusal_is_logged_once_per_saturated_interval() {
    let mut log = CapacityRefusalLog::default();
    assert!(log.should_record(false));
    assert!(!log.should_record(false));
    assert!(!log.should_record(true));
    assert!(log.should_record(false));
}

#[test]
fn periodic_failure_log_records_transitions_and_recovery() {
    let mut log = FailureTransitionLog::default();
    assert_eq!(log.changed(Some("first".into())), Some("first".into()));
    assert_eq!(log.changed(Some("first".into())), None);
    assert_eq!(log.changed(Some("second".into())), Some("second".into()));
    assert_eq!(log.changed(None), None);
    assert_eq!(log.changed(Some("second".into())), Some("second".into()));
}

#[test]
fn endpoint_probe_accepts_a_framed_refusal_but_not_transport_failure() {
    use usagi_core::infrastructure::ipc::ProtocolError;

    assert!(daemon_probe_result_is_reachable(&Ok(())));
    assert!(daemon_probe_result_is_reachable::<()>(&Err(
        ClientError::Protocol(ProtocolError::new(
            ErrorCode::ProtocolMismatch,
            "older daemon"
        ))
    )));
    assert!(!daemon_probe_result_is_reachable::<()>(&Err(
        ClientError::Unavailable("socket closed".into())
    )));
}

#[derive(Clone)]
struct ResponseDeadlineTestClock(Arc<AtomicU64>);

impl MonotonicClock for ResponseDeadlineTestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct ResponseWriteObservation {
    deadlines: Vec<Duration>,
    bytes: Vec<u8>,
}

struct PartialResponseConnection {
    clock: Arc<AtomicU64>,
    observation: Arc<Mutex<ResponseWriteObservation>>,
    max_write: usize,
    advance_ms: u64,
}

impl Read for PartialResponseConnection {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        unreachable!("the response adapter never reads from its writer half")
    }
}

impl Write for PartialResponseConnection {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = bytes.len().min(self.max_write);
        self.observation
            .lock()
            .unwrap()
            .bytes
            .extend_from_slice(&bytes[..written]);
        self.clock.fetch_add(self.advance_ms, Ordering::SeqCst);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl DeadlineConnection for PartialResponseConnection {
    fn set_read_deadline(&mut self, _: Duration) -> std::io::Result<()> {
        unreachable!("the response adapter never arms its writer half for reads")
    }

    fn set_write_deadline(&mut self, timeout: Duration) -> std::io::Result<()> {
        self.observation.lock().unwrap().deadlines.push(timeout);
        Ok(())
    }
}

struct ErrorResponseConnection {
    arm_failure: Option<std::io::ErrorKind>,
    write_fault: Option<std::io::ErrorKind>,
    flush_error: Option<std::io::ErrorKind>,
}

impl Read for ErrorResponseConnection {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        unreachable!("the response adapter never reads from its writer half")
    }
}

impl Write for ErrorResponseConnection {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self.write_fault {
            Some(kind) => Err(std::io::Error::new(kind, "write failed")),
            None => Ok(bytes.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.flush_error {
            Some(kind) => Err(std::io::Error::new(kind, "flush failed")),
            None => Ok(()),
        }
    }
}

impl DeadlineConnection for ErrorResponseConnection {
    fn set_read_deadline(&mut self, _: Duration) -> std::io::Result<()> {
        unreachable!("the response adapter never arms its writer half for reads")
    }

    fn set_write_deadline(&mut self, _: Duration) -> std::io::Result<()> {
        match self.arm_failure {
            Some(kind) => Err(std::io::Error::new(kind, "deadline failed")),
            None => Ok(()),
        }
    }
}

fn error_response_writer(
    deadline_error: Option<std::io::ErrorKind>,
    write_error: Option<std::io::ErrorKind>,
    flush_error: Option<std::io::ErrorKind>,
) -> EstablishedResponseWriter<ResponseDeadlineTestClock, ErrorResponseConnection> {
    EstablishedResponseWriter::new(
        ResponseDeadlineTestClock(Arc::new(AtomicU64::new(0))),
        ErrorResponseConnection {
            arm_failure: deadline_error,
            write_fault: write_error,
            flush_error,
        },
        100,
    )
}

fn response_deadline_writer(
    budget_ms: u64,
    max_write: usize,
    advance_ms: u64,
) -> (
    EstablishedResponseWriter<ResponseDeadlineTestClock, PartialResponseConnection>,
    Arc<Mutex<ResponseWriteObservation>>,
) {
    let clock = Arc::new(AtomicU64::new(0));
    let observation = Arc::new(Mutex::new(ResponseWriteObservation::default()));
    (
        EstablishedResponseWriter::new(
            ResponseDeadlineTestClock(Arc::clone(&clock)),
            PartialResponseConnection {
                clock,
                observation: Arc::clone(&observation),
                max_write,
                advance_ms,
            },
            budget_ms,
        ),
        observation,
    )
}

#[test]
fn established_response_partial_writes_share_one_absolute_frame_deadline() {
    let (mut writer, observation) = response_deadline_writer(100, 2, 20);
    let frame = [0, 0, 0, 4, b't', b'e', b's', b't'];

    writer.write_all(&frame).unwrap();
    writer.write_all(&frame).unwrap();

    let observation = observation.lock().unwrap();
    assert_eq!(observation.bytes, [frame, frame].concat());
    assert_eq!(
        observation.deadlines,
        [100, 80, 60, 40, 100, 80, 60, 40].map(Duration::from_millis)
    );
}

#[test]
fn established_response_deadline_expires_despite_partial_progress() {
    let (mut writer, observation) = response_deadline_writer(100, 2, 100);
    let frame = [0, 0, 0, 4, b't', b'e', b's', b't'];

    let error = writer.write_all(&frame).unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    let observation = observation.lock().unwrap();
    assert_eq!(observation.bytes, frame[..2]);
    assert_eq!(observation.deadlines, [Duration::from_millis(100)]);
}

#[test]
fn established_response_writer_covers_frame_and_transport_boundaries() {
    let mut progress = ResponseFrameProgress::default();
    progress.observe(&[0, 0, 0, 0, 0, 0, 0, 1, b'x']);
    assert!(progress.at_frame_start());

    let (mut writer, observation) = response_deadline_writer(100, 2, 0);
    assert_eq!(writer.write(&[]).unwrap(), 0);
    writer.flush().unwrap();
    assert!(observation.lock().unwrap().deadlines.is_empty());
    assert_eq!(writer.write(&[0]).unwrap(), 1);
    writer.flush().unwrap();
    assert_eq!(
        observation.lock().unwrap().deadlines,
        [Duration::from_millis(100), Duration::from_millis(100)]
    );

    let mut blocked = error_response_writer(None, Some(std::io::ErrorKind::WouldBlock), None);
    assert_eq!(
        blocked.write(&[0]).unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    let mut broken = error_response_writer(None, Some(std::io::ErrorKind::BrokenPipe), None);
    assert_eq!(
        broken.write(&[0]).unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
    let mut unarmable =
        error_response_writer(Some(std::io::ErrorKind::PermissionDenied), None, None);
    assert_eq!(
        unarmable.write(&[0]).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    let mut blocked_flush = error_response_writer(None, None, Some(std::io::ErrorKind::WouldBlock));
    assert_eq!(
        blocked_flush.flush().unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    let mut broken_flush = error_response_writer(None, None, Some(std::io::ErrorKind::BrokenPipe));
    assert_eq!(
        broken_flush.flush().unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
}

#[test]
fn census_fence_tracks_every_admitted_connection_until_disconnect() {
    use usagi_daemon::presentation::ipc::ConnectionFence as _;

    let (cleanup, inbox) = connection_cleanup_channel();
    let connection = ConnectionId::new();
    let fence = CensusConnectionFence {
        inner: &usagi_daemon::presentation::ipc::UnfencedConnection,
        cleanup: cleanup.clone(),
        peer_pid: 41,
    };

    fence
        .admitted(connection, &fence_client_hello(Vec::new()))
        .unwrap();
    assert_eq!(
        inbox.live(),
        (BTreeSet::from([connection]), BTreeSet::from([41]))
    );
    assert!(fence.admit(&serde_json::Value::Null).unwrap().is_none());
    fence.disconnected(connection);
    assert_eq!(inbox.live(), (BTreeSet::new(), BTreeSet::new()));

    // A repeated disconnect is a no-op, and a dead cleanup worker never
    // turns disconnection into a producer failure or a wait.
    fence.disconnected(connection);
    drop(inbox);
    let after_worker = ConnectionId::new();
    cleanup.connected(after_worker, 42);
    cleanup.disconnected(after_worker);
}

#[test]
fn connection_cleanup_worker_converges_to_the_final_live_census() {
    let (disconnected, disconnects) = connection_cleanup_channel();
    let cleaned = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&cleaned);
    let first = ConnectionId::new();
    let second = ConnectionId::new();

    disconnected.connected(first, 41);
    disconnected.connected(second, 42);
    disconnected.disconnected(first);
    disconnected.disconnected(second);
    drop(disconnected);
    let worker = start_connection_cleanup_worker_with(disconnects, move |inbox| {
        observed.lock().unwrap().push(inbox.live());
    })
    .unwrap();
    worker.join().unwrap();

    assert_eq!(
        *cleaned.lock().unwrap(),
        vec![(BTreeSet::new(), BTreeSet::new())]
    );
}

#[test]
fn connection_cleanup_submission_does_not_wait_for_a_stalled_consumer() {
    let (disconnected, disconnects) = connection_cleanup_channel();
    let (entered, observed_entry) = mpsc::sync_channel(0);
    let (release, wait_for_release) = mpsc::sync_channel(0);
    let mut first_batch = true;
    let worker = start_connection_cleanup_worker_with(disconnects, move |_| {
        if first_batch {
            entered.send(()).unwrap();
            wait_for_release.recv().unwrap();
            first_batch = false;
        }
    })
    .unwrap();
    let initial = ConnectionId::new();
    disconnected.connected(initial, 40);
    disconnected.disconnected(initial);
    observed_entry.recv_timeout(Duration::from_secs(1)).unwrap();

    let producer = disconnected.clone();
    let (completion_notice, submission_complete) = mpsc::channel();
    let producer_worker = std::thread::spawn(move || {
        for _ in 0..=client_connection_limit() {
            let connection = ConnectionId::new();
            producer.connected(connection, 40);
            producer.disconnected(connection);
        }
        completion_notice.send(()).unwrap();
    });
    let submission = submission_complete.recv_timeout(Duration::from_secs(1));

    release.send(()).unwrap();
    producer_worker.join().unwrap();
    drop(disconnected);
    worker.join().unwrap();
    assert!(
        submission.is_ok(),
        "disconnect notification inherited cleanup backpressure"
    );
}

#[test]
fn inbox_query_errors_preserve_client_faults_and_hide_store_failures() {
    use usagi_core::infrastructure::ipc::ErrorCode;

    let invalid = map_inbox_query_error(&anyhow::anyhow!(
        "dispatch inbox cursor expired: earliest retained sequence is 7"
    ));
    assert_eq!(invalid.code, ErrorCode::InvalidArgument);
    assert!(invalid.message.contains("earliest retained sequence is 7"));
    for message in [
        "dispatch inbox ACK cursor is outside the published sequence range",
        "dispatch inbox page limit must be 1..=100",
    ] {
        assert_eq!(
            map_inbox_query_error(&anyhow::anyhow!(message)).code,
            ErrorCode::InvalidArgument
        );
    }
    let unavailable = map_inbox_query_error(&anyhow::anyhow!("disk secret"));
    assert_eq!(unavailable.code, ErrorCode::Unavailable);
    assert_eq!(unavailable.message, "dispatch inbox is unavailable");
}
