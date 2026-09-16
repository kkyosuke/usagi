//! worker の振る舞いを固定するテスト。

use super::*;

#[test]
fn pending_worker_stop_projects_and_checks_every_exact_worker_fence() {
    let workspace = WorkspaceId::new();
    let worker = delegated_worker(workspace);
    let profile = AgentProfileId::new("claude").unwrap();
    let agent_id = AgentId::new();
    let mut pending = root_pending_stop(OperationId::new(), workspace, SupervisorRunId::new());
    pending.requires_session = true;
    pending.worker_session_id = worker.session_id;
    pending.worker_runtime_id = Some(worker.agent_runtime_id);
    pending.worker_profile_id = Some(profile.clone());
    pending.worker_agent_id = Some(agent_id);
    pending.worker_semantic_digest = Some("semantic".into());

    assert!(pending.matches_worker_scope(&worker));
    assert_eq!(pending.worker_profile_id(), Some(&profile));
    assert_eq!(pending.worker_agent_id(), Some(agent_id));
    assert_eq!(pending.worker_semantic_digest(), Some("semantic"));
    assert_eq!(
        pending.provenance(&worker).unwrap().dispatch_run_id,
        pending.operation_id()
    );

    let wrong_session = delegated_worker(workspace);
    assert!(!pending.matches_worker_scope(&wrong_session));
    let wrong_workspace = delegated_worker(WorkspaceId::new());
    assert!(!pending.matches_worker_scope(&wrong_workspace));
    assert!(pending.provenance(&wrong_workspace).is_err());

    let mut terminal = unbound_goal_run(Some(workspace));
    terminal
        .tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .required_artifact_contract = NO_ARTIFACT_CONTRACT;
    let mut caller_reservation = start_reservation(terminal.supervisor_run_id);
    caller_reservation.caller_dispatch_run_id = Some(OperationId::new());
    assert!(has_unbound_root_worker(
        &terminal,
        Some(&caller_reservation)
    ));
    assert!(!has_unbound_root_worker(&terminal, None));
}

#[test]
fn aborted_child_stop_uses_its_reserved_parent_fence_after_parent_retry() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let parent_operation = OperationId::new();
    scheduler
        .dispatch
        .upsert_run(DispatchRun {
            run_id: parent_operation,
            agent_id: AgentId::new(),
            prompt: "parent".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    persist_root_dispatch_agent(&scheduler, workspace, parent_operation);
    let run = scheduler
        .start_for_workspace_root_dispatch(
            "goal-composer",
            workspace,
            &parent_operation.to_string(),
            goal("parent work"),
            None,
            &root_worker(workspace),
            now(),
        )
        .unwrap();
    let child_operation = OperationId::new();
    scheduler
        .reserve_delegated_dispatch(
            parent_operation,
            &child_operation.to_string(),
            "child work",
            now(),
        )
        .unwrap()
        .unwrap();

    // A retry advances the live parent generation and replaces current
    // provenance. Abort cleanup must still use the child's immutable
    // reservation fence instead of the parent's new operation.
    let mut retrying = scheduler
        .supervisor
        .load(run.supervisor_run_id)
        .unwrap()
        .unwrap();
    let root_id = TaskId::new("root").unwrap();
    let retry_operation = OperationId::new();
    let root = retrying.tasks.get_mut(&root_id).unwrap();
    root.generation = 2;
    root.assigned_dispatch_run = Some(retry_operation);
    root.state = TaskState::Dispatched;
    let mut retry_provenance =
        provenance(retrying.supervisor_run_id, &root_id, None, retry_operation);
    retry_provenance.generation = 2;
    retrying.provenance.insert(root_id, retry_provenance);
    scheduler.supervisor.initialize(&retrying).unwrap();
    scheduler
        .control_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: run.supervisor_run_id,
                reason: "cancel after parent retry".into(),
            },
            now(),
        )
        .unwrap();

    let stops = scheduler
        .pending_worker_stops_for_run(run.supervisor_run_id)
        .unwrap();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].operation_id, child_operation);
    assert_eq!(stops[0].parent_dispatch_run, Some(parent_operation));
}

