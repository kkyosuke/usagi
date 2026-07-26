//! daemon 面へ Unix process / socket / signal を接続する composition adapter。

#![coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=root_ipc_fixture_codex_survives_disconnect_and_replays_final,planned_stop_retires_generation_endpoint_and_allows_safe_autostart

use std::backtrace::Backtrace;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::Deserialize;
use usagi_cli::cli::DaemonCommand as CliDaemonCommand;
use usagi_core::domain::AppInfo;
use usagi_core::domain::agent::{AgentProfileId, DurableLaunchSnapshot, EnvironmentVariableName};
use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
use usagi_core::domain::id::{SessionId, TerminalRef, WorkspaceId, WorktreeId};
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonReady, DaemonRecordStore, InstanceLock, LivenessProbe,
    ProcessIdentitySource, RecordFile, ShutdownSignal, Sleeper, Terminator, WorkspaceFence,
    WorkspaceFenceOutcome,
};
use usagi_core::infrastructure::env_resolver::OpCli;
use usagi_core::infrastructure::error_log::ErrorLog;
use usagi_core::infrastructure::ipc::{
    BuildArtifactDecision, BuildIdentity, BuildRolloverTrigger, ClientWorkspace,
    build_artifact_decision, build_rollover_trigger,
};
use usagi_core::infrastructure::paths;
use usagi_core::infrastructure::persistence::json_file;
use usagi_core::infrastructure::store::dispatch::DispatchStore;
use usagi_core::infrastructure::store::issue::AmbiguousIssueNumber;
use usagi_core::infrastructure::store::pr_inventory::PrInventoryStore;
use usagi_core::infrastructure::store::user_decision::UserDecisionStore;
use usagi_core::usecase::claude_sandbox::{self, SandboxMode};
use usagi_core::usecase::client::{
    ClientError, ClientPolicy, DaemonClient, DeadlineConnection, DeadlineStream, IpcClient,
    MonotonicClock, PolicyClient,
};
use usagi_core::usecase::client::{DaemonRequest, DispatchToolAction, SupervisorToolAction};
use usagi_daemon::infrastructure::child_identity::UnixChildProbe;
use usagi_daemon::infrastructure::generation_registry::{
    CurrentLocatorFile, GenerationRegistryFile,
};
use usagi_daemon::infrastructure::pty::PtyTerminal;
use usagi_daemon::infrastructure::unix_transport::{
    EndpointCleanup, EndpointLocator, SecureUnixListener, ensure_private_dir,
    ensure_private_dir_all, peer_pid, read_locator, retire_stale_current,
};
use usagi_daemon::presentation::{DaemonCommand as PresentationDaemonCommand, DaemonEnv};
use usagi_daemon::usecase::agent_ipc::{
    AgentRuntime, AgentTerminalActor, ResolvedAgentScope, ScopeResolveError, SessionScopeResolver,
    SharedTerminalOwner, TerminalOutcome,
};
use usagi_daemon::usecase::authority::activation::{
    AuthorityClaim, claim_authority, release_authority,
};
use usagi_daemon::usecase::authority::handoff::{
    LocatorObservation, PublishedLocator, RecoveryOutcome,
};
use usagi_daemon::usecase::authority::registry::{DEFAULT_GENERATION_LIMIT, GenerationRegistry};
use usagi_daemon::usecase::authority::rollover::CurrentLocator;
use usagi_daemon::usecase::claude::{
    ClaudeAdapter, ClaudeProvision, ClaudeProvisionFailure, ClaudeProvisioner, scoped_settings_json,
};
use usagi_daemon::usecase::codex::{
    CodexAdapter, CodexProvision, CodexProvisionFailure, CodexProvisioner,
};
use usagi_daemon::usecase::custody::{Custody, CustodyProbe, NodeIdentity};
use usagi_daemon::usecase::generation::{ProcessIdentity, ProcessObservation};
use usagi_daemon::usecase::generic_terminal::{
    GenericPtySpawner, TerminalProfileResolver, TerminalStore, TerminalStoreSnapshot,
};
use usagi_daemon::usecase::metrics::{MetricsBroker, MetricsObserver, MetricsSample};
use usagi_daemon::usecase::orchestration::AdapterRegistry;
use usagi_daemon::usecase::pr_inventory::{
    GhProcessPort, OutputPrProjector, RefreshClock, RefreshWorker,
};
use usagi_daemon::usecase::pr_projection::{
    PrProjection, PrProjectionQueue, pr_projection_counters,
};
use usagi_daemon::usecase::replacement::{
    LiveResources, ResourceCensus, SeamlessRefusal, TransitionMode, census_of, manual_operation_id,
    seamless_refusal,
};
use usagi_daemon::usecase::resources::identity::ChildProcessProbe;
use usagi_daemon::usecase::runtime::{
    OutputJournal, ProvisionContext, PtySpawner, RuntimeStore, RuntimeStoreSnapshot,
    SandboxLauncher, SpawnProvision, TerminateReapError,
};
use usagi_daemon::usecase::serve::{DaemonRecordPort, GenerationAuthority};
use usagi_daemon::usecase::session_runtime::{
    SessionRuntime, SessionRuntimeError, SharedSessionTeardown, SystemGit, WorktreeTeardown,
    perform_create, perform_remove,
};
use usagi_daemon::usecase::session_teardown::{
    TeardownEffect, TeardownJournal, TeardownSignal, drain_pending_teardowns,
};
use usagi_daemon::usecase::shutdown::ShutdownRequest;
use usagi_daemon::usecase::stop::{StaleCleanup, StaleDaemonCleanup};
use usagi_daemon::usecase::supervisor_runtime::{
    DecisionWake, DecisionWaker, InitialTask, SupervisorRuntime,
};
use usagi_daemon::usecase::terminal::{
    Geometry, Output, PtyWriteError, PtyWriter, SnapshotWire, SpawnFailure,
    output_pipeline_counters,
};
use usagi_daemon::usecase::terminal_ipc::{
    GenericTerminalRuntime, ResolvedTerminalScope, TerminalScopeResolveError, TerminalScopeResolver,
};
use usagi_daemon::usecase::terminal_profile::{LoginShellProfile, TERMINAL_ENVIRONMENT_VARIABLES};

use crate::runtime::user_env::{self, UserEnvironment};

/// The daemon's configured-environment reader, shared by the Agent adapters and
/// the terminal profile resolver.
type SharedUserEnvironment = UserEnvironment<OpCli>;

struct TrustedLoginShell {
    profile: LoginShellProfile,
    /// The configured environment for this daemon's repository, resolved at launch
    /// time. `None` in tests that exercise only the shell profile.
    environment: Option<Arc<SharedUserEnvironment>>,
    /// The repository the configured workspace bindings belong to.
    workspace_root: PathBuf,
}

impl TerminalProfileResolver for TrustedLoginShell {
    fn resolve(
        &mut self,
        request: &usagi_core::domain::terminal_launch::TerminalLaunchRequest,
    ) -> Result<
        usagi_core::domain::terminal_launch::ResolvedTerminalLaunch,
        usagi_core::domain::terminal_launch::TerminalLaunchValidationError,
    > {
        let resolved = self.profile.resolve(request)?;
        let Some(environment) = self.environment.as_ref() else {
            return Ok(resolved);
        };
        with_user_environment(resolved, &environment.resolved(&self.workspace_root))
    }
}

/// Add the configured environment to a resolved terminal launch.
///
/// Configured bindings win over the inherited terminal characteristics, which is
/// what makes a workspace able to override an ambient value. Their **names** join
/// the durable allowlist (values and secrets never do), because that allowlist is
/// what the launch boundary validates the ephemeral environment against.
fn with_user_environment(
    resolved: usagi_core::domain::terminal_launch::ResolvedTerminalLaunch,
    user: &BTreeMap<String, String>,
) -> Result<
    usagi_core::domain::terminal_launch::ResolvedTerminalLaunch,
    usagi_core::domain::terminal_launch::TerminalLaunchValidationError,
> {
    let mut snapshot = resolved.snapshot;
    let mut environment = resolved.environment;
    for (name, value) in user_env::typed(user) {
        snapshot.environment_allowlist.insert(name.clone());
        environment.insert(name, value.clone());
    }
    usagi_core::domain::terminal_launch::ResolvedTerminalLaunch::new(snapshot, environment)
}

fn terminal_environment() -> BTreeMap<String, String> {
    TERMINAL_ENVIRONMENT_VARIABLES
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| (name.to_owned(), value))
        })
        .collect()
}

struct FileTerminalStore(PathBuf);
impl TerminalStore for FileTerminalStore {
    fn save(&mut self, snapshot: TerminalStoreSnapshot) -> Result<(), ()> {
        let directory = snapshot_directory(&self.0).map_err(|_| ())?;
        json_file::write_atomic(directory, &self.0, &snapshot).map_err(|_| ())
    }
}

impl FileTerminalStore {
    /// Loads and fences terminal records which outlived their PTY-owning daemon.
    /// Invalid bytes or schema never reach launch admission and are not replaced.
    fn load_reconciled(&mut self) -> std::io::Result<(TerminalStoreSnapshot, usize)> {
        let snapshot = json_file::read::<TerminalStoreSnapshot>(&self.0)
            .map_err(std::io::Error::other)?
            .unwrap_or_default();
        let (snapshot, interrupted) = snapshot
            .reconcile_after_daemon_restart()
            .map_err(|_| std::io::Error::other("invalid generic terminal snapshot"))?;
        if interrupted != 0 {
            self.save(snapshot.clone())
                .map_err(|()| std::io::Error::other("could not reconcile terminal snapshot"))?;
        }
        Ok((snapshot, interrupted))
    }
}

/// Counts the live runtime a daemon owns, read from the two durable snapshots
/// it is the single writer of.
///
/// It deliberately reads rather than reconciles: a lifecycle verb that is about
/// to refuse must not rewrite the state it is refusing to destroy. Absent
/// snapshots mean a daemon that has never launched anything, and unreadable
/// ones are an error — never "nothing is live".
struct DurableResourceCensus {
    daemon_dir: PathBuf,
}

impl ResourceCensus for DurableResourceCensus {
    fn live(&self) -> std::io::Result<LiveResources> {
        let agents = json_file::read::<RuntimeStoreSnapshot>(&self.daemon_dir.join("agents.json"))
            .map_err(std::io::Error::other)?
            .unwrap_or_default();
        let terminals =
            json_file::read::<TerminalStoreSnapshot>(&self.daemon_dir.join("terminals.json"))
                .map_err(std::io::Error::other)?
                .unwrap_or_default();
        let agents: Vec<_> = agents.records.iter().map(|record| record.state).collect();
        let terminals: Vec<_> = terminals
            .records
            .iter()
            .map(|record| record.state)
            .collect();
        Ok(census_of(&agents, &terminals))
    }
}

/// Why this build cannot hand authority to a live successor, read from the
/// durable generation registry.
///
/// An unreadable or unparsable registry is reported as such rather than treated
/// as absent, so an operator sees the difference between "no daemon ever
/// registered a generation" and "the registry cannot be trusted".
fn observed_seamless_refusal(data_dir: &Path) -> SeamlessRefusal {
    match usagi_daemon::infrastructure::generation_registry::read_registry_document(data_dir) {
        Ok(document) => seamless_refusal(document.as_ref()),
        Err(error) => SeamlessRefusal::RegistryUnreadable(error.to_string()),
    }
}

/// Whether the operator explicitly gave up the live runtime a transition would
/// destroy.
const fn transition_mode(force: bool) -> TransitionMode {
    if force {
        TransitionMode::Cold
    } else {
        TransitionMode::Planned
    }
}

/// Persists the durable Agent runtime snapshot next to the terminal store.
struct FileRuntimeStore(PathBuf);
impl RuntimeStore for FileRuntimeStore {
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), ()> {
        let directory = snapshot_directory(&self.0).map_err(|_| ())?;
        json_file::write_atomic(directory, &self.0, &snapshot).map_err(|_| ())
    }
}

impl FileRuntimeStore {
    /// Reconcile a snapshot which outlived the daemon that owned its PTYs.
    /// Missing snapshots are normal on a first launch.  Parse/write failures
    /// deliberately leave the old bytes untouched so a later recovery can
    /// inspect the last known-good durable snapshot.
    fn reconcile_after_restart(&mut self) -> std::io::Result<RuntimeStoreSnapshot> {
        let Some(snapshot) =
            json_file::read::<RuntimeStoreSnapshot>(&self.0).map_err(std::io::Error::other)?
        else {
            return Ok(RuntimeStoreSnapshot::default());
        };
        snapshot.validate_schema().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid agent runtime snapshot schema: {error:?}"),
            )
        })?;
        snapshot.validate_ownership().map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid agent generation ownership: {error:?}"),
            )
        })?;
        let legacy = snapshot.schema_version < 3;
        let (snapshot, interrupted) = snapshot.reconcile_after_daemon_restart();
        if interrupted != 0 || legacy {
            self.save(snapshot.clone())
                .map_err(|()| std::io::Error::other("could not reconcile runtime snapshot"))?;
        }
        if interrupted != 0 {
            ErrorLog::record(&format!(
                "daemon startup reconciled {interrupted} agent runtime(s) as interrupted (identity_unknown)"
            ));
        }
        Ok(snapshot)
    }
}

/// Returns the durable snapshot's data directory.
fn snapshot_directory(path: &Path) -> std::io::Result<&Path> {
    path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "daemon snapshot path has no parent",
        )
    })
}

/// The registry's bounded in-memory replay buffer already serves reconnect
/// within retention; a durable on-disk output journal is intentionally deferred
/// with daemon-crash PTY FD continuation (out of scope for this issue).
struct DiscardJournal;
impl OutputJournal for DiscardJournal {
    fn append(&mut self, _output: &Output) -> Result<(), ()> {
        Ok(())
    }
}

/// Resolves the checkout path for a launch scope through the single managed
/// session writer, so agents never receive a client supplied path.
struct RootCodexProvisioner {
    sessions: SharedSessionRuntime,
    readiness: Arc<dyn AgentReadinessProbe>,
    mcp_command: PathBuf,
    data_home: PathBuf,
    /// The executable this profile launches: `codex`, or `codex-fugu` for the
    /// Codex-compatible `sakana-ai` profile.
    program: &'static str,
    /// The configured environment injected into the Agent child. `None` in tests
    /// that exercise only the MCP wiring.
    environment: Option<Arc<SharedUserEnvironment>>,
}
impl CodexProvisioner for RootCodexProvisioner {
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<CodexProvision, CodexProvisionFailure> {
        self.readiness
            .ready(self.program)
            .map_err(|()| CodexProvisionFailure::ExecutableUnavailable)?;
        let (working_directory, workspace_root) = working_directories(&self.sessions, context)
            .map_err(|()| CodexProvisionFailure::MaterializationFailed)?;
        let user = configured_environment(self.environment.as_ref(), &workspace_root);
        Ok(CodexProvision {
            working_directory,
            environment_allowlist: launch_allowlist(context, &user),
            spawn: SpawnProvision::new(
                launch_environment(
                    &user,
                    mcp_environment(context, &self.data_home, &workspace_root)
                        .map_err(|()| CodexProvisionFailure::MaterializationFailed)?,
                ),
                context
                    .inject_mcp
                    .then(|| codex_integration_arguments(&self.mcp_command))
                    .transpose()
                    .map_err(|()| CodexProvisionFailure::MaterializationFailed)?
                    .unwrap_or_default(),
            ),
        })
    }
}
struct RootClaudeProvisioner {
    sessions: SharedSessionRuntime,
    readiness: Arc<dyn AgentReadinessProbe>,
    mcp_command: PathBuf,
    data_home: PathBuf,
    /// The configured environment injected into the Agent child. `None` in tests
    /// that exercise only the sandbox and MCP wiring.
    environment: Option<Arc<SharedUserEnvironment>>,
    /// E2E テスト専用 seam（[`claude_sandbox::passthrough_requested`]）。true のとき launcher の子へ
    /// 同じ opt-in を伝え、backend の無い環境でも live 起動経路を通す。release ビルドでは常に false。
    sandbox_passthrough: bool,
}
impl ClaudeProvisioner for RootClaudeProvisioner {
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<ClaudeProvision, ClaudeProvisionFailure> {
        self.readiness
            .ready("claude")
            .map_err(|()| ClaudeProvisionFailure::ExecutableUnavailable)?;
        let (working_directory, workspace_root) = working_directories(&self.sessions, context)
            .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?;
        // Claude は必ず OS sandbox の中で起動する（多層防御の hard boundary）。論理境界の
        // `guard-workspace` フックは session 起動だけに配線し、root 起動では書き込みの境界を
        // sandbox の writable root に委ねる。
        let mode = sandbox_mode(context);
        let mut arguments = context
            .inject_mcp
            .then(|| claude_mcp_arguments(&self.mcp_command))
            .transpose()
            .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?
            .unwrap_or_default();
        arguments.extend(
            claude_settings_arguments(&self.mcp_command, mode)
                .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?,
        );
        let user = configured_environment(self.environment.as_ref(), &workspace_root);
        let mut spawn = SpawnProvision::new(
            launch_environment(
                &user,
                mcp_environment(context, &self.data_home, &workspace_root)
                    .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?,
            ),
            arguments,
        );
        spawn.set_sandbox_launcher(
            claude_sandbox_launcher(
                &self.mcp_command,
                mode,
                &claude_writable_roots(&working_directory, &workspace_root, &self.data_home),
            )
            .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?,
        );
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
fn sandbox_mode(context: &ProvisionContext) -> SandboxMode {
    if context.scope.session_id.is_some() {
        SandboxMode::Session
    } else {
        SandboxMode::Root
    }
}

/// The launch-specific writable roots handed to `usagi claude-sandbox`.  The
/// launcher adds the universal areas (`$TMPDIR`, `/tmp`, Claude state, …) itself.
/// A session launch therefore writes into its own worktree plus the shared usagi
/// state it must update (issue store, Git common dir, daemon data home), while a
/// root launch writes into the project root because that *is* its cwd.
fn claude_writable_roots(
    working_directory: &Path,
    workspace_root: &Path,
    data_home: &Path,
) -> Vec<PathBuf> {
    vec![
        working_directory.to_path_buf(),
        workspace_root.join(".usagi"),
        workspace_root.join(".git"),
        data_home.to_path_buf(),
    ]
}

/// `usagi claude-sandbox --mode <mode> [--writable-root <path>]… --`, the ephemeral
/// instruction that makes the spawned child the launcher instead of the bare
/// product.  Host paths stay out of the durable launch snapshot.
fn claude_sandbox_launcher(
    usagi: &Path,
    mode: SandboxMode,
    writable_roots: &[PathBuf],
) -> Result<SandboxLauncher, ()> {
    let mut prefix = vec![
        "claude-sandbox".to_owned(),
        "--mode".to_owned(),
        mode.as_str().to_owned(),
    ];
    for root in writable_roots {
        prefix.push("--writable-root".to_owned());
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
fn claude_settings_arguments(usagi: &Path, mode: SandboxMode) -> Result<Vec<String>, ()> {
    let usagi = usagi.to_str().ok_or(())?;
    Ok(vec![
        "--settings".to_owned(),
        scoped_settings_json(usagi, mode == SandboxMode::Session),
    ])
}

/// The configured environment for a launch in `workspace_root`, or nothing when
/// no reader is wired (tests that exercise only the MCP / sandbox wiring).
fn configured_environment(
    environment: Option<&Arc<SharedUserEnvironment>>,
    workspace_root: &Path,
) -> BTreeMap<String, String> {
    environment.map_or_else(BTreeMap::new, |environment| {
        environment.resolved(workspace_root)
    })
}

/// The durable allowlist for a launch: the MCP names plus the configured
/// variable names. Only names are durable — values and secrets stay in the
/// ephemeral spawn provision.
fn launch_allowlist(
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
fn launch_environment(
    user: &BTreeMap<String, String>,
    mcp: Vec<(EnvironmentVariableName, String)>,
) -> Vec<(EnvironmentVariableName, String)> {
    let mut environment = user_env::typed(user);
    environment.extend(mcp);
    environment
}

fn mcp_environment_allowlist(context: &ProvisionContext) -> BTreeSet<EnvironmentVariableName> {
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

fn mcp_environment(
    context: &ProvisionContext,
    data_home: &Path,
    workspace_root: &Path,
) -> Result<Vec<(EnvironmentVariableName, String)>, ()> {
    context
        .inject_mcp
        .then(|| {
            Ok([
                (
                    EnvironmentVariableName::new(usagi_core::infrastructure::paths::DATA_DIR_ENV)
                        .expect("literal environment variable name is valid"),
                    data_home.to_str().ok_or(())?.to_owned(),
                ),
                (
                    EnvironmentVariableName::new(
                        usagi_core::infrastructure::paths::RUNTIME_MODE_ENV,
                    )
                    .expect("literal environment variable name is valid"),
                    match paths::runtime_mode() {
                        paths::RuntimeMode::Production => "production",
                        paths::RuntimeMode::Development => "development",
                        paths::RuntimeMode::Local => "local",
                    }
                    .to_owned(),
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
fn codex_integration_arguments(command: &Path) -> Result<Vec<String>, ()> {
    let command = command.to_str().ok_or(())?;
    let hook_command = format!("{} codex-session-capture", shell_quote(command));
    let hook_command = serde_json::to_string(&hook_command).map_err(|_| ())?;
    let command = serde_json::to_string(command).map_err(|_| ())?;
    Ok(vec![
        "-c".into(),
        format!("mcp_servers.usagi.command = {command}"),
        "-c".into(),
        r#"mcp_servers.usagi.args = ["mcp"]"#.into(),
        // This is deliberately scoped to the daemon-provisioned `usagi` MCP
        // server. Codex keeps its normal approval policy for shell commands,
        // file edits, network access, and every other MCP server.
        // Codex starts stdio MCP servers with an explicit environment allowlist.
        // Forward the daemon-selected data home and runtime-fenced credential
        // so the MCP child reaches the owning daemon and proves its owner.
        "-c".into(),
        r#"mcp_servers.usagi.env_vars = ["USAGI_HOME", "USAGI_RUNTIME_MODE", "USAGI_WORKSPACE_ROOT", "USAGI_MCP_CALLER_CREDENTIAL"]"#.into(),
        "-c".into(),
        r#"mcp_servers.usagi.default_tools_approval_mode = "approve""#.into(),
        // SessionStart is Codex's documented structured lifecycle channel. It
        // sends a JSON object containing the current `session_id` on stdin.
        // Restrict capture to a newly-created provider conversation: explicit
        // resume already carries its validated durable provider identity.
        "-c".into(),
        r"features.hooks = true".into(),
        "-c".into(),
        format!(
            r#"hooks.SessionStart = [{{ matcher = "^startup$", hooks = [{{ type = "command", command = {hook_command}, timeout = 10 }}] }}]"#
        ),
    ])
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}

fn claude_mcp_arguments(command: &Path) -> Result<Vec<String>, ()> {
    let command = command.to_str().ok_or(())?;
    let config = serde_json::json!({
        "mcpServers": {
            "usagi": {
                "command": command,
                "args": ["mcp"],
            }
        }
    });
    // Pre-approve only the injected `usagi` server's tools so the agent never
    // hits a consent prompt for usagi MCP calls.  Claude scopes `mcp__<server>`
    // to that one server (wildcards are unsupported), so Bash, file edits, other
    // MCP servers, and network stay under the normal permission model — this is
    // deliberately narrower than `--dangerously-skip-permissions`.
    Ok(vec![
        "--mcp-config".into(),
        config.to_string(),
        "--allowedTools".into(),
        "mcp__usagi".into(),
    ])
}

/// Product-owned, non-secret pre-spawn readiness boundary.  Implementations
/// may discover an executable and invoke its public status command, but never
/// read, persist, or return credentials, configuration paths, argv, or raw OS
/// failures.  Keeping it injected makes the root composable with fixture
/// executables without installing or authenticating a real CLI.
trait AgentReadinessProbe: Send + Sync {
    fn ready(&self, product: &str) -> Result<(), ()>;
}

struct SystemAgentReadiness;
impl AgentReadinessProbe for SystemAgentReadiness {
    fn ready(&self, product: &str) -> Result<(), ()> {
        let (command, args) = match product {
            "codex" => ("codex", ["login", "status"]),
            "claude" => ("claude", ["auth", "status"]),
            _ => return Err(()),
        };
        Command::new(command)
            .args(args)
            .status()
            .ok()
            .filter(std::process::ExitStatus::success)
            .map(|_| ())
            .ok_or(())
    }
}
fn working_directories(
    sessions: &SharedSessionRuntime,
    context: &ProvisionContext,
) -> Result<(PathBuf, PathBuf), ()> {
    let runtime = sessions.lock().map_err(|_| ())?;
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

/// The #268 scope resolver, adapted to the Agent owner's product-neutral
/// `(workspace, session)` input by deriving the available session's worktree.
struct SharedScopeResolver(SharedSessionRuntime);
impl SessionScopeResolver for SharedScopeResolver {
    fn resolve_available_scope(
        &self,
        workspace: WorkspaceId,
        session: Option<SessionId>,
    ) -> Result<ResolvedAgentScope, ScopeResolveError> {
        let runtime = self.0.lock().map_err(|_| ScopeResolveError::Storage)?;
        // A workspace-root agent (no session) resolves to the trusted repository
        // root and its durable root-worktree identity; a session agent resolves
        // that session's available worktree. Neither trusts a client path.
        let Some(session) = session else {
            let worktree_id = runtime.root_worktree_id();
            let working_directory = runtime
                .resolve_root_scope(workspace, worktree_id)
                .map_err(|_| ScopeResolveError::Unavailable)?;
            return Ok(ResolvedAgentScope {
                worktree_id,
                working_directory,
            });
        };
        let snapshot = runtime
            .snapshot()
            .map_err(|_: SessionRuntimeError| ScopeResolveError::Storage)?;
        let worktree_id =
            available_worktree(&snapshot, session).ok_or(ScopeResolveError::Unavailable)?;
        let scope = runtime
            .resolve_scope(workspace, session, worktree_id)
            .map_err(|_| ScopeResolveError::Unavailable)?;
        Ok(ResolvedAgentScope {
            worktree_id: scope.worktree_id,
            working_directory: scope.path,
        })
    }
}

/// Resolves the complete client fence for a generic terminal. Unlike the Agent
/// resolver, generic terminal requests already carry a worktree ID, so the
/// runtime verifies that exact identity before admitting a PTY spawn.
struct SharedTerminalScopeResolver(SharedSessionRuntime);
impl TerminalScopeResolver for SharedTerminalScopeResolver {
    fn resolve_available_scope(
        &self,
        requested: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Result<ResolvedTerminalScope, TerminalScopeResolveError> {
        let runtime = self
            .0
            .lock()
            .map_err(|_| TerminalScopeResolveError::Unavailable)?;
        // A workspace-root scope (no session) resolves to the trusted repository
        // root; a session scope resolves that session's worktree. Neither path
        // trusts a client supplied path.
        let working_directory = match requested.session_id {
            None => runtime
                .resolve_root_scope(requested.workspace_id, requested.worktree_id)
                .map_err(|_| TerminalScopeResolveError::Unavailable)?,
            Some(session) => {
                runtime
                    .resolve_scope(requested.workspace_id, session, requested.worktree_id)
                    .map_err(|_| TerminalScopeResolveError::Unavailable)?
                    .path
            }
        };
        Ok(ResolvedTerminalScope {
            scope: requested.clone(),
            working_directory,
        })
    }
}
fn available_worktree(snapshot: &serde_json::Value, session: SessionId) -> Option<WorktreeId> {
    let target = serde_json::to_value(session).ok()?;
    snapshot
        .get("sessions")?
        .as_array()?
        .iter()
        .find(|candidate| {
            candidate.get("session_id") == Some(&target)
                && candidate
                    .get("lifecycle")
                    .and_then(serde_json::Value::as_str)
                    == Some("available")
        })
        .and_then(|candidate| serde_json::from_value(candidate.get("worktree_id")?.clone()).ok())
}

type RootAgentRuntime = AgentRuntime;
type SharedAgentRuntime = Arc<Mutex<RootAgentRuntime>>;
type SharedSupervisorRuntime = Arc<Mutex<SupervisorRuntime>>;

struct DeferredDecisionWaker;
impl DecisionWaker for DeferredDecisionWaker {
    fn wake(&mut self, _: &DecisionWake) -> anyhow::Result<()> {
        anyhow::bail!("parent agent wake adapter is unavailable")
    }
}

/// Locks the shared Agent owner for one terminal request; a poisoned lock is a
/// safe unavailable error rather than a client-side fallback.
struct SharedAgent(SharedAgentRuntime);
impl AgentTerminalActor for SharedAgent {
    fn handle_terminal(
        &mut self,
        connection: usagi_core::domain::id::ConnectionId,
        client: usagi_core::domain::id::ClientId,
        request_id: usagi_core::domain::id::RequestId,
        action: usagi_core::usecase::client::TerminalAction,
        request: usagi_core::usecase::client::TerminalRequest,
        wire: SnapshotWire,
    ) -> TerminalOutcome {
        match self.0.lock() {
            Ok(mut agent) => AgentTerminalActor::handle_terminal(
                &mut *agent,
                connection,
                client,
                request_id,
                action,
                request,
                wire,
            ),
            Err(_) => {
                TerminalOutcome::Handled(Err(usagi_core::infrastructure::ipc::ProtocolError::new(
                    usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                    "agent owner is unavailable",
                )))
            }
        }
    }
    // Composition glue: locks the shared runtime and delegates. The merge,
    // scope filtering, and redaction the inventory actually performs are
    // verified by `SharedTerminalOwner`'s fake in `usagi_daemon::usecase::agent_ipc`
    // (no test drives the real serve loop, which is where this lock wrapper is
    // reached), so only the lock/poison delegation lives here.
    fn terminal_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry> {
        // A poisoned lock is a safe empty inventory, never a client fallback.
        self.0
            .lock()
            .map(|agent| AgentTerminalActor::terminal_inventory(&*agent, scope))
            .unwrap_or_default()
    }
    fn completed_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_visibility::CompletedTerminalEntry> {
        // A poisoned lock is a safe empty tombstone list, never a fallback.
        self.0
            .lock()
            .map(|agent| AgentTerminalActor::completed_inventory(&*agent, scope))
            .unwrap_or_default()
    }
    fn disconnect(&mut self, connection: usagi_core::domain::id::ConnectionId) {
        if let Ok(mut agent) = self.0.lock() {
            AgentTerminalActor::disconnect(&mut *agent, connection);
        }
    }
}

enum AgentPtyObservation {
    Output(TerminalRef, Vec<u8>),
    Exited(TerminalRef, i32),
}

const PTY_OBSERVATION_QUEUE_ITEMS: usize = 64;

/// Process-local counters for the bounded PTY-to-registry pipeline. They only
/// contain byte counts; terminal output and terminal identities are never
/// recorded in metrics or logs.
#[derive(Default)]
struct TerminalPipelineMetrics {
    backpressured_bytes: AtomicU64,
}

impl TerminalPipelineMetrics {
    fn observe_backpressure(&self, bytes: usize) {
        self.backpressured_bytes
            .fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
    }
}

/// The daemon-owned PTY spawner/writer for Agent runtimes.  It spawns the real
/// rendered plan, drains output to the Agent owner, and reaps the child to
/// commit a durable exit — never a client-driven process.
struct AgentPty {
    terminals: BTreeMap<String, OwnedPty>,
    selected: Option<String>,
    observations: SyncSender<AgentPtyObservation>,
    metrics: Arc<TerminalPipelineMetrics>,
    environment: BTreeMap<String, String>,
}

struct OwnedPty {
    terminal: TerminalRef,
    pty: Arc<Mutex<PtyTerminal>>,
}

fn release_owned_pty(
    terminals: &mut BTreeMap<String, OwnedPty>,
    selected: &mut Option<String>,
    terminal: &TerminalRef,
) -> bool {
    let key = terminal.terminal_id.as_str();
    let owned = terminals
        .get(&key)
        .is_some_and(|entry| entry.terminal.fences(terminal));
    if owned {
        terminals.remove(&key);
        if selected.as_ref() == Some(&key) {
            *selected = None;
        }
    }
    owned
}
impl AgentPty {
    fn new(
        environment: BTreeMap<String, String>,
        metrics: Arc<TerminalPipelineMetrics>,
    ) -> (Self, Receiver<AgentPtyObservation>) {
        let (observations, receiver) = mpsc::sync_channel(PTY_OBSERVATION_QUEUE_ITEMS);
        (
            Self {
                terminals: BTreeMap::new(),
                selected: None,
                observations,
                metrics,
                environment,
            },
            receiver,
        )
    }
}
impl PtySpawner for AgentPty {
    fn spawn(
        &mut self,
        launch: &DurableLaunchSnapshot,
        provision: &SpawnProvision,
        terminal: &TerminalRef,
    ) -> Result<ProcessIdentity, SpawnFailure> {
        let plan = &launch.plan;
        // Product provisioning contributes global CLI options (MCP/config/hooks),
        // which must precede product subcommands and the optional prompt after
        // `--`.  The provision stays non-durable even though it is part of the
        // one-time process invocation.  When a sandbox launcher is present
        // (Claude), the spawned child is the usagi binary running
        // `claude-sandbox … -- <program> …`, so the product only ever runs
        // confined; the durable snapshot still records the bare product program.
        let (program, mut argv) = match provision.sandbox_launcher() {
            Some(launcher) => {
                let mut argv = launcher.prefix.clone();
                argv.push(plan.program.clone());
                (launcher.program.clone(), argv)
            }
            None => (plan.program.clone(), Vec::new()),
        };
        argv.extend(provision.arguments().iter().cloned());
        argv.extend(plan.argv.iter().cloned());
        let environment = provision.compose_environment(&self.environment);
        let pty = PtyTerminal::spawn_with(
            &program,
            &argv,
            &environment.into_iter().collect::<Vec<_>>(),
            &plan.working_directory,
            Geometry { cols: 80, rows: 24 },
        )
        .map_err(|_| SpawnFailure::Definite)?;
        let pid = pty.process_id().ok_or(SpawnFailure::Ambiguous)?;
        let reader = pty.reader().map_err(|_| SpawnFailure::Ambiguous)?;
        let pty = Arc::new(Mutex::new(pty));
        self.terminals.insert(
            terminal.terminal_id.as_str().clone(),
            OwnedPty {
                terminal: terminal.clone(),
                pty: Arc::clone(&pty),
            },
        );
        let observations = self.observations.clone();
        let metrics = Arc::clone(&self.metrics);
        let output_terminal = terminal.clone();
        let exit_pty = Arc::clone(&pty);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut bytes = [0_u8; 4096];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                let observation =
                    AgentPtyObservation::Output(output_terminal.clone(), bytes[..count].to_vec());
                if send_agent_observation(&observations, observation, count, &metrics).is_err() {
                    return;
                }
            }
            if let Ok(status) = exit_pty
                .lock()
                .map_or(Err(()), |pty| pty.wait().map_err(|_| ()))
            {
                let _ = observations.send(AgentPtyObservation::Exited(output_terminal, status));
            }
        });
        Ok(ProcessIdentity {
            pid,
            start_identity: "daemon-owned-agent-pty".to_owned(),
            process_group: pid,
        })
    }

    fn terminate_reap(&mut self, terminal: &TerminalRef) -> Result<(), TerminateReapError> {
        let key = terminal.terminal_id.as_str();
        let pty = Arc::clone(
            &self
                .terminals
                .get(&key)
                .filter(|entry| entry.terminal.fences(terminal))
                .ok_or(TerminateReapError)?
                .pty,
        );
        pty.lock()
            .map_err(|_| TerminateReapError)?
            .terminate_reap()
            .map_err(|_| TerminateReapError)?;
        release_owned_pty(&mut self.terminals, &mut self.selected, terminal);
        Ok(())
    }
}

fn send_agent_observation(
    sender: &SyncSender<AgentPtyObservation>,
    observation: AgentPtyObservation,
    bytes: usize,
    metrics: &TerminalPipelineMetrics,
) -> Result<(), ()> {
    match sender.try_send(observation) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(observation)) => {
            metrics.observe_backpressure(bytes);
            sender.send(observation).map_err(|_| ())
        }
        Err(TrySendError::Disconnected(_)) => Err(()),
    }
}
impl PtyWriter for AgentPty {
    fn select_terminal(&mut self, terminal: &TerminalRef) {
        self.selected = Some(terminal.terminal_id.as_str().clone());
    }
    fn resize(&mut self, terminal: &TerminalRef, geometry: Geometry) -> Result<(), PtyWriteError> {
        let Some(entry) = self
            .terminals
            .get(&terminal.terminal_id.as_str())
            .filter(|entry| entry.terminal.fences(terminal))
        else {
            return Err(PtyWriteError { applied_prefix: 0 });
        };
        entry
            .pty
            .lock()
            .map_err(|_| PtyWriteError { applied_prefix: 0 })?
            .resize(geometry)
            .map_err(|_| PtyWriteError { applied_prefix: 0 })
    }
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
        let Some(key) = self.selected.as_ref() else {
            return Err(PtyWriteError { applied_prefix: 0 });
        };
        let Some(terminal) = self.terminals.get(key) else {
            return Err(PtyWriteError { applied_prefix: 0 });
        };
        terminal
            .pty
            .lock()
            .map_err(|_| PtyWriteError { applied_prefix: 0 })?
            .write_all(bytes)
    }
    fn release(&mut self, terminal: &TerminalRef) -> bool {
        release_owned_pty(&mut self.terminals, &mut self.selected, terminal)
    }
}

enum PtyObservation {
    Output(usagi_core::domain::id::TerminalRef, Vec<u8>),
    Exited(usagi_core::domain::id::TerminalRef, i32),
}

struct DaemonPty {
    terminals: BTreeMap<String, OwnedPty>,
    selected: Option<String>,
    observations: SyncSender<PtyObservation>,
    metrics: Arc<TerminalPipelineMetrics>,
}
impl DaemonPty {
    fn new(metrics: Arc<TerminalPipelineMetrics>) -> (Self, Receiver<PtyObservation>) {
        let (observations, receiver) = mpsc::sync_channel(PTY_OBSERVATION_QUEUE_ITEMS);
        (
            Self {
                terminals: BTreeMap::new(),
                selected: None,
                observations,
                metrics,
            },
            receiver,
        )
    }
}
impl GenericPtySpawner for DaemonPty {
    fn spawn(
        &mut self,
        launch: &usagi_core::domain::terminal_launch::ResolvedTerminalLaunch,
        terminal: &usagi_core::domain::id::TerminalRef,
        geometry: Geometry,
    ) -> Result<ProcessIdentity, SpawnFailure> {
        let environment = launch
            .environment
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
            .collect::<Vec<_>>();
        let pty = PtyTerminal::spawn_with(
            &launch.snapshot.program,
            &launch.snapshot.arguments,
            &environment,
            &launch.snapshot.working_directory,
            geometry,
        )
        .map_err(|_| SpawnFailure::Definite)?;
        let pid = pty.process_id().ok_or(SpawnFailure::Ambiguous)?;
        let reader = pty.reader().map_err(|_| SpawnFailure::Ambiguous)?;
        let pty = Arc::new(Mutex::new(pty));
        self.terminals.insert(
            terminal.terminal_id.as_str().clone(),
            OwnedPty {
                terminal: terminal.clone(),
                pty: Arc::clone(&pty),
            },
        );
        let output_sender = self.observations.clone();
        let metrics = Arc::clone(&self.metrics);
        let output_terminal = terminal.clone();
        let exit_pty = Arc::clone(&pty);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut bytes = [0_u8; 4096];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                let observation =
                    PtyObservation::Output(output_terminal.clone(), bytes[..count].to_vec());
                if send_pty_observation(&output_sender, observation, count, &metrics).is_err() {
                    break;
                }
            }
            if let Ok(status) = exit_pty
                .lock()
                .map_or(Err(()), |pty| pty.wait().map_err(|_| ()))
            {
                let _ = output_sender.send(PtyObservation::Exited(output_terminal, status));
            }
        });
        Ok(ProcessIdentity {
            pid,
            start_identity: "daemon-owned-pty".to_owned(),
            process_group: pid,
        })
    }
}

fn send_pty_observation(
    sender: &SyncSender<PtyObservation>,
    observation: PtyObservation,
    bytes: usize,
    metrics: &TerminalPipelineMetrics,
) -> Result<(), ()> {
    match sender.try_send(observation) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(observation)) => {
            metrics.observe_backpressure(bytes);
            sender.send(observation).map_err(|_| ())
        }
        Err(TrySendError::Disconnected(_)) => Err(()),
    }
}
impl PtyWriter for DaemonPty {
    fn select_terminal(&mut self, terminal: &usagi_core::domain::id::TerminalRef) {
        self.selected = Some(terminal.terminal_id.as_str().clone());
    }
    fn resize(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        geometry: Geometry,
    ) -> Result<(), PtyWriteError> {
        let Some(entry) = self
            .terminals
            .get(&terminal.terminal_id.as_str())
            .filter(|entry| entry.terminal.fences(terminal))
        else {
            return Err(PtyWriteError { applied_prefix: 0 });
        };
        entry
            .pty
            .lock()
            .map_err(|_| PtyWriteError { applied_prefix: 0 })?
            .resize(geometry)
            .map_err(|_| PtyWriteError { applied_prefix: 0 })
    }
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
        let Some(key) = self.selected.as_ref() else {
            return Err(PtyWriteError { applied_prefix: 0 });
        };
        let Some(terminal) = self.terminals.get(key) else {
            return Err(PtyWriteError { applied_prefix: 0 });
        };
        terminal
            .pty
            .lock()
            .map_err(|_| PtyWriteError { applied_prefix: 0 })?
            .write_all(bytes)
    }
    fn release(&mut self, terminal: &TerminalRef) -> bool {
        release_owned_pty(&mut self.terminals, &mut self.selected, terminal)
    }
}

struct SharedTerminal(
    Arc<
        Mutex<
            GenericTerminalRuntime<
                TrustedLoginShell,
                FileTerminalStore,
                DaemonPty,
                SharedTerminalScopeResolver,
            >,
        >,
    >,
);
type SharedSessionRuntime = Arc<Mutex<SessionRuntime>>;
type SharedTerminalRuntime = Arc<
    Mutex<
        GenericTerminalRuntime<
            TrustedLoginShell,
            FileTerminalStore,
            DaemonPty,
            SharedTerminalScopeResolver,
        >,
    >,
>;
type SharedPrInventory = Arc<Mutex<OutputPrProjector<PrInventoryStore>>>;

/// How often the PR refresh worker claims due work.
///
/// This bounds how quickly a freshly detected PR gets its title and state, and
/// each tick claims at most [`PR_REFRESH_PER_TICK`] identities against a 60 s
/// freshness window. Now that the wait is edge-driven rather than a 10 ms poll,
/// the tick costs one wakeup, so there is no reason to lengthen it.
const PR_REFRESH_TICK: Duration = Duration::from_millis(250);
const PR_REFRESH_FRESHNESS_MS: u64 = 60_000;
const PR_REFRESH_PER_TICK: usize = 2;
/// How often a serving daemon re-checks that it is still the authority for its
/// data directory. One second is short enough that an abandoned daemon exits
/// promptly and long enough that the two `stat`s are free.
const CUSTODY_TICK: Duration = Duration::from_secs(1);

/// How long the teardown worker waits for an admitted removal before deriving
/// the pending set again anyway. An admission wakes it immediately, so this only
/// bounds the retry of a teardown whose durable finalization failed.
const SESSION_TEARDOWN_TICK: Duration = Duration::from_secs(1);

/// How long the accept loop waits after an accept error that may have left the
/// connection queued. This is the error path only: an idle daemon parks on
/// descriptor readiness and never reaches it.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(10);

/// How often the decision maintenance worker makes due expiries durable and
/// drains the resolved-decision outbox.
///
/// This bounds how long an already expired decision can still be read as
/// `Pending`. A tick that finds nothing due performs two small reads and no
/// write: expiry no longer takes the store lock or fsyncs unless something
/// actually changed.
const DECISION_MAINTENANCE_TICK: Duration = Duration::from_millis(250);

struct ProductionRefreshClock {
    started: Instant,
}

impl RefreshClock for ProductionRefreshClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

struct GhProcess;

impl GhProcessPort for GhProcess {
    type Error = std::io::Error;

    fn run(
        &mut self,
        program: &str,
        argv: &[String],
        timeout_ms: u64,
    ) -> Result<String, Self::Error> {
        let mut child = Command::new(program)
            .args(argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if let Some(status) = child.try_wait()? {
                let mut output = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    stdout.read_to_string(&mut output)?;
                }
                return status
                    .success()
                    .then_some(output)
                    .ok_or_else(|| std::io::Error::other("PR provider failed"));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "PR provider timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Supplies raw process-resource observations to the metrics authority.
struct ProcessResourceSampler {
    previous: Option<(Instant, u64)>,
}

impl ProcessResourceSampler {
    fn snapshot(&mut self) -> (u32, u64) {
        let now = Instant::now();
        let Some((cpu_micros, resident_memory_bytes)) = process_resource_usage() else {
            return (0, 0);
        };
        let cpu_percent_hundredths = self.previous.map_or(0, |(then, previous_cpu_micros)| {
            let elapsed_micros =
                u64::try_from(now.duration_since(then).as_micros()).unwrap_or(u64::MAX);
            let used_micros = cpu_micros.saturating_sub(previous_cpu_micros);
            u32::try_from(
                used_micros
                    .saturating_mul(10_000)
                    .checked_div(elapsed_micros)
                    .unwrap_or(0),
            )
            .unwrap_or(u32::MAX)
        });
        self.previous = Some((now, cpu_micros));
        (cpu_percent_hundredths, resident_memory_bytes)
    }
}

fn process_resource_usage() -> Option<(u64, u64)> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) } != 0 {
        return None;
    }
    let seconds = u64::try_from(usage.ru_utime.tv_sec)
        .ok()?
        .saturating_add(u64::try_from(usage.ru_stime.tv_sec).ok()?);
    let micros = u64::try_from(usage.ru_utime.tv_usec)
        .ok()?
        .saturating_add(u64::try_from(usage.ru_stime.tv_usec).ok()?);
    let cpu_micros = seconds.saturating_mul(1_000_000).saturating_add(micros);
    let max_rss = u64::try_from(usage.ru_maxrss).ok()?;
    #[cfg(target_os = "macos")]
    let resident_memory_bytes = max_rss;
    #[cfg(not(target_os = "macos"))]
    let resident_memory_bytes = max_rss.saturating_mul(1024);
    Some((cpu_micros, resident_memory_bytes))
}

type SharedMetricsBroker = Arc<Mutex<MetricsBroker>>;
type SharedProcessResourceSampler = Arc<Mutex<ProcessResourceSampler>>;
impl usagi_daemon::presentation::ipc::TerminalOwner for SharedTerminal {
    fn request(
        &mut self,
        connection: usagi_core::domain::id::ConnectionId,
        client: usagi_core::domain::id::ClientId,
        request_id: usagi_core::domain::id::RequestId,
        action: usagi_core::usecase::client::TerminalAction,
        payload: serde_json::Value,
        wire: SnapshotWire,
    ) -> Result<serde_json::Value, usagi_core::infrastructure::ipc::ProtocolError> {
        self.0
            .lock()
            .map_err(|_| {
                usagi_core::infrastructure::ipc::ProtocolError::new(
                    usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                    "terminal owner is unavailable",
                )
            })?
            .request(connection, client, request_id, action, payload, wire)
    }
    fn inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_launch::TerminalInventoryEntry> {
        self.0
            .lock()
            .map_or_else(|_| Vec::new(), |terminal| terminal.inventory(scope))
    }
    fn completed_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_visibility::CompletedTerminalEntry> {
        self.0.lock().map_or_else(
            |_| Vec::new(),
            |terminal| terminal.completed_inventory(scope),
        )
    }
    fn disconnect(&mut self, connection: usagi_core::domain::id::ConnectionId) {
        if let Ok(mut terminal) = self.0.lock() {
            terminal.disconnect(connection);
        }
    }
}

use super::bootstrap;
use super::launchd;

// IPC request routing remains in the composition adapter, and each argument is one
// independently resolved startup fact (endpoint, generation, data directory, fenced
// workspace, build, owner record, custody probe, shutdown); bundling them would only
// hide the composition wiring.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn spawn_ipc_server(
    listener: SecureUnixListener,
    generation: &usagi_core::infrastructure::ipc::DaemonGeneration,
    data_dir: &Path,
    workspace_root: &Path,
    build: &BuildIdentity,
    daemon_process: DaemonRecord,
    custody: FsCustodyProbe,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<SecureUnixListener>> {
    let owner = daemon_process.clone();
    let repo_root = workspace_root.to_path_buf();
    let daemon_generation = usagi_core::domain::id::DaemonGeneration::parse(&generation.0)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let runtime = open_session_runtime(
        repo_root.clone(),
        &data_dir.join("daemon"),
        daemon_generation,
    )?;
    let pr_inventory = Arc::new(Mutex::new(OutputPrProjector::new(PrInventoryStore::new(
        data_dir.join("daemon"),
    ))));
    // Deferred PR detection. The observers submit committed bytes here after
    // releasing the runtime lock, so no scan and no durable write happens inside
    // it (#555).
    let projection = Arc::new(PrProjectionQueue::new());
    let pipeline_metrics = Arc::new(TerminalPipelineMetrics::default());
    // One daemon-wide aggregate retention budget for exited terminal and Agent
    // finals (#526). Both owners reserve from it before spawning and commit
    // their finals into it, so short-lived runtimes cannot grow the daemon's
    // tombstones without bound.
    let retention = usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention::new();
    let (pty, observations) = DaemonPty::new(Arc::clone(&pipeline_metrics));
    let workspace_root = trusted_repository_root(&runtime)?;
    // The handshake fence compares a client's declared workspace against the
    // same trusted root the session runtime resolved, so a client working in
    // another workspace cannot be served this one's sessions (#548).
    let server = usagi_daemon::presentation::ipc::server_protocol(
        generation.clone(),
        generation.0.clone(),
        build.clone(),
        daemon_process,
        paths::wire_workspace_root(&workspace_root),
    );
    // One reader for the whole daemon: Agent adapters and the terminal profile
    // resolve the same configured environment and share its secret cache.
    let user_environment = Arc::new(UserEnvironment::new(data_dir.to_path_buf(), OpCli));
    let terminal = new_terminal_runtime(
        data_dir,
        daemon_generation,
        workspace_root,
        pty,
        Arc::clone(&runtime),
        Arc::clone(&user_environment),
        retention.clone(),
    )?;
    start_terminal_observer(Arc::clone(&terminal), observations, Arc::clone(&projection))?;
    let (agent_pty, agent_observations) =
        AgentPty::new(terminal_environment(), Arc::clone(&pipeline_metrics));
    let mcp_command = std::env::current_exe()?;
    let agent = open_agent_runtime(
        data_dir,
        daemon_generation,
        Arc::clone(&runtime),
        agent_pty,
        mcp_command,
        user_environment,
        retention.clone(),
    )?;
    let supervisor = Arc::new(Mutex::new(SupervisorRuntime::new(&data_dir.join("daemon"))));
    if let Ok(runtime) = supervisor.lock()
        && let Err(error) = runtime.tick_all(chrono::Utc::now(), &mut DeferredDecisionWaker)
    {
        ErrorLog::record(&format!(
            "supervisor startup reconciliation deferred: {error}"
        ));
    }
    start_agent_observer(
        Arc::clone(&agent),
        agent_observations,
        Arc::clone(&projection),
        Arc::clone(&supervisor),
    )?;
    start_pr_projection_worker(Arc::clone(&pr_inventory), Arc::clone(&projection))?;
    let decisions = Arc::new(UserDecisionStore::new(data_dir.join("daemon")));
    consume_user_decision_events(&decisions)
        .map_err(|error| std::io::Error::other(error.message))?;
    start_decision_maintenance(Arc::clone(&decisions), Arc::clone(&shutdown))?;
    start_pr_refresh_worker(Arc::clone(&pr_inventory), Arc::clone(&shutdown))?;
    let teardown = start_session_teardown_worker(Arc::clone(&runtime), Arc::clone(&shutdown))?;
    start_retention_gc_worker(
        Arc::clone(&terminal),
        Arc::clone(&agent),
        Arc::clone(&shutdown),
    )?;
    start_custody_worker(
        custody,
        owner,
        data_dir.to_path_buf(),
        Arc::clone(&shutdown),
    )?;
    start_ipc_accept_loop(
        listener,
        server,
        runtime,
        teardown,
        terminal,
        agent,
        retention,
        pr_inventory,
        projection,
        decisions,
        Arc::new(Mutex::new(MetricsBroker::default())),
        Arc::new(Mutex::new(ProcessResourceSampler { previous: None })),
        pipeline_metrics,
        supervisor,
        shutdown,
    )
}

fn bind_ipc_listener(
    data_dir: &Path,
) -> std::io::Result<(
    SecureUnixListener,
    usagi_core::infrastructure::ipc::DaemonGeneration,
)> {
    let generation = usagi_core::infrastructure::ipc::DaemonGeneration(
        usagi_core::domain::id::DaemonGeneration::new()
            .as_str()
            .clone(),
    );
    // Bound, not published: the endpoint has to be *accepting* before the
    // registry may name it, and it must not be *discoverable* until it does.
    // `serve` publishes `current` afterwards, through the generation authority.
    let listener = SecureUnixListener::bind_private(data_dir, generation.clone())?;
    Ok((listener, generation))
}

/// Starts the only production PR refresh worker. Remote calls happen outside
/// the shared inventory lock, so snapshot and terminal paths continue to make
/// progress while `gh` is slow.
fn start_pr_refresh_worker(
    pr_inventory: SharedPrInventory,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<()> {
    spawn_pr_refresh_worker(
        pr_inventory,
        shutdown,
        GhProcess,
        ProductionRefreshClock {
            started: Instant::now(),
        },
        PR_REFRESH_TICK,
    )
    .map(|_| ())
}

fn spawn_pr_refresh_worker<R, C>(
    pr_inventory: SharedPrInventory,
    shutdown: Arc<ShutdownRequest>,
    runner: R,
    clock: C,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    R: GhProcessPort + Send + 'static,
    C: RefreshClock + Send + 'static,
{
    std::thread::Builder::new()
        .name("usagi-pr-refresh".to_string())
        .spawn(move || {
            let mut worker =
                RefreshWorker::new(runner, clock, PR_REFRESH_PER_TICK, PR_REFRESH_FRESHNESS_MS);
            if let Ok(mut projector) = pr_inventory.lock()
                && worker.rebuild(&mut projector).is_err()
            {
                ErrorLog::record("PR refresh schedule rebuild failed");
            }
            while !shutdown.is_requested() {
                let due = pr_inventory
                    .lock()
                    .ok()
                    .and_then(|mut projector| worker.claim_due(&mut projector).ok())
                    .unwrap_or_default();
                for identity in due {
                    if shutdown.is_requested() {
                        break;
                    }
                    let result = worker.fetch(&identity);
                    if shutdown.is_requested() {
                        break;
                    }
                    if let Ok(mut projector) = pr_inventory.lock()
                        && worker.complete(&mut projector, &identity, result).is_err()
                    {
                        ErrorLog::record("PR refresh snapshot publish failed");
                    }
                }
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
        })
}

/// Starts the only production session teardown worker and returns the signal an
/// admitted removal uses to wake it.
///
/// The worker is what makes `session remove` answer inside a client's attempt
/// deadline: the IPC handler only marks the session `Deleting`, and this thread
/// owns the unbounded `git worktree remove` plus `remove_dir_all` afterwards.
/// Its work list is derived from durable state, so it also resumes a teardown
/// that a previous daemon was interrupted in.
fn start_session_teardown_worker(
    sessions: SharedSessionRuntime,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<Arc<TeardownSignal>> {
    let signal = Arc::new(TeardownSignal::new());
    spawn_session_teardown_worker(
        SharedSessionTeardown::new(sessions),
        WorktreeTeardown::new(SystemGit),
        Arc::clone(&signal),
        shutdown,
        SESSION_TEARDOWN_TICK,
    )?;
    Ok(signal)
}

fn spawn_session_teardown_worker<J, E>(
    journal: J,
    effect: E,
    signal: Arc<TeardownSignal>,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    J: TeardownJournal + Send + 'static,
    E: TeardownEffect + Send + 'static,
{
    std::thread::Builder::new()
        .name("usagi-session-teardown".to_string())
        .spawn(move || {
            let cancel = Arc::clone(&shutdown);
            let cancelled = move || cancel.is_requested();
            while !shutdown.is_requested() {
                for report in drain_pending_teardowns(&journal, &effect, &cancelled) {
                    if let Some(error) = report.effect_error {
                        ErrorLog::record(&format!(
                            "session teardown failed for \"{}\": {error}",
                            report.name
                        ));
                    }
                    if let Some(error) = report.finalize_error {
                        ErrorLog::record(&format!(
                            "session teardown outcome could not be recorded for \"{}\": {error}",
                            report.name
                        ));
                    }
                }
                if shutdown.is_requested() {
                    break;
                }
                // An admitted removal wakes this immediately; the tick only
                // re-derives the pending set so a teardown whose finalization
                // failed is retried without another request.
                signal.wait(tick);
            }
        })
}

/// Starts the only production custody supervisor. A daemon is deliberately
/// detached from its launcher's process group, so nothing else reaps it when the
/// launcher dies abnormally; this worker makes the daemon reap itself as soon as
/// it stops being the authority for its data directory (see
/// [`usagi_daemon::usecase::custody`]).
fn start_custody_worker(
    probe: FsCustodyProbe,
    owner: DaemonRecord,
    data_dir: PathBuf,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<()> {
    spawn_custody_worker(probe, owner, data_dir, shutdown, CUSTODY_TICK).map(|_| ())
}

fn spawn_custody_worker<P>(
    probe: P,
    owner: DaemonRecord,
    data_dir: PathBuf,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    P: CustodyProbe + Send + 'static,
{
    std::thread::Builder::new()
        .name("usagi-daemon-custody".to_string())
        .spawn(move || {
            while !shutdown.is_requested() {
                match usagi_daemon::usecase::custody::evaluate(&probe, &owner) {
                    Ok(Custody::Lost(loss)) => {
                        // The error log lives inside the data directory. Record
                        // the reason only while that directory still exists: a
                        // daemon exiting because its tree was deleted must not
                        // re-create the tree it is releasing.
                        if data_dir.exists() {
                            ErrorLog::record(&format!(
                                "daemon custody lost ({}); shutting down",
                                loss.reason()
                            ));
                        }
                        // Request the same graceful shutdown a SIGTERM does, so
                        // endpoint retirement and record clearing stay on one path.
                        shutdown.request();
                        return;
                    }
                    // An undecidable observation is not a loss: keep serving and
                    // re-evaluate on the next tick.
                    Ok(Custody::Held) | Err(_) => {}
                }
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
        })
}

/// How often the daemon ages exited terminal / Agent finals out of the aggregate
/// retention budget when nothing else drives collection.
///
/// Launch and exit already collect on the spot, so this only covers an idle
/// daemon, where the only things still moving are the age budget and the minimum
/// visibility TTL. Both are measured in minutes, so a 30 s tick is far finer than
/// the state it observes.
const RETENTION_GC_TICK: Duration = Duration::from_secs(30);

/// Starts the only production retention collector. Launch and exit already
/// collect on the spot; this worker covers an idle daemon, where the age budget
/// and the minimum visibility TTL are the only things still moving.
fn start_retention_gc_worker(
    terminal: SharedTerminalRuntime,
    agent: SharedAgentRuntime,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<()> {
    spawn_retention_gc_worker(
        move || {
            if let Ok(mut terminal) = terminal.lock() {
                terminal.collect_retention_garbage();
            }
            if let Ok(mut agent) = agent.lock() {
                agent.collect_retention_garbage();
            }
        },
        shutdown,
        RETENTION_GC_TICK,
    )
    .map(|_| ())
}

/// The worker loop, with the collection step injected so a test can drive it
/// without a daemon, a PTY, or a store.
fn spawn_retention_gc_worker<C>(
    mut collect: C,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    C: FnMut() + Send + 'static,
{
    std::thread::Builder::new()
        .name("usagi-retention-gc".to_string())
        .spawn(move || {
            while !shutdown.is_requested() {
                collect();
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
        })
}

/// Real filesystem observations behind [`usagi_daemon::usecase::custody`].
///
/// `locked` is observed through the descriptor the single-instance lock holds,
/// so replacing the pathname afterwards cannot forge the identity it is
/// compared against.
struct FsCustodyProbe {
    locked: Option<NodeIdentity>,
    lock_path: PathBuf,
    record: FsRecordFile,
}

impl CustodyProbe for FsCustodyProbe {
    fn locked_inode(&self) -> std::io::Result<NodeIdentity> {
        self.locked.ok_or_else(|| {
            std::io::Error::other("daemon instance lock identity was never observed")
        })
    }

    fn lock_pathname(&self) -> std::io::Result<Option<NodeIdentity>> {
        match std::fs::symlink_metadata(&self.lock_path) {
            Ok(metadata) => Ok(Some(node_identity(&metadata))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn owner_record(&self) -> std::io::Result<Option<DaemonRecord>> {
        // Read without taking `record.lock`: records commit by rename, so a
        // reader never observes a torn file, and locking would re-create a
        // directory this daemon may already have lost.
        self.record
            .read_unlocked()?
            .map(|contents| {
                serde_json::from_str(&contents)
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            })
            .transpose()
    }
}

fn node_identity(metadata: &std::fs::Metadata) -> NodeIdentity {
    use std::os::unix::fs::MetadataExt;

    NodeIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

/// Keeps decision deadlines progressing even when no subsequent MCP/TUI
/// request arrives. Every action is idempotent, so a daemon restart simply
/// resumes from the JSON store.
fn start_decision_maintenance(
    decisions: Arc<UserDecisionStore>,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<()> {
    spawn_decision_maintenance(decisions, shutdown, DECISION_MAINTENANCE_TICK).map(|_| ())
}

/// The loop, with the tick injected so a test can drive it without waiting out
/// the production cadence.
fn spawn_decision_maintenance(
    decisions: Arc<UserDecisionStore>,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-decision-maintenance".to_string())
        .spawn(move || {
            while !shutdown.is_requested() {
                let _ = decisions.expire_due(chrono::Utc::now());
                let _ = consume_user_decision_events(&decisions);
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
        })
}

fn open_agent_runtime(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    sessions: SharedSessionRuntime,
    pty: AgentPty,
    mcp_command: PathBuf,
    environment: Arc<SharedUserEnvironment>,
    retention: usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention,
) -> std::io::Result<SharedAgentRuntime> {
    let mut store = FileRuntimeStore(data_dir.join("daemon").join("agents.json"));
    let snapshot = store.reconcile_after_restart()?;
    let mut registry = AdapterRegistry::new();
    let readiness: Arc<dyn AgentReadinessProbe> = Arc::new(SystemAgentReadiness);
    // Agent MCP children receive the mode-neutral base. They apply the same
    // selected runtime mode themselves, so both `dev/` and `local/` reach the
    // daemon's already-selected directory without adding that child twice.
    let data_home = data_dir.parent().unwrap_or(data_dir).to_path_buf();
    // Duplicate registration cannot happen for the two literal profiles; a
    // failure here would only drop an adapter, so the launch would surface a
    // safe unknown-profile error rather than crash the daemon.
    let _ = registry.register_supported(
        CodexAdapter::new(RootCodexProvisioner {
            sessions: Arc::clone(&sessions),
            readiness: Arc::clone(&readiness),
            mcp_command: mcp_command.clone(),
            data_home: data_home.clone(),
            program: "codex",
            environment: Some(Arc::clone(&environment)),
        }),
        CodexAdapter::sakana(RootCodexProvisioner {
            sessions: Arc::clone(&sessions),
            readiness: Arc::clone(&readiness),
            mcp_command: mcp_command.clone(),
            data_home: data_home.clone(),
            program: "codex-fugu",
            environment: Some(Arc::clone(&environment)),
        }),
        ClaudeAdapter::new(RootClaudeProvisioner {
            sessions,
            readiness,
            mcp_command,
            data_home,
            environment: Some(environment),
            // E2E テスト専用 seam。release ビルドでは `cfg!(debug_assertions)` が false になるため、
            // 配布バイナリは常に拘束された Claude だけを起動する。
            sandbox_passthrough: claude_sandbox::passthrough_requested(
                cfg!(debug_assertions),
                std::env::var(claude_sandbox::PASSTHROUGH_ENVIRONMENT_VARIABLE)
                    .ok()
                    .as_deref(),
            ),
        }),
    );
    let runtime = AgentRuntime::hydrate_with_retention(
        generation,
        registry,
        store,
        DiscardJournal,
        pty,
        AgentProfileId::new("codex").expect("literal profile id is canonical"),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(data_dir.join("daemon")),
        usagi_core::infrastructure::runtime_model::PathExecutableLocator,
        snapshot,
        retention,
    )
    .map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid agent runtime snapshot: {error:?}"),
        )
    })?;
    Ok(Arc::new(Mutex::new(runtime)))
}

fn start_agent_observer(
    agent: SharedAgentRuntime,
    observations: Receiver<AgentPtyObservation>,
    projection: Arc<PrProjectionQueue>,
    supervisor: SharedSupervisorRuntime,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("usagi-agent-observer".to_string())
        .spawn(move || {
            while let Ok(observation) = observations.recv() {
                match observation {
                    AgentPtyObservation::Output(reference, bytes) => {
                        // The runtime lock covers journaling this chunk and
                        // nothing else. PR detection is submitted afterwards, so
                        // the lock is never held for a scan or for durable IO.
                        let committed = {
                            let Ok(mut agent) = agent.lock() else {
                                break;
                            };
                            agent.output(&reference, bytes.clone()).is_ok()
                        };
                        if committed {
                            projection.submit_output(
                                reference.terminal_id,
                                reference.session_id,
                                bytes,
                            );
                        }
                    }
                    AgentPtyObservation::Exited(reference, status) => {
                        {
                            let Ok(mut agent) = agent.lock() else {
                                break;
                            };
                            let _ = agent.exit(&reference, status);
                        }
                        // A candidate the output never terminated is only
                        // creditable once nothing more can arrive for it.
                        projection.submit_closed(reference.terminal_id, reference.session_id);
                        if let Ok(runtime) = supervisor.lock()
                            && let Err(error) =
                                runtime.tick_all(chrono::Utc::now(), &mut DeferredDecisionWaker)
                        {
                            ErrorLog::record(&format!(
                                "supervisor completion reconciliation deferred: {error}"
                            ));
                        }
                    }
                }
            }
        })
        .map(|_| ())
}

/// Starts the only production PR projection worker.
///
/// It owns every scan and every durable inventory write that PTY output causes.
/// The queue's `recv` parks on a condvar and returns `None` once the queue is
/// closed and drained, so this thread has no timer and no polling.
fn start_pr_projection_worker(
    pr_inventory: SharedPrInventory,
    projection: Arc<PrProjectionQueue>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("usagi-pr-projection".to_string())
        .spawn(move || {
            while let Some(item) = projection.recv() {
                let Ok(mut projector) = pr_inventory.lock() else {
                    break;
                };
                match item {
                    PrProjection::Output {
                        terminal,
                        session,
                        bytes,
                    } => {
                        let _ = projector.observe_committed(terminal, session, &bytes);
                    }
                    PrProjection::Gap { terminal } => projector.mark_gap(terminal),
                    PrProjection::Closed { terminal, session } => {
                        let _ = projector.release_terminal(terminal, session);
                    }
                }
            }
        })
        .map(|_| ())
}

fn open_session_runtime(
    repo_root: PathBuf,
    state_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
) -> std::io::Result<SharedSessionRuntime> {
    SessionRuntime::open(repo_root, state_dir, generation, SystemGit)
        .map(|runtime| Arc::new(Mutex::new(runtime)))
        .map_err(|error| std::io::Error::other(error.safe_message()))
}

/// Reads the root selected by the durable session store, rather than the
/// daemon process's startup directory. This keeps terminal profile resolution
/// aligned with restored managed-session state after a restart.
fn trusted_repository_root(sessions: &SharedSessionRuntime) -> std::io::Result<PathBuf> {
    sessions
        .lock()
        .map(|sessions| sessions.repository_root().to_path_buf())
        .map_err(|_| std::io::Error::other("session runtime is unavailable"))
}

fn new_terminal_runtime(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    repo_root: PathBuf,
    pty: DaemonPty,
    sessions: SharedSessionRuntime,
    environment: Arc<SharedUserEnvironment>,
    retention: usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention,
) -> std::io::Result<SharedTerminalRuntime> {
    let mut store = FileTerminalStore(data_dir.join("daemon").join("terminals.json"));
    let (snapshot, interrupted) = store.load_reconciled()?;
    if interrupted != 0 {
        ErrorLog::record(&format!(
            "daemon startup reconciled {interrupted} generic terminal(s) as identity_unknown"
        ));
    }
    let runtime = GenericTerminalRuntime::from_snapshot_with_retention(
        generation,
        TrustedLoginShell {
            profile: LoginShellProfile::new(terminal_environment(), repo_root.clone()),
            environment: Some(environment),
            workspace_root: repo_root,
        },
        store,
        pty,
        SharedTerminalScopeResolver(sessions),
        snapshot,
        retention,
    )
    .map_err(|_| std::io::Error::other("invalid generic terminal snapshot"))?;
    Ok(Arc::new(Mutex::new(runtime)))
}

fn start_terminal_observer<S, Q>(
    terminal: Arc<Mutex<GenericTerminalRuntime<TrustedLoginShell, S, DaemonPty, Q>>>,
    observations: Receiver<PtyObservation>,
    projection: Arc<PrProjectionQueue>,
) -> std::io::Result<()>
where
    S: TerminalStore + Send + 'static,
    Q: TerminalScopeResolver + Send + 'static,
{
    std::thread::Builder::new()
        .name("usagi-terminal-observer".to_string())
        .spawn(move || {
            while let Ok(observation) = observations.recv() {
                match observation {
                    PtyObservation::Output(reference, bytes) => {
                        // As in the Agent observer: the lock covers journaling
                        // only, and PR detection happens after it is released.
                        let committed = {
                            let Ok(mut terminal) = terminal.lock() else {
                                break;
                            };
                            terminal.output(&reference, bytes.clone()).is_ok()
                        };
                        if committed {
                            projection.submit_output(
                                reference.terminal_id,
                                reference.session_id,
                                bytes,
                            );
                        }
                    }
                    PtyObservation::Exited(reference, status) => {
                        {
                            let Ok(mut terminal) = terminal.lock() else {
                                break;
                            };
                            let _ = terminal.exit(&reference, status);
                        }
                        projection.submit_closed(reference.terminal_id, reference.session_id);
                    }
                }
            }
        })
        .map(|_| ())
}

#[allow(clippy::too_many_arguments)] // Composition owns the independently injected daemon services.
fn start_ipc_accept_loop(
    listener: SecureUnixListener,
    server: usagi_core::infrastructure::ipc::ServerProtocol,
    runtime: SharedSessionRuntime,
    teardown: Arc<TeardownSignal>,
    terminal: SharedTerminalRuntime,
    agent: SharedAgentRuntime,
    retention: usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention,
    pr_inventory: SharedPrInventory,
    projection: Arc<PrProjectionQueue>,
    decisions: Arc<UserDecisionStore>,
    metrics: SharedMetricsBroker,
    process_metrics: SharedProcessResourceSampler,
    pipeline_metrics: Arc<TerminalPipelineMetrics>,
    supervisor: SharedSupervisorRuntime,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<SecureUnixListener>> {
    std::thread::Builder::new()
        .name("usagi-ipc".to_string())
        .spawn(move || {
            let _exit = ShutdownOnIpcWorkerExit {
                shutdown: Arc::clone(&shutdown),
            };
            // Closing the projection queue is what retires its worker: `recv`
            // returns `None` once the queue is closed and drained, so the thread
            // needs no shutdown flag of its own and never polls one.
            let _projection = ClosePrProjectionOnExit { projection };
            // One workspace-global visibility authority for exited terminal
            // tombstones (#525), shared by every client connection so multiple
            // TUIs converge on the same Observed / Dismissed state.
            let visibility =
                usagi_daemon::usecase::terminal_visibility_ipc::SharedTerminalVisibility::new();
            // Waiting on the listening descriptor replaces a non-blocking accept
            // that retried every 10 ms. The wake pipe is what lets one wait cover
            // both a new connection and a shutdown request.
            let wake = match ShutdownPipe::mirroring(&shutdown) {
                Ok(wake) => wake,
                Err(error) => {
                    ErrorLog::record(&format!("daemon accept wait unavailable: {error}"));
                    return listener;
                }
            };
            while !shutdown.is_requested() {
                if !wake.wait_for_listener(listener.readiness_fd()) {
                    break;
                }
                // One readiness report can cover several queued connections, and
                // it is not repeated for the ones left behind. Accepting only the
                // first would park this loop while a client waits — which a
                // reconnecting terminal sees as an undelivered keystroke — so every
                // queued connection is drained before waiting again.
                while !shutdown.is_requested() {
                match listener.accept() {
                    Ok(stream) => {
                        if shutdown.is_requested() {
                            break;
                        }
                        let server = server.clone();
                        let session = Arc::clone(&runtime);
                        let scope_sessions = Arc::clone(&runtime);
                        let teardown = Arc::clone(&teardown);
                        let terminal = Arc::clone(&terminal);
                        let visibility = visibility.clone();
                        let retention = retention.clone();
                        let agent_owner = Arc::clone(&agent);
                        let agent_launch = Arc::clone(&agent);
                        let pr_inventory = Arc::clone(&pr_inventory);
                        let decisions = Arc::clone(&decisions);
                        let metrics = Arc::clone(&metrics);
                        let process_metrics = Arc::clone(&process_metrics);
                        let pipeline_metrics = Arc::clone(&pipeline_metrics);
                        let supervisor = Arc::clone(&supervisor);
                        let _ = std::thread::Builder::new()
                            .name("usagi-ipc-client".to_string())
                            .spawn(move || {
                                let _ = stream.set_nonblocking(false);
                                let Ok(mut writer) = stream.try_clone() else {
                                    return;
                                };
                                let mut reader = stream;
                                let mut owner =
                                    SharedTerminalOwner::with_visibility_and_retention(
                                        SharedAgent(agent_owner),
                                        SharedTerminal(terminal),
                                        visibility,
                                        retention,
                                    );
                                let mut metrics_observer = None;
                                let result = usagi_daemon::presentation::ipc::handle_connection_with_terminal_and(
                                    &mut reader,
                                    &mut writer,
                                    &server,
                                    &mut owner,
                                    &mut |request_id, body, hello, connection, _client| match body
                                        .get("kind")
                                        .and_then(serde_json::Value::as_str)
                                    {
                                        Some("session") => dispatch_session(&session, &teardown, &agent_launch, &pr_inventory, request_id, &body, hello),
                                        Some("agent" | "agent_inventory" | "resume_agent") => dispatch_agent(&agent_launch, &scope_sessions, request_id, &body, hello),
                                        Some("codex_session_capture") => dispatch_codex_session_capture(&agent_launch, request_id, &body, hello),
                                        Some("agent_phase_report") => dispatch_agent_phase_report(&agent_launch, request_id, &body, hello),
                                        Some("dispatch") => dispatch_dispatch(&agent_launch, &scope_sessions, request_id, &body, hello),
                                        Some("metrics") => dispatch_metrics(&metrics, &process_metrics, &pipeline_metrics, &mut metrics_observer, request_id, &body, hello),
                                        Some("pr") => dispatch_pr_snapshot(&pr_inventory, request_id, &body, hello),
                                        Some("dispatch_tool") => dispatch_dispatch_tool(&agent_launch, &scope_sessions, &decisions, request_id, &body, hello),
                                        Some("supervisor_tool") => dispatch_supervisor_tool(&supervisor, connection, request_id, &body, hello),
                                        Some("user_decision") => dispatch_user_decision(&agent_launch, &scope_sessions, &decisions, request_id, &body, hello),
                                        _ => usagi_daemon::presentation::ipc::dispatch(request_id, body, hello),
                                    },
                                );
                                if let Some(observer) = metrics_observer
                                    && let Ok(mut broker) = metrics.lock()
                                {
                                    broker.unsubscribe(observer.subscription());
                                }
                                let _ = result;
                            });
                    }
                    // Drained: nothing more is queued, so wait for readiness.
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    // A peer that failed the credential check was still accepted
                    // and dropped, so draining continues. An error that leaves the
                    // connection queued (descriptor exhaustion) would otherwise
                    // spin, so that path — and only that path, never the idle one —
                    // backs off before trying again.
                    Err(_) => std::thread::sleep(ACCEPT_ERROR_BACKOFF),
                }
                }
            }
            listener
        })
}

/// A pipe whose readable end lets `poll(2)` wait for a shutdown request
/// alongside the listening socket.
///
/// A condvar cannot be mixed into a descriptor wait, so the request is mirrored
/// onto a descriptor. The mirroring thread parks on the condvar, which means an
/// idle daemon still performs no timed wakeups.
struct ShutdownPipe {
    read: std::os::fd::RawFd,
    write: std::os::fd::RawFd,
}

impl ShutdownPipe {
    /// Creates the pipe and starts mirroring `shutdown` onto it.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
    fn mirroring(shutdown: &Arc<ShutdownRequest>) -> std::io::Result<Self> {
        let mut ends = [0_i32; 2];
        // SAFETY: `ends` is a two-element array, exactly what pipe(2) writes.
        if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let pipe = Self {
            read: ends[0],
            write: ends[1],
        };
        // The daemon execs children (PTYs, the PR provider). Neither end may be
        // inherited: this daemon guards every other descriptor it owns the same
        // way, and a shutdown wake belongs to this process only. macOS has no
        // `pipe2`, so close-on-exec is set right after the pipe exists.
        for end in ends {
            // SAFETY: `end` is an owned descriptor from the pipe above.
            if unsafe { libc::fcntl(end, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        let write = pipe.write;
        let requested = Arc::clone(shutdown);
        std::thread::Builder::new()
            .name("usagi-shutdown-wake".to_string())
            .spawn(move || {
                requested.wait_until_requested();
                // One byte is enough: the reader only needs readiness, and the
                // descriptor is never reused for anything else.
                // SAFETY: writing one byte from a local buffer to an owned pipe.
                unsafe { libc::write(write, [1_u8].as_ptr().cast(), 1) };
            })?;
        Ok(pipe)
    }

    /// Waits until the listener has a connection or shutdown was requested.
    /// Returns whether the listener is the one that became ready.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
    fn wait_for_listener(&self, listener: std::os::fd::RawFd) -> bool {
        let mut fds = [
            libc::pollfd {
                fd: listener,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.read,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // A negative timeout blocks indefinitely: there is nothing to poll for on
        // a timer, so an idle daemon performs no wakeups here at all.
        // SAFETY: both descriptors are owned and live for this call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        // Only readability means "accept now". An interrupted or failed wait is
        // also reported ready so the caller re-checks the request flag and retries
        // rather than treating an EINTR as a shutdown. Error bits on the listener
        // deliberately fall through as "not ready": the caller then leaves the
        // loop and the exit guard shuts the daemon down, instead of spinning on a
        // descriptor that `poll` reports immediately and `accept` cannot use.
        ready < 0 || fds[0].revents & libc::POLLIN != 0
    }
}

impl Drop for ShutdownPipe {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
    fn drop(&mut self) {
        // SAFETY: both ends are owned by this value.
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

/// Retires the PR projection worker whenever the accept worker exits, including
/// on an unwind, so no thread is left parked on a queue nothing will feed.
struct ClosePrProjectionOnExit {
    projection: Arc<PrProjectionQueue>,
}

impl Drop for ClosePrProjectionOnExit {
    fn drop(&mut self) {
        self.projection.close();
    }
}

/// Wakes the lifecycle owner whenever the accept worker unwinds or exits.
/// Normal signal-driven shutdown has already set the same flag, so the guard
/// is idempotent on the expected return path.
struct ShutdownOnIpcWorkerExit {
    shutdown: Arc<ShutdownRequest>,
}

impl Drop for ShutdownOnIpcWorkerExit {
    fn drop(&mut self) {
        self.shutdown.request();
    }
}

fn dispatch_dispatch_tool(
    agent: &SharedAgentRuntime,
    sessions: &SharedSessionRuntime,
    decisions: &UserDecisionStore,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    let action = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::DispatchTool { action, .. } => Some(action),
            _ => None,
        });
    if action.is_some_and(|action| {
        matches!(
            action,
            DispatchToolAction::Dispatch
                | DispatchToolAction::SessionGet
                | DispatchToolAction::AgentList
                | DispatchToolAction::AgentGet
                | DispatchToolAction::AgentComplete
                | DispatchToolAction::AgentFail
                | DispatchToolAction::AgentInbox
        )
    }) {
        dispatch_agent_tool(agent, sessions, request_id, body, hello)
    } else {
        dispatch_user_decision(agent, sessions, decisions, request_id, body, hello)
    }
}

#[allow(clippy::too_many_lines)] // One handler keeps authentication and durable routing atomic.
fn dispatch_agent_tool(
    agent: &SharedAgentRuntime,
    sessions: &SharedSessionRuntime,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use chrono::{DateTime, Utc};
    use usagi_core::domain::agent::{
        AgentProfileId, AgentStatus, InboxKind, ModelSelector, StructuredResult,
    };
    use usagi_core::domain::id::{AgentId, OperationId};
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};
    use usagi_core::usecase::client::{DispatchAgentIntent, DispatchIntent};

    #[derive(Deserialize)]
    struct SessionPayload {
        name: String,
    }
    #[derive(Deserialize)]
    struct DispatchPayload {
        session: SessionPayload,
        agent: serde_json::Value,
        prompt: String,
    }
    #[derive(Deserialize)]
    struct AgentIdPayload {
        agent_id: AgentId,
    }
    #[derive(Deserialize)]
    struct ReportPayload {
        summary: String,
        #[serde(default)]
        result: Option<StructuredResult>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        run_id: Option<OperationId>,
    }
    #[derive(Deserialize)]
    struct InboxPayload {
        #[serde(default)]
        since: Option<DateTime<Utc>>,
        #[serde(default)]
        unread_only: bool,
    }

    let parsed = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::DispatchTool {
                action,
                operation_id,
                payload,
                caller_context,
            } => Some((action, operation_id, payload, caller_context)),
            _ => None,
        });
    let Some((action, operation_id, payload, caller_context)) = parsed else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    let response = (|| -> Result<(ResponseOutcome, serde_json::Value), ProtocolError> {
        let credential = caller_context
            .as_ref()
            .filter(|context| !context.credential.is_empty())
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "agent caller provenance is unknown",
                )
            })?;
        let snapshot = sessions
            .lock()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
            })?
            .snapshot()
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::Unavailable,
                    "daemon could not read managed sessions",
                )
            })?;
        let workspace = snapshot
            .get("workspace_id")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::Unavailable, "workspace identity is unavailable")
            })?;
        let mut runtime = agent.lock().map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
        })?;
        let caller = runtime
            .mcp_dispatch_caller(&credential.credential)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "agent caller provenance is unknown",
                )
            })?;
        let store = runtime.dispatch_store();
        let task_for = |agent_id: AgentId| -> Result<serde_json::Value, ProtocolError> {
            let mut runs = store
                .runs()
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable")
                })?
                .into_iter()
                .filter(|run| run.agent_id == agent_id)
                .collect::<Vec<_>>();
            runs.sort_by_key(|run| run.started_at);
            Ok(runs
                .last()
                .map_or(serde_json::Value::Null, |run| serde_json::json!(run)))
        };
        match action {
            DispatchToolAction::Dispatch => {
                let input = serde_json::from_value::<DispatchPayload>(payload).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "invalid session_dispatch payload",
                    )
                })?;
                let selected = if let Some(id) = input.agent.get("id") {
                    if input.agent.as_object().is_none_or(|value| value.len() != 1) {
                        return Err(ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "agent selector must use exactly one branch",
                        ));
                    }
                    DispatchAgentIntent::Existing {
                        agent_id: serde_json::from_value(id.clone()).map_err(|_| {
                            ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent id")
                        })?,
                    }
                } else {
                    let object = input
                        .agent
                        .as_object()
                        .filter(|value| value.len() == 2)
                        .ok_or_else(|| {
                            ProtocolError::new(
                                ErrorCode::InvalidArgument,
                                "agent selector must use exactly one branch",
                            )
                        })?;
                    let runtime = object
                        .get("runtime")
                        .cloned()
                        .and_then(|value| serde_json::from_value::<AgentProfileId>(value).ok())
                        .ok_or_else(|| {
                            ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent runtime")
                        })?;
                    let model = object
                        .get("model")
                        .cloned()
                        .and_then(|value| serde_json::from_value::<ModelSelector>(value).ok())
                        .ok_or_else(|| {
                            ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent model")
                        })?;
                    DispatchAgentIntent::New { runtime, model }
                };
                let session_name = input.session.name;
                let session_id = if let Some(id) = session_id_by_name(&snapshot, &session_name) {
                    id
                } else {
                    drop(runtime);
                    let created = sessions
                        .lock()
                        .map_err(|_| {
                            ProtocolError::new(
                                ErrorCode::Unavailable,
                                "session runtime is unavailable",
                            )
                        })?
                        .handle(
                            usagi_core::usecase::client::SessionAction::Create,
                            &operation_id,
                            &serde_json::json!({"name": session_name}),
                        )
                        .map_err(|error| {
                            ProtocolError::new(ErrorCode::InvalidArgument, error.safe_message())
                        })?;
                    let id = session_id_by_name(&created.body, &session_name).ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::Unavailable,
                            "created session is not available",
                        )
                    })?;
                    runtime = agent.lock().map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                    })?;
                    id
                };
                let scope = SharedScopeResolver(Arc::clone(sessions));
                let admission = runtime.dispatch(
                    &operation_id,
                    &DispatchIntent {
                        workspace,
                        session_name: session_name.clone(),
                        caller,
                        agent: selected,
                        prompt: input.prompt,
                    },
                    session_id,
                    &scope,
                )?;
                let run_id = OperationId::parse(&admission.operation_id)
                    .map_err(|_| ProtocolError::new(ErrorCode::Internal, "invalid admitted run"))?;
                let run = runtime
                    .dispatch_store()
                    .runs()
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable")
                    })?
                    .into_iter()
                    .find(|run| run.run_id == run_id)
                    .ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::Unavailable,
                            "admitted dispatch is unavailable",
                        )
                    })?;
                Ok((
                    ResponseOutcome::Accepted {
                        operation_id: usagi_core::infrastructure::ipc::OperationId(
                            admission.operation_id.clone(),
                        ),
                        operation_revision: admission.revision,
                    },
                    serde_json::json!({"run_id": admission.operation_id, "session": session_name, "agent_id": run.agent_id, "terminal": admission.terminal, "completed": admission.completed}),
                ))
            }
            DispatchToolAction::SessionGet => {
                let input = serde_json::from_value::<SessionPayload>(payload).map_err(|_| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "invalid session_get payload")
                })?;
                let session_id = session_id_by_name(&snapshot, &input.name).ok_or_else(|| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "session was not found")
                })?;
                let agents = store.agents().map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable"))?.into_iter().filter(|item| item.session_id == Some(session_id)).map(|item| Ok(serde_json::json!({"agent_id": item.agent_id, "runtime": item.runtime, "model": item.model, "status": item.status, "task": task_for(item.agent_id)?}))).collect::<Result<Vec<_>, ProtocolError>>()?;
                Ok((
                    ResponseOutcome::Ok,
                    serde_json::json!({"session": input.name, "agents": agents}),
                ))
            }
            DispatchToolAction::AgentList => {
                let session = payload
                    .get("session")
                    .and_then(serde_json::Value::as_str)
                    .map(|name| {
                        session_id_by_name(&snapshot, name).ok_or_else(|| {
                            ProtocolError::new(ErrorCode::InvalidArgument, "session was not found")
                        })
                    })
                    .transpose()?;
                let status = payload
                    .get("status")
                    .cloned()
                    .map(serde_json::from_value::<AgentStatus>)
                    .transpose()
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent status")
                    })?;
                let agents = store.agents().map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable"))?.into_iter().filter(|item| session.is_none_or(|id| item.session_id == Some(id)) && status.is_none_or(|value| item.status == value)).map(|item| Ok(serde_json::json!({"agent_id": item.agent_id, "session_id": item.session_id, "runtime": item.runtime, "model": item.model, "status": item.status, "task": task_for(item.agent_id)?}))).collect::<Result<Vec<_>, ProtocolError>>()?;
                Ok((ResponseOutcome::Ok, serde_json::json!({"agents": agents})))
            }
            DispatchToolAction::AgentGet => {
                let input = serde_json::from_value::<AgentIdPayload>(payload).map_err(|_| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent_get payload")
                })?;
                let item = store
                    .agent(input.agent_id)
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable")
                    })?
                    .ok_or_else(|| {
                        ProtocolError::new(ErrorCode::InvalidArgument, "agent was not found")
                    })?;
                let runs = store
                    .runs()
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "dispatch state is unavailable")
                    })?
                    .into_iter()
                    .filter(|run| run.agent_id == item.agent_id)
                    .collect::<Vec<_>>();
                Ok((
                    ResponseOutcome::Ok,
                    serde_json::json!({"agent": item, "runs": runs}),
                ))
            }
            DispatchToolAction::AgentComplete | DispatchToolAction::AgentFail => {
                let input = serde_json::from_value::<ReportPayload>(payload).map_err(|_| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent report payload")
                })?;
                if input.summary.trim().is_empty() {
                    return Err(ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "report summary must not be empty",
                    ));
                }
                let kind = if action == DispatchToolAction::AgentComplete {
                    InboxKind::Completed
                } else {
                    InboxKind::Failed
                };
                let summary = input
                    .error
                    .filter(|_| kind == InboxKind::Failed)
                    .map_or(input.summary.clone(), |error| {
                        format!("{}: {error}", input.summary)
                    });
                let delivered = runtime.report_from_mcp(
                    &credential.credential,
                    input.run_id,
                    kind,
                    summary,
                    input.result,
                )?;
                Ok((
                    ResponseOutcome::Ok,
                    serde_json::json!({"delivered_to": delivered}),
                ))
            }
            DispatchToolAction::AgentInbox => {
                let input = serde_json::from_value::<InboxPayload>(payload).map_err(|_| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "invalid agent_inbox payload")
                })?;
                let messages = store
                    .inbox(&caller)
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "dispatch inbox is unavailable")
                    })?
                    .into_iter()
                    .filter(|message| !input.unread_only || !message.read)
                    .filter(|message| input.since.is_none_or(|since| message.created_at > since))
                    .collect::<Vec<_>>();
                Ok((
                    ResponseOutcome::Ok,
                    serde_json::json!({"messages": messages}),
                ))
            }
            _ => Err(ProtocolError::new(
                ErrorCode::InvalidArgument,
                "invalid agent tool action",
            )),
        }
    })();
    match response {
        Ok((outcome, body)) => envelope(hello, request_id, outcome, body),
        Err(error) => envelope(
            hello,
            request_id,
            usagi_core::infrastructure::ipc::ResponseOutcome::Error(error),
            serde_json::Value::Null,
        ),
    }
}

#[allow(clippy::too_many_lines)]
fn dispatch_supervisor_tool(
    runtime: &SharedSupervisorRuntime,
    connection: usagi_core::domain::id::ConnectionId,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use chrono::Utc;
    use usagi_core::domain::{
        id::OperationId,
        supervisor::{EscalationDecision, SupervisorRunId, SupervisorRunState},
    };
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    #[derive(Deserialize)]
    struct StartPayload {
        root_task: String,
        #[serde(default)]
        initial_task_dag: Vec<InitialTask>,
        policy_selector: Option<String>,
    }
    #[derive(Deserialize)]
    struct RunPayload {
        supervisor_run_id: SupervisorRunId,
    }
    #[derive(Deserialize)]
    struct ListPayload {
        state: Option<SupervisorRunState>,
        caller: Option<String>,
        session: Option<String>,
        cursor: Option<String>,
        #[serde(default = "default_page_limit")]
        limit: usize,
    }
    #[derive(Deserialize)]
    struct CancelPayload {
        supervisor_run_id: SupervisorRunId,
        reason: String,
    }
    #[derive(Deserialize)]
    struct ResolvePayload {
        supervisor_run_id: SupervisorRunId,
        escalation_id: OperationId,
        decision: EscalationDecision,
    }
    #[derive(Deserialize)]
    struct EventsPayload {
        supervisor_run_id: SupervisorRunId,
        #[serde(default)]
        after_sequence: u64,
        #[serde(default = "default_page_limit")]
        limit: usize,
    }

    fn default_page_limit() -> usize {
        50
    }

    let parsed = serde_json::from_value::<DaemonRequest>(body.clone());
    let Ok(DaemonRequest::SupervisorTool {
        action,
        operation_id,
        payload,
    }) = parsed
    else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    let caller = format!("ipc-connection:{connection}");
    let result = runtime
        .lock()
        .map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "supervisor runtime is unavailable")
        })
        .and_then(|runtime| match action {
            SupervisorToolAction::Start => {
                let input: StartPayload = serde_json::from_value(payload).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "invalid supervisor_start payload",
                    )
                })?;
                let started = runtime
                    .start(
                        &caller,
                        &operation_id,
                        input.root_task,
                        input.initial_task_dag,
                        input.policy_selector,
                        Utc::now(),
                    )
                    .map_err(supervisor_error)?;
                runtime
                    .tick(
                        started.supervisor_run_id,
                        Utc::now(),
                        &mut DeferredDecisionWaker,
                    )
                    .map_err(supervisor_error)?;
                serde_json::to_value(
                    runtime
                        .get(&caller, started.supervisor_run_id)
                        .map_err(supervisor_error)?
                        .ok_or_else(|| {
                            ProtocolError::new(
                                ErrorCode::Internal,
                                "started supervisor run disappeared",
                            )
                        })?,
                )
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Internal, "supervisor response encoding failed")
                })
            }
            SupervisorToolAction::Get => {
                let input: RunPayload = serde_json::from_value(payload).map_err(|_| {
                    ProtocolError::new(ErrorCode::InvalidArgument, "invalid supervisor_get payload")
                })?;
                serde_json::to_value(
                    runtime
                        .get(&caller, input.supervisor_run_id)
                        .map_err(supervisor_error)?
                        .ok_or_else(|| {
                            ProtocolError::new(
                                ErrorCode::OwnershipUnknown,
                                "supervisor run is unavailable to this caller",
                            )
                        })?,
                )
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Internal, "supervisor response encoding failed")
                })
            }
            SupervisorToolAction::List => {
                let input: ListPayload = serde_json::from_value(payload).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "invalid supervisor_list payload",
                    )
                })?;
                if input.limit == 0
                    || input.limit > 100
                    || input.session.is_some()
                    || input.caller.as_ref().is_some_and(|value| value != &caller)
                {
                    return Err(ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "invalid supervisor_list filter",
                    ));
                }
                let offset = input
                    .cursor
                    .as_deref()
                    .unwrap_or("0")
                    .parse::<usize>()
                    .map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "invalid supervisor_list cursor",
                        )
                    })?;
                let runs = runtime
                    .list(&caller, input.state)
                    .map_err(supervisor_error)?;
                let page: Vec<_> = runs.iter().skip(offset).take(input.limit).collect();
                let next_cursor =
                    (offset + page.len() < runs.len()).then(|| (offset + page.len()).to_string());
                Ok(serde_json::json!({"runs": page, "next_cursor": next_cursor}))
            }
            SupervisorToolAction::Cancel => {
                let input: CancelPayload = serde_json::from_value(payload).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "invalid supervisor_cancel payload",
                    )
                })?;
                serde_json::to_value(
                    runtime
                        .cancel(&caller, input.supervisor_run_id, input.reason, Utc::now())
                        .map_err(supervisor_error)?,
                )
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Internal, "supervisor response encoding failed")
                })
            }
            SupervisorToolAction::ResolveEscalation => {
                let input: ResolvePayload = serde_json::from_value(payload).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "invalid supervisor_resolve_escalation payload",
                    )
                })?;
                serde_json::to_value(
                    runtime
                        .resolve_escalation(
                            &caller,
                            input.supervisor_run_id,
                            input.escalation_id,
                            input.decision,
                            Utc::now(),
                        )
                        .map_err(supervisor_error)?,
                )
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Internal, "supervisor response encoding failed")
                })
            }
            SupervisorToolAction::Events => {
                let input: EventsPayload = serde_json::from_value(payload).map_err(|_| {
                    ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "invalid supervisor_events payload",
                    )
                })?;
                if input.limit == 0 || input.limit > 100 {
                    return Err(ProtocolError::new(
                        ErrorCode::InvalidArgument,
                        "invalid supervisor_events limit",
                    ));
                }
                let (events, cursor) = runtime
                    .events(
                        &caller,
                        input.supervisor_run_id,
                        input.after_sequence,
                        input.limit,
                    )
                    .map_err(supervisor_error)?;
                Ok(serde_json::json!({"events": events, "next_sequence": cursor.next_sequence}))
            }
        });
    match result {
        Ok(value) => envelope(hello, request_id, ResponseOutcome::Ok, value),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::json!(null),
        ),
    }
}

fn supervisor_error(error: anyhow::Error) -> usagi_core::infrastructure::ipc::ProtocolError {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    let message = error.to_string();
    drop(error);
    let code = if message.contains("reused") {
        ErrorCode::IdempotencyConflict
    } else if message.contains("does not exist") {
        ErrorCode::OwnershipUnknown
    } else {
        ErrorCode::InvalidArgument
    };
    ProtocolError::new(code, message)
}

/// PR events are deliberately only hints; the IPC request always returns this
/// durable snapshot so reconnects and dropped events converge without replay.
fn dispatch_pr_snapshot(
    inventory: &SharedPrInventory,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};
    use usagi_core::usecase::client::{DaemonRequest, PrAction};
    let result = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Pr {
                action: PrAction::Snapshot,
                payload,
            } => inventory
                .lock()
                .ok()
                .and_then(|mut projector| projector.snapshot(payload.session_id).ok())
                .and_then(|snapshot| serde_json::to_value(snapshot).ok()),
            _ => None,
        });
    let (outcome, body) = result.map_or_else(
        || {
            (
                ResponseOutcome::Error(ProtocolError::new(
                    ErrorCode::InvalidArgument,
                    "invalid PR snapshot request",
                )),
                serde_json::json!(null),
            )
        },
        |snapshot| (ResponseOutcome::Ok, snapshot),
    );
    usagi_core::infrastructure::ipc::Envelope {
        protocol: hello.protocol,
        daemon_generation: hello.daemon_generation.clone(),
        kind: usagi_core::infrastructure::ipc::EnvelopeKind::Response {
            request_id,
            outcome,
            body,
        },
    }
}

/// Handles the decision subset of the MCP dispatch registry.  The MCP payload
/// never carries an owner: it is reconstructed from the one active durable
/// dispatch binding.  Ambiguity is deliberately fail-closed, preventing an
/// agent from choosing another workspace, caller, or run.
#[allow(clippy::too_many_lines)] // The complete wire-to-store error mapping is one atomic routing contract.
fn dispatch_user_decision(
    agent: &SharedAgentRuntime,
    sessions: &SharedSessionRuntime,
    store: &UserDecisionStore,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use chrono::Utc;
    use usagi_core::domain::agent::RunStatus;
    use usagi_core::domain::id::UserDecisionId;
    use usagi_core::domain::user_decision::{
        UserDecision, UserDecisionAnswer, UserDecisionError, UserDecisionOwner, UserDecisionStatus,
    };
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    #[derive(Deserialize)]
    struct RequestPayload {
        title: String,
        prompt: String,
        options: Vec<usagi_core::domain::user_decision::UserDecisionOption>,
        #[serde(default)]
        allow_freeform: bool,
        #[serde(default)]
        expires_at: Option<chrono::DateTime<Utc>>,
        #[serde(default)]
        idempotency_key: Option<String>,
    }
    #[derive(Deserialize)]
    struct DecisionIdPayload {
        decision_id: UserDecisionId,
    }
    #[derive(Deserialize)]
    struct ResolvePayload {
        decision_id: UserDecisionId,
        answer: UserDecisionAnswer,
    }

    let parsed = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::DispatchTool {
                action,
                payload,
                caller_context,
                ..
            } => Some((action, payload, caller_context, false)),
            DaemonRequest::UserDecision { action, payload } => {
                use usagi_core::usecase::client::TuiUserDecisionAction;
                let action = match action {
                    TuiUserDecisionAction::Get => DispatchToolAction::UserDecisionGet,
                    TuiUserDecisionAction::List => DispatchToolAction::UserDecisionList,
                    TuiUserDecisionAction::Resolve => DispatchToolAction::UserDecisionResolve,
                    TuiUserDecisionAction::Cancel => DispatchToolAction::UserDecisionCancel,
                };
                Some((action, payload, None, true))
            }
            _ => None,
        });
    let Some((action, payload, caller_context, tui_access)) = parsed else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    if !matches!(
        action,
        DispatchToolAction::UserDecisionRequest
            | DispatchToolAction::UserDecisionGet
            | DispatchToolAction::UserDecisionList
            | DispatchToolAction::UserDecisionResolve
            | DispatchToolAction::UserDecisionCancel
            | DispatchToolAction::UserDecisionExpire
    ) {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    }

    let workspace = (|| -> Result<_, ProtocolError> {
        sessions
            .lock()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
            })?
            .snapshot()
            .map_err(|_| {
                ProtocolError::new(
                    ErrorCode::Unavailable,
                    "daemon could not read managed sessions",
                )
            })?
            .get("workspace_id")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or_else(|| {
                ProtocolError::new(ErrorCode::Unavailable, "workspace identity is unavailable")
            })
    })();
    let owner = workspace.and_then(|workspace| -> Result<_, ProtocolError> {
        if tui_access {
            return Ok((workspace, None));
        }
        let runtime = agent.lock().map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
        })?;
        let credential = caller_context.as_ref().ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "decision caller provenance is unknown",
            )
        })?;
        let run_id = runtime.mcp_caller(&credential.credential).ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "decision caller provenance is unknown",
            )
        })?;
        let dispatch = runtime.dispatch_store();
        let run = dispatch
            .runs()
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "dispatch provenance is unavailable")
            })?
            .into_iter()
            .find(|run| run.run_id == run_id && run.status == RunStatus::Running)
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "decision caller provenance is unknown",
                )
            })?;
        let binding = dispatch
            .binding(run_id)
            .map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "dispatch provenance is unavailable")
            })?
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::OwnershipUnknown,
                    "decision caller provenance is unavailable",
                )
            })?;
        if binding.worker.agent_id != run.agent_id {
            return Err(ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "decision caller provenance is inconsistent",
            ));
        }
        Ok((
            workspace,
            Some(UserDecisionOwner {
                workspace_id: workspace,
                session_id: binding.worker.session_id,
                caller: binding.caller,
                run_id,
            }),
        ))
    });
    let response = owner.and_then(|(workspace, owner)| {
        // Resolved events are retained only for atomicity with legacy durable
        // records. Their acknowledgement must not inject a continuation while
        // this MCP call is waiting for its own synchronous response.
        let _ = consume_user_decision_events(store);
        let request_owner = owner.clone();
        let decision_for = |id| -> Result<UserDecision, UserDecisionError> {
            let decision = store
                .get(workspace, id)
                .map_err(|_| UserDecisionError::Terminal)?
                .ok_or(UserDecisionError::Terminal)?;
            if request_owner
                .as_ref()
                .is_some_and(|expected| decision.owner != *expected)
            {
                return Err(UserDecisionError::Terminal);
            }
            Ok(decision)
        };
        let now = Utc::now();
        let result = (|| -> Result<serde_json::Value, UserDecisionError> { match action {
            DispatchToolAction::UserDecisionRequest => {
                let owner = owner.ok_or(UserDecisionError::Terminal)?;
                let input = serde_json::from_value::<RequestPayload>(payload)
                    .map_err(|_| UserDecisionError::Terminal)?;
                let decision = store
                    .create(UserDecision {
                        decision_id: UserDecisionId::new(), owner, title: input.title, prompt: input.prompt,
                        options: input.options, allow_freeform: input.allow_freeform, expires_at: input.expires_at,
                        idempotency_key: input.idempotency_key, status: UserDecisionStatus::Pending, answer: None,
                        created_at: now, resolved_at: None,
                    })
                    .map_err(|_| UserDecisionError::Terminal)?
                    ?;
                wait_for_user_decision(store, workspace, &decision)
            }
            DispatchToolAction::UserDecisionGet => {
                let input = serde_json::from_value::<DecisionIdPayload>(payload).map_err(|_| UserDecisionError::Terminal)?;
                decision_for(input.decision_id).map(|decision| serde_json::json!(decision))
            }
            DispatchToolAction::UserDecisionList => store.pending(workspace)
                .map_err(|_| UserDecisionError::Terminal)
                .map(|decisions| decisions.into_iter().filter(|decision| {
                    owner.as_ref().is_none_or(|expected| decision.owner == *expected)
                }).collect::<Vec<_>>())
                .map(|decisions| serde_json::json!({"workspace": workspace, "decisions": decisions})),
            DispatchToolAction::UserDecisionResolve => {
                let input = serde_json::from_value::<ResolvePayload>(payload).map_err(|_| UserDecisionError::Terminal)?;
                let _ = decision_for(input.decision_id)?;
                let decision = store.resolve(workspace, input.decision_id, input.answer, now)
                    .map_err(|_| UserDecisionError::Terminal)?
                    ?;
                Ok(serde_json::json!(decision))
            }
            DispatchToolAction::UserDecisionCancel | DispatchToolAction::UserDecisionExpire => {
                let input = serde_json::from_value::<DecisionIdPayload>(payload).map_err(|_| UserDecisionError::Terminal)?;
                let _ = decision_for(input.decision_id)?;
                let status = if action == DispatchToolAction::UserDecisionCancel { UserDecisionStatus::Cancelled } else { UserDecisionStatus::Expired };
                store.terminal(workspace, input.decision_id, status, now)
                    .map_err(|_| UserDecisionError::Terminal)?
                    .map(|decision| serde_json::json!(decision))
            }
            _ => unreachable!(),
        } })();
        let value = result.map_err(|error| {
            let (code, message) = match error {
                UserDecisionError::IdempotencyConflict => (ErrorCode::IdempotencyConflict, "decision idempotency key conflicts"),
                UserDecisionError::InvalidOption => (ErrorCode::InvalidArgument, "decision option is not allowed"),
                UserDecisionError::FreeformNotAllowed => (ErrorCode::InvalidArgument, "freeform decision answer is not allowed"),
                UserDecisionError::Expired => (ErrorCode::DeadlineExceeded, "decision has expired"),
                UserDecisionError::Terminal => (ErrorCode::RevisionConflict, "decision is not pending or is outside this workspace"),
            };
            ProtocolError::new(code, message)
        })?;
        let _ = consume_user_decision_events(store);
        Ok(value)
    });
    match response {
        Ok(value) => envelope(hello, request_id, ResponseOutcome::Ok, value),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::json!(null),
        ),
    }
}

fn wait_for_user_decision(
    decisions: &UserDecisionStore,
    workspace: usagi_core::domain::id::WorkspaceId,
    requested: &usagi_core::domain::user_decision::UserDecision,
) -> Result<serde_json::Value, usagi_core::domain::user_decision::UserDecisionError> {
    use usagi_core::domain::user_decision::UserDecisionStatus;

    loop {
        let decision = decisions
            .get(workspace, requested.decision_id)
            .map_err(|_| usagi_core::domain::user_decision::UserDecisionError::Terminal)?
            .ok_or(usagi_core::domain::user_decision::UserDecisionError::Terminal)?;
        match decision.status {
            UserDecisionStatus::Pending => std::thread::sleep(Duration::from_millis(25)),
            UserDecisionStatus::Resolved => {
                let answer = decision
                    .answer
                    .ok_or(usagi_core::domain::user_decision::UserDecisionError::Terminal)?;
                return Ok(serde_json::json!({
                    "decision_id": decision.decision_id,
                    "status": "resolved",
                    "answer": answer,
                }));
            }
            UserDecisionStatus::Cancelled => {
                return Err(usagi_core::domain::user_decision::UserDecisionError::Terminal);
            }
            UserDecisionStatus::Expired => {
                return Err(usagi_core::domain::user_decision::UserDecisionError::Expired);
            }
        }
    }
}

fn consume_user_decision_events(
    decisions: &UserDecisionStore,
) -> Result<(), usagi_core::infrastructure::ipc::ProtocolError> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};

    // A resolved event and its answer are atomically persisted together. The
    // caller now receives that answer from its still-open MCP request, so the
    // outbox has no asynchronous PTY continuation to deliver.
    for event in decisions
        .events()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "decision outbox is unavailable"))?
    {
        let Some(decision) = decisions.get_for_event(&event).map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "decision outbox is unavailable")
        })?
        else {
            return Err(ProtocolError::new(
                ErrorCode::Unavailable,
                "decision delivery record is inconsistent",
            ));
        };
        let _ = decision;
        decisions.ack_event(event.decision_id).map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "decision outbox is unavailable")
        })?;
    }
    Ok(())
}

fn dispatch_dispatch(
    agent: &SharedAgentRuntime,
    sessions: &SharedSessionRuntime,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};
    use usagi_core::usecase::client::{DaemonRequest, SessionAction};
    let Some((operation_id, intent)) = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Dispatch {
                operation_id,
                intent,
            } => Some((operation_id, intent)),
            _ => None,
        })
    else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    let session_id = (|| {
        let mut runtime = sessions.lock().map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
        })?;
        let snapshot = runtime.snapshot().map_err(|_| {
            ProtocolError::new(
                ErrorCode::Unavailable,
                "daemon could not read managed sessions",
            )
        })?;
        if let Some(id) = session_id_by_name(&snapshot, &intent.session_name) {
            return Ok(id);
        }
        let created = runtime
            .handle(
                SessionAction::Create,
                &operation_id,
                &serde_json::json!({"name": intent.session_name}),
            )
            .map_err(|error| {
                ProtocolError::new(ErrorCode::InvalidArgument, error.safe_message())
            })?;
        session_id_by_name(&created.body, &intent.session_name).ok_or_else(|| {
            ProtocolError::new(ErrorCode::Unavailable, "created session is not available")
        })
    })();
    let result = session_id.and_then(|session_id| {
        let scope = SharedScopeResolver(Arc::clone(sessions));
        agent
            .lock()
            .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))?
            .dispatch(&operation_id, &intent, session_id, &scope)
    });
    match result {
        Ok(admission) => envelope(
            hello,
            request_id,
            ResponseOutcome::Accepted {
                operation_id: usagi_core::infrastructure::ipc::OperationId(
                    admission.operation_id.clone(),
                ),
                operation_revision: admission.revision,
            },
            serde_json::json!({"run_id": admission.operation_id, "terminal": admission.terminal, "completed": admission.completed}),
        ),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::json!(null),
        ),
    }
}

fn session_id_by_name(snapshot: &serde_json::Value, name: &str) -> Option<SessionId> {
    snapshot
        .get("sessions")?
        .as_array()?
        .iter()
        .find(|session| {
            session.get("name").and_then(serde_json::Value::as_str) == Some(name)
                && session.get("lifecycle").and_then(serde_json::Value::as_str) == Some("available")
        })
        .and_then(|session| serde_json::from_value(session.get("session_id")?.clone()).ok())
}

fn dispatch_metrics(
    metrics: &SharedMetricsBroker,
    process_metrics: &SharedProcessResourceSampler,
    pipeline_metrics: &TerminalPipelineMetrics,
    observer: &mut Option<MetricsObserver>,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};
    use usagi_core::usecase::client::{DaemonRequest, MetricsAction};

    let action = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Metrics { action } => Some(action),
            _ => None,
        });
    let Some(action) = action else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    let snapshot = (|| {
        let mut broker = metrics
            .lock()
            .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "metrics are unavailable"))?;
        match action {
            MetricsAction::Subscribe => {
                if observer.is_none() {
                    *observer = Some(broker.subscribe());
                }
                Ok(broker.snapshot())
            }
            MetricsAction::Unsubscribe => {
                if let Some(current) = observer.take() {
                    broker.unsubscribe(current.subscription());
                }
                Ok(broker.snapshot())
            }
            MetricsAction::Snapshot => {
                let (cpu_percent_hundredths, resident_memory_bytes) = process_metrics
                    .lock()
                    .map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::Unavailable,
                            "process metrics are unavailable",
                        )
                    })?
                    .snapshot();
                let retention = output_pipeline_counters();
                let projection_counters = pr_projection_counters();
                let sampled_at_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |duration| {
                        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
                    });
                Ok(broker.publish(MetricsSample {
                    sampled_at_ms,
                    cpu_percent_hundredths,
                    resident_memory_bytes,
                    terminal_dropped_bytes: retention.dropped_bytes,
                    terminal_coalesced_bytes: retention.coalesced_bytes,
                    terminal_backpressured_bytes: pipeline_metrics
                        .backpressured_bytes
                        .load(Ordering::Relaxed),
                    pr_projection_dropped_bytes: projection_counters.dropped_bytes,
                    pr_projection_coalesced_bytes: projection_counters.coalesced_bytes,
                    pr_projection_gaps: projection_counters.gaps,
                }))
            }
        }
    })();
    match snapshot {
        Ok(snapshot) => envelope(
            hello,
            request_id,
            ResponseOutcome::Ok,
            serde_json::json!(snapshot),
        ),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::Value::Null,
        ),
    }
}

fn dispatch_session(
    session: &SharedSessionRuntime,
    teardown: &TeardownSignal,
    agent: &SharedAgentRuntime,
    pr_inventory: &SharedPrInventory,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::usecase::client::DaemonRequest;
    let request = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Session {
                action,
                operation_id,
                payload,
            } => Some((action, operation_id, payload)),
            _ => None,
        });
    let Some((action, operation_id, payload)) = request else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    let result = dispatch_session_action(
        session,
        teardown,
        agent,
        pr_inventory,
        action,
        &operation_id,
        &payload,
    );
    session_response_envelope(action, &payload, result, request_id, hello)
}

fn session_response_envelope(
    action: usagi_core::usecase::client::SessionAction,
    payload: &serde_json::Value,
    result: Result<usagi_daemon::usecase::session_runtime::SessionReply, SessionRuntimeError>,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::ResponseOutcome;
    use usagi_core::usecase::client::SessionAction;
    match result {
        Ok(reply) => {
            let recovery_apply =
                payload.get("apply").and_then(serde_json::Value::as_bool) == Some(true);
            let outcome = if matches!(
                action,
                SessionAction::Create | SessionAction::Remove | SessionAction::ResumeAgent
            ) || (action == SessionAction::RecoverLegacy && recovery_apply)
            {
                ResponseOutcome::Accepted {
                    operation_id: usagi_core::infrastructure::ipc::OperationId(
                        reply.operation_id.clone(),
                    ),
                    operation_revision: reply.revision,
                }
            } else {
                ResponseOutcome::Ok
            };
            // A mutation is synchronously finalized by the lifecycle runtime,
            // but its wire outcome remains Accepted so retries retain the
            // producer-issued operation identity.  Carry the safe final hook
            // beside the snapshot: interactive clients use it to retire their
            // pending UI only after the matching daemon operation completed.
            let mut body = reply.body;
            if let Some(kind) = match action {
                SessionAction::Create => Some("session.created"),
                SessionAction::Remove => Some("session.removed"),
                SessionAction::ResumeAgent => Some("agent.resumed"),
                SessionAction::RecoverLegacy if recovery_apply => Some("session.legacy_recovered"),
                SessionAction::RecoverLegacy
                | SessionAction::List
                | SessionAction::Status
                | SessionAction::Overview
                | SessionAction::Setup
                | SessionAction::Prompt
                | SessionAction::Complete
                | SessionAction::Pr
                | SessionAction::NoteGet
                | SessionAction::NoteUpdate
                | SessionAction::TodoList
                | SessionAction::TodoAdd
                | SessionAction::TodoUpdate
                | SessionAction::TodoRemove
                | SessionAction::DecisionList
                | SessionAction::DecisionLog
                | SessionAction::DelegateIssue
                | SessionAction::DelegateBrief => None,
            } && let Some(object) = body.as_object_mut()
            {
                object.insert(
                    "hook".to_owned(),
                    serde_json::json!({
                        "kind": kind,
                        "operation_id": reply.operation_id,
                        "revision": reply.revision,
                    }),
                );
            }
            envelope(hello, request_id, outcome, body)
        }
        Err(error) => {
            let code = match &error {
                SessionRuntimeError::IdempotencyConflict => {
                    usagi_core::infrastructure::ipc::ErrorCode::IdempotencyConflict
                }
                SessionRuntimeError::AgentFailure { code, .. } => *code,
                SessionRuntimeError::Delivery(_) => {
                    usagi_core::infrastructure::ipc::ErrorCode::Unavailable
                }
                _ => usagi_core::infrastructure::ipc::ErrorCode::InvalidArgument,
            };
            envelope(
                hello,
                request_id,
                ResponseOutcome::Error(usagi_core::infrastructure::ipc::ProtocolError::new(
                    code,
                    error.safe_message(),
                )),
                serde_json::json!(null),
            )
        }
    }
}

#[allow(clippy::too_many_lines)]
fn dispatch_session_action(
    sessions: &SharedSessionRuntime,
    teardown: &TeardownSignal,
    agent: &SharedAgentRuntime,
    pr_inventory: &SharedPrInventory,
    action: usagi_core::usecase::client::SessionAction,
    operation_id: &str,
    payload: &serde_json::Value,
) -> Result<usagi_daemon::usecase::session_runtime::SessionReply, SessionRuntimeError> {
    use usagi_core::infrastructure::store::{issue::IssueStore, state::WorkspaceStateStore};
    use usagi_core::usecase::client::SessionAction;
    use usagi_core::usecase::{issue, note};
    use usagi_daemon::usecase::agent_ipc::PromptMode;

    let reply = |body: serde_json::Value| {
        let revision = sessions
            .lock()
            .ok()
            .and_then(|runtime| runtime.snapshot().ok())
            .and_then(|snapshot| snapshot.get("revision").and_then(serde_json::Value::as_u64))
            .unwrap_or_default();
        Ok(usagi_daemon::usecase::session_runtime::SessionReply {
            operation_id: operation_id.to_owned(),
            revision,
            body,
        })
    };
    let string = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(SessionRuntimeError::InvalidRequest)
    };
    let caller_scope = || {
        let credential = string("_caller_credential")?;
        let session_id = agent
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .caller_session(credential)
            .ok_or(SessionRuntimeError::ScopeUnavailable)?;
        sessions
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .session_scope_by_id(session_id)
    };
    let named_session = |name: &str| {
        sessions
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .session_id(name)
    };

    match action {
        SessionAction::ResumeAgent => {
            let exact_target = payload
                .get("target")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| SessionRuntimeError::InvalidRequest)?;
            let (name, id) = if let Some(id) = exact_target
                .as_ref()
                .and_then(|target: &usagi_core::domain::agent::AgentResumeTarget| target.session_id)
            {
                (None, id)
            } else {
                let supplied_id = payload
                    .get("session_id")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|_| SessionRuntimeError::InvalidRequest)?;
                if let Some(id) = supplied_id {
                    (None, id)
                } else {
                    let name = string("name")?;
                    (Some(name), named_session(name)?)
                }
            };
            let target = sessions
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .session_scope_by_id(id)?;
            let resolver = SharedScopeResolver(Arc::clone(sessions));
            let admission = if let Some(exact_target) = exact_target {
                agent
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .resume_exact(operation_id, &exact_target, &resolver)
            } else {
                agent
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .resume_legacy(operation_id, target.workspace_id, Some(id), &resolver)
            }
            .map_err(|error| SessionRuntimeError::AgentFailure {
                code: error.code,
                message: error.message,
            })?;
            reply(serde_json::json!({
                "name": name,
                "session_id": id,
                "terminal": admission.terminal,
                "continuation": admission.continuation,
                "resume_relation": admission.resume_relation,
                "completed": admission.completed,
            }))
        }
        SessionAction::List | SessionAction::Status | SessionAction::Overview => {
            let mut status = sessions
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .handle(action, operation_id, payload)?;
            let runtime = agent.lock().map_err(|_| SessionRuntimeError::Storage)?;
            if let Some(items) = status
                .body
                .get_mut("sessions")
                .and_then(serde_json::Value::as_array_mut)
            {
                for item in items {
                    if let Some(id) = item
                        .get("session_id")
                        .cloned()
                        .and_then(|value| serde_json::from_value(value).ok())
                    {
                        item["agent_phase"] = serde_json::json!(runtime.session_phase(id));
                        let (resumable, reason) = runtime.session_resume_status(id);
                        item["agent_resumable"] = serde_json::json!(resumable);
                        item["agent_resume_reason"] = serde_json::json!(reason);
                    }
                }
            }
            Ok(status)
        }
        SessionAction::Prompt => {
            let name = string("name")?;
            let prompt = string("prompt")?;
            let target = if name == ":root" {
                None
            } else {
                Some(named_session(name)?)
            };
            let mode = match payload
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("auto")
            {
                "auto" => PromptMode::Auto,
                "queue" => PromptMode::Queue,
                "live" => PromptMode::Live,
                _ => return Err(SessionRuntimeError::InvalidRequest),
            };
            let delivery = agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .prompt(target, prompt, mode)
                .map_err(|error| SessionRuntimeError::Delivery(error.message))?;
            reply(
                serde_json::json!({"name": name, "delivered_to": delivery.delivered_to, "queued": delivery.queued}),
            )
        }
        SessionAction::Complete => {
            let message = string("message")?;
            let scope = caller_scope()?;
            let report = format!("Session {} completed:\n\n{message}", scope.session_id);
            let delivery = agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .prompt(None, &report, PromptMode::Auto)
                .map_err(|error| SessionRuntimeError::Delivery(error.message))?;
            reply(
                serde_json::json!({"session_id": scope.session_id, "reported_to": ":root", "delivered_to": delivery.delivered_to}),
            )
        }
        SessionAction::Pr => {
            let name = string("name")?;
            let id = named_session(name)?;
            let snapshot = pr_inventory
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .snapshot(id)
                .map_err(|_| SessionRuntimeError::Storage)?;
            let merged = snapshot
                .entries
                .iter()
                .any(|entry| entry.state == usagi_core::domain::pr_inventory::PrState::Merged);
            reply(
                serde_json::json!({"name": name, "session_id": id, "revision": snapshot.revision, "merged": merged, "pr": snapshot.entries}),
            )
        }
        SessionAction::NoteGet
        | SessionAction::NoteUpdate
        | SessionAction::TodoList
        | SessionAction::TodoAdd
        | SessionAction::TodoUpdate
        | SessionAction::TodoRemove
        | SessionAction::DecisionList
        | SessionAction::DecisionLog => {
            let scope = caller_scope()?;
            let store = WorkspaceStateStore::new(&scope.path);
            let target = note::Target::Root;
            let body = match action {
                SessionAction::NoteGet => {
                    serde_json::json!({"note": note::note(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::NoteUpdate => {
                    let value = payload
                        .get("note")
                        .and_then(serde_json::Value::as_str)
                        .ok_or(SessionRuntimeError::InvalidRequest)?;
                    note::set_note(&store, target, value, chrono::Utc::now())
                        .map_err(|_| SessionRuntimeError::Storage)?;
                    serde_json::json!({"note": note::note(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::TodoList => {
                    serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::TodoAdd => {
                    let text = string("text")?;
                    note::add_todo(&store, target, text, chrono::Utc::now())
                        .map_err(|_| SessionRuntimeError::Storage)?;
                    serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::TodoUpdate => {
                    let index = payload
                        .get("index")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok())
                        .ok_or(SessionRuntimeError::InvalidRequest)?;
                    let done = payload
                        .get("done")
                        .map(|value| value.as_bool().ok_or(SessionRuntimeError::InvalidRequest))
                        .transpose()?;
                    let text = payload
                        .get("text")
                        .map(|value| {
                            value
                                .as_str()
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .map(str::to_owned)
                                .ok_or(SessionRuntimeError::InvalidRequest)
                        })
                        .transpose()?;
                    if done.is_none() && text.is_none() {
                        return Err(SessionRuntimeError::InvalidRequest);
                    }
                    if !note::update_todo(&store, target, index, done, text, chrono::Utc::now())
                        .map_err(|_| SessionRuntimeError::Storage)?
                    {
                        return Err(SessionRuntimeError::InvalidRequest);
                    }
                    serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::TodoRemove => {
                    let index = payload
                        .get("index")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok())
                        .ok_or(SessionRuntimeError::InvalidRequest)?;
                    if !note::remove_todo(&store, target, index, chrono::Utc::now())
                        .map_err(|_| SessionRuntimeError::Storage)?
                    {
                        return Err(SessionRuntimeError::InvalidRequest);
                    }
                    serde_json::json!({"todos": note::todos(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::DecisionList => {
                    serde_json::json!({"decisions": note::decisions(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                SessionAction::DecisionLog => {
                    let text = string("text")?;
                    note::log_decision(&store, target, text, chrono::Utc::now())
                        .map_err(|_| SessionRuntimeError::Storage)?;
                    serde_json::json!({"decisions": note::decisions(&store, target).map_err(|_| SessionRuntimeError::Storage)?})
                }
                _ => unreachable!(),
            };
            reply(serde_json::json!({"session_id": scope.session_id, "scratchpad": body}))
        }
        SessionAction::DelegateIssue | SessionAction::DelegateBrief => {
            let (name, prompt) = if action == SessionAction::DelegateIssue {
                let number = payload
                    .get("number")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or(SessionRuntimeError::InvalidRequest)?;
                let root = sessions
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .repository_root()
                    .to_path_buf();
                let issue = issue::get(&IssueStore::new(root), number)
                    .map_err(|error| {
                        error
                            .chain()
                            .find_map(|cause| cause.downcast_ref::<AmbiguousIssueNumber>())
                            .cloned()
                            .map_or(
                                SessionRuntimeError::Storage,
                                SessionRuntimeError::AmbiguousIssue,
                            )
                    })?
                    .ok_or(SessionRuntimeError::InvalidRequest)?;
                (
                    payload
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map_or_else(|| format!("issue-{number}"), str::to_owned),
                    issue::to_prompt(&issue),
                )
            } else {
                let brief = string("brief")?;
                let suffix = operation_id
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .take(8)
                    .collect::<String>();
                let name = payload
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map_or_else(|| format!("triage-{suffix}"), str::to_owned);
                (
                    name,
                    format!(
                        "このセッションの worktree 内で次の依頼をトリアージし、必要なら issue 化して実装へつなげてください。リポジトリの規約に従ってください。\n\n{brief}"
                    ),
                )
            };
            if action == SessionAction::DelegateBrief {
                use usagi_core::domain::agent::{AgentProfileId, ModelSelector};
                use usagi_core::usecase::client::{DispatchAgentIntent, DispatchIntent};

                let selector = payload
                    .get("agent")
                    .and_then(serde_json::Value::as_object)
                    .ok_or(SessionRuntimeError::InvalidRequest)?;
                let selected = if let Some(id) = selector.get("id") {
                    if selector.len() != 1 {
                        return Err(SessionRuntimeError::InvalidRequest);
                    }
                    DispatchAgentIntent::Existing {
                        agent_id: serde_json::from_value(id.clone())
                            .map_err(|_| SessionRuntimeError::InvalidRequest)?,
                    }
                } else {
                    if selector.len() != 2 {
                        return Err(SessionRuntimeError::InvalidRequest);
                    }
                    let runtime = selector
                        .get("runtime")
                        .cloned()
                        .and_then(|value| serde_json::from_value::<AgentProfileId>(value).ok())
                        .ok_or(SessionRuntimeError::InvalidRequest)?;
                    let model = selector
                        .get("model")
                        .cloned()
                        .and_then(|value| serde_json::from_value::<ModelSelector>(value).ok())
                        .ok_or(SessionRuntimeError::InvalidRequest)?;
                    DispatchAgentIntent::New { runtime, model }
                };
                let credential = string("_caller_credential")?;
                let (workspace, caller) = {
                    let runtime = agent.lock().map_err(|_| SessionRuntimeError::Storage)?;
                    let caller = runtime
                        .mcp_dispatch_caller(credential)
                        .ok_or(SessionRuntimeError::ScopeUnavailable)?;
                    let workspace = sessions
                        .lock()
                        .map_err(|_| SessionRuntimeError::Storage)?
                        .snapshot()
                        .map_err(|_| SessionRuntimeError::Storage)?
                        .get("workspace_id")
                        .cloned()
                        .and_then(|value| serde_json::from_value(value).ok())
                        .ok_or(SessionRuntimeError::Storage)?;
                    (workspace, caller)
                };
                // Reject an invalid selector or an unauthenticated caller
                // before creating the isolated worktree. This composite
                // operation must not leave an orphan session on rejection.
                let created = sessions
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .handle(
                        SessionAction::Create,
                        operation_id,
                        &serde_json::json!({"name": name}),
                    )?;
                let id = sessions
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .session_id(&name)?;
                let scope = SharedScopeResolver(Arc::clone(sessions));
                let admission = agent
                    .lock()
                    .map_err(|_| SessionRuntimeError::Storage)?
                    .dispatch(
                        operation_id,
                        &DispatchIntent {
                            workspace,
                            session_name: name.clone(),
                            caller,
                            agent: selected,
                            prompt: prompt.clone(),
                        },
                        id,
                        &scope,
                    )
                    .map_err(|error| SessionRuntimeError::Delivery(error.message))?;
                return reply(serde_json::json!({
                    "name": name,
                    "session_id": id,
                    "created": created.body,
                    "run_id": admission.operation_id,
                    "terminal": admission.terminal,
                    "completed": admission.completed,
                }));
            }
            let created = sessions
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .handle(
                    SessionAction::Create,
                    operation_id,
                    &serde_json::json!({"name": name}),
                )?;
            let id = sessions
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .session_id(&name)?;
            let delivery = agent
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .prompt(Some(id), &prompt, PromptMode::Queue)
                .map_err(|error| SessionRuntimeError::Delivery(error.message))?;
            reply(
                serde_json::json!({"name": name, "session_id": id, "created": created.body, "delivered_to": delivery.delivered_to, "queued": delivery.queued}),
            )
        }
        // Create runs its heavy Git worktree build with the shared session lock
        // released, so a long `git worktree add` never freezes concurrent
        // readers (session list, terminal poll, user-decision list) on the
        // daemon. The fast durable transitions still run under the lock.
        SessionAction::Create => perform_create(sessions, &SystemGit, operation_id, payload),
        // Remove goes further: it answers as soon as the session is durably
        // `Deleting` and hands the unbounded worktree teardown to the daemon's
        // teardown worker. Keeping the teardown on this connection would hold
        // the reply past every client attempt deadline for a session with a
        // multi-gigabyte `target/`.
        SessionAction::Remove => perform_remove(sessions, teardown, operation_id, payload),
        _ => sessions
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .handle(action, operation_id, payload),
    }
}

fn dispatch_agent(
    agent: &SharedAgentRuntime,
    scope_sessions: &SharedSessionRuntime,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};
    use usagi_core::usecase::client::DaemonRequest;
    enum Request {
        Launch(String, usagi_core::usecase::client::AgentLaunchIntent),
        Inventory(usagi_core::domain::id::WorkspaceId),
        Resume(String, usagi_core::domain::agent::AgentResumeTarget),
    }
    let request = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Agent {
                operation_id,
                intent,
            } => Some(Request::Launch(operation_id, intent)),
            DaemonRequest::AgentInventory { workspace } => Some(Request::Inventory(workspace)),
            DaemonRequest::ResumeAgent {
                operation_id,
                target,
            } => Some(Request::Resume(operation_id, target)),
            _ => None,
        });
    let Some(request) = request else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    let scope = SharedScopeResolver(Arc::clone(scope_sessions));
    let result = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"));
    if let Request::Inventory(workspace) = &request {
        return match result {
            Ok(agent) => envelope(
                hello,
                request_id,
                ResponseOutcome::Ok,
                serde_json::to_value(agent.inventory(*workspace))
                    .expect("safe Agent inventory is serializable"),
            ),
            Err(error) => envelope(
                hello,
                request_id,
                ResponseOutcome::Error(error),
                serde_json::Value::Null,
            ),
        };
    }
    let result = result.and_then(|mut agent| match &request {
        Request::Launch(operation_id, intent) => agent.launch(operation_id, intent, &scope),
        Request::Resume(operation_id, target) => agent.resume_exact(operation_id, target, &scope),
        Request::Inventory(_) => unreachable!("inventory returned above"),
    });
    match result {
        Ok(admission) => {
            // `Ok` is the durable final — direct or replayed after a reconnect —
            // and `ResponseOutcome::Ok` carries no envelope operation identity, so
            // the body is what makes the final correlatable to the producer's
            // pending operation. Every answer therefore states its own
            // `operation_id` and the digest of the intent it was admitted for
            // (#522); the client refuses a final that does not match both.
            let outcome = if admission.completed {
                ResponseOutcome::Ok
            } else {
                ResponseOutcome::Accepted {
                    operation_id: usagi_core::infrastructure::ipc::OperationId(
                        admission.operation_id.clone(),
                    ),
                    operation_revision: admission.revision,
                }
            };
            envelope(
                hello,
                request_id,
                outcome,
                serde_json::json!({
                    "operation_id": admission.operation_id,
                    "semantic_digest": admission.semantic_digest,
                    "terminal": admission.terminal,
                    "continuation": admission.continuation,
                    "resume_relation": admission.resume_relation,
                    "completed": admission.completed,
                }),
            )
        }
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::json!(null),
        ),
    }
}

fn dispatch_codex_session_capture(
    agent: &SharedAgentRuntime,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    let request = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::CodexSessionCapture {
                native_session_id,
                caller_context,
            } => Some((native_session_id, caller_context)),
            _ => None,
        });
    let Some((native_session_id, caller_context)) = request else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    let result = (!caller_context.credential.is_empty())
        .then_some(())
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "Codex runtime credential is unknown",
            )
        })
        .and_then(|()| {
            agent
                .lock()
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                })?
                .capture_codex_session(&caller_context.credential, native_session_id)
        });
    match result {
        Ok(()) => envelope(
            hello,
            request_id,
            ResponseOutcome::Ok,
            serde_json::Value::Null,
        ),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::Value::Null,
        ),
    }
}

/// Routes one private agent lifecycle phase report to the Agent owner.
///
/// Unlike the generic fallback dispatch, a body which is not a well formed phase
/// report is refused here: an agent-originated report must fail closed instead
/// of being echoed back as a success.
fn dispatch_agent_phase_report(
    agent: &SharedAgentRuntime,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    let request = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::AgentPhaseReport {
                phase,
                caller_context,
            } => Some((phase, caller_context)),
            _ => None,
        });
    let result = request
        .filter(|(_, caller_context)| !caller_context.credential.is_empty())
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::InvalidArgument,
                "agent phase report is not a valid credential-bound report",
            )
        })
        .and_then(|(phase, caller_context)| {
            agent
                .lock()
                .map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                })?
                .report_agent_phase(&caller_context.credential, phase)
        });
    let outcome = match result {
        Ok(()) => ResponseOutcome::Ok,
        Err(error) => ResponseOutcome::Error(error),
    };
    envelope(hello, request_id, outcome, serde_json::Value::Null)
}

fn envelope(
    hello: &usagi_core::infrastructure::ipc::ServerHello,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    outcome: usagi_core::infrastructure::ipc::ResponseOutcome,
    body: serde_json::Value,
) -> usagi_core::infrastructure::ipc::Envelope {
    usagi_core::infrastructure::ipc::Envelope {
        protocol: hello.protocol,
        daemon_generation: hello.daemon_generation.clone(),
        kind: usagi_core::infrastructure::ipc::EnvelopeKind::Response {
            request_id,
            outcome,
            body,
        },
    }
}

struct FsRecordFile {
    path: PathBuf,
}

static DAEMON_RECORD_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static FAIL_PRIVATE_LOCK_AFTER_CREATE: RefCell<Option<PathBuf>> = const {
        RefCell::new(None)
    };
    static PRIVATE_LOCK_AFTER_FLOCK_BARRIER: RefCell<Option<PrivateLockAfterFlockBarrier>> = const {
        RefCell::new(None)
    };
}

#[cfg(test)]
struct PrivateLockAfterFlockBarrier {
    path: PathBuf,
    acquired: Arc<std::sync::Barrier>,
    replaced: Arc<std::sync::Barrier>,
}

#[cfg(test)]
fn fail_private_lock_after_create(path: &Path) {
    FAIL_PRIVATE_LOCK_AFTER_CREATE.with(|failpoint| {
        *failpoint.borrow_mut() = Some(path.to_path_buf());
    });
}

#[cfg(test)]
fn take_private_lock_create_failpoint(path: &Path) -> bool {
    FAIL_PRIVATE_LOCK_AFTER_CREATE.with(|failpoint| {
        if failpoint.borrow().as_deref() == Some(path) {
            failpoint.borrow_mut().take();
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
fn install_private_lock_after_flock_barrier(
    path: &Path,
    acquired: Arc<std::sync::Barrier>,
    replaced: Arc<std::sync::Barrier>,
) {
    PRIVATE_LOCK_AFTER_FLOCK_BARRIER.with(|barrier| {
        *barrier.borrow_mut() = Some(PrivateLockAfterFlockBarrier {
            path: path.to_path_buf(),
            acquired,
            replaced,
        });
    });
}

#[cfg(test)]
fn wait_private_lock_after_flock_barrier(path: &Path) {
    let barrier = PRIVATE_LOCK_AFTER_FLOCK_BARRIER.with(|slot| {
        let matches = slot
            .borrow()
            .as_ref()
            .is_some_and(|barrier| barrier.path == path);
        matches.then(|| slot.borrow_mut().take().expect("barrier was present"))
    });
    if let Some(barrier) = barrier {
        barrier.acquired.wait();
        barrier.replaced.wait();
    }
}

fn private_lock_error(label: &str, detail: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("{label} {detail}"),
    )
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PrivateLockModePolicy {
    CrashResidue,
    BootstrapLegacy0644,
}

fn verify_private_lock_metadata(
    metadata: &std::fs::Metadata,
    label: &str,
    mode_policy: Option<PrivateLockModePolicy>,
) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mode = metadata.permissions().mode() & 0o7777;
    let mode_is_safe = match mode_policy {
        None => mode == 0o600,
        Some(PrivateLockModePolicy::CrashResidue) => mode & !0o600 == 0,
        Some(PrivateLockModePolicy::BootstrapLegacy0644) => mode & !0o600 == 0 || mode == 0o644,
    };
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || !mode_is_safe
    {
        return Err(private_lock_error(
            label,
            "is not an exact private single-link regular owner file",
        ));
    }
    Ok(())
}

fn open_private_lock(
    path: &Path,
    label: &str,
    mode_policy: PrivateLockModePolicy,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{label} path has no parent"),
        )
    })?;
    ensure_private_dir(parent)?;
    let open = |create_new| {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        if create_new {
            options.create_new(true);
        }
        options.open(path)
    };

    let (file, created) = match open(true) {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => match open(false) {
            Ok(file) => (file, false),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                // A creator killed after create_new but before fd-fchmod can
                // leave an owner-only directory containing a mode-000 lock.
                // Validate that residue before path chmod, then require the
                // O_NOFOLLOW reopen to resolve to the exact inode inspected.
                let before = std::fs::symlink_metadata(path)?;
                verify_private_lock_metadata(&before, label, Some(mode_policy))?;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                let file = open(false)?;
                let after = file.metadata()?;
                if before.dev() != after.dev() || before.ino() != after.ino() {
                    return Err(private_lock_error(
                        label,
                        "changed while repairing its mode",
                    ));
                }
                (file, false)
            }
            Err(error) => return Err(error),
        },
        Err(error) => return Err(error),
    };

    #[cfg(not(test))]
    let _ = created;
    #[cfg(test)]
    if created && take_private_lock_create_failpoint(path) {
        return Err(std::io::Error::other(format!(
            "injected {label} failure after create_new"
        )));
    }

    // Reject links/non-owner nodes before chmod so widening a hostile inode is
    // impossible. fd-fchmod then repairs both umask-reduced creation and safe
    // legacy modes without reopening the pathname.
    verify_private_lock_metadata(&file.metadata()?, label, Some(mode_policy))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    verify_private_lock_metadata(&file.metadata()?, label, None)?;
    Ok(file)
}

fn verify_private_lock_path(path: &Path, file: &std::fs::File, label: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let descriptor = file.metadata()?;
    verify_private_lock_metadata(&descriptor, label, None)?;
    let pathname = std::fs::symlink_metadata(path)?;
    verify_private_lock_metadata(&pathname, label, None)?;
    if descriptor.dev() != pathname.dev() || descriptor.ino() != pathname.ino() {
        return Err(private_lock_error(
            label,
            "pathname does not name the locked inode",
        ));
    }
    let descriptor_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    if descriptor_flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if descriptor_flags & libc::FD_CLOEXEC == 0 {
        return Err(private_lock_error(label, "descriptor is not close-on-exec"));
    }
    Ok(())
}

/// How long a caller waits for a contended private lock before giving up.
///
/// Every cross-process section this module takes is bounded, because these
/// locks are held on a machine-wide data directory: a blocking `flock` here lets
/// any other usagi process — an MCP server, a CLI invocation, a rollover — stall
/// an interactive surface for as long as it likes, and a holder killed while
/// wedged would stall it forever. Each bound is sized against what its own
/// section can legitimately take.
#[derive(Clone, Copy)]
struct PrivateLockWait {
    limit: Duration,
    poll: Duration,
}

impl PrivateLockWait {
    const POLL: Duration = Duration::from_millis(20);

    /// A section that only reads or rewrites one small record file. The same
    /// two seconds the instance lock and the workspace fence already wait.
    const RECORD: Self = Self {
        limit: Duration::from_secs(2),
        poll: Self::POLL,
    };

    /// The bootstrap section, held across one `connect_or_start`. Its worst case
    /// is a cold start: spawning the lifecycle child, then
    /// [`bootstrap::READINESS_CEILING`] of endpoint polling. The budget is that
    /// ceiling plus a spawn margin, so a concurrent honest cold start is waited
    /// out while a wedged holder still returns a typed answer.
    const BOOTSTRAP: Self = Self {
        limit: bootstrap::READINESS_CEILING.saturating_add(Duration::from_secs(3)),
        poll: Self::POLL,
    };
}

fn lock_private_exclusive(
    path: &Path,
    label: &str,
    mode_policy: PrivateLockModePolicy,
    wait: PrivateLockWait,
) -> std::io::Result<std::fs::File> {
    let file = open_private_lock(path, label, mode_policy)?;
    let deadline = Instant::now() + wait.limit;
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(_) if Instant::now() < deadline => std::thread::sleep(wait.poll),
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!("{label} is held by another process"),
                ));
            }
        }
    }
    #[cfg(test)]
    wait_private_lock_after_flock_barrier(path);
    verify_private_lock_path(path, &file, label)?;
    Ok(file)
}

#[cfg(test)]
thread_local! {
    static FAIL_RECORD_WRITE_BEFORE_RENAME: RefCell<Option<PathBuf>> = const {
        RefCell::new(None)
    };
}

#[cfg(test)]
fn fail_record_write_before_rename(path: &Path) {
    FAIL_RECORD_WRITE_BEFORE_RENAME.with(|failpoint| {
        *failpoint.borrow_mut() = Some(path.to_path_buf());
    });
}

#[cfg(test)]
fn take_record_write_failpoint(path: &Path) -> bool {
    FAIL_RECORD_WRITE_BEFORE_RENAME.with(|failpoint| {
        if failpoint.borrow().as_deref() == Some(path) {
            failpoint.borrow_mut().take();
            true
        } else {
            false
        }
    })
}

impl FsRecordFile {
    fn transaction<T>(&self, operation: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
        let parent = self.path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "daemon record path has no parent",
            )
        })?;
        ensure_private_dir(parent)?;
        let _lock = lock_private_exclusive(
            &parent.join("record.lock"),
            "daemon record lock",
            PrivateLockModePolicy::CrashResidue,
            PrivateLockWait::RECORD,
        )?;
        operation()
    }

    fn parent(&self) -> std::io::Result<&Path> {
        self.path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "daemon record path has no parent",
            )
        })
    }

    fn sync_parent_best_effort(&self) {
        if let Ok(parent) = self.parent()
            && let Ok(directory) = std::fs::File::open(parent)
        {
            let _ = directory.sync_all();
        }
    }

    fn unique_temporary_path(&self) -> PathBuf {
        let mut temporary = self.path.as_os_str().to_owned();
        temporary.push(format!(
            ".tmp.{}.{}",
            std::process::id(),
            DAEMON_RECORD_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        PathBuf::from(temporary)
    }

    fn create_private_temporary(&self) -> std::io::Result<(PathBuf, std::fs::File)> {
        use std::os::unix::fs::OpenOptionsExt;

        loop {
            let temporary = self.unique_temporary_path();
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
                .open(&temporary)
            {
                Ok(file) => return Ok((temporary, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn write_unlocked(&self, contents: &str) -> std::io::Result<()> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let (temporary, mut file) = self.create_private_temporary()?;
        let result = (|| {
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.nlink() != 1
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "daemon record temporary is not a private owner file",
                ));
            }
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            if file.metadata()?.mode() & 0o777 != 0o600 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "daemon record temporary mode could not be made private",
                ));
            }
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
            drop(file);
            #[cfg(test)]
            if take_record_write_failpoint(&self.path) {
                return Err(std::io::Error::other(
                    "injected daemon record failure before rename",
                ));
            }
            std::fs::rename(&temporary, &self.path)?;
            // The rename has committed at this point. Directory fsync is not
            // supported on every filesystem, so do not turn a successful
            // replacement into an ambiguous error after the commit boundary.
            self.sync_parent_best_effort();
            Ok(())
        })();
        match result {
            Ok(()) => Ok(()),
            Err(error) => match std::fs::remove_file(&temporary) {
                Ok(()) => Err(error),
                Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(error),
                Err(cleanup) => Err(std::io::Error::new(
                    cleanup.kind(),
                    format!("{error}; daemon record temporary rollback failed: {cleanup}"),
                )),
            },
        }
    }

    fn read_unlocked(&self) -> std::io::Result<Option<String>> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "daemon record is not a private owner file",
            ));
        }
        if metadata.mode() & 0o777 != 0o600 {
            // Older usagi versions created daemon.json with the process umask.
            // Tighten an otherwise trusted owner file in place so upgrades keep
            // working while every subsequent read observes the 0600 invariant.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Ok(Some(contents))
    }
}

impl RecordFile for FsRecordFile {
    fn read(&self) -> std::io::Result<Option<String>> {
        self.transaction(|| self.read_unlocked())
    }

    fn write(&self, contents: &str) -> std::io::Result<()> {
        self.transaction(|| self.write_unlocked(contents))
    }

    fn remove_if(&self, expected: &str) -> std::io::Result<bool> {
        // A daemon whose data directory was deleted underneath it still runs the
        // ordinary shutdown path. There is no record left to clear, and
        // `transaction` would re-create the directory purely to take a lock, so
        // report the absent tree as a successful no-op.
        if self.parent().is_ok_and(|parent| !parent.exists()) {
            return Ok(false);
        }
        self.transaction(|| match self.read_unlocked()? {
            Some(current) if current == expected => match std::fs::remove_file(&self.path) {
                Ok(()) => {
                    // As with rename, unlink has already committed. Keep the
                    // API outcome unambiguous when directory fsync is unsupported.
                    self.sync_parent_best_effort();
                    Ok(true)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            },
            Some(_) | None => Ok(false),
        })
    }
}

struct ExactProcessControl;

impl ProcessIdentitySource for ExactProcessControl {
    fn process_start_identity(&self, pid: u32) -> std::io::Result<String> {
        process_start_identity(pid)
    }
}

impl LivenessProbe for ExactProcessControl {
    fn observe(&self, record: &DaemonRecord) -> DaemonProcessObservation {
        let Some(expected) = record
            .process_start_identity
            .as_deref()
            .filter(|identity| !identity.is_empty())
        else {
            return DaemonProcessObservation::Unknown;
        };
        match process_start_identity(record.pid) {
            Ok(actual) if actual == expected => DaemonProcessObservation::Exact,
            Ok(_) => DaemonProcessObservation::IdentityMismatch,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                DaemonProcessObservation::Gone
            }
            Err(_) => DaemonProcessObservation::Unknown,
        }
    }
}

struct SigtermTerminator;
impl Terminator for SigtermTerminator {
    fn terminate(&self, record: &DaemonRecord) -> std::io::Result<()> {
        // The record boundary already rejects a pid that cannot name a process,
        // so this is the last backstop rather than the fence: whatever route a
        // record took to get here, no `kill`-family call may be reached with a
        // value that would address a process group.
        if !usagi_core::domain::daemon::is_record_pid(record.pid) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                usagi_core::domain::daemon::InvalidRecordPid(record.pid),
            ));
        }
        signal_exact_process(record, libc::SIGTERM)
    }
}

#[cfg(target_os = "linux")]
fn process_start_identity(pid: u32) -> std::io::Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = stat.rfind(')').ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid /proc stat")
    })?;
    let start_time = stat[close + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "missing process start time",
            )
        })?;
    start_time
        .parse::<u64>()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(format!("linux:{start_time}"))
}

#[cfg(target_os = "macos")]
fn process_start_identity(pid: u32) -> std::io::Result<String> {
    let pid = libc::pid_t::try_from(pid).map_err(|_| std::io::Error::other("pid out of range"))?;
    // SAFETY: `info` is initialized and the buffer pointer/length describe the
    // exact `proc_bsdinfo` allocation for the duration of `proc_pidinfo`.
    let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    let size_arg = libc::c_int::try_from(size)
        .map_err(|_| std::io::Error::other("proc_bsdinfo size out of range"))?;
    // SAFETY: see the initialized buffer argument above.
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size_arg,
        )
    };
    if read == size_arg {
        Ok(format!(
            "macos:{}:{}",
            info.pbi_start_tvsec, info.pbi_start_tvusec
        ))
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "process does not exist",
            ))
        } else {
            Err(error)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process_start_identity(_pid: u32) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "process-start identity is unavailable on this platform",
    ))
}

#[cfg(target_os = "linux")]
/// Owns a `pidfd` returned by `pidfd_open` and closes it exactly once on drop.
struct PidFd(libc::c_int);

#[cfg(target_os = "linux")]
impl Drop for PidFd {
    fn drop(&mut self) {
        // SAFETY: this object exclusively owns the fd returned by pidfd_open and
        // drops it exactly once.
        unsafe {
            libc::close(self.0);
        }
    }
}

#[cfg(target_os = "linux")]
fn signal_exact_process(record: &DaemonRecord, signal: libc::c_int) -> std::io::Result<()> {
    let expected = record
        .process_start_identity
        .as_deref()
        .filter(|identity| !identity.is_empty())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "daemon process identity is unknown",
            )
        })?;
    let pid =
        libc::pid_t::try_from(record.pid).map_err(|_| std::io::Error::other("pid out of range"))?;
    // SAFETY: pidfd_open has no pointer arguments and returns an owned fd.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if pidfd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let pidfd = PidFd(
        libc::c_int::try_from(pidfd).map_err(|_| std::io::Error::other("pidfd out of range"))?,
    );
    if process_start_identity(record.pid)?.as_str() != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "daemon process identity mismatch",
        ));
    }
    // SAFETY: `pidfd` references the identity-verified process and null siginfo
    // plus zero flags are the documented pidfd_send_signal form.
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.0,
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn signal_exact_process(record: &DaemonRecord, signal: libc::c_int) -> std::io::Result<()> {
    let expected = record
        .process_start_identity
        .as_deref()
        .filter(|identity| !identity.is_empty())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "daemon process identity is unknown",
            )
        })?;
    if process_start_identity(record.pid)?.as_str() != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "daemon process identity mismatch",
        ));
    }
    let pid =
        libc::pid_t::try_from(record.pid).map_err(|_| std::io::Error::other("pid out of range"))?;
    // SAFETY: identity was re-read immediately above and `pid` is in range.
    if unsafe { libc::kill(pid, signal) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn signal_exact_process(_record: &DaemonRecord, _signal: libc::c_int) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "terminating a daemon is unsupported on this platform",
    ))
}

/// Root-bound IPC publication seam. `serve` invokes it only after the daemon
/// owns the singleton lock and has persisted its exact process-owner record. The guard makes a
/// future duplicate invocation a no-op instead of binding a second endpoint.
struct IpcReady<'a> {
    data_dir: &'a Path,
    /// The canonical workspace root resolved once at startup and fenced before
    /// publication, so the runtime this publishes owns exactly the workspace the
    /// fence guards.
    workspace_root: &'a Path,
    /// The single-instance lock this daemon holds. Publication reads the locked
    /// inode from it so the custody supervisor can prove, on every tick, that
    /// this process is still the singleton for `data_dir`.
    instance_lock: &'a FileInstanceLock,
    build: BuildIdentity,
    shutdown: Arc<ShutdownRequest>,
    published: AtomicBool,
    publication_attempted: AtomicBool,
    worker: RefCell<Option<std::thread::JoinHandle<SecureUnixListener>>>,
    listener: RefCell<Option<SecureUnixListener>>,
    cleanup: RefCell<Option<EndpointCleanup>>,
}
impl IpcReady<'_> {
    fn publish_with(
        &self,
        start: impl FnOnce(
            SecureUnixListener,
            usagi_core::infrastructure::ipc::DaemonGeneration,
        ) -> std::io::Result<std::thread::JoinHandle<SecureUnixListener>>,
    ) -> std::io::Result<()> {
        if self
            .published
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.publication_attempted.store(true, Ordering::Release);
            let (listener, generation) = match bind_ipc_listener(self.data_dir) {
                Ok(bound) => bound,
                Err(error) => {
                    self.published.store(false, Ordering::Release);
                    return Err(error);
                }
            };
            *self.cleanup.borrow_mut() = Some(listener.cleanup_handle());
            match start(listener, generation) {
                Ok(worker) => {
                    *self.worker.borrow_mut() = Some(worker);
                }
                Err(error) => {
                    self.published.store(false, Ordering::Release);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// The generation and endpoint this process bound, once it has bound one.
    ///
    /// It is read from the retained cleanup token rather than recomputed, so the
    /// durable registry entry, the published locator, and the socket that is
    /// actually accepting can only ever be the same generation.
    fn bound_endpoint(&self) -> Option<EndpointLocator> {
        self.cleanup
            .borrow()
            .as_ref()
            .map(|cleanup| cleanup.locator().clone())
    }

    /// Publish this generation's endpoint as `current`.
    ///
    /// The owner publishes through its own cleanup token, which re-verifies the
    /// socket's identity inside the locator lock — a locator naming a socket that
    /// was replaced between bind and publication is refused rather than written.
    fn publish_current(&self) -> std::io::Result<()> {
        self.cleanup.borrow().as_ref().map_or_else(
            || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "daemon endpoint is not bound",
                ))
            },
            EndpointCleanup::publish,
        )
    }

    /// Retires this daemon's published endpoint artifacts.
    ///
    /// A daemon that lost custody because its data directory was deleted has
    /// nothing left to retire, and every cleanup step would re-create that tree
    /// just to take a lock and prove absence. Treat the vanished directory as a
    /// successful no-op, so shutdown stays fail-closed for a live directory
    /// while never resurrecting a released one.
    fn retire_endpoint(&self) -> std::io::Result<()> {
        if !self.data_dir.exists() {
            return Ok(());
        }
        if let Some(cleanup) = self.cleanup.borrow().as_ref() {
            cleanup.retire()
        } else if self.publication_attempted.load(Ordering::Acquire) {
            // Binding itself can fail before returning a token. Scan only while
            // this serve process still owns daemon.lock, and require a complete
            // filesystem proof before permitting record cleanup.
            self.recover_stale_endpoint()
        } else {
            Ok(())
        }
    }
}

impl DaemonReady for IpcReady<'_> {
    fn recover_stale_endpoint(&self) -> std::io::Result<()> {
        retire_stale_current(self.data_dir)
    }

    fn publish(&self) -> std::io::Result<()> {
        let daemon_dir = self.data_dir.join("daemon");
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon_dir.join("daemon.json"),
        });
        let process = store.load()?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "daemon process record is unavailable for endpoint publication",
            )
        })?;
        // Both invariants the custody supervisor watches are established here:
        // the lock is held and the record names this process.
        let custody = FsCustodyProbe {
            locked: self.instance_lock.locked_inode(),
            lock_path: daemon_dir.join("daemon.lock"),
            record: FsRecordFile {
                path: daemon_dir.join("daemon.json"),
            },
        };
        self.publish_with(|listener, generation| {
            spawn_ipc_server(
                listener,
                &generation,
                self.data_dir,
                self.workspace_root,
                &self.build,
                process,
                custody,
                Arc::clone(&self.shutdown),
            )
        })
    }

    fn quiesce(&self) -> std::io::Result<()> {
        self.shutdown.request();
        let Some(worker) = self.worker.borrow_mut().take() else {
            return Ok(());
        };
        let listener = worker
            .join()
            .map_err(|_| std::io::Error::other("daemon IPC accept loop panicked"))?;
        *self.listener.borrow_mut() = Some(listener);
        Ok(())
    }

    fn retire(&self) -> std::io::Result<()> {
        let quiesce = self.quiesce();
        let cleanup = self.retire_endpoint();

        if cleanup.is_ok() {
            self.listener.borrow_mut().take();
            self.cleanup.borrow_mut().take();
            self.publication_attempted.store(false, Ordering::Release);
            self.published.store(false, Ordering::Release);
        }

        match (quiesce, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(quiesce), Err(cleanup)) => Err(std::io::Error::new(
                cleanup.kind(),
                format!("{quiesce}; endpoint cleanup also failed: {cleanup}"),
            )),
        }
    }
}

impl StaleDaemonCleanup for IpcReady<'_> {
    fn cleanup_if(
        &self,
        store: &dyn DaemonRecordPort,
        expected: &usagi_core::domain::daemon::DaemonRecord,
    ) -> std::io::Result<StaleCleanup> {
        if store.load()?.as_ref() != Some(expected) {
            return Ok(StaleCleanup::Superseded);
        }
        // This guard is intentionally scoped to this method. `restart` must
        // release daemon.lock before it launches the replacement serve process.
        let lock = FileInstanceLock {
            path: self.data_dir.join("daemon/daemon.lock"),
            held: RefCell::new(None),
        };
        if !lock.acquire()? {
            return match store.load()? {
                Some(current) if current == *expected => Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "daemon singleton lock is still held during stale cleanup",
                )),
                Some(_) | None => Ok(StaleCleanup::Superseded),
            };
        }
        if store.load()?.as_ref() != Some(expected) {
            return Ok(StaleCleanup::Superseded);
        }
        self.recover_stale_endpoint()?;
        if store.clear_if(expected)? {
            Ok(StaleCleanup::Cleared)
        } else {
            Ok(StaleCleanup::Superseded)
        }
    }
}

impl Drop for IpcReady<'_> {
    fn drop(&mut self) {
        let _ = DaemonReady::retire(self);
    }
}

/// The current locator, as the generation that owns the endpoint publishes it.
///
/// Publishing is not one operation with one implementation: the owner proves its
/// *own* socket inside the locator lock through the bind-time cleanup token,
/// while a recovering process that republishes on behalf of another generation
/// has to re-verify that generation's socket from the filesystem. Routing the two
/// cases here keeps [`claim_authority`] free of the distinction — it publishes a
/// [`PublishedLocator`], and the adapter knows which proof applies.
struct OwnedCurrentLocator<'a> {
    data_dir: &'a Path,
    ready: &'a IpcReady<'a>,
}

impl OwnedCurrentLocator<'_> {
    fn file(&self) -> CurrentLocatorFile {
        CurrentLocatorFile::new(self.data_dir)
    }
}

impl CurrentLocator for OwnedCurrentLocator<'_> {
    fn read(&self) -> std::io::Result<LocatorObservation> {
        self.file().read()
    }

    fn publish(&self, locator: &PublishedLocator) -> std::io::Result<()> {
        let owned = self
            .ready
            .bound_endpoint()
            .is_some_and(|bound| bound.generation.0 == locator.generation.as_str());
        if owned {
            self.ready.publish_current()
        } else {
            self.file().publish(locator)
        }
    }

    fn retire(&self) -> std::io::Result<()> {
        self.file().retire()
    }
}

/// This daemon's participation in the durable generation registry.
///
/// It is the composition of three durable objects the pure authority
/// ([`usagi_daemon::usecase::authority::activation`]) drives: the registry
/// document, the current locator, and the OS process table that says whether a
/// recorded authority is still alive.
///
/// The generation it claimed is remembered here rather than re-read on the way
/// out, because endpoint retirement drops the cleanup token that named it — and
/// the release must still be able to say *which* generation is giving up.
struct RegistryAuthority<'a> {
    data_dir: &'a Path,
    ready: &'a IpcReady<'a>,
    build: BuildIdentity,
    pid: u32,
    claimed: RefCell<Option<usagi_core::domain::id::DaemonGeneration>>,
}

impl RegistryAuthority<'_> {
    fn registry(&self) -> std::io::Result<GenerationRegistry<GenerationRegistryFile>> {
        Ok(GenerationRegistry::new(
            GenerationRegistryFile::new(self.data_dir)?,
            DEFAULT_GENERATION_LIMIT,
        ))
    }
}

impl GenerationAuthority for RegistryAuthority<'_> {
    fn claim(&self) -> std::io::Result<()> {
        let bound = self.ready.bound_endpoint().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "daemon endpoint must be bound before claiming generation authority",
            )
        })?;
        let generation = usagi_core::domain::id::DaemonGeneration::parse(&bound.generation.0)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "bound endpoint does not name a canonical daemon generation",
                )
            })?;
        let process = own_process_identity(self.pid)?;
        let claimed = claim_authority(
            &self.registry()?,
            &OwnedCurrentLocator {
                data_dir: self.data_dir,
                ready: self.ready,
            },
            &AuthorityClaim {
                generation,
                endpoint: &bound.endpoint,
                process: &process,
                build: &self.build,
            },
            &mut observe_generation_process,
        )?;
        // A start that had to reconcile something is a diagnosable event: an
        // abandoned handoff, a repaired locator, or an authority that had to be
        // failed closed all say a previous incarnation did not exit cleanly.
        if claimed.recovery != RecoveryOutcome::Consistent {
            ErrorLog::record(&format!(
                "daemon generation recovery before activation: {:?}",
                claimed.recovery
            ));
        }
        *self.claimed.borrow_mut() = Some(generation);
        Ok(())
    }

    fn release(&self) -> std::io::Result<()> {
        let Some(generation) = *self.claimed.borrow() else {
            return Ok(());
        };
        release_authority(&self.registry()?, generation).map_err(std::io::Error::other)
    }
}

/// This process's own OS-observed identity, as the registry records it.
///
/// Both fields come from the process table rather than from the PID: a recorded
/// authority is only ever re-verified by comparing them, and a PID alone cannot
/// tell a reused PID from the original process.
///
/// The start identity is deliberately the *daemon's own* token — the same one
/// `daemon.json` carries — rather than the child-probe spelling. One process must
/// not describe its start time two ways, or a comparison against the registry
/// would fail for a process that is plainly alive. Only the process group, which
/// the daemon record has no field for, is read through the child probe.
fn own_process_identity(pid: u32) -> std::io::Result<ProcessIdentity> {
    Ok(ProcessIdentity {
        pid,
        start_identity: process_start_identity(pid)?,
        process_group: ChildProcessProbe::process_group(&UnixChildProbe, pid)?,
    })
}

/// Whether a recorded generation process is still exactly the process recorded.
///
/// An identity that does not match is `Unknown` rather than `Gone`: the PID is
/// live, so nothing about the recorded owner has been proved either way. Only an
/// absent process is `Gone`, and only `Gone` lets recovery retire an authority.
fn observe_generation_process(process: &ProcessIdentity) -> ProcessObservation {
    if process.start_identity.is_empty() {
        return ProcessObservation::Unknown;
    }
    match process_start_identity(process.pid) {
        // The PID names a live process: either the recorded owner, or a different
        // incarnation that reused the PID — which proves nothing about the owner.
        Ok(identity) => {
            if identity == process.start_identity {
                ProcessObservation::VerifiedAlive(process.clone())
            } else {
                ProcessObservation::Unknown
            }
        }
        // Only an absent process is proof the owner is gone. An unreadable
        // process table is uncertainty, and uncertainty never retires anything.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProcessObservation::Gone,
        Err(_) => ProcessObservation::Unknown,
    }
}

/// Marks that signal delivery has been prepared. The blocking iterator now lives
/// in the signal thread, so the owner keeps only this proof.
struct SignalDelivery;

struct SignalShutdown {
    shutdown: Arc<ShutdownRequest>,
    signals: RefCell<Option<SignalDelivery>>,
    flag_ids: RefCell<Vec<signal_hook::SigId>>,
}

impl SignalShutdown {
    fn new(shutdown: Arc<ShutdownRequest>) -> Self {
        Self {
            shutdown,
            signals: RefCell::new(None),
            flag_ids: RefCell::new(Vec::new()),
        }
    }
}

impl Drop for SignalShutdown {
    fn drop(&mut self) {
        for id in self.flag_ids.get_mut().drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

impl ShutdownSignal for SignalShutdown {
    #[cfg(unix)]
    fn prepare(&self) -> std::io::Result<()> {
        let mut signals = self.signals.borrow_mut();
        if signals.is_none() {
            let mut flag_ids = Vec::with_capacity(2);
            for signal in [libc::SIGINT, libc::SIGTERM] {
                match signal_hook::flag::register(signal, self.shutdown.flag()) {
                    Ok(id) => flag_ids.push(id),
                    Err(error) => {
                        for id in flag_ids {
                            signal_hook::low_level::unregister(id);
                        }
                        return Err(error);
                    }
                }
            }
            let mut prepared =
                match signal_hook::iterator::Signals::new([libc::SIGINT, libc::SIGTERM]) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        for id in flag_ids {
                            signal_hook::low_level::unregister(id);
                        }
                        return Err(error);
                    }
                };
            // `signal_hook::flag::register` above writes the flag straight from
            // the handler, which is async-signal-safe but cannot wake a condvar.
            // This thread does the waking: it blocks on signal-hook's own pipe
            // (no timer) and converts the first delivery into one request. It is
            // started here, before any worker is spawned, so the documented
            // ordering of shutdown delivery is unchanged.
            let requested = Arc::clone(&self.shutdown);
            let handle = std::thread::Builder::new()
                .name("usagi-daemon-signal".to_string())
                .spawn(move || {
                    if prepared.forever().next().is_some() {
                        requested.request();
                    }
                });
            if let Err(error) = handle {
                for id in flag_ids {
                    signal_hook::low_level::unregister(id);
                }
                return Err(error);
            }
            *self.flag_ids.borrow_mut() = flag_ids;
            *signals = Some(SignalDelivery);
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn prepare(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "running the daemon is only supported on Unix",
        ))
    }

    #[cfg(unix)]
    fn wait(&self) -> std::io::Result<()> {
        if self.signals.borrow().is_none() {
            return Err(std::io::Error::other(
                "daemon shutdown delivery was not prepared",
            ));
        }
        // Both delivery paths converge on one request, so this parks instead of
        // polling: `prepare` runs a thread that turns a delivered signal into a
        // request, and the accept-worker exit guard requests directly. A worker
        // panic therefore still releases an owner that would otherwise hold
        // daemon.lock and a stale lifecycle record.
        self.shutdown.wait_until_requested();
        Ok(())
    }
    #[cfg(not(unix))]
    fn wait(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "running the daemon is only supported on Unix",
        ))
    }
}

struct ServeLauncher {
    exe: PathBuf,
}
impl DaemonLauncher for ServeLauncher {
    fn launch(&self) -> std::io::Result<()> {
        let mut command = std::process::Command::new(&self.exe);
        command
            .args(["daemon", "serve"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command.spawn()?;
        Ok(())
    }
}

struct RealSleeper;
impl Sleeper for RealSleeper {
    fn sleep(&self) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct FileInstanceLock {
    path: PathBuf,
    held: RefCell<Option<std::fs::File>>,
}
impl FileInstanceLock {
    /// Identity of the inode this process locked, read from the held descriptor
    /// rather than the pathname, so a later replacement of the pathname cannot
    /// forge the identity custody supervision compares against.
    ///
    /// `None` means this lock was never acquired (or its descriptor cannot be
    /// inspected), which leaves custody undecidable instead of lost.
    fn locked_inode(&self) -> Option<NodeIdentity> {
        let held = self.held.borrow();
        let metadata = held.as_ref()?.metadata().ok()?;
        Some(node_identity(&metadata))
    }
}
/// The workspace-scoped fence: an exclusive `flock` on
/// `<workspace>/.usagi/daemon/daemon.lock`.
///
/// The node is outside every runtime-mode child directory, so `production`,
/// `development`, and `local` — and any `$USAGI_HOME` — converge on one inode.
/// Path spelling cannot split it either, because `flock` excludes per inode and
/// the workspace root is canonicalized before it is spelled.
///
/// After acquiring, the owner writes its pid line into the node. That line is the
/// only cross-mode discovery channel there is: a refused daemon reads a different
/// data directory's `daemon.json` than the owner writes, so without the hint it
/// could not name the process holding the workspace.
struct FileWorkspaceFence {
    path: PathBuf,
    workspace: PathBuf,
    pid: u32,
    held: RefCell<Option<std::fs::File>>,
}

impl WorkspaceFence for FileWorkspaceFence {
    fn acquire(&self) -> std::io::Result<WorkspaceFenceOutcome> {
        const TIMEOUT: Duration = Duration::from_secs(2);
        const POLL: Duration = Duration::from_millis(20);
        // `<workspace>/.usagi` is user-visible project metadata, so it keeps
        // ordinary directory permissions; only the `daemon/` child holding the
        // fence is private, which `open_private_lock` establishes.
        std::fs::create_dir_all(self.workspace.join(paths::STATE_DIR))?;
        let file = open_private_lock(
            &self.path,
            "daemon workspace fence",
            PrivateLockModePolicy::CrashResidue,
        )?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => {
                    #[cfg(test)]
                    wait_private_lock_after_flock_barrier(&self.path);
                    verify_private_lock_path(&self.path, &file, "daemon workspace fence")?;
                    // Publish the owner hint immediately, so the window in which a
                    // refused start could read the previous owner's line is only
                    // as long as this write.
                    write_owner_hint(&file, self.pid)?;
                    *self.held.borrow_mut() = Some(file);
                    return Ok(WorkspaceFenceOutcome::Acquired);
                }
                Err(_) if Instant::now() < deadline => std::thread::sleep(POLL),
                Err(_) => {
                    return Ok(WorkspaceFenceOutcome::Held {
                        workspace: self.workspace.display().to_string(),
                        // The hint is diagnostic only. An empty or garbled line
                        // (a holder killed before publishing) yields no pid, and a
                        // holder mid-write can still show the departed owner's.
                        // Neither changes the refusal, which the `flock` decides.
                        owner: read_owner_hint(&file),
                    });
                }
            }
        }
    }
}

/// Replace the fence node's contents with this owner's pid line.
fn write_owner_hint(file: &std::fs::File, pid: u32) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};
    file.set_len(0)?;
    let mut file = file;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(format!("{pid}\n").as_bytes())?;
    file.flush()
}

/// Read the owner pid published by the daemon currently holding the fence.
fn read_owner_hint(file: &std::fs::File) -> Option<u32> {
    use std::io::Read;
    // The hint is one short decimal line; a longer node is not ours to trust.
    let mut contents = String::new();
    file.take(64).read_to_string(&mut contents).ok()?;
    contents.trim().parse().ok()
}

impl InstanceLock for FileInstanceLock {
    fn acquire(&self) -> std::io::Result<bool> {
        const TIMEOUT: Duration = Duration::from_secs(2);
        const POLL: Duration = Duration::from_millis(20);
        if let Some(parent) = self.path.parent() {
            ensure_private_dir(parent)?;
        }
        let file = open_private_lock(
            &self.path,
            "daemon instance lock",
            PrivateLockModePolicy::CrashResidue,
        )?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => {
                    #[cfg(test)]
                    wait_private_lock_after_flock_barrier(&self.path);
                    verify_private_lock_path(&self.path, &file, "daemon instance lock")?;
                    *self.held.borrow_mut() = Some(file);
                    return Ok(true);
                }
                Err(_) if Instant::now() < deadline => std::thread::sleep(POLL),
                Err(_) => return Ok(false),
            }
        }
    }
}

/// `usagi daemon` の実行時資源を組み立てて daemon presentation へ渡す。
pub(crate) fn run(
    out: &mut dyn Write,
    command: CliDaemonCommand,
    info: &AppInfo,
    operation: Option<usagi_core::infrastructure::ipc::OperationId>,
) -> std::io::Result<()> {
    install_panic_logger();
    match panic::catch_unwind(AssertUnwindSafe(|| {
        run_inner(out, command, info, operation)
    })) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            ErrorLog::record(&format!("daemon failed: {error}"));
            Err(error)
        }
        // `install_panic_logger` has already recorded the payload, location,
        // and backtrace. Convert the unwind to an ordinary process error so
        // callers do not continue after a failed daemon startup or serve loop.
        Err(_) => Err(std::io::Error::other(
            "daemon panicked; see the error log for details",
        )),
    }
}

/// Resolve and securely initialize the selected per-user data directory before
/// any global store can become its first writer.
///
/// Config intentionally runs without starting the daemon, so its settings
/// adapter cannot rely on bootstrap lock acquisition to establish the private
/// directory invariant first.
pub(crate) fn prepare_private_data_dir() -> std::io::Result<PathBuf> {
    let data_dir =
        paths::data_dir().map_err(|error| std::io::Error::other(format!("{error:#}")))?;
    ensure_private_dir_all(&data_dir)?;
    Ok(data_dir)
}

/// Install one process-wide panic hook for the daemon. A daemon owns several
/// worker threads, so a boundary around its main thread alone cannot observe a
/// panic in an IPC, PTY, or observer worker. The hook records every thread's
/// panic before the thread unwinds; [`run`] then catches a main-thread panic at
/// the outer daemon boundary and terminates the process with an ordinary error.
fn install_panic_logger() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        ErrorLog::record(&format_panic(info));
        previous(info);
    }));
}
fn format_panic(info: &PanicHookInfo<'_>) -> String {
    let payload = if let Some(message) = info.payload().downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = info.payload().downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    };
    let location = info
        .location()
        .map_or_else(|| "unknown location".to_owned(), ToString::to_string);
    format!(
        "daemon panicked: {payload}\nlocation: {location}\nbacktrace:\n{}",
        Backtrace::force_capture()
    )
}
fn run_inner(
    out: &mut dyn Write,
    command: CliDaemonCommand,
    info: &AppInfo,
    operation: Option<usagi_core::infrastructure::ipc::OperationId>,
) -> std::io::Result<()> {
    let data_dir = prepare_private_data_dir()?;
    let daemon_dir = data_dir.join("daemon");
    let command = match command {
        CliDaemonCommand::InstallService => {
            let path = launchd::install(&std::env::current_exe()?, &data_dir)?;
            return writeln!(
                out,
                "{}: launchd service installed ({})",
                info.describe(),
                path.display()
            );
        }
        CliDaemonCommand::UninstallService => {
            let path = launchd::uninstall()?;
            return writeln!(
                out,
                "{}: launchd service uninstalled ({})",
                info.describe(),
                path.display()
            );
        }
        CliDaemonCommand::Serve => PresentationDaemonCommand::Serve,
        CliDaemonCommand::Start => PresentationDaemonCommand::Start,
        CliDaemonCommand::Status => PresentationDaemonCommand::Status,
        CliDaemonCommand::Stop { force } => PresentationDaemonCommand::Stop(transition_mode(force)),
        // A manual restart is a forced replacement of the artifact that is
        // already running, so it carries exactly the operation id the build
        // trigger derives for that case. A repeated restart converges on it.
        CliDaemonCommand::Restart { force } => PresentationDaemonCommand::Replace {
            operation: operation
                .or_else(|| manual_operation_id(&current_build(), runtime_channel())),
            mode: transition_mode(force),
        },
        CliDaemonCommand::Replace { .. } => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "daemon replace must be routed through the client trigger",
            ));
        }
    };
    ensure_private_dir(&daemon_dir)?;
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon_dir.join("daemon.json"),
    });
    let launcher = ServeLauncher {
        exe: std::env::current_exe()?,
    };
    let lock = FileInstanceLock {
        path: daemon_dir.join("daemon.lock"),
        held: RefCell::new(None),
    };
    // One resolution of the workspace identity for the whole process: the fence
    // that guards the workspace and the runtime that owns it must key on the same
    // path, or a daemon could fence one workspace and then take authority over
    // another.
    let workspace_root = bound_workspace_root(&daemon_dir, std::env::current_dir()?)?;
    let pid = std::process::id();
    let workspace = FileWorkspaceFence {
        path: paths::workspace_fence_path(&workspace_root),
        workspace: workspace_root.clone(),
        pid,
        held: RefCell::new(None),
    };
    let ready = IpcReady {
        data_dir: &data_dir,
        workspace_root: &workspace_root,
        instance_lock: &lock,
        // The daemon advertises the exact artifact it started as for its whole
        // process lifetime. Atomic replacement of the executable path cannot
        // mutate this startup snapshot.
        build: current_build(),
        shutdown: Arc::new(ShutdownRequest::new()),
        published: AtomicBool::new(false),
        publication_attempted: AtomicBool::new(false),
        worker: RefCell::new(None),
        listener: RefCell::new(None),
        cleanup: RefCell::new(None),
    };
    let shutdown = SignalShutdown::new(Arc::clone(&ready.shutdown));
    let census = DurableResourceCensus {
        daemon_dir: daemon_dir.clone(),
    };
    let authority = RegistryAuthority {
        data_dir: &data_dir,
        ready: &ready,
        build: current_build(),
        pid,
        claimed: RefCell::new(None),
    };
    let env = DaemonEnv {
        store: &store,
        probe: &ExactProcessControl,
        terminator: &SigtermTerminator,
        ready: &ready,
        authority: &authority,
        shutdown: &shutdown,
        launcher: &launcher,
        sleeper: &RealSleeper,
        lock: &lock,
        workspace: &workspace,
        pid,
        census: &census,
        seamless: observed_seamless_refusal(&data_dir),
    };
    usagi_daemon::presentation::run(out, command, info, &env)
}

/// Resolve the canonical workspace root this daemon would bind, before anything
/// locks or publishes.
///
/// The candidate is the startup working directory — the same value the session
/// runtime takes — but a durable `repository_root` from a previous start wins, so
/// starting from a subdirectory cannot fence a workspace the runtime will not
/// own. Canonicalization then collapses spelling differences.
fn bound_workspace_root(daemon_dir: &Path, candidate: PathBuf) -> std::io::Result<PathBuf> {
    let bound = SessionRuntime::bound_workspace_root(daemon_dir, candidate)
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    paths::canonical_workspace_root(&bound)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))
}

/// The workspace this process opened, once a surface has selected one.
///
/// A TUI can open a workspace that is not the directory it was started from, and
/// `usagi hop` opens several in sequence within one process, so the selection is
/// process state rather than a start-up constant. It is the most accurate answer
/// to "whose resources will this connection touch", so it outranks both the
/// injected root and the working directory in [`client_workspace`].
static OPENED_WORKSPACE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Record the workspace a surface is opening and return its canonical root.
///
/// Every daemon connection this process makes afterwards declares that root, so
/// the daemon can refuse to answer with another workspace's sessions, and a cold
/// start puts the new daemon in the workspace being opened
/// ([`run_lifecycle`]). A root that cannot be canonicalized is reported here
/// instead of being declared as spelled: the surface has an explicit path to
/// complain about, unlike an ambient working directory.
///
/// A root that has no wire spelling (a path that is not UTF-8) is reported too,
/// before a connection or a daemon start is attempted. No daemon can serve such a
/// workspace: its own durable authority record (`sessions.json`) and the
/// workspace registry are JSON, so the root cannot even be written down. Opening
/// it would therefore either be refused by the fence for a root nothing can
/// compare, or — worse, and what used to happen — be answered by a daemon that
/// owns a different workspace.
pub(crate) fn declare_opened_workspace(root: &Path) -> std::io::Result<PathBuf> {
    let canonical = paths::canonical_workspace_root(root)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
    if paths::wire_workspace_root(&canonical).is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "workspace path is not valid UTF-8: {}; usagi cannot serve a workspace it cannot name",
                canonical.display()
            ),
        ));
    }
    *OPENED_WORKSPACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(canonical.clone());
    Ok(canonical)
}

fn opened_workspace() -> Option<PathBuf> {
    OPENED_WORKSPACE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The workspace a client process declares in its handshake.
///
/// An opened workspace wins: that is the workspace whose sessions, scopes, and
/// PR inventory the surface is about to display, and the daemon must serve
/// exactly it. Otherwise the daemon-injected trusted root wins, so a provisioned
/// MCP child declares the daemon's own workspace instead of whatever directory
/// the provider left it in. Every remaining surface declares its canonical
/// working directory: the daemon admits that directory when it is the trusted
/// root or below it, which covers subdirectories and session worktrees without
/// running Git per client start. A directory that cannot be canonicalized is
/// declared as spelled, so the daemon refuses it rather than this client
/// guessing that it matches.
fn declared_client_workspace(
    opened: Option<PathBuf>,
    injected: Option<std::ffi::OsString>,
    cwd: std::io::Result<PathBuf>,
) -> ClientWorkspace {
    if let Some(opened) = opened {
        return ClientWorkspace::Selected {
            root: paths::wire_workspace_root(opened),
        };
    }
    let candidate = injected
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| cwd.ok());
    let root = candidate.map_or_else(String::new, |path| {
        paths::wire_workspace_root(paths::canonical_workspace_root(&path).unwrap_or(path))
    });
    ClientWorkspace::Bound { root }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=the_declared_workspace_prefers_the_opened_one_then_the_injected_root
fn client_workspace() -> ClientWorkspace {
    declared_client_workspace(
        opened_workspace(),
        std::env::var_os(paths::WORKSPACE_ROOT_ENV),
        std::env::current_dir(),
    )
}

/// Connect to the daemon for this binary's isolated runtime channel. Every
/// channel reuses an exact artifact. A different known artifact returns one
/// deterministic rollover trigger. Development consumes it with a cold
/// restart; other channels preserve the old daemon for a future safe handoff.
///
/// The returned lane is deadline-armed by construction: there is no way to
/// obtain an unbounded daemon socket from this module. `connect_budget_ms`
/// bounds bootstrap, connect and handshake; each later request re-arms the lane
/// with its own budget through [`rearm_lane`].
pub(crate) fn client(
    policy: ClientPolicy,
    connect_budget_ms: u64,
) -> Result<LaneClient, ClientError> {
    client_for(policy, &client_workspace(), connect_budget_ms)
}

fn client_for(
    policy: ClientPolicy,
    workspace: &ClientWorkspace,
    connect_budget_ms: u64,
) -> Result<LaneClient, ClientError> {
    let clock = SystemClock::new();
    bootstrap_client(|data_dir, build| {
        connect_client(
            data_dir,
            policy,
            build.clone(),
            workspace.clone(),
            |stream| deadline_transport(clock, stream, connect_budget_ms),
        )
    })
}

/// Wraps an established socket in this process's deadline transport.
pub(crate) fn deadline_transport(
    clock: SystemClock,
    stream: std::os::unix::net::UnixStream,
    budget_ms: u64,
) -> LaneStream {
    DeadlineStream::new(clock, DeadlineUnixStream(stream), budget_ms)
}

/// Restarts a lane's end-to-end budget for the request that is about to be
/// sent. A lane keeps one connection across requests (its attachments and input
/// ledger live there), so the budget is per request rather than per connection.
pub(crate) fn rearm_lane(client: &mut LaneClient, budget_ms: u64) {
    usagi_core::usecase::client::DaemonSession::rearm(client, budget_ms);
}

/// Borrows a lane's underlying socket, for composition-owned passive
/// observation (the restore watcher clones it to peek for EOF).
pub(crate) fn lane_socket(client: &LaneClient) -> &std::os::unix::net::UnixStream {
    &client.transport().get_ref().0
}

/// Establishes a bootstrapped, build-fenced daemon connection. `connect` builds
/// one authenticated session over any stream type — the exact-owner-verified
/// [`LaneClient`] the terminal lanes use, or the per-request one
/// [`policy_client`] builds — so every surface shares the identical cold-start,
/// stale-recovery, and development rollover handling. Both are deadline-armed:
/// no caller can ask this for an unbounded socket.
///
/// Entering the bootstrap section is itself bounded
/// ([`acquire_bootstrap_lock`]), so a peer that is holding it cannot stall this
/// caller indefinitely.
// LLVM counts the deadline-stream instantiation as uncovered for branches the
// UnixStream instantiation already exercises through the integration suite.
#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=cli_tui_pty
fn bootstrap_client<S: Read + Write>(
    connect: impl Fn(&Path, &BuildIdentity) -> std::io::Result<IpcClient<S>>,
) -> Result<IpcClient<S>, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let exe =
        std::env::current_exe().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let _bootstrap_lock = acquire_bootstrap_lock(&data_dir)?;
    let expected_build = current_build();
    let channel = runtime_channel();
    let connection = bootstrap::connect_or_start(
        || connect(&data_dir, &expected_build),
        || run_lifecycle(&exe, "start"),
        || recover_stale_client_endpoint(&data_dir),
        &expected_build,
        channel,
        false,
        IpcClient::server_build,
    );
    let connection = match connection {
        Err(bootstrap::BootstrapError::RolloverRequired(_))
            if paths::runtime_mode() == paths::RuntimeMode::Development =>
        {
            bootstrap::restart_and_connect(
                || connect(&data_dir, &expected_build),
                // Development has already chosen a destructive replacement of a
                // different build: the cold transition must not be refused by
                // the live-runtime guard it deliberately overrides (#507).
                || run_lifecycle_with(&exe, &["daemon", "restart", "--force"], "restart"),
                &expected_build,
                IpcClient::server_build,
            )
        }
        other => other,
    };
    connection.map_err(|error| match error {
        bootstrap::BootstrapError::RolloverRequired(trigger) => {
            ClientError::RolloverRequired(trigger)
        }
        bootstrap::BootstrapError::UnknownBuildIdentity => ClientError::BuildIdentityUnavailable,
        // Keep the daemon's typed refusal (code, error id, message) so every
        // surface renders "this is another workspace's daemon" instead of an
        // unavailable transport.
        bootstrap::BootstrapError::WorkspaceMismatch(refusal) => ClientError::Protocol(refusal),
        other => ClientError::Lifecycle(other.to_string()),
    })
}

/// The real process monotonic clock. Only differences between observations are
/// meaningful; the origin is captured once so a wall-clock jump cannot rewind a
/// deadline.
#[derive(Clone, Copy)]
pub(crate) struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=mcp_e2e
    pub(crate) fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl MonotonicClock for SystemClock {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=mcp_e2e
    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// A deadline-armed Unix domain socket. Arming maps to OS receive/send timeouts
/// so a stalled daemon cannot block a surface past its policy budget.
pub(crate) struct DeadlineUnixStream(std::os::unix::net::UnixStream);

impl Read for DeadlineUnixStream {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=mcp_e2e
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for DeadlineUnixStream {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=mcp_e2e
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=mcp_e2e
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl DeadlineConnection for DeadlineUnixStream {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=mcp_e2e
    fn set_read_deadline(&mut self, timeout: std::time::Duration) -> std::io::Result<()> {
        self.0.set_read_timeout(Some(timeout))
    }
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=mcp_e2e
    fn set_write_deadline(&mut self, timeout: std::time::Duration) -> std::io::Result<()> {
        self.0.set_write_timeout(Some(timeout))
    }
}

/// The only daemon byte stream this composition root builds: an OS socket that
/// always carries an armed end-to-end deadline.
pub(crate) type LaneStream = DeadlineStream<SystemClock, DeadlineUnixStream>;
/// A daemon client over [`LaneStream`]. Every surface — per-request, terminal
/// lane, poll pump, inventory pump — is this one type, so an unbounded socket
/// cannot be introduced without changing the type.
pub(crate) type LaneClient = IpcClient<LaneStream>;

/// This process's client incarnation, declared by every connection it opens.
///
/// It is a canonical resource identity rather than a PID: PIDs are reused, and
/// the daemon keys durable per-client state on this value, so a reused PID would
/// let a new process inherit another one's terminal input operations (#519). It
/// is minted once per process and shared by every lane (per-request, terminal
/// stream, poll pump), which is what makes an operation issued before a reconnect
/// still resolvable afterwards.
fn client_incarnation() -> &'static str {
    static INCARNATION: OnceLock<String> = OnceLock::new();
    INCARNATION.get_or_init(|| usagi_core::domain::id::ClientId::new().as_str())
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=mcp_e2e
fn connect_deadline_client(
    data_dir: &Path,
    policy: ClientPolicy,
    build: BuildIdentity,
    workspace: ClientWorkspace,
    clock: SystemClock,
    budget_ms: u64,
) -> std::io::Result<LaneClient> {
    let stream = usagi_daemon::infrastructure::unix_transport::connect_current(data_dir)?;
    let deadline = deadline_transport(clock, stream, budget_ms);
    IpcClient::connect(
        deadline,
        client_incarnation().to_owned(),
        format!("{}", std::process::id()),
        policy,
        build,
        workspace,
    )
    .map_err(std::io::Error::other)
}

/// A resilient daemon client that enforces the surface [`ClientPolicy`] end to
/// end: each attempt consumes one monotonic deadline budget (connect/handshake,
/// write, response read) and `reconnect_attempts` bounds retries gated by the
/// request's retry eligibility. CLI, MCP, and the TUI's per-request calls use
/// this so a hung daemon cannot block a surface indefinitely.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=mcp_e2e
pub(crate) fn policy_client(policy: ClientPolicy) -> Result<impl DaemonClient, ClientError> {
    let clock = SystemClock::new();
    let workspace = client_workspace();
    let initial = bootstrap_client(|data_dir, build| {
        connect_deadline_client(
            data_dir,
            policy,
            build.clone(),
            workspace.clone(),
            clock,
            policy.timeout_ms,
        )
    })?;
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let build = current_build();
    // Reconnects target the already-running daemon; the initial bootstrap above
    // owns cold-start and rollover, so a plain connect that fails simply exhausts
    // the budget as a typed unavailable rather than churning the daemon.
    let reconnect = move |clock: SystemClock, budget_ms: u64| {
        connect_deadline_client(
            &data_dir,
            policy,
            build.clone(),
            workspace.clone(),
            clock,
            budget_ms,
        )
        .map_err(|error| ClientError::Unavailable(error.to_string()))
    };
    Ok(PolicyClient::new(clock, policy, reconnect, Some(initial)))
}

/// A daemon client for display-only observation: the TUI's metrics subscription.
///
/// It declares no workspace (the samples are process diagnostics, not workspace
/// state) and it never bootstraps, so an entry screen that has not chosen a
/// workspace yet cannot cold-start a daemon bound to whatever directory the TUI
/// happens to have been launched from. Without a running daemon there are simply
/// no metrics.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=cli_tui_pty
pub(crate) fn observation_client(policy: ClientPolicy) -> Result<impl DaemonClient, ClientError> {
    let clock = SystemClock::new();
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let build = current_build();
    let connect = move |clock: SystemClock, budget_ms: u64| {
        connect_deadline_client(
            &data_dir,
            policy,
            build.clone(),
            ClientWorkspace::Unbound,
            clock,
            budget_ms,
        )
        .map_err(|error| ClientError::Unavailable(error.to_string()))
    };
    let initial = connect(clock, policy.timeout_ms)?;
    Ok(PolicyClient::new(clock, policy, connect, Some(initial)))
}

/// A workspace-bound daemon client for a background observation lane.
///
/// It is [`policy_client`] without the bootstrap: same declared workspace, same
/// end-to-end deadline and reconnect budget, but it only connects to a daemon
/// that is already running. That is what makes it safe to hold resident on a
/// pump thread — a lane that observes every few hundred milliseconds must never
/// take the shared `bootstrap.lock`, spawn a lifecycle subprocess, or sleep out
/// a readiness wait, because doing so at that cadence serialises every other
/// client on this machine (#551).
///
/// Cold-start authority therefore stays with the surfaces that act on the user's
/// behalf: workspace entry, and the session-lifecycle lane that may retry it a
/// bounded number of times. Without a running daemon an observation lane simply
/// reports the failure and backs off.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=cli_tui_pty
pub(crate) fn attached_client(policy: ClientPolicy) -> Result<impl DaemonClient, ClientError> {
    let clock = SystemClock::new();
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let build = current_build();
    let workspace = client_workspace();
    let connect = move |clock: SystemClock, budget_ms: u64| {
        connect_deadline_client(
            &data_dir,
            policy,
            build.clone(),
            workspace.clone(),
            clock,
            budget_ms,
        )
        .map_err(|error| ClientError::Unavailable(error.to_string()))
    };
    let initial = connect(clock, policy.timeout_ms)?;
    Ok(PolicyClient::new(clock, policy, connect, Some(initial)))
}

/// Requests and performs an intentional replacement of the running daemon
/// artifact.
///
/// The trigger is derived first, effect free, from the two advertised artifact
/// identities; the replacement it keys is then carried out on exactly the path
/// `usagi daemon restart` takes, so a build/update swap can never reach a
/// `stop` → fresh `start` the manual verb is guarded against.
pub(crate) fn replace_running_daemon(
    out: &mut dyn Write,
    policy: ClientPolicy,
    force: bool,
    info: &AppInfo,
) -> std::io::Result<Result<(), ClientError>> {
    let trigger = match request_replacement(policy) {
        Ok(trigger) => trigger,
        Err(error) => return Ok(Err(error)),
    };
    run(
        out,
        CliDaemonCommand::Restart { force },
        info,
        Some(trigger.operation_id),
    )
    .map(Ok)
}

/// Requests intentional replacement of the currently running daemon artifact.
/// This only creates the deterministic trigger; it never sends a stop signal or
/// spawns a second daemon. [`replace_running_daemon`] consumes it.
fn request_replacement(policy: ClientPolicy) -> Result<BuildRolloverTrigger, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let _bootstrap_lock = acquire_bootstrap_lock(&data_dir)?;
    let expected_build = current_build();
    // Replacing the running artifact is a lifecycle observation, not workspace
    // work: it reads the daemon's advertised build and sends no request, so it
    // stays usable from outside the daemon's workspace.
    let clock = SystemClock::new();
    let client = connect_client(
        &data_dir,
        policy,
        expected_build.clone(),
        ClientWorkspace::Unbound,
        |stream| deadline_transport(clock, stream, policy.timeout_ms),
    )
    .map_err(|_| ClientError::Unavailable("daemon endpoint is unavailable".into()))?;
    let actual_build = client.server_build();
    match build_artifact_decision(actual_build, &expected_build, true) {
        BuildArtifactDecision::ForceReplace | BuildArtifactDecision::RolloverTrigger => {
            build_rollover_trigger(actual_build, &expected_build, runtime_channel(), true)
                .ok_or(ClientError::BuildIdentityUnavailable)
        }
        BuildArtifactDecision::Unknown => Err(ClientError::BuildIdentityUnavailable),
        BuildArtifactDecision::Reuse => Err(ClientError::Lifecycle(
            "daemon replacement trigger could not be created".into(),
        )),
    }
}

fn runtime_channel() -> &'static str {
    match paths::runtime_mode() {
        paths::RuntimeMode::Production => "production",
        paths::RuntimeMode::Development => "development",
        paths::RuntimeMode::Local => "local",
    }
}

/// Reclaims an unreachable endpoint only after proving that no daemon owns the
/// lifecycle singleton and that the exact durable record has not changed.
///
/// The caller holds `bootstrap.lock`, so only one ordinary client may cross
/// this recovery/start boundary. `daemon.lock` is the authoritative process
/// ownership proof: unlike a raw PID probe it remains safe when a PID has been
/// reused, and this path never signals a process. The record's exact identity
/// fields are part of the whole-record equality fence below.
///
/// The reclaim verdict comes from the domain
/// [`classify`](usagi_core::domain::daemon::classify), the same decision the
/// `stop` / `start` / `restart` lifecycle commands make, so one observation can
/// never mean "reclaimable" here and "refuse" there.
fn recover_stale_client_endpoint(data_dir: &Path) -> std::io::Result<bootstrap::StaleRecovery> {
    recover_stale_client_endpoint_with(data_dir, InstanceLock::acquire, || {})
}

fn recover_stale_client_endpoint_with(
    data_dir: &Path,
    acquire: impl FnOnce(&FileInstanceLock) -> std::io::Result<bool>,
    after_lock: impl FnOnce(),
) -> std::io::Result<bootstrap::StaleRecovery> {
    let daemon_dir = data_dir.join("daemon");
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon_dir.join("daemon.json"),
    });
    let Some(expected) = store.load()? else {
        return Ok(bootstrap::StaleRecovery::NotProven);
    };
    let lock = FileInstanceLock {
        path: daemon_dir.join("daemon.lock"),
        held: RefCell::new(None),
    };
    if !acquire(&lock)? {
        // A live or starting owner still holds the authoritative singleton.
        // Preserve every artifact and let bootstrap perform bounded reconnects
        // instead of launching a competing daemon.
        return Ok(bootstrap::StaleRecovery::OwnerActive);
    }
    after_lock();
    if store.load()?.as_ref() != Some(&expected) {
        return Ok(bootstrap::StaleRecovery::NotProven);
    }
    match usagi_core::domain::daemon::classify(
        Some(&expected),
        ExactProcessControl.observe(&expected),
    ) {
        // Owner gone or its pid reused: the OS has proved the recorded
        // incarnation no longer exists, so its endpoint is reclaimable.
        usagi_core::domain::daemon::DaemonState::Stale(_) => {}
        usagi_core::domain::daemon::DaemonState::Alive => {
            return Ok(bootstrap::StaleRecovery::OwnerActive);
        }
        // Ownership undecided, or the record vanished under us: preserve every
        // artifact.
        usagi_core::domain::daemon::DaemonState::Unverified
        | usagi_core::domain::daemon::DaemonState::Absent => {
            return Ok(bootstrap::StaleRecovery::NotProven);
        }
    }

    // Socket-first retirement and current.lock provide the endpoint commit
    // fence. The record remains present on every cleanup error.
    retire_stale_current(data_dir)?;
    if store.clear_if(&expected)? {
        Ok(bootstrap::StaleRecovery::Recovered)
    } else {
        Ok(bootstrap::StaleRecovery::NotProven)
    }
}

fn current_build() -> BuildIdentity {
    // The artifact identity is a compile-time constant baked in by `build.rs`
    // from this binary's source/tree, profile, and target. It is therefore
    // immutable for the process lifetime and never re-read from disk, so an
    // atomic replacement of the executable path cannot change what a running
    // daemon advertises. `build.rs` leaves the source id empty when it cannot
    // uniquely identify the source, which keeps the identity fail-safe unknown.
    usagi_core::infrastructure::ipc::build_identity(
        env!("CARGO_PKG_VERSION"),
        env!("USAGI_BUILD_COMMIT"),
        env!("USAGI_BUILD_TARGET"),
        env!("USAGI_BUILD_PROFILE"),
        env!("USAGI_BUILD_SOURCE_ID"),
    )
}
/// Connects one exact-owner-verified daemon session. `arm` wraps the accepted
/// socket in the transport the caller's lane runs over; it is applied only after
/// the peer's process-start identity, record and generation have been observed,
/// so the fence is identical for every lane.
fn connect_client<S: Read + Write>(
    data_dir: &Path,
    policy: ClientPolicy,
    build: BuildIdentity,
    workspace: ClientWorkspace,
    arm: impl FnOnce(std::os::unix::net::UnixStream) -> S,
) -> std::io::Result<IpcClient<S>> {
    let daemon = data_dir.join("daemon");
    let locator = read_locator(&daemon)?;
    let stream = usagi_daemon::infrastructure::unix_transport::connect_current(data_dir)?;
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon.join("daemon.json"),
    });
    let expected = store.load()?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "daemon process record is unavailable",
        )
    })?;
    let peer = peer_pid(&stream)?;
    let observation = ExactProcessControl.observe(&expected);
    IpcClient::connect_expected_owner(
        arm(stream),
        client_incarnation().to_owned(),
        format!("{}", std::process::id()),
        policy,
        build,
        workspace,
        &expected,
        &locator.generation,
        peer,
        observation,
    )
    .map_err(std::io::Error::other)
}
/// Build the lifecycle child that starts (or restarts) the daemon.
///
/// A daemon takes authority over the workspace of its start-up working directory
/// ([5. daemon](../../document/05-daemon.md)), so a client that is opening a
/// workspace starts the daemon *in* that workspace. Without this, opening
/// `~/project` from `~` would cold-start a daemon bound to `~` and then be
/// refused by the very fence that connection declares.
fn lifecycle_command(exe: &Path, args: &[&str], opened: Option<PathBuf>) -> std::process::Command {
    let mut child = std::process::Command::new(exe);
    child
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(opened) = opened {
        child.current_dir(opened);
    }
    child
}

fn run_lifecycle(exe: &Path, command: &str) -> std::io::Result<()> {
    run_lifecycle_with(exe, &["daemon", command], command)
}

fn run_lifecycle_with(exe: &Path, args: &[&str], command: &str) -> std::io::Result<()> {
    let status = lifecycle_command(exe, args, opened_workspace()).status()?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| std::io::Error::other(format!("daemon {command} failed")))
}
/// Enters the cross-process bootstrap section under a bounded wait.
///
/// The section serializes `connect_or_start` so two clients cannot cold-start
/// two daemons for one data directory. Because the data directory is shared by
/// every usagi process on the machine, the wait is bounded
/// ([`PrivateLockWait::BOOTSTRAP`]) and contention is reported as
/// [`ClientError::BootstrapContended`] rather than folded into "the daemon is
/// unavailable": the daemon may be perfectly healthy and the caller should
/// simply try again, which is exactly what the TUI's reattach backoff does.
fn acquire_bootstrap_lock(data_dir: &Path) -> Result<std::fs::File, ClientError> {
    acquire_bootstrap_lock_within(data_dir, PrivateLockWait::BOOTSTRAP)
}

fn acquire_bootstrap_lock_within(
    data_dir: &Path,
    wait: PrivateLockWait,
) -> Result<std::fs::File, ClientError> {
    let result = (|| {
        ensure_private_dir_all(data_dir)?;
        // `open_private_lock` runs `ensure_private_dir` on the lock's parent, so
        // creating (and directory-locking) `daemon/` here as well would double
        // the setup locking every bootstrap performs on the shared data dir.
        let path = data_dir.join("daemon").join("bootstrap.lock");
        lock_private_exclusive(
            &path,
            "bootstrap lock",
            PrivateLockModePolicy::BootstrapLegacy0644,
            wait,
        )
    })();
    result.map_err(|error: std::io::Error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            ClientError::BootstrapContended
        } else {
            ClientError::Unavailable(error.to_string())
        }
    })
}

/// Ensures that an active daemon endpoint exists before an interactive TUI is
/// shown. TUI operations still acquire their own client connection.
///
/// This readiness probe sends no request, so it declares no workspace: the entry
/// screens that need it (`usagi hop`'s Recent list, `usagi open <path>`) are
/// workspace switchers that must keep working from any directory. The
/// workspace-bound connections those screens make afterwards carry their own
/// declaration and are fenced there.
pub(crate) fn ensure_ready() -> Result<(), ClientError> {
    client_for(
        ClientPolicy::tui(),
        &ClientWorkspace::Unbound,
        ClientPolicy::tui().timeout_ms,
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::AtomicUsize;

    use usagi_core::domain::{
        id::{
            ClientId, ConnectionId, DaemonGeneration, RequestId, SessionId, TerminalId,
            WorkspaceId, WorktreeId,
        },
        terminal_launch::{TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId},
    };
    use usagi_core::usecase::client::{
        TerminalAction, TerminalGeometry, TerminalLaunchIntent, TerminalRequest,
    };
    use usagi_daemon::presentation::ipc::TerminalOwner;
    use usagi_daemon::usecase::terminal_ipc::{
        ResolvedTerminalScope, TerminalScopeResolveError, TerminalScopeResolver,
    };

    fn daemon_test_info() -> AppInfo {
        AppInfo {
            name: "usagi",
            version: "0.1.0",
        }
    }

    /// An instance lock fixture that was never acquired. These tests drive the
    /// publication and retirement seams directly; custody supervision starts
    /// only from the production `publish` path, which owns a real acquired lock.
    /// The fixture is leaked so it can satisfy `IpcReady`'s borrow without every
    /// call site threading an extra binding through its scope.
    fn unacquired_instance_lock(data_dir: &Path) -> &'static FileInstanceLock {
        Box::leak(Box::new(FileInstanceLock {
            path: data_dir.join("daemon/daemon.lock"),
            held: RefCell::new(None),
        }))
    }

    /// A workspace fence fixture that is already owned, so `serve` tests reach
    /// the publication and retirement seams under test. The real fence's own
    /// acquire / refuse / owner-hint behaviour has dedicated tests.
    struct AcquiredWorkspaceFence;

    impl WorkspaceFence for AcquiredWorkspaceFence {
        fn acquire(&self) -> std::io::Result<WorkspaceFenceOutcome> {
            Ok(WorkspaceFenceOutcome::Acquired)
        }
    }

    fn fresh_ipc_ready<'a>(data_dir: &'a Path, _info: &'a AppInfo) -> IpcReady<'a> {
        IpcReady {
            data_dir,
            // These tests never reach the real publication path, so the workspace
            // root only has to be a resolved directory.
            workspace_root: data_dir,
            instance_lock: unacquired_instance_lock(data_dir),
            build: BuildIdentity {
                version: "test".to_owned(),
                commit: "test".to_owned(),
                target: "test".to_owned(),
                artifact: "test-artifact".to_owned(),
            },
            shutdown: Arc::new(ShutdownRequest::new()),
            published: AtomicBool::new(false),
            publication_attempted: AtomicBool::new(false),
            worker: RefCell::new(None),
            listener: RefCell::new(None),
            cleanup: RefCell::new(None),
        }
    }

    fn ipc_generation() -> usagi_core::infrastructure::ipc::DaemonGeneration {
        usagi_core::infrastructure::ipc::DaemonGeneration(
            usagi_core::domain::id::DaemonGeneration::new()
                .as_str()
                .clone(),
        )
    }

    struct SupersededCleanup;

    impl usagi_daemon::usecase::stop::StaleDaemonCleanup for SupersededCleanup {
        fn cleanup_if(
            &self,
            _store: &dyn usagi_daemon::usecase::serve::DaemonRecordPort,
            _expected: &usagi_core::domain::daemon::DaemonRecord,
        ) -> std::io::Result<StaleCleanup> {
            Ok(StaleCleanup::Superseded)
        }
    }

    fn replace_private_lock_after_flock(path: &Path) -> std::thread::JoinHandle<()> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut replacement = path.as_os_str().to_owned();
        replacement.push(".replacement");
        let replacement = PathBuf::from(replacement);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&replacement)
            .unwrap();
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .unwrap();
        drop(file);

        let acquired = Arc::new(std::sync::Barrier::new(2));
        let replaced = Arc::new(std::sync::Barrier::new(2));
        install_private_lock_after_flock_barrier(
            path,
            Arc::clone(&acquired),
            Arc::clone(&replaced),
        );
        let path = path.to_path_buf();
        std::thread::spawn(move || {
            acquired.wait();
            std::fs::rename(replacement, path).unwrap();
            replaced.wait();
        })
    }

    fn assert_private_lock_descriptor(file: &std::fs::File) {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let metadata = file.metadata().unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let descriptor_flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(descriptor_flags, -1);
        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
    }

    struct ImmediateTestShutdown;

    impl ShutdownSignal for ImmediateTestShutdown {
        fn prepare(&self) -> std::io::Result<()> {
            Ok(())
        }

        fn wait(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct RecoveryOnlyReady<'a, 'b> {
        ready: &'a IpcReady<'b>,
        publishes: &'a Cell<u8>,
    }

    impl DaemonReady for RecoveryOnlyReady<'_, '_> {
        fn recover_stale_endpoint(&self) -> std::io::Result<()> {
            self.ready.recover_stale_endpoint()
        }

        fn publish(&self) -> std::io::Result<()> {
            self.publishes.set(self.publishes.get() + 1);
            Ok(())
        }

        fn quiesce(&self) -> std::io::Result<()> {
            Ok(())
        }

        fn retire(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A generation authority that takes no authority at all.
    ///
    /// The pre-registration recovery cases below never reach a bound endpoint, so
    /// there is nothing for a real authority to claim; this keeps those cases
    /// about the record and endpoint fence they are testing.
    struct NoGenerationAuthority;

    impl GenerationAuthority for NoGenerationAuthority {
        fn claim(&self) -> std::io::Result<()> {
            Ok(())
        }

        fn release(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A [`ProcessIdentitySource`] that returns a fixed identity for any pid, so
    /// `serve` tests can register a record without observing a real OS process.
    struct FixedIdentitySource(&'static str);

    impl ProcessIdentitySource for FixedIdentitySource {
        fn process_start_identity(&self, _pid: u32) -> std::io::Result<String> {
            Ok(self.0.to_string())
        }
    }

    #[test]
    fn daemon_process_identity_observation_fences_pid_reuse_and_legacy_records() {
        let pid = std::process::id();
        let identity = ExactProcessControl.process_start_identity(pid).unwrap();
        assert!(!identity.is_empty());
        let exact = DaemonRecord::identified(pid, identity.clone());
        assert_eq!(
            ExactProcessControl.observe(&exact),
            DaemonProcessObservation::Exact
        );

        let mismatch = DaemonRecord::identified(pid, format!("{identity}-other"));
        assert_eq!(
            ExactProcessControl.observe(&mismatch),
            DaemonProcessObservation::IdentityMismatch
        );
        assert_eq!(
            ExactProcessControl.observe(&DaemonRecord::new(pid)),
            DaemonProcessObservation::Unknown
        );
        let absent = DaemonRecord::identified(2_000_000_000, "not-present");
        assert_eq!(
            ExactProcessControl.observe(&absent),
            DaemonProcessObservation::Gone
        );
    }

    #[test]
    fn forged_same_uid_endpoint_cannot_echo_another_process_record() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let generation = ipc_generation();
        let listener = SecureUnixListener::bind(data, generation.clone()).unwrap();
        let mut recorded = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let record = DaemonRecord::identified(
            recorded.id(),
            process_start_identity(recorded.id()).unwrap(),
        );
        let store = DaemonRecordStore::new(FsRecordFile {
            path: data.join("daemon/daemon.json"),
        });
        store.save(&record).unwrap();
        let protocol = usagi_daemon::presentation::ipc::server_protocol(
            generation,
            "forged".into(),
            current_build(),
            record,
            paths::wire_workspace_root(data),
        );
        let server = std::thread::spawn(move || {
            let mut stream = loop {
                match listener.accept() {
                    Ok(stream) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::yield_now();
                    }
                    Err(error) => panic!("forged endpoint accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            let mut writer = stream.try_clone().unwrap();
            usagi_daemon::presentation::ipc::handshake(&mut stream, &mut writer, &protocol)
                .unwrap()
                .unwrap();
        });

        let clock = SystemClock::new();
        let error = connect_client(
            data,
            ClientPolicy::cli(),
            current_build(),
            ClientWorkspace::Bound {
                root: paths::wire_workspace_root(data),
            },
            |stream| deadline_transport(clock, stream, ClientPolicy::cli().timeout_ms),
        )
        .err()
        .expect("forged endpoint must be rejected");
        assert!(error.to_string().contains("endpoint owner"));
        server.join().unwrap();
        recorded.kill().unwrap();
        recorded.wait().unwrap();
    }

    #[test]
    fn daemon_shutdown_signals_only_the_exact_child_incarnation() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let identity = ExactProcessControl
            .process_start_identity(child.id())
            .unwrap();
        let exact = DaemonRecord::identified(child.id(), identity.clone());
        let mismatch = DaemonRecord::identified(child.id(), format!("{identity}-reused"));

        let error = SigtermTerminator.terminate(&mismatch).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(child.try_wait().unwrap().is_none());

        SigtermTerminator.terminate(&exact).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn delayed_record_clear_cannot_remove_a_concurrent_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon").join("daemon.json");
        let old_store = DaemonRecordStore::new(FsRecordFile { path: path.clone() });
        let old = usagi_core::domain::daemon::DaemonRecord::new(4242);
        let replacement = usagi_core::domain::daemon::DaemonRecord {
            pid: old.pid,
            process_start_identity: old.process_start_identity.clone(),
            started_at: old.started_at + chrono::Duration::nanoseconds(1),
        };
        old_store.save(&old).unwrap();
        let delayed_expected = old_store.load().unwrap().unwrap();

        let saved = Arc::new(std::sync::Barrier::new(2));
        let saved_by_replacement = Arc::clone(&saved);
        let replacement_for_thread = replacement.clone();
        let replacement_thread = std::thread::spawn(move || {
            let store = DaemonRecordStore::new(FsRecordFile { path });
            store.save(&replacement_for_thread).unwrap();
            saved_by_replacement.wait();
        });
        saved.wait();

        assert!(!old_store.clear_if(&delayed_expected).unwrap());
        assert_eq!(old_store.load().unwrap(), Some(replacement));
        replacement_thread.join().unwrap();
    }

    #[test]
    fn failed_atomic_record_save_preserves_old_record_and_removes_temporary() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let daemon = directory.path().join("daemon");
        let path = daemon.join("daemon.json");
        let store = DaemonRecordStore::new(FsRecordFile { path: path.clone() });
        let old = usagi_core::domain::daemon::DaemonRecord::new(4242);
        let replacement = usagi_core::domain::daemon::DaemonRecord::new(4343);
        store.save(&old).unwrap();

        fail_record_write_before_rename(&path);
        assert!(store.save(&replacement).is_err());

        assert_eq!(store.load().unwrap(), Some(old));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            std::fs::read_dir(&daemon).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("daemon.json.tmp.")
            }),
            "failed save left a daemon record temporary behind"
        );
    }

    #[test]
    fn lifecycle_private_files_override_a_restrictive_umask() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        const FIXTURE: &str = "USAGI_TEST_RESTRICTIVE_DAEMON_UMASK";
        if std::env::var_os(FIXTURE).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::daemon::tests::lifecycle_private_files_override_a_restrictive_umask",
                    "--nocapture",
                ])
                .env(FIXTURE, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        // This branch runs in its own test subprocess, so changing the process
        // umask cannot perturb parallel tests or unrelated persistence stores.
        let directory = tempfile::Builder::new()
            .prefix("umask-")
            .tempdir_in("/tmp")
            .unwrap();
        let previous_umask = unsafe { libc::umask(0o777) };
        let data = directory.path().join("data");
        ensure_private_dir(&data).unwrap();
        let daemon = data.join("daemon");
        let first_bootstrap = acquire_bootstrap_lock(&data).unwrap();
        let bootstrap_metadata = first_bootstrap.metadata().unwrap();
        assert!(bootstrap_metadata.is_file());
        assert_eq!(bootstrap_metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(bootstrap_metadata.nlink(), 1);
        assert_eq!(bootstrap_metadata.mode() & 0o777, 0o600);
        let descriptor_flags = unsafe { libc::fcntl(first_bootstrap.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(descriptor_flags, -1);
        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
        drop(first_bootstrap);
        // Reopening after the creating fd closes is the regression boundary:
        // the former code left a mode-000 node under umask 0777.
        let bootstrap = acquire_bootstrap_lock(&data).unwrap();
        let reopened_flags = unsafe { libc::fcntl(bootstrap.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(reopened_flags, -1);
        assert_ne!(reopened_flags & libc::FD_CLOEXEC, 0);
        let path = daemon.join("daemon.json");
        let store = DaemonRecordStore::new(FsRecordFile { path });
        store
            .save(&usagi_core::domain::daemon::DaemonRecord::new(4242))
            .unwrap();

        let instance = FileInstanceLock {
            path: daemon.join("daemon.lock"),
            held: RefCell::new(None),
        };
        assert!(instance.acquire().unwrap());
        let listener = SecureUnixListener::bind(
            &data,
            usagi_core::infrastructure::ipc::DaemonGeneration(
                usagi_core::domain::id::DaemonGeneration::new()
                    .as_str()
                    .clone(),
            ),
        )
        .unwrap();

        for private_file in [
            "daemon.json",
            "daemon.lock",
            "record.lock",
            "bootstrap.lock",
            "current.json",
            "current.lock",
        ] {
            assert_eq!(
                std::fs::metadata(daemon.join(private_file))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{private_file} did not override umask 0777"
            );
        }
        drop((listener, instance, bootstrap));
        unsafe {
            libc::umask(previous_umask);
        }
    }

    #[test]
    fn all_lifecycle_locks_recover_a_crash_after_restrictive_umask_creation() {
        use std::os::unix::fs::PermissionsExt;

        const FIXTURE: &str = "USAGI_TEST_PRIVATE_LOCK_CREATE_CRASH";
        if std::env::var_os(FIXTURE).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::daemon::tests::all_lifecycle_locks_recover_a_crash_after_restrictive_umask_creation",
                    "--nocapture",
                ])
                .env(FIXTURE, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        // Isolate the process-global umask, then stop each lock immediately
        // after create_new. The next API call must recover that durable mode-000
        // residue and leave the same exact private invariant on every fd.
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let record_daemon = directory.path().join("record-daemon");
        let instance_daemon = directory.path().join("instance-daemon");
        let bootstrap_data = directory.path().join("bootstrap-data");
        ensure_private_dir(&record_daemon).unwrap();
        ensure_private_dir(&instance_daemon).unwrap();
        ensure_private_dir(&bootstrap_data).unwrap();
        let previous_umask = unsafe { libc::umask(0o777) };

        let record_path = record_daemon.join("daemon.json");
        let record_lock = record_daemon.join("record.lock");
        let store = DaemonRecordStore::new(FsRecordFile { path: record_path });
        fail_private_lock_after_create(&record_lock);
        assert!(store.load().is_err());
        assert_eq!(
            std::fs::metadata(&record_lock)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0
        );
        assert_eq!(store.load().unwrap(), None);
        let record_descriptor = lock_private_exclusive(
            &record_lock,
            "daemon record lock",
            PrivateLockModePolicy::CrashResidue,
            PrivateLockWait::RECORD,
        )
        .unwrap();
        assert_private_lock_descriptor(&record_descriptor);
        drop(record_descriptor);

        let instance_path = instance_daemon.join("daemon.lock");
        let failed_instance = FileInstanceLock {
            path: instance_path.clone(),
            held: RefCell::new(None),
        };
        fail_private_lock_after_create(&instance_path);
        assert!(failed_instance.acquire().is_err());
        assert_eq!(
            std::fs::metadata(&instance_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0
        );
        let instance = FileInstanceLock {
            path: instance_path,
            held: RefCell::new(None),
        };
        assert!(instance.acquire().unwrap());
        assert_private_lock_descriptor(instance.held.borrow().as_ref().unwrap());

        let bootstrap_path = bootstrap_data.join("daemon/bootstrap.lock");
        fail_private_lock_after_create(&bootstrap_path);
        assert!(acquire_bootstrap_lock(&bootstrap_data).is_err());
        assert_eq!(
            std::fs::metadata(&bootstrap_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0
        );
        let bootstrap = acquire_bootstrap_lock(&bootstrap_data).unwrap();
        assert_private_lock_descriptor(&bootstrap);

        drop((bootstrap, instance));
        unsafe {
            libc::umask(previous_umask);
        }
    }

    /// The bootstrap section is bounded, and its contention is a distinct answer.
    ///
    /// The section is entered on a machine-wide data directory by every surface,
    /// including the TUI's render thread, and it is held across one
    /// `connect_or_start` — a cold start, in the worst case. A blocking `flock`
    /// there means any other usagi process (MCP server, CLI, rollover), or a
    /// holder that was killed while wedged, stalls the UI without limit. So a
    /// holder that outlasts the wait yields `BootstrapContended`, which tells the
    /// surface to retry rather than that the daemon is absent.
    #[test]
    fn a_contended_bootstrap_section_returns_bounded_typed_contention() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data_dir = directory.path().join("data");
        // Enter and leave once, so the uncontended path is the one that creates
        // the lock node and the `daemon/` directory chain.
        drop(acquire_bootstrap_lock(&data_dir).unwrap());

        // A second open file description on the same node: `flock` conflicts
        // across descriptions, so this is exactly what another process holding
        // the section looks like.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(data_dir.join("daemon").join("bootstrap.lock"))
            .unwrap();
        FileExt::lock_exclusive(&held).unwrap();

        let wait = PrivateLockWait {
            limit: Duration::from_millis(120),
            poll: Duration::from_millis(10),
        };
        let started = Instant::now();
        let error = acquire_bootstrap_lock_within(&data_dir, wait)
            .expect_err("a held bootstrap section must not be entered");
        let elapsed = started.elapsed();

        assert_eq!(error, ClientError::BootstrapContended);
        assert_eq!(
            error.side_effect(),
            usagi_core::infrastructure::ipc::SideEffect::None
        );
        assert!(
            elapsed >= wait.limit,
            "the wait was actually spent: {elapsed:?}"
        );
        assert!(
            elapsed < wait.limit * 10,
            "the wait is bounded, not blocking: {elapsed:?}"
        );

        // Once the holder leaves, the same section is entered normally.
        FileExt::unlock(&held).unwrap();
        drop(held);
        let entered = acquire_bootstrap_lock_within(&data_dir, wait).unwrap();
        assert_private_lock_descriptor(&entered);
    }

    /// The wait must outlast one honest cold start, or a client that legitimately
    /// waits for a peer's `daemon start` would report contention instead of using
    /// the daemon that peer is about to publish.
    #[test]
    fn the_bootstrap_wait_outlasts_one_cold_start() {
        assert!(PrivateLockWait::BOOTSTRAP.limit > bootstrap::READINESS_CEILING);
        assert!(PrivateLockWait::BOOTSTRAP.poll < PrivateLockWait::BOOTSTRAP.limit);
        assert!(PrivateLockWait::RECORD.limit < PrivateLockWait::BOOTSTRAP.limit);
    }

    /// Every daemon socket this composition root builds carries an armed
    /// end-to-end deadline, so no surface can be handed an unbounded stream: the
    /// only client type is [`LaneClient`], and its transport fails closed once
    /// the budget is spent.
    #[test]
    fn the_lane_transport_bounds_reads_and_writes_by_construction() {
        let (client_socket, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut lane = deadline_transport(SystemClock::new(), client_socket, 40);

        // The peer is alive and simply never answers, which is the shape a hung
        // daemon has: without the armed deadline this read would never return.
        let started = Instant::now();
        let mut byte = [0_u8; 1];
        let error = lane.read(&mut byte).unwrap_err();
        let elapsed = started.elapsed();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(elapsed < Duration::from_secs(2), "bounded: {elapsed:?}");

        // The budget is spent, so the next call fails without touching the OS;
        // re-arming is what gives the next request its own budget.
        assert_eq!(
            lane.read(&mut byte).unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        usagi_core::usecase::client::RearmableStream::rearm(&mut lane, 40);
        assert!(lane.write(b"x").is_ok());
        drop(peer);
    }

    #[test]
    fn record_lock_rejects_a_path_replacement_after_flock() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let daemon = directory.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let record_lock = daemon.join("record.lock");
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        let replacement = replace_private_lock_after_flock(&record_lock);

        let error = store.load().unwrap_err();
        replacement.join().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("daemon record lock"));
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn instance_lock_rejects_a_path_replacement_after_flock() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let daemon = directory.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let path = daemon.join("daemon.lock");
        let instance = FileInstanceLock {
            path: path.clone(),
            held: RefCell::new(None),
        };
        let replacement = replace_private_lock_after_flock(&path);

        let error = instance.acquire().unwrap_err();
        replacement.join().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("daemon instance lock"));
        assert!(instance.held.borrow().is_none());
        let retry = FileInstanceLock {
            path,
            held: RefCell::new(None),
        };
        assert!(retry.acquire().unwrap());
    }

    /// Build a fence for `workspace` as `pid` would see it.
    fn workspace_fence(workspace: &Path, pid: u32) -> FileWorkspaceFence {
        let workspace = paths::canonical_workspace_root(workspace).unwrap();
        FileWorkspaceFence {
            path: paths::workspace_fence_path(&workspace),
            workspace,
            pid,
            held: RefCell::new(None),
        }
    }

    #[test]
    fn workspace_fence_refuses_a_second_owner_and_names_its_pid() {
        let workspace = tempfile::tempdir_in("/tmp").unwrap();
        let owner = workspace_fence(workspace.path(), 4242);
        assert_eq!(owner.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);

        // A second daemon over the same workspace is refused and can name the
        // live owner, which is the only cross-data-directory discovery it has.
        let second = workspace_fence(workspace.path(), 5252);
        assert_eq!(
            second.acquire().unwrap(),
            WorkspaceFenceOutcome::Held {
                workspace: paths::canonical_workspace_root(workspace.path())
                    .unwrap()
                    .display()
                    .to_string(),
                owner: Some(4242),
            }
        );
        assert!(second.held.borrow().is_none());

        // The fence node lives in a daemon-private directory beside — not inside
        // — the runtime-mode children, and the OS releases it with the owner.
        assert_eq!(
            owner.path,
            paths::canonical_workspace_root(workspace.path())
                .unwrap()
                .join(".usagi/daemon/daemon.lock")
        );
        drop(owner);
        let third = workspace_fence(workspace.path(), 6262);
        assert_eq!(third.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);
    }

    #[test]
    fn workspace_fence_refuses_through_a_symlinked_or_relative_spelling() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let owner = workspace_fence(&workspace, 4242);
        assert_eq!(owner.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);

        let link = root.path().join("link");
        std::os::unix::fs::symlink(&workspace, &link).unwrap();
        for spelling in [
            link,
            workspace.join("."),
            workspace.join("..").join("workspace"),
        ] {
            let refused = workspace_fence(&spelling, 5252);
            assert!(
                matches!(
                    refused.acquire().unwrap(),
                    WorkspaceFenceOutcome::Held {
                        owner: Some(4242),
                        ..
                    }
                ),
                "{} escaped the workspace fence",
                spelling.display()
            );
        }
    }

    #[test]
    fn workspace_fence_refuses_when_the_owner_hint_is_unreadable() {
        let workspace = tempfile::tempdir_in("/tmp").unwrap();
        let owner = workspace_fence(workspace.path(), 4242);
        assert_eq!(owner.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);

        // A holder killed between `flock` and publishing its hint leaves an empty
        // node. The refusal must stand; only the diagnostic pid is lost.
        std::fs::write(&owner.path, "").unwrap();
        let refused = workspace_fence(workspace.path(), 5252);
        assert_eq!(
            refused.acquire().unwrap(),
            WorkspaceFenceOutcome::Held {
                workspace: paths::canonical_workspace_root(workspace.path())
                    .unwrap()
                    .display()
                    .to_string(),
                owner: None,
            }
        );

        // So does a garbled or over-long line.
        std::fs::write(&owner.path, "x".repeat(128)).unwrap();
        assert!(matches!(
            workspace_fence(workspace.path(), 5252).acquire().unwrap(),
            WorkspaceFenceOutcome::Held { owner: None, .. }
        ));
    }

    #[test]
    fn workspace_fence_rejects_a_path_replacement_after_flock() {
        let workspace = tempfile::tempdir_in("/tmp").unwrap();
        let fence = workspace_fence(workspace.path(), 4242);
        // Create the parent chain first: the replacement thread races the
        // pathname, not the directory setup.
        std::fs::create_dir_all(fence.workspace.join(paths::STATE_DIR)).unwrap();
        ensure_private_dir(fence.path.parent().unwrap()).unwrap();
        let replacement = replace_private_lock_after_flock(&fence.path);

        let error = fence.acquire().unwrap_err();
        replacement.join().unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("daemon workspace fence"));
        assert!(fence.held.borrow().is_none());
    }

    #[test]
    fn bound_workspace_root_canonicalizes_and_fails_on_an_unresolvable_root() {
        let workspace = tempfile::tempdir_in("/tmp").unwrap();
        let daemon = workspace.path().join("data/daemon");
        ensure_private_dir_all(&daemon).unwrap();

        // With no durable state the bound root is the (canonicalized) startup
        // directory, which is what the session runtime would adopt.
        assert_eq!(
            bound_workspace_root(&daemon, workspace.path().join(".")).unwrap(),
            paths::canonical_workspace_root(workspace.path()).unwrap()
        );

        // A startup directory that no longer resolves is a startup failure, not a
        // fence that silently keys some other path.
        let error = bound_workspace_root(&daemon, workspace.path().join("absent")).unwrap_err();
        assert!(error.to_string().contains("workspace root"), "{error}");

        // Unreadable durable state fails the same way, rather than falling back
        // to a candidate the runtime would not adopt.
        std::fs::write(daemon.join("sessions.json"), "not json").unwrap();
        assert!(
            bound_workspace_root(&daemon, workspace.path().to_path_buf())
                .unwrap_err()
                .to_string()
                .contains("Storage")
        );
    }

    #[test]
    fn bootstrap_lock_rejects_a_path_replacement_after_flock() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path().join("data");
        ensure_private_dir(&data).unwrap();
        let daemon = data.join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let path = daemon.join("bootstrap.lock");
        let replacement = replace_private_lock_after_flock(&path);

        let error = acquire_bootstrap_lock(&data).unwrap_err();
        replacement.join().unwrap();
        assert!(error.to_string().contains("bootstrap lock"));
        let retry = acquire_bootstrap_lock(&data).unwrap();
        assert_private_lock_descriptor(&retry);
    }

    #[test]
    fn record_and_instance_locks_reject_broad_modes_and_hardlinks_without_chmod() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let daemon = directory.path().join("daemon");
        ensure_private_dir(&daemon).unwrap();

        let broad_record_lock = daemon.join("record.lock");
        std::fs::write(&broad_record_lock, []).unwrap();
        std::fs::set_permissions(&broad_record_lock, std::fs::Permissions::from_mode(0o644))
            .unwrap();
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        assert!(store.load().is_err());
        assert_eq!(
            std::fs::metadata(&broad_record_lock)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        std::fs::remove_file(&broad_record_lock).unwrap();

        let record_target = daemon.join("record-target");
        std::fs::write(&record_target, b"preserve").unwrap();
        std::fs::set_permissions(&record_target, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::hard_link(&record_target, daemon.join("record.lock")).unwrap();
        assert!(store.load().is_err());
        assert_eq!(std::fs::read(&record_target).unwrap(), b"preserve");
        assert_eq!(
            std::fs::metadata(&record_target)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );

        let broad_instance_lock = daemon.join("daemon.lock");
        std::fs::write(&broad_instance_lock, []).unwrap();
        std::fs::set_permissions(&broad_instance_lock, std::fs::Permissions::from_mode(0o640))
            .unwrap();
        let broad_instance = FileInstanceLock {
            path: broad_instance_lock.clone(),
            held: RefCell::new(None),
        };
        assert!(broad_instance.acquire().is_err());
        assert_eq!(
            std::fs::metadata(&broad_instance_lock)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        std::fs::remove_file(&broad_instance_lock).unwrap();

        let instance_target = daemon.join("instance-target");
        std::fs::write(&instance_target, b"preserve").unwrap();
        std::fs::set_permissions(&instance_target, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::hard_link(&instance_target, daemon.join("daemon.lock")).unwrap();
        let instance = FileInstanceLock {
            path: daemon.join("daemon.lock"),
            held: RefCell::new(None),
        };
        assert!(instance.acquire().is_err());
        assert_eq!(std::fs::read(&instance_target).unwrap(), b"preserve");
        assert_eq!(
            std::fs::metadata(instance_target)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    #[test]
    fn bootstrap_lock_rejects_symlink_hardlink_and_non_regular_nodes() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path().join("data");
        ensure_private_dir(&data).unwrap();
        let daemon = data.join("daemon");
        ensure_private_dir(&daemon).unwrap();
        let lock = daemon.join("bootstrap.lock");
        let target = daemon.join("target");
        std::fs::write(&target, b"preserve").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        std::os::unix::fs::symlink(&target, &lock).unwrap();
        assert!(acquire_bootstrap_lock(&data).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
        std::fs::remove_file(&lock).unwrap();

        std::fs::hard_link(&target, &lock).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().nlink(), 2);
        assert!(acquire_bootstrap_lock(&data).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"preserve");
        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o600);
        std::fs::remove_file(&lock).unwrap();

        std::fs::create_dir(&lock).unwrap();
        assert!(acquire_bootstrap_lock(&data).is_err());
        std::fs::remove_dir(&lock).unwrap();

        // No broad mode other than the exact origin/main 0644 legacy state is
        // a valid umask residue or migration candidate.
        std::fs::write(&lock, []).unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(acquire_bootstrap_lock(&data).is_err());
        assert_eq!(std::fs::metadata(&lock).unwrap().mode() & 0o777, 0o666);

        // Use the sticky bit for the exact-mode boundary: Darwin strips set-id
        // bits when this test is built with coverage instrumentation.
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o1600)).unwrap();
        assert_eq!(std::fs::metadata(&lock).unwrap().mode() & 0o7777, 0o1600);
        assert!(acquire_bootstrap_lock(&data).is_err());
        assert_eq!(std::fs::metadata(&lock).unwrap().mode() & 0o7777, 0o1600);

        // origin/main created bootstrap.lock without an explicit mode, so the
        // exact historical 0644 owner file is a one-time migration exception.
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        let repaired = acquire_bootstrap_lock(&data).unwrap();
        assert_eq!(repaired.metadata().unwrap().mode() & 0o777, 0o600);
        drop(repaired);

        // A creator killed between create_new and fd-fchmod can leave the same
        // owner single-link inode at mode 000. Secure reopen repairs that
        // durable residue instead of permanently wedging every later client.
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();
        let repaired = acquire_bootstrap_lock(&data).unwrap();
        assert_eq!(repaired.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(repaired.metadata().unwrap().nlink(), 1);
    }

    #[test]
    fn ipc_ready_retains_listener_cleanup_ownership_until_retry_succeeds() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let listener = SecureUnixListener::bind(data, ipc_generation()).unwrap();
        let daemon = data.join("daemon");
        let socket = daemon.join(&listener.locator().endpoint);
        let cleanup = listener.cleanup_handle();
        let lock = daemon.join("current.lock");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        let ready = fresh_ipc_ready(data, &info);
        ready.publication_attempted.store(true, Ordering::Release);
        ready.published.store(true, Ordering::Release);
        *ready.listener.borrow_mut() = Some(listener);
        *ready.cleanup.borrow_mut() = Some(cleanup);

        assert!(ready.retire().is_err());
        assert!(ready.listener.borrow().is_some());
        assert!(ready.cleanup.borrow().is_some());
        assert!(socket.exists());
        assert!(daemon.join("current.json").exists());

        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
        ready.retire().unwrap();
        assert!(ready.listener.borrow().is_none());
        assert!(ready.cleanup.borrow().is_none());
        assert!(!socket.exists());
        assert!(!daemon.join("current.json").exists());
    }

    #[test]
    fn ipc_ready_retains_cleanup_token_when_accept_worker_panics() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let listener = SecureUnixListener::bind(data, ipc_generation()).unwrap();
        let daemon = data.join("daemon");
        let socket = daemon.join(&listener.locator().endpoint);
        let cleanup = listener.cleanup_handle();
        let lock = daemon.join("current.lock");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        let worker = std::thread::spawn(move || -> SecureUnixListener {
            let _listener = listener;
            panic!("injected accept-loop panic")
        });
        let ready = fresh_ipc_ready(data, &info);
        ready.publication_attempted.store(true, Ordering::Release);
        ready.published.store(true, Ordering::Release);
        *ready.worker.borrow_mut() = Some(worker);
        *ready.cleanup.borrow_mut() = Some(cleanup);

        assert!(ready.quiesce().is_err());
        assert!(ready.worker.borrow().is_none());
        assert!(ready.listener.borrow().is_none());
        assert!(ready.retire().is_err());
        assert!(ready.cleanup.borrow().is_some());
        assert!(socket.exists());
        assert!(daemon.join("current.json").exists());

        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
        ready.retire().unwrap();
        assert!(ready.cleanup.borrow().is_none());
        assert!(!socket.exists());
        assert!(!daemon.join("current.json").exists());
    }

    #[test]
    fn abnormal_startup_after_bind_keeps_retryable_cleanup_ownership() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let daemon = data.join("daemon");
        let socket = RefCell::new(None);
        let ready = fresh_ipc_ready(data, &info);

        let unsafe_locator_lock = |daemon: &Path| -> std::io::Result<()> {
            let lock = daemon.join("current.lock");
            if !lock.exists() {
                std::fs::write(&lock, b"")?;
            }
            std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644))
        };

        let error = ready
            .publish_with(|listener, _generation| {
                *socket.borrow_mut() = Some(daemon.join(&listener.locator().endpoint));
                // Break the locator lock so the listener's own `Drop` cannot
                // reclaim this endpoint: the retained cleanup token has to remain
                // the only retry path.
                unsafe_locator_lock(&daemon)?;
                Err(std::io::Error::other("injected post-bind startup failure"))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "injected post-bind startup failure");
        assert!(ready.cleanup.borrow().is_some());
        assert!(ready.publication_attempted.load(Ordering::Acquire));
        assert!(socket.borrow().as_ref().unwrap().exists());
        // Binding is not publishing: a startup that failed before the generation
        // authority ran leaves nothing for a client to discover.
        assert!(!daemon.join("current.json").exists());

        std::fs::set_permissions(
            daemon.join("current.lock"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        // Once published, an unreadable locator lock keeps retirement retryable
        // rather than letting the endpoint look cleanly reclaimed.
        ready.publish_current().unwrap();
        assert!(daemon.join("current.json").exists());
        unsafe_locator_lock(&daemon).unwrap();
        assert!(ready.retire().is_err());
        assert!(ready.cleanup.borrow().is_some());
        assert!(socket.borrow().as_ref().unwrap().exists());

        std::fs::set_permissions(
            daemon.join("current.lock"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        ready.retire().unwrap();
        assert!(ready.cleanup.borrow().is_none());
        assert!(!socket.borrow().as_ref().unwrap().exists());
        assert!(!daemon.join("current.json").exists());
    }

    /// A registry authority over a real data directory, bound to `ready`.
    fn registry_authority<'a>(
        data_dir: &'a Path,
        ready: &'a IpcReady<'a>,
    ) -> RegistryAuthority<'a> {
        RegistryAuthority {
            data_dir,
            ready,
            build: current_build(),
            pid: std::process::id(),
            claimed: RefCell::new(None),
        }
    }

    /// The durable registry document, which must exist by the time this is read.
    fn registry_document(
        data_dir: &Path,
    ) -> usagi_daemon::usecase::authority::registry::RegistryDocument {
        usagi_daemon::infrastructure::generation_registry::read_registry_document(data_dir)
            .unwrap()
            .expect("the daemon registered a generation")
    }

    #[test]
    fn claiming_authority_registers_this_generation_and_then_publishes_current() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let listener = SecureUnixListener::bind_private(data, ipc_generation()).unwrap();
        let generation = listener.locator().generation.clone();
        let ready = fresh_ipc_ready(data, &info);
        *ready.cleanup.borrow_mut() = Some(listener.cleanup_handle());
        let authority = registry_authority(data, &ready);

        // A bound endpoint is not yet discoverable.
        assert!(read_locator(&data.join("daemon")).is_err());

        authority.claim().unwrap();

        let document = registry_document(data);
        assert_eq!(
            document.current.map(|current| current.as_str()),
            Some(generation.0.clone())
        );
        let entry = document.generations.first().unwrap();
        assert_eq!(
            entry.role,
            usagi_daemon::usecase::generation::GenerationRole::Active
        );
        assert_eq!(entry.endpoint, listener.locator().endpoint);
        assert_eq!(entry.process.pid, std::process::id());
        // Only now is the endpoint discoverable, and by exactly the generation
        // the registry named.
        assert_eq!(
            read_locator(&data.join("daemon")).unwrap().generation,
            generation
        );

        // A repeated claim converges instead of consuming a second slot.
        authority.claim().unwrap();
        assert_eq!(registry_document(data).generations.len(), 1);

        authority.release().unwrap();
        assert_eq!(registry_document(data).current, None);
        // Releasing an authority that is already given up is not a failure.
        authority.release().unwrap();
    }

    #[test]
    fn claiming_authority_before_binding_is_refused_without_touching_the_registry() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let ready = fresh_ipc_ready(data, &info);
        let authority = registry_authority(data, &ready);

        assert_eq!(
            authority.claim().unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(
            usagi_daemon::infrastructure::generation_registry::read_registry_document(data),
            Ok(None)
        );
        // Nothing was claimed, so there is nothing to release either.
        authority.release().unwrap();
    }

    #[test]
    fn a_non_canonical_bound_generation_is_refused_before_the_registry_is_written() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let listener = SecureUnixListener::bind_private(
            data,
            usagi_core::infrastructure::ipc::DaemonGeneration("not-a-generation".to_owned()),
        )
        .unwrap();
        let ready = fresh_ipc_ready(data, &info);
        *ready.cleanup.borrow_mut() = Some(listener.cleanup_handle());
        let authority = registry_authority(data, &ready);

        assert_eq!(
            authority.claim().unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(
            usagi_daemon::infrastructure::generation_registry::read_registry_document(data),
            Ok(None)
        );
    }

    #[test]
    fn a_live_registered_authority_is_repaired_rather_than_displaced() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let daemon = data.join("daemon");

        // A generation whose recorded process is this very test binary: the
        // recovery below can therefore prove it alive.
        let holder_listener = SecureUnixListener::bind_private(data, ipc_generation()).unwrap();
        let holder = holder_listener.locator().clone();
        let holder_ready = fresh_ipc_ready(data, &info);
        *holder_ready.cleanup.borrow_mut() = Some(holder_listener.cleanup_handle());
        registry_authority(data, &holder_ready).claim().unwrap();
        // Drop only the published locator, leaving the holder's endpoint bound and
        // the registry as the only surviving statement of authority.
        std::fs::remove_file(daemon.join("current.json")).unwrap();
        assert!(read_locator(&daemon).is_err());

        let listener = SecureUnixListener::bind_private(data, ipc_generation()).unwrap();
        let ready = fresh_ipc_ready(data, &info);
        *ready.cleanup.borrow_mut() = Some(listener.cleanup_handle());

        let error = registry_authority(data, &ready).claim().unwrap_err();

        assert!(
            error.to_string().contains("still holds registry authority"),
            "{error}"
        );
        // Recovery republished the live holder's own locator — the foreign-owner
        // publication path — and this process's endpoint was never published.
        assert_eq!(read_locator(&daemon).unwrap().generation, holder.generation);
        assert_eq!(registry_document(data).generations.len(), 1);
    }

    #[test]
    fn a_generation_process_is_only_verified_by_its_exact_recorded_identity() {
        let pid = std::process::id();
        let live = own_process_identity(pid).unwrap();
        assert_eq!(
            observe_generation_process(&live),
            ProcessObservation::VerifiedAlive(live.clone())
        );

        let reused = ProcessIdentity {
            start_identity: "another-incarnation".to_owned(),
            ..live.clone()
        };
        assert_eq!(
            observe_generation_process(&reused),
            ProcessObservation::Unknown
        );

        let legacy = ProcessIdentity {
            start_identity: String::new(),
            ..live.clone()
        };
        assert_eq!(
            observe_generation_process(&legacy),
            ProcessObservation::Unknown
        );

        // A PID far above the OS maximum names no process at all.
        let absent = ProcessIdentity {
            pid: 2_000_000_000,
            ..live
        };
        assert_eq!(
            observe_generation_process(&absent),
            ProcessObservation::Gone
        );
    }

    #[test]
    fn publishing_current_before_binding_is_refused() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let info = daemon_test_info();
        let ready = fresh_ipc_ready(directory.path(), &info);

        assert_eq!(
            ready.publish_current().unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(ready.bound_endpoint().is_none());
    }

    #[test]
    fn stale_cleanup_keeps_record_until_endpoint_retry_proves_absence() {
        use std::mem::ManuallyDrop;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let daemon = data.join("daemon");
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let socket = daemon.join(&listener.locator().endpoint);
        let record_path = daemon.join("daemon.json");
        let store = DaemonRecordStore::new(FsRecordFile { path: record_path });
        let record = usagi_core::domain::daemon::DaemonRecord::new(4242);
        store.save(&record).unwrap();
        let ready = fresh_ipc_ready(data, &info);
        let lock = daemon.join("current.lock");
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(ready.cleanup_if(&store, &record).is_err());
        assert_eq!(store.load().unwrap(), Some(record.clone()));
        assert!(socket.exists());
        assert!(daemon.join("current.json").exists());

        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            ready.cleanup_if(&store, &record).unwrap(),
            StaleCleanup::Cleared
        );
        assert_eq!(store.load().unwrap(), None);
        assert!(!socket.exists());
        assert!(!daemon.join("current.json").exists());
        // SAFETY: the listener was not moved or dropped; cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn the_declared_workspace_prefers_the_opened_one_then_the_injected_root() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let canonical_root =
            paths::wire_workspace_root(paths::canonical_workspace_root(&workspace).unwrap());
        let canonical = ClientWorkspace::Bound {
            root: canonical_root.clone(),
        };

        // A daemon-provisioned child declares the trusted root the daemon
        // injected, not whatever directory the provider left it in.
        assert_eq!(
            declared_client_workspace(
                None,
                Some(workspace.clone().into_os_string()),
                Ok(directory.path().join("elsewhere")),
            ),
            canonical
        );

        // Every other surface declares its canonical working directory, so a
        // subdirectory spelling still resolves onto the one comparable root. An
        // empty injection is ignored rather than treated as a root.
        assert_eq!(
            declared_client_workspace(
                None,
                Some(std::ffi::OsString::new()),
                Ok(workspace.join(".").join("..").join("workspace")),
            ),
            canonical
        );

        // An unresolvable directory is declared exactly as spelled: the daemon
        // refuses it rather than this client assuming that it matches.
        let missing = workspace.join("absent");
        assert_eq!(
            declared_client_workspace(None, None, Ok(missing.clone())),
            ClientWorkspace::Bound {
                root: paths::wire_workspace_root(&missing),
            }
        );

        // With no working directory at all there is nothing to declare, and an
        // empty root is refused by every daemon.
        assert_eq!(
            declared_client_workspace(
                None,
                None,
                Err(std::io::Error::other("no working directory"))
            ),
            ClientWorkspace::Bound {
                root: String::new(),
            }
        );

        // An opened workspace outranks both: it is the workspace whose sessions
        // the surface is about to show, so the daemon must serve exactly it. The
        // injected root and the working directory would both be admitted here
        // (they are the trusted root and a directory below it), which is how the
        // title and the session list used to disagree.
        let opened = directory.path().join("other");
        std::fs::create_dir(&opened).unwrap();
        let opened_canonical = paths::canonical_workspace_root(&opened).unwrap();
        assert_eq!(
            declared_client_workspace(
                Some(opened_canonical.clone()),
                Some(workspace.clone().into_os_string()),
                Ok(workspace.join("crates")),
            ),
            ClientWorkspace::Selected {
                root: paths::wire_workspace_root(&opened_canonical),
            }
        );
    }

    #[test]
    fn declaring_the_opened_workspace_selects_it_for_every_later_connection() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();

        let canonical = declare_opened_workspace(&workspace).unwrap();
        assert_eq!(
            canonical,
            paths::canonical_workspace_root(&workspace).unwrap()
        );
        assert_eq!(opened_workspace().as_deref(), Some(canonical.as_path()));
        assert_eq!(
            client_workspace(),
            ClientWorkspace::Selected {
                root: paths::wire_workspace_root(&canonical),
            }
        );

        // `usagi hop` opens several workspaces in one process, so the latest
        // selection replaces the previous one.
        let second = directory.path().join("second");
        std::fs::create_dir(&second).unwrap();
        let second_canonical = declare_opened_workspace(&second).unwrap();
        assert_eq!(
            client_workspace(),
            ClientWorkspace::Selected {
                root: paths::wire_workspace_root(&second_canonical),
            }
        );

        // A path that cannot be resolved is reported instead of being declared,
        // and it leaves the previous selection untouched.
        assert!(declare_opened_workspace(&directory.path().join("absent")).is_err());
        assert_eq!(
            opened_workspace().as_deref(),
            Some(second_canonical.as_path())
        );

        // A root with no wire spelling is reported before anything connects or
        // starts a daemon: no daemon can own it, because its own authority record
        // and the workspace registry are JSON.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            let name = std::ffi::OsString::from_vec(b"workspace-\xff".to_vec());
            let unnameable = directory.path().join(name);
            if std::fs::create_dir(&unnameable).is_ok() {
                let error = declare_opened_workspace(&unnameable).unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
                assert!(error.to_string().contains("not valid UTF-8"), "{error}");
                assert_eq!(
                    opened_workspace().as_deref(),
                    Some(second_canonical.as_path())
                );
            }
        }

        *OPENED_WORKSPACE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    #[test]
    fn a_lifecycle_start_runs_in_the_workspace_being_opened() {
        let exe = PathBuf::from("/usr/bin/usagi");
        let start = lifecycle_command(&exe, &["daemon", "start"], None);
        // Without a selection the child inherits this process's directory, which
        // is what a plain `usagi daemon start` means.
        assert_eq!(start.get_current_dir(), None);
        assert_eq!(
            start.get_args().collect::<Vec<_>>(),
            vec!["daemon", "start"]
        );

        // A daemon takes authority over the workspace of its start-up directory,
        // so a client opening a workspace must start it there — otherwise the
        // fresh daemon would bind this process's directory and then refuse the
        // very connection that started it.
        let opened = PathBuf::from("/workspace/root");
        let restart = lifecycle_command(
            &exe,
            &["daemon", "restart", "--force"],
            Some(opened.clone()),
        );
        assert_eq!(restart.get_current_dir(), Some(opened.as_path()));
        // Development consumes a build-mismatch trigger by an explicit cold
        // transition, so its restart carries the guard override it chose.
        assert_eq!(
            restart.get_args().collect::<Vec<_>>(),
            vec!["daemon", "restart", "--force"]
        );
    }

    #[test]
    fn client_bootstrap_recovery_uses_the_instance_fence_not_a_raw_pid() {
        use std::mem::ManuallyDrop;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let daemon = data.join("daemon");
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let socket = daemon.join(&listener.locator().endpoint);
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        // The PID is deliberately live (this test process), modelling PID
        // reuse. Acquiring daemon.lock proves that this live process is not the
        // daemon owner, and recovery never sends it a signal.
        let record = DaemonRecord::identified(std::process::id(), "reused-process");
        store.save(&record).unwrap();

        assert_eq!(
            recover_stale_client_endpoint(data).unwrap(),
            bootstrap::StaleRecovery::Recovered
        );
        assert_eq!(store.load().unwrap(), None);
        assert!(!socket.exists());
        assert!(!daemon.join("current.json").exists());
        assert!(
            ExactProcessControl
                .process_start_identity(std::process::id())
                .is_ok()
        );

        // SAFETY: recovery removed only filesystem artifacts; dropping closes
        // the still-owned listener fd and its cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn client_bootstrap_recovers_a_socket_first_partial_retire_with_a_reused_live_pid() {
        use std::mem::ManuallyDrop;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let daemon = data.join("daemon");
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let cleanup = listener.cleanup_handle();
        let socket = daemon.join(&listener.locator().endpoint);
        let current = daemon.join("current.json");
        let alias = daemon.join("current.alias");
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        // Model PID reuse: this process is alive, but it does not own the
        // daemon singleton. Recovery must use daemon.lock rather than the PID.
        let record = DaemonRecord::identified(std::process::id(), "reused-process");
        store.save(&record).unwrap();

        // A locator hardlink forces retirement to stop after its socket-first
        // step. Once the unsafe alias is repaired, the durable state is the
        // exact crash window: record + locator remain, while the socket is
        // absent. That endpoint absence must enter fenced recovery instead of
        // the raw `NotFound => start` path.
        std::fs::hard_link(&current, &alias).unwrap();
        assert_eq!(
            cleanup.retire().unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(!socket.exists());
        assert!(current.exists());
        assert_eq!(store.load().unwrap(), Some(record.clone()));
        std::fs::remove_file(alias).unwrap();
        assert_eq!(
            usagi_daemon::infrastructure::unix_transport::connect_current(data)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::ConnectionRefused
        );

        assert_eq!(
            recover_stale_client_endpoint(data).unwrap(),
            bootstrap::StaleRecovery::Recovered
        );
        assert_eq!(store.load().unwrap(), None);
        assert!(!current.exists());
        assert!(
            ExactProcessControl
                .process_start_identity(std::process::id())
                .is_ok()
        );

        // SAFETY: recovery removed only filesystem artifacts; dropping closes
        // the still-owned listener fd and its cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn client_bootstrap_recovery_and_stop_agree_on_an_unverified_owner() {
        use std::mem::ManuallyDrop;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let daemon = data.join("daemon");
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let socket = daemon.join(&listener.locator().endpoint);
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        // A legacy record carries no identity, so ownership stays undecided. Both
        // the bootstrap recovery and the lifecycle `stop` read the same
        // observation through the same domain decision, so neither reclaims it.
        let record = DaemonRecord::new(std::process::id());
        store.save(&record).unwrap();
        assert_eq!(
            ExactProcessControl.observe(&record),
            DaemonProcessObservation::Unknown
        );

        assert_eq!(
            recover_stale_client_endpoint(data).unwrap(),
            bootstrap::StaleRecovery::NotProven
        );
        assert!(
            usagi_daemon::usecase::stop::stop(
                &store,
                &ExactProcessControl,
                &SigtermTerminator,
                &RealSleeper,
                &fresh_ipc_ready(data, &info),
                &info,
            )
            .is_err()
        );
        assert_eq!(store.load().unwrap(), Some(record));
        assert!(socket.exists());
        assert!(daemon.join("current.json").exists());

        // SAFETY: the listener has not moved and still owns normal cleanup.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn client_bootstrap_recovery_requires_an_exact_lifecycle_record() {
        use std::mem::ManuallyDrop;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let daemon = data.join("daemon");
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let socket = daemon.join(&listener.locator().endpoint);

        assert_eq!(
            recover_stale_client_endpoint(data).unwrap(),
            bootstrap::StaleRecovery::NotProven
        );
        assert!(socket.exists());
        assert!(daemon.join("current.json").exists());

        // SAFETY: the listener has not moved and still owns normal cleanup.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn client_bootstrap_recovery_preserves_an_active_owner() {
        use std::mem::ManuallyDrop;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let daemon = data.join("daemon");
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let locator = listener.locator().clone();
        let socket = daemon.join(&locator.endpoint);
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        let record = DaemonRecord::identified(4242, "gone-process");
        store.save(&record).unwrap();

        assert_eq!(
            recover_stale_client_endpoint_with(
                data,
                |_lock| Ok(false),
                || panic!("post-lock effects must not run for an active owner"),
            )
            .unwrap(),
            bootstrap::StaleRecovery::OwnerActive
        );
        assert_eq!(store.load().unwrap(), Some(record));
        assert_eq!(
            usagi_daemon::infrastructure::unix_transport::read_locator(&daemon).unwrap(),
            locator
        );
        assert!(socket.exists());

        // SAFETY: the listener has not moved and still owns normal cleanup.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn client_bootstrap_recovery_preserves_a_record_replaced_after_instance_lock() {
        use std::mem::ManuallyDrop;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let daemon = data.join("daemon");
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let locator = listener.locator().clone();
        let socket = daemon.join(&locator.endpoint);
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        let old = usagi_core::domain::daemon::DaemonRecord::new(4242);
        let replacement = usagi_core::domain::daemon::DaemonRecord {
            pid: old.pid,
            process_start_identity: old.process_start_identity.clone(),
            started_at: old.started_at + chrono::Duration::nanoseconds(1),
        };
        store.save(&old).unwrap();

        assert_eq!(
            recover_stale_client_endpoint_with(data, InstanceLock::acquire, || {
                store.save(&replacement).unwrap();
            })
            .unwrap(),
            bootstrap::StaleRecovery::NotProven
        );
        assert_eq!(store.load().unwrap(), Some(replacement));
        assert_eq!(
            usagi_daemon::infrastructure::unix_transport::read_locator(&daemon).unwrap(),
            locator
        );
        assert!(socket.exists());

        // SAFETY: the listener has not moved and still owns normal cleanup.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn client_bootstrap_recovery_keeps_record_when_current_lock_is_unsafe() {
        use std::mem::ManuallyDrop;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let daemon = data.join("daemon");
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        let record = DaemonRecord::identified(4242, "gone-process");
        store.save(&record).unwrap();
        let current_lock = daemon.join("current.lock");
        std::fs::set_permissions(&current_lock, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(recover_stale_client_endpoint(data).is_err());
        assert_eq!(store.load().unwrap(), Some(record));
        assert!(daemon.join("current.json").exists());

        std::fs::set_permissions(&current_lock, std::fs::Permissions::from_mode(0o600)).unwrap();
        // SAFETY: the listener has not moved and still owns normal cleanup.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn stale_retry_clears_record_only_after_socket_first_partial_retire_commits_locator() {
        use std::mem::ManuallyDrop;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let daemon = data.join("daemon");
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let cleanup = listener.cleanup_handle();
        let socket = daemon.join(&listener.locator().endpoint);
        let current = daemon.join("current.json");
        let alias = daemon.join("current.alias");
        std::fs::hard_link(&current, &alias).unwrap();
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        let record = usagi_core::domain::daemon::DaemonRecord::new(4242);
        store.save(&record).unwrap();

        assert_eq!(
            cleanup.retire().unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(!socket.exists(), "owned socket is the first cleanup step");
        assert!(current.exists(), "unsafe locator remains the commit fence");
        assert_eq!(store.load().unwrap(), Some(record.clone()));

        std::fs::remove_file(alias).unwrap();
        let ready = fresh_ipc_ready(data, &info);
        assert_eq!(
            ready.cleanup_if(&store, &record).unwrap(),
            StaleCleanup::Cleared
        );
        assert_eq!(store.load().unwrap(), None);
        assert!(!current.exists());
        // SAFETY: the listener was not moved or dropped; cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn serve_preserves_stale_record_until_real_pre_registration_recovery_succeeds() {
        use std::mem::ManuallyDrop;
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let daemon = data.join("daemon");
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let socket = daemon.join(&listener.locator().endpoint);
        let current = daemon.join("current.json");
        let current_lock = daemon.join("current.lock");
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        let stale = usagi_core::domain::daemon::DaemonRecord::new(4242);
        store.save(&stale).unwrap();
        std::fs::set_permissions(&current_lock, std::fs::Permissions::from_mode(0o644)).unwrap();
        let publishes = Cell::new(0);

        {
            let ready = fresh_ipc_ready(data, &info);
            let recovery = RecoveryOnlyReady {
                ready: &ready,
                publishes: &publishes,
            };
            let lock = FileInstanceLock {
                path: daemon.join("daemon.lock"),
                held: RefCell::new(None),
            };
            assert!(
                usagi_daemon::usecase::serve::serve(
                    &mut Vec::new(),
                    &store,
                    &recovery,
                    &NoGenerationAuthority,
                    &ImmediateTestShutdown,
                    &AcquiredWorkspaceFence,
                    &lock,
                    &FixedIdentitySource("test:7777"),
                    7777,
                    &info,
                )
                .is_err()
            );
        }
        assert_eq!(store.load().unwrap(), Some(stale));
        assert_eq!(publishes.get(), 0);
        assert!(socket.exists());
        assert!(current.exists());

        std::fs::set_permissions(&current_lock, std::fs::Permissions::from_mode(0o600)).unwrap();
        {
            let ready = fresh_ipc_ready(data, &info);
            let recovery = RecoveryOnlyReady {
                ready: &ready,
                publishes: &publishes,
            };
            let lock = FileInstanceLock {
                path: daemon.join("daemon.lock"),
                held: RefCell::new(None),
            };
            usagi_daemon::usecase::serve::serve(
                &mut Vec::new(),
                &store,
                &recovery,
                &NoGenerationAuthority,
                &ImmediateTestShutdown,
                &AcquiredWorkspaceFence,
                &lock,
                &FixedIdentitySource("test:7777"),
                7777,
                &info,
            )
            .unwrap();
        }
        assert_eq!(publishes.get(), 1);
        assert_eq!(store.load().unwrap(), None);
        assert!(!socket.exists());
        assert!(!current.exists());
        // SAFETY: the listener was not moved or dropped; stale recovery already
        // removed its filesystem endpoint and Drop only closes the descriptor.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn stale_cleanup_preserves_a_saved_replacement_generation() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let daemon = data.join("daemon");
        let old_listener = SecureUnixListener::bind(data, ipc_generation()).unwrap();
        let replacement_listener = SecureUnixListener::bind(data, ipc_generation()).unwrap();
        let replacement_locator = replacement_listener.locator().clone();
        let replacement_socket = daemon.join(&replacement_locator.endpoint);
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        let old = usagi_core::domain::daemon::DaemonRecord::new(4242);
        let replacement = usagi_core::domain::daemon::DaemonRecord {
            pid: old.pid,
            process_start_identity: old.process_start_identity.clone(),
            started_at: old.started_at + chrono::Duration::nanoseconds(1),
        };
        store.save(&replacement).unwrap();
        let ready = fresh_ipc_ready(data, &info);

        assert_eq!(
            ready.cleanup_if(&store, &old).unwrap(),
            StaleCleanup::Superseded
        );
        assert_eq!(store.load().unwrap(), Some(replacement));
        assert_eq!(
            usagi_daemon::infrastructure::unix_transport::read_locator(&daemon).unwrap(),
            replacement_locator
        );
        assert!(replacement_socket.exists());
        let client = usagi_daemon::infrastructure::unix_transport::connect_current(data).unwrap();
        let accepted = replacement_listener.accept().unwrap();
        drop((client, accepted, old_listener, replacement_listener));
    }

    #[test]
    fn production_stop_reclaims_a_reused_pid_in_socket_first_order_without_signalling() {
        use std::mem::ManuallyDrop;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let info = daemon_test_info();
        let daemon = data.join("daemon");
        let mut listener =
            ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let socket = daemon.join(&listener.locator().endpoint);
        let current = daemon.join("current.json");
        let socket_alias = socket.with_extension("alias");
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });

        // A live, unrelated process occupies the recorded pid, which is what the
        // OS leaves behind once a crashed owner's pid is handed out again. Only
        // the identity distinguishes it from the owner, so the record is
        // byte-identical to what the crashed daemon wrote apart from that field.
        let mut occupant = Command::new("sleep").arg("30").spawn().unwrap();
        let identity = ExactProcessControl
            .process_start_identity(occupant.id())
            .unwrap();
        let record = DaemonRecord::identified(occupant.id(), format!("{identity}-crashed-owner"));
        store.save(&record).unwrap();
        assert_eq!(
            ExactProcessControl.observe(&record),
            DaemonProcessObservation::IdentityMismatch
        );

        let stop = |ready: &IpcReady<'_>| {
            usagi_daemon::usecase::stop::stop(
                &store,
                &ExactProcessControl,
                &SigtermTerminator,
                &RealSleeper,
                ready,
                &info,
            )
        };

        // A second link to the socket makes its removal unsafe, so the reclaim
        // fails at its first step. The locator surviving that failure is what
        // pins the order: it is the commit fence, retired only after the socket,
        // so this crash point stays retryable through the retained record.
        std::fs::hard_link(&socket, &socket_alias).unwrap();
        let error = stop(&fresh_ipc_ready(data, &info)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            socket.exists(),
            "the socket step failed, so nothing committed"
        );
        assert!(current.exists(), "the locator commits after the socket");
        assert_eq!(store.load().unwrap(), Some(record.clone()));
        assert!(
            occupant.try_wait().unwrap().is_none(),
            "a failed reclaim must not signal the process holding the reused pid"
        );

        std::fs::remove_file(&socket_alias).unwrap();
        assert_eq!(
            stop(&fresh_ipc_ready(data, &info)).unwrap(),
            format!("{}: cleared stale daemon record", info.describe())
        );
        assert_eq!(store.load().unwrap(), None);
        assert!(!socket.exists());
        assert!(!current.exists());
        assert!(
            occupant.try_wait().unwrap().is_none(),
            "the reclaim completed with zero signals"
        );

        occupant.kill().unwrap();
        occupant.wait().unwrap();
        // SAFETY: reclaim removed only filesystem artifacts; dropping closes the
        // still-owned listener fd and its cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    #[test]
    fn a_record_pid_that_cannot_name_a_process_reaches_no_signal_path() {
        // Neither boundary lets such a value become durable state, and the
        // terminator refuses it even if one were handed to it directly.
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let store = DaemonRecordStore::new(FsRecordFile {
            path: directory.path().join("daemon").join("daemon.json"),
        });
        for pid in [0, 1] {
            let record = DaemonRecord::identified(pid, "forged");
            assert_eq!(
                store.save(&record).unwrap_err().kind(),
                std::io::ErrorKind::InvalidInput
            );
            assert_eq!(
                SigtermTerminator.terminate(&record).unwrap_err().kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn production_stop_preserves_a_superseded_stale_record() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let daemon = directory.path().join("daemon");
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        let record =
            usagi_core::domain::daemon::DaemonRecord::identified(2_000_000_000, "test:absent");
        store.save(&record).unwrap();

        let error = usagi_daemon::usecase::stop::stop(
            &store,
            &ExactProcessControl,
            &SigtermTerminator,
            &RealSleeper,
            &SupersededCleanup,
            &daemon_test_info(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(store.load().unwrap(), Some(record));
    }

    #[test]
    fn shutdown_signal_closes_admission_before_wait_consumes_it() {
        const FIXTURE: &str = "USAGI_TEST_EARLY_DAEMON_SHUTDOWN_FLAG";
        if std::env::var_os(FIXTURE).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::daemon::tests::shutdown_signal_closes_admission_before_wait_consumes_it",
                    "--nocapture",
                ])
                .env(FIXTURE, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let admission_closed = Arc::new(ShutdownRequest::new());
        let shutdown = SignalShutdown::new(Arc::clone(&admission_closed));
        shutdown.prepare().unwrap();
        signal_hook::low_level::raise(libc::SIGTERM).unwrap();

        assert!(admission_closed.is_requested());
    }

    #[test]
    fn accept_worker_exit_wakes_shutdown_wait_without_an_os_signal() {
        const FIXTURE: &str = "USAGI_TEST_IPC_WORKER_EXIT_WAKE";
        if std::env::var_os(FIXTURE).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::daemon::tests::accept_worker_exit_wakes_shutdown_wait_without_an_os_signal",
                    "--nocapture",
                ])
                .env(FIXTURE, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let admission_closed = Arc::new(ShutdownRequest::new());
        let shutdown = SignalShutdown::new(Arc::clone(&admission_closed));
        shutdown.prepare().unwrap();
        let worker_flag = Arc::clone(&admission_closed);
        let worker = std::thread::spawn(move || {
            let _exit = ShutdownOnIpcWorkerExit {
                shutdown: worker_flag,
            };
            panic!("injected accept-worker panic");
        });

        shutdown.wait().unwrap();

        assert!(admission_closed.is_requested());
        assert!(worker.join().is_err());
    }

    struct FixedRefreshClock {
        calls: Arc<AtomicUsize>,
        shutdown_after: Option<(usize, Arc<ShutdownRequest>)>,
    }
    impl RefreshClock for FixedRefreshClock {
        fn now_ms(&self) -> u64 {
            let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
            if let Some((after, shutdown)) = &self.shutdown_after
                && call >= *after
            {
                shutdown.request();
            }
            0
        }
    }

    struct CompositionGh {
        calls: Arc<AtomicUsize>,
        inventory: SharedPrInventory,
        unlocked_during_call: Arc<AtomicBool>,
    }
    impl GhProcessPort for CompositionGh {
        type Error = ();
        fn run(&mut self, _: &str, _: &[String], _: u64) -> Result<String, ()> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.unlocked_during_call
                .store(self.inventory.try_lock().is_ok(), Ordering::Release);
            Ok("{\"title\":\"production\",\"state\":\"MERGED\"}".into())
        }
    }

    #[test]
    fn production_pr_worker_rebuilds_publishes_without_locking_and_honors_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let identity =
            usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/493")
                .unwrap();
        let inventory = Arc::new(Mutex::new(OutputPrProjector::new(PrInventoryStore::new(
            directory.path(),
        ))));
        inventory
            .lock()
            .unwrap()
            .observe_committed(
                TerminalId::new(),
                Some(session),
                // The newline terminates the candidate. Without it the projector
                // carries the token into the next chunk instead of crediting a
                // token the output may not have finished writing.
                format!("{}\n", identity.as_url()).as_bytes(),
            )
            .unwrap();
        let shutdown = Arc::new(ShutdownRequest::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let unlocked = Arc::new(AtomicBool::new(false));
        let handle = spawn_pr_refresh_worker(
            Arc::clone(&inventory),
            Arc::clone(&shutdown),
            CompositionGh {
                calls: Arc::clone(&calls),
                inventory: Arc::clone(&inventory),
                unlocked_during_call: Arc::clone(&unlocked),
            },
            FixedRefreshClock {
                calls: Arc::new(AtomicUsize::new(0)),
                shutdown_after: Some((3, Arc::clone(&shutdown))),
            },
            Duration::from_millis(1),
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert!(unlocked.load(Ordering::Acquire));
        let snapshot = inventory.lock().unwrap().snapshot(session).unwrap();
        assert_eq!(snapshot.entries[0].title.as_deref(), Some("production"));

        let cancelled = Arc::new(ShutdownRequest::new());
        cancelled.request();
        let cancelled_calls = Arc::new(AtomicUsize::new(0));
        let handle = spawn_pr_refresh_worker(
            Arc::clone(&inventory),
            Arc::clone(&cancelled),
            CompositionGh {
                calls: Arc::clone(&cancelled_calls),
                inventory,
                unlocked_during_call: Arc::new(AtomicBool::new(false)),
            },
            FixedRefreshClock {
                calls: Arc::new(AtomicUsize::new(0)),
                shutdown_after: None,
            },
            Duration::from_millis(1),
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(cancelled_calls.load(Ordering::Acquire), 0);
    }

    /// A teardown journal whose pending set drains as it is finalized, plus a
    /// scripted effect failure, so the worker's logging arms are both exercised.
    struct FakeTeardownJournal {
        pending: Arc<Mutex<Vec<usagi_daemon::usecase::session_teardown::PendingTeardown>>>,
        finalize_error: Option<String>,
    }
    impl TeardownJournal for FakeTeardownJournal {
        fn pending(&self) -> Vec<usagi_daemon::usecase::session_teardown::PendingTeardown> {
            self.pending.lock().unwrap().clone()
        }
        fn finish(
            &self,
            teardown: &usagi_daemon::usecase::session_teardown::PendingTeardown,
            _outcome: Result<(), String>,
        ) -> Result<(), String> {
            self.pending
                .lock()
                .unwrap()
                .retain(|pending| pending.name != teardown.name);
            self.finalize_error.clone().map_or(Ok(()), Err)
        }
    }

    struct FakeTeardownEffect {
        torn_down: Arc<Mutex<Vec<String>>>,
        shutdown: Arc<ShutdownRequest>,
    }
    impl TeardownEffect for FakeTeardownEffect {
        fn tear_down(
            &self,
            teardown: &usagi_daemon::usecase::session_teardown::PendingTeardown,
        ) -> Result<(), String> {
            self.torn_down.lock().unwrap().push(teardown.name.clone());
            // End the worker as soon as it has taken the admitted work, so the
            // test observes exactly one drain.
            self.shutdown.request();
            Err("worktree is busy".into())
        }
    }

    #[test]
    fn production_teardown_worker_drains_an_admitted_removal_and_honors_shutdown() {
        let pending = Arc::new(Mutex::new(vec![
            usagi_daemon::usecase::session_teardown::PendingTeardown {
                session_id: SessionId::new(),
                operation_id: usagi_core::domain::id::OperationId::new(),
                name: "one".into(),
                session_root: PathBuf::from("/repo/.usagi/sessions/one"),
                force: false,
            },
        ]));
        let shutdown = Arc::new(ShutdownRequest::new());
        let torn_down = Arc::new(Mutex::new(Vec::new()));
        let signal = Arc::new(TeardownSignal::new());

        let handle = spawn_session_teardown_worker(
            FakeTeardownJournal {
                pending: Arc::clone(&pending),
                finalize_error: Some("session lifecycle owner is unavailable".into()),
            },
            FakeTeardownEffect {
                torn_down: Arc::clone(&torn_down),
                shutdown: Arc::clone(&shutdown),
            },
            Arc::clone(&signal),
            Arc::clone(&shutdown),
            Duration::from_millis(1),
        )
        .unwrap();
        handle.join().unwrap();

        assert_eq!(torn_down.lock().unwrap().as_slice(), ["one"]);
        assert!(pending.lock().unwrap().is_empty());

        // A worker started under shutdown takes no work at all.
        let already_stopped = Arc::new(ShutdownRequest::new());
        already_stopped.request();
        let untouched = Arc::new(Mutex::new(Vec::new()));
        spawn_session_teardown_worker(
            FakeTeardownJournal {
                pending: Arc::clone(&pending),
                finalize_error: None,
            },
            FakeTeardownEffect {
                torn_down: Arc::clone(&untouched),
                shutdown: Arc::clone(&already_stopped),
            },
            signal,
            already_stopped,
            Duration::from_millis(1),
        )
        .unwrap()
        .join()
        .unwrap();
        assert!(untouched.lock().unwrap().is_empty());
    }

    /// Prepares `<data>/daemon` with an acquired instance lock and a registered
    /// owner record, exactly as `serve` leaves it before publishing, and returns
    /// the production custody probe built from that state.
    fn custody_fixture(data_dir: &Path) -> (FileInstanceLock, DaemonRecord, FsCustodyProbe) {
        let daemon_dir = data_dir.join("daemon");
        ensure_private_dir_all(&daemon_dir).unwrap();
        let lock = FileInstanceLock {
            path: daemon_dir.join("daemon.lock"),
            held: RefCell::new(None),
        };
        assert!(lock.acquire().unwrap());
        let record = FsRecordFile {
            path: daemon_dir.join("daemon.json"),
        };
        let owner = DaemonRecord::identified(std::process::id(), "custody:test");
        DaemonRecordStore::new(FsRecordFile {
            path: daemon_dir.join("daemon.json"),
        })
        .save(&owner)
        .unwrap();
        let probe = FsCustodyProbe {
            locked: lock.locked_inode(),
            lock_path: daemon_dir.join("daemon.lock"),
            record,
        };
        (lock, owner, probe)
    }

    fn custody_worker(
        probe: FsCustodyProbe,
        owner: DaemonRecord,
        data_dir: &Path,
        shutdown: &Arc<ShutdownRequest>,
    ) -> std::thread::JoinHandle<()> {
        spawn_custody_worker(
            probe,
            owner,
            data_dir.to_path_buf(),
            Arc::clone(shutdown),
            Duration::from_millis(5),
        )
        .unwrap()
    }

    fn wait_for_request(shutdown: &ShutdownRequest, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if shutdown.is_requested() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        shutdown.is_requested()
    }

    #[test]
    fn production_custody_probe_observes_the_locked_inode_and_the_owner_record() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let (lock, owner, probe) = custody_fixture(home.path());
        let daemon_dir = home.path().join("daemon");

        assert_eq!(
            usagi_daemon::usecase::custody::evaluate(&probe, &owner).unwrap(),
            Custody::Held
        );

        // Replacing the pathname cannot forge the identity: it is read from the
        // descriptor this process locked, not from the path.
        let replacement = daemon_dir.join("replacement.lock");
        std::fs::write(&replacement, "").unwrap();
        std::fs::rename(&replacement, daemon_dir.join("daemon.lock")).unwrap();
        assert_eq!(
            usagi_daemon::usecase::custody::evaluate(&probe, &owner).unwrap(),
            Custody::Lost(usagi_daemon::usecase::custody::CustodyLoss::LockInodeReplaced)
        );
        drop(lock);

        // A malformed record is an undecidable observation, never a loss.
        std::fs::write(daemon_dir.join("daemon.json"), "not json").unwrap();
        std::fs::remove_file(daemon_dir.join("daemon.lock")).unwrap();
        std::fs::write(daemon_dir.join("daemon.lock"), "").unwrap();
        let unobserved = FsCustodyProbe {
            locked: None,
            lock_path: daemon_dir.join("daemon.lock"),
            record: FsRecordFile {
                path: daemon_dir.join("daemon.json"),
            },
        };
        assert!(usagi_daemon::usecase::custody::evaluate(&unobserved, &owner).is_err());
    }

    #[test]
    fn production_custody_worker_requests_shutdown_when_the_lock_path_disappears() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let (lock, owner, probe) = custody_fixture(home.path());
        let shutdown = Arc::new(ShutdownRequest::new());
        let handle = custody_worker(probe, owner, home.path(), &shutdown);

        // A live daemon keeps serving across ticks.
        assert!(!wait_for_request(&shutdown, Duration::from_millis(50)));

        std::fs::remove_file(home.path().join("daemon/daemon.lock")).unwrap();
        assert!(wait_for_request(&shutdown, Duration::from_secs(5)));
        handle.join().unwrap();
        drop(lock);
    }

    #[test]
    fn production_custody_worker_requests_shutdown_when_another_owner_takes_the_record() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let (lock, owner, probe) = custody_fixture(home.path());
        let shutdown = Arc::new(ShutdownRequest::new());
        let handle = custody_worker(probe, owner, home.path(), &shutdown);

        DaemonRecordStore::new(FsRecordFile {
            path: home.path().join("daemon/daemon.json"),
        })
        .save(&DaemonRecord::identified(4321, "custody:replacement"))
        .unwrap();
        assert!(wait_for_request(&shutdown, Duration::from_secs(5)));
        handle.join().unwrap();
        drop(lock);
    }

    #[test]
    fn production_custody_worker_stops_at_an_already_requested_shutdown() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let (lock, owner, probe) = custody_fixture(home.path());
        let shutdown = Arc::new(ShutdownRequest::new());
        shutdown.request();
        custody_worker(probe, owner, home.path(), &shutdown)
            .join()
            .unwrap();
        // The record and lock were left untouched by the supervisor itself.
        assert!(home.path().join("daemon/daemon.json").is_file());
        drop(lock);
    }

    #[test]
    fn a_deleted_data_directory_makes_endpoint_and_record_cleanup_a_successful_no_op() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let data_dir = home.path().join("local");
        let (lock, owner, _) = custody_fixture(&data_dir);
        let record = FsRecordFile {
            path: data_dir.join("daemon/daemon.json"),
        };
        let contents = serde_json::to_string(&owner).unwrap();
        let info = daemon_test_info();
        let ready = fresh_ipc_ready(&data_dir, &info);

        drop(lock);
        std::fs::remove_dir_all(&data_dir).unwrap();

        // Neither step re-creates the released tree, and both succeed so the
        // daemon exits through its ordinary path rather than failing closed.
        DaemonReady::retire(&ready).unwrap();
        assert!(!RecordFile::remove_if(&record, &contents).unwrap());
        assert!(!data_dir.exists());
    }

    #[test]
    fn decision_maintenance_never_writes_when_nothing_is_due_and_honors_shutdown() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let daemon = home.path().join("daemon");
        std::fs::create_dir_all(&daemon).unwrap();
        let decisions = Arc::new(UserDecisionStore::new(daemon.clone()));
        let store_path = decisions.path();
        let shutdown = Arc::new(ShutdownRequest::new());
        let stopper = Arc::clone(&shutdown);

        let handle =
            spawn_decision_maintenance(decisions, shutdown, Duration::from_millis(1)).unwrap();
        // Let several ticks run, then stop: the worker must observe the request
        // rather than needing its tick to be short.
        std::thread::sleep(Duration::from_millis(30));
        stopper.request();
        handle.join().unwrap();

        // An idle tick decides "nothing is due" from a lock-free read, so it must
        // not have created the durable document at all — no fsync, no store lock.
        assert!(
            !store_path.exists(),
            "an idle maintenance tick must not write the decision store"
        );
        assert!(
            !daemon
                .join(usagi_core::infrastructure::persistence::store_lock::LOCK_FILE_NAME)
                .exists(),
            "an idle maintenance tick must not take the store lock"
        );
    }

    #[test]
    fn decision_maintenance_stops_at_an_already_requested_shutdown() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let daemon = home.path().join("daemon");
        std::fs::create_dir_all(&daemon).unwrap();
        let shutdown = Arc::new(ShutdownRequest::new());
        shutdown.request();
        spawn_decision_maintenance(
            Arc::new(UserDecisionStore::new(daemon)),
            shutdown,
            Duration::from_secs(30),
        )
        .unwrap()
        .join()
        .unwrap();
    }

    #[test]
    fn the_retention_collector_ticks_until_shutdown_and_stops_when_already_down() {
        let shutdown = Arc::new(ShutdownRequest::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let ticking = Arc::clone(&calls);
        let stopper = Arc::clone(&shutdown);
        let handle = spawn_retention_gc_worker(
            move || {
                if ticking.fetch_add(1, Ordering::AcqRel) >= 1 {
                    stopper.request();
                }
            },
            Arc::clone(&shutdown),
            Duration::from_millis(1),
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 2);

        // A daemon already shutting down never collects.
        let cancelled = Arc::new(ShutdownRequest::new());
        cancelled.request();
        let skipped = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&skipped);
        let handle = spawn_retention_gc_worker(
            move || {
                counter.fetch_add(1, Ordering::AcqRel);
            },
            cancelled,
            Duration::from_millis(1),
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(skipped.load(Ordering::Acquire), 0);
    }

    fn session_test_hello() -> usagi_core::infrastructure::ipc::ServerHello {
        use usagi_core::infrastructure::ipc::{
            BuildIdentity, ConnectionId, DaemonGeneration, GenerationRole, ProtocolLimits,
            ProtocolVersion,
        };
        usagi_core::infrastructure::ipc::ServerHello {
            connection_nonce: "test".into(),
            connection_id: ConnectionId("connection".into()),
            daemon_generation: DaemonGeneration("generation".into()),
            generation_role: GenerationRole::Active,
            protocol: ProtocolVersion {
                generation: 1,
                revision: 0,
            },
            capabilities: vec![],
            build: BuildIdentity {
                version: "test".into(),
                commit: "test".into(),
                target: "test".into(),
                artifact: "test-artifact".into(),
            },
            limits: ProtocolLimits::default(),
            daemon_process: None,
        }
    }

    fn metrics_response(
        broker: &SharedMetricsBroker,
        sampler: &SharedProcessResourceSampler,
        pipeline: &TerminalPipelineMetrics,
        observer: &mut Option<MetricsObserver>,
        action: usagi_core::usecase::client::MetricsAction,
    ) -> usagi_core::usecase::client::DaemonMetrics {
        use usagi_core::infrastructure::ipc::{EnvelopeKind, ResponseOutcome};
        use usagi_core::usecase::client::DaemonRequest;

        let response = dispatch_metrics(
            broker,
            sampler,
            pipeline,
            observer,
            usagi_core::infrastructure::ipc::RequestId("metrics".into()),
            &serde_json::to_value(DaemonRequest::Metrics { action }).unwrap(),
            &session_test_hello(),
        );
        let EnvelopeKind::Response { outcome, body, .. } = response.kind else {
            panic!("metrics dispatch must produce a response")
        };
        assert_eq!(outcome, ResponseOutcome::Ok);
        serde_json::from_value(body).unwrap()
    }

    #[test]
    fn production_metrics_composition_shares_broker_lifecycle_and_resets_on_restart() {
        use usagi_core::usecase::client::MetricsAction;

        let broker = Arc::new(Mutex::new(MetricsBroker::default()));
        let sampler = Arc::new(Mutex::new(ProcessResourceSampler { previous: None }));
        let pipeline = TerminalPipelineMetrics::default();
        let mut slow = None;
        let mut fast = None;
        assert_eq!(
            metrics_response(
                &broker,
                &sampler,
                &pipeline,
                &mut slow,
                MetricsAction::Subscribe,
            )
            .active_subscribers,
            1
        );
        assert_eq!(
            metrics_response(
                &broker,
                &sampler,
                &pipeline,
                &mut fast,
                MetricsAction::Subscribe,
            )
            .active_subscribers,
            2
        );

        let mut snapshot_client = None;
        metrics_response(
            &broker,
            &sampler,
            &pipeline,
            &mut snapshot_client,
            MetricsAction::Snapshot,
        );
        assert!(fast.as_ref().unwrap().try_recv().is_ok());
        let snapshot = metrics_response(
            &broker,
            &sampler,
            &pipeline,
            &mut snapshot_client,
            MetricsAction::Snapshot,
        );
        assert_eq!(snapshot.active_subscribers, 2);
        assert_eq!(snapshot.dropped_updates, 1);
        assert!(fast.as_ref().unwrap().try_recv().is_ok());

        assert_eq!(
            metrics_response(
                &broker,
                &sampler,
                &pipeline,
                &mut fast,
                MetricsAction::Unsubscribe,
            )
            .active_subscribers,
            1
        );
        let disconnected = slow.take().unwrap();
        broker
            .lock()
            .unwrap()
            .unsubscribe(disconnected.subscription());
        assert_eq!(broker.lock().unwrap().snapshot().active_subscribers, 0);

        let restarted = Arc::new(Mutex::new(MetricsBroker::default()));
        let restarted_sampler = Arc::new(Mutex::new(ProcessResourceSampler { previous: None }));
        let restarted_snapshot = metrics_response(
            &restarted,
            &restarted_sampler,
            &pipeline,
            &mut snapshot_client,
            MetricsAction::Snapshot,
        );
        assert_eq!(restarted_snapshot.active_subscribers, 0);
        assert_eq!(restarted_snapshot.dropped_updates, 0);
    }

    #[test]
    fn failed_create_and_remove_replay_as_error_envelopes_without_success_hooks() {
        use usagi_core::infrastructure::ipc::{EnvelopeKind, ErrorCode, ResponseOutcome};
        use usagi_core::usecase::client::SessionAction;

        for action in [SessionAction::Create, SessionAction::Remove] {
            let response = session_response_envelope(
                action,
                &serde_json::json!({"name":"one"}),
                Err(SessionRuntimeError::DurableFailure(
                    "durable session failure".into(),
                )),
                usagi_core::infrastructure::ipc::RequestId("request".into()),
                &session_test_hello(),
            );
            let EnvelopeKind::Response { outcome, body, .. } = response.kind else {
                panic!("session dispatch must produce a response")
            };
            assert_eq!(body, serde_json::Value::Null);
            let ResponseOutcome::Error(error) = outcome else {
                panic!("failed session replay must not be accepted")
            };
            assert_eq!(error.code, ErrorCode::InvalidArgument);
            assert_eq!(error.message, "durable session failure");
            assert!(body.get("hook").is_none());
        }
    }

    #[test]
    fn product_mcp_arguments_start_usagi_mcp_from_the_daemon_binary() {
        let command = Path::new("/opt/usagi/bin/usagi");

        assert_eq!(
            codex_integration_arguments(command).unwrap(),
            [
                "-c",
                "mcp_servers.usagi.command = \"/opt/usagi/bin/usagi\"",
                "-c",
                "mcp_servers.usagi.args = [\"mcp\"]",
                "-c",
                "mcp_servers.usagi.env_vars = [\"USAGI_HOME\", \"USAGI_RUNTIME_MODE\", \"USAGI_WORKSPACE_ROOT\", \"USAGI_MCP_CALLER_CREDENTIAL\"]",
                "-c",
                "mcp_servers.usagi.default_tools_approval_mode = \"approve\"",
                "-c",
                "features.hooks = true",
                "-c",
                "hooks.SessionStart = [{ matcher = \"^startup$\", hooks = [{ type = \"command\", command = \"'/opt/usagi/bin/usagi' codex-session-capture\", timeout = 10 }] }]",
            ]
        );
        assert_eq!(
            claude_mcp_arguments(command).unwrap(),
            [
                "--mcp-config",
                r#"{"mcpServers":{"usagi":{"args":["mcp"],"command":"/opt/usagi/bin/usagi"}}}"#,
                "--allowedTools",
                "mcp__usagi",
            ]
        );
    }

    fn provision_context(session: Option<SessionId>) -> ProvisionContext {
        ProvisionContext {
            scope: usagi_core::domain::agent::LaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: session,
                worktree_id: WorktreeId::new(),
            },
            inject_mcp: true,
        }
    }

    #[test]
    fn a_session_claude_is_confined_to_its_worktree_and_gets_the_guard_hook() {
        let usagi = Path::new("/opt/usagi/bin/usagi");
        let context = provision_context(Some(SessionId::new()));
        let mode = sandbox_mode(&context);
        assert_eq!(mode, SandboxMode::Session);

        let roots = claude_writable_roots(
            Path::new("/repo/.usagi/sessions/work"),
            Path::new("/repo"),
            Path::new("/home/dev/.usagi"),
        );
        assert_eq!(
            roots,
            [
                PathBuf::from("/repo/.usagi/sessions/work"),
                PathBuf::from("/repo/.usagi"),
                PathBuf::from("/repo/.git"),
                PathBuf::from("/home/dev/.usagi"),
            ]
        );
        // The workspace root itself is not writable for a session launch.
        assert!(!roots.contains(&PathBuf::from("/repo")));

        let launcher = claude_sandbox_launcher(usagi, mode, &roots).unwrap();
        assert_eq!(launcher.program, "/opt/usagi/bin/usagi");
        assert_eq!(
            launcher.prefix,
            [
                "claude-sandbox",
                "--mode",
                "session",
                "--writable-root",
                "/repo/.usagi/sessions/work",
                "--writable-root",
                "/repo/.usagi",
                "--writable-root",
                "/repo/.git",
                "--writable-root",
                "/home/dev/.usagi",
                "--",
            ]
        );

        let arguments = claude_settings_arguments(usagi, mode).unwrap();
        assert_eq!(arguments[0], "--settings");
        let settings: serde_json::Value = serde_json::from_str(&arguments[1]).unwrap();
        let pre_tool_use = settings["hooks"]["PreToolUse"][0]["hooks"]
            .as_array()
            .unwrap();
        assert_eq!(
            pre_tool_use[1]["command"],
            serde_json::json!("'/opt/usagi/bin/usagi' guard-workspace")
        );
        assert_eq!(
            settings["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            serde_json::json!("'/opt/usagi/bin/usagi' agent-phase ready")
        );
    }

    #[test]
    fn a_root_claude_is_confined_to_the_project_root_without_the_guard_hook() {
        let usagi = Path::new("/opt/usagi/bin/usagi");
        let mode = sandbox_mode(&provision_context(None));
        assert_eq!(mode, SandboxMode::Root);

        // A root launch's cwd *is* the project root, so that root is writable.
        let roots = claude_writable_roots(
            Path::new("/repo"),
            Path::new("/repo"),
            Path::new("/home/dev/.usagi"),
        );
        assert!(roots.contains(&PathBuf::from("/repo")));
        let launcher = claude_sandbox_launcher(usagi, mode, &roots).unwrap();
        assert_eq!(&launcher.prefix[..3], ["claude-sandbox", "--mode", "root"]);
        assert_eq!(launcher.prefix.last().unwrap(), "--");

        let arguments = claude_settings_arguments(usagi, mode).unwrap();
        assert!(!arguments[1].contains("guard-workspace"));
        // Lifecycle phase reporting stays wired for a root coordinator.
        assert!(arguments[1].contains("agent-phase running"));
    }

    #[derive(Clone)]
    struct TestTerminalScope {
        scope: TerminalLaunchScope,
        working_directory: PathBuf,
    }

    impl TerminalScopeResolver for TestTerminalScope {
        fn resolve_available_scope(
            &self,
            scope: &TerminalLaunchScope,
        ) -> Result<ResolvedTerminalScope, TerminalScopeResolveError> {
            (scope == &self.scope)
                .then(|| ResolvedTerminalScope {
                    scope: self.scope.clone(),
                    working_directory: self.working_directory.clone(),
                })
                .ok_or(TerminalScopeResolveError::Unavailable)
        }
    }

    #[derive(Default)]
    struct TestTerminalStore;

    impl TerminalStore for TestTerminalStore {
        fn save(&mut self, _: TerminalStoreSnapshot) -> Result<(), ()> {
            Ok(())
        }
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct RestartEffects {
        spawns: usize,
        selections: usize,
        resizes: usize,
        writes: usize,
    }

    struct RestartPty(Arc<Mutex<RestartEffects>>);

    impl GenericPtySpawner for RestartPty {
        fn spawn(
            &mut self,
            _: &usagi_core::domain::terminal_launch::ResolvedTerminalLaunch,
            _: &TerminalRef,
            _: Geometry,
        ) -> Result<ProcessIdentity, SpawnFailure> {
            self.0.lock().unwrap().spawns += 1;
            Ok(ProcessIdentity {
                pid: 7,
                start_identity: "restart-test".to_owned(),
                process_group: 7,
            })
        }
    }

    impl PtyWriter for RestartPty {
        fn select_terminal(&mut self, _: &TerminalRef) {
            self.0.lock().unwrap().selections += 1;
        }

        fn resize(&mut self, _: &TerminalRef, _: Geometry) -> Result<(), PtyWriteError> {
            self.0.lock().unwrap().resizes += 1;
            Ok(())
        }

        fn write_all(&mut self, _: &[u8]) -> Result<(), PtyWriteError> {
            self.0.lock().unwrap().writes += 1;
            Ok(())
        }
    }

    #[test]
    fn generic_pty_reports_child_exit_after_the_shell_exits() {
        let directory = tempfile::tempdir().unwrap();
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        };
        let request = TerminalLaunchRequest {
            profile_id: TerminalProfileId::new("login-shell").unwrap(),
            scope: TerminalLaunchScope {
                workspace_id: terminal.workspace_id,
                session_id: terminal.session_id,
                worktree_id: terminal.worktree_id,
            },
        };
        let launch = TrustedLoginShell {
            profile: LoginShellProfile::new(BTreeMap::new(), directory.path().to_path_buf()),
            environment: None,
            workspace_root: PathBuf::new(),
        }
        .resolve(&request)
        .unwrap();
        let metrics = Arc::new(TerminalPipelineMetrics::default());
        let (mut pty, observations) = DaemonPty::new(metrics);

        pty.spawn(&launch, &terminal, Geometry { cols: 80, rows: 24 })
            .unwrap();
        pty.resize(&terminal, Geometry { cols: 91, rows: 37 })
            .unwrap();
        pty.select_terminal(&terminal);
        pty.write_all(b"exit\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match observations.recv_timeout(remaining).unwrap() {
                PtyObservation::Output(_, _) => {}
                PtyObservation::Exited(exited, status) => {
                    assert_eq!(exited, terminal);
                    assert_eq!(status, 0);
                    break;
                }
            }
        }
    }

    #[test]
    fn full_pty_observation_queue_backpressures_without_reordering() {
        let metrics = Arc::new(TerminalPipelineMetrics::default());
        let terminal = TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(PtyObservation::Output(terminal.clone(), vec![1]))
            .unwrap();
        let blocked_sender = sender.clone();
        let blocked_metrics = Arc::clone(&metrics);
        let blocked_terminal = terminal.clone();
        let producer = std::thread::spawn(move || {
            send_pty_observation(
                &blocked_sender,
                PtyObservation::Output(blocked_terminal.clone(), vec![2; 7]),
                7,
                &blocked_metrics,
            )
            .unwrap();
            blocked_sender
                .send(PtyObservation::Exited(blocked_terminal, 0))
                .unwrap();
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        while metrics.backpressured_bytes.load(Ordering::Relaxed) == 0 && Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert_eq!(metrics.backpressured_bytes.load(Ordering::Relaxed), 7);
        assert!(matches!(
            receiver.recv().unwrap(),
            PtyObservation::Output(_, bytes) if bytes == [1]
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            PtyObservation::Output(_, bytes) if bytes == [2; 7]
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            PtyObservation::Exited(actual, 0) if actual == terminal
        ));
        producer.join().unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One isolated process covers both real PTY transport owners.
    fn exited_generic_and_agent_pty_transports_return_to_the_fd_baseline() {
        const TERMINALS_PER_OWNER: usize = 24;
        const FD_TOLERANCE: usize = 4;

        if std::env::var_os("USAGI_PTY_RECLAIM_TEST_HELPER").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::daemon::tests::exited_generic_and_agent_pty_transports_return_to_the_fd_baseline",
                    "--nocapture",
                ])
                .env("USAGI_PTY_RECLAIM_TEST_HELPER", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }

        let baseline = std::fs::read_dir("/dev/fd").unwrap().count();
        let metrics = Arc::new(TerminalPipelineMetrics::default());
        let (mut generic, generic_observations) = DaemonPty::new(Arc::clone(&metrics));
        let (mut agent, agent_observations) = AgentPty::new(BTreeMap::new(), metrics);
        let generation = DaemonGeneration::new();

        let generic_scope = TerminalLaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        };
        let generic_request = TerminalLaunchRequest {
            profile_id: TerminalProfileId::new("login-shell").unwrap(),
            scope: generic_scope.clone(),
        };
        let generic_launch = usagi_core::domain::terminal_launch::ResolvedTerminalLaunch::new(
            usagi_core::domain::terminal_launch::DurableTerminalLaunchSnapshot::new(
                generic_request,
                1,
                "/bin/sh",
                vec![
                    "-c".to_owned(),
                    "printf generic-final; sleep 0.01".to_owned(),
                ],
                PathBuf::from("/"),
                [],
            )
            .unwrap(),
            BTreeMap::new(),
        )
        .unwrap();
        let generic_terminals = (0..TERMINALS_PER_OWNER)
            .map(|_| TerminalRef {
                daemon_generation: generation,
                terminal_id: TerminalId::new(),
                workspace_id: generic_scope.workspace_id,
                session_id: generic_scope.session_id,
                worktree_id: generic_scope.worktree_id,
            })
            .collect::<Vec<_>>();
        for terminal in &generic_terminals {
            generic
                .spawn(&generic_launch, terminal, Geometry { cols: 80, rows: 24 })
                .unwrap();
        }
        reclaim_generic_observations(&mut generic, &generic_observations, TERMINALS_PER_OWNER);
        assert!(generic.terminals.is_empty());

        let profile = AgentProfileId::new("codex").unwrap();
        let agent_scope = usagi_core::domain::agent::LaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        };
        let agent_request = usagi_core::domain::agent::LaunchRequest {
            profile_id: profile.clone(),
            mode: usagi_core::domain::agent::LaunchMode::Interactive,
            model: None,
            resume: false,
            provider_resume: None,
            initial_prompt: None,
            scope: agent_scope.clone(),
            required_capabilities: BTreeSet::new(),
        };
        let plan = usagi_core::domain::agent::LaunchPlan::new(
            profile,
            1,
            "/bin/sh",
            vec!["-c".to_owned(), "printf agent-final; sleep 0.01".to_owned()],
            [],
            PathBuf::from("/"),
        )
        .unwrap();
        let agent_launch = DurableLaunchSnapshot::new(agent_request, plan);
        let agent_terminals = (0..TERMINALS_PER_OWNER)
            .map(|_| TerminalRef {
                daemon_generation: generation,
                terminal_id: TerminalId::new(),
                workspace_id: agent_scope.workspace_id,
                session_id: agent_scope.session_id,
                worktree_id: agent_scope.worktree_id,
            })
            .collect::<Vec<_>>();
        for terminal in &agent_terminals {
            agent
                .spawn(
                    &agent_launch,
                    &SpawnProvision::new([], Vec::new()),
                    terminal,
                )
                .unwrap();
        }
        reclaim_agent_observations(&mut agent, &agent_observations, TERMINALS_PER_OWNER);
        assert!(agent.terminals.is_empty());

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let current = std::fs::read_dir("/dev/fd").unwrap().count();
            if current <= baseline + FD_TOLERANCE {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "PTY FDs did not return near baseline"
            );
            std::thread::yield_now();
        }
    }

    fn reclaim_generic_observations(
        pty: &mut DaemonPty,
        observations: &Receiver<PtyObservation>,
        expected_exits: usize,
    ) {
        let mut output = BTreeSet::new();
        let mut exits = 0;
        while exits != expected_exits {
            match observations.recv_timeout(Duration::from_secs(5)).unwrap() {
                PtyObservation::Output(terminal, bytes) => {
                    assert!(!bytes.is_empty());
                    output.insert(terminal.terminal_id.as_str().clone());
                }
                PtyObservation::Exited(terminal, 0) => {
                    assert!(output.contains(&terminal.terminal_id.as_str()));
                    assert!(pty.release(&terminal));
                    assert!(!pty.release(&terminal));
                    exits += 1;
                }
                PtyObservation::Exited(_, status) => panic!("unexpected exit status {status}"),
            }
        }
    }

    fn reclaim_agent_observations(
        pty: &mut AgentPty,
        observations: &Receiver<AgentPtyObservation>,
        expected_exits: usize,
    ) {
        let mut output = BTreeSet::new();
        let mut exits = 0;
        while exits != expected_exits {
            match observations.recv_timeout(Duration::from_secs(5)).unwrap() {
                AgentPtyObservation::Output(terminal, bytes) => {
                    assert!(!bytes.is_empty());
                    output.insert(terminal.terminal_id.as_str().clone());
                }
                AgentPtyObservation::Exited(terminal, 0) => {
                    assert!(output.contains(&terminal.terminal_id.as_str()));
                    assert!(pty.release(&terminal));
                    assert!(!pty.release(&terminal));
                    exits += 1;
                }
                AgentPtyObservation::Exited(_, status) => {
                    panic!("unexpected exit status {status}");
                }
            }
        }
    }

    /// Waits for `condition`, failing the test rather than hanging if the
    /// projection worker never applies the queued work.
    fn await_projection(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "the projection worker did not apply queued work"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_projection_worker_owns_every_scan_and_durable_write() {
        let directory = tempfile::tempdir().unwrap();
        let projector = Arc::new(Mutex::new(OutputPrProjector::new(PrInventoryStore::new(
            directory.path(),
        ))));
        let projection = Arc::new(PrProjectionQueue::new());
        start_pr_projection_worker(Arc::clone(&projector), Arc::clone(&projection)).unwrap();
        let session = SessionId::new();
        let terminal = TerminalId::new();

        // A terminated candidate is credited by the worker, not by the submitter.
        projection.submit_output(
            terminal,
            Some(session),
            b"opened https://github.com/o/r/pull/11\n".to_vec(),
        );
        await_projection(|| {
            projector
                .lock()
                .is_ok_and(|mut projector| !projector.snapshot(session).unwrap().entries.is_empty())
        });

        // A gap must discard the carry instead of joining across dropped bytes.
        projection.submit_output(
            terminal,
            Some(session),
            b" https://github.com/o/r/pu".to_vec(),
        );
        projection.submit_gap(terminal);
        projection.submit_output(terminal, Some(session), b"ll/12\n".to_vec());
        // A candidate the output never terminated is credited when the terminal
        // closes, and not before.
        projection.submit_output(
            terminal,
            Some(session),
            b" https://github.com/o/r/pull/13".to_vec(),
        );
        projection.submit_closed(terminal, Some(session));
        await_projection(|| {
            projector
                .lock()
                .is_ok_and(|mut projector| projector.snapshot(session).unwrap().entries.len() == 2)
        });
        let urls: Vec<String> = projector
            .lock()
            .unwrap()
            .snapshot(session)
            .unwrap()
            .entries
            .iter()
            .map(|entry| entry.identity.as_url().to_owned())
            .collect();
        assert_eq!(
            urls,
            [
                "https://github.com/o/r/pull/11",
                "https://github.com/o/r/pull/13"
            ],
            "pull/12 was split across a gap and must not be synthesized"
        );

        // Closing retires the worker: `recv` returns `None` once drained. The
        // accept worker's guard is what closes it in production, including on an
        // unwind, so the guard's drop is the path under test.
        drop(ClosePrProjectionOnExit {
            projection: Arc::clone(&projection),
        });
        assert_eq!(projection.recv(), None);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // PTY-to-IPC exit observation is one integration scenario.
    fn generic_terminal_exit_reaches_its_resume_response() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let scope = TerminalLaunchScope {
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: worktree,
        };
        let metrics = Arc::new(TerminalPipelineMetrics::default());
        let (pty, observations) = DaemonPty::new(metrics);
        let runtime = Arc::new(Mutex::new(GenericTerminalRuntime::new(
            DaemonGeneration::new(),
            TrustedLoginShell {
                profile: LoginShellProfile::new(BTreeMap::new(), directory.path().to_path_buf()),
                environment: None,
                workspace_root: PathBuf::new(),
            },
            TestTerminalStore,
            pty,
            TestTerminalScope {
                scope: scope.clone(),
                working_directory: directory.path().to_path_buf(),
            },
        )));
        let projection = Arc::new(PrProjectionQueue::new());
        start_terminal_observer(Arc::clone(&runtime), observations, Arc::clone(&projection))
            .unwrap();
        start_pr_projection_worker(
            Arc::new(Mutex::new(OutputPrProjector::new(PrInventoryStore::new(
                directory.path(),
            )))),
            Arc::clone(&projection),
        )
        .unwrap();
        let connection = ConnectionId::new();
        let client = ClientId::new();
        let launch = TerminalLaunchIntent {
            request: TerminalLaunchRequest {
                profile_id: TerminalProfileId::new("login-shell").unwrap(),
                scope,
            },
            geometry: TerminalGeometry { cols: 80, rows: 24 },
            launch_operation: None,
        };
        let terminal: TerminalRef = serde_json::from_value(
            runtime
                .lock()
                .unwrap()
                .request(
                    connection,
                    client,
                    RequestId::new(),
                    TerminalAction::Launch,
                    serde_json::to_value(TerminalRequest::Launch { intent: launch }).unwrap(),
                    SnapshotWire::RawTail,
                )
                .unwrap()["terminal"]
                .clone(),
        )
        .unwrap();
        let subscription = runtime
            .lock()
            .unwrap()
            .request(
                connection,
                client,
                RequestId::new(),
                TerminalAction::Attach,
                serde_json::to_value(TerminalRequest::Attach {
                    terminal: terminal.clone(),
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap()["subscription"]
            .as_u64()
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let racers = [
            (
                TerminalAction::Detach,
                TerminalRequest::Detach {
                    terminal: terminal.clone(),
                    subscription,
                },
            ),
            (
                TerminalAction::Resize,
                TerminalRequest::Resize {
                    terminal: terminal.clone(),
                    geometry: TerminalGeometry { cols: 81, rows: 25 },
                },
            ),
            (
                TerminalAction::Input,
                TerminalRequest::Input {
                    terminal: terminal.clone(),
                    subscription,
                    input_seq: 0,
                    input_operation: None,
                    bytes: b"printf race\n".to_vec(),
                },
            ),
        ]
        .into_iter()
        .map(|(action, request)| {
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                runtime.lock().unwrap().request(
                    connection,
                    client,
                    RequestId::new(),
                    action,
                    serde_json::to_value(request).unwrap(),
                    SnapshotWire::RawTail,
                )
            })
        })
        .collect::<Vec<_>>();
        for racer in racers {
            if let Err(error) = racer.join().unwrap() {
                assert_eq!(
                    error.code,
                    usagi_core::infrastructure::ipc::ErrorCode::StaleTarget
                );
            }
        }

        let exit_connection = ConnectionId::new();
        let exit_client = ClientId::new();
        let exit_subscription = runtime
            .lock()
            .unwrap()
            .request(
                exit_connection,
                exit_client,
                RequestId::new(),
                TerminalAction::Attach,
                serde_json::to_value(TerminalRequest::Attach {
                    terminal: terminal.clone(),
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap()["subscription"]
            .as_u64()
            .unwrap();
        runtime
            .lock()
            .unwrap()
            .request(
                exit_connection,
                exit_client,
                RequestId::new(),
                TerminalAction::Input,
                serde_json::to_value(TerminalRequest::Input {
                    terminal: terminal.clone(),
                    subscription: exit_subscription,
                    input_seq: 0,
                    input_operation: None,
                    bytes: b"exit\n".to_vec(),
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let response = runtime
                .lock()
                .unwrap()
                .request(
                    connection,
                    client,
                    RequestId::new(),
                    TerminalAction::Resume,
                    serde_json::to_value(TerminalRequest::Resume {
                        terminal: terminal.clone(),
                        after_offset: 0,
                    })
                    .unwrap(),
                    SnapshotWire::RawTail,
                )
                .unwrap();
            if response["exited"] == true {
                break;
            }
            assert!(Instant::now() < deadline, "terminal exit was not observed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(runtime.lock().unwrap().exit(&terminal, 0).is_err());
    }

    #[test]
    fn restart_from_another_directory_launches_terminals_at_the_restored_root() {
        let temporary = tempfile::tempdir().unwrap();
        let original_root = temporary.path().join("original-root");
        let restart_directory = temporary.path().join("restart-directory");
        let daemon_state = temporary.path().join("shared-daemon");
        std::fs::create_dir_all(&original_root).unwrap();
        std::fs::create_dir_all(&restart_directory).unwrap();

        let first = open_session_runtime(
            original_root.clone(),
            &daemon_state,
            usagi_core::domain::id::DaemonGeneration::new(),
        )
        .unwrap();
        drop(first);
        let restored = open_session_runtime(
            restart_directory,
            &daemon_state,
            usagi_core::domain::id::DaemonGeneration::new(),
        )
        .unwrap();

        let profile =
            LoginShellProfile::new(BTreeMap::new(), trusted_repository_root(&restored).unwrap());
        let launch = profile
            .resolve(&TerminalLaunchRequest {
                profile_id: TerminalProfileId::new("login-shell").unwrap(),
                scope: TerminalLaunchScope {
                    workspace_id: WorkspaceId::new(),
                    session_id: Some(SessionId::new()),
                    worktree_id: WorktreeId::new(),
                },
            })
            .unwrap();

        assert_eq!(launch.snapshot.working_directory, original_root);
    }

    #[test]
    fn file_terminal_store_writes_a_readable_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminals.json");
        let mut store = FileTerminalStore(path.clone());
        let snapshot = TerminalStoreSnapshot::default();

        store.save(snapshot.clone()).unwrap();

        assert_eq!(
            serde_json::from_slice::<TerminalStoreSnapshot>(&std::fs::read(path).unwrap()).unwrap(),
            snapshot
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Two daemon instances and every fenced effect form one restart contract.
    fn generic_terminal_restart_hydrates_inventory_and_preserves_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminals.json");
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let scope = TerminalLaunchScope {
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: worktree,
        };
        let request = TerminalLaunchRequest {
            profile_id: TerminalProfileId::new("login-shell").unwrap(),
            scope: scope.clone(),
        };
        let first_effects = Arc::new(Mutex::new(RestartEffects::default()));
        let mut first = GenericTerminalRuntime::new(
            DaemonGeneration::new(),
            TrustedLoginShell {
                profile: LoginShellProfile::new(BTreeMap::new(), dir.path().to_path_buf()),
                environment: None,
                workspace_root: PathBuf::new(),
            },
            FileTerminalStore(path.clone()),
            RestartPty(Arc::clone(&first_effects)),
            TestTerminalScope {
                scope: scope.clone(),
                working_directory: dir.path().to_path_buf(),
            },
        );
        let old_terminal: TerminalRef = serde_json::from_value(
            first
                .request(
                    ConnectionId::new(),
                    ClientId::new(),
                    RequestId::new(),
                    TerminalAction::Launch,
                    serde_json::to_value(TerminalRequest::Launch {
                        intent: TerminalLaunchIntent {
                            request: request.clone(),
                            geometry: TerminalGeometry { cols: 80, rows: 24 },
                            launch_operation: None,
                        },
                    })
                    .unwrap(),
                    SnapshotWire::RawTail,
                )
                .unwrap()["terminal"]
                .clone(),
        )
        .unwrap();
        assert_eq!(first_effects.lock().unwrap().spawns, 1);
        drop(first);

        let before_restart: TerminalStoreSnapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let old_record = before_restart.records[0].clone();
        let second_effects = Arc::new(Mutex::new(RestartEffects::default()));
        let mut second_store = FileTerminalStore(path.clone());
        let (reconciled, interrupted) = second_store.load_reconciled().unwrap();
        assert_eq!(interrupted, 1);
        let mut second = GenericTerminalRuntime::from_snapshot(
            DaemonGeneration::new(),
            TrustedLoginShell {
                profile: LoginShellProfile::new(BTreeMap::new(), dir.path().to_path_buf()),
                environment: None,
                workspace_root: PathBuf::new(),
            },
            second_store,
            RestartPty(Arc::clone(&second_effects)),
            TestTerminalScope {
                scope: scope.clone(),
                working_directory: dir.path().to_path_buf(),
            },
            reconciled,
        )
        .unwrap();

        let inventory = TerminalOwner::inventory(&second, &scope);
        assert_eq!(inventory.len(), 1);
        assert!(inventory[0].terminal.fences(&old_terminal));
        assert!(!inventory[0].live);
        for (action, request) in [
            (
                TerminalAction::Attach,
                TerminalRequest::Attach {
                    terminal: old_terminal.clone(),
                },
            ),
            (
                TerminalAction::Resize,
                TerminalRequest::Resize {
                    terminal: old_terminal.clone(),
                    geometry: TerminalGeometry {
                        cols: 100,
                        rows: 40,
                    },
                },
            ),
            (
                TerminalAction::Input,
                TerminalRequest::Input {
                    terminal: old_terminal.clone(),
                    subscription: 1,
                    input_seq: 0,
                    input_operation: None,
                    bytes: b"must-not-run".to_vec(),
                },
            ),
        ] {
            let error = second
                .request(
                    ConnectionId::new(),
                    ClientId::new(),
                    RequestId::new(),
                    action,
                    serde_json::to_value(request).unwrap(),
                    SnapshotWire::RawTail,
                )
                .unwrap_err();
            assert_eq!(
                error.code,
                usagi_core::infrastructure::ipc::ErrorCode::OwnershipUnknown
            );
        }
        assert_eq!(*second_effects.lock().unwrap(), RestartEffects::default());

        let new_terminal: TerminalRef = serde_json::from_value(
            second
                .request(
                    ConnectionId::new(),
                    ClientId::new(),
                    RequestId::new(),
                    TerminalAction::Launch,
                    serde_json::to_value(TerminalRequest::Launch {
                        intent: TerminalLaunchIntent {
                            request,
                            geometry: TerminalGeometry { cols: 80, rows: 24 },
                            launch_operation: None,
                        },
                    })
                    .unwrap(),
                    SnapshotWire::RawTail,
                )
                .unwrap()["terminal"]
                .clone(),
        )
        .unwrap();
        assert!(!new_terminal.fences(&old_terminal));
        assert_eq!(second_effects.lock().unwrap().spawns, 1);

        let after_launch: TerminalStoreSnapshot =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(after_launch.records.len(), 2);
        let retained = after_launch
            .records
            .iter()
            .find(|record| record.terminal.fences(&old_terminal))
            .unwrap();
        assert_eq!(retained.terminal, old_record.terminal);
        assert_eq!(retained.operation, old_record.operation);
        assert_eq!(retained.launch, old_record.launch);
        assert_eq!(
            retained.state,
            usagi_daemon::usecase::terminal::TerminalRuntimeState::ReconcileRequired(
                usagi_daemon::usecase::terminal::TerminalReconcileState::IdentityUnknown,
            )
        );
    }

    #[test]
    fn corrupt_or_unknown_terminal_snapshot_fails_closed_without_effect_or_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminals.json");
        let effects = Arc::new(Mutex::new(RestartEffects::default()));
        for bytes in [
            b"{broken".as_slice(),
            br#"{"schema_version":999,"records":[]}"#.as_slice(),
        ] {
            std::fs::write(&path, bytes).unwrap();
            let preserved = std::fs::read(&path).unwrap();
            assert!(FileTerminalStore(path.clone()).load_reconciled().is_err());
            assert_eq!(std::fs::read(&path).unwrap(), preserved);
            assert_eq!(*effects.lock().unwrap(), RestartEffects::default());
        }
    }

    #[test]
    fn file_runtime_store_writes_a_readable_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        let mut store = FileRuntimeStore(path.clone());
        let snapshot = RuntimeStoreSnapshot::default();

        store.save(snapshot.clone()).unwrap();

        assert_eq!(
            serde_json::from_slice::<RuntimeStoreSnapshot>(&std::fs::read(path).unwrap()).unwrap(),
            snapshot
        );
    }

    #[test]
    fn corrupt_or_unknown_agent_snapshot_fails_closed_without_overwrite() {
        for bytes in [
            b"{not-json".as_slice(),
            br#"{"schema_version":999,"records":[]}"#.as_slice(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("agents.json");
            std::fs::write(&path, bytes).unwrap();
            let before = std::fs::read(&path).unwrap();

            assert!(
                FileRuntimeStore(path.clone())
                    .reconcile_after_restart()
                    .is_err()
            );
            assert_eq!(std::fs::read(path).unwrap(), before);
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        let generation = DaemonGeneration::new();
        let mut corrupt = RuntimeStoreSnapshot::default();
        corrupt
            .generation
            .terminals
            .push(usagi_daemon::usecase::generation::TerminalOwnership {
                terminal: TerminalRef {
                    daemon_generation: generation,
                    terminal_id: TerminalId::new(),
                    workspace_id: WorkspaceId::new(),
                    session_id: Some(SessionId::new()),
                    worktree_id: WorktreeId::new(),
                },
                process: None,
                state: usagi_daemon::usecase::generation::TerminalState::IdentityUnknown,
            });
        assert_eq!(
            usagi_daemon::usecase::generation::GenerationCoordinator::restore(
                corrupt.generation.clone(),
                2,
            )
            .unwrap_err(),
            usagi_daemon::usecase::generation::GenerationError::UnknownGeneration
        );
        std::fs::write(&path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
        let before = std::fs::read(&path).unwrap();

        assert!(
            FileRuntimeStore(path.clone())
                .reconcile_after_restart()
                .is_err()
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn file_terminal_store_failure_preserves_target_and_cleans_temp() {
        assert_failed_snapshot_write_is_consistent(|path| {
            FileTerminalStore(path.to_path_buf()).save(TerminalStoreSnapshot::default())
        });
    }

    #[test]
    fn file_runtime_store_failure_preserves_target_and_cleans_temp() {
        assert_failed_snapshot_write_is_consistent(|path| {
            FileRuntimeStore(path.to_path_buf()).save(RuntimeStoreSnapshot::default())
        });
    }

    fn assert_failed_snapshot_write_is_consistent(save: impl FnOnce(&Path) -> Result<(), ()>) {
        let dir = tempfile::tempdir().unwrap();
        // An existing non-empty directory cannot be replaced by the final
        // rename. This fails after the durable temp has been written, so it
        // exercises both preservation of the old target and temp cleanup.
        let target = dir.path().join("snapshot.json");
        std::fs::create_dir(&target).unwrap();
        let preserved = target.join("preserved");
        std::fs::write(&preserved, "old snapshot owner").unwrap();

        assert!(save(&target).is_err());
        assert_eq!(
            std::fs::read_to_string(preserved).unwrap(),
            "old snapshot owner"
        );

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }
}
