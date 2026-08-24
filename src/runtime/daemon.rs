//! daemon 面へ Unix process / socket / signal を接続する composition adapter。

#![coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=root_ipc_fixture_codex_survives_disconnect_and_replays_final,planned_stop_retires_generation_endpoint_and_allows_safe_autostart

use std::backtrace::Backtrace;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as FmtWrite;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, LockResult, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use usagi_cli::cli::DaemonCommand as CliDaemonCommand;
use usagi_core::domain::AppInfo;
use usagi_core::domain::agent::prompt::{McpToolFamilies, PromptScope, launch_system_prompt};
use usagi_core::domain::agent::{AgentProfileId, DurableLaunchSnapshot, EnvironmentVariableName};
use usagi_core::domain::daemon::{DaemonProcessObservation, DaemonRecord};
use usagi_core::domain::id::{ConnectionId, SessionId, TerminalRef, WorkspaceId, WorktreeId};
use usagi_core::domain::settings::DefaultModel;
use usagi_core::infrastructure::bounded_process::{ChildObservation, ChildPolicy, observe};
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonReady, DaemonRecordStore, InstanceLock, LivenessProbe,
    ProcessIdentitySource, RecordFile, ShutdownSignal, Sleeper, Terminator, WorkspaceFence,
    WorkspaceFenceOutcome,
};
use usagi_core::infrastructure::env_resolver::OpCli;
use usagi_core::infrastructure::error_log::ErrorLog;
use usagi_core::infrastructure::ipc::{
    BuildArtifactDecision, BuildIdentity, BuildRolloverTrigger, ClientWorkspace, OperationId,
    build_artifact_decision, build_rollover_trigger,
};
use usagi_core::infrastructure::paths;
use usagi_core::infrastructure::store::dispatch::DispatchStore;
use usagi_core::infrastructure::store::issue::AmbiguousIssueNumber;
use usagi_core::infrastructure::store::pr_inventory::PrInventoryStore;
use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;
use usagi_core::infrastructure::store::user_decision::UserDecisionStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::infrastructure::workspace_state;
use usagi_core::usecase::claude_sandbox::{self, SandboxMode};
use usagi_core::usecase::client::{
    ClientError, ClientPolicy, DaemonClient, DeadlineConnection, DeadlineStream, IpcClient,
    MonotonicClock, PolicyClient,
};
use usagi_core::usecase::client::{DaemonRequest, DispatchToolAction, SupervisorToolAction};
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
    AGENT_RUNTIME_LIMIT, AgentReadinessPreflight, AgentRuntime, AgentTerminalActor,
    ResolvedAgentScope, ScopeResolveError, SessionScopeResolver, SharedTerminalOwner,
    TerminalOutcome,
};
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
use usagi_daemon::usecase::authority::rollover::CurrentLocator;
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
use usagi_daemon::usecase::pr_inventory::{
    GhProcessPort, OutputPrProjector, RefreshClock, RefreshWorker,
};
use usagi_daemon::usecase::pr_projection::{
    PrProjection, PrProjectionQueue, pr_projection_counters,
};
use usagi_daemon::usecase::replacement::{
    LiveResources, ResourceCensus, RolloverRequester, SeamlessRefusal, TransitionMode,
    manual_operation_id, seamless_refusal,
};
use usagi_daemon::usecase::resources::allocator::{CapacityPolicy, ResourceAllocator};
use usagi_daemon::usecase::resources::durable::{
    IdentityAuthority, ShardedAgentStore, ShardedRuntimeState, ShardedTerminalStore, census,
    shipping_retention_limits,
};
use usagi_daemon::usecase::resources::fence::FencedPrInventory;
use usagi_daemon::usecase::resources::identity::{ChildIdentity, ChildProcessProbe, record_child};
use usagi_daemon::usecase::resources::retention::LogicalClock;
use usagi_daemon::usecase::rollover_trigger;
use usagi_daemon::usecase::runtime::{
    OutputJournal, ProvisionContext, PtySpawner, RuntimeStoreSnapshot, SandboxLauncher,
    SpawnProvision, TerminateReapError,
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
    DecisionWake, DecisionWaker, InitialTask, SupervisorRuntime,
};
use usagi_daemon::usecase::tenant::{
    DEFAULT_TENANT_LIMIT, OpenedTenant, TenantRegistry, TenantRuntimeOpener, WorkspaceFenceFactory,
};
use usagi_daemon::usecase::terminal::{
    Geometry, Output, PtyWriteError, PtyWriter, SpawnFailure, output_pipeline_counters,
};
use usagi_daemon::usecase::terminal_ipc::{
    GENERIC_TERMINAL_LIMIT, GenericTerminalRuntime, ResolvedTerminalScope,
    TerminalScopeResolveError, TerminalScopeResolver,
};
use usagi_daemon::usecase::terminal_profile::{LoginShellProfile, TERMINAL_ENVIRONMENT_VARIABLES};

use crate::runtime::user_env::{self, UserEnvironment};

/// The daemon's configured-environment reader, shared by the Agent adapters and
/// the terminal profile resolver.
type SharedUserEnvironment = UserEnvironment<OpCli>;

struct TrustedLoginShell {
    profile: LoginShellProfile,
    /// The configured environment for the launch's workspace, resolved at launch
    /// time. `None` in tests that exercise only the shell profile.
    environment: Option<Arc<SharedUserEnvironment>>,
    /// Where the workspace whose configured bindings apply is found. A launch
    /// names its workspace, so a daemon holding several resolves it per request
    /// instead of binding the one it was started in. `None` in tests that
    /// exercise only the shell profile, which then use [`Self::workspace_root`].
    workspaces: Option<Workspaces>,
    /// The repository the configured workspace bindings belong to, when no
    /// registry is bound.
    workspace_root: PathBuf,
}

impl TrustedLoginShell {
    /// The workspace whose configured bindings this launch inherits.
    fn launch_workspace_root(
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

/// The store-side view of [`SpawnedChildren`]: it can only ask, never record.
struct ObservedChildren(Arc<SpawnedChildren>);

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
fn open_runtime_state(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    children: &Arc<SpawnedChildren>,
) -> std::io::Result<ShardedRuntimeState> {
    ShardedRuntimeState::new(
        generation,
        GenerationRole::Active,
        ResourceAllocator::new(
            AllocatorFile::new(data_dir)?,
            CapacityPolicy::new(AGENT_RUNTIME_LIMIT, GENERIC_TERMINAL_LIMIT),
        ),
        Box::new(ShardArchiveFiles::new(data_dir)?),
        Box::new(ObservedChildren(Arc::clone(children))),
        Box::new(SystemLogicalClock),
    )
}

/// Reads this generation's shard and every retained one, migrating the legacy
/// whole-snapshot stores on the first start that finds them.
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

/// Why this build cannot hand authority to a live successor, read from the
/// durable generation registry.
///
/// An unreadable or unparsable registry is reported as such rather than treated
/// as absent, so an operator sees the difference between "no daemon ever
/// registered a generation" and "the registry cannot be trusted".
fn observed_seamless_refusal(data_dir: &Path) -> Option<SeamlessRefusal> {
    match usagi_daemon::infrastructure::generation_registry::read_registry_document(data_dir) {
        Ok(document) => {
            let active_is_alive = document
                .as_ref()
                .and_then(RegistryDocument::active)
                .is_some_and(|entry| {
                    observe_generation_process(&entry.process)
                        == ProcessObservation::VerifiedAlive(entry.process.clone())
                });
            seamless_refusal(document.as_ref(), active_is_alive, DEFAULT_GENERATION_LIMIT)
        }
        Err(error) => Some(SeamlessRefusal::RegistryUnreadable(error.to_string())),
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
    workspaces: Workspaces,
    mcp_command: PathBuf,
    data_home: paths::DataHome,
    /// The executable this profile launches: `codex`, or `codex-fugu` for the
    /// Codex-compatible `sakana-ai` profile.
    program: &'static str,
    /// The configured environment injected into the Agent child. `None` in tests
    /// that exercise only the MCP wiring.
    environment: Option<Arc<SharedUserEnvironment>>,
    sandbox_backend: Option<PathBuf>,
    sandbox_tmpdir: Option<PathBuf>,
    sandbox_home: Option<PathBuf>,
    sandbox_cache_dir: Option<PathBuf>,
    sandbox_passthrough: bool,
}
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
            .map(|tools| codex_integration_arguments(&self.mcp_command, tools.model()))
            .transpose()
            .map_err(|()| CodexProvisionFailure::MaterializationFailed)?
            .unwrap_or_default();
        arguments.extend(codex_system_prompt_arguments(
            mode,
            tools.as_ref().map(ConfiguredMcpTools::families),
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
        if mode == SandboxMode::Root {
            let sandbox_roots =
                root_agent_writable_roots(self.sandbox_home.as_deref(), self.program)
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
            )
            .map_err(|()| CodexProvisionFailure::MaterializationFailed)?;
            spawn.set_sandbox_launcher(launcher);
            insert_root_git_environment(&mut spawn);
            if self.sandbox_passthrough {
                spawn.insert_daemon_environment(
                    EnvironmentVariableName::new(claude_sandbox::PASSTHROUGH_ENVIRONMENT_VARIABLE)
                        .expect("literal environment variable name is valid"),
                    "1".to_owned(),
                );
            }
        }
        Ok(CodexProvision {
            working_directory,
            environment_allowlist: launch_allowlist(context, &user),
            spawn,
        })
    }
}
/// The Claude provisioner's product program: what the readiness probe proves,
/// what the launcher execs, and whose `$HOME` state root the sandbox grants.
/// The Codex provisioner carries the same value per profile (`RootCodexProvisioner::program`).
const CLAUDE_PROGRAM: &str = "claude";

/// Ensure the launched agent's private state directory exists before a root
/// sandbox starts. Linux `--bind-try` cannot make a missing bind source writable,
/// so the daemon creates and validates the provider-specific directory first.
fn root_agent_writable_roots(
    home: Option<&Path>,
    program: &str,
) -> Result<Vec<PathBuf>, ClaudeSandboxPolicyError> {
    let (Some(home), Some(state_directory)) =
        (home, claude_sandbox::agent_state_directory(program))
    else {
        return Ok(Vec::new());
    };
    validate_owned_directory(home)?;
    let state = home.join(state_directory);
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
    state
        .canonicalize()
        .map(|state| vec![state])
        .map_err(|_| ClaudeSandboxPolicyError::InvalidWritableRoot)
}

/// Codex's arg0 janitor cannot open the `.lock` inside a directory left with no
/// owner permissions by an interrupted sandboxed cleanup. Repair only the mode
/// of owned, provider-named temp directories; Codex still owns lock validation
/// and deletion, so a live helper is never removed here.
const CODEX_ARG0_REPAIR_LIMIT: usize = 4_096;

fn repair_codex_arg0_permissions(state_root: &Path) -> std::io::Result<usize> {
    repair_codex_arg0_permissions_with_limit(state_root, CODEX_ARG0_REPAIR_LIMIT)
}

fn repair_codex_arg0_permissions_with_limit(
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

struct RootClaudeProvisioner {
    workspaces: Workspaces,
    mcp_command: PathBuf,
    data_home: paths::DataHome,
    /// daemon bootstrap の trusted environment から一度だけ確定した backend。
    sandbox_backend: Option<PathBuf>,
    /// daemon bootstrap の trusted environment から一度だけ確定した policy paths。
    sandbox_tmpdir: Option<PathBuf>,
    sandbox_home: Option<PathBuf>,
    /// daemon bootstrap の trusted environment から一度だけ確定した macOS の per-user cache root。
    sandbox_cache_dir: Option<PathBuf>,
    /// The configured environment injected into the Agent child. `None` in tests
    /// that exercise only the sandbox and MCP wiring.
    environment: Option<Arc<SharedUserEnvironment>>,
    /// E2E テスト専用 seam（[`claude_sandbox::passthrough_requested`]）。true のとき launcher の子へ
    /// 同じ opt-in を伝え、backend の無い環境でも live 起動経路を通す。release ビルドでは常に false。
    sandbox_passthrough: bool,
}
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
impl ClaudeProvisioner for RootClaudeProvisioner {
    fn provision(
        &mut self,
        context: &ProvisionContext,
    ) -> Result<ClaudeProvision, ClaudeProvisionFailure> {
        let (working_directory, workspace_root) = working_directories(&self.workspaces, context)
            .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?;
        // Claude は必ず OS sandbox の中で起動する（多層防御の hard boundary）。論理境界の
        // `guard-workspace` も両 scope に配線し、root は tool と OS の両方で fail-closed にする。
        let mode = sandbox_mode(context);
        let launch_roots = claude_writable_roots(mode, &working_directory);
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
            .map(|tools| claude_mcp_arguments(&self.mcp_command, tools.model()))
            .transpose()
            .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?
            .unwrap_or_default();
        arguments.extend(
            claude_settings_arguments(&self.mcp_command)
                .map_err(|()| ClaudeProvisionFailure::MaterializationFailed)?,
        );
        arguments.extend(claude_system_prompt_arguments(
            mode,
            tools.as_ref().map(ConfiguredMcpTools::families),
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
fn sandbox_mode(context: &ProvisionContext) -> SandboxMode {
    if context.scope.session_id.is_some() {
        SandboxMode::Session
    } else {
        SandboxMode::Root
    }
}

/// The launch-specific writable roots handed to `usagi claude-sandbox`.
/// A session launch receives exactly its own worktree. A root coordinator receives
/// no repository-local writable root. Daemon bootstrap is delegated to the
/// out-of-sandbox bootstrap broker.
fn claude_writable_roots(mode: SandboxMode, working_directory: &Path) -> Vec<PathBuf> {
    if mode == SandboxMode::Session {
        vec![working_directory.to_path_buf()]
    } else {
        Vec::new()
    }
}

/// Root providers may run the small read-only Git allowlist accepted by
/// `guard-workspace`. Override process-launching repository configuration and
/// optional index refreshes from daemon-owned, highest-precedence environment.
fn insert_root_git_environment(spawn: &mut SpawnProvision) {
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
fn validate_root_git_common_dir_policy(
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
        writable.extend(
            [
                claude_sandbox::agent_state_directory(program),
                claude_sandbox::agent_config_prefix(program),
            ]
            .into_iter()
            .flatten()
            .map(|granted| home.join(granted)),
        );
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

fn git_common_dir(workspace_root: &Path) -> Result<PathBuf, ()> {
    fn read_path(path: &Path, prefix: Option<&str>) -> Result<PathBuf, ()> {
        let metadata = std::fs::metadata(path).map_err(|_| ())?;
        if metadata.len() > 16 * 1024 {
            return Err(());
        }
        let value = std::fs::read_to_string(path).map_err(|_| ())?;
        let value = prefix.map_or(value.trim(), |prefix| {
            value.trim().strip_prefix(prefix).map_or("", str::trim)
        });
        (!value.is_empty()).then(|| PathBuf::from(value)).ok_or(())
    }

    let marker = workspace_root.join(".git");
    let marker_path = marker.canonicalize().map_err(|_| ())?;
    let git_dir = if marker_path.is_dir() {
        marker_path
    } else {
        let path = read_path(&marker_path, Some("gitdir:"))?;
        let path = if path.is_absolute() {
            path
        } else {
            workspace_root.join(path)
        };
        path.canonicalize().map_err(|_| ())?
    };
    let common_marker = git_dir.join("commondir");
    if !common_marker.exists() {
        return Ok(git_dir);
    }
    let path = read_path(&common_marker, None)?;
    let path = if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    };
    path.canonicalize().map_err(|_| ())
}

/// Policy paths are daemon-owned inputs.  Validate their identity before user
/// bindings (and therefore secrets) are resolved.  The launcher later receives
/// only these checked paths through argv and never consults its child environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeSandboxPolicyError {
    MissingBackend,
    InvalidBackend,
    InvalidWritableRoot,
    ProtectedWorkspaceAncestor,
}

/// daemon が確定した、1 回の launch 分の sandbox policy 入力。
struct SandboxPolicyInputs<'a> {
    mode: SandboxMode,
    /// sandbox の中で exec する agent CLI（`claude` / `codex` / `codex-fugu`）。root mode で
    /// launcher が足す `$HOME` 配下の state root（`~/.claude` / `~/.codex` / …）を決めるため、
    /// daemon 側の検証もこの program に追従する。
    program: &'a str,
    workspace_root: &'a Path,
    launch_roots: &'a [PathBuf],
    tmpdir: Option<&'a Path>,
    home: Option<&'a Path>,
    /// macOS の per-user cache root。root mode の launcher はこの下の `mds`（Keychain 検索が
    /// 更新する MDS cache）を writable にするため、daemon 側も同じ root を検証する。
    cache_dir: Option<&'a Path>,
    backend: Option<&'a Path>,
    passthrough: bool,
}

/// launcher へ host path を渡す前に通す policy gate。writable root 集合・`$HOME` 配下の
/// state root・（root mode では）Git common dir を、保護対象 workspace と突き合わせる。
fn validate_claude_sandbox_policy(
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
        // gate は launcher の grant を写す: state directory（subtree）と、その隣に置かれる
        // global config の path prefix（`~/.claude.json*`）の両方を見る。
        for granted in [
            claude_sandbox::agent_state_directory(program),
            claude_sandbox::agent_config_prefix(program),
        ]
        .into_iter()
        .flatten()
        {
            let granted = home.join(granted);
            let granted = granted.canonicalize().unwrap_or(granted);
            if protected_workspace.starts_with(&granted)
                || (mode == SandboxMode::Root && granted.starts_with(&protected_workspace))
            {
                return Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor);
            }
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
        validate_owned_directory(&root)?;
        let canonical = root
            .canonicalize()
            .map_err(|_| ClaudeSandboxPolicyError::InvalidWritableRoot)?;
        if protected_workspace.starts_with(&canonical)
            || (mode == SandboxMode::Root && canonical.starts_with(&protected_workspace))
        {
            return Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor);
        }
    }
    Ok(())
}

fn validate_sandbox_backend(path: &Path) -> Result<(), ClaudeSandboxPolicyError> {
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

fn validate_owned_directory(path: &Path) -> Result<(), ClaudeSandboxPolicyError> {
    if !path.is_absolute() || path == Path::new("/") {
        return Err(ClaudeSandboxPolicyError::InvalidWritableRoot);
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| ClaudeSandboxPolicyError::InvalidWritableRoot)?;
    if !metadata.file_type().is_dir() || path_has_symlink_component(path) {
        return Err(ClaudeSandboxPolicyError::InvalidWritableRoot);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(ClaudeSandboxPolicyError::InvalidWritableRoot);
        }
    }
    Ok(())
}

fn path_has_symlink_component(path: &Path) -> bool {
    path.ancestors().any(|component| {
        std::fs::symlink_metadata(component).map_or(true, |metadata| {
            metadata.file_type().is_symlink() && !is_macos_system_firmlink(component)
        })
    })
}

fn is_macos_system_firmlink(path: &Path) -> bool {
    cfg!(target_os = "macos") && matches!(path.to_str(), Some("/var" | "/tmp" | "/etc"))
}

/// macOS の per-user cache root（`confstr(_CS_DARWIN_USER_CACHE_DIR)`）を canonical path で確定する。
///
/// Keychain 検索は Module Directory Service (MDS) の per-user cache（`<cache>/mds`）を更新するため、
/// root sandbox がここへ書けないと `SecKeychainSearchCreateFromAttributes` が失敗し、agent CLI は
/// Keychain の credential を読めないまま古い file 側 credential へ fallback して 401 で起動できない。
/// 値は `$TMPDIR` / `$HOME` と同じく daemon bootstrap の trusted environment で一度だけ確定し、
/// Agent child は再解決しない。macOS 以外には MDS が無いため `None` を返す。
#[coverage(off)] // coverage: reason=real_io owner=root-cli expires=2027-01-31 tests=root_policy_accepts_the_per_user_cache_root
fn resolve_sandbox_cache_dir() -> Option<PathBuf> {
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
struct SandboxLauncherPaths<'a> {
    backend: Option<&'a Path>,
    tmpdir: Option<&'a Path>,
    home: Option<&'a Path>,
    /// macOS の per-user cache root。`<cache>/mds` を writable にするために渡す。
    cache_dir: Option<&'a Path>,
}

/// `usagi claude-sandbox --mode <mode> [--writable-root <path>]… --`, the ephemeral
/// instruction that makes the spawned child the launcher instead of the bare
/// product.  Host paths stay out of the durable launch snapshot.
fn claude_sandbox_launcher(
    usagi: &Path,
    mode: SandboxMode,
    protected_root: &Path,
    paths: &SandboxLauncherPaths<'_>,
    writable_roots: &[PathBuf],
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
    prefix.push("--".to_owned());
    Ok(SandboxLauncher {
        program: usagi.to_str().ok_or(())?.to_owned(),
        prefix,
    })
}

/// `--settings <json>`: the scoped hook wiring Claude loads for this launch.
/// The payload is passed inline so no host path or rendered product payload has
/// to be materialized on disk.
fn claude_settings_arguments(usagi: &Path) -> Result<Vec<String>, ()> {
    let usagi = usagi.to_str().ok_or(())?;
    Ok(vec!["--settings".to_owned(), scoped_settings_json(usagi)])
}

/// The scope-specific system prompt passed as one opaque argv value. Unlike the
/// hook command payload, this never crosses a shell or JSON boundary.
fn claude_system_prompt_arguments(
    mode: SandboxMode,
    mcp: Option<McpToolFamilies>,
    role: Option<(&usagi_core::domain::role::RoleId, &str)>,
) -> Vec<String> {
    claude_prompt_arguments(launch_system_prompt(prompt_scope(mode), mcp, role))
}

/// The prompt boundary a sandbox mode launches into.
const fn prompt_scope(mode: SandboxMode) -> PromptScope {
    match mode {
        SandboxMode::Root => PromptScope::Root,
        SandboxMode::Session => PromptScope::Session,
    }
}

fn claude_prompt_arguments(prompt: String) -> Vec<String> {
    vec!["--append-system-prompt".to_owned(), prompt]
}

/// The configured environment for a launch in `workspace_root`, or nothing when
/// no reader is wired (tests that exercise only the MCP / sandbox wiring).
fn configured_environment(
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

/// The child's data-home half of the contract: it receives the mode-neutral
/// base plus the mode that selects the daemon's own directory below it, so
/// re-applying the mode lands it on the very directory the daemon is using.
/// Both values come from the one [`paths::DataHome`] pair, never from separate
/// derivations that could disagree.
fn mcp_environment(
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
fn codex_integration_arguments(
    command: &Path,
    local_llm_model: Option<&str>,
) -> Result<Vec<String>, ()> {
    let command = command.to_str().ok_or(())?;
    let hook_command = format!("{} codex-session-capture", shell_quote(command));
    let hook_command = serde_json::to_string(&hook_command).map_err(|_| ())?;
    let mut arguments = codex_product_mcp_arguments(command, local_llm_model);
    arguments.extend([
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
    ]);
    Ok(arguments)
}

/// The scope-specific system prompt rendered as one Codex `-c` assignment.
/// Both argv elements stay ephemeral and precede the durable product argv.
fn codex_system_prompt_arguments(
    mode: SandboxMode,
    mcp: Option<McpToolFamilies>,
    role: Option<(&usagi_core::domain::role::RoleId, &str)>,
) -> Vec<String> {
    codex_developer_instructions_arguments(&launch_system_prompt(prompt_scope(mode), mcp, role))
}

fn codex_developer_instructions_arguments(prompt: &str) -> Vec<String> {
    vec![
        "-c".to_owned(),
        format!("developer_instructions={}", toml_basic_string(prompt)),
    ]
}

/// Renders a TOML basic string without involving a shell. The prompt contains
/// newlines, and callers may supply quotes, backslashes, or control characters,
/// so every character TOML forbids literally is escaped.
fn toml_basic_string(text: &str) -> String {
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r#"'"'"'"#))
}

fn claude_mcp_arguments(command: &Path, local_llm_model: Option<&str>) -> Result<Vec<String>, ()> {
    let command = command.to_str().ok_or(())?;
    Ok(claude_product_mcp_arguments(command, local_llm_model))
}

/// What the MCP server this launch injects will expose: the tool families it
/// registers and the local-LLM model it wires beside itself.
///
/// The local-LLM model is held as the single `Option`, so "the delegation server
/// is wired" and "a model was chosen" cannot disagree.
struct ConfiguredMcpTools {
    issue: bool,
    memory: bool,
    local_llm_model: Option<String>,
}

impl ConfiguredMcpTools {
    /// The families the injected server registers, as the prompt describes them.
    fn families(&self) -> McpToolFamilies {
        McpToolFamilies {
            issue: self.issue,
            memory: self.memory,
            local_llm: self.local_llm_model.is_some(),
        }
    }

    fn model(&self) -> Option<&str> {
        self.local_llm_model.as_deref()
    }
}

/// Resolve the effective MCP tool configuration for one launch.
///
/// Two authorities, each the one the MCP server itself uses. Issue and memory
/// availability is the Global baseline overlaid with the *registered* workspace's
/// `.usagi/settings.json` — the same two layers `usagi mcp` resolves — and that
/// file lives only in the registered root, never in a session worktree. The
/// local-LLM model stays Global-only, which `with_local` preserves by not owning
/// it. A hand-edited model has already been sanitized by
/// [`Storage::load_settings`].
///
/// Global settings live in the *selected* directory — that is where
/// `Storage::open_default` and the daemon's own [`UserEnvironment`] write them —
/// so this reads the same file those writers own, not the mode-neutral base.
///
/// Unreadable settings fail the launch, exactly as they fail `usagi mcp` before
/// its serve loop starts. Falling back to the defaults here would launch an agent
/// whose prompt advertises tools its own MCP server could not register.
fn configured_mcp_tools(
    data_home: &paths::DataHome,
    workspace_root: &Path,
) -> Result<ConfiguredMcpTools, ()> {
    let resolve = || -> anyhow::Result<ConfiguredMcpTools> {
        let global = Storage::new(data_home.selected()).load_settings()?;
        let local = WorkspaceSettingsStore::new(workspace_root).load()?;
        let effective = global.with_local(&local);
        Ok(ConfiguredMcpTools {
            issue: effective.issue_enabled,
            memory: effective.memory_enabled,
            local_llm_model: effective
                .local_llm
                .enabled
                .then_some(effective.local_llm.model),
        })
    };
    resolve().map_err(|error| {
        ErrorLog::record(&format!(
            "could not resolve MCP tool settings for {}: {error}",
            workspace_root.display()
        ));
    })
}

/// Product-owned, non-secret pre-spawn readiness boundary.  Implementations
/// may discover an executable and invoke its public status command, but never
/// read, persist, or return credentials, configuration paths, argv, or raw OS
/// failures.  Keeping it injected makes the root composable with fixture
/// executables without installing or authenticating a real CLI.
trait AgentReadinessProbe: Send + Sync {
    fn observe(&self, product: &str) -> AgentReadiness;
}

const AGENT_READINESS_TIMEOUT: Duration = Duration::from_secs(2);
const AGENT_READINESS_TERMINATE_GRACE: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum AgentReadiness {
    Ready,
    #[default]
    Unavailable,
}

#[derive(Default)]
struct ReadinessSlot {
    running: bool,
    result: Option<AgentReadiness>,
}

#[derive(Default)]
struct ReadinessState {
    providers: BTreeMap<String, ReadinessSlot>,
}

/// Runs at most one bounded status child per provider. Callers arriving during
/// that run share its safe success/failure result instead of creating another
/// process or reader thread.
struct SystemAgentReadiness {
    state: Mutex<ReadinessState>,
    completed: Condvar,
    timeout: Duration,
    terminate_grace: Duration,
}

impl Default for SystemAgentReadiness {
    fn default() -> Self {
        Self {
            state: Mutex::new(ReadinessState::default()),
            completed: Condvar::new(),
            timeout: AGENT_READINESS_TIMEOUT,
            terminate_grace: AGENT_READINESS_TERMINATE_GRACE,
        }
    }
}

impl AgentReadinessProbe for SystemAgentReadiness {
    fn observe(&self, product: &str) -> AgentReadiness {
        // Which products exist, and which status command proves each one usable,
        // is the shared agent CLI vocabulary owned by core domain settings. This
        // root only runs the resolved probe, so the Codex-compatible
        // `codex-fugu` behind the `sakana-ai` profile is recognised without a
        // second table here (#609). An unmodelled product still fails closed.
        let Some(probe) = DefaultModel::readiness_command_for(product) else {
            return AgentReadiness::Unavailable;
        };
        self.ready_command(product, probe.program(), probe.arguments())
    }
}

impl SystemAgentReadiness {
    fn ready_command(&self, product: &str, program: &str, arguments: &[&str]) -> AgentReadiness {
        let Ok(mut state) = self.state.lock() else {
            return AgentReadiness::Unavailable;
        };
        let slot = state.providers.entry(product.to_owned()).or_default();
        if slot.running {
            let Ok((state_after_wait, timeout)) = self.completed.wait_timeout_while(
                state,
                self.timeout + self.terminate_grace,
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

        let result =
            bounded_readiness_command(program, arguments, self.timeout, self.terminate_grace);
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

fn bounded_readiness_command(
    program: &str,
    arguments: &[&str],
    timeout: Duration,
    terminate_grace: Duration,
) -> AgentReadiness {
    readiness_from_observation(&observe(
        program,
        arguments,
        ChildPolicy {
            timeout,
            terminate_grace,
            output_limit: 16 * 1024,
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
fn effective_role_instruction(
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

/// Resolves the workspace a connecting client will act on, adopting it when the
/// client selected one this daemon does not hold yet.
///
/// This is the point where "which workspace does this daemon serve?" stops being
/// a start-up constant. What each declaration means is unchanged
/// ([4. IPC の workspace fence](../../document/04-ipc.md#workspace-fence)); only
/// the answer's source moves from one fixed root to the tenant registry.
struct TenantWorkspaces {
    tenants: Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
    /// Where this data directory keeps the state subtree of every workspace it
    /// has opened. A bound client inside one of them is resolved against that
    /// record even when the workspace is no longer held.
    daemon_dir: PathBuf,
    /// The workspace this process started in. A client that names no workspace
    /// touches no workspace resource, so it is admitted against this one.
    initial: PathBuf,
}

impl TenantWorkspaces {
    /// The canonical spelling of a declared root, or the typed refusal for a
    /// root that cannot be resolved on this machine.
    fn canonical(root: &str) -> Result<PathBuf, usagi_core::infrastructure::ipc::ProtocolError> {
        paths::canonical_workspace_root(root).map_err(|_| {
            usagi_core::infrastructure::ipc::workspace_refusal(
                "the declared workspace does not resolve on this machine",
                root,
            )
        })
    }

    /// Every workspace this daemon currently holds, in wire spelling.
    ///
    /// A refusal names these rather than one fixed root, so a reader can tell
    /// whether the workspace they meant is among them.
    fn served(&self) -> Vec<String> {
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
    (!is_session_worktree_path(path) && path.join(".git").exists()).then(|| path.to_path_buf())
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
                let opening = workspace_state::owner(&self.daemon_dir, &declared)
                    .ok()
                    .flatten()
                    .map(|known| known.root().to_path_buf())
                    .or_else(|| adoptable_workspace_root(&declared))
                    .ok_or_else(|| {
                        usagi_core::infrastructure::ipc::workspace_refusal_serving(
                            &format!(
                                "this daemon has not opened {}; run this from a repository root \
                                 to open it, or open it explicitly with `usagi open {}`",
                                paths::wire_workspace_root(&declared),
                                paths::wire_workspace_root(&declared)
                            ),
                            &self.served(),
                        )
                    })?;
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

/// The workspace a connection acts on, once its handshake resolved one.
///
/// The handshake has already adopted or refused; this is the lookup of what it
/// settled on. A miss means the workspace was retired between the two steps, and
/// the connection is closed rather than served another workspace's state.
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

/// Resolves the complete client fence for a generic terminal. Unlike the Agent
/// resolver, generic terminal requests already carry a worktree ID, so the
/// runtime verifies that exact identity before admitting a PTY spawn.
struct SharedTerminalScopeResolver(Workspaces);
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
struct SharedAgentState {
    owner: Mutex<RootAgentRuntime>,
    readiness: Arc<dyn AgentReadinessProbe>,
}

impl SharedAgentState {
    fn lock(&self) -> LockResult<MutexGuard<'_, RootAgentRuntime>> {
        self.owner.lock()
    }
}

type SharedAgentRuntime = Arc<SharedAgentState>;
type SharedSupervisorRuntime = Arc<Mutex<SupervisorRuntime>>;

struct DeferredDecisionWaker;
impl DecisionWaker for DeferredDecisionWaker {
    fn wake(&mut self, _: &DecisionWake) -> anyhow::Result<()> {
        anyhow::bail!("parent agent wake adapter is unavailable")
    }
}

/// Locks the shared Agent owner for one terminal request; a poisoned lock is a
/// safe unavailable error rather than a client-side fallback.
struct SharedAgent {
    runtime: SharedAgentRuntime,
    disconnected: SyncSender<ConnectionId>,
}
impl AgentTerminalActor for SharedAgent {
    fn handle(
        &mut self,
        context: usagi_daemon::usecase::terminal_owner::TerminalRequestContext,
        request: usagi_core::usecase::client::TerminalRequest,
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
    fn disconnect(&mut self, connection: usagi_core::domain::id::ConnectionId) {
        // Connection workers must release their socket and JoinHandle promptly.
        // Runtime cleanup can be O(number of terminals) and contends with live
        // output/input, so serialize it on the daemon-owned cleanup worker
        // instead of leaving one blocked worker (and three socket descriptors)
        // behind for every short-lived client.
        let _ = self.disconnected.send(connection);
    }
}

enum AgentPtyObservation {
    Output(TerminalRef, Vec<u8>),
    /// The child is gone. Its identity proof rides along so that the durable
    /// exit is still committed by a process that can prove the child was its
    /// own, and the proof is released the instant that commit is behind us.
    Exited(TerminalRef, i32, Option<ChildRelease>),
    Shutdown,
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
    children: Arc<SpawnedChildren>,
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
        children: Arc<SpawnedChildren>,
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
        let (program, argv) = provisioned_agent_command(&plan.program, &plan.argv, provision);
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
        // The identity is observed before the watcher owns it, so the token this
        // thread carries is the very one the exit observation hands back. Every
        // way out of the thread — a drained reader, an unreadable wait, a
        // receiver that hung up — drops it, so no dead pid keeps its proof.
        let (identity, release) =
            self.children
                .observe(&UnixChildProbe, pid, "daemon-owned-agent-pty");
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
                let _ = observations.send(AgentPtyObservation::Exited(
                    output_terminal,
                    status,
                    release,
                ));
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

fn provisioned_agent_command(
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
    /// Carries the child's identity proof for the same reason the Agent
    /// observation does: the commit needs it, and nothing after the commit does.
    Exited(
        usagi_core::domain::id::TerminalRef,
        i32,
        Option<ChildRelease>,
    ),
    Shutdown,
}

struct DaemonPty {
    terminals: BTreeMap<String, OwnedPty>,
    selected: Option<String>,
    observations: SyncSender<PtyObservation>,
    metrics: Arc<TerminalPipelineMetrics>,
    children: Arc<SpawnedChildren>,
}
impl DaemonPty {
    fn new(
        metrics: Arc<TerminalPipelineMetrics>,
        children: Arc<SpawnedChildren>,
    ) -> (Self, Receiver<PtyObservation>) {
        let (observations, receiver) = mpsc::sync_channel(PTY_OBSERVATION_QUEUE_ITEMS);
        (
            Self {
                terminals: BTreeMap::new(),
                selected: None,
                observations,
                metrics,
                children,
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
        // As in the Agent spawner: the watcher thread owns the release token, so
        // the proof lives exactly as long as this child does.
        let (identity, release) = self
            .children
            .observe(&UnixChildProbe, pid, "daemon-owned-pty");
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
                    // The lifecycle owner dropped the observer. Do not move on
                    // to a child wait that could retain this reader forever;
                    // returning also releases the child-identity proof.
                    return;
                }
            }
            if let Ok(status) = exit_pty
                .lock()
                .map_or(Err(()), |pty| pty.wait().map_err(|_| ()))
            {
                let _ =
                    output_sender.send(PtyObservation::Exited(output_terminal, status, release));
            }
        });
        Ok(identity)
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
                ShardedTerminalStore,
                DaemonPty,
                SharedTerminalScopeResolver,
            >,
        >,
    >,
);
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

/// One absolute budget for reading and answering the complete first frame.
/// Established connections have their own policy and are deliberately not
/// subject to this deadline.
const PRE_HANDSHAKE_DEADLINE: Duration = Duration::from_secs(2);

/// Fallback when the process soft descriptor limit cannot be observed.
const CLIENT_CONNECTION_LIMIT_FALLBACK: usize = 32;
/// Established connections remain bounded even when the process has a very
/// large descriptor allowance: each one also owns a worker thread.
const CLIENT_CONNECTION_LIMIT_CEILING: usize = 256;
/// Descriptors reserved for PTYs, stores, wake pipes, listeners, and children.
const CLIENT_CONNECTION_RESERVED_FDS: u64 = 128;
/// Reader, writer, and retirement/shutdown descriptor retained per worker.
const CLIENT_CONNECTION_FDS: u64 = 3;

/// How often the decision maintenance worker makes due expiries durable and
/// drains the resolved-decision outbox.
///
/// This bounds how long an already expired decision can still be read as
/// `Pending`. A tick that finds nothing due performs two small reads and no
/// write: expiry no longer takes the store lock or fsyncs unless something
/// actually changed.
const DECISION_MAINTENANCE_TICK: Duration = Duration::from_millis(250);
/// Maximum time a synchronous decision waiter may take to observe a connection
/// cancellation when no decision state transition occurs.
const DECISION_CANCELLATION_POLL: Duration = Duration::from_millis(250);

struct DecisionWaiter {
    token: u64,
    notify: SyncSender<()>,
}

/// Process-local notification for durable decision state transitions.
///
/// The JSON document remains authoritative. This registry only prevents every
/// synchronous MCP waiter from reading that complete document forty times per
/// second while its decision is still pending.
#[derive(Default)]
struct DecisionWaiters {
    next_token: AtomicU64,
    waiting: Mutex<BTreeMap<usagi_core::domain::id::UserDecisionId, Vec<DecisionWaiter>>>,
}

impl DecisionWaiters {
    fn subscribe(
        self: &Arc<Self>,
        decision_id: usagi_core::domain::id::UserDecisionId,
    ) -> DecisionWaitSubscription {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let (notify, changes) = mpsc::sync_channel(1);
        self.waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(decision_id)
            .or_default()
            .push(DecisionWaiter { token, notify });
        DecisionWaitSubscription {
            registry: Arc::clone(self),
            decision_id,
            token,
            changes,
        }
    }

    fn notify(&self, decision_id: usagi_core::domain::id::UserDecisionId) {
        let waiters = self
            .waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&decision_id)
            .unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.notify.try_send(());
        }
    }

    fn unsubscribe(&self, decision_id: usagi_core::domain::id::UserDecisionId, token: u64) {
        let mut waiting = self
            .waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remove_entry = if let Some(waiters) = waiting.get_mut(&decision_id) {
            waiters.retain(|waiter| waiter.token != token);
            waiters.is_empty()
        } else {
            false
        };
        if remove_entry {
            waiting.remove(&decision_id);
        }
    }

    #[cfg(test)]
    fn waiting_count(&self, decision_id: usagi_core::domain::id::UserDecisionId) -> usize {
        self.waiting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&decision_id)
            .map_or(0, Vec::len)
    }
}

struct DecisionWaitSubscription {
    registry: Arc<DecisionWaiters>,
    decision_id: usagi_core::domain::id::UserDecisionId,
    token: u64,
    changes: Receiver<()>,
}

impl Drop for DecisionWaitSubscription {
    fn drop(&mut self) {
        self.registry.unsubscribe(self.decision_id, self.token);
    }
}

trait DecisionWaitCancellation {
    fn is_cancelled(&self) -> bool;
}

#[derive(Clone, Copy)]
struct DecisionWaitContext<'a> {
    waiters: &'a Arc<DecisionWaiters>,
    cancellation: &'a dyn DecisionWaitCancellation,
}

struct DecisionConnectionCancellation {
    connection: AcceptedStream,
    gate: AdmissionGate,
}

impl DecisionWaitCancellation for DecisionConnectionCancellation {
    fn is_cancelled(&self) -> bool {
        !self.gate.is_open(LeaseClass::ActiveControl) || self.connection.peer_disconnected()
    }
}

struct ProductionRefreshClock {
    started: Instant,
}

impl RefreshClock for ProductionRefreshClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

#[derive(Clone, Copy)]
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
impl usagi_daemon::usecase::terminal_owner::TerminalOwner for SharedTerminal {
    fn handle(
        &mut self,
        context: usagi_daemon::usecase::terminal_owner::TerminalRequestContext,
        request: usagi_core::usecase::client::TerminalRequest,
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

/// Serializes connection-local terminal cleanup away from socket workers.
///
/// A disconnect visits every terminal ledger. Long-lived chats can make that
/// visit contend with output and input requests; doing it in the connection
/// worker retains the accepted reader, writer, and retirement descriptor until
/// the mutex is acquired. One daemon-owned consumer bounds the contention and
/// lets the connection worker close all three descriptors immediately.
fn start_connection_cleanup_worker(
    agent: SharedAgentRuntime,
    terminal: SharedTerminalRuntime,
    disconnected: Receiver<ConnectionId>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    start_connection_cleanup_worker_with(disconnected, move |connection| {
        if let Ok(mut agent) = agent.lock() {
            AgentTerminalActor::disconnect(&mut *agent, connection);
        }
        if let Ok(mut terminal) = terminal.lock() {
            usagi_daemon::usecase::terminal_owner::TerminalOwner::disconnect(
                &mut *terminal,
                connection,
            );
        }
    })
}

fn start_connection_cleanup_worker_with(
    disconnected: Receiver<ConnectionId>,
    mut cleanup: impl FnMut(ConnectionId) + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-connection-cleanup".to_string())
        .spawn(move || {
            while let Ok(connection) = disconnected.recv() {
                cleanup(connection);
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

/// Owns every daemon-wide worker from its first successful spawn.
///
/// Startup errors and accept-loop unwinds therefore take the same close-and-join
/// path as planned shutdown instead of detaching the handles accumulated so far.
struct DaemonBackgroundWorkers {
    handles: Vec<std::thread::JoinHandle<()>>,
    shutdown: Arc<ShutdownRequest>,
    projection: Arc<PrProjectionQueue>,
    agent_observations: Option<SyncSender<AgentPtyObservation>>,
    terminal_observations: Option<SyncSender<PtyObservation>>,
}

impl DaemonBackgroundWorkers {
    fn new(shutdown: Arc<ShutdownRequest>, projection: Arc<PrProjectionQueue>) -> Self {
        Self {
            handles: Vec::new(),
            shutdown,
            projection,
            agent_observations: None,
            terminal_observations: None,
        }
    }

    fn bind_agent_observations(&mut self, sender: SyncSender<AgentPtyObservation>) {
        self.agent_observations = Some(sender);
    }

    fn bind_terminal_observations(&mut self, sender: SyncSender<PtyObservation>) {
        self.terminal_observations = Some(sender);
    }

    fn push(&mut self, handle: std::thread::JoinHandle<()>) {
        self.handles.push(handle);
    }

    fn shutdown_and_join(&mut self) {
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
    custody: Option<FsCustodyProbe>,
    hydrate_retained: bool,
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
    let (pty, observations) = DaemonPty::new(Arc::clone(&pipeline_metrics), Arc::clone(&children));
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
        hydrate_retained,
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
        hydrate_retained,
    )?;
    reconcile_removed_session_agents(&data_dir.join("daemon"), &agent)?;
    let supervisor = Arc::new(Mutex::new(SupervisorRuntime::new(&data_dir.join("daemon"))));
    if let Ok(runtime) = supervisor.lock()
        && let Err(error) = runtime.tick_all(chrono::Utc::now(), &mut DeferredDecisionWaker)
    {
        ErrorLog::record(&format!(
            "supervisor startup reconciliation deferred: {error}"
        ));
    }
    background_workers.push(start_agent_observer(
        Arc::downgrade(&agent),
        agent_observations,
        Arc::clone(&projection),
        Arc::clone(&supervisor),
        Arc::clone(&shutdown),
    )?);
    // Socket workers only enqueue connection-local ledger cleanup. The single
    // consumer prevents a disconnect storm from retaining one accepted socket
    // triplet per worker while all of them contend on the terminal owners.
    let (disconnected, disconnects) = mpsc::sync_channel(client_connection_limit());
    let connection_cleanup =
        start_connection_cleanup_worker(Arc::clone(&agent), Arc::clone(&terminal), disconnects)?;
    background_workers.push(start_pr_projection_worker(
        Arc::clone(&pr_inventory),
        Arc::clone(&projection),
        Arc::clone(&shutdown),
    )?);
    let decisions = Arc::new(UserDecisionStore::new(data_dir.join("daemon")));
    let decision_waiters = Arc::new(DecisionWaiters::default());
    consume_user_decision_events(&decisions)
        .map_err(|error| std::io::Error::other(error.message))?;
    background_workers.push(start_decision_maintenance(
        Arc::clone(&decisions),
        Arc::clone(&decision_waiters),
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
        Arc::clone(&shutdown),
    )?;
    background_workers.push(teardown_worker);
    // Workspaces adopted for a client that has gone away are given back, so a
    // daemon that served many of them over a day does not still own them all.
    background_workers.push(start_tenant_retire_worker(
        Arc::clone(&tenants),
        DaemonWorkspaceActivity {
            terminal: Arc::clone(&terminal),
            agent: Arc::clone(&agent),
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
        open_runtime_state(data_dir, daemon_generation, &children)?,
        Arc::clone(&shutdown),
    )?);
    background_workers.push(start_draining_collection_worker(
        open_runtime_state(data_dir, daemon_generation, &children)?,
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
        data_dir.to_path_buf(),
        initial,
        workspaces,
        resolver,
        teardown,
        terminal,
        agent,
        retention,
        pr_inventory,
        projection,
        decisions,
        decision_waiters,
        Arc::new(Mutex::new(MetricsBroker::with_runtime_health(
            agent_concurrency,
            shutdown.background_worker_health(),
        ))),
        Arc::new(Mutex::new(ProcessResourceSampler { previous: None })),
        pipeline_metrics,
        supervisor,
        fence,
        workers,
        disconnected,
        connection_cleanup,
        background_workers,
        shutdown,
    )
}

/// Removes Agent records whose managed session was already retired by an
/// older daemon. This startup pass repairs the historical state where session
/// teardown removed the lifecycle row without closing its Agent owner.
fn reconcile_removed_session_agents(
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

/// Retain one accepted connection's worker so a collection can unblock and join it.
///
/// A worker whose shutdown half could not be duplicated is deliberately *not*
/// retained: a collection could never unblock it, so pretending it is joinable
/// would park retirement. Production accept loops fail closed before spawning
/// in that case; the error branch remains defensive for injected callers.
fn retain_client_worker(
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

/// Reap completed workers before deciding whether another accepted connection
/// can acquire daemon-owned descriptors and a thread.
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

fn client_connection_limit() -> usize {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: `limit` points to writable storage for one `rlimit`, and the
    // successful call initializes it before `assume_init`.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return CLIENT_CONNECTION_LIMIT_FALLBACK;
    }
    // SAFETY: the successful `getrlimit` above initialized the value.
    let limit = unsafe { limit.assume_init() };
    if limit.rlim_cur == libc::RLIM_INFINITY {
        CLIENT_CONNECTION_LIMIT_CEILING
    } else {
        client_connection_limit_from_nofile(limit.rlim_cur)
    }
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
    daemon_dir: PathBuf,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_pr_refresh_worker(
        pr_inventory,
        Some(daemon_dir),
        shutdown,
        GhProcess,
        ProductionRefreshClock {
            started: Instant::now(),
        },
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

fn spawn_pr_refresh_worker<R, C>(
    pr_inventory: SharedPrInventory,
    daemon_dir: Option<PathBuf>,
    shutdown: Arc<ShutdownRequest>,
    runner: R,
    clock: C,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    R: GhProcessPort + Clone + Send + 'static,
    C: RefreshClock + Send + 'static,
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
fn start_session_teardown_worker(
    workspaces: Workspaces,
    agent: SharedAgentRuntime,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<(Arc<TeardownSignal>, std::thread::JoinHandle<()>)> {
    let signal = Arc::new(TeardownSignal::new());
    let worker = spawn_session_teardown_worker(
        WorkspacesTeardown { workspaces },
        AgentAndWorktreeTeardown {
            agent,
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
struct WorkspacesTeardown {
    workspaces: Workspaces,
}

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

/// Orders session destruction so no Agent process or durable Agent inventory
/// can outlive the worktree scope it belongs to.
struct AgentAndWorktreeTeardown<E> {
    agent: SharedAgentRuntime,
    worktree: E,
}

impl<E: TeardownEffect> TeardownEffect for AgentAndWorktreeTeardown<E> {
    fn tear_down(&self, teardown: &PendingTeardown) -> Result<(), String> {
        self.agent
            .lock()
            .map_err(|_| "agent owner is unavailable".to_owned())?
            .close_session(teardown.session_id)
            .map_err(|error| error.message)?;
        self.worktree.tear_down(teardown)
    }
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
}

impl usagi_daemon::usecase::tenant::WorkspaceActivity<SharedSessionRuntime>
    for DaemonWorkspaceActivity
{
    fn has_work(
        &self,
        workspace: usagi_core::domain::id::WorkspaceId,
        runtime: &SharedSessionRuntime,
    ) -> bool {
        let running_terminal = self.terminal.lock().map_or(true, |terminal| {
            terminal.has_running_in_workspace(workspace)
        });
        let running_agent = self
            .agent
            .lock()
            .map_or(true, |agent| agent.has_running_agent(workspace));
        let unfinished = runtime.lock().map_or(true, |runtime| {
            runtime.has_unfinished_work().unwrap_or(true)
        });
        running_terminal || running_agent || unfinished
    }
}

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

fn spawn_tenant_retire_worker<A>(
    tenants: Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
    activity: A,
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
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
            worker_health.finish_planned();
        })
}

fn start_custody_worker(
    probe: FsCustodyProbe,
    owner: DaemonRecord,
    data_dir: PathBuf,
    gate: AdmissionGate,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_custody_worker(probe, owner, data_dir, gate, shutdown, CUSTODY_TICK)
}

fn spawn_custody_worker<P>(
    probe: P,
    owner: DaemonRecord,
    data_dir: PathBuf,
    gate: AdmissionGate,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>>
where
    P: CustodyProbe + Send + 'static,
{
    std::thread::Builder::new()
        .name("usagi-daemon-custody".to_string())
        .spawn(move || {
            let worker_health = shutdown.monitor_background_worker(BackgroundWorker::Custody);
            while !shutdown.is_requested() {
                // After a handoff this process deliberately no longer owns the
                // lifecycle record. Its authority is the draining registry
                // entry and the exact PTYs it still owns, so losing active
                // custody must not tear those PTYs down.
                if gate.role() != GenerationRole::Active {
                    if shutdown.wait_for_tick(tick) {
                        break;
                    }
                    continue;
                }
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
                        break;
                    }
                    // An undecidable observation is not a loss: keep serving and
                    // re-evaluate on the next tick.
                    Ok(Custody::Held) | Err(_) => {}
                }
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
            worker_health.finish_planned();
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
/// How quickly a generation notices that its last draining claim disappeared.
///
/// Resource exits already wake their own observers; this worker only bridges
/// the two durable documents (owner shard and global allocator) to process
/// lifetime, so a sub-second tick keeps retirement prompt without putting the
/// allocator lock on a hot path.
const DRAINING_COLLECTION_TICK: Duration = Duration::from_millis(250);

/// Starts the only production retention collector. Launch and exit already
/// collect on the spot; this worker covers an idle daemon, where the age budget
/// and the minimum visibility TTL are the only things still moving.
fn start_retention_gc_worker(
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
fn start_draining_collection_worker(
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
fn spawn_draining_collection_worker<C>(
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
    waiters: Arc<DecisionWaiters>,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_decision_maintenance(decisions, waiters, shutdown, DECISION_MAINTENANCE_TICK)
}

/// The loop, with the tick injected so a test can drive it without waiting out
/// the production cadence.
fn spawn_decision_maintenance(
    decisions: Arc<UserDecisionStore>,
    waiters: Arc<DecisionWaiters>,
    shutdown: Arc<ShutdownRequest>,
    tick: Duration,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-decision-maintenance".to_string())
        .spawn(move || {
            let worker_health =
                shutdown.monitor_background_worker(BackgroundWorker::DecisionMaintenance);
            while !shutdown.is_requested() {
                if let Ok(expired) = decisions.expire_due(chrono::Utc::now()) {
                    for decision_id in expired {
                        waiters.notify(decision_id);
                    }
                }
                let _ = consume_user_decision_events(&decisions);
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
            worker_health.finish_planned();
        })
}

fn repair_agent_codex_arg0_permissions(sandbox_home: Option<&Path>) {
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

#[allow(clippy::too_many_arguments)] // Composition injects each Agent dependency separately.
fn open_agent_runtime(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    workspaces: Workspaces,
    pty: AgentPty,
    mcp_command: PathBuf,
    environment: Arc<SharedUserEnvironment>,
    retention: usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention,
    concurrency: AgentConcurrencyGauge,
    children: &Arc<SpawnedChildren>,
    hydrate_retained: bool,
) -> std::io::Result<SharedAgentRuntime> {
    let state = open_runtime_state(data_dir, generation, children)?;
    let snapshot = if hydrate_retained {
        hydrate_runtime_state(&state, "agent runtime")?.agents
    } else {
        RuntimeStoreSnapshot::default()
    };
    let store = ShardedAgentStore::new(state);
    let mut registry = AdapterRegistry::new();
    let readiness: Arc<dyn AgentReadinessProbe> = Arc::new(SystemAgentReadiness::default());
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
    let sandbox_backend = super::cli::resolve_sandbox_backend(sandbox_platform);
    let sandbox_tmpdir = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .and_then(|path| path.canonicalize().ok());
    let sandbox_home = std::env::var_os("HOME")
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
            program: DefaultModel::OpenAi.command(),
            environment: Some(Arc::clone(&environment)),
            sandbox_backend: sandbox_backend.clone(),
            sandbox_tmpdir: sandbox_tmpdir.clone(),
            sandbox_home: sandbox_home.clone(),
            sandbox_cache_dir: sandbox_cache_dir.clone(),
            sandbox_passthrough,
        }),
        CodexAdapter::sakana(RootCodexProvisioner {
            workspaces: Arc::clone(&workspaces),
            mcp_command: mcp_command.clone(),
            data_home: data_home.clone(),
            program: DefaultModel::SakanaAi.command(),
            environment: Some(Arc::clone(&environment)),
            sandbox_backend: sandbox_backend.clone(),
            sandbox_tmpdir: sandbox_tmpdir.clone(),
            sandbox_home: sandbox_home.clone(),
            sandbox_cache_dir: sandbox_cache_dir.clone(),
            sandbox_passthrough,
        }),
        ClaudeAdapter::new(RootClaudeProvisioner {
            workspaces,
            mcp_command,
            data_home,
            sandbox_backend,
            sandbox_tmpdir,
            sandbox_home,
            sandbox_cache_dir,
            environment: Some(environment),
            // E2E テスト専用 seam。release ビルドでは `cfg!(debug_assertions)` が false になるため、
            // 配布バイナリは常に拘束された Claude だけを起動する。
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

fn start_agent_observer(
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
                        if let Ok(runtime) = supervisor.lock()
                            && let Err(error) =
                                runtime.tick_all(chrono::Utc::now(), &mut DeferredDecisionWaker)
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

/// Starts the only production PR projection worker.
///
/// It owns every scan and every durable inventory write that PTY output causes.
/// The queue's `recv` parks on a condvar and returns `None` once the queue is
/// closed and drained, so this thread has no timer and no polling.
fn start_pr_projection_worker(
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

fn spawn_critical_worker<R, F>(
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
                drop(monitor);
                shutdown.request();
                on_failure();
            }
            if let Err(payload) = result {
                panic::resume_unwind(payload);
            }
        })
}

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
fn trusted_repository_root(sessions: &SharedSessionRuntime) -> std::io::Result<PathBuf> {
    sessions
        .lock()
        .map(|sessions| sessions.repository_root().to_path_buf())
        .map_err(|_| std::io::Error::other("session runtime is unavailable"))
}

#[allow(clippy::too_many_arguments)] // Composition injects each terminal dependency separately.
fn new_terminal_runtime(
    data_dir: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    repo_root: PathBuf,
    pty: DaemonPty,
    workspaces: Workspaces,
    environment: Arc<SharedUserEnvironment>,
    retention: usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention,
    children: &Arc<SpawnedChildren>,
    hydrate_retained: bool,
) -> std::io::Result<SharedTerminalRuntime> {
    let state = open_runtime_state(data_dir, generation, children)?;
    let snapshot = if hydrate_retained {
        hydrate_runtime_state(&state, "generic terminal")?.terminals
    } else {
        TerminalStoreSnapshot::default()
    };
    let store = ShardedTerminalStore::new(state);
    let runtime = GenericTerminalRuntime::from_snapshot_with_retention(
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
    )
    .map_err(|_| std::io::Error::other("invalid generic terminal snapshot"))?;
    Ok(Arc::new(Mutex::new(runtime)))
}

fn start_terminal_observer<S, Q>(
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

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // Composition owns the independently injected daemon services.
fn start_ipc_accept_loop(
    listener: SecureUnixListener,
    server: usagi_core::infrastructure::ipc::ServerProtocol,
    data_dir: PathBuf,
    initial: usagi_daemon::usecase::tenant::Tenant<SharedSessionRuntime>,
    workspaces: Workspaces,
    resolver: Arc<TenantWorkspaces>,
    teardown: Arc<TeardownSignal>,
    terminal: SharedTerminalRuntime,
    agent: SharedAgentRuntime,
    retention: usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention,
    pr_inventory: SharedPrInventory,
    projection: Arc<PrProjectionQueue>,
    decisions: Arc<UserDecisionStore>,
    decision_waiters: Arc<DecisionWaiters>,
    metrics: SharedMetricsBroker,
    process_metrics: SharedProcessResourceSampler,
    pipeline_metrics: Arc<TerminalPipelineMetrics>,
    supervisor: SharedSupervisorRuntime,
    fence: Arc<GenerationFence>,
    workers: Arc<ClientWorkers>,
    disconnected: SyncSender<ConnectionId>,
    connection_cleanup: std::thread::JoinHandle<()>,
    mut background_workers: DaemonBackgroundWorkers,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<SecureUnixListener>> {
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
                        let Ok(peer_process) = peer_pid(&stream).and_then(|pid| {
                            let parent = parent_pid(pid)?;
                            process_group(pid).map(|process_group| (pid, parent, process_group))
                        }) else {
                            ErrorLog::record(
                                "daemon connection refused: peer process identity unavailable",
                            );
                            continue;
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
                        let connection_workspaces = Arc::clone(&workspaces);
                        let connection_resolver = Arc::clone(&resolver);
                        let teardown = Arc::clone(&teardown);
                        let terminal = Arc::clone(&terminal);
                        let visibility = visibility.clone();
                        let retention = retention.clone();
                        let agent_owner = Arc::clone(&agent);
                        let agent_launch = Arc::clone(&agent);
                        let pr_inventory = Arc::clone(&pr_inventory);
                        let decisions = Arc::clone(&decisions);
                        let decision_waiters = Arc::clone(&decision_waiters);
                        let metrics = Arc::clone(&metrics);
                        let process_metrics = Arc::clone(&process_metrics);
                        let pipeline_metrics = Arc::clone(&pipeline_metrics);
                        let supervisor = Arc::clone(&supervisor);
                        let connection_fence = Arc::clone(&fence);
                        let connection_data_dir = data_dir.clone();
                        let connection_disconnected = disconnected.clone();
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
                        let decision_cancellation = DecisionConnectionCancellation {
                            connection: unblock.clone(),
                            gate: connection_fence.gate.clone(),
                        };
                        let spawned = std::thread::Builder::new()
                            .name("usagi-ipc-client".to_string())
                            .spawn(move || {
                                // The retained shutdown descriptor must not keep
                                // the peer apparently open after this worker has
                                // returned. Completion shuts the shared socket on
                                // every early-return and established-connection
                                // exit; ClientWorkers still owns the handle needed
                                // to join the finished thread exactly once.
                                let _completion =
                                    ShutdownAcceptedStreamOnDrop(worker_completion);
                                if stream.set_nonblocking(false).is_err() {
                                    return;
                                }
                                let Ok(writer) = stream.try_clone() else {
                                    return;
                                };
                                let deadline = Instant::now() + PRE_HANDSHAKE_DEADLINE;
                                let mut reader = PreHandshakeDeadlineStream::new(stream, deadline);
                                let mut writer =
                                    PreHandshakeDeadlineStream::new(writer, deadline);
                                let admitted =
                                    usagi_daemon::presentation::ipc::handshake_admitted_with(
                                        &mut reader,
                                        &mut writer,
                                        &server,
                                        Some(connection_resolver.as_ref()),
                                    );
                                // Capacity covers the complete hello response, on
                                // every success/refusal/error path, but never the
                                // established connection that follows it.
                                drop(pre_handshake_permit);
                                let admitted = match admitted {
                                    Ok(Some(admitted)) => admitted,
                                    Ok(None) => return,
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
                                            "daemon pre-handshake connection refused: {reason}"
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
                                    ErrorLog::record(
                                        "daemon admitted connection closed: its workspace is no longer held",
                                    );
                                    return;
                                };
                                // A pre-handshake timeout must not become an idle
                                // policy for an admitted subscription. Failure to
                                // remove it fails this socket closed.
                                if reader.clear_deadlines().is_err()
                                    || writer.clear_deadlines().is_err()
                                {
                                    ErrorLog::record(
                                        "daemon admitted connection closed: pre-handshake deadline could not be cleared",
                                    );
                                    return;
                                }
                                // Established policy: no deadline, but every read
                                // is gated on a bounded readiness wait the worker
                                // uses to observe retirement. `shutdown(2)` alone
                                // can leave this thread parked forever, and the
                                // barrier would then never join it. The writer is
                                // untouched: a worker parks waiting for the next
                                // frame, not waiting to send one.
                                let mut reader = RetiringReader::new(
                                    reader.into_inner(),
                                    retirement,
                                    CLIENT_RETIREMENT_POLL,
                                );
                                let mut writer = writer.into_inner();
                                let mut owner =
                                    SharedTerminalOwner::with_visibility_and_retention(
                                        SharedAgent {
                                            runtime: agent_owner,
                                            disconnected: connection_disconnected,
                                        },
                                        SharedTerminal(terminal),
                                        visibility,
                                        retention,
                                    );
                                let mut metrics_observer = None;
                                let result = usagi_daemon::presentation::ipc::handle_admitted_connection_with_terminal_and(
                                    &mut reader,
                                    &mut writer,
                                    admitted,
                                    connection_fence.as_ref(),
                                    &mut owner,
                                    &mut |request_id, body, hello, _connection, client| {
                                        if let Some(credential) = request_mcp_credential(&body)
                                            && !agent_launch
                                                .lock()
                                                .is_ok_and(|runtime| runtime.authenticates_mcp_child(credential, peer_process.0))
                                        {
                                            return envelope(
                                                hello,
                                                request_id,
                                                usagi_core::infrastructure::ipc::ResponseOutcome::Error(
                                                    usagi_core::infrastructure::ipc::ProtocolError::new(
                                                        usagi_core::infrastructure::ipc::ErrorCode::OwnershipUnknown,
                                                        "MCP caller is not the claimed child process",
                                                    ),
                                                ),
                                                serde_json::Value::Null,
                                            );
                                        }
                                        match body.get("kind").and_then(serde_json::Value::as_str) {
                                        Some("mcp_child_claim") => dispatch_mcp_child_claim(&agent_launch, peer_process, request_id, &body, hello),
                                        Some("rollover") => dispatch_rollover(&connection_data_dir, connection_fence.as_ref(), request_id, &body, hello),
                                        Some("session") => dispatch_session(&bound, &teardown, &agent_launch, &pr_inventory, request_id, &body, hello),
                                        Some("agent" | "agent_inventory" | "resume_agent") => dispatch_agent(&agent_launch, &bound, request_id, &body, hello),
                                        Some("codex_session_capture") => dispatch_codex_session_capture(&agent_launch, peer_process.2, request_id, &body, hello),
                                        Some("agent_phase_report") => dispatch_agent_phase_report(&agent_launch, peer_process.2, request_id, &body, hello),
                                        Some("dispatch") => dispatch_dispatch(&agent_launch, &bound, request_id, &body, hello),
                                        Some("metrics") => dispatch_metrics(&metrics, &process_metrics, &pipeline_metrics, &mut metrics_observer, request_id, &body, hello),
                                        Some("pr" | "pr_batch" | "pr_dismiss") => dispatch_pr_snapshot(&pr_inventory, request_id, &body, hello),
                                        Some("dispatch_tool") => dispatch_dispatch_tool(&agent_launch, &bound, &decisions, DecisionWaitContext { waiters: &decision_waiters, cancellation: &decision_cancellation }, request_id, &body, hello),
                                        Some("supervisor_tool") => {
                                            let caller = authenticated_supervisor_caller(&agent_launch, &client, &body);
                                            dispatch_supervisor_tool(&supervisor, caller, request_id, &body, hello)
                                        },
                                        Some("user_decision") => dispatch_user_decision(&agent_launch, &bound, &decisions, DecisionWaitContext { waiters: &decision_waiters, cancellation: &decision_cancellation }, request_id, &body, hello),
                                        _ => usagi_daemon::presentation::ipc::dispatch(request_id, body, hello),
                                        }
                                    },
                                );
                                if let Some(observer) = metrics_observer
                                    && let Ok(mut broker) = metrics.lock()
                                {
                                    broker.unsubscribe(observer.subscription());
                                }
                                if let Ok(mut agent) = agent_launch.lock() {
                                    agent.release_mcp_child(peer_process.0);
                                }
                                let _ = result;
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
                    Err(_) => std::thread::sleep(ACCEPT_ERROR_BACKOFF),
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
            // Every connection worker has now returned and dropped its sender.
            // Closing the last producer drains all queued ledger cleanup before
            // the owner runtimes are allowed to leave the daemon generation.
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

/// A pipe whose readable end lets `poll(2)` wait for a shutdown request
/// alongside the listening socket.
///
/// A condvar cannot be mixed into a descriptor wait, so the request is mirrored
/// onto a descriptor. The mirroring thread parks on the condvar, which means an
/// idle daemon still performs no timed wakeups.
struct ShutdownPipe {
    read: OwnedFd,
    shutdown: Arc<ShutdownRequest>,
    worker: Option<std::thread::JoinHandle<()>>,
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
    fn wait_for_listener(&self, listener: std::os::fd::RawFd) -> bool {
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
    bound: &ConnectionWorkspace,
    decisions: &UserDecisionStore,
    wait: DecisionWaitContext<'_>,
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
        dispatch_agent_tool(agent, bound, request_id, body, hello)
    } else {
        dispatch_user_decision(agent, bound, decisions, wait, request_id, body, hello)
    }
}

#[allow(clippy::too_many_lines)] // One handler keeps authentication and durable routing atomic.
fn dispatch_agent_tool(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
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
        #[serde(default)]
        role: Option<usagi_core::domain::role::RoleId>,
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
        let snapshot = bound
            .sessions()
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
                let requested_role = input.session.role;
                drop(runtime);
                let created = bound
                    .sessions()
                    .lock()
                    .map_err(|_| {
                        ProtocolError::new(ErrorCode::Unavailable, "session runtime is unavailable")
                    })?
                    .handle(
                        usagi_core::usecase::client::SessionAction::Create,
                        &operation_id,
                        &serde_json::json!({"name": session_name, "role": requested_role}),
                    )
                    .map_err(|error| {
                        let code = if matches!(error, SessionRuntimeError::RoleConflict(..)) {
                            ErrorCode::RevisionConflict
                        } else {
                            ErrorCode::InvalidArgument
                        };
                        ProtocolError::new(code, error.safe_message())
                    })?;
                let session_id =
                    session_id_by_name(&created.body, &session_name).ok_or_else(|| {
                        ProtocolError::new(
                            ErrorCode::Unavailable,
                            "created session is not available",
                        )
                    })?;
                let scope = bound.scope_resolver();
                let dispatch_intent = DispatchIntent {
                    workspace,
                    session_name: session_name.clone(),
                    caller,
                    agent: selected,
                    prompt: input.prompt,
                };
                let admission = dispatch_agent_after_preflight(
                    agent,
                    &operation_id,
                    &dispatch_intent,
                    session_id,
                    &scope,
                )?;
                runtime = agent.lock().map_err(|_| {
                    ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
                })?;
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
                let session_metadata = snapshot
                    .get("sessions")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|items| {
                        items.iter().find(|item| {
                            item.get("session_id") == Some(&serde_json::json!(session_id))
                        })
                    });
                let role_id = session_metadata
                    .and_then(|item| item.get("role_id"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let role_summary = session_metadata
                    .and_then(|item| item.get("role_summary"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                Ok((
                    ResponseOutcome::Ok,
                    serde_json::json!({"session": input.name, "role_id": role_id, "role_summary": role_summary, "agents": agents}),
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
    caller: Result<String, usagi_core::infrastructure::ipc::ProtocolError>,
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
        caller_context: _,
    }) = parsed
    else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    let result = runtime
        .lock()
        .map_err(|_| {
            ProtocolError::new(ErrorCode::Unavailable, "supervisor runtime is unavailable")
        })
        .and_then(|runtime| {
            let caller = caller?;
            match action {
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
                        ProtocolError::new(
                            ErrorCode::Internal,
                            "supervisor response encoding failed",
                        )
                    })
                }
                SupervisorToolAction::Get => {
                    let input: RunPayload = serde_json::from_value(payload).map_err(|_| {
                        ProtocolError::new(
                            ErrorCode::InvalidArgument,
                            "invalid supervisor_get payload",
                        )
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
                        ProtocolError::new(
                            ErrorCode::Internal,
                            "supervisor response encoding failed",
                        )
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
                    let next_cursor = (offset + page.len() < runs.len())
                        .then(|| (offset + page.len()).to_string());
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
                        ProtocolError::new(
                            ErrorCode::Internal,
                            "supervisor response encoding failed",
                        )
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
                        ProtocolError::new(
                            ErrorCode::Internal,
                            "supervisor response encoding failed",
                        )
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

fn authenticated_supervisor_caller(
    agent: &SharedAgentRuntime,
    client: &usagi_core::domain::id::ClientId,
    body: &serde_json::Value,
) -> Result<String, usagi_core::infrastructure::ipc::ProtocolError> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};

    let credential = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::SupervisorTool { caller_context, .. } => caller_context,
            _ => None,
        })
        .filter(|context| !context.credential.is_empty())
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "supervisor caller provenance is unknown",
            )
        })?;
    let caller = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))?
        .mcp_dispatch_caller(&credential.credential)
        .ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::OwnershipUnknown,
                "supervisor caller provenance is unknown",
            )
        })?;
    Ok(supervisor_caller_descriptor(client, &caller))
}

fn supervisor_caller_descriptor(
    client: &usagi_core::domain::id::ClientId,
    caller: &usagi_core::domain::agent::CallerRef,
) -> String {
    let session = caller
        .session_id
        .map_or_else(|| "root".to_owned(), |session| session.to_string());
    format!(
        "ipc-client:{};session:{session};agent:{}",
        client, caller.agent_id
    )
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
            DaemonRequest::PrBatch { payload } => inventory
                .lock()
                .ok()
                .and_then(|mut projector| projector.snapshots(&payload.session_ids).ok())
                .and_then(|snapshots| serde_json::to_value(snapshots).ok()),
            DaemonRequest::PrDismiss { payload } => inventory
                .lock()
                .ok()
                .and_then(|mut projector| {
                    projector.dismiss(payload.session_id, &payload.url).ok()?;
                    projector.snapshot(payload.session_id).ok()
                })
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
#[derive(Debug)]
enum UserDecisionDispatchError {
    Decision(usagi_core::domain::user_decision::UserDecisionError),
    Cancelled,
}

impl From<usagi_core::domain::user_decision::UserDecisionError> for UserDecisionDispatchError {
    fn from(error: usagi_core::domain::user_decision::UserDecisionError) -> Self {
        Self::Decision(error)
    }
}

#[allow(clippy::too_many_lines)] // The complete wire-to-store error mapping is one atomic routing contract.
fn dispatch_user_decision(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
    store: &UserDecisionStore,
    wait: DecisionWaitContext<'_>,
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
        bound
            .sessions()
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
        let result = (|| -> Result<serde_json::Value, UserDecisionDispatchError> {
            match action {
                DispatchToolAction::UserDecisionRequest => {
                    let owner = owner.ok_or(UserDecisionError::Terminal)?;
                    let input = serde_json::from_value::<RequestPayload>(payload)
                        .map_err(|_| UserDecisionError::Terminal)?;
                    let decision = store
                        .create(UserDecision {
                            decision_id: UserDecisionId::new(),
                            owner,
                            title: input.title,
                            prompt: input.prompt,
                            options: input.options,
                            allow_freeform: input.allow_freeform,
                            expires_at: input.expires_at,
                            idempotency_key: input.idempotency_key,
                            status: UserDecisionStatus::Pending,
                            answer: None,
                            created_at: now,
                            resolved_at: None,
                        })
                        .map_err(|_| UserDecisionError::Terminal)??;
                    wait_for_user_decision(
                        store,
                        wait.waiters,
                        wait.cancellation,
                        workspace,
                        &decision,
                    )
                }
                DispatchToolAction::UserDecisionGet => {
                    let input = serde_json::from_value::<DecisionIdPayload>(payload)
                        .map_err(|_| UserDecisionError::Terminal)?;
                    Ok(serde_json::json!(decision_for(input.decision_id)?))
                }
                DispatchToolAction::UserDecisionList => {
                    let decisions = store
                        .pending(workspace)
                        .map_err(|_| UserDecisionError::Terminal)?;
                    let decisions = decisions
                        .into_iter()
                        .filter(|decision| {
                            owner
                                .as_ref()
                                .is_none_or(|expected| decision.owner == *expected)
                        })
                        .collect::<Vec<_>>();
                    Ok(serde_json::json!({"workspace": workspace, "decisions": decisions}))
                }
                DispatchToolAction::UserDecisionResolve => {
                    let input = serde_json::from_value::<ResolvePayload>(payload)
                        .map_err(|_| UserDecisionError::Terminal)?;
                    let _ = decision_for(input.decision_id)?;
                    let decision = store
                        .resolve(workspace, input.decision_id, input.answer, now)
                        .map_err(|_| UserDecisionError::Terminal)??;
                    wait.waiters.notify(input.decision_id);
                    Ok(serde_json::json!(decision))
                }
                DispatchToolAction::UserDecisionCancel | DispatchToolAction::UserDecisionExpire => {
                    let input = serde_json::from_value::<DecisionIdPayload>(payload)
                        .map_err(|_| UserDecisionError::Terminal)?;
                    let _ = decision_for(input.decision_id)?;
                    let status = if action == DispatchToolAction::UserDecisionCancel {
                        UserDecisionStatus::Cancelled
                    } else {
                        UserDecisionStatus::Expired
                    };
                    let decision = store
                        .terminal(workspace, input.decision_id, status, now)
                        .map_err(|_| UserDecisionError::Terminal)??;
                    wait.waiters.notify(input.decision_id);
                    Ok(serde_json::json!(decision))
                }
                _ => unreachable!(),
            }
        })();
        let value = result.map_err(|error| {
            let (code, message) = match error {
                UserDecisionDispatchError::Decision(UserDecisionError::IdempotencyConflict) => (
                    ErrorCode::IdempotencyConflict,
                    "decision idempotency key conflicts",
                ),
                UserDecisionDispatchError::Decision(UserDecisionError::InvalidOption) => {
                    (ErrorCode::InvalidArgument, "decision option is not allowed")
                }
                UserDecisionDispatchError::Decision(UserDecisionError::FreeformNotAllowed) => (
                    ErrorCode::InvalidArgument,
                    "freeform decision answer is not allowed",
                ),
                UserDecisionDispatchError::Decision(UserDecisionError::Expired) => {
                    (ErrorCode::DeadlineExceeded, "decision has expired")
                }
                UserDecisionDispatchError::Decision(UserDecisionError::Terminal) => (
                    ErrorCode::RevisionConflict,
                    "decision is not pending or is outside this workspace",
                ),
                // Nothing was written, so retrying after the backlog clears is
                // safe and is the intended response.
                UserDecisionDispatchError::Decision(UserDecisionError::PendingLimitReached) => (
                    ErrorCode::ResourceExhausted,
                    "this workspace already holds the maximum number of unanswered decisions; \
                     answer some before asking another",
                ),
                UserDecisionDispatchError::Cancelled => {
                    (ErrorCode::Cancelled, "decision wait was cancelled")
                }
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
    waiters: &Arc<DecisionWaiters>,
    cancellation: &dyn DecisionWaitCancellation,
    workspace: usagi_core::domain::id::WorkspaceId,
    requested: &usagi_core::domain::user_decision::UserDecision,
) -> Result<serde_json::Value, UserDecisionDispatchError> {
    use usagi_core::domain::user_decision::UserDecisionStatus;

    // Subscribe before the first authoritative read. A terminal transition that
    // races this setup either appears in that read or leaves a queued wakeup, so
    // no state edge can be missed.
    let subscription = waiters.subscribe(requested.decision_id);
    let mut refresh = true;
    loop {
        if cancellation.is_cancelled() {
            return Err(UserDecisionDispatchError::Cancelled);
        }
        if !refresh {
            match subscription
                .changes
                .recv_timeout(DECISION_CANCELLATION_POLL)
            {
                Ok(()) => refresh = true,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(UserDecisionDispatchError::Cancelled);
                }
            }
            continue;
        }
        let decision = decisions
            .get(workspace, requested.decision_id)
            .map_err(|_| usagi_core::domain::user_decision::UserDecisionError::Terminal)?
            .ok_or(usagi_core::domain::user_decision::UserDecisionError::Terminal)?;
        match decision.status {
            UserDecisionStatus::Pending => refresh = false,
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
                return Err(usagi_core::domain::user_decision::UserDecisionError::Terminal.into());
            }
            UserDecisionStatus::Expired => {
                return Err(usagi_core::domain::user_decision::UserDecisionError::Expired.into());
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
    bound: &ConnectionWorkspace,
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
        let mut runtime = bound.sessions().lock().map_err(|_| {
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
        let scope = bound.scope_resolver();
        dispatch_agent_after_preflight(agent, &operation_id, &intent, session_id, &scope)
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

fn dispatch_rollover(
    data_dir: &Path,
    fence: &GenerationFence,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};

    let operation = serde_json::from_value::<DaemonRequest>(body.clone())
        .ok()
        .and_then(|request| match request {
            DaemonRequest::Rollover { operation_id } => Some(OperationId(operation_id)),
            _ => None,
        });
    let Some(operation) = operation else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    let result = GenerationRegistryFile::new(data_dir)
        .map(|file| GenerationRegistry::new(file, DEFAULT_GENERATION_LIMIT))
        .map_err(|error| error.to_string())
        .and_then(|registry| {
            rollover_trigger::execute(
                &registry,
                &CurrentLocatorFile::new(data_dir),
                &fence.gate,
                &fence.ledger,
                &UnixStandbyProbe {
                    data_dir,
                    build: current_build(),
                },
                &operation,
            )
            .map_err(|error| error.to_string())
        });
    match result {
        Ok(outcome) => envelope(
            hello,
            request_id,
            ResponseOutcome::Accepted {
                operation_id: operation,
                operation_revision: 1,
            },
            serde_json::json!({"outcome": format!("{outcome:?}")}),
        ),
        Err(message) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(ProtocolError::new(ErrorCode::Busy, message)),
            serde_json::Value::Null,
        ),
    }
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
    bound: &ConnectionWorkspace,
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
        bound,
        teardown,
        agent,
        pr_inventory,
        action,
        &operation_id,
        &payload,
    );
    session_response_envelope(action, &payload, result, request_id, hello)
}

fn request_mcp_credential(body: &serde_json::Value) -> Option<&str> {
    body.get("caller_context")
        .and_then(|context| context.get("credential"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            body.get("payload")
                .and_then(|payload| payload.get("_caller_credential"))
                .and_then(serde_json::Value::as_str)
        })
}

fn dispatch_mcp_child_claim(
    agent: &SharedAgentRuntime,
    peer_process: (u32, u32, u32),
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError, ResponseOutcome};
    use usagi_core::usecase::client::DaemonRequest;

    let result = matches!(
        serde_json::from_value::<DaemonRequest>(body.clone()),
        Ok(DaemonRequest::McpChildClaim)
    )
    .then_some(())
    .ok_or_else(|| ProtocolError::new(ErrorCode::InvalidArgument, "invalid MCP child claim"))
    .and_then(|()| {
        agent
            .lock()
            .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))?
            .claim_mcp_child(peer_process.0, peer_process.1, peer_process.2)
    });
    match result {
        Ok(credential) => envelope(
            hello,
            request_id,
            ResponseOutcome::Ok,
            serde_json::json!({ "credential": credential }),
        ),
        Err(error) => envelope(
            hello,
            request_id,
            ResponseOutcome::Error(error),
            serde_json::Value::Null,
        ),
    }
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
        // A delegation answers with its own structured outcome: the caller has to
        // be able to tell a clean rejection from a session that is still there
        // because its worker's fate is unknown, and a code and a sentence cannot
        // carry that.
        Err(SessionRuntimeError::Delegation(failure)) => {
            let mut error =
                usagi_core::infrastructure::ipc::ProtocolError::new(failure.code, &failure.message);
            error.side_effect = if failure.reconcile.left_side_effect() {
                usagi_core::infrastructure::ipc::SideEffect::PartialOrUnknown
            } else {
                usagi_core::infrastructure::ipc::SideEffect::None
            };
            error.details = Some(failure.details());
            envelope(
                hello,
                request_id,
                ResponseOutcome::Error(error),
                serde_json::json!(null),
            )
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

fn exact_merged_pr_head(
    inventory: Option<usagi_core::usecase::client::PrSnapshot>,
    branch_head: Option<String>,
) -> Option<String> {
    inventory.and_then(|inventory| {
        branch_head.and_then(|head| {
            inventory.entries.into_iter().find_map(|entry| {
                (entry.state == usagi_core::domain::pr_inventory::PrState::Merged
                    && entry.head_oid.as_deref() == Some(head.as_str()))
                .then_some(head.clone())
            })
        })
    })
}

fn best_effort_merged_pr_head(
    inventory: &SharedPrInventory,
    session_id: SessionId,
    branch_head: Option<String>,
) -> Option<String> {
    // PR state is optional evidence for squash-merge branch deletion. If its
    // independent projection is unavailable, retain Git's safe `branch -d`
    // behavior instead of blocking worktree removal.
    let snapshot = inventory
        .lock()
        .ok()
        .and_then(|mut inventory| inventory.snapshot(session_id).ok());
    exact_merged_pr_head(snapshot, branch_head)
}

#[allow(clippy::too_many_lines)]
fn dispatch_session_action(
    bound: &ConnectionWorkspace,
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
        let revision = bound
            .sessions()
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
        bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .session_scope_by_id(session_id)
    };
    let named_session = |name: &str| {
        bound
            .sessions()
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
            let target = bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .session_scope_by_id(id)?;
            let resolver = bound.scope_resolver();
            let admission = resume_agent_after_preflight(
                agent,
                operation_id,
                exact_target.as_ref(),
                target.workspace_id,
                Some(id),
                &resolver,
            )
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
            let mut status = bound
                .sessions()
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
        SessionAction::DelegateBrief => reply(delegate_brief(
            bound,
            teardown,
            agent,
            operation_id,
            payload,
        )?),
        SessionAction::DelegateIssue => {
            let (name, prompt) = {
                let number = payload
                    .get("number")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or(SessionRuntimeError::InvalidRequest)?;
                let root = bound
                    .sessions()
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
            };
            let requested_role = payload.get("role").cloned();
            let created = bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .handle(
                    SessionAction::Create,
                    operation_id,
                    &serde_json::json!({"name": name, "role": requested_role}),
                )?;
            let id = bound
                .sessions()
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
        SessionAction::Create => {
            perform_create(bound.sessions(), &SystemGit, operation_id, payload)
        }
        // Remove goes further: it answers as soon as the session is durably
        // `Deleting` and hands the unbounded worktree teardown to the daemon's
        // teardown worker. Keeping the teardown on this connection would hold
        // the reply past every client attempt deadline for a session with a
        // multi-gigabyte `target/`.
        SessionAction::Remove => {
            let name = string("name")?;
            let (id, branch_head) = bound
                .sessions()
                .lock()
                .map_err(|_| SessionRuntimeError::Storage)?
                .removal_identity(name)?;
            let merged_head_oid = best_effort_merged_pr_head(pr_inventory, id, branch_head);
            perform_remove_with_merged_head(
                bound.sessions(),
                teardown,
                operation_id,
                payload,
                merged_head_oid,
            )
        }
        _ => bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?
            .handle(action, operation_id, payload),
    }
}

/// Reads a delegation's `agent` selector, which names a runtime and model and
/// nothing else.
///
/// An `agent.id` is refused rather than resolved. No existing Agent can belong to
/// a session the same request is about to create, so the dispatch ownership check
/// would reject every such selector — after the worktree already existed. The
/// tool schema no longer advertises that branch and this is the daemon-side half
/// of the same rule.
fn new_agent_selector(
    selector: Option<&serde_json::Value>,
) -> Result<
    (
        usagi_core::domain::agent::AgentProfileId,
        usagi_core::domain::agent::ModelSelector,
    ),
    SessionRuntimeError,
> {
    use usagi_core::domain::agent::{AgentProfileId, ModelSelector};

    let selector = selector
        .and_then(serde_json::Value::as_object)
        .filter(|selector| selector.len() == 2 && !selector.contains_key("id"))
        .ok_or(SessionRuntimeError::InvalidRequest)?;
    let field = |key: &str| {
        selector
            .get(key)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    Ok((
        serde_json::from_value::<AgentProfileId>(field("runtime"))
            .map_err(|_| SessionRuntimeError::InvalidRequest)?,
        serde_json::from_value::<ModelSelector>(field("model"))
            .map_err(|_| SessionRuntimeError::InvalidRequest)?,
    ))
}

/// Creates a triage session for a brief and dispatches a fresh worker into it,
/// as one operation that either takes effect completely or leaves nothing.
///
/// The order is what makes that true. Every rejection the daemon can decide
/// without a side effect — the selector, the caller, the runtime/model
/// allowlist, the runtime executable, an operation that already owns an
/// admission — is decided before the worktree exists. Only after that does the
/// create run, and a dispatch that then fails definitively is rolled back by the
/// same durable teardown `session_remove` uses, which the daemon resumes across
/// a restart. A dispatch whose spawn outcome is *unknown* is deliberately not
/// rolled back: the worktree may already hold a running worker, so the caller
/// gets the session and run identity to reconcile instead.
fn delegate_brief(
    bound: &ConnectionWorkspace,
    teardown: &TeardownSignal,
    agent: &SharedAgentRuntime,
    operation_id: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, SessionRuntimeError> {
    use usagi_core::usecase::client::{DispatchAgentIntent, DispatchIntent};

    let string = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(SessionRuntimeError::InvalidRequest)
    };
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
    let prompt = format!(
        "このセッションの worktree 内で次の依頼をトリアージし、必要なら issue 化して実装へつなげてください。リポジトリの規約に従ってください。\n\n{brief}"
    );
    let (runtime, model) = new_agent_selector(payload.get("agent"))?;

    let credential = string("_caller_credential")?;
    let (workspace, caller, repository_root) = {
        let runtime = agent.lock().map_err(|_| SessionRuntimeError::Storage)?;
        let caller = runtime
            .mcp_dispatch_caller(credential)
            .ok_or(SessionRuntimeError::ScopeUnavailable)?;
        let sessions = bound
            .sessions()
            .lock()
            .map_err(|_| SessionRuntimeError::Storage)?;
        let workspace = sessions
            .snapshot()
            .map_err(|_| SessionRuntimeError::Storage)?
            .get("workspace_id")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or(SessionRuntimeError::Storage)?;
        (workspace, caller, sessions.repository_root().to_path_buf())
    };
    // Machine-local runtime/model policy belongs to the workspace root and is
    // not copied into managed worktrees. Decide every read-only refusal here;
    // `dispatch` still re-reads the same trusted root and stays the authority.
    agent
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?
        .preflight_dispatch(operation_id, &prompt, &runtime, &model, &repository_root)
        .map_err(|error| SessionRuntimeError::AgentFailure {
            code: error.code,
            message: error.message,
        })?;

    let created = perform_delegated_create(
        bound.sessions(),
        &SystemGit,
        operation_id,
        &serde_json::json!({"name": name, "role": payload.get("role").cloned()}),
    )?;
    let id = bound
        .sessions()
        .lock()
        .map_err(|_| SessionRuntimeError::Storage)?
        .session_id(&name)?;
    let scope = bound.scope_resolver();
    let dispatch_intent = DispatchIntent {
        workspace,
        session_name: name.clone(),
        caller,
        agent: DispatchAgentIntent::New { runtime, model },
        prompt,
    };
    let admission =
        dispatch_agent_after_preflight(agent, operation_id, &dispatch_intent, id, &scope);
    let admission = match admission {
        Ok(admission) => admission,
        Err(error) => {
            return Err(compensate_delegation(
                bound.sessions(),
                teardown,
                id,
                &name,
                operation_id,
                error,
            ));
        }
    };
    Ok(serde_json::json!({
        "name": name,
        "session_id": id,
        "created": created.body,
        "run_id": admission.operation_id,
        "terminal": admission.terminal,
        "completed": admission.completed,
    }))
}

/// Rolls a delegated create back, or reports why it must not be rolled back.
///
/// The teardown is admitted under a fresh operation identity because the
/// delegation's own identity already names the create it is compensating. Once
/// admitted it is durable: the daemon's teardown worker finishes it, and a
/// daemon that dies first resumes it from the `Deleting` record on the next
/// start.
fn compensate_delegation(
    sessions: &SharedSessionRuntime,
    teardown: &TeardownSignal,
    session_id: usagi_core::domain::id::SessionId,
    name: &str,
    run_operation_id: &str,
    error: usagi_core::infrastructure::ipc::ProtocolError,
) -> SessionRuntimeError {
    use usagi_daemon::usecase::session_runtime::{DelegationFailure, DelegationReconcile};

    let reconcile = if error.code == usagi_core::infrastructure::ipc::ErrorCode::OwnershipUnknown {
        DelegationReconcile::Retained
    } else {
        match perform_compensating_remove(
            sessions,
            teardown,
            &usagi_core::domain::id::OperationId::new().to_string(),
            name,
        ) {
            // A session that is already gone needs no compensation: an earlier
            // attempt's teardown removed it, so nothing was left behind either
            // way.
            Ok(_) | Err(SessionRuntimeError::UnknownSession) => DelegationReconcile::Compensated,
            Err(_) => DelegationReconcile::CompensationFailed,
        }
    };
    SessionRuntimeError::Delegation(DelegationFailure {
        code: error.code,
        message: error.message,
        session_id,
        run_operation_id: run_operation_id.to_owned(),
        reconcile,
    })
}

/// Compensates delegated creates whose dispatch never became durable.
///
/// A delegation builds its worktree before it can dispatch into it, so a daemon
/// that died inside that window left an available session no caller owns and no
/// run points at. This runs before the daemon accepts connections, so no client
/// ever observes such a session, and it uses the same durable teardown a live
/// compensation does.
///
/// A reservation in the dispatch store — even one a restart already failed — is
/// not an orphan: that operation reached the dispatch side, which owns its
/// outcome. Only a create with nothing at all behind it is rolled back.
fn reconcile_orphan_delegations(
    bound: &ConnectionWorkspace,
    dispatch: &DispatchStore,
    teardown: &TeardownSignal,
) -> usize {
    let Ok(candidates) = bound
        .sessions()
        .lock()
        .map_err(|_| ())
        .and_then(|sessions| sessions.delegated_sessions().map_err(|_| ()))
    else {
        return 0;
    };
    candidates
        .into_iter()
        .filter(|candidate| {
            matches!(dispatch.run(candidate.operation_id), Ok(None))
                && matches!(dispatch.admission(candidate.operation_id), Ok(None))
        })
        .filter(|candidate| {
            perform_compensating_remove(
                bound.sessions(),
                teardown,
                &usagi_core::domain::id::OperationId::new().to_string(),
                &candidate.name,
            )
            .is_ok()
        })
        .count()
}

enum AgentDispatchRequest {
    Launch(String, usagi_core::usecase::client::AgentLaunchIntent),
    Inventory(WorkspaceId),
    Resume(String, usagi_core::domain::agent::AgentResumeTarget),
}

fn admit_agent_dispatch_request(
    agent: &SharedAgentRuntime,
    scope: &dyn SessionScopeResolver,
    request: &AgentDispatchRequest,
) -> Result<
    usagi_daemon::usecase::agent_ipc::AgentAdmission,
    usagi_core::infrastructure::ipc::ProtocolError,
> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    let preflight = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))
        .and_then(|owner| match request {
            AgentDispatchRequest::Launch(operation_id, intent) => {
                owner.prepare_launch_readiness(operation_id, intent)
            }
            AgentDispatchRequest::Resume(operation_id, target) => {
                owner.prepare_resume_readiness(operation_id, target)
            }
            AgentDispatchRequest::Inventory(_) => unreachable!("inventory is read-only"),
        })?;
    run_agent_readiness(agent, preflight.as_ref())?;
    agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))
        .and_then(|mut owner| match request {
            AgentDispatchRequest::Launch(operation_id, intent) => {
                owner.launch_after_readiness(operation_id, intent, scope, preflight.as_ref())
            }
            AgentDispatchRequest::Resume(operation_id, target) => {
                owner.resume_exact_after_readiness(operation_id, target, scope, preflight.as_ref())
            }
            AgentDispatchRequest::Inventory(_) => unreachable!("inventory is read-only"),
        })
}

fn dispatch_agent(
    agent: &SharedAgentRuntime,
    bound: &ConnectionWorkspace,
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
            } => Some(AgentDispatchRequest::Launch(operation_id, intent)),
            DaemonRequest::AgentInventory { workspace } => {
                Some(AgentDispatchRequest::Inventory(workspace))
            }
            DaemonRequest::ResumeAgent {
                operation_id,
                target,
            } => Some(AgentDispatchRequest::Resume(operation_id, target)),
            _ => None,
        });
    let Some(request) = request else {
        return usagi_daemon::presentation::ipc::dispatch(request_id, body.clone(), hello);
    };
    let scope = bound.scope_resolver();
    if let AgentDispatchRequest::Inventory(workspace) = &request {
        let result = agent
            .lock()
            .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"));
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
    // The first owner visit captures immutable facts, the provider command runs
    // after its guard is dropped, and the second visit repeats every fence.
    let result = admit_agent_dispatch_request(agent, &scope, &request);
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

fn run_agent_readiness(
    agent: &SharedAgentRuntime,
    preflight: Option<&AgentReadinessPreflight>,
) -> Result<(), usagi_core::infrastructure::ipc::ProtocolError> {
    let Some(preflight) = preflight else {
        return Ok(());
    };
    match agent.readiness.observe(preflight.product()) {
        AgentReadiness::Ready => Ok(()),
        AgentReadiness::Unavailable => Err(usagi_core::infrastructure::ipc::ProtocolError::new(
            usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
            "agent CLI is unavailable or not authenticated; install it and sign in, then retry",
        )),
    }
}

fn dispatch_agent_after_preflight(
    agent: &SharedAgentRuntime,
    operation_id: &str,
    intent: &usagi_core::usecase::client::DispatchIntent,
    session: SessionId,
    scope: &dyn SessionScopeResolver,
) -> Result<
    usagi_daemon::usecase::agent_ipc::AgentAdmission,
    usagi_core::infrastructure::ipc::ProtocolError,
> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    let preflight = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))?
        .prepare_dispatch_readiness(operation_id, intent)?;
    run_agent_readiness(agent, preflight.as_ref())?;
    agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))?
        .dispatch_after_readiness(operation_id, intent, session, scope, preflight.as_ref())
}

fn resume_agent_after_preflight(
    agent: &SharedAgentRuntime,
    operation_id: &str,
    target: Option<&usagi_core::domain::agent::AgentResumeTarget>,
    workspace: WorkspaceId,
    session: Option<SessionId>,
    scope: &dyn SessionScopeResolver,
) -> Result<
    usagi_daemon::usecase::agent_ipc::AgentAdmission,
    usagi_core::infrastructure::ipc::ProtocolError,
> {
    use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
    let preflight = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))
        .and_then(|owner| match target {
            Some(target) => owner.prepare_resume_readiness(operation_id, target),
            None => owner.prepare_legacy_resume_readiness(operation_id, workspace, session),
        })?;
    run_agent_readiness(agent, preflight.as_ref())?;
    agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))
        .and_then(|mut owner| match target {
            Some(target) => {
                owner.resume_exact_after_readiness(operation_id, target, scope, preflight.as_ref())
            }
            None => owner.resume_legacy_after_readiness(
                operation_id,
                workspace,
                session,
                scope,
                preflight.as_ref(),
            ),
        })
}

fn dispatch_codex_session_capture(
    agent: &SharedAgentRuntime,
    process_group: u32,
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
    let result = agent
        .lock()
        .map_err(|_| ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable"))
        .and_then(|mut agent| {
            let credential = caller_context
                .as_ref()
                .map(|context| context.credential.as_str())
                .filter(|credential| !credential.is_empty())
                .or_else(|| agent.hook_credential(process_group))
                .map(str::to_owned)
                .ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::OwnershipUnknown,
                        "Codex hook process does not belong to a live Agent runtime",
                    )
                })?;
            agent.capture_codex_session(&credential, native_session_id)
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
    process_group: u32,
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
        .ok_or_else(|| {
            ProtocolError::new(ErrorCode::InvalidArgument, "agent phase report is invalid")
        })
        .and_then(|(phase, caller_context)| {
            let mut agent = agent.lock().map_err(|_| {
                ProtocolError::new(ErrorCode::Unavailable, "agent owner is unavailable")
            })?;
            let credential = caller_context
                .as_ref()
                .map(|context| context.credential.as_str())
                .filter(|credential| !credential.is_empty())
                .or_else(|| agent.hook_credential(process_group))
                .map(str::to_owned)
                .ok_or_else(|| {
                    ProtocolError::new(
                        ErrorCode::OwnershipUnknown,
                        "phase hook process does not belong to a live Agent runtime",
                    )
                })?;
            agent.report_agent_phase(&credential, phase)
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
    OwnerLegacy0644,
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
        Some(PrivateLockModePolicy::OwnerLegacy0644) => mode & !0o600 == 0 || mode == 0o644,
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
impl<'a> IpcReady<'a> {
    /// Bind the production endpoint seam for one `serve` process.
    fn new(
        data_dir: &'a Path,
        workspace_root: &'a Path,
        instance_lock: &'a FileInstanceLock,
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
                true,
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
    fn registry(&self) -> std::io::Result<GenerationRegistry> {
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

/// The generations whose endpoints pre-bind reclamation must leave alone.
///
/// The sweep cannot tell a live standby's socket from a crashed generation's
/// leftover, because a standby holds no lock to be excluded by. This is the
/// durable answer it uses instead: a generation the registry still retains and
/// whose recorded process the OS proves is exactly the process recorded. An
/// unreadable registry names nothing, which is the safe direction for a *sweep*
/// — it reclaims what it can prove is residue and the registry's own recovery
/// still fails the authority closed.
fn live_generation_endpoints(data_dir: &Path) -> BTreeSet<String> {
    let Ok(Some(document)) = read_registry_document(data_dir) else {
        return BTreeSet::new();
    };
    document
        .generations
        .iter()
        .filter(|entry| {
            entry.role != usagi_daemon::usecase::generation::GenerationRole::Retired
                && observe_generation_process(&entry.process)
                    == ProcessObservation::VerifiedAlive(entry.process.clone())
        })
        .map(|entry| entry.generation.as_str().clone())
        .collect()
}

/// How often a standby re-reads its registry entry.
///
/// The same period as the active daemon's custody supervision, and for the same
/// reason: both are detached from their launcher, so a process that has lost its
/// authority has to reap itself. Only the invariant differs — the active watches
/// a lock and a record, a standby watches the one entry that names it.
const STANDBY_CUSTODY_TICK: Duration = Duration::from_secs(1);

/// The private endpoint a standby generation binds, and nothing else.
///
/// Everything the active [`IpcReady`] does that a standby must not do is simply
/// absent here: no locator publication, no runtime store reconcile or save, no
/// PTY / supervisor / PR / teardown worker, no spawn. What remains is a socket
/// that completes a readiness handshake and refuses every request through the
/// role admission fence.
struct StandbyIpc<'a> {
    data_dir: &'a Path,
    /// The workspace this process would take authority over. A standby reads
    /// that workspace's durable state to hydrate; it never adopts a new one.
    workspace_root: PathBuf,
    build: BuildIdentity,
    pid: u32,
    shutdown: Arc<ShutdownRequest>,
    standby_shutdown: Arc<ShutdownRequest>,
    worker: Arc<Mutex<Option<std::thread::JoinHandle<SecureUnixListener>>>>,
    listener: Arc<Mutex<Option<SecureUnixListener>>>,
    cleanup: RefCell<Option<EndpointCleanup>>,
    /// The admission fence this process answers requests through. It is created
    /// at bind time in the `standby` role and never activated here: promoting it
    /// is the handoff's job, in a process that has taken the authority.
    gate: RefCell<Option<AdmissionGate>>,
}

impl<'a> StandbyIpc<'a> {
    /// Bind the standby endpoint seam for one `serve --standby` process.
    fn new(
        data_dir: &'a Path,
        workspace_root: PathBuf,
        pid: u32,
        shutdown: Arc<ShutdownRequest>,
    ) -> Self {
        Self {
            data_dir,
            workspace_root,
            build: current_build(),
            pid,
            shutdown,
            standby_shutdown: Arc::new(ShutdownRequest::new()),
            worker: Arc::new(Mutex::new(None)),
            listener: Arc::new(Mutex::new(None)),
            cleanup: RefCell::new(None),
            gate: RefCell::new(None),
        }
    }

    /// The generation and endpoint this process bound, read from the retained
    /// cleanup token so the registry entry and the accepting socket can only
    /// ever be the same generation.
    fn bound_endpoint(&self) -> Option<EndpointLocator> {
        self.cleanup
            .borrow()
            .as_ref()
            .map(|cleanup| cleanup.locator().clone())
    }

    /// Read the durable runtime state without touching it.
    ///
    /// This is the whole of a standby's hydrate in this build: one read of the
    /// lifecycle store, which yields the workspace root the active generation
    /// took authority over and the state revision that read was sealed at. No
    /// reconcile, no save, no legacy migration — every one of those is a write,
    /// and the active generation is the only writer.
    ///
    /// An uninitialized store is refused rather than initialized: the process
    /// that initializes it is by definition the one that owns it.
    fn hydrate(&self) -> std::io::Result<(PathBuf, u64)> {
        let store = usagi_core::infrastructure::store::lifecycle::DaemonLifecycleStore::new(
            &adopted_workspace_state_dir(&self.data_dir.join("daemon"), &self.workspace_root)?,
        );
        let (root, state) = store
            .load_with_workspace()
            .map_err(|error| std::io::Error::other(format!("{error:#}")))?
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "durable runtime state is not initialized; a standby hydrates it read-only",
                )
            })?;
        Ok((root, state.state_revision))
    }
}

impl StandbyEndpoint for StandbyIpc<'_> {
    fn bind(&self) -> std::io::Result<()> {
        // Hydrate first: a standby that cannot read the state it would serve has
        // nothing to prove by binding, and refusing here leaves no socket for a
        // rollback to reclaim.
        let (workspace_root, revision) = self.hydrate()?;
        let (listener, wire) = bind_ipc_listener(self.data_dir)?;
        let generation = usagi_core::domain::id::DaemonGeneration::parse(&wire.0)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let cleanup = listener.cleanup_handle();
        let gate = AdmissionGate::new(
            generation,
            usagi_daemon::usecase::generation::GenerationRole::Standby,
        );
        let protocol = usagi_daemon::presentation::ipc::standby_server_protocol(
            wire,
            generation.as_str().clone(),
            self.build.clone(),
            // The standby asserts its *own* process, which is the only process it
            // can speak for. It is not the data directory's owner record and is
            // never written down; owner binding requires the `active` role, so no
            // client can mistake this for authority.
            DaemonRecord::identified(self.pid, process_start_identity(self.pid)?),
            paths::wire_workspace_root(&workspace_root),
        );
        let worker = spawn_standby_ipc_server(
            listener,
            protocol,
            gate.clone(),
            Arc::clone(&self.standby_shutdown),
        );
        match worker {
            Ok(worker) => {
                *self.cleanup.borrow_mut() = Some(cleanup);
                *self.gate.borrow_mut() = Some(gate);
                *self
                    .worker
                    .lock()
                    .map_err(|_| std::io::Error::other("standby worker lock is poisoned"))? =
                    Some(worker);
                ErrorLog::record(&format!(
                    "daemon standby hydrated read-only at runtime state revision {revision}"
                ));
                Ok(())
            }
            // The listener is dropped with the failure, and its Drop retires the
            // socket it bound. Nothing durable was written.
            Err(error) => Err(error),
        }
    }

    fn retire(&self) -> std::io::Result<()> {
        self.shutdown.request();
        self.standby_shutdown.request();
        // Closing the two lease classes is what makes "stopped admitting"
        // observable to a request already in flight, rather than only to the next
        // connection.
        if let Some(gate) = self.gate.borrow().as_ref() {
            gate.close(LeaseClass::ActiveControl);
            gate.close(LeaseClass::OwnerTerminal);
        }
        let joined = match self
            .worker
            .lock()
            .map_err(|_| std::io::Error::other("standby worker lock is poisoned"))?
            .take()
        {
            Some(worker) => worker
                .join()
                .map(|listener| {
                    if let Ok(mut retained) = self.listener.lock() {
                        *retained = Some(listener);
                    }
                })
                .map_err(|_| std::io::Error::other("daemon standby accept loop panicked")),
            None => Ok(()),
        };
        let cleanup = match self.cleanup.borrow().as_ref() {
            // A standby never published `current.json`, so this only ever removes
            // its own socket: the token refuses to touch a locator that names
            // another generation.
            Some(cleanup) => cleanup.retire(),
            None => Ok(()),
        };
        if cleanup.is_ok() {
            if let Ok(mut listener) = self.listener.lock() {
                listener.take();
            }
            self.cleanup.borrow_mut().take();
        }
        joined.and(cleanup)
    }
}

impl Drop for StandbyIpc<'_> {
    fn drop(&mut self) {
        // A panic unwinds past the state machine's own stand-down, and a socket
        // this process bound is a socket only this process can prove it owns.
        // Retirement is idempotent, so the ordinary path is unaffected.
        //
        // The guard matters because the composition root binds this seam for
        // *both* roles: an active `serve` never binds it, and dropping it must
        // not then request that process's shutdown.
        if self.cleanup.borrow().is_some() {
            let _ = StandbyEndpoint::retire(self);
        }
    }
}

/// Serve a standby's private endpoint.
///
/// The loop is deliberately not [`start_ipc_accept_loop`]: that one owns a
/// session runtime, a terminal runtime, an Agent runtime, a supervisor and a
/// PR projector, and a standby owns none of them. Every admitted connection here
/// gets a handshake and then a typed refusal.
fn spawn_standby_ipc_server(
    listener: SecureUnixListener,
    protocol: usagi_core::infrastructure::ipc::ServerProtocol,
    gate: AdmissionGate,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<SecureUnixListener>> {
    let connection_limit = client_connection_limit();
    std::thread::Builder::new()
        .name("usagi-ipc-standby".to_string())
        .spawn(move || {
            let _exit = ShutdownOnIpcWorkerExit {
                shutdown: Arc::clone(&shutdown),
            };
            let workers = Arc::new(ClientWorkers::new());
            let pre_handshake = PreHandshakeAdmission::new(PRE_HANDSHAKE_CONNECTION_LIMIT);
            let mut capacity_log = CapacityRefusalLog::default();
            let wake = match ShutdownPipe::mirroring(&shutdown) {
                Ok(wake) => wake,
                Err(error) => {
                    ErrorLog::record(&format!("daemon standby accept wait unavailable: {error}"));
                    return listener;
                }
            };
            while !shutdown.is_requested() {
                if !wake.wait_for_listener(listener.readiness_fd()) {
                    break;
                }
                while !shutdown.is_requested() {
                    match listener.accept() {
                        Ok(stream) => {
                            if shutdown.is_requested() {
                                break;
                            }
                            let capacity_available = client_connection_capacity_available(
                                &workers,
                                connection_limit,
                            );
                            if capacity_log.should_record(capacity_available) {
                                ErrorLog::record(
                                    "daemon standby connection refused: client capacity exhausted",
                                );
                            }
                            if !capacity_available {
                                drop(stream);
                                continue;
                            }
                            let Some(pre_handshake_permit) = pre_handshake.try_admit() else {
                                ErrorLog::record(
                                    "daemon standby pre-handshake connection refused: capacity exhausted",
                                );
                                drop(stream);
                                continue;
                            };
                            let unblock = match stream.try_clone() {
                                Ok(stream) => AcceptedStream::new(stream),
                                Err(error) => {
                                    ErrorLog::record(&format!(
                                        "daemon standby connection refused: accepted stream could not be duplicated: {error}"
                                    ));
                                    continue;
                                }
                            };
                            match spawn_standby_client_worker(
                                stream,
                                unblock.clone(),
                                protocol.clone(),
                                gate.clone(),
                                pre_handshake_permit,
                            ) {
                                Ok(handle) => {
                                    retain_client_worker(&workers, Ok(unblock), handle);
                                }
                                Err(error) => ErrorLog::record(&format!(
                                    "daemon standby client worker unavailable: {error}"
                                )),
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => std::thread::sleep(ACCEPT_ERROR_BACKOFF),
                    }
                }
            }
            let report = workers.retire();
            if !report.is_clean() {
                ErrorLog::record(&format!(
                    "daemon standby shutdown retired with client worker failures: {report:?}"
                ));
            }
            listener
        })
}

fn spawn_standby_client_worker(
    stream: std::os::unix::net::UnixStream,
    completion: AcceptedStream,
    protocol: usagi_core::infrastructure::ipc::ServerProtocol,
    gate: AdmissionGate,
    pre_handshake_permit: PreHandshakePermit,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-ipc-standby-client".to_string())
        .spawn(move || {
            let retirement = completion.retirement();
            let _completion = ShutdownAcceptedStreamOnDrop(Some(completion));
            if stream.set_nonblocking(false).is_err() {
                return;
            }
            let Ok(writer) = stream.try_clone() else {
                return;
            };
            let deadline = Instant::now() + PRE_HANDSHAKE_DEADLINE;
            let mut reader = PreHandshakeDeadlineStream::new(stream, deadline);
            let mut writer = PreHandshakeDeadlineStream::new(writer, deadline);
            let admitted = usagi_daemon::presentation::ipc::handshake_admitted(
                &mut reader,
                &mut writer,
                &protocol,
            );
            drop(pre_handshake_permit);
            let admitted = match admitted {
                Ok(Some(admitted)) => admitted,
                Ok(None) => return,
                Err(error) => {
                    let reason = if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) {
                        "deadline exceeded"
                    } else {
                        "invalid or incomplete hello"
                    };
                    ErrorLog::record(&format!(
                        "daemon standby pre-handshake connection refused: {reason}"
                    ));
                    return;
                }
            };
            if reader.clear_deadlines().is_err() || writer.clear_deadlines().is_err() {
                ErrorLog::record(
                    "daemon standby admitted connection closed: pre-handshake deadline could not be cleared",
                );
                return;
            }
            // Same reason as the active worker: a standby is retired by the same
            // barrier, and `shutdown(2)` can fail to return this parked read.
            let mut reader =
                RetiringReader::new(reader.into_inner(), retirement, CLIENT_RETIREMENT_POLL);
            let mut writer = writer.into_inner();
            let _ = usagi_daemon::presentation::ipc::handle_admitted_connection_with(
                &mut reader,
                &mut writer,
                admitted,
                &mut |request_id, body, hello| {
                    standby_reply(&gate, request_id, &body, hello)
                },
            );
        })
}

/// The serving generation's authority over one client connection.
///
/// Both halves of it were already implemented and had no production caller. This
/// is where the shipping active daemon acquires them:
///
/// | half | what it decides |
/// |---|---|
/// | [`AdmissionGate`] | may this request produce an effect on this generation *right now* |
/// | [`RoutingLedger`] | may a rollover leave this generation draining — can every live client still address it |
///
/// Neither changes what a single active generation does: the gate opens both
/// lease classes for the `active` role, so every request that this build
/// dispatched before is still dispatched, and the ledger only records. What they
/// add is the *ability* to stop: a generation whose role moves to `draining`
/// refuses control and new spawns from the next request onwards while its owned
/// terminals keep being served, and the barrier a handoff waits on is the leases
/// the gate has already issued.
///
/// It is shared by every connection thread of one generation, so it is `Sync` and
/// both halves are internally locked.
struct GenerationFence {
    gate: AdmissionGate,
    ledger: Arc<RoutingLedger>,
}

impl usagi_daemon::presentation::ipc::ConnectionFence for GenerationFence {
    fn admitted(
        &self,
        connection: usagi_core::domain::id::ConnectionId,
        hello: &usagi_core::infrastructure::ipc::ClientHello,
    ) {
        self.ledger.admit(connection, hello);
    }

    fn admit(
        &self,
        body: &serde_json::Value,
    ) -> Result<Option<AdmissionLease>, usagi_core::infrastructure::ipc::ProtocolError> {
        // The stance is `Own` because this process is the one that holds the data
        // directory's runtime state. Which *exact* record a ref names is the
        // terminal runtime's answer, not the fence's
        // (`usagi_daemon::usecase::authority::fence`).
        let (class, owner) = classify_request(body, OwnedRuntime::Own);
        self.gate.admit(class, owner).map_err(|refusal| {
            // The same code and the same meaning as a standby's refusal
            // ([`standby_reply`]): "this generation may not do this; re-resolve
            // the authority", with zero effect.
            usagi_core::infrastructure::ipc::ProtocolError::new(
                usagi_core::infrastructure::ipc::ErrorCode::GenerationRolledOver,
                refusal.to_string(),
            )
        })
    }

    fn disconnected(&self, connection: usagi_core::domain::id::ConnectionId) {
        self.ledger.disconnect(&connection);
    }
}

/// A Unix stream armed against one fixed handshake completion instant.
///
/// Every individual `read` and `write` re-arms the OS timeout with only the
/// remaining budget. Partial prefix/body progress therefore cannot extend the
/// deadline, while the kernel still performs the blocking wait efficiently.
struct PreHandshakeDeadlineStream {
    stream: std::os::unix::net::UnixStream,
    deadline: Instant,
}

impl PreHandshakeDeadlineStream {
    fn new(stream: std::os::unix::net::UnixStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }

    fn remaining(&self) -> std::io::Result<Duration> {
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

    fn clear_deadlines(&self) -> std::io::Result<()> {
        self.stream.set_read_timeout(None)?;
        self.stream.set_write_timeout(None)
    }

    fn into_inner(self) -> std::os::unix::net::UnixStream {
        self.stream
    }

    fn deadline_error(error: std::io::Error) -> std::io::Error {
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

impl Read for PreHandshakeDeadlineStream {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(bytes).map_err(Self::deadline_error)
    }
}

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

/// How often a parked client worker re-checks whether its connection was
/// retired.
///
/// This is a backstop, not the ordinary path: `shutdown(2)` normally returns the
/// parked read immediately. It only has to be short enough that a lost wakeup
/// costs retirement one extra tick, and long enough that an idle connection is
/// not a busy loop.
const CLIENT_RETIREMENT_POLL: Duration = Duration::from_millis(250);

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
struct AcceptedStream {
    stream: Arc<Mutex<Option<std::os::unix::net::UnixStream>>>,
    retired: Arc<AtomicBool>,
}

impl AcceptedStream {
    fn new(stream: std::os::unix::net::UnixStream) -> Self {
        Self {
            stream: Arc::new(Mutex::new(Some(stream))),
            retired: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The flag a worker parked on this connection watches to learn that
    /// retirement asked it to stop.
    fn retirement(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.retired)
    }

    /// Observes a peer close without consuming bytes that may belong to a later
    /// request. The retained duplicate is already owned for retirement, so this
    /// adds no descriptor to a waiting decision.
    fn peer_disconnected(&self) -> bool {
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

impl DecisionWaitCancellation for AcceptedStream {
    fn is_cancelled(&self) -> bool {
        self.peer_disconnected()
    }
}

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

/// An established connection's reader, which cannot outlive its own retirement.
///
/// Every read is gated on `poll(2)` with a bounded timeout, so the worker is
/// never parked in the kernel on a socket that has nothing to give it. The
/// timeout is *not* an idle policy: it is retried transparently, so an idle
/// subscription behaves exactly as it did before. The only thing it adds is a
/// point at which the worker observes [`AcceptedStream::shutdown`] and returns,
/// which is what keeps the retirement barrier joinable when the socket wakeup is
/// lost.
struct RetiringReader {
    stream: std::os::unix::net::UnixStream,
    retired: Arc<AtomicBool>,
    poll: Duration,
    /// How many waits expired without readability.
    ///
    /// The observation seam the tests use to prove this is a retry rather than
    /// an idle policy: a live connection must cross timeouts and still serve the
    /// frame that eventually arrives.
    timeouts: Arc<AtomicUsize>,
}

impl RetiringReader {
    fn new(
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
    fn timeouts(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.timeouts)
    }
}

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
struct ShutdownAcceptedStreamOnDrop(Option<AcceptedStream>);

impl Drop for ShutdownAcceptedStreamOnDrop {
    fn drop(&mut self) {
        if let Some(stream) = &self.0 {
            let _ = stream.shutdown();
        }
    }
}

/// The one answer a standby has for a post-handshake request.
///
/// The role admission fence decides it, which is what
/// `daemon.generation-handoff.v1` claims this peer does. Control, spawn and
/// terminal IO are refused by the fence itself; a read the fence admits is still
/// refused here, because this build's standby holds no runtime state to read —
/// the owner shard it would read is not wired yet.
///
/// The classification is
/// [`classify_request`](usagi_daemon::usecase::authority::fence::classify_request),
/// the same one the active generation's fence reads, under this role's honest
/// stance: a standby owns nothing, so every request that names a runtime names
/// another generation's ([`OwnedRuntime::Nothing`]).
///
/// A fence refusal is reported as `generation_rolled_over`, which is the same
/// code the draining generation's fence reports for the same decision
/// (`crates/daemon/tests/generation_authority.rs`). Both mean "this generation
/// may not do this; re-resolve the authority", and both are effect zero, so the
/// two roles stay one contract for a client rather than two.
fn standby_reply(
    gate: &AdmissionGate,
    request_id: usagi_core::infrastructure::ipc::RequestId,
    body: &serde_json::Value,
    hello: &usagi_core::infrastructure::ipc::ServerHello,
) -> usagi_core::infrastructure::ipc::Envelope {
    use usagi_core::infrastructure::ipc::{
        Envelope, EnvelopeKind, ErrorCode, ProtocolError, ResponseOutcome,
    };
    let (class, owner) = classify_request(body, OwnedRuntime::Nothing);
    let error = match gate.admit(class, owner) {
        Ok(lease) => {
            drop(lease);
            ProtocolError::new(
                ErrorCode::Unavailable,
                "standby generation serves no runtime state",
            )
        }
        Err(refusal) => ProtocolError::new(ErrorCode::GenerationRolledOver, refusal.to_string()),
    };
    Envelope {
        protocol: hello.protocol,
        daemon_generation: hello.daemon_generation.clone(),
        kind: EnvelopeKind::Response {
            request_id,
            outcome: ResponseOutcome::Error(error),
            body: serde_json::Value::Null,
        },
    }
}

/// A standby's participation in the durable generation registry.
///
/// It is the composition of the registry document, the data directory's owner
/// record, and a read-only handshake against this process's own private
/// endpoint. The pure decisions it drives —
/// [`admissible_active`] and [`prepare_standby`] — never touch the current
/// locator, which is what keeps every client pointed at the active generation
/// throughout.
struct StandbyRegistryAuthority<'a> {
    data_dir: &'a Path,
    endpoint: &'a StandbyIpc<'a>,
    build: BuildIdentity,
    pid: u32,
    shutdown: Arc<ShutdownRequest>,
    registered: RefCell<Option<usagi_core::domain::id::DaemonGeneration>>,
}

impl<'a> StandbyRegistryAuthority<'a> {
    /// Bind the standby registry seam against the endpoint that process bound.
    fn new(data_dir: &'a Path, endpoint: &'a StandbyIpc<'a>, pid: u32) -> Self {
        Self {
            data_dir,
            build: current_build(),
            pid,
            shutdown: Arc::clone(&endpoint.shutdown),
            endpoint,
            registered: RefCell::new(None),
        }
    }

    fn registry(&self) -> std::io::Result<GenerationRegistry> {
        Ok(GenerationRegistry::new(
            GenerationRegistryFile::new(self.data_dir)?,
            DEFAULT_GENERATION_LIMIT,
        ))
    }

    /// The registry document, as a reader that must not become a writer sees it.
    fn document(&self) -> std::io::Result<Option<RegistryDocument>> {
        read_registry_document(self.data_dir).map_err(std::io::Error::other)
    }

    /// The live registered active generation of this data directory, proved from
    /// the registry document and the owner record together. Reads only.
    fn active_generation(&self) -> std::io::Result<usagi_core::domain::id::DaemonGeneration> {
        let record = DaemonRecordStore::new(FsRecordFile {
            path: self.data_dir.join("daemon").join("daemon.json"),
        })
        .load()?;
        let observation = record
            .as_ref()
            .map_or(DaemonProcessObservation::Unknown, |record| {
                LivenessProbe::observe(&ExactProcessControl, record)
            });
        let document = self.document()?;
        Ok(admissible_active(
            document.as_ref(),
            &ActiveOwner {
                record: record.as_ref(),
                observation,
            },
        )?)
    }
}

impl StandbyAuthority for StandbyRegistryAuthority<'_> {
    fn preflight(&self) -> std::io::Result<()> {
        self.active_generation().map(|_| ())
    }

    fn admit(&self) -> std::io::Result<()> {
        let bound = self.endpoint.bound_endpoint().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "standby endpoint must be bound before registering",
            )
        })?;
        let generation = usagi_core::domain::id::DaemonGeneration::parse(&bound.generation.0)
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "bound endpoint does not name a canonical daemon generation",
                )
            })?;
        // Re-proved immediately before the compare-and-swap: the owner could have
        // died while this process was binding, and a standby beside a dead active
        // is a successor with nothing to succeed.
        let active = self.active_generation()?;
        let process = own_process_identity(self.pid)?;
        prepare_standby(
            &self.registry()?,
            &UnixStandbyProbe {
                data_dir: self.data_dir,
                build: self.build.clone(),
            },
            generation,
            &bound.endpoint,
            &process,
            &self.build,
        )
        .map_err(std::io::Error::other)?;
        *self.registered.borrow_mut() = Some(generation);
        ErrorLog::record(&format!(
            "daemon standby {generation} verified for active generation {active}"
        ));
        // Supervision starts once there is an entry to supervise, so a refused
        // admission never leaves a thread watching for one.
        start_standby_custody_worker(
            self.data_dir.to_path_buf(),
            generation,
            process,
            self.endpoint.hydrate()?.0,
            self.build.clone(),
            Arc::clone(&self.endpoint.standby_shutdown),
            Arc::clone(&self.endpoint.worker),
            Arc::clone(&self.shutdown),
        )
    }

    fn release(&self) -> std::io::Result<()> {
        let Some(generation) = *self.registered.borrow() else {
            return Ok(());
        };
        release_authority(&self.registry()?, generation).map_err(std::io::Error::other)
    }
}

impl Drop for StandbyRegistryAuthority<'_> {
    fn drop(&mut self) {
        // Dropped before the endpoint it registered (declaration order in the
        // composition root is what fixes that), so an unwind gives up the entry
        // that names the socket before the socket goes.
        if self.registered.borrow().is_some() {
            let _ = StandbyAuthority::release(self);
        }
    }
}

/// The real readiness handshake: connect to this generation's own private
/// endpoint by name and complete one hello.
///
/// It is deliberately the same endpoint resolution a client uses for a
/// non-current generation, so readiness proves the socket a rollover would
/// actually name rather than a path this process remembers.
struct UnixStandbyProbe<'a> {
    data_dir: &'a Path,
    build: BuildIdentity,
}

impl StandbyProbe for UnixStandbyProbe<'_> {
    fn hello(
        &self,
        endpoint: &str,
    ) -> std::io::Result<usagi_core::infrastructure::ipc::ServerHello> {
        use usagi_core::infrastructure::ipc::{
            Bootstrap, ClientHello, ClientId, DEFAULT_MAX_FRAME_BYTES, ProtocolRange,
            TERMINAL_CHECKPOINT_REVISION, TERMINAL_WIRE_GENERATION, read_json_frame,
            write_json_frame,
        };
        let generation = usagi_core::domain::id::DaemonGeneration::parse(
            endpoint
                .strip_prefix("generations/")
                .and_then(|rest| rest.split('/').next())
                .unwrap_or_default(),
        )
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "standby endpoint does not name a canonical daemon generation",
            )
        })?;
        let mut stream = connect_generation(
            self.data_dir,
            &usagi_core::usecase::owner_routing::TrustedEndpoint {
                generation,
                // Only the endpoint spelling is used by the connect; the role is
                // carried for the caller's own bookkeeping.
                role: usagi_core::infrastructure::ipc::GenerationRole::Standby,
                endpoint: endpoint.to_owned(),
            },
        )?;
        // One bootstrap frame out, one in, then the connection is dropped. There
        // is deliberately no request path here: a readiness probe that could
        // mutate its peer would not be a proof of readiness.
        write_json_frame(
            &mut stream,
            &Bootstrap::ClientHello(ClientHello {
                client_id: ClientId(format!("standby-readiness-{}", std::process::id())),
                connection_nonce: format!("{}", std::process::id()),
                expected_daemon_generation: None,
                supported_protocols: vec![ProtocolRange {
                    generation: TERMINAL_WIRE_GENERATION,
                    min_revision: 0,
                    max_revision: TERMINAL_CHECKPOINT_REVISION,
                }],
                capabilities: Vec::new(),
                required_capabilities: Vec::new(),
                build: self.build.clone(),
                workspace: Some(ClientWorkspace::Unbound),
            }),
            DEFAULT_MAX_FRAME_BYTES,
        )?;
        match read_json_frame::<Bootstrap>(&mut stream, DEFAULT_MAX_FRAME_BYTES)? {
            Some(Bootstrap::ServerHello(hello)) => Ok(hello),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("standby endpoint did not complete a handshake: {other:?}"),
            )),
        }
    }
}

/// Start the only standby custody supervisor.
///
/// A standby holds neither the instance lock nor a lifecycle record, so the
/// active daemon's two custody invariants do not exist for it. Its registry entry
/// is the whole of its authority: recovery that fails an abandoned handoff closed
/// retires every generation, and the standby it retired must exit rather than
/// keep a socket a future rollover might trust.
#[allow(clippy::too_many_arguments)] // Promotion carries the exact process, endpoint, runtime root, and both shutdown domains.
fn start_standby_custody_worker(
    data_dir: PathBuf,
    generation: usagi_core::domain::id::DaemonGeneration,
    process: ProcessIdentity,
    workspace_root: PathBuf,
    build: BuildIdentity,
    standby_shutdown: Arc<ShutdownRequest>,
    worker: Arc<Mutex<Option<std::thread::JoinHandle<SecureUnixListener>>>>,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("usagi-daemon-standby-custody".to_string())
        .spawn(move || {
            let mut promoted = false;
            while !shutdown.is_requested() {
                // An unreadable registry is uncertainty, not a loss: it never
                // terminates a standby that may still hold its entry.
                if let Ok(Some(document)) = read_registry_document(&data_dir) {
                    if !promoted && document.role(generation) == Some(GenerationRole::Active) {
                        match promote_standby_generation(
                            &data_dir,
                            &workspace_root,
                            generation,
                            &process,
                            &build,
                            &standby_shutdown,
                            &worker,
                            Arc::clone(&shutdown),
                        ) {
                            Ok(()) => promoted = true,
                            Err(error) => {
                                ErrorLog::record(&format!(
                                    "daemon standby promotion failed: {error}"
                                ));
                                shutdown.request();
                                return;
                            }
                        }
                    }
                    if let StandbyCustody::Lost(loss) = evaluate_custody(
                        &document,
                        generation,
                        &process,
                        &mut observe_generation_process,
                    ) {
                        ErrorLog::record(&format!(
                            "daemon standby custody lost ({}); shutting down",
                            loss.reason()
                        ));
                        shutdown.request();
                        return;
                    }
                }
                if shutdown.wait_for_tick(STANDBY_CUSTODY_TICK) {
                    break;
                }
            }
        })
        .map(|_| ())
}

/// Replace the readiness-only standby accept loop with the full active runtime
/// on the same bound socket and generation after the durable handoff commits.
#[allow(clippy::too_many_arguments)] // Each handoff fence is passed explicitly; bundling would hide identity or listener ownership.
fn promote_standby_generation(
    data_dir: &Path,
    workspace_root: &Path,
    generation: usagi_core::domain::id::DaemonGeneration,
    process: &ProcessIdentity,
    build: &BuildIdentity,
    standby_shutdown: &ShutdownRequest,
    worker: &Mutex<Option<std::thread::JoinHandle<SecureUnixListener>>>,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<()> {
    standby_shutdown.request();
    let standby = worker
        .lock()
        .map_err(|_| std::io::Error::other("standby worker lock is poisoned"))?
        .take()
        .ok_or_else(|| std::io::Error::other("standby accept loop is unavailable"))?;
    let listener = standby
        .join()
        .map_err(|_| std::io::Error::other("daemon standby accept loop panicked"))?;

    let record = DaemonRecord::identified(process.pid, process.start_identity.clone());
    DaemonRecordStore::new(FsRecordFile {
        path: data_dir.join("daemon/daemon.json"),
    })
    .save(&record)?;
    let wire = usagi_core::infrastructure::ipc::DaemonGeneration(generation.as_str().clone());
    let active = spawn_ipc_server(
        listener,
        &wire,
        data_dir,
        workspace_root,
        build,
        record,
        None,
        false,
        shutdown,
    )?;
    *worker
        .lock()
        .map_err(|_| std::io::Error::other("standby worker lock is poisoned"))? = Some(active);
    Ok(())
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
impl ServeLauncher {
    fn launch_standby(&self) -> std::io::Result<u32> {
        let mut command = std::process::Command::new(&self.exe);
        command
            .args(["daemon", "serve", "--standby"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command.spawn().map(|child| child.id())
    }
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

#[derive(Debug, PartialEq, Eq)]
struct BootstrapBrokerAddress {
    socket: PathBuf,
    lock: PathBuf,
}

fn bootstrap_broker_address(
    data_dir: &Path,
    workspace: &Path,
    exe: &Path,
) -> BootstrapBrokerAddress {
    let mut digest = Sha256::new();
    for component in [
        b"usagi-bootstrap-broker-v1".as_slice(),
        workspace.as_os_str().as_encoded_bytes(),
        exe.as_os_str().as_encoded_bytes(),
    ] {
        digest.update((component.len() as u64).to_be_bytes());
        digest.update(component);
    }
    let digest = digest.finalize();
    let mut key = String::with_capacity(32);
    for byte in &digest[..16] {
        write!(&mut key, "{byte:02x}").expect("writing to a String cannot fail");
    }
    let daemon_dir = data_dir.join("daemon");
    BootstrapBrokerAddress {
        socket: daemon_dir.join(format!("bootstrap-broker-{key}.sock")),
        lock: daemon_dir.join(format!("bootstrap-broker-{key}.lock")),
    }
}

fn broker_workspace(workspace: &ClientWorkspace) -> std::io::Result<PathBuf> {
    let root = match workspace {
        ClientWorkspace::Bound { root } | ClientWorkspace::Selected { root }
            if !root.is_empty() =>
        {
            root
        }
        ClientWorkspace::Bound { .. }
        | ClientWorkspace::Selected { .. }
        | ClientWorkspace::Unbound => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "daemon bootstrap broker requires a canonical workspace",
            ));
        }
    };
    paths::canonical_workspace_root(root)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))
}

fn request_bootstrap_broker(address: &BootstrapBrokerAddress, request: u8) -> std::io::Result<()> {
    let mut stream = std::os::unix::net::UnixStream::connect(&address.socket)?;
    let timeout = if request == BROKER_START {
        Duration::from_secs(6)
    } else {
        BROKER_REQUEST_TIMEOUT
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(&[request])?;
    let mut reply = [0_u8; 1];
    stream.read_exact(&mut reply)?;
    (reply[0] == BROKER_OK)
        .then_some(())
        .ok_or_else(|| std::io::Error::other("daemon bootstrap broker refused the request"))
}

fn request_broker_start(
    data_dir: &Path,
    workspace: &ClientWorkspace,
    exe: &Path,
) -> std::io::Result<()> {
    let workspace = broker_workspace(workspace)?;
    let exe = exe.canonicalize()?;
    let address = bootstrap_broker_address(data_dir, &workspace, &exe);
    request_bootstrap_broker(&address, BROKER_START)?;
    for _ in 0..BROKER_READINESS_ATTEMPTS {
        if usagi_daemon::infrastructure::unix_transport::connect_current(data_dir).is_ok() {
            return Ok(());
        }
        RealSleeper.sleep();
    }
    Err(std::io::Error::other(
        "daemon bootstrap broker started no reachable daemon",
    ))
}

fn spawn_bootstrap_broker(exe: &Path, data_dir: &Path, workspace: &Path) -> std::io::Result<()> {
    let workspace = paths::canonical_workspace_root(workspace)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))?;
    let exe = exe.canonicalize()?;
    let address = bootstrap_broker_address(data_dir, &workspace, &exe);
    // A daemon launched by this broker reaches here while the broker is still
    // waiting for that daemon's readiness. Requiring a ping reply would make
    // both processes wait on each other. The identity-scoped socket path and a
    // successful connect are sufficient to prove that this broker is present.
    if std::os::unix::net::UnixStream::connect(&address.socket).is_ok() {
        return Ok(());
    }
    let mut command = std::process::Command::new(&exe);
    command
        .args(["daemon", "bootstrap-broker"])
        .current_dir(&workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let child = command.spawn()?;
    reap_child(child);
    for _ in 0..BROKER_READINESS_ATTEMPTS {
        if request_bootstrap_broker(&address, BROKER_PING).is_ok() {
            return Ok(());
        }
        RealSleeper.sleep();
    }
    Err(std::io::Error::other(
        "daemon bootstrap broker did not become ready",
    ))
}

fn reap_child(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

fn bootstrap_serve_command(exe: &Path, workspace: &Path) -> std::process::Command {
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

fn launch_broker_daemon(exe: &Path, workspace: &Path, data_dir: &Path) -> std::io::Result<()> {
    if usagi_daemon::infrastructure::unix_transport::connect_current(data_dir).is_ok() {
        return Ok(());
    }
    let child = bootstrap_serve_command(exe, workspace).spawn()?;
    for _ in 0..BROKER_READINESS_ATTEMPTS {
        if usagi_daemon::infrastructure::unix_transport::connect_current(data_dir).is_ok() {
            reap_child(child);
            return Ok(());
        }
        RealSleeper.sleep();
    }
    reap_child(child);
    Err(std::io::Error::other(
        "brokered daemon did not become ready",
    ))
}

/// What answering one broker request decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BrokerOutcome {
    /// Whether the peer is told the request succeeded.
    accepted: bool,
    /// Whether the broker closes its endpoint after replying.
    retire: bool,
}

impl BrokerOutcome {
    const fn served(accepted: bool) -> Self {
        Self {
            accepted,
            retire: false,
        }
    }

    const RETIRE: Self = Self {
        accepted: true,
        retire: true,
    };
}

fn handle_bootstrap_broker_request(
    request: u8,
    launch: impl FnOnce() -> std::io::Result<()>,
    daemon_live: impl FnOnce() -> bool,
) -> BrokerOutcome {
    match request {
        BROKER_PING => BrokerOutcome::served(true),
        BROKER_START => BrokerOutcome::served(launch().is_ok()),
        // Retiring is acknowledged before it happens: the peer asked for the
        // endpoint to go away, so its disappearance is the success case.
        //
        // A reachable daemon vetoes it. Both senders decide to retire from
        // outside this loop — `usagi daemon stop` after it stopped the daemon,
        // the idle watch after it found none — and in between either decision
        // and this point a `BROKER_START` can have put one back. Retiring then
        // would leave a live daemon with no broker to outlive it, which is the
        // one state the broker exists to prevent. Re-reading the endpoint here
        // is the only place both senders pass through.
        BROKER_STOP if daemon_live() => BrokerOutcome::served(false),
        BROKER_STOP => BrokerOutcome::RETIRE,
        _ => BrokerOutcome::served(false),
    }
}

/// Whether a broker that has been idle for `idle_for` may retire now.
///
/// A live daemon keeps the broker alive however long it has been quiet: the
/// broker's whole purpose is to already exist when that daemon dies, and a
/// sandboxed client cannot spawn a replacement for it.
const fn broker_may_retire(idle_for: Duration, timeout: Duration, daemon_live: bool) -> bool {
    !daemon_live && idle_for.as_secs() >= timeout.as_secs()
}

/// How long a broker tolerates being unused before retiring itself.
#[derive(Debug, Clone, Copy)]
struct BrokerIdlePolicy {
    timeout: Duration,
    poll: Duration,
}

impl BrokerIdlePolicy {
    const fn production() -> Self {
        Self {
            timeout: BROKER_IDLE_TIMEOUT,
            poll: BROKER_IDLE_POLL,
        }
    }
}

/// The last time a broker answered a request, shared with its idle watch.
struct BrokerActivity {
    state: Mutex<BrokerActivityState>,
    signal: Condvar,
}

struct BrokerActivityState {
    last: Instant,
    stopped: bool,
}

impl BrokerActivity {
    fn started() -> Self {
        Self {
            state: Mutex::new(BrokerActivityState {
                last: Instant::now(),
                stopped: false,
            }),
            signal: Condvar::new(),
        }
    }

    fn touch(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.last = Instant::now();
        }
    }

    /// Stop the idle watch and let it be joined without waiting out a poll.
    fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
        }
        self.signal.notify_all();
    }
}

/// Watch an idle broker and ask it to retire once nothing needs it.
///
/// The retirement is delivered as an ordinary [`BROKER_STOP`] request rather
/// than by killing the loop from outside: the accept loop stays blocked (so a
/// cold start pays no polling latency), and the endpoint is torn down by the
/// same path an operator's `usagi daemon stop` takes.
fn spawn_broker_idle_watch(
    activity: &Arc<BrokerActivity>,
    address: BootstrapBrokerAddress,
    data_dir: &Path,
    idle: BrokerIdlePolicy,
) -> std::thread::JoinHandle<()> {
    let activity = Arc::clone(activity);
    let data_dir = data_dir.to_path_buf();
    std::thread::spawn(move || {
        loop {
            let Ok(state) = activity.state.lock() else {
                return;
            };
            let Ok((state, _)) = activity.signal.wait_timeout(state, idle.poll) else {
                return;
            };
            if state.stopped {
                return;
            }
            let idle_for = state.last.elapsed();
            // The daemon probe is IO, so the lock is released before it runs.
            drop(state);
            let daemon_live =
                usagi_daemon::infrastructure::unix_transport::connect_current(&data_dir).is_ok();
            if broker_may_retire(idle_for, idle.timeout, daemon_live) {
                let _ = request_bootstrap_broker(&address, BROKER_STOP);
                return;
            }
        }
    })
}

fn serve_bootstrap_broker(
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
    let lock = FileInstanceLock {
        path: bootstrap_broker_address(data_dir, &workspace, &exe).lock,
        held: RefCell::new(None),
    };
    if !lock.acquire()? {
        return Ok(());
    }
    let socket = bootstrap_broker_address(data_dir, &workspace, &exe).socket;
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
    let activity = Arc::new(BrokerActivity::started());
    let watch = spawn_broker_idle_watch(
        &activity,
        bootstrap_broker_address(data_dir, &workspace, &exe),
        data_dir,
        idle,
    );
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
            || usagi_daemon::infrastructure::unix_transport::connect_current(data_dir).is_ok(),
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
    let _ = std::fs::remove_file(socket);
    Ok(())
}

struct IpcRolloverRequester<'a> {
    data_dir: &'a Path,
    launcher: &'a ServeLauncher,
}

impl IpcRolloverRequester<'_> {
    fn committed(&self, operation: &OperationId, standby_pid: u32) -> bool {
        read_registry_document(self.data_dir)
            .ok()
            .flatten()
            .is_some_and(|document| {
                document.completed_operation.as_ref() == Some(operation)
                    || document.handoff.as_ref().is_some_and(|handoff| {
                        handoff.operation == *operation
                            && handoff.phase
                                == usagi_daemon::usecase::authority::registry::HandoffPhase::Committed
                    })
                    || document.generations.iter().any(|entry| {
                        entry.process.pid == standby_pid && entry.role == GenerationRole::Active
                    })
            })
    }

    fn stop_standby(pid: u32) {
        let Ok(identity) = process_start_identity(pid) else {
            return;
        };
        let record = DaemonRecord::identified(pid, identity);
        let _ = Terminator::terminate(&SigtermTerminator, &record);
    }

    fn wait_until_verified(&self, pid: u32) -> std::io::Result<()> {
        for _ in 0..40 {
            if read_registry_document(self.data_dir)
                .ok()
                .flatten()
                .is_some_and(|document| {
                    document.generations.iter().any(|entry| {
                        entry.process.pid == pid
                            && entry.role == GenerationRole::Standby
                            && entry.is_build_verified()
                    })
                })
            {
                return Ok(());
            }
            RealSleeper.sleep();
        }
        Err(std::io::Error::other(
            "standby did not reach verified readiness within the startup window",
        ))
    }
}

impl RolloverRequester for IpcRolloverRequester<'_> {
    fn rollover(&self, operation: &OperationId) -> std::io::Result<String> {
        let pid = self.launcher.launch_standby()?;
        if let Err(error) = self.wait_until_verified(pid) {
            Self::stop_standby(pid);
            return Err(error);
        }
        let result = policy_client(ClientPolicy::cli()).and_then(|mut client| {
            client.request(DaemonRequest::Rollover {
                operation_id: operation.0.clone(),
            })
        });
        match result {
            Ok(_) => Ok(format!(
                "daemon authority handed off (operation {})",
                operation.0
            )),
            Err(error) => {
                if !self.committed(operation, pid) {
                    Self::stop_standby(pid);
                }
                Err(std::io::Error::other(error.to_string()))
            }
        }
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
    /// How long to wait for a departing owner before refusing. A start can wait
    /// for the previous daemon to exit; an adoption happens inside a client's
    /// handshake, which has its own deadline, so it refuses quickly instead.
    patience: Duration,
    held: RefCell<Option<std::fs::File>>,
}

/// How long a `serve` start waits for a departing owner to release a workspace.
const WORKSPACE_FENCE_PATIENCE: Duration = Duration::from_secs(2);

/// How long an adoption waits. It runs inside the client's pre-handshake
/// deadline, so a contended workspace is reported rather than waited out.
const WORKSPACE_ADOPTION_PATIENCE: Duration = Duration::from_millis(200);

/// Builds a [`FileWorkspaceFence`] for any workspace this daemon adopts.
///
/// The daemon serves one workspace per tenant and holds one fence per tenant, so
/// the fence stops being a single start-up value and becomes something the
/// registry asks for by root.
struct FileWorkspaceFences {
    pid: u32,
}

impl WorkspaceFenceFactory for FileWorkspaceFences {
    fn fence_for(&self, workspace_root: &Path) -> Box<dyn WorkspaceFence + Send> {
        Box::new(FileWorkspaceFence {
            path: paths::workspace_fence_path(workspace_root),
            workspace: workspace_root.to_path_buf(),
            pid: self.pid,
            patience: WORKSPACE_ADOPTION_PATIENCE,
            held: RefCell::new(None),
        })
    }
}

/// Opens one workspace's lifecycle runtime with the real git and filesystem
/// seams, reading the shared catalogs from the data home every workspace shares.
struct SystemTenantOpener {
    data_home: PathBuf,
    generation: usagi_core::domain::id::DaemonGeneration,
}

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

impl WorkspaceFence for FileWorkspaceFence {
    fn acquire(&self) -> std::io::Result<WorkspaceFenceOutcome> {
        const POLL: Duration = Duration::from_millis(20);
        // `<workspace>/.usagi` is user-visible project metadata, so it keeps
        // ordinary directory permissions; only the `daemon/` child holding the
        // fence is private, which `open_private_lock` establishes.
        std::fs::create_dir_all(self.workspace.join(paths::STATE_DIR))?;
        let file = open_private_lock(
            &self.path,
            "daemon workspace fence",
            PrivateLockModePolicy::OwnerLegacy0644,
        )?;
        let deadline = Instant::now() + self.patience;
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
            PrivateLockModePolicy::OwnerLegacy0644,
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

fn run_broker_lifecycle_command(command: CliDaemonCommand) -> Option<std::io::Result<()>> {
    if command == CliDaemonCommand::BootstrapBroker {
        return Some((|| {
            let data_dir =
                paths::data_dir().map_err(|error| std::io::Error::other(format!("{error:#}")))?;
            serve_bootstrap_broker(
                &data_dir,
                &std::env::current_dir()?,
                &std::env::current_exe()?,
                BrokerIdlePolicy::production(),
            )
        })());
    }
    None
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
fn run_inner(
    out: &mut dyn Write,
    command: CliDaemonCommand,
    info: &AppInfo,
    operation: Option<usagi_core::infrastructure::ipc::OperationId>,
) -> std::io::Result<()> {
    if let Some(result) = run_broker_lifecycle_command(command) {
        return result;
    }
    let data_dir = prepare_private_data_dir()?;
    let daemon_dir = data_dir.join("daemon");
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
            // neither of which is the workspace anyone meant. Worse, when the
            // workspace resolves to the home directory, the workspace fence
            // (`<workspace>/.usagi/daemon/daemon.lock`) and the single-instance
            // lock (`<data-dir>/daemon/daemon.lock`) name the same file under the
            // default `~/.usagi` data home, and the daemon refuses its own start
            // as "already running". `lifecycle_command` already pins the
            // directory for a cold start from a client; a supervised start needs
            // the same pin.
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
    let rollover = IpcRolloverRequester {
        data_dir: &data_dir,
        launcher: &launcher,
    };
    let lock = FileInstanceLock {
        path: daemon_dir.join("daemon.lock"),
        held: RefCell::new(None),
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
    let ready = IpcReady::new(&data_dir, &workspace_root, &lock);
    let shutdown = SignalShutdown::new(Arc::clone(&ready.shutdown));
    let census = DurableResourceCensus {
        data_dir: data_dir.clone(),
    };
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
        seamless: observed_seamless_refusal(&data_dir),
        rollover: &rollover,
    };
    // A stop that leaves the broker running leaves a usagi process the operator
    // has no command to end. Retirement follows the stop rather than preceding
    // it, so a refused stop keeps the broker that a later cold start needs.
    let stopping = matches!(command, PresentationDaemonCommand::Stop(_));
    let outcome = usagi_daemon::presentation::run(out, command, info, &env);
    if stopping && outcome.is_ok() {
        retire_bootstrap_broker(&data_dir, &workspace_root, &launcher.exe);
    }
    outcome
}

/// Ask the broker for `workspace` and `exe` to close its endpoint.
///
/// Best effort by construction: there may be no broker (nothing started one, or
/// it already retired), and a daemon that is going away anyway must not fail a
/// stop because a helper could not be reached.
fn retire_bootstrap_broker(data_dir: &Path, workspace: &Path, exe: &Path) {
    let Ok(exe) = exe.canonicalize() else {
        return;
    };
    let Ok(workspace) = paths::canonical_workspace_root(workspace) else {
        return;
    };
    let address = bootstrap_broker_address(data_dir, &workspace, &exe);
    let _ = request_bootstrap_broker(&address, BROKER_STOP);
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

/// The state subtree of an already adopted `workspace_root`, without creating
/// one.
///
/// A standby hydrates read-only — every write belongs to the active generation —
/// so an unadopted workspace is reported as uninitialized rather than adopted
/// here.
fn adopted_workspace_state_dir(
    daemon_dir: &Path,
    workspace_root: &Path,
) -> std::io::Result<PathBuf> {
    workspace_state::owner(daemon_dir, workspace_root)
        .map_err(|error| std::io::Error::other(format!("{error:#}")))?
        .map(|state| state.dir().to_path_buf())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "durable runtime state is not initialized; a standby hydrates it read-only",
            )
        })
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
    workspace: &ClientWorkspace,
    connect: impl Fn(&Path, &BuildIdentity) -> std::io::Result<IpcClient<S>>,
) -> Result<IpcClient<S>, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let exe =
        std::env::current_exe().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let expected_build = current_build();
    let _bootstrap_lock =
        match acquire_bootstrap_lock_io_within(&data_dir, PrivateLockWait::BOOTSTRAP) {
            Ok(lock) => lock,
            Err(lock_error) if lock_error.kind() == std::io::ErrorKind::PermissionDenied => {
                if request_broker_start(&data_dir, workspace, &exe).is_err() {
                    return Err(map_bootstrap_lock_error(&lock_error));
                }
                for _ in 0..40 {
                    if let Ok(client) = connect(&data_dir, &expected_build) {
                        return match build_artifact_decision(
                            client.server_build(),
                            &expected_build,
                            false,
                        ) {
                            BuildArtifactDecision::Reuse => Ok(client),
                            BuildArtifactDecision::ForceReplace
                            | BuildArtifactDecision::RolloverTrigger => {
                                Err(ClientError::RolloverRequired(
                                    build_rollover_trigger(
                                        client.server_build(),
                                        &expected_build,
                                        runtime_channel(),
                                        false,
                                    )
                                    .ok_or(ClientError::BuildIdentityUnavailable)?,
                                ))
                            }
                            BuildArtifactDecision::Unknown => {
                                Err(ClientError::BuildIdentityUnavailable)
                            }
                        };
                    }
                    RealSleeper.sleep();
                }
                return Err(ClientError::Unavailable(
                    "daemon bootstrap broker started no reachable daemon".into(),
                ));
            }
            Err(lock_error) => return Err(map_bootstrap_lock_error(&lock_error)),
        };
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
        Err(bootstrap::BootstrapError::RolloverRequired(trigger))
            if paths::runtime_mode() == paths::RuntimeMode::Development =>
        {
            // Keyed by the artifact this daemon advertises, so a client whose own
            // build no longer exists on disk asks once instead of once per lane.
            let may_attempt = ATTEMPTED_REPLACEMENTS.claim(&trigger.running_artifact);
            match bootstrap::replace_or_reuse(
                || connect(&data_dir, &expected_build),
                // Planned, never forced. A rebuild is not a reason to destroy the
                // Agent conversations this daemon owns for another client: its
                // census picks a cold transition only when nothing is live, and a
                // seamless rollover keeps the old PTY masters alive otherwise
                // (#507 / #559).
                || run_lifecycle_with(&exe, &["daemon", "restart"], "restart"),
                &expected_build,
                IpcClient::server_build,
                may_attempt,
            ) {
                Ok(bootstrap::DevelopmentConnection::Replaced(stream)) => Ok(stream),
                Ok(bootstrap::DevelopmentConnection::Reused { stream, reason }) => {
                    if let Some(entry) = reused_build_mismatch_record(&trigger, &reason) {
                        ErrorLog::record(&entry);
                    }
                    Ok(stream)
                }
                Err(error) => Err(error),
            }
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

/// Development's one-attempt-per-daemon-artifact guard
/// ([`bootstrap::OncePerArtifact`]).
static ATTEMPTED_REPLACEMENTS: bootstrap::OncePerArtifact = bootstrap::OncePerArtifact::new();

/// The daemon artifacts whose reuse this process has already recorded, so a
/// standing mismatch costs one log line instead of one per bootstrapped lane.
static LOGGED_MISMATCHES: bootstrap::OncePerArtifact = bootstrap::OncePerArtifact::new();

/// The log entry for a development client that keeps talking to a daemon built
/// from another artifact, or `None` when this process already recorded that same
/// standing mismatch.
///
/// Reusing the daemon preserves live Agent conversations, but a stale client is
/// exactly what to look for when a freshly built binary behaves like an older
/// one, so the deliberate mismatch leaves a trail instead of being silent. Every
/// bootstrapped lane observes the same mismatch, hence one entry per daemon
/// artifact rather than one per connection. Only the artifact identities and the
/// non-sensitive reason are recorded.
fn reused_build_mismatch_record(trigger: &BuildRolloverTrigger, reason: &str) -> Option<String> {
    LOGGED_MISMATCHES
        .claim(&trigger.running_artifact)
        .then(|| {
            format!(
                "development client reused the daemon build {} instead of replacing it with {}: {reason}",
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
fn route_cache(data_dir: &Path) -> &'static Mutex<usagi_core::usecase::owner_routing::RouteCache> {
    static CACHE: OnceLock<Mutex<usagi_core::usecase::owner_routing::RouteCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(usagi_core::usecase::owner_routing::RouteCache::new(
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
fn invalidate_routes() {
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
fn owner_endpoint(
    generation: usagi_core::domain::id::DaemonGeneration,
) -> Result<usagi_core::usecase::owner_routing::TrustedEndpoint, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let mut cache = route_cache(&data_dir)
        .lock()
        .map_err(|_| ClientError::Unavailable("generation routing cache is poisoned".into()))?;
    cache
        .owner(generation)
        .map_err(|error| error.to_client_error())
}

/// Every generation a scope inventory must be asked, active first.
///
/// A scope query has more than one answer while a generation is draining, and
/// taking only the active one's would read the draining generation's terminals
/// as absent. Absence is what collects a tab, so the fan-out is what keeps a
/// terminal whose owner is merely busy from being reaped.
pub(crate) fn trusted_generations()
-> Result<Vec<usagi_core::usecase::owner_routing::TrustedEndpoint>, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let mut cache = route_cache(&data_dir)
        .lock()
        .map_err(|_| ClientError::Unavailable("generation routing cache is poisoned".into()))?;
    cache
        .every_generation()
        .map_err(|error| error.to_client_error())
}

/// One lane, together with the role of the generation it reached.
pub(crate) struct OwnerLane {
    pub(crate) client: LaneClient,
    pub(crate) role: usagi_core::infrastructure::ipc::GenerationRole,
}

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
            usagi_core::usecase::owner_routing::RoutingError::UnknownGeneration(generation)
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
fn connect_draining(
    policy: ClientPolicy,
    endpoint: &usagi_core::usecase::owner_routing::TrustedEndpoint,
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
    acquire_bootstrap_lock_io_within(data_dir, wait)
        .map_err(|error| map_bootstrap_lock_error(&error))
}

fn acquire_bootstrap_lock_io_within(
    data_dir: &Path,
    wait: PrivateLockWait,
) -> std::io::Result<std::fs::File> {
    (|| {
        ensure_private_dir_all(data_dir)?;
        // `open_private_lock` runs `ensure_private_dir` on the lock's parent, so
        // creating (and directory-locking) `daemon/` here as well would double
        // the setup locking every bootstrap performs on the shared data dir.
        let path = data_dir.join("daemon").join("bootstrap.lock");
        lock_private_exclusive(
            &path,
            "bootstrap lock",
            PrivateLockModePolicy::OwnerLegacy0644,
            wait,
        )
    })()
}

fn map_bootstrap_lock_error(error: &std::io::Error) -> ClientError {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        ClientError::BootstrapContended
    } else {
        ClientError::Unavailable(error.to_string())
    }
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

    use usagi_daemon::usecase::generic_terminal::TerminalStoreSnapshot;
    use usagi_daemon::usecase::runtime::{RuntimeStore, RuntimeStoreSnapshot};

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
    use usagi_daemon::presentation::ipc::encode_terminal_response;
    use usagi_daemon::usecase::terminal::SnapshotWire;
    use usagi_daemon::usecase::terminal_ipc::{
        ResolvedTerminalScope, TerminalScopeResolveError, TerminalScopeResolver,
    };
    use usagi_daemon::usecase::terminal_owner::{TerminalOwner, TerminalRequestContext};

    #[cfg(unix)]
    #[test]
    fn readiness_timeout_coalesces_and_reaps_the_exact_child() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = tempfile::tempdir().unwrap();
        let program = fixture.path().join("codex");
        let pid_file = fixture.path().join("pid");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\necho $$ >> '{}'\ntrap '' TERM\nwhile :; do :; done\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let script = program.to_string_lossy().into_owned();
        let readiness = Arc::new(SystemAgentReadiness {
            state: Mutex::new(ReadinessState::default()),
            completed: Condvar::new(),
            timeout: Duration::from_millis(150),
            terminate_grace: Duration::from_millis(50),
        });
        let first = {
            let readiness = Arc::clone(&readiness);
            let script = script.clone();
            std::thread::spawn(move || readiness.ready_command("codex", "/bin/sh", &[&script]))
        };
        let started = Instant::now();
        while !pid_file.is_file() && started.elapsed() < Duration::from_secs(1) {
            std::thread::yield_now();
        }
        assert!(pid_file.is_file(), "fixture readiness child started");
        let second = {
            let readiness = Arc::clone(&readiness);
            std::thread::spawn(move || readiness.ready_command("codex", "/bin/sh", &[&script]))
        };
        assert_eq!(first.join().unwrap(), AgentReadiness::Unavailable);
        assert_eq!(second.join().unwrap(), AgentReadiness::Unavailable);

        let pids = std::fs::read_to_string(pid_file).unwrap();
        let pids = pids.lines().collect::<Vec<_>>();
        assert_eq!(pids.len(), 1, "concurrent callers share one provider child");
        let pid = pids[0].parse::<libc::pid_t>().unwrap();
        // SAFETY: signal 0 only observes whether the fixture PID remains.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH),
            "timed-out readiness child was reaped"
        );
    }

    #[test]
    fn readiness_is_distinct_from_install_and_rejects_unauthenticated_status() {
        assert_eq!(
            readiness_from_observation(&ChildObservation::EmptyOutput),
            AgentReadiness::Ready
        );
        assert_eq!(
            readiness_from_observation(&ChildObservation::ExitFailure),
            AgentReadiness::Unavailable
        );
        assert_eq!(
            readiness_from_observation(&ChildObservation::TimedOut),
            AgentReadiness::Unavailable
        );
        assert_eq!(
            readiness_from_observation(&ChildObservation::OutputTooLarge),
            AgentReadiness::Unavailable
        );
    }

    fn request_terminal_json(
        owner: &mut dyn TerminalOwner,
        connection: ConnectionId,
        client: ClientId,
        request_id: RequestId,
        _action: TerminalAction,
        payload: serde_json::Value,
        wire: SnapshotWire,
    ) -> Result<serde_json::Value, usagi_core::infrastructure::ipc::ProtocolError> {
        let request = serde_json::from_value(payload).unwrap();
        owner
            .handle(
                TerminalRequestContext {
                    connection,
                    client,
                    request: request_id,
                },
                request,
            )
            .map(|response| encode_terminal_response(response, wire))
    }

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
            patience: WORKSPACE_FENCE_PATIENCE,
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
    fn workspace_fence_narrows_the_exact_legacy_owner_mode() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir_in("/tmp").unwrap();
        let fence = workspace_fence(workspace.path(), 4242);
        std::fs::create_dir_all(fence.workspace.join(paths::STATE_DIR)).unwrap();
        ensure_private_dir(fence.path.parent().unwrap()).unwrap();
        std::fs::write(&fence.path, []).unwrap();
        std::fs::set_permissions(&fence.path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(fence.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);
        assert_eq!(
            std::fs::metadata(&fence.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
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
            bound_workspace_root(&daemon, &workspace.path().join(".")).unwrap(),
            paths::canonical_workspace_root(workspace.path()).unwrap()
        );

        // A startup directory that no longer resolves is a startup failure, not a
        // fence that silently keys some other path.
        let error = bound_workspace_root(&daemon, &workspace.path().join("absent")).unwrap_err();
        assert!(error.to_string().contains("workspace root"), "{error}");

        // Unreadable durable state fails the same way, rather than falling back
        // to a candidate the runtime would not adopt. Here the unreadable
        // document is the workspace's own, inside its state subtree.
        let canonical = paths::canonical_workspace_root(workspace.path()).unwrap();
        let state_dir = workspace_state::resolve(&daemon, &canonical)
            .unwrap()
            .dir()
            .to_path_buf();
        std::fs::write(state_dir.join("sessions.json"), "not json").unwrap();
        assert!(
            bound_workspace_root(&daemon, workspace.path())
                .unwrap_err()
                .to_string()
                .contains("Storage")
        );
    }

    /// A CLI or MCP client is as entitled to open a workspace as the TUI is.
    /// Refusing here is what forced an operator to open every new repository in
    /// the TUI once before their CLI would work in it.
    #[test]
    fn a_bound_client_adopts_the_repository_it_is_running_inside() {
        use usagi_core::infrastructure::ipc::WorkspaceResolver;

        let temporary = tempfile::tempdir_in("/tmp").unwrap();
        let data = temporary.path().join("data");
        let held = temporary.path().join("held");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&held).unwrap();
        let daemon_dir = data.join("daemon");
        let held_root = paths::canonical_workspace_root(&held).unwrap();
        let tenants = Arc::new(TenantRegistry::new(
            daemon_dir.clone(),
            FileWorkspaceFences {
                pid: std::process::id(),
            },
            SystemTenantOpener {
                data_home: data.clone(),
                generation: usagi_core::domain::id::DaemonGeneration::new(),
            },
            DEFAULT_TENANT_LIMIT,
        ));
        tenants.adopt_initial(&held_root).unwrap();
        let resolver = TenantWorkspaces {
            tenants: Arc::clone(&tenants),
            daemon_dir,
            initial: held_root.clone(),
        };
        let wire = |root: &Path| paths::wire_workspace_root(root);

        // Standing in a plain directory opens nothing.
        let outside = tempfile::tempdir_in("/tmp").unwrap();
        let refusal = resolver
            .resolve(Some(&ClientWorkspace::Bound {
                root: wire(&paths::canonical_workspace_root(outside.path()).unwrap()),
            }))
            .unwrap_err();
        assert!(usagi_core::infrastructure::ipc::is_workspace_mismatch(
            &refusal
        ));
        assert!(
            refusal.message.contains("run this from a repository root"),
            "the refusal gives the caller no next step: {refusal:?}"
        );
        assert!(
            refusal.message.contains("usagi open"),
            "the refusal omits the way to open a directory that is not a repository: {refusal:?}"
        );
        // The refusal names what this daemon really holds, not the root it just
        // refused — naming that one is what made the message contradict itself.
        assert!(refusal.message.contains(&wire(&held_root)), "{refusal:?}");
        assert_eq!(tenants.adopted().len(), 1);

        // Standing *at* a repository this daemon has never seen opens it. That
        // is the whole of what a bound declaration may open.
        let project = temporary.path().join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(project.join("crates/core")).unwrap();
        std::fs::create_dir_all(project.join(".usagi/sessions/worker/.git")).unwrap();
        let project_root = paths::canonical_workspace_root(&project).unwrap();

        // Below it, nothing is opened. A dotfiles repository at `$HOME` is an
        // ordinary setup, so searching upwards would let `usagi session create`
        // in any plain directory under it fence `$HOME` and open a branch in the
        // caller's dotfiles. Standing at a repository says which workspace is
        // meant; standing anywhere underneath one does not.
        for below in [
            project_root.join("crates/core"),
            project_root.join(".usagi/sessions/worker"),
        ] {
            let refusal = resolver
                .resolve(Some(&ClientWorkspace::Bound { root: wire(&below) }))
                .unwrap_err();
            assert!(
                usagi_core::infrastructure::ipc::is_workspace_mismatch(&refusal),
                "{below:?} opened a workspace from below its root"
            );
        }
        assert_eq!(tenants.adopted().len(), 1);

        assert_eq!(
            resolver
                .resolve(Some(&ClientWorkspace::Bound {
                    root: wire(&project_root),
                }))
                .unwrap(),
            wire(&project_root)
        );
        assert_eq!(tenants.adopted().len(), 2);

        // Once it is open, everything below it resolves to it again — that is
        // ancestor matching, which this narrowing does not touch.
        for below in [
            project_root.join("crates/core"),
            project_root.join(".usagi/sessions/worker"),
        ] {
            assert_eq!(
                resolver
                    .resolve(Some(&ClientWorkspace::Bound { root: wire(&below) }))
                    .unwrap(),
                wire(&project_root),
                "{below:?} did not resolve to the workspace that owns it"
            );
        }
        assert_eq!(tenants.adopted().len(), 2);
    }

    /// The real activity observer over a fixture data directory.
    fn daemon_activity(
        data: &Path,
        root: &Path,
        generation: usagi_core::domain::id::DaemonGeneration,
        tenants: &Arc<TenantRegistry<FileWorkspaceFences, SystemTenantOpener>>,
    ) -> DaemonWorkspaceActivity {
        let children = Arc::new(SpawnedChildren::default());
        let metrics = Arc::new(TerminalPipelineMetrics::default());
        DaemonWorkspaceActivity {
            terminal: new_terminal_runtime(
                data,
                generation,
                root.to_path_buf(),
                DaemonPty::new(Arc::clone(&metrics), Arc::clone(&children)).0,
                Arc::clone(tenants) as Workspaces,
                Arc::new(UserEnvironment::new(data.to_path_buf(), OpCli)),
                usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention::new(),
                &children,
                false,
            )
            .unwrap(),
            agent: open_agent_runtime(
                data,
                generation,
                Arc::clone(tenants) as Workspaces,
                AgentPty::new(terminal_environment(), metrics, Arc::clone(&children)).0,
                std::env::current_exe().unwrap(),
                Arc::new(UserEnvironment::new(data.to_path_buf(), OpCli)),
                usagi_daemon::usecase::terminal_retention_ipc::SharedTerminalRetention::new(),
                AgentConcurrencyGauge::default(),
                &children,
                false,
            )
            .unwrap(),
        }
    }

    /// The daemon-wide registries are keyed by session alone, so what they may
    /// keep is every session this *data directory* knows — not the sessions of
    /// the workspaces held right now. A workspace given back by retirement still
    /// owns its sessions, and pruning against a set that lost them would delete
    /// the user's own PR records for a workspace that is merely closed.
    #[test]
    fn a_closed_workspace_still_counts_as_owning_its_sessions() {
        let temporary = tempfile::tempdir_in("/tmp").unwrap();
        let data = temporary.path().join("data");
        let workspace = temporary.path().join("workspace");
        for directory in [&data, &workspace] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let daemon_dir = data.join("daemon");
        ensure_private_dir_all(&daemon_dir).unwrap();
        let root = paths::canonical_workspace_root(&workspace).unwrap();
        let generation = usagi_core::domain::id::DaemonGeneration::new();
        let tenants = Arc::new(TenantRegistry::new(
            daemon_dir.clone(),
            FileWorkspaceFences {
                pid: std::process::id(),
            },
            SystemTenantOpener {
                data_home: data.clone(),
                generation,
            },
            DEFAULT_TENANT_LIMIT,
        ));

        // Nothing opened yet: no session is known, and the empty answer is a
        // fact rather than a read failure.
        assert_eq!(
            known_sessions(&daemon_dir),
            Some(std::collections::BTreeSet::new())
        );

        let tenant = tenants.adopt_initial(&root).unwrap();
        let session = {
            let mut runtime = tenant.runtime().lock().unwrap();
            let created = runtime
                .handle(
                    usagi_core::usecase::client::SessionAction::Create,
                    &usagi_core::domain::id::OperationId::new().to_string(),
                    &serde_json::json!({"name": "kept"}),
                )
                .unwrap();
            serde_json::from_value::<SessionId>(created.body["sessions"][0]["session_id"].clone())
                .unwrap()
        };
        assert!(known_sessions(&daemon_dir).unwrap().contains(&session));

        // Giving the workspace back does not un-own its sessions: the lifecycle
        // document is still there, and it is the authority.
        drop(tenant);
        assert!(tenants.retire(&root));
        assert!(tenants.adopted().is_empty());
        assert!(known_sessions(&daemon_dir).unwrap().contains(&session));

        // A subtree that cannot be read is not "no sessions": pruning on a
        // partial view is exactly the deletion this guards against.
        std::fs::write(
            daemon_dir
                .join(paths::WORKSPACE_STATE_DIR)
                .join(paths::workspace_state_digest(&root))
                .join("sessions.json"),
            "not json",
        )
        .unwrap();
        assert_eq!(known_sessions(&daemon_dir), None);
    }

    /// A workspace with nothing left to do is given back, and one with work is
    /// not. The observation fails closed on every side: a runtime that cannot be
    /// read keeps its workspace, because keeping one costs a fence while
    /// releasing a working one hands its worktrees to a second owner.
    #[test]
    fn an_idle_workspace_is_released_and_a_working_one_is_kept() {
        use usagi_daemon::usecase::tenant::WorkspaceActivity;

        let temporary = tempfile::tempdir_in("/tmp").unwrap();
        let data = temporary.path().join("data");
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        for directory in [&data, &first, &second] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let daemon_dir = data.join("daemon");
        ensure_private_dir_all(&daemon_dir).unwrap();
        let first_root = paths::canonical_workspace_root(&first).unwrap();
        let second_root = paths::canonical_workspace_root(&second).unwrap();
        let generation = usagi_core::domain::id::DaemonGeneration::new();
        let tenants = Arc::new(TenantRegistry::new(
            daemon_dir,
            FileWorkspaceFences {
                pid: std::process::id(),
            },
            SystemTenantOpener {
                data_home: data.clone(),
                generation,
            },
            DEFAULT_TENANT_LIMIT,
        ));
        let initial = tenants.adopt_initial(&first_root).unwrap();
        let adopted = tenants.adopt(&second_root).unwrap();

        // A fresh workspace has no runtime and no unfinished lifecycle work, so
        // the real observer reports it idle; a session mid-creation does not.
        let activity = daemon_activity(&data, &first_root, generation, &tenants);
        assert!(!activity.has_work(adopted.workspace_id(), adopted.runtime()));

        // A handle held outside the registry keeps the workspace whatever the
        // observation says, so the sweep only sees it once the handle is gone.
        let now = chrono::Utc::now();
        let idle_for = chrono::Duration::zero();
        assert!(tenants.retire_idle(&activity, now, idle_for).is_empty());
        drop(adopted);

        // The worker gives it back and leaves the startup workspace alone.
        let shutdown = Arc::new(ShutdownRequest::new());
        spawn_tenant_retire_worker(
            Arc::clone(&tenants),
            activity,
            Arc::clone(&shutdown),
            Duration::from_millis(5),
            Duration::ZERO,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while tenants.adopted().len() > 1 {
            assert!(
                Instant::now() < deadline,
                "the idle workspace was not released"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            tenants
                .adopted()
                .iter()
                .map(|tenant| tenant.root().to_path_buf())
                .collect::<Vec<_>>(),
            vec![initial.root().to_path_buf()]
        );
        shutdown.request();
    }

    /// An observation that cannot be made keeps the workspace.
    #[test]
    fn an_unreadable_runtime_keeps_its_workspace() {
        use usagi_daemon::usecase::tenant::WorkspaceActivity;

        let temporary = tempfile::tempdir_in("/tmp").unwrap();
        let data = temporary.path().join("data");
        let workspace = temporary.path().join("workspace");
        for directory in [&data, &workspace] {
            std::fs::create_dir_all(directory).unwrap();
        }
        ensure_private_dir_all(&data.join("daemon")).unwrap();
        let generation = usagi_core::domain::id::DaemonGeneration::new();
        let root = paths::canonical_workspace_root(&workspace).unwrap();
        let tenants = Arc::new(TenantRegistry::new(
            data.join("daemon"),
            FileWorkspaceFences {
                pid: std::process::id(),
            },
            SystemTenantOpener {
                data_home: data.clone(),
                generation,
            },
            DEFAULT_TENANT_LIMIT,
        ));
        let tenant = tenants.adopt_initial(&root).unwrap();
        let activity = daemon_activity(&data, &root, generation, &tenants);
        assert!(!activity.has_work(tenant.workspace_id(), tenant.runtime()));

        // A lifecycle runtime whose lock is poisoned cannot be read, so the
        // workspace is kept rather than released on an unknown state.
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = tenant.runtime().lock().unwrap();
            panic!("a reader panicked while holding the lifecycle runtime");
        }));
        assert!(poisoned.is_err());
        assert!(activity.has_work(tenant.workspace_id(), tenant.runtime()));
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Every declaration and refusal in one flow.
    fn the_handshake_resolves_a_selected_workspace_by_adopting_it() {
        use usagi_core::infrastructure::ipc::WorkspaceResolver;

        let temporary = tempfile::tempdir_in("/tmp").unwrap();
        let data = temporary.path().join("data");
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        for directory in [&data, &first, &second] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let daemon_dir = data.join("daemon");
        let first_root = paths::canonical_workspace_root(&first).unwrap();
        let second_root = paths::canonical_workspace_root(&second).unwrap();
        let generation = usagi_core::domain::id::DaemonGeneration::new();
        let tenants = Arc::new(TenantRegistry::new(
            daemon_dir.clone(),
            FileWorkspaceFences {
                pid: std::process::id(),
            },
            SystemTenantOpener {
                data_home: data.clone(),
                generation,
            },
            DEFAULT_TENANT_LIMIT,
        ));
        let initial = tenants.adopt_initial(&first_root).unwrap();
        let workspaces: Workspaces = tenants.clone();
        let resolver = TenantWorkspaces {
            tenants: Arc::clone(&tenants),
            daemon_dir: daemon_dir.clone(),
            initial: first_root.clone(),
        };
        let wire = |root: &Path| paths::wire_workspace_root(root);

        // A client that names no workspace is answered with the one this process
        // started in: it reads no workspace state either way.
        for declared in [None, Some(ClientWorkspace::Unbound)] {
            assert_eq!(
                resolver.resolve(declared.as_ref()).unwrap(),
                wire(&first_root)
            );
        }

        // Selecting a workspace this daemon has never seen adopts it, and the
        // second selection is the same tenant rather than a second adoption.
        let selected = ClientWorkspace::Selected {
            root: wire(&second_root),
        };
        assert_eq!(
            resolver.resolve(Some(&selected)).unwrap(),
            wire(&second_root)
        );
        assert_eq!(
            resolver.resolve(Some(&selected)).unwrap(),
            wire(&second_root)
        );
        assert_eq!(tenants.adopted().len(), 2);

        // A bound client resolves to the workspace containing it, including from
        // a path that no longer exists — a worktree its own teardown removed.
        for candidate in [
            second_root.clone(),
            second_root.join(".usagi/sessions/gone"),
        ] {
            let bound = ClientWorkspace::Bound {
                root: wire(&candidate),
            };
            assert_eq!(resolver.resolve(Some(&bound)).unwrap(), wire(&second_root));
        }

        // A selected root that does not resolve on this machine is refused, and
        // nothing is adopted for it.
        let refusal = resolver
            .resolve(Some(&ClientWorkspace::Selected {
                root: wire(&temporary.path().join("absent")),
            }))
            .unwrap_err();
        assert!(usagi_core::infrastructure::ipc::is_workspace_mismatch(
            &refusal
        ));
        assert_eq!(tenants.adopted().len(), 2);

        // A workspace this data directory has opened before keeps answering for
        // the clients inside it, even once it has been given back: its state
        // subtree records the root, so the resolution adopts it again. Without
        // this, a workspace that idled out of tenancy would refuse the very CLI
        // and MCP clients running in it.
        assert!(tenants.retire(&second_root));
        let inside = ClientWorkspace::Bound {
            root: wire(&second_root.join("nested")),
        };
        assert_eq!(resolver.resolve(Some(&inside)).unwrap(), wire(&second_root));
        assert!(
            tenants.tenant(&second_root).is_some(),
            "resolving a known workspace adopts it again"
        );

        // The connection binds the workspace its handshake settled on.
        for (declared, expected) in [
            (None, first_root.clone()),
            (Some(ClientWorkspace::Unbound), first_root.clone()),
            (Some(selected.clone()), second_root.clone()),
            (
                Some(ClientWorkspace::Bound {
                    root: wire(&second_root.join("nested")),
                }),
                second_root.clone(),
            ),
        ] {
            let bound = connection_workspace(&workspaces, &initial, declared.as_ref())
                .expect("the handshake resolved this workspace");
            assert_eq!(bound.tenant.root(), expected);
        }

        // A workspace retired between the handshake and the lookup closes the
        // connection instead of serving another workspace's state.
        assert!(tenants.retire(&second_root));
        assert!(connection_workspace(&workspaces, &initial, Some(&selected)).is_none());
    }

    #[test]
    fn the_fence_factory_owns_one_workspace_per_root() {
        let first = tempfile::tempdir_in("/tmp").unwrap();
        let second = tempfile::tempdir_in("/tmp").unwrap();
        let fences = FileWorkspaceFences { pid: 4242 };

        // Each root gets its own fence node, so owning one workspace never
        // implies owning another.
        let held = fences.fence_for(first.path());
        assert_eq!(held.acquire().unwrap(), WorkspaceFenceOutcome::Acquired);
        assert_eq!(
            fences.fence_for(second.path()).acquire().unwrap(),
            WorkspaceFenceOutcome::Acquired
        );

        // A second owner of the same root is refused and names the holder, which
        // is what lets one workspace be refused without disturbing the rest.
        let contender = std::thread::spawn({
            let root = first.path().to_path_buf();
            move || FileWorkspaceFences { pid: 5252 }.fence_for(&root).acquire()
        })
        .join()
        .unwrap()
        .unwrap();
        assert_eq!(
            contender,
            WorkspaceFenceOutcome::Held {
                workspace: first.path().display().to_string(),
                owner: Some(4242),
            }
        );
    }

    #[test]
    fn workspace_state_resolution_reports_every_failure_it_can_meet() {
        let workspace = tempfile::tempdir_in("/tmp").unwrap();
        let canonical = paths::canonical_workspace_root(workspace.path()).unwrap();
        let daemon = workspace.path().join("data/daemon");
        ensure_private_dir_all(&daemon).unwrap();

        // A legacy document that cannot be parsed names the workspace this
        // daemon would otherwise adopt, so the start fails instead of adopting
        // the startup directory in its place.
        let legacy = daemon.join("sessions.json");
        std::fs::write(&legacy, "not json").unwrap();
        let error = bound_workspace_root(&daemon, workspace.path()).unwrap_err();
        assert!(error.to_string().contains("sessions.json"), "{error}");
        std::fs::remove_file(&legacy).unwrap();

        // A container that cannot be enumerated is reported rather than read as
        // "no workspace has been adopted", which would adopt a second subtree
        // for a workspace that already owns one.
        let container = daemon.join(paths::WORKSPACE_STATE_DIR);
        std::fs::write(&container, "").unwrap();
        for error in [
            bound_workspace_root(&daemon, workspace.path()).unwrap_err(),
            adopted_workspace_state_dir(&daemon, &canonical).unwrap_err(),
        ] {
            assert!(error.to_string().contains("could not"), "{error}");
        }
    }

    #[test]
    fn bound_workspace_root_migrates_a_legacy_document_and_prefers_the_adopted_owner() {
        let workspace = tempfile::tempdir_in("/tmp").unwrap();
        let canonical = paths::canonical_workspace_root(workspace.path()).unwrap();
        let daemon = workspace.path().join("data/daemon");
        ensure_private_dir_all(&daemon).unwrap();

        // A data directory written before workspace subtrees existed keeps its
        // lifecycle document beside the locator. The first resolution moves it
        // into the subtree of the workspace it names, and binds that workspace.
        let legacy = daemon.join("sessions.json");
        std::fs::write(
            &legacy,
            format!(
                r#"{{"repository_root":{:?},"state":{{"format":"usagi-workspace-lifecycle","version":{{"major":2,"minor":0}},"workspace_id":"543166c9-3923-4086-b3c9-05a69a66550c","state_revision":0,"sessions":[],"operations":[],"updated_at":"2026-08-20T23:22:45.487133Z"}}}}"#,
                canonical.to_str().unwrap()
            ),
        )
        .unwrap();

        let subdirectory = workspace.path().join("nested/deeper");
        std::fs::create_dir_all(&subdirectory).unwrap();
        assert_eq!(
            bound_workspace_root(&daemon, &subdirectory).unwrap(),
            canonical
        );
        assert!(!legacy.exists());
        let state_dir = workspace_state::resolve(&daemon, &canonical)
            .unwrap()
            .dir()
            .to_path_buf();
        assert!(state_dir.join("sessions.json").is_file());

        // A subdirectory of an adopted workspace resolves to the workspace, so a
        // daemon started there fences what it will actually own rather than
        // adopting the subdirectory as a second workspace.
        assert_eq!(
            adopted_workspace_state_dir(&daemon, &canonical).unwrap(),
            state_dir
        );
        let unadopted = tempfile::tempdir_in("/tmp").unwrap();
        let error = adopted_workspace_state_dir(
            &daemon,
            &paths::canonical_workspace_root(unadopted.path()).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
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

        // Pre-identity daemon builds created their singleton lock through the
        // process umask. The exact owner/single-link 0644 residue is narrowed
        // through the validated descriptor; no other broad mode is accepted.
        std::fs::set_permissions(&broad_instance_lock, std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(broad_instance.acquire().unwrap());
        assert_eq!(
            std::fs::metadata(&broad_instance_lock)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(broad_instance);
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
        let restart = lifecycle_command(&exe, &["daemon", "restart"], Some(opened.clone()));
        assert_eq!(restart.get_current_dir(), Some(opened.as_path()));
        // Development consumes a build-mismatch trigger with a *planned*
        // replacement: the live-runtime guard decides between a cold transition
        // and a seamless rollover, so no `--force` override is passed and a
        // rebuild cannot kill another client's Agent.
        assert_eq!(
            restart.get_args().collect::<Vec<_>>(),
            vec!["daemon", "restart"]
        );
    }

    #[test]
    fn a_reused_development_mismatch_is_recorded_once_per_daemon_artifact() {
        let running = test_build("a");
        let expected = test_build("b");
        let trigger = build_rollover_trigger(&running, &expected, "development", false).unwrap();

        let entry = reused_build_mismatch_record(&trigger, "live runtime preserved")
            .expect("the first observation of a mismatch is recorded");
        assert!(entry.contains(&running.artifact), "{entry}");
        assert!(entry.contains(&expected.artifact), "{entry}");
        assert!(entry.contains("live runtime preserved"), "{entry}");
        // Every bootstrapped lane observes the same standing mismatch, so the
        // trail stays one entry instead of one per connection.
        assert_eq!(
            reused_build_mismatch_record(&trigger, "live runtime preserved"),
            None
        );
    }

    /// A known artifact identity whose source digest is distinguished by `seed`.
    fn test_build(seed: &str) -> BuildIdentity {
        usagi_core::infrastructure::ipc::build_identity(
            "2.0.0",
            "test",
            "test-target",
            "debug",
            &seed.repeat(64),
        )
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
    fn client_bootstrap_reclaims_an_unverified_owner_without_signalling() {
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
        // A legacy record carries no signal identity. The live PID therefore
        // remains unverified, but daemon.lock proves this process does not own
        // the active role. Recovery reclaims the endpoint without ever
        // addressing that PID.
        let record = DaemonRecord::new(std::process::id());
        store.save(&record).unwrap();
        assert_eq!(
            ExactProcessControl.observe(&record),
            DaemonProcessObservation::Unknown
        );

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
                .is_ok(),
            "recovery must not signal the unverified PID"
        );

        // SAFETY: the listener has not moved and still owns normal cleanup.
        unsafe { ManuallyDrop::drop(&mut listener) };
    }

    /// The instance lock this recovery holds excludes another *active* daemon,
    /// not a standby — which holds no lock, so its live socket looks exactly like
    /// a crashed generation's leftover on the filesystem. Sweeping it would leave
    /// the registry naming a verified successor that nobody accepts on, which is
    /// the same hazard the daemon-side sweep already guards against.
    #[test]
    fn client_bootstrap_recovery_preserves_a_live_standby_endpoint() {
        use std::mem::ManuallyDrop;
        use std::os::unix::fs::PermissionsExt;
        use usagi_daemon::usecase::authority::registry::{
            GenerationEntry, REGISTRY_SCHEMA, RegistryDocument,
        };
        use usagi_daemon::usecase::generation::GenerationRole;

        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let data = directory.path();
        let daemon = data.join("daemon");

        // The dead active's published endpoint, and a live standby's private one.
        let mut dead = ManuallyDrop::new(SecureUnixListener::bind(data, ipc_generation()).unwrap());
        let dead_socket = daemon.join(&dead.locator().endpoint);
        let standby = SecureUnixListener::bind_private(data, ipc_generation()).unwrap();
        let standby_socket = daemon.join(&standby.locator().endpoint);
        assert!(dead_socket.exists() && standby_socket.exists());

        let active_generation =
            usagi_core::domain::id::DaemonGeneration::parse(&dead.locator().generation.0).unwrap();
        let standby_generation =
            usagi_core::domain::id::DaemonGeneration::parse(&standby.locator().generation.0)
                .unwrap();
        // The standby's recorded process is this one, which the OS proves alive;
        // the active's is a PID that has been reused, which it cannot.
        let live = own_process_identity(std::process::id()).unwrap();
        let mut gone = live.clone();
        gone.start_identity = "gone".to_owned();
        let entry = |generation, role, endpoint: &str, process: ProcessIdentity| GenerationEntry {
            generation,
            role,
            endpoint: endpoint.to_owned(),
            process,
            expected_build: current_build(),
            verified_build: Some(current_build()),
            revision: 1,
        };
        let document = RegistryDocument {
            schema: REGISTRY_SCHEMA.to_owned(),
            revision: 1,
            current: Some(active_generation),
            generations: vec![
                entry(
                    active_generation,
                    GenerationRole::Active,
                    &dead.locator().endpoint,
                    gone,
                ),
                entry(
                    standby_generation,
                    GenerationRole::Standby,
                    &standby.locator().endpoint,
                    live,
                ),
            ],
            handoff: None,
            completed_operation: None,
        };
        // Written the way the daemon writes it: the private read this recovery
        // performs rejects a world-readable document.
        let registry = daemon.join("generations.json");
        std::fs::write(&registry, serde_json::to_string(&document).unwrap()).unwrap();
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o600)).unwrap();

        // A record whose identity no longer matches its PID is proved stale, which
        // is what admits this recovery at all.
        let store = DaemonRecordStore::new(FsRecordFile {
            path: daemon.join("daemon.json"),
        });
        store
            .save(&DaemonRecord::identified(std::process::id(), "gone"))
            .unwrap();

        assert_eq!(
            recover_stale_client_endpoint(data).unwrap(),
            bootstrap::StaleRecovery::Recovered
        );

        // The crashed generation's residue is reclaimed, and the live standby's
        // socket — which its own process is still accepting on — is not.
        assert!(!dead_socket.exists());
        assert!(
            standby_socket.exists(),
            "client recovery swept a live standby endpoint"
        );
        assert_eq!(store.load().unwrap(), None);

        drop(standby);
        // SAFETY: recovery removed only filesystem artifacts; dropping closes the
        // still-owned listener fd and its cleanup is idempotent.
        unsafe { ManuallyDrop::drop(&mut dead) };
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

    #[test]
    fn dropping_a_shutdown_pipe_joins_its_writer_before_closing_descriptors() {
        let shutdown = Arc::new(ShutdownRequest::new());
        let pipe = ShutdownPipe::mirroring(&shutdown).unwrap();
        assert!(!shutdown.is_requested());

        drop(pipe);

        assert!(shutdown.is_requested());
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

    #[derive(Clone)]
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
            Ok("{\"title\":\"production\",\"state\":\"MERGED\",\"headRefOid\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}".into())
        }
    }

    #[test]
    fn production_pr_worker_rebuilds_publishes_without_locking_and_honors_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let identity =
            usagi_core::domain::pr_inventory::canonicalize("https://github.com/o/r/pull/493")
                .unwrap();
        let inventory = Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
            PrInventoryStore::new(directory.path()),
            GenerationRole::Active,
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
            None,
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
            None,
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
        pending_calls: Arc<AtomicUsize>,
        finalize_error: Option<String>,
    }
    impl TeardownJournal for FakeTeardownJournal {
        fn pending(&self) -> Vec<usagi_daemon::usecase::session_teardown::PendingTeardown> {
            let pending = self.pending.lock().unwrap().clone();
            self.pending_calls.fetch_add(1, Ordering::AcqRel);
            pending
        }
        fn finish(
            &self,
            teardown: &usagi_daemon::usecase::session_teardown::PendingTeardown,
            _outcome: Result<(), String>,
        ) -> Result<(), String> {
            if let Some(error) = &self.finalize_error {
                return Err(error.clone());
            }
            self.pending
                .lock()
                .unwrap()
                .retain(|pending| pending.name != teardown.name);
            Ok(())
        }
    }

    struct FakeTeardownEffect {
        torn_down: Arc<Mutex<Vec<String>>>,
        shutdown: Arc<ShutdownRequest>,
        shutdown_after: usize,
    }
    impl TeardownEffect for FakeTeardownEffect {
        fn tear_down(
            &self,
            teardown: &usagi_daemon::usecase::session_teardown::PendingTeardown,
        ) -> Result<(), String> {
            let mut torn_down = self.torn_down.lock().unwrap();
            torn_down.push(teardown.name.clone());
            if torn_down.len() == self.shutdown_after {
                self.shutdown.request();
            }
            Err("worktree is busy".into())
        }
    }

    #[test]
    fn production_teardown_worker_drains_an_admitted_removal_and_honors_shutdown() {
        let pending = Arc::new(Mutex::new(Vec::new()));
        let pending_calls = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(ShutdownRequest::new());
        let torn_down = Arc::new(Mutex::new(Vec::new()));
        let signal = Arc::new(TeardownSignal::new());

        let handle = spawn_session_teardown_worker(
            FakeTeardownJournal {
                pending: Arc::clone(&pending),
                pending_calls: Arc::clone(&pending_calls),
                finalize_error: Some("session lifecycle owner is unavailable".into()),
            },
            FakeTeardownEffect {
                torn_down: Arc::clone(&torn_down),
                shutdown: Arc::clone(&shutdown),
                shutdown_after: 1,
            },
            Arc::clone(&signal),
            Arc::clone(&shutdown),
            Duration::from_millis(1),
        )
        .unwrap();

        while pending_calls.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        pending
            .lock()
            .unwrap()
            .push(usagi_daemon::usecase::session_teardown::PendingTeardown {
                session_id: SessionId::new(),
                operation_id: usagi_core::domain::id::OperationId::new(),
                name: "one".into(),
                repository_root: PathBuf::from("/repo"),
                data_home: PathBuf::from("/data"),
                session_container: PathBuf::from("/repo/.usagi/sessions"),
                session_root: PathBuf::from("/repo/.usagi/sessions/one"),
                force: false,
                delete_branch: false,
                force_delete_branch: false,
                merged_head_oid: None,
            });
        signal.notify();
        handle.join().unwrap();

        assert_eq!(torn_down.lock().unwrap().as_slice(), ["one"]);
        assert_eq!(pending.lock().unwrap().len(), 1);
        assert_eq!(pending_calls.load(Ordering::Acquire), 2);

        // A worker started under shutdown takes no work at all.
        let already_stopped = Arc::new(ShutdownRequest::new());
        already_stopped.request();
        let untouched = Arc::new(Mutex::new(Vec::new()));
        spawn_session_teardown_worker(
            FakeTeardownJournal {
                pending: Arc::clone(&pending),
                pending_calls: Arc::new(AtomicUsize::new(0)),
                finalize_error: None,
            },
            FakeTeardownEffect {
                torn_down: Arc::clone(&untouched),
                shutdown: Arc::clone(&already_stopped),
                shutdown_after: 1,
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

    #[test]
    fn production_teardown_worker_does_not_reread_an_idle_journal_on_each_tick() {
        let pending_calls = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(ShutdownRequest::new());
        let handle = spawn_session_teardown_worker(
            FakeTeardownJournal {
                pending: Arc::new(Mutex::new(Vec::new())),
                pending_calls: Arc::clone(&pending_calls),
                finalize_error: None,
            },
            FakeTeardownEffect {
                torn_down: Arc::new(Mutex::new(Vec::new())),
                shutdown: Arc::clone(&shutdown),
                shutdown_after: 1,
            },
            Arc::new(TeardownSignal::new()),
            Arc::clone(&shutdown),
            Duration::from_millis(1),
        )
        .unwrap();

        while pending_calls.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(pending_calls.load(Ordering::Acquire), 1);

        shutdown.request();
        handle.join().unwrap();
    }

    #[test]
    fn production_teardown_worker_retries_a_failed_finalization_on_the_tick() {
        let pending = Arc::new(Mutex::new(vec![
            usagi_daemon::usecase::session_teardown::PendingTeardown {
                session_id: SessionId::new(),
                operation_id: usagi_core::domain::id::OperationId::new(),
                name: "one".into(),
                repository_root: PathBuf::from("/repo"),
                data_home: PathBuf::from("/data"),
                session_container: PathBuf::from("/repo/.usagi/sessions"),
                session_root: PathBuf::from("/repo/.usagi/sessions/one"),
                force: false,
                delete_branch: false,
                force_delete_branch: false,
                merged_head_oid: None,
            },
        ]));
        let pending_calls = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(ShutdownRequest::new());
        let torn_down = Arc::new(Mutex::new(Vec::new()));

        spawn_session_teardown_worker(
            FakeTeardownJournal {
                pending,
                pending_calls: Arc::clone(&pending_calls),
                finalize_error: Some("session lifecycle owner is unavailable".into()),
            },
            FakeTeardownEffect {
                torn_down: Arc::clone(&torn_down),
                shutdown: Arc::clone(&shutdown),
                shutdown_after: 2,
            },
            Arc::new(TeardownSignal::new()),
            shutdown,
            Duration::from_millis(1),
        )
        .unwrap()
        .join()
        .unwrap();

        assert_eq!(torn_down.lock().unwrap().as_slice(), ["one", "one"]);
        assert_eq!(pending_calls.load(Ordering::Acquire), 2);
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
            AdmissionGate::new(DaemonGeneration::new(), GenerationRole::Active),
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
    fn draining_custody_ignores_the_record_transferred_to_its_successor() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let (lock, owner, probe) = custody_fixture(home.path());
        let shutdown = Arc::new(ShutdownRequest::new());
        let gate = AdmissionGate::new(DaemonGeneration::new(), GenerationRole::Active);
        gate.close(LeaseClass::ActiveControl);
        gate.await_drain(LeaseClass::ActiveControl).unwrap();
        gate.enter_draining().unwrap();
        let handle = spawn_custody_worker(
            probe,
            owner,
            home.path().to_path_buf(),
            gate,
            Arc::clone(&shutdown),
            Duration::from_millis(5),
        )
        .unwrap();

        DaemonRecordStore::new(FsRecordFile {
            path: home.path().join("daemon/daemon.json"),
        })
        .save(&DaemonRecord::identified(4321, "custody:successor"))
        .unwrap();
        assert!(!wait_for_request(&shutdown, Duration::from_millis(50)));
        shutdown.request();
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

    fn pending_decision(
        store: &UserDecisionStore,
    ) -> usagi_core::domain::user_decision::UserDecision {
        use usagi_core::domain::{
            agent::CallerRef,
            id::{AgentId, OperationId, UserDecisionId},
            user_decision::{
                UserDecision, UserDecisionOption, UserDecisionOwner, UserDecisionStatus,
            },
        };

        store
            .create(UserDecision {
                decision_id: UserDecisionId::new(),
                owner: UserDecisionOwner {
                    workspace_id: WorkspaceId::new(),
                    session_id: Some(SessionId::new()),
                    caller: CallerRef {
                        session_id: Some(SessionId::new()),
                        agent_id: AgentId::new(),
                    },
                    run_id: OperationId::new(),
                },
                title: "Choose".into(),
                prompt: "Continue?".into(),
                options: vec![UserDecisionOption {
                    id: "yes".into(),
                    label: "Yes".into(),
                    description: None,
                }],
                allow_freeform: false,
                expires_at: None,
                idempotency_key: Some("decision-wait-test".into()),
                status: UserDecisionStatus::Pending,
                answer: None,
                created_at: chrono::Utc::now(),
                resolved_at: None,
            })
            .unwrap()
            .unwrap()
    }

    fn wait_until_decision_waiter_is_registered(
        waiters: &DecisionWaiters,
        decision_id: usagi_core::domain::id::UserDecisionId,
    ) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while waiters.waiting_count(decision_id) == 0 {
            assert!(
                Instant::now() < deadline,
                "decision waiter did not register"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn decision_transition_notifies_the_synchronous_waiter() {
        use usagi_core::domain::user_decision::UserDecisionAnswer;

        struct NeverCancelled;
        impl DecisionWaitCancellation for NeverCancelled {
            fn is_cancelled(&self) -> bool {
                false
            }
        }

        let home = tempfile::tempdir_in("/tmp").unwrap();
        let store = Arc::new(UserDecisionStore::new(home.path()));
        let decision = pending_decision(&store);
        let waiters = Arc::new(DecisionWaiters::default());
        let waiting_store = Arc::clone(&store);
        let waiting_registry = Arc::clone(&waiters);
        let requested = decision.clone();
        let handle = std::thread::spawn(move || {
            wait_for_user_decision(
                &waiting_store,
                &waiting_registry,
                &NeverCancelled,
                requested.owner.workspace_id,
                &requested,
            )
        });
        wait_until_decision_waiter_is_registered(&waiters, decision.decision_id);

        store
            .resolve(
                decision.owner.workspace_id,
                decision.decision_id,
                UserDecisionAnswer::Option {
                    option_id: "yes".into(),
                },
                chrono::Utc::now(),
            )
            .unwrap()
            .unwrap();
        waiters.notify(decision.decision_id);

        let response = handle.join().unwrap().unwrap();
        assert_eq!(response["status"], "resolved");
        assert_eq!(waiters.waiting_count(decision.decision_id), 0);
    }

    #[test]
    fn accepted_stream_observes_peer_close_behind_buffered_data() {
        let (server, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let connection = AcceptedStream::new(server);

        std::io::Write::write_all(&mut peer, b"pipelined request").unwrap();
        drop(peer);

        // The close is observed through `poll`, so it lands when the kernel has
        // processed the peer's exit rather than when `drop` returns. A single
        // sample turns that ordinary scheduling delay into a failure on a loaded
        // machine, so the observation is driven until it lands.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !connection.peer_disconnected() {
            assert!(
                Instant::now() < deadline,
                "the peer close was never observed"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn disconnected_client_releases_a_pending_decision_waiter_without_mutating_it() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let store = Arc::new(UserDecisionStore::new(home.path()));
        let decision = pending_decision(&store);
        let waiters = Arc::new(DecisionWaiters::default());
        let (server, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let cancellation = AcceptedStream::new(server);
        let waiting_store = Arc::clone(&store);
        let waiting_registry = Arc::clone(&waiters);
        let requested = decision.clone();
        let handle = std::thread::spawn(move || {
            wait_for_user_decision(
                &waiting_store,
                &waiting_registry,
                &cancellation,
                requested.owner.workspace_id,
                &requested,
            )
        });
        wait_until_decision_waiter_is_registered(&waiters, decision.decision_id);

        drop(peer);
        assert!(matches!(
            handle.join().unwrap(),
            Err(UserDecisionDispatchError::Cancelled)
        ));
        let retained = store
            .get(decision.owner.workspace_id, decision.decision_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            retained.status,
            usagi_core::domain::user_decision::UserDecisionStatus::Pending
        );
        assert_eq!(waiters.waiting_count(decision.decision_id), 0);
    }

    #[test]
    fn rollover_control_barrier_cancels_a_pending_decision_before_waiting_for_its_lease() {
        let home = tempfile::tempdir_in("/tmp").unwrap();
        let store = Arc::new(UserDecisionStore::new(home.path()));
        let decision = pending_decision(&store);
        let waiters = Arc::new(DecisionWaiters::default());
        let (server, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let gate = AdmissionGate::new(DaemonGeneration::new(), GenerationRole::Active);
        let lease = gate.acquire(LeaseClass::ActiveControl).unwrap();
        let cancellation = DecisionConnectionCancellation {
            connection: AcceptedStream::new(server),
            gate: gate.clone(),
        };
        let waiting_store = Arc::clone(&store);
        let waiting_registry = Arc::clone(&waiters);
        let requested = decision.clone();
        let handle = std::thread::spawn(move || {
            let _lease = lease;
            assert!(matches!(
                wait_for_user_decision(
                    &waiting_store,
                    &waiting_registry,
                    &cancellation,
                    requested.owner.workspace_id,
                    &requested,
                ),
                Err(UserDecisionDispatchError::Cancelled)
            ));
        });
        wait_until_decision_waiter_is_registered(&waiters, decision.decision_id);

        let started = Instant::now();
        gate.close(LeaseClass::ActiveControl);
        gate.await_drain(LeaseClass::ActiveControl).unwrap();
        handle.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(waiters.waiting_count(decision.decision_id), 0);
        let retained = store
            .get(decision.owner.workspace_id, decision.decision_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            retained.status,
            usagi_core::domain::user_decision::UserDecisionStatus::Pending
        );
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

        let handle = spawn_decision_maintenance(
            decisions,
            Arc::new(DecisionWaiters::default()),
            shutdown,
            Duration::from_millis(1),
        )
        .unwrap();
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
            Arc::new(DecisionWaiters::default()),
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

    #[test]
    fn a_panicked_background_worker_is_reported_as_daemon_health_danger() {
        use usagi_core::usecase::client::MetricsAction;
        use usagi_core::usecase::daemon_health::{DaemonHealth, DaemonHealthTracker, HealthReason};

        let shutdown = Arc::new(ShutdownRequest::new());
        let handle = spawn_retention_gc_worker(
            || panic!("injected retention worker panic"),
            Arc::clone(&shutdown),
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(handle.join().is_err());

        let broker = Arc::new(Mutex::new(MetricsBroker::with_runtime_health(
            AgentConcurrencyGauge::default(),
            shutdown.background_worker_health(),
        )));
        let sampler = Arc::new(Mutex::new(ProcessResourceSampler { previous: None }));
        let pipeline = TerminalPipelineMetrics::default();
        let mut observer = None;
        let snapshot = metrics_response(
            &broker,
            &sampler,
            &pipeline,
            &mut observer,
            MetricsAction::Snapshot,
        );
        assert_eq!(snapshot.failed_background_workers, 1);

        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&snapshot);
        assert_eq!(
            tracker.evaluate(i64::try_from(snapshot.sampled_at_ms).unwrap()),
            DaemonHealth::Danger(HealthReason::BackgroundWorkerStopped)
        );
    }

    #[test]
    fn every_critical_worker_unexpected_return_requests_shutdown_and_closes_its_source() {
        for worker in [
            BackgroundWorker::AgentObserver,
            BackgroundWorker::TerminalObserver,
            BackgroundWorker::PrProjection,
        ] {
            let shutdown = Arc::new(ShutdownRequest::new());
            let closed = Arc::new(AtomicBool::new(false));
            let observed = Arc::clone(&closed);
            let handle = spawn_critical_worker(
                "injected-critical-worker",
                worker,
                Arc::clone(&shutdown),
                move || observed.store(true, Ordering::Release),
                |_| {},
            )
            .unwrap();

            handle.join().unwrap();
            assert!(
                shutdown.is_requested(),
                "{worker:?} did not stop the daemon"
            );
            assert!(
                closed.load(Ordering::Acquire),
                "{worker:?} source stayed open"
            );
            assert_eq!(shutdown.background_worker_health().failed_count(), 1);
        }
    }

    #[test]
    fn every_critical_worker_panic_is_recorded_before_unwind_and_requests_shutdown() {
        for worker in [
            BackgroundWorker::AgentObserver,
            BackgroundWorker::TerminalObserver,
            BackgroundWorker::PrProjection,
        ] {
            let shutdown = Arc::new(ShutdownRequest::new());
            let closed = Arc::new(AtomicBool::new(false));
            let observed = Arc::clone(&closed);
            let handle = spawn_critical_worker(
                "injected-critical-worker",
                worker,
                Arc::clone(&shutdown),
                move || observed.store(true, Ordering::Release),
                move |_| panic!("injected {worker:?} panic"),
            )
            .unwrap();

            assert!(handle.join().is_err());
            assert!(
                shutdown.is_requested(),
                "{worker:?} did not stop the daemon"
            );
            assert!(
                closed.load(Ordering::Acquire),
                "{worker:?} source stayed open"
            );
            assert_eq!(shutdown.background_worker_health().failed_count(), 1);
        }
    }

    #[test]
    fn planned_critical_worker_shutdown_joins_without_a_health_failure() {
        let shutdown = Arc::new(ShutdownRequest::new());
        let handle = spawn_critical_worker(
            "planned-critical-worker",
            BackgroundWorker::AgentObserver,
            Arc::clone(&shutdown),
            || panic!("planned shutdown must not run failure cleanup"),
            ShutdownRequest::wait_until_requested,
        )
        .unwrap();

        shutdown.request();
        handle.join().unwrap();
        assert_eq!(shutdown.background_worker_health().failed_count(), 0);
    }

    #[test]
    fn lifecycle_owner_closes_sources_and_joins_every_critical_worker() {
        let shutdown = Arc::new(ShutdownRequest::new());
        let health = shutdown.background_worker_health();
        let projection = Arc::new(PrProjectionQueue::new());
        let joined = Arc::new(AtomicUsize::new(0));
        let mut workers = DaemonBackgroundWorkers::new(shutdown, Arc::clone(&projection));

        for worker in [
            BackgroundWorker::AgentObserver,
            BackgroundWorker::TerminalObserver,
            BackgroundWorker::PrProjection,
        ] {
            let completed = Arc::clone(&joined);
            workers.push(
                spawn_critical_worker(
                    "planned-critical-worker",
                    worker,
                    Arc::clone(&workers.shutdown),
                    || panic!("planned shutdown must not run failure cleanup"),
                    move |shutdown| {
                        shutdown.wait_until_requested();
                        completed.fetch_add(1, Ordering::AcqRel);
                    },
                )
                .unwrap(),
            );
        }

        drop(workers);
        assert_eq!(joined.load(Ordering::Acquire), 3);
        assert_eq!(health.failed_count(), 0);
        assert_eq!(projection.recv(), None);
    }

    #[test]
    fn the_draining_collector_retries_observations_and_never_outlives_shutdown() {
        let shutdown = Arc::new(ShutdownRequest::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let handle = spawn_draining_collection_worker(
            move || observed.fetch_add(1, Ordering::AcqRel) >= 1,
            Arc::clone(&shutdown),
            Duration::from_millis(1),
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 2);
        assert!(shutdown.is_requested());

        // Tests do not leave the product worker parked on a fixed sleep: an
        // already-observed shutdown makes it return without another collection
        // observation, and the handle is always joined.
        let skipped = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&skipped);
        let handle = spawn_draining_collection_worker(
            move || {
                counter.fetch_add(1, Ordering::AcqRel);
                false
            },
            shutdown,
            Duration::from_secs(30),
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
    fn production_snapshot_polling_does_not_drop_but_a_slow_observer_does() {
        use usagi_core::usecase::client::MetricsAction;

        let broker = Arc::new(Mutex::new(MetricsBroker::default()));
        let sampler = Arc::new(Mutex::new(ProcessResourceSampler { previous: None }));
        let pipeline = TerminalPipelineMetrics::default();
        let mut snapshot_client = None;
        for _ in 0..4 {
            let snapshot = metrics_response(
                &broker,
                &sampler,
                &pipeline,
                &mut snapshot_client,
                MetricsAction::Snapshot,
            );
            assert_eq!(snapshot.active_subscribers, 0);
            assert_eq!(snapshot.dropped_updates, 0);
        }

        let mut slow = None;
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
        metrics_response(
            &broker,
            &sampler,
            &pipeline,
            &mut snapshot_client,
            MetricsAction::Snapshot,
        );
        let dropped = metrics_response(
            &broker,
            &sampler,
            &pipeline,
            &mut snapshot_client,
            MetricsAction::Snapshot,
        );
        assert_eq!(dropped.dropped_updates, 1);

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

    /// The metrics reply carries the Agent concurrency the daemon's own admission
    /// authority published, and it reads that level **without** the Agent runtime
    /// lock: a display-only tick may never wait behind a launch (#644).
    #[test]
    fn production_metrics_report_agent_concurrency_without_taking_the_agent_lock() {
        use usagi_core::usecase::client::{AgentConcurrency, MetricsAction};

        let gauge = AgentConcurrencyGauge::default();
        let broker = Arc::new(Mutex::new(MetricsBroker::with_agent_concurrency(
            gauge.clone(),
        )));
        let sampler = Arc::new(Mutex::new(ProcessResourceSampler { previous: None }));
        let pipeline = TerminalPipelineMetrics::default();
        let mut client = None;

        // Before any authority publishes, the reply says "unknown" rather than an
        // idle zero, and it declares the schema that carries the projection.
        let unknown = metrics_response(
            &broker,
            &sampler,
            &pipeline,
            &mut client,
            MetricsAction::Subscribe,
        );
        assert_eq!(unknown.agent_concurrency, None);
        assert_eq!(unknown.schema_version, 4);

        // What the authority publishes is what the reply reports, on the very next
        // request and without another sample being pushed.
        gauge.publish(2, AGENT_RUNTIME_LIMIT);
        let reported = metrics_response(
            &broker,
            &sampler,
            &pipeline,
            &mut client,
            MetricsAction::Snapshot,
        );
        assert_eq!(
            reported.agent_concurrency,
            Some(AgentConcurrency {
                in_use: 2,
                limit: u32::try_from(AGENT_RUNTIME_LIMIT).unwrap(),
            })
        );

        // The reply is produced while another thread holds the Agent runtime's
        // authority. `dispatch_metrics` has no access to that runtime by
        // construction; this bounds the regression that would give it one, so a
        // launch could no longer stall the mascot's metrics tick.
        let authority = Arc::new(Mutex::new(()));
        let held = Arc::clone(&authority);
        let (holding, holds) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let guard = held.lock().expect("fresh mutex");
            holding.send(()).expect("the test waits for the held lock");
            released.recv().expect("the test releases the lock");
            drop(guard);
        });
        holds.recv().expect("the authority is held");
        let (answered, answer) = mpsc::channel();
        let replying = std::thread::spawn(move || {
            let mut isolated = None;
            let reply = metrics_response(
                &broker,
                &sampler,
                &TerminalPipelineMetrics::default(),
                &mut isolated,
                MetricsAction::Snapshot,
            );
            let _ = answered.send(reply);
        });
        let reply = answer
            .recv_timeout(Duration::from_secs(10))
            .expect("a metrics reply never waits on the Agent authority");
        assert_eq!(
            reply.agent_concurrency,
            Some(AgentConcurrency {
                in_use: 2,
                limit: u32::try_from(AGENT_RUNTIME_LIMIT).unwrap(),
            })
        );
        replying.join().expect("the reply thread finished");
        release.send(()).expect("the holder is still waiting");
        holder.join().expect("the holder released the authority");
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
    fn only_an_exact_merged_pr_head_authorizes_squash_branch_deletion() {
        use usagi_core::domain::pr_inventory::{PrInventory, PrState, canonicalize};

        let session = SessionId::new();
        let identity = canonicalize("https://github.com/o/r/pull/1").unwrap();
        let head = "a".repeat(40);
        let mut inventory = PrInventory::default();
        inventory.discover([identity.clone()]);
        inventory.entries.get_mut(&identity).unwrap().state = PrState::Merged;
        inventory.entries.get_mut(&identity).unwrap().head_oid = Some(head.clone());
        let snapshot = usagi_core::usecase::client::PrSnapshot::from((session, inventory.clone()));
        assert_eq!(
            exact_merged_pr_head(Some(snapshot), Some(head.clone())),
            Some(head.clone())
        );

        inventory.entries.get_mut(&identity).unwrap().state = PrState::Open;
        assert_eq!(
            exact_merged_pr_head(
                Some(usagi_core::usecase::client::PrSnapshot::from((
                    session,
                    inventory.clone()
                ))),
                Some(head.clone())
            ),
            None
        );
        inventory.entries.get_mut(&identity).unwrap().state = PrState::Merged;
        assert_eq!(
            exact_merged_pr_head(
                Some(usagi_core::usecase::client::PrSnapshot::from((
                    session, inventory
                ))),
                Some("b".repeat(40))
            ),
            None
        );
        assert_eq!(
            exact_merged_pr_head(
                Some(usagi_core::usecase::client::PrSnapshot::from((
                    session,
                    PrInventory::default()
                ))),
                None
            ),
            None
        );
        assert_eq!(exact_merged_pr_head(None, Some(head)), None);
    }

    #[test]
    fn unavailable_pr_inventory_falls_back_to_safe_branch_deletion() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("pr-inventory.json"), "not json").unwrap();
        let inventory = Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
            PrInventoryStore::new(directory.path()),
            GenerationRole::Active,
        ))));

        assert_eq!(
            best_effort_merged_pr_head(&inventory, SessionId::new(), Some("a".repeat(40))),
            None
        );
    }

    #[test]
    fn product_mcp_arguments_start_usagi_mcp_from_the_daemon_binary() {
        let command = Path::new("/opt/usagi/bin/usagi");

        assert_eq!(
            codex_integration_arguments(command, None).unwrap(),
            [
                "-c",
                "mcp_servers.usagi.command = \"/opt/usagi/bin/usagi\"",
                "-c",
                "mcp_servers.usagi.args = [\"mcp\"]",
                "-c",
                "mcp_servers.usagi.env_vars = [\"USAGI_HOME\", \"USAGI_RUNTIME_MODE\", \"USAGI_WORKSPACE_ROOT\"]",
                "-c",
                "mcp_servers.usagi.default_tools_approval_mode = \"approve\"",
                "-c",
                "features.hooks = true",
                "-c",
                "hooks.SessionStart = [{ matcher = \"^startup$\", hooks = [{ type = \"command\", command = \"'/opt/usagi/bin/usagi' codex-session-capture\", timeout = 10 }] }]",
            ]
        );
        assert_eq!(
            claude_mcp_arguments(command, None).unwrap(),
            [
                "--mcp-config",
                r#"{"mcpServers":{"usagi":{"args":["mcp"],"command":"/opt/usagi/bin/usagi"}}}"#,
                "--allowedTools",
                "mcp__usagi",
            ]
        );
    }

    #[test]
    fn product_mcp_arguments_append_local_llm_and_keep_payloads_parseable() {
        let command = Path::new("/opt/usagi/bin/usagi");
        let model = "qwen2.5-coder:7b";

        let codex = codex_integration_arguments(command, Some(model)).unwrap();
        let usagi_position = codex
            .iter()
            .position(|value| value.starts_with("mcp_servers.usagi.command"))
            .unwrap();
        let local_position = codex
            .iter()
            .position(|value| value.starts_with("mcp_servers.usagi-llm.command"))
            .unwrap();
        assert!(usagi_position < local_position);
        for assignment in codex
            .iter()
            .filter(|value| value.starts_with("mcp_servers.") || value.starts_with("features."))
        {
            toml::from_str::<toml::Value>(assignment).unwrap();
        }

        let claude = claude_mcp_arguments(command, Some(model)).unwrap();
        let config: serde_json::Value = serde_json::from_str(&claude[1]).unwrap();
        assert_eq!(
            config["mcpServers"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["usagi", "usagi-llm"]
        );
        assert_eq!(
            config["mcpServers"]["usagi-llm"]["args"],
            serde_json::json!(["llm-mcp", "--model", model])
        );
        assert_eq!(&claude[3..], ["mcp__usagi", "mcp__usagi-llm"]);
    }

    #[test]
    fn local_llm_setting_is_sanitized_before_daemon_provisioning() {
        use usagi_core::domain::settings::{LocalLlm, Settings};

        let base = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        // Production selects the base itself, so its settings file is the base's.
        let data_home = paths::DataHome::new(base.path(), paths::RuntimeMode::Production);
        let storage = Storage::new(data_home.selected());
        let tools = configured_mcp_tools(&data_home, workspace.path()).unwrap();
        assert_eq!(tools.model(), None);
        // Both stores default to enabled, so a workspace with no files gets both
        // families and no delegation server.
        assert_eq!(
            tools.families(),
            McpToolFamilies {
                issue: true,
                memory: true,
                local_llm: false,
            }
        );

        storage
            .save_settings(&Settings {
                local_llm: LocalLlm {
                    enabled: true,
                    model: "x\"], owned = \"pwned'; #".to_owned(),
                },
                ..Settings::default()
            })
            .unwrap();
        let tools = configured_mcp_tools(&data_home, workspace.path()).unwrap();
        assert_eq!(
            tools.model(),
            Some(usagi_core::domain::settings::DEFAULT_LOCAL_LLM_MODEL)
        );
        assert!(tools.families().local_llm);
    }

    #[test]
    fn tool_families_follow_the_registered_workspace_and_fail_closed_when_unreadable() {
        use usagi_core::domain::settings::LocalSettings;

        let base = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let data_home = paths::DataHome::new(base.path(), paths::RuntimeMode::Production);
        let store = WorkspaceSettingsStore::new(workspace.path());

        // The workspace layer decides, exactly as it does for `usagi mcp`.
        store
            .save(&LocalSettings {
                issue_enabled: Some(false),
                ..LocalSettings::default()
            })
            .unwrap();
        assert_eq!(
            configured_mcp_tools(&data_home, workspace.path())
                .unwrap()
                .families(),
            McpToolFamilies {
                issue: false,
                memory: true,
                local_llm: false,
            }
        );

        // A prompt that advertised tools the MCP server cannot register would be
        // worse than no launch, so an unreadable layer fails the provision.
        std::fs::write(store.path(), "{ not json").unwrap();
        assert!(configured_mcp_tools(&data_home, workspace.path()).is_err());
    }

    #[test]
    fn system_prompt_arguments_follow_scope_once_and_stay_parseable() {
        use usagi_core::domain::agent::prompt::{launch_system_prompt, scope_prompt};

        for mode in [SandboxMode::Root, SandboxMode::Session] {
            let expected = scope_prompt(prompt_scope(mode));
            let claude = claude_system_prompt_arguments(mode, None, None);
            assert_eq!(claude, ["--append-system-prompt", expected]);
            assert_eq!(
                claude
                    .iter()
                    .filter(|argument| argument.as_str() == "--append-system-prompt")
                    .count(),
                1
            );

            let codex = codex_system_prompt_arguments(mode, None, None);
            assert_eq!(codex[0], "-c");
            assert_eq!(
                codex
                    .iter()
                    .filter(|argument| argument.starts_with("developer_instructions="))
                    .count(),
                1
            );
            let parsed: toml::Value = toml::from_str(&codex[1]).unwrap();
            assert_eq!(parsed["developer_instructions"].as_str(), Some(expected));
        }

        // A later resolve (including a resume replacement) regenerates from
        // its current scope instead of retaining the previous provision.
        assert_ne!(
            claude_system_prompt_arguments(SandboxMode::Root, None, None),
            claude_system_prompt_arguments(SandboxMode::Session, None, None)
        );
        assert_ne!(
            codex_system_prompt_arguments(SandboxMode::Root, None, None),
            codex_system_prompt_arguments(SandboxMode::Session, None, None)
        );

        // The families the injected server registers reach both products through
        // the one composition, so neither adapter can describe a different set.
        let families = McpToolFamilies {
            issue: false,
            memory: true,
            local_llm: true,
        };
        let expected = launch_system_prompt(PromptScope::Session, Some(families), None);
        assert_eq!(
            claude_system_prompt_arguments(SandboxMode::Session, Some(families), None),
            ["--append-system-prompt", &expected]
        );
        let codex = codex_system_prompt_arguments(SandboxMode::Session, Some(families), None);
        let parsed: toml::Value = toml::from_str(&codex[1]).unwrap();
        assert_eq!(
            parsed["developer_instructions"].as_str(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn role_instruction_is_injected_once_for_claude_and_codex_without_entering_user_prompt() {
        let role = usagi_core::domain::role::RoleId::new("reviewer").unwrap();
        let instructions = "Review correctness and tests.";
        let claude =
            claude_system_prompt_arguments(SandboxMode::Session, None, Some((&role, instructions)));
        assert_eq!(claude[0], "--append-system-prompt");
        assert_eq!(claude[1].matches("<role id=\"reviewer\">").count(), 1);
        assert_eq!(claude[1].matches(instructions).count(), 1);

        let codex =
            codex_system_prompt_arguments(SandboxMode::Session, None, Some((&role, instructions)));
        let parsed: toml::Value = toml::from_str(&codex[1]).unwrap();
        let prompt = parsed["developer_instructions"].as_str().unwrap();
        assert_eq!(prompt.matches("<role id=\"reviewer\">").count(), 1);
        assert_eq!(prompt.matches(instructions).count(), 1);
        assert!(!codex.iter().any(|argument| argument == instructions));
    }

    #[test]
    fn root_role_definition_is_resolved_from_the_current_catalog_at_each_launch() {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let data = temporary.path().join("data");
        std::fs::create_dir_all(workspace.join(".usagi")).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let data_home = paths::DataHome::new(&data, paths::RuntimeMode::Production);
        let sessions = open_session_runtime(
            workspace.clone(),
            &data.join("daemon"),
            &data,
            DaemonGeneration::new(),
        )
        .unwrap();
        let context = provision_context(None);
        let workspaces = one_workspace(
            &data.join("daemon"),
            &workspace,
            sessions,
            context.scope.workspace_id,
        );
        let catalog = |instructions: &str| {
            format!(
                r#"version = 1
[defaults]
root = "director"
[roles.director]
summary = "Direct"
scopes = ["root"]
instructions = "{instructions}"
"#
            )
        };

        std::fs::write(data.join("roles.toml"), catalog("first launch policy")).unwrap();
        let first = effective_role_instruction(&workspaces, &data_home, &workspace, &context)
            .unwrap()
            .unwrap();
        assert_eq!(first.0.as_str(), "director");
        assert_eq!(first.1, "first launch policy");

        std::fs::write(data.join("roles.toml"), catalog("next launch policy")).unwrap();
        let next = effective_role_instruction(&workspaces, &data_home, &workspace, &context)
            .unwrap()
            .unwrap();
        assert_eq!(next.1, "next launch policy");
    }

    #[test]
    fn prompt_renderers_preserve_opaque_argv_and_escape_toml_controls() {
        let prompt = "don't reinterpret \"quotes\", C:\\work\nnext\tline\u{0000}\u{007f}";

        let claude = claude_prompt_arguments(prompt.to_owned());
        assert_eq!(claude, ["--append-system-prompt", prompt]);
        assert_eq!(claude.len(), 2);

        let codex = codex_developer_instructions_arguments(prompt);
        assert_eq!(codex[0], "-c");
        assert!(codex[1].contains(r#"\"quotes\""#));
        assert!(codex[1].contains(r"C:\\work\nnext\tline\u0000\u007F"));
        let parsed: toml::Value = toml::from_str(&codex[1]).unwrap();
        assert_eq!(parsed["developer_instructions"].as_str(), Some(prompt));
    }

    #[test]
    fn integration_and_system_prompt_precede_resume_and_durable_prompt() {
        let mut codex_arguments =
            codex_integration_arguments(Path::new("/opt/usagi/bin/usagi"), None).unwrap();
        codex_arguments.extend(codex_system_prompt_arguments(
            SandboxMode::Session,
            None,
            None,
        ));
        codex_arguments.extend(["resume".to_owned(), "provider-session".to_owned()]);
        let codex = SpawnProvision::new([], codex_arguments);
        let (_, argv) = provisioned_agent_command(
            "codex",
            &["--".to_owned(), "user prompt".to_owned()],
            &codex,
        );
        let developer = argv
            .iter()
            .position(|argument| argument.starts_with("developer_instructions="))
            .unwrap();
        let resume = argv
            .iter()
            .position(|argument| argument == "resume")
            .unwrap();
        let separator = argv.iter().position(|argument| argument == "--").unwrap();
        assert!(developer < resume);
        assert!(resume < separator);
        assert_eq!(
            argv.iter()
                .filter(|argument| argument.starts_with("developer_instructions="))
                .count(),
            1
        );

        let prompt =
            usagi_core::domain::agent::prompt::scope_prompt(PromptScope::Session).to_owned();
        let mut claude = SpawnProvision::new(
            [],
            claude_system_prompt_arguments(SandboxMode::Session, None, None),
        );
        claude.set_sandbox_launcher(SandboxLauncher {
            program: "/opt/usagi/bin/usagi".to_owned(),
            prefix: vec!["claude-sandbox".to_owned(), "--".to_owned()],
        });
        claude.append_sensitive_arguments(["--resume".to_owned(), "provider-session".to_owned()]);
        let (program, argv) = provisioned_agent_command(
            "claude",
            &[
                "--model".to_owned(),
                "sonnet".to_owned(),
                "--".to_owned(),
                "user prompt".to_owned(),
            ],
            &claude,
        );
        assert_eq!(program, "/opt/usagi/bin/usagi");
        assert_eq!(
            argv,
            [
                "claude-sandbox",
                "--",
                "claude",
                "--append-system-prompt",
                prompt.as_str(),
                "--resume",
                "provider-session",
                "--model",
                "sonnet",
                "--",
                "user prompt",
            ]
        );
        assert_eq!(
            argv.iter()
                .filter(|argument| argument.as_str() == "--append-system-prompt")
                .count(),
            1
        );
    }

    #[test]
    fn saved_environment_reaches_terminal_and_agent_with_workspace_precedence() {
        use usagi_core::domain::settings::{LocalSettings, Settings};
        use usagi_core::infrastructure::store::settings::WorkspaceSettingsStore;

        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        Storage::new(data.path().to_path_buf())
            .save_settings(&Settings {
                env: BTreeMap::from([
                    ("GLOBAL_ONLY".to_owned(), "global".to_owned()),
                    (
                        "OP_SERVICE_ACCOUNT_TOKEN".to_owned(),
                        "daemon-only".to_owned(),
                    ),
                    ("SHARED".to_owned(), "global".to_owned()),
                ]),
                ..Settings::default()
            })
            .unwrap();
        WorkspaceSettingsStore::new(workspace.path())
            .save(&LocalSettings {
                env: BTreeMap::from([
                    ("SHARED".to_owned(), "workspace".to_owned()),
                    ("WORKSPACE_ONLY".to_owned(), "workspace".to_owned()),
                ]),
                ..LocalSettings::default()
            })
            .unwrap();

        let configured = Arc::new(UserEnvironment::new(data.path().to_path_buf(), OpCli));
        let request = TerminalLaunchRequest {
            profile_id: TerminalProfileId::new("login-shell").unwrap(),
            scope: TerminalLaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: Some(SessionId::new()),
                worktree_id: WorktreeId::new(),
            },
        };
        let terminal = TrustedLoginShell {
            workspaces: None,
            profile: LoginShellProfile::new(
                BTreeMap::from([("SHELL".to_owned(), "/bin/sh".to_owned())]),
                workspace.path().to_path_buf(),
            ),
            environment: Some(Arc::clone(&configured)),
            workspace_root: workspace.path().to_path_buf(),
        }
        .resolve(&request)
        .unwrap();
        let terminal_environment = terminal
            .environment
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(terminal_environment["GLOBAL_ONLY"], "global");
        assert_eq!(terminal_environment["SHARED"], "workspace");
        assert_eq!(terminal_environment["WORKSPACE_ONLY"], "workspace");
        assert!(!terminal_environment.contains_key("OP_SERVICE_ACCOUNT_TOKEN"));

        let user = configured_environment(Some(&configured), workspace.path()).unwrap();
        let agent = SpawnProvision::new(launch_environment(&user, Vec::new()), Vec::new())
            .compose_environment(&BTreeMap::new());
        assert_eq!(agent["GLOBAL_ONLY"], "global");
        assert_eq!(agent["SHARED"], "workspace");
        assert_eq!(agent["WORKSPACE_ONLY"], "workspace");
        assert!(!agent.contains_key("OP_SERVICE_ACCOUNT_TOKEN"));
    }

    /// A registry holding exactly one workspace, for tests that exercise a
    /// single runtime through the daemon's per-workspace resolution.
    fn one_workspace(
        daemon_dir: &Path,
        root: &Path,
        runtime: SharedSessionRuntime,
        workspace_id: WorkspaceId,
    ) -> Workspaces {
        struct FixedOpener {
            runtime: SharedSessionRuntime,
            workspace_id: WorkspaceId,
        }
        impl TenantRuntimeOpener for FixedOpener {
            type Runtime = SharedSessionRuntime;
            fn open(
                &self,
                _: &Path,
                _: &Path,
            ) -> std::io::Result<OpenedTenant<SharedSessionRuntime>> {
                Ok(OpenedTenant {
                    runtime: Arc::clone(&self.runtime),
                    workspace_id: self.workspace_id,
                })
            }
        }
        let registry = Arc::new(TenantRegistry::new(
            daemon_dir.to_path_buf(),
            FileWorkspaceFences {
                pid: std::process::id(),
            },
            FixedOpener {
                runtime,
                workspace_id,
            },
            DEFAULT_TENANT_LIMIT,
        ));
        registry
            .adopt_initial(root)
            .expect("the fixture workspace is adopted");
        registry
    }

    /// A connection bound to one workspace, for tests that exercise a single
    /// runtime through the per-connection resolution.
    fn bound_to(
        daemon_dir: &Path,
        root: &Path,
        runtime: SharedSessionRuntime,
        workspace_id: WorkspaceId,
    ) -> ConnectionWorkspace {
        let workspaces = one_workspace(daemon_dir, root, runtime, workspace_id);
        let tenant = workspaces
            .workspace_at(root)
            .expect("the fixture workspace is adopted");
        ConnectionWorkspace { tenant, workspaces }
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

    /// Every consumer of the Agent child's data home must agree on the base the
    /// daemon actually runs from, in all three runtime modes and under a custom
    /// `$USAGI_HOME`. Before #608 the base was guessed as the selected
    /// directory's `parent()`, which is correct only for `dev/` and `local/`:
    /// production selects the base itself, so the guess handed the child — and
    /// the sandbox — the directory *above* the data home.
    #[test]
    fn the_agent_child_data_home_follows_the_runtime_mode_in_every_channel() {
        use usagi_core::domain::settings::{LocalLlm, Settings};

        let context = provision_context(Some(SessionId::new()));
        for mode in [
            paths::RuntimeMode::Production,
            paths::RuntimeMode::Development,
            paths::RuntimeMode::Local,
        ] {
            // One custom `$USAGI_HOME` per mode, so a value read from the wrong
            // directory cannot be satisfied by another mode's leftovers.
            let home = tempfile::tempdir_in("/tmp").unwrap();
            let base = home.path();
            let selected = paths::DataHome::new(base, mode).selected();

            // The daemon holds only its selected directory; this is the one
            // place the mode-neutral base is recovered from it.
            let data_home = paths::DataHome::from_selected(&selected, mode);
            assert_eq!(data_home.base(), base);
            assert_eq!(data_home.selected(), selected);

            // 1. Child env: the base plus the mode that re-selects `selected`.
            let environment = mcp_environment(&context, &data_home, Path::new("/repo")).unwrap();
            let value = |name: &str| {
                environment
                    .iter()
                    .find(|(variable, _)| variable.as_str() == name)
                    .map_or_else(|| panic!("{name} is injected"), |(_, value)| value.clone())
            };
            let child_home = PathBuf::from(value(usagi_core::infrastructure::paths::DATA_DIR_ENV));
            assert_eq!(child_home, base);
            let child_mode = value(usagi_core::infrastructure::paths::RUNTIME_MODE_ENV);
            assert_eq!(child_mode, mode.as_env_value());
            // Re-applying the announced mode lands the child on the daemon's
            // own directory — the round trip the E2E pins end to end. The
            // artifact default is passed as the fallback so a dropped wire
            // spelling would resolve somewhere else instead of passing.
            assert_eq!(
                paths::DataHome::new(
                    &child_home,
                    paths::RuntimeMode::from_env_value(
                        Some(&child_mode),
                        paths::DEFAULT_RUNTIME_MODE
                    )
                )
                .selected(),
                selected
            );

            // 2. Settings source: the selected directory the daemon writes.
            std::fs::create_dir_all(&selected).unwrap();
            Storage::new(&selected)
                .save_settings(&Settings {
                    local_llm: LocalLlm {
                        enabled: true,
                        model: usagi_core::domain::settings::DEFAULT_LOCAL_LLM_MODEL.to_owned(),
                    },
                    ..Settings::default()
                })
                .unwrap();
            assert_eq!(
                configured_mcp_tools(&data_home, home.path())
                    .unwrap()
                    .model(),
                Some(usagi_core::domain::settings::DEFAULT_LOCAL_LLM_MODEL)
            );

            // 3. Root sandbox scope: daemon bootstrap is brokered out of process,
            // so neither the selected directory nor its mode-neutral base is writable.
            let roots =
                claude_writable_roots(SandboxMode::Root, Path::new("/repo/.usagi/sessions/work"));
            assert!(roots.is_empty(), "{roots:?}");
        }
    }

    #[test]
    fn root_agent_writable_roots_include_only_provider_state() {
        std::fs::create_dir_all("target").unwrap();
        let fixture = tempfile::tempdir_in("target").unwrap();
        let home = fixture.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        let roots = root_agent_writable_roots(Some(&home), "codex").unwrap();
        assert_eq!(roots, [home.join(".codex").canonicalize().unwrap()]);
        assert!(!roots.contains(&fixture.path().canonicalize().unwrap()));

        let roots = root_agent_writable_roots(None, "/bin/sh").unwrap();
        assert!(roots.is_empty());
    }

    #[test]
    fn bootstrap_broker_accepts_only_ping_start_and_stop() {
        let launches = std::cell::Cell::new(0_u8);
        let count = |request| {
            handle_bootstrap_broker_request(
                request,
                || {
                    launches.set(launches.get() + 1);
                    Ok(())
                },
                || false,
            )
        };

        assert_eq!(count(BROKER_PING), BrokerOutcome::served(true));
        assert_eq!(launches.get(), 0);
        assert_eq!(count(BROKER_START), BrokerOutcome::served(true));
        assert_eq!(launches.get(), 1);
        // An unknown byte is refused without starting anything, and without
        // ending the broker: a stray peer must not be able to retire it.
        assert_eq!(count(b'Z'), BrokerOutcome::served(false));
        assert_eq!(launches.get(), 1);
        assert_eq!(
            handle_bootstrap_broker_request(
                BROKER_START,
                || Err(std::io::Error::other("launch refused")),
                || false,
            ),
            BrokerOutcome::served(false)
        );
        // Stop is the one request that ends the loop, and it starts no daemon.
        assert_eq!(count(BROKER_STOP), BrokerOutcome::RETIRE);
        assert_eq!(launches.get(), 1);

        // A daemon that came back between the decision to retire and this point
        // vetoes it: retiring would leave it with no broker to outlive it, which
        // is the one state the broker exists to prevent.
        assert_eq!(
            handle_bootstrap_broker_request(BROKER_STOP, || Ok(()), || true),
            BrokerOutcome::served(false)
        );
    }

    /// The broker exists so that a sandboxed client can cold-start a daemon it
    /// cannot spawn itself. Retiring next to a live daemon would remove exactly
    /// the helper that daemon's death is going to need.
    #[test]
    fn an_idle_broker_retires_only_once_no_daemon_is_left_to_outlive() {
        let timeout = Duration::from_secs(60);
        assert!(broker_may_retire(Duration::from_secs(60), timeout, false));
        assert!(broker_may_retire(Duration::from_secs(600), timeout, false));
        assert!(!broker_may_retire(Duration::from_secs(59), timeout, false));
        for idle in [Duration::ZERO, Duration::from_hours(24)] {
            assert!(
                !broker_may_retire(idle, timeout, true),
                "a live daemon must keep its broker"
            );
        }
    }

    #[test]
    fn bootstrap_broker_launches_only_serve_in_its_fixed_workspace() {
        let command = bootstrap_serve_command(Path::new("/opt/usagi"), Path::new("/repo"));
        assert_eq!(command.get_program(), "/opt/usagi");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["daemon", "serve"]);
        assert_eq!(command.get_current_dir(), Some(Path::new("/repo")));
    }

    #[test]
    fn bootstrap_broker_address_is_fenced_by_workspace_and_executable() {
        let data = Path::new("/data");
        let first = bootstrap_broker_address(data, Path::new("/repo-a"), Path::new("/bin/usagi"));
        assert_eq!(
            first,
            bootstrap_broker_address(data, Path::new("/repo-a"), Path::new("/bin/usagi"))
        );
        assert_ne!(
            first,
            bootstrap_broker_address(data, Path::new("/repo-b"), Path::new("/bin/usagi"))
        );
        assert_ne!(
            first,
            bootstrap_broker_address(data, Path::new("/repo-a"), Path::new("/opt/usagi"))
        );
        assert_eq!(first.socket.parent(), Some(Path::new("/data/daemon")));
        assert_eq!(first.lock.parent(), Some(Path::new("/data/daemon")));
        assert!(first.socket.to_string_lossy().ends_with(".sock"));
        assert!(first.lock.to_string_lossy().ends_with(".lock"));
    }

    #[test]
    fn bootstrap_broker_requires_a_named_workspace() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let canonical = directory.path().canonicalize().unwrap();
        for workspace in [
            ClientWorkspace::Bound {
                root: paths::wire_workspace_root(&canonical),
            },
            ClientWorkspace::Selected {
                root: paths::wire_workspace_root(&canonical),
            },
        ] {
            assert_eq!(broker_workspace(&workspace).unwrap(), canonical);
        }
        assert_eq!(
            broker_workspace(&ClientWorkspace::Unbound)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            broker_workspace(&ClientWorkspace::Bound {
                root: String::new(),
            })
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn ordinary_daemon_start_does_not_use_a_workspace_fixed_broker() {
        assert!(run_broker_lifecycle_command(CliDaemonCommand::Start).is_none());
    }

    /// One broker serving a throwaway workspace, plus everything a test needs to
    /// address it and to know it finished.
    struct BrokerFixture {
        workspace_dir: tempfile::TempDir,
        _data_parent: tempfile::TempDir,
        address: BootstrapBrokerAddress,
        server: std::thread::JoinHandle<std::io::Result<()>>,
    }

    fn start_broker(idle: BrokerIdlePolicy) -> BrokerFixture {
        let workspace_dir = tempfile::tempdir_in("/tmp").unwrap();
        let workspace = workspace_dir.path().canonicalize().unwrap();
        let data_parent = tempfile::tempdir_in("/tmp").unwrap();
        let data = data_parent.path().join("data");
        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        let address = bootstrap_broker_address(&data, &workspace, &exe);
        let (server_data, server_workspace, server_exe) =
            (data.clone(), workspace.clone(), exe.clone());
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let result = serve_bootstrap_broker(&server_data, &server_workspace, &server_exe, idle);
            let _ = finished_tx.send(result.as_ref().err().map(ToString::to_string));
            result
        });
        for _ in 0..500 {
            if std::os::unix::net::UnixStream::connect(&address.socket).is_ok() {
                return BrokerFixture {
                    workspace_dir,
                    _data_parent: data_parent,
                    address,
                    server,
                };
            }
            if let Ok(error) = finished_rx.try_recv() {
                panic!("broker failed before binding its socket: {error:?}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("broker socket never became ready");
    }

    /// A broker that never retires is a process the operator cannot end: nothing
    /// else stops it, and one accumulates per workspace and per executable path.
    #[test]
    fn a_stop_request_retires_the_broker_and_removes_its_endpoint() {
        let fixture = start_broker(BrokerIdlePolicy {
            timeout: Duration::from_secs(3600),
            poll: Duration::from_secs(3600),
        });

        // Retirement is acknowledged, so a caller learns the endpoint is going
        // rather than having to infer it from a closed connection.
        request_bootstrap_broker(&fixture.address, BROKER_STOP).unwrap();

        fixture.server.join().unwrap().unwrap();
        assert!(!fixture.address.socket.exists());
        assert!(
            std::os::unix::net::UnixStream::connect(&fixture.address.socket).is_err(),
            "a retired broker still answered"
        );
        fixture.workspace_dir.close().unwrap();
    }

    /// With no daemon to outlive and no request to serve, the broker is holding
    /// a process open for a workspace nobody is using.
    #[test]
    fn an_unused_broker_retires_itself_once_nothing_needs_it() {
        let fixture = start_broker(BrokerIdlePolicy {
            timeout: Duration::ZERO,
            poll: Duration::from_millis(20),
        });

        // The idle watch reaches the broker through its own endpoint, so the
        // accept loop stays blocked until then and pays no polling latency.
        let started = Instant::now();
        fixture.server.join().unwrap().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the idle watch did not retire an unused broker"
        );
        assert!(!fixture.address.socket.exists());
        fixture.workspace_dir.close().unwrap();
    }

    #[test]
    fn an_idle_broker_client_cannot_block_the_next_request_forever() {
        let fixture = start_broker(BrokerIdlePolicy {
            timeout: Duration::from_secs(3600),
            poll: Duration::from_secs(3600),
        });
        let idle = std::os::unix::net::UnixStream::connect(&fixture.address.socket).unwrap();
        drop(std::os::unix::net::UnixStream::connect(&fixture.address.socket).unwrap());

        let started = Instant::now();
        request_bootstrap_broker(&fixture.address, BROKER_PING).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an idle peer blocked the broker beyond its IO deadline"
        );
        drop(idle);

        fixture.workspace_dir.close().unwrap();
        let _ = request_bootstrap_broker(&fixture.address, BROKER_PING);
        fixture.server.join().unwrap().unwrap();
        assert!(!fixture.address.socket.exists());
    }

    #[test]
    fn root_git_environment_overrides_untrusted_process_launch_configuration() {
        let mut spawn = SpawnProvision::new([], Vec::new());
        insert_root_git_environment(&mut spawn);
        let environment = spawn.compose_environment(&BTreeMap::from([
            ("GIT_CONFIG_COUNT".to_owned(), "0".to_owned()),
            ("GIT_PAGER".to_owned(), "touch PWNED".to_owned()),
            ("GIT_EXTERNAL_DIFF".to_owned(), "touch".to_owned()),
        ]));
        assert_eq!(environment["GIT_CONFIG_COUNT"], "5");
        assert_eq!(environment["GIT_CONFIG_KEY_0"], "core.fsmonitor");
        assert_eq!(environment["GIT_CONFIG_VALUE_0"], "false");
        assert_eq!(environment["GIT_CONFIG_KEY_1"], "core.hooksPath");
        assert_eq!(environment["GIT_CONFIG_VALUE_1"], "/dev/null");
        assert_eq!(environment["GIT_PAGER"], "");
        assert_eq!(environment["GIT_EXTERNAL_DIFF"], "");
        assert_eq!(environment["GIT_OPTIONAL_LOCKS"], "0");
    }

    #[test]
    fn root_codex_uses_the_outer_boundary_without_nesting_the_native_sandbox() {
        let mut spawn = SpawnProvision::new([], Vec::new());
        spawn.set_sandbox_launcher(SandboxLauncher {
            program: "/opt/usagi/bin/usagi".to_owned(),
            prefix: vec!["claude-sandbox".to_owned(), "--".to_owned()],
        });
        insert_root_git_environment(&mut spawn);

        assert!(spawn.sandbox_launcher().is_some());
        let (program, argv) = provisioned_agent_command(
            "codex",
            &[
                "--sandbox".to_owned(),
                "danger-full-access".to_owned(),
                "--ask-for-approval".to_owned(),
                "never".to_owned(),
            ],
            &spawn,
        );
        assert_eq!(program, "/opt/usagi/bin/usagi");
        assert_eq!(
            argv,
            [
                "claude-sandbox",
                "--",
                "codex",
                "--sandbox",
                "danger-full-access",
                "--ask-for-approval",
                "never"
            ]
        );

        let environment = spawn.compose_environment(&BTreeMap::new());
        assert_eq!(environment["GIT_CONFIG_NOSYSTEM"], "1");
        assert_eq!(environment["GIT_CONFIG_GLOBAL"], "/dev/null");
        assert_eq!(environment["GIT_OPTIONAL_LOCKS"], "0");
    }

    #[cfg(unix)]
    #[test]
    fn codex_arg0_preflight_repairs_only_owned_provider_temp_directory_modes() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let state = tempfile::tempdir().unwrap();
        let arg0 = state.path().join("tmp/arg0");
        let stale_dir = arg0.join("codex-arg0-stale");
        let unrelated = arg0.join("other-temp");
        let target = arg0.join("target");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::create_dir(&target).unwrap();
        symlink(&target, arg0.join("codex-arg0-alias")).unwrap();
        std::fs::set_permissions(&stale_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o000)).unwrap();

        assert_eq!(repair_codex_arg0_permissions(state.path()).unwrap(), 1);
        assert_eq!(
            std::fs::symlink_metadata(&stale_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&unrelated)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o000
        );

        // Let TempDir clean up the intentionally untouched fixture.
        std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn codex_arg0_preflight_has_a_hard_scan_bound() {
        let state = tempfile::tempdir().unwrap();
        let arg0 = state.path().join("tmp/arg0");
        std::fs::create_dir_all(arg0.join("codex-arg0-first")).unwrap();
        std::fs::create_dir(arg0.join("codex-arg0-second")).unwrap();

        assert_eq!(
            repair_codex_arg0_permissions_with_limit(state.path(), 1)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn root_git_common_dir_must_not_overlap_sandbox_writable_state() {
        std::fs::create_dir_all("target").unwrap();
        let safe = tempfile::tempdir_in("target").unwrap();
        std::fs::create_dir(safe.path().join(".git")).unwrap();
        assert_eq!(
            git_common_dir(safe.path()).unwrap(),
            safe.path().join(".git").canonicalize().unwrap()
        );
        assert!(
            validate_root_git_common_dir_policy(
                safe.path(),
                CLAUDE_PROGRAM,
                Some(Path::new("/tmp")),
                None,
                None
            )
            .is_ok()
        );

        let linked = tempfile::tempdir_in("target").unwrap();
        let common = tempfile::tempdir_in("/tmp").unwrap();
        let git_dir = common.path().join("worktrees/linked");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            linked.path().join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        assert_eq!(
            git_common_dir(linked.path()).unwrap(),
            common.path().canonicalize().unwrap()
        );
        assert!(
            validate_root_git_common_dir_policy(
                linked.path(),
                CLAUDE_PROGRAM,
                Some(Path::new("/tmp")),
                None,
                None
            )
            .is_err()
        );

        // The `$HOME` state root covered by this check is the launched agent's own
        // (`~/.codex` for Codex), so a Git common directory under it is refused for
        // that provider while an unknown program contributes no home-derived area.
        let home = tempfile::tempdir_in("target").unwrap();
        let state = home.path().join(".codex");
        std::fs::create_dir_all(state.join("worktrees/linked")).unwrap();
        let under_state = tempfile::tempdir_in("target").unwrap();
        std::fs::write(
            under_state.path().join(".git"),
            format!("gitdir: {}\n", state.join("worktrees/linked").display()),
        )
        .unwrap();
        std::fs::write(state.join("worktrees/linked/commondir"), "../..\n").unwrap();
        for (program, allowed) in [("codex", false), ("claude", true), ("/bin/sh", true)] {
            assert_eq!(
                validate_root_git_common_dir_policy(
                    under_state.path(),
                    program,
                    None,
                    Some(&home.path().canonicalize().unwrap()),
                    None,
                )
                .is_ok(),
                allowed,
                "{program} must {} a Git common directory under ~/.codex",
                if allowed { "accept" } else { "refuse" }
            );
        }
    }

    #[test]
    fn a_session_claude_is_confined_to_its_worktree_and_gets_the_guard_hook() {
        let usagi = Path::new("/opt/usagi/bin/usagi");
        let context = provision_context(Some(SessionId::new()));
        let mode = sandbox_mode(&context);
        assert_eq!(mode, SandboxMode::Session);

        let roots = claude_writable_roots(mode, Path::new("/repo/.usagi/sessions/work"));
        assert_eq!(roots, [PathBuf::from("/repo/.usagi/sessions/work")]);

        let launcher = claude_sandbox_launcher(
            usagi,
            mode,
            Path::new("/repo"),
            &SandboxLauncherPaths::default(),
            &roots,
        )
        .unwrap();
        assert_eq!(launcher.program, "/opt/usagi/bin/usagi");
        assert_eq!(
            launcher.prefix,
            [
                "claude-sandbox",
                "--mode",
                "session",
                "--protected-root",
                "/repo",
                "--writable-root",
                "/repo/.usagi/sessions/work",
                "--",
            ]
        );

        // A session launch carries the same universal policy paths a root
        // coordinator does. Withholding them does not confine the agent to its
        // worktree — it leaves Claude Code unable to create its fixed
        // `/tmp/claude-<uid>` scratchpad on every tool call, and restarts it
        // against an empty `~/.claude` (first-run flow, no settings, no
        // permission mode) on every launch.
        let universal = claude_sandbox_launcher(
            usagi,
            mode,
            Path::new("/repo"),
            &SandboxLauncherPaths {
                backend: Some(Path::new("/usr/bin/sandbox-exec")),
                tmpdir: Some(Path::new("/tmp/user")),
                home: Some(Path::new("/home/dev")),
                cache_dir: Some(Path::new("/cache")),
            },
            &roots,
        )
        .unwrap();
        assert_eq!(
            universal.prefix,
            [
                "claude-sandbox",
                "--mode",
                "session",
                "--protected-root",
                "/repo",
                "--backend",
                "/usr/bin/sandbox-exec",
                "--tmpdir",
                "/tmp/user",
                "--cache-dir",
                "/cache",
                "--home",
                "/home/dev",
                "--writable-root",
                "/repo/.usagi/sessions/work",
                "--",
            ]
        );

        let arguments = claude_settings_arguments(usagi).unwrap();
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
    fn root_policy_accepts_the_per_user_cache_root() {
        // macOS の Keychain 検索は per-user の MDS cache を更新する。root sandbox がここへ
        // 書けないと agent CLI は Keychain の credential を読めず、古い file 側 credential へ
        // fallback して 401 で起動できない。launcher へは cache root を渡し、writable にする
        // subpath（`<cache>/mds`）は core の純粋な決定部が決める。
        let usagi = Path::new("/opt/usagi/bin/usagi");
        let launcher = claude_sandbox_launcher(
            usagi,
            SandboxMode::Root,
            Path::new("/repo"),
            &SandboxLauncherPaths {
                cache_dir: Some(Path::new("/private/var/folders/ab/cd/C")),
                ..SandboxLauncherPaths::default()
            },
            &[],
        )
        .unwrap();
        assert!(
            launcher
                .prefix
                .windows(2)
                .any(|pair| pair[0] == "--cache-dir" && pair[1] == "/private/var/folders/ab/cd/C"),
            "{:?}",
            launcher.prefix
        );

        // daemon 側の gate も cache root を writable root と同じ規則で検証する。
        std::fs::create_dir_all("target").unwrap();
        let workspace = tempfile::tempdir_in("target").unwrap();
        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        let cache = tempfile::tempdir_in("target").unwrap();
        let backend_dir = tempfile::tempdir().unwrap();
        let backend = backend_dir.path().join("backend");
        std::fs::write(&backend, "fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let backend = backend.canonicalize().unwrap();
        let workspace_root = workspace.path().canonicalize().unwrap();
        let cache_root = cache.path().canonicalize().unwrap();
        let validate = |cache_dir: Option<&Path>| {
            validate_claude_sandbox_policy(&SandboxPolicyInputs {
                mode: SandboxMode::Root,
                program: CLAUDE_PROGRAM,
                workspace_root: &workspace_root,
                launch_roots: &[],
                tmpdir: None,
                home: None,
                cache_dir,
                backend: Some(&backend),
                passthrough: false,
            })
        };
        assert_eq!(validate(Some(&cache_root)), Ok(()));
        // 判定の対象は grant する `<cache>/mds` である。workspace がその中にある構成だけを
        // 拒否し、workspace の単なる兄弟（`<cache>/…`）は grant と重ならないので通す。
        let overlapping = tempfile::tempdir_in("target").unwrap();
        let overlapping_root = overlapping.path().canonicalize().unwrap();
        let nested_workspace = claude_sandbox::macos_mds_cache_root(&overlapping_root).join("repo");
        std::fs::create_dir_all(nested_workspace.join(".git")).unwrap();
        assert_eq!(
            validate_claude_sandbox_policy(&SandboxPolicyInputs {
                mode: SandboxMode::Root,
                program: CLAUDE_PROGRAM,
                workspace_root: &nested_workspace,
                launch_roots: &[],
                tmpdir: None,
                home: None,
                cache_dir: Some(&overlapping_root),
                backend: Some(&backend),
                passthrough: false,
            }),
            Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor)
        );
        // cache root 自体は実在しなければならない（grant の親を所有者ごと確かめる）。
        assert_eq!(
            validate(Some(&cache_root.join("missing"))),
            Err(ClaudeSandboxPolicyError::InvalidWritableRoot)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_cache_root_resolves_to_an_owned_canonical_directory() {
        // bootstrap が実際に確定できることを実 platform で確かめる。ここが None のままだと
        // root sandbox は per-user MDS cache を許可できず、Keychain 検索が壊れる。
        let cache = resolve_sandbox_cache_dir().unwrap();
        assert!(cache.is_absolute() && cache.is_dir());
        assert_eq!(validate_owned_directory(&cache), Ok(()));
    }

    #[test]
    fn session_sandbox_policy_rejects_root_workspace_ancestors_and_symlink_aliases() {
        let workspace = tempfile::tempdir().unwrap();
        let owned = tempfile::tempdir().unwrap();
        let backend_dir = tempfile::tempdir().unwrap();
        let backend = backend_dir.path().join("backend");
        std::fs::write(&backend, "fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let backend = backend.canonicalize().unwrap();
        let workspace_root = workspace.path().canonicalize().unwrap();
        let owned_root = owned.path().canonicalize().unwrap();
        let validate = |roots: &[PathBuf], tmpdir: Option<&Path>| {
            validate_claude_sandbox_policy(&SandboxPolicyInputs {
                mode: SandboxMode::Session,
                program: CLAUDE_PROGRAM,
                workspace_root: &workspace_root,
                launch_roots: roots,
                tmpdir,
                home: None,
                cache_dir: None,
                backend: Some(&backend),
                passthrough: false,
            })
        };

        assert_eq!(
            validate(&[PathBuf::from("/")], None),
            Err(ClaudeSandboxPolicyError::InvalidWritableRoot)
        );
        assert_eq!(
            validate(std::slice::from_ref(&workspace_root), None),
            Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor)
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let alias = owned_root.with_extension("alias");
            symlink(&owned_root, &alias).unwrap();
            assert_eq!(
                validate(&[], Some(&alias)),
                Err(ClaudeSandboxPolicyError::InvalidWritableRoot)
            );
            std::fs::remove_file(alias).unwrap();
        }
    }

    #[test]
    fn root_sandbox_policy_checks_the_state_root_of_the_agent_it_launches() {
        // The launcher grants the state directory of the CLI it execs, so the daemon
        // checks that same directory against the protected workspace. A workspace
        // living inside `~/.codex` is refused for Codex, accepted for a provider whose
        // state is elsewhere, and unaffected by a program usagi does not launch.
        let backend_dir = tempfile::tempdir_in("/tmp").unwrap();
        let backend = backend_dir.path().join("backend");
        std::fs::write(&backend, "fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let backend = backend.canonicalize().unwrap();
        // The fixture home stays outside `/tmp`, which a root launch may write in its
        // own right: a Git common directory under it is refused for every provider.
        std::fs::create_dir_all("target").unwrap();
        let home = tempfile::tempdir_in("target").unwrap();
        let home = home.path().canonicalize().unwrap();
        let workspace_root = home.join(".codex/repo");
        std::fs::create_dir_all(workspace_root.join(".git")).unwrap();

        for (program, expected) in [
            (
                "codex",
                Err(ClaudeSandboxPolicyError::ProtectedWorkspaceAncestor),
            ),
            ("codex-fugu", Ok(())),
            ("claude", Ok(())),
            ("/bin/sh", Ok(())),
        ] {
            assert_eq!(
                validate_claude_sandbox_policy(&SandboxPolicyInputs {
                    mode: SandboxMode::Root,
                    program,
                    workspace_root: &workspace_root,
                    launch_roots: &[],
                    tmpdir: None,
                    home: Some(&home),
                    cache_dir: None,
                    backend: Some(&backend),
                    passthrough: false,
                }),
                expected,
                "{program} state root against a workspace inside ~/.codex"
            );
        }
    }

    #[test]
    fn a_root_claude_keeps_the_repository_read_only_and_gets_the_guard_hook() {
        let usagi = Path::new("/opt/usagi/bin/usagi");
        let mode = sandbox_mode(&provision_context(None));
        assert_eq!(mode, SandboxMode::Root);

        // A root launch's cwd and daemon data stay read-only; bootstrap uses the broker.
        let roots = claude_writable_roots(mode, Path::new("/repo"));
        assert!(roots.is_empty());
        let launcher = claude_sandbox_launcher(
            usagi,
            mode,
            Path::new("/repo"),
            &SandboxLauncherPaths::default(),
            &roots,
        )
        .unwrap();
        assert_eq!(&launcher.prefix[..3], ["claude-sandbox", "--mode", "root"]);
        assert_eq!(launcher.prefix.last().unwrap(), "--");

        let arguments = claude_settings_arguments(usagi).unwrap();
        assert!(arguments[1].contains("guard-workspace"));
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
            workspaces: None,
            profile: LoginShellProfile::new(BTreeMap::new(), directory.path().to_path_buf()),
            environment: None,
            workspace_root: PathBuf::new(),
        }
        .resolve(&request)
        .unwrap();
        let metrics = Arc::new(TerminalPipelineMetrics::default());
        let (mut pty, observations) = DaemonPty::new(metrics, Arc::new(SpawnedChildren::default()));

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
                PtyObservation::Exited(exited, status, _) => {
                    assert_eq!(exited, terminal);
                    assert_eq!(status, 0);
                    break;
                }
                PtyObservation::Shutdown => panic!("unexpected observer shutdown"),
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
                .send(PtyObservation::Exited(blocked_terminal, 0, None))
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
            PtyObservation::Exited(actual, 0, None) if actual == terminal
        ));
        producer.join().unwrap();
    }

    /// A [`ChildProcessProbe`] the test writes the OS's answers into.
    ///
    /// Pid reuse is the case this fix turns on, and it cannot be raced for
    /// against a real kernel: here the test simply says that the same number now
    /// answers as a different process. A pid with no answer is a process the
    /// platform cannot see.
    #[derive(Default)]
    struct ScriptedProbe(Mutex<BTreeMap<u32, (String, u32)>>);

    impl ScriptedProbe {
        fn answers(&self, pid: u32, start_identity: &str, process_group: u32) {
            self.0
                .lock()
                .unwrap()
                .insert(pid, (start_identity.to_owned(), process_group));
        }

        /// One lookup behind both reads, so a pid is either a whole process or
        /// no process at all — the platform never half-answers here.
        fn answer(&self, pid: u32) -> std::io::Result<(String, u32)> {
            self.0
                .lock()
                .unwrap()
                .get(&pid)
                .cloned()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        }
    }

    impl ChildProcessProbe for ScriptedProbe {
        fn start_identity(&self, pid: u32) -> std::io::Result<String> {
            self.answer(pid).map(|(start_identity, _)| start_identity)
        }

        fn process_group(&self, pid: u32) -> std::io::Result<u32> {
            self.answer(pid).map(|(_, process_group)| process_group)
        }
    }

    #[test]
    fn an_observed_child_stays_provable_until_its_release_is_dropped() {
        let children = Arc::new(SpawnedChildren::default());
        let probe = ScriptedProbe::default();
        probe.answers(4242, "start-a", 4242);

        let (identity, release) = children.observe(&probe, 4242, "daemon-owned-pty");
        assert_eq!(identity.start_identity, "start-a");
        let authority = ObservedChildren(Arc::clone(&children));
        // The durable store may only call a record `Running` while the child is
        // provable, so the proof has to outlive everything up to the exit commit.
        assert!(authority.verified(&identity).is_some());
        assert_eq!(children.0.lock().unwrap().len(), 1);

        drop(release);
        assert!(authority.verified(&identity).is_none());
        assert!(children.0.lock().unwrap().is_empty());
    }

    #[test]
    fn releasing_an_exited_child_leaves_the_pid_its_successor_took() {
        const REUSED: u32 = 4243;
        let children = Arc::new(SpawnedChildren::default());
        let probe = ScriptedProbe::default();
        probe.answers(REUSED, "first-start", REUSED);
        let (first, first_release) = children.observe(&probe, REUSED, "daemon-owned-pty");

        // The kernel reaped the first child and handed the number to the next one.
        probe.answers(REUSED, "second-start", REUSED);
        let (second, second_release) = children.observe(&probe, REUSED, "daemon-owned-pty");

        let authority = ObservedChildren(Arc::clone(&children));
        drop(first_release);
        assert!(authority.verified(&first).is_none());
        assert!(
            authority.verified(&second).is_some(),
            "the live child lost the proof its namesake released"
        );
        assert_eq!(children.0.lock().unwrap().len(), 1);

        drop(second_release);
        assert!(authority.verified(&second).is_none());
        assert!(children.0.lock().unwrap().is_empty());
    }

    #[test]
    fn a_long_run_of_short_lived_children_returns_the_registry_to_its_baseline() {
        const CHILDREN: u32 = 1024;
        let children = Arc::new(SpawnedChildren::default());
        let probe = ScriptedProbe::default();

        for pid in 1..=CHILDREN {
            probe.answers(pid, &format!("start-{pid}"), pid);
            let (_, release) = children.observe(&probe, pid, "daemon-owned-pty");
            assert!(release.is_some());
            drop(release);
            let observed = children.0.lock().unwrap().len();
            assert_eq!(observed, 0, "child {pid} left {observed} proof(s) behind");
        }
    }

    #[test]
    fn a_child_the_platform_cannot_read_records_no_proof_and_needs_no_release() {
        let children = Arc::new(SpawnedChildren::default());

        let (identity, release) =
            children.observe(&ScriptedProbe::default(), 7, "daemon-owned-pty");

        // The unverifiable token stays visible so the record fails closed, but
        // nothing was recorded, so there is nothing to release either.
        assert_eq!(identity.start_identity, "daemon-owned-pty");
        assert_eq!(identity.process_group, 7);
        assert!(release.is_none());
        assert!(children.0.lock().unwrap().is_empty());
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
        let children = Arc::new(SpawnedChildren::default());
        let (mut generic, generic_observations) =
            DaemonPty::new(Arc::clone(&metrics), Arc::clone(&children));
        let (mut agent, agent_observations) =
            AgentPty::new(BTreeMap::new(), metrics, Arc::clone(&children));
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
        // Both owners spawned real children through the real probe, so the
        // identity registry proves the leak is closed end to end and not only in
        // the unit tests' fake: every observation released exactly its own entry.
        assert!(children.0.lock().unwrap().is_empty());

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
                PtyObservation::Exited(terminal, 0, release) => {
                    assert!(output.contains(&terminal.terminal_id.as_str()));
                    assert!(pty.release(&terminal));
                    assert!(!pty.release(&terminal));
                    // The observer's contract: the identity proof dies with the
                    // observation that reported the exit.
                    drop(release);
                    exits += 1;
                }
                PtyObservation::Exited(_, status, _) => {
                    panic!("unexpected exit status {status}")
                }
                PtyObservation::Shutdown => panic!("unexpected observer shutdown"),
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
                AgentPtyObservation::Exited(terminal, 0, release) => {
                    assert!(output.contains(&terminal.terminal_id.as_str()));
                    assert!(pty.release(&terminal));
                    assert!(!pty.release(&terminal));
                    drop(release);
                    exits += 1;
                }
                AgentPtyObservation::Exited(_, status, _) => {
                    panic!("unexpected exit status {status}");
                }
                AgentPtyObservation::Shutdown => panic!("unexpected observer shutdown"),
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
        let projector = Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
            PrInventoryStore::new(directory.path()),
            GenerationRole::Active,
        ))));
        let projection = Arc::new(PrProjectionQueue::new());
        let shutdown = Arc::new(ShutdownRequest::new());
        let worker = start_pr_projection_worker(
            Arc::clone(&projector),
            Arc::clone(&projection),
            Arc::clone(&shutdown),
        )
        .unwrap();
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
        shutdown.request();
        drop(ClosePrProjectionOnExit {
            projection: Arc::clone(&projection),
        });
        worker.join().unwrap();
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
        let (pty, observations) = DaemonPty::new(metrics, Arc::new(SpawnedChildren::default()));
        let observer_stop = pty.observations.clone();
        let runtime = Arc::new(Mutex::new(GenericTerminalRuntime::new(
            DaemonGeneration::new(),
            TrustedLoginShell {
                workspaces: None,
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
        let shutdown = Arc::new(ShutdownRequest::new());
        let observer = start_terminal_observer(
            Arc::downgrade(&runtime),
            observations,
            Arc::clone(&projection),
            Arc::clone(&shutdown),
        )
        .unwrap();
        let projector = start_pr_projection_worker(
            Arc::new(Mutex::new(OutputPrProjector::new(FencedPrInventory::new(
                PrInventoryStore::new(directory.path()),
                GenerationRole::Active,
            )))),
            Arc::clone(&projection),
            Arc::clone(&shutdown),
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
            request_terminal_json(
                &mut *runtime.lock().unwrap(),
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
        let subscription = request_terminal_json(
            &mut *runtime.lock().unwrap(),
            connection,
            client,
            RequestId::new(),
            TerminalAction::Attach,
            serde_json::to_value(TerminalRequest::Attach {
                terminal: terminal.clone(),
                geometry: None,
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
                request_terminal_json(
                    &mut *runtime.lock().unwrap(),
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
        let exit_subscription = request_terminal_json(
            &mut *runtime.lock().unwrap(),
            exit_connection,
            exit_client,
            RequestId::new(),
            TerminalAction::Attach,
            serde_json::to_value(TerminalRequest::Attach {
                terminal: terminal.clone(),
                geometry: None,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap()["subscription"]
            .as_u64()
            .unwrap();
        request_terminal_json(
            &mut *runtime.lock().unwrap(),
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
            let response = request_terminal_json(
                &mut *runtime.lock().unwrap(),
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
        shutdown.request();
        observer_stop.send(PtyObservation::Shutdown).unwrap();
        projection.close();
        observer.join().unwrap();
        projector.join().unwrap();
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
            temporary.path(),
            usagi_core::domain::id::DaemonGeneration::new(),
        )
        .unwrap();
        drop(first);
        let restored = open_session_runtime(
            restart_directory,
            &daemon_state,
            temporary.path(),
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
    fn root_composition_resolves_an_available_session_by_stable_id() {
        struct SuccessfulGit;
        impl usagi_core::infrastructure::git::GitRunner for SuccessfulGit {
            fn run(
                &self,
                _: &Path,
                _: &[&str],
            ) -> anyhow::Result<usagi_core::infrastructure::git::GitOutput> {
                Ok(usagi_core::infrastructure::git::GitOutput {
                    success: true,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
        }

        struct NoopSessionWorktreeIo;
        impl usagi_daemon::usecase::session_runtime::SessionWorktreeIo for NoopSessionWorktreeIo {
            fn remove_file_best_effort(&self, _: &Path) {}
            fn path_occupied(&self, _: &Path) -> bool {
                false
            }
            fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
                Some(path.to_path_buf())
            }
            fn is_repo_root(&self, _: &Path) -> bool {
                false
            }
            fn is_linked_worktree(&self, _: &Path) -> bool {
                true
            }
            fn build_session_tree(
                &self,
                _: &dyn usagi_core::infrastructure::git::GitRunner,
                _: &Path,
                _: &Path,
                _: &str,
            ) -> anyhow::Result<()> {
                Ok(())
            }
            fn remove_session_tree(
                &self,
                _: &dyn usagi_core::infrastructure::git::GitRunner,
                _: &Path,
                _: bool,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let temporary = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(
            SessionRuntime::open(
                temporary.path().join("repository"),
                &temporary.path().join("daemon"),
                DaemonGeneration::new(),
                SuccessfulGit,
                NoopSessionWorktreeIo,
            )
            .unwrap(),
        ));
        perform_create(
            &runtime,
            &SuccessfulGit,
            &usagi_core::domain::id::OperationId::new().to_string(),
            &serde_json::json!({"name": "one"}),
        )
        .unwrap();

        let runtime = runtime.lock().unwrap();
        let session_id = runtime.session_id("one").unwrap();
        assert!(runtime.session_scope_by_id(session_id).is_ok());
    }

    /// A delegation builds its worktree before it can dispatch into it, so a
    /// daemon that died in that window left a session no caller owns. The next
    /// start rolls exactly those back — and leaves the ones whose dispatch did
    /// reach the store, because that operation's outcome is the dispatch side's
    /// to decide (#611).
    #[test]
    fn startup_compensates_only_delegated_creates_with_nothing_dispatched() {
        let temporary = tempfile::tempdir().unwrap();
        let sessions = Arc::new(Mutex::new(
            SessionRuntime::open(
                temporary.path().join("repository"),
                &temporary.path().join("daemon"),
                DaemonGeneration::new(),
                AlwaysSuccessfulGit,
                PermissiveSessionWorktreeIo,
            )
            .unwrap(),
        ));
        let dispatch = DispatchStore::new(temporary.path().join("dispatch"));
        let teardown = TeardownSignal::new();
        let delegate = |name: &str| {
            let operation = usagi_core::domain::id::OperationId::new();
            perform_delegated_create(
                &sessions,
                &AlwaysSuccessfulGit,
                &operation.to_string(),
                &serde_json::json!({"name": name}),
            )
            .unwrap();
            operation
        };

        let orphan = delegate("orphan");
        let dispatched = delegate("dispatched");
        // A plain `session_create` is complete on its own and is never a
        // compensation candidate.
        perform_create(
            &sessions,
            &AlwaysSuccessfulGit,
            &usagi_core::domain::id::OperationId::new().to_string(),
            &serde_json::json!({"name": "plain"}),
        )
        .unwrap();
        // The dispatched delegation reached the dispatch store, which now owns
        // that operation's outcome.
        let agent = usagi_core::domain::agent::Agent {
            agent_id: usagi_core::domain::id::AgentId::new(),
            session_id: None,
            runtime: usagi_core::domain::agent::AgentProfileId::new("claude").unwrap(),
            model: usagi_core::domain::agent::ModelSelector::new("test").unwrap(),
            status: usagi_core::domain::agent::AgentStatus::Idle,
            current_run: None,
        };
        dispatch
            .upsert_run(usagi_core::domain::agent::DispatchRun {
                run_id: dispatched,
                agent_id: agent.agent_id,
                prompt: "finish".into(),
                started_at: chrono::Utc::now(),
                ended_at: None,
                status: usagi_core::domain::agent::RunStatus::Running,
            })
            .unwrap();

        let bound = bound_to(
            &temporary.path().join("tenants"),
            &temporary.path().join("repository"),
            Arc::clone(&sessions),
            WorkspaceId::new(),
        );
        assert_eq!(
            reconcile_orphan_delegations(&bound, &dispatch, &teardown),
            1
        );

        let names = |lifecycle: &str| {
            sessions.lock().unwrap().snapshot().unwrap()["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|session| session["lifecycle"] == lifecycle)
                .map(|session| session["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("deleting"), ["orphan"]);
        assert_eq!(names("available"), ["dispatched", "plain"]);
        // The teardown worker was woken for the admitted compensation, and the
        // durable plan takes the branch with the worktree.
        assert!(teardown.wait(std::time::Duration::from_millis(1)));
        let pending = sessions.lock().unwrap().pending_teardowns().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].force && pending[0].delete_branch);

        // A second start finds nothing new: the orphan is already `Deleting`, so
        // it is the teardown worker's, not another compensation's.
        assert_eq!(
            reconcile_orphan_delegations(&bound, &dispatch, &teardown),
            0
        );
        assert_ne!(orphan, dispatched);
    }

    /// Whether a failed dispatch rolls its session back is decided by the failure,
    /// not by the caller: an unknown spawn outcome must keep the worktree, because
    /// a worker may be running in it (#611).
    #[test]
    fn an_unknown_spawn_outcome_keeps_the_delegated_session_and_a_definite_one_rolls_it_back() {
        use usagi_core::infrastructure::ipc::{ErrorCode, ProtocolError};
        use usagi_daemon::usecase::session_runtime::DelegationReconcile;

        let temporary = tempfile::tempdir().unwrap();
        let sessions = Arc::new(Mutex::new(
            SessionRuntime::open(
                temporary.path().join("repository"),
                &temporary.path().join("daemon"),
                DaemonGeneration::new(),
                AlwaysSuccessfulGit,
                PermissiveSessionWorktreeIo,
            )
            .unwrap(),
        ));
        // Bound before the test poisons the runtime lock: the compensation path
        // under test is the one that meets an unreadable workspace, not a
        // fixture that cannot be built.
        let bound = bound_to(
            &temporary.path().join("tenants"),
            &temporary.path().join("repository"),
            Arc::clone(&sessions),
            WorkspaceId::new(),
        );
        let teardown = TeardownSignal::new();
        let run = usagi_core::domain::id::OperationId::new().to_string();
        let delegate = |name: &str| {
            perform_delegated_create(
                &sessions,
                &AlwaysSuccessfulGit,
                &usagi_core::domain::id::OperationId::new().to_string(),
                &serde_json::json!({"name": name}),
            )
            .unwrap();
            sessions.lock().unwrap().session_id(name).unwrap()
        };
        let compensate = |name: &str, id, code| {
            compensate_delegation(
                &sessions,
                &teardown,
                id,
                name,
                &run,
                ProtocolError::new(code, "refused"),
            )
        };

        // Unknown: the session stays available and the caller is told to
        // reconcile it.
        let retained_id = delegate("retained");
        let retained = compensate("retained", retained_id, ErrorCode::OwnershipUnknown);
        let SessionRuntimeError::Delegation(retained) = retained else {
            panic!("a failed delegation reports a delegation failure");
        };
        assert_eq!(retained.reconcile, DelegationReconcile::Retained);
        assert_eq!(retained.session_id, retained_id);
        assert_eq!(retained.run_operation_id, run);
        assert!(sessions.lock().unwrap().session_id("retained").is_ok());

        // Definite: the session is rolled back by a durable teardown.
        let rolled_back_id = delegate("rolled-back");
        let compensated = compensate("rolled-back", rolled_back_id, ErrorCode::Unavailable);
        let SessionRuntimeError::Delegation(compensated) = compensated else {
            panic!("a failed delegation reports a delegation failure");
        };
        assert_eq!(compensated.reconcile, DelegationReconcile::Compensated);
        assert_eq!(
            sessions.lock().unwrap().pending_teardowns().unwrap()[0].name,
            "rolled-back"
        );
        // A session the compensation cannot find is also nothing left behind: an
        // earlier attempt's teardown already removed it.
        let already_gone = compensate("never-created", rolled_back_id, ErrorCode::Unavailable);
        let SessionRuntimeError::Delegation(already_gone) = already_gone else {
            panic!("a failed delegation reports a delegation failure");
        };
        assert_eq!(already_gone.reconcile, DelegationReconcile::Compensated);

        // A rollback that cannot be admitted at all leaves the session present,
        // and says so rather than claiming a clean rejection.
        let poisoned = Arc::clone(&sessions);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison the session lock");
        })
        .join();
        let failed = compensate("rolled-back", rolled_back_id, ErrorCode::Unavailable);
        let SessionRuntimeError::Delegation(failed) = failed else {
            panic!("a failed delegation reports a delegation failure");
        };
        assert_eq!(failed.reconcile, DelegationReconcile::CompensationFailed);
        // The same poisoned lock makes the startup reconcile report nothing
        // rather than guessing at an empty candidate set.
        assert_eq!(
            reconcile_orphan_delegations(
                &bound,
                &DispatchStore::new(temporary.path().join("dispatch")),
                &teardown
            ),
            0
        );
    }

    /// A delegation that fails answers with structured state, not a sentence: the
    /// caller has to tell a clean rejection from a session that is still there
    /// because its worker's fate is unknown (#611).
    #[test]
    fn a_failed_delegation_reports_its_reconcile_state_on_the_wire() {
        use usagi_core::infrastructure::ipc::{ErrorCode, SideEffect};
        use usagi_daemon::usecase::session_runtime::{DelegationFailure, DelegationReconcile};

        let session_id = SessionId::new();
        let run = usagi_core::domain::id::OperationId::new().to_string();
        let envelope_for = |reconcile: DelegationReconcile, code: ErrorCode| {
            session_response_envelope(
                usagi_core::usecase::client::SessionAction::DelegateBrief,
                &serde_json::json!({}),
                Err(SessionRuntimeError::Delegation(DelegationFailure {
                    code,
                    message: "dispatch runtime executable is unavailable".into(),
                    session_id,
                    run_operation_id: run.clone(),
                    reconcile,
                })),
                usagi_core::infrastructure::ipc::RequestId("delegate".into()),
                &session_test_hello(),
            )
        };

        let compensated =
            envelope_for(DelegationReconcile::Compensated, ErrorCode::InvalidArgument);
        let error = response_error(&compensated);
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        // A rolled-back delegation left nothing behind.
        assert_eq!(error.side_effect, SideEffect::None);
        let details = error.details.unwrap();
        assert_eq!(details["reconcile"], "compensated");
        assert_eq!(details["run_operation_id"], run);
        assert_eq!(
            details["session_id"],
            serde_json::to_value(session_id).unwrap()
        );

        for (reconcile, token) in [
            (DelegationReconcile::Retained, "retained"),
            (
                DelegationReconcile::CompensationFailed,
                "compensation_failed",
            ),
        ] {
            let error = response_error(&envelope_for(reconcile, ErrorCode::OwnershipUnknown));
            assert_eq!(error.code, ErrorCode::OwnershipUnknown);
            // Something durable is still there, so the caller must reconcile it
            // rather than assume a clean rejection.
            assert_eq!(error.side_effect, SideEffect::PartialOrUnknown);
            assert_eq!(error.details.unwrap()["reconcile"], token);
        }
    }

    /// The protocol error one response envelope carries, or a failure naming what
    /// it carried instead.
    fn response_error(
        envelope: &usagi_core::infrastructure::ipc::Envelope,
    ) -> usagi_core::infrastructure::ipc::ProtocolError {
        match &envelope.kind {
            usagi_core::infrastructure::ipc::EnvelopeKind::Response {
                outcome: usagi_core::infrastructure::ipc::ResponseOutcome::Error(error),
                ..
            } => error.clone(),
            other => panic!("expected an error response, got {other:?}"),
        }
    }

    /// A Git runner that reports success for everything, for composition tests
    /// whose subject is the durable lifecycle rather than Git.
    struct AlwaysSuccessfulGit;
    impl usagi_core::infrastructure::git::GitRunner for AlwaysSuccessfulGit {
        fn run(
            &self,
            _: &Path,
            _: &[&str],
        ) -> anyhow::Result<usagi_core::infrastructure::git::GitOutput> {
            Ok(usagi_core::infrastructure::git::GitOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    /// Worktree IO that accepts every managed path and performs no effect.
    struct PermissiveSessionWorktreeIo;
    impl usagi_daemon::usecase::session_runtime::SessionWorktreeIo for PermissiveSessionWorktreeIo {
        fn remove_file_best_effort(&self, _: &Path) {}
        fn path_occupied(&self, _: &Path) -> bool {
            false
        }
        fn canonical_path(&self, path: &Path) -> Option<PathBuf> {
            Some(path.to_path_buf())
        }
        fn is_repo_root(&self, _: &Path) -> bool {
            false
        }
        fn is_linked_worktree(&self, _: &Path) -> bool {
            true
        }
        fn build_session_tree(
            &self,
            _: &dyn usagi_core::infrastructure::git::GitRunner,
            _: &Path,
            _: &Path,
            _: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove_session_tree(
            &self,
            _: &dyn usagi_core::infrastructure::git::GitRunner,
            _: &Path,
            _: bool,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// One process's durable runtime state over a real data directory.
    fn sharded_state(
        data_dir: &Path,
        generation: DaemonGeneration,
    ) -> usagi_daemon::usecase::resources::durable::ShardedRuntimeState {
        open_runtime_state(data_dir, generation, &Arc::new(SpawnedChildren::default())).unwrap()
    }

    /// The shard document one generation wrote, as raw bytes.
    fn shard_bytes(data_dir: &Path, generation: DaemonGeneration) -> Vec<u8> {
        std::fs::read(shard_path(data_dir, generation)).unwrap()
    }

    fn shard_path(data_dir: &Path, generation: DaemonGeneration) -> PathBuf {
        data_dir
            .join("daemon")
            .join("shards")
            .join(format!("{}.json", generation.as_str()))
    }

    /// One durable generic terminal record, reserved and owned by `generation`.
    fn reserved_terminal_record(
        generation: DaemonGeneration,
    ) -> usagi_daemon::usecase::generic_terminal::DurableTerminalRecord {
        let workspace = WorkspaceId::new();
        let session = SessionId::new();
        let worktree = WorktreeId::new();
        let scope = TerminalLaunchScope {
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: worktree,
        };
        let terminal = TerminalRef {
            daemon_generation: generation,
            terminal_id: TerminalId::new(),
            workspace_id: workspace,
            session_id: Some(session),
            worktree_id: worktree,
        };
        usagi_daemon::usecase::generic_terminal::DurableTerminalRecord {
            terminal,
            operation: usagi_core::domain::id::CompletionFence {
                workspace_id: workspace,
                session_id: Some(session),
                operation_id: usagi_core::domain::id::OperationId::new(),
                owner_daemon_generation: generation,
                execution_attempt: 1,
                lifecycle_attempt: 1,
                expected_revision: 1,
            },
            launch: usagi_core::domain::terminal_launch::DurableTerminalLaunchSnapshot::new(
                TerminalLaunchRequest {
                    profile_id: TerminalProfileId::new("login-shell").unwrap(),
                    scope,
                },
                1,
                "sh",
                Vec::new(),
                PathBuf::from("/tmp"),
                [],
            )
            .unwrap(),
            state: usagi_daemon::usecase::terminal::TerminalRuntimeState::Reserved,
            process: None,
            launch_digest: Some("digest".to_owned()),
        }
    }

    fn terminal_truth(generation: DaemonGeneration) -> TerminalStoreSnapshot {
        TerminalStoreSnapshot {
            records: vec![reserved_terminal_record(generation)],
            ..TerminalStoreSnapshot::default()
        }
    }

    #[test]
    fn the_terminal_store_writes_this_generations_own_shard() {
        let dir = tempfile::tempdir().unwrap();
        let generation = DaemonGeneration::new();
        let mut store = ShardedTerminalStore::new(sharded_state(dir.path(), generation));

        store.save(terminal_truth(generation)).unwrap();

        // The shard is named after its only writer and carries the record itself.
        let document: serde_json::Value =
            serde_json::from_slice(&shard_bytes(dir.path(), generation)).unwrap();
        assert_eq!(document["owner"], generation.as_str());
        assert_eq!(
            document["schema"],
            usagi_daemon::usecase::resources::shard::SHARD_SCHEMA
        );
        let resources = document["resources"].as_array().unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0]["kind"], "terminal");
        assert_eq!(resources[0]["state"], "reserved");
        assert!(resources[0]["payload"].is_string());
        // The capacity claim is durable before the reservation the spawn follows.
        assert!(dir.path().join("daemon").join("allocations.json").exists());
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Two daemon instances and every fenced effect form one restart contract.
    fn generic_terminal_restart_hydrates_inventory_and_preserves_records() {
        let dir = tempfile::tempdir().unwrap();
        let first_generation = DaemonGeneration::new();
        let second_generation = DaemonGeneration::new();
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
            first_generation,
            TrustedLoginShell {
                workspaces: None,
                profile: LoginShellProfile::new(BTreeMap::new(), dir.path().to_path_buf()),
                environment: None,
                workspace_root: PathBuf::new(),
            },
            ShardedTerminalStore::new(sharded_state(dir.path(), first_generation)),
            RestartPty(Arc::clone(&first_effects)),
            TestTerminalScope {
                scope: scope.clone(),
                working_directory: dir.path().to_path_buf(),
            },
        );
        let old_terminal: TerminalRef = serde_json::from_value(
            request_terminal_json(
                &mut first,
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

        // The restarted process owns a new generation, so the old record reaches it
        // through the retained shard its dead owner wrote, never by rewriting it.
        let before_restart = sharded_state(dir.path(), second_generation)
            .hydrate()
            .unwrap();
        assert_eq!(before_restart.interrupted, 1);
        let old_record = before_restart.terminals.records[0].clone();
        let reconciled = before_restart.terminals;
        let second_effects = Arc::new(Mutex::new(RestartEffects::default()));
        let second_store = ShardedTerminalStore::new(sharded_state(dir.path(), second_generation));
        let mut second = GenericTerminalRuntime::from_snapshot(
            second_generation,
            TrustedLoginShell {
                workspaces: None,
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
                    geometry: None,
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
            let error = request_terminal_json(
                &mut second,
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
            request_terminal_json(
                &mut second,
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

        let after_launch = sharded_state(dir.path(), second_generation)
            .hydrate()
            .unwrap()
            .terminals;
        assert_eq!(after_launch.records.len(), 2);
        // The old owner's shard still holds its record exactly as it left it.
        assert!(!shard_bytes(dir.path(), first_generation).is_empty());
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
    fn a_corrupt_or_unknown_shard_fails_closed_without_effect_or_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let generation = DaemonGeneration::new();
        // Write the shard the way a first start would.
        ShardedTerminalStore::new(sharded_state(dir.path(), generation))
            .save(terminal_truth(generation))
            .unwrap();
        let path = shard_path(dir.path(), generation);
        for bytes in [
            b"{broken".as_slice(),
            br#"{"schema":"usagi-owner-shard-v999"}"#.as_slice(),
        ] {
            std::fs::write(&path, bytes).unwrap();
            let preserved = std::fs::read(&path).unwrap();
            assert!(sharded_state(dir.path(), generation).hydrate().is_err());
            // Startup fails closed and leaves the last bytes for inspection.
            assert_eq!(std::fs::read(&path).unwrap(), preserved);
        }
    }

    #[test]
    fn both_stores_share_one_shard_without_clobbering_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let generation = DaemonGeneration::new();
        let mut agents = ShardedAgentStore::new(sharded_state(dir.path(), generation));
        let mut terminals = ShardedTerminalStore::new(sharded_state(dir.path(), generation));

        // Two stores, one document, and every write a compare-and-swap: the Agent
        // save must not erase the terminal reservation or the other way round.
        terminals.save(terminal_truth(generation)).unwrap();
        agents.save(RuntimeStoreSnapshot::default()).unwrap();

        let document: serde_json::Value =
            serde_json::from_slice(&shard_bytes(dir.path(), generation)).unwrap();
        assert_eq!(document["owner"], generation.as_str());
        assert_eq!(document["resources"].as_array().unwrap().len(), 1);
        assert_eq!(document["resources"][0]["kind"], "terminal");
    }

    #[test]
    fn a_legacy_store_this_build_cannot_read_is_never_sealed() {
        for bytes in [
            b"{not-json".as_slice(),
            br#"{"schema_version":999,"records":[]}"#.as_slice(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            // The archive creates the private directories a daemon start needs.
            let state = sharded_state(dir.path(), DaemonGeneration::new());
            let daemon = dir.path().join("daemon");
            let legacy = daemon.join("agents.json");
            std::fs::write(&legacy, bytes).unwrap();
            let before = std::fs::read(&legacy).unwrap();

            assert!(state.hydrate().is_err());
            // The legacy bytes stay exactly where they are: nothing is migrated,
            // renamed, or marked, so a fix or a rollback is still possible.
            assert_eq!(std::fs::read(&legacy).unwrap(), before);
            assert!(!daemon.join("runtime-migration.json").exists());
        }
    }

    #[test]
    fn a_legacy_store_is_migrated_once_and_retired_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_generation = DaemonGeneration::new();
        let state = sharded_state(dir.path(), DaemonGeneration::new());
        let daemon = dir.path().join("daemon");
        std::fs::write(
            daemon.join("terminals.json"),
            serde_json::to_vec(&terminal_truth(legacy_generation)).unwrap(),
        )
        .unwrap();

        let hydrated = state.hydrate().unwrap();
        let marker = hydrated.migration.unwrap().marker;
        assert_eq!(
            marker.schema,
            usagi_daemon::usecase::resources::durable::MIGRATION_SCHEMA
        );
        assert_eq!(marker.generations, vec![legacy_generation.as_str()]);
        // A legacy reservation cannot prove a child, so it is adopted as a
        // non-spawnable safe failure rather than as live runtime.
        assert_eq!(marker.unknown, 1);
        assert_eq!(hydrated.terminals.records.len(), 1);
        // The store is retired by rename, so its bytes stay inspectable while no
        // build reads them again — the migration is one way.
        assert!(!daemon.join("terminals.json").exists());
        assert!(daemon.join("terminals.json.migrated").exists());
        assert!(daemon.join("runtime-migration.json").exists());
        assert!(state.hydrate().unwrap().migration.is_none());
    }

    #[test]
    fn a_record_that_leaves_the_owners_truth_is_fenced_and_still_counted() {
        let dir = tempfile::tempdir().unwrap();
        let generation = DaemonGeneration::new();
        let mut store = ShardedTerminalStore::new(sharded_state(dir.path(), generation));
        let truth = terminal_truth(generation);
        store.save(truth.clone()).unwrap();

        // A reserved record still owns a PTY reservation a cold transition would
        // destroy, so the lifecycle census counts it.
        let census = DurableResourceCensus {
            data_dir: dir.path().to_path_buf(),
        };
        assert_eq!(census.live().unwrap().terminals, 1);

        // Dropping it from the owner's truth cannot silently forget a live record:
        // it becomes unprovable and keeps its capacity instead.
        store.save(TerminalStoreSnapshot::default()).unwrap();
        let document: serde_json::Value =
            serde_json::from_slice(&shard_bytes(dir.path(), generation)).unwrap();
        assert_eq!(document["resources"][0]["state"], "ownership_unknown");
        assert_eq!(census.live().unwrap().terminals, 0);
    }

    #[test]
    fn a_collection_pass_removes_the_shard_of_a_generation_nothing_retains() {
        let dir = tempfile::tempdir().unwrap();
        let old = DaemonGeneration::new();
        let record = reserved_terminal_record(old);
        let mut exited = record.clone();
        exited.state = usagi_daemon::usecase::terminal::TerminalRuntimeState::Exited;
        let mut store = ShardedTerminalStore::new(sharded_state(dir.path(), old));
        store
            .save(TerminalStoreSnapshot {
                records: vec![exited],
                ..TerminalStoreSnapshot::default()
            })
            .unwrap();
        assert!(shard_path(dir.path(), old).exists());

        let active = sharded_state(dir.path(), DaemonGeneration::new());
        let limits = shipping_retention_limits();
        let retained: BTreeSet<String> =
            std::iter::once(record.terminal.terminal_id.as_str()).collect();

        // While the active generation still answers for the record, its history
        // stays; once it does not, the whole document goes.
        assert_eq!(active.collect(&retained, &limits).unwrap().1, 0);
        assert!(shard_path(dir.path(), old).exists());
        assert_eq!(active.collect(&BTreeSet::new(), &limits).unwrap().1, 1);
        assert!(!shard_path(dir.path(), old).exists());
    }

    #[test]
    fn a_failed_shard_write_is_a_refused_save_that_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let generation = DaemonGeneration::new();
        let mut store = ShardedTerminalStore::new(sharded_state(dir.path(), generation));
        store.save(terminal_truth(generation)).unwrap();
        let shards = dir.path().join("daemon").join("shards");
        let document = shard_path(dir.path(), generation);
        let preserved = std::fs::read(&document).unwrap();
        // An unwritable shard directory fails the swap after the document was read.
        let mut mode = std::fs::metadata(&shards).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o500);
        std::fs::set_permissions(&shards, mode).unwrap();

        let refused = store.save(terminal_truth(generation)).is_err();

        let mut mode = std::fs::metadata(&shards).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o700);
        std::fs::set_permissions(&shards, mode).unwrap();
        assert!(refused || std::fs::read(&document).unwrap() == preserved);
        assert_eq!(std::fs::read(&document).unwrap(), preserved);
        let leftovers: Vec<_> = std::fs::read_dir(&shards)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    // ---------------------------------------------------------------- fence
    //
    // The generation fence the shipping accept loop now serves every connection
    // through (#559). These tests drive the real presentation connection loop
    // with the real `GenerationFence`, so what they fix is the *wiring*: the
    // pure decisions are covered in `usagi_daemon::usecase::authority`.

    /// A terminal owner that records what actually reached it. The fence's whole
    /// job is to decide what does, so "the owner never saw it" is the only
    /// statement of effect zero worth making.
    #[derive(Default)]
    struct FenceWitness {
        seen: Vec<TerminalAction>,
    }

    impl usagi_daemon::usecase::terminal_owner::TerminalOwner for FenceWitness {
        fn handle(
            &mut self,
            _context: usagi_daemon::usecase::terminal_owner::TerminalRequestContext,
            request: TerminalRequest,
        ) -> Result<
            usagi_daemon::usecase::terminal_owner::TerminalResponse,
            usagi_core::infrastructure::ipc::ProtocolError,
        > {
            let action = match request {
                TerminalRequest::Launch { .. } => TerminalAction::Launch,
                TerminalRequest::Inventory { .. } => TerminalAction::Inventory,
                TerminalRequest::Attach { .. } => TerminalAction::Attach,
                TerminalRequest::Resume { .. } => TerminalAction::Resume,
                TerminalRequest::Resync { .. } => TerminalAction::Resync,
                TerminalRequest::Input { .. } => TerminalAction::Input,
                TerminalRequest::InputOutcome { .. } => TerminalAction::InputOutcome,
                TerminalRequest::Resize { .. } => TerminalAction::Resize,
                TerminalRequest::Detach { .. } => TerminalAction::Detach,
                TerminalRequest::CompletedInventory { .. } => TerminalAction::CompletedInventory,
                TerminalRequest::Observe { .. } => TerminalAction::Observe,
                TerminalRequest::Dismiss { .. } => TerminalAction::Dismiss,
            };
            self.seen.push(action);
            Ok(usagi_daemon::usecase::terminal_owner::TerminalResponse::Detached)
        }

        fn disconnect(&mut self, _connection: ConnectionId) {}
    }

    /// The client hello a routing-capable peer sends.
    fn fence_client_hello(
        capabilities: Vec<String>,
    ) -> usagi_core::infrastructure::ipc::ClientHello {
        use usagi_core::infrastructure::ipc::{
            ClientHello, ProtocolRange, TERMINAL_CHECKPOINT_REVISION, TERMINAL_WIRE_GENERATION,
        };
        ClientHello {
            client_id: usagi_core::infrastructure::ipc::ClientId(ClientId::new().as_str().clone()),
            connection_nonce: "fence".to_owned(),
            expected_daemon_generation: None,
            supported_protocols: vec![ProtocolRange {
                generation: TERMINAL_WIRE_GENERATION,
                min_revision: 0,
                max_revision: TERMINAL_CHECKPOINT_REVISION,
            }],
            capabilities,
            required_capabilities: Vec::new(),
            build: current_build(),
            workspace: Some(ClientWorkspace::Unbound),
        }
    }

    /// Serve `requests` to one connection through `fence` and report each
    /// response's outcome alongside what reached the terminal owner.
    ///
    /// The bytes are a real hello frame plus real request envelopes, so the fence
    /// is exercised exactly where production puts it: inside
    /// `handle_connection_with_terminal_and`, ahead of both the terminal path and
    /// the dispatch closure.
    fn serve_through_fence(
        fence: &GenerationFence,
        hello: &usagi_core::infrastructure::ipc::ClientHello,
        requests: &[serde_json::Value],
    ) -> (
        Vec<usagi_core::infrastructure::ipc::ResponseOutcome>,
        Vec<TerminalAction>,
    ) {
        use usagi_core::infrastructure::ipc::{
            Bootstrap, DEFAULT_MAX_FRAME_BYTES, Envelope, EnvelopeKind, RequestId as WireRequestId,
            read_json_frame, write_json_frame,
        };
        let generation = ipc_generation();
        let protocol = usagi_daemon::presentation::ipc::server_protocol(
            generation.clone(),
            generation.0.clone(),
            current_build(),
            DaemonRecord::new(std::process::id()),
            String::new(),
        );
        // The version and generation every envelope must target are the ones the
        // handshake will settle on, so they are read from the same negotiation the
        // connection loop performs rather than assumed. An envelope that named a
        // different pair would be answered by the generation-mismatch branch,
        // which never reaches the fence.
        let negotiated = usagi_core::infrastructure::ipc::negotiate(hello, &protocol)
            .expect("the fence fixture's client must be admissible");
        let mut inbound = Vec::new();
        write_json_frame(
            &mut inbound,
            &Bootstrap::ClientHello(hello.clone()),
            DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap();
        for body in requests {
            write_json_frame(
                &mut inbound,
                &Envelope {
                    protocol: negotiated.protocol,
                    daemon_generation: negotiated.daemon_generation.clone(),
                    kind: EnvelopeKind::Request {
                        request_id: WireRequestId(RequestId::new().as_str().clone()),
                        timeout_ms: None,
                        body: body.clone(),
                    },
                },
                DEFAULT_MAX_FRAME_BYTES,
            )
            .unwrap();
        }

        let mut reader = std::io::Cursor::new(inbound);
        let mut outbound = Vec::new();
        let mut owner = FenceWitness::default();
        usagi_daemon::presentation::ipc::handle_connection_with_terminal_and(
            &mut reader,
            &mut outbound,
            &protocol,
            fence,
            &mut owner,
            &mut |request_id, _body, hello, _connection, _client| Envelope {
                protocol: hello.protocol,
                daemon_generation: hello.daemon_generation.clone(),
                kind: EnvelopeKind::Response {
                    request_id,
                    outcome: usagi_core::infrastructure::ipc::ResponseOutcome::Ok,
                    body: serde_json::json!({"dispatched": true}),
                },
            },
        )
        .unwrap();

        let mut replies = std::io::Cursor::new(outbound);
        // The server hello is the first frame out; the responses follow it.
        assert!(matches!(
            read_json_frame::<Bootstrap>(&mut replies, DEFAULT_MAX_FRAME_BYTES).unwrap(),
            Some(Bootstrap::ServerHello(_))
        ));
        let mut outcomes = Vec::new();
        while let Some(envelope) =
            read_json_frame::<Envelope>(&mut replies, DEFAULT_MAX_FRAME_BYTES).unwrap()
        {
            let EnvelopeKind::Response { outcome, .. } = envelope.kind else {
                panic!("daemon replied with something other than a response");
            };
            outcomes.push(outcome);
        }
        (outcomes, owner.seen)
    }

    fn serve_supervisor_request(
        runtime: &SharedSupervisorRuntime,
        generation: &usagi_core::infrastructure::ipc::DaemonGeneration,
        hello: &usagi_core::infrastructure::ipc::ClientHello,
        authenticated: Option<&usagi_core::domain::agent::CallerRef>,
        body: serde_json::Value,
    ) -> (
        usagi_core::infrastructure::ipc::ResponseOutcome,
        serde_json::Value,
    ) {
        use usagi_core::infrastructure::ipc::{
            Bootstrap, DEFAULT_MAX_FRAME_BYTES, Envelope, EnvelopeKind, ErrorCode, ProtocolError,
            RequestId as WireRequestId, read_json_frame, write_json_frame,
        };
        let protocol = usagi_daemon::presentation::ipc::server_protocol(
            generation.clone(),
            generation.0.clone(),
            current_build(),
            DaemonRecord::new(std::process::id()),
            String::new(),
        );
        let negotiated = usagi_core::infrastructure::ipc::negotiate(hello, &protocol).unwrap();
        let mut inbound = Vec::new();
        write_json_frame(
            &mut inbound,
            &Bootstrap::ClientHello(hello.clone()),
            DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap();
        write_json_frame(
            &mut inbound,
            &Envelope {
                protocol: negotiated.protocol,
                daemon_generation: negotiated.daemon_generation,
                kind: EnvelopeKind::Request {
                    request_id: WireRequestId(RequestId::new().as_str().clone()),
                    timeout_ms: None,
                    body,
                },
            },
            DEFAULT_MAX_FRAME_BYTES,
        )
        .unwrap();

        let mut reader = std::io::Cursor::new(inbound);
        let mut outbound = Vec::new();
        let mut owner = FenceWitness::default();
        usagi_daemon::presentation::ipc::handle_connection_with_terminal_and(
            &mut reader,
            &mut outbound,
            &protocol,
            &usagi_daemon::presentation::ipc::UnfencedConnection,
            &mut owner,
            &mut |request_id, body, server, _connection, client| {
                let caller = authenticated.map_or_else(
                    || {
                        Err(ProtocolError::new(
                            ErrorCode::OwnershipUnknown,
                            "supervisor caller provenance is unknown",
                        ))
                    },
                    |caller| Ok(supervisor_caller_descriptor(&client, caller)),
                );
                dispatch_supervisor_tool(runtime, caller, request_id, &body, server)
            },
        )
        .unwrap();
        let mut replies = std::io::Cursor::new(outbound);
        assert!(matches!(
            read_json_frame::<Bootstrap>(&mut replies, DEFAULT_MAX_FRAME_BYTES).unwrap(),
            Some(Bootstrap::ServerHello(_))
        ));
        let reply = read_json_frame::<Envelope>(&mut replies, DEFAULT_MAX_FRAME_BYTES)
            .unwrap()
            .unwrap();
        let EnvelopeKind::Response { outcome, body, .. } = reply.kind else {
            panic!("supervisor dispatcher returned a non-response envelope");
        };
        (outcome, body)
    }

    fn supervisor_request(
        action: SupervisorToolAction,
        operation_id: &str,
        payload: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::to_value(DaemonRequest::SupervisorTool {
            action,
            operation_id: operation_id.to_owned(),
            payload,
            caller_context: None,
        })
        .unwrap()
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One matrix keeps every authority transition on one durable run.
    fn supervisor_authority_survives_reconnect_and_rollover_but_not_forgery_or_restart() {
        use chrono::Utc;
        use usagi_core::domain::{
            agent::CallerRef,
            id::{AgentId, OperationId},
            supervisor::{
                EscalationDecision, SupervisorEvent, SupervisorEventKind, SupervisorEventSource,
            },
        };
        use usagi_core::infrastructure::{
            ipc::{ErrorCode, ResponseOutcome},
            store::supervisor::SupervisorStore,
        };

        let temp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Mutex::new(SupervisorRuntime::new(temp.path())));
        let caller = CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: AgentId::new(),
        };
        let hello = fence_client_hello(Vec::new());
        let first_generation = ipc_generation();
        let start = supervisor_request(
            SupervisorToolAction::Start,
            "lost-response-operation",
            serde_json::json!({"root_task":"root"}),
        );

        // The first response is deliberately discarded. A new production
        // connection with the same handshake incarnation converges on its run.
        let _ = serve_supervisor_request(
            &runtime,
            &first_generation,
            &hello,
            Some(&caller),
            start.clone(),
        );
        let (retry_outcome, retry_body) =
            serve_supervisor_request(&runtime, &first_generation, &hello, Some(&caller), start);
        assert_eq!(retry_outcome, ResponseOutcome::Ok);
        let run_id = retry_body["supervisor_run_id"].as_str().unwrap();

        // A generation rollover keeps the daemon-issued credential registry and
        // the process client incarnation, so every control surface remains owned.
        let rollover = ipc_generation();
        for (action, payload) in [
            (
                SupervisorToolAction::Get,
                serde_json::json!({"supervisor_run_id":run_id}),
            ),
            (SupervisorToolAction::List, serde_json::json!({})),
            (
                SupervisorToolAction::Events,
                serde_json::json!({"supervisor_run_id":run_id}),
            ),
        ] {
            let (outcome, _) = serve_supervisor_request(
                &runtime,
                &rollover,
                &hello,
                Some(&caller),
                supervisor_request(action, "observe", payload),
            );
            assert_eq!(outcome, ResponseOutcome::Ok);
        }

        // Put the aggregate in a real durable escalation so the authorized
        // resolve path is exercised through the dispatcher too.
        let store = SupervisorStore::new(temp.path());
        let id = serde_json::from_value(retry_body["supervisor_run_id"].clone()).unwrap();
        let run = store.load(id).unwrap().unwrap();
        store
            .apply(
                id,
                run.state_revision,
                &SupervisorEvent {
                    sequence: run.state_revision + 1,
                    event_id: OperationId::new(),
                    causation_id: None,
                    correlation_id: None,
                    observed_at: Utc::now(),
                    payload_digest: "test-escalation".into(),
                    source: SupervisorEventSource::Admission,
                    kind: SupervisorEventKind::Escalate {
                        task_id: None,
                        reason: "operator decision required".into(),
                        safe_evidence: "fixture".into(),
                        choices: vec!["resume".into()],
                    },
                },
            )
            .unwrap();
        let escalated = store.load(id).unwrap().unwrap();
        let actual_escalation = escalated.escalation.unwrap().escalation_id;
        let (resolved, _) = serve_supervisor_request(
            &runtime,
            &rollover,
            &hello,
            Some(&caller),
            supervisor_request(
                SupervisorToolAction::ResolveEscalation,
                "resolve",
                serde_json::json!({
                    "supervisor_run_id":run_id,
                    "escalation_id":actual_escalation,
                    "decision":EscalationDecision::Resume,
                }),
            ),
        );
        assert_eq!(resolved, ResponseOutcome::Ok);

        // A different incarnation or missing/expired capability cannot observe
        // or mutate the run. In particular a daemon restart loses the in-memory
        // credential registry even though the durable aggregate is reloaded.
        let foreign_hello = fence_client_hello(Vec::new());
        let foreign_scope = CallerRef {
            session_id: Some(SessionId::new()),
            agent_id: AgentId::new(),
        };
        for (candidate_hello, authenticated) in [
            (&foreign_hello, Some(&caller)),
            (&hello, Some(&foreign_scope)),
        ] {
            let before = store.load(id).unwrap().unwrap();
            for (action, operation, payload) in [
                (
                    SupervisorToolAction::Start,
                    "lost-response-operation",
                    serde_json::json!({"root_task":"root"}),
                ),
                (
                    SupervisorToolAction::Get,
                    "foreign-get",
                    serde_json::json!({"supervisor_run_id":run_id}),
                ),
                (
                    SupervisorToolAction::Events,
                    "foreign-events",
                    serde_json::json!({"supervisor_run_id":run_id}),
                ),
                (
                    SupervisorToolAction::Cancel,
                    "foreign-cancel",
                    serde_json::json!({"supervisor_run_id":run_id,"reason":"foreign"}),
                ),
                (
                    SupervisorToolAction::ResolveEscalation,
                    "foreign-resolve",
                    serde_json::json!({
                        "supervisor_run_id":run_id,
                        "escalation_id":actual_escalation,
                        "decision":EscalationDecision::Cancel,
                    }),
                ),
            ] {
                let (outcome, _) = serve_supervisor_request(
                    &runtime,
                    &rollover,
                    candidate_hello,
                    authenticated,
                    supervisor_request(action, operation, payload),
                );
                assert!(matches!(outcome, ResponseOutcome::Error(_)));
                assert_eq!(store.load(id).unwrap().unwrap(), before);
            }
            let (listed, body) = serve_supervisor_request(
                &runtime,
                &rollover,
                candidate_hello,
                authenticated,
                supervisor_request(
                    SupervisorToolAction::List,
                    "foreign-list",
                    serde_json::json!({}),
                ),
            );
            assert_eq!(listed, ResponseOutcome::Ok);
            assert_eq!(body["runs"].as_array().unwrap().len(), 0);
            assert_eq!(store.load(id).unwrap().unwrap(), before);
        }
        let before_unauthenticated = store.load(id).unwrap().unwrap();
        let (unauthenticated, _) = serve_supervisor_request(
            &runtime,
            &rollover,
            &hello,
            None,
            supervisor_request(
                SupervisorToolAction::Cancel,
                "missing-capability",
                serde_json::json!({"supervisor_run_id":run_id,"reason":"foreign"}),
            ),
        );
        assert!(
            matches!(unauthenticated, ResponseOutcome::Error(error) if error.code == ErrorCode::OwnershipUnknown)
        );
        assert_eq!(store.load(id).unwrap().unwrap(), before_unauthenticated);
        let restarted = Arc::new(Mutex::new(SupervisorRuntime::new(temp.path())));
        let (after_restart, _) = serve_supervisor_request(
            &restarted,
            &ipc_generation(),
            &hello,
            None,
            supervisor_request(
                SupervisorToolAction::Get,
                "restart",
                serde_json::json!({"supervisor_run_id":run_id}),
            ),
        );
        assert!(
            matches!(after_restart, ResponseOutcome::Error(error) if error.code == ErrorCode::OwnershipUnknown)
        );

        let (cancelled, _) = serve_supervisor_request(
            &runtime,
            &rollover,
            &hello,
            Some(&caller),
            supervisor_request(
                SupervisorToolAction::Cancel,
                "cancel",
                serde_json::json!({"supervisor_run_id":run_id,"reason":"owner"}),
            ),
        );
        assert_eq!(cancelled, ResponseOutcome::Ok);
    }

    fn fence_in(role: GenerationRole) -> GenerationFence {
        GenerationFence {
            gate: AdmissionGate::new(DaemonGeneration::new(), role),
            ledger: Arc::new(RoutingLedger::new()),
        }
    }

    fn session_request() -> serde_json::Value {
        serde_json::json!({"kind": "session", "action": "list", "operation_id": "op", "payload": null})
    }

    fn attach_request() -> serde_json::Value {
        let terminal = usagi_core::domain::id::TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        };
        serde_json::to_value(DaemonRequest::Terminal {
            action: TerminalAction::Attach,
            payload: serde_json::to_value(TerminalRequest::Attach {
                terminal,
                geometry: None,
            })
            .unwrap(),
        })
        .unwrap()
    }

    /// The fence changes nothing for the one active generation this build runs:
    /// every request a client sent before is still dispatched, and the terminal
    /// owner still sees its own IO.
    #[test]
    fn an_active_generation_serves_every_request_through_its_fence_unchanged() {
        use usagi_core::infrastructure::ipc::ResponseOutcome;
        let fence = fence_in(GenerationRole::Active);
        let (outcomes, seen) = serve_through_fence(
            &fence,
            &fence_client_hello(vec![
                usagi_core::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY.to_owned(),
            ]),
            &[session_request(), attach_request()],
        );
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes
                .iter()
                .all(|outcome| !matches!(outcome, ResponseOutcome::Error(_))),
            "{outcomes:?}"
        );
        assert_eq!(seen, vec![TerminalAction::Attach]);
        // Every lease the two requests took has been released, so a barrier
        // starting now would not wait on this connection.
        assert_eq!(fence.gate.outstanding(LeaseClass::ActiveControl), 0);
        assert_eq!(fence.gate.outstanding(LeaseClass::OwnerTerminal), 0);
    }

    /// The pair this fence exists for: once the role is `draining`, control is
    /// refused with zero effect from the *next request onwards* — on a connection
    /// that was admitted while the generation was still active — while IO on the
    /// terminals it owns keeps being served.
    #[test]
    fn a_draining_generation_refuses_control_and_still_serves_its_own_terminals() {
        use usagi_core::infrastructure::ipc::{ErrorCode, ResponseOutcome};
        let fence = fence_in(GenerationRole::Active);
        fence.gate.close(LeaseClass::ActiveControl);
        fence.gate.await_drain(LeaseClass::ActiveControl).unwrap();
        fence.gate.enter_draining().unwrap();

        let (outcomes, seen) = serve_through_fence(
            &fence,
            &fence_client_hello(vec![
                usagi_core::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY.to_owned(),
            ]),
            &[session_request(), attach_request()],
        );
        match &outcomes[0] {
            ResponseOutcome::Error(error) => {
                assert_eq!(error.code, ErrorCode::GenerationRolledOver);
            }
            other => panic!("a draining generation admitted control work: {other:?}"),
        }
        assert!(
            !matches!(outcomes[1], ResponseOutcome::Error(_)),
            "{:?}",
            outcomes[1]
        );
        // Effect zero for the refused control request: the owner saw only the
        // terminal IO it owns.
        assert_eq!(seen, vec![TerminalAction::Attach]);
    }

    /// A retired generation admits nothing at all, terminal IO included, and the
    /// terminal owner is never reached.
    #[test]
    fn a_retired_generation_admits_nothing_and_reaches_no_owner() {
        use usagi_core::infrastructure::ipc::{ErrorCode, ResponseOutcome};
        let fence = fence_in(GenerationRole::Active);
        fence.gate.close(LeaseClass::ActiveControl);
        fence.gate.close(LeaseClass::OwnerTerminal);
        fence.gate.enter_retired().unwrap();

        let (outcomes, seen) = serve_through_fence(
            &fence,
            &fence_client_hello(Vec::new()),
            &[session_request(), attach_request()],
        );
        assert_eq!(outcomes.len(), 2);
        for outcome in &outcomes {
            match outcome {
                ResponseOutcome::Error(error) => {
                    assert_eq!(error.code, ErrorCode::GenerationRolledOver);
                }
                other => panic!("a retired generation admitted work: {other:?}"),
            }
        }
        assert!(seen.is_empty(), "{seen:?}");
    }

    /// The ledger half: a connection is recorded with the routing answer it
    /// advertised, which is what decides whether a rollover may leave this
    /// generation draining at all — a client that cannot address a draining owner
    /// is counted as unsupported and blocks the rollover.
    #[test]
    fn the_fence_records_each_connections_routing_answer() {
        use usagi_daemon::presentation::ipc::ConnectionFence;
        let fence = fence_in(GenerationRole::Active);
        let routing = fence_client_hello(vec![
            usagi_core::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY.to_owned(),
        ]);
        let old_build = fence_client_hello(Vec::new());

        let (first, second) = (ConnectionId::new(), ConnectionId::new());
        fence.admitted(first, &routing);
        fence.admitted(second, &old_build);
        assert_eq!(fence.ledger.connections(), 2);
        assert_eq!(fence.ledger.unsupported(), 1);

        // The peer that could not address a draining owner has gone away, so it
        // stops blocking a rollover.
        fence.disconnected(second);
        assert_eq!(fence.ledger.connections(), 1);
        assert_eq!(fence.ledger.unsupported(), 0);
    }

    /// The loop itself performs that pair: a connection is admitted after the
    /// handshake and forgotten on every exit, so the ledger tracks live
    /// connections rather than historical ones.
    #[test]
    fn serving_a_connection_admits_it_to_the_ledger_and_forgets_it_at_the_end() {
        let fence = fence_in(GenerationRole::Active);
        assert_eq!(fence.ledger.connections(), 0);
        serve_through_fence(
            &fence,
            &fence_client_hello(Vec::new()),
            &[session_request()],
        );
        assert_eq!(fence.ledger.connections(), 0);
    }

    /// A worker whose stream could not be duplicated is not retained: retirement
    /// must never park on a thread it has no way to unblock.
    #[test]
    fn only_a_collectable_client_worker_is_retained() {
        let workers = ClientWorkers::new();
        // A refused worker's handle is consumed and dropped, so nothing can join
        // it afterwards — it is driven to completion *before* being handed over.
        // A worker thread still running writes coverage counters while the harness
        // dumps the profile, and that race reports lines other tests certainly
        // executed as unreached.
        let refused = std::thread::spawn(|| {});
        while !refused.is_finished() {
            std::thread::yield_now();
        }
        retain_client_worker(
            &workers,
            Err(std::io::Error::other("no descriptors")),
            refused,
        );
        assert_eq!(workers.outstanding(), 0);

        // The production-owned admission counter and worker set are injected
        // independently. Holding the permit inside the worker makes the two
        // observable lifetimes match the accept-loop contract without asking
        // the OS for a process-wide thread census.
        let pre_handshake = PreHandshakeAdmission::new(1);
        let permit = pre_handshake
            .try_admit()
            .expect("the incomplete handshake reserves the only permit");
        // Shaped exactly as the accept loop builds it: the retained half is a
        // duplicate of the *accepted* socket, and the worker parks on that same
        // socket armed with the retirement poll. `peer` stays open so nothing but
        // retirement can end the read.
        let (mut peer, accepted) = std::os::unix::net::UnixStream::pair().unwrap();
        let unblock = accepted.try_clone().map(AcceptedStream::new);
        let mut parked_stream = RetiringReader::new(
            accepted,
            unblock
                .as_ref()
                .expect("the accepted socket duplicates")
                .retirement(),
            Duration::from_millis(10),
        );
        let parked = std::thread::spawn(move || {
            let _permit = permit;
            // Parked exactly as a client worker is: blocked reading a frame that
            // never arrives.
            let mut byte = [0_u8; 1];
            let _ = parked_stream.read(&mut byte);
        });
        retain_client_worker(&workers, unblock, parked);
        assert_eq!(pre_handshake.in_flight(), 1);
        assert!(pre_handshake.try_admit().is_none());
        assert_eq!(workers.outstanding(), 1);

        // Retirement shuts the retained half down, which is what lets the join
        // return. A test that hung here would be reporting a real defect.
        let report = workers.retire();
        assert_eq!(report.joined, 1);
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(pre_handshake.in_flight(), 0);
        assert_eq!(workers.outstanding(), 0);
        let mut byte = [0_u8; 1];
        assert_eq!(
            peer.read(&mut byte).unwrap(),
            0,
            "retirement closes the socket"
        );
    }

    /// Builds a reader parked on a socketpair nothing ever writes to, plus the
    /// peer that keeps it open.
    fn parked_reader(
        retired: &Arc<AtomicBool>,
    ) -> (std::os::unix::net::UnixStream, RetiringReader) {
        let (peer, accepted) = std::os::unix::net::UnixStream::pair().unwrap();
        let reader = RetiringReader::new(accepted, Arc::clone(retired), Duration::from_millis(5));
        (peer, reader)
    }

    /// The defect this exists for: `shutdown(2)` can return `Ok` for a duplicate
    /// of an `AF_UNIX` socket without returning a peer parked in an indefinite
    /// `recv` — and once the socket is in that state a receive timeout is not
    /// honoured either. The worker must therefore stop on the flag alone, with no
    /// socket wakeup of any kind: nothing here is ever written, closed or shut
    /// down.
    #[test]
    fn a_retired_reader_stops_without_any_socket_wakeup() {
        let retired = Arc::new(AtomicBool::new(true));
        let (_peer, mut reader) = parked_reader(&retired);

        let mut byte = [0_u8; 1];
        assert_eq!(
            reader.read(&mut byte).unwrap(),
            0,
            "retirement reads as end of stream"
        );
    }

    /// The readiness wait is a retirement backstop, not an idle policy. The
    /// reader is driven until it has actually parked across several waits —
    /// observed, not assumed — and only then is a frame written; it must still be
    /// served.
    #[test]
    fn a_live_reader_crosses_its_waits_and_still_serves_the_next_frame() {
        let retired = Arc::new(AtomicBool::new(false));
        let (mut peer, mut reader) = parked_reader(&retired);
        let timeouts = reader.timeouts();

        let served = std::thread::spawn(move || {
            let mut byte = [0_u8; 1];
            let read = reader.read(&mut byte).unwrap();
            (read, byte[0])
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while timeouts.load(Ordering::Acquire) < 3 {
            assert!(
                Instant::now() < deadline,
                "the reader never parked, so this run proves nothing about retrying"
            );
            std::thread::yield_now();
        }
        peer.write_all(b"f").unwrap();

        assert_eq!(served.join().unwrap(), (1, b'f'));
        assert!(!retired.load(Ordering::Acquire));
    }

    /// The retained half is what publishes the flag, so shutting it down is what
    /// a parked worker observes — including through the ordinary socket wakeup.
    #[test]
    fn shutting_the_retained_half_down_stops_the_parked_reader() {
        let (_peer, accepted) = std::os::unix::net::UnixStream::pair().unwrap();
        let retained = AcceptedStream::new(accepted.try_clone().unwrap());
        let mut reader =
            RetiringReader::new(accepted, retained.retirement(), Duration::from_millis(5));
        retained.shutdown().unwrap();

        let mut byte = [0_u8; 1];
        assert_eq!(
            reader.read(&mut byte).unwrap(),
            0,
            "a retired worker stops on its own"
        );
    }

    /// A finished worker can stay in `ClientWorkers` until the next accept
    /// triggers reaping. Its collection handle must not keep the accepted fd
    /// open during that interval: a long-lived daemon otherwise accumulates one
    /// descriptor for every historical short-lived client.
    #[test]
    fn worker_completion_closes_the_shared_retirement_descriptor_before_reaping() {
        let (mut peer, accepted) = std::os::unix::net::UnixStream::pair().unwrap();
        let retained = AcceptedStream::new(accepted);
        let completion = retained.clone();

        drop(ShutdownAcceptedStreamOnDrop(Some(completion)));

        let mut byte = [0_u8; 1];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
        assert!(retained.shutdown().is_ok(), "closing twice is idempotent");
    }

    #[test]
    fn established_client_capacity_reaps_completion_but_refuses_live_workers() {
        let workers = ClientWorkers::new();
        let (mut peer, mut accepted) = std::os::unix::net::UnixStream::pair().unwrap();
        let retained = AcceptedStream::new(accepted.try_clone().unwrap());
        let completion = retained.clone();
        let worker = std::thread::spawn(move || {
            let _completion = ShutdownAcceptedStreamOnDrop(Some(completion));
            let mut byte = [0_u8; 1];
            let _ = accepted.read(&mut byte);
        });
        retain_client_worker(&workers, Ok(retained), worker);

        assert!(!client_connection_capacity_available(&workers, 1));
        peer.write_all(&[1]).unwrap();
        drop(peer);
        while workers.outstanding() != 0 && !client_connection_capacity_available(&workers, 1) {
            std::thread::yield_now();
        }
        assert!(client_connection_capacity_available(&workers, 1));
        assert_eq!(workers.outstanding(), 0);
    }

    #[test]
    fn established_client_capacity_uses_the_process_descriptor_budget() {
        assert_eq!(client_connection_limit_from_nofile(32), 1);
        assert_eq!(client_connection_limit_from_nofile(256), 42);
        assert_eq!(client_connection_limit_from_nofile(2_560), 256);
        assert_eq!(client_connection_limit_from_nofile(u64::MAX), 256);
    }

    #[test]
    fn capacity_refusal_is_logged_once_per_saturated_interval() {
        let mut log = CapacityRefusalLog::default();
        assert!(log.should_record(false));
        assert!(!log.should_record(false));
        assert!(!log.should_record(true));
        assert!(log.should_record(false));
    }

    #[test]
    fn connection_cleanup_worker_drains_disconnects_in_order_before_shutdown() {
        let (disconnected, disconnects) = mpsc::channel();
        let cleaned = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&cleaned);
        let worker = start_connection_cleanup_worker_with(disconnects, move |connection| {
            observed.lock().unwrap().push(connection);
        })
        .unwrap();
        let first = ConnectionId::new();
        let second = ConnectionId::new();

        disconnected.send(first).unwrap();
        disconnected.send(second).unwrap();
        drop(disconnected);
        worker.join().unwrap();

        assert_eq!(*cleaned.lock().unwrap(), vec![first, second]);
    }
}
