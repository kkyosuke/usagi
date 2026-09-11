//! Product-specific Agent launch provisioning for the daemon composition.
//!
//! This module owns Antigravity/Codex/Claude argv, sandbox, environment, role, and MCP
//! materialization. Socket admission, runtime ownership, and background-worker
//! orchestration remain in the parent composition module.

use super::secure_path::{InvalidOwnedDirectory, validate_owned_directory, validate_owned_path};
use super::{
    AGENT_PHASE_HOOK_EVENTS, Arc, BTreeMap, BTreeSet, ClaudeProvision, ClaudeProvisionFailure,
    ClaudeProvisioner, CodexProvision, CodexProvisionFailure, CodexProvisioner, DefaultModel,
    EnvironmentVariableName, ErrorLog, FmtWrite, McpToolFamilies, Output, OutputJournal, Path,
    PathBuf, PromptScope, ProvisionContext, SandboxLauncher, SandboxMode, SharedUserEnvironment,
    SpawnProvision, Storage, WorkspaceId, WorkspaceSettingsStore, Workspaces,
    claude_product_mcp_arguments, claude_sandbox, codex_product_mcp_arguments,
    launch_system_prompt, paths, scoped_settings_json, user_env,
};

mod agy;
pub(super) use agy::RootAgyProvisioner;
#[cfg(test)]
pub(super) use agy::{
    agy_arguments_for_integration, agy_plugin_arguments, agy_plugin_documents,
    materialize_agy_plugin,
};

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_role_prompt_contract_reaches_every_shipping_agent_argv
fn working_directories(
    workspaces: &Workspaces,
    context: &ProvisionContext,
) -> Result<(PathBuf, PathBuf), ()> {
    // The launch names its workspace, so the runtime that materializes it is the
    // one holding that identity. A daemon serving several workspaces would
    // otherwise provision every Agent from the workspace it was started in.
    let tenant = workspaces.workspace(context.scope.workspace_id).ok_or(())?;
    let runtime = tenant.runtime().lock().map_err(|_| ())?;
    let workspace_root = runtime.repository_root().to_path_buf();
    // A workspace-root launch has no session; its trusted cwd is the repository
    // root. A session launch resolves that session's worktree path.
    let working_directory = match context.scope.session_id {
        None => runtime
            .resolve_root_scope(context.scope.workspace_id, context.scope.worktree_id)
            .map_err(|_| ()),
        Some(session) => runtime
            .resolve_scope(
                context.scope.workspace_id,
                session,
                context.scope.worktree_id,
            )
            .map(|scope| scope.path)
            .map_err(|_| ()),
    }?;
    Ok((working_directory, workspace_root))
}

/// Resolves only safe role identity under the session lock, then reads the
/// current definition from the registered workspace catalog. The instruction
/// remains in this ephemeral provision path and is never copied into a launch
/// request, durable snapshot, dispatch record, response, or log.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_role_prompt_contract_reaches_every_shipping_agent_argv
pub(super) fn effective_role_instruction(
    workspaces: &Workspaces,
    data_home: &paths::DataHome,
    workspace_root: &Path,
    context: &ProvisionContext,
) -> Result<Option<(usagi_core::domain::role::RoleId, String)>, ()> {
    use usagi_core::domain::role::RoleScope;

    let assigned = match context.scope.session_id {
        Some(session_id) => workspaces
            .workspace(context.scope.workspace_id)
            .ok_or(())?
            .runtime()
            .lock()
            .map_err(|_| ())?
            .session_role(session_id)
            .map_err(|_| ())?,
        None => None,
    };
    let catalog = usagi_core::infrastructure::role_catalog::load_effective(
        &data_home.selected(),
        workspace_root,
    )
    .map_err(|_| ())?;
    let scope = if context.scope.session_id.is_some() {
        RoleScope::Session
    } else {
        RoleScope::Root
    };
    // A legacy managed session remains generic even if a catalog is introduced
    // later. Root launches have no durable session assignment and therefore
    // resolve the current root default at each launch.
    let selected = if context.scope.session_id.is_some() && assigned.is_none() {
        None
    } else {
        catalog.resolve(assigned.as_ref(), scope).map_err(|_| ())?
    };
    let Some(selected) = selected else {
        return Ok(None);
    };
    let definition = catalog.roles.get(&selected).ok_or(())?;
    Ok(Some((selected, definition.instructions.clone())))
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=repairs_reused_codex_arg0_files_before_launch
pub(super) fn repair_agent_codex_arg0_permissions(sandbox_home: Option<&Path>) {
    // Repair only stale directory modes before the Agent owner mutex exists.
    // Codex performs the lock-aware deletion itself after startup.
    for program in [
        DefaultModel::OpenAi.command(),
        DefaultModel::SakanaAi.command(),
    ] {
        if let Ok(roots) = root_agent_writable_roots(sandbox_home, program) {
            for root in roots {
                if let Err(error) = repair_codex_arg0_permissions(&root) {
                    ErrorLog::record(&format!(
                        "could not repair Codex arg0 temp permissions: {:?}",
                        error.kind()
                    ));
                }
            }
        }
    }
}

/// The registry's bounded in-memory replay buffer already serves reconnect
/// within retention; a durable on-disk output journal is intentionally deferred
/// with daemon-crash PTY FD continuation (out of scope for this issue).
pub(super) struct DiscardJournal;
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_dispatch_worker_complete_reaches_the_caller_inbox
impl OutputJournal for DiscardJournal {
    fn append(&mut self, _output: &Output) -> Result<(), ()> {
        Ok(())
    }
}

/// Resolves the checkout path for a launch scope through the single managed
/// session writer, so agents never receive a client supplied path.
pub(super) struct RootCodexProvisioner {
    pub(super) workspaces: Workspaces,
    pub(super) mcp_command: PathBuf,
    pub(super) data_home: paths::DataHome,
    /// The executable this profile launches: `codex`, or `codex-fugu` for the
    /// Codex-compatible `sakana-ai` profile.
    pub(super) program: &'static str,
    /// The configured environment injected into the Agent child. `None` in tests
    /// that exercise only the MCP wiring.
    pub(super) environment: Option<Arc<SharedUserEnvironment>>,
    pub(super) sandbox_backend: Option<PathBuf>,
    pub(super) sandbox_tmpdir: Option<PathBuf>,
    pub(super) sandbox_home: Option<PathBuf>,
    pub(super) sandbox_cache_dir: Option<PathBuf>,
    pub(super) sandbox_passthrough: bool,
}
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_root_codex_launch_stays_in_the_repository_and_uses_the_daemon_mcp
impl CodexProvisioner for RootCodexProvisioner {
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<CodexProvision, CodexProvisionFailure> {
        let (working_directory, workspace_root) = working_directories(&self.workspaces, context)
            .map_err(|()| CodexProvisionFailure::MaterializationFailed)?;
        let mode = sandbox_mode(context);
        let role =
            effective_role_instruction(&self.workspaces, &self.data_home, &workspace_root, context)
                .map_err(|()| CodexProvisionFailure::MaterializationFailed)?;
        let tools = context
            .inject_mcp
            .then(|| configured_mcp_tools(&self.data_home, &workspace_root))
            .transpose()
            .map_err(|()| CodexProvisionFailure::MaterializationFailed)?;
        let mut arguments = tools
            .as_ref()
            .map(|_| codex_integration_arguments(&self.mcp_command))
            .transpose()
            .map_err(|()| CodexProvisionFailure::MaterializationFailed)?
            .unwrap_or_default();
        arguments.extend(codex_system_prompt_arguments(
            mode,
            tools.as_ref().map(|tools| tools.families),
            role.as_ref()
                .map(|(id, instructions)| (id, instructions.as_str())),
        ));
        let user = configured_environment(self.environment.as_ref(), &workspace_root)
            .map_err(|_| CodexProvisionFailure::MaterializationFailed)?;
        let mut spawn = SpawnProvision::new(
            launch_environment(
                &user,
                mcp_environment(context, &self.data_home, &workspace_root)
                    .map_err(|()| CodexProvisionFailure::MaterializationFailed)?,
            ),
            arguments,
        );
        let session_git = if mode == SandboxMode::Session {
            session_git_policy(&workspace_root, &working_directory)
                .map_err(|()| CodexProvisionFailure::MaterializationFailed)?
        } else {
            None
        };
        let sandbox_roots = agent_writable_roots(
            mode,
            &working_directory,
            session_git.as_ref(),
            self.sandbox_home.as_deref(),
            self.program,
            &self.data_home,
            context.scope.workspace_id,
        )
        .map_err(|_| CodexProvisionFailure::MaterializationFailed)?;
        validate_claude_sandbox_policy(&SandboxPolicyInputs {
            mode,
            program: self.program,
            workspace_root: &workspace_root,
            launch_roots: &sandbox_roots,
            tmpdir: self.sandbox_tmpdir.as_deref(),
            home: self.sandbox_home.as_deref(),
            cache_dir: self.sandbox_cache_dir.as_deref(),
            backend: self.sandbox_backend.as_deref(),
            passthrough: self.sandbox_passthrough,
            read_only_roots: &[],
        })
        .map_err(|_| CodexProvisionFailure::MaterializationFailed)?;
        let protected_root = workspace_root
            .canonicalize()
            .map_err(|_| CodexProvisionFailure::MaterializationFailed)?;
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
            &[],
        )
        .map_err(|()| CodexProvisionFailure::MaterializationFailed)?;
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
        Ok(CodexProvision {
            working_directory,
            environment_allowlist: launch_allowlist(context, &user),
            spawn,
        })
    }
}

