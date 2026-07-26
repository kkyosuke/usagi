//! Cross-process generation authority against real processes and real sockets.
//!
//! The deterministic state machine, handoff protocol, and admission barrier are
//! covered by unit tests. What only a real process pair can show is the part
//! that matters most in production:
//!
//! * a standby binds its **own** Unix socket and completes readiness without
//!   the active generation's `current.json` ever changing,
//! * a client connection opened *before* the handoff keeps its socket, and is
//!   refused a spawn afterwards while its owned terminal IO still works,
//! * a `SIGKILL` at each durable write boundary leaves a state that a fresh
//!   process reconciles into exactly one active generation and one locator.
//!
//! Each server is this test binary re-executed in a server role, so the two
//! daemons are genuinely separate processes with separate address spaces.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use usagi_core::domain::id::DaemonGeneration;
use usagi_core::infrastructure::ipc::{
    Bootstrap, ClientHello, ClientWorkspace, ConnectionId, DaemonGeneration as WireGeneration,
    Envelope, EnvelopeKind, ErrorCode, GenerationRole as WireRole, ProtocolLimits, ProtocolRange,
    RequestId, ResponseOutcome, ServerHello, ServerProtocol, build_identity, read_json_frame,
    write_json_frame,
};
use usagi_daemon::infrastructure::generation_registry::{
    CurrentLocatorFile, GenerationRegistryFile,
};
use usagi_daemon::infrastructure::unix_transport::{SecureUnixListener, read_locator};
use usagi_daemon::presentation::ipc::handshake;
use usagi_daemon::usecase::authority::admission::{
    AdmissionGate, LeaseClass, RequestClass, ResourceOwner,
};
use usagi_daemon::usecase::authority::handoff::{
    LocatorObservation, RecoveryOutcome, begin_handoff,
};
use usagi_daemon::usecase::authority::registry::{
    DEFAULT_GENERATION_LIMIT, GenerationRegistry, HandoffPhase, RegistryFile,
};
use usagi_daemon::usecase::authority::rollover::{
    CurrentLocator, HandoffStep, collect_retired, execute_rollover, execute_rollover_with, recover,
};
use usagi_daemon::usecase::authority::standby::{
    BUILD_ARTIFACT_CAPABILITY, GENERATION_HANDOFF_CAPABILITY, StandbyProbe, prepare_standby,
};
use usagi_daemon::usecase::authority::workers::{ClientWorkers, ConnectionShutdown};
use usagi_daemon::usecase::generation::{GenerationRole, ProcessIdentity, ProcessObservation};

const SERVER_ROLE: &str = "USAGI_AUTHORITY_SERVER";
const SERVER_DATA_DIR: &str = "USAGI_AUTHORITY_DATA_DIR";
const SERVER_GENERATION: &str = "USAGI_AUTHORITY_GENERATION";
const SERVER_ARTIFACT: &str = "USAGI_AUTHORITY_ARTIFACT";
const SERVER_CRASH_AT: &str = "USAGI_AUTHORITY_CRASH_AT";
const CRASH_STATUS: i32 = 91;
const POLL: Duration = Duration::from_millis(10);
const DEADLINE: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------- server side

/// A canonical artifact identity that differs per tag, so "same version,
/// different build" is expressible between the two processes.
fn artifact(tag: &str) -> usagi_core::infrastructure::ipc::BuildIdentity {
    let source = match tag {
        "next" => "a",
        "other" => "b",
        _ => "c",
    }
    .repeat(64);
    build_identity("2.6.0", "fixture", "test-target", "debug", &source)
}

fn registry_of(data_dir: &Path) -> GenerationRegistry {
    GenerationRegistry::new(
        GenerationRegistryFile::new(data_dir).unwrap(),
        DEFAULT_GENERATION_LIMIT,
    )
}

fn server_protocol(generation: DaemonGeneration, tag: &str) -> ServerProtocol {
    ServerProtocol {
        daemon_generation: WireGeneration(generation.as_str()),
        connection_id: ConnectionId(String::new()),
        generation_role: WireRole::Active,
        supported_protocols: vec![ProtocolRange {
            generation: 1,
            min_revision: 1,
            max_revision: 2,
        }],
        capabilities: vec![
            BUILD_ARTIFACT_CAPABILITY.to_owned(),
            GENERATION_HANDOFF_CAPABILITY.to_owned(),
        ],
        build: artifact(tag),
        limits: ProtocolLimits::default(),
        daemon_process: None,
        workspace_root: String::new(),
    }
}

