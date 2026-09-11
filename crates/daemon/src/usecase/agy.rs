//! Google Antigravity CLI (`agy`) launch adapter.
//!
//! Antigravity owns its argv grammar and discovers MCP servers and lifecycle
//! hooks from a provider-native plugin. The provisioner materializes that
//! ephemeral integration before this adapter renders the public launch plan.

use std::{collections::BTreeSet, path::PathBuf};

use usagi_core::{
    domain::{
        agent::{
            AgentCapability, AgentProfile, AgentProfileId, DurableLaunchSnapshot,
            EnvironmentVariableName, LaunchMode, LaunchPlan, LaunchRequest, LaunchValidationError,
            ProviderKind, ProviderResumePhase, ProviderResumeRef, ProviderResumeStatus,
        },
        settings::DefaultModel,
    },
    usecase::agent::{AgentProfileCatalog, validate_request, validate_snapshot},
};

use super::runtime::{
    AdapterError, AgentAdapter, ProvisionContext, ResolvedLaunch, SpawnProvision,
};

/// Revision 2 scopes the plugin to managed launches through a private workspace.
pub const PROFILE_REVISION: u32 = 3;

/// Product-private provisioning result for one Antigravity launch.
pub struct AgyProvision {
    pub working_directory: PathBuf,
    pub environment_allowlist: BTreeSet<EnvironmentVariableName>,
    pub spawn: SpawnProvision,
    /// Scope and role instructions prepended to the first provider turn.
    pub system_prompt: String,
}

/// Typed failures raised before Antigravity is reserved or spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgyProvisionFailure {
    ExecutableUnavailable,
    MaterializationFailed,
}

/// Materializes Antigravity's MCP/hook plugin and sandbox inputs for one scope.
pub trait AgyProvisioner {
    /// # Errors
    ///
    /// Returns a typed failure when the executable, plugin, prompt, or sandbox
    /// inputs cannot be prepared.
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<AgyProvision, AgyProvisionFailure>;
}

/// Antigravity's code-defined daemon profile.
#[derive(Debug)]
pub struct AgyAdapter<P> {
    provisioner: P,
    profile: AgentProfile,
}

impl<P> AgyAdapter<P> {
    #[must_use]
    pub fn new(provisioner: P) -> Self {
        Self::with_revision(provisioner, PROFILE_REVISION)
    }

