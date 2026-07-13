//! daemon 面へ Unix process / socket / signal を接続する composition adapter。

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fs2::FileExt;
use usagi_core::domain::AppInfo;
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonRecordStore, InstanceLock, LivenessProbe, RecordFile, ShutdownSignal,
    Sleeper, Terminator,
};
use usagi_core::infrastructure::paths;
use usagi_core::usecase::client::{ClientError, ClientPolicy, IpcClient};
use usagi_daemon::infrastructure::pty::PtyTerminal;
use usagi_daemon::infrastructure::unix_transport::SecureUnixListener;
use usagi_daemon::presentation::DaemonEnv;
use usagi_daemon::usecase::generation::ProcessIdentity;
use usagi_daemon::usecase::generic_terminal::{
    GenericPtySpawner, TerminalProfileResolver, TerminalStore, TerminalStoreSnapshot,
};
use usagi_daemon::usecase::session_runtime::{SessionRuntime, SystemGit};
use usagi_daemon::usecase::terminal::{Geometry, PtyWriteError, PtyWriter, SpawnFailure};
use usagi_daemon::usecase::terminal_ipc::GenericTerminalRuntime;

struct TrustedLoginShell {
    repository_root: PathBuf,
}

impl TerminalProfileResolver for TrustedLoginShell {
    fn resolve(
        &mut self,
        request: &usagi_core::domain::terminal_launch::TerminalLaunchRequest,
    ) -> Result<
        usagi_core::domain::terminal_launch::ResolvedTerminalLaunch,
        usagi_core::domain::terminal_launch::TerminalLaunchValidationError,
    > {
        use usagi_core::domain::terminal_launch::{
            DurableTerminalLaunchSnapshot, ResolvedTerminalLaunch, TerminalLaunchValidationError,
            TerminalProfileId,
        };
        let login_shell = TerminalProfileId::new("login-shell").expect("static profile is valid");
        if request.profile_id != login_shell {
            return Err(TerminalLaunchValidationError::UnknownProfile {
                profile_id: request.profile_id.clone(),
            });
        }
        ResolvedTerminalLaunch::new(
            DurableTerminalLaunchSnapshot::new(
                request.clone(),
                1,
                "/bin/sh",
                self.repository_root.clone(),
                BTreeSet::new(),
            )?,
            BTreeMap::new(),
        )
    }
}

struct FileTerminalStore(PathBuf);
impl TerminalStore for FileTerminalStore {
    type Error = std::io::Error;
    fn save(&mut self, snapshot: TerminalStoreSnapshot) -> Result<(), Self::Error> {
        let encoded = serde_json::to_vec(&snapshot).map_err(std::io::Error::other)?;
        let temporary = self.0.with_extension("json.tmp");
        std::fs::write(&temporary, encoded)?;
        std::fs::rename(temporary, &self.0)
    }
}

enum PtyObservation {
    Output(usagi_core::domain::id::TerminalRef, Vec<u8>),
}

