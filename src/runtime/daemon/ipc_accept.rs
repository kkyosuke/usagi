//! Unix socket の accept ループと handshake、response の書き出し。

#[cfg(test)]
use std::os::fd::AsRawFd as _;

use usagi_core::infrastructure::daemon::InstanceLock as _;

use usagi_core::infrastructure::paths;
use usagi_core::infrastructure::persistence::json_file;

use super::{tenant_control, workflow};

use super::{
    ACCEPT_ERROR_BACKOFF, AdmissionGate, AgentConcurrencyGauge, AgentDecisionWaker, AgentPty,
    AgentPtyObservation, Arc, AtomicBool, AtomicUsize, BROKER_IO_TIMEOUT, BROKER_OK,
    BackgroundWorker, BootstrapBrokerRecord, BrokerActivity, BrokerIdlePolicy, BuildIdentity,
    CLIENT_RETIREMENT_POLL, CapacityRefusalLog, CensusConnectionFence, ChildIdentity,
    ClientWorkers, ClosePrProjectionOnExit, ConnectionCleanup, ConnectionShutdown,
    ConnectionWorkspace, DEFAULT_GENERATION_LIMIT, DEFAULT_TENANT_LIMIT, DaemonBackgroundWorkers,
    DaemonLauncher, DaemonPty, DaemonReady, DaemonRecord, DaemonRecordPort, DaemonRecordStore,
    DaemonRequest, DaemonWorkspaceActivity, DeadlineConnection, DeadlineUnixStream, DispatchStore,
    DispatchToolContext, Duration, ESTABLISHED_RESPONSE_WRITE_DEADLINE_MS, EndpointCleanup,
    EndpointLocator, ErrorCode, ErrorLog, FencedPrInventory, FileInstanceLock, FileWorkspaceFences,
    FsCustodyProbe, FsRecordFile, GenerationFence, GenerationRegistry, GenerationRegistryFile,
    GenerationRole, GenericTerminalRuntime, IdentityAuthority, InstanceLockCustody, Instant,
    LaunchedStandby, MetricsBroker, MonotonicClock, Mutex, OpCli, Ordering, OutputPrProjector,
    PRE_HANDSHAKE_CONNECTION_LIMIT, PRE_HANDSHAKE_DEADLINE, Path, PathBuf, PeerProcess,
    PrInventoryStore, PrProjectionQueue, PreHandshakeAdmission, ProcessIdentity,
    ProcessObservation, ProcessResourceSampler, PtyObservation, Read, Receiver, RefCell,
    RegistryDocument, ResponseOutcome, RoutingLedger, RuntimeHydration, SeamlessRefusal,
    SecureUnixListener, SessionDispatchContext, SharedAgent, SharedAgentRuntime, SharedAgentState,
    SharedMetricsBroker, SharedPrInventory, SharedProcessResourceSampler, SharedSessionRuntime,
    SharedSupervisorRuntime, SharedTerminal, SharedTerminalOwner, SharedTerminalRuntime,
    SharedVerificationCache, ShutdownOnIpcWorkerExit, ShutdownOnWorkerPanic, ShutdownPipe,
    ShutdownRequest, SpawnedChildren, StaleCleanup, StaleDaemonCleanup, SupervisorRuntime,
    SystemClock, SystemTenantOpener, TeardownSignal, TenantRegistry, TenantWorkspaces,
    TerminalPipelineMetrics, TerminalScopeResolver, TerminalStore, TrustedLoginShell,
    UnixChildProbe, UserDecisionStore, UserEnvironment, WORKFLOW_LANE_TICK, Workspaces, Write,
    authenticated_supervisor_caller, bind_ipc_listener, bootstrap_broker_address,
    client_connection_capacity_available, client_connection_limit, connection_cleanup_channel,
    connection_workspace, current_build, current_daemon_is_reachable, daemon_request_surface,
    dispatch_agent, dispatch_agent_phase_report, dispatch_codex_session_capture, dispatch_dispatch,
    dispatch_dispatch_tool, dispatch_mcp_child_claim, dispatch_metrics, dispatch_pr_snapshot,
    dispatch_rollover, dispatch_session, dispatch_supervisor_control, dispatch_supervisor_snapshot,
    dispatch_supervisor_tool, dispatch_user_decision, draining_collection, ensure_private_dir,
    ensure_private_dir_all, envelope, expected_client_disconnect, handle_bootstrap_broker_request,
    is_same_child, launch_broker_daemon, live_generation_endpoints, new_terminal_runtime,
    observe_generation_process, open_agent_runtime, open_runtime_state, parent_pid, peer_pid,
    process_group, process_start_identity, read_allocator_document, read_shard_documents,
    readable_within, reconcile_orphan_delegations, reconcile_pending_supervisor_promotions,
    reconcile_removed_session_agents, reconcile_startup_supervisor_promotions,
    reconcile_startup_supervisor_workers, request_mcp_credential, retain_client_worker,
    retire_stale_current_preserving, seamless_refusal, spawn_bootstrap_broker,
    spawn_broker_idle_watch, spawn_critical_worker, start_connection_cleanup_worker,
    start_custody_worker, start_daemon_agent_restart_recovery, start_decision_maintenance,
    start_draining_collection_worker, start_orphan_cleanup_worker, start_pr_projection_worker,
    start_pr_refresh_worker, start_retention_gc_worker, start_session_teardown_worker,
    start_supervisor_recovery, start_tenant_retire_worker, start_workflow_lane,
    terminal_capacity_limit, terminal_environment, trusted_repository_root,
    unexpected_daemon_response_entry,
};

/// The store-side view of [`SpawnedChildren`]: it can only ask, never record.
pub(super) struct ObservedChildren(pub(super) Arc<SpawnedChildren>);

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=an_observed_child_stays_provable_until_its_release_is_dropped
impl IdentityAuthority for ObservedChildren {
    fn verified(&self, process: &ProcessIdentity) -> Option<ChildIdentity> {
        self.0
            .0
            .lock()
            .ok()?
            .get(&process.pid)
            .filter(|identity| {
                is_same_child(identity, &process.start_identity, process.process_group)
            })
            .cloned()
    }

    fn observe(
        &self,
        identity: &ChildIdentity,
    ) -> usagi_daemon::usecase::resources::identity::ChildObservation {
        usagi_daemon::usecase::resources::identity::observe_child(&UnixChildProbe, identity)
    }
}

/// Why this build cannot hand authority to a live successor, read from the
/// durable generation registry.
///
/// An unreadable or unparsable registry is reported as such rather than treated
/// as absent, so an operator sees the difference between "no daemon ever
/// registered a generation" and "the registry cannot be trusted".
///
/// The draining predecessor's own shard and the global allocator are read too,
/// so a refusal about a wait can name what that wait is. Both reads are best
/// effort: an unobservable wait leaves the refusal without a cause rather than
/// with a guessed one.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=explicit_artifact_replacement_runs_under_one_coalesced_operation
pub(super) fn observed_seamless_refusal(data_dir: &Path) -> Option<SeamlessRefusal> {
    match usagi_daemon::infrastructure::generation_registry::read_registry_document(data_dir) {
        Ok(document) => {
            let active_is_alive = document
                .as_ref()
                .and_then(RegistryDocument::active)
                .is_some_and(|entry| {
                    observe_generation_process(&entry.process)
                        == ProcessObservation::VerifiedAlive(entry.process.clone())
                });
            let refusal = seamless_refusal(
                document.as_ref(),
                active_is_alive,
                DEFAULT_GENERATION_LIMIT,
                None,
            );
            // Only the refusal that is *about* a wait pays for observing it, so
            // the lifecycle commands that never report one read nothing extra.
            let Some(SeamlessRefusal::DrainingCollectionPending(None)) = refusal else {
                return refusal;
            };
            let Some(registry) = document.as_ref() else {
                return refusal;
            };
            let Ok(shards) = read_shard_documents(data_dir) else {
                return refusal;
            };
            let Ok(allocator) = read_allocator_document(data_dir) else {
                return refusal;
            };
            Some(SeamlessRefusal::DrainingCollectionPending(
                draining_collection(registry, &shards, allocator.as_ref()),
            ))
        }
        Err(error) => Some(SeamlessRefusal::RegistryUnreadable(error.to_string())),
    }
}

