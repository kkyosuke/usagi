//! daemon の背景 worker 群と shutdown、orphan / retention の回収。

use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::panic::{self, AssertUnwindSafe};

use super::agent_provisioning;

use super::{
    AGENT_READINESS_TERMINATE_GRACE, AcceptedStream, AdmissionGate, AgentPtyObservation,
    AgentReadinessCommand, Arc, BTreeMap, BTreeSet, BackgroundWorker, ClientWorkers, Collection,
    Condvar, ConnectionId, ConnectionWorkspace, DRAINING_COLLECTION_TICK, DaemonRecord,
    DefaultModel, Duration, ErrorLog, FileWorkspaceFence, FileWorkspaceFences, GenerationRegistry,
    GenerationRole, GhProcessPort, MonotonicClock, Mutex, OwnedFd, PR_REFRESH_FRESHNESS_MS,
    PR_REFRESH_PER_TICK, PathBuf, PendingTeardown, PrProjection, PrProjectionQueue, PtyObservation,
    RETENTION_GC_TICK, Receiver, RefCell, RefreshWorker, SESSION_TEARDOWN_TICK,
    ShardedRuntimeState, SharedAgentRuntime, SharedPrInventory, SharedSessionRuntime,
    SharedSessionTeardown, SharedTerminalRuntime, SharedUserEnvironment, ShutdownRequest,
    ShutdownSignal, SyncSender, SystemGit, SystemSessionWorktreeIo, SystemTenantOpener,
    TeardownEffect, TeardownJournal, TeardownSignal, TenantRegistry, Terminator, Workspaces,
    WorktreeTeardown, active_cleanup_lease, bounded_readiness_command,
    clean_orphan_session_resources, collect_if_drained, drain_pending_teardowns, known_sessions,
    shipping_retention_limits, signal_exact_process, start_connection_cleanup_worker_with,
};

/// Product-owned, non-secret pre-spawn readiness boundary.  Implementations
/// may discover an executable and invoke its public status command, but never
/// read, persist, or return credentials, configuration paths, argv, or raw OS
/// failures.  Keeping it injected makes the root composable with fixture
/// executables without installing or authenticating a real CLI.
pub(super) trait AgentReadinessProbe: Send + Sync {
    fn observe(&self, product: &str) -> AgentReadiness;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum AgentReadiness {
    Ready,
    #[default]
    Unavailable,
}

/// The per-product half of one readiness probe's policy, resolved from the
/// shared agent CLI vocabulary. The terminate grace and the coalescing rule
/// stay owned by this root because they are properties of how it runs a child,
/// not of the product being probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReadinessBounds {
    pub(super) timeout: Duration,
    pub(super) output_limit: usize,
}

impl ReadinessBounds {
    /// Reads the bounds off the same value that names the status command, so a
    /// launcher cannot run one product's command under another product's
    /// budget. Keeping the resolution here — rather than inline in the real-IO
    /// probe — lets a test prove the root still carries the per-product budget
    /// without spawning a child.
    pub(super) fn for_probe(probe: AgentReadinessCommand) -> Self {
        Self {
            timeout: probe.timeout(),
            output_limit: probe.output_limit(),
        }
    }
}

#[derive(Default)]
pub(super) struct ReadinessSlot {
    pub(super) running: bool,
    pub(super) result: Option<AgentReadiness>,
}

#[derive(Default)]
pub(super) struct ReadinessState {
    pub(super) providers: BTreeMap<String, ReadinessSlot>,
}

/// Runs at most one bounded status child per provider. Callers arriving during
/// that run share its safe success/failure result instead of creating another
/// process or reader thread.
pub(super) struct SystemAgentReadiness {
    pub(super) state: Mutex<ReadinessState>,
    pub(super) completed: Condvar,
    pub(super) terminate_grace: Duration,
    /// `$HOME`, for the provider state directory a gateway provider's CLI must
    /// be pointed at. A provider that needs one and has no home is unavailable:
    /// probing it in the shared CLI's default home would answer for the other
    /// provider that lives there.
    pub(super) home: Option<PathBuf>,
    /// Where a provider's configured API key comes from. The probe resolves it
    /// the same way a launch does, because "is this provider usable" is mostly
    /// "is its credential configured" — and a probe run without the key refuses
    /// a provider that would have launched.
    pub(super) environment: Option<Arc<SharedUserEnvironment>>,
    /// The workspace whose settings the credential is read from. Gateway and
    /// credential names are reserved from workspace bindings, so this resolves
    /// the same value for every workspace; the daemon's own root is simply the
    /// one that always exists.
    pub(super) workspace: PathBuf,
}