#[test]
#[allow(clippy::too_many_lines)] // Exercise durable reserve/replay/bind with the exact cross-runtime peer fence.
fn supervised_peer_handoff_preserves_cross_runtime_fences_across_restart_and_binding() {
    let (temp, scheduler, workspace, parent_operation, worker, peer) = supervised_peer_fixture();
    let operation = OperationId::new().to_string();
    let reserved = scheduler
        .reserve_peer_handoff(
            parent_operation,
            &operation,
            "review",
            &peer,
            "worker",
            now(),
        )
        .unwrap()
        .unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let replay = scheduler
        .reserve_peer_handoff(
            parent_operation,
            &operation,
            "review",
            &peer,
            "worker",
            now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(reserved.prompt, replay.prompt);
    let child_operation = OperationId::parse(&operation).unwrap();
    let fence = scheduler
        .supervision_fence(child_operation)
        .unwrap()
        .unwrap();
    let run = scheduler
        .supervisor
        .load(fence.supervisor_run_id)
        .unwrap()
        .unwrap();
    let task = &run.tasks[&fence.task_id];
    assert_eq!(task.promotion_worker_profile_id, Some(peer.runtime.clone()));
    assert_eq!(task.promotion_worker_agent_id, Some(peer.agent_id));
    assert_eq!(task.promotion_worker_session_id, peer.session_id);
    assert_eq!(task.promotion_parent_dispatch_run, Some(parent_operation));

    let mut different_runtime = peer.clone();
    different_runtime.runtime = AgentProfileId::new("codex").unwrap();
    let mut different_agent = peer.clone();
    different_agent.agent_id = AgentId::new();
    for conflicting in [&different_runtime, &different_agent] {
        assert!(
            scheduler
                .reserve_peer_handoff(
                    parent_operation,
                    &operation,
                    "review",
                    conflicting,
                    "worker",
                    now()
                )
                .is_err()
        );
    }
    assert!(
        scheduler
            .reserve_peer_handoff(
                parent_operation,
                &operation,
                "changed",
                &peer,
                "worker",
                now()
            )
            .is_err()
    );
    assert!(
        scheduler
            .reserve_peer_handoff(
                parent_operation,
                &operation,
                "review",
                &peer,
                "renamed",
                now()
            )
            .is_err()
    );
    let parent_id = scheduler
        .dispatch
        .run(parent_operation)
        .unwrap()
        .unwrap()
        .agent_id;
    let mut admitted = peer.clone();
    admitted.status = AgentStatus::Running;
    admitted.current_run = Some(child_operation);
    scheduler
        .dispatch
        .reserve_admission_for_workspace(
            workspace,
            admitted.clone(),
            DispatchRun {
                run_id: child_operation,
                agent_id: peer.agent_id,
                prompt: reserved.prompt.clone(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            },
            DispatchBinding {
                run_id: child_operation,
                caller: CallerRef {
                    session_id: peer.session_id,
                    agent_id: parent_id,
                },
                worker: WorkerRef {
                    session_id: peer.session_id,
                    agent_id: peer.agent_id,
                },
            },
            AgentAdmissionReservation {
                operation_id: child_operation,
                semantic_key: usagi_core::infrastructure::ipc::agent_dispatch_semantic_key(
                    "worker",
                    peer.agent_id,
                    &reserved.prompt,
                ),
                credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
            },
        )
        .unwrap();
    let mut wrong_profile = admitted.clone();
    wrong_profile.runtime = AgentProfileId::new("codex").unwrap();
    scheduler
        .dispatch
        .upsert_agent(workspace, wrong_profile)
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_delegated_dispatch(&operation, &worker, now())
            .is_err()
    );
    scheduler
        .dispatch
        .upsert_agent(workspace, admitted)
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_delegated_dispatch(&operation, &delegated_worker(workspace), now())
            .is_err()
    );
    scheduler
        .bind_reserved_delegated_dispatch(&operation, &worker, now())
        .unwrap()
        .unwrap();
    assert!(
        scheduler
            .reserve_peer_handoff(
                parent_operation,
                &operation,
                "review",
                &peer,
                "worker",
                now()
            )
            .unwrap()
            .is_some()
    );
}

