//! runtime の振る舞いを固定するテスト。

use super::*;
use std::{collections::BTreeSet, path::PathBuf};
use usagi_core::domain::{
    agent::{
        AgentProfileId, LaunchMode, LaunchPlan, LaunchScope, ProviderCaptureProvenance,
        ProviderKind, ProviderSessionId,
    },
    id::{
        AgentRuntimeId, ClientId, DaemonGeneration, OperationId, RequestId, SessionId, TerminalId,
        WorkspaceId, WorktreeId,
    },
};
use usagi_core::infrastructure::ipc::AgentConcurrency;
#[test]
fn spawn_provision_carries_an_optional_ephemeral_sandbox_launcher() {
    let mut provision = SpawnProvision::new([], Vec::new());
    assert!(provision.sandbox_launcher().is_none());
    let launcher = SandboxLauncher {
        program: "/usr/bin/usagi".to_owned(),
        prefix: vec!["claude-sandbox".to_owned(), "--".to_owned()],
    };
    provision.set_sandbox_launcher(launcher.clone());
    assert_eq!(provision.sandbox_launcher(), Some(&launcher));
    // derive された Debug / Clone / PartialEq を実行する。
    assert_eq!(launcher.clone(), launcher);
    assert!(format!("{launcher:?}").contains("claude-sandbox"));
}

#[derive(Default)]
struct Store(Vec<RuntimeStoreSnapshot>);
impl RuntimeStore for Store {
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), ()> {
        self.0.push(snapshot);
        Ok(())
    }
}
struct ConditionalStore {
    saves: usize,
    fail_after: Option<usize>,
}
impl RuntimeStore for ConditionalStore {
    fn save(&mut self, _: RuntimeStoreSnapshot) -> Result<(), ()> {
        self.saves += 1;
        if self.fail_after.is_some_and(|limit| self.saves > limit) {
            Err(())
        } else {
            Ok(())
        }
    }
}
struct FailingStore(usize);
impl RuntimeStore for FailingStore {
    fn save(&mut self, _: RuntimeStoreSnapshot) -> Result<(), ()> {
        self.0 += 1;
        if self.0 == 2 { Err(()) } else { Ok(()) }
    }
}
#[derive(Default)]
struct Resolver {
    calls: usize,
}
impl AgentAdapter for Resolver {
    fn resolve(&mut self, request: &LaunchRequest) -> Result<ResolvedLaunch, AdapterError> {
        self.calls += 1;
        let provider_resume = request.provider_resume.clone();
        let mut durable_request = request.clone();
        durable_request.provider_resume = None;
        Ok(ResolvedLaunch {
            snapshot: DurableLaunchSnapshot::new(
                durable_request,
                LaunchPlan::new(
                    request.profile_id.clone(),
                    7,
                    "agent",
                    vec!["--safe".into()],
                    [],
                    PathBuf::from("."),
                )
                .unwrap(),
            ),
            provision: SpawnProvision::new([], Vec::new()),
            provider_resume,
        })
    }
}
struct Spawner(Result<ProcessIdentity, SpawnFailure>);
impl PtySpawner for Spawner {
    fn spawn(
        &mut self,
        _: &DurableLaunchSnapshot,
        _: &SpawnProvision,
        _: &TerminalRef,
    ) -> Result<ProcessIdentity, SpawnFailure> {
        self.0.clone()
    }
}
struct CompensatingSpawner {
    terminated: bool,
}
impl PtySpawner for CompensatingSpawner {
    fn spawn(
        &mut self,
        _: &DurableLaunchSnapshot,
        _: &SpawnProvision,
        _: &TerminalRef,
    ) -> Result<ProcessIdentity, SpawnFailure> {
        Ok(process())
    }
    fn terminate_reap(&mut self, _: &TerminalRef) -> Result<(), TerminateReapError> {
        self.terminated = true;
        Ok(())
    }
}
#[derive(Default)]
struct Journal(Vec<Output>);
impl OutputJournal for Journal {
    fn append(&mut self, output: &Output) -> Result<(), ()> {
        self.0.push(output.clone());
        Ok(())
    }
}
fn request() -> LaunchRequest {
    LaunchRequest {
        profile_id: AgentProfileId::new("test").unwrap(),
        mode: LaunchMode::Interactive,
        model: None,
        resume: false,
        provider_resume: None,
        initial_prompt: None,
        scope: LaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        },
        required_capabilities: BTreeSet::new(),
    }
}
fn refs(request: &LaunchRequest) -> (AgentRuntimeRef, CompletionFence) {
    static GENERATION: std::sync::OnceLock<DaemonGeneration> = std::sync::OnceLock::new();
    let generation = *GENERATION.get_or_init(DaemonGeneration::new);
    let terminal = TerminalRef {
        daemon_generation: generation,
        terminal_id: TerminalId::new(),
        workspace_id: request.scope.workspace_id,
        session_id: request.scope.session_id,
        worktree_id: request.scope.worktree_id,
    };
    let runtime =
        AgentRuntimeRef::new(AgentRuntimeId::new(), terminal, request.scope.session_id).unwrap();
    let fence = CompletionFence {
        workspace_id: request.scope.workspace_id,
        session_id: request.scope.session_id,
        operation_id: OperationId::new(),
        owner_daemon_generation: generation,
        execution_attempt: 1,
        lifecycle_attempt: 1,
        expected_revision: 1,
    };
    (runtime, fence)
}
fn process() -> ProcessIdentity {
    ProcessIdentity {
        pid: 7,
        start_identity: "start".into(),
        process_group: 7,
    }
}

#[test]
fn restart_reconcile_marks_only_unfinished_runtimes_identity_unknown() {
    let request = request();
    let (runtime, operation) = refs(&request);
    let launch = Resolver { calls: 0 }.resolve(&request).unwrap().snapshot;
    let snapshot = RuntimeStoreSnapshot {
        schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
        records: vec![
            DurableRuntimeRecord {
                runtime: runtime.clone(),
                operation: operation.clone(),
                launch: launch.clone(),
                state: RuntimeState::Running,
                process: Some(process()),
                provider_resume: None,
                continuation: None,
                resume_source: None,
                resumed_from: None,
                superseded_by: None,
                semantic_key: Some("first".into()),
                outcome: DurableOperationOutcome::Accepted,
                credential_provenance: Some(CredentialProvenance::DaemonMintedEphemeral),
            },
            DurableRuntimeRecord {
                runtime,
                operation,
                launch,
                state: RuntimeState::Exited,
                process: Some(process()),
                provider_resume: None,
                continuation: None,
                resume_source: None,
                resumed_from: None,
                superseded_by: None,
                semantic_key: Some("second".into()),
                outcome: DurableOperationOutcome::Completed,
                credential_provenance: Some(CredentialProvenance::DaemonMintedEphemeral),
            },
        ],
        generation: GenerationSnapshot::default(),
    };

    let (reconciled, interrupted) = snapshot.reconcile_after_daemon_restart();

    assert_eq!(interrupted, 1);
    assert_eq!(
        reconciled.records[0].state,
        RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown)
    );
    assert_eq!(reconciled.records[1].state, RuntimeState::Exited);
}

