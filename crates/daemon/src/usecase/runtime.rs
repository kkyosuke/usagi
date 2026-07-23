//! Durable Agent runtime reservation and terminal-stream orchestration.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::unused_self
)] // Generic injected ports make individual error types and launch dependencies part of the contract.

use std::collections::{BTreeMap, BTreeSet};

use usagi_core::domain::{
    agent::{
        DurableLaunchSnapshot, LaunchRequest, LaunchValidationError, ProviderResumePhase,
        ProviderResumeRef, ProviderResumeStatus,
    },
    id::{AgentRuntimeRef, CompletionFence, ConnectionId, TerminalRef},
};

pub use super::terminal::{
    SpawnFailure, TerminalReconcileState as ReconcileState, TerminalRuntimeState as RuntimeState,
};
use super::{
    generation::{
        DEFAULT_GENERATION_LIMIT, GenerationCoordinator, GenerationError, GenerationRecord,
        GenerationRole, GenerationSnapshot, ProcessIdentity, ProcessObservation, TerminalOwnership,
        TerminalState,
    },
    terminal::{
        Attached, Geometry, InputAck, InputRequest, Output, PtyWriter, RegistryError, Snapshot,
        TerminalRegistry,
    },
};

/// Durable association; `launch` is never re-resolved during reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DurableRuntimeRecord {
    pub runtime: AgentRuntimeRef,
    pub operation: CompletionFence,
    pub launch: DurableLaunchSnapshot,
    pub state: RuntimeState,
    pub process: Option<ProcessIdentity>,
    /// Provider-owned conversation identity. It is sensitive metadata, never a
    /// usagi session or terminal identity, and is absent on legacy/Codex runs
    /// for which no documented structured capture channel was available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_resume: Option<ProviderResumeRef>,
    /// Daemon-issued public lineage identity. Legacy records omit it and remain
    /// visible but are never exact-resume targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<usagi_core::domain::id::AgentContinuationRef>,
    /// Opaque public identity of this runtime as a future resume source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_source: Option<usagi_core::domain::id::AgentResumeSourceId>,
    /// Source used to create this replacement runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<usagi_core::domain::id::AgentResumeSourceId>,
    /// Replacement which consumed this exact source. This fence prevents a
    /// second operation from spawning the same provider conversation again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<usagi_core::domain::id::AgentRuntimeId>,
    /// Canonical caller intent used to reject operation-id reuse after restart.
    /// Legacy snapshots omit it and are therefore replayed only as a safe,
    /// non-spawnable failure.
    #[serde(default)]
    pub semantic_key: Option<String>,
    /// Safe public operation result. Private process output and credentials are
    /// deliberately absent from the durable form.
    #[serde(default)]
    pub outcome: DurableOperationOutcome,
    /// Secret-free provenance only. The minted credential value exists solely
    /// in the live Agent owner and spawn provision.
    #[serde(default)]
    pub credential_provenance: Option<CredentialProvenance>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialProvenance {
    DaemonMintedEphemeral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableOperationOutcome {
    #[default]
    Accepted,
    /// A resume replacement was spawned and durably fenced. Its source relation
    /// remains replayable even if a later daemon no longer owns the PTY.
    ResumeSucceeded,
    Completed,
    SpawnUnavailable,
    ExitUnavailable,
    OwnershipUnknown,
}

const GENERATION_SNAPSHOT_SCHEMA_VERSION: u32 = 3;
const RUNTIME_SNAPSHOT_SCHEMA_VERSION: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeStoreSnapshot {
    #[serde(default = "legacy_runtime_snapshot_version")]
    pub schema_version: u32,
    pub records: Vec<DurableRuntimeRecord>,
    /// Generation ownership is committed with runtime records as one atomic
    /// snapshot. It is empty only for schema v1/v2 migration input.
    #[serde(default)]
    pub generation: GenerationSnapshot,
}

const fn legacy_runtime_snapshot_version() -> u32 {
    1
}

impl Default for RuntimeStoreSnapshot {
    fn default() -> Self {
        Self {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: Vec::new(),
            generation: GenerationSnapshot::default(),
        }
    }
}

impl RuntimeStoreSnapshot {
    /// Reconcile a snapshot recovered after its daemon process died.
    ///
    /// The PTY master belongs to the dead daemon, so even a PID which still
    /// exists is not enough authority to attach, write to, kill, or replace a
    /// runtime.  Keep terminal records durable and make their lack of a
    /// provable live owner explicit instead.  A later, explicit recovery path
    /// may inspect the record, but startup itself never spawns a replacement.
    #[must_use]
    pub fn reconcile_after_daemon_restart(mut self) -> (Self, usize) {
        let mut interrupted = 0;
        for record in &mut self.records {
            if matches!(
                record.state,
                RuntimeState::Reserved | RuntimeState::Running | RuntimeState::ReconcileRequired(_)
            ) {
                record.state = RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown);
                if record.outcome != DurableOperationOutcome::ResumeSucceeded {
                    record.outcome = DurableOperationOutcome::OwnershipUnknown;
                }
                if let Some(provider) = &mut record.provider_resume {
                    provider.last_known_status = ProviderResumeStatus::Interrupted;
                    provider.last_known_phase = Some(ProviderResumePhase::Interrupted);
                }
                interrupted += 1;
            }
            if self.schema_version == 1 && record.semantic_key.is_none() {
                record.outcome = DurableOperationOutcome::OwnershipUnknown;
            }
        }
        let mut generations = BTreeMap::new();
        let mut terminals = Vec::new();
        for record in &self.records {
            let owner = record.runtime.terminal.daemon_generation;
            generations
                .entry(owner.as_str())
                .or_insert(GenerationRecord {
                    generation: owner,
                    endpoint: "retired-agent-runtime".to_owned(),
                    role: GenerationRole::Retired,
                    expected_build: usagi_core::infrastructure::ipc::BuildIdentity::default(),
                    build_verified: false,
                });
            terminals.push(TerminalOwnership {
                terminal: record.runtime.terminal.clone(),
                process: record.process.clone(),
                state: terminal_ownership_state(record.state),
            });
        }
        self.generation = GenerationSnapshot {
            current: None,
            records: generations.into_values().collect(),
            terminals,
        };
        self.schema_version = RUNTIME_SNAPSHOT_SCHEMA_VERSION;
        (self, interrupted)
    }

    pub fn validate_schema(&self) -> Result<(), RuntimeSnapshotError> {
        if matches!(
            self.schema_version,
            1 | 2 | 3 | RUNTIME_SNAPSHOT_SCHEMA_VERSION
        ) {
            Ok(())
        } else {
            Err(RuntimeSnapshotError::UnknownSchema(self.schema_version))
        }
    }

    /// Validates the atomic generation/runtime binding before restart is
    /// allowed to normalize either half. Legacy v1/v2 input has no binding and
    /// follows the conservative migration above.
    pub fn validate_ownership(&self) -> Result<(), RuntimeSnapshotError> {
        if self.schema_version < GENERATION_SNAPSHOT_SCHEMA_VERSION {
            return Ok(());
        }
        GenerationCoordinator::restore(self.generation.clone(), DEFAULT_GENERATION_LIMIT)
            .map_err(|_| RuntimeSnapshotError::Generation)?;
        if self.generation.terminals.len() != self.records.len()
            || self.records.iter().any(|record| {
                !self.generation.terminals.iter().any(|ownership| {
                    ownership.terminal.fences(&record.runtime.terminal)
                        && ownership.process == record.process
                        && ownership.state == terminal_ownership_state(record.state)
                })
            })
        {
            return Err(RuntimeSnapshotError::Generation);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSnapshotError {
    UnknownSchema(u32),
    DuplicateRuntime,
    DuplicateOperation,
    DuplicateResumeSource,
    ResumeRelation,
    ScopeMismatch,
    DispatchReconcile,
    Generation,
    OwnershipPersist,
}

pub trait RuntimeStore {
    #[allow(clippy::result_unit_err)] // Persistence detail is intentionally erased at the usecase port.
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), ()>;
}
/// Called exactly once by [`RuntimeCoordinator::launch`], before PTY spawn.
/// Ephemeral, adapter-owned spawn inputs. This value is never copied into a
/// [`DurableLaunchSnapshot`] or a runtime record.
pub struct SpawnProvision {
    environment: BTreeMap<usagi_core::domain::agent::EnvironmentVariableName, String>,
    daemon_environment: BTreeMap<usagi_core::domain::agent::EnvironmentVariableName, String>,
    arguments: Vec<String>,
}

/// The product-neutral inputs an adapter may use while materializing scoped
/// launch artifacts.  It deliberately contains no rendered product payload or
/// credential.  MCP wiring is opt-in: an adapter must not create it unless the
/// validated request asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionContext {
    pub scope: usagi_core::domain::agent::LaunchScope,
    pub inject_mcp: bool,
}

impl ProvisionContext {
    #[must_use]
    pub fn from_request(request: &LaunchRequest) -> Self {
        Self {
            scope: request.scope.clone(),
            inject_mcp: request
                .required_capabilities()
                .contains(&usagi_core::domain::agent::AgentCapability::McpWiring),
        }
    }
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
            daemon_environment: BTreeMap::new(),
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

    /// Rebuilds the complete Agent child environment from its three permitted
    /// live sources. Later sources win collisions: public terminal profile,
    /// adapter provision, then daemon-issued ephemeral provision.
    #[must_use]
    pub fn compose_environment(
        &self,
        public_profile: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        let mut environment = public_profile.clone();
        environment.extend(
            self.environment
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.clone())),
        );
        environment.extend(
            self.daemon_environment
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.clone())),
        );
        environment
    }

    /// Adds a daemon-issued ephemeral environment value after adapter
    /// provisioning. This is the highest-priority source: it replaces an
    /// adapter value with the same name, while adapter values replace public
    /// profile values when the process environment is composed.
    pub fn insert_daemon_environment(
        &mut self,
        name: usagi_core::domain::agent::EnvironmentVariableName,
        value: String,
    ) {
        self.daemon_environment.insert(name, value);
    }

    /// Appends adapter-private invocation arguments before the public durable
    /// plan. Provider-native IDs use this path so they never appear in the
    /// durable argv snapshot or diagnostics derived from it.
    pub fn append_sensitive_arguments(&mut self, arguments: impl IntoIterator<Item = String>) {
        self.arguments.extend(arguments);
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
    pub provider_resume: Option<ProviderResumeRef>,
}
pub trait PtySpawner {
    fn spawn(
        &mut self,
        launch: &DurableLaunchSnapshot,
        provision: &SpawnProvision,
        terminal: &TerminalRef,
    ) -> Result<ProcessIdentity, SpawnFailure>;

