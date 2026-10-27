//! Product-neutral daemon orchestration for agent runtimes.
//!
//! Product identity is resolved only through [`AdapterRegistry`].  This module
//! never branches on a product name and never retains adapter payloads,
//! credentials, rendered argv, or phase-hook input.

#![allow(clippy::missing_errors_doc, clippy::too_many_arguments)] // Port methods expose injected runtime dependencies and typed fences.

use usagi_core::{
    domain::{
        agent::{AgentCapability, AgentProfile, AgentProfileId, LaunchRequest},
        id::{AgentRuntimeRef, CompletionFence},
    },
    infrastructure::runtime_model::supported_agent_runtimes,
    usecase::agent::AgentProfileCatalog,
};

use super::{
    agy::{AgyAdapter, AgyProvisioner},
    claude::{ClaudeAdapter, ClaudeProvisioner},
    codex::{CodexAdapter, CodexProvisioner},
    runtime::{AgentAdapter, PtySpawner, RuntimeCoordinator, RuntimeError, RuntimeStore},
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
        A: AgyProvisioner + Send + 'static,
    >(
        &mut self,
        codex: CodexAdapter<C>,
        claude: ClaudeAdapter<L>,
        agy: AgyAdapter<A>,
    ) -> Result<(), RegistryError> {
        self.register(claude.profile().clone(), Box::new(claude))?;
        self.register(codex.profile().clone(), Box::new(codex))?;
        self.register(agy.profile().clone(), Box::new(agy))?;
        let matches_catalog = self
            .profile_ids()
            .eq(supported_agent_runtimes().map(|runtime| runtime.id));
        if matches_catalog {
            Ok(())
        } else {
            Err(RegistryError::ProfileCatalogMismatch)
        }
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

    /// Registered profile IDs in registration order.
    #[must_use]
    pub fn profile_ids(&self) -> impl ExactSizeIterator<Item = &str> {
        self.adapters.iter().map(|(profile, _)| profile.id.as_str())
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
    ProfileCatalogMismatch,
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
    pub launch_provenance: Option<usagi_core::domain::agent::AgentLaunchProvenance>,
    pub user_decision_owner: Option<usagi_core::domain::user_decision::UserDecisionOwner>,
    pub user_decision_owner_aliases: Vec<usagi_core::domain::user_decision::UserDecisionOwner>,
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

/// Resolves a profile through the shared adapter registry and launches it once.
/// MCP materialization remains adapter-scoped and requires explicit authorization.
pub fn launch_with_semantic(
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
    launch_with_semantic_superseding(
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
    launch_with_semantic_superseding(
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
    runtime
        .launch_with_provenance(
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
            authorization.launch_provenance.clone(),
            authorization.user_decision_owner.clone(),
            authorization.user_decision_owner_aliases.clone(),
        )
        .map_err(OrchestrationError::Runtime)
}

#[derive(Debug, PartialEq, Eq)]
pub enum OrchestrationError {
    Unauthorized,
    UnknownProfile,
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
        agent::{DurableLaunchSnapshot, LaunchMode, LaunchPlan, LaunchScope},
        id::{
            AgentRuntimeId, DaemonGeneration, OperationId, SessionId, TerminalId, TerminalRef,
            WorkspaceId, WorktreeId,
        },
    };
    use usagi_core::infrastructure::runtime_model::supported_agent_runtimes;

    struct CodexNever;
    impl CodexProvisioner for CodexNever {
        fn provision(
            &mut self,
            _: &crate::usecase::runtime::ProvisionContext,
        ) -> Result<
            crate::usecase::codex::CodexProvision,
            crate::usecase::codex::CodexProvisionFailure,
        > {
            Err(crate::usecase::codex::CodexProvisionFailure::ExecutableUnavailable)
        }
    }

    struct ClaudeNever;
    impl ClaudeProvisioner for ClaudeNever {
        fn provision(
            &mut self,
            _: &crate::usecase::runtime::ProvisionContext,
        ) -> Result<
            crate::usecase::claude::ClaudeProvision,
            crate::usecase::claude::ClaudeProvisionFailure,
        > {
            Err(crate::usecase::claude::ClaudeProvisionFailure::ExecutableUnavailable)
        }
    }

    struct AgyNever;
    impl AgyProvisioner for AgyNever {
        fn provision(
            &mut self,
            _: &crate::usecase::runtime::ProvisionContext,
        ) -> Result<crate::usecase::agy::AgyProvision, crate::usecase::agy::AgyProvisionFailure>
        {
            Err(crate::usecase::agy::AgyProvisionFailure::ExecutableUnavailable)
        }
    }

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
    fn supported_registry_profiles_match_the_shared_runtime_catalog() {
        let mut registry = AdapterRegistry::new();
        registry
            .register_supported(
                CodexAdapter::new(CodexNever),
                ClaudeAdapter::new(ClaudeNever),
                AgyAdapter::new(AgyNever),
            )
            .unwrap();
        assert_eq!(
            registry.profile_ids().collect::<Vec<_>>(),
            supported_agent_runtimes()
                .map(|runtime| runtime.id)
                .collect::<Vec<_>>()
        );

        let (_, mut registry_with_extra_profile, _, request) = setup();
        assert_eq!(
            registry_with_extra_profile.register_supported(
                CodexAdapter::new(CodexNever),
                ClaudeAdapter::new(ClaudeNever),
                AgyAdapter::new(AgyNever),
            ),
            Err(RegistryError::ProfileCatalogMismatch)
        );

        let context = crate::usecase::runtime::ProvisionContext::from_request(&request);
        assert!(matches!(
            CodexNever.provision(&context),
            Err(crate::usecase::codex::CodexProvisionFailure::ExecutableUnavailable)
        ));
        assert!(matches!(
            ClaudeNever.provision(&context),
            Err(crate::usecase::claude::ClaudeProvisionFailure::ExecutableUnavailable)
        ));
        assert!(matches!(
            AgyNever.provision(&context),
            Err(crate::usecase::agy::AgyProvisionFailure::ExecutableUnavailable)
        ));
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
            launch_provenance: None,
            user_decision_owner: None,
            user_decision_owner_aliases: Vec::new(),
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
    #[allow(clippy::too_many_lines)] // One production-symbol test enumerates every fail-closed orchestration route.
    fn registry_and_launch_preserve_authorization_and_failure_routes() {
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

        let mut store = Store;
        let mut spawner = Spawner;
        let mut foreign = auth.clone();
        foreign.operation.owner_daemon_generation = DaemonGeneration::new();
        assert_eq!(
            launch_with_semantic(
                &mut runtime,
                &mut registry,
                &foreign,
                &request,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut spawner,
                None,
                "test-launch".to_owned(),
            ),
            Err(OrchestrationError::Unauthorized)
        );
        let mut no_mcp = auth.clone();
        no_mcp.mcp_allowed = false;
        assert_eq!(
            launch_with_semantic(
                &mut runtime,
                &mut registry,
                &no_mcp,
                &request,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut spawner,
                None,
                "test-launch".to_owned(),
            ),
            Err(OrchestrationError::Unauthorized)
        );
        let mut unknown = request.clone();
        unknown.profile_id = AgentProfileId::new("unknown").unwrap();
        assert_eq!(
            launch_with_semantic(
                &mut runtime,
                &mut registry,
                &auth,
                &unknown,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut spawner,
                None,
                "test-launch".to_owned(),
            ),
            Err(OrchestrationError::UnknownProfile)
        );
        let (mut failed_runtime, mut failed_registry, failed_auth, failed_request) = setup();
        assert!(matches!(
            launch_with_semantic(
                &mut failed_runtime,
                &mut failed_registry,
                &failed_auth,
                &failed_request,
                Geometry { cols: 80, rows: 24 },
                &mut store,
                &mut FailingSpawner,
                None,
                "test-launch".to_owned(),
            ),
            Err(OrchestrationError::Runtime(_))
        ));

        launch_with_semantic(
            &mut runtime,
            &mut registry,
            &auth,
            &request,
            Geometry { cols: 80, rows: 24 },
            &mut store,
            &mut spawner,
            None,
            "test-launch".to_owned(),
        )
        .unwrap();
        assert_eq!(
            runtime.record_for(&auth.runtime).unwrap().state,
            crate::usecase::runtime::RuntimeState::Running
        );
        assert_eq!(
            runtime
                .record_for(&auth.runtime)
                .unwrap()
                .semantic_key
                .as_deref(),
            Some("test-launch")
        );
    }
}
