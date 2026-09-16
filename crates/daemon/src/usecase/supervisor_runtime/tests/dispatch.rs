//! dispatch の振る舞いを固定するテスト。

use super::*;

#[test]
#[allow(clippy::too_many_lines)] // One matrix covers every independent root dispatch reservation fence.
fn root_dispatch_binding_checks_reserved_identity_contract_and_planning_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let semantic_digest =
        usagi_core::infrastructure::ipc::agent_operation_digest("caller-semantic");

    let missing_agent_operation = OperationId::new();
    scheduler
        .dispatch
        .upsert_run(DispatchRun {
            run_id: missing_agent_operation,
            agent_id: AgentId::new(),
            prompt: "missing Agent".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    scheduler
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &missing_agent_operation.to_string(),
            goal("missing Agent"),
            None,
            now(),
        )
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_workspace_root_dispatch(
                &missing_agent_operation.to_string(),
                &root_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("Agent does not exist")
    );

    let goal_operation = OperationId::new();
    let goal_worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, goal_operation, &goal_worker);
    scheduler
        .reserve_goal_for_workspace_with_profile(
            "goal",
            workspace,
            &goal_operation.to_string(),
            goal("profiled goal"),
            AgentProfileId::new("claude").unwrap(),
            semantic_digest.clone(),
            None,
            now(),
        )
        .unwrap();
    scheduler
        .bind_reserved_workspace_root_dispatch(&goal_operation.to_string(), &goal_worker, now())
        .unwrap();

    let profile_operation = OperationId::new();
    let profile_worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, profile_operation, &profile_worker);
    scheduler
        .reserve_goal_for_workspace_with_profile(
            "goal",
            workspace,
            &profile_operation.to_string(),
            goal("wrong profile"),
            AgentProfileId::new("codex").unwrap(),
            semantic_digest.clone(),
            None,
            now(),
        )
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_workspace_root_dispatch(
                &profile_operation.to_string(),
                &profile_worker,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("reserved Agent scope")
    );

    let semantic_operation = OperationId::new();
    let semantic_worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, semantic_operation, &semantic_worker);
    scheduler
        .reserve_goal_for_workspace_with_profile(
            "goal",
            workspace,
            &semantic_operation.to_string(),
            goal("wrong semantics"),
            AgentProfileId::new("claude").unwrap(),
            "wrong-digest".into(),
            None,
            now(),
        )
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_workspace_root_dispatch(
                &semantic_operation.to_string(),
                &semantic_worker,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("another semantic intent")
    );

    let session_operation = OperationId::new();
    let session_worker = delegated_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, session_operation, &session_worker);
    scheduler
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &session_operation.to_string(),
            goal("must be root"),
            None,
            now(),
        )
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_workspace_root_dispatch(
                &session_operation.to_string(),
                &session_worker,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("workspace root scope")
    );

    let caller_session_operation = OperationId::new();
    let caller_session_worker = delegated_worker(workspace);
    persist_caller_dispatch(
        &scheduler,
        workspace,
        caller_session_operation,
        &caller_session_worker,
    );
    let caller_session_start = OperationId::new().to_string();
    scheduler
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &caller_session_start,
            "session fence".into(),
            None,
            caller_session_operation,
            &caller_session_worker,
            now(),
        )
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_caller_dispatch(
                &caller_session_start,
                caller_session_operation,
                &delegated_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("reserved Agent scope")
    );

    let caller_operation = OperationId::new();
    let caller_worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, caller_operation, &caller_worker);
    assert!(
        scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &OperationId::new().to_string(),
                "wrong scope".into(),
                None,
                caller_operation,
                &root_worker(WorkspaceId::new()),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("outside its authenticated scope")
    );
    let start_operation = OperationId::new().to_string();
    scheduler
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &start_operation,
            "generic root".into(),
            None,
            caller_operation,
            &caller_worker,
            now(),
        )
        .unwrap();
    let other_operation = OperationId::new();
    let other_worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, other_operation, &other_worker);
    assert!(
        scheduler
            .bind_reserved_caller_dispatch(&start_operation, other_operation, &other_worker, now(),)
            .unwrap_err()
            .to_string()
            .contains("conflicts with its reservation")
    );
    assert!(
        scheduler
            .bind_reserved_caller_dispatch(
                &start_operation,
                caller_operation,
                &root_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("reserved Agent scope")
    );

    let planning_temp = tempfile::tempdir().unwrap();
    let planning = SupervisorRuntime::new(planning_temp.path());
    let planning_operation = OperationId::new();
    let planning_worker = root_worker(workspace);
    persist_caller_dispatch(&planning, workspace, planning_operation, &planning_worker);
    let planning_start = OperationId::new().to_string();
    planning.fail_apply_at(1);
    assert!(
        planning
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &planning_start,
                "planning root".into(),
                None,
                planning_operation,
                &planning_worker,
                now(),
            )
            .is_err()
    );
    planning.fail_apply_at(2);
    assert!(
        planning
            .bind_reserved_caller_dispatch(
                &planning_start,
                planning_operation,
                &planning_worker,
                now(),
            )
            .is_err()
    );
    let recovered = planning
        .bind_reserved_caller_dispatch(&planning_start, planning_operation, &planning_worker, now())
        .unwrap();
    assert_eq!(recovered.state, SupervisorRunState::Running);
}