// IPC request routing remains in the composition adapter, and each argument is one
// independently resolved startup fact (endpoint, generation, data directory, fenced
// workspace, build, owner record, custody probe, shutdown); bundling them would only
// hide the composition wiring.
// 1 つの決定表を分けると読み手が追う状態が増えるため、この関数はまとめて置く。
#[allow(clippy::too_many_lines)]
// 注入された port をそのまま受け取る composition 境界で、束ねると呼び手が構造体を組むだけになる。
#[allow(clippy::too_many_arguments)]
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
pub(super) fn spawn_ipc_server(
    listener: SecureUnixListener,
    generation: &usagi_core::infrastructure::ipc::DaemonGeneration,
    data_dir: &Path,
    workspace_root: &Path,
    build: &BuildIdentity,
    daemon_process: DaemonRecord,
    custody: Option<FsCustodyProbe>,
    hydration: RuntimeHydration,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<SecureUnixListener>> {
    let owner = daemon_process.clone();
    let daemon_generation = usagi_core::domain::id::DaemonGeneration::parse(&generation.0)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    // The workspaces this daemon holds. The one it was started in is registered
    // with the fence `serve` already took for the process's lifetime; any later
    // one acquires its own before it becomes a tenant.
    let tenants = Arc::new(TenantRegistry::new(
        data_dir.join("daemon"),
        FileWorkspaceFences {
            pid: std::process::id(),
        },
        SystemTenantOpener {
            data_home: data_dir.to_path_buf(),
            generation: daemon_generation,
        },
        DEFAULT_TENANT_LIMIT,
    ));
    let initial = tenants.adopt_initial(workspace_root)?;
    let runtime = initial.runtime().clone();
    // Daemon-wide components (the PTY registry, the Agent runtime and its
    // provisioners) resolve the workspace each request names through this port,
    // rather than capturing the workspace this process started in.
    let resolver = Arc::new(TenantWorkspaces {
        tenants: Arc::clone(&tenants),
        daemon_dir: data_dir.join("daemon"),
        initial: initial.root().to_path_buf(),
    });
    let workspaces: Workspaces = tenants.clone();
    // The inventory is a whole-snapshot document, so exactly one generation may
    // write it. This process is the active one; a draining generation's projector
    // is refused the document rather than merged with it (#562).
    let pr_inventory = Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
        PrInventoryStore::new(data_dir.join("daemon")),
        GenerationRole::Active,
    ))));
    // This generation's authority over the connections it serves. It is created
    // in the `active` role, which is the role `serve` binds and the registry
    // claim confirms: the gate opens both lease classes, so nothing this build
    // dispatched before is refused, and the leases it now issues are what a
    // handoff barrier gets to wait on (#559).
    let fence = Arc::new(GenerationFence {
        gate: AdmissionGate::new(daemon_generation, GenerationRole::Active),
        ledger: Arc::new(RoutingLedger::new()),
    });
    // Every client worker this generation must unblock and join before it may be
    // collected. Nothing collects it in this build, so it is retained and reaped
    // rather than retired.
    let workers = Arc::new(ClientWorkers::new());
    // The children this process observes while spawning them. It is the only proof
    // that a durable record describes a child this generation owns (#562).
    let children = Arc::new(SpawnedChildren::default());
    // Terminal PTY capacity is global because one daemon owns every workspace
    // tenant and every retained generation shares the allocator. A malformed
    // settings file keeps the daemon available under the bounded default and is
    // already surfaced by `usagi doctor`.
    let terminal_limit = terminal_capacity_limit(data_dir);
    // Deferred PR detection. The observers submit committed bytes here after
    // releasing the runtime lock, so no scan and no durable write happens inside
    // it (#555).
    let projection = Arc::new(PrProjectionQueue::new());
    let mut background_workers =
        DaemonBackgroundWorkers::new(Arc::clone(&shutdown), Arc::clone(&projection));
    let pipeline_metrics = Arc::new(TerminalPipelineMetrics::default());
    // One daemon-wide aggregate retention budget for exited terminal and Agent
    // finals (#526). Both owners reserve from it before spawning and commit
    // their finals into it, so short-lived runtimes cannot grow the daemon's
    // tombstones without bound.
    let retention = usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention::new();
    let (pty, observations) = DaemonPty::new(
        Arc::clone(&pipeline_metrics),
        Arc::clone(&children),
        Arc::clone(&shutdown),
    );
    background_workers.bind_terminal_observations(pty.observations.clone());
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
        Arc::clone(&workspaces),
        Arc::clone(&user_environment),
        retention.clone(),
        &children,
        hydration == RuntimeHydration::All,
        terminal_limit,
    )?;
    background_workers.push(start_terminal_observer(
        Arc::downgrade(&terminal),
        observations,
        Arc::clone(&projection),
        Arc::clone(&shutdown),
    )?);
    let (agent_pty, agent_observations) = AgentPty::new(
        terminal_environment(),
        Arc::clone(&pipeline_metrics),
        Arc::clone(&children),
        Arc::clone(&shutdown),
    );
    background_workers.bind_agent_observations(agent_pty.observations.clone());
    let mcp_command = std::env::current_exe()?;
    // The Agent runtime publishes the concurrency it admits from here, and the
    // metrics broker below reads it without taking the runtime's lock: a
    // display-only observation must never wait behind a launch (#644).
    let agent_concurrency = AgentConcurrencyGauge::default();
    let agent = open_agent_runtime(
        data_dir,
        daemon_generation,
        Arc::clone(&workspaces),
        agent_pty,
        mcp_command,
        user_environment,
        retention.clone(),
        agent_concurrency.clone(),
        &children,
        hydration,
        terminal_limit,
    )?;
    background_workers.push(start_daemon_agent_restart_recovery(
        data_dir.to_path_buf(),
        daemon_generation,
        fence.gate.clone(),
        Arc::clone(&tenants),
        Arc::clone(&workspaces),
        Arc::clone(&agent),
        Arc::clone(&shutdown),
    )?);
    reconcile_removed_session_agents(&data_dir.join("daemon"), &agent)?;
    let supervisor = Arc::new(Mutex::new(SupervisorRuntime::new(&data_dir.join("daemon"))));
    if let Err(error) = reconcile_startup_supervisor_promotions(&supervisor, &agent) {
        ErrorLog::record(&format!(
            "supervisor promotion reconciliation deferred: {error}"
        ));
    }
    if let Err(error) = reconcile_startup_supervisor_workers(&supervisor, &agent) {
        ErrorLog::record(&format!(
            "supervisor worker termination reconciliation deferred: {error}"
        ));
    }
    if let Ok(runtime) = supervisor.lock()
        && let Err(error) = runtime.tick_all(
            chrono::Utc::now(),
            &mut AgentDecisionWaker { agent: &agent },
        )
    {
        ErrorLog::record(&format!(
            "supervisor startup reconciliation deferred: {error}"
        ));
    }
    background_workers.push(start_supervisor_recovery(
        Arc::clone(&supervisor),
        Arc::clone(&agent),
        Arc::clone(&workspaces),
        Arc::clone(&shutdown),
    )?);
    background_workers.push(start_agent_observer(
        Arc::downgrade(&agent),
        agent_observations,
        Arc::clone(&projection),
        Arc::clone(&supervisor),
        Arc::clone(&shutdown),
    )?);
    // Socket workers only remove themselves from the bounded live census and
    // coalesce a wake. The single consumer sweeps stale ledger state without a
    // disconnect storm retaining historical queue entries or socket triplets.
    let (disconnected, disconnects) = connection_cleanup_channel();
    let connection_cleanup = start_connection_cleanup_worker(
        Arc::clone(&agent),
        Arc::clone(&terminal),
        disconnects,
        Arc::clone(&shutdown),
    )?;
    background_workers.push(start_pr_projection_worker(
        Arc::clone(&pr_inventory),
        Arc::clone(&projection),
        Arc::clone(&shutdown),
    )?);
    let verification: SharedVerificationCache = Arc::default();
    // `SystemClock` measures from its own construction, so the cache's TTL only
    // means anything while one clock outlives every read of it.
    let verification_clock = Arc::new(SystemClock::new());
    background_workers.push(start_workflow_lane(
        Arc::clone(&agent),
        Arc::clone(&pr_inventory),
        Arc::clone(&verification),
        Arc::clone(&verification_clock),
        Arc::clone(&workspaces),
        Arc::clone(&shutdown),
        WORKFLOW_LANE_TICK,
    )?);
    let decisions = Arc::new(UserDecisionStore::new(data_dir.join("daemon")));
    background_workers.push(start_decision_maintenance(
        Arc::clone(&decisions),
        Arc::clone(&shutdown),
    )?);
    background_workers.push(start_pr_refresh_worker(
        Arc::clone(&pr_inventory),
        data_dir.join("daemon"),
        Arc::clone(&shutdown),
    )?);
    let (teardown, teardown_worker) = start_session_teardown_worker(
        Arc::clone(&workspaces),
        Arc::clone(&agent),
        Arc::clone(&terminal),
        Arc::clone(&shutdown),
    )?;
    background_workers.push(teardown_worker);
    background_workers.push(start_orphan_cleanup_worker(
        &workspaces,
        fence.gate.clone(),
        Arc::clone(&shutdown),
    )?);
    // Workspaces adopted for a client that has gone away are given back, so a
    // daemon that served many of them over a day does not still own them all.
    background_workers.push(start_tenant_retire_worker(
        Arc::clone(&tenants),
        DaemonWorkspaceActivity {
            terminal: Arc::clone(&terminal),
            agent: Arc::clone(&agent),
            supervisor: Arc::clone(&supervisor),
        },
        Arc::clone(&shutdown),
    )?);
    // Before any client can observe them: roll back the sessions a delegation
    // created and then died before dispatching into.
    let compensated = reconcile_orphan_delegations(
        &ConnectionWorkspace {
            tenant: initial.clone(),
            workspaces: Arc::clone(&workspaces),
        },
        &DispatchStore::new(data_dir.join("daemon")),
        &teardown,
    );
    if compensated != 0 {
        ErrorLog::record(&format!(
            "daemon startup compensated {compensated} delegated session(s) whose dispatch never started"
        ));
    }
    background_workers.push(start_retention_gc_worker(
        Arc::clone(&terminal),
        Arc::clone(&agent),
        open_runtime_state(data_dir, daemon_generation, &children, terminal_limit)?,
        Arc::clone(&shutdown),
    )?);
    background_workers.push(start_draining_collection_worker(
        open_runtime_state(data_dir, daemon_generation, &children, terminal_limit)?,
        GenerationRegistry::new(
            GenerationRegistryFile::new(data_dir)?,
            DEFAULT_GENERATION_LIMIT,
        ),
        fence.gate.clone(),
        daemon_generation,
        Arc::clone(&workers),
        Arc::clone(&shutdown),
    )?);
    if let Some(custody) = custody {
        background_workers.push(start_custody_worker(
            custody,
            owner,
            data_dir.to_path_buf(),
            fence.gate.clone(),
            Arc::clone(&shutdown),
        )?);
    }
    start_ipc_accept_loop(
        listener,
        server,
        IpcAcceptContext {
            data_dir: data_dir.to_path_buf(),
            initial,
            tenants,
            workspaces,
            resolver,
            teardown,
            terminal,
            agent,
            retention,
            pr_inventory,
            verification,
            verification_clock,
            projection,
            decisions,
            metrics: Arc::new(Mutex::new(MetricsBroker::with_runtime_health(
                agent_concurrency,
                shutdown.background_worker_health(),
            ))),
            process_metrics: Arc::new(Mutex::new(ProcessResourceSampler { previous: None })),
            pipeline_metrics,
            supervisor,
            fence,
            workers,
            disconnected,
            connection_cleanup,
            background_workers,
            shutdown,
        },
    )
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=root_ipc_fixture_codex_survives_disconnect_and_replays_final
pub(super) fn start_agent_observer(
    agent: std::sync::Weak<SharedAgentState>,
    observations: Receiver<AgentPtyObservation>,
    projection: Arc<PrProjectionQueue>,
    supervisor: SharedSupervisorRuntime,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let failed_projection = Arc::clone(&projection);
    spawn_critical_worker(
        "usagi-agent-observer",
        BackgroundWorker::AgentObserver,
        shutdown,
        move || failed_projection.close(),
        move |_| {
            while let Ok(observation) = observations.recv() {
                match observation {
                    AgentPtyObservation::Output(reference, bytes) => {
                        // The runtime lock covers journaling this chunk and
                        // nothing else. PR detection is submitted afterwards, so
                        // the lock is never held for a scan or for durable IO.
                        let committed = {
                            let Some(agent) = agent.upgrade() else {
                                break;
                            };
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
                    AgentPtyObservation::Exited(reference, status, release) => {
                        {
                            let Some(agent) = agent.upgrade() else {
                                break;
                            };
                            let Ok(mut agent) = agent.lock() else {
                                break;
                            };
                            let _ = agent.exit(&reference, status);
                        }
                        // The commit above is the last reader of this child's
                        // identity, so the proof is released here rather than
                        // where the exit was seen: a record still projecting as
                        // `Running` must not lose its authority mid-commit.
                        drop(release);
                        // A candidate the output never terminated is only
                        // creditable once nothing more can arrive for it.
                        projection.submit_closed(reference.terminal_id, reference.session_id);
                        if let Some(agent) = agent.upgrade()
                            && let Err(error) =
                                reconcile_pending_supervisor_promotions(&supervisor, &agent)
                        {
                            ErrorLog::record(&format!(
                                "supervisor promotion reconciliation deferred: {error}"
                            ));
                        }
                        if let (Some(agent), Ok(runtime)) = (agent.upgrade(), supervisor.lock())
                            && let Err(error) = runtime.tick_all(
                                chrono::Utc::now(),
                                &mut AgentDecisionWaker { agent: &agent },
                            )
                        {
                            ErrorLog::record(&format!(
                                "supervisor completion reconciliation deferred: {error}"
                            ));
                        }
                    }
                    AgentPtyObservation::Shutdown => break,
                }
            }
        },
    )
}

#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=production_backend_factory_preserves_terminal_arguments_and_completes_store_routes
pub(super) fn start_terminal_observer<S, Q>(
    terminal: std::sync::Weak<Mutex<GenericTerminalRuntime<TrustedLoginShell, S, DaemonPty, Q>>>,
    observations: Receiver<PtyObservation>,
    projection: Arc<PrProjectionQueue>,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    S: TerminalStore + Send + 'static,
    Q: TerminalScopeResolver + Send + 'static,
{
    let failed_projection = Arc::clone(&projection);
    spawn_critical_worker(
        "usagi-terminal-observer",
        BackgroundWorker::TerminalObserver,
        shutdown,
        move || failed_projection.close(),
        move |_| {
            while let Ok(observation) = observations.recv() {
                match observation {
                    PtyObservation::Output(reference, bytes) => {
                        // As in the Agent observer: the lock covers journaling
                        // only, and PR detection happens after it is released.
                        let committed = {
                            let Some(terminal) = terminal.upgrade() else {
                                break;
                            };
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
                    PtyObservation::Exited(reference, status, release) => {
                        {
                            let Some(terminal) = terminal.upgrade() else {
                                break;
                            };
                            let Ok(mut terminal) = terminal.lock() else {
                                break;
                            };
                            let _ = terminal.exit(&reference, status);
                        }
                        // Released after the commit, exactly as the Agent
                        // observer does.
                        drop(release);
                        projection.submit_closed(reference.terminal_id, reference.session_id);
                    }
                    PtyObservation::Shutdown => break,
                }
            }
        },
    )
}

/// Services owned by the IPC accept lifetime. Keeping the ownership graph in
/// one value prevents the composition entry point from becoming a positional
/// argument list as new daemon capabilities are introduced.
pub(super) struct IpcAcceptContext {
    pub(super) data_dir: PathBuf,
    pub(super) initial: usagi_daemon::usecase::tenant::Tenant<SharedSessionRuntime>,
    pub(super) tenants: Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
    pub(super) workspaces: Workspaces,
    pub(super) resolver: Arc<TenantWorkspaces>,
    pub(super) teardown: Arc<TeardownSignal>,
    pub(super) terminal: SharedTerminalRuntime,
    pub(super) agent: SharedAgentRuntime,
    pub(super) retention: usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention,
    pub(super) pr_inventory: SharedPrInventory,
    /// Shared with the resident workflow lane so both sides reuse one GitHub read.
    pub(super) verification: SharedVerificationCache,
    pub(super) verification_clock: Arc<SystemClock>,
    pub(super) projection: Arc<PrProjectionQueue>,
    pub(super) decisions: Arc<UserDecisionStore>,
    pub(super) metrics: SharedMetricsBroker,
    pub(super) process_metrics: SharedProcessResourceSampler,
    pub(super) pipeline_metrics: Arc<TerminalPipelineMetrics>,
    pub(super) supervisor: SharedSupervisorRuntime,
    pub(super) fence: Arc<GenerationFence>,
    pub(super) workers: Arc<ClientWorkers>,
    pub(super) disconnected: ConnectionCleanup,
    pub(super) connection_cleanup: std::thread::JoinHandle<()>,
    pub(super) background_workers: DaemonBackgroundWorkers,
    pub(super) shutdown: Arc<ShutdownRequest>,
}

#[allow(clippy::too_many_lines)] // Composition owns the independently injected daemon services.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
pub(super) fn start_ipc_accept_loop(
    listener: SecureUnixListener,
    server: usagi_core::infrastructure::ipc::ServerProtocol,
    context: IpcAcceptContext,
) -> std::io::Result<std::thread::JoinHandle<SecureUnixListener>> {
    let IpcAcceptContext {
        data_dir,
        initial,
        tenants,
        workspaces,
        resolver,
        teardown,
        terminal,
        agent,
        retention,
        pr_inventory,
        verification,
        verification_clock,
        projection,
        decisions,
        metrics,
        process_metrics,
        pipeline_metrics,
        supervisor,
        fence,
        workers,
        disconnected,
        connection_cleanup,
        mut background_workers,
        shutdown,
    } = context;
    let connection_limit = client_connection_limit();
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
            let pre_handshake =
                PreHandshakeAdmission::new(PRE_HANDSHAKE_CONNECTION_LIMIT);
            let mut capacity_log = CapacityRefusalLog::default();
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
                        let capacity_available =
                            client_connection_capacity_available(&workers, connection_limit);
                        if capacity_log.should_record(capacity_available) {
                            ErrorLog::record(
                                "daemon connection refused: client capacity exhausted",
                            );
                        }
                        if !capacity_available {
                            drop(stream);
                            continue;
                        }
                        let Ok(peer_pid) = peer_pid(&stream) else {
                            ErrorLog::record(
                                "daemon connection refused: peer process identity unavailable",
                            );
                            continue;
                        };
                        // Parent and process-group identity is authority only
                        // for credential bootstrap and bearer-less hooks. An
                        // ordinary same-UID client (including an orphaned
                        // bootstrap broker reparented to PID 1) needs only its
                        // kernel-authenticated peer PID to complete hello.
                        let peer_process = PeerProcess {
                            pid: peer_pid,
                            process_start_identity: process_start_identity(peer_pid).ok(),
                            lineage: parent_pid(peer_pid).and_then(|parent| {
                                process_group(peer_pid)
                                    .map(|process_group| (parent, process_group))
                            }).ok(),
                        };
                        let Some(pre_handshake_permit) = pre_handshake.try_admit() else {
                            // No hello has been read, so sending a framed protocol
                            // error here would invent a new wire state. Closing the
                            // sole accepted descriptor is the compatible, minimum-
                            // resource refusal. The message contains no peer or
                            // workspace material.
                            ErrorLog::record(
                                "daemon pre-handshake connection refused: capacity exhausted",
                            );
                            drop(stream);
                            continue;
                        };
                        let server = server.clone();
                        // The workspace this connection acts on is decided by its
                        // handshake, below: every session command it issues
                        // belongs to that workspace, while requests that name a
                        // workspace resolve through the registry.
                        let connection_initial = initial.clone();
                        let connection_tenants = Arc::clone(&tenants);
                        let connection_workspaces = Arc::clone(&workspaces);
                        let connection_resolver = Arc::clone(&resolver);
                        let teardown = Arc::clone(&teardown);
                        let terminal = Arc::clone(&terminal);
                        let tenant_terminal = Arc::clone(&terminal);
                        let visibility = visibility.clone();
                        let retention = retention.clone();
                        let agent_owner = Arc::clone(&agent);
                        let agent_launch = Arc::clone(&agent);
                        let pr_inventory = Arc::clone(&pr_inventory);
                        let verification = Arc::clone(&verification);
                        let verification_clock = Arc::clone(&verification_clock);
                        let decisions = Arc::clone(&decisions);
                        let metrics = Arc::clone(&metrics);
                        let process_metrics = Arc::clone(&process_metrics);
                        let pipeline_metrics = Arc::clone(&pipeline_metrics);
                        let supervisor = Arc::clone(&supervisor);
                        let connection_fence = Arc::clone(&fence);
                        let connection_data_dir = data_dir.clone();
                        let connection_cleanup = disconnected.clone();
                        let connection_shutdown = Arc::clone(&shutdown);
                        // A worker without a shutdown half cannot participate in
                        // the generation retirement barrier, so descriptor
                        // duplication failure refuses the connection before a
                        // thread or request state is created.
                        let unblock = match stream.try_clone() {
                            Ok(stream) => AcceptedStream::new(stream),
                            Err(error) => {
                                ErrorLog::record(&format!(
                                    "daemon connection refused: accepted stream could not be duplicated: {error}"
                                ));
                                continue;
                            }
                        };
                        let worker_completion = Some(unblock.clone());
                        let retirement = unblock.retirement();
                        let spawned = std::thread::Builder::new()
                            .name("usagi-ipc-client".to_string())
                            .spawn(move || {
                                let _panic = ShutdownOnWorkerPanic {
                                    shutdown: connection_shutdown,
                                };
                                // The retained shutdown descriptor must not keep
                                // the peer apparently open after this worker has
                                // returned. Completion shuts the shared socket on
                                // every early-return and established-connection
                                // exit; ClientWorkers still owns the handle needed
                                // to join the finished thread exactly once.
                                let completion_guard =
                                    ShutdownAcceptedStreamOnDrop(worker_completion);
                                if let Err(error) = stream.set_nonblocking(false) {
                                    ErrorLog::record(&format!(
                                        "daemon client worker failed: blocking mode could not be restored: {error}"
                                    ));
                                    return;
                                }
                                let writer = match stream.try_clone() {
                                    Ok(writer) => writer,
                                    Err(error) => {
                                        ErrorLog::record(&format!(
                                            "daemon client worker failed: response stream could not be duplicated: {error}"
                                        ));
                                        return;
                                    }
                                };
                                let deadline = Instant::now() + PRE_HANDSHAKE_DEADLINE;
                                let mut reader = PreHandshakeDeadlineStream::new(stream, deadline);
                                let mut writer =
                                    PreHandshakeDeadlineStream::new(writer, deadline);
                                let census_fence = CensusConnectionFence {
                                    inner: connection_fence.as_ref(),
                                    cleanup: connection_cleanup,
                                    peer_pid: peer_process.pid,
                                };
                                let admitted =
                                    usagi_daemon::presentation::ipc::handshake_admitted_with_fence(
                                        &mut reader,
                                        &mut writer,
                                        &server,
                                        Some(connection_resolver.as_ref()),
                                        &census_fence,
                                    );
                                // Capacity covers the complete hello response, on
                                // every success/refusal/error path, but never the
                                // established connection that follows it.
                                drop(pre_handshake_permit);
                                let admitted = match admitted {
                                    Ok(Some(admitted)) => admitted,
                                    Ok(None) => {
                                        ErrorLog::record(
                                            "daemon pre-handshake connection refused by protocol policy",
                                        );
                                        return;
                                    }
                                    Err(error) => {
                                        let reason = if matches!(
                                            error.kind(),
                                            std::io::ErrorKind::TimedOut
                                                | std::io::ErrorKind::WouldBlock
                                        ) {
                                            "deadline exceeded"
                                        } else {
                                            "invalid or incomplete hello"
                                        };
                                        ErrorLog::record(&format!(
                                            "daemon pre-handshake connection refused: {reason}: {error}"
                                        ));
                                        return;
                                    }
                                };
                                // The handshake resolved which workspace this
                                // connection acts on; a workspace retired between
                                // the two steps closes the connection rather than
                                // serving another workspace's state.
                                let Some(bound) = connection_workspace(
                                    &connection_workspaces,
                                    &connection_initial,
                                    admitted.client.workspace.as_ref(),
                                ) else {
                                    if let Some(connection) = admitted.registered_connection() {
                                        usagi_daemon::presentation::ipc::ConnectionFence::disconnected(
                                            &census_fence,
                                            connection,
                                        );
                                    }
                                    ErrorLog::record(
                                        "daemon admitted connection closed: its workspace is no longer held",
                                    );
                                    return;
                                };
                                // A pre-handshake timeout must not become an idle
                                // policy for an admitted subscription. Failure to
                                // remove it fails this socket closed.
                                if let Err(error) = reader.clear_deadlines() {
                                    if let Some(connection) = admitted.registered_connection() {
                                        usagi_daemon::presentation::ipc::ConnectionFence::disconnected(
                                            &census_fence,
                                            connection,
                                        );
                                    }
                                    ErrorLog::record(&format!(
                                        "daemon admitted connection closed: reader pre-handshake deadline could not be cleared: {error}"
                                    ));
                                    return;
                                }
                                if let Err(error) = writer.clear_deadlines() {
                                    if let Some(connection) = admitted.registered_connection() {
                                        usagi_daemon::presentation::ipc::ConnectionFence::disconnected(
                                            &census_fence,
                                            connection,
                                        );
                                    }
                                    ErrorLog::record(&format!(
                                        "daemon admitted connection closed: writer pre-handshake deadline could not be cleared: {error}"
                                    ));
                                    return;
                                }
                                // Established reads have no idle deadline, but
                                // each response write has one fixed frame budget.
                                // Read readiness is gated so the worker observes
                                // retirement; `shutdown(2)` alone can leave this
                                // thread parked forever, and the barrier would
                                // then never join it.
                                let mut reader = RetiringReader::new(
                                    reader.into_inner(),
                                    retirement,
                                    CLIENT_RETIREMENT_POLL,
                                );
                                let mut writer = EstablishedResponseWriter::new(
                                    SystemClock::new(),
                                    DeadlineUnixStream(writer.into_inner()),
                                    ESTABLISHED_RESPONSE_WRITE_DEADLINE_MS,
                                );
                                let mut owner =
                                    SharedTerminalOwner::with_visibility_and_retention(
                                        SharedAgent { runtime: agent_owner },
                                        SharedTerminal(Arc::clone(&terminal)),
                                        visibility,
                                        retention,
                                    );
                                let mut metrics_observer = None;
                                let result = usagi_daemon::presentation::ipc::handle_admitted_connection_with_terminal_and_observe(
                                    &mut reader,
                                    &mut writer,
                                    admitted,
                                    &census_fence,
                                    &mut owner,
                                    &mut |request_id, body, hello, connection, client| {
                                        let Ok(request) = serde_json::from_value::<DaemonRequest>(body.clone()) else {
                                            return usagi_daemon::presentation::ipc::reject_unhandled_request(
                                                request_id,
                                                body,
                                                hello,
                                            );
                                        };
                                        if let Some(credential) = request_mcp_credential(&body)
                                            && !agent_launch
                                                .lock()
                                                .is_ok_and(|mut runtime| {
                                                    peer_process
                                                        .process_start_identity
                                                        .as_deref()
                                                        .is_some_and(|identity| {
                                                            runtime.authenticate_mcp_child_connection(
                                                                credential,
                                                                peer_process.pid,
                                                                identity,
                                                                connection,
                                                            )
                                                        })
                                                })
                                        {
                                            return envelope(
                                                hello,
                                                request_id,
                                                ResponseOutcome::Error(
                                                    usagi_core::infrastructure::ipc::ProtocolError::new(
                                                        ErrorCode::OwnershipUnknown,
                                                        "MCP caller is not the claimed child process",
                                                    ),
                                                ),
                                                serde_json::Value::Null,
                                            );
                                        }
                                        match request {
                                            DaemonRequest::McpChildClaim => dispatch_mcp_child_claim(&agent_launch, &bound, &connection_data_dir, &peer_process, connection, request_id, &body, hello),
                                            DaemonRequest::Rollover { .. } => dispatch_rollover(&connection_data_dir, connection_fence.as_ref(), &agent_launch, &bound, request_id, &body, hello),
                                            DaemonRequest::Tenant { .. } => tenant_control::dispatch(&connection_tenants, &tenant_terminal, &agent_launch, request_id, &body, hello),
                                            DaemonRequest::Session { .. } => dispatch_session(&SessionDispatchContext { bound: &bound, teardown: &teardown, agent: &agent_launch, pr_inventory: &pr_inventory, verification: &verification, verification_clock: &verification_clock, supervisor: &supervisor }, request_id, &body, hello),
                                            DaemonRequest::Agent { .. }
                                            | DaemonRequest::AgentGoal { .. }
                                            | DaemonRequest::AgentInventory { .. }
                                            | DaemonRequest::AgentWorkspaceObservation { .. }
                                            | DaemonRequest::DiagnoseAgents { .. }
                                            | DaemonRequest::PlanDaemonRestartAgents { .. }
                                            | DaemonRequest::RestartAgents { .. }
                                            | DaemonRequest::ResumeAgent { .. }
                                            | DaemonRequest::ResumeAgentWithCurrentIntegration { .. } => dispatch_agent(&agent_launch, &supervisor, &bound, request_id, &body, hello),
                                            DaemonRequest::CodexSessionCapture { .. } => dispatch_codex_session_capture(&agent_launch, &peer_process, request_id, &body, hello),
                                            DaemonRequest::AgentPhaseReport { .. } => dispatch_agent_phase_report(&agent_launch, &peer_process, request_id, &body, hello),
                                            DaemonRequest::Dispatch { .. } => dispatch_dispatch(&agent_launch, &bound, request_id, &body, hello),
                                            DaemonRequest::Metrics { .. } => dispatch_metrics(&metrics, &process_metrics, &pipeline_metrics, &mut metrics_observer, request_id, &body, hello),
                                            DaemonRequest::Pr { .. }
                                            | DaemonRequest::PrBatch { .. }
                                            | DaemonRequest::PrDismiss { .. } => dispatch_pr_snapshot(&pr_inventory, request_id, &body, hello),
                                            DaemonRequest::DispatchTool { .. } => dispatch_dispatch_tool(&DispatchToolContext { agent: &agent_launch, terminal: &terminal, bound: &bound, pr_inventory: &pr_inventory, decisions: &decisions, supervisor: &supervisor }, request_id, &body, hello),
                                            DaemonRequest::SupervisorTool { .. } => {
                                                let caller = authenticated_supervisor_caller(&agent_launch, &bound, &client, &body);
                                                dispatch_supervisor_tool(&supervisor, caller, request_id, &body, hello)
                                            },
                                            DaemonRequest::SupervisorSnapshot { .. } => dispatch_supervisor_snapshot(&supervisor, &bound, request_id, &body, hello),
                                            DaemonRequest::SupervisorControl { .. } => dispatch_supervisor_control(&supervisor, &agent_launch, &bound, request_id, &body, hello),
                                            DaemonRequest::WorkflowSnapshot { .. } | DaemonRequest::WorkflowControl { .. } => workflow::dispatch(&workflow::WorkflowDispatchContext { agent: &agent_launch, inventory: &pr_inventory, verification: workflow::Verification { cache: &verification, clock: verification_clock.as_ref() }, bound: &bound }, request_id, request, &body, hello),
                                            DaemonRequest::UserDecision { .. } => dispatch_user_decision(&agent_launch, &bound, &decisions, request_id, &body, hello),
                                            DaemonRequest::Terminal { .. } => usagi_daemon::presentation::ipc::reject_unhandled_request(request_id, body, hello),
                                        }
                                    },
                                    &mut |body, response| {
                                        let surface = daemon_request_surface(body);
                                        if let Some(entry) =
                                            unexpected_daemon_response_entry(surface, response)
                                        {
                                            ErrorLog::record(&entry);
                                        }
                                    },
                                );
                                // No disconnect-side bookkeeping may extend the
                                // accepted connection's lifetime. Close the reader,
                                // writer, and retained retirement descriptor before
                                // touching any daemon-wide runtime.
                                drop(owner);
                                drop(reader);
                                drop(writer);
                                drop(completion_guard);
                                if let Some(observer) = metrics_observer
                                    && let Ok(mut broker) = metrics.try_lock()
                                {
                                    broker.unsubscribe(observer.subscription());
                                }
                                if let Err(error) = result
                                    && !expected_client_disconnect(error.kind())
                                {
                                    ErrorLog::record(&format!(
                                        "daemon admitted connection failed: {error}"
                                    ));
                                }
                            });
                        match spawned {
                            Ok(handle) => retain_client_worker(&workers, Ok(unblock), handle),
                            Err(error) => ErrorLog::record(&format!(
                                "daemon client worker unavailable: {error}"
                            )),
                        }
                    }
                    // Drained: nothing more is queued, so wait for readiness.
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    // A peer that failed the credential check was still accepted
                    // and dropped, so draining continues. An error that leaves the
                    // connection queued (descriptor exhaustion) would otherwise
                    // spin, so that path — and only that path, never the idle one —
                    // backs off before trying again.
                    Err(error) => {
                        ErrorLog::record(&format!("daemon accept failed: {error}"));
                        std::thread::sleep(ACCEPT_ERROR_BACKOFF);
                    }
                }
                }
            }
            // Active shutdown and rollover collection share the same barrier.
            // `retire` seals registration, shuts every socket to unblock frame
            // reads, and joins every worker; a concurrent collection may have
            // performed it already, in which case this is an idempotent no-op.
            let report = workers.retire();
            if !report.is_clean() {
                ErrorLog::record(&format!(
                    "daemon shutdown retired with client worker failures: {report:?}"
                ));
            }
            // Every connection worker has now returned and removed itself from
            // the census. Closing the last producer lets the final coalesced
            // sweep finish before owner runtimes leave this daemon generation.
            drop(disconnected);
            if connection_cleanup.join().is_err() {
                ErrorLog::record("daemon connection cleanup worker panicked");
            }
            // Stop every daemon-owned pipeline from the lifecycle owner, then
            // join every retained handle. Observer receive timeouts make their
            // source channels close promptly, unblocking a PTY reader that was
            // backpressured in a bounded send. Projection is closed only after
            // serving has stopped, and drains its already accepted work.
            background_workers.shutdown_and_join();
            listener
        })
}