pub(super) fn agent_writable_roots(
    mode: SandboxMode,
    working_directory: &Path,
    session_git: Option<&SessionGitPolicy>,
    sandbox_home: Option<&Path>,
    program: &str,
    data_home: &paths::DataHome,
    workspace: WorkspaceId,
) -> Result<Vec<PathBuf>, ClaudeSandboxPolicyError> {
    let mut roots = if mode == SandboxMode::Root {
        root_agent_writable_roots(sandbox_home, program)?
    } else {
        let mut roots = claude_writable_roots(mode, working_directory);
        roots.extend(
            session_git
                .into_iter()
                .flat_map(|policy| policy.writable_roots.iter().cloned()),
        );
        roots
    };
    roots.push(root_memory_store_root(data_home, workspace)?);
    roots.sort();
    roots.dedup();
    Ok(roots)
}

/// The Claude provisioner's product program: what the readiness probe proves,
/// what the launcher execs, and whose `$HOME` state root the sandbox grants.
/// The Codex provisioner carries the same value per profile (`RootCodexProvisioner::program`).
pub(super) const CLAUDE_PROGRAM: &str = "claude";

/// Ensure the launched agent's private state directory exists before a root
/// sandbox starts. Linux `--bind-try` cannot make a missing bind source writable,
/// so the daemon creates and validates the provider-specific directory first.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=root_scope_grants_only_the_state_directory_of_the_agent_it_launches
pub(super) fn root_agent_writable_roots(
    home: Option<&Path>,
    program: &str,
) -> Result<Vec<PathBuf>, ClaudeSandboxPolicyError> {
    let (Some(home), Some(state_directory)) =
        (home, claude_sandbox::agent_state_directory(program))
    else {
        return Ok(Vec::new());
    };
    validate_owned_directory(home)?;
    let mut state = home.to_path_buf();
    // Some providers intentionally expose a nested state subtree. Validate
    // every ancestor before descending so a symlink cannot redirect creation.
    for segment in state_directory.split('/') {
        state.push(segment);
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(&state) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(ClaudeSandboxPolicyError::InvalidWritableRoot),
        }
        validate_owned_directory(&state)?;
    }
    state
        .canonicalize()
        .map(|state| vec![state])
        .map_err(|_| ClaudeSandboxPolicyError::InvalidWritableRoot)
}

/// A root coordinator's reusable memory lives outside every Git checkout and
/// outside daemon control state. The MCP adapter adds `.usagi/memory` beneath
/// this synthetic store root, preserving the existing `MemoryStore` contract.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=production_root_memory_survives_without_dirtying_the_workspace
pub(super) fn root_memory_store_root(
    data_home: &paths::DataHome,
    workspace: WorkspaceId,
) -> Result<PathBuf, ClaudeSandboxPolicyError> {
    use std::os::unix::fs::PermissionsExt as _;

    let requested = data_home
        .selected()
        .join("agent-memory")
        .join(workspace.to_string());
    std::fs::create_dir_all(requested.join(".usagi/memory"))
        .map_err(|_| ClaudeSandboxPolicyError::InvalidWritableRoot)?;
    std::fs::set_permissions(&requested, std::fs::Permissions::from_mode(0o700))
        .map_err(|_| ClaudeSandboxPolicyError::InvalidWritableRoot)?;
    // The launcher re-validates exact canonical paths. In particular, macOS
    // exposes `/tmp` through `/private/tmp`; returning the lexical spelling
    // would make an otherwise valid Agent exit before its provider starts.
    let root = requested
        .canonicalize()
        .map_err(|_| ClaudeSandboxPolicyError::InvalidWritableRoot)?;
    validate_owned_directory(&root)?;
    Ok(root)
}

