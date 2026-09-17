//! promotion の振る舞いを固定するテスト。

use super::*;

#[test]
fn retry_resolution_checks_every_run_escalation_and_agent_fence() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    assert!(
        scheduler
            .retry_work_for_workspace(workspace, SupervisorRunId::new(), OperationId::new(),)
            .unwrap()
            .is_none()
    );

    let (run, task_id, dispatch_run_id, escalation_id) = escalated_retry_run(workspace);
    scheduler.supervisor.initialize(&run).unwrap();
    assert!(
        scheduler
            .retry_work_for_workspace(WorkspaceId::new(), run.supervisor_run_id, escalation_id,)
            .unwrap()
            .is_none()
    );
    assert!(
        scheduler
            .retry_work_for_workspace(workspace, run.supervisor_run_id, OperationId::new(),)
            .unwrap_err()
            .to_string()
            .contains("escalation fence is stale")
    );

    let mut without_blocker = run.clone();
    without_blocker
        .escalation
        .as_mut()
        .unwrap()
        .blocking_task_id = None;
    scheduler.supervisor.initialize(&without_blocker).unwrap();
    assert!(
        scheduler
            .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
            .unwrap()
            .is_none()
    );

    let mut ordinary = run.clone();
    ordinary.tasks.get_mut(&task_id).unwrap().state = TaskState::Ready;
    scheduler.supervisor.initialize(&ordinary).unwrap();
    assert!(
        scheduler
            .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
            .unwrap()
            .is_none()
    );

    let mut without_provenance = run.clone();
    without_provenance.provenance.clear();
    scheduler
        .supervisor
        .initialize(&without_provenance)
        .unwrap();
    assert!(
        scheduler
            .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
            .unwrap_err()
            .to_string()
            .contains("retry provenance is missing")
    );

    let mut without_task = run.clone();
    without_task.tasks.clear();
    scheduler.supervisor.initialize(&without_task).unwrap();
    assert!(
        scheduler
            .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
            .unwrap_err()
            .to_string()
            .contains("retry task is missing")
    );

    let mut stale = run.clone();
    stale.tasks.get_mut(&task_id).unwrap().generation += 1;
    scheduler.supervisor.initialize(&stale).unwrap();
    assert!(
        scheduler
            .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
            .unwrap_err()
            .to_string()
            .contains("retry provenance fence is stale")
    );

    scheduler.supervisor.initialize(&run).unwrap();
    let retry = scheduler
        .retry_work_for_workspace(workspace, run.supervisor_run_id, escalation_id)
        .unwrap()
        .unwrap();
    assert_eq!(retry.provenance.dispatch_run_id, dispatch_run_id);
    assert_eq!(retry.reason, "fresh Agent result required");
    assert_eq!(
        retry.safe_evidence,
        "artifact verification rejected the previous result"
    );
}