/// Root-bound IPC publication seam. `serve` invokes it only after the daemon
/// owns the singleton lock and has persisted its exact process-owner record. The guard makes a
/// future duplicate invocation a no-op instead of binding a second endpoint.
pub(super) struct IpcReady<'a> {
    pub(super) data_dir: &'a Path,
    /// The canonical workspace root resolved once at startup and fenced before
    /// publication, so the runtime this publishes owns exactly the workspace the
    /// fence guards.
    pub(super) workspace_root: &'a Path,
    /// The single-instance lock this daemon holds. Publication reads the locked
    /// inode from it so the custody supervisor can prove, on every tick, that
    /// this process is still the singleton for `data_dir`.
    pub(super) instance_lock: &'a dyn InstanceLockCustody,
    pub(super) build: BuildIdentity,
    pub(super) shutdown: Arc<ShutdownRequest>,
    pub(super) published: AtomicBool,
    pub(super) publication_attempted: AtomicBool,
    pub(super) worker: RefCell<Option<std::thread::JoinHandle<SecureUnixListener>>>,
    pub(super) listener: RefCell<Option<SecureUnixListener>>,
    pub(super) cleanup: RefCell<Option<EndpointCleanup>>,
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=daemon_lifecycle_recovers_a_crash_record_whose_pid_was_reused
impl<'a> IpcReady<'a> {
    /// Bind the production endpoint seam for one `serve` process.
    pub(super) fn new(
        data_dir: &'a Path,
        workspace_root: &'a Path,
        instance_lock: &'a dyn InstanceLockCustody,
    ) -> Self {
        Self {
            data_dir,
            workspace_root,
            instance_lock,
            // The daemon advertises the exact artifact it started as for its
            // whole process lifetime. Atomic replacement of the executable path
            // cannot mutate this startup snapshot.
            build: current_build(),
            shutdown: Arc::new(ShutdownRequest::new()),
            published: AtomicBool::new(false),
            publication_attempted: AtomicBool::new(false),
            worker: RefCell::new(None),
            listener: RefCell::new(None),
            cleanup: RefCell::new(None),
        }
    }

