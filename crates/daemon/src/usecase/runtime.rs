#![coverage(off)]

//! Durable Agent runtime reservation and terminal-stream orchestration.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::unused_self
)] // Generic injected ports make individual error types and launch dependencies part of the contract.

use std::collections::BTreeMap;

use usagi_core::domain::{
    agent::{DurableLaunchSnapshot, LaunchRequest, LaunchValidationError},
    id::{AgentRuntimeRef, CompletionFence, TerminalRef},
};

pub use super::terminal::{
    SpawnFailure, TerminalReconcileState as ReconcileState, TerminalRuntimeState as RuntimeState,
};
use super::{
    generation::{ProcessIdentity, ProcessObservation},
    terminal::{Geometry, Output, RegistryError, Snapshot, TerminalRegistry},
};

/// Durable association; `launch` is never re-resolved during reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableRuntimeRecord {
    pub runtime: AgentRuntimeRef,
    pub operation: CompletionFence,
    pub launch: DurableLaunchSnapshot,
    pub state: RuntimeState,
    pub process: Option<ProcessIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeStoreSnapshot {
    pub records: Vec<DurableRuntimeRecord>,
}

pub trait RuntimeStore {
    type Error;
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), Self::Error>;
}
/// Called exactly once by [`RuntimeCoordinator::launch`], before PTY spawn.
/// Ephemeral, adapter-owned spawn inputs. This value is never copied into a
/// [`DurableLaunchSnapshot`] or a runtime record.
pub struct SpawnProvision {
    environment: BTreeMap<usagi_core::domain::agent::EnvironmentVariableName, String>,
    arguments: Vec<String>,
}

impl SpawnProvision {
    #[must_use]
    pub fn new(
        environment: impl IntoIterator<
            Item = (usagi_core::domain::agent::EnvironmentVariableName, String),
        >,
        arguments: Vec<String>,
    ) -> Self {
        Self {
            environment: environment.into_iter().collect(),
            arguments,
        }
    }

    #[must_use]
    pub fn environment(
        &self,
    ) -> &BTreeMap<usagi_core::domain::agent::EnvironmentVariableName, String> {
        &self.environment
    }

    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    Validation(LaunchValidationError),
    ExecutableUnavailable,
    ProvisionFailed,
}

/// Product adapter boundary. It validates/renders a durable snapshot and
/// materializes the non-durable spawn inputs exactly once before reservation.
pub trait AgentAdapter {
    fn resolve(&mut self, request: &LaunchRequest) -> Result<ResolvedLaunch, AdapterError>;
}

pub struct ResolvedLaunch {
    pub snapshot: DurableLaunchSnapshot,
    pub provision: SpawnProvision,
}
pub trait PtySpawner {
    fn spawn(
        &mut self,
        launch: &DurableLaunchSnapshot,
        provision: &SpawnProvision,
        terminal: &TerminalRef,
    ) -> Result<ProcessIdentity, SpawnFailure>;
}
pub trait OutputJournal {
    type Error;
    fn append(&mut self, output: &Output) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    Adapter(AdapterError),
    RuntimeAlreadyExists,
    ScopeMismatch,
    ConcurrencyExhausted,
    Terminal(RegistryError),
    Store,
    Journal,
    SpawnFailed,
    ReconcileRequired(ReconcileState),
    UnknownRuntime,
    TerminalGenerationMismatch,
}

/// The daemon owns this coordinator. Callers persist each mutation as one
/// snapshot and must reconcile, rather than replace, unknown external effects.
#[derive(Debug)]
pub struct RuntimeCoordinator {
    limit: usize,
    records: BTreeMap<String, DurableRuntimeRecord>,
    terminals: TerminalRegistry,
}

impl RuntimeCoordinator {
    #[must_use]
    pub fn new(limit: usize, journal_limit: usize, input_cache_limit: usize) -> Self {
        Self {
            limit,
            records: BTreeMap::new(),
            terminals: TerminalRegistry::new(journal_limit, input_cache_limit),
        }
    }

