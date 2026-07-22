//! daemon 面へ Unix process / socket / signal を接続する composition adapter。

#![coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=root_ipc_fixture_codex_survives_disconnect_and_replays_final

use std::backtrace::Backtrace;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::Deserialize;
use usagi_cli::cli::DaemonCommand as CliDaemonCommand;
use usagi_core::domain::AppInfo;
use usagi_core::domain::agent::{AgentProfileId, DurableLaunchSnapshot, EnvironmentVariableName};
use usagi_core::domain::id::{SessionId, TerminalRef, WorkspaceId, WorktreeId};
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonReady, DaemonRecordStore, InstanceLock, LivenessProbe, RecordFile,
    ShutdownSignal, Sleeper, Terminator,
};
use usagi_core::infrastructure::error_log::ErrorLog;
use usagi_core::infrastructure::ipc::BuildIdentity;
use usagi_core::infrastructure::paths;
use usagi_core::infrastructure::persistence::json_file;
use usagi_core::infrastructure::store::dispatch::DispatchStore;
use usagi_core::infrastructure::store::pr_inventory::PrInventoryStore;
use usagi_core::infrastructure::store::user_decision::UserDecisionStore;
use usagi_core::usecase::client::{ClientError, ClientPolicy, IpcClient};
use usagi_core::usecase::client::{DaemonRequest, DispatchToolAction, SupervisorToolAction};
use usagi_daemon::infrastructure::pty::PtyTerminal;
use usagi_daemon::infrastructure::unix_transport::{SecureUnixListener, ensure_private_dir};
use usagi_daemon::presentation::{DaemonCommand as PresentationDaemonCommand, DaemonEnv};
use usagi_daemon::usecase::agent_ipc::{
    AgentRuntime, AgentTerminalActor, ResolvedAgentScope, ScopeResolveError, SessionScopeResolver,
    SharedTerminalOwner, TerminalOutcome,
};
use usagi_daemon::usecase::claude::{
    ClaudeAdapter, ClaudeProvision, ClaudeProvisionFailure, ClaudeProvisioner,
};
use usagi_daemon::usecase::codex::{
    CodexAdapter, CodexProvision, CodexProvisionFailure, CodexProvisioner,
};
use usagi_daemon::usecase::generation::ProcessIdentity;
use usagi_daemon::usecase::generic_terminal::{
    GenericPtySpawner, TerminalProfileResolver, TerminalStore, TerminalStoreSnapshot,
};
use usagi_daemon::usecase::metrics::{MetricsBroker, MetricsObserver, MetricsSample};
use usagi_daemon::usecase::orchestration::AdapterRegistry;
use usagi_daemon::usecase::pr_inventory::{
    GhProcessPort, OutputPrProjector, RefreshClock, RefreshWorker,
};
use usagi_daemon::usecase::runtime::{
    OutputJournal, ProvisionContext, PtySpawner, RuntimeStore, RuntimeStoreSnapshot,
    SpawnProvision, TerminateReapError,
};
use usagi_daemon::usecase::session_runtime::{SessionRuntime, SessionRuntimeError, SystemGit};
use usagi_daemon::usecase::supervisor_runtime::{
    DecisionWake, DecisionWaker, InitialTask, SupervisorRuntime,
};
use usagi_daemon::usecase::terminal::{
    Geometry, Output, PtyWriteError, PtyWriter, SpawnFailure, output_pipeline_counters,
};
use usagi_daemon::usecase::terminal_ipc::{
    GenericTerminalRuntime, ResolvedTerminalScope, TerminalScopeResolveError, TerminalScopeResolver,
};
use usagi_daemon::usecase::terminal_profile::{LoginShellProfile, TERMINAL_ENVIRONMENT_VARIABLES};

struct TrustedLoginShell {
    profile: LoginShellProfile,
}

