//! lifecycle の振る舞いを固定するテスト。

use super::*;

#[test]
fn core_reducer_projects_dependents_and_keeps_terminal_cancellation_idempotent() {
    let mut run = SupervisorRun::new(
        "caller".into(),
        "reducer".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    run.state = SupervisorRunState::Running;
    let root_id = TaskId::new("root").unwrap();
    let mut root = task(run.supervisor_run_id, "root", None);
    root.state = TaskState::Running;
    let child_id = TaskId::new("child").unwrap();
    let mut child = task(run.supervisor_run_id, "child", Some("root"));
    child.dependencies.insert(root_id.clone());
    run.tasks.insert(root_id.clone(), root);
    run.tasks.insert(child_id.clone(), child);

    let succeeded = event(
        &run,
        SupervisorEventKind::SetTaskState {
            task_id: root_id.clone(),
            generation: 1,
            state: TaskState::Succeeded,
        },
    );
    reduce(&mut run, &succeeded).unwrap();
    assert_eq!(run.tasks[&child_id].state, TaskState::Ready);
    let cancel_terminal = event(
        &run,
        SupervisorEventKind::Cancel {
            task_id: Some(root_id.clone()),
            reason: "late cancellation replay".into(),
        },
    );
    reduce(&mut run, &cancel_terminal).unwrap();
    assert_eq!(run.tasks[&root_id].state, TaskState::Succeeded);
}

#[test]
#[allow(clippy::too_many_lines)] // Durable replay compares each field independently, including legacy backfill phases.
fn start_reservation_replay_validates_and_backfills_every_identity_field() {
    let workspace = WorkspaceId::new();

    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let operation = OperationId::new().to_string();
    scheduler
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &operation,
            goal("workspace fence"),
            None,
            now(),
        )
        .unwrap();
    assert!(
        scheduler
            .reserve_goal_for_workspace(
                "goal",
                WorkspaceId::new(),
                &operation,
                goal("workspace fence"),
                None,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("different workspace")
    );

    let mut state = scheduler.load_state().unwrap();
    state.starts.get_mut(&operation).unwrap().workspace_id = None;
    scheduler.save_state(&state).unwrap();
    assert!(
        scheduler
            .reserve_goal_for_workspace(
                "goal",
                WorkspaceId::new(),
                &operation,
                goal("workspace fence"),
                None,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("different workspace")
    );
    let mut state = scheduler.load_state().unwrap();
    state
        .starts
        .get_mut(&operation)
        .unwrap()
        .artifact_repository = None;
    scheduler.save_state(&state).unwrap();
    scheduler
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &operation,
            goal("workspace fence"),
            None,
            now(),
        )
        .unwrap();

    let repository_temp = tempfile::tempdir().unwrap();
    let repository = SupervisorRuntime::new(repository_temp.path());
    let repository_operation = OperationId::new().to_string();
    let repository_run = repository
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &repository_operation,
            goal("repository fence"),
            None,
            now(),
        )
        .unwrap();
    let mut repository_state = repository.load_state().unwrap();
    repository_state
        .starts
        .get_mut(&repository_operation)
        .unwrap()
        .artifact_repository = None;
    repository.save_state(&repository_state).unwrap();
    let mut repository_snapshot = repository
        .supervisor
        .load(repository_run.supervisor_run_id)
        .unwrap()
        .unwrap();
    repository_snapshot.artifact_repository =
        Some(GitHubRepository::from_name_with_owner("other/repository").unwrap());
    repository
        .supervisor
        .initialize(&repository_snapshot)
        .unwrap();
    assert!(
        repository
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &repository_operation,
                goal("repository fence"),
                None,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("different artifact repository")
    );

    let profile_temp = tempfile::tempdir().unwrap();
    let profile = SupervisorRuntime::new(profile_temp.path());
    let profile_operation = OperationId::new().to_string();
    profile
        .reserve_goal_for_workspace_with_profile(
            "goal",
            workspace,
            &profile_operation,
            goal("profile fence"),
            AgentProfileId::new("claude").unwrap(),
            "digest-a".into(),
            None,
            now(),
        )
        .unwrap();
    assert!(
        profile
            .reserve_goal_for_workspace_with_profile(
                "goal",
                workspace,
                &profile_operation,
                goal("profile fence"),
                AgentProfileId::new("codex").unwrap(),
                "digest-a".into(),
                None,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("different Agent runtime")
    );
    assert!(
        profile
            .reserve_goal_for_workspace_with_profile(
                "goal",
                workspace,
                &profile_operation,
                goal("profile fence"),
                AgentProfileId::new("claude").unwrap(),
                "digest-b".into(),
                None,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("different Agent intent")
    );
    let mut profile_state = profile.load_state().unwrap();
    profile_state
        .starts
        .get_mut(&profile_operation)
        .unwrap()
        .artifact_repository =
        Some(GitHubRepository::from_name_with_owner("other/repository").unwrap());
    profile.save_state(&profile_state).unwrap();
    assert!(
        profile
            .reserve_goal_for_workspace_with_profile(
                "goal",
                workspace,
                &profile_operation,
                goal("profile fence"),
                AgentProfileId::new("claude").unwrap(),
                "digest-a".into(),
                None,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("different artifact repository")
    );

    let caller_temp = tempfile::tempdir().unwrap();
    let caller = SupervisorRuntime::new(caller_temp.path());
    let first_dispatch = OperationId::new();
    let first_worker = root_worker(workspace);
    persist_caller_dispatch(&caller, workspace, first_dispatch, &first_worker);
    let caller_start = OperationId::new().to_string();
    caller
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &caller_start,
            "caller fence".into(),
            None,
            first_dispatch,
            &first_worker,
            now(),
        )
        .unwrap();
    caller
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &caller_start,
            "caller fence".into(),
            None,
            first_dispatch,
            &first_worker,
            now(),
        )
        .unwrap();
    let second_dispatch = OperationId::new();
    let second_worker = root_worker(workspace);
    persist_caller_dispatch(&caller, workspace, second_dispatch, &second_worker);
    assert!(
        caller
            .start_for_workspace_caller_dispatch(
                "caller",
                workspace,
                &caller_start,
                "caller fence".into(),
                None,
                second_dispatch,
                &second_worker,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("different caller dispatch")
    );

    let backfill_temp = tempfile::tempdir().unwrap();
    let backfill = SupervisorRuntime::new(backfill_temp.path());
    let backfill_start = OperationId::new().to_string();
    backfill
        .start_for_workspace(
            "caller",
            workspace,
            &backfill_start,
            "legacy root".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    let backfill_dispatch = OperationId::new();
    let backfill_worker = root_worker(workspace);
    persist_caller_dispatch(&backfill, workspace, backfill_dispatch, &backfill_worker);
    backfill
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &backfill_start,
            "legacy root".into(),
            None,
            backfill_dispatch,
            &backfill_worker,
            now(),
        )
        .unwrap();
    let backfilled = backfill.load_state().unwrap();
    assert_eq!(
        backfilled.starts[&backfill_start].caller_dispatch_run_id,
        Some(backfill_dispatch)
    );

    let missing_temp = tempfile::tempdir().unwrap();
    let missing = SupervisorRuntime::new(missing_temp.path());
    let mut missing_state = backfill.load_state().unwrap();
    let missing_reservation = missing_state.starts.get_mut(&backfill_start).unwrap();
    missing_reservation.workspace_id = None;
    missing_reservation.caller_dispatch_run_id = None;
    missing_reservation.worker_session_id = None;
    missing_reservation.worker_agent_id = None;
    missing_reservation.worker_runtime_id = None;
    missing_reservation.worker_profile_id = None;
    missing_reservation.worker_semantic_digest = None;
    missing.save_state(&missing_state).unwrap();
    assert!(
        missing
            .start_for_workspace(
                "caller",
                workspace,
                &backfill_start,
                "legacy root".into(),
                Vec::new(),
                None,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("no durable workspace authority")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Every field/count boundary belongs to one admission matrix.
fn start_input_limits_are_utf8_byte_bounds_before_any_durable_effect() {
    let exact_root = format!("{}a", "う".repeat((MAX_SUPERVISOR_TEXT_BYTES - 1) / 3));
    let exact_id = format!(
        "{}aa",
        "う".repeat((usagi_core::domain::supervisor::MAX_TASK_ID_BYTES - 2) / 3)
    );
    let task = InitialTask {
        task_id: exact_id,
        parent_task_id: None,
        dependencies: Vec::new(),
        instruction: "work".into(),
        required_artifact_contract: NO_ARTIFACT_CONTRACT,
    };
    assert_eq!(exact_root.len(), MAX_SUPERVISOR_TEXT_BYTES);
    validate_start_input(
        "operation",
        &exact_root,
        std::slice::from_ref(&task),
        Some("policy"),
    )
    .unwrap();
    assert!(
        validate_start_input("operation", &(exact_root + "x"), &[task], None,)
            .unwrap_err()
            .to_string()
            .contains("root task")
    );
    assert!(
        validate_start_input(&"x".repeat(MAX_SUPERVISOR_KEY_BYTES + 1), "root", &[], None,)
            .is_err()
    );
    assert!(
        validate_start_input(
            "operation",
            "root",
            &vec![
                InitialTask {
                    task_id: "task".into(),
                    parent_task_id: None,
                    dependencies: Vec::new(),
                    instruction: "work".into(),
                    required_artifact_contract: NO_ARTIFACT_CONTRACT,
                };
                MAX_INITIAL_TASKS + 1
            ],
            None,
        )
        .is_err()
    );
    for invalid in [
        InitialTask {
            task_id: "x".repeat(usagi_core::domain::supervisor::MAX_TASK_ID_BYTES + 1),
            parent_task_id: None,
            dependencies: Vec::new(),
            instruction: "work".into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
        },
        InitialTask {
            task_id: "task".into(),
            parent_task_id: Some("x".repeat(usagi_core::domain::supervisor::MAX_TASK_ID_BYTES + 1)),
            dependencies: Vec::new(),
            instruction: "work".into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
        },
        InitialTask {
            task_id: "task".into(),
            parent_task_id: None,
            dependencies: vec!["dependency".into(); MAX_TASK_DEPENDENCIES + 1],
            instruction: "work".into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
        },
        InitialTask {
            task_id: "task".into(),
            parent_task_id: None,
            dependencies: vec!["x".repeat(usagi_core::domain::supervisor::MAX_TASK_ID_BYTES + 1)],
            instruction: "work".into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
        },
        InitialTask {
            task_id: "task".into(),
            parent_task_id: None,
            dependencies: Vec::new(),
            instruction: "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
        },
    ] {
        assert!(validate_start_input("operation", "root", &[invalid], None).is_err());
    }
    assert!(validate_start_input("operation", "root", &[], Some("")).is_err());
    assert!(
        serde_json::from_value::<InitialTask>(serde_json::json!({
            "task_id": "task",
            "instruction": "work",
            "required_artifact_contract": "unsupported"
        }))
        .is_err()
    );

    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    assert!(
        scheduler
            .start(
                "caller",
                "operation",
                "x".repeat(MAX_SUPERVISOR_TEXT_BYTES + 1),
                Vec::new(),
                None,
                now(),
            )
            .is_err()
    );
    assert!(!scheduler.state_path.exists());
    assert!(!temp.path().join("supervisor-runs").exists());
}

#[test]
fn tick_reconciles_only_retries_whose_durable_deadline_is_due() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let store = SupervisorStore::new(temp.path());
    let mut run = SupervisorRun::new(
        "caller".into(),
        "root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    let due_id = TaskId::new("due").unwrap();
    let future_id = TaskId::new("future").unwrap();
    let mut due = task(run.supervisor_run_id, "due", None);
    due.state = TaskState::Retrying;
    due.retry_at = Some(now());
    let mut future = task(run.supervisor_run_id, "future", None);
    future.state = TaskState::Retrying;
    future.retry_at = Some(now() + chrono::Duration::seconds(1));
    run.tasks = BTreeMap::from([(due_id.clone(), due), (future_id.clone(), future)]);
    store.initialize(&run).unwrap();

    scheduler.fail_apply_at(0);
    assert!(
        scheduler
            .tick(run.supervisor_run_id, now(), &mut Waker::default())
            .unwrap_err()
            .to_string()
            .contains("injected")
    );
    let scheduler = SupervisorRuntime::new(temp.path());
    scheduler
        .tick(run.supervisor_run_id, now(), &mut Waker::default())
        .unwrap();

    let saved = store.load(run.supervisor_run_id).unwrap().unwrap();
    assert_eq!(saved.tasks[&due_id].state, TaskState::Ready);
    assert_eq!(saved.tasks[&future_id].state, TaskState::Retrying);
}

#[test]
fn start_propagates_each_injected_partial_apply_failure() {
    for fail_at in 0..=2 {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        scheduler.fail_apply_at(fail_at);
        let operation = format!("operation-{fail_at}");
        let initial_tasks = vec![InitialTask {
            task_id: "child".into(),
            parent_task_id: None,
            dependencies: vec!["root".into()],
            instruction: "child".into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
        }];
        assert!(
            scheduler
                .start(
                    "caller",
                    &operation,
                    "root".into(),
                    initial_tasks.clone(),
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains("injected")
        );
        let recovered = scheduler
            .start(
                "caller",
                &operation,
                "root".into(),
                initial_tasks,
                None,
                now(),
            )
            .unwrap();
        assert_eq!(recovered.state, SupervisorRunState::Running);
        assert_eq!(recovered.tasks.len(), 2);
    }
}

#[test]
fn start_recovery_refuses_inconsistent_partial_snapshots() {
    for (fail_at, expected) in [
        (0, "reservation does not match"),
        (1, "root task conflicts"),
        (2, "initial task conflicts"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let operation = format!("inconsistent-{fail_at}");
        let initial_tasks = vec![InitialTask {
            task_id: "child".into(),
            parent_task_id: None,
            dependencies: vec!["root".into()],
            instruction: "child".into(),
            required_artifact_contract: NO_ARTIFACT_CONTRACT,
        }];
        scheduler.fail_apply_at(fail_at);
        assert!(
            scheduler
                .start(
                    "caller",
                    &operation,
                    "root".into(),
                    initial_tasks.clone(),
                    None,
                    now(),
                )
                .is_err()
        );
        let id = scheduler.load_state().unwrap().starts[&operation].supervisor_run_id;
        let mut run = scheduler.supervisor.load(id).unwrap().unwrap();
        if fail_at == 0 {
            run.root_caller_ref = "other-caller".into();
        } else if fail_at == 1 {
            run.tasks
                .get_mut(&TaskId::new("root").unwrap())
                .unwrap()
                .instruction_body = "other root".into();
        } else {
            run.tasks
                .get_mut(&TaskId::new("child").unwrap())
                .unwrap()
                .instruction_body = "other child".into();
        }
        scheduler.supervisor.initialize(&run).unwrap();
        assert!(
            scheduler
                .start(
                    "caller",
                    &operation,
                    "root".into(),
                    initial_tasks,
                    None,
                    now(),
                )
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
    }
}

#[test]
fn tick_skips_blocked_provenance_and_accepts_terminal_work_without_a_parent() {
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
    let blocked = TaskId::new("blocked").unwrap();
    let standalone = TaskId::new("standalone").unwrap();
    let terminal_parent = TaskId::new("terminal-parent").unwrap();
    let blocked_run = OperationId::new();
    let standalone_run = OperationId::new();
    let mut blocked_task = task(run.supervisor_run_id, "blocked", None);
    blocked_task.state = TaskState::AwaitingDecision;
    let mut standalone_task = task(run.supervisor_run_id, "standalone", None);
    standalone_task.state = TaskState::Dispatched;
    let mut terminal_parent_task = task(run.supervisor_run_id, "terminal-parent", None);
    terminal_parent_task.state = TaskState::Succeeded;
    run.tasks = BTreeMap::from([
        (blocked.clone(), blocked_task),
        (standalone.clone(), standalone_task),
        (terminal_parent.clone(), terminal_parent_task),
    ]);
    run.provenance.insert(
        blocked.clone(),
        provenance(run.supervisor_run_id, &blocked, None, blocked_run),
    );
    run.provenance.insert(
        standalone.clone(),
        provenance(run.supervisor_run_id, &standalone, None, standalone_run),
    );
    scheduler
        .reserve_parent_wake(
            &mut run,
            &terminal_parent,
            OperationId::new(),
            InboxKind::Completed,
            now(),
        )
        .unwrap();
    store.initialize(&run).unwrap();
    for run_id in [blocked_run, standalone_run] {
        dispatch
            .upsert_run(DispatchRun {
                run_id,
                agent_id: AgentId::new(),
                prompt: "work".into(),
                started_at: now(),
                ended_at: Some(now()),
                status: RunStatus::Completed,
            })
            .unwrap();
    }

    scheduler
        .tick(run.supervisor_run_id, now(), &mut Waker::default())
        .unwrap();
    let saved = store.load(run.supervisor_run_id).unwrap().unwrap();
    assert_eq!(saved.tasks[&blocked].state, TaskState::AwaitingDecision);
    assert_eq!(saved.tasks[&standalone].state, TaskState::Succeeded);
}

#[test]
#[allow(clippy::too_many_lines)] // The fixture is a complete durable history.
fn completion_is_reconciled_once_and_restart_does_not_duplicate_the_parent_wake() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let store = SupervisorStore::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let initial = SupervisorRun::new(
        "caller".into(),
        "root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    let id = initial.supervisor_run_id;
    store.initialize(&initial).unwrap();
    let parent_id = TaskId::new("parent").unwrap();
    let child_id = TaskId::new("child").unwrap();
    let parent_run = OperationId::new();
    let child_run = OperationId::new();
    let mut run = store.load(id).unwrap().unwrap();
    run = store
        .apply(
            id,
            run.state_revision,
            &event(
                &run,
                SupervisorEventKind::SetRunState {
                    state: SupervisorRunState::Running,
                    terminal_reason: None,
                },
            ),
        )
        .unwrap();
    run = store
        .apply(
            id,
            run.state_revision,
            &event(
                &run,
                SupervisorEventKind::AddTask {
                    task: task(id, "parent", None),
                },
            ),
        )
        .unwrap();
    run = store
        .apply(
            id,
            run.state_revision,
            &event(
                &run,
                SupervisorEventKind::Dispatch {
                    task_id: parent_id.clone(),
                    generation: 1,
                    provenance: provenance(id, &parent_id, None, parent_run),
                },
            ),
        )
        .unwrap();
    run = store
        .apply(
            id,
            run.state_revision,
            &event(
                &run,
                SupervisorEventKind::Running {
                    task_id: parent_id.clone(),
                    generation: 1,
                },
            ),
        )
        .unwrap();
    run = store
        .apply(
            id,
            run.state_revision,
            &event(
                &run,
                SupervisorEventKind::AddTask {
                    task: task(id, "child", Some("parent")),
                },
            ),
        )
        .unwrap();
    let _ = store
        .apply(
            id,
            run.state_revision,
            &event(
                &run,
                SupervisorEventKind::Dispatch {
                    task_id: child_id.clone(),
                    generation: 1,
                    provenance: provenance(
                        id,
                        &child_id,
                        Some((&parent_id, parent_run)),
                        child_run,
                    ),
                },
            ),
        )
        .unwrap();
    dispatch
        .upsert_run(DispatchRun {
            run_id: child_run,
            agent_id: AgentId::new(),
            prompt: "child".into(),
            started_at: now(),
            ended_at: Some(now()),
            status: RunStatus::Completed,
        })
        .unwrap();
    let mut waker = Waker::default();
    scheduler.tick(id, now(), &mut waker).unwrap();
    let saved = store.load(id).unwrap().unwrap();
    assert_eq!(saved.tasks[&child_id].state, TaskState::Succeeded);
    assert_eq!(saved.tasks[&parent_id].state, TaskState::Running);
    assert_eq!(waker.wakes.len(), 1);
    assert_eq!(waker.wakes[0].child_run_id, child_run);

    dispatch
        .upsert_run(DispatchRun {
            run_id: parent_run,
            agent_id: AgentId::new(),
            prompt: "parent".into(),
            started_at: now(),
            ended_at: Some(now()),
            status: RunStatus::Completed,
        })
        .unwrap();
    scheduler.tick(id, now(), &mut waker).unwrap();
    let saved = store.load(id).unwrap().unwrap();
    assert_eq!(saved.tasks[&parent_id].state, TaskState::Succeeded);
    assert_eq!(saved.state, SupervisorRunState::Succeeded);
    assert_eq!(waker.wakes.len(), 1);

    let restarted = SupervisorRuntime::new(temp.path());
    restarted.tick(id, now(), &mut waker).unwrap();
    assert_eq!(waker.wakes.len(), 1);
}

// 1 つの決定表を分けると読み手が追う状態が増えるため、この関数はまとめて置く。
#[allow(clippy::too_many_lines)]
#[test]
fn control_surface_is_idempotent_owned_and_durable() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let initial = vec![InitialTask {
        task_id: "child".into(),
        parent_task_id: None,
        dependencies: vec!["root".into()],
        instruction: "secret child instruction".into(),
        required_artifact_contract: NO_ARTIFACT_CONTRACT,
    }];
    let started = runtime
        .start(
            "caller-a",
            "operation-a",
            "secret root instruction".into(),
            initial.clone(),
            None,
            now(),
        )
        .unwrap();
    assert_eq!(started.state, SupervisorRunState::Running);
    assert_eq!(started.tasks.len(), 2);
    assert_eq!(
        started
            .tasks
            .iter()
            .find(|task| task.task_id.0 == "child")
            .and_then(|task| task.parent_task_id.as_ref())
            .map(|task| task.0.as_str()),
        Some("root")
    );
    assert_eq!(
        runtime
            .start(
                "caller-a",
                "operation-a",
                "secret root instruction".into(),
                initial,
                None,
                now(),
            )
            .unwrap()
            .supervisor_run_id,
        started.supervisor_run_id
    );
    assert!(
        runtime
            .start(
                "caller-a",
                "operation-a",
                "different".into(),
                vec![],
                None,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("reused")
    );
    assert!(
        runtime
            .get("caller-b", started.supervisor_run_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        runtime
            .get("caller-a", started.supervisor_run_id)
            .unwrap()
            .unwrap(),
        started
    );
    assert_eq!(
        runtime
            .list("caller-a", Some(SupervisorRunState::Running))
            .unwrap()
            .len(),
        1
    );
    let page = runtime
        .list_page("caller-a", Some(SupervisorRunState::Running), 0, 1)
        .unwrap();
    assert_eq!(page.runs.len(), 1);
    assert!(page.next_cursor.is_none());
    assert!(
        runtime
            .list_page("caller-b", None, 0, 1)
            .unwrap()
            .runs
            .is_empty()
    );
    let (events, cursor) = runtime
        .events("caller-a", started.supervisor_run_id, 0, 10)
        .unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(cursor.next_sequence, 4);
    assert!(
        runtime
            .events("caller-b", started.supervisor_run_id, 0, 10)
            .unwrap_err()
            .to_string()
            .contains("does not exist")
    );
    assert!(
        runtime
            .cancel(
                "caller-b",
                started.supervisor_run_id,
                "foreign".into(),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("does not exist")
    );
    assert!(
        runtime
            .resolve_escalation(
                "caller-b",
                started.supervisor_run_id,
                OperationId::new(),
                EscalationDecision::Resume,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("does not exist")
    );
    let run = runtime
        .supervisor
        .load(started.supervisor_run_id)
        .unwrap()
        .unwrap();
    let escalated = runtime
        .apply(
            &run,
            now(),
            SupervisorEventSource::Admission,
            SupervisorEventKind::Escalate {
                task_id: None,
                reason: "operator decision required".into(),
                safe_evidence: "safe evidence".into(),
                choices: vec!["resume".into()],
            },
        )
        .unwrap();
    let escalation_id = escalated.escalation.as_ref().unwrap().escalation_id;
    let resumed = runtime
        .resolve_escalation(
            "caller-a",
            started.supervisor_run_id,
            escalation_id,
            EscalationDecision::Resume,
            now(),
        )
        .unwrap();
    assert_eq!(resumed.state, SupervisorRunState::Running);
    let cancelled = runtime
        .cancel(
            "caller-a",
            started.supervisor_run_id,
            "operator requested".into(),
            now(),
        )
        .unwrap();
    assert_eq!(cancelled.state, SupervisorRunState::Cancelled);
    assert_eq!(
        SupervisorRuntime::new(temp.path())
            .list("caller-a", None)
            .unwrap()
            .len(),
        1
    );
    runtime.tick_all(now(), &mut Waker::default()).unwrap();
}

#[test]
fn cancel_rejects_unsafe_or_unbounded_presented_reasons() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let run = runtime
        .start(
            "caller",
            "operation",
            "root".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    for reason in [
        String::new(),
        "clear\u{1b}[2J".into(),
        "line\nbreak".into(),
        "direction\u{202e}override".into(),
        "x".repeat(MAX_SUPERVISOR_REASON_BYTES + 1),
    ] {
        assert!(
            runtime
                .cancel("caller", run.supervisor_run_id, reason, now())
                .is_err()
        );
    }
    assert_eq!(
        runtime
            .cancel(
                "caller",
                run.supervisor_run_id,
                "operator requested stop".into(),
                now(),
            )
            .unwrap()
            .state,
        SupervisorRunState::Cancelled
    );
}

#[test]
fn workspace_delete_is_terminal_revisioned_and_scoped() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();

    let mut active = SupervisorRun::new(
        "goal-composer".into(),
        "active".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    active.workspace_id = Some(workspace);
    runtime.supervisor.initialize(&active).unwrap();
    let active_command = SupervisorWorkspaceCommand::Delete {
        supervisor_run_id: active.supervisor_run_id,
        observed_state_revision: active.state_revision,
    };
    assert!(
        runtime
            .delete_for_workspace(workspace, OperationId::new(), &active_command, now())
            .unwrap_err()
            .to_string()
            .contains("must finish")
    );

    let finished = aborted_run(Some(workspace));
    let id = finished.supervisor_run_id;
    let revision = finished.state_revision;
    runtime.supervisor.initialize(&finished).unwrap();
    let command = SupervisorWorkspaceCommand::Delete {
        supervisor_run_id: id,
        observed_state_revision: revision,
    };
    assert!(
        runtime
            .control_for_workspace(workspace, OperationId::new(), &command, now())
            .unwrap_err()
            .to_string()
            .contains("delete control path")
    );
    assert!(
        runtime
            .delete_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: id,
                    reason: "not a deletion".into(),
                },
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("delete command is required")
    );
    let expired_operation = OperationId::new();
    let mut durable_state = runtime.load_state().unwrap();
    durable_state
        .expired_controls
        .insert(&expired_operation.to_string());
    runtime.save_state(&durable_state).unwrap();
    assert!(
        runtime
            .delete_for_workspace(workspace, expired_operation, &command, now())
            .unwrap_err()
            .to_string()
            .contains("outside the retained replay window")
    );
    assert!(
        runtime
            .delete_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Delete {
                    supervisor_run_id: id,
                    observed_state_revision: revision + 1,
                },
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("stale supervisor state revision")
    );
    assert!(
        runtime
            .delete_for_workspace(WorkspaceId::new(), OperationId::new(), &command, now())
            .unwrap_err()
            .to_string()
            .contains("does not belong")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Deletion durability and both root reservation forms are one replay contract.
fn workspace_delete_is_durable_and_replayable() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let finished = aborted_run(Some(workspace));
    let id = finished.supervisor_run_id;
    let revision = finished.state_revision;
    runtime.supervisor.initialize(&finished).unwrap();
    let command = SupervisorWorkspaceCommand::Delete {
        supervisor_run_id: id,
        observed_state_revision: revision,
    };

    let operation = OperationId::new();
    let receipt = runtime
        .delete_for_workspace(workspace, operation, &command, now())
        .unwrap();
    assert_eq!(receipt.supervisor_run_id, id);
    assert_eq!(receipt.state_revision, revision);
    assert!(runtime.supervisor.load(id).unwrap().is_none());
    assert!(runtime.list_workspace(workspace).unwrap().is_empty());

    let restarted = SupervisorRuntime::new(temp.path());
    assert_eq!(
        restarted
            .delete_for_workspace(workspace, operation, &command, now())
            .unwrap(),
        receipt
    );
    assert!(
        restarted
            .delete_for_workspace(
                workspace,
                operation,
                &SupervisorWorkspaceCommand::Delete {
                    supervisor_run_id: id,
                    observed_state_revision: revision + 1,
                },
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("conflicts with its reservation")
    );
    assert!(
        restarted
            .delete_for_workspace(workspace, OperationId::new(), &command, now())
            .unwrap_err()
            .to_string()
            .contains("does not exist")
    );

    let reserved_temp = tempfile::tempdir().unwrap();
    let reserved = SupervisorRuntime::new(reserved_temp.path());
    let start_operation = OperationId::new();
    let started = reserved
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &start_operation.to_string(),
            goal("delete reserved root"),
            None,
            now(),
        )
        .unwrap();
    let terminal = reserved
        .control_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: started.supervisor_run_id,
                reason: "delete".into(),
            },
            now(),
        )
        .unwrap();
    reserved
        .delete_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Delete {
                supervisor_run_id: terminal.supervisor_run_id,
                observed_state_revision: terminal.state_revision,
            },
            now(),
        )
        .unwrap();
    let state = reserved.load_state().unwrap();
    assert!(!state.starts.contains_key(&start_operation.to_string()));
    assert!(state.expired_starts.contains(&start_operation.to_string()));

    let caller_temp = tempfile::tempdir().unwrap();
    let caller = SupervisorRuntime::new(caller_temp.path());
    let caller_dispatch = OperationId::new();
    let caller_worker = root_worker(workspace);
    persist_caller_dispatch(&caller, workspace, caller_dispatch, &caller_worker);
    let caller_start = OperationId::new().to_string();
    let started = caller
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &caller_start,
            "delete caller root".into(),
            None,
            caller_dispatch,
            &caller_worker,
            now(),
        )
        .unwrap();
    let terminal = caller
        .control_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: started.supervisor_run_id,
                reason: "delete".into(),
            },
            now(),
        )
        .unwrap();
    caller
        .delete_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Delete {
                supervisor_run_id: terminal.supervisor_run_id,
                observed_state_revision: terminal.state_revision,
            },
            now(),
        )
        .unwrap();
    let state = caller.load_state().unwrap();
    assert!(state.expired_starts.contains(&caller_start));
    assert!(state.expired_starts.contains(&caller_dispatch.to_string()));
}

#[test]
fn expired_workspace_control_operations_are_refused_before_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let run = runtime
        .start_for_workspace(
            "caller",
            workspace,
            "start",
            "root".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    let operation = OperationId::new();
    let mut state = RuntimeState::default();
    state.expired_controls.insert(&operation.to_string());
    runtime.save_state(&state).unwrap();
    assert!(
        runtime
            .control_for_workspace(
                workspace,
                operation,
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: run.supervisor_run_id,
                    reason: "operator cancelled".into(),
                },
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("outside the retained replay window")
    );

    let recyclable_temp = tempfile::tempdir().unwrap();
    let recyclable = SupervisorRuntime::new(recyclable_temp.path());
    let recyclable_run = recyclable
        .start_for_workspace(
            "caller",
            workspace,
            "recyclable",
            "root".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    let mut state = RuntimeState::default();
    for _ in 0..MAX_CONTROL_RESERVATIONS {
        state.controls.insert(
            OperationId::new().to_string(),
            ControlReservation {
                semantic_digest: format!("sha256:{}", "0".repeat(64)),
                supervisor_run_id: SupervisorRunId::new(),
                reserved_at: now(),
            },
        );
    }
    recyclable.save_state(&state).unwrap();
    assert_eq!(
        recyclable
            .control_for_workspace(
                workspace,
                OperationId::new(),
                &SupervisorWorkspaceCommand::Cancel {
                    supervisor_run_id: recyclable_run.supervisor_run_id,
                    reason: "operator cancelled".into(),
                },
                now(),
            )
            .unwrap()
            .state,
        SupervisorRunState::Cancelled
    );
}

#[test]
fn start_rejects_an_unresolvable_initial_dag() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let error = runtime
        .start(
            "caller",
            "operation",
            "root".into(),
            vec![InitialTask {
                task_id: "child".into(),
                parent_task_id: None,
                dependencies: vec!["missing".into()],
                instruction: "child".into(),
                required_artifact_contract: NO_ARTIFACT_CONTRACT,
            }],
            Some("strict".into()),
            now(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("missing dependency or cycle"));
    let parsed: InitialTask = serde_json::from_value(serde_json::json!({
        "task_id": "default-contract",
        "instruction": "body"
    }))
    .unwrap();
    assert_eq!(parsed.required_artifact_contract, NO_ARTIFACT_CONTRACT);
}

#[test]
fn control_helpers_validate_and_project_both_commands() {
    let run = SupervisorRun::new(
        "caller".into(),
        "task".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    let cancel = SupervisorWorkspaceCommand::Cancel {
        supervisor_run_id: run.supervisor_run_id,
        reason: "operator cancelled".into(),
    };
    validate_control_command(&cancel).unwrap();
    let invalid = SupervisorWorkspaceCommand::Cancel {
        supervisor_run_id: run.supervisor_run_id,
        reason: "line\nbreak".into(),
    };
    assert!(validate_control_command(&invalid).is_err());
    let cancel_digest = control_semantic_digest(&cancel).unwrap();
    assert!(cancel_digest.starts_with("sha256:"));
    let event = control_event(
        &run,
        OperationId::new(),
        cancel_digest.clone(),
        &cancel,
        now(),
    );
    assert_eq!(event.source, SupervisorEventSource::Cancel);
    assert!(matches!(
        event.kind,
        SupervisorEventKind::Cancel { task_id: None, ref reason }
            if reason == "operator cancelled"
    ));

    let escalation_id = OperationId::new();
    let resolve = SupervisorWorkspaceCommand::ResolveEscalation {
        supervisor_run_id: run.supervisor_run_id,
        escalation_id,
        decision: EscalationDecision::Fail,
    };
    validate_control_command(&resolve).unwrap();
    let resolve_digest = control_semantic_digest(&resolve).unwrap();
    assert_ne!(resolve_digest, cancel_digest);
    let event = control_event(&run, OperationId::new(), resolve_digest, &resolve, now());
    assert_eq!(event.source, SupervisorEventSource::Admission);
    assert!(matches!(
        event.kind,
        SupervisorEventKind::ResolveEscalation {
            escalation_id: actual,
            decision: EscalationDecision::Fail,
        } if actual == escalation_id
    ));
}