    pub fn launch<A: AgentAdapter, S: RuntimeStore, P: PtySpawner>(
        &mut self,
        request: &LaunchRequest,
        runtime: AgentRuntimeRef,
        operation: CompletionFence,
        geometry: Geometry,
        adapter: &mut A,
        store: &mut S,
        spawner: &mut P,
    ) -> Result<(), RuntimeError> {
        self.validate_scope(&runtime, &operation)?;
        let key = runtime.agent_runtime_id.as_str();
        if self.records.contains_key(&key) {
            return Err(RuntimeError::RuntimeAlreadyExists);
        }
        if self.occupied_slots() >= self.limit {
            return Err(RuntimeError::ConcurrencyExhausted);
        }
        let resolved = adapter.resolve(request).map_err(RuntimeError::Adapter)?;
        let launch = resolved.snapshot;
        if launch.request != *request
            || launch.plan.profile_id != request.profile_id
            || launch.plan.profile_revision == 0
        {
            return Err(RuntimeError::ScopeMismatch);
        }
        self.records.insert(
            key.clone(),
            DurableRuntimeRecord {
                runtime: runtime.clone(),
                operation,
                launch,
                state: RuntimeState::Reserved,
                process: None,
            },
        );
        self.persist(store)?; // durable reservation/snapshot precedes every external effect
        if let Err(error) = self.terminals.register(runtime.terminal.clone(), geometry) {
            // The store already contains a reservation. Keep it in memory too:
            // removing it would make a later actor believe a replacement is safe.
            return Err(RuntimeError::Terminal(error));
        }
        match spawner.spawn(
            &self.records[&key].launch,
            &resolved.provision,
            &runtime.terminal,
        ) {
            Ok(process) => {
                let record = self.records.get_mut(&key).expect("inserted");
                record.process = Some(process);
                record.state = RuntimeState::Running;
                if self.persist(store).is_err() {
                    self.records.get_mut(&key).expect("inserted").state =
                        RuntimeState::ReconcileRequired(ReconcileState::PersistAfterSpawn);
                    return Err(RuntimeError::ReconcileRequired(
                        ReconcileState::PersistAfterSpawn,
                    ));
                }
                Ok(())
            }
            Err(SpawnFailure::Definite) => {
                self.records.get_mut(&key).expect("inserted").state = RuntimeState::SpawnFailed;
                self.persist(store)?;
                Err(RuntimeError::SpawnFailed)
            }
            Err(SpawnFailure::Ambiguous) => {
                self.records.get_mut(&key).expect("inserted").state =
                    RuntimeState::ReconcileRequired(ReconcileState::SpawnAmbiguous);
                self.persist(store)?;
                Err(RuntimeError::ReconcileRequired(
                    ReconcileState::SpawnAmbiguous,
                ))
            }
        }
    }

    /// Journal output before it becomes available to terminal replay clients.
    pub fn append_output<J: OutputJournal>(
        &mut self,
        runtime: &AgentRuntimeRef,
        data: Vec<u8>,
        journal: &mut J,
    ) -> Result<Output, RuntimeError> {
        self.running(runtime)?;
        let start_offset = self
            .terminals
            .snapshot(&runtime.terminal)
            .map_err(RuntimeError::Terminal)?
            .output_offset;
        let output = Output {
            terminal: runtime.terminal.clone(),
            start_offset,
            end_offset: start_offset + data.len() as u64,
            data,
        };
        journal.append(&output).map_err(|_| RuntimeError::Journal)?;
        self.terminals
            .append_output(&runtime.terminal, output.data.clone())
            .map_err(RuntimeError::Terminal)
    }

    /// Caller drains all output before this verified exit is committed.
    pub fn exit<S: RuntimeStore>(
        &mut self,
        runtime: &AgentRuntimeRef,
        status: i32,
        store: &mut S,
    ) -> Result<(), RuntimeError> {
        self.running(runtime)?;
        self.terminals
            .exited(&runtime.terminal, status)
            .map_err(RuntimeError::Terminal)?;
        self.record_mut(runtime)?.state = RuntimeState::Exited;
        if self.persist(store).is_err() {
            self.record_mut(runtime)?.state =
                RuntimeState::ReconcileRequired(ReconcileState::PersistAfterExit);
            return Err(RuntimeError::ReconcileRequired(
                ReconcileState::PersistAfterExit,
            ));
        }
        Ok(())
    }

