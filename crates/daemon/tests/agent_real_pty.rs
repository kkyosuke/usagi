//! Injected real-PTY regression for the daemon Agent runtime.
//!
//! The fully deterministic fake-IPC + fake-PTY E2E lives in the
//! `usecase::agent_ipc` unit tests. This suite instead drives a *real*
//! [`PtyTerminal`] through the public Agent owner so the reader/drain/exit
//! worker wiring is exercised end to end:
//!
//! * a real shell PTY streams output and commits a durable exit, and
//! * the real Claude adapter renders a durable plan whose real PTY spawn fails
//!   closed (the product binary cannot be found) into a safe daemon error —
//!   never a replacement spawn or a guessed terminal.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use usagi_core::domain::agent::{
    AgentProfile, AgentProfileId, AgentResumeTarget, DurableLaunchSnapshot,
    EnvironmentVariableName, LaunchMode, LaunchPlan, ProviderResumeReason,
};
use usagi_core::domain::id::{
    ClientId, ConnectionId, DaemonGeneration, OperationId, RequestId, SessionId, TerminalId,
    TerminalRef, WorkspaceId, WorktreeId,
};
use usagi_core::domain::terminal_launch::TerminalLaunchScope;
use usagi_core::domain::terminal_visibility::{CompletedTerminalEntry, TerminalVisibilityState};
use usagi_core::infrastructure::ipc::ErrorCode;
use usagi_core::infrastructure::store::dispatch::DispatchStore;
use usagi_core::usecase::agent::AgentProfileCatalog;
use usagi_core::usecase::client::{AgentLaunchIntent, TerminalAction, TerminalRequest};
use usagi_daemon::infrastructure::pty::PtyTerminal;
use usagi_daemon::presentation::ipc::TerminalOwner;
use usagi_daemon::usecase::agent_ipc::{
    AgentRuntime, AgentTerminalActor, ResolvedAgentScope, ScopeResolveError, SessionScopeResolver,
    SharedTerminalOwner, TerminalOutcome,
};
use usagi_daemon::usecase::claude::{
    ClaudeAdapter, ClaudeProvision, ClaudeProvisionFailure, ClaudeProvisioner,
};
use usagi_daemon::usecase::generation::{
    GenerationCoordinator, GenerationError, ProcessIdentity, TerminalOwnership, TerminalState,
};
use usagi_daemon::usecase::orchestration::AdapterRegistry;
use usagi_daemon::usecase::runtime::{
    AdapterError, AgentAdapter, OutputJournal, ProvisionContext, PtySpawner, ResolvedLaunch,
    RuntimeStore, RuntimeStoreSnapshot, SpawnProvision, TerminateReapError,
};
use usagi_daemon::usecase::terminal::{
    Geometry, Output, PtyWriteError, PtyWriter, SnapshotWire, SpawnFailure,
};

// ---- shared fakes -----------------------------------------------------------

/// A generic terminal owner that holds nothing, so a `SharedTerminalOwner` can
/// route Agent-only scenarios without a real generic PTY runtime.
struct EmptyGeneric;
impl TerminalOwner for EmptyGeneric {
    fn request(
        &mut self,
        _: ConnectionId,
        _: ClientId,
        _: RequestId,
        _: TerminalAction,
        _: Value,
        _: SnapshotWire,
    ) -> Result<Value, usagi_core::infrastructure::ipc::ProtocolError> {
        Err(usagi_core::infrastructure::ipc::ProtocolError::new(
            ErrorCode::NotFound,
            "no generic terminal",
        ))
    }
    fn disconnect(&mut self, _: ConnectionId) {}
}

#[derive(Default)]
struct MemoryStore(Vec<RuntimeStoreSnapshot>);
impl RuntimeStore for MemoryStore {
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), ()> {
        self.0.push(snapshot);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct SharedMemoryStore(Arc<Mutex<Vec<RuntimeStoreSnapshot>>>);
impl RuntimeStore for SharedMemoryStore {
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), ()> {
        self.0.lock().map_err(|_| ())?.push(snapshot);
        Ok(())
    }
}