    /// Terminates and reaps the exact child owned by `terminal` after an
    /// admission commit failure. Implementations which cannot prove both
    /// effects fail closed and leave the runtime reconcile-required.
    fn terminate_reap(&mut self, _terminal: &TerminalRef) -> Result<(), TerminateReapError> {
        Err(TerminateReapError)
    }
}

/// The exact child could not be both terminated and reaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminateReapError;
pub trait OutputJournal {
    #[allow(clippy::result_unit_err)] // Journal detail is intentionally erased at the usecase port.
    fn append(&mut self, output: &Output) -> Result<(), ()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    Adapter(AdapterError),
    RuntimeAlreadyExists,
    ScopeMismatch,
    ProviderResumeMismatch,
    ConcurrencyExhausted,
    Terminal(RegistryError),
    Store,
    Journal,
    SpawnFailed,
    ReconcileRequired(ReconcileState),
    UnknownRuntime,
    TerminalGenerationMismatch,
    Generation(GenerationError),
}

/// The daemon owns this coordinator. Callers persist each mutation as one
/// snapshot and must reconcile, rather than replace, unknown external effects.
#[derive(Debug)]
pub struct RuntimeCoordinator {
    limit: usize,
    records: BTreeMap<String, DurableRuntimeRecord>,
    terminals: TerminalRegistry,
    generation: GenerationCoordinator,
}

