//! Codex-specific launch adapter.
//!
//! The adapter owns Codex argv syntax and the opaque config/MCP/hook
//! materialization request.  It exposes only the product-neutral durable launch
//! snapshot to the runtime coordinator; no materialized payload or environment
//! value is retained in that snapshot.

use std::{collections::BTreeSet, path::PathBuf};

use usagi_core::{
    domain::agent::{
        AgentCapability, AgentProfile, AgentProfileId, DurableLaunchSnapshot,
        EnvironmentVariableName, LaunchMode, LaunchPlan, LaunchRequest, LaunchScope,
        LaunchValidationError,
    },
    usecase::agent::{AgentProfileCatalog, validate_request, validate_snapshot},
};

use super::runtime::{AdapterError, AgentAdapter, ResolvedLaunch, SpawnProvision};

#[cfg(test)]
mod fixture;

const PROFILE_NAME: &str = "codex";
const PROFILE_REVISION: u32 = 1;

/// The non-secret outcome that the renderer may use to build a durable plan.
pub struct CodexProvision {
    pub working_directory: PathBuf,
    pub environment_allowlist: BTreeSet<EnvironmentVariableName>,
    pub spawn: SpawnProvision,
}

/// Typed pre-spawn failures from the injected Codex provisioning boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexProvisionFailure {
    ExecutableUnavailable,
    MaterializationFailed,
}

/// Materializes Codex-private config, MCP, and hook artifacts for one scope.
///
/// Implementations may inject secrets into the spawned process environment, but
/// must not return them in [`CodexProvision`]. The coordinator persists only the
/// resulting public launch plan.
///
/// # Errors
///
/// Returns [`CodexProvisionFailure`] when the Codex executable cannot be used
/// or its scoped artifacts cannot be materialized.
pub trait CodexProvisioner {
    /// # Errors
    ///
    /// Returns [`CodexProvisionFailure`] when the Codex executable cannot be
    /// used or its scoped artifacts cannot be materialized.
    fn provision(&mut self, scope: &LaunchScope) -> Result<CodexProvision, CodexProvisionFailure>;
}

/// An [`AgentAdapter`] for the code-defined `codex` profile.
#[derive(Debug)]
pub struct CodexAdapter<P> {
    provisioner: P,
    profile: AgentProfile,
}

impl<P> CodexAdapter<P> {
    #[must_use]
    pub fn new(provisioner: P) -> Self {
        Self::with_revision(provisioner, PROFILE_REVISION)
    }

    /// # Panics
    ///
    /// Panics only if the hard-coded `codex` profile ID stops satisfying the
    /// core contract, which is a programmer error.
    #[must_use]
    pub fn with_revision(provisioner: P, revision: u32) -> Self {
        Self {
            provisioner,
            profile: AgentProfile::new(
                AgentProfileId::new(PROFILE_NAME).expect("literal profile ID is canonical"),
                "Codex",
                revision,
                [
                    AgentCapability::Resume,
                    AgentCapability::InitialPrompt,
                    AgentCapability::Headless,
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

    /// Checks a restored snapshot against this adapter revision without
    /// re-rendering or re-provisioning it.
    ///
    /// # Errors
    ///
    /// Returns a typed validation error when the snapshot is not compatible
    /// with this static Codex profile.
    pub fn validate_snapshot(
        &self,
        snapshot: &DurableLaunchSnapshot,
    ) -> Result<AgentProfile, LaunchValidationError> {
        validate_snapshot(self, snapshot)
    }
}

impl<P> AgentProfileCatalog for CodexAdapter<P> {
    fn find(&self, profile_id: &AgentProfileId) -> Option<AgentProfile> {
        (profile_id == &self.profile.id).then(|| self.profile.clone())
    }
}

impl<P: CodexProvisioner> AgentAdapter for CodexAdapter<P> {
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
        let provision =
            self.provisioner
                .provision(&request.scope)
                .map_err(|failure| match failure {
                    CodexProvisionFailure::ExecutableUnavailable => {
                        AdapterError::ExecutableUnavailable
                    }
                    CodexProvisionFailure::MaterializationFailed => AdapterError::ProvisionFailed,
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
    provision: &CodexProvision,
) -> Result<LaunchPlan, LaunchValidationError> {
    let mut argv = match request.mode {
        LaunchMode::Interactive if request.resume && request.initial_prompt.is_none() => vec![
            "resume".into(),
            "--last".into(),
            "--dangerously-bypass-hook-trust".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "--ask-for-approval".into(),
            "on-request".into(),
        ],
        LaunchMode::Interactive => vec![
            "--dangerously-bypass-hook-trust".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "--ask-for-approval".into(),
            "on-request".into(),
        ],
        LaunchMode::Headless => vec![
            "exec".into(),
            "--dangerously-bypass-approvals-and-sandbox".into(),
        ],
    };
    if let Some(model) = &request.model {
        argv.extend(["-m".into(), model.as_str().into()]);
    }
    if let Some(prompt) = &request.initial_prompt {
        argv.extend(["--".into(), prompt.clone()]);
    }
    LaunchPlan::new(
        profile.id.clone(),
        profile.revision,
        "codex",
        argv,
        provision.environment_allowlist.clone(),
        provision.working_directory.clone(),
    )
}