    pub(super) fn publish_with(
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
    pub(super) fn bound_endpoint(&self) -> Option<EndpointLocator> {
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
    pub(super) fn publish_current(&self) -> std::io::Result<()> {
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
    pub(super) fn retire_endpoint(&self) -> std::io::Result<()> {
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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=daemon_lifecycle_recovers_a_crash_record_whose_pid_was_reused
impl DaemonReady for IpcReady<'_> {
    fn recover_stale_endpoint(&self) -> std::io::Result<()> {
        // The instance lock excludes another *active* daemon, not every daemon:
        // a standby runs in this data directory without holding it, so its live
        // socket must be told apart from residue by the durable registry rather
        // than by the lock.
        let live = live_generation_endpoints(self.data_dir);
        retire_stale_current_preserving(self.data_dir, &|generation| live.contains(generation))
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
                Some(custody),
                RuntimeHydration::All,
                Arc::clone(&self.shutdown),
            )
        })?;
        spawn_bootstrap_broker(
            &std::env::current_exe()?,
            self.data_dir,
            self.workspace_root,
        )
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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=daemon_lifecycle_recovers_a_crash_record_whose_pid_was_reused
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

/// A Unix stream armed against one fixed handshake completion instant.
///
/// Every individual `read` and `write` re-arms the OS timeout with only the
/// remaining budget. Partial prefix/body progress therefore cannot extend the
/// deadline, while the kernel still performs the blocking wait efficiently.
pub(super) struct PreHandshakeDeadlineStream {
    pub(super) stream: std::os::unix::net::UnixStream,
    pub(super) deadline: Instant,
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=root_ipc_pre_handshake_cap_deadline_fairness_and_shutdown_are_bounded
impl PreHandshakeDeadlineStream {
    pub(super) fn new(stream: std::os::unix::net::UnixStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }

    pub(super) fn remaining(&self) -> std::io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "daemon pre-handshake deadline exceeded",
                )
            })
    }

