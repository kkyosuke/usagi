//! daemon 面へ Unix process / socket / signal を接続する composition adapter。

#![coverage(off)] // Unix socket / process / PTY wiring; fake-PTY owner contracts live in usagi-daemon tests.

use std::backtrace::Backtrace;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fs2::FileExt;
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
use usagi_core::usecase::client::{ClientError, ClientPolicy, IpcClient};
use usagi_daemon::infrastructure::pty::PtyTerminal;
use usagi_daemon::infrastructure::unix_transport::SecureUnixListener;
use usagi_daemon::presentation::DaemonEnv;
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
use usagi_daemon::usecase::orchestration::AdapterRegistry;
use usagi_daemon::usecase::pr_inventory::OutputPrProjector;
use usagi_daemon::usecase::runtime::{
    OutputJournal, ProvisionContext, PtySpawner, RuntimeStore, RuntimeStoreSnapshot, SpawnProvision,
};
use usagi_daemon::usecase::session_runtime::{SessionRuntime, SessionRuntimeError, SystemGit};
use usagi_daemon::usecase::terminal::{Geometry, Output, PtyWriteError, PtyWriter, SpawnFailure};
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
    type Error = std::io::Error;
    fn save(&mut self, snapshot: TerminalStoreSnapshot) -> Result<(), Self::Error> {
        json_file::write_atomic(snapshot_directory(&self.0)?, &self.0, &snapshot)
            .map_err(std::io::Error::other)
    }
}

/// Persists the durable Agent runtime snapshot next to the terminal store.
struct FileRuntimeStore(PathBuf);
impl RuntimeStore for FileRuntimeStore {
    type Error = std::io::Error;
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), Self::Error> {
        json_file::write_atomic(snapshot_directory(&self.0)?, &self.0, &snapshot)
            .map_err(std::io::Error::other)
    }
}

