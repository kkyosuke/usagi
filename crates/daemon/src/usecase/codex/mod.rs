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
        EnvironmentVariableName, LaunchMode, LaunchPlan, LaunchRequest, LaunchValidationError,
        ProviderKind, ProviderResumePhase, ProviderResumeRef, ProviderResumeStatus,
    },
    domain::settings::DefaultModel,
    usecase::agent::{AgentProfileCatalog, validate_request, validate_snapshot},
};

use super::runtime::{
    AdapterError, AgentAdapter, ProvisionContext, ResolvedLaunch, SpawnProvision,
};

#[cfg(test)]
mod fixture;

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
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<CodexProvision, CodexProvisionFailure>;
}

/// Render daemon-owned MCP servers as Codex `-c` overrides.
///
/// `usagi` always precedes the optional `usagi-llm` server. Every value is a
/// TOML basic string rendered through `serde_json`'s compatible string escaping,
/// so neither a command path nor a model token can create another override.
#[must_use]
pub fn mcp_arguments(usagi_command: &str, local_llm_model: Option<&str>) -> Vec<String> {
    fn assignment(key: &str, value: &str) -> [String; 2] {
        ["-c".to_owned(), format!("{key} = {value}")]
    }
    fn string(value: &str) -> String {
        serde_json::to_string(value).expect("serializing a string cannot fail")
    }
    fn array(values: &[&str]) -> String {
        format!(
            "[{}]",
            values
                .iter()
                .map(|value| string(value))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    let mut arguments = Vec::new();
    arguments.extend(assignment(
        "mcp_servers.usagi.command",
        &string(usagi_command),
    ));
    arguments.extend(assignment("mcp_servers.usagi.args", &array(&["mcp"])));
    arguments.extend(assignment(
        "mcp_servers.usagi.env_vars",
        &array(&[
            "USAGI_HOME",
            "USAGI_RUNTIME_MODE",
            "USAGI_WORKSPACE_ROOT",
            "USAGI_MCP_CALLER_CREDENTIAL",
        ]),
    ));
    arguments.extend(assignment(
        "mcp_servers.usagi.default_tools_approval_mode",
        &string("approve"),
    ));
    if let Some(model) = local_llm_model {
        arguments.extend(assignment(
            "mcp_servers.usagi-llm.command",
            &string(usagi_command),
        ));
        arguments.extend(assignment(
            "mcp_servers.usagi-llm.args",
            &array(&["llm-mcp", "--model", model]),
        ));
        arguments.extend(assignment(
            "mcp_servers.usagi-llm.default_tools_approval_mode",
            &string("approve"),
        ));
    }
    arguments
}

/// An [`AgentAdapter`] for the code-defined `codex` and `sakana-ai` profiles.
///
/// One instance serves exactly one profile: `program` is the executable that
/// profile launches, so the rendered plan never depends on a product-name
/// switch downstream.
#[derive(Debug)]
pub struct CodexAdapter<P> {
    provisioner: P,
    profile: AgentProfile,
    program: &'static str,
}

impl<P> CodexAdapter<P> {
    #[must_use]
    pub fn new(provisioner: P) -> Self {
        Self::with_revision(provisioner, PROFILE_REVISION)
    }

    /// Builds the `sakana-ai` profile over the same Codex CLI grammar, launching
    /// `codex-fugu`.
    #[must_use]
    pub fn sakana(provisioner: P) -> Self {
        Self::sakana_with_revision(provisioner, PROFILE_REVISION)
    }

    /// # Panics
    ///
    /// Panics only if the hard-coded `codex` profile ID stops satisfying the
    /// core contract, which is a programmer error.
    #[must_use]
    pub fn with_revision(provisioner: P, revision: u32) -> Self {
        Self::build(
            provisioner,
            revision,
            DefaultModel::OpenAi.profile_id(),
            "Codex",
            DefaultModel::OpenAi.command(),
        )
    }

    /// # Panics
    ///
    /// Panics only if the hard-coded `sakana-ai` profile ID stops satisfying the
    /// core contract, which is a programmer error.
    #[must_use]
    pub fn sakana_with_revision(provisioner: P, revision: u32) -> Self {
        Self::build(
            provisioner,
            revision,
            DefaultModel::SakanaAi.profile_id(),
            "sakana.ai",
            DefaultModel::SakanaAi.command(),
        )
    }

    fn build(
        provisioner: P,
        revision: u32,
        profile_name: &str,
        display_name: &str,
        program: &'static str,
    ) -> Self {
        Self {
            provisioner,
            profile: AgentProfile::new(
                AgentProfileId::new(profile_name).expect("literal profile ID is canonical"),
                display_name,
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
            program,
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
        let mut provision = self
            .provisioner
            .provision(&ProvisionContext::from_request(request))
            .map_err(|failure| match failure {
                CodexProvisionFailure::ExecutableUnavailable => AdapterError::ExecutableUnavailable,
                CodexProvisionFailure::MaterializationFailed => AdapterError::ProvisionFailed,
            })?;
        let provider_resume =
            validate_provider_resume(request, &profile).map_err(AdapterError::Validation)?;
        if let Some(reference) = &provider_resume {
            provision.spawn.append_sensitive_arguments([
                "resume".to_owned(),
                reference.native_session_id.expose_sensitive().to_owned(),
            ]);
        }
        let plan = render_plan(request, &profile, &provision, self.program)
            .map_err(AdapterError::Validation)?;
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
    provision: &CodexProvision,
    program: &str,
) -> Result<LaunchPlan, LaunchValidationError> {
    let mut argv = match request.mode {
        LaunchMode::Interactive => vec![
            "--dangerously-bypass-hook-trust".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "--ask-for-approval".into(),
            "never".into(),
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
        program,
        argv,
        provision.environment_allowlist.clone(),
        provision.working_directory.clone(),
    )
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
            reference.provider == ProviderKind::Codex
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
mod wiring_tests {
    use super::mcp_arguments;

    #[test]
    fn mcp_arguments_add_local_llm_after_usagi_only_when_enabled() {
        let disabled = mcp_arguments("/opt/usagi", None);
        assert!(!disabled.join(" ").contains("usagi-llm"));
        assert_eq!(
            disabled,
            [
                "-c",
                "mcp_servers.usagi.command = \"/opt/usagi\"",
                "-c",
                "mcp_servers.usagi.args = [\"mcp\"]",
                "-c",
                "mcp_servers.usagi.env_vars = [\"USAGI_HOME\", \"USAGI_RUNTIME_MODE\", \"USAGI_WORKSPACE_ROOT\", \"USAGI_MCP_CALLER_CREDENTIAL\"]",
                "-c",
                "mcp_servers.usagi.default_tools_approval_mode = \"approve\"",
            ]
        );

        let enabled = mcp_arguments("/opt/usagi", Some("qwen2.5-coder:7b"));
        let usagi = enabled
            .iter()
            .position(|value| value.starts_with("mcp_servers.usagi.command"))
            .unwrap();
        let local = enabled
            .iter()
            .position(|value| value.starts_with("mcp_servers.usagi-llm.command"))
            .unwrap();
        assert!(usagi < local);
        assert!(enabled.iter().any(|value| {
            value == "mcp_servers.usagi-llm.args = [\"llm-mcp\", \"--model\", \"qwen2.5-coder:7b\"]"
        }));
    }

    #[test]
    fn mcp_arguments_toml_escape_untrusted_values_without_new_overrides() {
        let model = "x\"], owned = \"pwned\\\n";
        let arguments = mcp_arguments("/opt/\"usagi", Some(model));
        assert_eq!(
            arguments
                .iter()
                .filter(|value| value.starts_with("mcp_servers."))
                .count(),
            7
        );
        let model_override = arguments
            .iter()
            .find(|value| value.starts_with("mcp_servers.usagi-llm.args"))
            .unwrap();
        assert_eq!(
            model_override,
            r#"mcp_servers.usagi-llm.args = ["llm-mcp", "--model", "x\"], owned = \"pwned\\\n"]"#
        );
    }
}