    pub(super) fn clear_deadlines(&self) -> std::io::Result<()> {
        self.stream.set_read_timeout(None)?;
        self.stream.set_write_timeout(None)
    }

    pub(super) fn into_inner(self) -> std::os::unix::net::UnixStream {
        self.stream
    }

    pub(super) fn deadline_error(error: std::io::Error) -> std::io::Error {
        if matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ) {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "daemon pre-handshake deadline exceeded",
            )
        } else {
            error
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=root_ipc_pre_handshake_cap_deadline_fairness_and_shutdown_are_bounded
impl Read for PreHandshakeDeadlineStream {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(bytes).map_err(Self::deadline_error)
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=root_ipc_pre_handshake_cap_deadline_fairness_and_shutdown_are_bounded
impl Write for PreHandshakeDeadlineStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(bytes).map_err(Self::deadline_error)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.flush().map_err(Self::deadline_error)
    }
}

/// Progress through the u32-length-prefixed response currently being written.
///
/// The presentation layer writes the prefix and payload separately, while
/// `Write::write` may accept either only partially. Tracking accepted bytes here
/// makes one fixed deadline span every syscall of that complete frame.
#[derive(Default)]
pub(super) struct ResponseFrameProgress {
    pub(super) prefix: [u8; 4],
    pub(super) prefix_len: usize,
    pub(super) payload_remaining: usize,
}