impl RuntimeCoordinator {
    #[must_use]
    pub fn new(limit: usize, journal_limit: usize, input_cache_limit: usize) -> Self {
        Self {
            limit,
            records: BTreeMap::new(),
            terminals: TerminalRegistry::new(journal_limit, input_cache_limit),
            generation: GenerationCoordinator::new(DEFAULT_GENERATION_LIMIT),
        }
    }

    pub fn hydrate(
        snapshot: RuntimeStoreSnapshot,
        limit: usize,
        journal_limit: usize,
        input_cache_limit: usize,
    ) -> Result<Self, RuntimeSnapshotError> {
        snapshot.validate_ownership()?;
        let generation =
            GenerationCoordinator::restore(snapshot.generation.clone(), DEFAULT_GENERATION_LIMIT)
                .map_err(|_| RuntimeSnapshotError::Generation)?;
        let records = hydrated_records(snapshot)?;
        Ok(Self {
            limit,
            records,
            terminals: TerminalRegistry::new(journal_limit, input_cache_limit),
            generation,
        })
    }

    /// Claims production ownership for this daemon generation. The caller
    /// persists the returned snapshot before exposing any admission path.
    pub fn activate_generation(
        &mut self,
        generation: usagi_core::domain::id::DaemonGeneration,
    ) -> Result<(), RuntimeSnapshotError> {
        self.generation
            .register_standby(generation, "in-process-agent-runtime".to_owned())
            .and_then(|()| self.generation.activate_initial(generation))
            .map_err(|_| RuntimeSnapshotError::Generation)
    }

    #[must_use]
    pub fn active_generation(&self) -> Option<usagi_core::domain::id::DaemonGeneration> {
        self.generation.current()
    }

    pub fn launch(
        &mut self,
        request: &LaunchRequest,
        runtime: AgentRuntimeRef,
        operation: CompletionFence,
        geometry: Geometry,
        adapter: &mut dyn AgentAdapter,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
        mcp_credential: Option<String>,
    ) -> Result<(), RuntimeError> {
        self.launch_with_semantic(
            request,
            runtime,
            operation,
            geometry,
            adapter,
            store,
            spawner,
            mcp_credential,
            "internal-launch".to_owned(),
        )
    }

