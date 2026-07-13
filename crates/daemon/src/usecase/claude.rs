//! Claude-specific launch adapter.
//!
//! Claude owns its CLI grammar and private config/MCP/hook materialization;
//! the common runtime owns the durable snapshot and PTY lifecycle boundary.

use std::{collections::BTreeSet, path::PathBuf};

use usagi_core::{
    domain::agent::{
        AgentCapability, AgentProfile, AgentProfileId, DurableLaunchSnapshot,
        EnvironmentVariableName, LaunchMode, LaunchPlan, LaunchRequest, LaunchValidationError,
    },
    usecase::agent::{AgentProfileCatalog, validate_request, validate_snapshot},
};

use super::runtime::{
    AdapterError, AgentAdapter, ProvisionContext, ResolvedLaunch, SpawnProvision,
};

const PROFILE_NAME: &str = "claude";
const PROFILE_REVISION: u32 = 1;

/// Claude's product-private provisioning result.
///
/// Only the public plan inputs and common ephemeral [`SpawnProvision`] cross
/// the adapter boundary. Config paths, hook payloads, and secret values must
/// remain inside the provisioner implementation.
pub struct ClaudeProvision {
    pub working_directory: PathBuf,
    pub environment_allowlist: BTreeSet<EnvironmentVariableName>,
    pub spawn: SpawnProvision,
}

/// Typed pre-spawn failures from the Claude provisioning boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeProvisionFailure {
    ExecutableUnavailable,
    MaterializationFailed,
}

/// Materializes Claude-private config, MCP, and hook artifacts for one scope.
pub trait ClaudeProvisioner {
    /// # Errors
    ///
    /// Returns a typed failure before the common runtime reserves a terminal.
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<ClaudeProvision, ClaudeProvisionFailure>;
}

/// An [`AgentAdapter`] for the code-defined `claude` profile.
#[derive(Debug)]
pub struct ClaudeAdapter<P> {
    provisioner: P,
    profile: AgentProfile,
}

impl<P> ClaudeAdapter<P> {
    #[must_use]
    pub fn new(provisioner: P) -> Self {
        Self::with_revision(provisioner, PROFILE_REVISION)
    }

    /// # Panics
    ///
    /// Panics only if the hard-coded `claude` profile ID stops satisfying the
    /// core canonical-ID contract.
    #[must_use]
    pub fn with_revision(provisioner: P, revision: u32) -> Self {
        Self {
            provisioner,
            profile: AgentProfile::new(
                AgentProfileId::new(PROFILE_NAME).expect("literal profile ID is canonical"),
                "Claude",
                revision,
                [
                    AgentCapability::Resume,
                    AgentCapability::InitialPrompt,
                    AgentCapability::Headless,
                    AgentCapability::PhaseReporting,
                    AgentCapability::McpWiring,
                ],
                [LaunchMode::Interactive, LaunchMode::Headless],
            ),
        }
    }

    #[must_use]
    pub fn profile(&self) -> &AgentProfile {
        &self.profile
    }

    /// # Errors
    ///
    /// Returns a typed rejection when a restored snapshot is incompatible with
    /// the adapter's static profile revision.
    pub fn validate_snapshot(
        &self,
        snapshot: &DurableLaunchSnapshot,
    ) -> Result<AgentProfile, LaunchValidationError> {
        validate_snapshot(self, snapshot)
    }
}

impl<P> AgentProfileCatalog for ClaudeAdapter<P> {
    fn find(&self, profile_id: &AgentProfileId) -> Option<AgentProfile> {
        (profile_id == &self.profile.id).then(|| self.profile.clone())
    }
}

impl<P: ClaudeProvisioner> AgentAdapter for ClaudeAdapter<P> {
    fn resolve(&mut self, request: &LaunchRequest) -> Result<ResolvedLaunch, AdapterError> {
        let profile = validate_request(self, request).map_err(AdapterError::Validation)?;
        if request.mode == LaunchMode::Headless && request.initial_prompt.is_none() {
            return Err(AdapterError::Validation(LaunchValidationError::EmptyPrompt));
        }
        let provision = self
            .provisioner
            .provision(&ProvisionContext::from_request(request))
            .map_err(|failure| match failure {
                ClaudeProvisionFailure::ExecutableUnavailable => {
                    AdapterError::ExecutableUnavailable
                }
                ClaudeProvisionFailure::MaterializationFailed => AdapterError::ProvisionFailed,
            })?;
        let plan = render_plan(request, &profile, &provision).map_err(AdapterError::Validation)?;
        Ok(ResolvedLaunch {
            snapshot: DurableLaunchSnapshot::new(request.clone(), plan),
            provision: provision.spawn,
        })
    }
}

