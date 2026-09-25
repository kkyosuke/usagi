//! daemon が持つ Agent runtime の open / restart 復旧・tenant inventory・decision 保守。

use usagi_core::infrastructure::client::DaemonClient as _;
use usagi_core::infrastructure::paths;
use usagi_core::infrastructure::persistence::json_file;
use usagi_core::infrastructure::workspace_state;
use usagi_core::usecase::claude_sandbox;
#[cfg(test)]
use usagi_daemon::usecase::runtime::RuntimeStoreSnapshot;

use crate::runtime::cli;

use super::agent_provisioning;

use super::{
    AGENT_RUNTIME_LIMIT, AdapterRegistry, AdmissionGate, AgentConcurrencyGauge,
    AgentIntegrationRevision, AgentProfileId, AgentPty, AgentPtyObservation, AgentReadinessProbe,
    AgentRuntime, AgentTerminalActor, AgyAdapter, Arc, BTreeSet, BackgroundWorker, ClaudeAdapter,
    ClientPolicy, ClientWorkspace, CodexAdapter, ConnectionWorkspace, CurrentLocatorFile,
    DECISION_MAINTENANCE_TICK, DEFAULT_GENERATION_LIMIT, DaemonRequest, DaemonRestartAgent,
    DaemonRestartAgentPlan, DecisionWake, DecisionWaker, DefaultModel, Deserialize, DiscardJournal,
    DispatchStore, Duration, ErrorLog, FailureTransitionLog, FileWorkspaceFences,
    GenerationRegistry, GenerationRegistryFile, GenerationRole, Geometry, LeaseClass, LockResult,
    Mutex, MutexGuard, OpenedTenant, OperationId, PENDING_DAEMON_AGENT_RESTART_MAX_BYTES,
    PENDING_DAEMON_AGENT_RESTART_SCHEMA, PENDING_DAEMON_AGENT_RESTART_TICK, Path, PathBuf,
    PromptMode, RootAgentRuntime, RootClaudeProvisioner, RootCodexProvisioner, RuntimeHydration,
    Serialize, SessionScopeResolver, ShardedAgentStore, SharedAgentRuntime, SharedSessionRuntime,
    SharedUserEnvironment, ShutdownRequest, SpawnProvision, SpawnedChildren, SyncSender,
    SystemAgentReadiness, TenantRegistry, TenantRuntimeOpener, TerminalOutcome,
    TerminalPipelineMetrics, TrySendError, UserDecisionStore, Workspaces, Write,
    existing_policy_client, hydrate_runtime_state, implicit_bound_workspace, known_sessions,
    observe_generation_process, open_runtime_state, open_session_runtime,
    pending_daemon_agent_restart_path, recover_rollover, repair_agent_codex_arg0_permissions,
    resolve_sandbox_cache_dir, run_agent_readiness, unopened_bound_workspace_refusal,
};

/// Resolves the workspace a connecting client will act on, adopting it when the
/// client selected one this daemon does not hold yet.
///
/// This is the point where "which workspace does this daemon serve?" stops being
/// a start-up constant. What each declaration means is unchanged
/// ([4. IPC の workspace fence](../../document/04-ipc.md#workspace-fence)); only
/// the answer's source moves from one fixed root to the tenant registry.
pub(super) struct TenantWorkspaces {
    pub(super) tenants: Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
    /// Where this data directory keeps the state subtree of every workspace it
    /// has opened. A bound client inside one of them is resolved against that
    /// record even when the workspace is no longer held.
    pub(super) daemon_dir: PathBuf,
    /// The workspace this process started in. A client that names no workspace
    /// touches no workspace resource, so it is admitted against this one, and it
    /// keeps answering for this root after the workspace is given back
    /// ([`retained_startup_root`]).
    pub(super) initial: PathBuf,
    /// This generation's authority. Opening a workspace is taking authority over
    /// it, which a generation that has handed off may no longer do
    /// ([`Self::may_open`]).
    pub(super) gate: AdmissionGate,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
impl TenantWorkspaces {
    /// The canonical spelling of a declared root, or the typed refusal for a
    /// root that cannot be resolved on this machine.
    pub(super) fn canonical(
        root: &str,
    ) -> Result<PathBuf, usagi_core::infrastructure::ipc::ProtocolError> {
        paths::canonical_workspace_root(root).map_err(|_| {
            usagi_core::infrastructure::ipc::workspace_refusal(
                "the declared workspace does not resolve on this machine",
                root,
            )
        })
    }

    /// Whether this generation may take authority over a workspace it does not
    /// already hold.
    ///
    /// Adopting a workspace fences its worktrees, branches, and session names
    /// for as long as the process lives, which only a generation that is still
    /// the authority may do. A replaced generation stays reachable — clients
    /// address it over its own socket to read the terminals it still owns, and
    /// their handshake declares their own cwd while doing so — but it takes no
    /// new work, and a workspace it adopted there would be fenced by a process
    /// that will never serve it. That is the same standing refusal its own
    /// startup workspace produced until it was given back
    /// (`release_initial_workspace`).
    pub(super) fn may_open(&self) -> bool {
        !self.gate.handed_off()
    }

