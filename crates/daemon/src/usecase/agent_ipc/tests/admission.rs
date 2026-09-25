//! admission の振る舞いを固定するテスト。

use super::*;

#[test]
fn saturated_launch_sleeps_the_oldest_completed_resumable_agent() {
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
    // A one-slot fixture exercises the shipping 16-slot policy without
    // launching sixteen identical test processes.
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
    let first_runtime = agent
        .coordinator
        .runtime_for_terminal(&first.terminal)
        .unwrap();
    agent
        .reported_phases
        .insert(first_runtime.agent_runtime_id, AgentPhase::Ended);

    let second = agent
        .launch(
            &OperationId::new().to_string(),
            &AgentLaunchIntent {
                workspace,
                session: Some(session),
                profile: None,
            },
            &FakeScope(Ok(resolved)),
        )
        .unwrap();

    let records = agent.coordinator.snapshot().records;
    assert_eq!(agent.concurrency().in_use, 1);
    assert!(records.iter().any(|record| {
        record.runtime.terminal == first.terminal
            && record.state == crate::usecase::runtime::RuntimeState::Sleeping
    }));
    assert!(records.iter().any(|record| {
        record.runtime.terminal == second.terminal
            && record.state == crate::usecase::runtime::RuntimeState::Running
    }));
    assert_eq!(agent.session_phase(session), AgentPhase::Running);
    assert!(agent.inventory(workspace).runtimes.iter().any(|runtime| {
        runtime.runtime.terminal == first.terminal
            && runtime.state == AgentRuntimeInventoryState::Sleeping
    }));
}

#[test]
fn integration_diagnosis_admits_only_resumable_or_ownership_unknown_states() {
    use crate::usecase::runtime::{ReconcileState, RuntimeState};

    for state in [
        RuntimeState::Reserved,
        RuntimeState::Running,
        RuntimeState::Exited,
        RuntimeState::Interrupted,
        RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown),
    ] {
        assert!(integration_diagnosable_state(state));
    }
    for state in [
        RuntimeState::Reclaimed,
        RuntimeState::SpawnFailed,
        RuntimeState::ReconcileRequired(ReconcileState::SpawnAmbiguous),
        RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning),
    ] {
        assert!(!integration_diagnosable_state(state));
    }
}