    pub fn launch_with_semantic(
        &mut self,
        request: &LaunchRequest,
        runtime: AgentRuntimeRef,
        operation: CompletionFence,
        geometry: Geometry,
        adapter: &mut dyn AgentAdapter,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
        mcp_credential: Option<String>,
        semantic_key: String,
    ) -> Result<(), RuntimeError> {
        self.launch_with_semantic_superseding(
            request,
            runtime,
            operation,
            geometry,
            adapter,
            store,
            spawner,
            mcp_credential,
            semantic_key,
            &[],
        )
    }

    /// Reserves a replacement runtime while superseding interrupted runtime
    /// incarnations in the same durable snapshot. Exited/reclaimed sources stay
    /// as history; only `identity_unknown` sources release occupied capacity.
    pub fn resume_with_semantic(
        &mut self,
        request: &LaunchRequest,
        runtime: AgentRuntimeRef,
        operation: CompletionFence,
        geometry: Geometry,
        adapter: &mut dyn AgentAdapter,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
        mcp_credential: Option<String>,
        semantic_key: String,
        superseded: &[AgentRuntimeRef],
    ) -> Result<(), RuntimeError> {
        self.launch_with_semantic_superseding(
            request,
            runtime,
            operation,
            geometry,
            adapter,
            store,
            spawner,
            mcp_credential,
            semantic_key,
            superseded,
        )
    }