#[test]
fn legacy_goal_without_pre_spawn_repository_escalates_instead_of_stalling() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let operation = OperationId::new();
    scheduler
        .dispatch
        .upsert_run(DispatchRun {
            run_id: operation,
            agent_id: AgentId::new(),
            prompt: String::new(),
            started_at: now(),
            ended_at: Some(now()),
            status: RunStatus::Completed,
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
    let mut legacy = scheduler
        .supervisor
        .load(started.supervisor_run_id)
        .unwrap()
        .unwrap();
    legacy.artifact_repository = None;
    scheduler.supervisor.initialize(&legacy).unwrap();

    scheduler.fail_apply_at(scheduler.apply_calls.get());
    assert!(
        scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
    assert!(
        scheduler
            .prepare_artifact_verification(operation, now())
            .unwrap()
            .is_none()
    );
    let escalated = scheduler
        .get("goal", started.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert_eq!(escalated.state, SupervisorRunState::Escalated);
    assert_eq!(
        escalated.escalation.unwrap().safe_evidence,
        "artifact repository was not recorded before Goal worker spawn"
    );
}

#[test]
fn promotion_reservations_fence_recursive_delegation_before_provenance_binding() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let root_operation = OperationId::new();
    let root = scheduler
        .reserve_goal_for_workspace(
            "goal-composer",
            workspace,
            &root_operation.to_string(),
            goal("root work"),
            None,
            now(),
        )
        .unwrap();

    // The Agent can start its MCP child as soon as spawn returns, before
    // the composition root has persisted exact provenance. The durable
    // promotion reservation must already classify that caller as
    // supervised, including after a daemon restart.
    let scheduler = SupervisorRuntime::new(temp.path());
    assert!(scheduler.supervises_dispatch(root_operation).unwrap());

    let child_operation = OperationId::new();
    let child = scheduler
        .reserve_delegated_dispatch(
            root_operation,
            &child_operation.to_string(),
            "child work",
            now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(child.run.supervisor_run_id, root.supervisor_run_id);
    assert!(child.run.provenance.is_empty());
    assert_eq!(
        child
            .run
            .tasks
            .iter()
            .find(|task| task.task_id == delegated_task_id(child_operation).unwrap())
            .unwrap()
            .parent_task_id,
        Some(TaskId::new("root").unwrap())
    );

    // A child can recursively delegate in the same post-spawn/pre-bind
    // interval. Its own reservation is the authoritative parent fence.
    assert!(scheduler.supervises_dispatch(child_operation).unwrap());
    let grandchild_operation = OperationId::new();
    let grandchild = scheduler
        .reserve_delegated_dispatch(
            child_operation,
            &grandchild_operation.to_string(),
            "grandchild work",
            now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(grandchild.run.supervisor_run_id, root.supervisor_run_id);
    assert_eq!(
        grandchild
            .run
            .tasks
            .iter()
            .find(|task| task.task_id == delegated_task_id(grandchild_operation).unwrap())
            .unwrap()
            .parent_task_id,
        Some(delegated_task_id(child_operation).unwrap())
    );
    assert!(!scheduler.supervises_dispatch(OperationId::new()).unwrap());
}

#[test]
fn recursive_unbound_promotions_retain_exact_stop_fences_after_cancel() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let root_operation = OperationId::new();
    let run = scheduler
        .reserve_goal_for_workspace(
            "goal-composer",
            workspace,
            &root_operation.to_string(),
            goal("root work"),
            None,
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
    let grandchild_operation = OperationId::new();
    scheduler
        .reserve_delegated_dispatch(
            child_operation,
            &grandchild_operation.to_string(),
            "grandchild work",
            now(),
        )
        .unwrap()
        .unwrap();
    scheduler
        .control_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: run.supervisor_run_id,
                reason: "cancel before provenance binding".into(),
            },
            now(),
        )
        .unwrap();

    let scheduler = SupervisorRuntime::new(temp.path());
    let stops = scheduler
        .pending_worker_stops_for_run(run.supervisor_run_id)
        .unwrap();
    assert_eq!(stops.len(), 3);
    let stop = |operation| {
        stops
            .iter()
            .find(|stop| stop.operation_id == operation)
            .unwrap()
    };
    assert_eq!(stop(root_operation).parent_dispatch_run, None);
    assert_eq!(
        stop(child_operation).parent_dispatch_run,
        Some(root_operation)
    );
    assert_eq!(
        stop(grandchild_operation).parent_dispatch_run,
        Some(child_operation)
    );
}

#[test]
fn child_policy_denial_escalates_durably_before_task_or_agent_effect() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let root_operation = OperationId::new();
    let root = scheduler
        .reserve_goal_for_workspace(
            "goal-composer",
            workspace,
            &root_operation.to_string(),
            goal("root work"),
            None,
            now(),
        )
        .unwrap();
    let mut stored = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    stored.policy.max_dispatches = 1;
    json_file::write_atomic(
        scheduler
            .supervisor
            .snapshot_path(root.supervisor_run_id)
            .parent()
            .unwrap(),
        &scheduler.supervisor.snapshot_path(root.supervisor_run_id),
        &stored,
    )
    .unwrap();
    let child_operation = OperationId::new();
    scheduler.fail_apply_at(2);
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &child_operation.to_string(),
                "child work",
                now(),
            )
            .is_err()
    );
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
            .contains("policy denied")
    );
    let escalated = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert_eq!(escalated.state, SupervisorRunState::Escalated);
    assert_eq!(
        escalated.escalation.unwrap().reason,
        "dispatch budget exhausted"
    );
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
            .contains("policy denied")
    );
    assert!(
        !escalated
            .tasks
            .contains_key(&delegated_task_id(child_operation).unwrap())
    );
    assert!(scheduler.dispatch.run(child_operation).unwrap().is_none());
}