#[test]
#[allow(clippy::too_many_lines)] // One source fixture exercises every pre-reservation resume lineage fence.
fn resume_rejects_a_live_superseded_runtime_before_reserving_a_replacement() {
    let request = request();
    let (source, source_fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(2, 64, 1);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut coordinator,
        &request,
        source.clone(),
        source_fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    let (replacement, replacement_fence) = refs(&request);

    assert_eq!(
        coordinator.resume_with_semantic(
            &request,
            replacement.clone(),
            replacement_fence.clone(),
            Geometry { cols: 80, rows: 24 },
            &mut Resolver::default(),
            &mut store,
            &mut spawner,
            None,
            "resume".into(),
            std::slice::from_ref(&source),
        ),
        Err(RuntimeError::ProviderResumeMismatch)
    );
    assert_eq!(coordinator.snapshot().records.len(), 1);
    coordinator.exit(&source, 0, &mut store).unwrap();
    assert_eq!(
        coordinator.resume_with_semantic(
            &request,
            replacement.clone(),
            replacement_fence.clone(),
            Geometry { cols: 80, rows: 24 },
            &mut Resolver::default(),
            &mut store,
            &mut spawner,
            None,
            "multiple-sources".into(),
            &[source.clone(), source.clone()],
        ),
        Err(RuntimeError::ProviderResumeMismatch)
    );
    coordinator
        .records
        .get_mut(&source.agent_runtime_id.as_str())
        .unwrap()
        .superseded_by = Some(AgentRuntimeId::new());
    assert_eq!(
        coordinator.resume_with_semantic(
            &request,
            replacement.clone(),
            replacement_fence.clone(),
            Geometry { cols: 80, rows: 24 },
            &mut Resolver::default(),
            &mut store,
            &mut spawner,
            None,
            "already-superseded".into(),
            std::slice::from_ref(&source),
        ),
        Err(RuntimeError::ProviderResumeMismatch)
    );
    let source_record = coordinator
        .records
        .get_mut(&source.agent_runtime_id.as_str())
        .unwrap();
    source_record.superseded_by = None;
    source_record.continuation = None;
    assert_eq!(
        coordinator.resume_with_semantic(
            &request,
            replacement,
            replacement_fence,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver::default(),
            &mut store,
            &mut spawner,
            None,
            "missing-lineage".into(),
            &[source],
        ),
        Err(RuntimeError::ProviderResumeMismatch)
    );
}

#[test]
fn reconcile_projects_provider_metadata_for_gone_and_interrupted_processes() {
    for (observation, expected_status, expected_phase) in [
        (
            ProcessObservation::Gone,
            ProviderResumeStatus::Exited,
            ProviderResumePhase::Ended,
        ),
        (
            ProcessObservation::Unknown,
            ProviderResumeStatus::Interrupted,
            ProviderResumePhase::Interrupted,
        ),
    ] {
        let mut request = request();
        request.resume = true;
        request
            .required_capabilities
            .insert(usagi_core::domain::agent::AgentCapability::Resume);
        request.provider_resume = Some(ProviderResumeRef {
            provider: ProviderKind::Claude,
            native_session_id: ProviderSessionId::new("provider-session").unwrap(),
            adapter_revision: 7,
            scope: request.scope.clone(),
            provenance: ProviderCaptureProvenance::ProviderStructured,
            last_known_status: ProviderResumeStatus::Active,
            last_known_phase: Some(ProviderResumePhase::Running),
        });
        let (runtime, fence) = refs(&request);
        let mut coordinator = RuntimeCoordinator::new(1, 64, 1);
        let mut store = Store::default();
        launch(
            &mut coordinator,
            &request,
            runtime.clone(),
            fence,
            &mut Spawner(Ok(process())),
            &mut store,
        )
        .unwrap();

        coordinator
            .reconcile(&runtime, observation, &mut store)
            .unwrap();
        let provider = coordinator
            .record_for(&runtime)
            .unwrap()
            .provider_resume
            .as_ref()
            .unwrap();
        assert_eq!(provider.last_known_status, expected_status);
        assert_eq!(provider.last_known_phase, Some(expected_phase));
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One table-style test covers every snapshot validation edge.
fn hydrate_validates_schema_identity_and_legacy_outcomes() {
    assert_eq!(
        RuntimeStoreSnapshot::default(),
        RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: Vec::new(),
            generation: GenerationSnapshot::default(),
        }
    );
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: 99,
            records: Vec::new(),
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::UnknownSchema(99)
    );
    assert_eq!(
        RuntimeCoordinator::hydrate(
            RuntimeStoreSnapshot {
                schema_version: 99,
                records: Vec::new(),
                generation: GenerationSnapshot::default(),
            },
            1,
            64,
            1,
        )
        .unwrap_err(),
        RuntimeSnapshotError::UnknownSchema(99)
    );
    assert!(RuntimeCoordinator::hydrate(RuntimeStoreSnapshot::default(), 1, 64, 1).is_ok());

    let request = request();
    let (runtime, operation) = refs(&request);
    let launch = Resolver::default().resolve(&request).unwrap().snapshot;
    let record = DurableRuntimeRecord {
        runtime,
        operation,
        launch,
        state: RuntimeState::Exited,
        process: Some(process()),
        provider_resume: None,
        continuation: None,
        resume_source: None,
        resumed_from: None,
        superseded_by: None,
        semantic_key: Some("intent".into()),
        outcome: DurableOperationOutcome::Completed,
        credential_provenance: Some(CredentialProvenance::DaemonMintedEphemeral),
    };
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![record.clone()],
            generation: GenerationSnapshot::default(),
        })
        .unwrap()
        .len(),
        1
    );

    let mut mismatched = record.clone();
    mismatched.operation.workspace_id = WorkspaceId::new();
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![mismatched],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::ScopeMismatch
    );

    let mut mismatched_launch_scope = record.clone();
    mismatched_launch_scope.launch.request.scope.worktree_id = WorktreeId::new();
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![mismatched_launch_scope],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::ScopeMismatch
    );

    let mut same_runtime = record.clone();
    same_runtime.operation.operation_id = OperationId::new();
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![record.clone(), same_runtime],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::DuplicateRuntime
    );

    let (other_runtime, mut same_operation) = refs(&request);
    same_operation.operation_id = record.operation.operation_id;
    let duplicate_operation = DurableRuntimeRecord {
        runtime: other_runtime,
        operation: same_operation,
        ..record.clone()
    };
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![record.clone(), duplicate_operation],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::DuplicateOperation
    );

    let continuation = usagi_core::domain::id::AgentContinuationRef::new();
    let source_id = usagi_core::domain::id::AgentResumeSourceId::new();
    let mut lineage_source = record.clone();
    lineage_source.continuation = Some(continuation);
    lineage_source.resume_source = Some(source_id);
    let (replacement_runtime, replacement_operation) = refs(&request);
    let mut replacement = DurableRuntimeRecord {
        runtime: replacement_runtime,
        operation: replacement_operation,
        ..record.clone()
    };
    replacement.continuation = Some(continuation);
    replacement.resume_source = Some(usagi_core::domain::id::AgentResumeSourceId::new());
    replacement.resumed_from = Some(source_id);
    lineage_source.superseded_by = Some(replacement.runtime.agent_runtime_id);
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![lineage_source.clone(), replacement.clone()],
            generation: GenerationSnapshot::default(),
        })
        .unwrap()
        .len(),
        2
    );
    let mut mismatched_continuation = replacement.clone();
    mismatched_continuation.continuation =
        Some(usagi_core::domain::id::AgentContinuationRef::new());
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![lineage_source.clone(), mismatched_continuation],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::ResumeRelation
    );
    let mut missing_source_backref = lineage_source.clone();
    missing_source_backref.superseded_by = None;
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![missing_source_backref.clone(), replacement.clone()],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::ResumeRelation
    );
    let foreign_generation = DaemonGeneration::new();
    let mut foreign_replacement = replacement.clone();
    foreign_replacement.runtime.terminal.daemon_generation = foreign_generation;
    foreign_replacement.operation.owner_daemon_generation = foreign_generation;
    let repaired = hydrated_records(RuntimeStoreSnapshot {
        schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
        records: vec![missing_source_backref.clone(), foreign_replacement.clone()],
        generation: GenerationSnapshot::default(),
    })
    .unwrap();
    assert_eq!(
        repaired
            .values()
            .find(|candidate| candidate.resume_source == Some(source_id))
            .unwrap()
            .superseded_by,
        Some(foreign_replacement.runtime.agent_runtime_id)
    );
    let mut conflicting_source = lineage_source.clone();
    conflicting_source.superseded_by = Some(AgentRuntimeId::new());
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![conflicting_source, foreign_replacement.clone()],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::ResumeRelation
    );
    let (competing_runtime, competing_operation) = refs(&request);
    let mut competing_replacement = DurableRuntimeRecord {
        runtime: competing_runtime,
        operation: competing_operation,
        resume_source: Some(usagi_core::domain::id::AgentResumeSourceId::new()),
        ..replacement.clone()
    };
    competing_replacement.runtime.terminal.daemon_generation = foreign_generation;
    competing_replacement.operation.owner_daemon_generation = foreign_generation;
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![
                missing_source_backref,
                foreign_replacement,
                competing_replacement,
            ],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::ResumeRelation
    );
    let mut unknown_replacement = lineage_source.clone();
    unknown_replacement.superseded_by = Some(AgentRuntimeId::new());
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![unknown_replacement],
            generation: GenerationSnapshot::default(),
        })
        .unwrap()
        .len(),
        1,
        "a superseded source remains a no-double-resume tombstone after replacement GC"
    );
    let mut missing_replacement_backref = replacement.clone();
    missing_replacement_backref.resumed_from = None;
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![lineage_source.clone(), missing_replacement_backref],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::ResumeRelation
    );
    let (unfenced_runtime, unfenced_operation) = refs(&request);
    let mut unfenced_source = record.clone();
    unfenced_source.superseded_by = Some(unfenced_runtime.agent_runtime_id);
    let unfenced_replacement = DurableRuntimeRecord {
        runtime: unfenced_runtime,
        operation: unfenced_operation,
        ..record.clone()
    };
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![unfenced_source, unfenced_replacement],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::ResumeRelation
    );
    let first_source_id = usagi_core::domain::id::AgentResumeSourceId::new();
    let second_source_id = usagi_core::domain::id::AgentResumeSourceId::new();
    let (first_runtime, first_operation) = refs(&request);
    let (second_runtime, second_operation) = refs(&request);
    let mut first_cycle = DurableRuntimeRecord {
        runtime: first_runtime,
        operation: first_operation,
        continuation: Some(continuation),
        resume_source: Some(first_source_id),
        resumed_from: Some(second_source_id),
        ..record.clone()
    };
    let second_cycle = DurableRuntimeRecord {
        runtime: second_runtime,
        operation: second_operation,
        continuation: Some(continuation),
        resume_source: Some(second_source_id),
        resumed_from: Some(first_source_id),
        superseded_by: Some(first_cycle.runtime.agent_runtime_id),
        ..record.clone()
    };
    first_cycle.superseded_by = Some(second_cycle.runtime.agent_runtime_id);
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![first_cycle, second_cycle],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::ResumeRelation
    );
    let mut duplicate_source = replacement.clone();
    duplicate_source.resume_source = Some(source_id);
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![lineage_source.clone(), duplicate_source],
            generation: GenerationSnapshot::default(),
        })
        .unwrap_err(),
        RuntimeSnapshotError::DuplicateResumeSource
    );
    let mut broken_relation = replacement;
    broken_relation.resumed_from = Some(usagi_core::domain::id::AgentResumeSourceId::new());
    assert_eq!(
        hydrated_records(RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![broken_relation],
            generation: GenerationSnapshot::default(),
        })
        .unwrap()
        .len(),
        1,
        "a replacement may outlive its bounded source shard"
    );

    let mut legacy = record;
    legacy.semantic_key = None;
    legacy.outcome = DurableOperationOutcome::Accepted;
    let legacy: RuntimeStoreSnapshot = serde_json::from_value(serde_json::json!({
        "records": [legacy]
    }))
    .unwrap();
    assert_eq!(legacy.schema_version, 1);
    legacy.validate_ownership().unwrap();
    let (legacy, interrupted) = legacy.reconcile_after_daemon_restart();
    assert_eq!(interrupted, 0);
    assert_eq!(legacy.schema_version, RUNTIME_SNAPSHOT_SCHEMA_VERSION);
    assert_eq!(
        legacy.records[0].outcome,
        DurableOperationOutcome::OwnershipUnknown
    );
}