impl ResponseFrameProgress {
    pub(super) fn at_frame_start(&self) -> bool {
        self.prefix_len == 0
    }

    pub(super) fn observe(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.prefix_len < self.prefix.len() {
                let copied = (self.prefix.len() - self.prefix_len).min(bytes.len());
                self.prefix[self.prefix_len..self.prefix_len + copied]
                    .copy_from_slice(&bytes[..copied]);
                self.prefix_len += copied;
                bytes = &bytes[copied..];
                if self.prefix_len == self.prefix.len() {
                    self.payload_remaining = u32::from_be_bytes(self.prefix) as usize;
                    if self.payload_remaining == 0 {
                        self.prefix_len = 0;
                    }
                }
                continue;
            }

            let copied = self.payload_remaining.min(bytes.len());
            self.payload_remaining -= copied;
            bytes = &bytes[copied..];
            if self.payload_remaining == 0 {
                self.prefix_len = 0;
            }
        }
    }
}

/// Arms an absolute monotonic write deadline for each established response.
/// Partial prefix or payload progress consumes the original budget instead of
/// restarting it, so a peer that stops reading cannot retain its worker forever.
pub(super) struct EstablishedResponseWriter<Cl, C> {
    pub(super) clock: Cl,
    pub(super) inner: C,
    pub(super) budget_ms: u64,
    pub(super) deadline_ms: Option<u64>,
    pub(super) progress: ResponseFrameProgress,
}