    /// Reconciliation performs no replacement spawn. A slot is released only
    /// on a verified disappearance (or [`Self::exit`]).
    pub fn reconcile<S: RuntimeStore>(
        &mut self,
        runtime: &AgentRuntimeRef,
        observation: ProcessObservation,
        store: &mut S,
    ) -> Result<(), RuntimeError> {
        let record = self.record_mut(runtime)?;
        record.state = match observation {
            ProcessObservation::Gone => RuntimeState::Reclaimed,
            ProcessObservation::VerifiedAlive(actual)
                if record.process.as_ref() == Some(&actual) =>
            {
                RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning)
            }
            _ => RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown),
        };
        self.persist(store)
    }

    pub fn terminal_snapshot(&self, runtime: &AgentRuntimeRef) -> Result<Snapshot, RuntimeError> {
        self.record(runtime)?;
        self.terminals
            .snapshot(&runtime.terminal)
            .map_err(|_| RuntimeError::TerminalGenerationMismatch)
    }
    #[must_use]
    pub fn snapshot(&self) -> RuntimeStoreSnapshot {
        RuntimeStoreSnapshot {
            records: self.records.values().cloned().collect(),
        }
    }
    #[must_use]
    pub fn occupied_slots(&self) -> usize {
        self.records
            .values()
            .filter(|record| {
                matches!(
                    record.state,
                    RuntimeState::Reserved
                        | RuntimeState::Running
                        | RuntimeState::ReconcileRequired(_)
                )
            })
            .count()
    }
    fn persist<S: RuntimeStore>(&self, store: &mut S) -> Result<(), RuntimeError> {
        store.save(self.snapshot()).map_err(|_| RuntimeError::Store)
    }
    fn validate_scope(
        &self,
        runtime: &AgentRuntimeRef,
        operation: &CompletionFence,
    ) -> Result<(), RuntimeError> {
        (runtime.terminal.session_id == Some(runtime.session_id)
            && runtime.session_id == operation.session_id
            && runtime.terminal.workspace_id == operation.workspace_id
            && runtime.terminal.daemon_generation == operation.owner_daemon_generation)
            .then_some(())
            .ok_or(RuntimeError::ScopeMismatch)
    }
    fn record(&self, runtime: &AgentRuntimeRef) -> Result<&DurableRuntimeRecord, RuntimeError> {
        self.records
            .get(&runtime.agent_runtime_id.as_str())
            .filter(|record| record.runtime.fences(runtime))
            .ok_or(RuntimeError::UnknownRuntime)
    }
    fn record_mut(
        &mut self,
        runtime: &AgentRuntimeRef,
    ) -> Result<&mut DurableRuntimeRecord, RuntimeError> {
        self.records
            .get_mut(&runtime.agent_runtime_id.as_str())
            .filter(|record| record.runtime.fences(runtime))
            .ok_or(RuntimeError::UnknownRuntime)
    }
    fn running(&self, runtime: &AgentRuntimeRef) -> Result<(), RuntimeError> {
        (self.record(runtime)?.state == RuntimeState::Running)
            .then_some(())
            .ok_or(RuntimeError::ReconcileRequired(
                ReconcileState::IdentityUnknown,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, path::PathBuf};
    use usagi_core::domain::{
        agent::{AgentProfileId, LaunchMode, LaunchPlan, LaunchScope},
        id::{
            AgentRuntimeId, DaemonGeneration, OperationId, SessionId, TerminalId, WorkspaceId,
            WorktreeId,
        },
    };
    #[derive(Default)]
    struct Store(Vec<RuntimeStoreSnapshot>);
    impl RuntimeStore for Store {
        type Error = ();
        fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), ()> {
            self.0.push(snapshot);
            Ok(())
        }
    }
    struct FailingStore(usize);
    impl RuntimeStore for FailingStore {
        type Error = ();
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
            Ok(ResolvedLaunch {
                snapshot: DurableLaunchSnapshot::new(
                    request.clone(),
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
    #[derive(Default)]
    struct Journal(Vec<Output>);
    impl OutputJournal for Journal {
        type Error = ();
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
            initial_prompt: None,
            scope: LaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: SessionId::new(),
                worktree_id: WorktreeId::new(),
            },
            required_capabilities: BTreeSet::new(),
        }
    }
    fn refs(request: &LaunchRequest) -> (AgentRuntimeRef, CompletionFence) {
        let generation = DaemonGeneration::new();
        let terminal = TerminalRef {
            daemon_generation: generation,
            terminal_id: TerminalId::new(),
            workspace_id: request.scope.workspace_id,
            session_id: Some(request.scope.session_id),
            worktree_id: request.scope.worktree_id,
        };
        let runtime =
            AgentRuntimeRef::new(AgentRuntimeId::new(), terminal, request.scope.session_id)
                .unwrap();
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
    fn launch<S: RuntimeStore>(
        coordinator: &mut RuntimeCoordinator,
        request: &LaunchRequest,
        runtime: AgentRuntimeRef,
        fence: CompletionFence,
        spawner: &mut Spawner,
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
        )
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
        c.terminals.disconnect(connection);
        assert_eq!(attached.snapshot.replay, b"hello");
        assert_eq!(c.occupied_slots(), 1);
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
        let mut stale = runtime.clone();
        stale.terminal.daemon_generation = DaemonGeneration::new();
        assert_eq!(
            c.terminal_snapshot(&stale),
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
                ReconcileState::PersistAfterSpawn
            ))
        );
        assert_eq!(c.occupied_slots(), 1);

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
                let mut resolved = Resolver::default().resolve(request)?;
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
                &mut spawner
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
            type Error = ();
            fn save(&mut self, _: RuntimeStoreSnapshot) -> Result<(), ()> {
                Err(())
            }
        }
        struct RejectingJournal;
        impl OutputJournal for RejectingJournal {
            type Error = ();
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
                &mut spawner
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
                &mut spawner
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
                &mut spawner
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
}
