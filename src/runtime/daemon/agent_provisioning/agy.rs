//! Antigravity-specific daemon provisioning and provider-native plugin files.

use std::{path::PathBuf, sync::Arc};

use usagi_core::{
    domain::{agent::EnvironmentVariableName, settings::DefaultModel},
    infrastructure::persistence::json_file,
    usecase::claude_sandbox::SandboxMode,
};
use usagi_daemon::usecase::{
    agy::{AgyProvision, AgyProvisionFailure, AgyProvisioner},
    runtime::{ProvisionContext, SpawnProvision},
};

use super::{
    SandboxLauncherPaths, SandboxPolicyInputs, SharedUserEnvironment, Workspaces,
    agent_writable_roots, claude_sandbox, claude_sandbox_launcher, configured_environment,
    configured_mcp_tools, effective_role_instruction, insert_root_git_environment,
    launch_allowlist, launch_environment, launch_system_prompt, mcp_environment, paths,
    prompt_scope, root_agent_writable_roots, sandbox_mode, session_git_policy, shell_quote,
    validate_claude_sandbox_policy, validate_owned_directory,
};

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
        if context.inject_mcp {
            materialize_agy_plugin(self.sandbox_home.as_deref(), &self.mcp_command)
                .map_err(|()| AgyProvisionFailure::MaterializationFailed)?;
        }
        let user = configured_environment(self.environment.as_ref(), &workspace_root)
            .map_err(|_| AgyProvisionFailure::MaterializationFailed)?;
        let mut spawn = SpawnProvision::new(
            launch_environment(
                &user,
                mcp_environment(context, &self.data_home, &workspace_root)
                    .map_err(|()| AgyProvisionFailure::MaterializationFailed)?,
            ),
            Vec::new(),
        );
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
        validate_claude_sandbox_policy(&SandboxPolicyInputs {
            mode,
            program: DefaultModel::Agy.command(),
            workspace_root: &workspace_root,
            launch_roots: &sandbox_roots,
            tmpdir: self.sandbox_tmpdir.as_deref(),
            home: self.sandbox_home.as_deref(),
            cache_dir: self.sandbox_cache_dir.as_deref(),
            backend: self.sandbox_backend.as_deref(),
            passthrough: self.sandbox_passthrough,
        })
        .map_err(|_| AgyProvisionFailure::MaterializationFailed)?;
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

/// Provider-native global plugin documents loaded by Antigravity CLI.
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

/// Writes only usagi's dedicated Antigravity plugin directory. Existing user
/// MCP, hook, and settings documents remain untouched.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agy_plugin_documents_are_scoped_and_shell_safe
fn materialize_agy_plugin(
    home: Option<&std::path::Path>,
    command: &std::path::Path,
) -> Result<(), ()> {
    let home = home.ok_or(())?;
    validate_owned_directory(home).map_err(|_| ())?;
    let mut state_roots =
        root_agent_writable_roots(Some(home), DefaultModel::Agy.command()).map_err(|_| ())?;
    let mut plugin = state_roots.pop().ok_or(())?;
    for component in ["config", "plugins", "usagi-runtime"] {
        plugin.push(component);
        match std::fs::create_dir(&plugin) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(()),
        }
        validate_owned_directory(&plugin).map_err(|_| ())?;
    }
    let command = command.to_str().ok_or(())?;
    let (manifest, mcp, hooks) = agy_plugin_documents(command);
    for (name, value) in [
        ("plugin.json", manifest),
        ("mcp_config.json", mcp),
        ("hooks.json", hooks),
    ] {
        json_file::write_atomic(&plugin, &plugin.join(name), &value).map_err(|_| ())?;
    }
    Ok(())
}
