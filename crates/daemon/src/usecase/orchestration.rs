//! Product-neutral daemon orchestration for agent runtimes.
//!
//! Product identity is resolved only through [`AdapterRegistry`].  This module
//! never branches on a product name and never retains adapter payloads,
//! credentials, rendered argv, or phase-hook input.

#![allow(clippy::missing_errors_doc, clippy::too_many_arguments)] // Port methods expose injected runtime dependencies and typed fences.
use std::collections::BTreeMap;

use usagi_core::{
    domain::{
        agent::{
            AgentCapability, AgentProfile, AgentProfileId, DurableLaunchSnapshot, LaunchRequest,
            LaunchValidationError,
        },
        id::{AgentRuntimeRef, CompletionFence},
    },
    usecase::agent::{AgentProfileCatalog, validate_snapshot},
};

use super::{
    claude::{ClaudeAdapter, ClaudeProvisioner},
    codex::{CodexAdapter, CodexProvisioner},
    control::AgentPhase,
    generation::ProcessObservation,
    runtime::{
        AgentAdapter, PtySpawner, ReconcileState, RuntimeCoordinator, RuntimeError, RuntimeState,
        RuntimeStore,
    },
    terminal::Geometry,
};

/// A single product adapter registered with the daemon orchestration port.
///
/// `Send` is required because the composition root shares one registry (behind
/// the Agent owner) across every IPC connection thread.
pub trait RegisteredAdapter: AgentAdapter + AgentProfileCatalog + Send {}
impl<T: AgentAdapter + AgentProfileCatalog + Send> RegisteredAdapter for T {}

/// Code-defined adapter registry.  Lookup is by profile descriptor, never by
/// a daemon-owned product-name switch.
#[derive(Default)]
pub struct AdapterRegistry {
    adapters: Vec<(AgentProfile, Box<dyn RegisteredAdapter>)>,
}

impl AdapterRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the supported product adapters with the same orchestration
    /// port. Product-specific behavior remains behind each adapter; callers
    /// select it solely by the typed profile ID in a launch request.
    pub fn register_supported<
        C: CodexProvisioner + Send + 'static,
        L: ClaudeProvisioner + Send + 'static,
    >(
        &mut self,
        codex: CodexAdapter<C>,
        claude: ClaudeAdapter<L>,
    ) -> Result<(), RegistryError> {
        self.register(codex.profile().clone(), Box::new(codex))?;
        self.register(claude.profile().clone(), Box::new(claude))
    }

    /// Registers one adapter. Duplicate profile IDs are rejected so a restored
    /// snapshot cannot be routed ambiguously.
    pub fn register(
        &mut self,
        profile: AgentProfile,
        adapter: Box<dyn RegisteredAdapter>,
    ) -> Result<(), RegistryError> {
        if adapter.find(&profile.id).as_ref() != Some(&profile) {
            return Err(RegistryError::ProfileMismatch);
        }
        if self
            .adapters
            .iter()
            .any(|(existing, _)| existing.id == profile.id)
        {
            return Err(RegistryError::DuplicateProfile);
        }
        self.adapters.push((profile, adapter));
        Ok(())
    }

    pub fn profile(&self, id: &AgentProfileId) -> Result<AgentProfile, RegistryError> {
        self.adapters
            .iter()
            .find_map(|(profile, _)| (profile.id == *id).then(|| profile.clone()))
            .ok_or(RegistryError::UnknownProfile)
    }

    fn adapter_mut(
        &mut self,
        id: &AgentProfileId,
    ) -> Result<&mut (dyn RegisteredAdapter + '_), RegistryError> {
        for (profile, adapter) in &mut self.adapters {
            if profile.id == *id {
                return Ok(adapter.as_mut());
            }
        }
        Err(RegistryError::UnknownProfile)
    }

    fn validate(
        &self,
        snapshot: &DurableLaunchSnapshot,
    ) -> Result<AgentProfile, LaunchValidationError> {
        validate_snapshot(self, snapshot)
    }
}

