//! Owner-generation routing against two real generations on two real sockets.
//!
//! The unit tests decide the routing table, the merge, and the presence rules
//! deterministically. What only a real pair of endpoints can show is the thing
//! a planned restart actually depends on:
//!
//! * a client whose `current.json` names the **new** generation still reaches
//!   the **old** one, over the old one's own socket, for every request that
//!   carries a complete `TerminalRef`;
//! * the old generation's child process is never respawned, signalled, or
//!   replaced by that traffic — routing moves requests, not processes;
//! * a draining endpoint that stops answering leaves its terminals reconnecting,
//!   and only its retirement from the registry collects them;
//! * a rollover is refused outright, with both endpoints and both children
//!   untouched, when a connected client cannot route by owner generation.
//!
//! Both generations run as threads in this test binary, each owning its own
//! `SecureUnixListener`, its own generation directory, and its own real child
//! process. The registry and the current locator are the real durable files, so
//! the client resolves endpoints exactly the way the shipping client does.
//!
//! The real `usagi daemon restart` end to end — a second daemon *process* and a
//! real provider PTY — stays with #507, which owns enabling the shipping
//! rollover once this routing exists.

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use usagi_core::domain::id::{DaemonGeneration, TerminalId, WorkspaceId, WorktreeId};
use usagi_core::domain::terminal_launch::{
    TerminalInventoryEntry, TerminalKind, TerminalLaunchScope,
};
use usagi_core::infrastructure::ipc::{
    ClientWorkspace, DaemonGeneration as WireGeneration, Envelope, EnvelopeKind, ProtocolLimits,
    ProtocolRange, ProtocolVersion, ResponseOutcome, ServerProtocol, build_identity,
    read_json_frame, write_json_frame,
};
use usagi_core::usecase::client::{
    ClientError, ClientPolicy, DaemonRequest, IpcClient, RearmableStream, TerminalGeometry,
    TerminalRequest,
};
use usagi_core::usecase::owner_routing::{
    GenerationDirectory, GenerationTransport, OwnerPresence, OwnerRouter, TrustedEndpoint,
    presence_of, terminal_request,
};
use usagi_daemon::infrastructure::generation_registry::{
    GenerationRegistryFile, TrustedGenerationDirectory,
};
use usagi_daemon::infrastructure::unix_transport::{SecureUnixListener, connect_generation};
use usagi_daemon::presentation::ipc::handshake;
use usagi_daemon::usecase::authority::registry::{
    DEFAULT_GENERATION_LIMIT, GenerationEntry, GenerationRegistry,
};
use usagi_daemon::usecase::authority::routing::{RolloverRefusal, RoutingLedger, admit_rollover};
use usagi_daemon::usecase::generation::{GenerationRole, ProcessIdentity};

const POLL: Duration = Duration::from_millis(10);
const DEADLINE: Duration = Duration::from_secs(20);

// ------------------------------------------------------------------- fixtures

fn artifact() -> usagi_core::infrastructure::ipc::BuildIdentity {
    build_identity("2.6.0", "fixture", "test-target", "debug", &"c".repeat(64))
}

fn scope() -> TerminalLaunchScope {
    TerminalLaunchScope {
        workspace_id: WorkspaceId::parse("11111111-1111-4111-8111-111111111111").unwrap(),
        session_id: None,
        worktree_id: WorktreeId::parse("22222222-2222-4222-8222-222222222222").unwrap(),
    }
}

fn terminal(owner: DaemonGeneration) -> usagi_core::domain::id::TerminalRef {
    usagi_core::domain::id::TerminalRef {
        daemon_generation: owner,
        terminal_id: TerminalId::new(),
        workspace_id: scope().workspace_id,
        session_id: scope().session_id,
        worktree_id: scope().worktree_id,
    }
}

// --------------------------------------------------------------- server side

/// What one generation's server observed, so the test can assert *where* each
/// request landed rather than only that it succeeded.
#[derive(Default)]
struct Observed {
    actions: Mutex<Vec<String>>,
    inputs: Mutex<Vec<Vec<u8>>>,
    resume_offsets: Mutex<Vec<u64>>,
    launches: AtomicUsize,
}