    /// The startup workspace this generation still answers for at `root`, if it
    /// is that workspace. See [`retained_startup_root`].
    pub(super) fn retained_startup(&self, root: &Path) -> Option<PathBuf> {
        retained_startup_root(&self.initial, root)
    }

    /// The refusal for a workspace this generation holds no authority to open.
    ///
    /// The list names what it still answers for — the tenants it holds, plus the
    /// startup workspace it gave back — so a client that reached the wrong
    /// generation can see whether the workspace it meant is among them.
    pub(super) fn replaced_generation_refusal(
        &self,
    ) -> usagi_core::infrastructure::ipc::ProtocolError {
        let mut answering = self.served();
        let initial = paths::wire_workspace_root(&self.initial);
        if !answering.contains(&initial) {
            answering.push(initial);
            answering.sort_unstable();
        }
        usagi_core::infrastructure::ipc::workspace_refusal_serving(
            "this daemon generation was replaced and opens no further workspace; \
             reconnect to the daemon that is serving now",
            &answering,
        )
    }

    /// Every workspace this daemon currently holds, in wire spelling.
    ///
    /// A refusal names these rather than one fixed root, so a reader can tell
    /// whether the workspace they meant is among them.
    pub(super) fn served(&self) -> Vec<String> {
        let mut served: Vec<String> = self
            .tenants
            .adopted()
            .iter()
            .map(|tenant| paths::wire_workspace_root(tenant.root()))
            .collect();
        served.sort_unstable();
        served
    }
}

/// `initial` itself, when `root` still names the startup workspace.
///
/// A generation that has handed off gives that workspace back as soon as nothing
/// is running there, but it keeps serving the terminals it owns — and those are
/// addressed by clients standing in that very workspace. A handoff may only
/// begin when every participant can still reach the draining generation, so the
/// registry entry going away must not take the answer with it. Answering is not
/// owning: nothing here adopts the workspace or takes its fence again.
///
/// Matching is by path component, like every other workspace comparison, so
/// `<root>-2` is never read as a child of `<root>`.
pub(super) fn retained_startup_root(initial: &Path, root: &Path) -> Option<PathBuf> {
    if root.starts_with(initial) {
        return Some(initial.to_path_buf());
    }
    None
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
impl usagi_core::infrastructure::ipc::WorkspaceResolver for TenantWorkspaces {
    fn resolve(
        &self,
        declared: Option<&ClientWorkspace>,
    ) -> Result<String, usagi_core::infrastructure::ipc::ProtocolError> {
        match declared {
            // A client that names no workspace reads no workspace state, so the
            // workspace it is admitted against is immaterial; the one this
            // process started in keeps the refusal message meaningful.
            None | Some(ClientWorkspace::Unbound) => Ok(paths::wire_workspace_root(&self.initial)),
            // Selecting a workspace is what opens it: this daemon takes
            // authority over it now, or refuses that workspace alone.
            Some(ClientWorkspace::Selected { root }) => {
                let root = Self::canonical(root)?;
                // One read, not a check followed by an `adopt`: between the two
                // the sweep could give this very workspace back, and the `adopt`
                // would fence it again for a generation that will never serve it.
                if !self.may_open() {
                    // `selected` names one workspace and only that one, so the
                    // retained startup root answers only for itself.
                    return match self.tenants.tenant(&root) {
                        Some(_) => Ok(paths::wire_workspace_root(&root)),
                        None if self.retained_startup(&root).as_deref() == Some(root.as_path()) => {
                            Ok(paths::wire_workspace_root(&root))
                        }
                        None => Err(self.replaced_generation_refusal()),
                    };
                }
                self.tenants.adopt(&root).map_err(|error| {
                    // The refused root is the one this daemon could *not* take,
                    // so naming it as the workspace served would contradict the
                    // sentence it is appended to.
                    usagi_core::infrastructure::ipc::workspace_refusal_serving(
                        &error.to_string(),
                        &self.served(),
                    )
                })?;
                Ok(paths::wire_workspace_root(&root))
            }
            // A bound client says where it is running, not which workspace to
            // open. What the daemon already holds answers first, so a client
            // anywhere inside an adopted workspace resolves to it.
            //
            // A miss is not the end: a CLI or MCP client is as entitled to open a
            // workspace as the TUI is, and refusing here is what forced an
            // operator to open every new repository in the TUI once before their
            // CLI would work in it. What may be opened is narrow on purpose —
            // only a repository the caller is standing *at*, never one merely
            // above them ([`adoptable_workspace_root`]).
            //
            // The declared path need not exist: an Agent hook or a session tool
            // names a worktree path that its own teardown may already have
            // removed. Ancestor matching is a spelling comparison, so an
            // unresolvable path is compared as declared rather than refused.
            Some(ClientWorkspace::Bound { root }) => {
                let declared =
                    paths::canonical_workspace_root(root).unwrap_or_else(|_| PathBuf::from(root));
                if let Some(owner) = self.tenants.owner_of(&declared) {
                    return Ok(paths::wire_workspace_root(owner.root()));
                }
                if !self.may_open() {
                    // The held tenants were already consulted just above; only
                    // the workspace this process was started in is left.
                    return match self.retained_startup(&declared) {
                        Some(root) => Ok(paths::wire_workspace_root(&root)),
                        None => Err(self.replaced_generation_refusal()),
                    };
                }
                // Two ways a bound client may still name a workspace, tried in
                // this order because the first is a workspace that exists and the
                // second creates one.
                //
                // 1. A workspace this data directory has opened before records
                //    its canonical root in its state subtree, so a client inside
                //    it resolves even while the workspace is not held. Without
                //    this, a workspace that idled out of tenancy would refuse the
                //    very CLI and MCP clients running in it (#1537).
                // 2. Otherwise the caller may be standing *at* a repository this
                //    daemon has never seen. Opening that is what lets a CLI or
                //    MCP client start working in a fresh clone without opening it
                //    in the TUI first — and only the path itself is ever
                //    considered, never an ancestor ([`adoptable_workspace_root`]).
                let opening = implicit_bound_workspace(&self.daemon_dir, &declared)
                    .ok_or_else(|| unopened_bound_workspace_refusal(&declared, &self.served()))?;
                self.tenants.adopt(&opening).map_err(|error| {
                    usagi_core::infrastructure::ipc::workspace_refusal_serving(
                        &error.to_string(),
                        &self.served(),
                    )
                })?;
                Ok(paths::wire_workspace_root(&opening))
            }
        }
    }
}

pub(super) struct SharedAgentState {
    pub(super) owner: Mutex<RootAgentRuntime>,
    pub(super) readiness: Arc<dyn AgentReadinessProbe>,
}

impl SharedAgentState {
    pub(super) fn lock(&self) -> LockResult<MutexGuard<'_, RootAgentRuntime>> {
        self.owner.lock()
    }
}

pub(super) struct AgentDecisionWaker<'a> {
    pub(super) agent: &'a SharedAgentRuntime,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_user_decision_round_trip_reaches_the_original_caller
impl DecisionWaker for AgentDecisionWaker<'_> {
    fn wake(&mut self, wake: &DecisionWake) -> anyhow::Result<()> {
        let prompt = format!(
            "Supervisor child {} finished ({:?}). Re-open the durable task tree, verify and aggregate the child result, then continue the parent decision. Summary: {}",
            wake.child_run_id, wake.outcome.kind, wake.outcome.summary
        );
        let mut runtime = self
            .agent
            .lock()
            .map_err(|_| anyhow::anyhow!("agent owner is unavailable"))?;
        if runtime
            .prompt_run(wake.parent.dispatch_run_id, &prompt)
            .is_ok()
        {
            return Ok(());
        }
        let binding = runtime
            .dispatch_store()
            .binding(wake.parent.dispatch_run_id)?
            .ok_or_else(|| anyhow::anyhow!("parent dispatch binding is unavailable"))?;
        let workspace = runtime
            .dispatch_store()
            .workspace_for_agent(binding.worker.agent_id)?
            .ok_or_else(|| anyhow::anyhow!("parent workspace is unavailable"))?;
        if runtime
            .prompt(
                workspace,
                binding.worker.session_id,
                &prompt,
                PromptMode::Live,
            )
            .is_ok()
        {
            return Ok(());
        }
        runtime
            .queue_prompt_for_next_launch(workspace, binding.worker.session_id, &prompt)
            .map_err(|error| anyhow::anyhow!(error.message))?;
        Ok(())
    }
}