/// Codex's arg0 janitor cannot open the `.lock` inside a directory left with no
/// owner permissions by an interrupted sandboxed cleanup. Repair only the mode
/// of owned, provider-named temp directories; Codex still owns lock validation
/// and deletion, so a live helper is never removed here.
pub(super) const CODEX_ARG0_REPAIR_LIMIT: usize = 4_096;

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=repairs_reused_codex_arg0_files_before_launch
pub(super) fn repair_codex_arg0_permissions(state_root: &Path) -> std::io::Result<usize> {
    repair_codex_arg0_permissions_with_limit(state_root, CODEX_ARG0_REPAIR_LIMIT)
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=repairs_reused_codex_arg0_files_before_launch
pub(super) fn repair_codex_arg0_permissions_with_limit(
    state_root: &Path,
    limit: usize,
) -> std::io::Result<usize> {
    #[cfg(not(unix))]
    {
        let _ = (state_root, limit);
        return Ok(0);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let arg0 = state_root.join("tmp/arg0");
        let metadata = match std::fs::symlink_metadata(&arg0) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let expected_uid = unsafe { libc::geteuid() };
        if !metadata.file_type().is_dir() || metadata.uid() != expected_uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Codex arg0 root is not an owned directory",
            ));
        }
        if metadata.mode() & 0o700 != 0o700 {
            std::fs::set_permissions(&arg0, std::fs::Permissions::from_mode(0o700))?;
        }

        let mut repaired = 0;
        for (index, entry) in std::fs::read_dir(&arg0)?.enumerate() {
            if index == limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Codex arg0 repair scan limit exceeded",
                ));
            }
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !name.starts_with("codex-arg0") {
                continue;
            }
            let metadata = std::fs::symlink_metadata(entry.path())?;
            if !metadata.file_type().is_dir() || metadata.uid() != expected_uid {
                continue;
            }
            if metadata.mode() & 0o700 != 0o700 {
                std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o700))?;
                repaired += 1;
            }
        }
        Ok(repaired)
    }
}

pub(super) struct RootClaudeProvisioner {
    pub(super) workspaces: Workspaces,
    pub(super) mcp_command: PathBuf,
    pub(super) data_home: paths::DataHome,
    /// daemon bootstrap の trusted environment から一度だけ確定した backend。
    pub(super) sandbox_backend: Option<PathBuf>,
    /// daemon bootstrap の trusted environment から一度だけ確定した policy paths。
    pub(super) sandbox_tmpdir: Option<PathBuf>,
    pub(super) sandbox_home: Option<PathBuf>,
    /// daemon bootstrap の trusted environment から一度だけ確定した macOS の per-user cache root。
    pub(super) sandbox_cache_dir: Option<PathBuf>,
    /// The configured environment injected into the Agent child. `None` in tests
    /// that exercise only the sandbox and MCP wiring.
    pub(super) environment: Option<Arc<SharedUserEnvironment>>,
    /// E2E テスト専用 seam（[`claude_sandbox::passthrough_requested`]）。true のとき launcher の子へ
    /// 同じ opt-in を伝え、backend の無い環境でも live 起動経路を通す。release ビルドでは常に false。
    pub(super) sandbox_passthrough: bool,
}
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_root_claude_keeps_the_repository_read_only_and_gets_the_guard_hook
impl RootClaudeProvisioner {
    /// The policy paths this launch may carry. Both scopes carry the same
    /// universal areas: the agent CLI keeps its scratchpad, state and credential
    /// caches outside the repository, and withholding them does not confine the
    /// agent — it stops it from running at all
    /// ([`claude_sandbox`](usagi_core::usecase::claude_sandbox)). What separates a
    /// session launch from a root coordinator is the repository write boundary
    /// (`launch_roots` plus `protected_root`), not these paths.
    fn launcher_paths(&self) -> SandboxLauncherPaths<'_> {
        SandboxLauncherPaths {
            backend: self.sandbox_backend.as_deref(),
            tmpdir: self.sandbox_tmpdir.as_deref(),
            home: self.sandbox_home.as_deref(),
            cache_dir: self.sandbox_cache_dir.as_deref(),
        }
    }
}
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_root_claude_keeps_the_repository_read_only_and_gets_the_guard_hook
impl ClaudeProvisioner for RootClaudeProvisioner {
    #[allow(clippy::too_many_lines)] // One path keeps Claude's sandbox, hooks, prompt, and spawn arguments visibly atomic.
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<ClaudeProvision, ClaudeProvisionFailure> {
        let (working_directory, workspace_root) = working_directories(&self.workspaces, context)
            .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?;
        // Claude は OS sandbox と `guard-workspace` の両方で fail-closed 起動する。
        let mode = sandbox_mode(context);
        let session_git = if mode == SandboxMode::Session {
            session_git_policy(&workspace_root, &working_directory)
                .map_err(|()| ClaudeProvisionFailure::InvalidSandboxPolicy)?
        } else {
            None
        };
        let mut launch_roots = claude_writable_roots(mode, &working_directory);
        launch_roots.push(
            root_memory_store_root(&self.data_home, context.scope.workspace_id)
                .map_err(|_| ClaudeProvisionFailure::InvalidSandboxPolicy)?,
        );
        launch_roots.extend(
            session_git
                .as_ref()
                .into_iter()
                .flat_map(|policy| policy.writable_roots.iter().cloned()),
        );
        let paths = self.launcher_paths();
        validate_claude_sandbox_policy(&SandboxPolicyInputs {
            mode,
            program: CLAUDE_PROGRAM,
            workspace_root: &workspace_root,
            launch_roots: &launch_roots,
            tmpdir: paths.tmpdir,
            home: paths.home,
            cache_dir: paths.cache_dir,
            backend: paths.backend,
            passthrough: self.sandbox_passthrough,
            read_only_roots: &[],
        })
        .map_err(|_| ClaudeProvisionFailure::InvalidSandboxPolicy)?;
        let sandbox_roots = launch_roots
            .iter()
            .map(|root| root.canonicalize())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ClaudeProvisionFailure::InvalidSandboxPolicy)?;
        let protected_root = workspace_root
            .canonicalize()
            .map_err(|_| ClaudeProvisionFailure::InvalidSandboxPolicy)?;
        let sandbox_launcher = claude_sandbox_launcher(
            &self.mcp_command,
            mode,
            &protected_root,
            &paths,
            &sandbox_roots,
            &[],
        )
        .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?;
        let role =
            effective_role_instruction(&self.workspaces, &self.data_home, &workspace_root, context)
                .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?;
        let tools = context
            .inject_mcp
            .then(|| configured_mcp_tools(&self.data_home, &workspace_root))
            .transpose()
            .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?;
        let mut arguments = tools
            .as_ref()
            .map(|_| claude_mcp_arguments(&self.mcp_command))
            .transpose()
            .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?
            .unwrap_or_default();
        arguments.extend(
            claude_settings_arguments(&self.mcp_command)
                .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?,
        );
        arguments.extend(claude_system_prompt_arguments(
            mode,
            tools.as_ref().map(|tools| tools.families),
            role.as_ref()
                .map(|(id, instructions)| (id, instructions.as_str())),
        ));
        let user = configured_environment(self.environment.as_ref(), &workspace_root)
            .map_err(|_| ClaudeProvisionFailure::MaterializationFailed)?;
        let mut spawn = SpawnProvision::new(
            launch_environment(
                &user,
                mcp_environment(context, &self.data_home, &workspace_root)
                    .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?,
            ),
            arguments,
        );
        spawn.set_sandbox_launcher(sandbox_launcher);
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
        Ok(ClaudeProvision {
            working_directory,
            environment_allowlist: launch_allowlist(context, &user),
            spawn,
        })
    }
}

