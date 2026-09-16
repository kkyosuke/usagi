//! PTY の確保と所有、terminal runtime の composition。

use std::sync::mpsc;

use super::{
    Arc, AtomicU64, BTreeMap, ChildRelease, DurableLaunchSnapshot, ErrorLog,
    GENERIC_TERMINAL_LIMIT, GenericPtySpawner, GenericTerminalRuntime, Geometry, LoginShellProfile,
    Mutex, Ordering, PTY_OBSERVATION_QUEUE_ITEMS, Path, PathBuf, ProcessIdentity, PtySpawner,
    PtyTerminal, PtyWriteError, PtyWriter, Receiver, ResolvedTerminalScope, ShardedTerminalStore,
    SharedTerminalRuntime, SharedUserEnvironment, ShutdownOnUnexpectedWorkerExit, ShutdownRequest,
    SpawnFailure, SpawnProvision, SpawnedChildren, Storage, SyncSender, TerminalProfileResolver,
    TerminalRef, TerminalScopeResolveError, TerminalScopeResolver, TerminalStoreSnapshot,
    TerminateReapError, TrySendError, UnixChildProbe, Workspaces, hydrate_runtime_state,
    open_runtime_state, provisioned_agent_command, public_terminal_environment, resolved_os_user,
    send_agent_observation, with_user_environment,
};