/// The connection half a retiring generation uses to unblock a parked reader.
struct AcceptedConnection(UnixStream);

impl ConnectionShutdown for AcceptedConnection {
    fn shutdown(&self) -> io::Result<()> {
        match self.0.shutdown(std::net::Shutdown::Both) {
            // A peer that already went away leaves nothing to unblock, which is
            // the outcome the shutdown was for.
            Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(()),
            other => other,
        }
    }
}

/// One server process: it binds a private endpoint, follows the durable
/// registry for its role, and re-decides authority for every request.
#[allow(clippy::too_many_lines)] // One loop is the whole server; splitting it would hide the ordering.
fn run_server() {
    let data_dir = PathBuf::from(std::env::var(SERVER_DATA_DIR).unwrap());
    let generation = DaemonGeneration::parse(&std::env::var(SERVER_GENERATION).unwrap()).unwrap();
    let tag = std::env::var(SERVER_ARTIFACT).unwrap();
    let crash_at = std::env::var(SERVER_CRASH_AT).ok();

    let listener =
        SecureUnixListener::bind_private(&data_dir, WireGeneration(generation.as_str())).unwrap();
    let registry = registry_of(&data_dir);
    let locator = CurrentLocatorFile::new(&data_dir);
    let gate = AdmissionGate::new(generation, GenerationRole::Standby);
    let workers = ClientWorkers::new();
    let protocol = Arc::new(server_protocol(generation, &tag));
    let retire_flag = data_dir.join(format!("retire-{}", generation.as_str()));

    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        let snapshot = registry.load().unwrap();
        let document = snapshot.document();
        if document.role(generation) == Some(GenerationRole::Active)
            && gate.role() == GenerationRole::Standby
        {
            gate.activate().unwrap();
        }

        // A handoff naming this generation as its source is ours to drive: the
        // barrier can only be closed by the process that holds the leases.
        if let Some(handoff) = document.handoff.clone()
            && handoff.from == Some(generation)
            && handoff.phase == HandoffPhase::Preparing
            && gate.role() == GenerationRole::Active
        {
            let operation = handoff.operation.clone();
            execute_rollover_with(
                &registry,
                &locator,
                Some(&gate),
                &operation,
                handoff.from,
                handoff.to,
                &mut |step| {
                    if crash_at.as_deref() == Some(&format!("{step:?}")) {
                        // A durable-write boundary: die exactly here.
                        std::process::exit(CRASH_STATUS);
                    }
                    Ok(())
                },
            )
            .unwrap();
        }

        match listener.accept() {
            Ok(stream) => {
                stream.set_nonblocking(false).unwrap();
                let shutdown = AcceptedConnection(stream.try_clone().unwrap());
                let gate = gate.clone();
                let protocol = Arc::clone(&protocol);
                let handle = std::thread::spawn(move || serve_connection(stream, &protocol, &gate));
                workers.register(Box::new(shutdown), handle);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("accept failed: {error}"),
        }

        if retire_flag.exists() && gate.role() == GenerationRole::Draining {
            let report = collect_retired(&registry, &gate, &workers, generation).unwrap();
            assert!(report.is_clean(), "{report:?}");
            return;
        }
        std::thread::sleep(POLL);
    }
    panic!("server {generation} timed out");
}

