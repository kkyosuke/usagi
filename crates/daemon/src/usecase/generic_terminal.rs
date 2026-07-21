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

use serde::{Deserialize, Serialize};

use usagi_core::domain::{
    id::{CompletionFence, ConnectionId, TerminalRef},
    terminal_launch::{
        DurableTerminalLaunchSnapshot, ResolvedTerminalLaunch, TerminalInventoryEntry,
        TerminalKind, TerminalLaunchRequest, TerminalLaunchValidationError,
    },
};

use super::{
    generation::{ProcessIdentity, ProcessObservation},
    terminal::{
        Attached, Geometry, InputAck, InputRequest, Output, PtyWriter, RegistryError, Snapshot,
        SpawnFailure, TerminalReconcileState, TerminalRegistry, TerminalRuntimeState,
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableTerminalRecord {
    pub terminal: TerminalRef,
    pub operation: CompletionFence,
    pub launch: DurableTerminalLaunchSnapshot,
    pub state: TerminalRuntimeState,
    pub process: Option<ProcessIdentity>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalStoreSnapshot {
    #[serde(default = "TerminalStoreSnapshot::current_schema_version")]
    pub schema_version: u16,
    pub records: Vec<DurableTerminalRecord>,
}
impl Default for TerminalStoreSnapshot {
    fn default() -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            records: Vec::new(),
        }
    }
}
impl TerminalStoreSnapshot {
    pub const SCHEMA_VERSION: u16 = 1;

    const fn current_schema_version() -> u16 {
        Self::SCHEMA_VERSION
    }

    /// Validates and projects records whose PTY owner died with the previous daemon.
    pub fn reconcile_after_daemon_restart(mut self) -> Result<(Self, usize), GenericTerminalError> {
        self.validate()?;
        let mut interrupted = 0;
        for record in &mut self.records {
            if record.state == TerminalRuntimeState::Reserved
                || record.state == TerminalRuntimeState::Running
                || matches!(record.state, TerminalRuntimeState::ReconcileRequired(_))
            {
                record.state = TerminalRuntimeState::ReconcileRequired(
                    TerminalReconcileState::IdentityUnknown,
                );
                interrupted += 1;
            }
        }
        Ok((self, interrupted))
    }

    fn validate(&self) -> Result<(), GenericTerminalError> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(GenericTerminalError::InvalidSnapshot);
        }
        let mut keys = std::collections::BTreeSet::new();
        for record in &self.records {
            let terminal = &record.terminal;
            let scope = &record.launch.request.scope;
            if record.launch.schema_version != DurableTerminalLaunchSnapshot::SCHEMA_VERSION
                || !keys.insert(terminal.terminal_id.as_str())
                || terminal.workspace_id != scope.workspace_id
                || terminal.session_id != scope.session_id
                || terminal.worktree_id != scope.worktree_id
                || terminal.workspace_id != record.operation.workspace_id
                || terminal.session_id != record.operation.session_id
                || terminal.daemon_generation != record.operation.owner_daemon_generation
            {
                return Err(GenericTerminalError::InvalidSnapshot);
            }
        }
        Ok(())
    }
}
pub trait TerminalStore {
    #[allow(clippy::result_unit_err)] // Persistence detail is intentionally erased at the usecase port.
    fn save(&mut self, snapshot: TerminalStoreSnapshot) -> Result<(), ()>;
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
        geometry: Geometry,
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
    InvalidSnapshot,
    SpawnFailed,
    ReconcileRequired(TerminalReconcileState),
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
    pub fn from_snapshot(
        limit: usize,
        journal_limit: usize,
        input_cache_limit: usize,
        snapshot: TerminalStoreSnapshot,
    ) -> Result<Self, GenericTerminalError> {
        snapshot.validate()?;
        if snapshot.records.iter().any(|record| {
            matches!(
                record.state,
                TerminalRuntimeState::Reserved | TerminalRuntimeState::Running
            ) || matches!(
                record.state,
                TerminalRuntimeState::ReconcileRequired(state)
                    if state != TerminalReconcileState::IdentityUnknown
            )
        }) {
            return Err(GenericTerminalError::InvalidSnapshot);
        }
        let records = snapshot
            .records
            .into_iter()
            .map(|record| (record.terminal.terminal_id.as_str(), record))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            limit,
            records,
            terminals: TerminalRegistry::new(journal_limit, input_cache_limit),
        })
    }
    pub fn launch(
        &mut self,
        request: &TerminalLaunchRequest,
        terminal: TerminalRef,
        operation: CompletionFence,
        geometry: Geometry,
        resolver: &mut dyn TerminalProfileResolver,
        store: &mut dyn TerminalStore,
        spawner: &mut dyn GenericPtySpawner,
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
                state: TerminalRuntimeState::Reserved,
                process: None,
            },
        );
        self.persist(store)?;
        self.terminals
            .register(terminal.clone(), geometry)
            .expect("a newly reserved terminal cannot already be registered");
        match spawner.spawn(&resolved, &terminal, geometry) {
            Ok(process) => {
                let record = self.records.get_mut(&key).expect("reserved record");
                record.process = Some(process);
                record.state = TerminalRuntimeState::Running;
                if self.persist(store).is_err() {
                    self.records.get_mut(&key).expect("reserved record").state =
                        TerminalRuntimeState::ReconcileRequired(
                            TerminalReconcileState::PersistAfterSpawn,
                        );
                    return Err(GenericTerminalError::ReconcileRequired(
                        TerminalReconcileState::PersistAfterSpawn,
                    ));
                }
                Ok(())
            }
            Err(SpawnFailure::Definite) => {
                self.records.get_mut(&key).expect("reserved record").state =
                    TerminalRuntimeState::SpawnFailed;
                self.persist(store)?;
                Err(GenericTerminalError::SpawnFailed)
            }
            Err(SpawnFailure::Ambiguous) => {
                self.records.get_mut(&key).expect("reserved record").state =
                    TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::SpawnAmbiguous);
                self.persist(store)?;
                Err(GenericTerminalError::ReconcileRequired(
                    TerminalReconcileState::SpawnAmbiguous,
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
    /// Atomically takes a snapshot and assigns a connection-owned subscription.
    pub fn attach(
        &mut self,
        terminal: &TerminalRef,
        connection: ConnectionId,
    ) -> Result<Attached, GenericTerminalError> {
        self.running(terminal)?;
        self.terminals
            .attach(terminal, connection)
            .map_err(GenericTerminalError::Terminal)
    }
    /// Removes only the named attachment, never the daemon-owned process.
    pub fn detach(
        &mut self,
        terminal: &TerminalRef,
        subscription: u64,
        connection: ConnectionId,
    ) -> Result<(), GenericTerminalError> {
        self.record(terminal)?;
        self.terminals
            .detach(terminal, subscription, connection)
            .map_err(GenericTerminalError::Terminal)
    }
    /// Applies PTY output to the daemon journal and returns its fenced cursor.
    pub fn output(
        &mut self,
        terminal: &TerminalRef,
        bytes: Vec<u8>,
    ) -> Result<Output, GenericTerminalError> {
        self.running(terminal)?;
        self.terminals
            .append_output(terminal, bytes)
            .map_err(GenericTerminalError::Terminal)
    }
    pub fn resize(
        &mut self,
        terminal: &TerminalRef,
        geometry: Geometry,
        writer: &mut dyn PtyWriter,
    ) -> Result<Snapshot, GenericTerminalError> {
        self.running(terminal)?;
        self.terminals
            .resize(terminal, geometry, writer)
            .map_err(GenericTerminalError::Terminal)
    }
    /// Verifies durable ownership before an IPC adapter performs an effect.
    pub fn ensure_running(&self, terminal: &TerminalRef) -> Result<(), GenericTerminalError> {
        self.running(terminal)
    }
    pub fn input(
        &mut self,
        terminal: &TerminalRef,
        input: InputRequest,
        bytes: &[u8],
        writer: &mut dyn PtyWriter,
    ) -> Result<InputAck, GenericTerminalError> {
        self.running(terminal)?;
        self.terminals
            .write_input(terminal, input, bytes, writer)
            .map_err(GenericTerminalError::Terminal)
    }
    pub fn replay_from(
        &self,
        terminal: &TerminalRef,
        offset: u64,
    ) -> Result<Vec<Output>, GenericTerminalError> {
        self.replayable(terminal)?;
        self.terminals
            .replay_from(terminal, offset)
            .map_err(GenericTerminalError::Terminal)
    }
    pub fn exit(
        &mut self,
        terminal: &TerminalRef,
        status: i32,
        store: &mut dyn TerminalStore,
    ) -> Result<(), GenericTerminalError> {
        self.running(terminal)?;
        self.terminals
            .exited(terminal, status)
            .map_err(GenericTerminalError::Terminal)?;
        self.record_mut(terminal)?.state = TerminalRuntimeState::Exited;
        if self.persist(store).is_err() {
            self.record_mut(terminal)?.state =
                TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::PersistAfterExit);
            return Err(GenericTerminalError::ReconcileRequired(
                TerminalReconcileState::PersistAfterExit,
            ));
        }
        Ok(())
    }
    /// Never starts a replacement after an ambiguous outcome.
    pub fn reconcile(
        &mut self,
        terminal: &TerminalRef,
        observation: ProcessObservation,
        store: &mut dyn TerminalStore,
    ) -> Result<(), GenericTerminalError> {
        let record = self.record_mut(terminal)?;
        record.state = match observation {
            ProcessObservation::Gone => TerminalRuntimeState::Reclaimed,
            ProcessObservation::VerifiedAlive(actual)
                if record.process.as_ref() == Some(&actual) =>
            {
                TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::OrphanRunning)
            }
            _ => TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown),
        };
        self.persist(store)
    }
    #[must_use]
    pub fn snapshot(&self) -> TerminalStoreSnapshot {
        TerminalStoreSnapshot {
            schema_version: TerminalStoreSnapshot::SCHEMA_VERSION,
            records: self.records.values().cloned().collect(),
        }
    }
    /// Lists only terminals in the exact requested durable scope. Each entry is
    /// tagged `Terminal` and marked `live` only while the current daemon
    /// generation still owns a running PTY, so a restoring client attaches to
    /// running terminals and never to exited, reclaimed, or reconcile-required
    /// records.
    #[must_use]
    pub fn inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<TerminalInventoryEntry> {
        self.records
            .values()
            .filter(|record| {
                record.terminal.workspace_id == scope.workspace_id
                    && record.terminal.session_id == scope.session_id
                    && record.terminal.worktree_id == scope.worktree_id
            })
            .map(|record| TerminalInventoryEntry {
                terminal: record.terminal.clone(),
                kind: TerminalKind::Terminal,
                live: matches!(record.state, TerminalRuntimeState::Running),
            })
            .collect()
    }
    #[must_use]
    pub fn occupied_slots(&self) -> usize {
        self.records
            .values()
            .filter(|record| {
                matches!(
                    record.state,
                    TerminalRuntimeState::Reserved
                        | TerminalRuntimeState::Running
                        | TerminalRuntimeState::ReconcileRequired(_)
                )
            })
            .count()
    }
    fn persist(&self, store: &mut dyn TerminalStore) -> Result<(), GenericTerminalError> {
        store
            .save(self.snapshot())
            .map_err(|()| GenericTerminalError::Store)
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
            && terminal.session_id == operation.session_id
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
        match self.record(terminal)?.state {
            TerminalRuntimeState::Running => Ok(()),
            TerminalRuntimeState::Exited | TerminalRuntimeState::Reclaimed => {
                Err(GenericTerminalError::Terminal(RegistryError::Exited))
            }
            _ => Err(GenericTerminalError::ReconcileRequired(
                TerminalReconcileState::IdentityUnknown,
            )),
        }
    }

    /// Retained output remains readable after a terminal exits. Only launches,
    /// input, output, and resize require a running PTY.
    fn replayable(&self, terminal: &TerminalRef) -> Result<(), GenericTerminalError> {
        matches!(
            self.record(terminal)?.state,
            TerminalRuntimeState::Running | TerminalRuntimeState::Exited
        )
        .then_some(())
        .ok_or(GenericTerminalError::ReconcileRequired(
            TerminalReconcileState::IdentityUnknown,
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
        fn save(&mut self, snapshot: TerminalStoreSnapshot) -> Result<(), ()> {
            self.0.push(snapshot);
            Ok(())
        }
    }
    struct FailingStore;
    impl TerminalStore for FailingStore {
        fn save(&mut self, _: TerminalStoreSnapshot) -> Result<(), ()> {
            Err(())
        }
    }
    struct FailAfter(usize);
    impl TerminalStore for FailAfter {
        fn save(&mut self, _: TerminalStoreSnapshot) -> Result<(), ()> {
            self.0 = self.0.saturating_sub(1);
            (self.0 != 0).then_some(()).ok_or(())
        }
    }
    struct Resolver;
    impl TerminalProfileResolver for Resolver {
        fn resolve(
            &mut self,
            request: &TerminalLaunchRequest,
        ) -> Result<ResolvedTerminalLaunch, TerminalLaunchValidationError> {
            Ok(ResolvedTerminalLaunch::new(
                DurableTerminalLaunchSnapshot::new(
                    request.clone(),
                    1,
                    "/bin/sh",
                    vec![],
                    PathBuf::from("."),
                    [EnvironmentVariableName::new("TERM").unwrap()],
                )
                .expect("the trusted test profile is valid"),
                BTreeMap::from([(
                    EnvironmentVariableName::new("TERM").unwrap(),
                    "xterm-256color".into(),
                )]),
            )
            .expect("the trusted test environment matches its allowlist"))
        }
    }
    struct Spawner(Result<ProcessIdentity, SpawnFailure>);
    impl GenericPtySpawner for Spawner {
        fn spawn(
            &mut self,
            _: &ResolvedTerminalLaunch,
            _: &TerminalRef,
            _: Geometry,
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
            session_id: request.scope.session_id,
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
    fn restart_projection_fences_reserved_records_and_rejects_unknown_launch_schema() {
        let request = request();
        let (terminal, fence) = refs(&request);
        let mut coordinator = GenericTerminalCoordinator::new(1, 64, 1);
        coordinator
            .launch(
                &request,
                terminal,
                fence,
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut Store::default(),
                &mut Spawner(Ok(process())),
            )
            .unwrap();
        let mut reserved = coordinator.snapshot();
        reserved.records[0].state = TerminalRuntimeState::Reserved;
        let (reserved, interrupted) = reserved.reconcile_after_daemon_restart().unwrap();
        assert_eq!(interrupted, 1);
        assert_eq!(
            reserved.records[0].state,
            TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown)
        );

        let mut unknown = coordinator.snapshot();
        unknown.records[0].launch.schema_version += 1;
        assert_eq!(
            unknown.reconcile_after_daemon_restart(),
            Err(GenericTerminalError::InvalidSnapshot)
        );
    }

    #[test]
    fn snapshot_restore_and_capacity_edges_are_total() {
        let legacy: TerminalStoreSnapshot =
            serde_json::from_value(serde_json::json!({"records": []})).unwrap();
        assert_eq!(legacy, TerminalStoreSnapshot::default());

        let request = request();
        let (terminal, fence) = refs(&request);
        let mut coordinator = GenericTerminalCoordinator::new(1, 64, 1);
        let mut store = Store::default();
        coordinator
            .launch(
                &request,
                terminal.clone(),
                fence.clone(),
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut store,
                &mut Spawner(Ok(process())),
            )
            .unwrap();
        assert_eq!(
            coordinator.launch(
                &request,
                terminal.clone(),
                fence,
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut store,
                &mut Spawner(Ok(process())),
            ),
            Err(GenericTerminalError::TerminalAlreadyExists)
        );
        let (other, other_fence) = refs(&request);
        assert_eq!(
            coordinator.launch(
                &request,
                other,
                other_fence,
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut store,
                &mut Spawner(Ok(process())),
            ),
            Err(GenericTerminalError::ConcurrencyExhausted)
        );

        let running = coordinator.snapshot();
        let (reconciled, count) = running.clone().reconcile_after_daemon_restart().unwrap();
        assert_eq!(count, 1);
        let (already_reconciling, count) =
            reconciled.clone().reconcile_after_daemon_restart().unwrap();
        assert_eq!(count, 1);
        assert_eq!(already_reconciling, reconciled);
        assert!(GenericTerminalCoordinator::from_snapshot(1, 64, 1, running).is_err());
        assert!(GenericTerminalCoordinator::from_snapshot(1, 64, 1, reconciled.clone()).is_ok());
        let mut wrong_reconcile = reconciled;
        wrong_reconcile.records[0].state =
            TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::SpawnAmbiguous);
        assert!(GenericTerminalCoordinator::from_snapshot(1, 64, 1, wrong_reconcile).is_err());

        coordinator
            .records
            .get_mut(&terminal.terminal_id.as_str())
            .unwrap()
            .state = TerminalRuntimeState::Reclaimed;
        assert_eq!(
            coordinator.replay_from(&terminal, 0),
            Err(GenericTerminalError::ReconcileRequired(
                TerminalReconcileState::IdentityUnknown
            ))
        );
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
    fn workspace_root_scope_launches_and_fences_without_a_session() {
        let request = TerminalLaunchRequest {
            profile_id: TerminalProfileId::new("login-shell").unwrap(),
            scope: TerminalLaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: None,
                worktree_id: WorktreeId::new(),
            },
        };
        let (terminal, fence) = refs(&request);
        assert_eq!(terminal.session_id, None);
        assert_eq!(fence.session_id, None);
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
        // The root terminal is registered and fenced by its own reference.
        c.output(&terminal, b"root\n".to_vec()).unwrap();
        assert_eq!(c.terminal_snapshot(&terminal).unwrap().terminal, terminal);
    }

    #[test]
    fn exited_terminal_keeps_its_retained_output_available_for_resume() {
        let request = request();
        let (terminal, fence) = refs(&request);
        let mut coordinator = GenericTerminalCoordinator::new(1, 64, 1);
        let mut store = Store::default();
        coordinator
            .launch(
                &request,
                terminal.clone(),
                fence,
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut store,
                &mut Spawner(Ok(process())),
            )
            .unwrap();
        coordinator.output(&terminal, b"done".to_vec()).unwrap();
        coordinator.exit(&terminal, 0, &mut store).unwrap();

        assert_eq!(
            coordinator.replay_from(&terminal, 0).unwrap()[0].data,
            b"done"
        );
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
                TerminalReconcileState::SpawnAmbiguous
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
            TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::OrphanRunning)
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
            TerminalRuntimeState::ReconcileRequired(TerminalReconcileState::IdentityUnknown)
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
    #[test]
    fn resolver_store_and_terminal_identity_failures_are_typed() {
        struct RejectingResolver;
        impl TerminalProfileResolver for RejectingResolver {
            fn resolve(
                &mut self,
                request: &TerminalLaunchRequest,
            ) -> Result<ResolvedTerminalLaunch, TerminalLaunchValidationError> {
                Err(TerminalLaunchValidationError::UnknownProfile {
                    profile_id: request.profile_id.clone(),
                })
            }
        }
        let request = request();
        let (terminal, fence) = refs(&request);
        let mut coordinator = GenericTerminalCoordinator::new(2, 64, 1);
        assert_eq!(
            coordinator.launch(
                &request,
                terminal.clone(),
                fence.clone(),
                Geometry { cols: 80, rows: 24 },
                &mut RejectingResolver,
                &mut Store::default(),
                &mut Spawner(Ok(process()))
            ),
            Err(GenericTerminalError::Launch(
                TerminalLaunchValidationError::UnknownProfile {
                    profile_id: request.profile_id.clone()
                }
            ))
        );
        let (persist_after_spawn, spawn_fence) = refs(&request);
        assert_eq!(
            coordinator.launch(
                &request,
                persist_after_spawn,
                spawn_fence,
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut FailAfter(2),
                &mut Spawner(Ok(process()))
            ),
            Err(GenericTerminalError::ReconcileRequired(
                TerminalReconcileState::PersistAfterSpawn
            ))
        );
        let (live, live_fence) = refs(&request);
        let mut store = Store::default();
        coordinator
            .launch(
                &request,
                live.clone(),
                live_fence,
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut store,
                &mut Spawner(Ok(process())),
            )
            .unwrap();
        let key = live.terminal_id.as_str();
        coordinator
            .records
            .get_mut(&key)
            .unwrap()
            .terminal
            .daemon_generation = DaemonGeneration::new();
        let stale = coordinator.records[&key].terminal.clone();
        assert_eq!(
            coordinator.terminal_snapshot(&stale),
            Err(GenericTerminalError::TerminalGenerationMismatch)
        );
    }
}
