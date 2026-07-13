//! daemon 面へ Unix process / socket / signal を接続する composition adapter。

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;
use usagi_core::domain::AppInfo;
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonRecordStore, InstanceLock, LivenessProbe, RecordFile, ShutdownSignal,
    Sleeper, Terminator,
};
use usagi_core::infrastructure::paths;
use usagi_core::usecase::client::{ClientError, ClientPolicy, IpcClient};
use usagi_daemon::infrastructure::unix_transport::SecureUnixListener;
use usagi_daemon::presentation::DaemonEnv;

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
    std::thread::Builder::new()
        .name("usagi-ipc".to_string())
        .spawn(move || {
            loop {
                match listener.accept() {
                    Ok(stream) => {
                        let server = server.clone();
                        let _ = std::thread::Builder::new()
                            .name("usagi-ipc-client".to_string())
                            .spawn(move || {
                                let _ = stream.set_nonblocking(false);
                                let Ok(mut writer) = stream.try_clone() else {
                                    return;
                                };
                                let mut reader = stream;
                                let _ = usagi_daemon::presentation::ipc::handle_connection(
                                    &mut reader,
                                    &mut writer,
                                    &server,
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