#[test]
fn corrupt_generation_binding_fails_closed_before_hydrate() {
    let request = request();
    let (runtime, fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(1, 64, 1);
    let mut store = Store::default();
    launch(
        &mut coordinator,
        &request,
        runtime,
        fence,
        &mut Spawner(Ok(process())),
        &mut store,
    )
    .unwrap();
    let mut corrupt = coordinator.snapshot();
    corrupt.generation.terminals[0].terminal.worktree_id = WorktreeId::new();

    assert_eq!(
        corrupt.validate_ownership(),
        Err(RuntimeSnapshotError::Generation)
    );
    assert_eq!(
        RuntimeCoordinator::hydrate(corrupt, 1, 64, 1).unwrap_err(),
        RuntimeSnapshotError::Generation
    );
}

#[test]
fn terminal_ownership_projection_covers_orphan_and_lost_states() {
    assert_eq!(
        terminal_ownership_state(RuntimeState::ReconcileRequired(
            ReconcileState::OrphanRunning
        )),
        TerminalState::OrphanRunning
    );
    assert_eq!(
        terminal_ownership_state(RuntimeState::SpawnFailed),
        TerminalState::Lost
    );
    assert_eq!(
        terminal_ownership_state(RuntimeState::Reclaimed),
        TerminalState::Lost
    );
}

#[test]
fn durable_snapshot_schema_round_trips_every_safe_outcome_and_rejects_unknown_fields() {
    let request = request();
    let (runtime, operation) = refs(&request);
    let launch = Resolver::default().resolve(&request).unwrap().snapshot;
    for outcome in [
        DurableOperationOutcome::Accepted,
        DurableOperationOutcome::ResumeSucceeded,
        DurableOperationOutcome::Completed,
        DurableOperationOutcome::SpawnUnavailable,
        DurableOperationOutcome::ExitUnavailable,
        DurableOperationOutcome::OwnershipUnknown,
    ] {
        let snapshot = RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: vec![DurableRuntimeRecord {
                runtime: runtime.clone(),
                operation: operation.clone(),
                launch: launch.clone(),
                state: RuntimeState::Exited,
                process: Some(process()),
                provider_resume: None,
                continuation: None,
                resume_source: None,
                resumed_from: None,
                superseded_by: None,
                semantic_key: Some("intent".into()),
                outcome,
                credential_provenance: Some(CredentialProvenance::DaemonMintedEphemeral),
            }],
            generation: GenerationSnapshot::default(),
        };
        assert_eq!(
            serde_json::from_str::<RuntimeStoreSnapshot>(
                &serde_json::to_string(&snapshot).unwrap()
            )
            .unwrap(),
            snapshot
        );
    }
    assert!(
        serde_json::from_value::<RuntimeStoreSnapshot>(serde_json::json!({
            "schema_version": RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            "records": [],
            "future_field": true
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<RuntimeStoreSnapshot>(serde_json::json!({
            "schema_version": RUNTIME_SNAPSHOT_SCHEMA_VERSION
        }))
        .is_err()
    );
}
fn launch<S: RuntimeStore, P: PtySpawner>(
    coordinator: &mut RuntimeCoordinator,
    request: &LaunchRequest,
    runtime: AgentRuntimeRef,
    fence: CompletionFence,
    spawner: &mut P,
    store: &mut S,
) -> Result<(), RuntimeError> {
    coordinator.launch(
        request,
        runtime,
        fence,
        Geometry { cols: 80, rows: 24 },
        &mut Resolver::default(),
        store,
        spawner,
        None,
    )
}

#[test]
fn closing_an_empty_session_converges_without_a_terminal_snapshot() {
    let mut coordinator = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = CompensatingSpawner { terminated: false };

    assert!(
        coordinator
            .close_session(SessionId::new(), &mut store, &mut spawner)
            .unwrap()
            .is_empty()
    );
    assert!(!spawner.terminated);
    assert_eq!(store.0.len(), 1, "only the converging save is required");
    assert!(store.0[0].records.is_empty());
}

#[test]
fn closing_a_session_terminates_and_forgets_its_agent_runtime() {
    let request = request();
    let session = request.scope.session_id.unwrap();
    let (runtime, fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = CompensatingSpawner { terminated: false };
    launch(
        &mut coordinator,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    coordinator
        .write_provider_resume(
            &runtime,
            ProviderResumeRef {
                provider: ProviderKind::Claude,
                native_session_id: ProviderSessionId::new("closing-session").unwrap(),
                adapter_revision: 7,
                scope: request.scope.clone(),
                provenance: ProviderCaptureProvenance::DaemonIssued,
                last_known_status: ProviderResumeStatus::Active,
                last_known_phase: Some(ProviderResumePhase::Running),
            },
            ProviderResumeWrite::Attach,
            &mut store,
        )
        .unwrap();

    let closed = coordinator
        .close_session(session, &mut store, &mut spawner)
        .unwrap();

    assert!(spawner.terminated);
    assert_eq!(closed, [runtime]);
    assert!(coordinator.snapshot().records.is_empty());
    coordinator.snapshot().validate_ownership().unwrap();
    assert_eq!(
        store.0[store.0.len() - 2].records[0].state,
        RuntimeState::Reclaimed,
        "the terminal state is durable before the record is forgotten"
    );
    let provider = store.0[store.0.len() - 2].records[0]
        .provider_resume
        .as_ref()
        .unwrap();
    assert_eq!(provider.last_known_status, ProviderResumeStatus::Exited);
    assert_eq!(provider.last_known_phase, Some(ProviderResumePhase::Ended));
    assert!(store.0.last().unwrap().records.is_empty());
}

#[test]
fn closing_a_workspace_leaves_another_workspaces_live_agent_untouched() {
    let first_request = request();
    let second_request = request();
    let (first, first_fence) = refs(&first_request);
    let (second, second_fence) = refs(&second_request);
    let mut coordinator = RuntimeCoordinator::new(2, 1024, 2);
    let mut store = Store::default();
    let mut spawner = CompensatingSpawner { terminated: false };
    for (request, runtime, fence) in [
        (&first_request, first.clone(), first_fence),
        (&second_request, second.clone(), second_fence),
    ] {
        launch(
            &mut coordinator,
            request,
            runtime,
            fence,
            &mut spawner,
            &mut store,
        )
        .unwrap();
    }

    assert_eq!(
        coordinator
            .close_workspace(first.terminal.workspace_id, &mut store, &mut spawner)
            .unwrap(),
        [first]
    );
    assert!(spawner.terminated);
    assert_eq!(coordinator.snapshot().records.len(), 1);
    assert_eq!(coordinator.snapshot().records[0].runtime, second);
}

#[test]
fn closing_a_session_forgets_an_agent_that_already_exited() {
    let request = request();
    let session = request.scope.session_id.unwrap();
    let (runtime, fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = CompensatingSpawner { terminated: false };
    launch(
        &mut coordinator,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    coordinator.exit(&runtime, 0, &mut store).unwrap();

    assert_eq!(
        coordinator
            .close_session(session, &mut store, &mut spawner)
            .unwrap(),
        [runtime]
    );
    assert!(!spawner.terminated);
    assert!(coordinator.snapshot().records.is_empty());
    assert_eq!(
        store.0[store.0.len() - 2].records[0].state,
        RuntimeState::Reclaimed,
        "the acknowledged terminal state is durable before removal"
    );
    assert!(store.0.last().unwrap().records.is_empty());
}

#[test]
fn a_failed_pre_spawn_reservation_becomes_terminal_and_releases_capacity() {
    let request = request();
    let (runtime, fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = CompensatingSpawner { terminated: false };
    launch(
        &mut coordinator,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    let runtime_id = runtime.agent_runtime_id.as_str();
    let record = coordinator.records.get_mut(&runtime_id).unwrap();
    record.state = RuntimeState::Reserved;
    record.process = None;

    assert!(
        coordinator
            .fail_reserved_launch(&runtime, &mut store)
            .unwrap()
    );
    let record = coordinator.record_for(&runtime).unwrap();
    assert_eq!(record.state, RuntimeState::SpawnFailed);
    assert_eq!(record.outcome, DurableOperationOutcome::SpawnUnavailable);
    assert_eq!(coordinator.occupied_slots(), 0);
    assert!(
        !coordinator
            .fail_reserved_launch(&runtime, &mut store)
            .unwrap()
    );
}

#[test]
fn force_clean_repairs_only_a_processless_failed_launch() {
    let request = request();
    let (runtime, fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = CompensatingSpawner { terminated: false };
    launch(
        &mut coordinator,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    let runtime_id = runtime.agent_runtime_id.as_str();
    coordinator.records.get_mut(&runtime_id).unwrap().state = RuntimeState::Reserved;

    assert!(
        !coordinator
            .clean_failed_launch(&runtime, &mut store)
            .unwrap()
    );
    coordinator.records.get_mut(&runtime_id).unwrap().process = None;
    assert!(
        coordinator
            .clean_failed_launch(&runtime, &mut store)
            .unwrap()
    );
    assert_eq!(
        coordinator.record_for(&runtime).unwrap().state,
        RuntimeState::SpawnFailed
    );
    assert!(
        !coordinator
            .clean_failed_launch(&runtime, &mut store)
            .unwrap()
    );
}

#[test]
fn closing_a_reconciled_session_persists_termination_before_forgetting_it() {
    let request = request();
    let session = request.scope.session_id.unwrap();
    let (runtime, fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = CompensatingSpawner { terminated: false };
    launch(
        &mut coordinator,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    let (reconciled, interrupted) = coordinator.snapshot().reconcile_after_daemon_restart();
    assert_eq!(interrupted, 1);
    let mut coordinator = RuntimeCoordinator::hydrate(reconciled, 1, 1024, 2).unwrap();

    let closed = coordinator
        .close_session(session, &mut store, &mut spawner)
        .unwrap();

    assert_eq!(closed, [runtime]);
    assert!(!spawner.terminated);
    assert_eq!(
        store.0[store.0.len() - 2].records[0].state,
        RuntimeState::Reclaimed,
        "startup reconciliation remains durable long enough to release a foreign claim"
    );
    assert!(store.0.last().unwrap().records.is_empty());
}

#[test]
fn closing_a_session_keeps_an_agent_whose_process_cannot_be_reaped() {
    let request = request();
    let session = request.scope.session_id.unwrap();
    let (runtime, fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut coordinator,
        &request,
        runtime,
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();

    assert_eq!(
        coordinator.close_session(session, &mut store, &mut spawner),
        Err(RuntimeError::ReconcileRequired(
            ReconcileState::OrphanRunning
        ))
    );
    assert_eq!(coordinator.snapshot().records.len(), 1);
}

#[test]
fn closing_a_session_does_not_forget_an_ambiguous_spawn() {
    let request = request();
    let session = request.scope.session_id.unwrap();
    let (runtime, fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut coordinator,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    coordinator
        .records
        .get_mut(&runtime.agent_runtime_id.as_str())
        .unwrap()
        .state = RuntimeState::ReconcileRequired(ReconcileState::PersistAfterSpawn);

    assert!(
        coordinator
            .close_session(session, &mut store, &mut spawner)
            .is_err()
    );
    assert_eq!(coordinator.snapshot().records[0].runtime, runtime);
}

#[test]
fn interrupting_agents_marks_a_process_that_cannot_be_reaped_for_reconcile() {
    let request = request();
    let (runtime, fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut coordinator,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();

    assert_eq!(
        coordinator.interrupt_agents(
            &[runtime.agent_runtime_id.as_str().clone()]
                .into_iter()
                .collect(),
            &mut store,
            &mut spawner,
        ),
        Err(RuntimeError::ReconcileRequired(
            ReconcileState::OrphanRunning
        ))
    );
    assert_eq!(
        coordinator.snapshot().records[0].state,
        RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning)
    );
}

#[test]
fn sleeping_an_agent_releases_its_slot_and_becomes_bounded_resume_history() {
    let request = request();
    let (runtime, fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = CompensatingSpawner { terminated: false };
    launch(
        &mut coordinator,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    let reference = ProviderResumeRef {
        provider: ProviderKind::Claude,
        native_session_id: ProviderSessionId::new("sleep-source").unwrap(),
        adapter_revision: 7,
        scope: request.scope.clone(),
        provenance: ProviderCaptureProvenance::DaemonIssued,
        last_known_status: ProviderResumeStatus::Active,
        last_known_phase: Some(ProviderResumePhase::Running),
    };
    coordinator
        .write_provider_resume(&runtime, reference, ProviderResumeWrite::Attach, &mut store)
        .unwrap();

    assert_eq!(coordinator.occupied_slots(), 1);
    assert_eq!(
        coordinator.sleep_agents(
            &[runtime.agent_runtime_id.as_str()].into_iter().collect(),
            &mut store,
            &mut spawner,
        ),
        Ok(1)
    );

    let slept = &coordinator.snapshot().records[0];
    assert!(spawner.terminated);
    assert_eq!(slept.state, RuntimeState::Sleeping);
    assert_eq!(slept.process, None);
    assert_eq!(coordinator.occupied_slots(), 0);
    assert_eq!(
        slept.provider_resume.as_ref().unwrap().last_known_status,
        ProviderResumeStatus::Interrupted
    );
    assert_eq!(
        store.0.last().unwrap().records[0].state,
        RuntimeState::Sleeping
    );
    assert_eq!(
        coordinator.sleep_agents(
            &[runtime.agent_runtime_id.as_str()].into_iter().collect(),
            &mut store,
            &mut spawner,
        ),
        Ok(0)
    );

    let (replacement, replacement_fence) = refs(&request);
    coordinator
        .resume_with_semantic(
            &request,
            replacement.clone(),
            replacement_fence,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver::default(),
            &mut store,
            &mut spawner,
            None,
            "resume-sleeping".into(),
            std::slice::from_ref(&runtime),
        )
        .unwrap();

    let snapshot = coordinator.snapshot();
    let source = snapshot
        .records
        .iter()
        .find(|record| record.runtime == runtime)
        .unwrap();
    assert_eq!(source.state, RuntimeState::Reclaimed);
    assert_eq!(source.superseded_by, Some(replacement.agent_runtime_id));
    assert!(matches!(
        coordinator.retention().lookup(&runtime.terminal),
        FinalLookup::Retained(_)
    ));
    assert_eq!(coordinator.occupied_slots(), 1);
    snapshot.validate_ownership().unwrap();
}

#[test]
fn sleeping_an_agent_fails_closed_when_the_process_cannot_be_reaped() {
    let request = request();
    let (runtime, fence) = refs(&request);
    let mut coordinator = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut coordinator,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();

    assert_eq!(
        coordinator.sleep_agents(
            &[runtime.agent_runtime_id.as_str()].into_iter().collect(),
            &mut store,
            &mut spawner,
        ),
        Err(RuntimeError::ReconcileRequired(
            ReconcileState::OrphanRunning
        ))
    );
    assert_eq!(
        coordinator.snapshot().records[0].state,
        RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning)
    );
}

#[test]
fn resolve_once_persists_before_spawn_and_replays_after_detach() {
    let first_request = request();
    let (runtime, fence) = refs(&first_request);
    let mut c = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut c,
        &first_request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    assert_eq!(store.0.len(), 2);
    assert_eq!(store.0[0].records[0].state, RuntimeState::Reserved);
    let mut journal = Journal::default();
    assert_eq!(
        c.append_output(&runtime, b"hello".to_vec(), &mut journal)
            .unwrap()
            .end_offset,
        5
    );
    let connection = usagi_core::domain::id::ConnectionId::new();
    let attached = c.terminals.attach(&runtime.terminal, connection).unwrap();
    c.terminals.disconnect(connection, &mut Writer::default());
    assert_eq!(attached.snapshot.replay, b"hello");
    assert_eq!(c.occupied_slots(), 1);
}
#[test]
fn provider_phase_refinement_is_live_only_deduped_and_never_synthesizes_metadata() {
    let request = request();
    let (runtime, fence) = refs(&request);
    let mut c = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut c,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();

    // A record without provider metadata has no durable phase to refine.
    let saves = store.0.len();
    c.record_provider_phase(&runtime, ProviderResumePhase::Running, &mut store)
        .unwrap();
    assert_eq!(store.0.len(), saves);
    assert!(c.record_for(&runtime).unwrap().provider_resume.is_none());

    // With metadata, only a changed phase persists a snapshot.
    let reference = ProviderResumeRef {
        provider: usagi_core::domain::agent::ProviderKind::Claude,
        native_session_id: usagi_core::domain::agent::ProviderSessionId::new("native").unwrap(),
        adapter_revision: 7,
        scope: request.scope.clone(),
        provenance: usagi_core::domain::agent::ProviderCaptureProvenance::DaemonIssued,
        last_known_status: ProviderResumeStatus::Active,
        last_known_phase: Some(ProviderResumePhase::Starting),
    };
    c.write_provider_resume(&runtime, reference, ProviderResumeWrite::Attach, &mut store)
        .unwrap();
    let saves = store.0.len();
    c.record_provider_phase(&runtime, ProviderResumePhase::Starting, &mut store)
        .unwrap();
    assert_eq!(store.0.len(), saves);
    c.record_provider_phase(&runtime, ProviderResumePhase::Running, &mut store)
        .unwrap();
    assert_eq!(store.0.len(), saves + 1);
    let refined = c.record_for(&runtime).unwrap().provider_resume.clone();
    assert_eq!(
        refined.as_ref().and_then(|value| value.last_known_phase),
        Some(ProviderResumePhase::Running)
    );
    let (unknown, _) = refs(&request);
    assert_eq!(
        c.write_provider_resume(
            &unknown,
            refined.clone().unwrap(),
            ProviderResumeWrite::Replace,
            &mut store,
        ),
        Err(RuntimeError::UnknownRuntime)
    );
    let mut mismatched = refined.clone().unwrap();
    mismatched.adapter_revision += 1;
    assert_eq!(
        c.write_provider_resume(
            &runtime,
            mismatched,
            ProviderResumeWrite::Replace,
            &mut store,
        ),
        Err(RuntimeError::ProviderResumeMismatch)
    );
    // The refinement never touches liveness.
    assert_eq!(
        refined.map(|value| value.last_known_status),
        Some(ProviderResumeStatus::Active)
    );

    // A runtime which is no longer live refuses the refinement outright.
    c.exit(&runtime, 0, &mut store).unwrap();
    assert_eq!(
        c.record_provider_phase(&runtime, ProviderResumePhase::Running, &mut store)
            .unwrap_err(),
        RuntimeError::ProviderResumeMismatch
    );
}
#[test]
fn inventory_lists_only_in_scope_agents_and_marks_live_until_exit() {
    use usagi_core::domain::terminal_launch::{TerminalKind, TerminalLaunchScope};

    let request = request();
    let (runtime, fence) = refs(&request);
    let mut c = RuntimeCoordinator::new(2, 1024, 2);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut c,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();

    let scope = TerminalLaunchScope {
        workspace_id: request.scope.workspace_id,
        session_id: request.scope.session_id,
        worktree_id: request.scope.worktree_id,
    };
    let live = c.inventory(&scope);
    assert_eq!(live.len(), 1);
    assert!(live[0].terminal.fences(&runtime.terminal));
    assert_eq!(live[0].kind, TerminalKind::Agent);
    assert!(live[0].live);

    // A foreign session scope sees no agent.
    let foreign = TerminalLaunchScope {
        workspace_id: request.scope.workspace_id,
        session_id: Some(SessionId::new()),
        worktree_id: request.scope.worktree_id,
    };
    assert!(c.inventory(&foreign).is_empty());

    // After the Agent exits it is no longer attachable (`live == false`).
    c.exit(&runtime, 0, &mut store).unwrap();
    let exited = c.inventory(&scope);
    assert_eq!(exited.len(), 1);
    assert!(!exited[0].live);
}

#[derive(Default)]
struct Writer(Vec<u8>);
impl PtyWriter for Writer {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), super::super::terminal::PtyWriteError> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }
}
#[test]
fn public_terminal_stream_attaches_inputs_detaches_reattaches_and_resizes() {
    let request = request();
    let (runtime, fence) = refs(&request);
    let mut c = RuntimeCoordinator::new(1, 1024, 4);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut c,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    assert_eq!(
        c.runtime_for_terminal(&runtime.terminal).unwrap(),
        runtime.clone()
    );
    let mut stale = runtime.terminal.clone();
    stale.terminal_id = TerminalId::new();
    assert_eq!(c.runtime_for_terminal(&stale), None);

    let connection = ConnectionId::new();
    let client = ClientId::new();
    let attached = c.attach(&runtime, connection).unwrap();
    let mut journal = Journal::default();
    c.append_output(&runtime, b"boot\n".to_vec(), &mut journal)
        .unwrap();
    let mut writer = Writer::default();
    assert_eq!(
        c.input(
            &runtime,
            InputRequest {
                subscription: attached.subscription,
                connection,
                client,
                request: RequestId::new(),
                input_seq: 0,
                operation: None,
            },
            b"go\n",
            &mut writer,
        )
        .unwrap(),
        InputAck::Written
    );
    assert_eq!(writer.0, b"go\n");
    c.detach(
        &runtime,
        attached.subscription,
        connection,
        &mut Writer::default(),
    )
    .unwrap();
    let reattached = c.attach(&runtime, connection).unwrap();
    assert_eq!(reattached.snapshot.replay, b"boot\n");
    assert_eq!(c.replay_from(&runtime, 0, None).unwrap()[0].data, b"boot\n");
    let mut resize_writer = Writer::default();
    assert_eq!(
        c.resize(
            &runtime,
            Geometry {
                cols: 120,
                rows: 40
            },
            None,
            &mut resize_writer,
        )
        .unwrap()
        .geometry
        .cols,
        120
    );
    c.disconnect(connection, &mut Writer::default());
    assert!(c.terminal_snapshot(&runtime).is_ok());
}
#[test]
fn ambiguous_spawn_and_unknown_identity_block_replacement() {
    let second_request = request();
    let (runtime, fence) = refs(&second_request);
    let mut c = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = Spawner(Err(SpawnFailure::Ambiguous));
    assert_eq!(
        launch(
            &mut c,
            &second_request,
            runtime.clone(),
            fence,
            &mut spawner,
            &mut store
        ),
        Err(RuntimeError::ReconcileRequired(
            ReconcileState::SpawnAmbiguous
        ))
    );
    assert_eq!(c.occupied_slots(), 1);
    c.reconcile(&runtime, ProcessObservation::Unknown, &mut store)
        .unwrap();
    assert_eq!(c.occupied_slots(), 1);
}
/// The published level is the level admission decides from, at every step of
/// a runtime's life. An observer therefore never has to count records or know
/// the limit constant, and never sees a level the coordinator would refuse to
/// act on.
#[test]
fn the_bound_gauge_tracks_the_level_admission_admits_from() {
    let gauge = AgentConcurrencyGauge::default();
    // Nothing is published before an authority binds it.
    assert_eq!(gauge.observe(), None);

    let mut c = RuntimeCoordinator::new(1, 1024, 2);
    c.bind_concurrency_gauge(gauge.clone());
    // Binding publishes the current level, so an idle pool is reported as an
    // explicit zero rather than as "unknown".
    assert_eq!(
        gauge.observe(),
        Some(AgentConcurrency {
            in_use: 0,
            limit: 1
        })
    );
    assert_eq!(c.concurrency().limit, 1);

    let first = request();
    let (runtime, fence) = refs(&first);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut c,
        &first,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    assert_eq!(gauge.observe(), Some(c.concurrency()));
    assert_eq!(
        gauge.observe(),
        Some(AgentConcurrency {
            in_use: 1,
            limit: 1
        })
    );
    // At the limit the next launch is refused, and the published level says so
    // before the refusal happens.
    assert!(gauge.observe().unwrap().is_saturated());
    let second = request();
    let (blocked, blocked_fence) = refs(&second);
    assert_eq!(
        launch(
            &mut c,
            &second,
            blocked,
            blocked_fence,
            &mut spawner,
            &mut store
        ),
        Err(RuntimeError::ConcurrencyExhausted)
    );
    // A refusal is effect free, including on the published level.
    assert_eq!(
        gauge.observe(),
        Some(AgentConcurrency {
            in_use: 1,
            limit: 1
        })
    );

    // An exit releases the slot, and the observer sees the release.
    c.exit(&runtime, 0, &mut store).unwrap();
    assert_eq!(
        gauge.observe(),
        Some(AgentConcurrency {
            in_use: 0,
            limit: 1
        })
    );
    assert!(!gauge.observe().unwrap().is_saturated());
}

/// A reservation whose durable write failed is kept in memory on purpose, so
/// the published level must follow the records — not the write. Otherwise an
/// observer would report room in a pool that refuses the next launch.
#[test]
fn a_failed_persist_publishes_the_reservation_it_kept() {
    let gauge = AgentConcurrencyGauge::default();
    let mut c = RuntimeCoordinator::new(1, 1024, 2);
    c.bind_concurrency_gauge(gauge.clone());
    let request = request();
    let (runtime, fence) = refs(&request);
    let mut store = ConditionalStore {
        saves: 0,
        fail_after: Some(0),
    };
    let mut spawner = Spawner(Ok(process()));
    assert_eq!(
        launch(&mut c, &request, runtime, fence, &mut spawner, &mut store),
        Err(RuntimeError::Store)
    );
    assert_eq!(c.occupied_slots(), 1);
    assert_eq!(
        gauge.observe(),
        Some(AgentConcurrency {
            in_use: 1,
            limit: 1
        })
    );
}

/// A definite spawn failure means no child exists, so the slot is free again
/// and the observer must see that without waiting for another mutation.
#[test]
fn a_definite_spawn_failure_publishes_the_released_slot() {
    let gauge = AgentConcurrencyGauge::default();
    let mut c = RuntimeCoordinator::new(2, 1024, 2);
    c.bind_concurrency_gauge(gauge.clone());
    let request = request();
    let (runtime, fence) = refs(&request);
    let mut store = Store::default();
    let mut spawner = Spawner(Err(SpawnFailure::Definite));
    assert_eq!(
        launch(&mut c, &request, runtime, fence, &mut spawner, &mut store),
        Err(RuntimeError::SpawnFailed)
    );
    assert_eq!(
        gauge.observe(),
        Some(AgentConcurrency {
            in_use: 0,
            limit: 2
        })
    );
}

#[test]
fn verified_exit_or_disappearance_releases_slot() {
    let first_request = request();
    let (runtime, fence) = refs(&first_request);
    let mut c = RuntimeCoordinator::new(1, 1024, 2);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut c,
        &first_request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    c.exit(&runtime, 0, &mut store).unwrap();
    assert_eq!(c.occupied_slots(), 0);
    let second_request = request();
    let (runtime, fence) = refs(&second_request);
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut c,
        &second_request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    c.reconcile(&runtime, ProcessObservation::Gone, &mut store)
        .unwrap();
    assert_eq!(c.occupied_slots(), 0);
}

#[test]
fn runtime_failures_remain_typed_and_fail_closed() {
    let initial_request = request();
    let (runtime, fence) = refs(&initial_request);
    let mut c = RuntimeCoordinator::new(1, 64, 1);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut c,
        &initial_request,
        runtime.clone(),
        fence.clone(),
        &mut spawner,
        &mut store,
    )
    .unwrap();
    assert_eq!(
        launch(
            &mut c,
            &initial_request,
            runtime.clone(),
            fence.clone(),
            &mut spawner,
            &mut store
        ),
        Err(RuntimeError::RuntimeAlreadyExists)
    );
    let other_request = request();
    let (other_runtime, other_fence) = refs(&other_request);
    assert_eq!(
        launch(
            &mut c,
            &other_request,
            other_runtime,
            other_fence,
            &mut spawner,
            &mut store
        ),
        Err(RuntimeError::ConcurrencyExhausted)
    );
    assert_eq!(
        c.terminal_snapshot(&runtime).unwrap().terminal,
        runtime.terminal
    );
    // Liveness is readable without capturing a screen.
    assert_eq!(c.terminal_exit_status(&runtime), Ok(None));
    let mut stale = runtime.clone();
    stale.terminal.daemon_generation = DaemonGeneration::new();
    assert_eq!(
        c.terminal_snapshot(&stale),
        Err(RuntimeError::UnknownRuntime)
    );
    assert_eq!(
        c.terminal_exit_status(&stale),
        Err(RuntimeError::UnknownRuntime)
    );
    assert_eq!(
        c.reconcile(&stale, ProcessObservation::Gone, &mut store),
        Err(RuntimeError::Generation(
            GenerationError::TerminalOwnedElsewhere
        ))
    );
    assert_eq!(
        c.attach(&stale, ConnectionId::new()),
        Err(RuntimeError::UnknownRuntime)
    );
    assert_eq!(
        c.detach(&stale, 1, ConnectionId::new(), &mut Writer::default()),
        Err(RuntimeError::UnknownRuntime)
    );
    assert_eq!(
        c.replay_from(&stale, 0, None),
        Err(RuntimeError::UnknownRuntime)
    );
    assert_eq!(
        c.input(
            &stale,
            InputRequest {
                subscription: 1,
                connection: ConnectionId::new(),
                client: ClientId::new(),
                request: RequestId::new(),
                input_seq: 0,
                operation: None,
            },
            b"ignored",
            &mut Writer::default(),
        ),
        Err(RuntimeError::UnknownRuntime)
    );
    c.reconcile(
        &runtime,
        ProcessObservation::VerifiedAlive(process()),
        &mut store,
    )
    .unwrap();
    assert_eq!(
        c.snapshot().records[0].state,
        RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning)
    );
}

#[test]
#[allow(clippy::too_many_lines)] // The failpoint matrix shares setup and asserts each retained state in order.
fn spawn_and_persistence_uncertainty_are_retained_for_reconcile() {
    let failed_request = request();
    let (runtime, fence) = refs(&failed_request);
    let mut c = RuntimeCoordinator::new(2, 64, 1);
    let mut store = Store::default();
    let mut definite = Spawner(Err(SpawnFailure::Definite));
    assert_eq!(
        launch(
            &mut c,
            &failed_request,
            runtime,
            fence,
            &mut definite,
            &mut store
        ),
        Err(RuntimeError::SpawnFailed)
    );

    for failure in [SpawnFailure::Definite, SpawnFailure::Ambiguous] {
        let successful_request = request();
        let (runtime, fence) = refs(&successful_request);
        let mut coordinator = RuntimeCoordinator::new(1, 64, 1);
        let mut successful_store = ConditionalStore {
            saves: 0,
            fail_after: None,
        };
        assert!(
            launch(
                &mut coordinator,
                &successful_request,
                runtime,
                fence,
                &mut Spawner(Err(failure)),
                &mut successful_store,
            )
            .is_err()
        );

        let failing_request = request();
        let (runtime, fence) = refs(&failing_request);
        let mut coordinator = RuntimeCoordinator::new(1, 64, 1);
        let mut failing_store = ConditionalStore {
            saves: 0,
            fail_after: Some(1),
        };
        assert_eq!(
            launch(
                &mut coordinator,
                &failing_request,
                runtime,
                fence,
                &mut Spawner(Err(failure)),
                &mut failing_store,
            ),
            Err(RuntimeError::Store)
        );
    }

    let persisted_request = request();
    let (runtime, fence) = refs(&persisted_request);
    let mut store = FailingStore(0);
    let mut spawner = Spawner(Ok(process()));
    assert_eq!(
        launch(
            &mut c,
            &persisted_request,
            runtime.clone(),
            fence,
            &mut spawner,
            &mut store
        ),
        Err(RuntimeError::ReconcileRequired(
            ReconcileState::OrphanRunning
        ))
    );
    assert_eq!(c.occupied_slots(), 1);

    let compensated_request = request();
    let (compensated_runtime, compensated_fence) = refs(&compensated_request);
    let mut compensated = RuntimeCoordinator::new(1, 64, 1);
    let mut one_shot_failure = FailingStore(0);
    let mut terminating = CompensatingSpawner { terminated: false };
    assert_eq!(
        launch(
            &mut compensated,
            &compensated_request,
            compensated_runtime,
            compensated_fence,
            &mut terminating,
            &mut one_shot_failure,
        ),
        Err(RuntimeError::SpawnFailed)
    );
    assert!(terminating.terminated);
    assert_eq!(compensated.occupied_slots(), 0);
    assert_eq!(
        compensated.snapshot().records[0].state,
        RuntimeState::SpawnFailed
    );

    for terminate_succeeds in [true, false] {
        let request = request();
        let (runtime, fence) = refs(&request);
        let mut coordinator = RuntimeCoordinator::new(1, 64, 1);
        let mut store = ConditionalStore {
            saves: 0,
            fail_after: Some(1),
        };
        let error = if terminate_succeeds {
            let mut spawner = CompensatingSpawner { terminated: false };
            launch(
                &mut coordinator,
                &request,
                runtime,
                fence,
                &mut spawner,
                &mut store,
            )
        } else {
            launch(
                &mut coordinator,
                &request,
                runtime,
                fence,
                &mut Spawner(Ok(process())),
                &mut store,
            )
        };
        assert_eq!(
            error,
            Err(RuntimeError::ReconcileRequired(if terminate_succeeds {
                ReconcileState::PersistAfterSpawn
            } else {
                ReconcileState::OrphanRunning
            }))
        );
    }

    let request = request();
    let (runtime, fence) = refs(&request);
    let mut exit_coordinator = RuntimeCoordinator::new(1, 64, 1);
    let mut normal_store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut exit_coordinator,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut normal_store,
    )
    .unwrap();
    let mut exit_store = FailingStore(1);
    assert_eq!(
        exit_coordinator.exit(&runtime, 0, &mut exit_store),
        Err(RuntimeError::ReconcileRequired(
            ReconcileState::PersistAfterExit
        ))
    );
}

#[test]
fn invalid_resolver_provenance_and_duplicate_terminal_reservation_are_rejected() {
    struct BadResolver;
    impl AgentAdapter for BadResolver {
        fn resolve(&mut self, request: &LaunchRequest) -> Result<ResolvedLaunch, AdapterError> {
            let mut resolved = Resolver::default()
                .resolve(request)
                .expect("test resolver accepts the canonical request");
            resolved.snapshot.request.resume = true;
            Ok(resolved)
        }
    }
    let request = request();
    let (runtime, fence) = refs(&request);
    let mut c = RuntimeCoordinator::new(2, 64, 1);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    assert_eq!(
        c.launch(
            &request,
            runtime.clone(),
            fence.clone(),
            Geometry { cols: 80, rows: 24 },
            &mut BadResolver,
            &mut store,
            &mut spawner,
            None
        ),
        Err(RuntimeError::ScopeMismatch)
    );
    launch(
        &mut c,
        &request,
        runtime.clone(),
        fence.clone(),
        &mut spawner,
        &mut store,
    )
    .unwrap();
    let duplicate = AgentRuntimeRef::new(
        AgentRuntimeId::new(),
        runtime.terminal.clone(),
        runtime.session_id,
    )
    .unwrap();
    assert_eq!(
        launch(&mut c, &request, duplicate, fence, &mut spawner, &mut store),
        Err(RuntimeError::Terminal(RegistryError::StaleTarget))
    );

    let pre_registered_request = request.clone();
    let (pre_registered_runtime, pre_registered_fence) = refs(&pre_registered_request);
    let mut pre_registered = RuntimeCoordinator::new(2, 64, 1);
    pre_registered
        .terminals
        .register(
            pre_registered_runtime.terminal.clone(),
            Geometry { cols: 80, rows: 24 },
        )
        .unwrap();
    assert_eq!(
        launch(
            &mut pre_registered,
            &pre_registered_request,
            pre_registered_runtime,
            pre_registered_fence,
            &mut spawner,
            &mut store,
        ),
        Err(RuntimeError::Terminal(RegistryError::StaleTarget))
    );
}

#[test]
fn pre_spawn_and_output_failures_do_not_create_a_replacement_path() {
    struct RejectingResolver;
    impl AgentAdapter for RejectingResolver {
        fn resolve(&mut self, _: &LaunchRequest) -> Result<ResolvedLaunch, AdapterError> {
            Err(AdapterError::Validation(
                LaunchValidationError::InvalidProgram,
            ))
        }
    }
    struct RejectingStore;
    impl RuntimeStore for RejectingStore {
        fn save(&mut self, _: RuntimeStoreSnapshot) -> Result<(), ()> {
            Err(())
        }
    }
    struct RejectingJournal;
    impl OutputJournal for RejectingJournal {
        fn append(&mut self, _: &Output) -> Result<(), ()> {
            Err(())
        }
    }

    let first_request = request();
    let (runtime, mut fence) = refs(&first_request);
    let valid_fence = fence.clone();
    let mut coordinator = RuntimeCoordinator::new(2, 64, 1);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    fence.owner_daemon_generation = DaemonGeneration::new();
    assert_eq!(
        coordinator.launch(
            &first_request,
            runtime.clone(),
            fence,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver::default(),
            &mut store,
            &mut spawner,
            None
        ),
        Err(RuntimeError::ScopeMismatch)
    );
    assert_eq!(
        coordinator.launch(
            &first_request,
            runtime.clone(),
            valid_fence,
            Geometry { cols: 80, rows: 24 },
            &mut RejectingResolver,
            &mut store,
            &mut spawner,
            None
        ),
        Err(RuntimeError::Adapter(AdapterError::Validation(
            LaunchValidationError::InvalidProgram
        )))
    );
    let (runtime, fence) = refs(&first_request);
    assert_eq!(
        coordinator.launch(
            &first_request,
            runtime,
            fence,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver::default(),
            &mut RejectingStore,
            &mut spawner,
            None
        ),
        Err(RuntimeError::Store)
    );

    let request = request();
    let (runtime, fence) = refs(&request);
    launch(
        &mut coordinator,
        &request,
        runtime.clone(),
        fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    assert_eq!(
        coordinator.append_output(&runtime, b"x".to_vec(), &mut RejectingJournal),
        Err(RuntimeError::Journal)
    );
    coordinator
        .reconcile(&runtime, ProcessObservation::Unknown, &mut store)
        .unwrap();
    assert_eq!(
        coordinator.append_output(&runtime, b"x".to_vec(), &mut Journal::default()),
        Err(RuntimeError::ReconcileRequired(
            ReconcileState::IdentityUnknown
        ))
    );
}

/// Launches, journals one output chunk, and exits one Agent runtime.
fn run_agent(
    coordinator: &mut RuntimeCoordinator,
    store: &mut dyn RuntimeStore,
    bytes: &[u8],
) -> AgentRuntimeRef {
    let request = request();
    let (runtime, operation) = refs(&request);
    coordinator
        .launch(
            &request,
            runtime.clone(),
            operation,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver { calls: 0 },
            store,
            &mut Spawner(Ok(process())),
            None,
        )
        .expect("the fixture admits this launch");
    coordinator
        .append_output(&runtime, bytes.to_vec(), &mut Journal::default())
        .unwrap();
    coordinator.exit(&runtime, 0, store).unwrap();
    runtime
}

#[test]
fn an_agent_launch_reserves_its_final_and_a_failed_spawn_returns_the_capacity() {
    let (retention, _clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
    let mut coordinator = RuntimeCoordinator::with_retention(8, 64, 1, retention.clone());
    let request = request();
    let (runtime, operation) = refs(&request);
    assert_eq!(
        coordinator.launch(
            &request,
            runtime,
            operation,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver { calls: 0 },
            &mut Store::default(),
            &mut Spawner(Err(SpawnFailure::Definite)),
            None,
        ),
        Err(RuntimeError::SpawnFailed)
    );
    assert_eq!(retention.metrics().reserved_finals, 0);

    let mut store = Store::default();
    let runtime = run_agent(&mut coordinator, &mut store, b"agent final");
    let metrics = retention.metrics();
    assert_eq!(metrics.retained_finals, 1);
    assert_eq!(metrics.retained_bytes, 11);
    assert_eq!(metrics.reserved_finals, 0);
    assert!(retention.lookup(&runtime.terminal).retained().is_some());
}

#[test]
fn an_exhausted_retention_budget_refuses_agent_admission_before_spawn() {
    let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
    let mut coordinator = RuntimeCoordinator::with_retention(8, 64, 1, retention.clone());
    let mut store = Store::default();
    for _ in 0..3 {
        run_agent(&mut coordinator, &mut store, b"x");
    }
    clock.advance(1);
    let request = request();
    let (runtime, operation) = refs(&request);
    let mut spawner = Spawner(Ok(process()));
    let rejected = coordinator.launch(
        &request,
        runtime,
        operation,
        Geometry { cols: 80, rows: 24 },
        &mut Resolver { calls: 0 },
        &mut store,
        &mut spawner,
        None,
    );
    assert!(matches!(rejected, Err(RuntimeError::RetentionExhausted(_))));
    // No protected final was deleted to make room.
    assert_eq!(retention.metrics().retained_finals, 3);
    assert_eq!(retention.metrics().evicted_finals, 0);
}

#[test]
fn a_collected_agent_final_leaves_no_record_and_answers_typed() {
    let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
    let mut coordinator = RuntimeCoordinator::with_retention(8, 64, 1, retention.clone());
    let mut store = Store::default();
    let runtime = run_agent(&mut coordinator, &mut store, b"bye");
    clock.advance(1000);
    retention.collect();
    assert_eq!(coordinator.collect_garbage(&mut store), 1);
    assert!(coordinator.snapshot().records.is_empty());
    let scope = usagi_core::domain::terminal_launch::TerminalLaunchScope {
        workspace_id: runtime.terminal.workspace_id,
        session_id: runtime.terminal.session_id,
        worktree_id: runtime.terminal.worktree_id,
    };
    assert!(coordinator.completed_inventory(&scope).is_empty());
    assert_eq!(
        coordinator.terminal_snapshot(&runtime),
        Err(RuntimeError::FinalEvicted(
            usagi_core::domain::terminal_retention::EvictionReason::AgeExpired
        ))
    );
    // A runtime the authority never held stays unknown.
    let (stranger, _) = refs(&request());
    assert_eq!(
        coordinator.terminal_snapshot(&stranger),
        Err(RuntimeError::UnknownRuntime)
    );
    assert_eq!(coordinator.collect_garbage(&mut store), 0);
    assert_eq!(coordinator.retention().metrics().retained_finals, 0);
}

#[test]
fn resume_does_not_resurrect_a_source_final_already_marked_evicted() {
    let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
    let mut coordinator = RuntimeCoordinator::with_retention(8, 64, 1, retention.clone());
    let request = request();
    let (source, source_fence) = refs(&request);
    let mut store = Store::default();
    let mut spawner = Spawner(Ok(process()));
    launch(
        &mut coordinator,
        &request,
        source.clone(),
        source_fence,
        &mut spawner,
        &mut store,
    )
    .unwrap();
    coordinator.exit(&source, 0, &mut store).unwrap();
    clock.advance(1000);
    retention.collect();
    assert!(matches!(
        retention.lookup(&source.terminal),
        FinalLookup::Evicted(_)
    ));

    let (replacement, replacement_fence) = refs(&request);
    coordinator
        .resume_with_semantic(
            &request,
            replacement,
            replacement_fence,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver::default(),
            &mut store,
            &mut spawner,
            None,
            "resume-evicted".into(),
            std::slice::from_ref(&source),
        )
        .unwrap();

    assert!(matches!(
        retention.lookup(&source.terminal),
        FinalLookup::Evicted(_)
    ));
}

#[test]
fn an_agent_final_a_client_is_draining_is_protected_until_it_detaches() {
    let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
    let mut coordinator = RuntimeCoordinator::with_retention(8, 64, 1, retention.clone());
    let mut store = Store::default();
    let request = request();
    let (runtime, operation) = refs(&request);
    coordinator
        .launch(
            &request,
            runtime.clone(),
            operation,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver { calls: 0 },
            &mut store,
            &mut Spawner(Ok(process())),
            None,
        )
        .unwrap();
    let connection = ConnectionId::new();
    let attached = coordinator.attach(&runtime, connection).unwrap();
    coordinator.exit(&runtime, 0, &mut store).unwrap();
    clock.advance(1000);
    retention.collect();
    assert_eq!(coordinator.collect_garbage(&mut store), 0);

    coordinator
        .detach(
            &runtime,
            attached.subscription,
            connection,
            &mut Writer::default(),
        )
        .unwrap();
    retention.collect();
    assert_eq!(coordinator.collect_garbage(&mut store), 1);
}

#[test]
fn a_live_connection_sweep_releases_every_stale_agent_final() {
    let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
    let mut coordinator = RuntimeCoordinator::with_retention(8, 64, 1, retention.clone());
    let mut store = Store::default();
    let request = request();
    let (runtime, operation) = refs(&request);
    coordinator
        .launch(
            &request,
            runtime.clone(),
            operation,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver { calls: 0 },
            &mut store,
            &mut Spawner(Ok(process())),
            None,
        )
        .unwrap();
    let connection = ConnectionId::new();
    coordinator.attach(&runtime, connection).unwrap();
    coordinator.exit(&runtime, 0, &mut store).unwrap();
    clock.advance(1000);
    coordinator.retain_live_connections(&BTreeSet::new(), &mut Writer::default());
    // The exact path remains idempotent after a coalesced sweep.
    coordinator.disconnect(connection, &mut Writer::default());
    retention.collect();
    assert_eq!(coordinator.collect_garbage(&mut store), 1);
}

#[test]
fn a_restart_reimports_exited_agent_finals_into_the_budget() {
    let (retention, _clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
    let mut coordinator = RuntimeCoordinator::with_retention(8, 64, 1, retention.clone());
    let mut store = Store::default();
    let runtime = run_agent(&mut coordinator, &mut store, b"gone");
    let snapshot = coordinator.snapshot();
    drop(coordinator);

    let (restored, restart_clock) =
        crate::usecase::terminal_retention_ipc::tests::manual_retention();
    let mut restarted =
        RuntimeCoordinator::hydrate_with_retention(snapshot, 8, 64, 1, restored.clone()).unwrap();
    let metrics = restored.metrics();
    assert_eq!(metrics.retained_finals, 1);
    assert_eq!(metrics.reserved_finals, 0);
    assert_eq!(
        metrics.retained_bytes,
        crate::usecase::terminal_retention_ipc::RESTORED_FINAL_BYTES
    );
    restart_clock.advance(1000);
    restored.collect();
    let mut store = Store::default();
    assert_eq!(restarted.collect_garbage(&mut store), 1);
    assert!(restored.lookup(&runtime.terminal).marker().is_some());
}

#[test]
fn a_reclaimed_resume_source_ages_out_without_leaving_generation_ownership() {
    let (retention, _clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
    let mut coordinator = RuntimeCoordinator::with_retention(8, 64, 1, retention);
    let mut store = Store::default();
    let runtime = run_agent(&mut coordinator, &mut store, b"superseded");
    let mut snapshot = coordinator.snapshot();
    snapshot.records[0].state = RuntimeState::Reclaimed;
    snapshot.records[0].superseded_by = Some(AgentRuntimeId::new());
    snapshot.generation.terminals[0].state = terminal_ownership_state(RuntimeState::Reclaimed);

    let (restored, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
    let mut restarted =
        RuntimeCoordinator::hydrate_with_retention(snapshot, 8, 64, 1, restored.clone()).unwrap();
    let imported = restored.lookup(&runtime.terminal).retained().unwrap();
    assert!(imported.superseded);

    clock.advance(1000);
    restored.collect();
    assert_eq!(restarted.collect_garbage(&mut store), 1);
    let collected = restarted.snapshot();
    assert!(collected.records.is_empty());
    assert!(collected.generation.terminals.is_empty());
    collected.validate_ownership().unwrap();
}

#[test]
fn garbage_collection_keeps_a_record_while_generation_ownership_is_live() {
    let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
    let mut coordinator = RuntimeCoordinator::with_retention(8, 64, 1, retention.clone());
    let mut store = Store::default();
    let runtime = run_agent(&mut coordinator, &mut store, b"still-owned");
    coordinator
        .generation
        .record_spawn(&runtime.terminal, process())
        .unwrap();
    clock.advance(1000);
    retention.collect();

    assert_eq!(coordinator.collect_garbage(&mut store), 0);
    assert_eq!(coordinator.snapshot().records.len(), 1);
}