/// Serve one client. Every request re-reads the live role, so a connection that
/// was admitted under `active` gains nothing once the role changes.
fn serve_connection(mut stream: UnixStream, protocol: &ServerProtocol, gate: &AdmissionGate) {
    let mut writer = stream.try_clone().unwrap();
    let Ok(Some(hello)) = handshake(&mut stream, &mut writer, protocol) else {
        return;
    };
    let limit = hello.limits.max_frame_bytes as usize;
    while let Ok(Some(envelope)) = read_json_frame::<Envelope>(&mut stream, limit) {
        let EnvelopeKind::Request {
            request_id, body, ..
        } = envelope.kind
        else {
            return;
        };
        let (class, owner) = match body.get("kind").and_then(serde_json::Value::as_str) {
            Some("spawn") => (RequestClass::Spawn, ResourceOwner::Unscoped),
            Some("terminal") => (RequestClass::TerminalIo, ResourceOwner::SelfGeneration),
            Some("foreign-terminal") => (RequestClass::TerminalIo, ResourceOwner::OtherGeneration),
            _ => (RequestClass::Read, ResourceOwner::Unscoped),
        };
        let outcome = match gate.admit(class, owner) {
            Ok(lease) => {
                // The lease is held across the whole effect, exactly as a real
                // spawn would hold it across its durable commit.
                drop(lease);
                ResponseOutcome::Ok
            }
            Err(refusal) => {
                ResponseOutcome::Error(usagi_core::infrastructure::ipc::ProtocolError::new(
                    ErrorCode::GenerationRolledOver,
                    refusal.to_string(),
                ))
            }
        };
        let reply = Envelope {
            protocol: hello.protocol,
            daemon_generation: hello.daemon_generation.clone(),
            kind: EnvelopeKind::Response {
                request_id,
                outcome,
                body: serde_json::Value::Null,
            },
        };
        if write_json_frame(&mut writer, &reply, limit).is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------- client side

struct Client {
    stream: UnixStream,
    hello: ServerHello,
}

impl Client {
    fn connect(endpoint: &Path) -> io::Result<Self> {
        let mut stream = UnixStream::connect(endpoint)?;
        let hello = ClientHello {
            client_id: usagi_core::infrastructure::ipc::ClientId("test-client".into()),
            connection_nonce: "nonce".into(),
            expected_daemon_generation: None,
            supported_protocols: vec![ProtocolRange {
                generation: 1,
                min_revision: 1,
                max_revision: 2,
            }],
            capabilities: Vec::new(),
            required_capabilities: Vec::new(),
            build: artifact("next"),
            workspace: Some(ClientWorkspace::Unbound),
        };
        write_json_frame(
            &mut stream,
            &Bootstrap::ClientHello(hello),
            usagi_core::infrastructure::ipc::DEFAULT_MAX_FRAME_BYTES,
        )?;
        let reply = read_json_frame::<Bootstrap>(
            &mut stream,
            usagi_core::infrastructure::ipc::DEFAULT_MAX_FRAME_BYTES,
        )?;
        match reply {
            Some(Bootstrap::ServerHello(hello)) => Ok(Self { stream, hello }),
            other => Err(io::Error::other(format!("unexpected reply: {other:?}"))),
        }
    }

    fn request(&mut self, kind: &str) -> ResponseOutcome {
        let limit = self.hello.limits.max_frame_bytes as usize;
        let envelope = Envelope {
            protocol: self.hello.protocol,
            daemon_generation: self.hello.daemon_generation.clone(),
            kind: EnvelopeKind::Request {
                request_id: RequestId(format!("request-{kind}")),
                timeout_ms: None,
                body: serde_json::json!({ "kind": kind }),
            },
        };
        write_json_frame(&mut self.stream, &envelope, limit).unwrap();
        let reply = read_json_frame::<Envelope>(&mut self.stream, limit)
            .unwrap()
            .expect("server replied");
        match reply.kind {
            EnvelopeKind::Response { outcome, .. } => outcome,
            other => panic!("unexpected reply: {other:?}"),
        }
    }
}

/// A standby probe over the real private endpoint: connect, hello, close.
struct SocketProbe {
    daemon: PathBuf,
}

impl StandbyProbe for SocketProbe {
    fn hello(&self, endpoint: &str) -> io::Result<ServerHello> {
        Client::connect(&self.daemon.join(endpoint)).map(|client| client.hello)
    }
}

// ------------------------------------------------------------------- fixtures

struct Server {
    child: Child,
    generation: DaemonGeneration,
    endpoint: String,
}

impl Server {
    fn start(data_dir: &Path, tag: &str, crash_at: Option<HandoffStep>) -> Self {
        let generation = DaemonGeneration::new();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "server_process_entry_point", "--nocapture"])
            .env(SERVER_ROLE, "1")
            .env(SERVER_DATA_DIR, data_dir)
            .env(SERVER_GENERATION, generation.as_str())
            .env(SERVER_ARTIFACT, tag);
        if let Some(step) = crash_at {
            command.env(SERVER_CRASH_AT, format!("{step:?}"));
        }
        let child = command.spawn().unwrap();
        let endpoint = format!("generations/{}/sock", generation.as_str());
        let socket = data_dir.join("daemon").join(&endpoint);
        await_until(|| socket.exists());
        Self {
            child,
            generation,
            endpoint,
        }
    }

    fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.child.id(),
            start_identity: format!("test-start-{}", self.child.id()),
            process_group: self.child.id(),
        }
    }

    fn socket(&self, data_dir: &Path) -> PathBuf {
        data_dir.join("daemon").join(&self.endpoint)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn await_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(POLL);
    }
    panic!("condition was not reached within {DEADLINE:?}");
}