#[derive(Default)]
struct FailSecondSaveStore {
    saves: usize,
    snapshots: Vec<RuntimeStoreSnapshot>,
}
struct FailFirstSaveStore;
impl RuntimeStore for FailFirstSaveStore {
    fn save(&mut self, _: RuntimeStoreSnapshot) -> Result<(), ()> {
        Err(())
    }
}
impl RuntimeStore for FailSecondSaveStore {
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), ()> {
        self.saves += 1;
        if self.saves == 2 {
            return Err(());
        }
        self.snapshots.push(snapshot);
        Ok(())
    }
}

#[derive(Default)]
struct MemoryJournal(Vec<Output>);
impl OutputJournal for MemoryJournal {
    fn append(&mut self, output: &Output) -> Result<(), ()> {
        self.0.push(output.clone());
        Ok(())
    }
}

struct FixedScope {
    worktree_id: usagi_core::domain::id::WorktreeId,
    working_directory: PathBuf,
}
impl SessionScopeResolver for FixedScope {
    fn resolve_available_scope(
        &self,
        _: WorkspaceId,
        _: Option<SessionId>,
    ) -> Result<ResolvedAgentScope, ScopeResolveError> {
        Ok(ResolvedAgentScope {
            worktree_id: self.worktree_id,
            working_directory: self.working_directory.clone(),
        })
    }
}

/// A real-PTY spawner: it opens an actual pseudo-terminal for the rendered
/// plan, drains its output to a channel, and reaps the child to a durable exit.
enum Observation {
    Output(TerminalRef, Vec<u8>),
    Exited(TerminalRef, i32),
}
struct RealPtySpawner {
    observations: Sender<Observation>,
    environment: Vec<(String, String)>,
    terminals: BTreeMap<String, Arc<Mutex<PtyTerminal>>>,
    spawns: Arc<AtomicUsize>,
    terminations: Arc<AtomicUsize>,
    break_registry_after_spawn: Option<PathBuf>,
}
impl PtySpawner for RealPtySpawner {
    fn spawn(
        &mut self,
        launch: &DurableLaunchSnapshot,
        provision: &SpawnProvision,
        terminal: &TerminalRef,
    ) -> Result<ProcessIdentity, SpawnFailure> {
        let plan = &launch.plan;
        let mut argv = plan.argv.clone();
        argv.extend(provision.arguments().iter().cloned());
        let environment = provision.compose_environment(
            &self
                .environment
                .iter()
                .cloned()
                .collect::<std::collections::BTreeMap<_, _>>(),
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
        self.spawns.fetch_add(1, Ordering::SeqCst);
        if let Some(path) = &self.break_registry_after_spawn {
            std::fs::rename(path, path.with_extension("saved"))
                .map_err(|_| SpawnFailure::Ambiguous)?;
            std::fs::create_dir(path).map_err(|_| SpawnFailure::Ambiguous)?;
        }
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
                    .send(Observation::Output(
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
                let _ = observations.send(Observation::Exited(output_terminal, status));
            }
        });
        Ok(ProcessIdentity {
            pid,
            start_identity: "real-pty".to_owned(),
            process_group: pid,
        })
    }

