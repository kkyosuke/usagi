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
    id::{
        AgentRuntimeRef, ClientId, CompletionFence, ConnectionId, OperationId, SessionId,
        TerminalRef, WorkspaceId,
    },
    terminal_launch::TerminalKind,
    terminal_retention::{AdmissionRejection, EvictionReason, FinalLookup, RetainedFinal},
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
    metrics::AgentConcurrencyGauge,
    terminal::{
        Attached, Geometry, InputAck, InputRequest, Output, PtyWriter, RegistryError, Snapshot,
        TerminalRegistry,
    },
    terminal_retention_ipc::{RESTORED_FINAL_BYTES, SharedTerminalRetention},
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
    /// in the live Agent owner and claimed MCP child process.
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
                        && terminal_ownership_matches(record.state, ownership.state.clone())
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
/// A non-durable instruction to wrap the spawned child in an OS sandbox
/// launcher.  When present, the composition-root spawner runs `program` (the
/// `usagi` binary) with `prefix` (`claude-sandbox --mode … --writable-root … --`)
/// in front of the product program, so Claude only ever runs confined.  Its host
/// paths are deliberately kept out of the [`DurableLaunchSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxLauncher {
    /// The launcher executable actually spawned (the `usagi` binary).
    pub program: String,
    /// Arguments placed before the product program, ending in `--`.
    pub prefix: Vec<String>,
}

/// Ephemeral, adapter-owned spawn inputs. This value is never copied into a
/// [`DurableLaunchSnapshot`] or a runtime record.
pub struct SpawnProvision {
    environment: BTreeMap<usagi_core::domain::agent::EnvironmentVariableName, String>,
    daemon_environment: BTreeMap<usagi_core::domain::agent::EnvironmentVariableName, String>,
    arguments: Vec<String>,
    sandbox_launcher: Option<SandboxLauncher>,
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
            sandbox_launcher: None,
        }
    }

    /// Wraps the spawned child in an OS sandbox launcher. The composition root
    /// sets this for Claude so the product only ever runs confined; it stays
    /// ephemeral and never reaches the durable snapshot.
    pub fn set_sandbox_launcher(&mut self, launcher: SandboxLauncher) {
        self.sandbox_launcher = Some(launcher);
    }

    /// The OS sandbox launcher wrapping this spawn, if any.
    #[must_use]
    pub fn sandbox_launcher(&self) -> Option<&SandboxLauncher> {
        self.sandbox_launcher.as_ref()
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
        // Reserved authentication material is never part of an Agent spawn.
        // This also defeats stale public profile or adapter configuration that
        // attempts to recreate the historical ambient bearer channel.
        environment.remove("USAGI_MCP_CALLER_CREDENTIAL");
        environment
    }

    /// Adds a daemon-issued ephemeral environment value after adapter
    /// provisioning. Caller credentials deliberately do not use this channel;
    /// it remains for non-secret launcher policy selected by the daemon.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderResumeWrite {
    /// Adds metadata only when it is absent or already byte-for-byte equal.
    Attach,
    /// Replaces the current conversation after a documented `SessionStart`.
    Replace,
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
    /// The aggregate retention budget cannot reserve this launch's worst-case
    /// final, so admission is refused before any PTY is spawned (#526).
    RetentionExhausted(AdmissionRejection),
    /// The runtime existed, and its final was collected by aggregate retention.
    /// It is never answered as unknown or with another runtime's history.
    FinalEvicted(EvictionReason),
}

/// The daemon owns this coordinator. Callers persist each mutation as one
/// snapshot and must reconcile, rather than replace, unknown external effects.
#[derive(Debug)]
pub struct RuntimeCoordinator {
    limit: usize,
    records: BTreeMap<String, DurableRuntimeRecord>,
    terminals: TerminalRegistry,
    generation: GenerationCoordinator,
    retention: SharedTerminalRetention,
    /// Where this coordinator publishes the concurrency level it admits from, so
    /// an observer never has to take the owner's lock to read it. Unbound by
    /// default: a coordinator nobody observes publishes into its own gauge.
    concurrency: AgentConcurrencyGauge,
}

impl RuntimeCoordinator {
    #[must_use]
    pub fn new(limit: usize, journal_limit: usize, input_cache_limit: usize) -> Self {
        Self::with_retention(
            limit,
            journal_limit,
            input_cache_limit,
            SharedTerminalRetention::new(),
        )
    }

