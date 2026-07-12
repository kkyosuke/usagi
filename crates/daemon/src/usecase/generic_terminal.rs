//! Daemon-owned, terminal-only launch orchestration.
//!
//! The IPC-facing request selects only a trusted profile. This coordinator
//! never accepts a shell command, argv, or client-provided environment.

#![allow(
    clippy::implicit_clone,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::unused_self
)] // Injected daemon ports make these boundary signatures part of the contract.

use std::collections::BTreeMap;

use usagi_core::domain::{
    id::{CompletionFence, ConnectionId, TerminalRef},
    terminal_launch::{
        DurableTerminalLaunchSnapshot, ResolvedTerminalLaunch, TerminalLaunchRequest,
        TerminalLaunchValidationError,
    },
};

use super::{
    generation::{ProcessIdentity, ProcessObservation},
    runtime::{ReconcileState, RuntimeState, SpawnFailure},
    terminal::{Geometry, RegistryError, Snapshot, TerminalRegistry},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableTerminalRecord {
    pub terminal: TerminalRef,
    pub operation: CompletionFence,
    pub launch: DurableTerminalLaunchSnapshot,
    pub state: RuntimeState,
    pub process: Option<ProcessIdentity>,
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalStoreSnapshot {
    pub records: Vec<DurableTerminalRecord>,
}
pub trait TerminalStore {
    type Error;
    fn save(&mut self, snapshot: TerminalStoreSnapshot) -> Result<(), Self::Error>;
}
/// Resolves a code-defined profile or trusted local settings once, before spawn.
pub trait TerminalProfileResolver {
    fn resolve(
        &mut self,
        request: &TerminalLaunchRequest,
    ) -> Result<ResolvedTerminalLaunch, TerminalLaunchValidationError>;
}
pub trait GenericPtySpawner {
    fn spawn(
        &mut self,
        launch: &ResolvedTerminalLaunch,
        terminal: &TerminalRef,
    ) -> Result<ProcessIdentity, SpawnFailure>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenericTerminalError {
    Launch(TerminalLaunchValidationError),
    TerminalAlreadyExists,
    ScopeMismatch,
    ConcurrencyExhausted,
    Terminal(RegistryError),
    Store,
    SpawnFailed,
    ReconcileRequired(ReconcileState),
    UnknownTerminal,
    TerminalGenerationMismatch,
}

/// Owns generic shell PTYs. It has no `AgentRuntimeId` or adapter hook path.
#[derive(Debug)]
pub struct GenericTerminalCoordinator {
    limit: usize,
    records: BTreeMap<String, DurableTerminalRecord>,
    terminals: TerminalRegistry,
}
impl GenericTerminalCoordinator {
    #[must_use]
    pub fn new(limit: usize, journal_limit: usize, input_cache_limit: usize) -> Self {
        Self {
            limit,
            records: BTreeMap::new(),
            terminals: TerminalRegistry::new(journal_limit, input_cache_limit),
        }
    }
    pub fn launch<R: TerminalProfileResolver, S: TerminalStore, P: GenericPtySpawner>(
        &mut self,
        request: &TerminalLaunchRequest,
        terminal: TerminalRef,
        operation: CompletionFence,
        geometry: Geometry,
        resolver: &mut R,
        store: &mut S,
        spawner: &mut P,
    ) -> Result<(), GenericTerminalError> {
        self.validate_scope(request, &terminal, &operation)?;
        let key = terminal.terminal_id.as_str();
        if self.records.contains_key(&key) {
            return Err(GenericTerminalError::TerminalAlreadyExists);
        }
        if self.occupied_slots() >= self.limit {
            return Err(GenericTerminalError::ConcurrencyExhausted);
        }
        let resolved = resolver
            .resolve(request)
            .map_err(GenericTerminalError::Launch)?;
        if resolved.snapshot.request != *request
            || resolved.snapshot.schema_version != DurableTerminalLaunchSnapshot::SCHEMA_VERSION
        {
            return Err(GenericTerminalError::ScopeMismatch);
        }
        self.records.insert(
            key.to_owned(),
            DurableTerminalRecord {
                terminal: terminal.clone(),
                operation,
                launch: resolved.snapshot.clone(),
                state: RuntimeState::Reserved,
                process: None,
            },
        );
        self.persist(store)?;
        if let Err(error) = self.terminals.register(terminal.clone(), geometry) {
            return Err(GenericTerminalError::Terminal(error));
        }
        match spawner.spawn(&resolved, &terminal) {
            Ok(process) => {
                let record = self.records.get_mut(&key).expect("reserved record");
                record.process = Some(process);
                record.state = RuntimeState::Running;
                if self.persist(store).is_err() {
                    self.records.get_mut(&key).expect("reserved record").state =
                        RuntimeState::ReconcileRequired(ReconcileState::PersistAfterSpawn);
                    return Err(GenericTerminalError::ReconcileRequired(
                        ReconcileState::PersistAfterSpawn,
                    ));
                }
                Ok(())
            }
            Err(SpawnFailure::Definite) => {
                self.records.get_mut(&key).expect("reserved record").state =
                    RuntimeState::SpawnFailed;
                self.persist(store)?;
                Err(GenericTerminalError::SpawnFailed)
            }
            Err(SpawnFailure::Ambiguous) => {
                self.records.get_mut(&key).expect("reserved record").state =
                    RuntimeState::ReconcileRequired(ReconcileState::SpawnAmbiguous);
                self.persist(store)?;
                Err(GenericTerminalError::ReconcileRequired(
                    ReconcileState::SpawnAmbiguous,
                ))
            }
        }
    }
    /// Detach only removes this connection's subscriptions; the PTY stays alive.
    pub fn disconnect(&mut self, connection: ConnectionId) {
        self.terminals.disconnect(connection);
    }
    pub fn terminal_snapshot(
        &self,
        terminal: &TerminalRef,
    ) -> Result<Snapshot, GenericTerminalError> {
        self.record(terminal)?;
        self.terminals
            .snapshot(terminal)
            .map_err(|_| GenericTerminalError::TerminalGenerationMismatch)
    }
    pub fn exit<S: TerminalStore>(
        &mut self,
        terminal: &TerminalRef,
        status: i32,
        store: &mut S,
    ) -> Result<(), GenericTerminalError> {
        self.running(terminal)?;
        self.terminals
            .exited(terminal, status)
            .map_err(GenericTerminalError::Terminal)?;
        self.record_mut(terminal)?.state = RuntimeState::Exited;
        self.persist(store)
    }
    /// Never starts a replacement after an ambiguous outcome.
    pub fn reconcile<S: TerminalStore>(
        &mut self,
        terminal: &TerminalRef,
        observation: ProcessObservation,
        store: &mut S,
    ) -> Result<(), GenericTerminalError> {
        let record = self.record_mut(terminal)?;
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
    #[must_use]
    pub fn snapshot(&self) -> TerminalStoreSnapshot {
        TerminalStoreSnapshot {
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
    fn persist<S: TerminalStore>(&self, store: &mut S) -> Result<(), GenericTerminalError> {
        store
            .save(self.snapshot())
            .map_err(|_| GenericTerminalError::Store)
    }
    fn validate_scope(
        &self,
        request: &TerminalLaunchRequest,
        terminal: &TerminalRef,
        operation: &CompletionFence,
    ) -> Result<(), GenericTerminalError> {
        (request.scope.workspace_id == terminal.workspace_id
            && request.scope.session_id == terminal.session_id
            && request.scope.worktree_id == terminal.worktree_id
            && terminal.workspace_id == operation.workspace_id
            && terminal.session_id == Some(operation.session_id)
            && terminal.daemon_generation == operation.owner_daemon_generation)
            .then_some(())
            .ok_or(GenericTerminalError::ScopeMismatch)
    }
    fn record(
        &self,
        terminal: &TerminalRef,
    ) -> Result<&DurableTerminalRecord, GenericTerminalError> {
        self.records
            .get(&terminal.terminal_id.as_str())
            .filter(|record| record.terminal.fences(terminal))
            .ok_or(GenericTerminalError::UnknownTerminal)
    }
    fn record_mut(
        &mut self,
        terminal: &TerminalRef,
    ) -> Result<&mut DurableTerminalRecord, GenericTerminalError> {
        self.records
            .get_mut(&terminal.terminal_id.as_str())
            .filter(|record| record.terminal.fences(terminal))
            .ok_or(GenericTerminalError::UnknownTerminal)
    }
    fn running(&self, terminal: &TerminalRef) -> Result<(), GenericTerminalError> {
        (self.record(terminal)?.state == RuntimeState::Running)
            .then_some(())
            .ok_or(GenericTerminalError::ReconcileRequired(
                ReconcileState::IdentityUnknown,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, path::PathBuf};
    use usagi_core::domain::{
        agent::EnvironmentVariableName,
        id::{DaemonGeneration, OperationId, SessionId, TerminalId, WorkspaceId, WorktreeId},
        terminal_launch::{TerminalLaunchScope, TerminalProfileId},
    };
    #[derive(Default)]
    struct Store(Vec<TerminalStoreSnapshot>);
    impl TerminalStore for Store {
        type Error = ();
        fn save(&mut self, snapshot: TerminalStoreSnapshot) -> Result<(), ()> {
            self.0.push(snapshot);
            Ok(())
        }
    }
    struct FailingStore;
    impl TerminalStore for FailingStore {
        type Error = ();
        fn save(&mut self, _: TerminalStoreSnapshot) -> Result<(), ()> {
            Err(())
        }
    }
    struct Resolver;
    impl TerminalProfileResolver for Resolver {
        fn resolve(
            &mut self,
            request: &TerminalLaunchRequest,
        ) -> Result<ResolvedTerminalLaunch, TerminalLaunchValidationError> {
            ResolvedTerminalLaunch::new(
                DurableTerminalLaunchSnapshot::new(
                    request.clone(),
                    1,
                    "/bin/sh",
                    PathBuf::from("."),
                    [EnvironmentVariableName::new("TERM").unwrap()],
                )?,
                BTreeMap::from([(
                    EnvironmentVariableName::new("TERM").unwrap(),
                    "xterm-256color".into(),
                )]),
            )
        }
    }
    struct Spawner(Result<ProcessIdentity, SpawnFailure>);
    impl GenericPtySpawner for Spawner {
        fn spawn(
            &mut self,
            _: &ResolvedTerminalLaunch,
            _: &TerminalRef,
        ) -> Result<ProcessIdentity, SpawnFailure> {
            self.0.clone()
        }
    }
    fn request() -> TerminalLaunchRequest {
        TerminalLaunchRequest {
            profile_id: TerminalProfileId::new("login-shell").unwrap(),
            scope: TerminalLaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: Some(SessionId::new()),
                worktree_id: WorktreeId::new(),
            },
        }
    }
    fn refs(request: &TerminalLaunchRequest) -> (TerminalRef, CompletionFence) {
        let generation = DaemonGeneration::new();
        let terminal = TerminalRef {
            daemon_generation: generation,
            terminal_id: TerminalId::new(),
            workspace_id: request.scope.workspace_id,
            session_id: request.scope.session_id,
            worktree_id: request.scope.worktree_id,
        };
        let fence = CompletionFence {
            workspace_id: request.scope.workspace_id,
            session_id: request
                .scope
                .session_id
                .expect("test request has a session"),
            operation_id: OperationId::new(),
            owner_daemon_generation: generation,
            execution_attempt: 1,
            lifecycle_attempt: 1,
            expected_revision: 1,
        };
        (terminal, fence)
    }
    fn process() -> ProcessIdentity {
        ProcessIdentity {
            pid: 7,
            start_identity: "start".into(),
            process_group: 7,
        }
    }
    #[test]
    fn resolve_once_persists_without_env_and_disconnect_keeps_slot() {
        let request = request();
        let (terminal, fence) = refs(&request);
        let mut c = GenericTerminalCoordinator::new(1, 64, 1);
        let mut store = Store::default();
        c.launch(
            &request,
            terminal.clone(),
            fence,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver,
            &mut store,
            &mut Spawner(Ok(process())),
        )
        .unwrap();
        assert_eq!(store.0.len(), 2);
        let encoded = format!("{:?}", store.0);
        assert!(!encoded.contains("xterm-256color"));
        c.disconnect(ConnectionId::new());
        assert_eq!(c.occupied_slots(), 1);
        assert_eq!(c.terminal_snapshot(&terminal).unwrap().terminal, terminal);
    }
    #[test]
    fn ambiguity_blocks_replacement_until_verified_exit_or_gone() {
        let request = request();
        let (terminal, fence) = refs(&request);
        let mut c = GenericTerminalCoordinator::new(1, 64, 1);
        let mut store = Store::default();
        assert_eq!(
            c.launch(
                &request,
                terminal.clone(),
                fence,
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut store,
                &mut Spawner(Err(SpawnFailure::Ambiguous))
            ),
            Err(GenericTerminalError::ReconcileRequired(
                ReconcileState::SpawnAmbiguous
            ))
        );
        assert_eq!(c.occupied_slots(), 1);
        c.reconcile(&terminal, ProcessObservation::Gone, &mut store)
            .unwrap();
        assert_eq!(c.occupied_slots(), 0);
    }
    #[test]
    fn rejects_scope_mismatch_before_resolve() {
        let request = request();
        let (mut terminal, fence) = refs(&request);
        terminal.daemon_generation = DaemonGeneration::new();
        let mut c = GenericTerminalCoordinator::new(1, 64, 1);
        assert_eq!(
            c.launch(
                &request,
                terminal,
                fence,
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut Store::default(),
                &mut Spawner(Ok(process()))
            ),
            Err(GenericTerminalError::ScopeMismatch)
        );
    }
    #[test]
    fn failures_and_reconciliation_remain_fenced() {
        struct BadResolver;
        impl TerminalProfileResolver for BadResolver {
            fn resolve(
                &mut self,
                request: &TerminalLaunchRequest,
            ) -> Result<ResolvedTerminalLaunch, TerminalLaunchValidationError> {
                let mut resolved = Resolver.resolve(request)?;
                resolved.snapshot.schema_version = 0;
                Ok(resolved)
            }
        }
        let request = request();
        let (terminal, fence) = refs(&request);
        let mut c = GenericTerminalCoordinator::new(2, 64, 1);
        assert_eq!(
            c.terminal_snapshot(&terminal),
            Err(GenericTerminalError::UnknownTerminal)
        );
        assert_eq!(
            c.launch(
                &request,
                terminal.clone(),
                fence.clone(),
                Geometry { cols: 80, rows: 24 },
                &mut BadResolver,
                &mut Store::default(),
                &mut Spawner(Ok(process()))
            ),
            Err(GenericTerminalError::ScopeMismatch)
        );
        assert_eq!(
            c.launch(
                &request,
                terminal.clone(),
                fence.clone(),
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut Store::default(),
                &mut Spawner(Err(SpawnFailure::Definite))
            ),
            Err(GenericTerminalError::SpawnFailed)
        );
        let (live, live_fence) = refs(&request);
        let mut store = Store::default();
        c.launch(
            &request,
            live.clone(),
            live_fence,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver,
            &mut store,
            &mut Spawner(Ok(process())),
        )
        .unwrap();
        c.reconcile(
            &live,
            ProcessObservation::VerifiedAlive(process()),
            &mut store,
        )
        .unwrap();
        assert_eq!(
            c.snapshot()
                .records
                .iter()
                .find(|record| record.terminal == live)
                .unwrap()
                .state,
            RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning)
        );
        c.reconcile(&live, ProcessObservation::Unknown, &mut store)
            .unwrap();
        assert_eq!(
            c.snapshot()
                .records
                .iter()
                .find(|record| record.terminal == live)
                .unwrap()
                .state,
            RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown)
        );
        let (exiting, exiting_fence) = refs(&request);
        c.launch(
            &request,
            exiting.clone(),
            exiting_fence,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver,
            &mut store,
            &mut Spawner(Ok(process())),
        )
        .unwrap();
        c.exit(&exiting, 0, &mut store).unwrap();
        assert_eq!(c.occupied_slots(), 1);
        let (failing, failing_fence) = refs(&request);
        assert_eq!(
            c.launch(
                &request,
                failing,
                failing_fence,
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut FailingStore,
                &mut Spawner(Ok(process()))
            ),
            Err(GenericTerminalError::Store)
        );
    }
}