/// A test-local liveness observer. It proves the *recorded* identity is the one
/// still running by pairing a live PID with the identity this fixture recorded
/// for it, which is what the production observer does with an OS process-start
/// token.
fn observer(alive: Vec<ProcessIdentity>) -> impl FnMut(&ProcessIdentity) -> ProcessObservation {
    move |process| {
        let running = alive.iter().any(|known| known == process)
            // SAFETY: signal 0 performs no action; it only reports whether the
            // caller may signal a live process with that PID.
            && unsafe { libc::kill(libc::pid_t::try_from(process.pid).unwrap_or(-1), 0) } == 0;
        if running {
            ProcessObservation::VerifiedAlive(process.clone())
        } else {
            ProcessObservation::Gone
        }
    }
}

/// Bring `server` up as the first active generation and publish its endpoint.
fn activate(data_dir: &Path, server: &Server, tag: &str) {
    let registry = registry_of(data_dir);
    let locator = CurrentLocatorFile::new(data_dir);
    let probe = SocketProbe {
        daemon: data_dir.join("daemon"),
    };
    prepare_standby(
        &registry,
        &probe,
        server.generation,
        &server.endpoint,
        &server.identity(),
        &artifact(tag),
    )
    .unwrap();
    execute_rollover(
        &registry,
        &locator,
        None,
        &usagi_core::infrastructure::ipc::OperationId("activate".into()),
        None,
        server.generation,
    )
    .unwrap();
    await_until(|| {
        read_locator(&data_dir.join("daemon"))
            .is_ok_and(|current| current.generation.0 == server.generation.as_str())
    });
}

// ---------------------------------------------------------------------- tests

/// The child process entry point. It is a `#[test]` only so the harness can
/// select it by name; the parent never runs it directly.
#[test]
fn server_process_entry_point() {
    if std::env::var_os(SERVER_ROLE).is_none() {
        return;
    }
    run_server();
}

#[test]
#[allow(clippy::too_many_lines)] // One scenario covers standby, handoff, and drain across two processes.
fn a_standby_takes_authority_without_the_old_connection_keeping_its_privileges() {
    let temp = TempDir::new_in("/tmp").unwrap();
    let data_dir = temp.path();
    let daemon = data_dir.join("daemon");
    let registry = registry_of(data_dir);

    let mut old = Server::start(data_dir, "old", None);
    activate(data_dir, &old, "old");

    // A client connects while the old generation is active and keeps its
    // socket open across the whole handoff.
    let mut persistent = Client::connect(&old.socket(data_dir)).unwrap();
    assert_eq!(persistent.request("spawn"), ResponseOutcome::Ok);

    // The standby binds its own socket. Readiness must not move `current`.
    let next = Server::start(data_dir, "next", None);
    assert_ne!(next.socket(data_dir), old.socket(data_dir));
    let probe = SocketProbe {
        daemon: daemon.clone(),
    };
    prepare_standby(
        &registry,
        &probe,
        next.generation,
        &next.endpoint,
        &next.identity(),
        &artifact("next"),
    )
    .unwrap();
    assert_eq!(
        read_locator(&daemon).unwrap().generation.0,
        old.generation.as_str()
    );
    assert!(
        registry
            .load()
            .unwrap()
            .document()
            .entry(next.generation)
            .unwrap()
            .is_build_verified()
    );
    // The standby serves no mutation of its own before it is named active.
    let mut standby_client = Client::connect(&next.socket(data_dir)).unwrap();
    assert!(matches!(
        standby_client.request("spawn"),
        ResponseOutcome::Error(_)
    ));

    // Record the intent; the active process drives its own barrier and commit.
    let operation = usagi_core::infrastructure::ipc::OperationId("handoff-1".into());
    registry
        .update(|document| {
            begin_handoff(document, &operation, Some(old.generation), next.generation).map(|_| ())
        })
        .unwrap();
    await_until(|| {
        registry.load().is_ok_and(|snapshot| {
            snapshot.document().current == Some(next.generation)
                && snapshot.document().handoff.is_none()
        })
    });
    assert_eq!(
        read_locator(&daemon).unwrap().generation.0,
        next.generation.as_str()
    );

    let document = registry.load().unwrap();
    let document = document.document();
    assert_eq!(document.current, Some(next.generation));
    assert_eq!(
        document.role(old.generation),
        Some(GenerationRole::Draining)
    );
    assert!(document.handoff.is_none());

    // The connection that predates the handoff is still open on the old
    // socket, and is now refused control work on every request.
    match persistent.request("spawn") {
        ResponseOutcome::Error(error) => assert_eq!(error.code, ErrorCode::GenerationRolledOver),
        other => panic!("late spawn was admitted: {other:?}"),
    }
    // Its own terminals keep working; another owner's never did.
    assert_eq!(persistent.request("terminal"), ResponseOutcome::Ok);
    assert!(matches!(
        persistent.request("foreign-terminal"),
        ResponseOutcome::Error(_)
    ));

    // The new authority serves control work on its own endpoint. Its gate is
    // opened by its *own* poll loop observing the committed registry, which is
    // a separate process from the one that committed it — so this waits for
    // that observation rather than assuming it already happened. The refusal
    // before the commit is asserted above, so waiting here cannot hide a
    // generation that admits control work too early.
    await_until(|| standby_client.request("spawn") == ResponseOutcome::Ok);

    // Collection unblocks the parked worker of the still-open connection and
    // joins it before the registry records the retirement.
    std::fs::write(
        data_dir.join(format!("retire-{}", old.generation.as_str())),
        b"",
    )
    .unwrap();
    await_until(|| {
        registry.load().unwrap().document().role(old.generation) == Some(GenerationRole::Retired)
    });
    assert!(old.child.wait().unwrap().success());
    // The retiring generation unblocked the parked reader and joined it before
    // exiting, so the client's stream is at end of file rather than hung.
    let mut drained = Vec::new();
    persistent.stream.read_to_end(&mut drained).unwrap();
    assert!(drained.is_empty());
}