    fn terminate_reap(&mut self, terminal: &TerminalRef) -> Result<(), TerminateReapError> {
        let key = terminal.terminal_id.as_str();
        let pty = self.terminals.get(&key).ok_or(TerminateReapError)?;
        pty.lock()
            .map_err(|_| TerminateReapError)?
            .terminate_reap()
            .map_err(|_| TerminateReapError)?;
        self.terminals.remove(&key);
        self.terminations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
impl PtyWriter for RealPtySpawner {
    fn write_all(&mut self, _: &[u8]) -> Result<(), PtyWriteError> {
        Ok(())
    }
}

fn intent(profile: Option<&str>) -> AgentLaunchIntent {
    AgentLaunchIntent {
        workspace: WorkspaceId::new(),
        session: Some(SessionId::new()),
        profile: profile.map(|name| AgentProfileId::new(name).unwrap()),
    }
}

fn handled(outcome: TerminalOutcome) -> Value {
    match outcome {
        TerminalOutcome::Handled(result) => result.unwrap(),
        TerminalOutcome::NotOwned => panic!("agent owner should own its terminal"),
    }
}

fn finish_real_pty(
    runtime: &mut AgentRuntime,
    observations: &Receiver<Observation>,
    terminal: &TerminalRef,
) {
    loop {
        match observations.recv_timeout(Duration::from_secs(10)) {
            Ok(Observation::Output(reference, bytes)) => {
                assert_eq!(&reference, terminal);
                runtime.output(&reference, bytes).unwrap();
            }
            Ok(Observation::Exited(reference, status)) => {
                assert_eq!(&reference, terminal);
                runtime.exit(&reference, status).unwrap();
                return;
            }
            Err(error) => panic!("real PTY produced no exit before the timeout: {error}"),
        }
    }
}

// ---- happy path: a real shell PTY streams output and commits an exit --------

/// A test adapter that renders a harmless real shell into the durable plan so
/// the regression does not depend on any product binary being installed.
struct ShellAdapter {
    profile: AgentProfile,
    script: String,
}
impl AgentProfileCatalog for ShellAdapter {
    fn find(&self, id: &AgentProfileId) -> Option<AgentProfile> {
        (id == &self.profile.id).then(|| self.profile.clone())
    }
}
impl AgentAdapter for ShellAdapter {
    fn resolve(
        &mut self,
        request: &usagi_core::domain::agent::LaunchRequest,
    ) -> Result<ResolvedLaunch, AdapterError> {
        let plan = LaunchPlan::new(
            request.profile_id.clone(),
            self.profile.revision,
            "/bin/sh",
            vec!["-c".to_owned(), self.script.clone()],
            [],
            PathBuf::from("/"),
        )
        .expect("shell plan is valid");
        Ok(ResolvedLaunch {
            snapshot: DurableLaunchSnapshot::new(request.clone(), plan),
            provision: SpawnProvision::new(
                [
                    (
                        EnvironmentVariableName::new("USAGI_ADAPTER_CREDENTIAL").unwrap(),
                        "adapter-present".to_owned(),
                    ),
                    (
                        EnvironmentVariableName::new("USAGI_PRIORITY").unwrap(),
                        "adapter".to_owned(),
                    ),
                    (
                        EnvironmentVariableName::new("USAGI_MCP_CALLER_CREDENTIAL").unwrap(),
                        "adapter-forged".to_owned(),
                    ),
                ],
                Vec::new(),
            ),
            provider_resume: request.provider_resume.clone(),
        })
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One real-PTY scenario covers spawn, output, and durable exit.
fn agent_real_pty_rebuilds_the_allowlisted_environment_and_commits_exit() {
    if std::env::var_os("USAGI_AGENT_PTY_TEST_HELPER").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent_real_pty_rebuilds_the_allowlisted_environment_and_commits_exit",
                "--nocapture",
            ])
            .env("USAGI_AGENT_PTY_TEST_HELPER", "1")
            .env("USAGI_AGENT_PTY_SENTINEL", "must-not-leak")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let (sender, observations): (Sender<Observation>, Receiver<Observation>) = mpsc::channel();
    let profile = AgentProfile::new(
        AgentProfileId::new("claude").unwrap(),
        "Claude",
        1,
        [],
        [LaunchMode::Interactive],
    );
    let mut registry = AdapterRegistry::new();
    registry
        .register(
            profile.clone(),
            Box::new(ShellAdapter {
                profile,
                script: concat!(
                    "printf '%s|%s|%s|%s|%s|%s' ",
                    "\"${USAGI_AGENT_PTY_SENTINEL-unset}\" ",
                    "\"$PATH\" \"$HOME\" \"$USAGI_ADAPTER_CREDENTIAL\" ",
                    "\"$USAGI_PRIORITY\" ",
                    "\"$(test \"$USAGI_MCP_CALLER_CREDENTIAL\" = adapter-forged && ",
                    "printf adapter-forged || printf daemon-present)\""
                )
                .to_owned(),
            }),
        )
        .unwrap();
    let mut runtime = AgentRuntime::new(
        DaemonGeneration::new(),
        registry,
        MemoryStore::default(),
        MemoryJournal::default(),
        RealPtySpawner {
            observations: sender,
            environment: vec![
                ("PATH".to_owned(), "/public/bin".to_owned()),
                ("HOME".to_owned(), "/public/home".to_owned()),
                ("USAGI_PRIORITY".to_owned(), "profile".to_owned()),
            ],
            terminals: BTreeMap::new(),
            spawns: Arc::new(AtomicUsize::new(0)),
            terminations: Arc::new(AtomicUsize::new(0)),
            break_registry_after_spawn: None,
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    let scope = FixedScope {
        worktree_id: usagi_core::domain::id::WorktreeId::new(),
        working_directory: PathBuf::from("/"),
    };
    let admission = runtime
        .launch(&OperationId::new().to_string(), &intent(None), &scope)
        .unwrap();
    let terminal = admission.terminal.clone();

    // Attach while running, then drain the real PTY into the durable journal.
    let connection = ConnectionId::new();
    let client = ClientId::new();
    handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Attach,
        TerminalRequest::Attach {
            terminal: terminal.clone(),
        },
        SnapshotWire::RawTail,
    ));

    finish_real_pty(&mut runtime, &observations, &terminal);

    let resync = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resync,
        TerminalRequest::Resync {
            terminal: terminal.clone(),
        },
        SnapshotWire::RawTail,
    ));
    assert_eq!(resync["exited"], 0);
    let replay = resync["replay"].as_array().unwrap();
    let bytes: Vec<u8> = replay
        .iter()
        .map(|value| u8::try_from(value.as_u64().unwrap()).unwrap())
        .collect();
    assert_eq!(
        bytes,
        b"unset|/public/bin|/public/home|adapter-present|adapter|daemon-present"
    );

