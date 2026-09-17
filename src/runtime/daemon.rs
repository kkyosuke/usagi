//! daemon 面へ Unix process / socket / signal を接続する composition adapter。

mod agent;
mod agent_provisioning;
mod broker;
mod dispatch;
mod instance_lock;
mod ipc_accept;
mod pty;
mod secure_path;
mod standby;
mod tenant_control;
mod workers;
mod workflow;

use ipc_accept::{
    AcceptedStream, EstablishedResponseWriter, IpcReady, ObservedChildren,
    PreHandshakeDeadlineStream, RetiringReader, ServeLauncher, ShutdownAcceptedStreamOnDrop,
    bootstrap_serve_command, observed_seamless_refusal, serve_bootstrap_broker, spawn_ipc_server,
};
#[cfg(test)]
use ipc_accept::{ResponseFrameProgress, start_terminal_observer};

#[cfg(test)]
use agent::spawn_decision_maintenance;
use agent::{
    AgentDecisionWaker, DeferredDecisionWaker, PendingDaemonAgentRestart, SharedAgent,
    SharedAgentState, SystemTenantOpener, TenantWorkspaces, append_live_tenant_inventory,
    clear_pending_daemon_agent_restart, current_agent_integrations, open_agent_runtime,
    planned_agent_workspace_root, provisioned_agent_command, read_pending_daemon_agent_restart,
    reconcile_removed_session_agents, restore_pending_daemon_agents, send_agent_observation,
    start_daemon_agent_restart_recovery, start_decision_maintenance,
    write_pending_daemon_agent_restart,
};

use workers::{
    AgentReadiness, AgentReadinessProbe, AutomaticOrphanCleanup, ClosePrProjectionOnExit,
    ConnectionCleanup, ConnectionCleanupInbox, DaemonBackgroundWorkers, OrphanCleanupPass,
    ReadinessBounds, ShutdownOnIpcWorkerExit, ShutdownOnUnexpectedWorkerExit,
    ShutdownOnWorkerPanic, ShutdownPipe, SignalShutdown, SigtermTerminator, SystemAgentReadiness,
    retain_client_worker, spawn_critical_worker, spawn_orphan_cleanup_worker,
    spawn_pr_refresh_worker, spawn_tenant_retire_worker, start_connection_cleanup_worker,
    start_draining_collection_worker, start_pr_projection_worker, start_retention_gc_worker,
    start_session_teardown_worker,
};
#[cfg(test)]
use workers::{
    ReadinessState, spawn_draining_collection_worker, spawn_retention_gc_worker,
    spawn_session_teardown_worker,
};

pub(crate) use standby::trusted_generations;
use standby::{
    GenerationFence, LaunchedStandby, RegistryGenerationControl, StandbyIpc,
    StandbyRegistryAuthority, UnixStandbyProbe, live_generation_endpoints,
    observe_generation_process, registered_generations,
};
#[cfg(test)]
use standby::{StandbyShutdownDomains, standby_workspace_state_dir};

use pty::{
    AgentPty, AgentPtyObservation, DaemonPty, PtyObservation, SharedTerminal,
    SharedTerminalScopeResolver, TerminalPipelineMetrics, TrustedLoginShell, new_terminal_runtime,
    terminal_capacity_limit, terminal_environment,
};
#[cfg(test)]
use pty::{daemon_pty_failure_entry, send_pty_observation, terminal_environment_from};

use instance_lock::{
    CensusConnectionFence, ExactProcessControl, FileInstanceLock, FileWorkspaceFence,
    FileWorkspaceFences, FsCustodyProbe, InstanceLockCustody, PrivateLockModePolicy,
    PrivateLockWait, acquire_lifecycle_lock_io_within, lock_private_exclusive,
    process_instance_lock, request_replacement_while_locked, run_with_lifecycle_custody,
    start_custody_worker,
};
#[cfg(test)]
use instance_lock::{
    PrivateLockAfterFlockBarrier, ProcessInstanceLock, install_private_lock_after_flock_barrier,
    lock_paths_alias, spawn_custody_worker,
};

#[cfg(test)]
use broker::{
    BootstrapBrokerAddress, BrokerOutcome, acquire_bootstrap_lock_within, broker_endpoint_present,
    request_bootstrap_broker,
};
use broker::{
    BootstrapBrokerRecord, BrokerActivity, BrokerIdlePolicy, acquire_bootstrap_lock,
    bootstrap_broker_address, bootstrap_client, handle_bootstrap_broker_request,
    launch_broker_daemon, map_bootstrap_lock_error, retire_bootstrap_broker,
    run_broker_lifecycle_command, spawn_bootstrap_broker, spawn_broker_idle_watch,
};

use dispatch::session::reconcile_orphan_delegations;
use dispatch::{
    DispatchToolContext, SessionDispatchContext, authenticated_supervisor_caller,
    clean_orphan_session_resources, daemon_request_surface, dispatch_agent,
    dispatch_agent_phase_report, dispatch_codex_session_capture, dispatch_dispatch,
    dispatch_dispatch_tool, dispatch_mcp_child_claim, dispatch_metrics, dispatch_pr_snapshot,
    dispatch_rollover, dispatch_session, dispatch_supervisor_control, dispatch_supervisor_snapshot,
    dispatch_supervisor_tool, dispatch_user_decision, envelope, expected_client_disconnect,
    reconcile_aborted_supervisor_workers, reconcile_pending_goal_artifacts,
    reconcile_pending_supervisor_promotions, reconcile_startup_supervisor_promotions,
    reconcile_startup_supervisor_workers, request_mcp_credential, run_agent_readiness,
    unexpected_daemon_response_entry,
};

#[cfg(test)]
use dispatch::{
    AuthenticatedSupervisorCaller, PendingPromotionCandidate, PendingPromotionKind,
    best_effort_merged_pr_head, exact_merged_pr_head, finish_supervisor_promotion_reconciliation,
    goal_supervisor_caller, lock_agent_runtime, lock_supervisor_runtime, map_inbox_query_error,
    project_reported_pr, promotion_admission_matches, prompt_supervisor_retry,
    reconcile_supervisor_promotion, reconcile_supervisor_promotion_outcome,
    reconcile_supervisor_promotions, reconcile_supervisor_run_workers,
    record_supervisor_promotion_result, require_stable_supervisor_fence,
    require_supervisor_reservation_presence, reserve_goal_supervisor_run,
    resolve_goal_artifact_repository, safe_log_token, session_response_envelope,
    start_goal_supervisor_run, supervisor_caller_descriptor, supervisor_control_error,
    supervisor_control_unconfirmed, supervisor_error,
};

#[cfg(test)]
use agent_provisioning::{
    ClaudeSandboxPolicyError, SandboxLauncherPaths, SandboxPolicyInputs, agent_writable_roots,
    agy_arguments_for_integration, agy_plugin_arguments, agy_plugin_documents,
    claude_mcp_arguments, claude_prompt_arguments, claude_sandbox_launcher,
    claude_settings_arguments, claude_system_prompt_arguments, claude_writable_roots,
    codex_developer_instructions_arguments, codex_integration_arguments,
    codex_system_prompt_arguments, configured_environment, configured_mcp_tools,
    effective_role_instruction, git_common_dir, insert_root_git_environment, launch_environment,
    lexical_prefix_overlaps_path, materialize_agy_plugin, mcp_environment,
    mcp_environment_allowlist, prompt_scope, provider_gateway_environment,
    repair_codex_arg0_permissions, repair_codex_arg0_permissions_with_limit,
    root_agent_writable_roots, root_memory_store_root, sandbox_mode, session_git_common_dir,
    session_git_policy, shell_quote, toml_basic_string, validate_claude_sandbox_policy,
    validate_isolated_sandbox_root, validate_root_git_common_dir_policy,
};
use agent_provisioning::{
    DiscardJournal, RootClaudeProvisioner, RootCodexProvisioner,
    repair_agent_codex_arg0_permissions, resolve_sandbox_cache_dir,
};
use secure_path::validate_owned_directory;
use std::backtrace::Backtrace;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as FmtWrite;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
#[cfg(test)]
use std::panic::AssertUnwindSafe;
use std::panic::{self, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, LockResult, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use usagi_cli::cli::DaemonCommand as CliDaemonCommand;
use usagi_core::domain::AppInfo;
use usagi_core::domain::agent::mcp_tools::McpToolFamilies;
use usagi_core::domain::agent::prompt::{PromptScope, launch_system_prompt};
use usagi_core::domain::agent::{
    AgentIntegrationRevision, AgentProfileId, DaemonRestartAgent, DaemonRestartAgentPlan,
    DurableLaunchSnapshot, EnvironmentVariableName, aggregate_agent_status,
};
use usagi_core::domain::clock::LogicalClock;
use usagi_core::domain::clock::MonotonicClock;
use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
use usagi_core::domain::id::{
    AgentRuntimeRef, ConnectionId, SessionId, TerminalId, TerminalRef, WorkspaceId, WorktreeId,
};
use usagi_core::domain::session_lifecycle::AGENT_PHASE_HOOK_EVENTS;
use usagi_core::domain::settings::{AgentReadinessCommand, DefaultModel};
use usagi_core::infrastructure::bounded_process::{
    ChildObservation, ChildPolicy, observe, observe_with_environment,
};
use usagi_core::infrastructure::client::{
    ClientPolicy, DaemonClient, DeadlineConnection, DeadlineStream, IpcClient, PolicyClient,
    TerminalLaneBudget,
};
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonReady, DaemonRecordStore, InstanceLock, LivenessProbe,
    ProcessIdentitySource, RecordFile, ShutdownSignal, Sleeper, Terminator, WorkspaceFence,
    WorkspaceFenceOutcome,
};
use usagi_core::infrastructure::env_resolver::OpCli;
use usagi_core::infrastructure::error_log::ErrorLog;
use usagi_core::infrastructure::ipc::{
    BuildArtifactDecision, BuildIdentity, BuildRolloverTrigger, ClientWorkspace, Envelope,
    EnvelopeKind, ErrorCode, OperationId, ResponseOutcome, build_artifact_decision,
    build_rollover_trigger,
};
use usagi_core::infrastructure::ipc::{ClientError, DaemonRestartAgents};
use usagi_core::infrastructure::ipc::{DaemonRequest, DispatchToolAction, SupervisorToolAction};
use usagi_core::infrastructure::paths;
#[cfg(test)]
use usagi_core::infrastructure::persistence::json_file;
use usagi_core::infrastructure::store::dispatch::{DispatchStore, INBOX_PAGE_MAX, InboxCursor};
use usagi_core::infrastructure::store::issue::AmbiguousIssueNumber;
use usagi_core::infrastructure::store::pr_inventory::PrInventoryStore;
use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
use usagi_core::infrastructure::store::user_decision::UserDecisionStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::infrastructure::workspace_state;
use usagi_core::usecase::claude_sandbox::{self, SandboxMode};
use usagi_daemon::infrastructure::child_identity::UnixChildProbe;
use usagi_daemon::infrastructure::generation_registry::{
    CurrentLocatorFile, GenerationRegistryFile, read_registry_document,
};
use usagi_daemon::infrastructure::pty::PtyTerminal;
use usagi_daemon::infrastructure::resource_store::{AllocatorFile, ShardArchiveFiles};
use usagi_daemon::infrastructure::session_worktree::{SystemGit, SystemSessionWorktreeIo};
use usagi_daemon::infrastructure::unix_transport::{
    EndpointCleanup, EndpointLocator, SecureUnixListener, connect_generation, ensure_private_dir,
    ensure_private_dir_all, parent_pid, peer_pid, process_group, read_locator,
    retire_stale_current_preserving,
};
use usagi_daemon::presentation::{
    DaemonCommand as PresentationDaemonCommand, DaemonEnv, ServeRole,
};
use usagi_daemon::usecase::agent_ipc::{
    AGENT_RUNTIME_LIMIT, AgentAdmission, AgentReadinessPreflight, AgentRuntime, AgentTerminalActor,
    PromptMode, ResolvedAgentScope, ScopeResolveError, SessionScopeResolver, SharedTerminalOwner,
    TerminalOutcome,
};
use usagi_daemon::usecase::agy::AgyAdapter;
use usagi_daemon::usecase::authority::activation::{
    AuthorityClaim, claim_authority, release_authority,
};
use usagi_daemon::usecase::authority::admission::{AdmissionGate, AdmissionLease, LeaseClass};
use usagi_daemon::usecase::authority::collection::{Collection, collect_if_drained};
use usagi_daemon::usecase::authority::fence::{OwnedRuntime, classify_request};
use usagi_daemon::usecase::authority::handoff::{
    LocatorObservation, PublishedLocator, RecoveryOutcome,
};
use usagi_daemon::usecase::authority::pre_handshake::{
    PRE_HANDSHAKE_CONNECTION_LIMIT, PreHandshakeAdmission, PreHandshakePermit,
};
use usagi_daemon::usecase::authority::registry::{
    DEFAULT_GENERATION_LIMIT, GenerationRegistry, RegistryDocument,
};
use usagi_daemon::usecase::authority::rollover::{CurrentLocator, recover as recover_rollover};
use usagi_daemon::usecase::authority::routing::RoutingLedger;
use usagi_daemon::usecase::authority::standby::{
    ActiveOwner, StandbyCustody, StandbyProbe, admissible_active, evaluate_custody, prepare_standby,
};
use usagi_daemon::usecase::authority::workers::{ClientWorkers, ConnectionShutdown};
use usagi_daemon::usecase::claude::{
    ClaudeAdapter, ClaudeProvision, ClaudeProvisionFailure, ClaudeProvisioner,
    mcp_arguments as claude_product_mcp_arguments, scoped_settings_json,
};
use usagi_daemon::usecase::codex::{
    CodexAdapter, CodexProvision, CodexProvisionFailure, CodexProvisioner,
    mcp_arguments as codex_product_mcp_arguments,
};
use usagi_daemon::usecase::custody::{Custody, CustodyProbe, NodeIdentity};
use usagi_daemon::usecase::generation::{GenerationRole, ProcessIdentity, ProcessObservation};
use usagi_daemon::usecase::generic_terminal::{
    GenericPtySpawner, TerminalProfileResolver, TerminalStore, TerminalStoreSnapshot,
};
use usagi_daemon::usecase::metrics::{
    AgentConcurrencyGauge, MetricsBroker, MetricsObserver, MetricsSample,
};
use usagi_daemon::usecase::orchestration::AdapterRegistry;
use usagi_daemon::usecase::pr_inventory::{GhProcessPort, OutputPrProjector, RefreshWorker};
use usagi_daemon::usecase::pr_projection::{
    PrProjection, PrProjectionQueue, pr_projection_counters,
};
use usagi_daemon::usecase::replacement::{
    LiveResources, ResourceCensus, RetainedGenerationControl, RolloverRequester, SeamlessRefusal,
    TransitionMode, manual_operation_id, seamless_refusal,
};
use usagi_daemon::usecase::resources::allocator::{CapacityPolicy, ResourceAllocator};
use usagi_daemon::usecase::resources::durable::{
    IdentityAuthority, ShardedAgentStore, ShardedRuntimeState, ShardedTerminalStore, census,
    shipping_retention_limits,
};
use usagi_daemon::usecase::resources::fence::FencedPrInventory;
use usagi_daemon::usecase::resources::identity::{ChildIdentity, ChildProcessProbe, record_child};
use usagi_daemon::usecase::rollover_trigger;
use usagi_daemon::usecase::runtime::{
    OutputJournal, ProvisionContext, PtySpawner, SandboxLauncher, SpawnProvision,
    TerminateReapError,
};
use usagi_daemon::usecase::serve::{DaemonRecordPort, GenerationAuthority};
use usagi_daemon::usecase::serve_standby::{StandbyAuthority, StandbyEndpoint};
use usagi_daemon::usecase::session_runtime::{
    SessionRuntime, SessionRuntimeError, SharedSessionTeardown, WorktreeTeardown,
    perform_compensating_remove, perform_create, perform_delegated_create,
    perform_remove_with_merged_head,
};
use usagi_daemon::usecase::session_teardown::{
    PendingTeardown, TeardownEffect, TeardownJournal, TeardownSignal, drain_pending_teardowns,
};
use usagi_daemon::usecase::shutdown::{BackgroundWorker, ShutdownRequest};
use usagi_daemon::usecase::stop::{StaleCleanup, StaleDaemonCleanup};
use usagi_daemon::usecase::supervisor_runtime::{
    ArtifactVerification, ArtifactVerificationRequest, ArtifactVerificationStatus,
    ArtifactVerifier, DecisionWake, DecisionWaker, InitialTask, SupervisorRuntime,
    bounded_supervisor_query,
};
use usagi_daemon::usecase::tenant::{
    DEFAULT_TENANT_LIMIT, OpenedTenant, Tenant, TenantRegistry, TenantRuntimeOpener,
    WorkspaceFenceFactory,
};
use usagi_daemon::usecase::terminal::{
    Geometry, Output, PtyWriteError, PtyWriter, SpawnFailure, output_pipeline_counters,
};
use usagi_daemon::usecase::terminal_ipc::{
    GENERIC_TERMINAL_LIMIT, GenericTerminalRuntime, ResolvedTerminalScope,
    TerminalScopeResolveError, TerminalScopeResolver,
};
use usagi_daemon::usecase::terminal_profile::{LoginShellProfile, public_terminal_environment};