fn render_plan(
    request: &LaunchRequest,
    profile: &AgentProfile,
    provision: &ClaudeProvision,
) -> Result<LaunchPlan, LaunchValidationError> {
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
    LaunchPlan::new(
        profile.id.clone(),
        profile.revision,
        "claude",
        argv,
        provision.environment_allowlist.clone(),
        provision.working_directory.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::{
        agent::{LaunchScope, ModelSelector},
        id::{SessionId, WorkspaceId, WorktreeId},
    };

    struct FakeProvisioner(Option<Result<ClaudeProvision, ClaudeProvisionFailure>>);

    impl ClaudeProvisioner for FakeProvisioner {
        fn provision(
            &mut self,
            _: &ProvisionContext,
        ) -> Result<ClaudeProvision, ClaudeProvisionFailure> {
            self.0.take().expect("fake provisioner called once")
        }
    }

    fn request() -> LaunchRequest {
        LaunchRequest {
            profile_id: AgentProfileId::new(PROFILE_NAME).unwrap(),
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
        ClaudeProvision {
            working_directory: PathBuf::from("/workspace"),
            environment_allowlist: [EnvironmentVariableName::new("CLAUDE_CONFIG_DIR").unwrap()]
                .into_iter()
                .collect(),
            spawn: SpawnProvision::new(
                [(
                    EnvironmentVariableName::new("CLAUDE_TOKEN").unwrap(),
                    "secret".into(),
                )],
                vec!["--settings".into(), "/scoped/claude.json".into()],
            ),
        }
    }

    #[test]
    fn renders_claude_plan_and_keeps_private_provision_outside_snapshot() {
        let mut adapter = ClaudeAdapter::new(FakeProvisioner(Some(Ok(provision()))));
        let resolved = adapter.resolve(&request()).unwrap();
        assert_eq!(resolved.snapshot.plan.program, "claude");
        assert_eq!(
            resolved.snapshot.plan.argv,
            [
                "--print",
                "--continue",
                "--model",
                "sonnet",
                "inspect this workspace"
            ]
        );
        let durable = serde_json::to_string(&resolved.snapshot).unwrap();
        assert!(!durable.contains("CLAUDE_TOKEN"));
        assert!(!durable.contains("/scoped/claude.json"));
        assert_eq!(
            resolved.provision.arguments(),
            ["--settings", "/scoped/claude.json"]
        );
    }

    #[test]
    fn rejects_missing_headless_prompt_and_provision_failures() {
        let mut missing = request();
        missing.initial_prompt = None;
        let mut adapter = ClaudeAdapter::new(FakeProvisioner(Some(Err(
            ClaudeProvisionFailure::ExecutableUnavailable,
        ))));
        assert!(matches!(
            adapter.resolve(&missing),
            Err(AdapterError::Validation(LaunchValidationError::EmptyPrompt))
        ));
        assert!(matches!(
            adapter.resolve(&request()),
            Err(AdapterError::ExecutableUnavailable)
        ));
        let mut failed = ClaudeAdapter::new(FakeProvisioner(Some(Err(
            ClaudeProvisionFailure::MaterializationFailed,
        ))));
        assert!(matches!(
            failed.resolve(&request()),
            Err(AdapterError::ProvisionFailed)
        ));
    }

    #[test]
    fn exposes_its_profile_and_validates_its_own_durable_snapshot() {
        let mut adapter = ClaudeAdapter::with_revision(FakeProvisioner(Some(Ok(provision()))), 3);
        assert_eq!(adapter.profile().id.as_str(), PROFILE_NAME);
        assert_eq!(adapter.profile().revision, 3);
        let snapshot = adapter.resolve(&request()).unwrap().snapshot;
        assert_eq!(
            adapter.validate_snapshot(&snapshot).unwrap(),
            adapter.profile().clone()
        );
    }
}