    /// Builds a coordinator bound to the daemon-wide retention authority so
    /// Agent finals share one aggregate budget with generic terminals (#526).
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
            generation: GenerationCoordinator::new(DEFAULT_GENERATION_LIMIT),
            retention,
            concurrency: AgentConcurrencyGauge::default(),
        }
    }

    pub fn hydrate(
        snapshot: RuntimeStoreSnapshot,
        limit: usize,
        journal_limit: usize,
        input_cache_limit: usize,
    ) -> Result<Self, RuntimeSnapshotError> {
        Self::hydrate_with_retention(
            snapshot,
            limit,
            journal_limit,
            input_cache_limit,
            SharedTerminalRetention::new(),
        )
    }

    /// Restores durable records and re-imports their finals into the shared
    /// retention accounting, which is derived state a restart rebuilds. Records
    /// that predate the aggregate budget are migrated here and become ordinary
    /// collection candidates.
    pub fn hydrate_with_retention(
        snapshot: RuntimeStoreSnapshot,
        limit: usize,
        journal_limit: usize,
        input_cache_limit: usize,
        retention: SharedTerminalRetention,
    ) -> Result<Self, RuntimeSnapshotError> {
        snapshot.validate_ownership()?;
        let generation =
            GenerationCoordinator::restore(snapshot.generation.clone(), DEFAULT_GENERATION_LIMIT)
                .map_err(|_| RuntimeSnapshotError::Generation)?;
        let records = hydrated_records(snapshot)?;
        let restored_at = retention.now();
        for record in records.values() {
            if matches!(record.state, RuntimeState::Exited | RuntimeState::Reclaimed) {
                let mut final_record = RetainedFinal::new(
                    record.runtime.terminal.clone(),
                    TerminalKind::Agent,
                    RESTORED_FINAL_BYTES,
                    restored_at,
                );
                final_record.superseded = record.superseded_by.is_some();
                retention.import_existing(final_record);
            }
        }
        Ok(Self {
            limit,
            records,
            terminals: TerminalRegistry::new(journal_limit, input_cache_limit),
            generation,
            retention,
            concurrency: AgentConcurrencyGauge::default(),
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

    /// Reserves a replacement runtime while superseding a non-live runtime
    /// incarnation in the same durable snapshot. The source becomes bounded
    /// history, and states that still occupy capacity release their slot.
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

    /// Releases the pre-admission retention reservation on every failure: a
    /// launch that never reaches `Running` will never commit a final.
    #[allow(clippy::too_many_arguments)]
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
        let terminal = runtime.terminal.clone();
        let outcome = self.admit_with_semantic_superseding(
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
        );
        if outcome.is_err() {
            self.retention.release(&terminal);
        }
        outcome
    }

    #[allow(clippy::too_many_lines)] // Keep the reservation, source transition, and spawn compensation in one transactional flow.
    fn admit_with_semantic_superseding(
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
                    | RuntimeState::Interrupted
                    | RuntimeState::Sleeping
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
        // Reserve the worst-case final this runtime will leave behind before
        // anything is spawned. An exhausted aggregate budget refuses admission
        // here instead of dropping somebody else's protected final later.
        self.retention
            .reserve(&runtime.terminal)
            .map_err(RuntimeError::RetentionExhausted)?;
        let resolved = adapter.resolve(request).map_err(RuntimeError::Adapter)?;
        let credential_provenance = mcp_credential
            .as_ref()
            .map(|_| CredentialProvenance::DaemonMintedEphemeral);
        // The bearer stays daemon-owned. The canonical MCP child claims it over
        // its OS-authenticated IPC connection after the Agent process exists.
        drop(mcp_credential);
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
            // A replaced source is the least valuable history in its lineage:
            // it keeps its minimum TTL but is collected before anything else.
            let source_terminal = record.runtime.terminal.clone();
            let reclaimed = matches!(
                record.state,
                RuntimeState::Interrupted
                    | RuntimeState::Sleeping
                    | RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown)
            );
            if reclaimed {
                record.state = RuntimeState::Reclaimed;
                if let Some(provider) = &mut record.provider_resume {
                    provider.last_known_status = ProviderResumeStatus::Exited;
                    provider.last_known_phase = Some(ProviderResumePhase::Ended);
                }
            }
            if reclaimed {
                // Exact resume is the explicit acknowledgement that closes this
                // already non-live incarnation. Keep generation ownership in
                // lock-step with the Reclaimed runtime record so later bounded
                // GC can forget both halves atomically.
                self.generation
                    .resolve_orphan(&source_terminal, ProcessObservation::Unknown, true)
                    .map_err(RuntimeError::Generation)?;
                if record.process.is_none() {
                    self.generation
                        .forget_resolved_process(&source_terminal)
                        .map_err(RuntimeError::Generation)?;
                }
            }
            if !self.retention.mark_superseded(&source_terminal)
                && matches!(
                    self.retention.lookup(&source_terminal),
                    FinalLookup::Unknown
                )
            {
                // A restart-interrupted source has no in-memory final ledger
                // entry. Once a successful resume reclaims it, import the
                // bounded tombstone so the durable source can age out exactly
                // like a normally exited source instead of living forever. An
                // existing eviction marker remains authoritative and is never
                // resurrected by resume.
                let mut final_record = RetainedFinal::new(
                    source_terminal.clone(),
                    TerminalKind::Agent,
                    RESTORED_FINAL_BYTES,
                    self.retention.now(),
                );
                final_record.superseded = true;
                self.retention.import_existing(final_record);
            }
            self.retention.set_pinned(&source_terminal, false);
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
            let _ = self.generation.forget_resolved_process(&runtime.terminal);
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
        self.append_output_with_replies(runtime, data, journal)
            .map(|(output, _)| output)
    }

    /// Journals PTY output and returns terminal-protocol replies for the
    /// daemon-owned PTY endpoint without recording them as client input.
    pub fn append_output_with_replies(
        &mut self,
        runtime: &AgentRuntimeRef,
        data: Vec<u8>,
        journal: &mut dyn OutputJournal,
    ) -> Result<(Output, Vec<u8>), RuntimeError> {
        self.running(runtime)?;
        // Offsets only: journaling an accepted chunk must not capture a screen,
        // or every PTY chunk would pay for a full checkpoint.
        let start_offset = self
            .terminals
            .output_window(&runtime.terminal)
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
        // The journal borrowed the chunk and is done with it, so the retention
        // registry takes the same allocation rather than a second copy of it.
        let Output { data, .. } = output;
        self.terminals
            .append_output_with_replies(&runtime.terminal, data)
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
            // The reservation stays held: the journal still holds these bytes
            // and the record needs reconciliation, so its capacity is not freed.
            return Err(RuntimeError::ReconcileRequired(
                ReconcileState::PersistAfterExit,
            ));
        }
        // The exit result is stored into the capacity reserved before spawn, so
        // no cap can drop it. A client still draining this final pins it.
        let bytes = self.terminals.retained_bytes(&runtime.terminal);
        self.retention
            .commit_final(&runtime.terminal, TerminalKind::Agent, bytes);
        let attached = self.terminals.is_attached(&runtime.terminal);
        self.retention.set_pinned(&runtime.terminal, attached);
        // A runtime can only be superseded once it has already exited, so the
        // launch path — not this one — lowers a replaced source's priority.
        self.collect_garbage(store);
        Ok(())
    }

    /// Applies the aggregate retention authority's decisions to this owner:
    /// every terminal runtime whose final the authority collected loses its
    /// durable record and its output journal, and the store is rewritten once.
    ///
    /// Only a final the authority evicted with a typed marker is removed, so a
    /// record the ledger never accounted for is never deleted by accident. A
    /// runtime that is still a live resume source keeps its record because a
    /// pinned or in-TTL final is never collected in the first place. The work is
    /// bounded by the collection batch, and a failed store write leaves the
    /// removal to converge on a later pass or the next startup import.
    pub fn collect_garbage(&mut self, store: &mut dyn RuntimeStore) -> usize {
        let candidates: Vec<(String, TerminalRef)> = self
            .records
            .iter()
            .filter(|(_, record)| {
                matches!(record.state, RuntimeState::Exited | RuntimeState::Reclaimed)
            })
            .filter(|(_, record)| {
                matches!(
                    self.retention.lookup(&record.runtime.terminal),
                    FinalLookup::Evicted(_)
                )
            })
            .map(|(key, record)| (key.clone(), record.runtime.terminal.clone()))
            .collect();
        let mut collected = 0;
        for (key, terminal) in &candidates {
            if self.generation.forget_terminal(terminal).is_err() {
                // The generation half is the safety fence. An inconsistent or
                // unexpectedly live owner keeps the runtime record for a later
                // reconcile instead of turning GC into a process panic.
                continue;
            }
            self.records.remove(key);
            self.terminals.forget(terminal);
            collected += 1;
        }
        if collected != 0 {
            let _ = self.persist(store);
        }
        collected
    }

    /// Terminates and forgets every Agent runtime owned by one managed session.
    ///
    /// Session teardown calls this before removing the worktree. Running
    /// processes are terminated through their exact fenced terminal identity;
    /// exited and interrupted records are forgotten as well, so removing a
    /// session cannot leave an Agent inventory row behind. A partial terminate
    /// failure keeps only the runtimes whose process could not be reaped and is
    /// retryable by the durable session teardown worker.
    pub fn close_session(
        &mut self,
        session: SessionId,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
    ) -> Result<Vec<AgentRuntimeRef>, RuntimeError> {
        self.close_matching(
            |record| record.runtime.session_id == Some(session),
            store,
            spawner,
        )
    }

    /// Terminates and forgets every Agent runtime owned by one workspace.
    pub fn close_workspace(
        &mut self,
        workspace: WorkspaceId,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
    ) -> Result<Vec<AgentRuntimeRef>, RuntimeError> {
        self.close_matching(
            |record| record.runtime.terminal.workspace_id == workspace,
            store,
            spawner,
        )
    }

    fn close_matching(
        &mut self,
        selected: impl Fn(&DurableRuntimeRecord) -> bool,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
    ) -> Result<Vec<AgentRuntimeRef>, RuntimeError> {
        let targets = self
            .records
            .iter()
            .filter(|(_, record)| selected(record))
            .map(|(key, record)| (key.clone(), record.clone()))
            .collect::<Vec<_>>();
        let mut terminate_failed = false;

        for (_, record) in &targets {
            if runtime_state_requires_termination(record.state) {
                if spawner.terminate_reap(&record.runtime.terminal).is_err() {
                    terminate_failed = true;
                    continue;
                }
                self.generation
                    .resolve_orphan(&record.runtime.terminal, ProcessObservation::Gone, false)
                    .map_err(RuntimeError::Generation)?;
                self.generation
                    .forget_resolved_process(&record.runtime.terminal)
                    .map_err(RuntimeError::Generation)?;
                let retained = self.record_mut(&record.runtime)?;
                retained.state = RuntimeState::Reclaimed;
                retained.process = None;
            }
        }
        if terminate_failed {
            self.persist(store)?;
            return Err(RuntimeError::ReconcileRequired(
                ReconcileState::OrphanRunning,
            ));
        }

        // Session teardown is the explicit acknowledgement that resolves any
        // retained orphan whose process was already unowned. Keep that fence in
        // the generation snapshot and project the matching terminal record
        // before either is forgotten.
        for (_, record) in &targets {
            self.generation
                .resolve_orphan(&record.runtime.terminal, ProcessObservation::Unknown, true)
                .map_err(RuntimeError::Generation)?;
            self.generation
                .forget_resolved_process(&record.runtime.terminal)
                .map_err(RuntimeError::Generation)?;
            let retained = self.record_mut(&record.runtime)?;
            retained.state = RuntimeState::Reclaimed;
            retained.process = None;
            if let Some(provider) = &mut retained.provider_resume {
                provider.last_known_status = ProviderResumeStatus::Exited;
                provider.last_known_phase = Some(ProviderResumePhase::Ended);
            }
        }

        // Publish every target's terminal state before removing it from the
        // snapshot. The sharded store releases global allocator claims from
        // these terminal projections; persisting only the final empty snapshot
        // would erase that evidence and leave the capacity claim behind.
        //
        // This first save is also the crash fence for session teardown. A retry
        // after it sees only terminal records and can safely converge on the
        // second, forgetting save without spawning or signalling anything.
        if !targets.is_empty() {
            self.persist(store)?;
        }

        let mut closed = Vec::new();
        for (key, record) in targets {
            self.generation
                .forget_terminal(&record.runtime.terminal)
                .map_err(RuntimeError::Generation)?;
            self.records.remove(&key);
            self.terminals.forget(&record.runtime.terminal);
            self.retention.forget(&record.runtime.terminal);
            closed.push(record.runtime);
        }

        // Persist even when a retry finds no records. If a prior store write
        // failed after the in-memory close, this converges the durable snapshot
        // before the worktree teardown is allowed to continue.
        self.persist(store)?;
        Ok(closed)
    }

    /// Stops the exact selected Agents while retaining provider resume metadata.
    /// Selection and the user-confirmation policy belong to the Agent usecase;
    /// this coordinator only performs fenced PTY termination.
    pub fn interrupt_agents(
        &mut self,
        runtime_ids: &BTreeSet<String>,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
    ) -> Result<usize, RuntimeError> {
        let targets = self
            .records
            .iter()
            .filter(|(key, _)| runtime_ids.contains(*key))
            .map(|(key, record)| (key.clone(), record.clone()))
            .collect::<Vec<_>>();

        let mut interrupted = 0;
        for (key, record) in targets {
            if record.state != RuntimeState::Reserved
                && !runtime_state_requires_termination(record.state)
            {
                continue;
            }
            if record.process.is_some() && spawner.terminate_reap(&record.runtime.terminal).is_err()
            {
                self.records
                    .get_mut(&key)
                    .expect("selected runtime exists")
                    .state = RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning);
                self.persist(store)?;
                return Err(RuntimeError::ReconcileRequired(
                    ReconcileState::OrphanRunning,
                ));
            }
            self.generation
                .resolve_orphan(&record.runtime.terminal, ProcessObservation::Unknown, true)
                .map_err(RuntimeError::Generation)?;
            self.generation
                .forget_resolved_process(&record.runtime.terminal)
                .map_err(RuntimeError::Generation)?;
            let retained = self.records.get_mut(&key).expect("selected runtime exists");
            retained.state = RuntimeState::Exited;
            retained.process = None;
            if let Some(provider) = &mut retained.provider_resume {
                provider.last_known_status = ProviderResumeStatus::Interrupted;
                provider.last_known_phase = Some(ProviderResumePhase::Interrupted);
            }
            interrupted += 1;
        }
        self.persist(store)?;
        Ok(interrupted)
    }

    /// Intentionally stops exact selected Agents while retaining their records
    /// as provider-resumable sleep sources. The worktree and managed session
    /// are outside this runtime transition and remain untouched.
    pub fn sleep_agents(
        &mut self,
        runtime_ids: &BTreeSet<String>,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
    ) -> Result<usize, RuntimeError> {
        let targets = self
            .records
            .iter()
            .filter(|(key, _)| runtime_ids.contains(*key))
            .map(|(key, record)| (key.clone(), record.clone()))
            .collect::<Vec<_>>();

        let mut slept = 0;
        for (key, record) in targets {
            if record.state != RuntimeState::Running {
                continue;
            }
            if spawner.terminate_reap(&record.runtime.terminal).is_err() {
                self.records
                    .get_mut(&key)
                    .expect("selected runtime exists")
                    .state = RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning);
                self.persist(store)?;
                return Err(RuntimeError::ReconcileRequired(
                    ReconcileState::OrphanRunning,
                ));
            }
            self.generation
                .resolve_orphan(&record.runtime.terminal, ProcessObservation::Unknown, true)
                .map_err(RuntimeError::Generation)?;
            self.generation
                .forget_resolved_process(&record.runtime.terminal)
                .map_err(RuntimeError::Generation)?;
            let retained = self.records.get_mut(&key).expect("selected runtime exists");
            retained.state = RuntimeState::Sleeping;
            retained.process = None;
            if let Some(provider) = &mut retained.provider_resume {
                provider.last_known_status = ProviderResumeStatus::Interrupted;
                provider.last_known_phase = Some(ProviderResumePhase::Interrupted);
            }
            slept += 1;
        }
        self.persist(store)?;
        Ok(slept)
    }

    /// The aggregate retention authority this owner shares with the generic
    /// terminal owner.
    #[must_use]
    pub fn retention(&self) -> &SharedTerminalRetention {
        &self.retention
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

    /// Converts a launch reservation which is known not to have spawned a
    /// process into its terminal failure state. This is used both by the
    /// immediate launch error path and by explicit cleanup of an older leaked
    /// admission whose dispatch run is already terminal.
    pub fn fail_reserved_launch(
        &mut self,
        runtime: &AgentRuntimeRef,
        store: &mut dyn RuntimeStore,
    ) -> Result<bool, RuntimeError> {
        let record = self.record(runtime)?;
        if record.state != RuntimeState::Reserved || record.process.is_some() {
            return Ok(false);
        }
        self.generation
            .resolve_orphan(&runtime.terminal, ProcessObservation::Gone, false)
            .map_err(RuntimeError::Generation)?;
        self.generation
            .forget_resolved_process(&runtime.terminal)
            .map_err(RuntimeError::Generation)?;
        let record = self.record_mut(runtime)?;
        record.state = RuntimeState::SpawnFailed;
        record.outcome = DurableOperationOutcome::SpawnUnavailable;
        record.process = None;
        self.persist(store)?;
        Ok(true)
    }

    /// Force-clean counterpart for a failed admission restored after daemon
    /// restart. The caller has already matched a terminal dispatch run and the
    /// explicit clean command acknowledges the identity-unknown reservation.
    pub fn clean_failed_launch(
        &mut self,
        runtime: &AgentRuntimeRef,
        store: &mut dyn RuntimeStore,
    ) -> Result<bool, RuntimeError> {
        let record = self.record(runtime)?;
        if record.process.is_some() {
            return Ok(false);
        }
        let observation = match record.state {
            RuntimeState::Reserved => ProcessObservation::Gone,
            RuntimeState::ReconcileRequired(ReconcileState::IdentityUnknown) => {
                ProcessObservation::Unknown
            }
            _ => return Ok(false),
        };
        let acknowledged = matches!(observation, ProcessObservation::Unknown);
        self.generation
            .resolve_orphan(&runtime.terminal, observation, acknowledged)
            .map_err(RuntimeError::Generation)?;
        self.generation
            .forget_resolved_process(&runtime.terminal)
            .map_err(RuntimeError::Generation)?;
        let record = self.record_mut(runtime)?;
        record.state = RuntimeState::SpawnFailed;
        record.outcome = DurableOperationOutcome::SpawnUnavailable;
        record.process = None;
        self.persist(store)?;
        Ok(true)
    }

    pub fn terminal_snapshot(&self, runtime: &AgentRuntimeRef) -> Result<Snapshot, RuntimeError> {
        self.record(runtime)?;
        // The registry's typed failure is preserved: a fencing failure and a
        // screen that does not fit one frame are different client contracts.
        self.terminals
            .snapshot(&runtime.terminal)
            .map_err(RuntimeError::Terminal)
    }

    /// The hosting terminal's committed exit status without capturing a screen,
    /// for the incremental `Resume` path.
    pub fn terminal_exit_status(
        &self,
        runtime: &AgentRuntimeRef,
    ) -> Result<Option<i32>, RuntimeError> {
        self.record(runtime)?;
        self.terminals
            .exit_status(&runtime.terminal)
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

    /// Atomically attaches and exposes the connection/client input ledger cursor.
    pub fn attach_for_client(
        &mut self,
        runtime: &AgentRuntimeRef,
        connection: ConnectionId,
        client: ClientId,
        viewport: Option<Geometry>,
        writer: &mut dyn PtyWriter,
    ) -> Result<Attached, RuntimeError> {
        self.running(runtime)?;
        self.terminals
            .attach_for_client(&runtime.terminal, connection, client, viewport, writer)
            .map_err(RuntimeError::Terminal)
    }

    /// Removes only the named attachment; the daemon-owned Agent process and its
    /// PTY intentionally stay alive.
    pub fn detach(
        &mut self,
        runtime: &AgentRuntimeRef,
        subscription: u64,
        connection: ConnectionId,
        writer: &mut dyn PtyWriter,
    ) -> Result<(), RuntimeError> {
        self.record(runtime)?;
        let detached = self
            .terminals
            .detach(&runtime.terminal, subscription, connection, writer)
            .map_err(RuntimeError::Terminal);
        // A final nobody is draining any more is an ordinary GC candidate.
        let attached = self.terminals.is_attached(&runtime.terminal);
        self.retention.set_pinned(&runtime.terminal, attached);
        detached
    }

    /// Updates the fenced runtime terminal geometry.
    pub fn resize(
        &mut self,
        runtime: &AgentRuntimeRef,
        geometry: Geometry,
        client: Option<&ClientId>,
        writer: &mut dyn PtyWriter,
    ) -> Result<Snapshot, RuntimeError> {
        self.running(runtime)?;
        self.terminals
            .resize(&runtime.terminal, geometry, client, writer)
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
            .write_input(
                &runtime.terminal,
                input,
                bytes,
                self.retention.now_ms(),
                writer,
            )
            .map_err(RuntimeError::Terminal)
    }

    /// Reads the recorded final of one durable input operation (#519).
    ///
    /// It is read-only and deliberately not gated on liveness: a client resolving
    /// a lost acknowledgement must reach the same final even after the Agent's
    /// PTY has exited. `Ok(None)` is a typed unknown, never a rewrite licence.
    pub fn input_outcome(
        &mut self,
        runtime: &AgentRuntimeRef,
        client: ClientId,
        operation: OperationId,
    ) -> Result<Option<InputAck>, RuntimeError> {
        let now_ms = self.retention.now_ms();
        self.terminals
            .input_outcome(&runtime.terminal, client, operation, now_ms)
            .map_err(RuntimeError::Terminal)
    }

    /// Replays retained output after `offset` for a reconnecting attachment.
    pub fn replay_from(
        &self,
        runtime: &AgentRuntimeRef,
        offset: u64,
        client: Option<&ClientId>,
    ) -> Result<Vec<Output>, RuntimeError> {
        self.record(runtime)?;
        self.terminals
            .replay_from(&runtime.terminal, offset, client)
            .map_err(RuntimeError::Terminal)
    }

    /// Drops only this connection's subscriptions across every runtime terminal.
    /// It never kills an Agent process, its PTY, or the completion worker.
    pub fn disconnect(&mut self, connection: ConnectionId, writer: &mut dyn PtyWriter) {
        self.terminals.disconnect(connection, writer);
        // Finals this connection was draining are no longer pinned.
        let exited: Vec<TerminalRef> = self
            .records
            .values()
            .filter(|record| record.state == RuntimeState::Exited)
            .map(|record| record.runtime.terminal.clone())
            .collect();
        for terminal in exited {
            let attached = self.terminals.is_attached(&terminal);
            self.retention.set_pinned(&terminal, attached);
        }
    }

    /// Coalesces cleanup for every connection absent from the daemon's current
    /// live census. This has the same ownership semantics as repeated
    /// [`Self::disconnect`] calls without retaining historical connection IDs.
    pub fn retain_live_connections(
        &mut self,
        live: &BTreeSet<ConnectionId>,
        writer: &mut dyn PtyWriter,
    ) {
        self.terminals.retain_live_connections(live, writer);
        let exited: Vec<TerminalRef> = self
            .records
            .values()
            .filter(|record| record.state == RuntimeState::Exited)
            .map(|record| record.runtime.terminal.clone())
            .collect();
        for terminal in exited {
            let attached = self.terminals.is_attached(&terminal);
            self.retention.set_pinned(&terminal, attached);
        }
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

    /// Resolves the runtime admitted by one durable operation fence.
    ///
    /// Operation ownership is unique by construction: hydration rejects a
    /// duplicate and launch refuses to reserve one twice.
    #[must_use]
    pub fn runtime_for_operation(&self, operation_id: OperationId) -> Option<AgentRuntimeRef> {
        self.records
            .values()
            .find(|record| record.operation.operation_id == operation_id)
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
    /// Lists exited Agent-runtime tombstones in the exact requested scope with
    /// their exit status and bounded final-replay locator (#525). The
    /// visibility field is a placeholder; the shared owner overwrites it from
    /// the authoritative workspace-global ledger. Only `Exited` records appear.
    #[must_use]
    pub fn completed_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_visibility::CompletedTerminalEntry> {
        use usagi_core::domain::{
            terminal_launch::TerminalKind,
            terminal_visibility::{CompletedTerminalEntry, TerminalVisibility},
        };
        self.records
            .values()
            .filter(|record| {
                record.runtime.terminal.workspace_id == scope.workspace_id
                    && record.runtime.terminal.session_id == scope.session_id
                    && record.runtime.terminal.worktree_id == scope.worktree_id
                    && matches!(record.state, RuntimeState::Exited)
            })
            .filter_map(|record| {
                // A tombstone listing needs the final replay locator, not a
                // screen: capturing one per entry would make every inventory
                // query proportional to the retained screens.
                let window = self
                    .terminals
                    .output_window(&record.runtime.terminal)
                    .ok()?;
                let exit_status = window.exited?;
                Some(CompletedTerminalEntry {
                    terminal: record.runtime.terminal.clone(),
                    kind: TerminalKind::Agent,
                    exit_status,
                    base_offset: window.base_offset,
                    final_output_offset: window.output_offset,
                    visibility: TerminalVisibility::unobserved(),
                })
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
    /// Writes an ID obtained from a documented provider-owned structured
    /// channel. `Attach` protects an existing conversation while `Replace`
    /// admits `/clear` and interactive resume transitions. Both modes retain
    /// the complete runtime, launch scope, and adapter fences.
    pub fn write_provider_resume(
        &mut self,
        runtime: &AgentRuntimeRef,
        provider_resume: ProviderResumeRef,
        write: ProviderResumeWrite,
        store: &mut dyn RuntimeStore,
    ) -> Result<(), RuntimeError> {
        let record = self.record_mut(runtime)?;
        if record.state != RuntimeState::Running
            || record.launch.request.scope != provider_resume.scope
            || record.launch.plan.profile_revision != provider_resume.adapter_revision
            || record.provider_resume.as_ref().is_some_and(|existing| {
                write == ProviderResumeWrite::Attach && existing != &provider_resume
            })
        {
            return Err(RuntimeError::ProviderResumeMismatch);
        }
        record.provider_resume = Some(provider_resume);
        self.persist(store)
    }
    /// Refines only the safe phase of an existing provider resume reference for
    /// a live runtime.
    ///
    /// Process death stays observation-owned: this path never writes
    /// `last_known_status`, and a runtime which is not `Running` is refused so a
    /// late report cannot make a reconciled or exited record look alive.  A
    /// record without provider metadata (for example Antigravity, Claude, or Codex before its
    /// structured capture) is a no-op rather than a synthesized reference, and
    /// an unchanged phase does not persist a snapshot.
    pub fn record_provider_phase(
        &mut self,
        runtime: &AgentRuntimeRef,
        phase: ProviderResumePhase,
        store: &mut dyn RuntimeStore,
    ) -> Result<(), RuntimeError> {
        let record = self.record_mut(runtime)?;
        if record.state != RuntimeState::Running {
            return Err(RuntimeError::ProviderResumeMismatch);
        }
        let Some(reference) = record.provider_resume.as_mut() else {
            return Ok(());
        };
        if reference.last_known_phase == Some(phase) {
            return Ok(());
        }
        reference.last_known_phase = Some(phase);
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
    /// The concurrency level as [`admission`](Self::occupied_slots) counts it,
    /// over the limit that check compares against.
    ///
    /// Both numbers come from this coordinator, so an observer never restates the
    /// constant that supplied the limit.
    #[must_use]
    pub fn concurrency(&self) -> usagi_core::infrastructure::ipc::AgentConcurrency {
        usagi_core::infrastructure::ipc::AgentConcurrency {
            in_use: u32::try_from(self.occupied_slots()).unwrap_or(u32::MAX),
            limit: u32::try_from(self.limit).unwrap_or(u32::MAX),
        }
    }

    /// Publishes this coordinator's concurrency level into `gauge` from now on,
    /// starting with the level it holds right now.
    ///
    /// Composition binds the gauge the metrics broker reads. Binding publishes
    /// immediately so a daemon that hydrated interrupted records reports them
    /// before its first mutation, rather than reading as an idle pool.
    pub fn bind_concurrency_gauge(&mut self, gauge: AgentConcurrencyGauge) {
        self.concurrency = gauge;
        self.publish_concurrency();
    }

    /// Republishes the level. Called from [`persist`](Self::persist), the single
    /// choke point every record mutation passes through, so the published level
    /// cannot drift from the records admission counts.
    fn publish_concurrency(&self) {
        self.concurrency.publish(self.occupied_slots(), self.limit);
    }

    fn persist(&self, store: &mut dyn RuntimeStore) -> Result<(), RuntimeError> {
        // Before the store result: the in-memory records are what admission
        // consults, and they already changed. A failed write must not leave the
        // observed level behind the level that refuses the next launch.
        self.publish_concurrency();
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
        let missing = self.missing(&runtime.terminal);
        self.records
            .get(&runtime.agent_runtime_id.as_str())
            .filter(|record| record.runtime.fences(runtime))
            .ok_or(missing)
    }
    fn record_mut(
        &mut self,
        runtime: &AgentRuntimeRef,
    ) -> Result<&mut DurableRuntimeRecord, RuntimeError> {
        let missing = self.missing(&runtime.terminal);
        self.records
            .get_mut(&runtime.agent_runtime_id.as_str())
            .filter(|record| record.runtime.fences(runtime))
            .ok_or(missing)
    }
    /// Why a runtime is absent: collected by aggregate retention, or never
    /// owned here. A collected final is a typed outcome, never a fallback to
    /// some other history.
    fn missing(&self, terminal: &TerminalRef) -> RuntimeError {
        match self.retention.lookup(terminal) {
            FinalLookup::Evicted(marker) => RuntimeError::FinalEvicted(marker.reason),
            _ => RuntimeError::UnknownRuntime,
        }
    }
    fn running(&self, runtime: &AgentRuntimeRef) -> Result<(), RuntimeError> {
        match self.record(runtime)?.state {
            RuntimeState::Running => self
                .generation
                .require_terminal(&runtime.terminal)
                .map_err(RuntimeError::Generation),
            RuntimeState::Interrupted
            | RuntimeState::Sleeping
            | RuntimeState::Exited
            | RuntimeState::Reclaimed => Err(RuntimeError::Terminal(RegistryError::Exited)),
            _ => Err(RuntimeError::ReconcileRequired(
                ReconcileState::IdentityUnknown,
            )),
        }
    }
}

const fn runtime_state_requires_termination(state: RuntimeState) -> bool {
    matches!(
        state,
        RuntimeState::Running
            | RuntimeState::ReconcileRequired(
                ReconcileState::SpawnAmbiguous
                    | ReconcileState::PersistAfterSpawn
                    | ReconcileState::OrphanRunning
            )
    )
}

fn terminal_ownership_state(state: RuntimeState) -> TerminalState {
    match state {
        RuntimeState::Running => TerminalState::Available,
        RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning) => {
            TerminalState::OrphanRunning
        }
        RuntimeState::Interrupted
        | RuntimeState::Reserved
        | RuntimeState::ReconcileRequired(
            ReconcileState::SpawnAmbiguous
            | ReconcileState::PersistAfterSpawn
            | ReconcileState::PersistAfterExit
            | ReconcileState::IdentityUnknown,
        ) => TerminalState::IdentityUnknown,
        RuntimeState::Sleeping | RuntimeState::Exited => TerminalState::Terminated,
        RuntimeState::SpawnFailed | RuntimeState::Reclaimed => TerminalState::Lost,
    }
}

fn terminal_ownership_matches(state: RuntimeState, ownership: TerminalState) -> bool {
    ownership == terminal_ownership_state(state)
        || (state == RuntimeState::Reclaimed && ownership == TerminalState::Terminated)
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
            || record.launch.request.scope.workspace_id != record.runtime.terminal.workspace_id
            || record.launch.request.scope.session_id != record.runtime.terminal.session_id
            || record.launch.request.scope.worktree_id != record.runtime.terminal.worktree_id
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
    // A replacement is always persisted by its active-generation owner, while
    // its retired source can live in a foreign shard. Rebuild that derived
    // back-reference only across that generation boundary; a one-sided relation
    // inside one atomic owner shard remains corruption and fails closed.
    let mut replacement_sources = std::collections::BTreeSet::new();
    let source_backrefs = records
        .values()
        .filter_map(|record| {
            record.resumed_from.map(|source_id| {
                (
                    source_id,
                    record.runtime.agent_runtime_id,
                    record.runtime.terminal.daemon_generation,
                    record.continuation,
                    record.launch.request.scope.clone(),
                )
            })
        })
        .collect::<Vec<_>>();
    for (source_id, replacement_id, replacement_generation, continuation, scope) in source_backrefs
    {
        if !replacement_sources.insert(source_id) {
            return Err(RuntimeSnapshotError::ResumeRelation);
        }
        let Some(source) = records
            .values_mut()
            .find(|candidate| candidate.resume_source == Some(source_id))
        else {
            // `resumed_from` is historical evidence, not ownership authority.
            // Its retired source shard may already have passed bounded
            // retention; keep the exact source id as a tombstone without
            // pinning that shard or refusing startup.
            continue;
        };
        if source.continuation != continuation || source.launch.request.scope != scope {
            return Err(RuntimeSnapshotError::ResumeRelation);
        }
        match source.superseded_by {
            Some(existing_id) if existing_id != replacement_id => {
                return Err(RuntimeSnapshotError::ResumeRelation);
            }
            None if source.runtime.terminal.daemon_generation == replacement_generation => {
                return Err(RuntimeSnapshotError::ResumeRelation);
            }
            None => source.superseded_by = Some(replacement_id),
            Some(_) => {}
        }
    }
    for record in records.values() {
        if let Some(replacement_id) = record.superseded_by {
            let Some(source_id) = record.resume_source else {
                return Err(RuntimeSnapshotError::ResumeRelation);
            };
            let Some(replacement) = records
                .values()
                .find(|candidate| candidate.runtime.agent_runtime_id == replacement_id)
            else {
                // The source remains a no-double-resume tombstone even after
                // bounded retention collects the replacement history.
                continue;
            };
            if replacement.resumed_from != Some(source_id)
                || replacement.continuation != record.continuation
            {
                return Err(RuntimeSnapshotError::ResumeRelation);
            }
        }
    }
    validate_acyclic_resume_lineage(&records)?;
    Ok(records)
}

fn validate_acyclic_resume_lineage(
    records: &BTreeMap<String, DurableRuntimeRecord>,
) -> Result<(), RuntimeSnapshotError> {
    let mut acyclic = std::collections::BTreeSet::new();
    for record in records.values() {
        let mut seen = std::collections::BTreeSet::new();
        let mut cursor = record;
        while !acyclic.contains(&cursor.runtime.agent_runtime_id) {
            if !seen.insert(cursor.runtime.agent_runtime_id) {
                return Err(RuntimeSnapshotError::ResumeRelation);
            }
            let Some(replacement_id) = cursor.superseded_by else {
                break;
            };
            let Some(replacement) = records.get(&replacement_id.as_str()) else {
                break;
            };
            cursor = replacement;
        }
        acyclic.extend(seen);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