impl<Cl: MonotonicClock, C: DeadlineConnection> EstablishedResponseWriter<Cl, C> {
    pub(super) fn new(clock: Cl, inner: C, budget_ms: u64) -> Self {
        Self {
            clock,
            inner,
            budget_ms,
            deadline_ms: None,
            progress: ResponseFrameProgress::default(),
        }
    }

    pub(super) fn arm(&mut self) -> std::io::Result<()> {
        let now = self.clock.now_ms();
        let deadline = *self
            .deadline_ms
            .get_or_insert_with(|| now.saturating_add(self.budget_ms));
        if deadline <= now {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "daemon established response write deadline exceeded",
            ));
        }
        self.inner
            .set_write_deadline(Duration::from_millis(deadline - now))
    }

    pub(super) fn deadline_error(error: std::io::Error) -> std::io::Error {
        if matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ) {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "daemon established response write deadline exceeded",
            )
        } else {
            error
        }
    }
}

impl<Cl: MonotonicClock, C: DeadlineConnection> Write for EstablishedResponseWriter<Cl, C> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return self.inner.write(bytes);
        }
        self.arm()?;
        let written = self.inner.write(bytes).map_err(Self::deadline_error)?;
        self.progress.observe(&bytes[..written]);
        if self.progress.at_frame_start() {
            self.deadline_ms = None;
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.deadline_ms.is_some() {
            self.arm()?;
        }
        self.inner.flush().map_err(Self::deadline_error)
    }
}

/// The accepted stream half that can unblock a worker parked in a frame read.
///
/// A retained worker is joined at collection ([`ClientWorkers::retire`]), and a
/// thread blocked in `read` on a live socket would never return to be joined —
/// so what is retained alongside it is a duplicate descriptor that
/// `shutdown(2)` can close from the outside.
///
/// `shutdown(2)` alone is not enough. On Darwin it can return `Ok` for a
/// duplicate of an `AF_UNIX` socket *without* returning a peer parked in an
/// indefinite `recv`, which leaves the retirement barrier joining a thread that
/// never wakes — a daemon that then never finishes shutting down or rolling
/// over. Measured at roughly 1.5% of retirements on macOS. So the retired state
/// is also published as a flag that [`RetirableStream`] observes on its own
/// receive timeout: the syscall wakeup stays the fast path, and the flag is what
/// makes the wakeup guaranteed.
#[derive(Clone)]
pub(super) struct AcceptedStream {
    pub(super) stream: Arc<Mutex<Option<std::os::unix::net::UnixStream>>>,
    pub(super) retired: Arc<AtomicBool>,
}

