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
    id::{ClientId, CompletionFence, ConnectionId, OperationId, TerminalRef},
    terminal_launch::{
        DurableTerminalLaunchSnapshot, ResolvedTerminalLaunch, TerminalInventoryEntry,
        TerminalKind, TerminalLaunchRequest, TerminalLaunchValidationError,
        canonical_launch_digest,
    },
    terminal_retention::{AdmissionRejection, EvictionReason, FinalLookup, RetainedFinal},
};

use super::{
    generation::{ProcessIdentity, ProcessObservation},
    terminal::{
        Attached, Geometry, InputAck, InputRequest, Output, PtyWriter, RegistryError, Snapshot,
        SpawnFailure, TerminalReconcileState, TerminalRegistry, TerminalRuntimeState,
    },
    terminal_retention_ipc::{RESTORED_FINAL_BYTES, SharedTerminalRetention},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableTerminalRecord {
    pub terminal: TerminalRef,
    pub operation: CompletionFence,
    pub launch: DurableTerminalLaunchSnapshot,
    pub state: TerminalRuntimeState,
    pub process: Option<ProcessIdentity>,
    /// Canonical intent digest of the launch this record was created for. It is
    /// what proves a repeated producer `OperationId` carries the *same* request,
    /// so a replay answers with this terminal and a different intent is refused
    /// (#518). Absent on records written before the producer id reached the wire;
    /// such a record can never prove a replay and therefore refuses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_digest: Option<String>,
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
    /// The aggregate retention budget cannot reserve this launch's worst-case
    /// final, so the launch is refused before any PTY is spawned (#526).
    RetentionExhausted(AdmissionRejection),
    /// The terminal existed, and its final was collected by aggregate
    /// retention. It is never answered as unknown or with another terminal's
    /// history.
    FinalEvicted(EvictionReason),
}

/// Owns generic shell PTYs. It has no `AgentRuntimeId` or adapter hook path.
#[derive(Debug)]
pub struct GenericTerminalCoordinator {
    limit: usize,
    records: BTreeMap<String, DurableTerminalRecord>,
    terminals: TerminalRegistry,
    retention: SharedTerminalRetention,
}
impl GenericTerminalCoordinator {
    #[must_use]
    pub fn new(limit: usize, journal_limit: usize, input_cache_limit: usize) -> Self {
        Self::with_retention(
            limit,
            journal_limit,
            input_cache_limit,
            SharedTerminalRetention::new(),
        )
    }
    /// Builds an owner bound to the daemon-wide retention authority so its
    /// finals share one aggregate budget with the Agent owner's (#526).
    #[must_use]
    pub fn with_retention(
        limit: usize,
        journal_limit: usize,
        input_cache_limit: usize,
        retention: SharedTerminalRetention,
    ) -> Self {
        Self {
            limit,
            records: BTreeMap::new(),
            terminals: TerminalRegistry::new(journal_limit, input_cache_limit),
            retention,
        }
    }
    pub fn from_snapshot(
        limit: usize,
        journal_limit: usize,
        input_cache_limit: usize,
        snapshot: TerminalStoreSnapshot,
    ) -> Result<Self, GenericTerminalError> {
        Self::from_snapshot_with_retention(
            limit,
            journal_limit,
            input_cache_limit,
            snapshot,
            SharedTerminalRetention::new(),
        )
    }
    /// Restores durable records and re-imports their finals into the shared
    /// retention accounting, which is derived state a restart rebuilds. Records
    /// that predate the aggregate budget are migrated here and become ordinary
    /// collection candidates.
    pub fn from_snapshot_with_retention(
        limit: usize,
        journal_limit: usize,
        input_cache_limit: usize,
        snapshot: TerminalStoreSnapshot,
        retention: SharedTerminalRetention,
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
        let restored_at = retention.now();
        for record in records.values() {
            if record.state == TerminalRuntimeState::Exited {
                retention.import_existing(RetainedFinal::new(
                    record.terminal.clone(),
                    TerminalKind::Terminal,
                    RESTORED_FINAL_BYTES,
                    restored_at,
                ));
            }
        }
        Ok(Self {
            limit,
            records,
            terminals: TerminalRegistry::new(journal_limit, input_cache_limit),
            retention,
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
        // Reserve the worst-case final this runtime will leave behind before
        // anything is spawned. An exhausted aggregate budget refuses the launch
        // here instead of dropping somebody else's protected final later.
        self.retention
            .reserve(&terminal)
            .map_err(GenericTerminalError::RetentionExhausted)?;
        // Applying the collection the reservation may have triggered keeps the
        // records, the journals, and the accounting converged.
        self.collect_garbage(store);
        let outcome = self.launch_admitted(
            request,
            terminal.clone(),
            operation,
            geometry,
            resolver,
            store,
            spawner,
        );
        if outcome.is_err() {
            // No final will ever be committed for a refused or failed launch.
            self.retention.release(&terminal);
        }
        outcome
    }

    /// The record created for one producer launch operation, if this owner still
    /// holds it. It is how a repeated `Launch` is answered without spawning: the
    /// caller compares the recorded canonical digest before replaying (#518).
    #[must_use]
    pub fn launch_by_operation(&self, operation: &OperationId) -> Option<&DurableTerminalRecord> {
        self.records
            .values()
            .find(|record| record.operation.operation_id == *operation)
    }

    fn launch_admitted(
        &mut self,
        request: &TerminalLaunchRequest,
        terminal: TerminalRef,
        operation: CompletionFence,
        geometry: Geometry,
        resolver: &mut dyn TerminalProfileResolver,
        store: &mut dyn TerminalStore,
        spawner: &mut dyn GenericPtySpawner,
    ) -> Result<(), GenericTerminalError> {
        let key = terminal.terminal_id.as_str();
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
                launch_digest: Some(canonical_launch_digest(
                    request,
                    geometry.cols,
                    geometry.rows,
                )),
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
    pub fn disconnect(&mut self, connection: ConnectionId, writer: &mut dyn PtyWriter) {
        self.terminals.disconnect(connection, writer);
        // Finals this connection was draining are no longer pinned.
        for record in self.records.values() {
            if record.state == TerminalRuntimeState::Exited {
                self.retention.set_pinned(
                    &record.terminal,
                    self.terminals.is_attached(&record.terminal),
                );
            }
        }
    }
    pub fn terminal_snapshot(
        &self,
        terminal: &TerminalRef,
    ) -> Result<Snapshot, GenericTerminalError> {
        self.record(terminal)?;
        // The registry's typed failure is preserved: a fencing failure and a
        // screen that does not fit one frame are different client contracts.
        self.terminals
            .snapshot(terminal)
            .map_err(GenericTerminalError::Terminal)
    }
    /// The committed exit status without capturing a screen, for the incremental
    /// `Resume` path.
    pub fn terminal_exit_status(
        &self,
        terminal: &TerminalRef,
    ) -> Result<Option<i32>, GenericTerminalError> {
        self.record(terminal)?;
        self.terminals
            .exit_status(terminal)
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
    /// Atomically attaches and exposes the connection/client input ledger cursor.
    pub fn attach_for_client(
        &mut self,
        terminal: &TerminalRef,
        connection: ConnectionId,
        client: ClientId,
        viewport: Option<Geometry>,
        writer: &mut dyn PtyWriter,
    ) -> Result<Attached, GenericTerminalError> {
        self.running(terminal)?;
        self.terminals
            .attach_for_client(terminal, connection, client, viewport, writer)
            .map_err(GenericTerminalError::Terminal)
    }
    /// Removes only the named attachment, never the daemon-owned process.
    pub fn detach(
        &mut self,
        terminal: &TerminalRef,
        subscription: u64,
        connection: ConnectionId,
        writer: &mut dyn PtyWriter,
    ) -> Result<(), GenericTerminalError> {
        self.record(terminal)?;
        let detached = self
            .terminals
            .detach(terminal, subscription, connection, writer)
            .map_err(GenericTerminalError::Terminal);
        // A final nobody is draining any more is an ordinary GC candidate.
        self.retention
            .set_pinned(terminal, self.terminals.is_attached(terminal));
        detached
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
        client: Option<&ClientId>,
        writer: &mut dyn PtyWriter,
    ) -> Result<Snapshot, GenericTerminalError> {
        self.running(terminal)?;
        self.terminals
            .resize(terminal, geometry, client, writer)
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
        // The liveness gate stays on the write path: a *new* operation must
        // never reach a reserved, reconciling, or exited runtime. Resolving an
        // already recorded operation is the read-only `input_outcome` path, which
        // is deliberately not gated this way.
        self.running(terminal)?;
        self.terminals
            .write_input(terminal, input, bytes, self.retention.now_ms(), writer)
            .map_err(GenericTerminalError::Terminal)
    }

    /// Reads the recorded final of one durable input operation. `Ok(None)` is a
    /// typed unknown rather than an error, and never authorizes a rewrite.
    pub fn input_outcome(
        &mut self,
        terminal: &TerminalRef,
        client: ClientId,
        operation: OperationId,
    ) -> Result<Option<InputAck>, GenericTerminalError> {
        let now_ms = self.retention.now_ms();
        self.terminals
            .input_outcome(terminal, client, operation, now_ms)
            .map_err(GenericTerminalError::Terminal)
    }
    pub fn replay_from(
        &self,
        terminal: &TerminalRef,
        offset: u64,
        client: Option<&ClientId>,
    ) -> Result<Vec<Output>, GenericTerminalError> {
        self.replayable(terminal)?;
        self.terminals
            .replay_from(terminal, offset, client)
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
            // The reservation stays held: the journal still holds these bytes
            // and the record needs reconciliation, so its capacity is not freed.
            return Err(GenericTerminalError::ReconcileRequired(
                TerminalReconcileState::PersistAfterExit,
            ));
        }
        // The exit result is stored into the capacity reserved before spawn, so
        // no cap can drop it. A client still draining this final pins it.
        let bytes = self.terminals.retained_bytes(terminal);
        self.retention
            .commit_final(terminal, TerminalKind::Terminal, bytes);
        self.retention
            .set_pinned(terminal, self.terminals.is_attached(terminal));
        self.collect_garbage(store);
        Ok(())
    }

    /// Applies the aggregate retention authority's decisions to this owner:
    /// every exited record whose final the authority collected loses its
    /// durable record and its output journal, and the store is rewritten once.
    ///
    /// Only a final the authority evicted with a typed marker is removed, so a
    /// record the ledger never accounted for is never deleted by accident. The
    /// work is bounded by the collection batch, and a failed store write leaves
    /// the removal to converge on a later pass or the next startup import
    /// rather than resurrecting the runtime.
    pub fn collect_garbage(&mut self, store: &mut dyn TerminalStore) -> usize {
        let collected: Vec<TerminalRef> = self
            .records
            .values()
            .filter(|record| record.state == TerminalRuntimeState::Exited)
            .filter(|record| {
                matches!(
                    self.retention.lookup(&record.terminal),
                    FinalLookup::Evicted(_)
                )
            })
            .map(|record| record.terminal.clone())
            .collect();
        for terminal in &collected {
            self.records.remove(&terminal.terminal_id.as_str());
            self.terminals.forget(terminal);
        }
        if !collected.is_empty() {
            let _ = self.persist(store);
        }
        collected.len()
    }

    /// The aggregate retention authority this owner shares with the Agent owner.
    #[must_use]
    pub fn retention(&self) -> &SharedTerminalRetention {
        &self.retention
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
    /// Whether this workspace still has a generic terminal that is running.
    ///
    /// Unlike [`Self::inventory`], the question is about the whole workspace
    /// rather than one scope inside it: a retirement gives back the workspace,
    /// so any live child anywhere in it must keep it.
    #[must_use]
    pub fn has_running_in_workspace(&self, workspace: usagi_core::domain::id::WorkspaceId) -> bool {
        self.records.values().any(|record| {
            record.terminal.workspace_id == workspace
                && matches!(record.state, TerminalRuntimeState::Running)
        })
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
    /// Lists exited generic-terminal tombstones in the exact requested scope
    /// with their exit status and bounded final-replay locator (#525). The
    /// visibility field is a placeholder; the shared owner overwrites it from
    /// the authoritative workspace-global ledger. Running / reserved /
    /// reconcile-required / reclaimed records are excluded.
    #[must_use]
    pub fn completed_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_visibility::CompletedTerminalEntry> {
        use usagi_core::domain::terminal_visibility::{CompletedTerminalEntry, TerminalVisibility};
        self.records
            .values()
            .filter(|record| {
                record.terminal.workspace_id == scope.workspace_id
                    && record.terminal.session_id == scope.session_id
                    && record.terminal.worktree_id == scope.worktree_id
                    && matches!(record.state, TerminalRuntimeState::Exited)
            })
            .filter_map(|record| {
                // Offsets only: a tombstone listing must not capture one screen
                // per entry (see `TerminalRegistry::output_window`).
                let window = self.terminals.output_window(&record.terminal).ok()?;
                let exit_status = window.exited?;
                Some(CompletedTerminalEntry {
                    terminal: record.terminal.clone(),
                    kind: TerminalKind::Terminal,
                    exit_status,
                    base_offset: window.base_offset,
                    final_output_offset: window.output_offset,
                    visibility: TerminalVisibility::unobserved(),
                })
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
        let missing = self.missing(terminal);
        self.records
            .get(&terminal.terminal_id.as_str())
            .filter(|record| record.terminal.fences(terminal))
            .ok_or(missing)
    }
    fn record_mut(
        &mut self,
        terminal: &TerminalRef,
    ) -> Result<&mut DurableTerminalRecord, GenericTerminalError> {
        let missing = self.missing(terminal);
        self.records
            .get_mut(&terminal.terminal_id.as_str())
            .filter(|record| record.terminal.fences(terminal))
            .ok_or(missing)
    }
    /// Why a terminal is absent: collected by aggregate retention, or never
    /// owned here. A collected final is a typed outcome, never a fallback to
    /// some other history.
    fn missing(&self, terminal: &TerminalRef) -> GenericTerminalError {
        match self.retention.lookup(terminal) {
            FinalLookup::Evicted(marker) => GenericTerminalError::FinalEvicted(marker.reason),
            _ => GenericTerminalError::UnknownTerminal,
        }
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
        id::{
            DaemonGeneration, OperationId, RequestId, SessionId, TerminalId, WorkspaceId,
            WorktreeId,
        },
        terminal_launch::{TerminalLaunchScope, TerminalProfileId},
    };
    /// A PTY writer that accepts everything: these tests exercise the
    /// coordinator, not the transport.
    #[derive(Default)]
    struct Writer(Vec<u8>);
    impl PtyWriter for Writer {
        fn write_all(
            &mut self,
            bytes: &[u8],
        ) -> Result<(), crate::usecase::terminal::PtyWriteError> {
            self.0.extend_from_slice(bytes);
            Ok(())
        }
    }
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
            coordinator.replay_from(&terminal, 0, None),
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
        c.disconnect(ConnectionId::new(), &mut Writer::default());
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
            coordinator.replay_from(&terminal, 0, None).unwrap()[0].data,
            b"done"
        );
    }
    #[test]
    fn completed_inventory_lists_only_exited_in_scope_tombstones() {
        use usagi_core::domain::terminal_launch::TerminalKind;
        let request = request();
        let (terminal, fence) = refs(&request);
        let mut coordinator = GenericTerminalCoordinator::new(4, 64, 1);
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
        // A running terminal is not a completed tombstone.
        assert!(coordinator.completed_inventory(&request.scope).is_empty());

        coordinator
            .output(&terminal, b"final output".to_vec())
            .unwrap();
        coordinator.exit(&terminal, 5, &mut store).unwrap();

        let completed = coordinator.completed_inventory(&request.scope);
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].kind, TerminalKind::Terminal);
        assert!(completed[0].terminal.fences(&terminal));
        assert_eq!(completed[0].exit_status, 5);
        assert_eq!(completed[0].base_offset, 0);
        assert_eq!(
            completed[0].final_output_offset,
            "final output".len() as u64
        );

        // Another scope does not see this tombstone.
        let other_scope = usagi_core::domain::terminal_launch::TerminalLaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: None,
            worktree_id: WorktreeId::new(),
        };
        assert!(coordinator.completed_inventory(&other_scope).is_empty());
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
            Err(GenericTerminalError::Terminal(RegistryError::StaleTarget))
        );
        assert_eq!(
            coordinator.terminal_exit_status(&stale),
            Err(GenericTerminalError::TerminalGenerationMismatch)
        );
    }

    /// Launches, drains one output chunk, and exits one generic terminal.
    fn run_terminal(
        coordinator: &mut GenericTerminalCoordinator,
        store: &mut dyn TerminalStore,
        bytes: &[u8],
    ) -> TerminalRef {
        let request = request();
        let (terminal, fence) = refs(&request);
        coordinator
            .launch(
                &request,
                terminal.clone(),
                fence,
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                store,
                &mut Spawner(Ok(process())),
            )
            .expect("the fixture admits this launch");
        coordinator.output(&terminal, bytes.to_vec()).unwrap();
        coordinator.exit(&terminal, 0, store).unwrap();
        terminal
    }

    #[test]
    fn a_launch_reserves_its_final_and_a_failed_launch_gives_the_capacity_back() {
        let (retention, _clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
        let mut coordinator =
            GenericTerminalCoordinator::with_retention(4, 64, 1, retention.clone());
        let request = request();
        let (terminal, fence) = refs(&request);
        // A definite spawn failure never produces a final, so its reservation
        // must not keep occupying the aggregate budget.
        assert_eq!(
            coordinator.launch(
                &request,
                terminal.clone(),
                fence,
                Geometry { cols: 80, rows: 24 },
                &mut Resolver,
                &mut Store::default(),
                &mut Spawner(Err(SpawnFailure::Definite)),
            ),
            Err(GenericTerminalError::SpawnFailed)
        );
        assert_eq!(retention.metrics().reserved_finals, 0);
        assert_eq!(retention.metrics().retained_finals, 0);

        // A successful run commits its final into the reserved capacity.
        let mut store = Store::default();
        let terminal = run_terminal(&mut coordinator, &mut store, b"final output");
        let metrics = retention.metrics();
        assert_eq!(metrics.reserved_finals, 0);
        assert_eq!(metrics.retained_finals, 1);
        assert_eq!(metrics.retained_bytes, 12);
        assert!(retention.lookup(&terminal).retained().is_some());
    }

    /// Counts spawns so a rejected launch can prove no PTY was started.
    struct CountingSpawner<'a>(&'a std::cell::Cell<usize>);
    impl GenericPtySpawner for CountingSpawner<'_> {
        fn spawn(
            &mut self,
            _: &ResolvedTerminalLaunch,
            _: &TerminalRef,
            _: Geometry,
        ) -> Result<ProcessIdentity, SpawnFailure> {
            self.0.set(self.0.get() + 1);
            Ok(ProcessIdentity {
                pid: 9,
                start_identity: "start".into(),
                process_group: 9,
            })
        }
    }

    #[test]
    fn an_exhausted_retention_budget_refuses_the_launch_before_spawn() {
        let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
        let mut coordinator =
            GenericTerminalCoordinator::with_retention(8, 64, 1, retention.clone());
        let mut store = Store::default();
        let mut retained = Vec::new();
        for _ in 0..3 {
            retained.push(run_terminal(&mut coordinator, &mut store, b"x"));
        }
        // All three finals are inside the minimum visibility TTL.
        clock.advance(1);
        let request = request();
        let (terminal, fence) = refs(&request);
        let spawns = std::cell::Cell::new(0);
        let rejected = coordinator.launch(
            &request,
            terminal.clone(),
            fence,
            Geometry { cols: 80, rows: 24 },
            &mut Resolver,
            &mut store,
            &mut CountingSpawner(&spawns),
        );
        assert!(matches!(
            rejected,
            Err(GenericTerminalError::RetentionExhausted(_))
        ));
        // Nothing was spawned and no protected final was deleted to make room.
        assert_eq!(spawns.get(), 0);
        assert_eq!(retention.metrics().retained_finals, 3);
        assert_eq!(retention.metrics().evicted_finals, 0);
        for terminal in &retained {
            assert!(coordinator.terminal_exit_status(terminal).is_ok());
        }
        // Past the TTL the same launch admits, because the reserve path
        // collects first.
        clock.advance(30);
        let later = super::tests::request();
        let (terminal, fence) = refs(&later);
        assert!(
            coordinator
                .launch(
                    &later,
                    terminal,
                    fence,
                    Geometry { cols: 80, rows: 24 },
                    &mut Resolver,
                    &mut store,
                    &mut CountingSpawner(&spawns),
                )
                .is_ok()
        );
        // The one spawn belongs to the admitted launch, never to the rejected one.
        assert_eq!(spawns.get(), 1);
        assert!(retention.metrics().evicted_finals >= 1);
    }

    #[test]
    fn a_collected_final_leaves_no_record_journal_or_untyped_query() {
        let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
        let mut coordinator =
            GenericTerminalCoordinator::with_retention(4, 64, 1, retention.clone());
        let mut store = Store::default();
        let terminal = run_terminal(&mut coordinator, &mut store, b"bye");
        clock.advance(1000);
        retention.collect();
        assert_eq!(coordinator.collect_garbage(&mut store), 1);
        // The durable record, the journal, and the inventory entry are gone.
        assert!(coordinator.snapshot().records.is_empty());
        assert!(coordinator.completed_inventory(&request().scope).is_empty());
        assert!(
            store
                .0
                .last()
                .is_some_and(|snapshot| snapshot.records.is_empty())
        );
        // The query is typed: expired history, not an unknown terminal and not
        // some other terminal's replay.
        assert_eq!(
            coordinator.terminal_snapshot(&terminal),
            Err(GenericTerminalError::FinalEvicted(
                usagi_core::domain::terminal_retention::EvictionReason::AgeExpired
            ))
        );
        assert_eq!(
            coordinator.replay_from(&terminal, 0, None),
            Err(GenericTerminalError::FinalEvicted(
                usagi_core::domain::terminal_retention::EvictionReason::AgeExpired
            ))
        );
        // A terminal the authority never held stays unknown.
        let (stranger, _) = refs(&request());
        assert_eq!(
            coordinator.terminal_snapshot(&stranger),
            Err(GenericTerminalError::UnknownTerminal)
        );
        // A second pass is idempotent.
        assert_eq!(coordinator.collect_garbage(&mut store), 0);
        assert_eq!(coordinator.retention().metrics().retained_finals, 0);
    }

    #[test]
    fn a_final_a_client_is_still_draining_is_not_collected() {
        let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
        let mut coordinator =
            GenericTerminalCoordinator::with_retention(4, 64, 1, retention.clone());
        let mut store = Store::default();
        let request = request();
        let (terminal, fence) = refs(&request);
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
        let connection = ConnectionId::new();
        let attached = coordinator.attach(&terminal, connection).unwrap();
        // A live attachment can write, and the bytes reach the PTY writer.
        let mut pty = Writer::default();
        assert_eq!(
            coordinator.input(
                &terminal,
                InputRequest {
                    subscription: attached.subscription,
                    connection,
                    client: ClientId::new(),
                    request: RequestId::new(),
                    input_seq: 0,
                    operation: None,
                },
                b"echo ok\n",
                &mut pty,
            ),
            Ok(InputAck::Written)
        );
        assert_eq!(pty.0, b"echo ok\n");
        coordinator.exit(&terminal, 0, &mut store).unwrap();
        clock.advance(1000);
        retention.collect();
        assert_eq!(coordinator.collect_garbage(&mut store), 0);
        assert!(coordinator.terminal_snapshot(&terminal).is_ok());

        // Detaching releases the protection, and the next pass collects it.
        coordinator
            .detach(
                &terminal,
                attached.subscription,
                connection,
                &mut Writer::default(),
            )
            .unwrap();
        retention.collect();
        assert_eq!(coordinator.collect_garbage(&mut store), 1);
    }

    #[test]
    fn a_disconnect_releases_the_protection_of_every_final_it_was_draining() {
        let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
        let mut coordinator =
            GenericTerminalCoordinator::with_retention(4, 64, 1, retention.clone());
        let mut store = Store::default();
        let request = request();
        let (terminal, fence) = refs(&request);
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
        let connection = ConnectionId::new();
        coordinator.attach(&terminal, connection).unwrap();
        coordinator.exit(&terminal, 0, &mut store).unwrap();
        clock.advance(1000);
        coordinator.disconnect(connection, &mut Writer::default());
        retention.collect();
        assert_eq!(coordinator.collect_garbage(&mut store), 1);
    }

    #[test]
    fn a_restart_reimports_exited_records_so_the_budget_survives_it() {
        let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
        let mut coordinator =
            GenericTerminalCoordinator::with_retention(4, 64, 1, retention.clone());
        let mut store = Store::default();
        let terminal = run_terminal(&mut coordinator, &mut store, b"gone");
        let snapshot = coordinator.snapshot();
        drop(coordinator);

        // A fresh daemon rebuilds the accounting from the durable records; the
        // reservation of the previous process is not carried over.
        let (restarted_retention, restart_clock) =
            crate::usecase::terminal_retention_ipc::tests::manual_retention();
        let mut restarted = GenericTerminalCoordinator::from_snapshot_with_retention(
            4,
            64,
            1,
            snapshot,
            restarted_retention.clone(),
        )
        .unwrap();
        let metrics = restarted_retention.metrics();
        assert_eq!(metrics.retained_finals, 1);
        assert_eq!(metrics.reserved_finals, 0);
        assert_eq!(
            metrics.retained_bytes,
            crate::usecase::terminal_retention_ipc::RESTORED_FINAL_BYTES
        );
        // It ages out of the restored budget like any other final.
        restart_clock.advance(1000);
        restarted_retention.collect();
        let mut store = Store::default();
        assert_eq!(restarted.collect_garbage(&mut store), 1);
        assert!(restarted_retention.lookup(&terminal).marker().is_some());
        clock.advance(1);
    }

    #[test]
    fn a_store_failure_during_collection_converges_on_the_next_pass() {
        let (retention, clock) = crate::usecase::terminal_retention_ipc::tests::manual_retention();
        let mut coordinator =
            GenericTerminalCoordinator::with_retention(4, 64, 1, retention.clone());
        let mut store = Store::default();
        let terminal = run_terminal(&mut coordinator, &mut store, b"x");
        clock.advance(1000);
        retention.collect();
        // The store write fails, but the runtime is not resurrected: the record
        // is gone from memory and the query stays typed.
        assert_eq!(coordinator.collect_garbage(&mut FailingStore), 1);
        assert!(coordinator.snapshot().records.is_empty());
        assert_eq!(
            coordinator.terminal_snapshot(&terminal),
            Err(GenericTerminalError::FinalEvicted(
                usagi_core::domain::terminal_retention::EvictionReason::AgeExpired
            ))
        );
        // A later successful write publishes the same converged snapshot.
        coordinator.persist(&mut store).unwrap();
        assert!(store.0.last().unwrap().records.is_empty());
    }
}
