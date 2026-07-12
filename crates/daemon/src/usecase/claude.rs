//! Claude-specific adapter for the product-neutral agent launch contract.
//!
//! Claude CLI spelling and its private configuration materialization stay in
//! this module.  The daemon runtime receives only the validated, non-secret
//! [`DurableLaunchSnapshot`] through its existing [`LaunchResolver`] port.

use std::{collections::BTreeSet, path::PathBuf};

use usagi_core::{
    domain::agent::{
        AgentCapability, AgentProfile, AgentProfileId, DurableLaunchSnapshot,
        EnvironmentVariableName, LaunchMode, LaunchPlan, LaunchRequest, LaunchScope,
        LaunchValidationError,
    },
    usecase::agent::{AgentProfileCatalog, validate_request},
};

use super::runtime::LaunchResolver;

const PROFILE_ID: &str = "claude";
const PROFILE_REVISION: u32 = 1;

/// The non-secret data that is permitted to cross Claude provisioning into a
/// launch plan.  Claude hook/config contents and all secret values remain in
/// the provisioner implementation and never enter the durable snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeProvision {
    working_directory: PathBuf,
    environment_allowlist: BTreeSet<EnvironmentVariableName>,
}

impl ClaudeProvision {
    /// Creates the scope-limited, non-secret part of a completed provision.
    #[must_use]
    pub fn new(
        working_directory: PathBuf,
        environment_allowlist: impl IntoIterator<Item = EnvironmentVariableName>,
    ) -> Self {
        Self {
            working_directory,
            environment_allowlist: environment_allowlist.into_iter().collect(),
        }
    }
}

/// Claude-only provision boundary.  An implementation may write Claude config,
/// MCP, and hook files, but must not return their paths, contents, or secrets.
pub trait ClaudeProvisioner {
    type Error;

    /// # Errors
    ///
    /// Returns the provisioner's Claude-specific materialization failure.
    fn provision(&mut self, scope: &LaunchScope) -> Result<ClaudeProvision, Self::Error>;
}

/// Typed pre-spawn failure for Claude's private provisioning step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeProvisionError<E> {
    Failed(E),
    ScopeMismatch,
}

/// Runs Claude-only provisioning before the runtime reservation.  The caller
/// supplies the resulting adapter to [`super::runtime::RuntimeCoordinator`],
/// so a provisioning error cannot create a terminal or spawn a process.
///
/// # Errors
///
/// Returns a typed provisioning failure before a runtime reservation is made.
pub fn preflight<P: ClaudeProvisioner>(
    provisioner: &mut P,
    request: &LaunchRequest,
) -> Result<ClaudeAdapter, ClaudeProvisionError<P::Error>> {
    let provision = provisioner
        .provision(&request.scope)
        .map_err(ClaudeProvisionError::Failed)?;
    (!provision.working_directory.as_os_str().is_empty())
        .then_some(ClaudeAdapter { provision })
        .ok_or(ClaudeProvisionError::ScopeMismatch)
}

/// Renderer and runtime resolver for the `claude` profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeAdapter {
    provision: ClaudeProvision,
}

impl ClaudeAdapter {
    /// The static Claude profile used by request validation and provenance.
    ///
    /// # Panics
    ///
    /// Panics only if this module's compile-time `claude` profile ID ceases to
    /// satisfy the core canonical-ID contract.
    #[must_use]
    pub fn profile() -> AgentProfile {
        AgentProfile::new(
            AgentProfileId::new(PROFILE_ID).expect("constant profile ID is canonical"),
            "Claude",
            PROFILE_REVISION,
            [
                AgentCapability::Resume,
                AgentCapability::InitialPrompt,
                AgentCapability::Headless,
                AgentCapability::McpWiring,
            ],
            [LaunchMode::Interactive, LaunchMode::Headless],
        )
    }

    /// Renders a shell-neutral Claude plan after validating product support.
    ///
    /// # Errors
    ///
    /// Returns the core's typed validation rejection for unsupported request
    /// intent or an invalid public launch plan.
    pub fn render(
        &self,
        request: &LaunchRequest,
    ) -> Result<DurableLaunchSnapshot, LaunchValidationError> {
        let profile = validate_request(self, request)?;
        let mut argv = Vec::new();
        if request.mode == LaunchMode::Headless {
            argv.push("--print".into());
        }
        if request.resume {
            argv.push("--continue".into());
        }
        if let Some(model) = &request.model {
            argv.extend(["--model".into(), model.as_str().into()]);
        }
        if let Some(prompt) = &request.initial_prompt {
            argv.push(prompt.clone());
        }
        let plan = LaunchPlan::new(
            profile.id,
            profile.revision,
            "claude",
            argv,
            self.provision.environment_allowlist.clone(),
            self.provision.working_directory.clone(),
        )?;
        Ok(DurableLaunchSnapshot::new(request.clone(), plan))
    }
}

impl AgentProfileCatalog for ClaudeAdapter {
    fn find(&self, profile_id: &AgentProfileId) -> Option<AgentProfile> {
        let profile = Self::profile();
        (profile.id == *profile_id).then_some(profile)
    }
}

impl LaunchResolver for ClaudeAdapter {
    fn resolve(
        &mut self,
        request: &LaunchRequest,
    ) -> Result<DurableLaunchSnapshot, LaunchValidationError> {
        self.render(request)
    }
}

#[cfg(test)]
mod tests {
    use usagi_core::domain::{
        agent::{LaunchScope, ModelSelector},
        id::{
            AgentRuntimeId, AgentRuntimeRef, CompletionFence, DaemonGeneration, OperationId,
            SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
        },
    };