impl FileRuntimeStore {
    /// Reconcile a snapshot which outlived the daemon that owned its PTYs.
    /// Missing snapshots are normal on a first launch.  Parse/write failures
    /// deliberately leave the old bytes untouched so a later recovery can
    /// inspect the last known-good durable snapshot.
    fn reconcile_after_restart(&mut self) -> std::io::Result<usize> {
        let Some(snapshot) =
            json_file::read::<RuntimeStoreSnapshot>(&self.0).map_err(std::io::Error::other)?
        else {
            return Ok(0);
        };
        let (snapshot, interrupted) = snapshot.reconcile_after_daemon_restart();
        if interrupted != 0 {
            self.save(snapshot)?;
        }
        Ok(interrupted)
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
    type Error = std::convert::Infallible;
    fn append(&mut self, _output: &Output) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Resolves the checkout path for a launch scope through the single managed
/// session writer, so agents never receive a client supplied path.
struct RootCodexProvisioner {
    sessions: SharedSessionRuntime,
    readiness: Arc<dyn AgentReadinessProbe>,
    mcp_command: PathBuf,
}
impl CodexProvisioner for RootCodexProvisioner {
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<CodexProvision, CodexProvisionFailure> {
        self.readiness
            .ready("codex")
            .map_err(|()| CodexProvisionFailure::ExecutableUnavailable)?;
        let working_directory = working_directory(&self.sessions, context)
            .map_err(|()| CodexProvisionFailure::MaterializationFailed)?;
        Ok(CodexProvision {
            working_directory,
            environment_allowlist: BTreeSet::<EnvironmentVariableName>::new(),
            spawn: SpawnProvision::new(
                [],
                context
                    .inject_mcp
                    .then(|| codex_mcp_arguments(&self.mcp_command))
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
}
impl ClaudeProvisioner for RootClaudeProvisioner {
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<ClaudeProvision, ClaudeProvisionFailure> {
        self.readiness
            .ready("claude")
            .map_err(|()| ClaudeProvisionFailure::ExecutableUnavailable)?;
        let working_directory = working_directory(&self.sessions, context)
            .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?;
        Ok(ClaudeProvision {
            working_directory,
            environment_allowlist: BTreeSet::<EnvironmentVariableName>::new(),
            spawn: SpawnProvision::new(
                [],
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

/// Product-specific MCP launch arguments.  They stay ephemeral in
/// [`SpawnProvision`] so the durable launch plan never stores configuration
/// paths or rendered product payloads.
fn codex_mcp_arguments(command: &Path) -> Result<Vec<String>, ()> {
    let command = command.to_str().ok_or(())?;
    let command = serde_json::to_string(command).map_err(|_| ())?;
    Ok(vec![
        "-c".into(),
        format!("mcp_servers.usagi.command = {command}"),
        "-c".into(),
        r#"mcp_servers.usagi.args = ["mcp"]"#.into(),
    ])
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
    Ok(vec!["--mcp-config".into(), config.to_string()])
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
fn working_directory(
    sessions: &SharedSessionRuntime,
    context: &ProvisionContext,
) -> Result<PathBuf, ()> {
    sessions
        .lock()
        .map_err(|_| ())?
        .resolve_scope(
            context.scope.workspace_id,
            context.scope.session_id,
            context.scope.worktree_id,
        )
        .map(|scope| scope.path)
        .map_err(|_| ())
}

/// The #268 scope resolver, adapted to the Agent owner's product-neutral
/// `(workspace, session)` input by deriving the available session's worktree.
struct SharedScopeResolver(SharedSessionRuntime);
impl SessionScopeResolver for SharedScopeResolver {
    fn resolve_available_scope(
        &self,
        workspace: WorkspaceId,
        session: SessionId,
    ) -> Result<ResolvedAgentScope, ScopeResolveError> {
        let runtime = self.0.lock().map_err(|_| ScopeResolveError::Storage)?;
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
        let session = requested
            .session_id
            .ok_or(TerminalScopeResolveError::Unavailable)?;
        let runtime = self
            .0
            .lock()
            .map_err(|_| TerminalScopeResolveError::Unavailable)?;
        let scope = runtime
            .resolve_scope(requested.workspace_id, session, requested.worktree_id)
            .map_err(|_| TerminalScopeResolveError::Unavailable)?;
        Ok(ResolvedTerminalScope {
            scope: requested.clone(),
            working_directory: scope.path,
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

type RootAgentRuntime = AgentRuntime<FileRuntimeStore, AgentPty, DiscardJournal>;
type SharedAgentRuntime = Arc<Mutex<RootAgentRuntime>>;

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

/// The daemon-owned PTY spawner/writer for Agent runtimes.  It spawns the real
/// rendered plan, drains output to the Agent owner, and reaps the child to
/// commit a durable exit — never a client-driven process.
struct AgentPty {
    terminals: BTreeMap<String, Arc<Mutex<PtyTerminal>>>,
    selected: Option<String>,
    observations: Sender<AgentPtyObservation>,
    environment: BTreeMap<String, String>,
}
impl AgentPty {
    fn new(environment: BTreeMap<String, String>) -> (Self, Receiver<AgentPtyObservation>) {
        let (observations, receiver) = mpsc::channel();
        (
            Self {
                terminals: BTreeMap::new(),
                selected: None,
                observations,
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
        let mut environment = self.environment.clone();
        environment.extend(
            provision
                .environment()
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.clone())),
        );
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
        self.terminals
            .insert(terminal.terminal_id.as_str().clone(), Arc::clone(&pty));
        let observations = self.observations.clone();
        let output_terminal = terminal.clone();
        let exit_pty = Arc::clone(&pty);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut bytes = [0_u8; 4096];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                if observations
                    .send(AgentPtyObservation::Output(
                        output_terminal.clone(),
                        bytes[..count].to_vec(),
                    ))
                    .is_err()
                {
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
}
impl PtyWriter for AgentPty {
    fn select_terminal(&mut self, terminal: &TerminalRef) {
        self.selected = Some(terminal.terminal_id.as_str().clone());
    }
    #[coverage(off)] // Real PTY ioctl; the agent IPC fake verifies the fenced resize behavior.
    fn resize(&mut self, terminal: &TerminalRef, geometry: Geometry) -> Result<(), PtyWriteError> {
        let Some(pty) = self.terminals.get(&terminal.terminal_id.as_str()) else {
            return Err(PtyWriteError { applied_prefix: 0 });
        };
        pty.lock()
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
            .lock()
            .map_err(|_| PtyWriteError { applied_prefix: 0 })?
            .write_all(bytes)
    }
}

enum PtyObservation {
    Output(usagi_core::domain::id::TerminalRef, Vec<u8>),
    Exited(usagi_core::domain::id::TerminalRef, i32),
}

struct DaemonPty {
    terminals: BTreeMap<String, Arc<Mutex<PtyTerminal>>>,
    selected: Option<String>,
    observations: Sender<PtyObservation>,
}
impl DaemonPty {
    fn new() -> (Self, Receiver<PtyObservation>) {
        let (observations, receiver) = mpsc::channel();
        (
            Self {
                terminals: BTreeMap::new(),
                selected: None,
                observations,
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
        self.terminals
            .insert(terminal.terminal_id.as_str().clone(), Arc::clone(&pty));
        let output_sender = self.observations.clone();
        let output_terminal = terminal.clone();
        let exit_pty = Arc::clone(&pty);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut bytes = [0_u8; 4096];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                if output_sender
                    .send(PtyObservation::Output(
                        output_terminal.clone(),
                        bytes[..count].to_vec(),
                    ))
                    .is_err()
                {
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
impl PtyWriter for DaemonPty {
    fn select_terminal(&mut self, terminal: &usagi_core::domain::id::TerminalRef) {
        self.selected = Some(terminal.terminal_id.as_str().clone());
    }
    #[coverage(off)] // Real PTY ioctl; the generic terminal use case covers the request semantics.
    fn resize(
        &mut self,
        terminal: &usagi_core::domain::id::TerminalRef,
        geometry: Geometry,
    ) -> Result<(), PtyWriteError> {
        let Some(pty) = self.terminals.get(&terminal.terminal_id.as_str()) else {
            return Err(PtyWriteError { applied_prefix: 0 });
        };
        pty.lock()
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
            .lock()
            .map_err(|_| PtyWriteError { applied_prefix: 0 })?
            .write_all(bytes)
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
type SharedSessionRuntime = Arc<Mutex<SessionRuntime<SystemGit>>>;
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

/// Samples daemon-owned process resources between metrics requests.
struct ProcessMetrics {
    previous: Option<(Instant, u64)>,
}

impl ProcessMetrics {
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

type SharedProcessMetrics = Arc<Mutex<ProcessMetrics>>;
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
#[coverage(off)]
fn spawn_ipc_server(data_dir: &Path, info: &AppInfo) -> std::io::Result<()> {
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
    let (pty, observations) = DaemonPty::new();
    let terminal = new_terminal_runtime(
        data_dir,
        daemon_generation,
        trusted_repository_root(&runtime)?,
        pty,
        Arc::clone(&runtime),
    );
    start_terminal_observer(
        Arc::clone(&terminal),
        observations,
        Arc::clone(&pr_inventory),
    )?;
    let (agent_pty, agent_observations) = AgentPty::new(terminal_environment());
    let mcp_command = std::env::current_exe()?;
    let agent = open_agent_runtime(
        data_dir,
        daemon_generation,
        Arc::clone(&runtime),
        agent_pty,
        mcp_command,
    );
    start_agent_observer(
        Arc::clone(&agent),
        agent_observations,
        Arc::clone(&pr_inventory),
    )?;
    start_ipc_accept_loop(
        listener,
        server,
        runtime,
        terminal,
        agent,
        pr_inventory,
        Arc::new(Mutex::new(ProcessMetrics { previous: None })),
    )
}

fn open_agent_runtime(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    sessions: SharedSessionRuntime,
    pty: AgentPty,
    mcp_command: PathBuf,
) -> SharedAgentRuntime {
    let mut store = FileRuntimeStore(data_dir.join("daemon").join("agents.json"));
    match store.reconcile_after_restart() {
        Ok(0) => {}
        Ok(interrupted) => ErrorLog::record(&format!(
            "daemon startup reconciled {interrupted} agent runtime(s) as interrupted (identity_unknown)"
        )),
        Err(error) => ErrorLog::record(&format!(
            "daemon startup could not reconcile durable agent runtimes: {error}"
        )),
    }
    let mut registry = AdapterRegistry::new();
    let readiness: Arc<dyn AgentReadinessProbe> = Arc::new(SystemAgentReadiness);
    // Duplicate registration cannot happen for the two literal profiles; a
    // failure here would only drop an adapter, so the launch would surface a
    // safe unknown-profile error rather than crash the daemon.
    let _ = registry.register_supported(
        CodexAdapter::new(RootCodexProvisioner {
            sessions: Arc::clone(&sessions),
            readiness: Arc::clone(&readiness),
            mcp_command: mcp_command.clone(),
        }),
        ClaudeAdapter::new(RootClaudeProvisioner {
            sessions,
            readiness,
            mcp_command,
        }),
    );
    Arc::new(Mutex::new(AgentRuntime::with_dispatch(
        generation,
        registry,
        store,
        DiscardJournal,
        pty,
        AgentProfileId::new("codex").expect("literal profile id is canonical"),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(data_dir.join("daemon")),
    )))
}

fn start_agent_observer(
    agent: SharedAgentRuntime,
    observations: Receiver<AgentPtyObservation>,
    pr_inventory: SharedPrInventory,
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
) -> SharedTerminalRuntime {
    Arc::new(Mutex::new(GenericTerminalRuntime::new(
        generation,
        TrustedLoginShell {
            profile: LoginShellProfile::new(terminal_environment(), repo_root),
        },
        FileTerminalStore(data_dir.join("daemon").join("terminals.json")),
        pty,
        SharedTerminalScopeResolver(sessions),
    )))
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

fn start_ipc_accept_loop(
    listener: SecureUnixListener,
    server: usagi_core::infrastructure::ipc::ServerProtocol,
    runtime: SharedSessionRuntime,
    terminal: SharedTerminalRuntime,
    agent: SharedAgentRuntime,
    pr_inventory: SharedPrInventory,
    metrics: SharedProcessMetrics,
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
                        let metrics = Arc::clone(&metrics);
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
                                let _ = usagi_daemon::presentation::ipc::handle_connection_with_terminal_and(
                                    &mut reader,
                                    &mut writer,
                                    &server,
                                    &mut owner,
                                    |request_id, body, hello| match body
                                        .get("kind")
                                        .and_then(serde_json::Value::as_str)
                                    {
                                        Some("session") => dispatch_session(&session, request_id, &body, hello),
                                        Some("agent") => dispatch_agent(&agent_launch, &scope_sessions, request_id, &body, hello),
                                        Some("dispatch") => dispatch_dispatch(&agent_launch, &scope_sessions, request_id, &body, hello),
                                        Some("metrics") => dispatch_metrics(&metrics, request_id, &body, hello),
                                        Some("pr") => dispatch_pr_snapshot(&pr_inventory, request_id, &body, hello),
                                        _ => usagi_daemon::presentation::ipc::dispatch(request_id, body, hello),
                                    },
                                );
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
    metrics: &SharedProcessMetrics,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    _body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    let (cpu_percent_hundredths, resident_memory_bytes) = metrics
        .lock()
        .map_or((0, 0), |mut metrics| metrics.snapshot());
    let sampled_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        });
    envelope(
        hello,
        request_id,
        usagi_core::infrastructure::ipc::ResponseOutcome::Ok,
        serde_json::json!({
            "schema_version": 1,
            "sampled_at_ms": sampled_at_ms,
            "cpu_percent_hundredths": cpu_percent_hundredths,
            "resident_memory_bytes": resident_memory_bytes,
            "active_subscribers": 0,
            "dropped_updates": 0,
        }),
    )
}

fn dispatch_session(
    session: &SharedSessionRuntime,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::ResponseOutcome;
    use usagi_core::usecase::client::{DaemonRequest, SessionAction};
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
    let result = session
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)
        .and_then(|mut session| session.handle(action, &operation_id, &payload));
    match result {
        Ok(reply) => {
            let recovery_apply =
                payload.get("apply").and_then(serde_json::Value::as_bool) == Some(true);
            let outcome = if matches!(action, SessionAction::Create | SessionAction::Remove)
                || (action == SessionAction::RecoverLegacy && recovery_apply)
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
                SessionAction::RecoverLegacy if recovery_apply => Some("session.legacy_recovered"),
                SessionAction::RecoverLegacy
                | SessionAction::List
                | SessionAction::Overview
                | SessionAction::Setup
                | SessionAction::Prompt => None,
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
            let code = if error == SessionRuntimeError::IdempotencyConflict {
                usagi_core::infrastructure::ipc::ErrorCode::IdempotencyConflict
            } else {
                usagi_core::infrastructure::ipc::ErrorCode::InvalidArgument
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

fn dispatch_agent(
    agent: &SharedAgentRuntime,
    scope_sessions: &SharedSessionRuntime,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};
    use usagi_core::usecase::client::DaemonRequest;
    let request = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Agent {
                operation_id,
                intent,
            } => Some((operation_id, intent)),
            _ => None,
        });
    let Some((operation_id, intent)) = request else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    let scope = SharedScopeResolver(Arc::clone(scope_sessions));
    let result = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))
        .and_then(|mut agent| agent.launch(&operation_id, &intent, &scope));
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
    #[coverage(off)]
    fn read(&self) -> std::io::Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
    #[coverage(off)]
    fn write(&self, contents: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, contents)
    }
    #[coverage(off)]
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
    #[coverage(off)]
    fn is_alive(&self, pid: u32) -> bool {
        libc::pid_t::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
    }
    #[cfg(not(unix))]
    #[coverage(off)]
    fn is_alive(&self, _pid: u32) -> bool {
        false
    }
}

struct SigtermTerminator;
impl Terminator for SigtermTerminator {
    #[cfg(unix)]
    #[coverage(off)]
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
    #[coverage(off)]
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
    published: AtomicBool,
}
impl DaemonReady for IpcReady<'_> {
    #[coverage(off)]
    fn publish(&self) -> std::io::Result<()> {
        if self
            .published
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && let Err(error) = spawn_ipc_server(self.data_dir, self.info)
        {
            self.published.store(false, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }
}

struct SignalShutdown;
impl ShutdownSignal for SignalShutdown {
    #[cfg(unix)]
    #[coverage(off)]
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
        Ok(())
    }
    #[cfg(not(unix))]
    #[coverage(off)]
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
    #[coverage(off)]
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
    #[coverage(off)]
    fn sleep(&self) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct FileInstanceLock {
    path: PathBuf,
    held: RefCell<Option<std::fs::File>>,
}
impl InstanceLock for FileInstanceLock {
    #[coverage(off)]
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
#[coverage(off)]
pub(crate) fn run<W: Write>(
    out: &mut W,
    command: Option<&str>,
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
#[coverage(off)]
fn install_panic_logger() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        ErrorLog::record(&format_panic(info));
        previous(info);
    }));
}

#[coverage(off)]
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

#[coverage(off)]
fn run_inner<W: Write>(out: &mut W, command: Option<&str>, info: &AppInfo) -> std::io::Result<()> {
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
    match command {
        Some("install-service") => {
            let path = launchd::install(&std::env::current_exe()?, &data_dir)?;
            return writeln!(
                out,
                "{}: launchd service installed ({})",
                info.describe(),
                path.display()
            );
        }
        Some("uninstall-service") => {
            let path = launchd::uninstall()?;
            return writeln!(
                out,
                "{}: launchd service uninstalled ({})",
                info.describe(),
                path.display()
            );
        }
        _ => {}
    }
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
        published: AtomicBool::new(false),
    };
    let env = DaemonEnv {
        store: &store,
        probe: &KillProbe,
        terminator: &SigtermTerminator,
        ready: &ready,
        shutdown: &SignalShutdown,
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
#[coverage(off)]
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
        bootstrap::should_force_restart(
            matches!(paths::build_channel(), paths::BuildChannel::Development),
            invoked_by_cargo_run(),
        ),
        IpcClient::server_build,
    )
    .map_err(|error| ClientError::Lifecycle(error.to_string()))
}

/// `cargo run` remains the only debug entry point that intentionally replaces
/// a matching development daemon. Integration tests execute the binary from a
/// test harness, so their parent is not Cargo and they reuse the endpoint.
#[cfg(unix)]
#[coverage(off)]
fn invoked_by_cargo_run() -> bool {
    let parent = unsafe { libc::getppid() };
    let Ok(output) = std::process::Command::new("ps")
        .args(["-p", &parent.to_string(), "-o", "command="])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    let words: Vec<_> = command.split_whitespace().collect();
    words
        .first()
        .is_some_and(|program| program.rsplit('/').next() == Some("cargo"))
        && words.get(1).is_some_and(|argument| *argument == "run")
}

#[cfg(not(unix))]
#[coverage(off)]
fn invoked_by_cargo_run() -> bool {
    false
}

#[coverage(off)]
fn current_build() -> BuildIdentity {
    BuildIdentity {
        version: env!("CARGO_PKG_VERSION").into(),
        commit: "unknown".into(),
        target: std::env::consts::ARCH.into(),
    }
}

#[coverage(off)]
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

#[coverage(off)]
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

#[coverage(off)]
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
#[coverage(off)]
pub(crate) fn ensure_ready() -> Result<(), ClientError> {
    client(ClientPolicy::tui()).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn product_mcp_arguments_start_usagi_mcp_from_the_daemon_binary() {
        let command = Path::new("/opt/usagi/bin/usagi");

        assert_eq!(
            codex_mcp_arguments(command).unwrap(),
            [
                "-c",
                "mcp_servers.usagi.command = \"/opt/usagi/bin/usagi\"",
                "-c",
                "mcp_servers.usagi.args = [\"mcp\"]",
            ]
        );
        assert_eq!(
            claude_mcp_arguments(command).unwrap(),
            [
                "--mcp-config",
                r#"{"mcpServers":{"usagi":{"args":["mcp"],"command":"/opt/usagi/bin/usagi"}}}"#,
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
        type Error = std::convert::Infallible;

        fn save(&mut self, _: TerminalStoreSnapshot) -> Result<(), Self::Error> {
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
        let (mut pty, observations) = DaemonPty::new();

        pty.spawn(&launch, &terminal, Geometry { cols: 80, rows: 24 })
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
        let (pty, observations) = DaemonPty::new();
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
        runtime
            .lock()
            .unwrap()
            .request(
                connection,
                client,
                RequestId::new(),
                TerminalAction::Input,
                serde_json::to_value(TerminalRequest::Input {
                    terminal: terminal.clone(),
                    subscription,
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

    fn assert_failed_snapshot_write_is_consistent(save: impl FnOnce(&Path) -> std::io::Result<()>) {
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