/// A launch without a managed session is the workspace-root coordinator; every
/// other launch is confined to its session worktree.
pub(super) fn sandbox_mode(context: &ProvisionContext) -> SandboxMode {
    if context.scope.session_id.is_some() {
        SandboxMode::Session
    } else {
        SandboxMode::Root
    }
}

/// The launch-specific writable roots handed to `usagi claude-sandbox`.
///
/// A managed session receives its own checkout. The caller adds the narrow Git
/// metadata roots resolved by [`session_git_policy`]; a root coordinator
/// receives no repository-local writable root.
pub(super) fn claude_writable_roots(mode: SandboxMode, working_directory: &Path) -> Vec<PathBuf> {
    if mode == SandboxMode::Session {
        vec![working_directory.to_path_buf()]
    } else {
        Vec::new()
    }
}

/// Resolve the optional Git administrative authority for a managed workspace.
/// Absence means a non-Git workspace; a present but malformed marker is an
/// admission failure rather than a silently weakened Git launch.
pub(super) fn session_git_common_dir(workspace_root: &Path) -> Result<Option<PathBuf>, ()> {
    match std::fs::symlink_metadata(workspace_root.join(".git")) {
        Ok(_) => git_common_dir(workspace_root).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

/// Git authority granted to one linked session worktree.
pub(super) struct SessionGitPolicy {
    pub(super) writable_roots: Vec<PathBuf>,
}

/// Resolve the narrow Git metadata set a linked session worktree needs for a
/// normal `git add` / `git commit` transaction.
///
/// Linked worktrees keep their index, `HEAD`, commit message and `HEAD` reflog
/// in a worktree-private administrative directory, but new objects and branch
/// refs live in the repository's common directory. Granting only the checkout
/// therefore makes file edits work while every commit fails with `EPERM`.
/// Granting the whole common directory would also expose `main`, remotes and
/// repository configuration, so the session receives only its private admin
/// directory, the shared content-addressed object store, and the `usagi/*`
/// branch ref / reflog namespaces. The mutable `.git` pointer is not authority:
/// both its common directory and its private-admin backlink must match the
/// daemon-selected workspace/worktree pair before any external path is granted.
pub(super) fn session_git_policy(
    workspace_root: &Path,
    worktree: &Path,
) -> Result<Option<SessionGitPolicy>, ()> {
    let marker = worktree.join(".git");
    let marker_metadata = match std::fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match std::fs::symlink_metadata(workspace_root.join(".git")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                _ => Err(()),
            };
        }
        Err(_) => return Err(()),
    };
    // A standalone repository needs no external Git grant: its administrative
    // directory is already contained by the writable checkout. Accept it only
    // when the selected session directory is the registered workspace itself;
    // a nested or foreign repository must not become additional authority.
    if !marker_metadata.file_type().is_file() {
        let workspace_root = workspace_root.canonicalize().map_err(|_| ())?;
        let worktree = worktree.canonicalize().map_err(|_| ())?;
        return if marker_metadata.file_type().is_dir() && worktree == workspace_root {
            Ok(None)
        } else {
            Err(())
        };
    }
    let git_dir = read_git_indirection(&marker, Some("gitdir:"), worktree)?;
    let common = session_git_common_dir(worktree)?.ok_or(())?;
    let expected_common = session_git_common_dir(workspace_root)?.ok_or(())?;
    if common != expected_common {
        return Err(());
    }
    let worktrees = common.join("worktrees").canonicalize().map_err(|_| ())?;
    if git_dir.parent() != Some(worktrees.as_path()) {
        return Err(());
    }
    let backlink = read_git_indirection(&git_dir.join("gitdir"), None, &git_dir)?;
    if backlink != marker.canonicalize().map_err(|_| ())? {
        return Err(());
    }

    let writable_roots = vec![
        git_dir,
        common.join("objects"),
        common.join("refs/heads/usagi"),
        common.join("logs/refs/heads/usagi"),
    ];
    for root in &writable_roots {
        validate_owned_directory(root).map_err(|_| ())?;
    }
    Ok(Some(SessionGitPolicy { writable_roots }))
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=claude_sandbox_e2e
pub(super) fn read_git_indirection(
    path: &Path,
    prefix: Option<&str>,
    relative_to: &Path,
) -> Result<PathBuf, ()> {
    let metadata = std::fs::metadata(path).map_err(|_| ())?;
    if metadata.len() > 16 * 1024 {
        return Err(());
    }
    let value = std::fs::read_to_string(path).map_err(|_| ())?;
    let value = value.trim();
    let value = match prefix {
        Some(prefix) => value
            .strip_prefix(prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        None => (!value.is_empty()).then_some(value),
    }
    .ok_or(())?;
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        relative_to.join(path)
    };
    path.canonicalize().map_err(|_| ())
}

/// Root providers may run the small read-only Git allowlist accepted by
/// `guard-workspace`. Override process-launching repository configuration and
/// optional index refreshes from daemon-owned, highest-precedence environment.
pub(super) fn insert_root_git_environment(spawn: &mut SpawnProvision) {
    for (name, value) in [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_COUNT", "5"),
        ("GIT_CONFIG_KEY_0", "core.fsmonitor"),
        ("GIT_CONFIG_VALUE_0", "false"),
        ("GIT_CONFIG_KEY_1", "core.hooksPath"),
        ("GIT_CONFIG_VALUE_1", "/dev/null"),
        ("GIT_CONFIG_KEY_2", "submodule.recurse"),
        ("GIT_CONFIG_VALUE_2", "false"),
        ("GIT_CONFIG_KEY_3", "status.submoduleSummary"),
        ("GIT_CONFIG_VALUE_3", "false"),
        ("GIT_CONFIG_KEY_4", "diff.ignoreSubmodules"),
        ("GIT_CONFIG_VALUE_4", "all"),
        ("GIT_OPTIONAL_LOCKS", "0"),
        ("GIT_PAGER", ""),
        ("GIT_EXTERNAL_DIFF", ""),
    ] {
        spawn.insert_daemon_environment(
            EnvironmentVariableName::new(name).expect("literal environment variable name is valid"),
            value.to_owned(),
        );
    }
}

/// A linked worktree may keep its Git common directory outside the checkout.
/// The root sandbox's host-wide writable areas must never cover that directory;
/// otherwise a read-only checkout would still leave refs/index authority writable.
///
/// `program` names the agent CLI this launch execs, so the check covers the same
/// `$HOME` state root the launcher will grant it.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=root_scope_keeps_checkout_and_git_common_dir_byte_identical
pub(super) fn validate_root_git_common_dir_policy(
    workspace_root: &Path,
    program: &str,
    tmpdir: Option<&Path>,
    home: Option<&Path>,
    cache_dir: Option<&Path>,
) -> Result<(), ()> {
    let common = git_common_dir(workspace_root)?;
    let mut writable = vec![PathBuf::from("/tmp"), PathBuf::from("/var/tmp")];
    writable.extend(tmpdir.map(Path::to_path_buf));
    if let Some(home) = home {
        writable
            .extend(claude_sandbox::agent_state_directory(program).map(|state| home.join(state)));
        if claude_sandbox::agent_config_prefix(program)
            .is_some_and(|prefix| lexical_prefix_overlaps_path(&home.join(prefix), &common))
        {
            return Err(());
        }
        if cfg!(target_os = "macos") {
            writable.push(home.join("Library/Keychains"));
        }
    }
    if cfg!(target_os = "macos") {
        writable.extend([
            PathBuf::from("/Library/Keychains"),
            PathBuf::from("/private/var/db/mds"),
        ]);
        writable.extend(cache_dir.map(claude_sandbox::macos_mds_cache_root));
    }
    let overlaps = writable.into_iter().any(|root| {
        let root = root.canonicalize().unwrap_or(root);
        common.starts_with(&root) || root.starts_with(&common)
    });
    (!overlaps).then_some(()).ok_or(())
}

/// Whether the lexical file family beginning at `prefix` intersects `path`'s
/// subtree. Non-UTF-8 paths cannot be represented in the launcher argv and are
/// therefore treated as an overlap (fail closed).
pub(super) fn lexical_prefix_overlaps_path(prefix: &Path, path: &Path) -> bool {
    let Some((prefix_text, path_text)) = prefix.to_str().zip(path.to_str()) else {
        return true;
    };
    path_text.starts_with(prefix_text) || prefix.starts_with(path)
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=root_scope_keeps_checkout_and_git_common_dir_byte_identical
pub(super) fn git_common_dir(workspace_root: &Path) -> Result<PathBuf, ()> {
    let marker = workspace_root.join(".git");
    let marker_path = marker.canonicalize().map_err(|_| ())?;
    let git_dir = if marker_path.is_dir() {
        marker_path
    } else {
        read_git_indirection(&marker_path, Some("gitdir:"), workspace_root)?
    };
    let common_marker = git_dir.join("commondir");
    if !common_marker.exists() {
        return Ok(git_dir);
    }
    read_git_indirection(&common_marker, None, &git_dir)
}

/// Policy paths are daemon-owned inputs.  Validate their identity before user
/// bindings (and therefore secrets) are resolved.  The launcher later receives
/// only these checked paths through argv and never consults its child environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClaudeSandboxPolicyError {
    MissingBackend,
    InvalidBackend,
    InvalidWritableRoot,
    ProtectedWorkspaceAncestor,
}

impl From<InvalidOwnedDirectory> for ClaudeSandboxPolicyError {
    fn from(_: InvalidOwnedDirectory) -> Self {
        Self::InvalidWritableRoot
    }
}

/// daemon が確定した、1 回の launch 分の sandbox policy 入力。
pub(super) struct SandboxPolicyInputs<'a> {
    pub(super) mode: SandboxMode,
    /// sandbox の中で exec する agent CLI（`claude` / `codex` / `codex-fugu` / `agy`）。root mode で
    /// launcher が足す `$HOME` 配下の state root（`~/.claude` / `~/.codex` / …）を決めるため、
    /// daemon 側の検証もこの program に追従する。
    pub(super) program: &'a str,
    pub(super) workspace_root: &'a Path,
    pub(super) launch_roots: &'a [PathBuf],
    pub(super) tmpdir: Option<&'a Path>,
    pub(super) home: Option<&'a Path>,
    /// macOS の per-user cache root。root mode の launcher はこの下の `mds`（Keychain 検索が
    /// 更新する MDS cache）を writable にするため、daemon 側も同じ root を検証する。
    pub(super) cache_dir: Option<&'a Path>,
    pub(super) backend: Option<&'a Path>,
    pub(super) passthrough: bool,
    /// writable provider state の内側を再び read-only にする、実在・検証済み file / directory。
    pub(super) read_only_roots: &'a [PathBuf],
}