impl AcceptedStream {
    pub(super) fn new(stream: std::os::unix::net::UnixStream) -> Self {
        Self {
            stream: Arc::new(Mutex::new(Some(stream))),
            retired: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The flag a worker parked on this connection watches to learn that
    /// retirement asked it to stop.
    pub(super) fn retirement(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.retired)
    }

    /// Observes a peer close without consuming bytes that may belong to a later
    /// request. The retained duplicate is already owned for retirement, so this
    /// adds no descriptor to a waiting decision.
    #[cfg(test)]
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=accepted_stream_observes_peer_close_behind_buffered_data
    pub(super) fn peer_disconnected(&self) -> bool {
        if self.retired.load(Ordering::Acquire) {
            return true;
        }
        let stream = self
            .stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(stream) = stream.as_ref() else {
            return true;
        };
        let mut pending = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        #[cfg(any(target_os = "android", target_os = "linux"))]
        {
            pending.events |= libc::POLLRDHUP;
        }
        loop {
            // SAFETY: one initialized `pollfd` names the live descriptor held by
            // `stream`; a zero timeout only observes its current state.
            let ready = unsafe { libc::poll(&raw mut pending, 1, 0) };
            if ready >= 0 {
                break;
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return true;
            }
        }
        let disconnected = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
        #[cfg(any(target_os = "android", target_os = "linux"))]
        let disconnected = disconnected | libc::POLLRDHUP;
        if pending.revents & disconnected != 0 {
            return true;
        }
        let mut byte = 0_u8;
        loop {
            // SAFETY: `byte` is a writable one-byte buffer and `stream` owns a
            // live descriptor for this call. MSG_PEEK never consumes payload.
            let read = unsafe {
                libc::recv(
                    stream.as_raw_fd(),
                    (&raw mut byte).cast(),
                    1,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            if read == 0 {
                return true;
            }
            if read > 0 {
                return false;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return error.kind() != std::io::ErrorKind::WouldBlock;
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=accepted_stream_observes_peer_close_behind_buffered_data
impl ConnectionShutdown for AcceptedStream {
    fn shutdown(&self) -> std::io::Result<()> {
        // Published before the syscall, so a worker that wakes for any reason —
        // including a receive timeout that races this call — observes the
        // retirement rather than parking again.
        self.retired.store(true, Ordering::Release);
        // The worker and collector share one closeable duplicate, not one fd
        // each. Taking it here means normal worker completion releases the
        // retirement descriptor immediately even though the finished
        // JoinHandle remains registered until the accept loop's next reap.
        let Some(stream) = self
            .stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return Ok(());
        };
        stream.shutdown(std::net::Shutdown::Both)
    }
}

/// An established connection's reader, which cannot outlive its own retirement.
///
/// Every read is gated on `poll(2)` with a bounded timeout, so the worker is
/// never parked in the kernel on a socket that has nothing to give it. The
/// timeout is *not* an idle policy: it is retried transparently, so an idle
/// subscription behaves exactly as it did before. The only thing it adds is a
/// point at which the worker observes [`AcceptedStream::shutdown`] and returns,
/// which is what keeps the retirement barrier joinable when the socket wakeup is
/// lost.
pub(super) struct RetiringReader {
    pub(super) stream: std::os::unix::net::UnixStream,
    pub(super) retired: Arc<AtomicBool>,
    pub(super) poll: Duration,
    /// How many waits expired without readability.
    ///
    /// The observation seam the tests use to prove this is a retry rather than
    /// an idle policy: a live connection must cross timeouts and still serve the
    /// frame that eventually arrives.
    pub(super) timeouts: Arc<AtomicUsize>,
}

impl RetiringReader {
    pub(super) fn new(
        stream: std::os::unix::net::UnixStream,
        retired: Arc<AtomicBool>,
        poll: Duration,
    ) -> Self {
        Self {
            stream,
            retired,
            poll,
            timeouts: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The timeout counter, so a test can wait until the reader has actually
    /// parked instead of assuming it has.
    #[cfg(test)]
    pub(super) fn timeouts(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.timeouts)
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_retired_reader_stops_without_any_socket_wakeup
impl Read for RetiringReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if readable_within(std::os::fd::AsRawFd::as_raw_fd(&self.stream), self.poll)? {
                return (&self.stream).read(bytes);
            }
            self.timeouts.fetch_add(1, Ordering::Release);
            // Retirement reads as end of stream, which is the same thing the
            // frame loop sees from a peer that hung up: it stops serving and
            // returns, with no invented protocol state.
            if self.retired.load(Ordering::Acquire) {
                return Ok(0);
            }
        }
    }
}

/// Close the accepted socket when its worker returns, independently of when
/// the retained join handle is next reaped.
pub(super) struct ShutdownAcceptedStreamOnDrop(pub(super) Option<AcceptedStream>);

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=accepted_stream_observes_peer_close_behind_buffered_data
impl Drop for ShutdownAcceptedStreamOnDrop {
    fn drop(&mut self) {
        if let Some(stream) = &self.0 {
            let _ = stream.shutdown();
        }
    }
}

pub(super) struct ServeLauncher {
    pub(super) exe: PathBuf,
    pub(super) launched: RefCell<Option<std::process::Child>>,
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=explicit_artifact_replacement_runs_under_one_coalesced_operation
impl ServeLauncher {
    pub(super) fn launch_standby(
        &self,
        workspace_root: Option<&Path>,
    ) -> std::io::Result<LaunchedStandby> {
        let mut command = std::process::Command::new(&self.exe);
        command
            .args(["daemon", "serve", "--standby"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(workspace_root) = workspace_root {
            // Standby hydration chooses the initialized workspace containing
            // its cwd. Pinning it to the sole planned Agent workspace makes
            // that workspace the successor's initial tenant, so exact resume
            // never has to steal a fence still held by the draining owner.
            command.current_dir(workspace_root);
        }
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let mut child = command.spawn()?;
        let pid = child.id();
        let identity = match process_start_identity(pid) {
            Ok(identity) => identity,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok(LaunchedStandby {
            child,
            record: DaemonRecord::identified(pid, identity),
        })
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=explicit_artifact_replacement_runs_under_one_coalesced_operation
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
        let child = command.spawn()?;
        self.launched.replace(Some(child));
        Ok(())
    }

    fn launched_exit(&self) -> std::io::Result<Option<String>> {
        let mut launched = self.launched.borrow_mut();
        let Some(child) = launched.as_mut() else {
            return Ok(None);
        };
        let Some(status) = child.try_wait()? else {
            return Ok(None);
        };
        launched.take();
        Ok(Some(status.to_string()))
    }

    fn abort_launch(&self) -> std::io::Result<()> {
        let Some(mut child) = self.launched.borrow_mut().take() else {
            return Ok(());
        };
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        child.kill()?;
        child.wait()?;
        Ok(())
    }

    fn recorded_failure(&self) -> Option<String> {
        ErrorLog::open_default()
            .ok()?
            .last_entry(chrono::Local::now().date_naive())
    }

    fn failure_log_hint(&self) -> Option<String> {
        ErrorLog::open_default()
            .ok()
            .map(|log| log.dir().display().to_string())
    }
}

pub(super) fn bootstrap_serve_command(exe: &Path, workspace: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command
        .args(["daemon", "serve"])
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=bootstrap_broker_accepts_only_ping_start_and_stop
pub(super) fn serve_bootstrap_broker(
    data_dir: &Path,
    workspace: &Path,
    exe: &Path,
    idle: BrokerIdlePolicy,
) -> std::io::Result<()> {
    use std::os::unix::fs::FileTypeExt as _;

    let workspace = paths::canonical_workspace_root(workspace)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
    let exe = exe.canonicalize()?;
    ensure_private_dir_all(data_dir)?;
    let daemon_dir = data_dir.join("daemon");
    ensure_private_dir(&daemon_dir)?;
    let address = bootstrap_broker_address(data_dir, &workspace, &exe);
    let lock = FileInstanceLock {
        path: address.lock.clone(),
        held: RefCell::new(None),
    };
    if !lock.acquire()? {
        return Ok(());
    }
    let socket = address.socket.clone();
    match std::fs::symlink_metadata(&socket) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(&socket)?,
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "daemon bootstrap broker endpoint is not a socket",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = std::os::unix::net::UnixListener::bind(&socket)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    let pid = std::process::id();
    let record = BootstrapBrokerRecord {
        pid,
        process_start_identity: process_start_identity(pid)?,
    };
    if let Err(error) = json_file::write_atomic(&daemon_dir, &address.record, &record) {
        drop(listener);
        let _ = std::fs::remove_file(&socket);
        return Err(std::io::Error::other(error.to_string()));
    }
    let activity = Arc::new(BrokerActivity::started());
    let watch = spawn_broker_idle_watch(&activity, address.clone(), data_dir, idle);
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        if workspace.canonicalize().ok().as_deref() != Some(workspace.as_path()) {
            break;
        }
        // A readiness probe may disconnect immediately after `connect`. Some
        // platforms reject timeout setup on that already-disconnected socket;
        // one vanished client must not terminate the broker process.
        if stream.set_read_timeout(Some(BROKER_IO_TIMEOUT)).is_err()
            || stream.set_write_timeout(Some(BROKER_IO_TIMEOUT)).is_err()
        {
            continue;
        }
        let mut request = [0_u8; 1];
        if stream.read_exact(&mut request).is_err() {
            continue;
        }
        // A peer that reached this point is using the broker, so the idle clock
        // restarts even for a request that is refused.
        activity.touch();
        let outcome = handle_bootstrap_broker_request(
            request[0],
            || launch_broker_daemon(&exe, &workspace, data_dir),
            || current_daemon_is_reachable(data_dir),
        );
        let _ = stream.write_all(&[if outcome.accepted { BROKER_OK } else { b'E' }]);
        if outcome.retire {
            break;
        }
    }
    // The watch is joined rather than detached: leaving it running would keep a
    // thread of this process alive past the endpoint it watches, and in tests it
    // would outlive the case that started it.
    activity.stop();
    let _ = watch.join();
    drop(listener);
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&address.record);
    Ok(())
}