struct DaemonPty {
    terminals: BTreeMap<String, Arc<Mutex<PtyTerminal>>>,
    selected: Option<String>,
    observations: Sender<PtyObservation>,
}
impl DaemonPty {
    fn new() -> (Self, Receiver<PtyObservation>) {
        let (observations, receiver) = mpsc::channel();
        (
            Self {
                terminals: BTreeMap::new(),
                selected: None,
                observations,
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
    ) -> Result<ProcessIdentity, SpawnFailure> {
        let pty = PtyTerminal::spawn(
            &launch.snapshot.program,
            &launch.snapshot.working_directory,
            Geometry { cols: 80, rows: 24 },
        )
        .map_err(|_| SpawnFailure::Definite)?;
        let pid = pty.process_id().ok_or(SpawnFailure::Ambiguous)?;
        let reader = pty.reader().map_err(|_| SpawnFailure::Ambiguous)?;
        let pty = Arc::new(Mutex::new(pty));
        self.terminals
            .insert(terminal.terminal_id.as_str().clone(), Arc::clone(&pty));
        let output_sender = self.observations.clone();
        let output_terminal = terminal.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut bytes = [0_u8; 4096];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                if output_sender
                    .send(PtyObservation::Output(
                        output_terminal.clone(),
                        bytes[..count].to_vec(),
                    ))
                    .is_err()
                {
                    break;
                }
            }
        });
        Ok(ProcessIdentity {
            pid,
            start_identity: "daemon-owned-pty".to_owned(),
            process_group: pid,
        })
    }
}
impl PtyWriter for DaemonPty {
    fn select_terminal(&mut self, terminal: &usagi_core::domain::id::TerminalRef) {
        self.selected = Some(terminal.terminal_id.as_str().clone());
    }
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), PtyWriteError> {
        let Some(key) = self.selected.as_ref() else {
            return Err(PtyWriteError { applied_prefix: 0 });
        };
        let Some(terminal) = self.terminals.get(key) else {
            return Err(PtyWriteError { applied_prefix: 0 });
        };
        terminal
            .lock()
            .map_err(|_| PtyWriteError { applied_prefix: 0 })?
            .write_all(bytes)
    }
}