use crate::runtime::user_env::{self, UserEnvironment};

/// The daemon's configured-environment reader, shared by the Agent adapters and
/// the terminal profile resolver.
type SharedUserEnvironment = UserEnvironment<OpCli>;

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

/// The OS user name this daemon runs as, resolved from its effective UID once
/// per process.
///
/// The lookup is a passwd database call, so caching it is what keeps every PTY
/// launch from re-asking the platform — and what keeps the product from ever
/// shelling out to `id` on a launch path. The answer cannot change while the
/// process lives: a running process does not change its effective UID here.
fn resolved_os_user() -> Option<&'static str> {
    static RESOLVED: OnceLock<Option<String>> = OnceLock::new();
    RESOLVED
        .get_or_init(usagi_daemon::infrastructure::os_user::effective_user_name)
        .as_deref()
}

/// The children this process spawned and observed through the OS.
///
/// Verifiability cannot be recovered from a durable record — a stored token is
/// only bytes, and this build's predecessor stored a fixed string — so the proof
/// lives here, in the process that watched the child start. The PTY spawners write
/// it and the durable stores read it, which is what lets a shard resource be
/// `Running` at all. It deliberately does not survive a restart: a recovered
/// record is `identity_unknown`, exactly as the shipping reconcile reports it.
#[derive(Default)]
struct SpawnedChildren(Mutex<BTreeMap<u32, ChildIdentity>>);

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=an_observed_child_stays_provable_until_its_release_is_dropped
impl SpawnedChildren {
    /// Observe a freshly spawned child and record it as this process's own.
    ///
    /// A platform that cannot answer yields the explicitly unverifiable token
    /// instead of a fabricated one, so the record stays visible and fails closed.
    ///
    /// The recorded proof is handed back as a [`ChildRelease`], because a map
    /// that is only ever inserted into is a leak: a daemon that runs thousands of
    /// short-lived children would keep a growing table of dead pids, and the
    /// kernel reuses those numbers. The caller holds the token for exactly as
    /// long as the child may still have to be proven — until its exit is
    /// committed — and the proof is gone the moment the token is dropped.
    fn observe(
        self: &Arc<Self>,
        probe: &dyn ChildProcessProbe,
        pid: u32,
        fallback: &str,
    ) -> (ProcessIdentity, Option<ChildRelease>) {
        let Ok(identity) = record_child(probe, pid) else {
            return (
                ProcessIdentity {
                    pid,
                    start_identity: fallback.to_owned(),
                    process_group: pid,
                },
                None,
            );
        };
        let recorded = identity.to_process_identity();
        let mut release = None;
        if let Ok(mut observed) = self.0.lock() {
            observed.insert(pid, identity.clone());
            release = Some(ChildRelease {
                children: Arc::clone(self),
                identity,
            });
        }
        let recorded = recorded.unwrap_or_else(|_| ProcessIdentity {
            pid,
            start_identity: fallback.to_owned(),
            process_group: pid,
        });
        (recorded, release)
    }

    /// Release exactly the observation that was recorded, never a namesake.
    ///
    /// The kernel may hand the pid to a new process as soon as the old one is
    /// reaped, so removing by pid alone would delete the successor's proof and
    /// leave a live child unprovable. Only an entry that still answers with the
    /// recorded start identity and process group is removed; anything else
    /// already belongs to somebody else's child.
    fn release(&self, identity: &ChildIdentity) {
        if let Ok(mut observed) = self.0.lock()
            && observed.get(&identity.pid).is_some_and(|recorded| {
                is_same_child(recorded, &identity.start_identity, identity.process_group)
            })
        {
            observed.remove(&identity.pid);
        }
    }
}

/// The exact release token for one observed child.
///
/// It releases on drop so that every way a child's life can end — a committed
/// exit, a wait the platform could not read, an observation nobody is left to
/// receive — frees the proof without having to remember to.
struct ChildRelease {
    children: Arc<SpawnedChildren>,
    identity: ChildIdentity,
}

impl Drop for ChildRelease {
    fn drop(&mut self) {
        self.children.release(&self.identity);
    }
}

/// Whether a recorded observation still describes the same process. A pid alone
/// never answers that question, because the kernel reuses it.
fn is_same_child(recorded: &ChildIdentity, start_identity: &str, process_group: u32) -> bool {
    recorded.start_identity == start_identity && recorded.process_group == process_group
}

/// Logical time for the operation ledger, in whole seconds of wall clock.
///
/// The ledger only ever compares it against its own recorded seals, so a coarse
/// monotonic-enough reading is all its windows need.
struct SystemLogicalClock;

impl LogicalClock for SystemLogicalClock {
    fn now(&self) -> u64 {
        u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0)
    }
}

/// Binds this daemon generation to its own runtime shard and the shared allocator.
///
/// The pools keep the per-kind limits the coordinators enforce in memory, so the
/// allocator refuses exactly what a single process would have refused — except
/// that it also counts the generations that are still draining.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=restart_hydrates_file_snapshot_before_dispatch_admission_and_preserves_ledger
fn open_runtime_state(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    children: &Arc<SpawnedChildren>,
    terminal_limit: usize,
) -> std::io::Result<ShardedRuntimeState> {
    let state = ShardedRuntimeState::new(
        generation,
        GenerationRole::Active,
        ResourceAllocator::new(
            AllocatorFile::new(data_dir)?,
            CapacityPolicy::new(AGENT_RUNTIME_LIMIT, terminal_limit),
        ),
        Box::new(ShardArchiveFiles::new(data_dir)?),
        Box::new(ObservedChildren(Arc::clone(children))),
        Box::new(SystemLogicalClock),
    )?;
    Ok(match registered_generations(data_dir, generation) {
        // A registry this process cannot read proves nothing retired, so the
        // reclaim stays closed rather than guessing against a live generation.
        Some(registered) => state.with_registered_generations(registered),
        None => state,
    })
}

/// Reads this generation's shard and every retained one, migrating the legacy
/// whole-snapshot stores on the first start that finds them.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=restart_hydrates_file_snapshot_before_dispatch_admission_and_preserves_ledger
fn hydrate_runtime_state(
    state: &ShardedRuntimeState,
    what: &str,
) -> std::io::Result<usagi_daemon::usecase::resources::durable::HydratedState> {
    let hydrated = state.hydrate().map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid durable runtime state: {error}"),
        )
    })?;
    if let Some(migration) = &hydrated.migration {
        ErrorLog::record(&format!(
            "daemon startup migrated {} legacy runtime record(s) into {} owner shard(s); {} could not prove ownership",
            migration.marker.adopted,
            migration.marker.generations.len(),
            migration.marker.unknown
        ));
    }
    if hydrated.reclaimed != 0 {
        ErrorLog::record(&format!(
            "daemon startup reclaimed {} leaked {what} capacity claim(s) no retained generation accounted for",
            hydrated.reclaimed
        ));
    }
    if hydrated.interrupted != 0 {
        ErrorLog::record(&format!(
            "daemon startup reconciled {} {what}(s) as interrupted (identity_unknown)",
            hydrated.interrupted
        ));
    }
    Ok(hydrated)
}

/// Counts the live runtime this data directory holds, across every retained
/// generation.
///
/// It deliberately reads rather than reconciles: a lifecycle verb that is about
/// to refuse must not rewrite the state it is refusing to destroy. Absent
/// documents mean a daemon that has never launched anything, and unreadable
/// ones are an error — never "nothing is live".
struct DurableResourceCensus {
    data_dir: PathBuf,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=shipping_retention_limits_are_wired_into_root_composition
impl ResourceCensus for DurableResourceCensus {
    fn live(&self) -> std::io::Result<LiveResources> {
        let archive = ShardArchiveFiles::new(&self.data_dir)?;
        let live = census(&archive).map_err(std::io::Error::other)?;
        Ok(LiveResources {
            agents: live.agents,
            terminals: live.terminals,
        })
    }
}

/// How long a forced cold transition lets SIGTERM drain a generation before it
/// escalates.
///
/// A daemon that owns Agent runtimes and generic terminals closes and reaps
/// every PTY child before it exits, which routinely outlasts a few hundred
/// milliseconds. The window used to be five seconds of SIGTERM and nothing
/// else, so a busy daemon was reported as "did not stop" while it was still
/// draining normally — and the operator was left with no escape hatch at all.
const FORCED_SHUTDOWN_TERM_GRACE: Duration = Duration::from_secs(30);
/// How long the same transition waits after SIGKILL before reporting failure.
///
/// SIGKILL cannot be caught, so this only has to cover the kernel tearing the
/// process down. Surviving it means the pid is wedged in the kernel, which is a
/// host problem rather than something another retry can fix.
const FORCED_SHUTDOWN_KILL_GRACE: Duration = Duration::from_secs(5);
/// How often a forced cold transition re-reads the registry while waiting.
const FORCED_SHUTDOWN_POLL: Duration = Duration::from_millis(50);

/// Whether the operator explicitly gave up the live runtime a transition would
/// destroy.
const fn transition_mode(force: bool) -> TransitionMode {
    if force {
        TransitionMode::Cold
    } else {
        TransitionMode::Planned
    }
}

const AGENT_READINESS_TERMINATE_GRACE: Duration = Duration::from_millis(250);

fn bounded_readiness_command(
    program: &str,
    arguments: &[&str],
    environment: &[(String, String)],
    bounds: ReadinessBounds,
    terminate_grace: Duration,
) -> AgentReadiness {
    readiness_from_observation(&observe_with_environment(
        program,
        arguments,
        environment,
        ChildPolicy {
            timeout: bounds.timeout,
            terminate_grace,
            output_limit: bounds.output_limit,
        },
    ))
}

fn readiness_from_observation(observation: &ChildObservation) -> AgentReadiness {
    match observation {
        // Status commands need not print a version or other public detail.
        ChildObservation::Success(_) | ChildObservation::EmptyOutput => AgentReadiness::Ready,
        ChildObservation::SpawnFailed
        | ChildObservation::ExitFailure
        | ChildObservation::TimedOut
        | ChildObservation::OutputTooLarge
        | ChildObservation::InvalidOutput
        | ChildObservation::ObservationFailed => AgentReadiness::Unavailable,
    }
}

/// `path` itself, when it is a git repository the caller is standing at.
///
/// This is the only shape of bound declaration a daemon will *open* a workspace
/// for. Deliberately no walk up the ancestors: the nearest enclosing repository
/// is not the same thing as the workspace the caller meant. A dotfiles
/// repository at `$HOME` is an ordinary setup, and searching upwards would make
/// `usagi session create` in any plain directory below it fence `$HOME`, create
/// `~/.usagi/sessions/<name>` as a worktree of the caller's dotfiles, and open a
/// branch in them. Standing *at* a repository is an unambiguous statement about
/// which workspace is meant; standing anywhere underneath one is not.
///
/// A subdirectory still resolves to its workspace once that workspace is
/// adopted — that is [`TenantRegistry::owner_of`], and it is unaffected by this.
/// What this decides is only whether a *new* workspace may be opened.
///
/// A session worktree carries its own `.git` file and would otherwise answer as
/// its own workspace. It is not one: it belongs to the workspace that created
/// it, which must already be adopted for the worktree to exist.
fn adoptable_workspace_root(path: &Path) -> Option<PathBuf> {
    (path.is_absolute() && !is_session_worktree_path(path) && path.join(".git").exists())
        .then(|| path.to_path_buf())
}

/// The workspace a bound declaration may open implicitly.
///
/// A running daemon's handshake and a client's cold-start preflight share this
/// decision, so daemon liveness cannot change the meaning of the same cwd.
fn implicit_bound_workspace(daemon_dir: &Path, declared: &Path) -> Option<PathBuf> {
    workspace_state::owner(daemon_dir, declared)
        .ok()
        .flatten()
        .map(|known| known.root().to_path_buf())
        .or_else(|| adoptable_workspace_root(declared))
}

fn unopened_bound_workspace_refusal(
    declared: &Path,
    served: &[String],
) -> usagi_core::infrastructure::ipc::ProtocolError {
    usagi_core::infrastructure::ipc::workspace_refusal_serving(
        &format!(
            "this daemon has not opened {}; run this from a repository root \
             to open it, or open it explicitly with `usagi open {}`",
            paths::wire_workspace_root(declared),
            paths::wire_workspace_root(declared)
        ),
        served,
    )
}

/// Whether `path` is at or below a `\.usagi/sessions/<name>` worktree.
fn is_session_worktree_path(path: &Path) -> bool {
    let components: Vec<_> = path
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect();
    components
        .windows(2)
        .any(|pair| pair[0] == *".usagi" && pair[1] == *"sessions")
}

/// The workspace a connection acts on, once its handshake resolved one.
///
/// The handshake has already adopted or refused; this is the lookup of what it
/// settled on. A miss means the workspace was retired between the two steps, and
/// the connection is closed rather than served another workspace's state.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
fn connection_workspace(
    workspaces: &Workspaces,
    initial: &usagi_daemon::usecase::tenant::Tenant<SharedSessionRuntime>,
    declared: Option<&ClientWorkspace>,
) -> Option<ConnectionWorkspace> {
    let tenant = match declared {
        None | Some(ClientWorkspace::Unbound) => initial.clone(),
        Some(ClientWorkspace::Selected { root }) => {
            workspaces.workspace_at(&paths::canonical_workspace_root(root).ok()?)?
        }
        Some(ClientWorkspace::Bound { root }) => workspaces.owner_of_path(
            &paths::canonical_workspace_root(root).unwrap_or_else(|_| PathBuf::from(root)),
        )?,
    };
    Some(ConnectionWorkspace {
        tenant,
        workspaces: Arc::clone(workspaces),
    })
}

/// The workspace one connection acts on, plus the daemon's other workspaces.
///
/// A connection is bound to one workspace by its handshake, and the session
/// commands it issues belong to that workspace. Requests that *name* a workspace
/// — an Agent launch, a terminal scope — are resolved through the registry
/// instead, so the identity in the request decides which runtime answers it.
#[derive(Clone)]
struct ConnectionWorkspace {
    tenant: usagi_daemon::usecase::tenant::Tenant<SharedSessionRuntime>,
    workspaces: Workspaces,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
impl ConnectionWorkspace {
    /// The lifecycle runtime of the workspace this connection is bound to.
    fn sessions(&self) -> &SharedSessionRuntime {
        self.tenant.runtime()
    }