pub(super) struct DeferredDecisionWaker;

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_supervisor_tools_observe_one_durable_aggregate
impl DecisionWaker for DeferredDecisionWaker {
    fn wake(&mut self, _: &DecisionWake) -> anyhow::Result<()> {
        anyhow::bail!("parent agent wake is deferred until the agent owner is available")
    }
}

/// Locks the shared Agent owner for one terminal request; a poisoned lock is a
/// safe unavailable error rather than a client-side fallback.
pub(super) struct SharedAgent {
    pub(super) runtime: SharedAgentRuntime,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_dispatch_worker_complete_reaches_the_caller_inbox
impl AgentTerminalActor for SharedAgent {
    fn handle(
        &mut self,
        context: usagi_daemon::usecase::terminal_owner::TerminalRequestContext,
        request: usagi_core::infrastructure::ipc::TerminalRequest,
    ) -> TerminalOutcome {
        match self.runtime.lock() {
            Ok(mut agent) => AgentTerminalActor::handle(&mut *agent, context, request),
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
        self.runtime
            .lock()
            .map(|agent| AgentTerminalActor::terminal_inventory(&*agent, scope))
            .unwrap_or_default()
    }
    fn completed_inventory(
        &self,
        scope: &usagi_core::domain::terminal_launch::TerminalLaunchScope,
    ) -> Vec<usagi_core::domain::terminal_visibility::CompletedTerminalEntry> {
        // A poisoned lock is a safe empty tombstone list, never a fallback.
        self.runtime
            .lock()
            .map(|agent| AgentTerminalActor::completed_inventory(&*agent, scope))
            .unwrap_or_default()
    }
    fn disconnect(&mut self, _connection: usagi_core::domain::id::ConnectionId) {}
}

pub(super) fn provisioned_agent_command(
    product_program: &str,
    durable_argv: &[String],
    provision: &SpawnProvision,
) -> (String, Vec<String>) {
    let (program, mut argv) = match provision.sandbox_launcher() {
        Some(launcher) => {
            let mut argv = launcher.prefix.clone();
            argv.push(product_program.to_owned());
            (launcher.program.clone(), argv)
        }
        None => (product_program.to_owned(), Vec::new()),
    };
    argv.extend(provision.arguments().iter().cloned());
    argv.extend(durable_argv.iter().cloned());
    (program, argv)
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=root_ipc_fixture_codex_survives_disconnect_and_replays_final
pub(super) fn send_agent_observation(
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

/// Removes Agent records whose managed session was already retired by an
/// older daemon. This startup pass repairs the historical state where session
/// teardown removed the lifecycle row without closing its Agent owner.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_session_remove_is_accepted_before_the_daemon_tears_the_worktree_down
pub(super) fn reconcile_removed_session_agents(
    daemon_dir: &Path,
    agent: &SharedAgentRuntime,
) -> std::io::Result<usize> {
    // The Agent runtime is daemon-wide while sessions belong to workspaces, and
    // its records outlive both a workspace's tenancy and the daemon itself. What
    // is still owned is therefore every session this data directory knows, not
    // the sessions of the workspaces adopted so far: at startup only one is, so
    // reconciling against that would close every other workspace's Agents.
    let retained = known_sessions(daemon_dir)
        .ok_or_else(|| std::io::Error::other("workspace lifecycle state is unavailable"))?;
    let mut agent = agent
        .lock()
        .map_err(|_| std::io::Error::other("agent owner is unavailable"))?;
    let removed = agent
        .managed_session_ids()
        .difference(&retained)
        .copied()
        .collect::<Vec<_>>();
    let mut closed = 0;
    for session in removed {
        closed += agent
            .close_session(session)
            .map_err(|error| std::io::Error::other(error.message))?;
    }
    Ok(closed)
}

/// Keeps decision deadlines progressing even when no subsequent MCP/TUI
/// request arrives. Every action is idempotent, so a daemon restart simply
/// resumes from the JSON store.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_user_decision_round_trip_reaches_the_original_caller
pub(super) fn start_decision_maintenance(
    decisions: Arc<UserDecisionStore>,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_decision_maintenance(decisions, shutdown, DECISION_MAINTENANCE_TICK)
}

/// The loop, with the tick injected so a test can drive it without waiting out
/// the production cadence.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_user_decision_round_trip_reaches_the_original_caller
pub(super) fn spawn_decision_maintenance(
    decisions: Arc<UserDecisionStore>,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-decision-maintenance".to_string())
        .spawn(move || {
            let worker_health =
                shutdown.monitor_background_worker(BackgroundWorker::DecisionMaintenance);
            while !shutdown.is_requested() {
                let _ = decisions.expire_due(chrono::Utc::now());
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
            worker_health.finish_planned();
        })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Composition injects each Agent dependency separately.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_dispatch_uses_the_trusted_root_before_and_after_session_creation
pub(super) fn open_agent_runtime(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    workspaces: Workspaces,
    pty: AgentPty,
    mcp_command: PathBuf,
    environment: Arc<SharedUserEnvironment>,
    retention: usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention,
    concurrency: AgentConcurrencyGauge,
    children: &Arc<SpawnedChildren>,
    hydration: RuntimeHydration,
    terminal_limit: usize,
) -> std::io::Result<SharedAgentRuntime> {
    let state = open_runtime_state(data_dir, generation, children, terminal_limit)?;
    let snapshot = match hydration {
        RuntimeHydration::All => hydrate_runtime_state(&state, "agent runtime")?.agents,
        RuntimeHydration::AgentResumeHistory => {
            let mut snapshot = hydrate_runtime_state(&state, "Agent resume history")?.agents;
            snapshot.records.retain(|record| {
                matches!(
                    record.state,
                    usagi_daemon::usecase::runtime::RuntimeState::Exited
                        | usagi_daemon::usecase::runtime::RuntimeState::Reclaimed
                        | usagi_daemon::usecase::runtime::RuntimeState::Interrupted
                        | usagi_daemon::usecase::runtime::RuntimeState::Sleeping
                        | usagi_daemon::usecase::runtime::RuntimeState::ReconcileRequired(
                            usagi_daemon::usecase::runtime::ReconcileState::IdentityUnknown
                        )
                )
            });
            snapshot.reconcile_after_daemon_restart().0
        }
        #[cfg(test)]
        RuntimeHydration::Empty => RuntimeStoreSnapshot::default(),
    };
    let store = ShardedAgentStore::new(state);
    let mut registry = AdapterRegistry::new();
    // The `$HOME` a managed launch resolves. The readiness probe takes the same
    // value rather than resolving it again: a provider whose config directory is
    // named relative to a *different* home would be probed somewhere it will
    // never run.
    let sandbox_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|path| path.canonicalize().ok());
    // The readiness probe needs the same home and configured credential the
    // launch will use; a probe that lacks them answers about a different
    // provider or a missing key it would in fact have had.
    let readiness: Arc<dyn AgentReadinessProbe> = Arc::new(SystemAgentReadiness {
        home: sandbox_home.clone(),
        environment: Some(Arc::clone(&environment)),
        workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        ..SystemAgentReadiness::default()
    });
    // Agent MCP children receive the mode-neutral base. They apply the same
    // selected runtime mode themselves, so every mode reaches the daemon's
    // already-selected directory without adding that child twice. Production
    // selects the base itself, so the pair — not a `parent()` guess — is what
    // keeps this from resolving one level above the data home (#608).
    let data_home = paths::DataHome::from_selected(data_dir, paths::runtime_mode());
    let sandbox_platform = if cfg!(target_os = "macos") {
        claude_sandbox::Platform::MacOs
    } else if cfg!(target_os = "linux") {
        claude_sandbox::Platform::Linux
    } else {
        claude_sandbox::Platform::Unsupported
    };
    let sandbox_backend = cli::resolve_sandbox_backend(sandbox_platform);
    let sandbox_tmpdir = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .and_then(|path| path.canonicalize().ok());
    let sandbox_cache_dir = resolve_sandbox_cache_dir();
    let sandbox_passthrough = claude_sandbox::passthrough_requested(
        cfg!(debug_assertions),
        std::env::var(claude_sandbox::PASSTHROUGH_ENVIRONMENT_VARIABLE)
            .ok()
            .as_deref(),
    );
    repair_agent_codex_arg0_permissions(sandbox_home.as_deref());
    // Duplicate registration cannot happen for the two literal profiles; a
    // failure here would only drop an adapter, so the launch would surface a
    // safe unknown-profile error rather than crash the daemon.
    let _ = registry.register_supported(
        CodexAdapter::new(RootCodexProvisioner {
            workspaces: Arc::clone(&workspaces),
            mcp_command: mcp_command.clone(),
            data_home: data_home.clone(),
            agent: DefaultModel::OpenAi,
            environment: Some(Arc::clone(&environment)),
            sandbox_backend: sandbox_backend.clone(),
            sandbox_tmpdir: sandbox_tmpdir.clone(),
            sandbox_home: sandbox_home.clone(),
            sandbox_cache_dir: sandbox_cache_dir.clone(),
            sandbox_passthrough,
        }),
        // Fugu is the same Claude CLI pointed at Sakana's Anthropic-compatible
        // endpoint, so it reuses this adapter and differs only in the provider
        // its provisioner carries: gateway variables, its own config directory,
        // and its own API key.
        ClaudeAdapter::sakana(RootClaudeProvisioner {
            workspaces: Arc::clone(&workspaces),
            mcp_command: mcp_command.clone(),
            data_home: data_home.clone(),
            agent: DefaultModel::SakanaAi,
            sandbox_backend: sandbox_backend.clone(),
            sandbox_tmpdir: sandbox_tmpdir.clone(),
            sandbox_home: sandbox_home.clone(),
            sandbox_cache_dir: sandbox_cache_dir.clone(),
            environment: Some(Arc::clone(&environment)),
            sandbox_passthrough,
        }),
        ClaudeAdapter::new(RootClaudeProvisioner {
            workspaces: Arc::clone(&workspaces),
            mcp_command: mcp_command.clone(),
            data_home: data_home.clone(),
            agent: DefaultModel::Claude,
            sandbox_backend: sandbox_backend.clone(),
            sandbox_tmpdir: sandbox_tmpdir.clone(),
            sandbox_home: sandbox_home.clone(),
            sandbox_cache_dir: sandbox_cache_dir.clone(),
            environment: Some(Arc::clone(&environment)),
            // E2E テスト専用 seam。release ビルドでは `cfg!(debug_assertions)` が false になるため、
            // 配布バイナリは常に拘束された Claude だけを起動する。
            sandbox_passthrough,
        }),
        AgyAdapter::new(agent_provisioning::RootAgyProvisioner {
            workspaces,
            mcp_command,
            data_home,
            environment: Some(environment),
            sandbox_backend,
            sandbox_tmpdir,
            sandbox_home,
            sandbox_cache_dir,
            sandbox_passthrough,
        }),
    );
    let mut runtime = AgentRuntime::hydrate_with_retention(
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
    // Bind before the runtime is shared, so the metrics broker never observes an
    // unpublished level for a runtime that already hydrated interrupted records.
    runtime.bind_concurrency_gauge(concurrency);
    Ok(Arc::new(SharedAgentState {
        owner: Mutex::new(runtime),
        readiness,
    }))
}

/// Durable custody for the exact Agent sources stopped immediately before W1.
///
/// The requester process is intentionally absent from this authority. Once the
/// old owner has stopped a source, either that owner rolls it back after a
/// pre-commit refusal or whichever generation becomes active resumes it with
/// the staged build's integration. Stable per-item operation IDs make every
/// retry converge on the same durable source relation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingDaemonAgentRestart {
    pub(super) schema_version: u16,
    pub(super) operation_id: String,
    pub(super) from_generation: usagi_core::domain::id::DaemonGeneration,
    pub(super) workspace_root: PathBuf,
    pub(super) agents: Vec<PendingDaemonAgentRestartItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingDaemonAgentRestartItem {
    pub(super) resume_operation_id: String,
    pub(super) agent: DaemonRestartAgent,
    #[serde(default)]
    pub(super) completed: bool,
}

impl PendingDaemonAgentRestart {
    pub(super) fn new(
        operation: &OperationId,
        from_generation: usagi_core::domain::id::DaemonGeneration,
        workspace_root: PathBuf,
        plan: &DaemonRestartAgentPlan,
    ) -> Self {
        Self {
            schema_version: PENDING_DAEMON_AGENT_RESTART_SCHEMA,
            operation_id: operation.0.clone(),
            from_generation,
            workspace_root,
            agents: plan
                .agents
                .iter()
                .cloned()
                .map(|agent| PendingDaemonAgentRestartItem {
                    resume_operation_id: usagi_core::domain::id::OperationId::new().to_string(),
                    agent,
                    completed: false,
                })
                .collect(),
        }
    }

    #[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=pending_daemon_agent_restart_round_trips_and_clears_only_its_operation
    pub(super) fn validate(&self) -> std::io::Result<()> {
        if self.schema_version != PENDING_DAEMON_AGENT_RESTART_SCHEMA
            || self.operation_id.is_empty()
            || self.agents.is_empty()
            || self.agents.len() > AGENT_RUNTIME_LIMIT
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "pending daemon Agent restart has an unsupported shape",
            ));
        }
        let canonical = paths::canonical_workspace_root(&self.workspace_root)
            .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
        if canonical != self.workspace_root {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "pending daemon Agent restart has a non-canonical workspace",
            ));
        }
        let workspaces = self
            .agents
            .iter()
            .map(|item| item.agent.runtime.terminal.workspace_id)
            .collect::<BTreeSet<_>>();
        let resume_operations = self
            .agents
            .iter()
            .map(|item| item.resume_operation_id.as_str())
            .collect::<BTreeSet<_>>();
        let runtimes = self
            .agents
            .iter()
            .map(|item| item.agent.runtime.agent_runtime_id)
            .collect::<BTreeSet<_>>();
        if workspaces.len() != 1
            || resume_operations.len() != self.agents.len()
            || runtimes.len() != self.agents.len()
            || self.agents.iter().any(|item| {
                item.agent.runtime.agent_runtime_id != item.agent.target.runtime_id
                    || item.agent.runtime.terminal.workspace_id != item.agent.target.workspace_id
                    || item.agent.runtime.session_id != item.agent.target.session_id
                    || item.agent.runtime.terminal.session_id != item.agent.target.session_id
                    || item.agent.runtime.terminal.worktree_id != item.agent.target.worktree_id
                    || usagi_core::domain::id::OperationId::parse(&item.resume_operation_id)
                        .is_err()
            })
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "pending daemon Agent restart has inconsistent Agent fences",
            ));
        }
        Ok(())
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=pending_daemon_agent_restart_round_trips_and_clears_only_its_operation
pub(super) fn read_pending_daemon_agent_restart(
    data_dir: &Path,
) -> std::io::Result<Option<PendingDaemonAgentRestart>> {
    let pending: Option<PendingDaemonAgentRestart> = json_file::read_bounded(
        &pending_daemon_agent_restart_path(data_dir),
        PENDING_DAEMON_AGENT_RESTART_MAX_BYTES,
    )
    .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
    if let Some(pending) = &pending {
        pending.validate()?;
    }
    Ok(pending)
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=pending_daemon_agent_restart_round_trips_and_clears_only_its_operation
pub(super) fn write_pending_daemon_agent_restart(
    data_dir: &Path,
    pending: &PendingDaemonAgentRestart,
) -> std::io::Result<()> {
    pending.validate()?;
    let daemon_dir = data_dir.join("daemon");
    json_file::write_atomic(
        &daemon_dir,
        &pending_daemon_agent_restart_path(data_dir),
        pending,
    )
    .map_err(|error| std::io::Error::other(format!("{error:#}")))
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=pending_daemon_agent_restart_round_trips_and_clears_only_its_operation
pub(super) fn clear_pending_daemon_agent_restart(
    data_dir: &Path,
    operation_id: &str,
) -> std::io::Result<bool> {
    let Some(pending) = read_pending_daemon_agent_restart(data_dir)? else {
        return Ok(false);
    };
    if pending.operation_id != operation_id {
        return Ok(false);
    }
    let path = pending_daemon_agent_restart_path(data_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            if let Some(parent) = path.parent()
                && let Ok(directory) = std::fs::File::open(parent)
            {
                let _ = directory.sync_all();
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=planned_agent_workspace_is_resolved_by_durable_identity
pub(super) fn planned_agent_workspace_root(
    data_dir: &Path,
    plan: &DaemonRestartAgentPlan,
) -> std::io::Result<Option<PathBuf>> {
    let workspaces = plan
        .agents
        .iter()
        .map(|agent| agent.runtime.terminal.workspace_id)
        .collect::<BTreeSet<_>>();
    let Some(workspace) = workspaces.iter().next().copied() else {
        return Ok(None);
    };
    if workspaces.len() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "live Agents span multiple workspaces; no Agent was stopped",
        ));
    }
    let mut roots = Vec::new();
    for state in workspace_state::adopted(&data_dir.join("daemon"))
        .map_err(|error| std::io::Error::other(format!("{error:#}")))?
    {
        let lifecycle =
            usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore::new(state.dir())
                .load()
                .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
        if lifecycle.is_some_and(|state| state.workspace_id == workspace) {
            roots.push(state.root().to_path_buf());
        }
    }
    match roots.as_slice() {
        [root] => Ok(Some(root.clone())),
        [] => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "live Agent workspace is not present in durable daemon state",
        )),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "live Agent workspace identity is ambiguous in durable daemon state",
        )),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=root_restart_recovers_agents_after_requester_exit