impl Observed {
    fn actions(&self) -> Vec<String> {
        self.actions.lock().unwrap().clone()
    }

    fn inputs(&self) -> Vec<Vec<u8>> {
        self.inputs.lock().unwrap().clone()
    }
}

/// One generation's server: its endpoint, the terminal it owns, the real child process
/// standing in for that terminal's PTY, and its accept loop.
struct Peer {
    generation: DaemonGeneration,
    endpoint: String,
    terminal: usagi_core::domain::id::TerminalRef,
    child: Child,
    observed: Arc<Observed>,
    /// Set to stop listing the owned terminal as live — an authoritative exit.
    exited: Arc<AtomicBool>,
    /// Set to stop answering at all — a transport failure, not an absence.
    silent: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Peer {
    fn start(data_dir: &Path) -> Self {
        let generation = DaemonGeneration::new();
        let wire = WireGeneration(generation.as_str());
        let listener = SecureUnixListener::bind_private(data_dir, wire.clone()).unwrap();
        let endpoint = format!("generations/{}/sock", generation.as_str());
        let terminal = terminal(generation);
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let observed = Arc::new(Observed::default());
        let exited = Arc::new(AtomicBool::new(false));
        let silent = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let protocol = ServerProtocol {
            daemon_generation: wire,
            connection_id: usagi_core::infrastructure::ipc::ConnectionId(String::new()),
            generation_role: usagi_core::infrastructure::ipc::GenerationRole::Active,
            supported_protocols: vec![ProtocolRange {
                generation: 1,
                min_revision: 1,
                max_revision: 2,
            }],
            capabilities: vec![
                "request.correlation.v1".into(),
                "pr.snapshot.v1".into(),
                "build.artifact.v1".into(),
                usagi_core::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY.into(),
            ],
            build: artifact(),
            limits: ProtocolLimits::default(),
            daemon_process: None,
            workspace_root: String::new(),
        };

        let worker = {
            let observed = Arc::clone(&observed);
            let exited = Arc::clone(&exited);
            let silent = Arc::clone(&silent);
            let stop = Arc::clone(&stop);
            let owned = terminal.clone();
            std::thread::spawn(move || {
                serve(
                    &listener, &protocol, &owned, &observed, &exited, &silent, &stop,
                );
            })
        };

        Self {
            generation,
            endpoint,
            terminal,
            child,
            observed,
            exited,
            silent,
            stop,
            worker: Some(worker),
        }
    }

    fn entry(&self, role: GenerationRole) -> GenerationEntry {
        GenerationEntry {
            generation: self.generation,
            role,
            endpoint: self.endpoint.clone(),
            process: ProcessIdentity {
                pid: self.child.id(),
                start_identity: format!("start-{}", self.child.id()),
                process_group: self.child.id(),
            },
            expected_build: artifact(),
            verified_build: Some(artifact()),
            revision: 1,
        }
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// One generation's accept loop. Every connection is served on this thread, one
/// at a time, which is enough: the client keeps one connection per generation.
fn serve(
    listener: &SecureUnixListener,
    protocol: &ServerProtocol,
    owned: &usagi_core::domain::id::TerminalRef,
    observed: &Observed,
    exited: &AtomicBool,
    silent: &AtomicBool,
    stop: &AtomicBool,
) {
    let deadline = Instant::now() + DEADLINE;
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        match listener.accept() {
            Ok(stream) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_millis(200)))
                    .unwrap();
                serve_connection(stream, protocol, owned, observed, exited, silent, stop);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(POLL);
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    }
}

fn serve_connection(
    mut stream: UnixStream,
    protocol: &ServerProtocol,
    owned: &usagi_core::domain::id::TerminalRef,
    observed: &Observed,
    exited: &AtomicBool,
    silent: &AtomicBool,
    stop: &AtomicBool,
) {
    let mut writer = stream.try_clone().unwrap();
    let Ok(Some(hello)) = handshake(&mut stream, &mut writer, protocol) else {
        return;
    };
    let limit = hello.limits.max_frame_bytes as usize;
    while !stop.load(Ordering::SeqCst) {
        let envelope = match read_json_frame::<Envelope>(&mut stream, limit) {
            Ok(Some(envelope)) => envelope,
            // A read timeout only means this client is idle.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            _ => return,
        };
        let EnvelopeKind::Request {
            request_id, body, ..
        } = envelope.kind
        else {
            return;
        };
        if silent.load(Ordering::SeqCst) {
            // A draining owner that stopped answering: close without a reply,
            // which is exactly the uncertainty the client must not read as an
            // authoritative absence.
            return;
        }
        let request: DaemonRequest = serde_json::from_value(body).unwrap();
        let reply_body = answer(&request, owned, observed, exited);
        let reply = Envelope {
            protocol: hello.protocol,
            daemon_generation: hello.daemon_generation.clone(),
            kind: EnvelopeKind::Response {
                request_id,
                outcome: ResponseOutcome::Ok,
                body: reply_body,
            },
        };
        if write_json_frame(&mut writer, &reply, limit).is_err() {
            return;
        }
    }
}

fn answer(
    request: &DaemonRequest,
    owned: &usagi_core::domain::id::TerminalRef,
    observed: &Observed,
    exited: &AtomicBool,
) -> serde_json::Value {
    let DaemonRequest::Terminal { payload, .. } = request else {
        observed.actions.lock().unwrap().push("control".into());
        return serde_json::json!({});
    };
    let terminal: TerminalRequest = serde_json::from_value(payload.clone()).unwrap();
    let action = format!(
        "{:?}",
        usagi_core::usecase::owner_routing::terminal_action_of(&terminal)
    );
    observed.actions.lock().unwrap().push(action);
    match terminal {
        TerminalRequest::Launch { .. } => {
            observed.launches.fetch_add(1, Ordering::SeqCst);
            serde_json::json!({"terminal": owned})
        }
        TerminalRequest::Inventory { .. } => {
            let entries = if exited.load(Ordering::SeqCst) {
                Vec::new()
            } else {
                vec![TerminalInventoryEntry {
                    terminal: owned.clone(),
                    kind: TerminalKind::Terminal,
                    live: true,
                }]
            };
            serde_json::json!({"terminals": entries})
        }
        TerminalRequest::Input {
            terminal, bytes, ..
        } => {
            // A request that reached the wrong generation would name a terminal
            // this owner does not have; failing loudly here is what proves the
            // routing rather than the reply.
            assert_eq!(&terminal, owned, "input reached the wrong generation");
            observed.inputs.lock().unwrap().push(bytes);
            serde_json::json!({"accepted": true})
        }
        TerminalRequest::Resume {
            terminal,
            after_offset,
        } => {
            assert_eq!(&terminal, owned, "resume reached the wrong generation");
            observed.resume_offsets.lock().unwrap().push(after_offset);
            serde_json::json!({"chunks": [], "output_offset": after_offset + 4})
        }
        other => {
            let target = match &other {
                TerminalRequest::Attach { terminal }
                | TerminalRequest::Resync { terminal }
                | TerminalRequest::Resize { terminal, .. }
                | TerminalRequest::Detach { terminal, .. } => Some(terminal),
                _ => None,
            };
            if let Some(target) = target {
                assert_eq!(target, owned, "request reached the wrong generation");
            }
            serde_json::json!({"subscription": 1})
        }
    }
}

// --------------------------------------------------------------- client side

/// A plain socket is deadline-free here: the servers answer promptly, and the
/// deadline state machine is the resilient client's contract, not routing's.
struct PlainStream(UnixStream);

impl Read for PlainStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for PlainStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl RearmableStream for PlainStream {
    fn rearm(&mut self, _budget_ms: u64) {}
}

/// Connects only through [`connect_generation`], so the endpoint is always the
/// one the daemon published for that generation.
struct Transport {
    data_dir: PathBuf,
    connects: Arc<Mutex<Vec<DaemonGeneration>>>,
}

impl GenerationTransport for Transport {
    type Session = IpcClient<PlainStream>;