    /// # Panics
    ///
    /// Panics only if the hard-coded `agy` profile ID stops satisfying the
    /// core canonical-ID contract.
    #[must_use]
    pub fn with_revision(provisioner: P, revision: u32) -> Self {
        Self {
            provisioner,
            profile: AgentProfile::new(
                AgentProfileId::new(DefaultModel::Agy.profile_id())
                    .expect("catalog profile ID is canonical"),
                "Antigravity",
                revision,
                [
                    AgentCapability::Resume,
                    AgentCapability::InitialPrompt,
                    AgentCapability::Headless,
                    AgentCapability::PhaseReporting,
                    AgentCapability::McpWiring,
                    AgentCapability::SystemPrompt,
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
    /// this Antigravity integration revision.
    pub fn validate_snapshot(
        &self,
        snapshot: &DurableLaunchSnapshot,
    ) -> Result<AgentProfile, LaunchValidationError> {
        validate_snapshot(self, snapshot)
    }
}

impl<P> AgentProfileCatalog for AgyAdapter<P> {
    fn find(&self, profile_id: &AgentProfileId) -> Option<AgentProfile> {
        (profile_id == &self.profile.id).then(|| self.profile.clone())
    }
}

impl<P: AgyProvisioner> AgentAdapter for AgyAdapter<P> {
    fn resolve(&mut self, request: &LaunchRequest) -> Result<ResolvedLaunch, AdapterError> {
        let profile = validate_request(self, request).map_err(AdapterError::Validation)?;
        if request.mode == LaunchMode::Headless && request.initial_prompt.is_none() {
            return Err(AdapterError::Validation(LaunchValidationError::EmptyPrompt));
        }
        if request.mode == LaunchMode::Headless && request.resume {
            return Err(AdapterError::Validation(
                LaunchValidationError::UnsupportedCapability {
                    capability: AgentCapability::Resume,
                },
            ));
        }
        let mut provision = self
            .provisioner
            .provision(&ProvisionContext::from_request(request))
            .map_err(|failure| match failure {
                AgyProvisionFailure::ExecutableUnavailable => AdapterError::ExecutableUnavailable,
                AgyProvisionFailure::MaterializationFailed => AdapterError::ProvisionFailed,
            })?;
        let provider_resume =
            validate_provider_resume(request, &profile).map_err(AdapterError::Validation)?;
        if let Some(reference) = &provider_resume {
            provision.spawn.append_sensitive_arguments([
                "--conversation".to_owned(),
                reference.native_session_id.expose_sensitive().to_owned(),
            ]);
        } else {
            let prompt =
                initial_prompt(&provision.system_prompt, request.initial_prompt.as_deref());
            provision.spawn.append_sensitive_arguments([
                match request.mode {
                    LaunchMode::Interactive => "--prompt-interactive".to_owned(),
                    LaunchMode::Headless => "--print".to_owned(),
                },
                prompt,
            ]);
        }
        let plan = render_plan(request, &profile, &provision).map_err(AdapterError::Validation)?;
        let mut durable_request = request.clone();
        durable_request.provider_resume = None;
        Ok(ResolvedLaunch {
            snapshot: DurableLaunchSnapshot::new(durable_request, plan),
            provision: provision.spawn,
            provider_resume,
        })
    }
}

fn render_plan(
    request: &LaunchRequest,
    profile: &AgentProfile,
    provision: &AgyProvision,
) -> Result<LaunchPlan, LaunchValidationError> {
    let mut argv = vec![
        "--dangerously-skip-permissions".to_owned(),
        "--mode".to_owned(),
        "accept-edits".to_owned(),
    ];
    if let Some(model) = &request.model {
        argv.extend(["--model".to_owned(), model.as_str().to_owned()]);
    }
    LaunchPlan::new(
        profile.id.clone(),
        profile.revision,
        DefaultModel::Agy.command(),
        argv,
        provision.environment_allowlist.clone(),
        provision.working_directory.clone(),
    )
}

fn initial_prompt(system_prompt: &str, task: Option<&str>) -> String {
    match task {
        Some(task) => format!("{system_prompt}\n\nTask:\n{task}"),
        None => system_prompt.to_owned(),
    }
}

fn validate_provider_resume(
    request: &LaunchRequest,
    profile: &AgentProfile,
) -> Result<Option<ProviderResumeRef>, LaunchValidationError> {
    if !request.resume {
        return request
            .provider_resume
            .is_none()
            .then_some(None)
            .ok_or(LaunchValidationError::ProviderResumeMismatch);
    }
    let reference = request
        .provider_resume
        .as_ref()
        .filter(|reference| {
            reference.provider == ProviderKind::Agy
                && reference.adapter_revision == profile.revision
                && reference.scope == request.scope
        })
        .ok_or(LaunchValidationError::ProviderResumeMismatch)?;
    let mut reference = reference.clone();
    reference.last_known_status = ProviderResumeStatus::Active;
    reference.last_known_phase = Some(ProviderResumePhase::Starting);
    Ok(Some(reference))
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::{
        agent::{LaunchScope, ModelSelector, ProviderCaptureProvenance, ProviderSessionId},
        id::{SessionId, WorkspaceId, WorktreeId},
    };

    struct FakeProvisioner(Option<Result<AgyProvision, AgyProvisionFailure>>);

    impl AgyProvisioner for FakeProvisioner {
        fn provision(&mut self, _: &ProvisionContext) -> Result<AgyProvision, AgyProvisionFailure> {
            self.0.take().expect("fake provisioner called once")
        }
    }

    fn request(mode: LaunchMode) -> LaunchRequest {
        LaunchRequest {
            profile_id: AgentProfileId::new("agy").unwrap(),
            mode,
            model: Some(ModelSelector::new("gemini-3.8-flash-high").unwrap()),
            resume: false,
            provider_resume: None,
            initial_prompt: Some("inspect this workspace".to_owned()),
            scope: LaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: Some(SessionId::new()),
                worktree_id: WorktreeId::new(),
            },
            required_capabilities: [AgentCapability::McpWiring, AgentCapability::SystemPrompt]
                .into_iter()
                .collect(),
        }
    }

    fn provision() -> AgyProvision {
        AgyProvision {
            working_directory: PathBuf::from("/workspace"),
            environment_allowlist: [EnvironmentVariableName::new("GEMINI_API_KEY").unwrap()]
                .into_iter()
                .collect(),
            spawn: SpawnProvision::new(
                [(
                    EnvironmentVariableName::new("GEMINI_API_KEY").unwrap(),
                    "secret".into(),
                )],
                vec!["--plugin-materialized".into()],
            ),
            system_prompt: "obey the scoped contract".into(),
        }
    }

    fn adapter() -> AgyAdapter<FakeProvisioner> {
        AgyAdapter::new(FakeProvisioner(Some(Ok(provision()))))
    }

    #[test]
    fn renders_interactive_and_headless_argv_with_scoped_prompt() {
        let interactive = adapter()
            .resolve(&request(LaunchMode::Interactive))
            .unwrap();
        assert_eq!(interactive.snapshot.plan.program, "agy");
        assert_eq!(
            interactive.snapshot.plan.argv,
            [
                "--dangerously-skip-permissions",
                "--mode",
                "accept-edits",
                "--model",
                "gemini-3.8-flash-high",
            ]
        );
        assert_eq!(
            interactive.provision.arguments(),
            [
                "--plugin-materialized",
                "--prompt-interactive",
                "obey the scoped contract\n\nTask:\ninspect this workspace",
            ]
        );
        let durable = serde_json::to_string(&interactive.snapshot).unwrap();
        assert!(!durable.contains("secret"));
        assert!(!durable.contains("obey the scoped contract"));
        assert!(interactive.provider_resume.is_none());

        let headless = adapter().resolve(&request(LaunchMode::Headless)).unwrap();
        assert_eq!(headless.provision.arguments()[1], "--print");
    }

    #[test]
    fn interactive_launch_without_task_still_receives_the_system_contract() {
        let mut request = request(LaunchMode::Interactive);
        request.initial_prompt = None;
        let resolved = adapter().resolve(&request).unwrap();
        assert_eq!(
            &resolved.provision.arguments()[1..],
            ["--prompt-interactive", "obey the scoped contract"]
        );
    }

    #[test]
    fn exact_resume_uses_private_conversation_argument_without_new_turn() {
        let mut request = request(LaunchMode::Interactive);
        request.resume = true;
        request.initial_prompt = None;
        request.provider_resume = Some(ProviderResumeRef {
            provider: ProviderKind::Agy,
            native_session_id: ProviderSessionId::new("agy-conversation").unwrap(),
            adapter_revision: PROFILE_REVISION,
            scope: request.scope.clone(),
            provenance: ProviderCaptureProvenance::ProviderStructured,
            last_known_status: ProviderResumeStatus::Interrupted,
            last_known_phase: Some(ProviderResumePhase::Interrupted),
        });
        let resolved = adapter().resolve(&request).unwrap();
        assert_eq!(
            resolved.provision.arguments(),
            [
                "--plugin-materialized",
                "--conversation",
                "agy-conversation"
            ]
        );
        assert_eq!(
            resolved.snapshot.plan.argv,
            [
                "--dangerously-skip-permissions",
                "--mode",
                "accept-edits",
                "--model",
                "gemini-3.8-flash-high",
            ]
        );
        assert!(
            !serde_json::to_string(&resolved.snapshot)
                .unwrap()
                .contains("agy-conversation")
        );
    }

    #[test]
    fn invalid_resume_and_headless_requests_fail_closed() {
        let mut missing = request(LaunchMode::Interactive);
        missing.resume = true;
        assert!(matches!(
            adapter().resolve(&missing),
            Err(AdapterError::Validation(
                LaunchValidationError::ProviderResumeMismatch
            ))
        ));

        let mut headless = request(LaunchMode::Headless);
        headless.initial_prompt = None;
        assert!(matches!(
            adapter().resolve(&headless),
            Err(AdapterError::Validation(LaunchValidationError::EmptyPrompt))
        ));

        let mut unsupported = request(LaunchMode::Headless);
        unsupported.resume = true;
        assert!(matches!(
            adapter().resolve(&unsupported),
            Err(AdapterError::Validation(
                LaunchValidationError::UnsupportedCapability { .. }
            ))
        ));
    }

    #[test]
    fn provision_failures_and_snapshot_revision_are_typed() {
        for (failure, expected) in [
            (
                AgyProvisionFailure::ExecutableUnavailable,
                AdapterError::ExecutableUnavailable,
            ),
            (
                AgyProvisionFailure::MaterializationFailed,
                AdapterError::ProvisionFailed,
            ),
        ] {
            let mut adapter = AgyAdapter::new(FakeProvisioner(Some(Err(failure))));
            assert!(matches!(
                adapter.resolve(&request(LaunchMode::Interactive)),
                Err(error) if error == expected
            ));
        }
        let resolved = adapter()
            .resolve(&request(LaunchMode::Interactive))
            .unwrap();
        assert!(
            AgyAdapter::new(FakeProvisioner(None))
                .validate_snapshot(&resolved.snapshot)
                .is_ok()
        );
        assert!(
            AgyAdapter::with_revision(FakeProvisioner(None), PROFILE_REVISION + 1)
                .validate_snapshot(&resolved.snapshot)
                .is_err()
        );
    }
}