#[test]
fn promotion_authority_refuses_stale_child_and_missing_root_fences() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let root_operation = OperationId::new();
    let root = scheduler
        .reserve_goal_for_workspace(
            "goal-composer",
            workspace,
            &root_operation.to_string(),
            goal("root work"),
            None,
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
    let mut stale_child = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    stale_child
        .tasks
        .get_mut(&delegated_task_id(child_operation).unwrap())
        .unwrap()
        .assigned_dispatch_run = Some(OperationId::new());
    scheduler.supervisor.initialize(&stale_child).unwrap();
    assert!(
        scheduler
            .supervises_dispatch(child_operation)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );

    let malformed = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(malformed.path());
    let operation = OperationId::new();
    let reserved = scheduler
        .reserve_goal_for_workspace(
            "goal-composer",
            WorkspaceId::new(),
            &operation.to_string(),
            goal("malformed root"),
            None,
            now(),
        )
        .unwrap();
    let mut missing_root = scheduler
        .supervisor
        .load(reserved.supervisor_run_id)
        .unwrap()
        .unwrap();
    missing_root.tasks.remove(&TaskId::new("root").unwrap());
    scheduler.supervisor.initialize(&missing_root).unwrap();
    assert!(
        scheduler
            .supervises_dispatch(operation)
            .unwrap_err()
            .to_string()
            .contains("has no authority")
    );
}