    fn connect(&mut self, endpoint: &TrustedEndpoint) -> Result<Self::Session, ClientError> {
        let stream = connect_generation(&self.data_dir, endpoint)
            .map_err(|error| ClientError::Unavailable(error.to_string()))?;
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        self.connects.lock().unwrap().push(endpoint.generation);
        IpcClient::connect(
            PlainStream(stream),
            usagi_core::domain::id::ClientId::new().as_str(),
            "nonce".into(),
            ClientPolicy::tui(),
            artifact(),
            ClientWorkspace::Unbound,
        )
    }
}

// --------------------------------------------------------------------- world

struct World {
    _temp: TempDir,
    data_dir: PathBuf,
    old: Peer,
    new: Peer,
    connects: Arc<Mutex<Vec<DaemonGeneration>>>,
}

impl World {
    /// Two generations, mid-rollover: the old one draining with its terminal
    /// still alive, the new one active and named by `current.json`.
    fn rolled_over() -> Self {
        let temp = TempDir::new_in("/tmp").unwrap();
        let data_dir = temp.path().to_path_buf();
        let old = Peer::start(&data_dir);
        let new = Peer::start(&data_dir);
        let registry = GenerationRegistry::new(
            GenerationRegistryFile::new(&data_dir).unwrap(),
            DEFAULT_GENERATION_LIMIT,
        );
        registry
            .update(|document| {
                document
                    .generations
                    .push(old.entry(GenerationRole::Draining));
                document.generations.push(new.entry(GenerationRole::Active));
                document.current = Some(new.generation);
                Ok(())
            })
            .unwrap();
        usagi_daemon::infrastructure::unix_transport::publish_recovered_locator(
            &data_dir,
            &WireGeneration(new.generation.as_str()),
            &new.endpoint,
        )
        .unwrap();
        Self {
            _temp: temp,
            data_dir,
            old,
            new,
            connects: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn router(&self) -> OwnerRouter<TrustedGenerationDirectory, Transport> {
        OwnerRouter::new(
            TrustedGenerationDirectory::new(&self.data_dir),
            Transport {
                data_dir: self.data_dir.clone(),
                connects: Arc::clone(&self.connects),
            },
        )
    }

    fn registry(&self) -> GenerationRegistry<GenerationRegistryFile> {
        GenerationRegistry::new(
            GenerationRegistryFile::new(&self.data_dir).unwrap(),
            DEFAULT_GENERATION_LIMIT,
        )
    }

    fn connects(&self) -> Vec<DaemonGeneration> {
        self.connects.lock().unwrap().clone()
    }

    fn shutdown(&mut self) {
        self.old.shutdown();
        self.new.shutdown();
    }
}

// --------------------------------------------------------------------- tests

#[test]
fn the_trusted_directory_lists_both_generations_and_names_the_published_active() {
    let mut world = World::rolled_over();
    let directory = TrustedGenerationDirectory::new(&world.data_dir);
    let endpoints = directory.snapshot().unwrap();

    assert_eq!(endpoints.all().len(), 2);
    assert_eq!(
        endpoints.active().unwrap().generation,
        world.new.generation,
        "the client follows the published locator for control work"
    );
    let owner = endpoints.owner(world.old.generation).unwrap();
    assert_eq!(owner.endpoint, world.old.endpoint);
    assert_eq!(
        owner.role,
        usagi_core::infrastructure::ipc::GenerationRole::Draining
    );
    world.shutdown();
}

#[test]
fn a_client_pointed_at_the_new_active_still_drives_the_old_generation_terminal() {
    let mut world = World::rolled_over();
    let mut router = world.router();
    let old_pid = world.old.child.id();
    let new_pid = world.new.child.id();

    // The merged inventory sees both generations' live runtimes, each exactly
    // once and under its own owner's reference.
    let merged = router.inventory(&scope()).unwrap();
    assert_eq!(merged.entries().len(), 2);
    assert!(!merged.is_partial());
    assert_eq!(
        presence_of(&world.old.terminal, &merged, router.endpoints()),
        OwnerPresence::Live
    );

    // Every reference-addressed request follows the old owner's socket …
    for request in [
        TerminalRequest::Attach {
            terminal: world.old.terminal.clone(),
        },
        TerminalRequest::Input {
            terminal: world.old.terminal.clone(),
            subscription: 1,
            input_seq: 1,
            input_operation: None,
            bytes: b"ls\n".to_vec(),
        },
        TerminalRequest::Resync {
            terminal: world.old.terminal.clone(),
        },
        TerminalRequest::Resize {
            terminal: world.old.terminal.clone(),
            geometry: TerminalGeometry {
                cols: 100,
                rows: 40,
            },
        },
    ] {
        router.request(terminal_request(&request)).unwrap();
    }

    // … while a launch is control work on the new active generation.
    router
        .request(terminal_request(&TerminalRequest::Launch {
            intent: usagi_core::usecase::client::TerminalLaunchIntent {
                request: usagi_core::domain::terminal_launch::TerminalLaunchRequest {
                    scope: scope(),
                    profile_id: usagi_core::domain::terminal_launch::TerminalProfileId::new(
                        "shell",
                    )
                    .unwrap(),
                },
                geometry: TerminalGeometry { cols: 80, rows: 24 },
                launch_operation: None,
            },
        }))
        .unwrap();

    let old_actions = world.old.observed.actions();
    assert!(old_actions.contains(&"Attach".to_owned()));
    assert!(old_actions.contains(&"Input".to_owned()));
    assert!(old_actions.contains(&"Resync".to_owned()));
    assert!(old_actions.contains(&"Resize".to_owned()));
    assert!(
        !old_actions.contains(&"Launch".to_owned()),
        "the draining generation never spawns"
    );
    assert_eq!(world.old.observed.inputs(), vec![b"ls\n".to_vec()]);
    assert_eq!(world.new.observed.launches.load(Ordering::SeqCst), 1);
    assert!(
        world.new.observed.inputs().is_empty(),
        "no input ever reached the new generation"
    );

    // Routing moved requests, not processes.
    assert_eq!(world.old.child.id(), old_pid);
    assert_eq!(world.new.child.id(), new_pid);
    assert!(world.old.child.try_wait().unwrap().is_none());
    world.shutdown();
}

#[test]
fn closing_and_reopening_the_client_re_establishes_the_old_subscription_from_its_cursor() {
    let mut world = World::rolled_over();
    let mut router = world.router();
    router
        .request(terminal_request(&TerminalRequest::Attach {
            terminal: world.old.terminal.clone(),
        }))
        .unwrap();
    router.links_mut().advance_cursor(&world.old.terminal, 64);

    // The client goes away and comes back — a fresh router, as a TUI restart is.
    drop(router);
    let mut reopened = world.router();
    let merged = reopened.inventory(&scope()).unwrap();
    assert_eq!(
        presence_of(&world.old.terminal, &merged, reopened.endpoints()),
        OwnerPresence::Live,
        "the old tab is restored under its original owner-generation reference"
    );
    reopened
        .request(terminal_request(&TerminalRequest::Resume {
            terminal: world.old.terminal.clone(),
            after_offset: 64,
        }))
        .unwrap();
    assert_eq!(
        *world.old.observed.resume_offsets.lock().unwrap(),
        vec![64],
        "the reopened stream resumes at the cursor instead of replaying"
    );
    assert!(world.old.observed.inputs().is_empty());
    world.shutdown();
}

#[test]
fn a_silent_draining_endpoint_keeps_its_tab_until_the_generation_is_retired() {
    let mut world = World::rolled_over();
    let mut router = world.router();
    router.inventory(&scope()).unwrap();

    // The draining endpoint stops answering. That is uncertainty about one
    // generation, not an answer about its terminals.
    world.old.silent.store(true, Ordering::SeqCst);
    let partial = router.inventory(&scope()).unwrap();
    assert!(partial.is_partial());
    assert!(partial.answered().contains(&world.new.generation));
    assert_eq!(
        presence_of(&world.old.terminal, &partial, router.endpoints()),
        OwnerPresence::Reconnecting
    );

    // Retiring the generation is the verified absence that finally collects it.
    world
        .registry()
        .update(|document| document.transition(world.old.generation, GenerationRole::Retired))
        .unwrap();
    router.refresh().unwrap();
    assert!(router.endpoints().owner(world.old.generation).is_none());
    assert_eq!(
        presence_of(&world.old.terminal, &partial, router.endpoints()),
        OwnerPresence::Gone
    );
    assert_eq!(router.links().len(), 1, "the retired link is collected");

    // A request for the retired owner is refused, never re-aimed at the active
    // endpoint, so the new generation's terminal is untouched.
    let before = world.new.observed.actions().len();
    assert!(
        router
            .request(terminal_request(&TerminalRequest::Input {
                terminal: world.old.terminal.clone(),
                subscription: 1,
                input_seq: 1,
                input_operation: None,
                bytes: b"rm\n".to_vec(),
            }))
            .is_err()
    );
    assert_eq!(world.new.observed.actions().len(), before);
    world.shutdown();
}

#[test]
fn the_last_old_terminal_exit_reaches_the_projection_once() {
    let mut world = World::rolled_over();
    let mut router = world.router();
    assert_eq!(router.inventory(&scope()).unwrap().entries().len(), 2);

    world.old.exited.store(true, Ordering::SeqCst);
    let after_exit = router.inventory(&scope()).unwrap();
    assert!(
        !after_exit.is_partial(),
        "the owner answered authoritatively"
    );
    assert_eq!(
        presence_of(&world.old.terminal, &after_exit, router.endpoints()),
        OwnerPresence::Gone
    );
    assert_eq!(after_exit.entries().len(), 1);
    // Repeating the observation is the same projection, not a second exit.
    let repeated = router.inventory(&scope()).unwrap();
    assert_eq!(repeated.entries(), after_exit.entries());
    world.shutdown();
}

#[test]
fn a_forged_or_unknown_generation_cannot_name_an_endpoint() {
    let mut world = World::rolled_over();

    // A generation that is not in the registry has no endpoint at all.
    let mut router = world.router();
    assert!(
        router
            .request(terminal_request(&TerminalRequest::Attach {
                terminal: terminal(DaemonGeneration::new()),
            }))
            .is_err()
    );
    assert!(world.connects().is_empty());

    // A record that names a socket outside its own generation directory is
    // refused by the transport, not connected to.
    let forged = TrustedEndpoint {
        generation: world.old.generation,
        role: usagi_core::infrastructure::ipc::GenerationRole::Draining,
        endpoint: format!("generations/{}/sock", world.new.generation.as_str()),
    };
    let error = connect_generation(&world.data_dir, &forged).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    world.shutdown();
}

#[test]
fn a_rollover_is_refused_while_a_connected_client_cannot_route_by_owner() {
    let mut world = World::rolled_over();
    let ledger = RoutingLedger::new();
    let mut legacy = usagi_core::infrastructure::ipc::ClientHello {
        client_id: usagi_core::infrastructure::ipc::ClientId("legacy".into()),
        connection_nonce: "nonce".into(),
        expected_daemon_generation: None,
        supported_protocols: Vec::new(),
        capabilities: Vec::new(),
        required_capabilities: Vec::new(),
        build: artifact(),
        workspace: None,
    };
    ledger.admit(usagi_core::domain::id::ConnectionId::new(), &legacy);

    let snapshot = world.registry().load().unwrap();
    let revision = snapshot.document().revision;
    let successor = usagi_core::infrastructure::ipc::ServerHello {
        connection_nonce: "nonce".into(),
        connection_id: usagi_core::infrastructure::ipc::ConnectionId("connection".into()),
        daemon_generation: WireGeneration(world.new.generation.as_str()),
        generation_role: usagi_core::infrastructure::ipc::GenerationRole::Active,
        protocol: ProtocolVersion {
            generation: 1,
            revision: 2,
        },
        capabilities: vec![
            usagi_core::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY.into(),
        ],
        build: artifact(),
        limits: ProtocolLimits::default(),
        daemon_process: None,
    };
    assert_eq!(
        admit_rollover(&ledger, snapshot.document(), revision, &successor),
        Err(RolloverRefusal::ClientRoutingUnsupported { connections: 1 })
    );
    // Effect zero: both endpoints still serve, both children still run.
    assert_eq!(
        world.registry().load().unwrap().document().revision,
        revision
    );
    assert!(world.old.child.try_wait().unwrap().is_none());
    assert!(world.new.child.try_wait().unwrap().is_none());
    let mut router = world.router();
    assert_eq!(router.inventory(&scope()).unwrap().entries().len(), 2);

    // The same client on a build that routes by owner generation lifts it.
    legacy.capabilities =
        vec![usagi_core::infrastructure::ipc::OWNER_GENERATION_ROUTING_CAPABILITY.into()];
    let ledger = RoutingLedger::new();
    ledger.admit(usagi_core::domain::id::ConnectionId::new(), &legacy);
    assert_eq!(
        admit_rollover(&ledger, snapshot.document(), revision, &successor),
        Ok(())
    );
    world.shutdown();
}

#[test]
fn a_single_generation_daemon_routes_from_the_published_locator_alone() {
    let temp = TempDir::new_in("/tmp").unwrap();
    let data_dir = temp.path().to_path_buf();
    let mut only = Peer::start(&data_dir);
    usagi_daemon::infrastructure::unix_transport::publish_recovered_locator(
        &data_dir,
        &WireGeneration(only.generation.as_str()),
        &only.endpoint,
    )
    .unwrap();

    // No `generations.json` exists: a daemon that never rolled over is the one
    // published locator, and routing behaves exactly as it does today.
    let endpoints = TrustedGenerationDirectory::new(&data_dir)
        .snapshot()
        .unwrap();
    assert_eq!(endpoints.all().len(), 1);
    assert_eq!(endpoints.active().unwrap().generation, only.generation);

    let mut router = OwnerRouter::new(
        TrustedGenerationDirectory::new(&data_dir),
        Transport {
            data_dir: data_dir.clone(),
            connects: Arc::new(Mutex::new(Vec::new())),
        },
    );
    let merged = router.inventory(&scope()).unwrap();
    assert_eq!(merged.entries().len(), 1);
    router
        .request(terminal_request(&TerminalRequest::Attach {
            terminal: only.terminal.clone(),
        }))
        .unwrap();
    assert!(only.observed.actions().contains(&"Attach".to_owned()));
    only.shutdown();
}

#[test]
fn an_empty_data_directory_addresses_nothing_rather_than_guessing() {
    let temp = TempDir::new_in("/tmp").unwrap();
    let endpoints = TrustedGenerationDirectory::new(temp.path())
        .snapshot()
        .unwrap();
    assert!(endpoints.is_empty());
    assert!(endpoints.active().is_none());
}

/// Every way the durable records can fail to describe one authority. None of
/// them may produce an endpoint: a client with an untrustworthy directory routes
/// nothing rather than reusing the address it happens to remember.
#[test]
fn untrustworthy_records_produce_no_endpoint_at_all() {
    let private = std::fs::Permissions::from_mode(0o600);
    for (name, bytes) in [
        // A registry this build did not write.
        (
            "generations.json",
            serde_json::to_vec(&serde_json::json!({
                "schema": "usagi-generation-registry-v99",
                "revision": 1,
                "generations": [],
            }))
            .unwrap(),
        ),
        // A registry that is not a document at all.
        ("generations.json", b"{ not json".to_vec()),
        // A registry that is not even text.
        ("generations.json", vec![0xff, 0xfe, 0x00]),
        // No registry, and a locator that cannot be trusted.
        ("current.json", b"{ not json".to_vec()),
    ] {
        let temp = TempDir::new_in("/tmp").unwrap();
        let daemon = temp.path().join("daemon");
        std::fs::create_dir_all(&daemon).unwrap();
        std::fs::set_permissions(&daemon, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = daemon.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, private.clone()).unwrap();
        assert!(
            TrustedGenerationDirectory::new(temp.path())
                .snapshot()
                .is_err(),
            "{name} must not resolve to an endpoint"
        );
    }
}