#[test]
fn supervised_peer_handoff_rejects_self_and_other_sessions_without_relaxing_delegation() {
    let (_temp, scheduler, _workspace, parent_operation, _worker, peer) = supervised_peer_fixture();
    let parent = scheduler
        .dispatch
        .agent(
            scheduler
                .dispatch
                .run(parent_operation)
                .unwrap()
                .unwrap()
                .agent_id,
        )
        .unwrap()
        .unwrap();
    let mut outside = peer.clone();
    outside.session_id = Some(SessionId::new());
    let mut unmanaged = peer.clone();
    unmanaged.session_id = None;
    for invalid in [&parent, &outside, &unmanaged] {
        assert!(
            scheduler
                .reserve_peer_handoff(
                    parent_operation,
                    &OperationId::new().to_string(),
                    "review",
                    invalid,
                    "worker",
                    now()
                )
                .unwrap_err()
                .to_string()
                .contains("distinct Agent")
        );
    }
    assert!(
        scheduler
            .reserve_delegated_dispatch_for_session(
                parent_operation,
                &OperationId::new().to_string(),
                "review",
                peer.session_id.unwrap(),
                &peer,
                "worker",
                now()
            )
            .unwrap_err()
            .to_string()
            .contains("outside its Supervisor scope")
    );
    assert!(
        scheduler
            .reserve_peer_handoff(
                OperationId::new(),
                &OperationId::new().to_string(),
                "review",
                &peer,
                "worker",
                now()
            )
            .unwrap()
            .is_none()
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One durable fixture proves report capture, prompt inheritance, restart replay, and classic isolation together.
fn completed_child_handoff_is_durable_and_inherited_by_later_delegations() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let root_operation = OperationId::new();
    scheduler
        .dispatch
        .upsert_run(DispatchRun {
            run_id: root_operation,
            agent_id: AgentId::new(),
            prompt: "root".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    persist_root_dispatch_agent(&scheduler, workspace, root_operation);
    let root = scheduler
        .start_for_workspace_root_dispatch(
            "goal-composer",
            workspace,
            &root_operation.to_string(),
            goal("ship the authentication change"),
            None,
            &root_worker(workspace),
            now(),
        )
        .unwrap();

    let first_operation = OperationId::new();
    let first = scheduler
        .reserve_delegated_dispatch(
            root_operation,
            &first_operation.to_string(),
            "inspect the authentication flow",
            now(),
        )
        .unwrap()
        .unwrap();
    let first_prompt = first.prompt.clone();
    assert!(first.prompt.contains("ship the authentication change"));
    assert!(first.prompt.contains("inspect the authentication flow"));
    assert!(
        first
            .prompt
            .contains("none recorded before this delegation")
    );

    let caller = CallerRef {
        session_id: None,
        agent_id: AgentId::new(),
    };
    scheduler
        .dispatch
        .upsert_run(DispatchRun {
            run_id: first_operation,
            agent_id: AgentId::new(),
            prompt: first.prompt,
            started_at: now(),
            ended_at: Some(now()),
            status: RunStatus::Completed,
        })
        .unwrap();
    scheduler
        .dispatch
        .upsert_binding(DispatchBinding {
            run_id: first_operation,
            caller: caller.clone(),
            worker: WorkerRef {
                session_id: Some(SessionId::new()),
                agent_id: AgentId::new(),
            },
        })
        .unwrap();
    scheduler
        .dispatch
        .append_inbox(
            &caller,
            InboxMessage {
                run_id: first_operation,
                from: WorkerRef {
                    session_id: Some(SessionId::new()),
                    agent_id: AgentId::new(),
                },
                kind: InboxKind::Completed,
                summary: "Mapped OAuth callbacks\nwithout copying a transcript\u{202e}".into(),
                result: Some(StructuredResult {
                    pr: Some("https://github.com/acme/repo/pull/42".into()),
                    commits: vec!["abc123".into()],
                    changed_files: vec!["src/auth.rs".into()],
                    verification: Some("targeted tests pass".into()),
                }),
                created_at: now(),
                read: false,
            },
        )
        .unwrap();
    scheduler
        .attach_delegated_dispatch(
            root_operation,
            &first_operation.to_string(),
            "inspect the authentication flow".into(),
            &delegated_worker(workspace),
            now(),
        )
        .unwrap();
    scheduler
        .tick(root.supervisor_run_id, now(), &mut Waker::default())
        .unwrap();

    let stored = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.handoff_context.len(), 1);
    let terminal_replay = scheduler
        .reserve_delegated_dispatch(
            root_operation,
            &first_operation.to_string(),
            "inspect the authentication flow",
            now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(terminal_replay.prompt, first_prompt);
    assert_eq!(
        stored.handoff_context[0].summary,
        "Mapped OAuth callbacks without copying a transcript"
    );
    assert!(
        stored.handoff_context[0]
            .artifacts
            .as_deref()
            .unwrap()
            .contains("src/auth.rs")
    );

    let second_operation = OperationId::new();
    let second = scheduler
        .reserve_delegated_dispatch(
            root_operation,
            &second_operation.to_string(),
            "implement the callback handler",
            now(),
        )
        .unwrap()
        .unwrap();
    assert!(second.prompt.contains("ship the authentication change"));
    assert!(
        second
            .prompt
            .contains("Mapped OAuth callbacks without copying a transcript")
    );
    assert!(
        second
            .prompt
            .contains("https://github.com/acme/repo/pull/42")
    );
    assert!(second.prompt.contains("abc123"));
    assert!(second.prompt.contains("src/auth.rs"));
    assert!(second.prompt.contains("implement the callback handler"));
    assert!(!second.prompt.contains('\u{202e}'));
    let suffix = delegated_task_suffix(second_operation, "implement the callback handler");
    assert!(second.prompt.len() <= MAX_HANDOFF_PROMPT_BYTES + suffix.len());

    let restarted = SupervisorRuntime::new(temp.path());
    let replay = restarted
        .reserve_delegated_dispatch(
            root_operation,
            &second_operation.to_string(),
            "implement the callback handler",
            now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(replay.prompt, second.prompt);
    assert!(
        restarted
            .reserve_delegated_dispatch(
                OperationId::new(),
                &OperationId::new().to_string(),
                "classic delegation",
                now(),
            )
            .unwrap()
            .is_none()
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One lifecycle fixture proves workspace fencing, restart replay, conflicts, and stop recovery selection together.
fn workspace_control_is_durable_scoped_and_projects_exact_stop_obligations() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let workspace = WorkspaceId::new();
    let dispatch_run = OperationId::new();
    dispatch
        .upsert_run(DispatchRun {
            run_id: dispatch_run,
            agent_id: AgentId::new(),
            prompt: String::new(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    persist_root_dispatch_agent(&runtime, workspace, dispatch_run);
    let worker = root_worker(workspace);
    let run = runtime
        .start_for_workspace_root_dispatch(
            "goal-composer",
            workspace,
            &dispatch_run.to_string(),
            goal("finish the goal"),
            Some("standard".into()),
            &worker,
            now(),
        )
        .unwrap();
    let operation = OperationId::new();
    let command = SupervisorWorkspaceCommand::Cancel {
        supervisor_run_id: run.supervisor_run_id,
        reason: "operator cancelled".into(),
    };
    let cancelled = runtime
        .control_for_workspace(workspace, operation, &command, now())
        .unwrap();
    assert_eq!(cancelled.state, SupervisorRunState::Cancelled);
    assert_eq!(cancelled.provenance.len(), 1);

    let restarted = SupervisorRuntime::new(temp.path());
    assert_eq!(
        restarted
            .control_for_workspace(
                workspace,
                operation,
                &command,
                now() + chrono::Duration::minutes(1),
            )
            .unwrap(),
        cancelled
    );
    assert!(
        restarted
            .control_for_workspace(
                workspace,
                operation,
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: run.supervisor_run_id,
                    reason: "different reason".into(),
                },
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("conflicts with its reservation")
    );
    assert!(
        restarted
            .control_for_workspace(WorkspaceId::new(), OperationId::new(), &command, now())
            .unwrap_err()
            .to_string()
            .contains("does not belong")
    );

    let obligations = restarted.worker_stop_obligations().unwrap();
    assert_eq!(obligations.len(), 1);
    assert_eq!(obligations[0].0, workspace);
    assert_eq!(obligations[0].1.worker_agent_id, worker.agent_runtime_id);
    assert_eq!(
        obligations[0].1.worker_worktree_id,
        worker.terminal.worktree_id
    );
    assert_eq!(
        restarted
            .worker_stop_obligations_for_run(run.supervisor_run_id)
            .unwrap(),
        obligations
    );
    assert!(
        restarted
            .worker_stop_obligations_for_run(SupervisorRunId::new())
            .unwrap()
            .is_empty()
    );

    let mut prior_attempt = restarted
        .supervisor
        .load(run.supervisor_run_id)
        .unwrap()
        .unwrap();
    let root = prior_attempt
        .tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap();
    root.generation += 1;
    root.assigned_dispatch_run = None;
    json_file::write_atomic(
        restarted
            .supervisor
            .snapshot_path(run.supervisor_run_id)
            .parent()
            .unwrap(),
        &restarted.supervisor.snapshot_path(run.supervisor_run_id),
        &prior_attempt,
    )
    .unwrap();
    assert!(restarted.worker_stop_obligations().unwrap().is_empty());

    let mut legacy = SupervisorRun::new(
        "legacy".into(),
        "task".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    legacy.state = SupervisorRunState::Cancelled;
    legacy.terminal_at = Some(now());
    restarted.supervisor.initialize(&legacy).unwrap();
    assert!(restarted.worker_stop_obligations().unwrap().is_empty());
}

#[test]
#[allow(clippy::too_many_lines)] // One corruption matrix proves every fail-closed unbound-worker recovery boundary.
fn pending_worker_stop_recovery_rejects_every_corrupt_reservation_shape() {
    let workspace = WorkspaceId::new();

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let mut state = RuntimeState::default();
    for operation in [OperationId::new(), OperationId::new()] {
        let run = unbound_goal_run(Some(workspace));
        runtime.supervisor.initialize(&run).unwrap();
        state.starts.insert(
            operation.to_string(),
            start_reservation(run.supervisor_run_id),
        );
    }
    runtime.save_state(&state).unwrap();
    assert_eq!(runtime.pending_worker_stops().unwrap().len(), 2);

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let running = SupervisorRun::new(
        "caller".into(),
        "task".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    runtime.supervisor.initialize(&running).unwrap();
    let mut state = RuntimeState::default();
    state.starts.insert(
        OperationId::new().to_string(),
        start_reservation(SupervisorRunId::new()),
    );
    state.starts.insert(
        OperationId::new().to_string(),
        start_reservation(running.supervisor_run_id),
    );
    runtime.save_state(&state).unwrap();
    assert!(runtime.pending_worker_stops().unwrap().is_empty());

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let unscoped = unbound_goal_run(None);
    let missing_root = aborted_run(Some(workspace));
    runtime.supervisor.initialize(&unscoped).unwrap();
    runtime.supervisor.initialize(&missing_root).unwrap();
    let mut state = RuntimeState::default();
    state.starts.insert(
        OperationId::new().to_string(),
        start_reservation(unscoped.supervisor_run_id),
    );
    state.starts.insert(
        OperationId::new().to_string(),
        start_reservation(missing_root.supervisor_run_id),
    );
    runtime.save_state(&state).unwrap();
    assert!(runtime.pending_worker_stops().unwrap().is_empty());

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let root = unbound_goal_run(Some(workspace));
    runtime.supervisor.initialize(&root).unwrap();
    let mut state = RuntimeState::default();
    state.starts.insert(
        "invalid-operation".into(),
        start_reservation(root.supervisor_run_id),
    );
    runtime.save_state(&state).unwrap();
    assert!(
        runtime
            .pending_worker_stops()
            .unwrap_err()
            .to_string()
            .contains("operation is invalid")
    );

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let mut invalid_operation = aborted_run(Some(workspace));
    let invalid_task = task(
        invalid_operation.supervisor_run_id,
        "delegated-invalid-operation",
        None,
    );
    invalid_operation
        .tasks
        .insert(invalid_task.task_id.clone(), invalid_task);
    let operation = OperationId::new();
    let mut wrong_digest = aborted_run(Some(workspace));
    let mut wrong_digest_task = task(
        wrong_digest.supervisor_run_id,
        &format!("{DELEGATED_TASK_PREFIX}{operation}"),
        None,
    );
    wrong_digest_task.instruction_digest = "wrong digest".into();
    wrong_digest
        .tasks
        .insert(wrong_digest_task.task_id.clone(), wrong_digest_task);
    runtime.supervisor.initialize(&invalid_operation).unwrap();
    runtime.supervisor.initialize(&wrong_digest).unwrap();
    assert!(runtime.pending_worker_stops().unwrap().is_empty());

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let operation = OperationId::new();
    let mut missing_parent = aborted_run(Some(workspace));
    let mut child = task(
        missing_parent.supervisor_run_id,
        &format!("{DELEGATED_TASK_PREFIX}{operation}"),
        None,
    );
    child.instruction_digest = delegated_task_digest(operation);
    child.promotion_reserved_at = Some(now());
    child.state = TaskState::Cancelled;
    missing_parent.tasks.insert(child.task_id.clone(), child);
    runtime.supervisor.initialize(&missing_parent).unwrap();
    assert!(
        runtime
            .pending_worker_stops()
            .unwrap_err()
            .to_string()
            .contains("has no parent task")
    );

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let operation = OperationId::new();
    let mut missing_provenance = aborted_run(Some(workspace));
    let mut child = task(
        missing_provenance.supervisor_run_id,
        &format!("{DELEGATED_TASK_PREFIX}{operation}"),
        Some("parent"),
    );
    child.instruction_digest = delegated_task_digest(operation);
    child.promotion_reserved_at = Some(now());
    child.state = TaskState::Cancelled;
    missing_provenance
        .tasks
        .insert(child.task_id.clone(), child);
    runtime.supervisor.initialize(&missing_provenance).unwrap();
    assert!(
        runtime
            .pending_worker_stops()
            .unwrap_err()
            .to_string()
            .contains("parent provenance is missing")
    );

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let operation = OperationId::new();
    let root = unbound_goal_run(Some(workspace));
    runtime.supervisor.initialize(&root).unwrap();
    let mut delegated = aborted_run(Some(workspace));
    let parent_id = TaskId::new("parent").unwrap();
    let parent_dispatch = OperationId::new();
    let parent = task(delegated.supervisor_run_id, "parent", None);
    let mut child = task(
        delegated.supervisor_run_id,
        &format!("{DELEGATED_TASK_PREFIX}{operation}"),
        Some("parent"),
    );
    child.instruction_digest = delegated_task_digest(operation);
    child.promotion_reserved_at = Some(now());
    child.state = TaskState::Cancelled;
    delegated.tasks.insert(parent_id.clone(), parent);
    delegated.tasks.insert(child.task_id.clone(), child);
    delegated.provenance.insert(
        parent_id.clone(),
        provenance(
            delegated.supervisor_run_id,
            &parent_id,
            None,
            parent_dispatch,
        ),
    );
    runtime.supervisor.initialize(&delegated).unwrap();
    let mut state = RuntimeState::default();
    state.starts.insert(
        operation.to_string(),
        start_reservation(root.supervisor_run_id),
    );
    runtime.save_state(&state).unwrap();
    assert!(
        runtime
            .pending_worker_stops()
            .unwrap_err()
            .to_string()
            .contains("multiple aborted supervisor reservations")
    );

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let caller_dispatch = OperationId::new();
    let caller_root = unbound_goal_run(Some(workspace));
    runtime.supervisor.initialize(&caller_root).unwrap();
    let mut state = RuntimeState::default();
    state.starts.insert(
        OperationId::new().to_string(),
        caller_start_reservation(caller_root.supervisor_run_id, workspace, caller_dispatch),
    );
    runtime.save_state(&state).unwrap();
    assert!(
        runtime
            .pending_worker_stops()
            .unwrap_err()
            .to_string()
            .contains("caller root reservation is malformed")
    );

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let caller_dispatch = OperationId::new();
    let mut caller_root = unbound_goal_run(Some(workspace));
    caller_root
        .tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .required_artifact_contract = NO_ARTIFACT_CONTRACT;
    runtime.supervisor.initialize(&caller_root).unwrap();
    let mut state = RuntimeState::default();
    state.starts.insert(
        OperationId::new().to_string(),
        caller_start_reservation(caller_root.supervisor_run_id, workspace, caller_dispatch),
    );
    runtime.save_state(&state).unwrap();
    let stops = runtime.pending_worker_stops().unwrap();
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].operation_id(), caller_dispatch);

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let mut non_goal = unbound_goal_run(Some(workspace));
    non_goal
        .tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .required_artifact_contract = NO_ARTIFACT_CONTRACT;
    runtime.supervisor.initialize(&non_goal).unwrap();
    let mut state = RuntimeState::default();
    state.starts.insert(
        OperationId::new().to_string(),
        start_reservation(non_goal.supervisor_run_id),
    );
    runtime.save_state(&state).unwrap();
    assert!(runtime.pending_worker_stops().unwrap().is_empty());

    let operation = OperationId::new();
    let mut live = SupervisorRun::new(
        "caller".into(),
        "root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    live.workspace_id = Some(workspace);
    live.state = SupervisorRunState::Running;
    let parent = task(live.supervisor_run_id, "parent", None);
    let mut pending_child = task(
        live.supervisor_run_id,
        &format!("{DELEGATED_TASK_PREFIX}{operation}"),
        Some("parent"),
    );
    pending_child.instruction_digest = delegated_task_digest(operation);
    pending_child.promotion_reserved_at = Some(now());
    pending_child.promotion_parent_dispatch_run = Some(OperationId::new());
    live.tasks.insert(parent.task_id.clone(), parent);
    live.tasks
        .insert(pending_child.task_id.clone(), pending_child.clone());
    runtime.supervisor.initialize(&live).unwrap();
    assert!(runtime.pending_worker_stops().unwrap().is_empty());
    pending_child.state = TaskState::Cancelled;
    pending_child.generation = 2;
    live.tasks
        .insert(pending_child.task_id.clone(), pending_child);
    runtime.supervisor.initialize(&live).unwrap();
    assert!(
        runtime
            .pending_worker_stops()
            .unwrap_err()
            .to_string()
            .contains("reservation fence is stale")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Forged acknowledgements share one matrix so no recovery fence is tested in isolation.
fn pending_worker_stop_acknowledgement_is_exact_and_idempotent() {
    let workspace = WorkspaceId::new();
    let operation = OperationId::new();
    let stop = root_pending_stop(operation, workspace, SupervisorRunId::new());

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let mut delegated = stop.clone();
    delegated.parent_task_id = Some(TaskId::new("parent").unwrap());
    runtime
        .acknowledge_pending_worker_stops(&[delegated])
        .unwrap();
    assert!(
        runtime
            .acknowledge_pending_worker_stops(std::slice::from_ref(&stop))
            .unwrap_err()
            .to_string()
            .contains("reservation disappeared")
    );

    let mut state = RuntimeState::default();
    state.expired_starts.insert(&operation.to_string());
    runtime.save_state(&state).unwrap();
    runtime
        .acknowledge_pending_worker_stops(std::slice::from_ref(&stop))
        .unwrap();

    let owned_run = SupervisorRunId::new();
    let mut state = RuntimeState::default();
    state
        .starts
        .insert(operation.to_string(), start_reservation(owned_run));
    runtime.save_state(&state).unwrap();
    assert!(
        runtime
            .acknowledge_pending_worker_stops(std::slice::from_ref(&stop))
            .unwrap_err()
            .to_string()
            .contains("changed run ownership")
    );

    let missing_run = root_pending_stop(operation, workspace, owned_run);
    assert!(
        runtime
            .acknowledge_pending_worker_stops(std::slice::from_ref(&missing_run))
            .unwrap_err()
            .to_string()
            .contains("run disappeared")
    );

    let stale_run = aborted_run(Some(workspace));
    runtime.supervisor.initialize(&stale_run).unwrap();
    let stale_stop = root_pending_stop(operation, workspace, stale_run.supervisor_run_id);
    let mut state = RuntimeState::default();
    state.starts.insert(
        operation.to_string(),
        start_reservation(stale_run.supervisor_run_id),
    );
    runtime.save_state(&state).unwrap();
    assert!(
        runtime
            .acknowledge_pending_worker_stops(&[stale_stop])
            .unwrap_err()
            .to_string()
            .contains("acknowledgement is stale")
    );

    let caller_temp = tempfile::tempdir().unwrap();
    let caller_runtime = SupervisorRuntime::new(caller_temp.path());
    let mut caller_run = aborted_run(Some(workspace));
    let mut root = task(caller_run.supervisor_run_id, "root", None);
    root.state = TaskState::Cancelled;
    caller_run.tasks.insert(root.task_id.clone(), root);
    caller_runtime.supervisor.initialize(&caller_run).unwrap();
    let caller_operation = OperationId::new();
    let caller_stop = root_pending_stop(caller_operation, workspace, caller_run.supervisor_run_id);
    let mut caller_state = RuntimeState::default();
    caller_state.starts.insert(
        OperationId::new().to_string(),
        caller_start_reservation(caller_run.supervisor_run_id, workspace, caller_operation),
    );
    caller_runtime.save_state(&caller_state).unwrap();
    caller_runtime
        .acknowledge_pending_worker_stops(std::slice::from_ref(&caller_stop))
        .unwrap();

    let ambiguous_temp = tempfile::tempdir().unwrap();
    let ambiguous = SupervisorRuntime::new(ambiguous_temp.path());
    ambiguous.supervisor.initialize(&caller_run).unwrap();
    let mut ambiguous_state = RuntimeState::default();
    for _ in 0..2 {
        ambiguous_state.starts.insert(
            OperationId::new().to_string(),
            caller_start_reservation(caller_run.supervisor_run_id, workspace, caller_operation),
        );
    }
    ambiguous.save_state(&ambiguous_state).unwrap();
    assert!(
        ambiguous
            .acknowledge_pending_worker_stops(std::slice::from_ref(&caller_stop))
            .unwrap_err()
            .to_string()
            .contains("ambiguous reservations")
    );

    let moved_temp = tempfile::tempdir().unwrap();
    let moved = SupervisorRuntime::new(moved_temp.path());
    let mut moved_state = RuntimeState::default();
    moved_state.starts.insert(
        OperationId::new().to_string(),
        caller_start_reservation(SupervisorRunId::new(), workspace, caller_operation),
    );
    moved.save_state(&moved_state).unwrap();
    assert!(
        moved
            .acknowledge_pending_worker_stops(&[caller_stop])
            .unwrap_err()
            .to_string()
            .contains("changed run ownership")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One matrix covers ordering plus each corrupt durable provenance fence.
fn worker_stop_obligations_sort_exact_workers_and_reject_corrupt_provenance() {
    let workspace = WorkspaceId::new();
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let mut running = SupervisorRun::new(
        "caller".into(),
        "running".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    running.workspace_id = Some(workspace);
    runtime.supervisor.initialize(&running).unwrap();
    for _ in 0..2 {
        let mut run = aborted_run(Some(workspace));
        let task_id = TaskId::new("worker").unwrap();
        let dispatch = OperationId::new();
        let mut worker = task(run.supervisor_run_id, "worker", None);
        worker.assigned_dispatch_run = Some(dispatch);
        run.tasks.insert(task_id.clone(), worker);
        run.provenance.insert(
            task_id.clone(),
            provenance(run.supervisor_run_id, &task_id, None, dispatch),
        );
        runtime.supervisor.initialize(&run).unwrap();
    }
    assert_eq!(runtime.worker_stop_obligations().unwrap().len(), 2);

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let mut missing_task = aborted_run(Some(workspace));
    let missing = TaskId::new("missing").unwrap();
    missing_task.provenance.insert(
        missing.clone(),
        provenance(
            missing_task.supervisor_run_id,
            &missing,
            None,
            OperationId::new(),
        ),
    );
    runtime.supervisor.initialize(&missing_task).unwrap();
    assert!(
        runtime
            .worker_stop_obligations()
            .unwrap_err()
            .to_string()
            .contains("provenance task is missing")
    );

    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let mut stale = aborted_run(Some(workspace));
    let task_id = TaskId::new("worker").unwrap();
    let dispatch = OperationId::new();
    let mut worker = task(stale.supervisor_run_id, "worker", None);
    worker.assigned_dispatch_run = Some(dispatch);
    stale.tasks.insert(task_id.clone(), worker);
    let mut stale_provenance = provenance(stale.supervisor_run_id, &task_id, None, dispatch);
    stale_provenance.supervisor_run_id = SupervisorRunId::new();
    stale.provenance.insert(task_id, stale_provenance);
    runtime.supervisor.initialize(&stale).unwrap();
    assert!(
        runtime
            .worker_stop_obligations()
            .unwrap_err()
            .to_string()
            .contains("provenance fence is stale")
    );
}