impl TerminalProfileResolver for TrustedLoginShell {
    fn resolve(
        &mut self,
        request: &usagi_core::domain::terminal_launch::TerminalLaunchRequest,
    ) -> Result<
        usagi_core::domain::terminal_launch::ResolvedTerminalLaunch,
        usagi_core::domain::terminal_launch::TerminalLaunchValidationError,
    > {
        self.profile.resolve(request)
    }
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
}
impl CodexProvisioner for RootCodexProvisioner {
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<CodexProvision, CodexProvisionFailure> {
        self.readiness
            .ready("codex")
            .map_err(|()| CodexProvisionFailure::ExecutableUnavailable)?;
        let (working_directory, workspace_root) = working_directories(&self.sessions, context)
            .map_err(|()| CodexProvisionFailure::MaterializationFailed)?;
        Ok(CodexProvision {
            working_directory,
            environment_allowlist: mcp_environment_allowlist(context),
            spawn: SpawnProvision::new(
                mcp_environment(context, &self.data_home, &workspace_root)
                    .map_err(|()| CodexProvisionFailure::MaterializationFailed)?,
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
        Ok(ClaudeProvision {
            working_directory,
            environment_allowlist: mcp_environment_allowlist(context),
            spawn: SpawnProvision::new(
                mcp_environment(context, &self.data_home, &workspace_root)
                    .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?,
                context
                    .inject_mcp
                    .then(|| claude_mcp_arguments(&self.mcp_command))
                    .transpose()
                    .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?
                    .unwrap_or_default(),
            ),
        })
    }
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
    ) -> TerminalOutcome {
        match self.0.lock() {
            Ok(mut agent) => AgentTerminalActor::handle_terminal(
                &mut *agent,
                connection,
                client,
                request_id,
                action,
                request,
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
        // one-time process invocation.
        let mut argv = provision.arguments().to_vec();
        argv.extend(plan.argv.iter().cloned());
        let environment = provision.compose_environment(&self.environment);
        let pty = PtyTerminal::spawn_with(
            &plan.program,
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

const PR_REFRESH_TICK: Duration = Duration::from_millis(250);
const PR_REFRESH_FRESHNESS_MS: u64 = 60_000;
const PR_REFRESH_PER_TICK: usize = 2;

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
    ) -> Result<serde_json::Value, usagi_core::infrastructure::ipc::ProtocolError> {
        self.0
            .lock()
            .map_err(|_| {
                usagi_core::infrastructure::ipc::ProtocolError::new(
                    usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                    "terminal owner is unavailable",
                )
            })?
            .request(connection, client, request_id, action, payload)
    }
    fn disconnect(&mut self, connection: usagi_core::domain::id::ConnectionId) {
        if let Ok(mut terminal) = self.0.lock() {
            terminal.disconnect(connection);
        }
    }
}

use super::bootstrap;
use super::launchd;

#[allow(clippy::too_many_lines)] // IPC request routing remains in the composition adapter.
fn spawn_ipc_server(
    data_dir: &Path,
    info: &AppInfo,
    shutdown: Arc<AtomicBool>,
) -> std::io::Result<()> {
    let generation = usagi_core::infrastructure::ipc::DaemonGeneration(
        usagi_core::domain::id::DaemonGeneration::new()
            .as_str()
            .clone(),
    );
    let listener = SecureUnixListener::bind(data_dir, generation.clone())?;
    let server = usagi_daemon::presentation::ipc::server_protocol(
        generation.clone(),
        generation.0.clone(),
        usagi_core::infrastructure::ipc::BuildIdentity {
            version: info.version.to_owned(),
            commit: "unknown".to_owned(),
            target: std::env::consts::ARCH.to_owned(),
        },
    );
    let repo_root = std::env::current_dir()?;
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
    let pipeline_metrics = Arc::new(TerminalPipelineMetrics::default());
    let (pty, observations) = DaemonPty::new(Arc::clone(&pipeline_metrics));
    let terminal = new_terminal_runtime(
        data_dir,
        daemon_generation,
        trusted_repository_root(&runtime)?,
        pty,
        Arc::clone(&runtime),
    )?;
    start_terminal_observer(
        Arc::clone(&terminal),
        observations,
        Arc::clone(&pr_inventory),
    )?;
    let (agent_pty, agent_observations) =
        AgentPty::new(terminal_environment(), Arc::clone(&pipeline_metrics));
    let mcp_command = std::env::current_exe()?;
    let agent = open_agent_runtime(
        data_dir,
        daemon_generation,
        Arc::clone(&runtime),
        agent_pty,
        mcp_command,
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
        Arc::clone(&pr_inventory),
        Arc::clone(&supervisor),
    )?;
    let decisions = Arc::new(UserDecisionStore::new(data_dir.join("daemon")));
    consume_user_decision_events(&decisions)
        .map_err(|error| std::io::Error::other(error.message))?;
    start_decision_maintenance(Arc::clone(&decisions))?;
    start_pr_refresh_worker(Arc::clone(&pr_inventory), shutdown)?;
    start_ipc_accept_loop(
        listener,
        server,
        runtime,
        terminal,
        agent,
        pr_inventory,
        decisions,
        Arc::new(Mutex::new(MetricsBroker::default())),
        Arc::new(Mutex::new(ProcessResourceSampler { previous: None })),
        pipeline_metrics,
        supervisor,
    )
}

/// Starts the only production PR refresh worker. Remote calls happen outside
/// the shared inventory lock, so snapshot and terminal paths continue to make
/// progress while `gh` is slow.
fn start_pr_refresh_worker(
    pr_inventory: SharedPrInventory,
    shutdown: Arc<AtomicBool>,
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
    shutdown: Arc<AtomicBool>,
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
            if let Ok(projector) = pr_inventory.lock()
                && worker.rebuild(&projector).is_err()
            {
                ErrorLog::record("PR refresh schedule rebuild failed");
            }
            while !shutdown.load(Ordering::Acquire) {
                let due = pr_inventory
                    .lock()
                    .ok()
                    .and_then(|projector| worker.claim_due(&projector).ok())
                    .unwrap_or_default();
                for identity in due {
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    let result = worker.fetch(&identity);
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if let Ok(mut projector) = pr_inventory.lock()
                        && worker.complete(&mut projector, &identity, result).is_err()
                    {
                        ErrorLog::record("PR refresh snapshot publish failed");
                    }
                }
                let deadline = Instant::now() + tick;
                while !shutdown.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        })
}

/// Keeps decision deadlines progressing even when no subsequent MCP/TUI
/// request arrives. Every action is idempotent, so a daemon restart simply
/// resumes from the JSON store.
fn start_decision_maintenance(decisions: Arc<UserDecisionStore>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("usagi-decision-maintenance".to_string())
        .spawn(move || {
            loop {
                let _ = decisions.expire_due(chrono::Utc::now());
                let _ = consume_user_decision_events(&decisions);
                std::thread::sleep(Duration::from_millis(250));
            }
        })
        .map(|_| ())
}

fn open_agent_runtime(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    sessions: SharedSessionRuntime,
    pty: AgentPty,
    mcp_command: PathBuf,
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
        }),
        ClaudeAdapter::new(RootClaudeProvisioner {
            sessions,
            readiness,
            mcp_command,
            data_home,
        }),
    );
    let runtime = AgentRuntime::hydrate_with_dispatch_and_locator(
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
    pr_inventory: SharedPrInventory,
    supervisor: SharedSupervisorRuntime,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("usagi-agent-observer".to_string())
        .spawn(move || {
            while let Ok(observation) = observations.recv() {
                let Ok(mut agent) = agent.lock() else {
                    break;
                };
                match observation {
                    AgentPtyObservation::Output(reference, bytes) => {
                        if agent.output(&reference, bytes.clone()).is_ok()
                            && let Ok(mut projector) = pr_inventory.lock()
                        {
                            let _ = projector.observe_committed(
                                reference.terminal_id,
                                reference.session_id,
                                &bytes,
                            );
                        }
                    }
                    AgentPtyObservation::Exited(reference, status) => {
                        let _ = agent.exit(&reference, status);
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
) -> std::io::Result<SharedTerminalRuntime> {
    let mut store = FileTerminalStore(data_dir.join("daemon").join("terminals.json"));
    let (snapshot, interrupted) = store.load_reconciled()?;
    if interrupted != 0 {
        ErrorLog::record(&format!(
            "daemon startup reconciled {interrupted} generic terminal(s) as identity_unknown"
        ));
    }
    let runtime = GenericTerminalRuntime::from_snapshot(
        generation,
        TrustedLoginShell {
            profile: LoginShellProfile::new(terminal_environment(), repo_root),
        },
        store,
        pty,
        SharedTerminalScopeResolver(sessions),
        snapshot,
    )
    .map_err(|_| std::io::Error::other("invalid generic terminal snapshot"))?;
    Ok(Arc::new(Mutex::new(runtime)))
}

fn start_terminal_observer<S, Q>(
    terminal: Arc<Mutex<GenericTerminalRuntime<TrustedLoginShell, S, DaemonPty, Q>>>,
    observations: Receiver<PtyObservation>,
    pr_inventory: SharedPrInventory,
) -> std::io::Result<()>
where
    S: TerminalStore + Send + 'static,
    Q: TerminalScopeResolver + Send + 'static,
{
    std::thread::Builder::new()
        .name("usagi-terminal-observer".to_string())
        .spawn(move || {
            while let Ok(observation) = observations.recv() {
                let Ok(mut terminal) = terminal.lock() else {
                    break;
                };
                match observation {
                    PtyObservation::Output(reference, bytes) => {
                        if terminal.output(&reference, bytes.clone()).is_ok()
                            && let Ok(mut projector) = pr_inventory.lock()
                        {
                            let _ = projector.observe_committed(
                                reference.terminal_id,
                                reference.session_id,
                                &bytes,
                            );
                        }
                    }
                    PtyObservation::Exited(reference, status) => {
                        let _ = terminal.exit(&reference, status);
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
    terminal: SharedTerminalRuntime,
    agent: SharedAgentRuntime,
    pr_inventory: SharedPrInventory,
    decisions: Arc<UserDecisionStore>,
    metrics: SharedMetricsBroker,
    process_metrics: SharedProcessResourceSampler,
    pipeline_metrics: Arc<TerminalPipelineMetrics>,
    supervisor: SharedSupervisorRuntime,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("usagi-ipc".to_string())
        .spawn(move || {
            loop {
                match listener.accept() {
                    Ok(stream) => {
                        let server = server.clone();
                        let session = Arc::clone(&runtime);
                        let scope_sessions = Arc::clone(&runtime);
                        let terminal = Arc::clone(&terminal);
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
                                let mut owner = SharedTerminalOwner::new(
                                    SharedAgent(agent_owner),
                                    SharedTerminal(terminal),
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
                                        Some("session") => dispatch_session(&session, &agent_launch, &pr_inventory, request_id, &body, hello),
                                        Some("agent" | "agent_inventory" | "resume_agent") => dispatch_agent(&agent_launch, &scope_sessions, request_id, &body, hello),
                                        Some("codex_session_capture") => dispatch_codex_session_capture(&agent_launch, request_id, &body, hello),
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
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        })
        .map(|_| ())
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
                .and_then(|projector| projector.snapshot(payload.session_id).ok())
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
                    .map_err(|_| SessionRuntimeError::Storage)?
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
            let outcome = if admission.completed {
                ResponseOutcome::Ok
            } else {
                ResponseOutcome::Accepted {
                    operation_id: usagi_core::infrastructure::ipc::OperationId(
                        admission.operation_id,
                    ),
                    operation_revision: admission.revision,
                }
            };
            envelope(
                hello,
                request_id,
                outcome,
                serde_json::json!({
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

impl RecordFile for FsRecordFile {
    fn read(&self) -> std::io::Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
    fn write(&self, contents: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, contents)
    }
    fn remove(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

struct KillProbe;
impl LivenessProbe for KillProbe {
    #[cfg(unix)]
    fn is_alive(&self, pid: u32) -> bool {
        libc::pid_t::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
    }
    #[cfg(not(unix))]
    fn is_alive(&self, _pid: u32) -> bool {
        false
    }
}

struct SigtermTerminator;
impl Terminator for SigtermTerminator {
    #[cfg(unix)]
    fn terminate(&self, pid: u32) -> std::io::Result<()> {
        let pid =
            libc::pid_t::try_from(pid).map_err(|_| std::io::Error::other("pid out of range"))?;
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    fn terminate(&self, _pid: u32) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "terminating a daemon is only supported on Unix",
        ))
    }
}

/// Root-bound IPC publication seam. `serve` invokes it only after the daemon
/// owns the singleton lock and has persisted its PID record. The guard makes a
/// future duplicate invocation a no-op instead of binding a second endpoint.
struct IpcReady<'a> {
    data_dir: &'a Path,
    info: &'a AppInfo,
    shutdown: Arc<AtomicBool>,
    published: AtomicBool,
}
impl DaemonReady for IpcReady<'_> {
    fn publish(&self) -> std::io::Result<()> {
        if self
            .published
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && let Err(error) =
                spawn_ipc_server(self.data_dir, self.info, Arc::clone(&self.shutdown))
        {
            self.published.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }
}

struct SignalShutdown(Arc<AtomicBool>);
impl ShutdownSignal for SignalShutdown {
    #[cfg(unix)]
    fn wait(&self) -> std::io::Result<()> {
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, libc::SIGINT);
            libc::sigaddset(&raw mut set, libc::SIGTERM);
            if libc::sigprocmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut received: libc::c_int = 0;
            if libc::sigwait(&raw const set, &raw mut received) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        self.0.store(true, Ordering::Release);
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
impl InstanceLock for FileInstanceLock {
    fn acquire(&self) -> std::io::Result<bool> {
        const TIMEOUT: Duration = Duration::from_secs(2);
        const POLL: Duration = Duration::from_millis(20);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.path)?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => {
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
) -> std::io::Result<()> {
    install_panic_logger();
    match panic::catch_unwind(AssertUnwindSafe(|| run_inner(out, command, info))) {
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
) -> std::io::Result<()> {
    let daemon_dir = paths::data_dir()
        .map_err(|err| std::io::Error::other(format!("{err:#}")))?
        .join("daemon");
    let data_dir = daemon_dir
        .parent()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "daemon data path has no parent",
            )
        })?
        .to_path_buf();
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
        CliDaemonCommand::Stop => PresentationDaemonCommand::Stop,
        CliDaemonCommand::Restart => PresentationDaemonCommand::Restart,
    };
    // The lifecycle lock is acquired before the listener binds. Prepare the
    // endpoint directory with the same private-mode invariant that the
    // listener enforces, so a first launch cannot leave it at create_dir_all's
    // process-dependent default mode.
    std::fs::create_dir_all(&data_dir)?;
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
    let ready = IpcReady {
        data_dir: &data_dir,
        info,
        shutdown: Arc::new(AtomicBool::new(false)),
        published: AtomicBool::new(false),
    };
    let env = DaemonEnv {
        store: &store,
        probe: &KillProbe,
        terminator: &SigtermTerminator,
        ready: &ready,
        shutdown: &SignalShutdown(Arc::clone(&ready.shutdown)),
        launcher: &launcher,
        sleeper: &RealSleeper,
        lock: &lock,
        pid: std::process::id(),
    };
    usagi_daemon::presentation::run(out, command, info, &env)
}

/// Connect to the daemon for this binary's isolated build channel. Debug
/// binaries restart their development daemon once per bootstrap; release
/// binaries reuse a matching production daemon and only roll over an older
/// build.
pub(crate) fn client(
    policy: ClientPolicy,
) -> Result<IpcClient<std::os::unix::net::UnixStream>, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let exe =
        std::env::current_exe().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let _bootstrap_lock = acquire_bootstrap_lock(&data_dir)?;
    let expected_build = current_build();
    bootstrap::connect_or_start(
        || connect_client(&data_dir, policy),
        || run_lifecycle(&exe, "start"),
        || run_lifecycle(&exe, "restart"),
        &expected_build,
        false,
        IpcClient::server_build,
    )
    .map_err(|error| ClientError::Lifecycle(error.to_string()))
}

fn current_build() -> BuildIdentity {
    BuildIdentity {
        version: env!("CARGO_PKG_VERSION").into(),
        commit: "unknown".into(),
        target: std::env::consts::ARCH.into(),
    }
}
fn connect_client(
    data_dir: &Path,
    policy: ClientPolicy,
) -> std::io::Result<IpcClient<std::os::unix::net::UnixStream>> {
    let stream = usagi_daemon::infrastructure::unix_transport::connect_current(data_dir)?;
    IpcClient::connect(
        stream,
        format!("cli-{}", std::process::id()),
        format!("{}", std::process::id()),
        policy,
    )
    .map_err(std::io::Error::other)
}
fn run_lifecycle(exe: &Path, command: &str) -> std::io::Result<()> {
    let status = std::process::Command::new(exe)
        .args(["daemon", command])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| std::io::Error::other(format!("daemon {command} failed")))
}
fn acquire_bootstrap_lock(data_dir: &Path) -> Result<std::fs::File, ClientError> {
    let daemon_dir = data_dir.join("daemon");
    std::fs::create_dir_all(data_dir)
        .map_err(|error| ClientError::Unavailable(error.to_string()))?;
    match std::fs::create_dir(&daemon_dir) {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&daemon_dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(|error| ClientError::Unavailable(error.to_string()))?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(ClientError::Unavailable(error.to_string())),
    }
    let lock = std::fs::File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(daemon_dir.join("bootstrap.lock"))
        .map_err(|error| ClientError::Unavailable(error.to_string()))?;
    FileExt::lock_exclusive(&lock).map_err(|error| ClientError::Unavailable(error.to_string()))?;
    Ok(lock)
}

/// Ensures that an active daemon endpoint exists before an interactive TUI is
/// shown. TUI operations still acquire their own client connection.
pub(crate) fn ensure_ready() -> Result<(), ClientError> {
    client(ClientPolicy::tui()).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    struct FixedRefreshClock {
        calls: Arc<AtomicUsize>,
        shutdown_after: Option<(usize, Arc<AtomicBool>)>,
    }
    impl RefreshClock for FixedRefreshClock {
        fn now_ms(&self) -> u64 {
            let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
            if let Some((after, shutdown)) = &self.shutdown_after
                && call >= *after
            {
                shutdown.store(true, Ordering::Release);
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
                identity.as_url().as_bytes(),
            )
            .unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
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

        let cancelled = Arc::new(AtomicBool::new(true));
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
            },
            limits: ProtocolLimits::default(),
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
            },
            TestTerminalStore,
            pty,
            TestTerminalScope {
                scope: scope.clone(),
                working_directory: directory.path().to_path_buf(),
            },
        )));
        start_terminal_observer(
            Arc::clone(&runtime),
            observations,
            Arc::new(Mutex::new(OutputPrProjector::new(PrInventoryStore::new(
                directory.path(),
            )))),
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
                    bytes: b"exit\n".to_vec(),
                })
                .unwrap(),
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
                        },
                    })
                    .unwrap(),
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
                        },
                    })
                    .unwrap(),
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