impl AgentProfileCatalog for AdapterRegistry {
    fn find(&self, profile_id: &AgentProfileId) -> Option<AgentProfile> {
        self.adapters
            .iter()
            .find_map(|(profile, _)| (profile.id == *profile_id).then(|| profile.clone()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    ProfileMismatch,
    DuplicateProfile,
    UnknownProfile,
}

/// Independent workspace/session authorization. Profile capabilities never
/// substitute this scope check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAuthorization {
    pub runtime: AgentRuntimeRef,
    pub operation: CompletionFence,
    pub mcp_allowed: bool,
}

impl RuntimeAuthorization {
    fn fences(&self, runtime: &AgentRuntimeRef, operation: &CompletionFence) -> bool {
        self.runtime.fences(runtime)
            && &self.operation == operation
            && runtime.session_id == operation.session_id
            && runtime.terminal.workspace_id == operation.workspace_id
            && runtime.terminal.daemon_generation == operation.owner_daemon_generation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PhaseLease {
    runtime: AgentRuntimeRef,
    operation: CompletionFence,
    token: String,
    source_sequence: u64,
    phase: AgentPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseResult {
    Applied,
    DuplicateOrStale,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseRejection {
    Unauthorized,
    CapabilityUnavailable,
    UnknownRuntime,
    Exited,
    InvalidToken,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeDecision {
    Compatible,
    ManualActionRequired,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimDecision {
    Reclaimed,
    OrphanNeedsAction,
    ManualActionRequired,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretAvailability {
    Available,
    LostOrUnknown,
}

/// In-memory phase leases and the common runtime coordinator. Phase tokens
/// intentionally disappear on daemon restart; a stale hook must then fail.
pub struct Orchestrator {
    phases: BTreeMap<String, PhaseLease>,
}

impl Orchestrator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            phases: BTreeMap::new(),
        }
    }

    /// Resolves the selected adapter through the common registry and launches
    /// it once. MCP materialization remains adapter-scoped and is requested
    /// only after profile capability and explicit authorization are checked.
    pub fn launch(
        &mut self,
        runtime: &mut RuntimeCoordinator,
        registry: &mut AdapterRegistry,
        authorization: &RuntimeAuthorization,
        request: &LaunchRequest,
        geometry: Geometry,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
        mcp_credential: Option<String>,
    ) -> Result<(), OrchestrationError> {
        self.launch_with_semantic(
            runtime,
            registry,
            authorization,
            request,
            geometry,
            store,
            spawner,
            mcp_credential,
            "internal-launch".to_owned(),
        )
    }

    pub fn launch_with_semantic(
        &mut self,
        runtime: &mut RuntimeCoordinator,
        registry: &mut AdapterRegistry,
        authorization: &RuntimeAuthorization,
        request: &LaunchRequest,
        geometry: Geometry,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
        mcp_credential: Option<String>,
        semantic_key: String,
    ) -> Result<(), OrchestrationError> {
        Self::launch_with_semantic_superseding(
            runtime,
            registry,
            authorization,
            request,
            geometry,
            store,
            spawner,
            mcp_credential,
            semantic_key,
            &[],
        )
    }

    /// Launches an explicit provider resume and durably supersedes its
    /// interrupted source incarnations in the replacement reservation.
    pub fn resume_with_semantic(
        &mut self,
        runtime: &mut RuntimeCoordinator,
        registry: &mut AdapterRegistry,
        authorization: &RuntimeAuthorization,
        request: &LaunchRequest,
        geometry: Geometry,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
        mcp_credential: Option<String>,
        semantic_key: String,
        superseded: &[AgentRuntimeRef],
    ) -> Result<(), OrchestrationError> {
        Self::launch_with_semantic_superseding(
            runtime,
            registry,
            authorization,
            request,
            geometry,
            store,
            spawner,
            mcp_credential,
            semantic_key,
            superseded,
        )
    }

    fn launch_with_semantic_superseding(
        runtime: &mut RuntimeCoordinator,
        registry: &mut AdapterRegistry,
        authorization: &RuntimeAuthorization,
        request: &LaunchRequest,
        geometry: Geometry,
        store: &mut dyn RuntimeStore,
        spawner: &mut dyn PtySpawner,
        mcp_credential: Option<String>,
        semantic_key: String,
        superseded: &[AgentRuntimeRef],
    ) -> Result<(), OrchestrationError> {
        if !authorization.fences(&authorization.runtime, &authorization.operation)
            || request.scope.session_id != authorization.runtime.session_id
            || request.scope.workspace_id != authorization.runtime.terminal.workspace_id
        {
            return Err(OrchestrationError::Unauthorized);
        }
        if request
            .required_capabilities()
            .contains(&AgentCapability::McpWiring)
            && !authorization.mcp_allowed
        {
            return Err(OrchestrationError::Unauthorized);
        }
        let adapter = registry
            .adapter_mut(&request.profile_id)
            .map_err(|_| OrchestrationError::UnknownProfile)?;
        if superseded.is_empty() {
            runtime.launch_with_semantic(
                request,
                authorization.runtime.clone(),
                authorization.operation.clone(),
                geometry,
                adapter,
                store,
                spawner,
                mcp_credential,
                semantic_key,
            )
        } else {
            runtime.resume_with_semantic(
                request,
                authorization.runtime.clone(),
                authorization.operation.clone(),
                geometry,
                adapter,
                store,
                spawner,
                mcp_credential,
                semantic_key,
                superseded,
            )
        }
        .map_err(OrchestrationError::Runtime)
    }

    /// Adds an ephemeral token lease for one successfully spawned runtime.
    /// The token is never persisted or returned by this API.
    pub fn enable_phase_reporting(
        &mut self,
        runtime: &RuntimeCoordinator,
        registry: &AdapterRegistry,
        authorization: &RuntimeAuthorization,
        token: String,
    ) -> Result<(), PhaseRejection> {
        let record = runtime
            .record_for(&authorization.runtime)
            .map_err(|_| PhaseRejection::UnknownRuntime)?;
        if !authorization.fences(&record.runtime, &record.operation) {
            return Err(PhaseRejection::Unauthorized);
        }
        let profile = registry
            .validate(&record.launch)
            .map_err(|_| PhaseRejection::CapabilityUnavailable)?;
        if !profile
            .capabilities
            .contains(&AgentCapability::PhaseReporting)
        {
            return Err(PhaseRejection::CapabilityUnavailable);
        }
        if record.state != RuntimeState::Running {
            return Err(PhaseRejection::Exited);
        }
        self.phases.insert(
            record.runtime.agent_runtime_id.as_str(),
            PhaseLease {
                runtime: record.runtime.clone(),
                operation: record.operation.clone(),
                token,
                source_sequence: 0,
                phase: AgentPhase::Ready,
            },
        );
        Ok(())
    }

    /// Validates every phase fence before changing only the in-memory phase
    /// projection. Raw hook payloads are intentionally absent from the input.
    pub fn report_phase(
        &mut self,
        runtime: &RuntimeCoordinator,
        authorization: &RuntimeAuthorization,
        token: &str,
        source_sequence: u64,
        phase: AgentPhase,
    ) -> Result<PhaseResult, PhaseRejection> {
        let record = runtime
            .record_for(&authorization.runtime)
            .map_err(|_| PhaseRejection::UnknownRuntime)?;
        if !authorization.fences(&record.runtime, &record.operation) {
            return Err(PhaseRejection::Unauthorized);
        }
        if record.state != RuntimeState::Running {
            return Err(PhaseRejection::Exited);
        }
        let lease = self
            .phases
            .get_mut(&record.runtime.agent_runtime_id.as_str())
            .ok_or(PhaseRejection::InvalidToken)?;
        if !lease.runtime.fences(&record.runtime)
            || lease.operation != record.operation
            || lease.token != token
        {
            return Err(PhaseRejection::InvalidToken);
        }
        if source_sequence <= lease.source_sequence {
            return Ok(PhaseResult::DuplicateOrStale);
        }
        lease.source_sequence = source_sequence;
        lease.phase = phase;
        Ok(PhaseResult::Applied)
    }

    /// Validates immutable request/plan provenance and adapter revision. It
    /// never re-renders, provisions, or spawns as part of recovery.
    #[must_use]
    pub fn resume(
        &self,
        runtime: &RuntimeCoordinator,
        registry: &AdapterRegistry,
        authorization: &RuntimeAuthorization,
    ) -> ResumeDecision {
        let Ok(record) = runtime.record_for(&authorization.runtime) else {
            return ResumeDecision::ManualActionRequired;
        };
        if !authorization.fences(&record.runtime, &record.operation) {
            return ResumeDecision::ManualActionRequired;
        }
        let Ok(profile) = registry.validate(&record.launch) else {
            return ResumeDecision::ManualActionRequired;
        };
        if !profile.capabilities.contains(&AgentCapability::Resume) {
            return ResumeDecision::ManualActionRequired;
        }
        match record.state {
            RuntimeState::Running
            | RuntimeState::ReconcileRequired(ReconcileState::OrphanRunning) => {
                ResumeDecision::Compatible
            }
            _ => ResumeDecision::ManualActionRequired,
        }
    }

    /// Reclaims only a verified absence or records a verified orphan. Missing
    /// secrets, ambiguous spawn, identity mismatch, and unknown observations
    /// remain manual-action outcomes and cannot cause a replacement spawn.
    pub fn reclaim(
        &mut self,
        runtime: &mut RuntimeCoordinator,
        authorization: &RuntimeAuthorization,
        observation: ProcessObservation,
        secrets: SecretAvailability,
        store: &mut dyn RuntimeStore,
    ) -> Result<ReclaimDecision, OrchestrationError> {
        let record = runtime
            .record_for(&authorization.runtime)
            .map_err(|_| OrchestrationError::UnknownRuntime)?;
        if !authorization.fences(&record.runtime, &record.operation)
            || secrets != SecretAvailability::Available
        {
            return Ok(ReclaimDecision::ManualActionRequired);
        }
        let verified = match (&record.process, &observation) {
            (Some(_), ProcessObservation::Gone) => true,
            (Some(expected), ProcessObservation::VerifiedAlive(actual)) => expected == actual,
            _ => false,
        };
        if !verified {
            return Ok(ReclaimDecision::ManualActionRequired);
        }
        runtime
            .reconcile(&authorization.runtime, observation, store)
            .map_err(OrchestrationError::Runtime)?;
        Ok(
            if runtime
                .record_for(&authorization.runtime)
                .map_err(|_| OrchestrationError::UnknownRuntime)?
                .state
                == RuntimeState::Reclaimed
            {
                ReclaimDecision::Reclaimed
            } else {
                ReclaimDecision::OrphanNeedsAction
            },
        )
    }
}

impl Default for Orchestrator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum OrchestrationError {
    Unauthorized,
    UnknownProfile,
    UnknownRuntime,
    Runtime(RuntimeError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::generation::ProcessIdentity;
    use crate::usecase::runtime::{
        AdapterError, ResolvedLaunch, RuntimeStoreSnapshot, SpawnFailure, SpawnProvision,
    };
    use std::{collections::BTreeSet, path::PathBuf};
    use usagi_core::domain::{
        agent::{LaunchMode, LaunchPlan, LaunchScope},
        id::{
            AgentRuntimeId, DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef,
            WorkspaceId, WorktreeId,
        },
    };

    struct Adapter {
        profile: AgentProfile,
    }
    impl AgentProfileCatalog for Adapter {
        fn find(&self, id: &AgentProfileId) -> Option<AgentProfile> {
            (id == &self.profile.id).then(|| self.profile.clone())
        }
    }
    impl AgentAdapter for Adapter {
        fn resolve(&mut self, request: &LaunchRequest) -> Result<ResolvedLaunch, AdapterError> {
            Ok(ResolvedLaunch {
                snapshot: DurableLaunchSnapshot::new(
                    request.clone(),
                    LaunchPlan::new(
                        request.profile_id.clone(),
                        self.profile.revision,
                        "fake",
                        vec![],
                        [],
                        PathBuf::from("."),
                    )
                    .unwrap(),
                ),
                provision: SpawnProvision::new([], vec![]),
                provider_resume: request.provider_resume.clone(),
            })
        }
    }
    #[derive(Default)]
    struct Store;
    impl RuntimeStore for Store {
        fn save(&mut self, _: RuntimeStoreSnapshot) -> Result<(), ()> {
            Ok(())
        }
    }
    struct Spawner;
    impl PtySpawner for Spawner {
        fn spawn(
            &mut self,
            _: &DurableLaunchSnapshot,
            _: &SpawnProvision,
            _: &usagi_core::domain::id::TerminalRef,
        ) -> Result<super::super::generation::ProcessIdentity, SpawnFailure> {
            Ok(ProcessIdentity {
                pid: 1,
                start_identity: "one".into(),
                process_group: 1,
            })
        }
    }
    struct FailingSpawner;
    impl PtySpawner for FailingSpawner {
        fn spawn(
            &mut self,
            _: &DurableLaunchSnapshot,
            _: &SpawnProvision,
            _: &usagi_core::domain::id::TerminalRef,
        ) -> Result<super::super::generation::ProcessIdentity, SpawnFailure> {
            Err(SpawnFailure::Definite)
        }
    }

    #[test]
    fn default_orchestrator_has_no_phase_leases() {
        assert!(Orchestrator::default().phases.is_empty());
    }

    fn setup() -> (
        RuntimeCoordinator,
        AdapterRegistry,
        RuntimeAuthorization,
        LaunchRequest,
    ) {
        let scope = LaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        };
        let generation = DaemonGeneration::new();
        let runtime = AgentRuntimeRef::new(
            AgentRuntimeId::new(),
            TerminalRef {
                daemon_generation: generation,
                terminal_id: TerminalId::new(),
                workspace_id: scope.workspace_id,
                session_id: scope.session_id,
                worktree_id: scope.worktree_id,
            },
            scope.session_id,
        )
        .unwrap();
        let authorization = RuntimeAuthorization {
            runtime,
            operation: CompletionFence {
                workspace_id: scope.workspace_id,
                session_id: scope.session_id,
                operation_id: OperationId::new(),
                owner_daemon_generation: generation,
                execution_attempt: 1,
                lifecycle_attempt: 1,
                expected_revision: 1,
            },
            mcp_allowed: true,
        };
        let request = LaunchRequest {
            profile_id: AgentProfileId::new("fake").unwrap(),
            mode: LaunchMode::Interactive,
            model: None,
            resume: true,
            provider_resume: None,
            initial_prompt: None,
            scope,
            required_capabilities: [AgentCapability::McpWiring]
                .into_iter()
                .collect::<BTreeSet<_>>(),
        };
        let mut registry = AdapterRegistry::new();
        let profile = AgentProfile::new(
            request.profile_id.clone(),
            "fake",
            1,
            [
                AgentCapability::Resume,
                AgentCapability::McpWiring,
                AgentCapability::PhaseReporting,
            ],
            [LaunchMode::Interactive],
        );
        registry
            .register(profile.clone(), Box::new(Adapter { profile }))
            .unwrap();
        (
            RuntimeCoordinator::new(2, 32, 1),
            registry,
            authorization,
            request,
        )
    }
    #[test]
    fn phase_token_sequence_generation_and_authorization_are_fenced() {
        let (mut runtime, mut registry, auth, request) = setup();
        let mut orchestration = Orchestrator::new();
        let mut store = Store;
        let mut spawner = Spawner;
        orchestration
            .launch(
                &mut runtime,
                &mut registry,
                &auth,
                &request,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut spawner,
                None,
            )
            .unwrap();
        orchestration
            .enable_phase_reporting(&runtime, &registry, &auth, "private".into())
            .unwrap();
        assert_eq!(
            orchestration.report_phase(&runtime, &auth, "private", 1, AgentPhase::Running),
            Ok(PhaseResult::Applied)
        );
        assert_eq!(
            orchestration.report_phase(&runtime, &auth, "private", 1, AgentPhase::Waiting),
            Ok(PhaseResult::DuplicateOrStale)
        );
        assert_eq!(
            orchestration.report_phase(&runtime, &auth, "replayed", 2, AgentPhase::Waiting),
            Err(PhaseRejection::InvalidToken)
        );
        let mut foreign = auth.clone();
        foreign.operation.owner_daemon_generation = DaemonGeneration::new();
        assert_eq!(
            orchestration.report_phase(&runtime, &foreign, "private", 2, AgentPhase::Waiting),
            Err(PhaseRejection::Unauthorized)
        );
    }
    #[test]
    fn resume_and_reclaim_fail_closed_without_verified_provenance_identity_and_secret() {
        let (mut runtime, mut registry, auth, request) = setup();
        let mut orchestration = Orchestrator::new();
        let mut store = Store;
        let mut spawner = Spawner;
        orchestration
            .launch(
                &mut runtime,
                &mut registry,
                &auth,
                &request,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut spawner,
                None,
            )
            .unwrap();
        assert_eq!(
            orchestration.resume(&runtime, &registry, &auth),
            ResumeDecision::Compatible
        );
        assert_eq!(
            orchestration.reclaim(
                &mut runtime,
                &auth,
                ProcessObservation::Unknown,
                SecretAvailability::Available,
                &mut store
            ),
            Ok(ReclaimDecision::ManualActionRequired)
        );
        assert_eq!(
            orchestration.reclaim(
                &mut runtime,
                &auth,
                ProcessObservation::Gone,
                SecretAvailability::LostOrUnknown,
                &mut store
            ),
            Ok(ReclaimDecision::ManualActionRequired)
        );
        assert_eq!(
            orchestration.reclaim(
                &mut runtime,
                &auth,
                ProcessObservation::Gone,
                SecretAvailability::Available,
                &mut store
            ),
            Ok(ReclaimDecision::Reclaimed)
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One production-symbol test enumerates every fail-closed orchestration route.
    fn registry_launch_phase_resume_and_reclaim_error_routes_are_total() {
        let (mut runtime, mut registry, auth, request) = setup();
        let profile = registry.profile(&request.profile_id).unwrap();
        assert_eq!(
            registry.register(
                profile.clone(),
                Box::new(Adapter {
                    profile: profile.clone()
                })
            ),
            Err(RegistryError::DuplicateProfile)
        );
        let mismatched = AgentProfile::new(
            AgentProfileId::new("mismatch").unwrap(),
            "mismatch",
            1,
            [],
            [LaunchMode::Interactive],
        );
        assert_eq!(
            registry.register(
                mismatched,
                Box::new(Adapter {
                    profile: profile.clone(),
                }),
            ),
            Err(RegistryError::ProfileMismatch)
        );
        assert_eq!(
            registry.profile(&AgentProfileId::new("unknown").unwrap()),
            Err(RegistryError::UnknownProfile)
        );

        let mut orchestration = Orchestrator::new();
        let mut store = Store;
        let mut spawner = Spawner;
        let mut foreign = auth.clone();
        foreign.operation.owner_daemon_generation = DaemonGeneration::new();
        assert_eq!(
            orchestration.launch(
                &mut runtime,
                &mut registry,
                &foreign,
                &request,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut spawner,
                None,
            ),
            Err(OrchestrationError::Unauthorized)
        );
        let mut no_mcp = auth.clone();
        no_mcp.mcp_allowed = false;
        assert_eq!(
            orchestration.launch(
                &mut runtime,
                &mut registry,
                &no_mcp,
                &request,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut spawner,
                None,
            ),
            Err(OrchestrationError::Unauthorized)
        );
        let mut unknown = request.clone();
        unknown.profile_id = AgentProfileId::new("unknown").unwrap();
        assert_eq!(
            orchestration.launch(
                &mut runtime,
                &mut registry,
                &auth,
                &unknown,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut spawner,
                None,
            ),
            Err(OrchestrationError::UnknownProfile)
        );
        assert_eq!(
            orchestration.enable_phase_reporting(&runtime, &registry, &auth, "token".into()),
            Err(PhaseRejection::UnknownRuntime)
        );

        let (mut failed_runtime, mut failed_registry, failed_auth, failed_request) = setup();
        assert!(matches!(
            orchestration.launch(
                &mut failed_runtime,
                &mut failed_registry,
                &failed_auth,
                &failed_request,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut FailingSpawner,
                None,
            ),
            Err(OrchestrationError::Runtime(_))
        ));

        orchestration
            .launch(
                &mut runtime,
                &mut registry,
                &auth,
                &request,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut spawner,
                None,
            )
            .unwrap();
        assert_eq!(
            orchestration.enable_phase_reporting(&runtime, &registry, &foreign, "token".into()),
            Err(PhaseRejection::Unauthorized)
        );
        registry.adapters[0].0.revision += 1;
        assert_eq!(
            orchestration.enable_phase_reporting(&runtime, &registry, &auth, "token".into()),
            Err(PhaseRejection::CapabilityUnavailable)
        );
        registry.adapters[0].0 = profile.clone();
        registry.adapters[0]
            .0
            .capabilities
            .remove(&AgentCapability::PhaseReporting);
        assert_eq!(
            orchestration.enable_phase_reporting(&runtime, &registry, &auth, "token".into()),
            Err(PhaseRejection::CapabilityUnavailable)
        );
        registry.adapters[0].0 = profile.clone();
        assert_eq!(
            Orchestrator::new().report_phase(&runtime, &auth, "missing", 1, AgentPhase::Running,),
            Err(PhaseRejection::InvalidToken)
        );
        orchestration
            .enable_phase_reporting(&runtime, &registry, &auth, "token".into())
            .unwrap();
        orchestration
            .phases
            .get_mut(&auth.runtime.agent_runtime_id.as_str())
            .unwrap()
            .operation = foreign.operation.clone();
        assert_eq!(
            orchestration.report_phase(&runtime, &auth, "token", 1, AgentPhase::Running),
            Err(PhaseRejection::InvalidToken)
        );

        let mut unknown_auth = auth.clone();
        unknown_auth.runtime.agent_runtime_id = AgentRuntimeId::new();
        assert_eq!(
            orchestration.report_phase(&runtime, &unknown_auth, "token", 2, AgentPhase::Running,),
            Err(PhaseRejection::UnknownRuntime)
        );
        assert_eq!(
            orchestration.resume(&runtime, &registry, &unknown_auth),
            ResumeDecision::ManualActionRequired
        );
        assert_eq!(
            orchestration.resume(&runtime, &registry, &foreign),
            ResumeDecision::ManualActionRequired
        );
        registry.adapters[0].0.revision += 1;
        assert_eq!(
            orchestration.resume(&runtime, &registry, &auth),
            ResumeDecision::ManualActionRequired
        );
        registry.adapters[0].0 = profile.clone();
        registry.adapters[0]
            .0
            .capabilities
            .remove(&AgentCapability::Resume);
        assert_eq!(
            orchestration.resume(&runtime, &registry, &auth),
            ResumeDecision::ManualActionRequired
        );
        registry.adapters[0].0 = profile;

        let (mut no_resume_runtime, mut no_resume_registry, no_resume_auth, mut no_resume_request) =
            setup();
        no_resume_request.resume = false;
        no_resume_registry.adapters[0]
            .0
            .capabilities
            .remove(&AgentCapability::Resume);
        orchestration
            .launch(
                &mut no_resume_runtime,
                &mut no_resume_registry,
                &no_resume_auth,
                &no_resume_request,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut spawner,
                None,
            )
            .unwrap();
        assert_eq!(
            orchestration.resume(&no_resume_runtime, &no_resume_registry, &no_resume_auth),
            ResumeDecision::ManualActionRequired
        );

        let (mut orphan_runtime, mut orphan_registry, orphan_auth, orphan_request) = setup();
        orchestration
            .launch(
                &mut orphan_runtime,
                &mut orphan_registry,
                &orphan_auth,
                &orphan_request,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut spawner,
                None,
            )
            .unwrap();
        let process = orphan_runtime
            .record_for(&orphan_auth.runtime)
            .unwrap()
            .process
            .clone()
            .unwrap();
        assert_eq!(
            orchestration.reclaim(
                &mut orphan_runtime,
                &orphan_auth,
                ProcessObservation::VerifiedAlive(process),
                SecretAvailability::Available,
                &mut store,
            ),
            Ok(ReclaimDecision::OrphanNeedsAction)
        );
        assert_eq!(
            orchestration.resume(&orphan_runtime, &orphan_registry, &orphan_auth),
            ResumeDecision::Compatible
        );

        runtime.exit(&auth.runtime, 0, &mut store).unwrap();
        assert_eq!(
            orchestration.report_phase(&runtime, &auth, "token", 2, AgentPhase::Running),
            Err(PhaseRejection::Exited)
        );
        assert_eq!(
            orchestration.enable_phase_reporting(&runtime, &registry, &auth, "token".into()),
            Err(PhaseRejection::Exited)
        );
        assert_eq!(
            orchestration.resume(&runtime, &registry, &auth),
            ResumeDecision::ManualActionRequired
        );
        assert_eq!(
            orchestration.reclaim(
                &mut runtime,
                &unknown_auth,
                ProcessObservation::Gone,
                SecretAvailability::Available,
                &mut store,
            ),
            Err(OrchestrationError::UnknownRuntime)
        );
    }
}