/// launcher へ host path を渡す前に通す policy gate。writable root 集合・`$HOME` 配下の
/// state root・（root mode では）Git common dir を、保護対象 workspace と突き合わせる。
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=claude_sandbox_e2e
#[allow(clippy::too_many_lines)] // One gate keeps every host path validation and overlap rule atomic.
pub(super) fn validate_claude_sandbox_policy(
    policy: &SandboxPolicyInputs<'_>,
) -> Result<(), ClaudeSandboxPolicyError> {
    let SandboxPolicyInputs {
        mode,
        program,
        workspace_root,
        launch_roots,
        tmpdir,
        home,
        cache_dir,
        backend,
        passthrough,
        read_only_roots,
    } = *policy;
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err(ClaudeSandboxPolicyError::MissingBackend);
    }
    if passthrough {
        return Ok(());
    }
    let backend = backend.ok_or(ClaudeSandboxPolicyError::MissingBackend)?;
    validate_sandbox_backend(backend)?;
    if mode == SandboxMode::Root {
        validate_root_git_common_dir_policy(workspace_root, program, tmpdir, home, cache_dir)
            // Git common dir が writable 領域に入っていれば、read-only な checkout でも
            // refs / index の権威は書けてしまう。保護対象が writable の中にある同じ誤りである。
            .map_err(|()| ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor)?;
    }
    let protected_workspace = workspace_root
        .canonicalize()
        .map_err(|_| ClaudeSandboxPolicyError::InvalidWritableRoot)?;

    let mut roots = launch_roots.to_vec();
    if let Some(tmpdir) = tmpdir {
        roots.push(tmpdir.to_path_buf());
    }
    if let Some(cache_dir) = cache_dir {
        // 所有者と canonical 性は実在する cache root で確かめ、workspace との重なりは
        // launcher が実際に grant する `<cache>/mds` で判定する（この子はまだ存在しない
        // ことがあるので、存在を要求できるのは親だけである）。
        validate_owned_directory(cache_dir)?;
        let granted = claude_sandbox::macos_mds_cache_root(cache_dir);
        let granted = granted.canonicalize().unwrap_or(granted);
        if protected_workspace.starts_with(&granted)
            || (mode == SandboxMode::Root && granted.starts_with(&protected_workspace))
        {
            return Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor);
        }
    }
    if let Some(home) = home {
        validate_owned_directory(home)?;
        // State is a path subtree, whereas the global config is a lexical file
        // family (`~/.claude.json*`). Keep those overlap rules distinct.
        if let Some(state) = claude_sandbox::agent_state_directory(program) {
            let granted = home.join(state);
            let granted = granted.canonicalize().unwrap_or(granted);
            if protected_workspace.starts_with(&granted)
                || (mode == SandboxMode::Root && granted.starts_with(&protected_workspace))
            {
                return Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor);
            }
        }
        if claude_sandbox::agent_config_prefix(program).is_some_and(|prefix| {
            let granted = home.join(prefix);
            let protected_in_family = lexical_prefix_overlaps_path(&granted, &protected_workspace);
            protected_in_family
                && (protected_workspace
                    .to_str()
                    .zip(granted.to_str())
                    .is_none_or(|(protected, granted)| protected.starts_with(granted))
                    || mode == SandboxMode::Root)
        }) {
            return Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor);
        }
        if cfg!(target_os = "macos") {
            let keychains = home.join("Library/Keychains");
            let keychains = keychains.canonicalize().unwrap_or(keychains);
            if protected_workspace.starts_with(&keychains)
                || (mode == SandboxMode::Root && keychains.starts_with(&protected_workspace))
            {
                return Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor);
            }
        }
    }
    for root in roots {
        validate_owned_path(&root, true)?;
        let canonical = root
            .canonicalize()
            .map_err(|_| ClaudeSandboxPolicyError::InvalidWritableRoot)?;
        if canonical != root {
            return Err(ClaudeSandboxPolicyError::InvalidWritableRoot);
        }
        if protected_workspace.starts_with(&canonical)
            || (mode == SandboxMode::Root && canonical.starts_with(&protected_workspace))
        {
            return Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor);
        }
    }
    for root in read_only_roots {
        validate_owned_path(root, true)?;
        if root
            .canonicalize()
            .ok()
            .as_deref()
            .is_none_or(|canonical| canonical != root)
        {
            return Err(ClaudeSandboxPolicyError::InvalidWritableRoot);
        }
    }
    Ok(())
}