pub(super) struct TrustedLoginShell {
    pub(super) profile: LoginShellProfile,
    /// The configured environment for the launch's workspace, resolved at launch
    /// time. `None` in tests that exercise only the shell profile.
    pub(super) environment: Option<Arc<SharedUserEnvironment>>,
    /// Where the workspace whose configured bindings apply is found. A launch
    /// names its workspace, so a daemon holding several resolves it per request
    /// instead of binding the one it was started in. `None` in tests that
    /// exercise only the shell profile, which then use [`Self::workspace_root`].
    pub(super) workspaces: Option<Workspaces>,
    /// The repository the configured workspace bindings belong to, when no
    /// registry is bound.
    pub(super) workspace_root: PathBuf,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_terminal_profile_contract
impl TrustedLoginShell {
    /// The workspace whose configured bindings this launch inherits.
    pub(super) fn launch_workspace_root(
        &self,
        request: &usagi_core::domain::terminal_launch::TerminalLaunchRequest,
    ) -> Result<PathBuf, usagi_core::domain::terminal_launch::TerminalLaunchValidationError> {
        let Some(workspaces) = self.workspaces.as_ref() else {
            return Ok(self.workspace_root.clone());
        };
        // The scope resolver has already refused a workspace this daemon does not
        // hold, so a miss here is a fenced launch that lost its workspace between
        // the two steps. It fails closed rather than inheriting another
        // workspace's environment.
        workspaces
            .workspace(request.scope.workspace_id)
            .map(|tenant| tenant.root().to_path_buf())
            .ok_or(
                usagi_core::domain::terminal_launch::TerminalLaunchValidationError::ScopeMismatch,
            )
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=production_terminal_profile_contract
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
        let workspace_root = self.launch_workspace_root(request)?;
        let user = environment.resolved(&workspace_root).map_err(|_| {
            usagi_core::domain::terminal_launch::TerminalLaunchValidationError::InvalidEnvironment
        })?;
        with_user_environment(resolved, &user)
    }
}

pub(super) fn terminal_environment() -> BTreeMap<String, String> {
    terminal_environment_from(|name| std::env::var(name).ok())
}

/// The same composition against an injected reader of the daemon's own
/// environment.
///
/// Splitting it out is what makes the `USER` precedence observable: on a
/// developer machine the inherited name usually equals the resolved one, so a
/// test reading the real environment cannot tell "the resolver won" from "the
/// inherited value happened to match".
pub(super) fn terminal_environment_from(
    inherited: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    public_terminal_environment(inherited, resolved_os_user())
}

/// Resolve the daemon-generation-wide generic Terminal PTY ceiling.
///
/// A malformed settings document must not make the daemon itself unavailable;
/// Config and doctor still surface that document error, while daemon admission
/// remains bounded by the compiled default.
pub(super) fn terminal_capacity_limit(data_dir: &Path) -> usize {
    Storage::new(data_dir)
        .load_settings()
        .map_or(GENERIC_TERMINAL_LIMIT, |settings| {
            settings.terminal_max_concurrent.get()
        })
}

/// Resolves the complete client fence for a generic terminal. Unlike the Agent
/// resolver, generic terminal requests already carry a worktree ID, so the
/// runtime verifies that exact identity before admitting a PTY spawn.
pub(super) struct SharedTerminalScopeResolver(pub(super) Workspaces);

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_backend_factory_preserves_terminal_arguments_and_completes_store_routes
impl TerminalScopeResolver for SharedTerminalScopeResolver {
    fn resolve_available_scope(
        &self,
        requested: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Result<ResolvedTerminalScope, TerminalScopeResolveError> {
        let tenant = self
            .0
            .workspace(requested.workspace_id)
            .ok_or(TerminalScopeResolveError::Unavailable)?;
        let runtime = tenant
            .runtime()
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

pub(super) enum AgentPtyObservation {
    Output(TerminalRef, Vec<u8>),
    /// The child is gone. Its identity proof rides along so that the durable
    /// exit is still committed by a process that can prove the child was its
    /// own, and the proof is released the instant that commit is behind us.
    Exited(TerminalRef, i32, Option<ChildRelease>),
    Shutdown,
}

/// Process-local counters for the bounded PTY-to-registry pipeline. They only
/// contain byte counts; terminal output and terminal identities are never
/// recorded in metrics or logs.
#[derive(Default)]
pub(super) struct TerminalPipelineMetrics {
    pub(super) backpressured_bytes: AtomicU64,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=metrics_snapshot_is_served_through_the_daemon_endpoint
impl TerminalPipelineMetrics {
    pub(super) fn observe_backpressure(&self, bytes: usize) {
        self.backpressured_bytes
            .fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
    }
}

/// The daemon-owned PTY spawner/writer for Agent runtimes.  It spawns the real
/// rendered plan, drains output to the Agent owner, and reaps the child to
/// commit a durable exit — never a client-driven process.
pub(super) struct AgentPty {
    pub(super) terminals: BTreeMap<String, OwnedPty>,
    pub(super) selected: Option<String>,
    pub(super) observations: SyncSender<AgentPtyObservation>,
    pub(super) metrics: Arc<TerminalPipelineMetrics>,
    pub(super) environment: BTreeMap<String, String>,
    pub(super) children: Arc<SpawnedChildren>,
    pub(super) shutdown: Arc<ShutdownRequest>,
}

pub(super) struct OwnedPty {
    pub(super) terminal: TerminalRef,
    pub(super) pty: Arc<Mutex<PtyTerminal>>,
}

pub(super) fn daemon_pty_failure_entry(
    owner: &str,
    action: &str,
    terminal: &TerminalRef,
    error: &str,
) -> String {
    let session = match terminal.session_id.as_ref() {
        Some(session) => session.as_str(),
        None => "workspace-root".to_owned(),
    };
    format!(
        "daemon {owner} PTY failed: action={action} generation={} workspace={} session={session} worktree={} terminal={} error={error}",
        terminal.daemon_generation,
        terminal.workspace_id.as_str(),
        terminal.worktree_id.as_str(),
        terminal.terminal_id.as_str(),
    )
}

pub(super) fn release_owned_pty(
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
    pub(super) fn new(
        environment: BTreeMap<String, String>,
        metrics: Arc<TerminalPipelineMetrics>,
        children: Arc<SpawnedChildren>,
        shutdown: Arc<ShutdownRequest>,
    ) -> (Self, Receiver<AgentPtyObservation>) {
        let (observations, receiver) = mpsc::sync_channel(PTY_OBSERVATION_QUEUE_ITEMS);
        (
            Self {
                terminals: BTreeMap::new(),
                selected: None,
                observations,
                metrics,
                environment,
                children,
                shutdown,
            },
            receiver,
        )
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=root_restart_rolls_over_two_real_pty_children_without_provider_resume
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
        let (program, argv) = provisioned_agent_command(&plan.program, &plan.argv, provision);
        let environment = provision.compose_environment(&self.environment);
        let pty = match PtyTerminal::spawn_with(
            &program,
            &argv,
            &environment.into_iter().collect::<Vec<_>>(),
            &plan.working_directory,
            Geometry { cols: 80, rows: 24 },
        ) {
            Ok(pty) => pty,
            Err(error) => {
                ErrorLog::record(&daemon_pty_failure_entry(
                    "Agent",
                    "spawn",
                    terminal,
                    &error.to_string(),
                ));
                return Err(SpawnFailure::Definite);
            }
        };
        let Some(pid) = pty.process_id() else {
            ErrorLog::record(&daemon_pty_failure_entry(
                "Agent",
                "observe-child",
                terminal,
                "spawned child did not expose a process id",
            ));
            return Err(SpawnFailure::Ambiguous);
        };
        let reader = match pty.reader() {
            Ok(reader) => reader,
            Err(error) => {
                ErrorLog::record(&daemon_pty_failure_entry(
                    "Agent",
                    "open-reader",
                    terminal,
                    &error.to_string(),
                ));
                return Err(SpawnFailure::Ambiguous);
            }
        };
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
        let shutdown = Arc::clone(&self.shutdown);
        // The identity is observed before the watcher owns it, so the token this
        // thread carries is the very one the exit observation hands back. Every
        // way out of the thread — a drained reader, an unreadable wait, a
        // receiver that hung up — drops it, so no dead pid keeps its proof.
        let (identity, release) =
            self.children
                .observe(&UnixChildProbe, pid, "daemon-owned-agent-pty");
        std::thread::spawn(move || {
            let mut lifecycle = ShutdownOnUnexpectedWorkerExit::new(shutdown);
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
            let Ok(status) = exit_pty
                .lock()
                .map_or(Err(()), |pty| pty.wait().map_err(|_| ()))
            else {
                return;
            };
            if observations
                .send(AgentPtyObservation::Exited(
                    output_terminal,
                    status,
                    release,
                ))
                .is_ok()
            {
                lifecycle.finish();
            }
        });
        Ok(identity)
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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=root_ipc_fixture_codex_survives_disconnect_and_replays_final
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

pub(super) enum PtyObservation {
    Output(usagi_core::domain::id::TerminalRef, Vec<u8>),
    /// Carries the child's identity proof for the same reason the Agent
    /// observation does: the commit needs it, and nothing after the commit does.
    Exited(
        usagi_core::domain::id::TerminalRef,
        i32,
        Option<ChildRelease>,
    ),
    Shutdown,
}

pub(super) struct DaemonPty {
    pub(super) terminals: BTreeMap<String, OwnedPty>,
    pub(super) selected: Option<String>,
    pub(super) observations: SyncSender<PtyObservation>,
    pub(super) metrics: Arc<TerminalPipelineMetrics>,
    pub(super) children: Arc<SpawnedChildren>,
    pub(super) shutdown: Arc<ShutdownRequest>,
}

impl DaemonPty {
    pub(super) fn new(
        metrics: Arc<TerminalPipelineMetrics>,
        children: Arc<SpawnedChildren>,
        shutdown: Arc<ShutdownRequest>,
    ) -> (Self, Receiver<PtyObservation>) {
        let (observations, receiver) = mpsc::sync_channel(PTY_OBSERVATION_QUEUE_ITEMS);
        (
            Self {
                terminals: BTreeMap::new(),
                selected: None,
                observations,
                metrics,
                children,
                shutdown,
            },
            receiver,
        )
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=real_pty_generic_terminal_survives_normal_quit_and_tui_sigkill_without_respawn
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
        let pty = match PtyTerminal::spawn_with(
            &launch.snapshot.program,
            &launch.snapshot.arguments,
            &environment,
            &launch.snapshot.working_directory,
            geometry,
        ) {
            Ok(pty) => pty,
            Err(error) => {
                ErrorLog::record(&daemon_pty_failure_entry(
                    "terminal",
                    "spawn",
                    terminal,
                    &error.to_string(),
                ));
                return Err(SpawnFailure::Definite);
            }
        };
        let Some(pid) = pty.process_id() else {
            ErrorLog::record(&daemon_pty_failure_entry(
                "terminal",
                "observe-child",
                terminal,
                "spawned child did not expose a process id",
            ));
            return Err(SpawnFailure::Ambiguous);
        };
        let reader = match pty.reader() {
            Ok(reader) => reader,
            Err(error) => {
                ErrorLog::record(&daemon_pty_failure_entry(
                    "terminal",
                    "open-reader",
                    terminal,
                    &error.to_string(),
                ));
                return Err(SpawnFailure::Ambiguous);
            }
        };
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
        let shutdown = Arc::clone(&self.shutdown);
        // As in the Agent spawner: the watcher thread owns the release token, so
        // the proof lives exactly as long as this child does.
        let (identity, release) = self
            .children
            .observe(&UnixChildProbe, pid, "daemon-owned-pty");
        std::thread::spawn(move || {
            let mut lifecycle = ShutdownOnUnexpectedWorkerExit::new(shutdown);
            let mut reader = reader;
            let mut bytes = [0_u8; 4096];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                let observation =
                    PtyObservation::Output(output_terminal.clone(), bytes[..count].to_vec());
                if send_pty_observation(&output_sender, observation, count, &metrics).is_err() {
                    // The lifecycle owner dropped the observer. Do not move on
                    // to a child wait that could retain this reader forever;
                    // returning also releases the child-identity proof.
                    return;
                }
            }
            let Ok(status) = exit_pty
                .lock()
                .map_or(Err(()), |pty| pty.wait().map_err(|_| ()))
            else {
                return;
            };
            if output_sender
                .send(PtyObservation::Exited(output_terminal, status, release))
                .is_ok()
            {
                lifecycle.finish();
            }
        });
        Ok(identity)
    }

    fn terminate_reap(
        &mut self,
        terminal: &TerminalRef,
    ) -> Result<(), usagi_daemon::usecase::generic_terminal::GenericTerminateReapError> {
        use usagi_daemon::usecase::generic_terminal::GenericTerminateReapError;

        let key = terminal.terminal_id.as_str();
        let pty = Arc::clone(
            &self
                .terminals
                .get(&key)
                .filter(|entry| entry.terminal.fences(terminal))
                .ok_or(GenericTerminateReapError)?
                .pty,
        );
        pty.lock()
            .map_err(|_| GenericTerminateReapError)?
            .terminate_reap()
            .map_err(|_| GenericTerminateReapError)?;
        release_owned_pty(&mut self.terminals, &mut self.selected, terminal);
        Ok(())
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=real_pty_generic_terminal_survives_normal_quit_and_tui_sigkill_without_respawn
pub(super) fn send_pty_observation(
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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=real_pty_entry_resize_quit_and_reattach_restore_terminal
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

pub(super) struct SharedTerminal(
    pub(super)  Arc<
        Mutex<
            GenericTerminalRuntime<
                TrustedLoginShell,
                ShardedTerminalStore,
                DaemonPty,
                SharedTerminalScopeResolver,
            >,
        >,
    >,
);

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_backend_factory_preserves_terminal_arguments_and_completes_store_routes
impl usagi_daemon::usecase::terminal_owner::TerminalOwner for SharedTerminal {
    fn handle(
        &mut self,
        context: usagi_daemon::usecase::terminal_owner::TerminalRequestContext,
        request: usagi_core::infrastructure::ipc::TerminalRequest,
    ) -> Result<
        usagi_daemon::usecase::terminal_owner::TerminalResponse,
        usagi_core::infrastructure::ipc::ProtocolError,
    > {
        self.0
            .lock()
            .map_err(|_| {
                usagi_core::infrastructure::ipc::ProtocolError::new(
                    usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                    "terminal owner is unavailable",
                )
            })?
            .handle(context, request)
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
    fn disconnect(&mut self, _connection: usagi_core::domain::id::ConnectionId) {
        // `SharedAgent::disconnect` enqueues the one cleanup operation for both
        // owners. Running generic cleanup here as well would put this connection
        // worker back behind the runtime mutex and defeat bounded socket life.
    }
}

#[allow(clippy::too_many_arguments)] // Composition injects each terminal dependency separately.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_backend_factory_preserves_terminal_arguments_and_completes_store_routes
pub(super) fn new_terminal_runtime(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    repo_root: PathBuf,
    pty: DaemonPty,
    workspaces: Workspaces,
    environment: Arc<SharedUserEnvironment>,
    retention: usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention,
    children: &Arc<SpawnedChildren>,
    hydrate_retained: bool,
    terminal_limit: usize,
) -> std::io::Result<SharedTerminalRuntime> {
    let state = open_runtime_state(data_dir, generation, children, terminal_limit)?;
    let snapshot = if hydrate_retained {
        hydrate_runtime_state(&state, "generic terminal")?.terminals
    } else {
        TerminalStoreSnapshot::default()
    };
    let store = ShardedTerminalStore::new(state);
    let runtime = GenericTerminalRuntime::from_snapshot_with_retention_and_limit(
        generation,
        TrustedLoginShell {
            // The launch cwd is replaced by the authoritative resolved scope, so
            // this placeholder never reaches a spawned child.
            profile: LoginShellProfile::new(terminal_environment(), repo_root.clone()),
            environment: Some(environment),
            workspaces: Some(Arc::clone(&workspaces)),
            workspace_root: repo_root,
        },
        store,
        pty,
        SharedTerminalScopeResolver(workspaces),
        snapshot,
        retention,
        terminal_limit,
    )
    .map_err(|_| std::io::Error::other("invalid generic terminal snapshot"))?;
    Ok(Arc::new(Mutex::new(runtime)))
}