    // #525: after the real PTY exits, the tombstone is reachable through the
    // completed inventory with the real exit status and final replay locator,
    // and observe/dismiss converge its workspace-global visibility without
    // resurrecting or removing it.
    let query_scope = TerminalLaunchScope {
        workspace_id: terminal.workspace_id,
        session_id: terminal.session_id,
        worktree_id: terminal.worktree_id,
    };
    let mut owner = SharedTerminalOwner::new(runtime, EmptyGeneric);
    let completed_inventory = |owner: &mut SharedTerminalOwner<EmptyGeneric, AgentRuntime>| {
        let response = owner
            .request(
                connection,
                client,
                RequestId::new(),
                TerminalAction::CompletedInventory,
                serde_json::to_value(TerminalRequest::CompletedInventory {
                    scope: query_scope.clone(),
                })
                .unwrap(),
                SnapshotWire::RawTail,
            )
            .unwrap();
        serde_json::from_value::<Vec<CompletedTerminalEntry>>(response["entries"].clone()).unwrap()
    };

    let entries = completed_inventory(&mut owner);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].terminal.fences(&terminal));
    assert_eq!(entries[0].exit_status, 0);
    assert_eq!(entries[0].final_output_offset, bytes.len() as u64);
    assert_eq!(
        entries[0].visibility.state,
        TerminalVisibilityState::Unobserved
    );

    let observed = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Observe,
            serde_json::to_value(TerminalRequest::Observe {
                terminal: terminal.clone(),
                expected_revision: 0,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap();
    assert_eq!(observed["applied"], serde_json::json!(true));
    let dismissed = owner
        .request(
            connection,
            client,
            RequestId::new(),
            TerminalAction::Dismiss,
            serde_json::to_value(TerminalRequest::Dismiss {
                terminal: terminal.clone(),
                expected_revision: 1,
            })
            .unwrap(),
            SnapshotWire::RawTail,
        )
        .unwrap();
    assert_eq!(dismissed["applied"], serde_json::json!(true));

    // A second query still returns the retained tombstone (never resurrected as
    // a fresh entry, never removed) and reports the converged Dismissed state.
    let entries = completed_inventory(&mut owner);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].visibility.state,
        TerminalVisibilityState::Dismissed
    );
}

