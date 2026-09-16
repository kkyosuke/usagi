//! artifact の振る舞いを固定するテスト。

use super::*;

#[test]
fn tick_all_reports_tick_failure_after_attempting_wake_delivery() {
    struct RejectingWaker {
        attempted: bool,
    }
    impl DecisionWaker for RejectingWaker {
        fn wake(&mut self, _wake: &DecisionWake) -> Result<()> {
            self.attempted = true;
            anyhow::bail!("injected wake failure")
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let mut retrying = SupervisorRun::new(
        "caller".into(),
        "root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    let retrying_id = TaskId::new("retrying").unwrap();
    let mut due = task(retrying.supervisor_run_id, "retrying", None);
    due.state = TaskState::Retrying;
    due.retry_at = Some(now());
    retrying.tasks.insert(retrying_id, due);
    scheduler.supervisor.initialize(&retrying).unwrap();
    let mut state = RuntimeState::default();
    state
        .wakes
        .insert("reject".into(), wake_reservation(0, false));
    scheduler.save_state(&state).unwrap();
    scheduler.fail_apply_at(0);
    let mut waker = RejectingWaker { attempted: false };
    assert!(
        scheduler
            .tick_all(now(), &mut waker)
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
    assert!(waker.attempted);
}

#[test]
#[allow(clippy::too_many_lines)] // One corruption fixture keeps terminal status, contracts, and provenance fences visibly related.
fn artifact_preparation_rejects_nonterminal_wrong_contract_and_corrupt_membership() {
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
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    persist_root_dispatch_agent(&scheduler, workspace, operation);
    let started = scheduler
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
    let mut missing_workspace = scheduler
        .supervisor
        .load(started.supervisor_run_id)
        .unwrap()
        .unwrap();
    missing_workspace.workspace_id = None;
    scheduler.supervisor.initialize(&missing_workspace).unwrap();
    assert!(
        scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap_err()
            .to_string()
            .contains("workspace is missing")
    );
    missing_workspace.workspace_id = Some(workspace);
    scheduler.supervisor.initialize(&missing_workspace).unwrap();
    assert!(
        scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .is_none()
    );
    assert!(
        scheduler
            .pending_artifact_verifications(now())
            .unwrap()
            .is_empty()
    );

    let mut unsupported_state = scheduler
        .supervisor
        .load(started.supervisor_run_id)
        .unwrap()
        .unwrap();
    unsupported_state
        .tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .state = TaskState::AwaitingDecision;
    scheduler.supervisor.initialize(&unsupported_state).unwrap();
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
    assert!(
        scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .is_none()
    );
    assert!(
        scheduler
            .pending_artifact_verifications(now())
            .unwrap()
            .is_empty()
    );

    let mut wrong_contract = scheduler
        .supervisor
        .load(started.supervisor_run_id)
        .unwrap()
        .unwrap();
    wrong_contract
        .tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .required_artifact_contract = NO_ARTIFACT_CONTRACT;
    scheduler.supervisor.initialize(&wrong_contract).unwrap();
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
    assert!(
        scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .is_none()
    );

    let missing_dispatch = OperationId::new();
    let mut corrupt = wrong_contract;
    let corrupt_root = corrupt
        .tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap();
    corrupt_root.required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
    corrupt_root.state = TaskState::Dispatched;
    corrupt
        .provenance
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .dispatch_run_id = missing_dispatch;
    scheduler.supervisor.initialize(&corrupt).unwrap();
    assert!(
        scheduler
            .pending_artifact_verifications(now())
            .unwrap_err()
            .to_string()
            .contains("provenance fence is stale")
    );
    assert!(
        scheduler
            .prepare_artifact_verification(missing_dispatch, now())
            .unwrap_err()
            .to_string()
            .contains("artifact dispatch is missing")
    );

    let mut missing_task = corrupt.clone();
    let provenance = missing_task
        .provenance
        .remove(&TaskId::new("root").unwrap())
        .unwrap();
    missing_task
        .provenance
        .insert(TaskId::new("missing").unwrap(), provenance);
    scheduler.supervisor.initialize(&missing_task).unwrap();
    assert!(
        scheduler
            .pending_artifact_verifications(now())
            .unwrap_err()
            .to_string()
            .contains("provenance is missing")
    );
    assert!(
        scheduler
            .prepare_artifact_verification(missing_dispatch, now())
            .unwrap_err()
            .to_string()
            .contains("supervisor task is missing")
    );
}

#[test]
fn handoff_rendering_bounds_and_sanitizes_every_report_shape() {
    let mut run = SupervisorRun::new(
        "caller".into(),
        "root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    let mut root = task(run.supervisor_run_id, "root", None);
    root.instruction_body = "é".repeat(MAX_HANDOFF_ROOT_GOAL_BYTES);
    run.tasks.insert(root.task_id.clone(), root);
    for index in 0..MAX_HANDOFF_CONTEXT_ENTRIES {
        let outcome = match index {
            63 => InboxKind::NoReport,
            62 => InboxKind::Failed,
            _ => InboxKind::Completed,
        };
        run.handoff_context.push(HandoffContextEntry {
            task_id: TaskId::new(format!("child-{index}")).unwrap(),
            generation: 1,
            dispatch_run_id: OperationId::new(),
            outcome,
            summary: "s".repeat(MAX_HANDOFF_SUMMARY_BYTES),
            artifacts: (index != 63).then_some("a".repeat(MAX_HANDOFF_ARTIFACT_BYTES)),
            recorded_at: now(),
        });
    }
    let operation = OperationId::new();
    let instruction = "continue with the bounded context";
    let prompt = delegated_handoff_prompt(&run, operation, instruction);
    let suffix = delegated_task_suffix(operation, instruction);
    assert!(prompt.contains("[failed]"));
    assert!(prompt.contains("[no-report]"));
    assert!(prompt.contains("older reports omitted"));
    assert!(prompt.ends_with(&suffix));
    assert!(prompt.len() - suffix.len() <= MAX_HANDOFF_PROMPT_BYTES);

    let mut full = "full".to_owned();
    let full_len = full.len();
    push_bounded_handoff(&mut full, "ignored", full_len);
    assert_eq!(full, "full");
    let mut multibyte = String::new();
    push_bounded_handoff(&mut multibyte, "aébcdef", 5);
    assert_eq!(multibyte, "a…");
    let mut tiny = "xx".to_owned();
    push_bounded_handoff(&mut tiny, "yy", 3);
    assert_eq!(tiny, "xx");

    assert_eq!(
        compact_handoff_text("\n\u{202e}", 16),
        "worker supplied no safe summary text"
    );
    assert_eq!(compact_handoff_text("abc d", 3), "…");
    assert_eq!(compact_handoff_text("aébcd", 5), "a…");
    assert_eq!(
        compact_handoff_text("abcd", 2),
        "worker supplied no safe summary text"
    );
    let many = StructuredResult {
        pr: None,
        commits: vec!["commit".into(); 9],
        changed_files: vec!["file".into(); 10],
        verification: None,
    };
    let artifacts = structured_artifact_summary(&many).unwrap();
    assert!(artifacts.contains("+1 omitted"));
    assert!(artifacts.contains("+2 omitted"));
    assert!(structured_artifact_summary(&StructuredResult::default()).is_none());
}

#[test]
fn artifact_transition_commit_failures_remain_retryable() {
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
    scheduler
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

    scheduler.fail_apply_at(scheduler.apply_calls.get());
    assert!(
        scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
    scheduler.fail_apply_at(scheduler.apply_calls.get() + 1);
    assert!(
        scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
    let request = scheduler
        .prepare_artifact_verification(operation, now())
        .unwrap()
        .unwrap();
    scheduler.fail_apply_at(scheduler.apply_calls.get());
    assert!(
        scheduler
            .prepare_artifact_verification_after_report(
                operation,
                Some(StructuredResult {
                    pr: Some("https://github.com/acme/repo/pull/2".into()),
                    ..StructuredResult::default()
                }),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
    let request = scheduler
        .record_artifact_expectation(&request, &artifact_expectation(), now())
        .unwrap();
    scheduler.fail_apply_at(scheduler.apply_calls.get());
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
            .contains("injected supervisor apply failure")
    );
}

#[test]
fn structured_inbox_report_is_used_for_the_wake_outcome() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let run_id = OperationId::new();
    let caller = CallerRef {
        session_id: Some(SessionId::new()),
        agent_id: AgentId::new(),
    };
    dispatch
        .upsert_binding(DispatchBinding {
            run_id,
            caller: caller.clone(),
            worker: WorkerRef {
                session_id: Some(SessionId::new()),
                agent_id: AgentId::new(),
            },
        })
        .unwrap();
    dispatch
        .append_inbox(
            &caller,
            InboxMessage {
                run_id,
                from: WorkerRef {
                    session_id: Some(SessionId::new()),
                    agent_id: AgentId::new(),
                },
                kind: InboxKind::Failed,
                summary: "safe failure".into(),
                result: None,
                created_at: now(),
                read: false,
            },
        )
        .unwrap();
    assert_eq!(
        scheduler.outcome(run_id, InboxKind::Completed).unwrap(),
        WakeOutcome {
            kind: InboxKind::Failed,
            summary: "safe failure".into(),
        }
    );
}