pub(super) fn restore_pending_daemon_agents(
    data_dir: &Path,
    agent: &SharedAgentRuntime,
    scope: &dyn SessionScopeResolver,
    pending: &mut PendingDaemonAgentRestart,
    current_integration: bool,
) -> std::io::Result<usize> {
    let mut restored = 0;
    for index in 0..pending.agents.len() {
        let item = pending.agents[index].clone();
        if item.completed {
            continue;
        }
        let owner = agent
            .lock()
            .map_err(|_| std::io::Error::other("agent owner is unavailable"))?;
        if !owner
            .daemon_restart_restore_needed(&item.agent.runtime)
            .map_err(|error| std::io::Error::other(error.message))?
        {
            drop(owner);
            pending.agents[index].completed = true;
            write_pending_daemon_agent_restart(data_dir, pending)?;
            continue;
        }
        let preflight = if current_integration {
            owner.prepare_current_integration_resume_readiness(
                &item.resume_operation_id,
                &item.agent.target,
                item.agent.expected_revision,
            )
        } else {
            owner.prepare_resume_readiness(&item.resume_operation_id, &item.agent.target)
        }
        .map_err(|error| std::io::Error::other(error.message))?;
        drop(owner);
        run_agent_readiness(agent, preflight.as_ref())
            .map_err(|error| std::io::Error::other(error.message))?;
        let mut owner = agent
            .lock()
            .map_err(|_| std::io::Error::other("agent owner is unavailable"))?;
        let resumed = if current_integration {
            owner.resume_with_current_integration_after_readiness(
                &item.resume_operation_id,
                &item.agent.target,
                item.agent.expected_revision,
                scope,
                preflight.as_ref(),
            )
        } else {
            owner.resume_exact_after_readiness(
                &item.resume_operation_id,
                &item.agent.target,
                scope,
                preflight.as_ref(),
            )
        };
        resumed.map_err(|error| std::io::Error::other(error.message))?;
        drop(owner);
        restored += 1;
        pending.agents[index].completed = true;
        // Progress is committed item by item. If a later provider is
        // unavailable for longer than source retention, an already completed
        // item is never looked up again; a crash between spawn and this write
        // still replays the same durable resume operation.
        write_pending_daemon_agent_restart(data_dir, pending)?;
    }
    Ok(restored)
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=root_restart_recovers_agents_after_requester_exit
pub(super) fn recover_pending_daemon_agents_once(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    gate: &AdmissionGate,
    tenants: &TenantRegistry<FileWorkspaceFences, SystemTenantOpener>,
    workspaces: &Workspaces,
    agent: &SharedAgentRuntime,
) -> std::io::Result<Option<usize>> {
    let Some(observed) = read_pending_daemon_agent_restart(data_dir)? else {
        return Ok(None);
    };
    let Ok(_lease) = gate.acquire(LeaseClass::ActiveControl) else {
        return Ok(None);
    };
    // Re-read after admission. A lifecycle transition may have completed and
    // cleared or replaced the intent while this worker waited for the gate.
    let Some(mut pending) = read_pending_daemon_agent_restart(data_dir)? else {
        return Ok(None);
    };
    if pending.operation_id != observed.operation_id {
        return Ok(None);
    }
    let registry = GenerationRegistry::new(
        GenerationRegistryFile::new(data_dir)?,
        DEFAULT_GENERATION_LIMIT,
    );
    let mut snapshot = registry
        .load()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if snapshot.document().current != Some(generation)
        || snapshot.document().role(generation) != Some(GenerationRole::Active)
    {
        return Ok(None);
    }
    if generation == pending.from_generation
        && snapshot
            .document()
            .handoff
            .as_ref()
            .is_some_and(|handoff| handoff.operation.0 == pending.operation_id)
    {
        // A request which returned after W1 but before W2 leaves a preparing
        // intent. Resolve that durable boundary before deciding whether the old
        // owner may roll back; resuming while recovery could still commit would
        // mint credentials on the wrong side of the handoff.
        recover_rollover(
            &registry,
            &CurrentLocatorFile::new(data_dir),
            &mut observe_generation_process,
        )
        .map_err(|error| std::io::Error::other(error.to_string()))?;
        snapshot = registry
            .load()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        if snapshot.document().current != Some(generation)
            || snapshot
                .document()
                .handoff
                .as_ref()
                .is_some_and(|handoff| handoff.operation.0 == pending.operation_id)
        {
            return Ok(None);
        }
    }
    let current_integration = generation != pending.from_generation;
    let tenant = match tenants.tenant(&pending.workspace_root) {
        Some(tenant) => tenant,
        None => tenants
            .adopt(&pending.workspace_root)
            .map_err(std::io::Error::from)?,
    };
    let workspace_id = pending.agents[0].agent.target.workspace_id;
    if tenant.workspace_id() != workspace_id {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "pending daemon Agent restart workspace identity changed",
        ));
    }
    let bound = ConnectionWorkspace {
        tenant,
        workspaces: Arc::clone(workspaces),
    };
    let restored = restore_pending_daemon_agents(
        data_dir,
        agent,
        &bound.scope_resolver(),
        &mut pending,
        current_integration,
    )?;
    clear_pending_daemon_agent_restart(data_dir, &pending.operation_id)?;
    Ok(Some(restored))
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=root_restart_recovers_agents_after_requester_exit
pub(super) fn start_daemon_agent_restart_recovery(
    data_dir: PathBuf,
    generation: usagi_core::domain::id::DaemonGeneration,
    gate: AdmissionGate,
    tenants: Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
    workspaces: Workspaces,
    agent: SharedAgentRuntime,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-agent-restart-recovery".to_owned())
        .spawn(move || {
            let mut failures = FailureTransitionLog::default();
            while !shutdown.is_requested() {
                match recover_pending_daemon_agents_once(
                    &data_dir,
                    generation,
                    &gate,
                    &tenants,
                    &workspaces,
                    &agent,
                ) {
                    Ok(Some(restored)) => {
                        failures.changed(None);
                        ErrorLog::record(&format!(
                            "daemon Agent restart recovery resumed {restored} Agent(s)"
                        ));
                    }
                    Ok(None) => {
                        failures.changed(None);
                    }
                    Err(error) => {
                        if let Some(error) = failures.changed(Some(error.to_string())) {
                            ErrorLog::record(&format!(
                                "daemon Agent restart recovery deferred: {error}"
                            ));
                        }
                    }
                }
                if shutdown.wait_for_tick(PENDING_DAEMON_AGENT_RESTART_TICK) {
                    break;
                }
            }
        })
}

