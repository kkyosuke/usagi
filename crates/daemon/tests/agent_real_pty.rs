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

use std::collections::BTreeSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use usagi_core::domain::agent::{
    AgentProfile, AgentProfileId, DurableLaunchSnapshot, EnvironmentVariableName, LaunchMode,
    LaunchPlan,
};
use usagi_core::domain::id::{
    ClientId, ConnectionId, DaemonGeneration, OperationId, RequestId, SessionId, TerminalRef,
    WorkspaceId,
};
use usagi_core::infrastructure::ipc::ErrorCode;
use usagi_core::usecase::agent::AgentProfileCatalog;
use usagi_core::usecase::client::{AgentLaunchIntent, TerminalAction, TerminalRequest};
use usagi_daemon::infrastructure::pty::PtyTerminal;
use usagi_daemon::usecase::agent_ipc::{
    AgentRuntime, AgentTerminalActor, ResolvedAgentScope, ScopeResolveError, SessionScopeResolver,
    TerminalOutcome,
};
use usagi_daemon::usecase::claude::{
    ClaudeAdapter, ClaudeProvision, ClaudeProvisionFailure, ClaudeProvisioner,
};
use usagi_daemon::usecase::generation::ProcessIdentity;
use usagi_daemon::usecase::orchestration::AdapterRegistry;
use usagi_daemon::usecase::runtime::{
    AdapterError, AgentAdapter, OutputJournal, ProvisionContext, PtySpawner, ResolvedLaunch,
    RuntimeStore, RuntimeStoreSnapshot, SpawnProvision,
};
use usagi_daemon::usecase::terminal::{Geometry, Output, PtyWriteError, PtyWriter, SpawnFailure};

// ---- shared fakes -----------------------------------------------------------

#[derive(Default)]
struct MemoryStore(Vec<RuntimeStoreSnapshot>);
impl RuntimeStore for MemoryStore {
    type Error = ();
    fn save(&mut self, snapshot: RuntimeStoreSnapshot) -> Result<(), ()> {
        self.0.push(snapshot);
        Ok(())
    }
}

#[derive(Default)]
struct MemoryJournal(Vec<Output>);
impl OutputJournal for MemoryJournal {
    type Error = ();
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
    ));

    loop {
        match observations.recv_timeout(Duration::from_secs(10)) {
            Ok(Observation::Output(reference, bytes)) => {
                runtime.output(&reference, bytes).unwrap();
            }
            Ok(Observation::Exited(reference, status)) => {
                runtime.exit(&reference, status).unwrap();
                break;
            }
            Err(error) => panic!("real PTY produced no exit before the timeout: {error}"),
        }
    }

    let resync = handled(runtime.handle_terminal(
        connection,
        client,
        RequestId::new(),
        TerminalAction::Resync,
        TerminalRequest::Resync {
            terminal: terminal.clone(),
        },
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
        ),
        TerminalOutcome::NotOwned
    ));
}