#[test]
fn a_standby_advertising_a_different_artifact_never_becomes_current() {
    let temp = TempDir::new_in("/tmp").unwrap();
    let data_dir = temp.path();
    let daemon = data_dir.join("daemon");
    let registry = registry_of(data_dir);

    let old = Server::start(data_dir, "old", None);
    activate(data_dir, &old, "old");

    // The replacement was admitted for `next` but the process that answers its
    // endpoint was built from a different source tree at the same version.
    let impostor = Server::start(data_dir, "other", None);
    let probe = SocketProbe {
        daemon: daemon.clone(),
    };
    let failure = prepare_standby(
        &registry,
        &probe,
        impostor.generation,
        &impostor.endpoint,
        &impostor.identity(),
        &artifact("next"),
    )
    .unwrap_err();
    assert!(failure.to_string().contains("does not match"), "{failure}");

    let document = registry.load().unwrap();
    let document = document.document();
    assert_eq!(document.current, Some(old.generation));
    assert!(
        !document
            .entry(impostor.generation)
            .unwrap()
            .is_build_verified()
    );
    assert_eq!(
        read_locator(&daemon).unwrap().generation.0,
        old.generation.as_str()
    );

    // An unverified standby can never be handed authority.
    let operation = usagi_core::infrastructure::ipc::OperationId("handoff-mismatch".into());
    assert!(
        registry
            .update(|document| begin_handoff(
                document,
                &operation,
                Some(old.generation),
                impostor.generation
            )
            .map(|_| ()))
            .is_err()
    );
    assert_eq!(
        read_locator(&daemon).unwrap().generation.0,
        old.generation.as_str()
    );
}