    #[allow(clippy::too_many_lines)] // Keep the reservation, source transition, and spawn compensation in one transactional flow.
    fn launch_with_semantic_superseding(
        &mut self,
        request: &LaunchRequest,
        runtime: AgentRuntimeRef,
        operation: CompletionFence,
        geometry: Geometry,
        adapter: &mut dyn AgentAdapter,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
        mcp_credential: Option<String>,
        semantic_key: String,
        superseded: &[AgentRuntimeRef],
    ) -> Result<(), RuntimeError> {
        self.validate_scope(&runtime, &operation)?;
        if self.generation.current().is_none() {
            self.generation
                .register_standby(
                    operation.owner_daemon_generation,
                    "in-process-agent-runtime".to_owned(),
                )
                .and_then(|()| {
                    self.generation
                        .activate_initial(operation.owner_daemon_generation)
                })
                .map_err(RuntimeError::Generation)?;
        }
        self.generation
            .require_active(operation.owner_daemon_generation)
            .map_err(RuntimeError::Generation)?;
        let key = runtime.agent_runtime_id.as_str();
        if self.records.contains_key(&key) {
            return Err(RuntimeError::RuntimeAlreadyExists);
        }
        if superseded.len() > 1 {
            return Err(RuntimeError::ProviderResumeMismatch);
        }
        let mut superseded_keys = BTreeSet::new();
        let mut continuation = None;
        let mut resumed_from = None;
        for source in superseded {
            let record = self.record(source)?;
            if !matches!(
                record.state,
                RuntimeState::Exited
                    | RuntimeState::Reclaimed
                    | RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown)
            ) {
                return Err(RuntimeError::ProviderResumeMismatch);
            }
            if record.superseded_by.is_some() {
                return Err(RuntimeError::ProviderResumeMismatch);
            }
            continuation = record.continuation;
            resumed_from = record.resume_source;
            if continuation.is_none() || resumed_from.is_none() {
                return Err(RuntimeError::ProviderResumeMismatch);
            }
            superseded_keys.insert(source.agent_runtime_id.as_str());
        }
        let continuation =
            continuation.unwrap_or_else(usagi_core::domain::id::AgentContinuationRef::new);
        let resume_source = usagi_core::domain::id::AgentResumeSourceId::new();
        let released_slots = superseded_keys
            .iter()
            .filter(|source| {
                self.records.get(*source).is_some_and(|record| {
                    record.state == RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown)
                })
            })
            .count();
        if self.occupied_slots().saturating_sub(released_slots) >= self.limit {
            return Err(RuntimeError::ConcurrencyExhausted);
        }
        let mut resolved = adapter.resolve(request).map_err(RuntimeError::Adapter)?;
        let credential_provenance = mcp_credential
            .as_ref()
            .map(|_| CredentialProvenance::DaemonMintedEphemeral);
        if let Some(credential) = mcp_credential {
            let name = usagi_core::domain::agent::EnvironmentVariableName::new(
                "USAGI_MCP_CALLER_CREDENTIAL",
            )
            .expect("literal environment variable name is valid");
            resolved
                .provision
                .insert_daemon_environment(name, credential);
        }
        let launch = resolved.snapshot;
        let provider_resume = resolved.provider_resume;
        let mut durable_request = request.clone();
        durable_request.provider_resume = None;
        if launch.request != durable_request
            || launch.plan.profile_id != request.profile_id
            || launch.plan.profile_revision == 0
        {
            return Err(RuntimeError::ScopeMismatch);
        }
        for source in superseded_keys {
            let record = self
                .records
                .get_mut(&source)
                .expect("validated resume source remains present");
            record.superseded_by = Some(runtime.agent_runtime_id);
            if record.state == RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown) {
                record.state = RuntimeState::Reclaimed;
                if let Some(provider) = &mut record.provider_resume {
                    provider.last_known_status = ProviderResumeStatus::Exited;
                    provider.last_known_phase = Some(ProviderResumePhase::Ended);
                }
            }
        }
        self.records.insert(
            key.clone(),
            DurableRuntimeRecord {
                runtime: runtime.clone(),
                operation,
                launch,
                state: RuntimeState::Reserved,
                process: None,
                provider_resume,
                continuation: Some(continuation),
                resume_source: Some(resume_source),
                resumed_from,
                superseded_by: None,
                semantic_key: Some(semantic_key),
                outcome: DurableOperationOutcome::Accepted,
                credential_provenance,
            },
        );
        self.generation
            .reserve_terminal(runtime.terminal.clone())
            .map_err(|error| {
                debug_assert_eq!(error, GenerationError::TerminalOwnedElsewhere);
                RuntimeError::Terminal(RegistryError::StaleTarget)
            })?;
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
                self.generation
                    .record_spawn(&runtime.terminal, process.clone())
                    .map_err(RuntimeError::Generation)?;
                let record = self.records.get_mut(&key).expect("inserted");
                record.process = Some(process);
                record.state = RuntimeState::Running;
                if record.resumed_from.is_some() {
                    record.outcome = DurableOperationOutcome::ResumeSucceeded;
                }
                if self.persist(store).is_err() {
                    return Err(self.compensate_spawn(&runtime, store, spawner));
                }
                Ok(())
            }
            Err(SpawnFailure::Definite) => {
                self.generation
                    .resolve_orphan(&runtime.terminal, ProcessObservation::Gone, false)
                    .map_err(RuntimeError::Generation)?;
                let record = self.records.get_mut(&key).expect("inserted");
                record.state = RuntimeState::SpawnFailed;
                record.outcome = DurableOperationOutcome::SpawnUnavailable;
                self.persist(store)?;
                Err(RuntimeError::SpawnFailed)
            }
            Err(SpawnFailure::Ambiguous) => {
                self.records.get_mut(&key).expect("inserted").state =
                    RuntimeState::ReconcileRequired(ReconcileState::SpawnAmbiguous);
                self.records.get_mut(&key).expect("inserted").outcome =
                    DurableOperationOutcome::OwnershipUnknown;
                self.persist(store)?;
                Err(RuntimeError::ReconcileRequired(
                    ReconcileState::SpawnAmbiguous,
                ))
            }
        }
    }

    /// Compensates a failure after spawn but before the whole admission has
    /// committed. A successful return is intentionally impossible: even when
    /// termination succeeds the original request remains a durable failure.
    pub fn compensate_after_spawn(
        &mut self,
        runtime: &AgentRuntimeRef,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
    ) -> RuntimeError {
        self.compensate_spawn(runtime, store, spawner)
    }

    fn compensate_spawn(
        &mut self,
        runtime: &AgentRuntimeRef,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
    ) -> RuntimeError {
        let terminated = spawner.terminate_reap(&runtime.terminal).is_ok();
        if terminated {
            let _ = self.generation.resolve_orphan(
                &runtime.terminal,
                ProcessObservation::Unknown,
                true,
            );
        }
        let record = self
            .record_mut(runtime)
            .expect("spawn compensation targets the reserved runtime");
        if terminated {
            record.state = RuntimeState::SpawnFailed;
            record.outcome = DurableOperationOutcome::SpawnUnavailable;
            record.process = None;
        } else {
            record.state = RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning);
            record.outcome = DurableOperationOutcome::OwnershipUnknown;
        }
        if self.persist(store).is_err() {
            return RuntimeError::ReconcileRequired(if terminated {
                ReconcileState::PersistAfterSpawn
            } else {
                ReconcileState::OrphanRunning
            });
        }
        if terminated {
            RuntimeError::SpawnFailed
        } else {
            RuntimeError::ReconcileRequired(ReconcileState::OrphanRunning)
        }
    }

    /// Journal output before it becomes available to terminal replay clients.
    pub fn append_output(
        &mut self,
        runtime: &AgentRuntimeRef,
        data: Vec<u8>,
        journal: &mut dyn OutputJournal,
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
        journal
            .append(&output)
            .map_err(|()| RuntimeError::Journal)?;
        self.terminals
            .append_output(&runtime.terminal, output.data.clone())
            .map_err(RuntimeError::Terminal)
    }

    /// Caller drains all output before this verified exit is committed.
    pub fn exit(
        &mut self,
        runtime: &AgentRuntimeRef,
        status: i32,
        store: &mut dyn RuntimeStore,
    ) -> Result<(), RuntimeError> {
        self.running(runtime)?;
        self.terminals
            .exited(&runtime.terminal, status)
            .map_err(RuntimeError::Terminal)?;
        self.record_mut(runtime)?.state = RuntimeState::Exited;
        self.record_mut(runtime)?.outcome = if status == 0 {
            DurableOperationOutcome::Completed
        } else {
            DurableOperationOutcome::ExitUnavailable
        };
        if let Some(provider) = &mut self.record_mut(runtime)?.provider_resume {
            provider.last_known_status = ProviderResumeStatus::Exited;
            provider.last_known_phase = Some(ProviderResumePhase::Ended);
        }
        self.generation
            .resolve_orphan(&runtime.terminal, ProcessObservation::Unknown, true)
            .map_err(RuntimeError::Generation)?;
        if self.persist(store).is_err() {
            self.record_mut(runtime)?.state =
                RuntimeState::ReconcileRequired(ReconcileState::PersistAfterExit);
            let _ = self.generation.resolve_orphan(
                &runtime.terminal,
                ProcessObservation::Unknown,
                false,
            );
            return Err(RuntimeError::ReconcileRequired(
                ReconcileState::PersistAfterExit,
            ));
        }
        Ok(())
    }

    /// Reconciliation performs no replacement spawn. A slot is released only
    /// on a verified disappearance (or [`Self::exit`]).
    pub fn reconcile(
        &mut self,
        runtime: &AgentRuntimeRef,
        observation: ProcessObservation,
        store: &mut dyn RuntimeStore,
    ) -> Result<(), RuntimeError> {
        let identity_unknown = matches!(observation, ProcessObservation::Unknown);
        let next_state = match &observation {
            ProcessObservation::Gone => RuntimeState::Reclaimed,
            ProcessObservation::VerifiedAlive(actual)
                if self.record(runtime)?.process.as_ref() == Some(actual) =>
            {
                RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning)
            }
            _ => RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown),
        };
        if let Err(error) = self
            .generation
            .resolve_orphan(&runtime.terminal, observation, false)
            && !(identity_unknown && error == GenerationError::TerminalUnavailable)
        {
            return Err(RuntimeError::Generation(error));
        }
        let record = self.record_mut(runtime)?;
        record.state = next_state;
        if let Some(provider) = &mut record.provider_resume {
            let exited = matches!(record.state, RuntimeState::Exited | RuntimeState::Reclaimed);
            provider.last_known_status = if exited {
                ProviderResumeStatus::Exited
            } else {
                ProviderResumeStatus::Interrupted
            };
            provider.last_known_phase = Some(if exited {
                ProviderResumePhase::Ended
            } else {
                ProviderResumePhase::Interrupted
            });
        }
        self.persist(store)
    }

    pub fn terminal_snapshot(&self, runtime: &AgentRuntimeRef) -> Result<Snapshot, RuntimeError> {
        self.record(runtime)?;
        self.terminals
            .snapshot(&runtime.terminal)
            .map_err(|_| RuntimeError::TerminalGenerationMismatch)
    }

    /// Atomically snapshots the runtime terminal and assigns a connection-owned
    /// subscription.  Only a running, fenced runtime is attachable.
    pub fn attach(
        &mut self,
        runtime: &AgentRuntimeRef,
        connection: ConnectionId,
    ) -> Result<Attached, RuntimeError> {
        self.running(runtime)?;
        self.terminals
            .attach(&runtime.terminal, connection)
            .map_err(RuntimeError::Terminal)
    }

    /// Removes only the named attachment; the daemon-owned Agent process and its
    /// PTY intentionally stay alive.
    pub fn detach(
        &mut self,
        runtime: &AgentRuntimeRef,
        subscription: u64,
        connection: ConnectionId,
    ) -> Result<(), RuntimeError> {
        self.record(runtime)?;
        self.terminals
            .detach(&runtime.terminal, subscription, connection)
            .map_err(RuntimeError::Terminal)
    }

    /// Updates the fenced runtime terminal geometry.
    pub fn resize(
        &mut self,
        runtime: &AgentRuntimeRef,
        geometry: Geometry,
        writer: &mut dyn PtyWriter,
    ) -> Result<Snapshot, RuntimeError> {
        self.running(runtime)?;
        self.terminals
            .resize(&runtime.terminal, geometry, writer)
            .map_err(RuntimeError::Terminal)
    }

    /// Writes fenced, de-duplicated terminal input to the daemon-owned PTY.
    pub fn input(
        &mut self,
        runtime: &AgentRuntimeRef,
        input: InputRequest,
        bytes: &[u8],
        writer: &mut dyn PtyWriter,
    ) -> Result<InputAck, RuntimeError> {
        self.running(runtime)?;
        self.terminals
            .write_input(&runtime.terminal, input, bytes, writer)
            .map_err(RuntimeError::Terminal)
    }

    /// Replays retained output after `offset` for a reconnecting attachment.
    pub fn replay_from(
        &self,
        runtime: &AgentRuntimeRef,
        offset: u64,
    ) -> Result<Vec<Output>, RuntimeError> {
        self.record(runtime)?;
        self.terminals
            .replay_from(&runtime.terminal, offset)
            .map_err(RuntimeError::Terminal)
    }

    /// Drops only this connection's subscriptions across every runtime terminal.
    /// It never kills an Agent process, its PTY, or the completion worker.
    pub fn disconnect(&mut self, connection: ConnectionId) {
        self.terminals.disconnect(connection);
    }

    /// Resolves the fenced runtime that currently owns `terminal`.  IPC terminal
    /// requests address a terminal only by its `TerminalRef`; this maps that ref
    /// back to the owning runtime without a name or PID fallback.
    #[must_use]
    pub fn runtime_for_terminal(&self, terminal: &TerminalRef) -> Option<AgentRuntimeRef> {
        if !self.generation.owns_terminal(terminal) {
            return None;
        }
        self.records
            .values()
            .find(|record| record.runtime.terminal.fences(terminal))
            .map(|record| record.runtime.clone())
    }
    /// Lists only Agent runtimes in the exact requested durable scope. Each
    /// entry is tagged `Agent` and marked `live` only while the current daemon
    /// generation still owns a running PTY, so a restoring client attaches to
    /// running Agents and never to exited, reclaimed, or reconcile-required
    /// records.
    #[must_use]
    pub fn inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry> {
        use usagi_core::domain::terminal_launch::{TerminalInventoryEntry, TerminalKind};
        self.records
            .values()
            .filter(|record| {
                record.runtime.terminal.workspace_id == scope.workspace_id
                    && record.runtime.terminal.session_id == scope.session_id
                    && record.runtime.terminal.worktree_id == scope.worktree_id
            })
            .map(|record| TerminalInventoryEntry {
                terminal: record.runtime.terminal.clone(),
                kind: TerminalKind::Agent,
                live: matches!(record.state, RuntimeState::Running),
            })
            .collect()
    }
    /// Returns the immutable record only when the complete runtime reference
    /// fences it.  This exposes no ephemeral provision or terminal output.
    pub fn record_for(
        &self,
        runtime: &AgentRuntimeRef,
    ) -> Result<&DurableRuntimeRecord, RuntimeError> {
        self.record(runtime)
    }
    /// Records an ID obtained from a documented provider-owned structured
    /// channel. The complete runtime and launch scope must still fence the
    /// record; callers cannot repair or infer legacy metadata by name/path.
    pub fn record_provider_resume(
        &mut self,
        runtime: &AgentRuntimeRef,
        provider_resume: ProviderResumeRef,
        store: &mut dyn RuntimeStore,
    ) -> Result<(), RuntimeError> {
        let record = self.record_mut(runtime)?;
        if record.state != RuntimeState::Running
            || record.launch.request.scope != provider_resume.scope
            || record.launch.plan.profile_revision != provider_resume.adapter_revision
            || record
                .provider_resume
                .as_ref()
                .is_some_and(|existing| existing != &provider_resume)
        {
            return Err(RuntimeError::ProviderResumeMismatch);
        }
        record.provider_resume = Some(provider_resume);
        self.persist(store)
    }
    #[must_use]
    pub fn snapshot(&self) -> RuntimeStoreSnapshot {
        RuntimeStoreSnapshot {
            schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
            records: self.records.values().cloned().collect(),
            generation: self.generation.snapshot(),
        }
    }

    /// Accepts an Agent completion only while its exact generation and
    /// terminal ownership are still live. Late outcomes are effect-free.
    pub fn require_outcome_owner(&self, runtime: &AgentRuntimeRef) -> Result<(), RuntimeError> {
        self.record(runtime)?;
        self.generation
            .require_terminal(&runtime.terminal)
            .map_err(RuntimeError::Generation)
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
    fn persist(&self, store: &mut dyn RuntimeStore) -> Result<(), RuntimeError> {
        store
            .save(self.snapshot())
            .map_err(|()| RuntimeError::Store)
    }
    fn validate_scope(
        &self,
        runtime: &AgentRuntimeRef,
        operation: &CompletionFence,
    ) -> Result<(), RuntimeError> {
        (runtime.terminal.session_id == runtime.session_id
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
        match self.record(runtime)?.state {
            RuntimeState::Running => self
                .generation
                .require_terminal(&runtime.terminal)
                .map_err(RuntimeError::Generation),
            RuntimeState::Exited | RuntimeState::Reclaimed => {
                Err(RuntimeError::Terminal(RegistryError::Exited))
            }
            _ => Err(RuntimeError::ReconcileRequired(
                ReconcileState::IdentityUnknown,
            )),
        }
    }
}

fn terminal_ownership_state(state: RuntimeState) -> TerminalState {
    match state {
        RuntimeState::Running => TerminalState::Available,
        RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning) => {
            TerminalState::OrphanRunning
        }
        RuntimeState::Reserved
        | RuntimeState::ReconcileRequired(
            ReconcileState::SpawnAmbiguous
            | ReconcileState::PersistAfterSpawn
            | ReconcileState::PersistAfterExit
            | ReconcileState::IdentityUnknown,
        ) => TerminalState::IdentityUnknown,
        RuntimeState::Exited => TerminalState::Terminated,
        RuntimeState::SpawnFailed | RuntimeState::Reclaimed => TerminalState::Lost,
    }
}

