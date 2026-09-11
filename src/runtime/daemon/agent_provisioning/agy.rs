//! Antigravity-specific daemon provisioning and provider-native plugin files.

use std::{path::PathBuf, sync::Arc};

use usagi_core::{
    domain::{agent::EnvironmentVariableName, settings::DefaultModel},
    infrastructure::persistence::json_file,
    usecase::claude_sandbox::SandboxMode,
};
use usagi_daemon::infrastructure::unix_transport::ensure_private_dir_all;
use usagi_daemon::usecase::{
    agy::{AgyProvision, AgyProvisionFailure, AgyProvisioner},
    runtime::{ProvisionContext, SpawnProvision},
};

use super::{
    SandboxLauncherPaths, SandboxPolicyInputs, SharedUserEnvironment, Workspaces,
    agent_writable_roots, claude_sandbox, claude_sandbox_launcher, configured_environment,
    configured_mcp_tools, effective_role_instruction, insert_root_git_environment,
    launch_allowlist, launch_environment, launch_system_prompt, mcp_environment, paths,
    prompt_scope, sandbox_mode, session_git_policy, shell_quote, validate_claude_sandbox_policy,
    validate_isolated_sandbox_root, validate_owned_directory,
};

mod security;
use security::prepare_agy_read_only_paths;

/// Resolves the checkout, Antigravity plugin, prompt, environment, and outer
/// sandbox for the `agy` adapter.
pub(in crate::runtime::daemon) struct RootAgyProvisioner {
    pub(in crate::runtime::daemon) workspaces: Workspaces,
    pub(in crate::runtime::daemon) mcp_command: PathBuf,
    pub(in crate::runtime::daemon) data_home: paths::DataHome,
    pub(in crate::runtime::daemon) environment: Option<Arc<SharedUserEnvironment>>,
    pub(in crate::runtime::daemon) sandbox_backend: Option<PathBuf>,
    pub(in crate::runtime::daemon) sandbox_tmpdir: Option<PathBuf>,
    pub(in crate::runtime::daemon) sandbox_home: Option<PathBuf>,
    pub(in crate::runtime::daemon) sandbox_cache_dir: Option<PathBuf>,
    pub(in crate::runtime::daemon) sandbox_passthrough: bool,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_role_prompt_contract_reaches_every_shipping_agent_argv
impl AgyProvisioner for RootAgyProvisioner {
    #[allow(clippy::too_many_lines)] // One path keeps AGY plugin isolation, sandbox, prompt, and spawn arguments visibly atomic.
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<AgyProvision, AgyProvisionFailure> {
        let (working_directory, workspace_root) =
            super::working_directories(&self.workspaces, context)
                .map_err(|()| AgyProvisionFailure::MaterializationFailed)?;
        let mode = sandbox_mode(context);
        let role =
            effective_role_instruction(&self.workspaces, &self.data_home, &workspace_root, context)
                .map_err(|()| AgyProvisionFailure::MaterializationFailed)?;
        let tools = context
            .inject_mcp
            .then(|| configured_mcp_tools(&self.data_home, &workspace_root))
            .transpose()
            .map_err(|()| AgyProvisionFailure::MaterializationFailed)?;
        let user = configured_environment(self.environment.as_ref(), &workspace_root)
            .map_err(|_| AgyProvisionFailure::MaterializationFailed)?;
        let session_git = if mode == SandboxMode::Session {
            session_git_policy(&workspace_root, &working_directory)
                .map_err(|()| AgyProvisionFailure::MaterializationFailed)?
        } else {
            None
        };
        let sandbox_roots = agent_writable_roots(
            mode,
            &working_directory,
            session_git.as_ref(),
            self.sandbox_home.as_deref(),
            DefaultModel::Agy.command(),
            &self.data_home,
            context.scope.workspace_id,
        )
        .map_err(|_| AgyProvisionFailure::MaterializationFailed)?;
        let mut read_only_roots = prepare_agy_read_only_paths(self.sandbox_home.as_deref())?;
        let policy = SandboxPolicyInputs {
            mode,
            program: DefaultModel::Agy.command(),
            workspace_root: &workspace_root,
            launch_roots: &sandbox_roots,
            tmpdir: self.sandbox_tmpdir.as_deref(),
            home: self.sandbox_home.as_deref(),
            cache_dir: self.sandbox_cache_dir.as_deref(),
            backend: self.sandbox_backend.as_deref(),
            passthrough: self.sandbox_passthrough,
            read_only_roots: &read_only_roots,
        };
        validate_claude_sandbox_policy(&policy)
            .map_err(|_| AgyProvisionFailure::MaterializationFailed)?;
        let (arguments, integration) = agy_plugin_arguments(
            &self.data_home,
            context.scope.workspace_id,
            &self.mcp_command,
            context.inject_mcp,
            self.sandbox_passthrough,
            &policy,
        )
        .map_err(|()| AgyProvisionFailure::MaterializationFailed)?;
        read_only_roots.extend(integration);
        let mut spawn = SpawnProvision::new(
            launch_environment(
                &user,
                mcp_environment(context, &self.data_home, &workspace_root)
                    .map_err(|()| AgyProvisionFailure::MaterializationFailed)?,
            ),
            arguments,
        );
        let protected_root = workspace_root
            .canonicalize()
            .map_err(|_| AgyProvisionFailure::MaterializationFailed)?;
        let launcher = claude_sandbox_launcher(
            &self.mcp_command,
            mode,
            &protected_root,
            &SandboxLauncherPaths {
                backend: self.sandbox_backend.as_deref(),
                tmpdir: self.sandbox_tmpdir.as_deref(),
                home: self.sandbox_home.as_deref(),
                cache_dir: self.sandbox_cache_dir.as_deref(),
            },
            &sandbox_roots,
            &read_only_roots,
        )
        .map_err(|()| AgyProvisionFailure::MaterializationFailed)?;
        spawn.set_sandbox_launcher(launcher);
        if mode == SandboxMode::Root {
            insert_root_git_environment(&mut spawn);
        }
        if self.sandbox_passthrough {
            spawn.insert_daemon_environment(
                EnvironmentVariableName::new(claude_sandbox::PASSTHROUGH_ENVIRONMENT_VARIABLE)
                    .expect("literal environment variable name is valid"),
                "1".to_owned(),
            );
        }
        Ok(AgyProvision {
            working_directory,
            environment_allowlist: launch_allowlist(context, &user),
            spawn,
            system_prompt: launch_system_prompt(
                prompt_scope(mode),
                tools.as_ref().map(|tools| tools.families),
                role.as_ref()
                    .map(|(id, instructions)| (id, instructions.as_str())),
            ),
        })
    }
}

pub(in crate::runtime::daemon) fn agy_plugin_arguments(
    data_home: &paths::DataHome,
    workspace: usagi_core::domain::id::WorkspaceId,
    command: &std::path::Path,
    enabled: bool,
    passthrough: bool,
    policy: &SandboxPolicyInputs<'_>,
) -> Result<(Vec<String>, Option<PathBuf>), ()> {
    if !enabled {
        return Ok((Vec::new(), None));
    }
    let target = agy_integration_root(data_home, workspace)?;
    if !passthrough {
        validate_isolated_sandbox_root(policy, &target).map_err(|_| ())?;
    }
    let integration = materialize_agy_plugin(data_home, workspace, command)?;
    agy_arguments_for_integration(&integration)
}

pub(in crate::runtime::daemon) fn agy_arguments_for_integration(
    integration: &std::path::Path,
) -> Result<(Vec<String>, Option<PathBuf>), ()> {
    let integration = integration.to_str().ok_or(())?;
    Ok((
        vec!["--add-dir".to_owned(), integration.to_owned()],
        Some(PathBuf::from(integration)),
    ))
}

fn agy_integration_root(
    data_home: &paths::DataHome,
    workspace: usagi_core::domain::id::WorkspaceId,
) -> Result<PathBuf, ()> {
    let selected = data_home.selected().canonicalize().map_err(|_| ())?;
    Ok(selected
        .join("agent-integrations")
        .join(workspace.to_string())
        .join("agy"))
}

/// Provider-native workspace plugin documents loaded by Antigravity CLI.
#[must_use]
pub(in crate::runtime::daemon) fn agy_plugin_documents(
    command: &str,
) -> (serde_json::Value, serde_json::Value, serde_json::Value) {
    let hook = |event: &str, phase: &str| {
        format!(
            "{} agent-phase {phase} --hook-event {event}",
            shell_quote(command)
        )
    };
    let plugin = serde_json::json!({"name": "usagi-runtime"});
    let mcp = serde_json::json!({
        "mcpServers": {
            "usagi": {
                "command": command,
                "args": ["mcp"]
            }
        }
    });
    let hooks = serde_json::json!({
        "usagi-runtime": {
            "PreInvocation": [{
                "type": "command",
                "command": hook("PreInvocation", "running"),
                "timeout": 10
            }],
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": hook("PreToolUse", "running"),
                    "timeout": 10
                }]
            }],
            "PostToolUse": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": hook("PostToolUse", "waiting"),
                    "timeout": 10
                }]
            }],
            "Stop": [{
                "type": "command",
                "command": hook("Stop", "ended"),
                "timeout": 3
            }]
        }
    });
    (plugin, mcp, hooks)
}