/// Opens one workspace's lifecycle runtime with the real git and filesystem
/// seams, reading the shared catalogs from the data home every workspace shares.
pub(super) struct SystemTenantOpener {
    pub(super) data_home: PathBuf,
    pub(super) generation: usagi_core::domain::id::DaemonGeneration,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=opening_a_second_workspace_adopts_it_without_disturbing_the_first
impl TenantRuntimeOpener for SystemTenantOpener {
    type Runtime = SharedSessionRuntime;

    fn open(
        &self,
        workspace_root: &Path,
        state_dir: &Path,
    ) -> std::io::Result<OpenedTenant<Self::Runtime>> {
        let runtime = open_session_runtime(
            workspace_root.to_path_buf(),
            state_dir,
            &self.data_home,
            self.generation,
        )?;
        let workspace_id = runtime
            .lock()
            .map_err(|_| std::io::Error::other("session runtime is unavailable"))?
            .workspace_id()
            .map_err(|error| std::io::Error::other(error.safe_message()))?;
        Ok(OpenedTenant {
            runtime,
            workspace_id,
        })
    }
}

/// Append live-only tenant state. An absent daemon or stale lifecycle record
/// deliberately leaves the presentation layer's existing status text intact.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-08-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
pub(super) fn append_live_tenant_inventory(out: &mut dyn Write) {
    use usagi_core::infrastructure::ipc::{DaemonReply, TenantAction, TenantInventory};

    let Ok(mut client) = existing_policy_client(ClientPolicy::cli(), ClientWorkspace::Unbound)
    else {
        return;
    };
    let Ok(DaemonReply::Ok(value)) = client.request(DaemonRequest::Tenant {
        action: TenantAction::Inventory,
        root: None,
        force: false,
    }) else {
        return;
    };
    let Ok(inventory) = serde_json::from_value::<TenantInventory>(value) else {
        return;
    };
    for tenant in inventory.tenants {
        let _ = writeln!(
            out,
            "  tenant: {} (sessions: {}, live/unknown runtimes: {})",
            tenant.root, tenant.sessions, tenant.live_runtimes
        );
    }
}

pub(super) fn current_agent_integrations() -> Vec<AgentIntegrationRevision> {
    [
        (
            DefaultModel::Claude,
            usagi_daemon::usecase::claude::PROFILE_REVISION,
        ),
        (
            DefaultModel::OpenAi,
            usagi_daemon::usecase::codex::PROFILE_REVISION,
        ),
        (
            DefaultModel::SakanaAi,
            usagi_daemon::usecase::codex::PROFILE_REVISION,
        ),
        (
            DefaultModel::Agy,
            usagi_daemon::usecase::agy::PROFILE_REVISION,
        ),
    ]
    .into_iter()
    .map(|(model, revision)| AgentIntegrationRevision {
        profile_id: AgentProfileId::new(model.profile_id())
            .expect("code-defined profile ID is canonical"),
        revision,
    })
    .collect()
}