#[test]
#[allow(clippy::too_many_lines)] // One lifecycle fixture covers verification, escalation, resume, and late-result fencing.
fn workspace_root_dispatch_is_bound_idempotently_and_completes_the_run() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let workspace = WorkspaceId::new();
    let operation = OperationId::new();
    dispatch
        .upsert_run(DispatchRun {
            run_id: operation,
            agent_id: AgentId::new(),
            prompt: String::new(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    persist_root_dispatch_agent(&scheduler, workspace, operation);
    let worker = root_worker(workspace);
    let unbound = scheduler
        .reserve_goal_for_workspace(
            "goal-composer",
            workspace,
            &operation.to_string(),
            goal("finish the requested work"),
            Some("standard".into()),
            now(),
        )
        .unwrap();
    let mut waker = Waker::default();
    scheduler
        .tick(unbound.supervisor_run_id, now(), &mut waker)
        .unwrap();
    assert_eq!(
        scheduler
            .get("goal-composer", unbound.supervisor_run_id)
            .unwrap()
            .unwrap()
            .state,
        SupervisorRunState::Running
    );
    assert!(
        scheduler
            .supervisor
            .load(unbound.supervisor_run_id)
            .unwrap()
            .unwrap()
            .tasks[&TaskId::new("root").unwrap()]
            .promotion_reserved_at
            .is_some()
    );
    // A pre-fix daemon could persist this escalation between reservation
    // and binding. The new binder must heal that exact legacy snapshot.
    let reserved = scheduler
        .supervisor
        .load(unbound.supervisor_run_id)
        .unwrap()
        .unwrap();
    scheduler
        .apply(
            &reserved,
            now(),
            SupervisorEventSource::DispatchFailure,
            SupervisorEventKind::Escalate {
                task_id: Some(TaskId::new("root").unwrap()),
                reason: MISSING_DISPATCH_ESCALATION_REASON.into(),
                safe_evidence: "pre-fix snapshot".into(),
                choices: vec!["resume".into(), "cancel".into()],
            },
        )
        .unwrap();
    let first = scheduler
        .start_for_workspace_root_dispatch(
            "goal-composer",
            workspace,
            &operation.to_string(),
            goal("finish the requested work"),
            Some("standard".into()),
            &worker,
            now(),
        )
        .unwrap();
    let replay = scheduler
        .start_for_workspace_root_dispatch(
            "goal-composer",
            workspace,
            &operation.to_string(),
            goal("finish the requested work"),
            Some("standard".into()),
            &worker,
            now(),
        )
        .unwrap();
    assert_eq!(replay, first);
    assert_eq!(first.tasks[0].state, TaskState::Dispatched);
    assert_eq!(first.tasks[0].assigned_dispatch_run, Some(operation));
    assert_eq!(
        scheduler
            .supervisor
            .load(first.supervisor_run_id)
            .unwrap()
            .unwrap()
            .tasks[&TaskId::new("root").unwrap()]
            .promotion_reserved_at,
        None
    );
    assert_eq!(first.provenance[0].worker_session_id, None);

    scheduler
        .tick(first.supervisor_run_id, now(), &mut waker)
        .unwrap();
    let active = scheduler
        .get("goal-composer", first.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert_eq!(active.state, SupervisorRunState::Running);
    assert!(active.escalation.is_none());

    dispatch
        .upsert_run(DispatchRun {
            run_id: operation,
            agent_id: AgentId::new(),
            prompt: String::new(),
            started_at: now(),
            ended_at: Some(now()),
            status: RunStatus::Completed,
        })
        .unwrap();
    scheduler
        .tick(first.supervisor_run_id, now(), &mut waker)
        .unwrap();
    let completed = scheduler
        .get("goal-composer", first.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert_eq!(completed.tasks[0].state, TaskState::Verifying);
    assert_eq!(completed.state, SupervisorRunState::Running);
    let request = scheduler
        .prepare_artifact_verification(operation, now())
        .unwrap()
        .unwrap();
    assert_eq!(request.repository, artifact_repository());
    for invalid in [
        ArtifactVerification {
            status: ArtifactVerificationStatus::Verified,
            result_digest: String::new(),
            safe_summary: "verified".into(),
        },
        ArtifactVerification {
            status: ArtifactVerificationStatus::Verified,
            result_digest: "verified".into(),
            safe_summary: "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
        },
    ] {
        assert!(
            scheduler
                .record_artifact_verification(&request, invalid, now())
                .is_err()
        );
    }
    assert_eq!(
        scheduler
            .get("goal-composer", first.supervisor_run_id)
            .unwrap()
            .unwrap()
            .tasks[0]
            .state,
        TaskState::Verifying
    );
    let deferred = scheduler
        .record_artifact_verification(
            &request,
            ArtifactVerification {
                status: ArtifactVerificationStatus::Retryable,
                result_digest: "provider-unavailable".into(),
                safe_summary: "pull request verification provider is unavailable".into(),
            },
            now(),
        )
        .unwrap();
    assert_eq!(deferred.state, SupervisorRunState::Running);
    assert_eq!(deferred.tasks[0].state, TaskState::Verifying);
    assert_eq!(deferred.tasks[0].verification_attempt, 1);
    assert_eq!(
        deferred.tasks[0].verification_retry_at,
        Some(now() + chrono::Duration::seconds(ARTIFACT_RETRY_BASE_SECONDS))
    );
    assert!(
        scheduler
            .pending_artifact_verifications(now())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        scheduler
            .record_artifact_verification(
                &request,
                ArtifactVerification {
                    status: ArtifactVerificationStatus::Retryable,
                    result_digest: "duplicate".into(),
                    safe_summary: "duplicate".into(),
                },
                now(),
            )
            .unwrap(),
        deferred
    );
    assert!(
        scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .is_none()
    );
    let due = now() + chrono::Duration::seconds(ARTIFACT_RETRY_BASE_SECONDS);
    assert_eq!(
        scheduler.pending_artifact_verifications(due).unwrap(),
        vec![PendingArtifactVerification {
            dispatch_run_id: operation,
        }]
    );
    let retry = scheduler
        .prepare_artifact_verification(operation, due)
        .unwrap()
        .unwrap();
    let retry = scheduler
        .record_artifact_expectation(&retry, &artifact_expectation(), due)
        .unwrap();
    let rejected = scheduler
        .record_artifact_verification(
            &retry,
            ArtifactVerification {
                status: ArtifactVerificationStatus::Rejected,
                result_digest: "draft-pr".into(),
                safe_summary: "pull request is still a draft".into(),
            },
            due,
        )
        .unwrap();
    assert_eq!(rejected.state, SupervisorRunState::Escalated);
    assert_eq!(
        rejected.escalation.as_ref().unwrap().safe_evidence,
        "pull request is still a draft"
    );
    scheduler
        .resolve_escalation(
            "goal-composer",
            first.supervisor_run_id,
            rejected.escalation.unwrap().escalation_id,
            EscalationDecision::Resume,
            due,
        )
        .unwrap();
    assert!(
        scheduler
            .prepare_artifact_verification(operation, due)
            .unwrap()
            .is_none()
    );
    assert!(
        scheduler
            .supervisor
            .load(first.supervisor_run_id)
            .unwrap()
            .unwrap()
            .verification_candidates
            .is_empty()
    );
    let retry = scheduler
        .prepare_artifact_verification_after_report(
            operation,
            Some(StructuredResult {
                pr: Some("https://github.com/acme/repo/pull/2".into()),
                ..StructuredResult::default()
            }),
            due,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        retry
            .result
            .as_ref()
            .and_then(|result| result.pr.as_deref()),
        Some("https://github.com/acme/repo/pull/2")
    );
    assert_eq!(
        scheduler
            .supervisor
            .load(first.supervisor_run_id)
            .unwrap()
            .unwrap()
            .verification_candidates[&TaskId::new("root").unwrap()]
            .as_deref(),
        Some("https://github.com/acme/repo/pull/2")
    );
    let retry = scheduler
        .record_artifact_expectation(&retry, &artifact_expectation(), due)
        .unwrap();
    let completed = scheduler
        .record_artifact_verification(
            &retry,
            ArtifactVerification {
                status: ArtifactVerificationStatus::Verified,
                result_digest: "verified".into(),
                safe_summary: "verified".into(),
            },
            due,
        )
        .unwrap();
    assert_eq!(completed.tasks[0].state, TaskState::Succeeded);
    assert_eq!(completed.state, SupervisorRunState::Succeeded);
    assert!(
        scheduler
            .pending_artifact_verifications(now())
            .unwrap()
            .is_empty()
    );
    let late = scheduler
        .record_artifact_verification(
            &retry,
            ArtifactVerification {
                status: ArtifactVerificationStatus::Rejected,
                result_digest: "late-provider-result".into(),
                safe_summary: "late provider result".into(),
            },
            due,
        )
        .unwrap();
    assert_eq!(late, completed);
}

#[test]
#[allow(clippy::too_many_lines)] // One artifact fence fixture covers candidate capture and every stale request dimension.
fn artifact_verification_preparation_captures_only_the_exact_completed_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let operation = OperationId::new();
    scheduler
        .dispatch
        .upsert_run(DispatchRun {
            run_id: operation,
            agent_id: AgentId::new(),
            prompt: "goal".into(),
            started_at: now(),
            ended_at: Some(now()),
            status: RunStatus::Completed,
        })
        .unwrap();
    persist_root_dispatch_agent(&scheduler, workspace, operation);
    let caller = CallerRef {
        session_id: None,
        agent_id: AgentId::new(),
    };
    let structured = StructuredResult {
        pr: Some("https://github.com/acme/repo/pull/1".into()),
        commits: vec!["abc".into()],
        changed_files: vec!["src/lib.rs".into()],
        verification: Some("candidate only".into()),
    };
    scheduler
        .dispatch
        .upsert_binding(DispatchBinding {
            run_id: operation,
            caller: caller.clone(),
            worker: WorkerRef {
                session_id: None,
                agent_id: AgentId::new(),
            },
        })
        .unwrap();
    scheduler
        .dispatch
        .append_inbox(
            &caller,
            InboxMessage {
                run_id: operation,
                from: WorkerRef {
                    session_id: None,
                    agent_id: AgentId::new(),
                },
                kind: InboxKind::Completed,
                summary: "worker says complete".into(),
                result: Some(structured.clone()),
                created_at: now(),
                read: false,
            },
        )
        .unwrap();
    let run = scheduler
        .start_for_workspace_root_dispatch(
            "goal",
            workspace,
            &operation.to_string(),
            goal("finish"),
            None,
            &root_worker(workspace),
            now(),
        )
        .unwrap();
    assert_eq!(run.tasks[0].state, TaskState::Dispatched);
    let second_operation = OperationId::new();
    scheduler
        .dispatch
        .upsert_run(DispatchRun {
            run_id: second_operation,
            agent_id: AgentId::new(),
            prompt: "another goal".into(),
            started_at: now(),
            ended_at: Some(now()),
            status: RunStatus::Completed,
        })
        .unwrap();
    persist_root_dispatch_agent(&scheduler, workspace, second_operation);
    scheduler
        .start_for_workspace_root_dispatch(
            "another-goal",
            workspace,
            &second_operation.to_string(),
            goal("another finish"),
            None,
            &root_worker(workspace),
            now(),
        )
        .unwrap();
    let mut expected_pending = vec![operation, second_operation];
    expected_pending.sort_by_key(ToString::to_string);
    assert_eq!(
        scheduler.pending_artifact_verifications(now()).unwrap(),
        expected_pending
            .into_iter()
            .map(|dispatch_run_id| PendingArtifactVerification { dispatch_run_id })
            .collect::<Vec<_>>()
    );

    let request = scheduler
        .prepare_artifact_verification(operation, now())
        .unwrap()
        .unwrap();
    assert_eq!(request.result, Some(structured));
    assert_eq!(request.task_id, TaskId::new("root").unwrap());
    assert_eq!(
        scheduler
            .get("goal", run.supervisor_run_id)
            .unwrap()
            .unwrap()
            .tasks[0]
            .state,
        TaskState::Verifying
    );
    assert!(
        scheduler
            .prepare_artifact_verification(OperationId::new(), now())
            .unwrap()
            .is_none()
    );

    let verified = || ArtifactVerification {
        status: ArtifactVerificationStatus::Verified,
        result_digest: "verified".into(),
        safe_summary: "verified".into(),
    };
    assert!(
        scheduler
            .record_artifact_verification(&request, verified(), now())
            .unwrap_err()
            .to_string()
            .contains("verified artifact expectation is missing")
    );
    let missing_expectation_task = ArtifactVerificationRequest {
        task_id: TaskId::new("missing").unwrap(),
        ..request.clone()
    };
    assert!(
        scheduler
            .record_artifact_expectation(&missing_expectation_task, &artifact_expectation(), now(),)
            .unwrap_err()
            .to_string()
            .contains("task is missing")
    );
    let other_expectation = ArtifactExpectation::new(
        GitHubRepository::from_name_with_owner("other/repo").unwrap(),
        "0123456789012345678901234567890123456789",
    )
    .unwrap();
    for stale in [
        ArtifactVerificationRequest {
            generation: request.generation + 1,
            ..request.clone()
        },
        ArtifactVerificationRequest {
            contract: NO_ARTIFACT_CONTRACT,
            ..request.clone()
        },
        ArtifactVerificationRequest {
            verification_attempt: request.verification_attempt + 1,
            ..request.clone()
        },
        ArtifactVerificationRequest {
            repository: GitHubRepository::from_name_with_owner("other/repo").unwrap(),
            ..request.clone()
        },
    ] {
        assert!(
            scheduler
                .record_artifact_expectation(&stale, &artifact_expectation(), now())
                .unwrap_err()
                .to_string()
                .contains("fence is stale")
        );
    }
    assert!(
        scheduler
            .record_artifact_expectation(&request, &other_expectation, now())
            .unwrap_err()
            .to_string()
            .contains("fence is stale")
    );
    scheduler.fail_apply_at(scheduler.apply_calls.get());
    assert!(
        scheduler
            .record_artifact_expectation(&request, &artifact_expectation(), now())
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
    let pinned = scheduler
        .record_artifact_expectation(&request, &artifact_expectation(), now())
        .unwrap();
    assert_eq!(
        scheduler
            .record_artifact_expectation(&pinned, &artifact_expectation(), now())
            .unwrap(),
        pinned
    );
    assert!(
        scheduler
            .record_artifact_verification(&request, verified(), now())
            .unwrap_err()
            .to_string()
            .contains("expectation fence is stale")
    );
    let future_attempt = ArtifactVerificationRequest {
        verification_attempt: pinned.verification_attempt + 1,
        ..pinned.clone()
    };
    assert!(
        scheduler
            .record_artifact_verification(&future_attempt, verified(), now())
            .unwrap_err()
            .to_string()
            .contains("attempt fence is stale")
    );
    let wrong_expectation = ArtifactVerificationRequest {
        expectation: Some(other_expectation),
        ..pinned
    };
    assert!(
        scheduler
            .record_artifact_verification(&wrong_expectation, verified(), now())
            .unwrap_err()
            .to_string()
            .contains("expectation fence is stale")
    );

    let missing_task = ArtifactVerificationRequest {
        task_id: TaskId::new("missing").unwrap(),
        ..request.clone()
    };
    assert!(
        scheduler
            .record_artifact_verification(
                &missing_task,
                ArtifactVerification {
                    status: ArtifactVerificationStatus::Verified,
                    result_digest: "verified".into(),
                    safe_summary: "verified".into(),
                },
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("task is missing")
    );
    for stale in [
        ArtifactVerificationRequest {
            generation: request.generation + 1,
            ..request.clone()
        },
        ArtifactVerificationRequest {
            contract: NO_ARTIFACT_CONTRACT,
            ..request.clone()
        },
    ] {
        assert!(
            scheduler
                .record_artifact_verification(
                    &stale,
                    ArtifactVerification {
                        status: ArtifactVerificationStatus::Verified,
                        result_digest: "verified".into(),
                        safe_summary: "verified".into(),
                    },
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("fence is stale")
        );
    }

    let mut unexpectedly_running = scheduler
        .supervisor
        .load(run.supervisor_run_id)
        .unwrap()
        .unwrap();
    unexpectedly_running.state = SupervisorRunState::Running;
    unexpectedly_running
        .tasks
        .get_mut(&request.task_id)
        .unwrap()
        .state = TaskState::Running;
    scheduler
        .supervisor
        .initialize(&unexpectedly_running)
        .unwrap();
    assert!(
        scheduler
            .record_artifact_verification(
                &request,
                ArtifactVerification {
                    status: ArtifactVerificationStatus::Verified,
                    result_digest: "verified".into(),
                    safe_summary: "verified".into(),
                },
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("fence is stale")
    );

    let mut duplicate = unexpectedly_running;
    duplicate.supervisor_run_id = SupervisorRunId::new();
    for task in duplicate.tasks.values_mut() {
        task.supervisor_run_id = duplicate.supervisor_run_id;
    }
    for provenance in duplicate.provenance.values_mut() {
        provenance.supervisor_run_id = duplicate.supervisor_run_id;
    }
    scheduler.supervisor.initialize(&duplicate).unwrap();
    assert!(
        scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap_err()
            .to_string()
            .contains("multiple supervisor runs")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One refusal matrix covers every fail-closed root provenance boundary.
fn workspace_root_dispatch_refuses_invalid_missing_and_conflicting_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let workspace = WorkspaceId::new();
    let worker = root_worker(workspace);
    assert!(
        scheduler
            .start_for_workspace_root_dispatch(
                "caller",
                workspace,
                "not-an-operation",
                goal("root"),
                None,
                &worker,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("operation is invalid")
    );
    let missing = OperationId::new();
    assert!(
        scheduler
            .start_for_workspace_root_dispatch(
                "caller",
                workspace,
                &missing.to_string(),
                goal("root"),
                None,
                &worker,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("dispatch does not exist")
    );
    let operation = OperationId::new();
    dispatch
        .upsert_run(DispatchRun {
            run_id: operation,
            agent_id: AgentId::new(),
            prompt: String::new(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    persist_root_dispatch_agent(&scheduler, workspace, operation);
    assert!(
        scheduler
            .start_for_workspace_root_dispatch(
                "caller",
                workspace,
                &operation.to_string(),
                goal(""),
                None,
                &worker,
                now(),
            )
            .is_err()
    );
    assert!(
        scheduler
            .start_for_workspace_root_dispatch(
                "caller",
                workspace,
                &operation.to_string(),
                goal("root"),
                None,
                &root_worker(WorkspaceId::new()),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("outside its reserved workspace")
    );
    scheduler
        .start_for_workspace_root_dispatch(
            "caller",
            workspace,
            &operation.to_string(),
            goal("root"),
            None,
            &worker,
            now(),
        )
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_root_task(
                &operation.to_string(),
                operation,
                &worker,
                NO_ARTIFACT_CONTRACT,
                false,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("another artifact contract")
    );
    assert!(
        scheduler
            .start_for_workspace_root_dispatch(
                "caller",
                workspace,
                &operation.to_string(),
                goal("root"),
                None,
                &root_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("provenance conflicts")
    );
    assert!(
        scheduler
            .load_started_run(SupervisorRunId::new())
            .unwrap_err()
            .to_string()
            .contains("disappeared")
    );

    let missing_root_temp = tempfile::tempdir().unwrap();
    let missing_root = SupervisorRuntime::new(missing_root_temp.path());
    let missing_root_dispatch = DispatchStore::new(missing_root_temp.path());
    let missing_root_operation = OperationId::new();
    missing_root_dispatch
        .upsert_run(DispatchRun {
            run_id: missing_root_operation,
            agent_id: AgentId::new(),
            prompt: String::new(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    persist_root_dispatch_agent(&missing_root, workspace, missing_root_operation);
    missing_root.fail_apply_at(0);
    assert!(
        missing_root
            .reserve_goal_for_workspace(
                "caller",
                workspace,
                &missing_root_operation.to_string(),
                goal("root"),
                None,
                now(),
            )
            .is_err()
    );
    let id = missing_root.load_state().unwrap().starts[&missing_root_operation.to_string()]
        .supervisor_run_id;
    let mut incomplete = missing_root.supervisor.load(id).unwrap().unwrap();
    incomplete.state = SupervisorRunState::Running;
    missing_root.supervisor.initialize(&incomplete).unwrap();
    assert!(
        missing_root
            .start_for_workspace_root_dispatch(
                "caller",
                workspace,
                &missing_root_operation.to_string(),
                goal("root"),
                None,
                &worker,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("root task is missing")
    );
}

#[test]
fn workspace_root_dispatch_propagates_each_binding_write_failure() {
    for (escalate_before_binding, failed_apply) in [(false, 2), (true, 3), (true, 4)] {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let dispatch = DispatchStore::new(temp.path());
        let workspace = WorkspaceId::new();
        let operation = OperationId::new();
        dispatch
            .upsert_run(DispatchRun {
                run_id: operation,
                agent_id: AgentId::new(),
                prompt: String::new(),
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            })
            .unwrap();
        persist_root_dispatch_agent(&scheduler, workspace, operation);
        let started = scheduler
            .reserve_goal_for_workspace(
                "caller",
                workspace,
                &operation.to_string(),
                goal("root"),
                None,
                now(),
            )
            .unwrap();
        if escalate_before_binding {
            let reserved = scheduler
                .supervisor
                .load(started.supervisor_run_id)
                .unwrap()
                .unwrap();
            scheduler
                .apply(
                    &reserved,
                    now(),
                    SupervisorEventSource::DispatchFailure,
                    SupervisorEventKind::Escalate {
                        task_id: Some(TaskId::new("root").unwrap()),
                        reason: MISSING_DISPATCH_ESCALATION_REASON.into(),
                        safe_evidence: "pre-fix snapshot".into(),
                        choices: vec!["resume".into(), "cancel".into()],
                    },
                )
                .unwrap();
        }
        scheduler.fail_apply_at(failed_apply);

        assert!(
            scheduler
                .start_for_workspace_root_dispatch(
                    "caller",
                    workspace,
                    &operation.to_string(),
                    goal("root"),
                    None,
                    &root_worker(workspace),
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("injected supervisor apply failure")
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One recovery fixture keeps reservation, collision, escalation, and exact replay assertions together.
fn delegated_dispatch_is_reserved_before_spawn_and_reconciled_by_exact_operation() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let workspace = WorkspaceId::new();
    let root_operation = OperationId::new();
    assert!(!scheduler.supervises_dispatch(root_operation).unwrap());
    dispatch
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
            goal("root work"),
            None,
            &root_worker(workspace),
            now(),
        )
        .unwrap();
    assert!(scheduler.supervises_dispatch(root_operation).unwrap());
    let before_oversized = scheduler
        .get("goal-composer", root.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &OperationId::new().to_string(),
                "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
                now(),
            )
            .is_err()
    );
    assert_eq!(
        scheduler
            .get("goal-composer", root.supervisor_run_id)
            .unwrap()
            .unwrap(),
        before_oversized
    );
    let child_operation = OperationId::new();
    let reserved = scheduler
        .reserve_delegated_dispatch(
            root_operation,
            &child_operation.to_string(),
            "child work",
            now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(reserved.run.tasks.len(), 2);
    assert_eq!(reserved.run.provenance.len(), 1);
    assert_eq!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work",
                now(),
            )
            .unwrap()
            .unwrap(),
        reserved
    );
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "different child work",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("conflicts")
    );
    let pending = scheduler.pending_delegated_promotions().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].operation_id, child_operation.to_string());

    scheduler
        .tick(root.supervisor_run_id, now(), &mut Waker::default())
        .unwrap();
    assert_eq!(
        scheduler
            .get("goal-composer", root.supervisor_run_id)
            .unwrap()
            .unwrap()
            .state,
        SupervisorRunState::Running
    );
    dispatch
        .upsert_run(DispatchRun {
            run_id: child_operation,
            agent_id: AgentId::new(),
            prompt: "child".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    assert!(
        scheduler
            .attach_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work".into(),
                &root_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("has no managed session")
    );
    let worker = delegated_worker(workspace);
    let attached = scheduler
        .attach_delegated_dispatch(
            root_operation,
            &child_operation.to_string(),
            "child work".into(),
            &worker,
            now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(attached.state, SupervisorRunState::Running);
    assert!(attached.escalation.is_none());
    assert_eq!(attached.tasks.len(), 2);
    assert_eq!(attached.provenance.len(), 2);
    assert!(
        attached
            .provenance
            .iter()
            .any(|item| item.parent_dispatch_run == Some(root_operation))
    );
    assert_eq!(
        scheduler
            .attach_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work".into(),
                &worker,
                now(),
            )
            .unwrap()
            .unwrap(),
        attached
    );
    assert!(
        scheduler
            .attach_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work".into(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("provenance conflicts")
    );

    // A user-defined task whose ID happens to look similar is not a
    // daemon reservation because it does not carry the origin marker.
    let fake_operation = OperationId::new();
    let run = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    scheduler
        .apply(
            &run,
            now(),
            SupervisorEventSource::Admission,
            SupervisorEventKind::AddTask {
                task: task_node(
                    &run,
                    delegated_task_id(fake_operation).unwrap(),
                    Some(TaskId::new("root").unwrap()),
                    BTreeSet::new(),
                    "ordinary task".into(),
                    NO_ARTIFACT_CONTRACT,
                ),
            },
        )
        .unwrap();
    assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
}

#[test]
#[allow(clippy::too_many_lines)] // Committed, pending, and new-root joins share the same terminal Agent fence.
fn terminal_agent_dispatch_cannot_delegate_or_start_a_supervisor_run() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();

    let committed_operation = OperationId::new();
    let committed_worker = root_worker(workspace);
    persist_caller_dispatch(
        &scheduler,
        workspace,
        committed_operation,
        &committed_worker,
    );
    let committed = scheduler
        .start_for_workspace_root_dispatch(
            "goal",
            workspace,
            &committed_operation.to_string(),
            goal("committed root"),
            None,
            &committed_worker,
            now(),
        )
        .unwrap();
    let mut committed_run = scheduler
        .supervisor
        .load(committed.supervisor_run_id)
        .unwrap()
        .unwrap();
    committed_run
        .tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .state = TaskState::Verifying;
    scheduler.supervisor.initialize(&committed_run).unwrap();
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                committed_operation,
                &OperationId::new().to_string(),
                "late from verifying",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("closed supervisor ownership")
    );
    committed_run
        .tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .state = TaskState::Dispatched;
    scheduler.supervisor.initialize(&committed_run).unwrap();
    let mut committed_dispatch = scheduler
        .dispatch
        .run(committed_operation)
        .unwrap()
        .unwrap();
    committed_dispatch.status = RunStatus::Completed;
    committed_dispatch.ended_at = Some(now());
    scheduler.dispatch.upsert_run(committed_dispatch).unwrap();
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                committed_operation,
                &OperationId::new().to_string(),
                "late child",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("closed supervisor ownership")
    );

    let pending_operation = OperationId::new();
    let pending_worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, pending_operation, &pending_worker);
    scheduler
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &pending_operation.to_string(),
            goal("pending root"),
            None,
            now(),
        )
        .unwrap();
    let mut pending_dispatch = scheduler.dispatch.run(pending_operation).unwrap().unwrap();
    pending_dispatch.status = RunStatus::Failed;
    pending_dispatch.ended_at = Some(now());
    scheduler.dispatch.upsert_run(pending_dispatch).unwrap();
    assert!(
        scheduler
            .supervision_fence(pending_operation)
            .unwrap_err()
            .to_string()
            .contains("closed supervisor ownership")
    );

    let new_root_operation = OperationId::new();
    let new_root_worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, new_root_operation, &new_root_worker);
    let mut terminal_caller = scheduler.dispatch.run(new_root_operation).unwrap().unwrap();
    terminal_caller.status = RunStatus::NoReport;
    terminal_caller.ended_at = Some(now());
    scheduler.dispatch.upsert_run(terminal_caller).unwrap();
    assert!(
        scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &OperationId::new().to_string(),
                "late supervisor".into(),
                None,
                new_root_operation,
                &new_root_worker,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("closed supervisor ownership")
    );
}

#[test]
fn supervised_peer_handoff_still_obeys_dispatch_budget() {
    let (_temp, scheduler, _workspace, parent_operation, _worker, peer) = supervised_peer_fixture();
    let fence = scheduler
        .supervision_fence(parent_operation)
        .unwrap()
        .unwrap();
    let mut run = scheduler
        .supervisor
        .load(fence.supervisor_run_id)
        .unwrap()
        .unwrap();
    run.policy.max_dispatches = 1;
    scheduler.supervisor.initialize(&run).unwrap();
    let operation = OperationId::new();
    assert!(
        scheduler
            .reserve_peer_handoff(
                parent_operation,
                &operation.to_string(),
                "review",
                &peer,
                "worker",
                now()
            )
            .unwrap_err()
            .to_string()
            .contains("policy denied")
    );
    let escalated = scheduler
        .supervisor
        .load(fence.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert_eq!(escalated.state, SupervisorRunState::Escalated);
    assert!(
        !escalated
            .tasks
            .contains_key(&delegated_task_id(operation).unwrap())
    );
    assert!(scheduler.dispatch.run(operation).unwrap().is_none());
}

#[test]
#[allow(clippy::too_many_lines)] // Each malformed reservation isolates one delegated bind fence.
fn delegated_binding_rejects_worker_authority_and_stale_operation_fences() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let root_operation = OperationId::new();
    let root_worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, root_operation, &root_worker);
    let root = scheduler
        .start_for_workspace_root_dispatch(
            "goal",
            workspace,
            &root_operation.to_string(),
            goal("delegated bind fences"),
            None,
            &root_worker,
            now(),
        )
        .unwrap();

    let mismatched_operation = OperationId::new();
    let expected_worker = delegated_worker(workspace);
    persist_caller_dispatch(
        &scheduler,
        workspace,
        mismatched_operation,
        &expected_worker,
    );
    assert!(
        scheduler
            .attach_delegated_dispatch(
                root_operation,
                &mismatched_operation.to_string(),
                "worker mismatch".into(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("reserved supervisor scope")
    );

    let missing_authority_operation = OperationId::new();
    scheduler
        .reserve_delegated_dispatch(
            root_operation,
            &missing_authority_operation.to_string(),
            "missing authority",
            now(),
        )
        .unwrap()
        .unwrap();
    let missing_authority_worker = delegated_worker(workspace);
    persist_caller_dispatch(
        &scheduler,
        workspace,
        missing_authority_operation,
        &missing_authority_worker,
    );
    let mut missing_authority_run = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    missing_authority_run
        .tasks
        .get_mut(&delegated_task_id(missing_authority_operation).unwrap())
        .unwrap()
        .promotion_reserved_at = None;
    scheduler
        .supervisor
        .initialize(&missing_authority_run)
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_delegated_dispatch(
                &missing_authority_operation.to_string(),
                &missing_authority_worker,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("promotion authority is missing")
    );

    let stale_operation = OperationId::new();
    scheduler
        .reserve_delegated_dispatch(
            root_operation,
            &stale_operation.to_string(),
            "stale authority",
            now(),
        )
        .unwrap()
        .unwrap();
    let stale_worker = delegated_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, stale_operation, &stale_worker);
    let mut stale_run = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    let stale_id = delegated_task_id(stale_operation).unwrap();
    let other_operation = OperationId::new();
    let stale_task = stale_run.tasks.get_mut(&stale_id).unwrap();
    stale_task.state = TaskState::Dispatched;
    stale_task.assigned_dispatch_run = Some(other_operation);
    stale_task.promotion_reserved_at = None;
    let root_id = TaskId::new("root").unwrap();
    let mut stale_provenance = provenance(
        stale_run.supervisor_run_id,
        &stale_id,
        Some((&root_id, root_operation)),
        other_operation,
    );
    stale_provenance.worker_session_id = stale_worker.session_id;
    stale_provenance.worker_agent_id = stale_worker.agent_runtime_id;
    stale_provenance.worker_worktree_id = stale_worker.terminal.worktree_id;
    stale_run
        .provenance
        .insert(stale_id.clone(), stale_provenance);
    scheduler.supervisor.initialize(&stale_run).unwrap();
    assert!(
        scheduler
            .bind_reserved_delegated_dispatch(&stale_operation.to_string(), &stale_worker, now(),)
            .unwrap_err()
            .to_string()
            .contains("promotion fence is stale")
    );
}

#[test]
fn one_dispatch_never_moves_between_retained_supervisor_roots() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let dispatch_operation = OperationId::new();
    let worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, dispatch_operation, &worker);
    let first_operation = OperationId::new().to_string();
    let first = scheduler
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &first_operation,
            "first work".into(),
            None,
            dispatch_operation,
            &worker,
            now(),
        )
        .unwrap();
    scheduler
        .bind_reserved_caller_dispatch(&first_operation, dispatch_operation, &worker, now())
        .unwrap();

    scheduler
        .ensure_supervisor_start_dispatch_available(&first_operation, dispatch_operation)
        .unwrap();
    scheduler
        .bind_reserved_caller_dispatch(&first_operation, dispatch_operation, &worker, now())
        .unwrap();
    let second_operation = OperationId::new().to_string();
    assert!(
        scheduler
            .ensure_supervisor_start_dispatch_available(&second_operation, dispatch_operation,)
            .unwrap_err()
            .to_string()
            .contains("another retained supervisor run")
    );
    assert_eq!(scheduler.list_workspace(workspace).unwrap().len(), 1);

    scheduler
        .control_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: first.supervisor_run_id,
                reason: "first finished".into(),
            },
            now(),
        )
        .unwrap();
    assert!(
        scheduler
            .supervision_fence(dispatch_operation)
            .unwrap_err()
            .to_string()
            .contains("stale supervisor ownership")
    );
    scheduler
        .ensure_supervisor_start_dispatch_available(&first_operation, dispatch_operation)
        .unwrap();
    scheduler
        .bind_reserved_caller_dispatch(&first_operation, dispatch_operation, &worker, now())
        .unwrap();
    assert!(
        scheduler
            .ensure_supervisor_start_dispatch_available(&second_operation, dispatch_operation)
            .unwrap_err()
            .to_string()
            .contains("another retained supervisor run")
    );
    assert_eq!(scheduler.list_workspace(workspace).unwrap().len(), 1);
}

#[test]
fn caller_dispatch_reservation_survives_partial_start_and_blocks_another_root() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let dispatch_operation = OperationId::new();
    let worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, dispatch_operation, &worker);
    let first_operation = OperationId::new().to_string();
    scheduler.fail_apply_at(1);
    assert!(
        scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &first_operation,
                "first work".into(),
                None,
                dispatch_operation,
                &worker,
                now(),
            )
            .is_err()
    );

    let second_operation = OperationId::new().to_string();
    assert!(
        scheduler
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &second_operation,
                "second work".into(),
                None,
                dispatch_operation,
                &worker,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("another retained supervisor run")
    );

    let recovered = scheduler
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &first_operation,
            "first work".into(),
            None,
            dispatch_operation,
            &worker,
            now(),
        )
        .unwrap();
    let pending = scheduler.pending_caller_promotions().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].start_operation_id, first_operation);
    assert_eq!(
        pending[0].dispatch_operation_id,
        dispatch_operation.to_string()
    );

    scheduler
        .bind_reserved_caller_dispatch(
            &pending[0].start_operation_id,
            dispatch_operation,
            &worker,
            now(),
        )
        .unwrap();
    assert!(scheduler.pending_caller_promotions().unwrap().is_empty());
    assert_eq!(
        scheduler
            .get("caller", recovered.supervisor_run_id)
            .unwrap()
            .unwrap()
            .tasks[0]
            .state,
        TaskState::Dispatched
    );
}

#[test]
fn caller_dispatch_failure_closes_only_the_exact_generic_root_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let caller_operation = OperationId::new();
    let worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, caller_operation, &worker);
    let start_operation = OperationId::new().to_string();
    let started = scheduler
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &start_operation,
            "work".into(),
            None,
            caller_operation,
            &worker,
            now(),
        )
        .unwrap();

    scheduler.fail_apply_at(2);
    assert!(
        scheduler
            .fail_reserved_caller_dispatch(&start_operation, "write failed".into(), now())
            .is_err()
    );
    let failed = scheduler
        .fail_reserved_caller_dispatch(&start_operation, "spawn failed".into(), now())
        .unwrap();
    assert_eq!(failed.state, SupervisorRunState::Failed);
    assert!(scheduler.pending_caller_promotions().unwrap().is_empty());
    assert_eq!(
        scheduler
            .fail_reserved_caller_dispatch(&start_operation, "replay".into(), now())
            .unwrap()
            .supervisor_run_id,
        started.supervisor_run_id
    );
    assert!(
        scheduler
            .fail_reserved_caller_dispatch(
                &OperationId::new().to_string(),
                "missing".into(),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("reservation does not exist")
    );

    let malformed_operation = OperationId::new();
    let malformed_worker = root_worker(workspace);
    persist_caller_dispatch(
        &scheduler,
        workspace,
        malformed_operation,
        &malformed_worker,
    );
    let malformed_start = OperationId::new().to_string();
    let malformed = scheduler
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &malformed_start,
            "malformed".into(),
            None,
            malformed_operation,
            &malformed_worker,
            now(),
        )
        .unwrap();
    let mut run = scheduler
        .supervisor
        .load(malformed.supervisor_run_id)
        .unwrap()
        .unwrap();
    run.tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
    scheduler.supervisor.initialize(&run).unwrap();
    assert!(
        scheduler
            .fail_reserved_caller_dispatch(&malformed_start, "malformed".into(), now())
            .unwrap_err()
            .to_string()
            .contains("not a caller-root run")
    );
}