    /// A scope resolver that answers for whichever workspace a request names.
    fn scope_resolver(&self) -> SharedScopeResolver {
        SharedScopeResolver(Arc::clone(&self.workspaces))
    }
}

/// The #268 scope resolver, adapted to the Agent owner's product-neutral
/// `(workspace, session)` input by deriving the available session's worktree.
struct SharedScopeResolver(Workspaces);
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_dispatch_uses_the_trusted_root_before_and_after_session_creation
impl SessionScopeResolver for SharedScopeResolver {
    fn resolve_available_scope(
        &self,
        workspace: WorkspaceId,
        session: Option<SessionId>,
    ) -> Result<ResolvedAgentScope, ScopeResolveError> {
        // The request names its workspace, so the runtime that answers it is the
        // one holding that identity — not whichever workspace this daemon was
        // started in.
        let tenant = self
            .0
            .workspace(workspace)
            .ok_or(ScopeResolveError::Unavailable)?;
        let runtime = tenant
            .runtime()
            .lock()
            .map_err(|_| ScopeResolveError::Storage)?;
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

type SharedAgentRuntime = Arc<SharedAgentState>;
type SharedSupervisorRuntime = Arc<Mutex<SupervisorRuntime>>;

const PTY_OBSERVATION_QUEUE_ITEMS: usize = 64;

type SharedSessionRuntime = usagi_daemon::usecase::tenant::SharedSessionRuntime;

/// The workspaces this daemon holds, as the daemon-wide components see them.
type Workspaces = Arc<dyn usagi_daemon::usecase::tenant::WorkspaceRuntimes>;
type SharedTerminalRuntime = Arc<
    Mutex<
        GenericTerminalRuntime<
            TrustedLoginShell,
            ShardedTerminalStore,
            DaemonPty,
            SharedTerminalScopeResolver,
        >,
    >,
>;
/// The PR inventory projector, behind the generation fence that keeps it a single
/// writer ([`FencedPrInventory`]). Only the active generation reaches the
/// document, so a draining process's PTY observation cannot lose an update.
type SharedPrInventory = Arc<Mutex<OutputPrProjector<FencedPrInventory<PrInventoryStore>>>>;

/// How often the PR refresh worker claims due work.
///
/// This bounds how quickly a freshly detected PR gets its title and state, and
/// each tick claims at most [`PR_REFRESH_PER_TICK`] identities against a 60 s
/// freshness window. Now that the wait is edge-driven rather than a 10 ms poll,
/// the tick costs one wakeup, so there is no reason to lengthen it.
const PR_REFRESH_TICK: Duration = Duration::from_millis(250);
const PR_REFRESH_FRESHNESS_MS: u64 = 60_000;
const PR_REFRESH_PER_TICK: usize = 2;
/// `gh pr view` returns a compact JSON document. This cap leaves room for a
/// large check rollup while preventing a broken provider from retaining
/// unbounded diagnostics in either pipe.
const PR_PROVIDER_OUTPUT_LIMIT: usize = 256 * 1024;
const PR_PROVIDER_TERMINATE_GRACE: Duration = Duration::from_millis(100);
/// How often a serving daemon re-checks that it is still the authority for its
/// data directory. One second is short enough that an abandoned daemon exits
/// promptly and long enough that the two `stat`s are free.
const CUSTODY_TICK: Duration = Duration::from_secs(1);

/// How long the teardown worker waits for an admitted removal before deriving
/// the pending set again anyway. An admission wakes it immediately, so this only
/// bounds the retry of a teardown whose durable finalization failed.
const SESSION_TEARDOWN_TICK: Duration = Duration::from_secs(1);

/// How often the active daemon removes safe Git resources that are no longer
/// linked from a workspace lifecycle document. The manual command remains the
/// only path for protected candidates.
const ORPHAN_CLEANUP_TICK: Duration = Duration::from_mins(5);

/// How long the accept loop waits after an accept error that may have left the
/// connection queued. This is the error path only: an idle daemon parks on
/// descriptor readiness and never reaches it.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(10);

/// One absolute budget for reading and answering the complete first frame.
/// Established connections have their own policy and are deliberately not
/// subject to this deadline.
const PRE_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(2);

/// One absolute budget for transmitting a complete established response frame.
/// It is deliberately not an idle timeout: the clock starts only when the first
/// prefix byte is written and is reset after the complete payload is accepted.
const ESTABLISHED_RESPONSE_WRITE_DEADLINE_MS: u64 = 2_000;

/// Fallback when the process soft descriptor limit cannot be observed.
const CLIENT_CONNECTION_LIMIT_FALLBACK: usize = 32;
/// Established connections remain bounded even when the process has a very
/// large descriptor allowance: each one also owns a worker thread.
const CLIENT_CONNECTION_LIMIT_CEILING: usize = 256;
/// Descriptors reserved for PTYs, stores, wake pipes, listeners, and children.
const CLIENT_CONNECTION_RESERVED_FDS: u64 = 128;
/// Reader, writer, and retirement/shutdown descriptor retained per worker.
const CLIENT_CONNECTION_FDS: u64 = 3;
/// The smallest soft descriptor allowance that admits the bounded maximum of
/// established client workers while retaining the daemon's internal reserve.
const CLIENT_NOFILE_TARGET: u64 =
    CLIENT_CONNECTION_RESERVED_FDS + CLIENT_CONNECTION_FDS * CLIENT_CONNECTION_LIMIT_CEILING as u64;

/// How often the decision maintenance worker makes due expiries durable and
/// drains the resolved-decision outbox.
///
/// This bounds how long an already expired decision can still be read as
/// `Pending`. A tick that finds nothing due performs two small reads and no
/// write: expiry no longer takes the store lock or fsyncs unless something
/// actually changed.
const DECISION_MAINTENANCE_TICK: Duration = Duration::from_millis(250);
const SUPERVISOR_RECOVERY_TICK: Duration = Duration::from_secs(1);
/// Workflow progress is measured in Agent turns, so the resident lane sweeps
/// slowly: often enough that a finished review reaches the human in seconds,
/// rarely enough that an idle run costs one journal read per sweep.
const WORKFLOW_LANE_TICK: Duration = Duration::from_secs(10);
#[derive(Clone, Copy)]
struct GhProcess;

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=pr_snapshot_events_cover_success_scoped_and_lane_errors
impl GhProcessPort for GhProcess {
    type Error = std::io::Error;

    fn run(
        &mut self,
        program: &str,
        argv: &[String],
        timeout_ms: u64,
    ) -> Result<String, Self::Error> {
        let arguments = argv.iter().map(String::as_str).collect::<Vec<_>>();
        gh_process_result(observe(
            program,
            &arguments,
            ChildPolicy {
                timeout: Duration::from_millis(timeout_ms),
                terminate_grace: PR_PROVIDER_TERMINATE_GRACE,
                output_limit: PR_PROVIDER_OUTPUT_LIMIT,
            },
        ))
    }
}

fn gh_process_result(observation: ChildObservation) -> std::io::Result<String> {
    match observation {
        ChildObservation::Success(output) => Ok(output),
        ChildObservation::TimedOut => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "PR provider timed out",
        )),
        ChildObservation::SpawnFailed
        | ChildObservation::ExitFailure
        | ChildObservation::OutputTooLarge
        | ChildObservation::InvalidOutput
        | ChildObservation::EmptyOutput
        | ChildObservation::ObservationFailed => Err(std::io::Error::other("PR provider failed")),
    }
}

/// Supplies raw process-resource observations to the metrics authority.
struct ProcessResourceSampler {
    previous: Option<(Instant, u64)>,
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=dispatch_metrics_reads_the_process_resource_sample
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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=dispatch_metrics_reads_the_process_resource_sample
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

#[derive(Clone)]
struct PeerProcess {
    pid: u32,
    process_start_identity: Option<String>,
    lineage: Option<(u32, u32)>,
}

/// Disconnect wakeups are bounded to one item and coalesced. The worker derives
/// stale state by comparing each owner ledger with the bounded live census, so
/// no historical disconnect queue exists and producers never block.
fn connection_cleanup_channel() -> (ConnectionCleanup, ConnectionCleanupInbox) {
    let live = Arc::new(Mutex::new(BTreeMap::new()));
    let (wake, receiver) = mpsc::sync_channel(1);
    (
        ConnectionCleanup {
            live: Arc::clone(&live),
            wake,
        },
        ConnectionCleanupInbox {
            live,
            wake: receiver,
        },
    )
}

fn start_connection_cleanup_worker_with(
    disconnected: ConnectionCleanupInbox,
    mut cleanup: impl FnMut(&ConnectionCleanupInbox) + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-connection-cleanup".to_string())
        .spawn(move || {
            while disconnected.wake.recv().is_ok() {
                cleanup(&disconnected);
            }
        })
}

use super::bootstrap;
// Only the platform's own supervisor backend is linked in here. The other
// module keeps its pure half compiled so its tests run on every host, but it
// exposes no real IO to link against.
#[cfg(target_os = "macos")]
use super::launchd;
#[cfg(target_os = "linux")]
use super::systemd;

/// Durable runtime state a newly active generation is allowed to import.
///
/// A promoted standby must not adopt another generation's live PTYs. It does
/// need non-live Agent source records so an explicit restart can exact-resume
/// the provider conversation that the old owner stopped before W2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeHydration {
    All,
    AgentResumeHistory,
    #[cfg(test)]
    Empty,
}

/// Reap completed workers before deciding whether another accepted connection
/// can acquire daemon-owned descriptors and a thread.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=root_ipc_pre_handshake_cap_deadline_fairness_and_shutdown_are_bounded
fn client_connection_capacity_available(workers: &ClientWorkers, limit: usize) -> bool {
    let report = workers.reap_finished();
    if !report.is_clean() {
        ErrorLog::record(&format!(
            "daemon completed client worker reaped with failures: {report:?}"
        ));
    }
    workers.outstanding() < limit
}

#[derive(Default)]
struct CapacityRefusalLog {
    reported: bool,
}

/// Coalesces an unchanged periodic failure until the lane succeeds or its
/// diagnostic changes. A durable fault remains visible without growing the
/// daily log once per scheduler tick.
#[derive(Default)]
struct FailureTransitionLog {
    last: Option<String>,
}

impl FailureTransitionLog {
    fn changed(&mut self, failure: Option<String>) -> Option<String> {
        let Some(failure) = failure else {
            self.last = None;
            return None;
        };
        if self.last.as_deref() == Some(&failure) {
            return None;
        }
        self.last = Some(failure.clone());
        Some(failure)
    }
}

impl CapacityRefusalLog {
    /// Report one transition into saturation, not every reconnect accepted and
    /// immediately refused while all established slots remain occupied.
    fn should_record(&mut self, available: bool) -> bool {
        if available {
            self.reported = false;
            return false;
        }
        !std::mem::replace(&mut self.reported, true)
    }
}

/// Derive the established-worker bound from the process's actual descriptor
/// allowance instead of assuming the smallest commonly configured macOS soft
/// limit. The old fixed value of 32 was lower than two TUIs plus the supported
/// sixteen long-lived Agent MCP connections, so a healthy workspace eventually
/// refused every reconnect even while thousands of descriptors were available.
fn client_connection_limit_from_nofile(soft_limit: u64) -> usize {
    let descriptor_bound =
        soft_limit.saturating_sub(CLIENT_CONNECTION_RESERVED_FDS) / CLIENT_CONNECTION_FDS;
    usize::try_from(descriptor_bound)
        .unwrap_or(usize::MAX)
        .clamp(1, CLIENT_CONNECTION_LIMIT_CEILING)
}

fn preferred_client_nofile_soft_limit(soft_limit: u64, hard_limit: u64) -> u64 {
    let available = if hard_limit == libc::RLIM_INFINITY {
        CLIENT_NOFILE_TARGET
    } else {
        hard_limit
    };
    soft_limit.max(available.min(CLIENT_NOFILE_TARGET))
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=root_ipc_pre_handshake_cap_deadline_fairness_and_shutdown_are_bounded
fn client_connection_limit() -> usize {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: `limit` points to writable storage for one `rlimit`, and the
    // successful call initializes it before `assume_init`.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return CLIENT_CONNECTION_LIMIT_FALLBACK;
    }
    // SAFETY: the successful `getrlimit` above initialized the value.
    let mut limit = unsafe { limit.assume_init() };
    let preferred = preferred_client_nofile_soft_limit(limit.rlim_cur, limit.rlim_max);
    if preferred > limit.rlim_cur {
        let raised = libc::rlimit {
            rlim_cur: preferred,
            rlim_max: limit.rlim_max,
        };
        // SAFETY: `raised` is a valid rlimit value derived from the current
        // hard bound. A refused raise leaves the observed soft limit in force.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const raised) } == 0 {
            limit.rlim_cur = preferred;
        }
    }
    if limit.rlim_cur == libc::RLIM_INFINITY {
        CLIENT_CONNECTION_LIMIT_CEILING
    } else {
        client_connection_limit_from_nofile(limit.rlim_cur)
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=agent_ipc_e2e
fn bind_ipc_listener(
    data_dir: &Path,
) -> std::io::Result<(
    SecureUnixListener,
    usagi_core::infrastructure::ipc::DaemonGeneration,
)> {
    let generation = usagi_core::infrastructure::ipc::DaemonGeneration(
        usagi_core::domain::id::DaemonGeneration::new().as_str(),
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
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=pr_snapshot_events_cover_success_scoped_and_lane_errors
fn start_pr_refresh_worker(
    pr_inventory: SharedPrInventory,
    daemon_dir: PathBuf,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_pr_refresh_worker(
        pr_inventory,
        Some(daemon_dir),
        shutdown,
        GhProcess,
        SystemClock::new(),
        PR_REFRESH_TICK,
    )
}

/// Every managed session this data directory knows about, across every
/// workspace it has adopted — including the ones no longer held.
///
/// The daemon-wide registries (PR inventory, Agent runtime) are keyed by session
/// alone, so what they may keep cannot be the sessions of the workspaces this
/// daemon *currently* holds: a workspace given back by
/// [retirement](usagi_daemon::usecase::tenant::TenantRegistry::retire_idle) still
/// owns its sessions, and pruning against a set that lost them would delete the
/// user's own records for a workspace that is merely closed.
///
/// The durable lifecycle documents are therefore the authority, and they are
/// read directly: a workspace that is not adopted has no runtime to ask.
///
/// `None` when any of them cannot be read: pruning on a partial view is exactly
/// the deletion this guards against.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=production_session_remove_is_accepted_before_the_daemon_tears_the_worktree_down
fn known_sessions(daemon_dir: &Path) -> Option<std::collections::BTreeSet<SessionId>> {
    let mut known = std::collections::BTreeSet::new();
    for state in usagi_core::infrastructure::workspace_state::adopted(daemon_dir).ok()? {
        let Some(lifecycle) =
            usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore::new(state.dir())
                .load()
                .ok()?
        else {
            // A subtree whose root is recorded but whose document is not written
            // yet owns no sessions.
            continue;
        };
        known.extend(
            lifecycle
                .sessions
                .into_iter()
                .map(|session| session.session_id),
        );
    }
    Some(known)
}

/// Periodically applies only the non-force half of `clean` to every workspace
/// whose fence this active generation still holds.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=automatic_orphan_cleanup_ticks_until_shutdown
fn start_orphan_cleanup_worker(
    workspaces: &Workspaces,
    gate: AdmissionGate,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_orphan_cleanup_worker(
        AutomaticOrphanCleanup {
            workspaces: Arc::clone(workspaces),
            gate,
        },
        shutdown,
        ORPHAN_CLEANUP_TICK,
    )
}

impl<C> OrphanCleanupPass for C
where
    C: FnMut() + Send,
{
    fn run(&mut self) {
        self();
    }
}

/// Acquires the same active-control barrier as mutating IPC requests. A rollover
/// closes admission and waits for a pass already in flight; a draining
/// predecessor cannot start another pass.
fn active_cleanup_lease(gate: &AdmissionGate) -> Option<AdmissionLease> {
    gate.acquire(LeaseClass::ActiveControl).ok()
}

/// Starts the only production custody supervisor. A daemon is deliberately
/// detached from its launcher's process group, so nothing else reaps it when the
/// launcher dies abnormally; this worker makes the daemon reap itself as soon as
/// it stops being the authority for its data directory (see
/// [`usagi_daemon::usecase::custody`]).
/// How often idle workspaces are looked at.
const TENANT_RETIRE_TICK: Duration = Duration::from_secs(30);

/// How long a workspace must have nothing to do before it is given back.
///
/// Long enough that leaving a workspace and coming back does not churn the
/// fence; short enough that a workspace opened once in the morning is not still
/// owned in the afternoon, blocking a development-mode daemon from taking it.
const TENANT_IDLE_RETIREMENT: Duration = Duration::from_mins(10);

/// What this daemon can see of a workspace's remaining work.
///
/// Every observation fails closed: a runtime whose lock cannot be taken, or a
/// lifecycle document that cannot be read, keeps the workspace. Keeping one
/// costs a fence; releasing one that is still working would hand its worktrees
/// to a second owner.
struct DaemonWorkspaceActivity {
    terminal: SharedTerminalRuntime,
    agent: SharedAgentRuntime,
    supervisor: SharedSupervisorRuntime,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
impl usagi_daemon::usecase::tenant::WorkspaceActivity<SharedSessionRuntime>
    for DaemonWorkspaceActivity
{
    fn has_work(
        &self,
        workspace: usagi_core::domain::id::WorkspaceId,
        runtime: &SharedSessionRuntime,
    ) -> bool {
        let running_terminal = self.terminal.lock().map_or(true, |terminal| {
            terminal.retirement_blocker_count_in_workspace(workspace) != 0
        });
        let running_agent = self
            .agent
            .lock()
            .map_or(true, |agent| agent.has_running_agent(workspace));
        let running_supervisor = self.supervisor.lock().map_or(true, |supervisor| {
            supervisor
                .has_unfinished_workspace(workspace)
                .unwrap_or(true)
        });
        let unfinished = runtime.lock().map_or(true, |runtime| {
            runtime.has_unfinished_work().unwrap_or(true)
        });
        running_terminal || running_agent || running_supervisor || unfinished
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
fn start_tenant_retire_worker(
    tenants: Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
    activity: DaemonWorkspaceActivity,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_tenant_retire_worker(
        tenants,
        activity,
        shutdown,
        TENANT_RETIRE_TICK,
        TENANT_IDLE_RETIREMENT,
    )
}

/// How often the daemon ages exited terminal / Agent finals out of the aggregate
/// retention budget when nothing else drives collection.
///
/// Launch and exit already collect on the spot, so this only covers an idle
/// daemon, where the only things still moving are the age budget and the minimum
/// visibility TTL. Both are measured in minutes, so a 30 s tick is far finer than
/// the state it observes.
const RETENTION_GC_TICK: Duration = Duration::from_secs(30);
/// How quickly a generation notices that its last draining claim disappeared.
///
/// Resource exits already wake their own observers; this worker only bridges
/// the two durable documents (owner shard and global allocator) to process
/// lifetime, so a sub-second tick keeps retirement prompt without putting the
/// allocator lock on a hot path.
const DRAINING_COLLECTION_TICK: Duration = Duration::from_millis(250);

fn node_identity(metadata: &std::fs::Metadata) -> NodeIdentity {
    use std::os::unix::fs::MetadataExt;

    NodeIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_session_create_reaches_daemon_and_durable_lifecycle
fn open_session_runtime(
    repo_root: PathBuf,
    state_dir: &Path,
    data_home: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
) -> std::io::Result<SharedSessionRuntime> {
    SessionRuntime::open_at(
        repo_root,
        state_dir,
        data_home,
        generation,
        SystemGit,
        SystemSessionWorktreeIo,
    )
    .map(|runtime| Arc::new(Mutex::new(runtime)))
    .map_err(|error| std::io::Error::other(error.safe_message()))
}

/// Reads the root selected by the durable session store, rather than the
/// daemon process's startup directory. This keeps terminal profile resolution
/// aligned with restored managed-session state after a restart.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=production_dispatch_uses_the_trusted_root_before_and_after_session_creation
fn trusted_repository_root(sessions: &SharedSessionRuntime) -> std::io::Result<PathBuf> {
    sessions
        .lock()
        .map(|sessions| sessions.repository_root().to_path_buf())
        .map_err(|_| std::io::Error::other("session runtime is unavailable"))
}

struct FsRecordFile {
    path: PathBuf,
}

static DAEMON_RECORD_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_PRIVATE_LOCK_AFTER_CREATE: RefCell<Option<PathBuf>> = const {
        RefCell::new(None)
    };
    pub(super) static PRIVATE_LOCK_AFTER_FLOCK_BARRIER: RefCell<Option<PrivateLockAfterFlockBarrier>> = const {
        RefCell::new(None)
    };
}

#[cfg(test)]
fn fail_private_lock_after_create(path: &Path) {
    FAIL_PRIVATE_LOCK_AFTER_CREATE.with(|failpoint| {
        *failpoint.borrow_mut() = Some(path.to_path_buf());
    });
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=private_lock_refuses_unsafe_metadata_and_paths
fn private_lock_error(label: &str, detail: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("{label} {detail}"),
    )
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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_legacy_store_is_migrated_once_and_retired_in_place
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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_legacy_store_is_migrated_once_and_retired_in_place
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

#[cfg(target_os = "linux")]
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_generation_process_is_only_verified_by_its_exact_recorded_identity
pub(crate) fn process_start_identity(pid: u32) -> std::io::Result<String> {
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
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_generation_process_is_only_verified_by_its_exact_recorded_identity
pub(crate) fn process_start_identity(pid: u32) -> std::io::Result<String> {
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
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_generation_process_is_only_verified_by_its_exact_recorded_identity
pub(crate) fn signal_exact_process(
    record: &DaemonRecord,
    signal: libc::c_int,
) -> std::io::Result<()> {
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
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_generation_process_is_only_verified_by_its_exact_recorded_identity
pub(crate) fn signal_exact_process(
    record: &DaemonRecord,
    signal: libc::c_int,
) -> std::io::Result<()> {
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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_started_daemon_registers_its_generation_and_retires_it_on_stop
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
    #[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_started_daemon_registers_its_generation_and_retires_it_on_stop
    fn registry(&self) -> std::io::Result<GenerationRegistry> {
        Ok(GenerationRegistry::new(
            GenerationRegistryFile::new(self.data_dir)?,
            DEFAULT_GENERATION_LIMIT,
        ))
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_started_daemon_registers_its_generation_and_retires_it_on_stop
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

/// How often a standby re-reads its registry entry.
///
/// The same period as the active daemon's custody supervision, and for the same
/// reason: both are detached from their launcher, so a process that has lost its
/// authority has to reap itself. Only the invariant differs — the active watches
/// a lock and a record, a standby watches the one entry that names it.
const STANDBY_CUSTODY_TICK: Duration = Duration::from_secs(1);

/// How often a parked client worker re-checks whether its connection was
/// retired.
///
/// This is a backstop, not the ordinary path: `shutdown(2)` normally returns the
/// parked read immediately. It only has to be short enough that a lost wakeup
/// costs retirement one extra tick, and long enough that an idle connection is
/// not a busy loop.
const CLIENT_RETIREMENT_POLL: Duration = Duration::from_millis(250);

/// Waits for `fd` to become readable, or for `timeout` to elapse.
///
/// The wait deliberately lives in `poll(2)` rather than in the socket. A receive
/// timeout is not enough: once `shutdown(2)` has been applied to the socket,
/// Darwin can leave a `recv` that is *already* blocked parked without honouring
/// `SO_RCVTIMEO` either, which is the state that used to park retirement forever.
/// Deciding readability before entering `recv` means the worker is never blocked
/// on a socket that has nothing to give it.
///
/// Returns whether the descriptor is readable; `false` means the timeout expired.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=daemon_retirement_poll
fn readable_within(fd: std::os::fd::RawFd, timeout: Duration) -> std::io::Result<bool> {
    let mut pending = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    loop {
        // SAFETY: one initialised `pollfd` is passed with a length of one, and
        // the descriptor is owned by the caller for the duration of the call.
        let ready = unsafe { libc::poll(&raw mut pending, 1, millis) };
        if ready >= 0 {
            return Ok(ready > 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
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
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_generation_process_is_only_verified_by_its_exact_recorded_identity
fn own_process_identity(pid: u32) -> std::io::Result<ProcessIdentity> {
    Ok(ProcessIdentity {
        pid,
        start_identity: process_start_identity(pid)?,
        process_group: ChildProcessProbe::process_group(&UnixChildProbe, pid)?,
    })
}

const BROKER_PING: u8 = b'P';
const BROKER_START: u8 = b'S';
/// Retire this broker: reply, then close the endpoint and leave the loop.
///
/// A broker outlives the daemon on purpose, so nothing else ends it. Without
/// this request `usagi daemon stop` leaves a usagi process running that the
/// operator has no command to stop, and one accumulates per workspace and per
/// executable path.
const BROKER_STOP: u8 = b'X';
const BROKER_OK: u8 = b'O';
const BROKER_READINESS_ATTEMPTS: u32 = 100;
const BROKER_IO_TIMEOUT: Duration = Duration::from_secs(1);
const BROKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a broker stays up with no request and no daemon to serve.
///
/// The broker exists to cold-start a daemon for a client that cannot spawn one
/// itself, so it must outlive the daemon it started. It must not outlive the
/// *use* of the workspace: a build that ran once, a test binary, a checkout that
/// was deleted all leave a broker that would otherwise never exit.
///
/// The wait is only charged while no daemon is reachable. A running daemon means
/// the broker's job — being there when that daemon dies — is still pending, so
/// an idle hour next to a live daemon is not idleness.
const BROKER_IDLE_TIMEOUT: Duration = Duration::from_hours(1);
/// How often the idle watch re-checks. Coarse on purpose: it costs a connect
/// attempt against the daemon endpoint each time.
const BROKER_IDLE_POLL: Duration = Duration::from_secs(60);

/// Resolves the workspace a client may use to cold-start a daemon before any
/// lifecycle child, workspace fence, or project-local `.usagi` path exists.
fn cold_start_workspace(
    daemon_dir: &Path,
    workspace: &ClientWorkspace,
    opened: Option<&Path>,
    ambient_cwd: Option<&Path>,
) -> Result<PathBuf, usagi_core::infrastructure::ipc::ProtocolError> {
    if let Some(opened) = opened {
        return paths::canonical_workspace_root(opened).map_err(|_| {
            usagi_core::infrastructure::ipc::workspace_refusal(
                "the selected workspace does not resolve on this machine",
                &paths::wire_workspace_root(opened),
            )
        });
    }
    match workspace {
        ClientWorkspace::Selected { root } => TenantWorkspaces::canonical(root),
        ClientWorkspace::Bound { root } => {
            // Match the running resolver: a teardown may already have removed
            // an Agent worktree, but its declared spelling can still belong to
            // a durably adopted workspace.
            let declared =
                paths::canonical_workspace_root(root).unwrap_or_else(|_| PathBuf::from(root));
            implicit_bound_workspace(daemon_dir, &declared)
                .ok_or_else(|| unopened_bound_workspace_refusal(&declared, &[]))
        }
        ClientWorkspace::Unbound => ambient_cwd
            .ok_or_else(|| {
                usagi_core::infrastructure::ipc::workspace_refusal_serving(
                    "a cold start requires a resolvable working directory",
                    &[],
                )
            })
            .and_then(|cwd| {
                let declared = TenantWorkspaces::canonical(&paths::wire_workspace_root(cwd))?;
                implicit_bound_workspace(daemon_dir, &declared)
                    .ok_or_else(|| unopened_bound_workspace_refusal(&declared, &[]))
            }),
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=bootstrap_broker_accepts_only_ping_start_and_stop
fn reap_child(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Whether a broker that has been idle for `idle_for` may retire now.
///
/// A live daemon keeps the broker alive however long it has been quiet: the
/// broker's whole purpose is to already exist when that daemon dies, and a
/// sandboxed client cannot spawn a replacement for it.
const fn broker_may_retire(idle_for: Duration, timeout: Duration, daemon_live: bool) -> bool {
    !daemon_live && idle_for.as_secs() >= timeout.as_secs()
}

const PENDING_DAEMON_AGENT_RESTART_FILE: &str = "agent-restart.json";
const PENDING_DAEMON_AGENT_RESTART_SCHEMA: u16 = 1;
const PENDING_DAEMON_AGENT_RESTART_MAX_BYTES: usize = 256 * 1024;
const PENDING_DAEMON_AGENT_RESTART_TICK: Duration = Duration::from_millis(250);

fn pending_daemon_agent_restart_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join("daemon")
        .join(PENDING_DAEMON_AGENT_RESTART_FILE)
}

/// The daemon's own desktop notice for a workflow that needs a human.
///
/// The TUI notifies about decisions it observes, but a workflow reaches
/// `Needs attention` or `PR ready` whether or not anyone has usagi open — which
/// is the whole point of the resident lane. The daemon runs as the same user, so
/// it raises the notice itself and the moment survives a closed TUI.
struct PlatformWorkflowNotifier {
    reaper: crate::runtime::platform_child_reaper::PlatformChildReaper,
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=platform_child_reaper_reaps_short_helpers_around_a_long_lived_child
impl workflow::AttentionNotifier for PlatformWorkflowNotifier {
    fn notify(&self, title: &str, body: &str) {
        let mut command = if cfg!(target_os = "macos") {
            let mut command = std::process::Command::new("osascript");
            command
                .arg("-e")
                .arg("on run argv\n display notification (item 2 of argv) with title (item 1 of argv)\nend run")
                .arg("--")
                .arg(title)
                .arg(body);
            command
        } else if cfg!(target_os = "linux") {
            let mut command = std::process::Command::new("notify-send");
            // `--` first: a goal line that starts with `-` is text, not a flag.
            command
                .arg("--app-name=usagi")
                .arg("--")
                .arg(title)
                .arg(body);
            command
        } else {
            return;
        };
        let _ = self.reaper.spawn(&mut command);
    }
}

/// Starts the resident lane that carries stored workflow runs forward without a
/// client connection.
///
/// Reconcile, queued-instruction delivery and PR verification used to run only
/// inside a Workflow request, which made progress a property of what the user
/// happened to be looking at. This lane owns that progress instead; the request
/// path keeps the same pass so an open tab still answers with fresh state.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=the_resident_workflow_lane_advances_a_run_without_any_client_request
fn start_workflow_lane(
    agent: SharedAgentRuntime,
    pr_inventory: SharedPrInventory,
    workspaces: Workspaces,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let sweeping = Arc::clone(&shutdown);
    let mut failures = FailureTransitionLog::default();
    let notifier = PlatformWorkflowNotifier {
        reaper: crate::runtime::platform_child_reaper::PlatformChildReaper::default(),
    };
    spawn_workflow_lane(
        Box::new(move || {
            let scope = SharedScopeResolver(Arc::clone(&workspaces));
            let failure = workflow::sweep(&agent, &pr_inventory, &scope, &notifier, &|| {
                sweeping.is_requested()
            })
            .err()
            .map(|error| format!("workflow lane sweep deferred: {}", error.message));
            if let Some(entry) = failures.changed(failure) {
                ErrorLog::record(&entry);
            }
        }),
        shutdown,
        tick,
    )
}

/// The lane loop, with the sweep injected so a test can drive it without a
/// daemon, a PTY, or a store.
fn spawn_workflow_lane(
    mut sweep: Box<dyn FnMut() + Send>,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-workflow-lane".to_owned())
        .spawn(move || {
            let worker_health = shutdown.monitor_background_worker(BackgroundWorker::WorkflowLane);
            while !shutdown.is_requested() {
                sweep();
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
            worker_health.finish_planned();
        })
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=artifact_verification_preparation_captures_only_the_exact_completed_dispatch
fn start_supervisor_recovery(
    supervisor: SharedSupervisorRuntime,
    agent: SharedAgentRuntime,
    workspaces: Workspaces,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-supervisor-recovery".to_owned())
        .spawn(move || {
            let worker_health =
                shutdown.monitor_background_worker(BackgroundWorker::SupervisorRecovery);
            let mut promotion_log = FailureTransitionLog::default();
            let mut worker_log = FailureTransitionLog::default();
            let mut artifact_log = FailureTransitionLog::default();
            let mut state_log = FailureTransitionLog::default();
            while !shutdown.is_requested() {
                let now = chrono::Utc::now();
                let failure = reconcile_pending_supervisor_promotions(&supervisor, &agent)
                    .err()
                    .map(|error| format!("supervisor promotion reconciliation deferred: {error}"));
                if let Some(entry) = promotion_log.changed(failure) {
                    ErrorLog::record(&entry);
                }
                let failure = reconcile_aborted_supervisor_workers(&supervisor, &agent)
                    .err()
                    .map(|error| {
                        format!("supervisor worker termination reconciliation deferred: {error}")
                    });
                if let Some(entry) = worker_log.changed(failure) {
                    ErrorLog::record(&entry);
                }
                let failure = reconcile_pending_goal_artifacts(&supervisor, &workspaces, now)
                    .err()
                    .map(|error| {
                        format!("Goal artifact verification reconciliation deferred: {error}")
                    });
                if let Some(entry) = artifact_log.changed(failure) {
                    ErrorLog::record(&entry);
                }
                let failure = supervisor.lock().map_or_else(
                    |_| {
                        Some("supervisor state reconciliation deferred: runtime unavailable".into())
                    },
                    |runtime| {
                        runtime
                            .tick_all(now, &mut AgentDecisionWaker { agent: &agent })
                            .err()
                            .map(|error| {
                                format!("supervisor state reconciliation deferred: {error}")
                            })
                    },
                );
                if let Some(entry) = state_log.changed(failure) {
                    ErrorLog::record(&entry);
                }
                if shutdown.wait_for_tick(SUPERVISOR_RECOVERY_TICK) {
                    break;
                }
            }
            worker_health.finish_planned();
        })
}

struct IpcRolloverRequester<'a> {
    data_dir: &'a Path,
    launcher: &'a ServeLauncher,
    restart_agents: Option<bool>,
}

/// Planned rollover uses the same slow-but-healthy startup allowance as a cold
/// daemon start. Promotion hydrates the full runtime after the durable handoff,
/// so registry commit alone is not readiness.
const ROLLOVER_STARTUP_WINDOW: Duration = Duration::from_secs(30);
const ROLLOVER_PROBE_BUDGET_MS: u64 = 250;

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=explicit_artifact_replacement_runs_under_one_coalesced_operation
impl IpcRolloverRequester<'_> {
    fn prepare_agent_restart(&self) -> std::io::Result<Option<DaemonRestartAgentPlan>> {
        let Some(force) = self.restart_agents else {
            return Ok(None);
        };
        let mut client = existing_policy_client(ClientPolicy::cli(), ClientWorkspace::Unbound)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let reply = client
            .request(DaemonRequest::PlanDaemonRestartAgents {
                expected: current_agent_integrations(),
                force,
            })
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let body = match reply {
            usagi_core::infrastructure::ipc::DaemonReply::Ok(body)
            | usagi_core::infrastructure::ipc::DaemonReply::Accepted { body, .. } => body,
        };
        serde_json::from_value(body)
            .map(Some)
            .map_err(|_| std::io::Error::other("daemon returned an invalid Agent restart plan"))
    }

    fn committed(&self, operation: &OperationId, standby: &DaemonRecord) -> bool {
        read_registry_document(self.data_dir)
            .ok()
            .flatten()
            .is_some_and(|document| {
                let operation_committed = document.completed_operation.as_ref() == Some(operation)
                    || document.handoff.as_ref().is_some_and(|handoff| {
                        handoff.operation == *operation
                            && handoff.phase
                                == usagi_daemon::usecase::authority::registry::HandoffPhase::Committed
                    });
                operation_committed
                    && document.current.is_some_and(|current| {
                        document.generations.iter().any(|entry| {
                            entry.generation == current
                                && entry.process.pid == standby.pid
                                && standby.process_start_identity.as_deref()
                                    == Some(entry.process.start_identity.as_str())
                                && entry.role == GenerationRole::Active
                        })
                    })
            })
    }

    fn stop_standby(&self, mut standby: LaunchedStandby) {
        let retired = GenerationRegistryFile::new(self.data_dir)
            .map(|file| GenerationRegistry::new(file, DEFAULT_GENERATION_LIMIT))
            .and_then(|registry| {
                registry
                    .update(|document| {
                        let generation = document
                            .generations
                            .iter()
                            .find(|entry| {
                                entry.role == GenerationRole::Standby
                                    && entry.process.pid == standby.record.pid
                                    && standby.record.process_start_identity.as_deref()
                                        == Some(entry.process.start_identity.as_str())
                            })
                            .map(|entry| entry.generation);
                        generation.map_or(Ok(()), |generation| {
                            document.transition(generation, GenerationRole::Retired)
                        })
                    })
                    .map(|_| ())
                    .map_err(std::io::Error::other)
            });
        if let Err(error) = retired {
            ErrorLog::record(&format!(
                "failed standby registry retirement deferred: {error}"
            ));
        }
        if standby
            .child
            .try_wait()
            .is_ok_and(|status| status.is_some())
        {
            return;
        }
        // Signal only the identity captured while the spawned Child still held
        // custody of its PID. Re-reading identity here could authenticate an
        // unrelated process if a failed standby had exited and the PID was
        // reused before cleanup.
        let _ = Terminator::terminate(&SigtermTerminator, &standby.record);
        reap_child(standby.child);
    }

    fn wait_until_verified(
        &self,
        standby: &DaemonRecord,
        deadline: Instant,
    ) -> std::io::Result<()> {
        loop {
            if read_registry_document(self.data_dir)
                .ok()
                .flatten()
                .is_some_and(|document| {
                    document.generations.iter().any(|entry| {
                        entry.process.pid == standby.pid
                            && standby.process_start_identity.as_deref()
                                == Some(entry.process.start_identity.as_str())
                            && entry.role == GenerationRole::Standby
                            && entry.is_build_verified()
                    })
                })
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            RealSleeper.sleep();
        }
        Err(std::io::Error::other(
            "standby did not reach verified readiness within 30 seconds",
        ))
    }

    fn wait_until_committed(
        &self,
        operation: &OperationId,
        standby: &DaemonRecord,
        deadline: Instant,
    ) -> bool {
        loop {
            if self.committed(operation, standby) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            RealSleeper.sleep();
        }
    }

    /// A committed successor is not ready until its active runtime answers a
    /// harmless inventory request. The registry and locator change before the
    /// standby custody loop finishes promotion, and returning in that window
    /// exposes `generation_rolled_over` to the command immediately following a
    /// successful restart.
    fn successor_is_serving(&self, standby: &DaemonRecord, operation: &OperationId) -> bool {
        use usagi_core::infrastructure::ipc::{DaemonReply, TenantAction};

        let Some(generation) = read_registry_document(self.data_dir)
            .ok()
            .flatten()
            .and_then(|document| {
                let current = document.current?;
                document
                    .generations
                    .iter()
                    .find(|entry| {
                        entry.generation == current
                            && entry.process.pid == standby.pid
                            && standby.process_start_identity.as_deref()
                                == Some(entry.process.start_identity.as_str())
                            && entry.role == GenerationRole::Active
                    })
                    .map(|entry| entry.generation)
            })
        else {
            return false;
        };
        let Ok(locator) = read_locator(&self.data_dir.join("daemon")) else {
            return false;
        };
        if locator.generation.0 != generation.as_str() {
            return false;
        }
        let Ok(mut client) = connect_deadline_client(
            self.data_dir,
            ClientPolicy::cli(),
            current_build(),
            ClientWorkspace::Unbound,
            SystemClock::new(),
            ROLLOVER_PROBE_BUDGET_MS,
        ) else {
            return false;
        };
        if client.daemon_generation().0 != generation.as_str() {
            return false;
        }
        if read_pending_daemon_agent_restart(self.data_dir)
            .ok()
            .flatten()
            .is_some_and(|pending| pending.operation_id == operation.0)
        {
            return false;
        }
        matches!(
            client.request(DaemonRequest::Tenant {
                action: TenantAction::Inventory,
                root: None,
                force: false,
            }),
            Ok(DaemonReply::Ok(_))
        )
    }

    fn wait_until_serving(
        &self,
        standby: &DaemonRecord,
        operation: &OperationId,
    ) -> std::io::Result<()> {
        // Promotion hydrates the active runtime only after W2. Give that second
        // healthy-but-slow stage its own complete readiness budget instead of
        // inheriting whatever standby verification happened to leave behind.
        let deadline = Instant::now() + ROLLOVER_STARTUP_WINDOW;
        loop {
            if self.successor_is_serving(standby, operation) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            RealSleeper.sleep();
        }
        let reason = if read_pending_daemon_agent_restart(self.data_dir)
            .ok()
            .flatten()
            .is_some_and(|pending| pending.operation_id == operation.0)
        {
            "successor is serving, but durable Agent restart recovery did not complete within 30 seconds"
        } else {
            "successor committed authority but did not begin serving within 30 seconds"
        };
        Err(std::io::Error::other(reason))
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=explicit_artifact_replacement_runs_under_one_coalesced_operation
impl RolloverRequester for IpcRolloverRequester<'_> {
    fn rollover(&self, operation: &OperationId) -> std::io::Result<String> {
        let agent_plan = self.prepare_agent_restart()?;
        let agent_workspace = agent_plan
            .as_ref()
            .map(|plan| planned_agent_workspace_root(self.data_dir, plan))
            .transpose()?
            .flatten();
        let deadline = Instant::now() + ROLLOVER_STARTUP_WINDOW;
        let standby = self.launcher.launch_standby(agent_workspace.as_deref())?;
        let standby_record = standby.record.clone();
        if let Err(error) = self.wait_until_verified(&standby_record, deadline) {
            self.stop_standby(standby);
            return Err(error);
        }
        // Rollover is a machine-wide lifecycle request. Binding this control
        // connection to the command's cwd would make an otherwise ready
        // successor impossible to commit when `daemon restart` is run outside
        // an adopted workspace.
        let result = existing_policy_client(ClientPolicy::cli(), ClientWorkspace::Unbound)
            .and_then(|mut client| {
                client.request(DaemonRequest::Rollover {
                    operation_id: operation.0.clone(),
                    restart_agents: agent_plan.as_ref().map(|plan| DaemonRestartAgents {
                        expected: current_agent_integrations(),
                        runtimes: plan
                            .agents
                            .iter()
                            .map(|agent| agent.runtime.clone())
                            .collect(),
                        force: self.restart_agents.unwrap_or(false),
                    }),
                })
            });
        let mut committed = self.committed(operation, &standby_record);
        if !committed
            && result
                .as_ref()
                .is_err_and(usagi_core::infrastructure::ipc::ClientError::is_transport_failure)
        {
            // A transport failure cannot distinguish a request that never
            // arrived from a reply lost while W2 was becoming observable.
            committed = self.wait_until_committed(operation, &standby_record, deadline);
        }
        if !committed {
            self.stop_standby(standby);
            let failure = match result {
                Ok(_) => "rollover returned before its authority commit".to_owned(),
                Err(error) => error.to_string(),
            };
            return Err(std::io::Error::other(failure));
        }
        // A lost ACK after commit is a success once the committed successor is
        // serving. Never roll back an observable handoff because the response
        // frame was lost.
        reap_child(standby.child);
        self.wait_until_serving(&standby_record, operation)?;
        Ok(format!(
            "daemon authority handed off (operation {}), restarted {} Agent(s)",
            operation.0,
            agent_plan.as_ref().map_or(0, |plan| plan.agents.len())
        ))
    }
}

struct RealSleeper;
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=reports_a_timeout_when_started_daemon_never_becomes_ready
impl Sleeper for RealSleeper {
    fn sleep(&self) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// How long a `serve` start waits for a departing owner to release a workspace.
const WORKSPACE_FENCE_PATIENCE: Duration = Duration::from_secs(2);

/// How long an adoption waits. It runs inside the client's pre-handshake
/// deadline, so a contended workspace is reported rather than waited out.
const WORKSPACE_ADOPTION_PATIENCE: Duration = Duration::from_millis(200);

/// Replace the fence node's contents with this owner's pid line.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=workspace_fence_refuses_when_the_owner_hint_is_unreadable
fn write_owner_hint(file: &std::fs::File, pid: u32) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};
    file.set_len(0)?;
    let mut file = file;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(format!("{pid}\n").as_bytes())?;
    file.flush()
}

/// Read the owner pid published by the daemon currently holding the fence.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=workspace_fence_refuses_when_the_owner_hint_is_unreadable
fn read_owner_hint(file: &std::fs::File) -> Option<u32> {
    use std::io::Read;
    // The hint is one short decimal line; a longer node is not ours to trust.
    let mut contents = String::new();
    file.take(64).read_to_string(&mut contents).ok()?;
    contents.trim().parse().ok()
}

/// `usagi daemon` の実行時資源を組み立てて daemon presentation へ渡す。
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=daemon_lifecycle_recovers_a_crash_record_whose_pid_was_reused
pub(crate) fn run(
    out: &mut dyn Write,
    command: CliDaemonCommand,
    info: &AppInfo,
    operation: Option<usagi_core::infrastructure::ipc::OperationId>,
) -> std::io::Result<()> {
    run_with_lifecycle_custody(out, command, info, operation, None)
}

/// Resolve and securely initialize the selected per-user data directory before
/// any global store can become its first writer.
///
/// Config intentionally runs without starting the daemon, so its settings
/// adapter cannot rely on bootstrap lock acquisition to establish the private
/// directory invariant first.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=config_first_boot_with_restrictive_umask_preserves_ordinary_daemon_bootstrap
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
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_panicked_background_worker_reports_danger_and_requests_shutdown
fn install_panic_logger() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        ErrorLog::record(&format_panic(info));
        previous(info);
    }));
}
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_panicked_background_worker_reports_danger_and_requests_shutdown
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
/// The service supervisor this build provisions, named in the command's output.
#[cfg(target_os = "macos")]
const SERVICE_SUPERVISOR: &str = "launchd";
/// The service supervisor this build provisions, named in the command's output.
#[cfg(target_os = "linux")]
const SERVICE_SUPERVISOR: &str = "systemd";
/// The service supervisor this build provisions, named in the command's output.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const SERVICE_SUPERVISOR: &str = "no";

/// Provision the platform's supervisor for the foreground `daemon serve`.
///
/// macOS uses a `LaunchAgent`, Linux a systemd **user** unit. Both receive the
/// [`paths::DataHome`] pair so the supervised daemon lands on the directory this
/// process selected, and `workspace` so it binds the workspace this process
/// resolved instead of the supervisor's default directory. Other platforms have
/// no supported supervisor; the detached `start` path and client bootstrap keep
/// working there.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=daemon_lifecycle_recovers_a_crash_record_whose_pid_was_reused
fn install_service(
    executable: &std::path::Path,
    data_home: &paths::DataHome,
    workspace: &std::path::Path,
) -> std::io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        launchd::install(executable, data_home, workspace)
    }
    #[cfg(target_os = "linux")]
    {
        systemd::install(executable, data_home, workspace)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (executable, data_home, workspace);
        Err(unsupported_service())
    }
}

/// Remove the platform's supervisor definition installed by [`install_service`].
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=daemon_lifecycle_recovers_a_crash_record_whose_pid_was_reused
fn uninstall_service() -> std::io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        launchd::uninstall()
    }
    #[cfg(target_os = "linux")]
    {
        systemd::uninstall()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(unsupported_service())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn unsupported_service() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "service supervision is only supported on macOS (launchd) and Linux (systemd)",
    )
}

#[allow(clippy::too_many_lines)] // Composition wires the closed lifecycle verbs and their IO ports.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=daemon_lifecycle_recovers_a_crash_record_whose_pid_was_reused
fn run_inner(
    out: &mut dyn Write,
    command: CliDaemonCommand,
    info: &AppInfo,
    operation: Option<usagi_core::infrastructure::ipc::OperationId>,
    lifecycle_custody_held: bool,
) -> std::io::Result<()> {
    if let Some(result) = run_broker_lifecycle_command(&command) {
        return result;
    }
    let data_dir = prepare_private_data_dir()?;
    let daemon_dir = data_dir.join("daemon");
    let restart_agents = match &command {
        CliDaemonCommand::Restart {
            restart_agents: true,
            force,
        } => Some(*force),
        _ => None,
    };
    // Stop and replacement observe and mutate one machine-wide daemon
    // lifecycle. Sharing this lifecycle lock with managed update makes that
    // observation linearizable: a stop either completes before update sees
    // absence, or runs after update has finished, never in the gap between its
    // owner observation and replacement plan. This is distinct from
    // `bootstrap.lock`: a client holding the cold-start lock may spawn the
    // lifecycle subprocess that reaches this point.
    let _lifecycle_custody = if !lifecycle_custody_held
        && matches!(
            command,
            CliDaemonCommand::Stop { .. } | CliDaemonCommand::Restart { .. }
        ) {
        Some(acquire_lifecycle_lock_io_within(
            &data_dir,
            PrivateLockWait::LIFECYCLE,
        )?)
    } else {
        None
    };
    if let Some(pending) = read_pending_daemon_agent_restart(&data_dir)? {
        match &command {
            CliDaemonCommand::Stop { force: true } => {
                // The explicit cold stop gives up every recoverable runtime as
                // well as every live one; do not revive this plan on a later
                // start after the operator made that choice.
                clear_pending_daemon_agent_restart(&data_dir, &pending.operation_id)?;
            }
            CliDaemonCommand::Stop { force: false } | CliDaemonCommand::Restart { .. } => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "an earlier daemon Agent restart is still recovering; wait for it to finish",
                ));
            }
            _ => {}
        }
    }
    if let CliDaemonCommand::Retire { path, force } = command {
        let root = paths::canonical_workspace_root(&path)
            .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
        let root = paths::wire_workspace_root(&root);
        let mut client = existing_policy_client(ClientPolicy::cli(), ClientWorkspace::Unbound)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        client
            .request(DaemonRequest::Tenant {
                action: usagi_core::infrastructure::ipc::TenantAction::Retire,
                root: Some(root.clone()),
                force,
            })
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        return writeln!(
            out,
            "{}: retired workspace tenant ({root})",
            info.describe()
        );
    }
    let command = match command {
        CliDaemonCommand::InstallService => {
            // The supervised service must resolve the same data home *and* the
            // same workspace as this process. Both launchd and systemd start it
            // from their own environment and working directory, so both travel in
            // the service definition rather than being re-derived there.
            //
            // The workspace matters as much as the data home: a daemon binds the
            // workspace its startup directory names, and a supervisor's default
            // directory is the user's home (systemd user units) or `/` (launchd) —
            // neither of which is the workspace anyone meant. When the workspace
            // resolves to the home directory, both logical fences name the same
            // inode under the default `~/.usagi` data home and deliberately share
            // one held descriptor. That makes the start safe, but it still binds
            // the daemon to the wrong workspace. `lifecycle_command` already pins
            // the directory for a cold start from a client; a supervised start
            // needs the same pin.
            let data_home = paths::DataHome::from_selected(&data_dir, paths::runtime_mode());
            let workspace = bound_workspace_root(&daemon_dir, &std::env::current_dir()?)?;
            let path = install_service(&std::env::current_exe()?, &data_home, &workspace)?;
            return writeln!(
                out,
                "{}: {} service installed ({})",
                info.describe(),
                SERVICE_SUPERVISOR,
                path.display()
            );
        }
        CliDaemonCommand::UninstallService => {
            let path = uninstall_service()?;
            return writeln!(
                out,
                "{}: {} service uninstalled ({})",
                info.describe(),
                SERVICE_SUPERVISOR,
                path.display()
            );
        }
        // The role is fixed by argv before anything is locked, bound, or
        // written: a process does not discover which role it is partway through
        // startup.
        CliDaemonCommand::Serve { standby } => PresentationDaemonCommand::Serve(if standby {
            ServeRole::Standby
        } else {
            ServeRole::Active
        }),
        CliDaemonCommand::Start => PresentationDaemonCommand::Start,
        CliDaemonCommand::BootstrapBroker => unreachable!("handled before daemon state setup"),
        CliDaemonCommand::Status => PresentationDaemonCommand::Status,
        CliDaemonCommand::Retire { .. } => unreachable!("handled before lifecycle setup"),
        CliDaemonCommand::Stop { force } => PresentationDaemonCommand::Stop(transition_mode(force)),
        // One manual invocation retains one identity across its internal
        // retries. A later deliberate restart receives another identity, so it
        // cannot be mistaken for a completed lost-ACK replay.
        CliDaemonCommand::Restart {
            restart_agents,
            force,
        } => PresentationDaemonCommand::Replace {
            operation: operation
                .or_else(|| manual_operation_id(&current_build(), runtime_channel())),
            // With `--restart-agents`, force authorizes interrupting a Running
            // Agent, never destruction of generic PTYs. The daemon transition
            // itself therefore remains planned and uses the rollover barrier.
            mode: transition_mode(force && !restart_agents),
        },
        CliDaemonCommand::Replace { .. } => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "daemon replace must be routed through the client trigger",
            ));
        }
        CliDaemonCommand::SyncAfterUpdate => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "managed daemon synchronization must be routed through the update adapter",
            ));
        }
    };
    ensure_private_dir(&daemon_dir)?;
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon_dir.join("daemon.json"),
    });
    let launcher = ServeLauncher {
        exe: std::env::current_exe()?,
        launched: RefCell::new(None),
    };
    let rollover = IpcRolloverRequester {
        data_dir: &data_dir,
        launcher: &launcher,
        restart_agents,
    };
    // One resolution of the workspace identity for the whole process: the fence
    // that guards the workspace and the runtime that owns it must key on the same
    // path, or a daemon could fence one workspace and then take authority over
    // another.
    let workspace_root = bound_workspace_root(&daemon_dir, &std::env::current_dir()?)?;
    let pid = std::process::id();
    let workspace = FileWorkspaceFence {
        path: paths::workspace_fence_path(&workspace_root),
        workspace: workspace_root.clone(),
        pid,
        patience: WORKSPACE_FENCE_PATIENCE,
        held: RefCell::new(None),
    };
    let lock = process_instance_lock(daemon_dir.join("daemon.lock"), &workspace);
    let ready = IpcReady::new(&data_dir, &workspace_root, &lock);
    let shutdown = SignalShutdown::new(Arc::clone(&ready.shutdown));
    let census = DurableResourceCensus {
        data_dir: data_dir.clone(),
    };
    let generations = RegistryGenerationControl::production(data_dir.clone());
    let authority = RegistryAuthority {
        data_dir: &data_dir,
        ready: &ready,
        build: current_build(),
        pid,
        claimed: RefCell::new(None),
    };
    // The standby seams share this process's one shutdown request, so a SIGTERM
    // to a standby takes the same graceful path it takes to an active daemon.
    let standby_endpoint = StandbyIpc::new(
        &data_dir,
        workspace_root.clone(),
        pid,
        Arc::clone(&ready.shutdown),
    );
    let standby_authority = StandbyRegistryAuthority::new(&data_dir, &standby_endpoint, pid);
    let env = DaemonEnv {
        store: &store,
        probe: &ExactProcessControl,
        terminator: &SigtermTerminator,
        ready: &ready,
        authority: &authority,
        standby_endpoint: &standby_endpoint,
        standby_authority: &standby_authority,
        shutdown: &shutdown,
        launcher: &launcher,
        sleeper: &RealSleeper,
        lock: &lock,
        workspace: &workspace,
        pid,
        census: &census,
        generations: &generations,
        seamless: observed_seamless_refusal(&data_dir),
        rollover: &rollover,
    };
    // A stop that leaves the broker running leaves a usagi process the operator
    // has no command to end. Retirement follows the stop rather than preceding
    // it, so a refused stop keeps the broker that a later cold start needs.
    let stopping = matches!(command, PresentationDaemonCommand::Stop(_));
    let reporting = matches!(command, PresentationDaemonCommand::Status);
    let outcome = usagi_daemon::presentation::run(out, command, info, &env);
    if stopping && outcome.is_ok() {
        retire_bootstrap_broker(&data_dir, &workspace_root, &launcher.exe);
    }
    if reporting && outcome.is_ok() {
        append_live_tenant_inventory(out);
    }
    outcome
}

/// Resolve the canonical workspace root this daemon would bind, before anything
/// locks or publishes.
///
/// The candidate is the startup working directory — the same value the session
/// runtime takes — but an already adopted workspace that contains it wins, and
/// within that workspace a durable `repository_root` from a previous start wins
/// again. Starting from a subdirectory or a session worktree therefore cannot
/// fence a workspace the runtime will not own. Canonicalization collapses
/// spelling differences before any of that comparison happens.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=bound_workspace_root_predicts_the_root_open_binds
fn bound_workspace_root(daemon_dir: &Path, candidate: &Path) -> std::io::Result<PathBuf> {
    // A data directory written before workspace state subtrees existed keeps its
    // lifecycle document beside the locator. Moving it is the first thing any
    // start does, so no later reader has to know both layouts.
    workspace_state::migrate_legacy(daemon_dir)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
    let candidate = paths::canonical_workspace_root(candidate)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
    // An adopted workspace wins over the startup directory, so starting from a
    // subdirectory or a session worktree fences the workspace that owns it
    // rather than adopting the directory itself as a second workspace.
    //
    // Resolution here is read-only. Every daemon verb asks which workspace it is
    // about to talk about, including the ones that only read a record, and
    // creating a subtree for each of those would adopt whatever directory the
    // caller happened to stand in. The subtree is created where the workspace is
    // actually opened, in the process that serves it.
    let Some(owner) = workspace_state::owner(daemon_dir, &candidate)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))?
    else {
        return Ok(candidate);
    };
    // Within an adopted subtree the durable document still has the last word: it
    // is the root the runtime will adopt.
    SessionRuntime::bound_workspace_root(owner.dir(), owner.root().to_path_buf())
        .map_err(|error| std::io::Error::other(format!("{error:?}")))
}

