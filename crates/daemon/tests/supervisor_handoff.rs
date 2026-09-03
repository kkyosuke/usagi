use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use usagi_core::{
    domain::{
        agent::{
            CallerRef, DispatchBinding, DispatchRun, InboxKind, InboxMessage, RunStatus,
            StructuredResult, WorkerRef,
        },
        id::{
            AgentId, AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, OperationId, SessionId,
            TerminalId, TerminalRef, WorkspaceId, WorktreeId,
        },
        pr_inventory::GitHubRepository,
        supervisor::{
            HandoffContextEntry, SupervisorEvent, SupervisorEventKind, SupervisorEventSource,
            SupervisorRunId, TaskId,
        },
    },
    infrastructure::store::{dispatch::DispatchStore, supervisor::SupervisorStore},
};
use usagi_daemon::usecase::supervisor_runtime::{
    DecisionWake, DecisionWaker, GoalSpecification, SupervisorRuntime,
};

#[derive(Default)]
struct Waker {
    wakes: usize,
}

impl DecisionWaker for Waker {
    fn wake(&mut self, _: &DecisionWake) -> Result<()> {
        self.wakes += 1;
        Ok(())
    }
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap()
}

fn worker(workspace_id: WorkspaceId, session_id: Option<SessionId>) -> AgentRuntimeRef {
    AgentRuntimeRef::new(
        AgentRuntimeId::new(),
        TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id,
            session_id,
            worktree_id: WorktreeId::new(),
        },
        session_id,
    )
    .unwrap()
}

fn mark_root_running(store: &SupervisorStore, supervisor_run_id: SupervisorRunId) {
    let run = store.load(supervisor_run_id).unwrap().unwrap();
    store
        .apply(
            supervisor_run_id,
            run.state_revision,
            &SupervisorEvent {
                sequence: run.state_revision + 1,
                event_id: OperationId::new(),
                causation_id: None,
                correlation_id: None,
                observed_at: now(),
                payload_digest: "integration-parent-running".into(),
                source: SupervisorEventSource::Admission,
                kind: SupervisorEventKind::Running {
                    task_id: TaskId::new("root").unwrap(),
                    generation: 1,
                },
            },
        )
        .unwrap();
}

fn append_structured_report(dispatch: &DispatchStore, child_operation: OperationId) {
    let caller = CallerRef {
        session_id: None,
        agent_id: AgentId::new(),
    };
    dispatch
        .upsert_binding(DispatchBinding {
            run_id: child_operation,
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
                run_id: child_operation,
                from: WorkerRef {
                    session_id: Some(SessionId::new()),
                    agent_id: AgentId::new(),
                },
                kind: InboxKind::Completed,
                summary: "mapped the implementation".into(),
                result: Some(StructuredResult {
                    pr: Some("https://github.com/acme/repo/pull/1".into()),
                    commits: vec!["abc123".into()],
                    changed_files: vec!["src/lib.rs".into()],
                    verification: Some("targeted test passed".into()),
                }),
                created_at: now(),
                read: false,
            },
        )
        .unwrap();
}