/// Materialization target が launcher の完全な write surface と重ならないことを確認する。
/// target 自体はまだ存在しなくてよく、呼び出し側はこの gate の後にだけ作成する。
pub(super) fn validate_isolated_sandbox_root(
    policy: &SandboxPolicyInputs<'_>,
    target: &Path,
) -> Result<(), ClaudeSandboxPolicyError> {
    if target.starts_with(policy.workspace_root) || policy.workspace_root.starts_with(target) {
        return Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor);
    }
    let request = claude_sandbox::SandboxRequest {
        platform: HOST_SANDBOX_PLATFORM,
        mode: policy.mode,
        protected_root: Some(policy.workspace_root.to_path_buf()),
        backend: None,
        launch_roots: policy.launch_roots.to_vec(),
        read_only_roots: policy.read_only_roots.to_vec(),
        tmpdir: policy.tmpdir.map(Path::to_path_buf),
        home: policy.home.map(Path::to_path_buf),
        linux_home_entries: None,
        cache_dir: policy.cache_dir.map(Path::to_path_buf),
        passthrough: false,
        command: vec![policy.program.to_owned()],
    };
    if claude_sandbox::writable_surface_overlaps(&request, target) {
        Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor)
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
const HOST_SANDBOX_PLATFORM: claude_sandbox::Platform = claude_sandbox::Platform::MacOs;
#[cfg(target_os = "linux")]
const HOST_SANDBOX_PLATFORM: claude_sandbox::Platform = claude_sandbox::Platform::Linux;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const HOST_SANDBOX_PLATFORM: claude_sandbox::Platform = claude_sandbox::Platform::Unsupported;

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=root_scope_cold_starts_through_the_out_of_sandbox_broker
pub(super) fn validate_sandbox_backend(path: &Path) -> Result<(), ClaudeSandboxPolicyError> {
    if !path.is_absolute() || path == Path::new("/") {
        return Err(ClaudeSandboxPolicyError::InvalidBackend);
    }
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| ClaudeSandboxPolicyError::InvalidBackend)?;
    if !metadata.file_type().is_file() || path.canonicalize().ok().as_deref() != Some(path) {
        return Err(ClaudeSandboxPolicyError::InvalidBackend);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ClaudeSandboxPolicyError::InvalidBackend);
        }
    }
    Ok(())
}