#[test]
fn a_sigkill_at_each_durable_boundary_recovers_to_exactly_one_authority() {
    for step in [
        HandoffStep::BeforeIntent,
        HandoffStep::AfterIntent,
        HandoffStep::AfterBarrier,
        HandoffStep::BeforeRegistryCommit,
        HandoffStep::AfterRegistryCommit,
        HandoffStep::BeforeLocatorPublish,
        HandoffStep::AfterLocatorPublish,
        HandoffStep::BeforeComplete,
    ] {
        let temp = TempDir::new_in("/tmp").unwrap();
        let data_dir = temp.path();
        let daemon = data_dir.join("daemon");
        let registry = registry_of(data_dir);

        let mut old = Server::start(data_dir, "old", Some(step));
        activate(data_dir, &old, "old");
        let next = Server::start(data_dir, "next", None);
        let probe = SocketProbe {
            daemon: daemon.clone(),
        };
        prepare_standby(
            &registry,
            &probe,
            next.generation,
            &next.endpoint,
            &next.identity(),
            &artifact("next"),
        )
        .unwrap();

        let operation = usagi_core::infrastructure::ipc::OperationId("crash-handoff".into());
        registry
            .update(|document| {
                begin_handoff(document, &operation, Some(old.generation), next.generation)
                    .map(|_| ())
            })
            .unwrap();

        // The active process dies exactly at its configured write boundary.
        let status = old.child.wait().unwrap();
        assert_eq!(status.code(), Some(CRASH_STATUS), "{step:?}");

        // A fresh process reconciles both durable objects. The successor is
        // still alive, the crashed predecessor is not.
        let locator = CurrentLocatorFile::new(data_dir);
        let outcome = recover(&registry, &locator, &mut observer(vec![next.identity()])).unwrap();
        let document = registry.load().unwrap();
        let document = document.document();
        document.validate(DEFAULT_GENERATION_LIMIT).unwrap();
        assert!(document.handoff.is_none(), "{step:?}");

        let committed = matches!(
            step,
            HandoffStep::AfterRegistryCommit
                | HandoffStep::BeforeLocatorPublish
                | HandoffStep::AfterLocatorPublish
                | HandoffStep::BeforeComplete
        );
        if committed {
            // An observable commit is rolled forward, never rolled back.
            assert_eq!(
                outcome,
                RecoveryOutcome::RolledForward(operation.clone()),
                "{step:?}"
            );
            assert_eq!(document.current, Some(next.generation), "{step:?}");
            assert_eq!(
                locator.read().unwrap(),
                LocatorObservation::Published(
                    usagi_daemon::usecase::authority::handoff::PublishedLocator {
                        generation: next.generation,
                        endpoint: next.endpoint.clone(),
                    }
                ),
                "{step:?}"
            );
        } else {
            // Nothing was observable, and the only owner that could have
            // served it is gone: fail closed rather than resurrect it.
            assert!(
                matches!(outcome, RecoveryOutcome::FailedClosed(_)),
                "{step:?}: {outcome:?}"
            );
            assert_eq!(document.current, None, "{step:?}");
            assert_eq!(
                locator.read().unwrap(),
                LocatorObservation::Absent,
                "{step:?}"
            );
        }

        // Replaying recovery converges on the same state.
        assert_eq!(
            recover(&registry, &locator, &mut observer(vec![next.identity()])).unwrap(),
            RecoveryOutcome::Consistent,
            "{step:?}"
        );
    }
}

#[test]
fn the_registry_document_is_a_byte_exact_compare_and_swap_across_processes() {
    let temp = TempDir::new_in("/tmp").unwrap();
    let file = GenerationRegistryFile::new(temp.path()).unwrap();
    assert!(file.read().unwrap().is_none());
    assert!(file.compare_and_write(None, "first").unwrap());
    assert_eq!(file.read().unwrap().as_deref(), Some("first"));
    // A writer holding older bytes loses instead of overwriting.
    assert!(!file.compare_and_write(None, "second").unwrap());
    assert!(!file.compare_and_write(Some("stale"), "second").unwrap());
    assert!(file.compare_and_write(Some("first"), "second").unwrap());
    assert_eq!(file.read().unwrap().as_deref(), Some("second"));

    // The temporary is never left behind and the document stays private.
    let published = temp.path().join("daemon").join("generations.json");
    let mode = std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&published).unwrap());
    assert_eq!(mode & 0o777, 0o600);
    let residue: Vec<_> = std::fs::read_dir(temp.path().join("daemon"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".generations.json.tmp.")
        })
        .collect();
    assert!(residue.is_empty(), "{residue:?}");
}

#[test]
fn a_lease_taken_before_a_barrier_is_what_the_barrier_waits_on() {
    // The cross-process barrier is exercised by the handoff test above; this
    // pins the local invariant the server loop depends on: a request admitted
    // as control work keeps the generation active until it finishes.
    let gate = AdmissionGate::new(DaemonGeneration::new(), GenerationRole::Active);
    let lease = gate
        .admit(RequestClass::Spawn, ResourceOwner::Unscoped)
        .unwrap()
        .unwrap();
    gate.close(LeaseClass::ActiveControl);
    let entered = Arc::new(AtomicBool::new(false));
    let waiter = {
        let gate = gate.clone();
        let entered = Arc::clone(&entered);
        std::thread::spawn(move || {
            gate.await_drain(LeaseClass::ActiveControl).unwrap();
            entered.store(true, Ordering::Release);
        })
    };
    assert!(!entered.load(Ordering::Acquire));
    drop(lease);
    waiter.join().unwrap();
    assert!(entered.load(Ordering::Acquire));
}