#[test]
fn post_spawn_commit_failure_terminates_reaps_and_never_respawns_the_operation() {
    let (sender, _observations): (Sender<Observation>, Receiver<Observation>) = mpsc::channel();
    let profile = AgentProfile::new(
        AgentProfileId::new("claude").unwrap(),
        "Claude",
        1,
        [],
        [LaunchMode::Interactive],
    );
    let mut registry = AdapterRegistry::new();
    registry
        .register(
            profile.clone(),
            Box::new(ShellAdapter {
                profile,
                script: "sleep 30".to_owned(),
            }),
        )
        .unwrap();
    let spawns = Arc::new(AtomicUsize::new(0));
    let terminations = Arc::new(AtomicUsize::new(0));
    let mut runtime = AgentRuntime::new(
        DaemonGeneration::new(),
        registry,
        FailSecondSaveStore::default(),
        MemoryJournal::default(),
        RealPtySpawner {
            observations: sender,
            environment: Vec::new(),
            terminals: BTreeMap::new(),
            spawns: Arc::clone(&spawns),
            terminations: Arc::clone(&terminations),
            break_registry_after_spawn: None,
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    let scope = FixedScope {
        worktree_id: usagi_core::domain::id::WorktreeId::new(),
        working_directory: PathBuf::from("/"),
    };
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);

    assert_eq!(
        runtime
            .launch(&operation, &launch_intent, &scope)
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
    assert_eq!(terminations.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .launch(&operation, &launch_intent, &scope)
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    let mut conflict = launch_intent;
    conflict.profile = Some(AgentProfileId::new("other").unwrap());
    assert_eq!(
        runtime
            .launch(&operation, &conflict, &scope)
            .unwrap_err()
            .code,
        ErrorCode::IdempotencyConflict
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
}

#[test]
fn pre_spawn_dispatch_and_runtime_save_failures_never_create_a_real_pty() {
    fn registry() -> AdapterRegistry {
        let profile = AgentProfile::new(
            AgentProfileId::new("claude").unwrap(),
            "Claude",
            1,
            [],
            [LaunchMode::Interactive],
        );
        let mut registry = AdapterRegistry::new();
        registry
            .register(
                profile.clone(),
                Box::new(ShellAdapter {
                    profile,
                    script: "sleep 30".to_owned(),
                }),
            )
            .unwrap();
        registry
    }
    let scope = FixedScope {
        worktree_id: usagi_core::domain::id::WorktreeId::new(),
        working_directory: PathBuf::from("/"),
    };

    let invalid_parent = tempfile::NamedTempFile::new().unwrap();
    let (sender, _observations) = mpsc::channel();
    let dispatch_spawns = Arc::new(AtomicUsize::new(0));
    let mut dispatch_failure = AgentRuntime::with_dispatch(
        DaemonGeneration::new(),
        registry(),
        MemoryStore::default(),
        MemoryJournal::default(),
        RealPtySpawner {
            observations: sender,
            environment: Vec::new(),
            terminals: BTreeMap::new(),
            spawns: Arc::clone(&dispatch_spawns),
            terminations: Arc::new(AtomicUsize::new(0)),
            break_registry_after_spawn: None,
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        DispatchStore::new(invalid_parent.path()),
    );
    assert_eq!(
        dispatch_failure
            .launch(&OperationId::new().to_string(), &intent(None), &scope)
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(dispatch_spawns.load(Ordering::SeqCst), 0);

    let (sender, _observations) = mpsc::channel();
    let runtime_spawns = Arc::new(AtomicUsize::new(0));
    let mut runtime_failure = AgentRuntime::new(
        DaemonGeneration::new(),
        registry(),
        FailFirstSaveStore,
        MemoryJournal::default(),
        RealPtySpawner {
            observations: sender,
            environment: Vec::new(),
            terminals: BTreeMap::new(),
            spawns: Arc::clone(&runtime_spawns),
            terminations: Arc::new(AtomicUsize::new(0)),
            break_registry_after_spawn: None,
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    assert_eq!(
        runtime_failure
            .launch(&OperationId::new().to_string(), &intent(None), &scope)
            .unwrap_err()
            .code,
        ErrorCode::OwnershipUnknown
    );
    assert_eq!(runtime_spawns.load(Ordering::SeqCst), 0);
}

#[test]
fn dispatch_commit_failure_compensates_real_pty_and_keeps_prepared_fence() {
    let (sender, _observations): (Sender<Observation>, Receiver<Observation>) = mpsc::channel();
    let profile = AgentProfile::new(
        AgentProfileId::new("claude").unwrap(),
        "Claude",
        1,
        [],
        [LaunchMode::Interactive],
    );
    let mut registry = AdapterRegistry::new();
    registry
        .register(
            profile.clone(),
            Box::new(ShellAdapter {
                profile,
                script: "sleep 30".to_owned(),
            }),
        )
        .unwrap();
    let dispatch_dir = tempfile::tempdir().unwrap();
    let dispatch = DispatchStore::new(dispatch_dir.path());
    let registry_path = dispatch.registry_path();
    let spawns = Arc::new(AtomicUsize::new(0));
    let terminations = Arc::new(AtomicUsize::new(0));
    let mut runtime = AgentRuntime::with_dispatch(
        DaemonGeneration::new(),
        registry,
        MemoryStore::default(),
        MemoryJournal::default(),
        RealPtySpawner {
            observations: sender,
            environment: Vec::new(),
            terminals: BTreeMap::new(),
            spawns: Arc::clone(&spawns),
            terminations: Arc::clone(&terminations),
            break_registry_after_spawn: Some(registry_path.clone()),
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
        dispatch,
    );
    let scope = FixedScope {
        worktree_id: usagi_core::domain::id::WorktreeId::new(),
        working_directory: PathBuf::from("/"),
    };
    let operation = OperationId::new().to_string();
    let launch_intent = intent(None);

    assert_eq!(
        runtime
            .launch(&operation, &launch_intent, &scope)
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
    assert_eq!(terminations.load(Ordering::SeqCst), 1);

    std::fs::remove_dir(&registry_path).unwrap();
    std::fs::rename(registry_path.with_extension("saved"), &registry_path).unwrap();
    let durable = std::fs::read_to_string(&registry_path).unwrap();
    let durable_json: serde_json::Value = serde_json::from_str(&durable).unwrap();
    assert_eq!(durable_json["runs"][0]["status"], "preparing");
    assert!(durable.contains("daemon_minted_ephemeral"));
    assert!(!durable.contains("USAGI_MCP_CALLER_CREDENTIAL"));
    assert_eq!(
        runtime
            .launch(&operation, &launch_intent, &scope)
            .unwrap_err()
            .code,
        ErrorCode::Unavailable
    );
    assert_eq!(spawns.load(Ordering::SeqCst), 1);
}

// ---- fail-closed: the real Claude adapter when the binary is unavailable ----

struct UnavailableBinaryProvisioner;
impl ClaudeProvisioner for UnavailableBinaryProvisioner {
    fn provision(
        &mut self,
        _: &ProvisionContext,
    ) -> Result<ClaudeProvision, ClaudeProvisionFailure> {
        Ok(ClaudeProvision {
            working_directory: PathBuf::from("/"),
            environment_allowlist: BTreeSet::new(),
            spawn: SpawnProvision::new([], Vec::new()),
        })
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One production fixture covers every legacy status over exact histories.
fn production_resume_status_distinguishes_exact_claude_histories() {
    let binaries = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", binaries.path().join("claude")).unwrap();
    if std::env::var_os("USAGI_RESUME_STATUS_PTY_TEST_HELPER").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "production_resume_status_distinguishes_exact_claude_histories",
                "--nocapture",
            ])
            .env("USAGI_RESUME_STATUS_PTY_TEST_HELPER", "1")
            .env("PATH", binaries.path())
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let (sender, observations): (Sender<Observation>, Receiver<Observation>) = mpsc::channel();
    let mut registry = AdapterRegistry::new();
    let adapter = ClaudeAdapter::new(UnavailableBinaryProvisioner);
    registry
        .register(adapter.profile().clone(), Box::new(adapter))
        .unwrap();
    let shell_profile = AgentProfile::new(
        AgentProfileId::new("shell").unwrap(),
        "Shell",
        1,
        [],
        [LaunchMode::Interactive],
    );
    registry
        .register(
            shell_profile.clone(),
            Box::new(ShellAdapter {
                profile: shell_profile,
                script: "exit 0".to_owned(),
            }),
        )
        .unwrap();
    let store = SharedMemoryStore::default();
    let spawns = Arc::new(AtomicUsize::new(0));
    let mut runtime = AgentRuntime::new(
        DaemonGeneration::new(),
        registry,
        store.clone(),
        MemoryJournal::default(),
        RealPtySpawner {
            observations: sender,
            environment: vec![(
                "PATH".to_owned(),
                binaries.path().to_string_lossy().into_owned(),
            )],
            terminals: BTreeMap::new(),
            spawns: Arc::clone(&spawns),
            terminations: Arc::new(AtomicUsize::new(0)),
            break_registry_after_spawn: None,
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    let scope = FixedScope {
        worktree_id: WorktreeId::new(),
        working_directory: PathBuf::from("/"),
    };
    let history = intent(Some("claude"));
    let session = history.session.unwrap();

    let first = runtime
        .launch(&OperationId::new().to_string(), &history, &scope)
        .unwrap();
    finish_real_pty(&mut runtime, &observations, &first.terminal);
    assert_eq!(
        runtime.session_resume_status(session),
        (true, ProviderResumeReason::ExplicitResumeAvailable)
    );

    let source = store
        .0
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .records
        .iter()
        .find(|record| record.runtime.terminal == first.terminal)
        .unwrap()
        .clone();
    let target = AgentResumeTarget {
        continuation: source.continuation.unwrap(),
        source: source.resume_source.unwrap(),
        workspace_id: source.runtime.terminal.workspace_id,
        session_id: source.runtime.session_id,
        worktree_id: source.runtime.terminal.worktree_id,
        runtime_id: source.runtime.agent_runtime_id,
        adapter_revision: source.launch.plan.profile_revision,
    };
    let replacement = runtime
        .resume_exact(&OperationId::new().to_string(), &target, &scope)
        .unwrap();
    let double_click = runtime
        .resume_exact(&OperationId::new().to_string(), &target, &scope)
        .unwrap();
    assert_eq!(double_click.terminal, replacement.terminal);
    assert_eq!(double_click.resume_relation, replacement.resume_relation);
    assert_eq!(spawns.load(Ordering::SeqCst), 2);
    finish_real_pty(&mut runtime, &observations, &replacement.terminal);

    let second = runtime
        .launch(&OperationId::new().to_string(), &history, &scope)
        .unwrap();
    finish_real_pty(&mut runtime, &observations, &second.terminal);
    assert_eq!(
        runtime.session_resume_status(session),
        (false, ProviderResumeReason::AmbiguousProviderMetadata)
    );

    let live = runtime
        .launch(&OperationId::new().to_string(), &history, &scope)
        .unwrap();
    assert_eq!(
        runtime.session_resume_status(session),
        (false, ProviderResumeReason::LiveOrOwnershipUnknown)
    );
    assert_eq!(
        runtime.session_resume_status(SessionId::new()),
        (false, ProviderResumeReason::ProviderMetadataUnavailable)
    );
    finish_real_pty(&mut runtime, &observations, &live.terminal);

    let unavailable = intent(Some("shell"));
    let unavailable_session = unavailable.session.unwrap();
    let admission = runtime
        .launch(&OperationId::new().to_string(), &unavailable, &scope)
        .unwrap();
    finish_real_pty(&mut runtime, &observations, &admission.terminal);
    assert_eq!(
        runtime.session_resume_status(unavailable_session),
        (false, ProviderResumeReason::ProviderMetadataUnavailable)
    );
}

#[test]
fn real_pty_claude_launch_fails_closed_when_the_binary_is_unavailable() {
    let (sender, _observations): (Sender<Observation>, Receiver<Observation>) = mpsc::channel();
    let mut registry = AdapterRegistry::new();
    let adapter = ClaudeAdapter::new(UnavailableBinaryProvisioner);
    registry
        .register(adapter.profile().clone(), Box::new(adapter))
        .unwrap();
    let mut runtime = AgentRuntime::new(
        DaemonGeneration::new(),
        registry,
        MemoryStore::default(),
        MemoryJournal::default(),
        RealPtySpawner {
            observations: sender,
            // A PATH with no directories guarantees the bare `claude` program is
            // never found, so the real PTY spawn fails closed deterministically
            // whether or not the product binary is installed.
            environment: vec![("PATH".to_owned(), String::new())],
            terminals: BTreeMap::new(),
            spawns: Arc::new(AtomicUsize::new(0)),
            terminations: Arc::new(AtomicUsize::new(0)),
            break_registry_after_spawn: None,
        },
        AgentProfileId::new("claude").unwrap(),
        Geometry { cols: 80, rows: 24 },
    );
    let scope = FixedScope {
        worktree_id: usagi_core::domain::id::WorktreeId::new(),
        working_directory: PathBuf::from("/"),
    };
    let operation = OperationId::new().to_string();
    let launch_intent = intent(Some("claude"));
    let error = runtime
        .launch(&operation, &launch_intent, &scope)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);

    // The fenced safe failure is durable: a resend replays it and never spawns
    // a replacement or exposes a terminal to attach to.
    let replay = runtime
        .launch(&operation, &launch_intent, &scope)
        .unwrap_err();
    assert_eq!(replay.code, ErrorCode::Unavailable);
    assert!(runtime.operation_outcome(&operation).unwrap().is_err());
    // A terminal request for an operation that never produced a terminal is
    // simply not owned by the agent.
    let foreign = TerminalRef {
        daemon_generation: DaemonGeneration::new(),
        terminal_id: usagi_core::domain::id::TerminalId::new(),
        workspace_id: launch_intent.workspace,
        session_id: launch_intent.session,
        worktree_id: usagi_core::domain::id::WorktreeId::new(),
    };
    assert!(matches!(
        runtime.handle_terminal(
            ConnectionId::new(),
            ClientId::new(),
            RequestId::new(),
            TerminalAction::Attach,
            TerminalRequest::Attach { terminal: foreign },
            SnapshotWire::RawTail,
        ),
        TerminalOutcome::NotOwned
    ));
}

#[test]
fn production_runtime_rejects_terminal_ownership_without_its_generation() {
    let mut snapshot = RuntimeStoreSnapshot::default();
    snapshot.generation.terminals.push(TerminalOwnership {
        terminal: TerminalRef {
            daemon_generation: DaemonGeneration::new(),
            terminal_id: TerminalId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: Some(SessionId::new()),
            worktree_id: WorktreeId::new(),
        },
        process: None,
        state: TerminalState::IdentityUnknown,
    });

    assert_eq!(
        GenerationCoordinator::restore(snapshot.generation, 1).unwrap_err(),
        GenerationError::UnknownGeneration
    );
}