fn capture_handoff(
    include_report: bool,
    child_status: RunStatus,
) -> (HandoffContextEntry, OperationId) {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let workspace_id = WorkspaceId::new();
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
    let root = runtime
        .start_for_workspace_root_dispatch(
            "goal-composer",
            workspace_id,
            &root_operation.to_string(),
            GoalSpecification::new(
                "ship the change".into(),
                GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
            ),
            None,
            &worker(workspace_id, None),
            now(),
        )
        .unwrap();
    let supervisor = SupervisorStore::new(temp.path());
    mark_root_running(&supervisor, root.supervisor_run_id);

    let child_operation = OperationId::new();
    let reservation = runtime
        .reserve_delegated_dispatch(
            root_operation,
            &child_operation.to_string(),
            "inspect the implementation",
            now(),
        )
        .unwrap()
        .unwrap();
    dispatch
        .upsert_run(DispatchRun {
            run_id: child_operation,
            agent_id: AgentId::new(),
            prompt: reservation.prompt,
            started_at: now(),
            ended_at: Some(now()),
            status: child_status,
        })
        .unwrap();
    if include_report {
        append_structured_report(&dispatch, child_operation);
    }
    runtime
        .attach_delegated_dispatch(
            root_operation,
            &child_operation.to_string(),
            "inspect the implementation".into(),
            &worker(workspace_id, Some(SessionId::new())),
            now(),
        )
        .unwrap()
        .unwrap();

    let mut waker = Waker::default();
    runtime
        .tick(root.supervisor_run_id, now(), &mut waker)
        .unwrap();
    assert_eq!(waker.wakes, 1);
    runtime
        .tick(root.supervisor_run_id, now(), &mut waker)
        .unwrap();

    let stored = supervisor.load(root.supervisor_run_id).unwrap().unwrap();
    assert_eq!(stored.handoff_context.len(), 1);
    (
        stored.handoff_context.into_iter().next().unwrap(),
        child_operation,
    )
}

fn capture_verifying_goal_handoff() -> HandoffContextEntry {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let workspace_id = WorkspaceId::new();
    let operation = OperationId::new();
    dispatch
        .upsert_run(DispatchRun {
            run_id: operation,
            agent_id: AgentId::new(),
            prompt: "root".into(),
            started_at: now(),
            ended_at: Some(now()),
            status: RunStatus::Completed,
        })
        .unwrap();
    let root = runtime
        .start_for_workspace_root_dispatch(
            "goal-composer",
            workspace_id,
            &operation.to_string(),
            GoalSpecification::new(
                "ship the change".into(),
                GitHubRepository::from_name_with_owner("acme/repo").unwrap(),
            ),
            None,
            &worker(workspace_id, None),
            now(),
        )
        .unwrap();
    runtime
        .tick(root.supervisor_run_id, now(), &mut Waker::default())
        .unwrap();
    SupervisorStore::new(temp.path())
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap()
        .handoff_context
        .into_iter()
        .next()
        .unwrap()
}

#[test]
fn public_tick_persists_child_handoffs_from_the_production_library() {
    let (fallback, fallback_operation) = capture_handoff(false, RunStatus::Completed);
    assert_eq!(fallback.dispatch_run_id, fallback_operation);
    assert_eq!(
        fallback.summary,
        "worker terminal state committed without an inbox report"
    );
    assert!(fallback.artifacts.is_none());

    let (reported, reported_operation) = capture_handoff(true, RunStatus::Completed);
    assert_eq!(reported.dispatch_run_id, reported_operation);
    assert_eq!(reported.summary, "mapped the implementation");
    let artifacts = reported.artifacts.unwrap();
    assert!(artifacts.contains("https://github.com/acme/repo/pull/1"));
    assert!(artifacts.contains("abc123"));
    assert!(artifacts.contains("src/lib.rs"));
    assert!(artifacts.contains("targeted test passed"));

    let (failed, failed_operation) = capture_handoff(false, RunStatus::Failed);
    assert_eq!(failed.dispatch_run_id, failed_operation);
    assert_eq!(failed.outcome, InboxKind::Failed);

    let verifying = capture_verifying_goal_handoff();
    assert_eq!(verifying.outcome, InboxKind::Completed);
}

#[test]
fn public_delegation_rejects_invalid_inputs_for_every_instruction_form() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let operation = OperationId::new();

    assert!(
        runtime
            .reserve_delegated_dispatch(OperationId::new(), &operation.to_string(), "", now(),)
            .unwrap_err()
            .to_string()
            .contains("expected 1..=")
    );

    assert!(
        runtime
            .reserve_delegated_dispatch(OperationId::new(), "invalid", "child", now())
            .unwrap_err()
            .to_string()
            .contains("operation is invalid")
    );
    assert!(
        runtime
            .reserve_delegated_dispatch(
                OperationId::new(),
                "invalid",
                String::from("child"),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("operation is invalid")
    );
}