/// Whether an adopted subtree has the lifecycle document a read-only standby
/// can hydrate.
///
/// `workspace_state::resolve` records `root.json` before the tenant opener
/// initializes `sessions.json`. A failed open may therefore leave an adopted
/// but uninitialized subtree behind. Absence is a candidate miss; every other
/// node or metadata failure is corruption and remains fail-closed.
fn lifecycle_state_initialized(state_dir: &Path) -> std::io::Result<bool> {
    let path = usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore::new(state_dir)
        .state_path();
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("lifecycle state is not a regular file: {}", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
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
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=open_registers_and_renders_an_explicit_or_current_workspace
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

pub(crate) fn opened_workspace() -> Option<PathBuf> {
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
/// channel reuses an exact artifact. A different known artifact is reused when
/// its protocol handshake is compatible. Development may first request one
/// planned replacement; installed/local clients never churn a live daemon as a
/// side effect of connecting.
///
/// The returned lane is deadline-armed by construction: there is no way to
/// obtain an unbounded daemon socket from this module. `connect_budget_ms`
/// bounds bootstrap, connect and handshake; each later request re-arms the lane
/// with its own budget through [`rearm_lane`].
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=cli_daemon_request_autostarts_without_manual_daemon_start
pub(crate) fn client(
    policy: ClientPolicy,
    connect_budget_ms: u64,
) -> Result<LaneClient, ClientError> {
    client_for(policy, &client_workspace(), connect_budget_ms)
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=cli_daemon_request_autostarts_without_manual_daemon_start
fn client_for(
    policy: ClientPolicy,
    workspace: &ClientWorkspace,
    connect_budget_ms: u64,
) -> Result<LaneClient, ClientError> {
    let clock = SystemClock::new();
    bootstrap_client(workspace, |data_dir, build| {
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
    usagi_core::infrastructure::client::DaemonSession::rearm(client, budget_ms);
}

/// Borrows a lane's underlying socket, for composition-owned passive
/// observation (the restore watcher clones it to peek for EOF).
pub(crate) fn lane_socket(client: &LaneClient) -> &std::os::unix::net::UnixStream {
    &client.transport().get_ref().0
}

/// Development's one-attempt-per-daemon-artifact guard
/// ([`bootstrap::OncePerArtifact`]).
static ATTEMPTED_REPLACEMENTS: bootstrap::OncePerArtifact = bootstrap::OncePerArtifact::new();

/// The daemon artifacts whose reuse this process has already recorded, so a
/// standing mismatch costs one log line instead of one per bootstrapped lane.
static LOGGED_MISMATCHES: bootstrap::OncePerArtifact = bootstrap::OncePerArtifact::new();

const fn should_attempt_automatic_replacement(mode: paths::RuntimeMode) -> bool {
    matches!(mode, paths::RuntimeMode::Development)
}

/// The log entry for a compatible client that keeps talking to a daemon built
/// from another artifact, or `None` when this process already recorded that same
/// standing mismatch.
///
/// Reusing the daemon preserves live Agent conversations, but a stale client is
/// exactly what to look for when a freshly built binary behaves like an older
/// one, so the deliberate mismatch leaves a trail instead of being silent. Every
/// bootstrapped lane observes the same mismatch, hence one entry per daemon
/// artifact rather than one per connection. Only the artifact identities and the
/// non-sensitive reason are recorded.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=a_reused_development_mismatch_is_recorded_once_per_daemon_artifact
fn reused_build_mismatch_record(trigger: &BuildRolloverTrigger, reason: &str) -> Option<String> {
    LOGGED_MISMATCHES
        .claim(&trigger.running_artifact)
        .then(|| {
            format!(
                "client reused the compatible daemon build {} instead of replacing it with {}: {reason}",
                trigger.running_artifact, trigger.expected_artifact
            )
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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_hung_daemon_bounds_one_keystroke_and_resolves_it_by_ledger_query
impl Read for DeadlineUnixStream {
    #[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=mcp_e2e
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_hung_daemon_bounds_one_keystroke_and_resolves_it_by_ledger_query
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

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_hung_daemon_bounds_one_keystroke_and_resolves_it_by_ledger_query
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

fn daemon_probe_result_is_reachable<T>(result: &Result<T, ClientError>) -> bool {
    matches!(result, Ok(_) | Err(ClientError::Protocol(_)))
}

/// Completes the mandatory hello against the published endpoint without
/// sending a request. A framed protocol refusal still proves the endpoint is
/// reachable; only a transport failure means a broker may start another daemon.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=passive_restore_socket_eof_emits_one_reconnect_epoch_and_drop_cancels_watchers
pub(crate) fn current_daemon_is_reachable(data_dir: &Path) -> bool {
    let policy = ClientPolicy::tui();
    let clock = SystemClock::new();
    let result = (|| {
        let stream = usagi_daemon::infrastructure::unix_transport::connect_current(data_dir)
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        let deadline = deadline_transport(clock, stream, TerminalLaneBudget::CONNECT_MS);
        IpcClient::connect(
            deadline,
            client_incarnation().to_owned(),
            format!("readiness-{}", usagi_core::domain::id::OperationId::new()),
            policy,
            current_build(),
            ClientWorkspace::Unbound,
        )
    })();
    daemon_probe_result_is_reachable(&result)
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
    policy_client_for(policy, client_workspace())
}

/// Connects a resilient client that declares the workspace selected by a TUI
/// surface, independently of the process-wide opened workspace.
///
/// # Errors
///
/// Returns an error when `root` cannot be resolved to a canonical workspace or
/// when the daemon cannot be bootstrapped and connected within `policy`.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_daemon_adopts_every_selected_workspace_and_refuses_only_the_fenced_one
pub(crate) fn policy_client_for_selected_workspace(
    policy: ClientPolicy,
    root: &Path,
) -> Result<impl DaemonClient, ClientError> {
    let root = paths::canonical_workspace_root(root)
        .map_err(|error| ClientError::Unavailable(error.to_string()))?;
    policy_client_for(
        policy,
        ClientWorkspace::Selected {
            root: paths::wire_workspace_root(root),
        },
    )
}

fn policy_client_for(
    policy: ClientPolicy,
    workspace: ClientWorkspace,
) -> Result<impl DaemonClient, ClientError> {
    let clock = SystemClock::new();
    let initial = bootstrap_client(&workspace, |data_dir, build| {
        connect_deadline_client(
            data_dir,
            policy,
            build.clone(),
            workspace.clone(),
            clock,
            policy.timeout_ms,
        )
    })?;
    let data_dir = client_result(paths::data_dir())?;
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

fn client_unavailable(error: anyhow::Error) -> ClientError {
    let message = error.to_string();
    drop(error);
    ClientError::Unavailable(message)
}

fn client_result<T>(result: anyhow::Result<T>) -> Result<T, ClientError> {
    result.map_err(client_unavailable)
}

/// Connect a resilient per-request client to the already-running generation
/// without applying bootstrap's artifact replacement decision.
///
/// The rollover controller is itself consuming that decision: asking its
/// control connection to reject the incumbent build would return
/// `RolloverRequired` before it could send the request that performs the
/// rollover.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=explicit_artifact_replacement_runs_under_one_coalesced_operation
fn existing_policy_client(
    policy: ClientPolicy,
    workspace: ClientWorkspace,
) -> Result<impl DaemonClient, ClientError> {
    let clock = SystemClock::new();
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let build = current_build();
    let deadline = Instant::now() + Duration::from_millis(policy.timeout_ms);
    let initial = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let budget_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        match connect_client(
            &data_dir,
            policy,
            build.clone(),
            workspace.clone(),
            |stream| deadline_transport(clock, stream, budget_ms),
        ) {
            Ok(client) => break client,
            Err(error)
                if Instant::now() < deadline
                    && matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound
                            | std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Other
                    ) =>
            {
                // A just-refused rollover can still be releasing its temporary
                // handshake/routing barrier when the next deliberate lifecycle
                // command arrives. Re-observe the exact current owner within
                // the original CLI deadline; never bootstrap or replace here.
                RealSleeper.sleep();
            }
            Err(error) => return Err(ClientError::Unavailable(error.to_string())),
        }
    };
    let reconnect = move |clock: SystemClock, budget_ms: u64| {
        connect_client(
            &data_dir,
            policy,
            build.clone(),
            workspace.clone(),
            |stream| deadline_transport(clock, stream, budget_ms),
        )
        .map_err(|error| ClientError::Unavailable(error.to_string()))
    };
    Ok(PolicyClient::new(clock, policy, reconnect, Some(initial)))
}

/// Connects to the currently published daemon without applying the build
/// replacement policy. Diagnostic and managed-update callers use this narrow
/// lane when cold-start or artifact replacement would change the state being
/// observed.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=doctor_reports_real_diagnostics
pub(crate) fn diagnostic_client(
    policy: ClientPolicy,
    unbound: bool,
) -> Result<LaneClient, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    connect_deadline_client(
        &data_dir,
        policy,
        current_build(),
        if unbound {
            ClientWorkspace::Unbound
        } else {
            client_workspace()
        },
        SystemClock::new(),
        policy.timeout_ms,
    )
    .map_err(|error| ClientError::Unavailable(error.to_string()))
}

/// Cross-process lifecycle custody for one managed update synchronization.
///
/// The updater holds both client bootstrap and explicit lifecycle custody from
/// its final daemon observation through replacement and serving verification.
/// Neither ordinary bootstrap nor a concurrent stop/restart can interleave
/// those steps. When absence was observed, the singleton lock is held too so a
/// direct serve cannot publish until the no-op synchronization has linearized.
pub(crate) struct ManagedUpdateLock {
    _bootstrap: std::fs::File,
    lifecycle: std::fs::File,
    _absence: Option<FileInstanceLock>,
}

/// Observe the exact published owner without cold-starting one, while holding
/// the lifecycle locks needed to keep that observation stable.
///
/// A stale crash record is recovered under the same bootstrap lock. `None`
/// means the singleton lock proved that no daemon is active; a live but not yet
/// reachable owner remains an error rather than being mistaken for absence.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=managed_update_with_a_live_generic_pty_keeps_the_draining_owner
pub(crate) fn managed_update_diagnostic_client(
    policy: ClientPolicy,
) -> Result<(ManagedUpdateLock, Option<LaneClient>), ClientError> {
    let data_dir = client_result(paths::data_dir())?;
    let bootstrap = acquire_bootstrap_lock(&data_dir)?;
    let lifecycle = acquire_lifecycle_lock_io_within(&data_dir, PrivateLockWait::LIFECYCLE)
        .map_err(|error| map_bootstrap_lock_error(&error))?;
    if read_pending_daemon_agent_restart(&data_dir)
        .map_err(|error| ClientError::Lifecycle(error.to_string()))?
        .is_some()
    {
        return Err(ClientError::Lifecycle(
            "an earlier daemon Agent restart is still recovering; daemon synchronization was deferred"
                .to_owned(),
        ));
    }
    let connect = || {
        connect_deadline_client(
            &data_dir,
            policy,
            current_build(),
            ClientWorkspace::Unbound,
            SystemClock::new(),
            policy.timeout_ms,
        )
    };
    match connect() {
        Ok(client) => Ok((
            ManagedUpdateLock {
                _bootstrap: bootstrap,
                lifecycle,
                _absence: None,
            },
            Some(client),
        )),
        Err(connect_error) => {
            match recover_stale_client_endpoint(&data_dir)
                .map_err(|error| ClientError::Unavailable(error.to_string()))?
            {
                bootstrap::StaleRecovery::OwnerActive => {
                    return Err(ClientError::Unavailable(
                        "daemon owner is active but its endpoint is not ready".into(),
                    ));
                }
                bootstrap::StaleRecovery::Recovered | bootstrap::StaleRecovery::NotProven => {}
            }
            let absence = FileInstanceLock {
                path: data_dir.join("daemon").join("daemon.lock"),
                held: RefCell::new(None),
            };
            if !absence
                .acquire()
                .map_err(|error| ClientError::Unavailable(error.to_string()))?
            {
                return Err(ClientError::Unavailable(
                    "daemon owner became active during managed update synchronization".into(),
                ));
            }
            let store = DaemonRecordStore::new(FsRecordFile {
                path: data_dir.join("daemon").join("daemon.json"),
            });
            let record = store
                .load()
                .map_err(|error| ClientError::Unavailable(error.to_string()))?;
            let current = data_dir.join("daemon").join("current.json").is_file();
            if record.is_some() || current {
                return Err(ClientError::Unavailable(format!(
                    "daemon absence could not be proved after endpoint failure: {connect_error}"
                )));
            }
            Ok((
                ManagedUpdateLock {
                    _bootstrap: bootstrap,
                    lifecycle,
                    _absence: Some(absence),
                },
                None,
            ))
        }
    }
}

/// Synchronize the published daemon with the exact installed binary while the
/// installer still owns `update.lock`.
///
/// This lifecycle-only path never starts an absent daemon and never repairs or
/// restarts Agents. A live process-local Agent credential leaves the old daemon
/// untouched and makes the update report a deferred synchronization.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=managed_update_with_a_live_generic_pty_keeps_the_draining_owner
pub(crate) fn sync_after_update(
    out: &mut dyn Write,
    policy: ClientPolicy,
    info: &AppInfo,
) -> std::io::Result<Result<(), ClientError>> {
    let expected_build = current_build();
    let (lock, client) = match managed_update_diagnostic_client(policy) {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    let Some(mut owner) = client else {
        writeln!(out, "daemon sync: daemon is not running; left it stopped")?;
        return Ok(Ok(()));
    };
    let published_build = owner.server_build().clone();
    if published_build != expected_build {
        let workspace = owner
            .request(DaemonRequest::Session {
                action: usagi_core::infrastructure::ipc::SessionAction::List,
                operation_id: usagi_core::domain::id::OperationId::new().to_string(),
                payload: serde_json::json!({}),
            })
            .and_then(|reply| {
                let body = match reply {
                    usagi_core::infrastructure::ipc::DaemonReply::Ok(body)
                    | usagi_core::infrastructure::ipc::DaemonReply::Accepted { body, .. } => body,
                };
                serde_json::from_value::<WorkspaceId>(body["workspace_id"].clone()).map_err(|_| {
                    ClientError::Unavailable(
                        "daemon returned an invalid workspace identity".to_owned(),
                    )
                })
            });
        let workspace = match workspace {
            Ok(workspace) => workspace,
            Err(error) => return Ok(Err(error)),
        };
        let diagnosis = owner
            .request(DaemonRequest::DiagnoseAgents {
                workspace,
                expected: current_agent_integrations(),
            })
            .and_then(|reply| {
                let body = match reply {
                    usagi_core::infrastructure::ipc::DaemonReply::Ok(body)
                    | usagi_core::infrastructure::ipc::DaemonReply::Accepted { body, .. } => body,
                };
                serde_json::from_value::<usagi_core::domain::agent::AgentIntegrationDiagnosis>(body)
                    .map_err(|_| {
                        ClientError::Lifecycle(
                            "daemon cannot prove server-side handoff fencing".to_owned(),
                        )
                    })
            });
        let diagnosis = match diagnosis {
            Ok(diagnosis) => diagnosis,
            Err(error) => return Ok(Err(error)),
        };
        match diagnosis.provisioned_mcp_callers {
            Some(0) => {}
            Some(credentials) => {
                return Ok(Err(ClientError::Lifecycle(format!(
                    "daemon synchronization deferred: {credentials} daemon-provisioned MCP caller credential(s) remain; use 'usagi daemon restart --restart-agents' when they can be restarted"
                ))));
            }
            None => {
                return Ok(Err(ClientError::Lifecycle(
                    "daemon cannot prove server-side handoff fencing".to_owned(),
                )));
            }
        }
        drop(owner);
        if let Err(error) = replace_running_daemon_during_update(out, policy, info, &lock)? {
            return Ok(Err(error));
        }
    }
    let mut current = match diagnostic_client(policy, true) {
        Ok(client) => client,
        Err(error) => return Ok(Err(error)),
    };
    if current.server_build() != &expected_build {
        return Ok(Err(ClientError::Lifecycle(
            "daemon synchronization returned before the installed build was serving".to_owned(),
        )));
    }
    if let Err(error) = current.request(DaemonRequest::Tenant {
        action: usagi_core::infrastructure::ipc::TenantAction::Inventory,
        root: None,
        force: false,
    }) {
        return Ok(Err(error));
    }
    writeln!(out, "daemon sync: installed build is current and serving")?;
    Ok(Ok(()))
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

// ------------------------------------------------- owner generation routing

/// This process's client-side view of the generations it may address.
///
/// The registry and the current locator are files, so reading them per request
/// would put a directory traversal and two `open`/`read` pairs on the IPC hot
/// path — the exact cost that had to be removed from the daemon's own PTY path
/// (#555). One [`RouteCache`] per process reads them on the first owner
/// resolution and then only when it has a reason to: a resolution that fails, or
/// [`invalidate_routes`] after the endpoint it named turned out not to be that
/// generation's. Reusing an already open lane resolves nothing at all.
///
/// The directory is bound to the first caller's data directory. That is the same
/// directory every other lane in this process uses ([`paths::data_dir`] is
/// process-stable), so there is no second authority to disagree with.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_published_generation_routes_to_the_same_endpoint_and_refuses_an_unknown_owner
fn route_cache(
    data_dir: &Path,
) -> &'static Mutex<usagi_core::infrastructure::owner_routing::RouteCache> {
    static CACHE: OnceLock<Mutex<usagi_core::infrastructure::owner_routing::RouteCache>> =
        OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(usagi_core::infrastructure::owner_routing::RouteCache::new(
            usagi_daemon::infrastructure::generation_registry::TrustedGenerationDirectory::new(
                data_dir,
            ),
        ))
    })
}

/// Report that the routing snapshot may no longer describe reality, so the next
/// owner resolution re-reads the durable records.
///
/// A client cannot observe a handoff by itself. What it can observe is that the
/// endpoint the snapshot named did not answer, or answered as a *different*
/// generation. That is the evidence this turns into a re-read, which keeps the
/// read off the per-request path without letting the snapshot outlive a
/// generation change indefinitely.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_published_generation_routes_to_the_same_endpoint_and_refuses_an_unknown_owner
pub(crate) fn invalidate_routes() {
    let Ok(data_dir) = paths::data_dir() else {
        return;
    };
    if let Ok(mut cache) = route_cache(&data_dir).lock() {
        cache.invalidate();
    }
}

/// Resolve the endpoint of the generation that owns a terminal, fail closed.
///
/// A `TerminalRef` names its owner, and only the daemon-written records may turn
/// that name into an address. An owner that is not in the trusted set — never
/// registered, already retired, or forged — is a typed `stale_target`; it is
/// never answered with the active endpoint, because the active generation would
/// happily serve a *different* terminal that merely shares a name.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_published_generation_routes_to_the_same_endpoint_and_refuses_an_unknown_owner
fn owner_endpoint(
    generation: usagi_core::domain::id::DaemonGeneration,
) -> Result<usagi_core::infrastructure::owner_routing::TrustedEndpoint, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let mut cache = route_cache(&data_dir)
        .lock()
        .map_err(|_| ClientError::Unavailable("generation routing cache is poisoned".into()))?;
    cache
        .owner(generation)
        .map_err(|error| error.to_client_error())
}

/// One lane, together with the role of the generation it reached.
pub(crate) struct OwnerLane {
    pub(crate) client: LaneClient,
    pub(crate) role: usagi_core::infrastructure::ipc::GenerationRole,
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=owner_addressed_requests_route_to_their_reference_and_the_rest_are_refused
impl OwnerLane {
    /// Whether this lane reached the generation that currently holds `current`.
    pub(crate) fn is_active(&self) -> bool {
        self.role == usagi_core::infrastructure::ipc::GenerationRole::Active
    }
}

/// Open a lane to the exact generation that owns a terminal.
///
/// The two roles take deliberately different paths:
///
/// | owner role | path |
/// |---|---|
/// | active | [`client`] — the published locator, the bootstrap that may cold-start a daemon, and the exact-owner process fence, all unchanged |
/// | draining | [`connect_generation`] on that generation's own verified socket, with no bootstrap at all |
///
/// A draining generation is never cold-started and never re-published, so
/// starting a daemon because it did not answer would produce a *different*
/// daemon rather than the owner that was asked for. It is reached over its own
/// socket or not at all.
///
/// With one generation published — every build that cannot yet roll over — the
/// resolution always lands on `Active`, so this is the connection [`client`] has
/// always made, over the same locator and behind the same fences.
///
/// Whichever path is taken, the peer must then **say** it is the generation that
/// was asked for before the lane is handed out. That is what makes a stale
/// snapshot harmless: resolving an owner the records no longer name as active
/// would otherwise hand back a lane onto the daemon that replaced it, keyed as
/// if it were the old one. A mismatch refuses the lane and marks the snapshot
/// stale, so the next resolution reads the records again and answers with the
/// typed refusal the reference deserves.
///
/// [`connect_generation`]: usagi_daemon::infrastructure::unix_transport::connect_generation
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=one_published_generation_routes_to_the_same_endpoint_and_refuses_an_unknown_owner
pub(crate) fn owner_client(
    policy: ClientPolicy,
    generation: usagi_core::domain::id::DaemonGeneration,
    connect_budget_ms: u64,
) -> Result<OwnerLane, ClientError> {
    let endpoint = owner_endpoint(generation)?;
    let opened = if endpoint.role == usagi_core::infrastructure::ipc::GenerationRole::Active {
        client(policy, connect_budget_ms)
    } else {
        connect_draining(policy, &endpoint, connect_budget_ms)
    };
    let opened = opened.inspect_err(|_| {
        // The endpoint the snapshot named could not be reached. Either the owner
        // is momentarily unavailable or the records have moved on; a re-read is
        // the only way to tell, and it happens on the next resolution rather
        // than on this failed one.
        invalidate_routes();
    })?;
    if opened.daemon_generation().0 != generation.as_str() {
        invalidate_routes();
        return Err(
            usagi_core::infrastructure::owner_routing::RoutingError::UnknownGeneration(generation)
                .to_client_error(),
        );
    }
    Ok(OwnerLane {
        client: opened,
        role: endpoint.role,
    })
}

/// Connect one draining generation over its own socket.
///
/// The handshake is the ordinary one: a draining generation has no `current`
/// locator entry and no active record to bind to, so the active path's
/// process-start fence cannot apply. What replaces it is the endpoint check —
/// the socket is re-derived and re-verified as that generation's own private
/// endpoint by `connect_generation` — plus the generation the peer names, which
/// [`owner_client`] checks for both roles alike.
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=a_draining_generation_refuses_control_and_still_serves_its_own_terminals
fn connect_draining(
    policy: ClientPolicy,
    endpoint: &usagi_core::infrastructure::owner_routing::TrustedEndpoint,
    connect_budget_ms: u64,
) -> Result<LaneClient, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let stream =
        usagi_daemon::infrastructure::unix_transport::connect_generation(&data_dir, endpoint)
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let clock = SystemClock::new();
    IpcClient::connect(
        deadline_transport(clock, stream, connect_budget_ms),
        client_incarnation().to_owned(),
        format!("{}", std::process::id()),
        policy,
        current_build(),
        client_workspace(),
    )
}

/// Requests and performs an intentional replacement of the running daemon
/// artifact.
///
/// The trigger is derived first, effect free, from the two advertised artifact
/// identities; the replacement it keys is then carried out on exactly the path
/// `usagi daemon restart` takes, so a build/update swap can never reach a
/// `stop` → fresh `start` the manual verb is guarded against.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=explicit_artifact_replacement_runs_under_one_coalesced_operation
pub(crate) fn replace_running_daemon(
    out: &mut dyn Write,
    policy: ClientPolicy,
    force: bool,
    info: &AppInfo,
) -> std::io::Result<Result<(), ClientError>> {
    let (_bootstrap, trigger) = match request_replacement(policy) {
        Ok(prepared) => prepared,
        Err(error) => return Ok(Err(error)),
    };
    run(
        out,
        CliDaemonCommand::Restart {
            restart_agents: false,
            force,
        },
        info,
        Some(trigger.operation_id),
    )
    .map(Ok)
}

/// Replace a published daemon while the managed updater already holds the
/// bootstrap and lifecycle locks returned by
/// [`managed_update_diagnostic_client`].
#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=managed_update_with_a_live_generic_pty_keeps_the_draining_owner
pub(crate) fn replace_running_daemon_during_update(
    out: &mut dyn Write,
    policy: ClientPolicy,
    info: &AppInfo,
    lock: &ManagedUpdateLock,
) -> std::io::Result<Result<(), ClientError>> {
    let trigger = match request_replacement_while_locked(policy) {
        Ok(trigger) => trigger,
        Err(error) => return Ok(Err(error)),
    };
    run_with_lifecycle_custody(
        out,
        CliDaemonCommand::Restart {
            restart_agents: false,
            force: false,
        },
        info,
        Some(trigger.operation_id),
        Some(&lock.lifecycle),
    )
    .map(Ok)
}

/// Requests intentional replacement of the currently running daemon artifact.
/// This only creates the deterministic trigger; it never sends a stop signal or
/// spawns a second daemon. [`replace_running_daemon`] consumes it.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=explicit_artifact_replacement_runs_under_one_coalesced_operation
fn request_replacement(
    policy: ClientPolicy,
) -> Result<(std::fs::File, BuildRolloverTrigger), ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let bootstrap = acquire_bootstrap_lock(&data_dir)?;
    request_replacement_while_locked(policy).map(|trigger| (bootstrap, trigger))
}

fn runtime_channel() -> &'static str {
    runtime_channel_for(paths::runtime_mode())
}

const fn runtime_channel_for(mode: paths::RuntimeMode) -> &'static str {
    match mode {
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

#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=ordinary_client_recovers_a_sigkilled_daemon_without_manual_lifecycle
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
        // A stale owner is process-verified gone. An unverified legacy PID is
        // not signal authority, but this callback is entered only after a
        // validated current locator was unreachable. In both cases the held
        // singleton lock is reclaim authority: after the exact-record recheck,
        // no active owner can be displaced and no PID is addressed.
        usagi_core::domain::daemon::DaemonState::Stale(_)
        | usagi_core::domain::daemon::DaemonState::Unverified => {}
        usagi_core::domain::daemon::DaemonState::Alive => {
            return Ok(bootstrap::StaleRecovery::OwnerActive);
        }
        usagi_core::domain::daemon::DaemonState::Absent => {
            return Ok(bootstrap::StaleRecovery::NotProven);
        }
    }

    // Socket-first retirement and current.lock provide the endpoint commit
    // fence. The record remains present on every cleanup error.
    //
    // The instance lock this path holds excludes another *active* daemon, not a
    // standby — which holds no lock and whose live socket is therefore
    // indistinguishable on the filesystem from a crashed generation's leftover.
    // Sweeping it would leave the registry naming a verified successor nobody
    // accepts on, so the same durable answer the daemon-side sweep uses applies
    // here.
    let live = live_generation_endpoints(data_dir);
    retire_stale_current_preserving(data_dir, &|generation| live.contains(generation))?;
    if store.clear_if(&expected)? {
        Ok(bootstrap::StaleRecovery::Recovered)
    } else {
        Ok(bootstrap::StaleRecovery::NotProven)
    }
}

pub(crate) fn current_build() -> BuildIdentity {
    // The artifact identity is a compile-time constant baked in by `build.rs`
    // from this binary's source/tree, profile, and target. It is therefore
    // immutable for the process lifetime and never re-read from disk, so an
    // atomic replacement of the executable path cannot change what a running
    // daemon advertises. `build.rs` leaves the source id empty when it cannot
    // uniquely identify the source, which keeps the identity fail-safe unknown.
    #[cfg(debug_assertions)]
    let source_id = std::env::var("USAGI_TEST_BUILD_SOURCE_ID")
        .unwrap_or_else(|_| env!("USAGI_BUILD_SOURCE_ID").to_owned());
    #[cfg(not(debug_assertions))]
    let source_id = env!("USAGI_BUILD_SOURCE_ID").to_owned();
    usagi_core::infrastructure::ipc::build_identity(
        env!("CARGO_PKG_VERSION"),
        env!("USAGI_BUILD_COMMIT"),
        env!("USAGI_BUILD_TARGET"),
        env!("USAGI_BUILD_PROFILE"),
        &source_id,
    )
}
/// Connects one exact-owner-verified daemon session. `arm` wraps the accepted
/// socket in the transport the caller's lane runs over; it is applied only after
/// the peer's process-start identity, record and generation have been observed,
/// so the fence is identical for every lane.
#[coverage(off)] // coverage: reason=generic_monomorphization owner=daemon expires=2027-01-31 tests=one_published_generation_routes_to_the_same_endpoint_and_refuses_an_unknown_owner
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
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(opened) = opened {
        child.current_dir(opened);
    }
    child
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=daemon_lifecycle_recovers_a_crash_record_whose_pid_was_reused
fn run_lifecycle(exe: &Path, command: &str, workspace: &Path) -> std::io::Result<()> {
    run_lifecycle_with(exe, &["daemon", command], command, workspace)
}

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=daemon_lifecycle_recovers_a_crash_record_whose_pid_was_reused
fn run_lifecycle_with(
    exe: &Path,
    args: &[&str],
    command: &str,
    workspace: &Path,
) -> std::io::Result<()> {
    let output = lifecycle_command(exe, args, Some(workspace.to_path_buf())).output()?;
    output.status.success().then_some(()).ok_or_else(|| {
        std::io::Error::other(lifecycle_failure_message(
            command,
            &output.stderr,
            &output.stdout,
        ))
    })
}

fn lifecycle_failure_message(command: &str, stderr: &[u8], stdout: &[u8]) -> String {
    let detail = [stderr, stdout]
        .into_iter()
        .map(|bytes| String::from_utf8_lossy(bytes))
        .map(|message| message.trim().to_owned())
        .find(|message| !message.is_empty());
    detail.map_or_else(
        || format!("daemon {command} failed"),
        |detail| format!("daemon {command} failed: {detail}"),
    )
}

/// Ensures that an active daemon endpoint exists before an interactive TUI is
/// shown. TUI operations still acquire their own client connection.
///
/// This readiness probe sends no request, so it declares no workspace: the entry
/// screens that need it (`usagi hop`'s Recent list, `usagi open <path>`) are
/// workspace switchers that must keep working from any directory. The
/// workspace-bound connections those screens make afterwards carry their own
/// declaration and are fenced there.
#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=cli_daemon_request_autostarts_without_manual_daemon_start
pub(crate) fn ensure_ready() -> Result<(), ClientError> {
    client_for(
        ClientPolicy::tui(),
        &ClientWorkspace::Unbound,
        ClientPolicy::tui().timeout_ms,
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests;