#[test]
fn handoff_capture_propagates_a_corrupt_dispatch_store() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let mut run = SupervisorRun::new(
        "caller".into(),
        "root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    let task_id = TaskId::new("child").unwrap();
    let dispatch_run_id = OperationId::new();
    let mut child = task(run.supervisor_run_id, "child", None);
    child.state = TaskState::Succeeded;
    child.assigned_dispatch_run = Some(dispatch_run_id);
    run.tasks.insert(task_id.clone(), child);
    let provenance = provenance(run.supervisor_run_id, &task_id, None, dispatch_run_id);
    run.provenance.insert(task_id.clone(), provenance.clone());

    let mut ignored = run.clone();
    ignored.tasks.get_mut(&task_id).unwrap().state = TaskState::Running;
    assert!(
        scheduler
            .record_terminal_handoff(ignored, &task_id, &provenance, InboxKind::Completed, now(),)
            .unwrap()
            .handoff_context
            .is_empty()
    );
    let mut stale_generation = provenance.clone();
    stale_generation.generation += 1;
    assert!(
        scheduler
            .record_terminal_handoff(
                run.clone(),
                &task_id,
                &stale_generation,
                InboxKind::Completed,
                now(),
            )
            .unwrap()
            .handoff_context
            .is_empty()
    );
    let mut stale_assignment = run.clone();
    stale_assignment
        .tasks
        .get_mut(&task_id)
        .unwrap()
        .assigned_dispatch_run = Some(OperationId::new());
    assert!(
        scheduler
            .record_terminal_handoff(
                stale_assignment,
                &task_id,
                &provenance,
                InboxKind::Completed,
                now(),
            )
            .unwrap()
            .handoff_context
            .is_empty()
    );
    let mut already_recorded = run.clone();
    already_recorded.handoff_context.push(HandoffContextEntry {
        task_id: task_id.clone(),
        generation: provenance.generation,
        dispatch_run_id,
        outcome: InboxKind::Completed,
        summary: "already captured".into(),
        artifacts: None,
        recorded_at: now(),
    });
    assert_eq!(
        scheduler
            .record_terminal_handoff(
                already_recorded,
                &task_id,
                &provenance,
                InboxKind::Completed,
                now(),
            )
            .unwrap()
            .handoff_context
            .len(),
        1
    );

    run.tasks.get_mut(&task_id).unwrap().state = TaskState::Verifying;
    std::fs::write(scheduler.dispatch.registry_path(), "broken").unwrap();

    assert!(
        scheduler
            .record_terminal_handoff(run, &task_id, &provenance, InboxKind::Completed, now(),)
            .is_err()
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One fail-closed matrix exercises the durable child join boundary.
fn delegated_dispatch_refuses_missing_malformed_and_ambiguous_reservations() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let workspace = WorkspaceId::new();
    let root_operation = OperationId::new();
    dispatch
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
            "goal",
            workspace,
            &root_operation.to_string(),
            goal("root"),
            None,
            &root_worker(workspace),
            now(),
        )
        .unwrap();

    let classic_operation = OperationId::new();
    assert_eq!(
        scheduler
            .reserve_delegated_dispatch(
                classic_operation,
                &classic_operation.to_string(),
                "classic child",
                now(),
            )
            .unwrap(),
        None
    );
    let before_identity_reuse = scheduler
        .get("goal", root.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &root_operation.to_string(),
                "recursive self reuse",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("must differ")
    );
    assert_eq!(
        scheduler
            .get("goal", root.supervisor_run_id)
            .unwrap()
            .unwrap(),
        before_identity_reuse
    );

    assert!(
        scheduler
            .reserve_delegated_dispatch(root_operation, "invalid", "child", now())
            .unwrap_err()
            .to_string()
            .contains("operation is invalid")
    );
    assert!(
        scheduler
            .reserve_delegated_dispatch(root_operation, "invalid", String::from("child"), now(),)
            .unwrap_err()
            .to_string()
            .contains("operation is invalid")
    );
    let outside = OperationId::new();
    assert!(
        scheduler
            .reserve_delegated_dispatch(root_operation, &outside.to_string(), "", now(),)
            .unwrap_err()
            .to_string()
            .contains("expected 1..=")
    );
    assert_eq!(
        scheduler
            .reserve_delegated_dispatch(OperationId::new(), &outside.to_string(), "child", now(),)
            .unwrap(),
        None
    );
    assert_eq!(
        scheduler
            .attach_delegated_dispatch(
                OperationId::new(),
                &outside.to_string(),
                "child".into(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap(),
        None
    );
    assert!(
        scheduler
            .bind_reserved_delegated_dispatch("invalid", &delegated_worker(workspace), now())
            .unwrap_err()
            .to_string()
            .contains("operation is invalid")
    );
    assert!(
        scheduler
            .bind_reserved_delegated_dispatch(
                &outside.to_string(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("dispatch does not exist")
    );
    dispatch
        .upsert_run(DispatchRun {
            run_id: outside,
            agent_id: AgentId::new(),
            prompt: "outside".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &outside.to_string(),
                "reused dispatch",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("already in use")
    );
    assert_eq!(
        scheduler
            .bind_reserved_delegated_dispatch(
                &outside.to_string(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap(),
        None
    );
    let generic_operation = OperationId::new();
    scheduler
        .start_for_workspace(
            "generic",
            workspace,
            &generic_operation.to_string(),
            "generic work".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &generic_operation.to_string(),
                "reused start",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("already in use")
    );
    assert!(
        scheduler
            .fail_reserved_delegated_dispatch("invalid", now())
            .unwrap_err()
            .to_string()
            .contains("operation is invalid")
    );
    assert_eq!(
        scheduler
            .fail_reserved_delegated_dispatch(&outside.to_string(), now())
            .unwrap(),
        None
    );

    let mut run = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    for (id, digest) in [
        ("ordinary-task".to_owned(), "ordinary".to_owned()),
        (
            "delegated-not-an-operation".to_owned(),
            "delegated-operation:not-an-operation".to_owned(),
        ),
        (
            format!("delegated-{outside}"),
            "not-the-daemon-origin-marker".to_owned(),
        ),
    ] {
        let id = TaskId::new(id).unwrap();
        let mut child = task_node(
            &run,
            id.clone(),
            Some(TaskId::new("root").unwrap()),
            BTreeSet::new(),
            "ordinary".into(),
            NO_ARTIFACT_CONTRACT,
        );
        child.instruction_digest = digest;
        child.state = TaskState::Ready;
        run.tasks.insert(id, child);
    }
    scheduler.supervisor.initialize(&run).unwrap();
    assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());

    let child_operation = OperationId::new();
    scheduler
        .reserve_delegated_dispatch(root_operation, &child_operation.to_string(), "child", now())
        .unwrap()
        .unwrap();
    assert_eq!(scheduler.pending_delegated_promotions().unwrap().len(), 1);
    let child_id = delegated_task_id(child_operation).unwrap();
    let mut retrying = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    retrying.tasks.get_mut(&child_id).unwrap().state = TaskState::Retrying;
    retrying.tasks.get_mut(&child_id).unwrap().generation = 2;
    scheduler.supervisor.initialize(&retrying).unwrap();
    assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
    retrying.tasks.get_mut(&child_id).unwrap().state = TaskState::Ready;
    retrying.tasks.get_mut(&child_id).unwrap().generation = 1;
    scheduler.supervisor.initialize(&retrying).unwrap();
    dispatch
        .upsert_run(DispatchRun {
            run_id: child_operation,
            agent_id: AgentId::new(),
            prompt: "child".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    let mut malformed = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    malformed.tasks.get_mut(&child_id).unwrap().parent_task_id = None;
    scheduler.supervisor.initialize(&malformed).unwrap();
    assert!(
        scheduler
            .bind_reserved_delegated_dispatch(
                &child_operation.to_string(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("has no parent")
    );
    malformed.tasks.get_mut(&child_id).unwrap().parent_task_id = Some(TaskId::new("root").unwrap());
    malformed
        .tasks
        .get_mut(&child_id)
        .unwrap()
        .promotion_parent_dispatch_run = None;
    malformed.provenance.clear();
    scheduler.supervisor.initialize(&malformed).unwrap();
    assert!(
        scheduler
            .bind_reserved_delegated_dispatch(
                &child_operation.to_string(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("parent provenance is missing")
    );
}

#[test]
fn duplicate_supervisor_membership_refuses_delegated_dispatch_ambiguity() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let workspace = WorkspaceId::new();
    let root_operation = OperationId::new();
    dispatch
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
            "goal",
            workspace,
            &root_operation.to_string(),
            goal("root"),
            None,
            &root_worker(workspace),
            now(),
        )
        .unwrap();
    let child_operation = OperationId::new();
    scheduler
        .reserve_delegated_dispatch(root_operation, &child_operation.to_string(), "child", now())
        .unwrap();

    let mut duplicate = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    duplicate.supervisor_run_id = SupervisorRunId::new();
    for task in duplicate.tasks.values_mut() {
        task.supervisor_run_id = duplicate.supervisor_run_id;
    }
    for provenance in duplicate.provenance.values_mut() {
        provenance.supervisor_run_id = duplicate.supervisor_run_id;
    }
    scheduler.supervisor.initialize(&duplicate).unwrap();

    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &OperationId::new().to_string(),
                "another child",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("multiple supervisor runs")
    );
    assert!(
        scheduler
            .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
            .unwrap_err()
            .to_string()
            .contains("multiple supervisor runs")
    );
    dispatch
        .upsert_run(DispatchRun {
            run_id: child_operation,
            agent_id: AgentId::new(),
            prompt: "child".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_delegated_dispatch(
                &child_operation.to_string(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("multiple supervisor runs")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One sequence contrasts a burned operation with a fresh reservation.
fn definite_delegated_spawn_failure_closes_the_pending_reservation() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let workspace = WorkspaceId::new();
    let root_operation = OperationId::new();
    dispatch
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
            goal("root work"),
            None,
            &root_worker(workspace),
            now(),
        )
        .unwrap();
    let child_operation = OperationId::new();
    scheduler
        .reserve_delegated_dispatch(
            root_operation,
            &child_operation.to_string(),
            "child work",
            now(),
        )
        .unwrap()
        .unwrap();

    scheduler
        .tick(root.supervisor_run_id, now(), &mut Waker::default())
        .unwrap();
    let reserved = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert_eq!(reserved.state, SupervisorRunState::Running);
    let child_id = delegated_task_id(child_operation).unwrap();
    scheduler
        .apply(
            &reserved,
            now(),
            SupervisorEventSource::DispatchFailure,
            SupervisorEventKind::Escalate {
                task_id: Some(child_id),
                reason: MISSING_DISPATCH_ESCALATION_REASON.into(),
                safe_evidence: "pre-fix snapshot".into(),
                choices: vec!["resume".into(), "cancel".into()],
            },
        )
        .unwrap();

    let failed = scheduler
        .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
        .unwrap()
        .unwrap();
    let child = failed
        .tasks
        .iter()
        .find(|task| task.task_id.0 == format!("delegated-{child_operation}"))
        .unwrap();
    assert_eq!(child.state, TaskState::Cancelled);
    assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
    assert!(
        scheduler
            .supervision_fence(child_operation)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    assert!(scheduler.supervises_dispatch(root_operation).unwrap());
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("existing supervisor task")
    );
    assert_eq!(
        scheduler
            .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
            .unwrap()
            .unwrap(),
        failed
    );
    scheduler
        .reserve_delegated_dispatch(
            root_operation,
            &OperationId::new().to_string(),
            "replacement child work",
            now(),
        )
        .unwrap()
        .unwrap();
}

#[test]
#[allow(clippy::too_many_lines)] // Each injected commit point proves that a reservation never reports an uncommitted transition.
fn goal_and_delegated_reservation_commit_failures_are_reported() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let goal_operation = OperationId::new();
    let goal_run = scheduler
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &goal_operation.to_string(),
            goal("goal"),
            None,
            now(),
        )
        .unwrap();
    scheduler.fail_apply_at(scheduler.apply_calls.get());
    assert!(
        scheduler
            .fail_reserved_goal(&goal_operation.to_string(), "failed".into(), now())
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
    let mut missing_root = scheduler
        .supervisor
        .load(goal_run.supervisor_run_id)
        .unwrap()
        .unwrap();
    missing_root.tasks.clear();
    scheduler.supervisor.initialize(&missing_root).unwrap();
    assert!(
        scheduler
            .fail_reserved_goal(&goal_operation.to_string(), "failed".into(), now())
            .unwrap_err()
            .to_string()
            .contains("root task is missing")
    );

    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let root_operation = OperationId::new();
    dispatch
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
            "goal",
            workspace,
            &root_operation.to_string(),
            goal("root"),
            None,
            &root_worker(workspace),
            now(),
        )
        .unwrap();
    let child_operation = OperationId::new();
    scheduler.fail_apply_at(scheduler.apply_calls.get());
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
    scheduler
        .reserve_delegated_dispatch(root_operation, &child_operation.to_string(), "child", now())
        .unwrap();
    scheduler.fail_apply_at(scheduler.apply_calls.get());
    assert!(
        scheduler
            .fail_reserved_delegated_dispatch(&child_operation.to_string(), now())
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
    dispatch
        .upsert_run(DispatchRun {
            run_id: child_operation,
            agent_id: AgentId::new(),
            prompt: "child".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    let reserved = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    scheduler
        .apply(
            &reserved,
            now(),
            SupervisorEventSource::DispatchFailure,
            SupervisorEventKind::Escalate {
                task_id: Some(delegated_task_id(child_operation).unwrap()),
                reason: MISSING_DISPATCH_ESCALATION_REASON.into(),
                safe_evidence: "pre-fix snapshot".into(),
                choices: vec!["resume".into(), "cancel".into()],
            },
        )
        .unwrap();
    scheduler.fail_apply_at(scheduler.apply_calls.get());
    assert!(
        scheduler
            .bind_reserved_delegated_dispatch(
                &child_operation.to_string(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
    scheduler.fail_apply_at(scheduler.apply_calls.get() + 1);
    assert!(
        scheduler
            .bind_reserved_delegated_dispatch(
                &child_operation.to_string(),
                &delegated_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
}

#[test]
fn a_ready_task_without_a_dispatch_reservation_escalates_instead_of_stalling() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let started = scheduler
        .start(
            "caller",
            "operation",
            "root work".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    let mut waker = Waker::default();

    scheduler
        .tick(started.supervisor_run_id, now(), &mut waker)
        .unwrap();

    let stopped = scheduler
        .get("caller", started.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert_eq!(stopped.state, SupervisorRunState::Escalated);
    let escalation = stopped.escalation.unwrap();
    assert_eq!(escalation.blocking_task_id.unwrap().0, "root");
    assert_eq!(
        escalation.reason,
        "no worker dispatch reservation was produced for a ready task"
    );
    assert!(escalation.safe_evidence.contains("runtime/model selection"));
    assert!(waker.wakes.is_empty());
}

#[test]
fn a_dispatch_escalation_persistence_failure_is_reported() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let started = scheduler
        .start(
            "caller",
            "operation",
            "root work".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    scheduler.fail_apply_at(2);

    let error = scheduler
        .tick(started.supervisor_run_id, now(), &mut Waker::default())
        .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("injected supervisor apply failure")
    );
}

#[test]
fn tick_retries_a_partial_parent_wake_and_ignores_nonterminal_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let store = SupervisorStore::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let mut run = SupervisorRun::new(
        "caller".into(),
        "root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    let parent = TaskId::new("parent").unwrap();
    let waiting = TaskId::new("waiting").unwrap();
    let child = TaskId::new("child").unwrap();
    let waiting_run = OperationId::new();
    let child_run = OperationId::new();
    let mut parent_task = task(run.supervisor_run_id, "parent", None);
    parent_task.state = TaskState::Running;
    let mut waiting_task = task(run.supervisor_run_id, "waiting", None);
    waiting_task.state = TaskState::Dispatched;
    let mut child_task = task(run.supervisor_run_id, "child", Some("parent"));
    child_task.state = TaskState::Dispatched;
    run.tasks = BTreeMap::from([
        (parent.clone(), parent_task),
        (waiting.clone(), waiting_task),
        (child.clone(), child_task),
    ]);
    run.provenance.insert(
        waiting.clone(),
        provenance(run.supervisor_run_id, &waiting, None, waiting_run),
    );
    run.provenance.insert(
        child.clone(),
        provenance(
            run.supervisor_run_id,
            &child,
            Some((&parent, OperationId::new())),
            child_run,
        ),
    );
    store.initialize(&run).unwrap();
    for (run_id, status) in [
        (waiting_run, RunStatus::Running),
        (child_run, RunStatus::Completed),
    ] {
        dispatch
            .upsert_run(DispatchRun {
                run_id,
                agent_id: AgentId::new(),
                prompt: "child".into(),
                started_at: now(),
                ended_at: None,
                status,
            })
            .unwrap();
    }

    scheduler.fail_apply_at(1);
    assert!(
        scheduler
            .tick(run.supervisor_run_id, now(), &mut Waker::default())
            .unwrap_err()
            .to_string()
            .contains("injected")
    );
    let scheduler = SupervisorRuntime::new(temp.path());
    scheduler.fail_apply_at(1);
    assert!(
        scheduler
            .tick(run.supervisor_run_id, now(), &mut Waker::default())
            .unwrap_err()
            .to_string()
            .contains("injected")
    );
    let scheduler = SupervisorRuntime::new(temp.path());
    let mut waker = Waker::default();
    scheduler
        .tick(run.supervisor_run_id, now(), &mut waker)
        .unwrap();
    assert_eq!(scheduler.dispatch_registry_reads.get(), 1);
    let saved = store.load(run.supervisor_run_id).unwrap().unwrap();
    assert_eq!(saved.tasks[&waiting].state, TaskState::Dispatched);
    assert_eq!(saved.tasks[&child].state, TaskState::Succeeded);
    assert_eq!(saved.tasks[&parent].state, TaskState::AwaitingDecision);
    assert!(waker.wakes.is_empty());
}