impl Default for SystemAgentReadiness {
    fn default() -> Self {
        Self {
            state: Mutex::new(ReadinessState::default()),
            completed: Condvar::new(),
            terminate_grace: AGENT_READINESS_TERMINATE_GRACE,
            home: None,
            environment: None,
            workspace: PathBuf::new(),
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=production_dispatch_uses_the_trusted_root_before_and_after_session_creation
impl AgentReadinessProbe for SystemAgentReadiness {
    fn observe(&self, product: &str) -> AgentReadiness {
        // Which products exist, and which status command proves each one usable,
        // is the shared agent CLI vocabulary owned by core domain settings. This
        // root only runs the resolved probe, so a provider is recognised without
        // a second table here (#609). An unmodelled product still fails closed.
        // The budget travels with the probe for the same reason: how long
        // `agy models` may take is a fact about Antigravity, not about this
        // root, and a single shared deadline reported an installed and
        // authenticated CLI as unavailable.
        let Some(agent) = DefaultModel::from_selector(product) else {
            return AgentReadiness::Unavailable;
        };
        let probe = agent.readiness_command();
        // A provider that is a shared CLI plus an environment is only probed
        // honestly under that environment: `claude auth status` run bare answers
        // for the user's own Anthropic account, not for this profile.
        let Ok(environment) = self.provider_environment(agent) else {
            return AgentReadiness::Unavailable;
        };
        self.ready_command(
            product,
            probe.program(),
            probe.arguments(),
            ReadinessBounds::for_probe(probe),
            &environment,
        )
    }
}

impl SystemAgentReadiness {
    /// The environment that makes this probe answer for `agent` rather than for
    /// whichever provider shares its executable. `Err` means the provider cannot
    /// be probed honestly — a missing home for a provider that needs its own
    /// config directory, or a credential it declares but nothing supplies — and
    /// the caller reports it unavailable rather than asking a question whose
    /// answer would be about something else.
    ///
    /// The gateway itself is assembled by the **same** function the launch uses
    /// (`agent_provisioning::provider_gateway_environment`), so "the probe
    /// answers for the product that would launch" is structural rather than two
    /// separate tables kept in step by hand. Only the credential rule differs,
    /// and in the stricter direction: provisioning tolerates a missing key
    /// because this probe is what refuses the launch first, with a reason an
    /// operator can act on.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=production_dispatch_uses_the_trusted_root_before_and_after_session_creation
    pub(super) fn provider_environment(
        &self,
        agent: DefaultModel,
    ) -> Result<Vec<(String, String)>, ()> {
        let user = match agent.credential_binding() {
            Some(_) => self
                .environment
                .as_ref()
                .ok_or(())?
                .resolved(&self.workspace)
                .map_err(|_| ())?,
            None => BTreeMap::new(),
        };
        let environment =
            agent_provisioning::provider_gateway_environment(agent, self.home.as_deref(), &user)
                .map_err(|()| {
                    ErrorLog::record(&format!(
                        "agent readiness: {} cannot be probed without a resolved $HOME",
                        agent.selector()
                    ));
                })?;
        if let Some((source, target)) = agent.credential_binding()
            && !environment.iter().any(|(name, _)| name.as_str() == target)
        {
            // The recovery a user needs, named once where an operator can
            // find it. The wire answer stays the generic safe refusal.
            ErrorLog::record(&format!(
                "agent readiness: {} is unavailable because {source} is not configured",
                agent.selector()
            ));
            return Err(());
        }
        Ok(environment
            .into_iter()
            .map(|(name, value)| (name.as_str().to_owned(), value))
            .collect())
    }

    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=production_dispatch_uses_the_trusted_root_before_and_after_session_creation
    pub(super) fn ready_command(
        &self,
        product: &str,
        program: &str,
        arguments: &[&str],
        bounds: ReadinessBounds,
        environment: &[(String, String)],
    ) -> AgentReadiness {
        let Ok(mut state) = self.state.lock() else {
            return AgentReadiness::Unavailable;
        };
        let slot = state.providers.entry(product.to_owned()).or_default();
        if slot.running {
            let Ok((state_after_wait, timeout)) = self.completed.wait_timeout_while(
                state,
                bounds.timeout + self.terminate_grace,
                |state| {
                    state
                        .providers
                        .get(product)
                        .is_some_and(|slot| slot.running)
                },
            ) else {
                return AgentReadiness::Unavailable;
            };
            if timeout.timed_out() {
                return AgentReadiness::Unavailable;
            }
            return state_after_wait
                .providers
                .get(product)
                .and_then(|slot| slot.result)
                .unwrap_or(AgentReadiness::Unavailable);
        }
        slot.running = true;
        slot.result = None;
        drop(state);

        let result = bounded_readiness_command(
            program,
            arguments,
            environment,
            bounds,
            self.terminate_grace,
        );
        let Ok(mut state) = self.state.lock() else {
            return AgentReadiness::Unavailable;
        };
        let slot = state
            .providers
            .get_mut(product)
            .expect("running readiness provider remains registered");
        slot.running = false;
        slot.result = Some(result);
        self.completed.notify_all();
        result
    }
}

/// The only per-connection state retained while cleanup is delayed: the live
/// connection census itself. Its cardinality is bounded by the accepted-worker
/// limit, unlike a queue containing one item for every historical disconnect.
#[derive(Clone)]
pub(super) struct ConnectionCleanup {
    pub(super) live: Arc<Mutex<BTreeMap<ConnectionId, u32>>>,
    pub(super) wake: SyncSender<()>,
}

pub(super) struct ConnectionCleanupInbox {
    pub(super) live: Arc<Mutex<BTreeMap<ConnectionId, u32>>>,
    pub(super) wake: Receiver<()>,
}

impl ConnectionCleanup {
    pub(super) fn connected(&self, connection: ConnectionId, peer_pid: u32) {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(connection, peer_pid);
    }

    pub(super) fn disconnected(&self, connection: ConnectionId) {
        let removed = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&connection)
            .is_some();
        if removed {
            // A full one-item channel already promises a sweep that will observe
            // this removal. Never wait for the consumer from a socket worker.
            let _ = self.wake.try_send(());
        }
    }
}

impl ConnectionCleanupInbox {
    pub(super) fn live(&self) -> (BTreeSet<ConnectionId>, BTreeSet<u32>) {
        let live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            live.keys().copied().collect(),
            live.values().copied().collect(),
        )
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=drawer_close_reopen_continues_input_on_the_same_daemon_connection
pub(super) fn start_connection_cleanup_worker(
    agent: SharedAgentRuntime,
    terminal: SharedTerminalRuntime,
    disconnected: ConnectionCleanupInbox,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    start_connection_cleanup_worker_with(disconnected, move |disconnected| {
        let _panic = ShutdownOnWorkerPanic {
            shutdown: Arc::clone(&shutdown),
        };
        if let Ok(mut agent) = agent.lock() {
            // The snapshot is taken while this owner is locked. A newly
            // registered connection cannot add owner state until after the
            // sweep, so it cannot be removed by a snapshot that predates it.
            let (live, _) = disconnected.live();
            agent.retain_live_connections(&live);
            agent.retain_live_mcp_connections(&live);
        }
        if let Ok(mut terminal) = terminal.lock() {
            // The generic owner has a separate mutex, so it needs a fresh
            // census snapshot under that mutex for the same ordering guarantee.
            let (live, _) = disconnected.live();
            terminal.retain_live_connections(&live);
        }
    })
}

/// Owns every daemon-wide worker from its first successful spawn.
///
/// Startup errors and accept-loop unwinds therefore take the same close-and-join
/// path as planned shutdown instead of detaching the handles accumulated so far.
pub(super) struct DaemonBackgroundWorkers {
    pub(super) handles: Vec<std::thread::JoinHandle<()>>,
    pub(super) shutdown: Arc<ShutdownRequest>,
    pub(super) projection: Arc<PrProjectionQueue>,
    pub(super) agent_observations: Option<SyncSender<AgentPtyObservation>>,
    pub(super) terminal_observations: Option<SyncSender<PtyObservation>>,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=lifecycle_owner_closes_sources_and_joins_every_critical_worker
impl DaemonBackgroundWorkers {
    pub(super) fn new(shutdown: Arc<ShutdownRequest>, projection: Arc<PrProjectionQueue>) -> Self {
        Self {
            handles: Vec::new(),
            shutdown,
            projection,
            agent_observations: None,
            terminal_observations: None,
        }
    }

    pub(super) fn bind_agent_observations(&mut self, sender: SyncSender<AgentPtyObservation>) {
        self.agent_observations = Some(sender);
    }

    pub(super) fn bind_terminal_observations(&mut self, sender: SyncSender<PtyObservation>) {
        self.terminal_observations = Some(sender);
    }

    pub(super) fn push(&mut self, handle: std::thread::JoinHandle<()>) {
        self.handles.push(handle);
    }

    pub(super) fn shutdown_and_join(&mut self) {
        self.shutdown.request();
        if let Some(sender) = self.agent_observations.take() {
            let _ = sender.send(AgentPtyObservation::Shutdown);
        }
        if let Some(sender) = self.terminal_observations.take() {
            let _ = sender.send(PtyObservation::Shutdown);
        }
        self.projection.close();
        for worker in self.handles.drain(..) {
            if worker.join().is_err() {
                ErrorLog::record("daemon background worker panicked during shutdown");
            }
        }
    }
}

impl Drop for DaemonBackgroundWorkers {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

/// Retain one accepted connection's worker so a collection can unblock and join it.
///
/// A worker whose shutdown half could not be duplicated is deliberately *not*
/// retained: a collection could never unblock it, so pretending it is joinable
/// would park retirement. Production accept loops fail closed before spawning
/// in that case; the error branch remains defensive for injected callers.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=accepted_stream_observes_peer_close_behind_buffered_data
pub(super) fn retain_client_worker(
    workers: &ClientWorkers,
    unblock: std::io::Result<AcceptedStream>,
    handle: std::thread::JoinHandle<()>,
) {
    match unblock {
        Ok(unblock) => {
            let report = workers.register(Box::new(unblock), handle);
            if !report.is_clean() {
                ErrorLog::record(&format!(
                    "daemon client worker retired after collection with failures: {report:?}"
                ));
            }
        }
        Err(error) => ErrorLog::record(&format!(
            "daemon client worker is not collectable: \
             the accepted stream could not be duplicated: {error}"
        )),
    }
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=pr_snapshot_events_cover_success_scoped_and_lane_errors
pub(super) fn spawn_pr_refresh_worker<R, C>(
    pr_inventory: SharedPrInventory,
    daemon_dir: Option<PathBuf>,
    shutdown: Arc<ShutdownRequest>,
    runner: R,
    clock: C,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    R: GhProcessPort + Clone + Send + 'static,
    C: MonotonicClock + Send + 'static,
{
    std::thread::Builder::new()
        .name("usagi-pr-refresh".to_string())
        .spawn(move || {
            let worker_health = shutdown.monitor_background_worker(BackgroundWorker::PrRefresh);
            let mut worker =
                RefreshWorker::new(runner, clock, PR_REFRESH_PER_TICK, PR_REFRESH_FRESHNESS_MS);
            if let Ok(mut projector) = pr_inventory.lock()
                && worker.rebuild(&mut projector).is_err()
            {
                ErrorLog::record("PR refresh schedule rebuild failed");
            }
            while !shutdown.is_requested() {
                // The inventory is daemon-wide while sessions belong to
                // workspaces, so what it may keep is the union over every
                // workspace this data directory knows — not just the ones held
                // right now, or a closed workspace would lose its records.
                if let Some(daemon_dir) = &daemon_dir
                    && let Some(retained) = known_sessions(daemon_dir)
                    && let Ok(mut projector) = pr_inventory.lock()
                    && projector.retain_sessions(&retained).is_err()
                {
                    ErrorLog::record("PR inventory session reconciliation failed");
                }
                let due = pr_inventory
                    .lock()
                    .ok()
                    .and_then(|mut projector| worker.claim_due(&mut projector).ok())
                    .unwrap_or_default();
                for (identity, result) in worker.fetch_many(due) {
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
            worker_health.finish_planned();
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
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_session_remove_is_accepted_before_the_daemon_tears_the_worktree_down
pub(super) fn start_session_teardown_worker(
    workspaces: Workspaces,
    agent: SharedAgentRuntime,
    terminal: SharedTerminalRuntime,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<(Arc<TeardownSignal>, std::thread::JoinHandle<()>)> {
    let signal = Arc::new(TeardownSignal::new());
    let worker = spawn_session_teardown_worker(
        WorkspacesTeardown { workspaces },
        AgentAndWorktreeTeardown {
            agent,
            terminal,
            worktree: WorktreeTeardown::new(SystemGit, SystemSessionWorktreeIo),
        },
        Arc::clone(&signal),
        shutdown,
        SESSION_TEARDOWN_TICK,
    )?;
    Ok((signal, worker))
}

/// The unfinished teardowns of every workspace this daemon holds.
///
/// One worker drains them all, because the work is process-level (`git worktree
/// remove` plus `remove_dir_all`) rather than per workspace. Each teardown names
/// the repository it belongs to, which is what routes its outcome back to the
/// workspace that recorded it.
pub(super) struct WorkspacesTeardown {
    pub(super) workspaces: Workspaces,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_session_remove_is_accepted_before_the_daemon_tears_the_worktree_down
impl TeardownJournal for WorkspacesTeardown {
    fn pending(&self) -> Vec<PendingTeardown> {
        self.workspaces
            .all()
            .into_iter()
            .flat_map(|tenant| SharedSessionTeardown::new(tenant.runtime().clone()).pending())
            .collect()
    }

    fn finish(
        &self,
        teardown: &PendingTeardown,
        outcome: Result<(), String>,
    ) -> Result<(), String> {
        // A teardown outcome belongs to the workspace whose durable record
        // produced it. Recording it anywhere else would leave that record
        // `Deleting` forever while corrupting another workspace's state.
        let tenant = self
            .workspaces
            .workspace_at(&teardown.repository_root)
            .ok_or_else(|| "session lifecycle owner is unavailable".to_owned())?;
        SharedSessionTeardown::new(tenant.runtime().clone()).finish(teardown, outcome)
    }
}

/// Orders session destruction so no Agent process, generic terminal, or durable
/// inventory row can outlive the worktree scope it belongs to.
///
/// Both runtime kinds are closed before the worktree is touched, because both
/// hold a PTY child whose cwd is inside that worktree and a claim in the shared
/// capacity pool. A terminal left running keeps the checkout busy so
/// `git worktree remove` fails, and keeps its pool slot for the life of the
/// daemon.
pub(super) struct AgentAndWorktreeTeardown<E> {
    pub(super) agent: SharedAgentRuntime,
    pub(super) terminal: SharedTerminalRuntime,
    pub(super) worktree: E,
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=production_session_remove_is_accepted_before_the_daemon_tears_the_worktree_down
impl<E: TeardownEffect> TeardownEffect for AgentAndWorktreeTeardown<E> {
    fn tear_down(&self, teardown: &PendingTeardown) -> Result<(), String> {
        self.agent
            .lock()
            .map_err(|_| "agent owner is unavailable".to_owned())?
            .close_session(teardown.session_id)
            .map_err(|error| error.message)?;
        self.terminal
            .lock()
            .map_err(|_| "terminal owner is unavailable".to_owned())?
            .close_session(teardown.session_id)
            .map_err(|error| error.message)?;
        self.worktree.tear_down(teardown)
    }
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=production_session_remove_is_accepted_before_the_daemon_tears_the_worktree_down
pub(super) fn spawn_session_teardown_worker<J, E>(
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
            let worker_health =
                shutdown.monitor_background_worker(BackgroundWorker::SessionTeardown);
            let cancel = Arc::clone(&shutdown);
            let cancelled = move || cancel.is_requested();
            // The first drain resumes a teardown left `Deleting` by a previous
            // daemon. Afterwards durable state can only gain pending work
            // through an admission notification. A periodic re-read is needed
            // only while durable finalization is failing.
            let mut should_drain = true;
            while !shutdown.is_requested() {
                let mut retry_finalization = false;
                if should_drain {
                    for report in drain_pending_teardowns(&journal, &effect, &cancelled) {
                        if let Some(error) = report.effect_error {
                            ErrorLog::record(&format!(
                                "session teardown failed for \"{}\": {error}",
                                report.name
                            ));
                        }
                        if let Some(error) = report.finalize_error {
                            retry_finalization = true;
                            ErrorLog::record(&format!(
                                "session teardown outcome could not be recorded for \"{}\": {error}",
                                report.name
                            ));
                        }
                    }
                }
                if shutdown.is_requested() {
                    break;
                }
                // An admitted removal wakes this immediately; the tick only
                // re-derives the pending set while a teardown whose
                // finalization failed still needs retrying.
                should_drain = signal.wait(tick) || retry_finalization;
            }
            worker_health.finish_planned();
        })
}

pub(super) trait OrphanCleanupPass: Send {
    fn run(&mut self);
}

pub(super) struct AutomaticOrphanCleanup {
    pub(super) workspaces: Workspaces,
    pub(super) gate: AdmissionGate,
}

impl OrphanCleanupPass for AutomaticOrphanCleanup {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=running_daemon_cleans_a_merged_orphan_branch_without_touching_active_sessions
    fn run(&mut self) {
        for tenant in self.workspaces.all() {
            let Some(_lease) = active_cleanup_lease(&self.gate) else {
                break;
            };
            let root = tenant.root().to_path_buf();
            let bound = ConnectionWorkspace {
                tenant,
                workspaces: Arc::clone(&self.workspaces),
            };
            if let Err(error) = clean_orphan_session_resources(&bound, None, true, false, None) {
                ErrorLog::record(&format!(
                    "automatic orphan cleanup deferred for {}: {}",
                    root.display(),
                    error.safe_message()
                ));
            }
        }
    }
}

/// The maintenance loop with its effect and cadence injected for deterministic
/// tests. Waiting before the first pass keeps daemon startup free of Git scans.
pub(super) fn spawn_orphan_cleanup_worker<C>(
    mut clean: C,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    C: OrphanCleanupPass + 'static,
{
    std::thread::Builder::new()
        .name("usagi-orphan-cleanup".to_string())
        .spawn(move || {
            let worker_health = shutdown.monitor_background_worker(BackgroundWorker::OrphanCleanup);
            while !shutdown.wait_for_tick(tick) {
                clean.run();
            }
            worker_health.finish_planned();
        })
}

/// The workspace `serve` fenced for this process, and the fence that keeps it.
///
/// The registry registers the initial tenant without a fence because this one
/// belongs to the process, which is why the idle sweep leaves it alone. Pairing
/// the two here is what lets the sweep give that workspace back as well, once
/// this generation has handed its authority on.
pub(super) struct InitialWorkspaceFence {
    pub(super) root: PathBuf,
    pub(super) fence: Arc<FileWorkspaceFence>,
}

/// Give the startup workspace back once this generation's handoff is durable.
///
/// A planned handoff keeps the old process alive so its PTYs survive the
/// replacement, but it is no longer anyone's authority: it admits reads and its
/// own terminals and refuses everything that would start new work. Holding that
/// workspace's fence for the rest of its life is what refused the new active
/// generation — and every other daemon — the workspace, with the departed
/// owner's pid hint.
///
/// The condition is [`AdmissionGate::handed_off`], never the bare role. The
/// pre-commit barrier enters `draining` *before* the registry commit and
/// reopens to `active` for every handoff that never commits, and that window is
/// exactly where the guard stops the live Agents — so it is also exactly where
/// the startup workspace looks idle. Releasing on the role alone would leave a
/// generation that came back to `active` without the workspace it was started
/// in, and with the fence possibly taken by someone else.
///
/// Returns whether this pass released it, which happens at most once: the
/// registry entry is gone afterwards, so every later tick answers `false`.
pub(super) fn release_initial_workspace<A>(
    tenants: &TenantRegistry<FileWorkspaceFences, SystemTenantOpener>,
    activity: &A,
    initial: &InitialWorkspaceFence,
    gate: &AdmissionGate,
) -> bool
where
    A: usagi_daemon::usecase::tenant::WorkspaceActivity<SharedSessionRuntime>,
{
    gate.handed_off() && tenants.release_initial(&initial.root, activity) && initial.fence.release()
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
pub(super) fn spawn_tenant_retire_worker<A>(
    tenants: Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
    activity: A,
    initial: Option<InitialWorkspaceFence>,
    gate: AdmissionGate,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
    idle_for: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    A: usagi_daemon::usecase::tenant::WorkspaceActivity<SharedSessionRuntime> + Send + 'static,
{
    let idle_for = chrono::Duration::from_std(idle_for)
        .map_err(|_| std::io::Error::other("tenant idle period is out of range"))?;
    std::thread::Builder::new()
        .name("usagi-daemon-tenants".to_string())
        .spawn(move || {
            let worker_health =
                shutdown.monitor_background_worker(BackgroundWorker::TenantRetirement);
            while !shutdown.is_requested() {
                for root in tenants.retire_idle(&activity, chrono::Utc::now(), idle_for) {
                    ErrorLog::record(&format!(
                        "daemon released the idle workspace {}",
                        root.display()
                    ));
                }
                if let Some(initial) = initial.as_ref()
                    && release_initial_workspace(&tenants, &activity, initial, &gate)
                {
                    ErrorLog::record(&format!(
                        "replaced daemon generation released its startup workspace {}",
                        initial.root.display()
                    ));
                }
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
            worker_health.finish_planned();
        })
}

/// Starts the only production retention collector. Launch and exit already
/// collect on the spot; this worker covers an idle daemon, where the age budget
/// and the minimum visibility TTL are the only things still moving.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=the_retention_collector_ticks_until_shutdown_and_stops_when_already_down
pub(super) fn start_retention_gc_worker(
    terminal: SharedTerminalRuntime,
    agent: SharedAgentRuntime,
    durable: ShardedRuntimeState,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let limits = shipping_retention_limits();
    spawn_retention_gc_worker(
        move || {
            // The in-memory budgets first: what the owners stop retaining is what
            // the durable pass is then allowed to collect.
            let mut retained = BTreeSet::new();
            if let Ok(mut terminal) = terminal.lock() {
                terminal.collect_retention_garbage();
                retained.extend(terminal.retained_resources());
            }
            if let Ok(mut agent) = agent.lock() {
                agent.collect_retention_garbage();
                retained.extend(agent.retained_resources());
            }
            if let Err(error) = durable.collect(&retained, &limits) {
                ErrorLog::record(&format!("durable runtime collection deferred: {error}"));
            }
        },
        shutdown,
        RETENTION_GC_TICK,
    )
}

/// The worker loop, with the collection step injected so a test can drive it
/// without a daemon, a PTY, or a store.
#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=the_retention_collector_ticks_until_shutdown_and_stops_when_already_down
pub(super) fn spawn_retention_gc_worker<C>(
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
            let worker_health = shutdown.monitor_background_worker(BackgroundWorker::RetentionGc);
            while !shutdown.is_requested() {
                collect();
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
            worker_health.finish_planned();
        })
}

/// Starts the worker that ends this process after a handoff once its owner shard
/// and global allocator have no claim left.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=the_draining_collector_retries_observations_and_never_outlives_shutdown
pub(super) fn start_draining_collection_worker(
    durable: ShardedRuntimeState,
    registry: GenerationRegistry,
    gate: AdmissionGate,
    generation: usagi_core::domain::id::DaemonGeneration,
    workers: Arc<ClientWorkers>,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_draining_collection_worker(
        move || match collect_if_drained(&registry, &gate, &workers, generation, &durable) {
            Ok(Collection::Collected(report)) => {
                if !report.is_clean() {
                    ErrorLog::record(&format!(
                        "draining generation retired with client worker failures: {report:?}"
                    ));
                }
                true
            }
            Ok(Collection::NotDraining | Collection::Pending(_)) => false,
            Err(error) => {
                ErrorLog::record(&format!("draining generation collection deferred: {error}"));
                // `collect_retired` moves the process-local gate first and the
                // registry second. If the second write failed, this process can
                // no longer serve anything; exit so activation can reclaim the
                // dead draining entry instead of leaving a retired endpoint
                // process alive forever.
                gate.role() == GenerationRole::Retired
            }
        },
        shutdown,
        DRAINING_COLLECTION_TICK,
    )
}

/// The collection loop with the observation injected for deterministic tests.
///
/// `collect` returns `true` only after retirement completed (or failed after the
/// local gate had irreversibly retired). The shutdown request wakes the serve
/// thread, which joins the accept loop and every late-registered client worker
/// before it unlinks the endpoint and exits the process.
#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=the_draining_collector_retries_observations_and_never_outlives_shutdown
pub(super) fn spawn_draining_collection_worker<C>(
    mut collect: C,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    C: FnMut() -> bool + Send + 'static,
{
    std::thread::Builder::new()
        .name("usagi-draining-collection".to_string())
        .spawn(move || {
            let worker_health =
                shutdown.monitor_background_worker(BackgroundWorker::DrainingCollection);
            while !shutdown.is_requested() {
                if collect() {
                    shutdown.request();
                    break;
                }
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
            worker_health.finish_planned();
        })
}

/// Starts the only production PR projection worker.
///
/// It owns every scan and every durable inventory write that PTY output causes.
/// The queue's `recv` parks on a condvar and returns `None` once the queue is
/// closed and drained, so this thread has no timer and no polling.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=pr_snapshot_events_cover_success_scoped_and_lane_errors
pub(super) fn start_pr_projection_worker(
    pr_inventory: SharedPrInventory,
    projection: Arc<PrProjectionQueue>,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let failed_projection = Arc::clone(&projection);
    spawn_critical_worker(
        "usagi-pr-projection",
        BackgroundWorker::PrProjection,
        shutdown,
        move || failed_projection.close(),
        move |_| {
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
        },
    )
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=planned_critical_worker_shutdown_joins_without_a_health_failure
pub(super) fn spawn_critical_worker<R, F>(
    name: &str,
    worker: BackgroundWorker,
    shutdown: Arc<ShutdownRequest>,
    on_failure: F,
    run: R,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    R: FnOnce(&ShutdownRequest) + Send + 'static,
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let monitor = shutdown.monitor_background_worker(worker);
            let result = panic::catch_unwind(AssertUnwindSafe(|| run(&shutdown)));
            if result.is_ok() && shutdown.is_requested() {
                monitor.finish_planned();
            } else {
                // Dropping an unfinished monitor records the failure and raises
                // the shared shutdown fence before source-specific cleanup.
                drop(monitor);
                on_failure();
            }
            if let Err(payload) = result {
                panic::resume_unwind(payload);
            }
        })
}

/// A pipe whose readable end lets `poll(2)` wait for a shutdown request
/// alongside the listening socket.
///
/// A condvar cannot be mixed into a descriptor wait, so the request is mirrored
/// onto a descriptor. The mirroring thread parks on the condvar, which means an
/// idle daemon still performs no timed wakeups.
pub(super) struct ShutdownPipe {
    pub(super) read: OwnedFd,
    pub(super) shutdown: Arc<ShutdownRequest>,
    pub(super) worker: Option<std::thread::JoinHandle<()>>,
}

impl ShutdownPipe {
    /// Creates the pipe and starts mirroring `shutdown` onto it.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
    pub(super) fn mirroring(shutdown: &Arc<ShutdownRequest>) -> std::io::Result<Self> {
        let mut ends = [0_i32; 2];
        // SAFETY: `ends` is a two-element array, exactly what pipe(2) writes.
        if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both descriptors were freshly returned by pipe(2), and each is
        // moved into exactly one `OwnedFd`.
        let read = unsafe { OwnedFd::from_raw_fd(ends[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(ends[1]) };
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
        let requested = Arc::clone(shutdown);
        let worker = std::thread::Builder::new()
            .name("usagi-shutdown-wake".to_string())
            .spawn(move || {
                requested.wait_until_requested();
                // One byte is enough: the reader only needs readiness, and the
                // descriptor is never reused for anything else.
                // SAFETY: writing one byte from a local buffer to the worker's
                // owned pipe descriptor.
                unsafe { libc::write(write.as_raw_fd(), [1_u8].as_ptr().cast(), 1) };
            })?;
        Ok(Self {
            read,
            shutdown: Arc::clone(shutdown),
            worker: Some(worker),
        })
    }

    /// Waits until the listener has a connection or shutdown was requested.
    /// Returns whether the listener is the one that became ready.
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
    pub(super) fn wait_for_listener(&self, listener: std::os::fd::RawFd) -> bool {
        let mut fds = [
            libc::pollfd {
                fd: listener,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.read.as_raw_fd(),
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
        // Wake and join before `read` is closed. The writer is owned by the
        // worker, so it can never write through a raw descriptor number that
        // this process has already closed and possibly reused for another file.
        self.shutdown.request();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Retires the PR projection worker whenever the accept worker exits, including
/// on an unwind, so no thread is left parked on a queue nothing will feed.
pub(super) struct ClosePrProjectionOnExit {
    pub(super) projection: Arc<PrProjectionQueue>,
}

impl Drop for ClosePrProjectionOnExit {
    fn drop(&mut self) {
        self.projection.close();
    }
}

/// Wakes the lifecycle owner whenever the accept worker unwinds or exits.
/// Normal signal-driven shutdown has already set the same flag, so the guard
/// is idempotent on the expected return path.
pub(super) struct ShutdownOnIpcWorkerExit {
    pub(super) shutdown: Arc<ShutdownRequest>,
}

impl Drop for ShutdownOnIpcWorkerExit {
    fn drop(&mut self) {
        self.shutdown.request();
    }
}

/// Shuts the process down when a worker that requires explicit completion is lost.
///
/// Unlike [`ShutdownOnWorkerPanic`], this guard also covers ordinary early
/// returns. The worker marks itself complete only after its final required
/// effect. This is especially important for a standby: [`ShutdownPipe`] requests
/// the internal replacement domain while it drops, so consulting that flag from
/// `Drop` would misclassify an unwind as a planned promotion.
pub(super) struct ShutdownOnUnexpectedWorkerExit {
    pub(super) shutdown: Arc<ShutdownRequest>,
    pub(super) completed: bool,
}

impl ShutdownOnUnexpectedWorkerExit {
    pub(super) fn new(shutdown: Arc<ShutdownRequest>) -> Self {
        Self {
            shutdown,
            completed: false,
        }
    }

    pub(super) fn finish(&mut self) {
        self.completed = true;
    }
}

impl Drop for ShutdownOnUnexpectedWorkerExit {
    fn drop(&mut self) {
        if !self.completed {
            self.shutdown.request();
        }
    }
}

/// Prevents an unwinding daemon worker from leaving an unusable process alive.
///
/// Request dispatch can hold a shared runtime mutex when it panics. Continuing
/// to serve after that unwind would keep the workspace fence while subsequent
/// clients see only a poisoned, unavailable runtime. Normal worker completion
/// does not request shutdown.
pub(super) struct ShutdownOnWorkerPanic {
    pub(super) shutdown: Arc<ShutdownRequest>,
}

impl Drop for ShutdownOnWorkerPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.shutdown.request();
        }
    }
}

pub(super) struct SigtermTerminator;

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_generation_process_is_only_verified_by_its_exact_recorded_identity
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

/// Marks that signal delivery has been prepared. The blocking iterator now lives
/// in the signal thread, so the owner keeps only this proof.
pub(super) struct SignalDelivery;

pub(super) struct SignalShutdown {
    pub(super) shutdown: Arc<ShutdownRequest>,
    pub(super) signals: RefCell<Option<SignalDelivery>>,
    pub(super) flag_ids: RefCell<Vec<signal_hook::SigId>>,
}

impl SignalShutdown {
    pub(super) fn new(shutdown: Arc<ShutdownRequest>) -> Self {
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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_stop_request_retires_the_broker_and_removes_its_endpoint
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
                    let _panic = ShutdownOnWorkerPanic {
                        shutdown: Arc::clone(&requested),
                    };
                    // `forever` normally returns only after a signal. If its
                    // delivery source ever closes, fail stop as well: the raw
                    // signal flag cannot wake the lifecycle owner's condvar on
                    // its own.
                    let _ = prepared.forever().next();
                    requested.request();
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