/// Writes usagi's dedicated Antigravity plugin beneath daemon-private data and
/// returns the synthetic workspace root passed only to this managed launch.
/// The root is deliberately not a sandbox writable root, so the provider may
/// read but cannot persistently replace its hook or MCP command documents.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agy_plugin_documents_are_scoped_and_shell_safe
pub(in crate::runtime::daemon) fn materialize_agy_plugin(
    data_home: &paths::DataHome,
    workspace: usagi_core::domain::id::WorkspaceId,
    command: &std::path::Path,
) -> Result<PathBuf, ()> {
    let integration = agy_integration_root(data_home, workspace)?;
    let plugin = integration.join(".agents/plugins/usagi-runtime");
    ensure_private_dir_all(&plugin).map_err(|_| ())?;
    validate_owned_directory(&plugin).map_err(|_| ())?;
    let command = command.to_str().ok_or(())?;
    let (manifest, mcp, hooks) = agy_plugin_documents(command);
    for (name, value) in [
        ("plugin.json", manifest),
        ("mcp_config.json", mcp),
        ("hooks.json", hooks),
    ] {
        json_file::write_atomic(&plugin, &plugin.join(name), &value).map_err(|_| ())?;
    }
    let integration = integration.canonicalize().map_err(|_| ())?;
    validate_owned_directory(&integration).map_err(|_| ())?;
    Ok(integration)
}