    use super::*;
    use crate::usecase::{
        generation::ProcessIdentity,
        runtime::{
            PtySpawner, RuntimeCoordinator, RuntimeState, RuntimeStore, RuntimeStoreSnapshot,
            SpawnFailure,
        },
        terminal::Geometry,
    };

    #[derive(Default)]
    struct FakeProvisioner {
        provision: Option<ClaudeProvision>,
        fail: bool,
        scopes: Vec<LaunchScope>,
    }

    impl ClaudeProvisioner for FakeProvisioner {
        type Error = &'static str;

        fn provision(&mut self, scope: &LaunchScope) -> Result<ClaudeProvision, Self::Error> {
            self.scopes.push(scope.clone());
            if self.fail {
                Err("config unavailable")
            } else {
                Ok(self.provision.clone().expect("test provision"))
            }
        }
    }

    fn request() -> LaunchRequest {
        LaunchRequest {
            profile_id: AgentProfileId::new(PROFILE_ID).unwrap(),
            mode: LaunchMode::Headless,
            model: Some(ModelSelector::new("sonnet").unwrap()),
            resume: true,
            initial_prompt: Some("inspect this workspace".into()),
            scope: LaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: SessionId::new(),
                worktree_id: WorktreeId::new(),
            },
            required_capabilities: [AgentCapability::McpWiring].into_iter().collect(),
        }
    }

    fn provision() -> ClaudeProvision {
        ClaudeProvision::new(
            PathBuf::from("/workspace"),
            [EnvironmentVariableName::new("CLAUDE_CONFIG_DIR").unwrap()],
        )
    }

    #[test]
    fn preflight_scopes_claude_materialization_before_runtime_reservation() {
        let request = request();
        let mut provisioner = FakeProvisioner {
            provision: Some(provision()),
            ..FakeProvisioner::default()
        };
        let adapter = preflight(&mut provisioner, &request).unwrap();
        assert_eq!(provisioner.scopes, vec![request.scope.clone()]);
        let plan = adapter.render(&request).unwrap().plan;
        assert_eq!(plan.program, "claude");
        assert_eq!(
            plan.argv,
            [
                "--print",
                "--continue",
                "--model",
                "sonnet",
                "inspect this workspace"
            ]
        );
        assert_eq!(plan.environment_allowlist.len(), 1);
    }

    #[test]
    fn renderer_rejects_unknown_profile_and_unsupported_capability() {
        let mut request = request();
        let adapter = ClaudeAdapter {
            provision: provision(),
        };
        request.profile_id = AgentProfileId::new("other").unwrap();
        assert_eq!(
            adapter.render(&request),
            Err(LaunchValidationError::UnknownProfile {
                profile_id: AgentProfileId::new("other").unwrap()
            })
        );
        request.profile_id = AgentProfileId::new(PROFILE_ID).unwrap();
        request
            .required_capabilities
            .insert(AgentCapability::PhaseReporting);
        assert_eq!(
            adapter.render(&request),
            Err(LaunchValidationError::UnsupportedCapability {
                capability: AgentCapability::PhaseReporting
            })
        );
    }

    #[test]
    fn failed_or_empty_provision_is_a_typed_pre_spawn_failure() {
        let request = request();
        let mut failed = FakeProvisioner {
            fail: true,
            ..FakeProvisioner::default()
        };
        assert_eq!(
            preflight(&mut failed, &request),
            Err(ClaudeProvisionError::Failed("config unavailable"))
        );
        let mut empty = FakeProvisioner {
            provision: Some(ClaudeProvision::new(PathBuf::new(), [])),
            ..FakeProvisioner::default()
        };
        assert_eq!(
            preflight(&mut empty, &request),
            Err(ClaudeProvisionError::ScopeMismatch)
        );
    }

    #[derive(Default)]
    struct Store(Vec<RuntimeStoreSnapshot>);

    impl RuntimeStore for Store {
        type Error = ();

        fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), Self::Error> {
            self.0.push(snapshot);
            Ok(())
        }
    }

    #[derive(Default)]
    struct Spawner {
        seen_programs: Vec<String>,
    }

    impl PtySpawner for Spawner {
        fn spawn(
            &mut self,
            launch: &DurableLaunchSnapshot,
            _: &TerminalRef,
        ) -> Result<ProcessIdentity, SpawnFailure> {
            self.seen_programs.push(launch.plan.program.clone());
            Ok(ProcessIdentity {
                pid: 42,
                start_identity: "fixture".into(),
                process_group: 42,
            })
        }
    }

    #[test]
    fn claude_adapter_launches_through_the_runtime_reservation_port() {
        let request = request();
        let mut provisioner = FakeProvisioner {
            provision: Some(provision()),
            ..FakeProvisioner::default()
        };
        let mut adapter = preflight(&mut provisioner, &request).unwrap();
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
        let mut coordinator = RuntimeCoordinator::new(1, 1024, 1);
        let mut store = Store::default();
        let mut spawner = Spawner::default();
        coordinator
            .launch(
                &request,
                runtime.clone(),
                fence,
                Geometry { cols: 80, rows: 24 },
                &mut adapter,
                &mut store,
                &mut spawner,
            )
            .unwrap();
        assert_eq!(store.0.len(), 2);
        assert_eq!(
            coordinator.snapshot().records[0].state,
            RuntimeState::Running
        );
        assert_eq!(spawner.seen_programs, ["claude"]);
    }
}