/// macOS の per-user cache root（`confstr(_CS_DARWIN_USER_CACHE_DIR)`）を canonical path で確定する。
///
/// Keychain 検索は Module Directory Service (MDS) の per-user cache（`<cache>/mds`）を更新するため、
/// root sandbox がここへ書けないと `SecKeychainSearchCreateFromAttributes` が失敗し、agent CLI は
/// Keychain の credential を読めないまま古い file 側 credential へ fallback して 401 で起動できない。
/// 値は `$TMPDIR` / `$HOME` と同じく daemon bootstrap の trusted environment で一度だけ確定し、
/// Agent child は再解決しない。macOS 以外には MDS が無いため `None` を返す。
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=root_policy_accepts_the_per_user_cache_root
pub(super) fn resolve_sandbox_cache_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        // confstr は終端 NUL を含む長さを返す。切り詰められた値は使わない（fail-closed）。
        let mut buffer = [0u8; 1024];
        let written = unsafe {
            libc::confstr(
                libc::_CS_DARWIN_USER_CACHE_DIR,
                buffer.as_mut_ptr().cast::<libc::c_char>(),
                buffer.len(),
            )
        };
        if written == 0 || written > buffer.len() {
            ErrorLog::record(
                "could not read the macOS per-user cache directory for the agent sandbox",
            );
            return None;
        }
        let Ok(text) = std::str::from_utf8(&buffer[..written - 1]) else {
            ErrorLog::record("the macOS per-user cache directory is not valid UTF-8");
            return None;
        };
        // ここが None のまま進むと、症状は「Keychain が読めず agent が 401 で起動できない」に
        // 戻る。原因が黙って消えないよう、解決できなかったことだけは残す。
        match PathBuf::from(text).canonicalize() {
            Ok(path) => Some(path),
            Err(error) => {
                ErrorLog::record(&format!(
                    "could not canonicalize the macOS per-user cache directory {text}: {error}"
                ));
                None
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// The policy paths the daemon bootstrap resolved once from its trusted
/// environment. The launcher re-validates each of them before it execs, and the
/// Agent child's own `PATH` / `TMPDIR` / `HOME` never reach this decision.
#[derive(Default)]
pub(super) struct SandboxLauncherPaths<'a> {
    pub(super) backend: Option<&'a Path>,
    pub(super) tmpdir: Option<&'a Path>,
    pub(super) home: Option<&'a Path>,
    /// macOS の per-user cache root。`<cache>/mds` を writable にするために渡す。
    pub(super) cache_dir: Option<&'a Path>,
}

/// `usagi claude-sandbox --mode <mode> [--writable-root <path>]… [--read-only-root <path>]… --`,
/// the ephemeral
/// instruction that makes the spawned child the launcher instead of the bare
/// product.  Host paths stay out of the durable launch snapshot.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=claude_sandbox_e2e
pub(super) fn claude_sandbox_launcher(
    usagi: &Path,
    mode: SandboxMode,
    protected_root: &Path,
    paths: &SandboxLauncherPaths<'_>,
    writable_roots: &[PathBuf],
    read_only_roots: &[PathBuf],
) -> Result<SandboxLauncher, ()> {
    let mut prefix = vec![
        "claude-sandbox".to_owned(),
        "--mode".to_owned(),
        mode.as_str().to_owned(),
        "--protected-root".to_owned(),
        protected_root.to_str().ok_or(())?.to_owned(),
    ];
    for (flag, path) in [
        ("--backend", paths.backend),
        ("--tmpdir", paths.tmpdir),
        ("--cache-dir", paths.cache_dir),
        ("--home", paths.home),
    ] {
        if let Some(path) = path {
            prefix.push(flag.to_owned());
            prefix.push(path.to_str().ok_or(())?.to_owned());
        }
    }
    for root in writable_roots {
        prefix.push("--writable-root".to_owned());
        prefix.push(root.to_str().ok_or(())?.to_owned());
    }
    for root in read_only_roots {
        prefix.push("--read-only-root".to_owned());
        prefix.push(root.to_str().ok_or(())?.to_owned());
    }
    prefix.push("--".to_owned());
    Ok(SandboxLauncher {
        program: usagi.to_str().ok_or(())?.to_owned(),
        prefix,
    })
}

/// `--settings <json>`: the scoped hook wiring Claude loads for this launch.
/// The payload is passed inline so no host path or rendered product payload has
/// to be materialized on disk.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_role_prompt_contract_reaches_every_shipping_agent_argv
pub(super) fn claude_settings_arguments(usagi: &Path) -> Result<Vec<String>, ()> {
    let usagi = usagi.to_str().ok_or(())?;
    Ok(vec!["--settings".to_owned(), scoped_settings_json(usagi)])
}

/// The scope-specific system prompt passed as one opaque argv value. Unlike the
/// hook command payload, this never crosses a shell or JSON boundary.
pub(super) fn claude_system_prompt_arguments(
    mode: SandboxMode,
    mcp: Option<McpToolFamilies>,
    role: Option<(&usagi_core::domain::role::RoleId, &str)>,
) -> Vec<String> {
    claude_prompt_arguments(launch_system_prompt(prompt_scope(mode), mcp, role))
}

/// The prompt boundary a sandbox mode launches into.
pub(super) const fn prompt_scope(mode: SandboxMode) -> PromptScope {
    match mode {
        SandboxMode::Root => PromptScope::Root,
        SandboxMode::Session => PromptScope::Session,
    }
}

pub(super) fn claude_prompt_arguments(prompt: String) -> Vec<String> {
    vec!["--append-system-prompt".to_owned(), prompt]
}

/// The configured environment for a launch in `workspace_root`, or nothing when
/// no reader is wired (tests that exercise only the MCP / sandbox wiring).
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_agent_fixture_is_injected_without_cli_credentials
pub(super) fn configured_environment(
    environment: Option<&Arc<SharedUserEnvironment>>,
    workspace_root: &Path,
) -> Result<BTreeMap<String, String>, user_env::UserEnvironmentError> {
    environment.map_or_else(
        || Ok(BTreeMap::new()),
        |environment| environment.resolved(workspace_root),
    )
}

/// The durable allowlist for a launch: the MCP names plus the configured
/// variable names. Only names are durable — values and secrets stay in the
/// ephemeral spawn provision.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_agent_fixture_is_injected_without_cli_credentials
pub(super) fn launch_allowlist(
    context: &ProvisionContext,
    user: &BTreeMap<String, String>,
) -> BTreeSet<EnvironmentVariableName> {
    let mut allowlist = mcp_environment_allowlist(context);
    allowlist.extend(user_env::allowlist(user));
    allowlist
}

/// The ephemeral spawn environment: the configured bindings first, then the
/// daemon's own MCP wiring, so a configured binding can never displace the
/// values that connect the child back to this daemon.
pub(super) fn launch_environment(
    user: &BTreeMap<String, String>,
    mcp: Vec<(EnvironmentVariableName, String)>,
) -> Vec<(EnvironmentVariableName, String)> {
    let mut environment = user_env::typed(user);
    environment.extend(mcp);
    environment
}

pub(super) fn mcp_environment_allowlist(
    context: &ProvisionContext,
) -> BTreeSet<EnvironmentVariableName> {
    if context.inject_mcp {
        [
            usagi_core::infrastructure::paths::DATA_DIR_ENV,
            usagi_core::infrastructure::paths::RUNTIME_MODE_ENV,
            usagi_core::infrastructure::paths::WORKSPACE_ROOT_ENV,
        ]
        .into_iter()
        .map(|name| {
            EnvironmentVariableName::new(name).expect("literal environment variable name is valid")
        })
        .collect()
    } else {
        BTreeSet::new()
    }
}

/// The child's data-home half of the contract: it receives the mode-neutral
/// base plus the mode that selects the daemon's own directory below it, so
/// re-applying the mode lands it on the very directory the daemon is using.
/// Both values come from the one [`paths::DataHome`] pair, never from separate
/// derivations that could disagree.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_agent_fixture_is_injected_without_cli_credentials
pub(super) fn mcp_environment(
    context: &ProvisionContext,
    data_home: &paths::DataHome,
    workspace_root: &Path,
) -> Result<Vec<(EnvironmentVariableName, String)>, ()> {
    context
        .inject_mcp
        .then(|| {
            Ok([
                (
                    EnvironmentVariableName::new(usagi_core::infrastructure::paths::DATA_DIR_ENV)
                        .expect("literal environment variable name is valid"),
                    data_home.base().to_str().ok_or(())?.to_owned(),
                ),
                (
                    EnvironmentVariableName::new(
                        usagi_core::infrastructure::paths::RUNTIME_MODE_ENV,
                    )
                    .expect("literal environment variable name is valid"),
                    data_home.mode().as_env_value().to_owned(),
                ),
                (
                    EnvironmentVariableName::new(
                        usagi_core::infrastructure::paths::WORKSPACE_ROOT_ENV,
                    )
                    .expect("literal environment variable name is valid"),
                    workspace_root.to_str().ok_or(())?.to_owned(),
                ),
            ])
        })
        .transpose()
        .map(Option::into_iter)
        .map(Iterator::flatten)
        .map(Iterator::collect)
}

/// Product-specific MCP and structured-hook launch arguments. They stay ephemeral in
/// [`SpawnProvision`] so the durable launch plan never stores configuration
/// paths or rendered product payloads.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_role_prompt_contract_reaches_every_shipping_agent_argv
pub(super) fn codex_integration_arguments(command: &Path) -> Result<Vec<String>, ()> {
    let command = command.to_str().ok_or(())?;
    let mut arguments = codex_product_mcp_arguments(command);
    arguments.extend(["-c".into(), r"features.hooks = true".into()]);
    for (event, phase) in AGENT_PHASE_HOOK_EVENTS {
        // Interactive Codex runs with approval_policy=never inside usagi's
        // outer sandbox, so PermissionRequest cannot fire. Notification is a
        // Claude-only hook event. Do not advertise either as a Codex signal.
        if matches!(event, "Notification" | "PermissionRequest") {
            continue;
        }
        let phase_command = format!("{} agent-phase {}", shell_quote(command), phase.as_token());
        let phase_command = serde_json::to_string(&phase_command).map_err(|_| ())?;
        let timeout = if event == "SessionEnd" { 3 } else { 10 };
        // `agent-phase ready` consumes the same SessionStart payload and sends
        // both its current session ID and phase in one private request. This
        // keeps startup, resume, clear, and compact transitions ordered.
        let groups = format!(
            r#"[{{ hooks = [{{ type = "command", command = {phase_command}, timeout = {timeout} }}] }}]"#
        );
        arguments.extend(["-c".into(), format!("hooks.{event} = {groups}")]);
    }
    Ok(arguments)
}

/// The scope-specific system prompt rendered as one Codex `-c` assignment.
/// Both argv elements stay ephemeral and precede the durable product argv.
pub(super) fn codex_system_prompt_arguments(
    mode: SandboxMode,
    mcp: Option<McpToolFamilies>,
    role: Option<(&usagi_core::domain::role::RoleId, &str)>,
) -> Vec<String> {
    codex_developer_instructions_arguments(&launch_system_prompt(prompt_scope(mode), mcp, role))
}

pub(super) fn codex_developer_instructions_arguments(prompt: &str) -> Vec<String> {
    vec![
        "-c".to_owned(),
        format!("developer_instructions={}", toml_basic_string(prompt)),
    ]
}

/// Renders a TOML basic string without involving a shell. The prompt contains
/// newlines, and callers may supply quotes, backslashes, or control characters,
/// so every character TOML forbids literally is escaped.
pub(super) fn toml_basic_string(text: &str) -> String {
    let mut rendered = String::with_capacity(text.len() + 2);
    rendered.push('"');
    for character in text.chars() {
        match character {
            '\\' => rendered.push_str(r"\\"),
            '"' => rendered.push_str(r#"\""#),
            '\u{0008}' => rendered.push_str(r"\b"),
            '\t' => rendered.push_str(r"\t"),
            '\n' => rendered.push_str(r"\n"),
            '\u{000c}' => rendered.push_str(r"\f"),
            '\r' => rendered.push_str(r"\r"),
            character if character.is_control() => {
                write!(&mut rendered, r"\u{:04X}", u32::from(character))
                    .expect("writing to a String cannot fail");
            }
            character => rendered.push(character),
        }
    }
    rendered.push('"');
    rendered
}

pub(super) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_disabled_family_leaves_both_the_registry_and_the_agent_prompt
pub(super) fn claude_mcp_arguments(command: &Path) -> Result<Vec<String>, ()> {
    let command = command.to_str().ok_or(())?;
    Ok(claude_product_mcp_arguments(command))
}

/// What the MCP server this launch injects will expose.
pub(super) struct ConfiguredMcpTools {
    pub(super) families: McpToolFamilies,
}

/// Resolve the effective MCP tool configuration for one launch.
///
/// This reads the settings; the rule that turns them into families lives in
/// `usagi-core` and is the same one `usagi mcp` builds its registry from, so the
/// prompt and `tools/list` cannot disagree.
///
/// What stays here is *which* configuration to read. The Global baseline is
/// overlaid with the **registered** workspace's `.usagi/settings.json`, because
/// that file is git-ignored and therefore exists only in the registered root,
/// never in a session worktree. Global settings live in the *selected* directory
/// — that is where `Storage::open_default` and the daemon's own
/// [`UserEnvironment`] write them — so this reads the same file those writers
/// own, not the mode-neutral base.
///
/// Unreadable settings fail the launch, exactly as they fail `usagi mcp` before
/// its serve loop starts. Falling back to the defaults here would launch an agent
/// whose prompt advertises tools its own MCP server could not register.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_disabled_family_leaves_both_the_registry_and_the_agent_prompt
pub(super) fn configured_mcp_tools(
    data_home: &paths::DataHome,
    workspace_root: &Path,
) -> Result<ConfiguredMcpTools, ()> {
    let resolve = || -> anyhow::Result<ConfiguredMcpTools> {
        let global = Storage::new(data_home.selected()).load_settings()?;
        let local = WorkspaceSettingsStore::new(workspace_root).load()?;
        let effective = global.with_local(&local);
        Ok(ConfiguredMcpTools {
            families: McpToolFamilies::from_settings(&effective),
        })
    };
    resolve().map_err(|error| {
        ErrorLog::record(&format!(
            "could not resolve MCP tool settings for {}: {error}",
            workspace_root.display()
        ));
    })
}