struct SharedTerminal(
    Arc<Mutex<GenericTerminalRuntime<TrustedLoginShell, FileTerminalStore, DaemonPty>>>,
);
impl usagi_daemon::presentation::ipc::TerminalOwner for SharedTerminal {
    fn request(
        &mut self,
        connection: usagi_core::domain::id::ConnectionId,
        client: usagi_core::domain::id::ClientId,
        request_id: usagi_core::domain::id::RequestId,
        action: usagi_core::usecase::client::TerminalAction,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, usagi_core::infrastructure::ipc::ProtocolError> {
        self.0
            .lock()
            .map_err(|_| {
                usagi_core::infrastructure::ipc::ProtocolError::new(
                    usagi_core::infrastructure::ipc::ErrorCode::Unavailable,
                    "terminal owner is unavailable",
                )
            })?
            .request(connection, client, request_id, action, payload)
    }
    fn disconnect(&mut self, connection: usagi_core::domain::id::ConnectionId) {
        if let Ok(mut terminal) = self.0.lock() {
            terminal.disconnect(connection);
        }
    }
}

#[coverage(off)]
fn spawn_ipc_server(data_dir: &Path, info: &AppInfo) -> std::io::Result<()> {
    let generation = usagi_core::infrastructure::ipc::DaemonGeneration(
        usagi_core::domain::id::DaemonGeneration::new()
            .as_str()
            .clone(),
    );
    let listener = SecureUnixListener::bind(data_dir, generation.clone())?;
    let server = usagi_daemon::presentation::ipc::server_protocol(
        generation.clone(),
        generation.0.clone(),
        usagi_core::infrastructure::ipc::BuildIdentity {
            version: info.version.to_owned(),
            commit: "unknown".to_owned(),
            target: std::env::consts::ARCH.to_owned(),
        },
    );
    let repo_root = std::env::current_dir()?;
    let runtime = Arc::new(Mutex::new(
        SessionRuntime::open(
            repo_root.clone(),
            usagi_core::domain::id::DaemonGeneration::parse(&generation.0)
                .map_err(|error| std::io::Error::other(error.to_string()))?,
            SystemGit,
        )
        .map_err(|error| std::io::Error::other(error.safe_message()))?,
    ));
    let (pty, observations) = DaemonPty::new();
    let terminal = Arc::new(Mutex::new(GenericTerminalRuntime::new(
        usagi_core::domain::id::DaemonGeneration::parse(&generation.0)
            .map_err(|error| std::io::Error::other(error.to_string()))?,
        TrustedLoginShell {
            repository_root: repo_root,
        },
        FileTerminalStore(data_dir.join("daemon").join("terminals.json")),
        pty,
    )));
    let observed_terminal = Arc::clone(&terminal);
    std::thread::Builder::new()
        .name("usagi-terminal-observer".to_string())
        .spawn(move || {
            while let Ok(observation) = observations.recv() {
                let Ok(mut terminal) = observed_terminal.lock() else {
                    break;
                };
                match observation {
                    PtyObservation::Output(reference, bytes) => {
                        let _ = terminal.output(&reference, bytes);
                    }
                }
            }
        })?;
    std::thread::Builder::new()
        .name("usagi-ipc".to_string())
        .spawn(move || {
            loop {
                match listener.accept() {
                    Ok(stream) => {
                        let server = server.clone();
                        let runtime = Arc::clone(&runtime);
                        let terminal = Arc::clone(&terminal);
                        let _ = std::thread::Builder::new()
                            .name("usagi-ipc-client".to_string())
                            .spawn(move || {
                                let _ = stream.set_nonblocking(false);
                                let Ok(mut writer) = stream.try_clone() else {
                                    return;
                                };
                                let mut reader = stream;
                                let mut terminal = SharedTerminal(terminal);
                                let _ = usagi_daemon::presentation::ipc::handle_connection_with_terminal_and(
                                    &mut reader,
                                    &mut writer,
                                    &server,
                                    &mut terminal,
                                    |request_id, body, hello| {
                                        let request = body.get("kind").and_then(serde_json::Value::as_str).filter(|kind| *kind == "session").and_then(|_| serde_json::from_value::<usagi_core::usecase::client::DaemonRequest>(body.clone()).ok()).and_then(|request| match request {
                                            usagi_core::usecase::client::DaemonRequest::Session { action, operation_id, payload } => Some((action, operation_id, payload)),
                                            _ => None,
                                        });
                                        let Some((action, operation_id, payload)) = request else {
                                            return usagi_daemon::presentation::ipc::dispatch(request_id, body, hello);
                                        };
                                            let result = runtime.lock().map_err(|_| ()).and_then(|mut runtime| runtime.handle(action, &operation_id, &payload).map_err(|_| ()));
                                            match result {
                                                Ok(reply) => usagi_core::infrastructure::ipc::Envelope { protocol: hello.protocol, daemon_generation: hello.daemon_generation.clone(), kind: usagi_core::infrastructure::ipc::EnvelopeKind::Response { request_id, outcome: usagi_core::infrastructure::ipc::ResponseOutcome::Ok, body: reply.body } },
                                                Err(()) => usagi_core::infrastructure::ipc::Envelope { protocol: hello.protocol, daemon_generation: hello.daemon_generation.clone(), kind: usagi_core::infrastructure::ipc::EnvelopeKind::Response { request_id, outcome: usagi_core::infrastructure::ipc::ResponseOutcome::Error(usagi_core::infrastructure::ipc::ProtocolError::new(usagi_core::infrastructure::ipc::ErrorCode::InvalidArgument, "session request was rejected")), body: serde_json::json!(null) } },
                                            }
                                    }
                                    ,
                                );
                            });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        })
        .map(|_| ())
}

struct FsRecordFile {
    path: PathBuf,
}

impl RecordFile for FsRecordFile {
    #[coverage(off)]
    fn read(&self) -> std::io::Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
    #[coverage(off)]
    fn write(&self, contents: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, contents)
    }
    #[coverage(off)]
    fn remove(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

struct KillProbe;
impl LivenessProbe for KillProbe {
    #[cfg(unix)]
    #[coverage(off)]
    fn is_alive(&self, pid: u32) -> bool {
        libc::pid_t::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
    }
    #[cfg(not(unix))]
    #[coverage(off)]
    fn is_alive(&self, _pid: u32) -> bool {
        false
    }
}

struct SigtermTerminator;
impl Terminator for SigtermTerminator {
    #[cfg(unix)]
    #[coverage(off)]
    fn terminate(&self, pid: u32) -> std::io::Result<()> {
        let pid =
            libc::pid_t::try_from(pid).map_err(|_| std::io::Error::other("pid out of range"))?;
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    #[coverage(off)]
    fn terminate(&self, _pid: u32) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "terminating a daemon is only supported on Unix",
        ))
    }
}

struct SignalShutdown;
impl ShutdownSignal for SignalShutdown {
    #[cfg(unix)]
    #[coverage(off)]
    fn wait(&self) -> std::io::Result<()> {
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, libc::SIGINT);
            libc::sigaddset(&raw mut set, libc::SIGTERM);
            if libc::sigprocmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut received: libc::c_int = 0;
            if libc::sigwait(&raw const set, &raw mut received) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    #[coverage(off)]
    fn wait(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "running the daemon is only supported on Unix",
        ))
    }
}

struct ServeLauncher {
    exe: PathBuf,
}
impl DaemonLauncher for ServeLauncher {
    #[coverage(off)]
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
}

struct RealSleeper;
impl Sleeper for RealSleeper {
    #[coverage(off)]
    fn sleep(&self) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct FileInstanceLock {
    path: PathBuf,
    held: RefCell<Option<std::fs::File>>,
}
impl InstanceLock for FileInstanceLock {
    #[coverage(off)]
    fn acquire(&self) -> std::io::Result<bool> {
        const TIMEOUT: Duration = Duration::from_secs(2);
        const POLL: Duration = Duration::from_millis(20);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.path)?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => {
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
#[coverage(off)]
pub(crate) fn run<W: Write>(
    out: &mut W,
    command: Option<&str>,
    info: &AppInfo,
) -> std::io::Result<()> {
    let daemon_dir = paths::data_dir()
        .map_err(|err| std::io::Error::other(format!("{err:#}")))?
        .join("daemon");
    if matches!(command, None | Some("serve")) {
        let data_dir = daemon_dir
            .parent()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "daemon data path has no parent",
                )
            })?
            .to_path_buf();
        spawn_ipc_server(&data_dir, info)?;
    }
    let store = DaemonRecordStore::new(FsRecordFile {
        path: daemon_dir.join("daemon.json"),
    });
    let launcher = ServeLauncher {
        exe: std::env::current_exe()?,
    };
    let lock = FileInstanceLock {
        path: daemon_dir.join("daemon.lock"),
        held: RefCell::new(None),
    };
    let env = DaemonEnv {
        store: &store,
        probe: &KillProbe,
        terminator: &SigtermTerminator,
        shutdown: &SignalShutdown,
        launcher: &launcher,
        sleeper: &RealSleeper,
        lock: &lock,
        pid: std::process::id(),
    };
    usagi_daemon::presentation::run(out, command, info, &env)
}

/// 管理 daemon へ接続し、endpoint がないときだけ一度起動を要求する。
#[coverage(off)]
pub(crate) fn client(
    policy: ClientPolicy,
) -> Result<IpcClient<std::os::unix::net::UnixStream>, ClientError> {
    let data_dir =
        paths::data_dir().map_err(|error| ClientError::Unavailable(error.to_string()))?;
    let connect = || usagi_daemon::infrastructure::unix_transport::connect_current(&data_dir);
    let stream = match connect() {
        Ok(stream) => stream,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::process::Command::new(
                std::env::current_exe().map_err(|e| ClientError::Unavailable(e.to_string()))?,
            )
            .args(["daemon", "start"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| ClientError::Unavailable(e.to_string()))?;
            let mut connected = None;
            for _ in 0..20 {
                match connect() {
                    Ok(stream) => {
                        connected = Some(stream);
                        break;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
            connected.ok_or_else(|| {
                ClientError::Unavailable("daemon did not publish an endpoint".into())
            })?
        }
        Err(error) => return Err(ClientError::Unavailable(error.to_string())),
    };
    IpcClient::connect(
        stream,
        format!("cli-{}", std::process::id()),
        format!("{}", std::process::id()),
        policy,
    )
}