#[inline(never)]
fn hydrated_records(
    snapshot: RuntimeStoreSnapshot,
) -> Result<BTreeMap<String, DurableRuntimeRecord>, RuntimeSnapshotError> {
    snapshot.validate_schema()?;
    let mut records = BTreeMap::new();
    let mut operations = std::collections::BTreeSet::new();
    let mut resume_sources = std::collections::BTreeSet::new();
    for record in snapshot.records {
        if record.runtime.terminal.session_id != record.runtime.session_id
            || record.runtime.session_id != record.operation.session_id
            || record.runtime.terminal.workspace_id != record.operation.workspace_id
            || record.runtime.terminal.daemon_generation != record.operation.owner_daemon_generation
        {
            return Err(RuntimeSnapshotError::ScopeMismatch);
        }
        if !operations.insert(record.operation.operation_id) {
            return Err(RuntimeSnapshotError::DuplicateOperation);
        }
        if record
            .resume_source
            .is_some_and(|source| !resume_sources.insert(source))
        {
            return Err(RuntimeSnapshotError::DuplicateResumeSource);
        }
        if records
            .insert(record.runtime.agent_runtime_id.as_str(), record)
            .is_some()
        {
            return Err(RuntimeSnapshotError::DuplicateRuntime);
        }
    }
    for record in records.values() {
        if let Some(source_id) = record.resumed_from {
            let Some(source) = records
                .values()
                .find(|candidate| candidate.resume_source == Some(source_id))
            else {
                return Err(RuntimeSnapshotError::ResumeRelation);
            };
            if source.superseded_by != Some(record.runtime.agent_runtime_id)
                || source.continuation != record.continuation
            {
                return Err(RuntimeSnapshotError::ResumeRelation);
            }
        }
        if let Some(replacement_id) = record.superseded_by {
            let Some(replacement) = records
                .values()
                .find(|candidate| candidate.runtime.agent_runtime_id == replacement_id)
            else {
                return Err(RuntimeSnapshotError::ResumeRelation);
            };
            if replacement.resumed_from != record.resume_source
                || replacement.continuation != record.continuation
            {
                return Err(RuntimeSnapshotError::ResumeRelation);
            }
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, path::PathBuf};
    use usagi_core::domain::{
        agent::{
            AgentProfileId, LaunchMode, LaunchPlan, LaunchScope, ProviderCaptureProvenance,
            ProviderKind, ProviderSessionId,
        },
        id::{
            AgentRuntimeId, ClientId, DaemonGeneration, OperationId, RequestId, SessionId,
            TerminalId, WorkspaceId, WorktreeId,
        },
    };
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
        let mut missing_source_backref = lineage_source.clone();
        missing_source_backref.superseded_by = None;
        assert_eq!(
            hydrated_records(RuntimeStoreSnapshot {
                schema_version: RUNTIME_SNAPSHOT_SCHEMA_VERSION,
                records: vec![missing_source_backref, replacement.clone()],
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
            .unwrap_err(),
            RuntimeSnapshotError::ResumeRelation
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
            .unwrap_err(),
            RuntimeSnapshotError::ResumeRelation
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
                },
                b"go\n",
                &mut writer,
            )
            .unwrap(),
            InputAck::Written
        );
        assert_eq!(writer.0, b"go\n");
        c.detach(&runtime, attached.subscription, connection)
            .unwrap();
        let reattached = c.attach(&runtime, connection).unwrap();
        assert_eq!(reattached.snapshot.replay, b"boot\n");
        assert_eq!(c.replay_from(&runtime, 0).unwrap()[0].data, b"boot\n");
        let mut resize_writer = Writer::default();
        assert_eq!(
            c.resize(
                &runtime,
                Geometry {
                    cols: 120,
                    rows: 40
                },
                &mut resize_writer,
            )
            .unwrap()
            .geometry
            .cols,
            120
        );
        c.disconnect(connection);
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
            c.detach(&stale, 1, ConnectionId::new()),
            Err(RuntimeError::UnknownRuntime)
        );
        assert_eq!(c.replay_from(&stale, 0), Err(RuntimeError::UnknownRuntime));
        assert_eq!(
            c.input(
                &stale,
                InputRequest {
                    subscription: 1,
                    connection: ConnectionId::new(),
                    client: ClientId::new(),
                    request: RequestId::new(),
                    input_seq: 0,
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
}