#[test]
fn promotion_authority_ignores_generic_starts_and_refuses_stale_root_fences() {
    let generic = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(generic.path());
    let operation = OperationId::new();
    scheduler
        .start_for_workspace(
            "caller",
            WorkspaceId::new(),
            &operation.to_string(),
            "generic root".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    assert!(!scheduler.supervises_dispatch(operation).unwrap());

    let stale = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(stale.path());
    let operation = OperationId::new();
    let reserved = scheduler
        .reserve_goal_for_workspace(
            "goal-composer",
            WorkspaceId::new(),
            &operation.to_string(),
            goal("stale root"),
            None,
            now(),
        )
        .unwrap();
    let mut run = scheduler
        .supervisor
        .load(reserved.supervisor_run_id)
        .unwrap()
        .unwrap();
    run.tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .assigned_dispatch_run = Some(OperationId::new());
    scheduler.supervisor.initialize(&run).unwrap();
    assert!(
        scheduler
            .supervises_dispatch(operation)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );

    // Goal reservations written before the timestamp marker was added
    // remain authoritative through their exact start-operation mapping.
    let legacy = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(legacy.path());
    let operation = OperationId::new();
    let reserved = scheduler
        .reserve_goal_for_workspace(
            "goal-composer",
            WorkspaceId::new(),
            &operation.to_string(),
            goal("legacy root"),
            None,
            now(),
        )
        .unwrap();
    let mut run = scheduler
        .supervisor
        .load(reserved.supervisor_run_id)
        .unwrap()
        .unwrap();
    run.tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .promotion_reserved_at = None;
    scheduler.supervisor.initialize(&run).unwrap();
    assert!(scheduler.supervises_dispatch(operation).unwrap());
}

#[test]
#[allow(clippy::too_many_lines)] // Root and delegated crash windows share one exact operation-join contract.
fn aborted_unbound_promotions_join_only_the_exact_agent_operation() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let root_operation = OperationId::new();
    let root_run = runtime
        .reserve_goal_for_workspace(
            "goal-composer",
            workspace,
            &root_operation.to_string(),
            goal("unbound root"),
            None,
            now(),
        )
        .unwrap();
    runtime
        .control_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: root_run.supervisor_run_id,
                reason: "cancel unbound root".into(),
            },
            now(),
        )
        .unwrap();
    let root_stops = runtime
        .pending_worker_stops_for_run(root_run.supervisor_run_id)
        .unwrap();
    assert_eq!(root_stops.len(), 1);
    assert_eq!(root_stops[0].operation_id(), root_operation);
    assert_eq!(root_stops[0].workspace_id(), workspace);
    let worker = root_worker(workspace);
    let root_provenance = root_stops[0].provenance(&worker).unwrap();
    assert_eq!(root_provenance.worker_agent_id, worker.agent_runtime_id);
    assert!(
        root_stops[0]
            .provenance(&delegated_worker(workspace))
            .unwrap_err()
            .to_string()
            .contains("outside its reserved scope")
    );
    let mut full = runtime.load_state().unwrap();
    for index in 1..MAX_START_RESERVATIONS {
        let live = SupervisorRun::new(
            "caller".into(),
            format!("live-{index}"),
            "input".into(),
            "policy".into(),
            now(),
        );
        runtime.supervisor.initialize(&live).unwrap();
        full.starts.insert(
            format!("live-{index}"),
            StartReservation {
                semantic_key: semantic_digest(format!("live-{index}").as_bytes()),
                supervisor_run_id: live.supervisor_run_id,
                artifact_repository: None,
                workspace_id: None,
                caller_dispatch_run_id: None,
                worker_session_id: None,
                worker_agent_id: None,
                worker_runtime_id: None,
                worker_profile_id: None,
                worker_semantic_digest: None,
            },
        );
    }
    assert!(
        runtime
            .ensure_start_capacity(&mut full)
            .unwrap_err()
            .to_string()
            .contains("capacity is exhausted")
    );
    assert!(full.starts.contains_key(&root_operation.to_string()));
    runtime
        .acknowledge_pending_worker_stops(&root_stops)
        .unwrap();
    runtime
        .acknowledge_pending_worker_stops(&root_stops)
        .unwrap();
    assert!(
        runtime
            .pending_worker_stops_for_run(root_run.supervisor_run_id)
            .unwrap()
            .is_empty()
    );

    let delegated_temp = tempfile::tempdir().unwrap();
    let delegated_runtime = SupervisorRuntime::new(delegated_temp.path());
    let dispatch = DispatchStore::new(delegated_temp.path());
    let parent_operation = OperationId::new();
    dispatch
        .upsert_run(DispatchRun {
            run_id: parent_operation,
            agent_id: AgentId::new(),
            prompt: String::new(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    persist_root_dispatch_agent(&delegated_runtime, workspace, parent_operation);
    let parent = delegated_runtime
        .start_for_workspace_root_dispatch(
            "goal-composer",
            workspace,
            &parent_operation.to_string(),
            goal("delegated parent"),
            None,
            &root_worker(workspace),
            now(),
        )
        .unwrap();
    let child_operation = OperationId::new();
    delegated_runtime
        .reserve_delegated_dispatch(
            parent_operation,
            &child_operation.to_string(),
            "delegated child",
            now(),
        )
        .unwrap()
        .unwrap();
    delegated_runtime
        .control_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: parent.supervisor_run_id,
                reason: "cancel unbound child".into(),
            },
            now(),
        )
        .unwrap();
    let delegated_stops = delegated_runtime
        .pending_worker_stops_for_run(parent.supervisor_run_id)
        .unwrap();
    assert_eq!(delegated_stops.len(), 1);
    assert_eq!(delegated_stops[0].operation_id(), child_operation);
    let child = delegated_stops[0]
        .provenance(&delegated_worker(workspace))
        .unwrap();
    assert_eq!(child.parent_dispatch_run, Some(parent_operation));
    assert_eq!(child.dispatch_run_id, child_operation);
    assert!(child.worker_session_id.is_some());
}
