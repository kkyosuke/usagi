//! supervisor runtime の振る舞いを固定するテスト。

mod artifact;
mod dispatch;
mod lifecycle;
mod promotion;
mod worker;

use super::*;
use chrono::TimeZone;
use std::collections::{BTreeMap, BTreeSet};
use usagi_core::domain::{
    agent::{
        Agent, AgentProfileId, AgentStatus, CallerRef, DispatchBinding, DispatchRun, InboxMessage,
        ModelSelector, WorkerRef,
    },
    id::{
        AgentId, AgentRuntimeId, AgentRuntimeRef, DaemonGeneration, SessionId, TerminalId,
        TerminalRef, WorktreeId,
    },
    pr_inventory::GitHubRepository,
    supervisor::{
        ArtifactExpectation, EscalationRecord, MAX_HANDOFF_CONTEXT_ENTRIES, SupervisorRun, TaskNode,
    },
};
use usagi_core::infrastructure::store::dispatch::{
    AgentAdmissionReservation, CredentialProvenance,
};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap()
}
fn artifact_repository() -> GitHubRepository {
    GitHubRepository::from_name_with_owner("acme/repo").unwrap()
}
fn artifact_expectation() -> ArtifactExpectation {
    ArtifactExpectation::new(
        artifact_repository(),
        "0123456789012345678901234567890123456789",
    )
    .unwrap()
}
fn goal(instruction: &str) -> GoalSpecification {
    GoalSpecification::new(instruction.into(), artifact_repository())
}
fn root_worker(workspace: WorkspaceId) -> AgentRuntimeRef {
    AgentRuntimeRef::new(
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
    .unwrap()
}
fn delegated_worker(workspace: WorkspaceId) -> AgentRuntimeRef {
    let session = SessionId::new();
    AgentRuntimeRef::new(
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
    .unwrap()
}
fn persist_caller_dispatch(
    scheduler: &SupervisorRuntime,
    workspace: WorkspaceId,
    operation: OperationId,
    worker: &AgentRuntimeRef,
) {
    let agent_id = AgentId::new();
    let agent = Agent {
        agent_id,
        session_id: worker.session_id,
        runtime: AgentProfileId::new("claude").unwrap(),
        model: ModelSelector::new("default").unwrap(),
        status: AgentStatus::Running,
        current_run: Some(operation),
    };
    scheduler
        .dispatch
        .reserve_admission_for_workspace(
            workspace,
            agent,
            DispatchRun {
                run_id: operation,
                agent_id,
                prompt: "caller".into(),
                started_at: now(),
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
                semantic_key: "caller-semantic".into(),
                credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
            },
        )
        .unwrap();
}
fn persist_root_dispatch_agent(
    scheduler: &SupervisorRuntime,
    workspace: WorkspaceId,
    operation: OperationId,
) {
    let run = scheduler.dispatch.run(operation).unwrap().unwrap();
    scheduler
        .dispatch
        .upsert_agent(
            workspace,
            Agent {
                agent_id: run.agent_id,
                session_id: None,
                runtime: AgentProfileId::new("claude").unwrap(),
                model: ModelSelector::new("default").unwrap(),
                status: AgentStatus::Running,
                current_run: Some(operation),
            },
        )
        .unwrap();
}
fn task(run: SupervisorRunId, id: &str, parent: Option<&str>) -> TaskNode {
    TaskNode {
        task_id: TaskId::new(id).unwrap(),
        supervisor_run_id: run,
        parent_task_id: parent.map(|id| TaskId::new(id).unwrap()),
        dependencies: BTreeSet::new(),
        instruction_digest: id.into(),
        instruction_body: id.into(),
        required_artifact_contract: NO_ARTIFACT_CONTRACT,
        attempt: 1,
        generation: 1,
        assigned_dispatch_run: None,
        promotion_reserved_at: None,
        promotion_parent_dispatch_run: None,
        promotion_worker_session_id: None,
        promotion_worker_profile_id: None,
        promotion_worker_agent_id: None,
        promotion_worker_semantic_digest: None,
        retry_at: None,
        verification_digest: None,
        verification_attempt: 0,
        verification_retry_at: None,
        verification_expectation: None,
        state: TaskState::Pending,
    }
}
fn aborted_run(workspace: Option<WorkspaceId>) -> SupervisorRun {
    let mut run = SupervisorRun::new(
        "caller".into(),
        "task".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    run.workspace_id = workspace;
    run.state = SupervisorRunState::Cancelled;
    run.terminal_at = Some(now());
    run.terminal_reason = Some("operator cancelled".into());
    run
}
fn unbound_goal_run(workspace: Option<WorkspaceId>) -> SupervisorRun {
    let mut run = aborted_run(workspace);
    let mut root = task(run.supervisor_run_id, "root", None);
    root.required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
    run.tasks.insert(root.task_id.clone(), root);
    run
}
fn start_reservation(supervisor_run_id: SupervisorRunId) -> StartReservation {
    StartReservation {
        semantic_key: semantic_digest(b"test"),
        supervisor_run_id,
        artifact_repository: None,
        workspace_id: None,
        caller_dispatch_run_id: None,
        worker_session_id: None,
        worker_agent_id: None,
        worker_runtime_id: None,
        worker_profile_id: None,
        worker_semantic_digest: None,
    }
}
fn caller_start_reservation(
    supervisor_run_id: SupervisorRunId,
    workspace_id: WorkspaceId,
    dispatch_run_id: OperationId,
) -> StartReservation {
    StartReservation {
        semantic_key: semantic_digest(b"caller"),
        supervisor_run_id,
        artifact_repository: None,
        workspace_id: Some(workspace_id),
        caller_dispatch_run_id: Some(dispatch_run_id),
        worker_session_id: None,
        worker_agent_id: Some(AgentId::new()),
        worker_runtime_id: Some(AgentRuntimeId::new()),
        worker_profile_id: Some(AgentProfileId::new("claude").unwrap()),
        worker_semantic_digest: Some(semantic_digest(b"agent")),
    }
}
fn root_pending_stop(
    operation_id: OperationId,
    workspace_id: WorkspaceId,
    supervisor_run_id: SupervisorRunId,
) -> PendingWorkerStop {
    PendingWorkerStop {
        operation_id,
        workspace_id,
        supervisor_run_id,
        task_id: TaskId::new("root").unwrap(),
        parent_task_id: None,
        parent_dispatch_run: None,
        generation: 1,
        requires_session: false,
        worker_session_id: None,
        worker_agent_id: None,
        worker_runtime_id: None,
        worker_profile_id: None,
        worker_semantic_digest: None,
    }
}
fn event(run: &SupervisorRun, kind: SupervisorEventKind) -> SupervisorEvent {
    SupervisorEvent {
        sequence: run.state_revision + 1,
        event_id: OperationId::new(),
        causation_id: None,
        correlation_id: None,
        observed_at: now(),
        payload_digest: "test".into(),
        source: SupervisorEventSource::Admission,
        kind,
    }
}
fn provenance(
    run: SupervisorRunId,
    task: &TaskId,
    parent: Option<(&TaskId, OperationId)>,
    dispatch: OperationId,
) -> RunProvenance {
    RunProvenance {
        supervisor_run_id: run,
        task_id: task.clone(),
        parent_task_id: parent.as_ref().map(|(id, _)| (*id).clone()),
        parent_dispatch_run: parent.map(|(_, id)| id),
        dispatch_run_id: dispatch,
        worker_session_id: Some(SessionId::new()),
        worker_agent_id: AgentRuntimeId::new(),
        worker_worktree_id: WorktreeId::new(),
        generation: 1,
    }
}
#[derive(Default)]
struct Waker {
    wakes: Vec<DecisionWake>,
}
impl DecisionWaker for Waker {
    fn wake(&mut self, wake: &DecisionWake) -> Result<()> {
        self.wakes.push(wake.clone());
        Ok(())
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One malformed-state matrix proves every provenance and pending-authority fence fails closed.
fn provenance_and_pending_authority_validation_is_fail_closed() {
    let workspace = WorkspaceId::new();
    let mut run = SupervisorRun::new(
        "caller".into(),
        "root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    run.workspace_id = Some(workspace);
    run.state = SupervisorRunState::Running;
    let root_id = TaskId::new("root").unwrap();
    let root_operation = OperationId::new();
    let mut root = task(run.supervisor_run_id, "root", None);
    root.state = TaskState::Running;
    root.assigned_dispatch_run = Some(root_operation);
    run.tasks.insert(root_id.clone(), root);
    let root_provenance = provenance(run.supervisor_run_id, &root_id, None, root_operation);
    run.provenance
        .insert(root_id.clone(), root_provenance.clone());
    validate_provenance_chain(&run, &root_id, &root_provenance).unwrap();

    let missing_id = TaskId::new("missing").unwrap();
    assert!(child_dispatch_policy_denial(&run, &missing_id).is_err());
    let missing = provenance(run.supervisor_run_id, &missing_id, None, OperationId::new());
    assert!(
        validate_provenance_chain(&run, &missing_id, &missing)
            .unwrap_err()
            .to_string()
            .contains("task is missing")
    );
    let mut stale = run.clone();
    stale.tasks.get_mut(&root_id).unwrap().generation = 2;
    assert!(validate_provenance_chain(&stale, &root_id, &root_provenance).is_err());
    let mut rooted_parent = root_provenance;
    rooted_parent.parent_dispatch_run = Some(OperationId::new());
    assert!(
        validate_provenance_chain(&run, &root_id, &rooted_parent)
            .unwrap_err()
            .to_string()
            .contains("root provenance has a parent")
    );

    let child_id = TaskId::new("child").unwrap();
    let child_operation = OperationId::new();
    let mut child = task(run.supervisor_run_id, "child", Some("root"));
    child.state = TaskState::Running;
    child.assigned_dispatch_run = Some(child_operation);
    run.tasks.insert(child_id.clone(), child);
    let child_provenance = provenance(
        run.supervisor_run_id,
        &child_id,
        Some((&root_id, root_operation)),
        child_operation,
    );
    validate_provenance_chain(&run, &child_id, &child_provenance).unwrap();
    let mut no_parent_dispatch = child_provenance.clone();
    no_parent_dispatch.parent_dispatch_run = None;
    assert!(
        validate_provenance_chain(&run, &child_id, &no_parent_dispatch)
            .unwrap_err()
            .to_string()
            .contains("no parent dispatch")
    );
    let mut missing_parent = run.clone();
    missing_parent.tasks.remove(&root_id);
    assert!(
        validate_provenance_chain(&missing_parent, &child_id, &child_provenance)
            .unwrap_err()
            .to_string()
            .contains("parent task is missing")
    );
    let mut historical = run.clone();
    historical
        .tasks
        .get_mut(&child_id)
        .unwrap()
        .promotion_parent_dispatch_run = Some(root_operation);
    historical.provenance.remove(&root_id);
    validate_provenance_chain(&historical, &child_id, &child_provenance).unwrap();
    let mut missing_parent_authority = run.clone();
    missing_parent_authority.provenance.remove(&root_id);
    assert!(
        validate_provenance_chain(&missing_parent_authority, &child_id, &child_provenance,)
            .unwrap_err()
            .to_string()
            .contains("parent authority is missing")
    );
    let mut cyclic = run.clone();
    let cyclic_task = cyclic.tasks.get_mut(&child_id).unwrap();
    cyclic_task.parent_task_id = Some(child_id.clone());
    let mut cyclic_provenance = child_provenance;
    cyclic_provenance.parent_task_id = Some(child_id.clone());
    cyclic_provenance.parent_dispatch_run = Some(child_operation);
    cyclic
        .provenance
        .insert(child_id.clone(), cyclic_provenance.clone());
    assert!(
        validate_provenance_chain(&cyclic, &child_id, &cyclic_provenance)
            .unwrap_err()
            .to_string()
            .contains("cycle")
    );

    let runtime_state = RuntimeState::default();
    assert!(
        live_task_dispatch_authority(&runtime_state, &run, &root_id, &mut BTreeSet::new(),)
            .unwrap()
            .unwrap()
            .committed
    );
    assert!(
        live_task_dispatch_authority(
            &runtime_state,
            &run,
            &TaskId::new("absent").unwrap(),
            &mut BTreeSet::new(),
        )
        .unwrap()
        .is_none()
    );
    let mut already_visiting = BTreeSet::from([root_id.clone()]);
    assert!(
        live_task_dispatch_authority(&runtime_state, &run, &root_id, &mut already_visiting,)
            .unwrap_err()
            .to_string()
            .contains("cycle")
    );

    let mut pending_root_run = run.clone();
    pending_root_run.provenance.clear();
    let pending_root = pending_root_run.tasks.get_mut(&root_id).unwrap();
    pending_root.state = TaskState::Ready;
    pending_root.generation = 1;
    pending_root.assigned_dispatch_run = None;
    pending_root.required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
    pending_root_run.tasks.remove(&child_id);
    assert!(
        live_task_dispatch_authority(
            &runtime_state,
            &pending_root_run,
            &root_id,
            &mut BTreeSet::new(),
        )
        .unwrap()
        .is_none()
    );
    let mut stale_pending = pending_root_run.clone();
    stale_pending.tasks.get_mut(&root_id).unwrap().generation = 2;
    assert!(
        live_task_dispatch_authority(
            &runtime_state,
            &stale_pending,
            &root_id,
            &mut BTreeSet::new(),
        )
        .is_err()
    );
    let mut malformed_root = pending_root_run.clone();
    malformed_root
        .tasks
        .get_mut(&root_id)
        .unwrap()
        .parent_task_id = Some(TaskId::new("parent").unwrap());
    assert!(
        live_task_dispatch_authority(
            &runtime_state,
            &malformed_root,
            &root_id,
            &mut BTreeSet::new(),
        )
        .is_err()
    );

    let mut reserved_state = RuntimeState::default();
    let mut reservation = start_reservation(pending_root_run.supervisor_run_id);
    reservation.workspace_id = Some(workspace);
    reserved_state
        .starts
        .insert(root_operation.to_string(), reservation.clone());
    assert!(!has_caller_root_reservation(
        &reserved_state,
        pending_root_run.supervisor_run_id
    ));
    assert_eq!(
        live_task_dispatch_authority(
            &reserved_state,
            &pending_root_run,
            &root_id,
            &mut BTreeSet::new(),
        )
        .unwrap()
        .unwrap()
        .operation_id,
        root_operation
    );
    let mut generic_root = pending_root_run.clone();
    generic_root
        .tasks
        .get_mut(&root_id)
        .unwrap()
        .required_artifact_contract = NO_ARTIFACT_CONTRACT;
    let mut generic_state = RuntimeState::default();
    let mut generic_reservation = start_reservation(generic_root.supervisor_run_id);
    generic_reservation.caller_dispatch_run_id = Some(root_operation);
    generic_state
        .starts
        .insert(OperationId::new().to_string(), generic_reservation);
    assert!(has_caller_root_reservation(
        &generic_state,
        generic_root.supervisor_run_id
    ));
    assert_eq!(
        live_task_dispatch_authority(
            &generic_state,
            &generic_root,
            &root_id,
            &mut BTreeSet::new(),
        )
        .unwrap()
        .unwrap()
        .operation_id,
        root_operation
    );
    let duplicate_operation = OperationId::new().to_string();
    reserved_state
        .starts
        .insert(duplicate_operation.clone(), reservation.clone());
    assert!(
        live_task_dispatch_authority(
            &reserved_state,
            &pending_root_run,
            &root_id,
            &mut BTreeSet::new(),
        )
        .is_err()
    );
    reserved_state.starts.remove(&duplicate_operation);
    let mut invalid_state = RuntimeState::default();
    invalid_state.starts.insert("invalid".into(), reservation);
    assert!(
        live_task_dispatch_authority(
            &invalid_state,
            &pending_root_run,
            &root_id,
            &mut BTreeSet::new(),
        )
        .unwrap_err()
        .to_string()
        .contains("operation is invalid")
    );

    let ordinary_id = TaskId::new("ordinary").unwrap();
    let mut ordinary_run = pending_root_run.clone();
    let mut ordinary = task(ordinary_run.supervisor_run_id, "ordinary", Some("root"));
    ordinary.state = TaskState::Ready;
    ordinary_run.tasks.insert(ordinary_id.clone(), ordinary);
    assert!(
        live_task_dispatch_authority(
            &RuntimeState::default(),
            &ordinary_run,
            &ordinary_id,
            &mut BTreeSet::new(),
        )
        .unwrap()
        .is_none()
    );

    let invalid_delegated = TaskId::new("delegated-invalid").unwrap();
    let mut delegated_run = pending_root_run.clone();
    let mut invalid_task = task(
        delegated_run.supervisor_run_id,
        invalid_delegated.0.as_str(),
        Some("root"),
    );
    invalid_task.state = TaskState::Ready;
    delegated_run
        .tasks
        .insert(invalid_delegated.clone(), invalid_task);
    assert!(
        live_task_dispatch_authority(
            &RuntimeState::default(),
            &delegated_run,
            &invalid_delegated,
            &mut BTreeSet::new(),
        )
        .unwrap_err()
        .to_string()
        .contains("operation is invalid")
    );

    let delegated_operation = OperationId::new();
    let delegated_id = delegated_task_id(delegated_operation).unwrap();
    let mut delegated = task(
        pending_root_run.supervisor_run_id,
        delegated_id.0.as_str(),
        Some("root"),
    );
    delegated.state = TaskState::Ready;
    delegated.instruction_digest = delegated_task_digest(delegated_operation);
    delegated.promotion_reserved_at = Some(now());
    delegated.promotion_parent_dispatch_run = Some(root_operation);
    let mut delegated_run = pending_root_run.clone();
    delegated_run.tasks.insert(delegated_id.clone(), delegated);
    assert_eq!(
        live_task_dispatch_authority(
            &RuntimeState::default(),
            &delegated_run,
            &delegated_id,
            &mut BTreeSet::new(),
        )
        .unwrap()
        .unwrap()
        .operation_id,
        delegated_operation
    );
    let mut malformed_delegated = delegated_run.clone();
    malformed_delegated
        .tasks
        .get_mut(&delegated_id)
        .unwrap()
        .promotion_reserved_at = None;
    assert!(
        live_task_dispatch_authority(
            &RuntimeState::default(),
            &malformed_delegated,
            &delegated_id,
            &mut BTreeSet::new(),
        )
        .unwrap()
        .is_none()
    );
    delegated_run.tasks.remove(&root_id);
    assert!(
        live_task_dispatch_authority(
            &RuntimeState::default(),
            &delegated_run,
            &delegated_id,
            &mut BTreeSet::new(),
        )
        .unwrap_err()
        .to_string()
        .contains("task is missing")
    );

    let mut legacy_delegated_run = run.clone();
    let mut legacy_delegated = task(
        legacy_delegated_run.supervisor_run_id,
        delegated_id.0.as_str(),
        Some("root"),
    );
    legacy_delegated.state = TaskState::Ready;
    legacy_delegated.instruction_digest = delegated_task_digest(delegated_operation);
    legacy_delegated.promotion_reserved_at = Some(now());
    legacy_delegated_run
        .tasks
        .insert(delegated_id.clone(), legacy_delegated.clone());
    assert!(
        live_task_dispatch_authority(
            &RuntimeState::default(),
            &legacy_delegated_run,
            &delegated_id,
            &mut BTreeSet::new(),
        )
        .unwrap()
        .is_some()
    );
    let mut pending_parent_run = pending_root_run.clone();
    legacy_delegated.supervisor_run_id = pending_parent_run.supervisor_run_id;
    pending_parent_run
        .tasks
        .insert(delegated_id.clone(), legacy_delegated);
    let error = live_task_dispatch_authority(
        &reserved_state,
        &pending_parent_run,
        &delegated_id,
        &mut BTreeSet::new(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("no durable parent fence"),
        "{error:#}"
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One identity matrix keeps every pending promotion collision fence explicit.
fn pending_operation_validation_joins_live_agent_identity_and_semantics() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let operation = OperationId::new();
    assert!(
        scheduler
            .ensure_pending_operation_matches_reservation(
                operation, workspace, false, None, None, None, None,
            )
            .is_ok()
    );

    let worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, operation, &worker);
    let dispatch = scheduler.dispatch.run(operation).unwrap().unwrap();
    let agent = scheduler
        .dispatch
        .agent(dispatch.agent_id)
        .unwrap()
        .unwrap();
    let digest = usagi_core::infrastructure::ipc::agent_operation_digest("caller-semantic");
    scheduler
        .ensure_pending_operation_matches_reservation(
            operation,
            workspace,
            false,
            None,
            Some(&agent.runtime),
            Some(agent.agent_id),
            Some(&digest),
        )
        .unwrap();
    for result in [
        scheduler.ensure_pending_operation_matches_reservation(
            operation, workspace, true, None, None, None, None,
        ),
        scheduler.ensure_pending_operation_matches_reservation(
            operation,
            workspace,
            false,
            None,
            Some(&AgentProfileId::new("codex").unwrap()),
            None,
            None,
        ),
        scheduler.ensure_pending_operation_matches_reservation(
            operation,
            workspace,
            false,
            None,
            None,
            Some(AgentId::new()),
            None,
        ),
        scheduler.ensure_pending_operation_matches_reservation(
            operation,
            workspace,
            false,
            None,
            None,
            None,
            Some("wrong-digest"),
        ),
    ] {
        assert!(result.is_err());
    }

    let mut closed = dispatch;
    closed.status = RunStatus::Completed;
    closed.ended_at = Some(now());
    scheduler.dispatch.upsert_run(closed).unwrap();
    assert!(
        scheduler
            .ensure_pending_operation_matches_reservation(
                operation, workspace, false, None, None, None, None,
            )
            .unwrap_err()
            .to_string()
            .contains("closed supervisor ownership")
    );

    let foreign = OperationId::new();
    scheduler
        .dispatch
        .upsert_run(DispatchRun {
            run_id: foreign,
            agent_id: AgentId::new(),
            prompt: "foreign".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    assert!(
        scheduler
            .ensure_pending_operation_matches_reservation(
                foreign, workspace, false, None, None, None, None,
            )
            .unwrap_err()
            .to_string()
            .contains("foreign Agent ownership")
    );

    let no_semantics = OperationId::new();
    let no_semantics_agent = AgentId::new();
    scheduler
        .dispatch
        .upsert_agent(
            workspace,
            Agent {
                agent_id: no_semantics_agent,
                session_id: None,
                runtime: AgentProfileId::new("claude").unwrap(),
                model: ModelSelector::new("default").unwrap(),
                status: AgentStatus::Running,
                current_run: Some(no_semantics),
            },
        )
        .unwrap();
    scheduler
        .dispatch
        .upsert_run(DispatchRun {
            run_id: no_semantics,
            agent_id: no_semantics_agent,
            prompt: "no semantics".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    assert!(
        scheduler
            .ensure_pending_operation_matches_reservation(
                no_semantics,
                workspace,
                false,
                None,
                None,
                None,
                Some("digest"),
            )
            .unwrap_err()
            .to_string()
            .contains("no semantic authority")
    );

    let session_operation = OperationId::new();
    let session_worker = delegated_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, session_operation, &session_worker);
    scheduler
        .ensure_pending_operation_matches_reservation(
            session_operation,
            workspace,
            true,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(
        scheduler
            .ensure_pending_operation_matches_reservation(
                session_operation,
                workspace,
                true,
                Some(SessionId::new()),
                None,
                None,
                None,
            )
            .is_err()
    );

    let root_operation = OperationId::new();
    let parent_worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, root_operation, &parent_worker);
    scheduler
        .start_for_workspace_root_dispatch(
            "goal",
            workspace,
            &root_operation.to_string(),
            goal("pending child validation"),
            None,
            &parent_worker,
            now(),
        )
        .unwrap();
    let child_operation = OperationId::new();
    scheduler
        .reserve_delegated_dispatch(
            root_operation,
            &child_operation.to_string(),
            "pending child",
            now(),
        )
        .unwrap()
        .unwrap();
    persist_caller_dispatch(
        &scheduler,
        workspace,
        child_operation,
        &root_worker(workspace),
    );
    assert!(
        scheduler
            .supervision_fence(child_operation)
            .unwrap_err()
            .to_string()
            .contains("conflicts with its Agent ownership")
    );
}

fn escalated_retry_run(
    workspace: WorkspaceId,
) -> (SupervisorRun, TaskId, OperationId, OperationId) {
    let mut run = SupervisorRun::new(
        "caller".into(),
        "retry-root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    run.workspace_id = Some(workspace);
    run.state = SupervisorRunState::Escalated;
    let task_id = TaskId::new("root").unwrap();
    let dispatch_run_id = OperationId::new();
    let mut root = task(run.supervisor_run_id, "root", None);
    root.state = TaskState::Verifying;
    root.required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
    root.assigned_dispatch_run = Some(dispatch_run_id);
    run.tasks.insert(task_id.clone(), root);
    run.provenance.insert(
        task_id.clone(),
        provenance(run.supervisor_run_id, &task_id, None, dispatch_run_id),
    );
    let escalation_id = OperationId::new();
    run.escalation = Some(EscalationRecord {
        escalation_id,
        reason: "fresh Agent result required".into(),
        blocking_task_id: Some(task_id.clone()),
        safe_evidence: "artifact verification rejected the previous result".into(),
        choices: vec!["resume".into(), "cancel".into()],
        created_at: now(),
    });
    (run, task_id, dispatch_run_id, escalation_id)
}

#[test]
fn display_labels_and_verification_candidates_are_bounded() {
    assert_eq!(work_run_display_label(" \n\t"), None);
    assert_eq!(work_run_display_label("unsafe\u{1b}[2J"), None);
    let expected = "x".repeat(95);
    assert_eq!(
        work_run_display_label(&format!("{}é", "x".repeat(95))).as_deref(),
        Some(expected.as_str())
    );

    let mut candidate_run = SupervisorRun::new(
        "caller".into(),
        "candidate".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    candidate_run.state = SupervisorRunState::Running;
    let candidate_id = TaskId::new("candidate").unwrap();
    let mut candidate_task = task(candidate_run.supervisor_run_id, "candidate", None);
    candidate_task.state = TaskState::Verifying;
    candidate_run
        .tasks
        .insert(candidate_id.clone(), candidate_task);
    let missing_candidate = event(
        &candidate_run,
        SupervisorEventKind::VerificationCandidateRecorded {
            task_id: TaskId::new("missing").unwrap(),
            generation: 1,
            candidate_pr: None,
        },
    );
    assert!(matches!(
        reduce(&mut candidate_run, &missing_candidate),
        Err(usagi_core::domain::supervisor::SupervisorError::MissingTask)
    ));
    let stale_candidate = event(
        &candidate_run,
        SupervisorEventKind::VerificationCandidateRecorded {
            task_id: candidate_id.clone(),
            generation: 2,
            candidate_pr: None,
        },
    );
    assert!(matches!(
        reduce(&mut candidate_run, &stale_candidate),
        Err(usagi_core::domain::supervisor::SupervisorError::StaleGeneration)
    ));
    let recorded_candidate = event(
        &candidate_run,
        SupervisorEventKind::VerificationCandidateRecorded {
            task_id: candidate_id.clone(),
            generation: 1,
            candidate_pr: None,
        },
    );
    reduce(&mut candidate_run, &recorded_candidate).unwrap();
    let replayed_candidate = event(
        &candidate_run,
        SupervisorEventKind::VerificationCandidateRecorded {
            task_id: candidate_id.clone(),
            generation: 1,
            candidate_pr: None,
        },
    );
    reduce(&mut candidate_run, &replayed_candidate).unwrap();
    let conflicting_candidate = event(
        &candidate_run,
        SupervisorEventKind::VerificationCandidateRecorded {
            task_id: candidate_id.clone(),
            generation: 1,
            candidate_pr: Some("https://github.com/acme/repo/pull/42".into()),
        },
    );
    assert!(matches!(
        reduce(&mut candidate_run, &conflicting_candidate),
        Err(usagi_core::domain::supervisor::SupervisorError::ProvenanceMismatch)
    ));
    let invalid_candidate = event(
        &candidate_run,
        SupervisorEventKind::VerificationCandidateRecorded {
            task_id: candidate_id,
            generation: 1,
            candidate_pr: Some("https://example.com/acme/repo/pull/42".into()),
        },
    );
    assert!(matches!(
        reduce(&mut candidate_run, &invalid_candidate),
        Err(usagi_core::domain::supervisor::SupervisorError::InvalidTransition)
    ));
}

#[test]
fn indexed_recovery_edges_are_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    assert!(
        scheduler
            .load_indexed_runs([SupervisorRunId::new()])
            .unwrap_err()
            .to_string()
            .contains("indexed supervisor run disappeared")
    );

    let workspace = WorkspaceId::new();
    let mut live = SupervisorRun::new(
        "caller".into(),
        "live".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    live.workspace_id = Some(workspace);
    live.state = SupervisorRunState::Running;
    scheduler.supervisor.initialize(&live).unwrap();
    let mut stale_finished = live;
    stale_finished.state = SupervisorRunState::Succeeded;
    stale_finished.terminal_at = Some(now());
    json_file::write_atomic(
        scheduler
            .supervisor
            .snapshot_path(stale_finished.supervisor_run_id)
            .parent()
            .unwrap(),
        &scheduler
            .supervisor
            .snapshot_path(stale_finished.supervisor_run_id),
        &stale_finished,
    )
    .unwrap();
    assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
    assert!(
        scheduler
            .pending_artifact_verifications(now())
            .unwrap()
            .is_empty()
    );

    let aborted_temp = tempfile::tempdir().unwrap();
    let aborted_scheduler = SupervisorRuntime::new(aborted_temp.path());
    let aborted = aborted_run(Some(workspace));
    aborted_scheduler.supervisor.initialize(&aborted).unwrap();
    let mut stale_running = aborted;
    stale_running.state = SupervisorRunState::Running;
    stale_running.terminal_at = None;
    json_file::write_atomic(
        aborted_scheduler
            .supervisor
            .snapshot_path(stale_running.supervisor_run_id)
            .parent()
            .unwrap(),
        &aborted_scheduler
            .supervisor
            .snapshot_path(stale_running.supervisor_run_id),
        &stale_running,
    )
    .unwrap();
    assert!(aborted_scheduler.pending_worker_stops().unwrap().is_empty());
    assert!(
        aborted_scheduler
            .worker_stop_obligations()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn wake_delivery_isolates_failures_and_persists_later_successes() {
    struct SelectiveWaker {
        failing_child: OperationId,
        attempted: Vec<OperationId>,
    }
    impl DecisionWaker for SelectiveWaker {
        fn wake(&mut self, wake: &DecisionWake) -> Result<()> {
            self.attempted.push(wake.child_run_id);
            if wake.child_run_id == self.failing_child {
                anyhow::bail!("injected wake failure");
            }
            Ok(())
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let mut state = RuntimeState::default();
    let mut runs = Vec::new();
    for (key, index) in [("a-fail", 0), ("b-pass", 1)] {
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        run.state = SupervisorRunState::Running;
        let parent_id = TaskId::new(format!("parent-{index}")).unwrap();
        let parent_dispatch = OperationId::new();
        let mut parent = task(run.supervisor_run_id, &parent_id.0, None);
        parent.state = TaskState::AwaitingDecision;
        parent.assigned_dispatch_run = Some(parent_dispatch);
        let parent_provenance =
            provenance(run.supervisor_run_id, &parent_id, None, parent_dispatch);
        run.tasks.insert(parent_id.clone(), parent);
        run.provenance
            .insert(parent_id.clone(), parent_provenance.clone());
        scheduler.supervisor.initialize(&run).unwrap();

        let child_run_id = OperationId::new();
        state.wakes.insert(
            key.into(),
            WakeReservation {
                wake: DecisionWake {
                    supervisor_run_id: run.supervisor_run_id,
                    parent_task_id: parent_id.clone(),
                    parent_generation: 1,
                    parent: parent_provenance,
                    child_run_id,
                    outcome: WakeOutcome {
                        kind: InboxKind::Completed,
                        summary: "done".into(),
                    },
                    dag: Vec::new(),
                    remaining_budget_summary: "none".into(),
                },
                delivered: false,
            },
        );
        runs.push((run.supervisor_run_id, parent_id, child_run_id));
    }
    scheduler.save_state(&state).unwrap();

    let mut waker = SelectiveWaker {
        failing_child: runs[0].2,
        attempted: Vec::new(),
    };
    assert!(
        scheduler
            .deliver_reserved(now(), &mut waker)
            .unwrap_err()
            .to_string()
            .contains("injected wake failure")
    );
    assert_eq!(waker.attempted, vec![runs[0].2, runs[1].2]);
    let state = scheduler.load_state().unwrap();
    assert!(!state.wakes["a-fail"].delivered);
    assert!(state.wakes["b-pass"].delivered);
    assert_eq!(
        scheduler.supervisor.load(runs[0].0).unwrap().unwrap().tasks[&runs[0].1].state,
        TaskState::AwaitingDecision
    );
    assert_eq!(
        scheduler.supervisor.load(runs[1].0).unwrap().unwrap().tasks[&runs[1].1].state,
        TaskState::Running
    );

    let mut retry = Waker::default();
    scheduler.deliver_reserved(now(), &mut retry).unwrap();
    assert_eq!(retry.wakes.len(), 1);
    assert_eq!(retry.wakes[0].child_run_id, runs[0].2);
    assert!(
        scheduler
            .load_state()
            .unwrap()
            .wakes
            .values()
            .all(|wake| wake.delivered)
    );
}

#[test]
fn parent_wake_failures_remain_observable_and_replay_is_idempotent() {
    let wake_temp = tempfile::tempdir().unwrap();
    let wake_scheduler = SupervisorRuntime::new(wake_temp.path());
    let mut run = SupervisorRun::new(
        "caller".into(),
        "root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    run.state = SupervisorRunState::Running;
    let parent_id = TaskId::new("parent").unwrap();
    let parent_dispatch = OperationId::new();
    let mut parent = task(run.supervisor_run_id, "parent", None);
    parent.state = TaskState::AwaitingDecision;
    parent.assigned_dispatch_run = Some(parent_dispatch);
    let parent_provenance = provenance(run.supervisor_run_id, &parent_id, None, parent_dispatch);
    run.tasks.insert(parent_id.clone(), parent);
    run.provenance
        .insert(parent_id.clone(), parent_provenance.clone());
    wake_scheduler.supervisor.initialize(&run).unwrap();
    let wake = DecisionWake {
        supervisor_run_id: run.supervisor_run_id,
        parent_task_id: parent_id.clone(),
        parent_generation: 1,
        parent: parent_provenance,
        child_run_id: OperationId::new(),
        outcome: WakeOutcome {
            kind: InboxKind::Completed,
            summary: "done".into(),
        },
        dag: Vec::new(),
        remaining_budget_summary: "none".into(),
    };

    let mut missing_run = wake.clone();
    missing_run.supervisor_run_id = SupervisorRunId::new();
    assert!(
        wake_scheduler
            .resume_parent_after_wake(&missing_run, now())
            .unwrap_err()
            .to_string()
            .contains("run is unavailable")
    );
    let mut missing_task = wake.clone();
    missing_task.parent_task_id = TaskId::new("missing").unwrap();
    assert!(
        wake_scheduler
            .resume_parent_after_wake(&missing_task, now())
            .unwrap_err()
            .to_string()
            .contains("task is unavailable")
    );
    let mut stale_wake = wake.clone();
    stale_wake.parent_generation += 1;
    assert!(
        wake_scheduler
            .resume_parent_after_wake(&stale_wake, now())
            .unwrap_err()
            .to_string()
            .contains("fence is stale")
    );

    let mut not_resumable = run.clone();
    not_resumable.tasks.get_mut(&parent_id).unwrap().state = TaskState::Pending;
    wake_scheduler
        .supervisor
        .initialize(&not_resumable)
        .unwrap();
    assert!(
        wake_scheduler
            .resume_parent_after_wake(&wake, now())
            .unwrap_err()
            .to_string()
            .contains("not resumable")
    );

    wake_scheduler.supervisor.initialize(&run).unwrap();
    wake_scheduler.fail_apply_at(wake_scheduler.apply_calls.get());
    assert!(
        wake_scheduler
            .resume_parent_after_wake(&wake, now())
            .unwrap_err()
            .to_string()
            .contains("injected supervisor apply failure")
    );
    wake_scheduler
        .resume_parent_after_wake(&wake, now())
        .unwrap();
    assert_eq!(
        wake_scheduler
            .supervisor
            .load(run.supervisor_run_id)
            .unwrap()
            .unwrap()
            .tasks[&parent_id]
            .state,
        TaskState::Running
    );
    wake_scheduler
        .resume_parent_after_wake(&wake, now())
        .unwrap();
}

#[test]
#[allow(clippy::too_many_lines)] // One recovery inventory fixture covers every reservation class and terminal replay.
fn pending_goal_inventory_and_definite_failure_are_exact() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let dispatch = DispatchStore::new(temp.path());
    let workspace = WorkspaceId::new();

    let unscoped_operation = OperationId::new().to_string();
    scheduler
        .start(
            "caller",
            &unscoped_operation,
            "unscoped".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    let classic_operation = OperationId::new();
    scheduler
        .start_for_workspace(
            "caller",
            workspace,
            &classic_operation.to_string(),
            "classic".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    dispatch
        .upsert_run(DispatchRun {
            run_id: classic_operation,
            agent_id: AgentId::new(),
            prompt: String::new(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_workspace_root_dispatch(
                &classic_operation.to_string(),
                &root_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("not a Goal run")
    );
    let classic_before = scheduler
        .get(
            "caller",
            scheduler.load_state().unwrap().starts[&classic_operation.to_string()]
                .supervisor_run_id,
        )
        .unwrap()
        .unwrap();
    assert!(
        scheduler
            .fail_reserved_goal(
                &classic_operation.to_string(),
                "must not cross contracts".into(),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("not a Goal run")
    );
    assert_eq!(
        scheduler
            .get("caller", classic_before.supervisor_run_id)
            .unwrap()
            .unwrap(),
        classic_before
    );
    assert_eq!(
        scheduler
            .reserved_goal_repository(&OperationId::new().to_string())
            .unwrap(),
        None
    );

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
    assert_eq!(
        scheduler
            .reserved_goal_repository(&goal_operation.to_string())
            .unwrap(),
        Some(artifact_repository())
    );
    assert!(
        scheduler
            .reserve_goal_for_workspace(
                "goal",
                workspace,
                &goal_operation.to_string(),
                GoalSpecification::new(
                    "goal".into(),
                    GitHubRepository::from_name_with_owner("other/repo").unwrap(),
                ),
                None,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("different supervisor start")
    );
    assert!(
        scheduler
            .pending_artifact_verifications(now())
            .unwrap()
            .is_empty()
    );
    let mut state = scheduler.load_state().unwrap();
    state.starts.insert(
        OperationId::new().to_string(),
        StartReservation {
            semantic_key: semantic_digest(b"orphan"),
            supervisor_run_id: SupervisorRunId::new(),
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
    scheduler.save_state(&state).unwrap();
    assert_eq!(
        scheduler.pending_goal_promotions().unwrap(),
        vec![PendingGoalPromotion {
            operation_id: goal_operation.to_string(),
            reserved_at: now(),
            workspace_id: workspace,
            worker_profile_id: None,
            worker_semantic_digest: None,
        }]
    );

    // A pre-fix scheduler could escalate the root during the promotion
    // window. Definite failure must still close that durable reservation.
    let reserved = scheduler
        .supervisor
        .load(goal_run.supervisor_run_id)
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

    assert!(
        scheduler
            .fail_reserved_goal("missing", "failed".into(), now())
            .unwrap_err()
            .to_string()
            .contains("reservation does not exist")
    );
    let failed = scheduler
        .fail_reserved_goal(
            &goal_operation.to_string(),
            "definite failure".into(),
            now(),
        )
        .unwrap();
    assert_eq!(failed.state, SupervisorRunState::Failed);
    assert_eq!(failed.terminal_reason.as_deref(), Some("definite failure"));
    assert_eq!(
        scheduler
            .fail_reserved_goal(&goal_operation.to_string(), "ignored replay".into(), now())
            .unwrap(),
        failed
    );
    assert!(scheduler.pending_goal_promotions().unwrap().is_empty());
    assert!(scheduler.pending_delegated_promotions().unwrap().is_empty());
    assert_eq!(
        scheduler
            .get("goal", goal_run.supervisor_run_id)
            .unwrap()
            .unwrap(),
        failed
    );

    let reservation_without_dispatch = OperationId::new();
    scheduler
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &reservation_without_dispatch.to_string(),
            goal("goal"),
            None,
            now(),
        )
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_workspace_root_dispatch(
                &reservation_without_dispatch.to_string(),
                &root_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("dispatch does not exist")
    );
    let dispatch_without_reservation = OperationId::new();
    dispatch
        .upsert_run(DispatchRun {
            run_id: dispatch_without_reservation,
            agent_id: AgentId::new(),
            prompt: String::new(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    assert!(
        scheduler
            .bind_reserved_workspace_root_dispatch(
                &dispatch_without_reservation.to_string(),
                &root_worker(workspace),
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("reservation does not exist")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One sequence covers reservation, parent retry, binding, and stale replay.
fn parent_retry_keeps_the_reserved_child_fence_and_rejects_the_old_parent() {
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
    let retry_operation = OperationId::new();
    dispatch
        .upsert_run(DispatchRun {
            run_id: retry_operation,
            agent_id: AgentId::new(),
            prompt: "retried parent".into(),
            started_at: now(),
            ended_at: None,
            status: RunStatus::Running,
        })
        .unwrap();
    let mut retried = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    let root_id = TaskId::new("root").unwrap();
    let root_task = retried.tasks.get_mut(&root_id).unwrap();
    root_task.generation = 2;
    root_task.assigned_dispatch_run = Some(retry_operation);
    root_task.state = TaskState::Dispatched;
    let mut retry_provenance =
        provenance(retried.supervisor_run_id, &root_id, None, retry_operation);
    retry_provenance.generation = 2;
    retried.provenance.insert(root_id, retry_provenance);
    scheduler.supervisor.initialize(&retried).unwrap();

    assert!(
        scheduler
            .supervision_fence(root_operation)
            .unwrap_err()
            .to_string()
            .contains("stale supervisor ownership")
    );
    assert!(
        scheduler
            .supervision_fence(retry_operation)
            .unwrap()
            .is_some()
    );
    assert!(
        scheduler
            .supervision_fence(child_operation)
            .unwrap()
            .is_some()
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
    scheduler
        .bind_reserved_delegated_dispatch(
            &child_operation.to_string(),
            &delegated_worker(workspace),
            now(),
        )
        .unwrap()
        .unwrap();
    let bound = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        bound.provenance[&delegated_task_id(child_operation).unwrap()].parent_dispatch_run,
        Some(root_operation)
    );
    assert_eq!(
        bound.provenance[&TaskId::new("root").unwrap()].dispatch_run_id,
        retry_operation
    );
    assert!(
        scheduler
            .supervision_fence(child_operation)
            .unwrap()
            .is_some()
    );
}

fn supervised_peer_fixture() -> (
    tempfile::TempDir,
    SupervisorRuntime,
    WorkspaceId,
    OperationId,
    AgentRuntimeRef,
    Agent,
) {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let parent_operation = OperationId::new();
    let worker = delegated_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, parent_operation, &worker);
    let parent_id = scheduler
        .dispatch
        .run(parent_operation)
        .unwrap()
        .unwrap()
        .agent_id;
    let mut parent = scheduler.dispatch.agent(parent_id).unwrap().unwrap();
    parent.runtime = AgentProfileId::new("codex").unwrap();
    scheduler.dispatch.upsert_agent(workspace, parent).unwrap();
    scheduler
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &OperationId::new().to_string(),
            "implement and review".into(),
            None,
            parent_operation,
            &worker,
            now(),
        )
        .unwrap();
    let peer = Agent {
        agent_id: AgentId::new(),
        session_id: worker.session_id,
        runtime: AgentProfileId::new("claude").unwrap(),
        model: ModelSelector::new("default").unwrap(),
        status: AgentStatus::Idle,
        current_run: None,
    };
    (temp, scheduler, workspace, parent_operation, worker, peer)
}

#[test]
#[allow(clippy::too_many_lines)] // One replay matrix contrasts pending, bound, legacy, and conflicting reservations.
fn session_delegation_replays_only_the_exact_reserved_agent_and_semantics() {
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
            goal("root"),
            None,
            &root_worker,
            now(),
        )
        .unwrap();

    let child_operation = OperationId::new();
    let child_worker = delegated_worker(workspace);
    let planned = Agent {
        agent_id: AgentId::new(),
        session_id: child_worker.session_id,
        runtime: AgentProfileId::new("claude").unwrap(),
        model: ModelSelector::new("default").unwrap(),
        status: AgentStatus::Idle,
        current_run: None,
    };
    let mut wrong_planned = planned.clone();
    wrong_planned.session_id = Some(SessionId::new());
    assert!(
        scheduler
            .reserve_delegated_dispatch_for_session(
                root_operation,
                &OperationId::new().to_string(),
                "wrong worker",
                child_worker.session_id.unwrap(),
                &wrong_planned,
                "worker",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("outside its Supervisor scope")
    );
    let reserved = scheduler
        .reserve_delegated_dispatch_for_session(
            root_operation,
            &child_operation.to_string(),
            "child",
            child_worker.session_id.unwrap(),
            &planned,
            "worker",
            now(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        scheduler
            .reserve_delegated_dispatch_for_session(
                root_operation,
                &child_operation.to_string(),
                "child",
                child_worker.session_id.unwrap(),
                &planned,
                "worker",
                now(),
            )
            .unwrap()
            .unwrap()
            .prompt,
        reserved.prompt
    );

    let mut admitted = planned.clone();
    admitted.status = AgentStatus::Running;
    admitted.current_run = Some(child_operation);
    let semantic_key = usagi_core::infrastructure::ipc::agent_dispatch_semantic_key(
        "worker",
        admitted.agent_id,
        &reserved.prompt,
    );
    scheduler
        .dispatch
        .reserve_admission_for_workspace(
            workspace,
            admitted.clone(),
            DispatchRun {
                run_id: child_operation,
                agent_id: admitted.agent_id,
                prompt: reserved.prompt,
                started_at: now(),
                ended_at: None,
                status: RunStatus::Running,
            },
            DispatchBinding {
                run_id: child_operation,
                caller: CallerRef {
                    session_id: root_worker.session_id,
                    agent_id: scheduler
                        .dispatch
                        .run(root_operation)
                        .unwrap()
                        .unwrap()
                        .agent_id,
                },
                worker: WorkerRef {
                    session_id: child_worker.session_id,
                    agent_id: admitted.agent_id,
                },
            },
            AgentAdmissionReservation {
                operation_id: child_operation,
                semantic_key,
                credential_provenance: CredentialProvenance::DaemonMintedEphemeral,
            },
        )
        .unwrap();
    scheduler
        .bind_reserved_delegated_dispatch(&child_operation.to_string(), &child_worker, now())
        .unwrap()
        .unwrap();
    assert!(
        scheduler
            .reserve_delegated_dispatch_for_session(
                root_operation,
                &child_operation.to_string(),
                "child",
                child_worker.session_id.unwrap(),
                &planned,
                "worker",
                now(),
            )
            .unwrap()
            .is_some()
    );
    assert!(
        scheduler
            .reserve_delegated_dispatch_for_session(
                root_operation,
                &child_operation.to_string(),
                "different child",
                child_worker.session_id.unwrap(),
                &planned,
                "worker",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("conflicts with its existing supervisor task")
    );

    let child_dispatch = scheduler.dispatch.run(child_operation).unwrap().unwrap();
    let task_id = delegated_task_id(child_operation).unwrap();
    let bound_run = scheduler
        .unfinished_runs()
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let bound_task = bound_run.tasks.get(&task_id).unwrap();
    let digest = bound_task
        .promotion_worker_semantic_digest
        .as_ref()
        .unwrap();
    assert!(delegated_worker_matches_reservation(
        bound_run.workspace_id,
        &child_worker,
        Some(&admitted),
        bound_task,
        &child_dispatch,
        Some(digest),
    ));
    assert!(!delegated_worker_matches_reservation(
        bound_run.workspace_id,
        &child_worker,
        None,
        bound_task,
        &child_dispatch,
        Some(digest),
    ));

    let legacy_operation = OperationId::new();
    scheduler
        .reserve_delegated_dispatch(
            root_operation,
            &legacy_operation.to_string(),
            "legacy child",
            now(),
        )
        .unwrap()
        .unwrap();
    let mut legacy_run = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    legacy_run
        .tasks
        .get_mut(&delegated_task_id(legacy_operation).unwrap())
        .unwrap()
        .promotion_parent_dispatch_run = None;
    scheduler.supervisor.initialize(&legacy_run).unwrap();
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &legacy_operation.to_string(),
                "legacy child",
                now(),
            )
            .unwrap()
            .is_some()
    );
    let legacy_worker = delegated_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, legacy_operation, &legacy_worker);
    scheduler
        .bind_reserved_delegated_dispatch(&legacy_operation.to_string(), &legacy_worker, now())
        .unwrap()
        .unwrap();
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &legacy_operation.to_string(),
                "legacy child",
                now(),
            )
            .unwrap()
            .is_some()
    );

    let attached_operation = OperationId::new();
    let attached_worker = delegated_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, attached_operation, &attached_worker);
    assert!(
        scheduler
            .attach_delegated_dispatch(
                root_operation,
                &attached_operation.to_string(),
                "attached child".into(),
                &attached_worker,
                now(),
            )
            .unwrap()
            .is_some()
    );
    assert!(
        scheduler
            .attach_delegated_dispatch(
                root_operation,
                &attached_operation.to_string(),
                "different attached child".into(),
                &attached_worker,
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("existing supervisor task")
    );

    let occupied_operation = OperationId::new();
    let mut occupied_run = aborted_run(Some(workspace));
    let occupied_id = delegated_task_id(occupied_operation).unwrap();
    let mut occupied = task(occupied_run.supervisor_run_id, occupied_id.0.as_str(), None);
    occupied.state = TaskState::Cancelled;
    occupied_run.tasks.insert(occupied_id, occupied);
    scheduler.supervisor.initialize(&occupied_run).unwrap();
    assert!(
        scheduler
            .reserve_delegated_dispatch(
                root_operation,
                &occupied_operation.to_string(),
                "occupied",
                now(),
            )
            .unwrap_err()
            .to_string()
            .contains("already owns a supervisor task")
    );

    let mut stale_run = scheduler
        .supervisor
        .load(root.supervisor_run_id)
        .unwrap()
        .unwrap();
    stale_run
        .tasks
        .get_mut(&delegated_task_id(child_operation).unwrap())
        .unwrap()
        .generation = 2;
    scheduler.supervisor.initialize(&stale_run).unwrap();
    assert!(
        scheduler
            .reserve_delegated_dispatch_for_session(
                root_operation,
                &child_operation.to_string(),
                "child",
                child_worker.session_id.unwrap(),
                &planned,
                "worker",
                now(),
            )
            .is_err()
    );
}

#[test]
fn supervised_parent_rejects_duplicate_pending_and_historical_owners() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let pending_operation = OperationId::new();
    let pending_worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, pending_operation, &pending_worker);
    let pending_start = OperationId::new().to_string();
    scheduler
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &pending_start,
            "pending".into(),
            None,
            pending_operation,
            &pending_worker,
            now(),
        )
        .unwrap();
    let mut state = scheduler.load_state().unwrap();
    let reservation = state.starts[&pending_start].clone();
    state
        .starts
        .insert(OperationId::new().to_string(), reservation);
    scheduler.save_state(&state).unwrap();
    assert!(
        scheduler
            .supervision_fence(pending_operation)
            .unwrap_err()
            .to_string()
            .contains("multiple promotion reservations")
    );

    let retained_temp = tempfile::tempdir().unwrap();
    let retained = SupervisorRuntime::new(retained_temp.path());
    let operation = OperationId::new();
    let worker = root_worker(workspace);
    persist_caller_dispatch(&retained, workspace, operation, &worker);
    retained
        .start_for_workspace_root_dispatch(
            "goal",
            workspace,
            &operation.to_string(),
            goal("active"),
            None,
            &worker,
            now(),
        )
        .unwrap();
    let mut historical = aborted_run(Some(workspace));
    let root_id = TaskId::new("root").unwrap();
    let mut root = task(historical.supervisor_run_id, "root", None);
    root.state = TaskState::Cancelled;
    root.assigned_dispatch_run = Some(operation);
    historical.tasks.insert(root_id.clone(), root);
    historical.provenance.insert(
        root_id.clone(),
        provenance(historical.supervisor_run_id, &root_id, None, operation),
    );
    retained.supervisor.initialize(&historical).unwrap();
    assert!(
        retained
            .supervision_fence(operation)
            .unwrap_err()
            .to_string()
            .contains("conflicting retained supervisor ownership")
    );

    let malformed_temp = tempfile::tempdir().unwrap();
    let malformed = SupervisorRuntime::new(malformed_temp.path());
    let malformed_dispatch = OperationId::new();
    let malformed_run = aborted_run(Some(workspace));
    malformed.supervisor.initialize(&malformed_run).unwrap();
    let mut malformed_state = RuntimeState::default();
    malformed_state.starts.insert(
        OperationId::new().to_string(),
        caller_start_reservation(
            malformed_run.supervisor_run_id,
            workspace,
            malformed_dispatch,
        ),
    );
    malformed.save_state(&malformed_state).unwrap();
    assert!(
        malformed
            .supervision_fence(malformed_dispatch)
            .unwrap_err()
            .to_string()
            .contains("root reservation is malformed")
    );
}

#[test]
fn legacy_pending_caller_root_still_consumes_a_child_policy_slot() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    let caller_operation = OperationId::new();
    let worker = root_worker(workspace);
    persist_caller_dispatch(&scheduler, workspace, caller_operation, &worker);
    let started = scheduler
        .start_for_workspace_caller_dispatch(
            "caller",
            workspace,
            &OperationId::new().to_string(),
            "legacy pending root".into(),
            None,
            caller_operation,
            &worker,
            now(),
        )
        .unwrap();
    let mut run = scheduler
        .supervisor
        .load(started.supervisor_run_id)
        .unwrap()
        .unwrap();
    run.tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .promotion_reserved_at = None;
    scheduler.supervisor.initialize(&run).unwrap();

    assert!(
        scheduler
            .reserve_delegated_dispatch(
                caller_operation,
                &OperationId::new().to_string(),
                "child",
                now(),
            )
            .unwrap()
            .is_some()
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One durable-state matrix covers every partial caller-root phase.
fn pending_caller_inventory_skips_partial_phases_and_rejects_stale_shapes() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let workspace = WorkspaceId::new();
    scheduler
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &OperationId::new().to_string(),
            goal("goal is not a caller root"),
            None,
            now(),
        )
        .unwrap();
    assert!(scheduler.pending_caller_promotions().unwrap().is_empty());

    let missing_dispatch = OperationId::new();
    let missing_run_id = SupervisorRunId::new();
    let mut state = scheduler.load_state().unwrap();
    let missing_start = OperationId::new().to_string();
    state.starts.insert(
        missing_start.clone(),
        caller_start_reservation(missing_run_id, workspace, missing_dispatch),
    );
    scheduler.save_state(&state).unwrap();
    assert!(scheduler.pending_caller_promotions().unwrap().is_empty());
    scheduler
        .ensure_supervisor_start_dispatch_available(&missing_start, missing_dispatch)
        .unwrap();
    let expired_dispatch = OperationId::new();
    state.expired_starts.insert(&expired_dispatch.to_string());
    scheduler.save_state(&state).unwrap();
    assert!(
        scheduler
            .ensure_supervisor_start_dispatch_available(
                &OperationId::new().to_string(),
                expired_dispatch,
            )
            .unwrap_err()
            .to_string()
            .contains("retained supervisor run")
    );

    let mut run = SupervisorRun::new(
        "caller".into(),
        "root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    run.workspace_id = Some(workspace);
    scheduler.supervisor.initialize(&run).unwrap();
    let start_operation = OperationId::new().to_string();
    state = scheduler.load_state().unwrap();
    state.starts.insert(
        start_operation.clone(),
        caller_start_reservation(run.supervisor_run_id, workspace, OperationId::new()),
    );
    scheduler.save_state(&state).unwrap();
    assert!(scheduler.pending_caller_promotions().unwrap().is_empty());
    let retained_dispatch = state.starts[&start_operation]
        .caller_dispatch_run_id
        .unwrap();
    assert_eq!(
        scheduler
            .retained_dispatch_owners(&state, retained_dispatch)
            .unwrap()
            .len(),
        1
    );

    run.state = SupervisorRunState::Running;
    scheduler.supervisor.initialize(&run).unwrap();
    assert!(
        scheduler
            .retained_dispatch_owners(&state, retained_dispatch)
            .unwrap_err()
            .to_string()
            .contains("reservation is malformed")
    );
    assert!(
        scheduler
            .pending_caller_promotions()
            .unwrap_err()
            .to_string()
            .contains("root task is missing")
    );

    let mut root = task(run.supervisor_run_id, "root", None);
    root.state = TaskState::Ready;
    run.tasks.insert(root.task_id.clone(), root);
    run.tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .required_artifact_contract = GOAL_REVIEW_READY_ARTIFACT_CONTRACT;
    run.artifact_repository = Some(artifact_repository());
    scheduler.supervisor.initialize(&run).unwrap();
    assert!(
        scheduler
            .retained_dispatch_owners(&state, retained_dispatch)
            .unwrap_err()
            .to_string()
            .contains("reservation is malformed")
    );
    run.tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .required_artifact_contract = NO_ARTIFACT_CONTRACT;
    run.artifact_repository = None;
    scheduler.supervisor.initialize(&run).unwrap();
    state = scheduler.load_state().unwrap();
    state.starts.get_mut(&start_operation).unwrap().workspace_id = Some(WorkspaceId::new());
    scheduler.save_state(&state).unwrap();
    assert!(
        scheduler
            .pending_caller_promotions()
            .unwrap_err()
            .to_string()
            .contains("workspace fence is stale")
    );

    state.starts.get_mut(&start_operation).unwrap().workspace_id = Some(workspace);
    scheduler.save_state(&state).unwrap();
    run.tasks
        .get_mut(&TaskId::new("root").unwrap())
        .unwrap()
        .parent_task_id = Some(TaskId::new("parent").unwrap());
    scheduler.supervisor.initialize(&run).unwrap();
    assert!(
        scheduler
            .pending_caller_promotions()
            .unwrap_err()
            .to_string()
            .contains("reservation is malformed")
    );
}

#[test]
fn missing_goal_snapshot_keeps_its_operation_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let operation = OperationId::new();
    let workspace = WorkspaceId::new();
    let specification = goal("partial goal");
    let reserved = scheduler
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &operation.to_string(),
            specification.clone(),
            None,
            now(),
        )
        .unwrap();
    let restarted_temp = tempfile::tempdir().unwrap();
    let restarted = SupervisorRuntime::new(restarted_temp.path());
    restarted
        .save_state(&scheduler.load_state().unwrap())
        .unwrap();

    assert!(
        restarted
            .supervision_fence(operation)
            .unwrap_err()
            .to_string()
            .contains("stale supervisor ownership")
    );
    assert!(
        restarted
            .ensure_supervisor_start_dispatch_available(&OperationId::new().to_string(), operation,)
            .unwrap_err()
            .to_string()
            .contains("another retained supervisor run")
    );
    assert_eq!(
        restarted
            .reserved_goal_repository(&operation.to_string())
            .unwrap(),
        Some(artifact_repository())
    );
    let replay = restarted
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &operation.to_string(),
            specification,
            None,
            now(),
        )
        .unwrap();
    assert_eq!(replay.supervisor_run_id, reserved.supervisor_run_id);

    let mut legacy_state = scheduler.load_state().unwrap();
    let legacy_reservation = legacy_state.starts.get_mut(&operation.to_string()).unwrap();
    legacy_reservation.artifact_repository = None;
    legacy_reservation.workspace_id = None;
    let legacy_temp = tempfile::tempdir().unwrap();
    let legacy = SupervisorRuntime::new(legacy_temp.path());
    legacy.save_state(&legacy_state).unwrap();
    assert_eq!(
        legacy
            .reserved_goal_repository(&operation.to_string())
            .unwrap(),
        None
    );
    let legacy_replay = legacy
        .reserve_goal_for_workspace(
            "goal",
            workspace,
            &operation.to_string(),
            goal("partial goal"),
            None,
            now(),
        )
        .unwrap();
    assert_eq!(legacy_replay.supervisor_run_id, reserved.supervisor_run_id);

    let mut conflicting_state = scheduler.load_state().unwrap();
    conflicting_state
        .starts
        .get_mut(&operation.to_string())
        .unwrap()
        .artifact_repository =
        Some(GitHubRepository::from_name_with_owner("other/repository").unwrap());
    scheduler.save_state(&conflicting_state).unwrap();
    assert!(
        scheduler
            .reserved_goal_repository(&operation.to_string())
            .unwrap_err()
            .to_string()
            .contains("conflicts with its durable run")
    );
}

#[test]
fn terminal_task_kinds_choose_a_safe_terminal_run_reason() {
    for (task_state, expected_reason) in [
        (TaskState::Failed, "one or more supervisor tasks failed"),
        (
            TaskState::Blocked,
            "one or more supervisor tasks were blocked",
        ),
        (
            TaskState::Cancelled,
            "one or more supervisor tasks were cancelled",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let scheduler = SupervisorRuntime::new(temp.path());
        let mut run = SupervisorRun::new(
            "caller".into(),
            "root".into(),
            "input".into(),
            "policy".into(),
            now(),
        );
        run.state = SupervisorRunState::Running;
        let mut root = task(run.supervisor_run_id, "root", None);
        root.state = task_state;
        run.tasks.insert(TaskId::new("root").unwrap(), root);
        scheduler.supervisor.initialize(&run).unwrap();
        let finalized = scheduler.finalize_terminal_tasks(run, now()).unwrap();
        assert_eq!(finalized.state, SupervisorRunState::Failed);
        assert_eq!(finalized.terminal_reason.as_deref(), Some(expected_reason));
    }
}

fn wake_reservation(index: usize, delivered: bool) -> WakeReservation {
    let run = SupervisorRunId::new();
    let parent = TaskId::new(format!("parent-{index}")).unwrap();
    let child = OperationId::new();
    WakeReservation {
        wake: DecisionWake {
            supervisor_run_id: run,
            parent_task_id: parent.clone(),
            parent_generation: 1,
            parent: provenance(run, &parent, None, OperationId::new()),
            child_run_id: child,
            outcome: WakeOutcome {
                kind: InboxKind::Completed,
                summary: "done".into(),
            },
            dag: Vec::new(),
            remaining_budget_summary: "none".into(),
        },
        delivered,
    }
}

#[test]
fn terminal_statuses_and_sources_preserve_the_safe_completion_vocabulary() {
    assert_eq!(terminal(RunStatus::Running), None);
    assert_eq!(
        terminal(RunStatus::Completed),
        Some((TaskState::Succeeded, InboxKind::Completed))
    );
    assert_eq!(
        terminal(RunStatus::Failed),
        Some((TaskState::Failed, InboxKind::Failed))
    );
    assert_eq!(
        terminal(RunStatus::NoReport),
        Some((TaskState::Failed, InboxKind::NoReport))
    );
    assert_eq!(
        source(InboxKind::Completed),
        SupervisorEventSource::DispatchCompletion
    );
    assert_eq!(
        source(InboxKind::Failed),
        SupervisorEventSource::DispatchFailure
    );
    assert_eq!(source(InboxKind::NoReport), SupervisorEventSource::NoReport);
}

#[test]
fn read_only_query_responses_have_an_aggregate_serialized_budget() {
    assert_eq!(
        bounded_supervisor_query(serde_json::json!({"runs": []})).unwrap(),
        serde_json::json!({"runs": []})
    );
    assert!(
        bounded_supervisor_query(serde_json::json!({
            "value": "x".repeat(RUN_LIST_RESPONSE_MAX_BYTES)
        }))
        .unwrap_err()
        .to_string()
        .contains("capacity is exhausted")
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Start, wake, and human-control retention share one durable metadata contract.
fn runtime_metadata_compacts_safe_history_and_backpressures_live_state() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let mut state = RuntimeState::default();

    for index in 0..MAX_START_RESERVATIONS {
        let run = SupervisorRun::new(
            "caller".into(),
            format!("task-{index}"),
            "input".into(),
            "policy".into(),
            now(),
        );
        scheduler.supervisor.initialize(&run).unwrap();
        state.starts.insert(
            format!("start-{index}"),
            StartReservation {
                semantic_key: semantic_digest(format!("semantic-{index}").as_bytes()),
                supervisor_run_id: run.supervisor_run_id,
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
        scheduler
            .ensure_start_capacity(&mut state)
            .unwrap_err()
            .to_string()
            .contains("capacity is exhausted")
    );

    let first_id = state.starts["start-0"].supervisor_run_id;
    let mut escalated = scheduler.supervisor.load(first_id).unwrap().unwrap();
    escalated.state = SupervisorRunState::Escalated;
    escalated.terminal_at = Some(now());
    json_file::write_atomic(
        scheduler
            .supervisor
            .snapshot_path(first_id)
            .parent()
            .unwrap(),
        &scheduler.supervisor.snapshot_path(first_id),
        &escalated,
    )
    .unwrap();
    assert!(
        scheduler
            .ensure_start_capacity(&mut state)
            .unwrap_err()
            .to_string()
            .contains("capacity is exhausted")
    );

    let mut finished = scheduler.supervisor.load(first_id).unwrap().unwrap();
    finished.state = SupervisorRunState::Succeeded;
    finished.terminal_at = Some(now());
    json_file::write_atomic(
        scheduler
            .supervisor
            .snapshot_path(first_id)
            .parent()
            .unwrap(),
        &scheduler.supervisor.snapshot_path(first_id),
        &finished,
    )
    .unwrap();
    let second_id = state.starts["start-1"].supervisor_run_id;
    let mut second_finished = scheduler.supervisor.load(second_id).unwrap().unwrap();
    second_finished.state = SupervisorRunState::Succeeded;
    second_finished.terminal_at = Some(now());
    json_file::write_atomic(
        scheduler
            .supervisor
            .snapshot_path(second_id)
            .parent()
            .unwrap(),
        &scheduler.supervisor.snapshot_path(second_id),
        &second_finished,
    )
    .unwrap();
    let caller_tombstone = OperationId::new();
    state
        .starts
        .get_mut("start-0")
        .unwrap()
        .caller_dispatch_run_id = Some(caller_tombstone);
    scheduler.ensure_start_capacity(&mut state).unwrap();
    assert_eq!(state.starts.len(), MAX_START_RESERVATIONS - 1);
    assert!(state.expired_starts.contains("start-0"));
    assert!(state.expired_starts.contains(&caller_tombstone.to_string()));

    let mut missing = RuntimeState::default();
    for index in 0..=MAX_START_RESERVATIONS {
        missing.starts.insert(
            format!("missing-{index}"),
            StartReservation {
                semantic_key: semantic_digest(format!("missing-semantic-{index}").as_bytes()),
                supervisor_run_id: SupervisorRunId::new(),
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
        scheduler
            .ensure_start_capacity(&mut missing)
            .unwrap_err()
            .to_string()
            .contains("capacity is exhausted")
    );
    assert_eq!(missing.starts.len(), MAX_START_RESERVATIONS + 1);
    assert!(!missing.expired_starts.contains("missing-0"));

    scheduler.save_state(&state).unwrap();
    assert!(
        scheduler
            .start("caller", "start-0", "root".into(), Vec::new(), None, now(),)
            .unwrap_err()
            .to_string()
            .contains("idempotency window expired")
    );

    for index in 0..MAX_WAKE_RESERVATIONS {
        state
            .wakes
            .insert(format!("wake-{index:02}"), wake_reservation(index, true));
    }
    state.compact_delivered_wakes();
    assert_eq!(state.wakes.len(), RETAIN_DELIVERED_WAKES);
    assert!(state.expired_wakes.contains("wake-00"));

    state.wakes.clear();
    for index in 0..=MAX_WAKE_RESERVATIONS {
        state.wakes.insert(
            format!("pending-{index:02}"),
            wake_reservation(index, false),
        );
    }
    state.compact_delivered_wakes();
    assert_eq!(state.wakes.len(), MAX_WAKE_RESERVATIONS + 1);
    assert!(
        scheduler
            .save_state(&state)
            .unwrap_err()
            .to_string()
            .contains("hard limit")
    );

    let run_id = SupervisorRunId::new();
    let parent_id = TaskId::new("parent-capacity").unwrap();
    let child = OperationId::new();
    let mut run = SupervisorRun::new_with_id(
        run_id,
        "caller".into(),
        "task".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    let mut parent = task(run_id, "parent-capacity", None);
    parent.state = TaskState::AwaitingDecision;
    run.tasks.insert(parent_id.clone(), parent);
    run.provenance.insert(
        parent_id.clone(),
        provenance(run_id, &parent_id, None, OperationId::new()),
    );
    state.wakes.clear();
    state.wakes = (0..MAX_WAKE_RESERVATIONS)
        .map(|index| (format!("full-{index}"), wake_reservation(index, false)))
        .collect();
    scheduler.save_state(&state).unwrap();
    assert!(
        scheduler
            .reserve_parent_wake(&mut run, &parent_id, child, InboxKind::Completed, now(),)
            .unwrap_err()
            .to_string()
            .contains("capacity is exhausted")
    );
    state.wakes.clear();
    let wake_key = format!("{}:{}:{}", child, parent_id.0, 1);
    state.expired_wakes.insert(&wake_key);
    scheduler.save_state(&state).unwrap();
    scheduler
        .reserve_parent_wake(&mut run, &parent_id, child, InboxKind::Completed, now())
        .unwrap();
    assert!(scheduler.load_state().unwrap().wakes.is_empty());

    let control_run = SupervisorRun::new(
        "workspace-operator".into(),
        "task".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    scheduler.supervisor.initialize(&control_run).unwrap();
    let command = SupervisorWorkspaceCommand::Cancel {
        supervisor_run_id: control_run.supervisor_run_id,
        reason: "capacity fixture".into(),
    };
    let digest = control_semantic_digest(&command).unwrap();
    let mut controls = RuntimeState::default();
    let mut oldest = None;
    for index in 0..MAX_CONTROL_RESERVATIONS {
        let operation = OperationId::new().to_string();
        oldest.get_or_insert_with(|| operation.clone());
        controls.controls.insert(
            operation,
            ControlReservation {
                semantic_digest: digest.clone(),
                supervisor_run_id: control_run.supervisor_run_id,
                reserved_at: now() + chrono::Duration::seconds(i64::try_from(index).unwrap()),
            },
        );
    }
    assert!(
        scheduler
            .ensure_control_capacity(&mut controls)
            .unwrap_err()
            .to_string()
            .contains("capacity is exhausted")
    );
    let mut finished_control = control_run;
    finished_control.state = SupervisorRunState::Cancelled;
    finished_control.terminal_at = Some(now());
    json_file::write_atomic(
        scheduler
            .supervisor
            .snapshot_path(finished_control.supervisor_run_id)
            .parent()
            .unwrap(),
        &scheduler
            .supervisor
            .snapshot_path(finished_control.supervisor_run_id),
        &finished_control,
    )
    .unwrap();
    scheduler.ensure_control_capacity(&mut controls).unwrap();
    assert_eq!(controls.controls.len(), MAX_CONTROL_RESERVATIONS - 1);
    assert!(controls.expired_controls.contains(&oldest.unwrap()));
}

#[test]
fn oversized_or_malformed_runtime_metadata_fails_closed_on_load() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let mut state = RuntimeState {
        wakes: (0..=MAX_WAKE_RESERVATIONS)
            .map(|index| (format!("pending-{index}"), wake_reservation(index, false)))
            .collect(),
        ..RuntimeState::default()
    };
    json_file::write_atomic(temp.path(), &scheduler.state_path, &state).unwrap();
    assert!(
        scheduler
            .load_state()
            .unwrap_err()
            .to_string()
            .contains("hard limit")
    );

    state.wakes.clear();
    state.expired_starts.words.push(1);
    json_file::write_atomic(temp.path(), &scheduler.state_path, &state).unwrap();
    assert!(
        scheduler
            .load_state()
            .unwrap_err()
            .to_string()
            .contains("hard limit")
    );

    let mut malformed_control = RuntimeState::default();
    malformed_control.controls.insert(
        "not-an-operation-id".into(),
        ControlReservation {
            semantic_digest: "unbounded-or-invalid".into(),
            supervisor_run_id: SupervisorRunId::new(),
            reserved_at: now(),
        },
    );
    json_file::write_atomic(temp.path(), &scheduler.state_path, &malformed_control).unwrap();
    assert!(
        scheduler
            .load_state()
            .unwrap_err()
            .to_string()
            .contains("hard limit")
    );

    let mut legacy = RuntimeState::default();
    legacy.starts.insert(
        "legacy-operation".into(),
        StartReservation {
            semantic_key: "legacy raw semantic material".into(),
            supervisor_run_id: SupervisorRunId::new(),
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
    json_file::write_atomic(temp.path(), &scheduler.state_path, &legacy).unwrap();
    let migrated = scheduler.load_state().unwrap();
    assert_eq!(
        migrated.starts["legacy-operation"].semantic_key,
        semantic_digest(b"legacy raw semantic material")
    );

    std::fs::File::create(&scheduler.state_path)
        .unwrap()
        .set_len(u64::try_from(MAX_RUNTIME_STATE_BYTES + 1).unwrap())
        .unwrap();
    assert!(
        scheduler
            .load_state()
            .unwrap_err()
            .to_string()
            .contains("JSON limit")
    );
}

#[test]
fn a_missing_run_is_a_noop_and_does_not_call_the_waker() {
    let temp = tempfile::tempdir().unwrap();
    let scheduler = SupervisorRuntime::new(temp.path());
    let initial = SupervisorRun::new(
        "caller".into(),
        "root".into(),
        "input".into(),
        "policy".into(),
        now(),
    );
    let mut waker = Waker::default();
    scheduler
        .tick(initial.supervisor_run_id, now(), &mut waker)
        .unwrap();
    assert!(waker.wakes.is_empty());
}

#[test]
fn incomplete_parent_provenance_is_fail_closed_after_child_completion() {
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
    let child = TaskId::new("child").unwrap();
    let child_run = OperationId::new();
    let mut parent_task = task(run.supervisor_run_id, "parent", None);
    parent_task.state = TaskState::AwaitingDecision;
    let mut child_task = task(run.supervisor_run_id, "child", Some("parent"));
    child_task.state = TaskState::Dispatched;
    run.tasks = BTreeMap::from([(parent.clone(), parent_task), (child.clone(), child_task)]);
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
    dispatch
        .upsert_run(DispatchRun {
            run_id: child_run,
            agent_id: AgentId::new(),
            prompt: "child".into(),
            started_at: now(),
            ended_at: Some(now()),
            status: RunStatus::NoReport,
        })
        .unwrap();
    let mut waker = Waker::default();
    scheduler
        .tick(run.supervisor_run_id, now(), &mut waker)
        .unwrap();
    assert_eq!(
        store.load(run.supervisor_run_id).unwrap().unwrap().tasks[&child].state,
        TaskState::Failed
    );
    assert!(waker.wakes.is_empty());
}

#[test]
#[should_panic(expected = "history deletion does not append an aggregate event")]
fn history_deletion_cannot_be_encoded_as_an_aggregate_event() {
    let run = aborted_run(Some(WorkspaceId::new()));
    let command = SupervisorWorkspaceCommand::Delete {
        supervisor_run_id: run.supervisor_run_id,
        observed_state_revision: run.state_revision,
    };
    let _ = control_event(
        &run,
        OperationId::new(),
        "sha256:delete".into(),
        &command,
        now(),
    );
}

#[test]
fn unfinished_workspace_detection_is_scoped_and_terminal_safe() {
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
    assert!(runtime.has_unfinished_workspace(workspace).unwrap());
    assert!(
        !runtime
            .has_unfinished_workspace(WorkspaceId::new())
            .unwrap()
    );
    runtime
        .control_for_workspace(
            workspace,
            OperationId::new(),
            &SupervisorWorkspaceCommand::Cancel {
                supervisor_run_id: run.supervisor_run_id,
                reason: "operator cancelled".into(),
            },
            now(),
        )
        .unwrap();
    assert!(!runtime.has_unfinished_workspace(workspace).unwrap());
}

#[test]
fn workspace_listing_exposes_only_explicitly_scoped_runs() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = SupervisorRuntime::new(temp.path());
    let first = WorkspaceId::new();
    let second = WorkspaceId::new();
    let visible = runtime
        .start_for_workspace(
            "caller-a",
            first,
            "scoped-a",
            "root".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    runtime
        .start_for_workspace(
            "caller-b",
            second,
            "scoped-b",
            "root".into(),
            Vec::new(),
            None,
            now(),
        )
        .unwrap();
    runtime
        .start("legacy", "unscoped", "root".into(), Vec::new(), None, now())
        .unwrap();

    let listed = runtime.list_workspace(first).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].supervisor_run_id, visible.supervisor_run_id);
    assert_eq!(
        runtime
            .get_for_workspace(first, visible.supervisor_run_id)
            .unwrap(),
        Some(visible)
    );
    assert_eq!(
        runtime
            .get_for_workspace(second, listed[0].supervisor_run_id)
            .unwrap(),
        None
    );
    assert_eq!(
        runtime
            .get_for_workspace(first, SupervisorRunId::new())
            .unwrap(),
        None
    );
    assert!(
        runtime
            .list_workspace(WorkspaceId::new())
            .unwrap()
            .is_empty()
    );
}
