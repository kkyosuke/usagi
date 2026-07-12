//! 合成ルート。実 IO（標準出力・プロセス引数）をここで束ね、ロジックはすべて
//! crates/ 配下のライブラリクレート（テスト可能な層）に置く。
//!
//! 配布物は単一バイナリ `usagi` のまま、第 1 引数で面を選ぶ:
//! `usagi daemon` は daemon 面（usagi-daemon）、`usagi mcp` は入口面の MCP、
//! その他のサブコマンドは入口面の CLI（usagi-cli）、引数なしは TUI 面（usagi-tui）。
//! CLI が TUI 起動を要求した場合は、両クレートに依存できるこの合成ルートだけが
//! CLI の起動要求を TUI の初期画面へ変換する。

use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Utc;
use crossterm::cursor;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use fs2::FileExt;

use usagi_cli::cli::{RunOutcome, TuiRequest};
use usagi_core::domain::AppInfo;
use usagi_core::domain::recent::Recent;
use usagi_core::domain::workspace::Workspace;
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonRecordStore, InstanceLock, LivenessProbe, RecordFile, ShutdownSignal,
    Sleeper, Terminator,
};
use usagi_core::infrastructure::git::{GitOutput, GitRunner};
use usagi_core::infrastructure::paths;
use usagi_core::infrastructure::store::state::WorkspaceStateStore;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_core::usecase::client::{
    ClientError, ClientPolicy, DaemonClient, DaemonReply, IpcClient,
};
use usagi_core::usecase::workspace as workspace_usecase;
use usagi_daemon::infrastructure::unix_transport::SecureUnixListener;
use usagi_daemon::presentation::DaemonEnv;
use usagi_tui::presentation::views::config;
use usagi_tui::presentation::views::welcome::{self, Welcome};
use usagi_tui::presentation::views::workspace::{self, Workspace as WorkspaceView};
use usagi_tui::presentation::{
    self, BannerScreenRunner, Exit, Start, WorkspaceLoader, WorkspaceSnapshot,
};
use usagi_tui::usecase::application::{self, EntryScreen, Key, Terminal};
use usagi_tui::usecase::terminal_input::{KeyCode, LiveInput, RuntimeEvent};

mod tui_input;
use tui_input::{CrosstermSource, EventPump, NoBackend};

/// crossterm を backing にした実端末。TUI 面の [`Terminal`] ポートを合成ルートで実装し、
/// raw mode の描画（毎フレーム全消去して描き直す）とキー/リサイズイベントの読み取りを
/// [`Key`] 語彙へ翻訳する。ここが唯一の実端末 IO 層である。
struct CrosstermTerminal {
    out: std::io::Stdout,
    input: EventPump<CrosstermSource, NoBackend<()>>,
    input_started: Instant,
}

impl Terminal for CrosstermTerminal {
    fn size(&mut self) -> std::io::Result<(usize, usize)> {
        let (cols, rows) = terminal::size()?;
        Ok((rows as usize, cols as usize))
    }

    fn draw(&mut self, frame: &[String]) -> std::io::Result<()> {
        queue!(
            self.out,
            cursor::MoveTo(0, 0),
            terminal::Clear(terminal::ClearType::All)
        )?;
        // 行の「間」だけを `\r\n` で区切り、最終行の後には改行を出さない。フレームは端末の
        // 高さちょうどなので、最下行で改行するとその 1 行分だけ画面がスクロールしてしまう
        // （代替スクリーン内でも起きる）。raw mode では改行だけでは行頭へ戻らないため区切りは
        // `\r\n` を使う。
        for (i, line) in frame.iter().enumerate() {
            if i > 0 {
                write!(self.out, "\r\n")?;
            }
            write!(self.out, "{line}")?;
        }
        self.out.flush()
    }

    fn read_key(&mut self) -> std::io::Result<Key> {
        loop {
            match self.input.next(self.input_started.elapsed())? {
                RuntimeEvent::Input(LiveInput::Key(key)) => {
                    if key.modifiers.control && key.code == KeyCode::Char('c') {
                        return Ok(Key::Quit);
                    }
                    if !matches!(
                        key.kind,
                        usagi_tui::usecase::terminal_input::KeyEventKind::Press
                    ) {
                        return Ok(Key::Other);
                    }
                    return Ok(match key.code {
                        KeyCode::Up => Key::Up,
                        KeyCode::Down => Key::Down,
                        KeyCode::Left => Key::Left,
                        KeyCode::Right => Key::Right,
                        KeyCode::Enter => Key::Enter,
                        KeyCode::Backspace => Key::Backspace,
                        KeyCode::Escape => Key::Escape,
                        KeyCode::Char(ch) => Key::Char(ch),
                        _ => Key::Other,
                    });
                }
                // 現行の管理画面 loop は Key port のため、次フレームを要求する Other として
                // 表す。lossless な RuntimeEvent は Home controller 接続時にそのまま渡せる。
                RuntimeEvent::Resize { .. }
                | RuntimeEvent::Input(
                    LiveInput::Text(_) | LiveInput::Paste(_) | LiveInput::Raw(_),
                )
                | RuntimeEvent::Backend(()) => return Ok(Key::Other),
                // legacy Key port は tick を公開しないため、次の stream event を待つ。
                RuntimeEvent::Tick => {}
            }
        }
    }
}

/// core の `anyhow` error を合成ルートの `io::Result` 境界へ写す。
fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

/// `path` が実在するディレクトリか検証する。非 UTF-8 path も filesystem が扱える場合は
/// bytes のまま lookup し、Darwin の標準 filesystem などが `EILSEQ` 等で拒否した場合は
/// その失敗を伝播する。
fn validate_workspace_directory(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("workspace path is not a directory: {}", path.display()),
        ));
    }
    Ok(())
}

/// CLI から受け取った workspace path を、実在する絶対ディレクトリへ解決する。
/// 検証できない path を実在扱いするフォールバックは行わない。
fn resolve_workspace_path(path: &Path) -> std::io::Result<PathBuf> {
    let resolved = std::fs::canonicalize(path)?;
    validate_workspace_directory(&resolved)?;
    Ok(resolved)
}

/// 実 workspace registry と repo state を [`WorkspaceLoader`] port に接続する。
struct FsWorkspaceLoader {
    storage: Storage,
}

impl FsWorkspaceLoader {
    fn open_default() -> std::io::Result<Self> {
        Ok(Self {
            storage: Storage::open_default().map_err(io_error)?,
        })
    }
}

impl WorkspaceLoader for FsWorkspaceLoader {
    fn open(&mut self, path: &Path) -> std::io::Result<WorkspaceSnapshot> {
        // Open / Recent が渡す registry path は identity を保ったまま検証する。ここで再度
        // canonicalize すると、登録済みの absolute symlink path が別 workspace として
        // 二重登録されるためである。CLI の新規入力は dispatch 時点で解決済み。
        validate_workspace_directory(path)?;

        let workspace =
            workspace_usecase::open(&self.storage, path, Utc::now()).map_err(io_error)?;
        // repo-local state の破損は workspace を開く導線そのものを塞がない。空 state で画面を
        // 成立させ、registry の登録・touch は保持する。
        let state = WorkspaceStateStore::new(&workspace.path)
            .load()
            .unwrap_or_default()
            .unwrap_or_default();
        Ok(WorkspaceSnapshot::new(workspace, state))
    }
}

/// 開始画面が最初の描画に必要とする workspace data を読む。
///
/// Config は単独で成立するため、直接起動時の registry 読み込み失敗は空一覧へ縮退する。正常な
/// registry は先に読んでおき、Esc で戻った Welcome の Open / Recent 導線を保持する。
fn load_screen_graph_data(
    storage: &Storage,
    start: Start,
) -> std::io::Result<(Vec<Workspace>, Vec<Recent>)> {
    match start {
        Start::Welcome => Ok((
            storage.load_workspaces().map_err(io_error)?,
            workspace_usecase::recent(storage).map_err(io_error)?,
        )),
        Start::Config => Ok((
            storage.load_workspaces().unwrap_or_default(),
            workspace_usecase::recent(storage).unwrap_or_default(),
        )),
    }
}

/// raw mode + 代替スクリーンの lifetime を 1 回だけ所有し、その中で `run` が Welcome / Open /
/// Workspace を遷移する。結果がエラーでも端末状態は必ず復元する。
fn run_in_terminal(
    run: impl FnOnce(&mut CrosstermTerminal) -> std::io::Result<Exit>,
) -> std::io::Result<Exit> {
    enable_raw_mode()?;
    let mut setup = std::io::stdout();
    // 代替スクリーンに入り、さらにマウスレポートを有効化する。代替スクリーンは起動前の
    // スクロールバックを隠すが、それだけでは端末によってはホイールで元のバッファをスクロール
    // でき、背後の起動前コマンドが見えてしまう。マウスレポートを有効にするとホイールは端末では
    // なくアプリへ報告され（[`CrosstermTerminal::read_key`] が読み飛ばす）、TUI をスクロール
    // 不能にできる（v1 と同じ手法）。
    if let Err(error) = execute!(
        setup,
        EnterAlternateScreen,
        EnableMouseCapture,
        cursor::Hide
    ) {
        let _ = execute!(
            setup,
            cursor::Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        return Err(error);
    }

    let mut terminal = CrosstermTerminal {
        out: std::io::stdout(),
        input: EventPump::new(
            CrosstermSource,
            NoBackend::default(),
            Duration::from_millis(16),
            Duration::ZERO,
        ),
        input_started: Instant::now(),
    };
    let result = run(&mut terminal);

    // 描画の成否によらず端末を復元する（マウスレポートも忘れず戻す）。
    let mut teardown = std::io::stdout();
    let _ = execute!(
        teardown,
        cursor::Show,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();

    result
}

/// `start` から Welcome / New / Config / Open / Workspace の画面グラフを起動する。対話端末では
/// すべてを同じ raw mode + 代替スクリーン上で回し、非対話環境では開始画面を 1 フレーム描く。
fn launch_screen_graph(out: &mut dyn Write, start: Start) -> std::io::Result<()> {
    let now = Utc::now();
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let storage = Storage::open_default().map_err(io_error)?;
        let (workspaces, recent) = load_screen_graph_data(&storage, start)?;
        let mut loader = FsWorkspaceLoader { storage };
        run_in_terminal(|terminal| {
            presentation::run(terminal, workspaces, recent, now, start, &mut loader)
        })?;
    } else {
        // サイズ 0 は各 render 側で 80x24 にフォールバックされる。
        let frame = match start {
            Start::Welcome => {
                let storage = Storage::open_default().map_err(io_error)?;
                let recent = workspace_usecase::recent(&storage).map_err(io_error)?;
                welcome::render(0, 0, &Welcome::new(recent), now)
            }
            Start::Config => config::render(0, 0),
        };
        for line in frame {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

/// `path` の Workspace 画面を直接起動する。登録・touch・state 読み込みは対話 / 非対話の
/// どちらでも同じ loader を通す。
fn launch_workspace(out: &mut dyn Write, path: &Path) -> std::io::Result<()> {
    let mut loader = FsWorkspaceLoader::open_default()?;
    let snapshot = loader.open(path)?;
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_in_terminal(|terminal| presentation::run_workspace(terminal, snapshot))?;
    } else {
        let workspace = WorkspaceView::new(snapshot.workspace, snapshot.state);
        for line in workspace::render(0, 0, &workspace) {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

/// Bind the OS-specific daemon transport at the composition boundary.  The
/// protocol handler remains in `usagi-daemon`, while this root owns threads and
/// the real socket.  Each accepted peer is credential-checked by the listener
/// before a frame is decoded.
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
                        // One blocked peer cannot prevent accepts or another
                        // client's control response from being serviced.
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
                    // The listener is owned for the daemon lifetime. A credential
                    // failure has already closed its peer; retain the endpoint and
                    // continue serving later clients.
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        })
        .map(|_| ())
}

/// The real `daemon.json` file: reads and writes `<data-dir>/daemon/daemon.json`.
struct FsRecordFile {
    path: PathBuf,
}

impl RecordFile for FsRecordFile {
    fn read(&self) -> std::io::Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn write(&self, contents: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, contents)
    }

    fn remove(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }
}

/// Probes process liveness with signal 0, which performs the kernel's permission
/// and existence checks without delivering a signal.
struct KillProbe;

impl LivenessProbe for KillProbe {
    #[cfg(unix)]
    fn is_alive(&self, pid: u32) -> bool {
        libc::pid_t::try_from(pid).is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
    }

    #[cfg(not(unix))]
    fn is_alive(&self, _pid: u32) -> bool {
        false
    }
}

/// Terminates a process by sending it SIGTERM, asking it to shut down.
struct SigtermTerminator;

impl Terminator for SigtermTerminator {
    #[cfg(unix)]
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
    fn terminate(&self, _pid: u32) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "terminating a daemon is only supported on Unix",
        ))
    }
}

/// Blocks the foreground daemon until it receives SIGINT or SIGTERM.
struct SignalShutdown;

impl ShutdownSignal for SignalShutdown {
    #[cfg(unix)]
    fn wait(&self) -> std::io::Result<()> {
        // Block SIGINT / SIGTERM so they are delivered synchronously to sigwait
        // instead of taking their default terminate action; then wait for one.
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
    fn wait(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(
            "running the daemon is only supported on Unix",
        ))
    }
}

/// Launches `usagi daemon serve` as a detached background process. It joins its
/// own process group and discards its stdio, so it outlives this parent and the
/// controlling terminal.
struct ServeLauncher {
    exe: PathBuf,
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
}

/// Sleeps a short interval between `start`'s registration polls.
struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep(&self) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The single-instance lock: an exclusive `flock` on `<data-dir>/daemon/daemon.lock`
/// (following `store_lock`'s style). The locked file is retained for the process's
/// lifetime, so the OS releases the lock only when the daemon exits.
struct FileInstanceLock {
    path: PathBuf,
    held: RefCell<Option<std::fs::File>>,
}

impl InstanceLock for FileInstanceLock {
    fn acquire(&self) -> std::io::Result<bool> {
        // How long to wait for a departing holder (a restart hands off in
        // milliseconds) before concluding another daemon genuinely holds it.
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
                    // Retain the locked handle; dropping it would release the lock.
                    *self.held.borrow_mut() = Some(file);
                    return Ok(true);
                }
                Err(_) if Instant::now() < deadline => std::thread::sleep(POLL),
                Err(_) => return Ok(false),
            }
        }
    }
}

fn launch_tui(
    out: &mut dyn std::io::Write,
    info: &AppInfo,
    entry: &EntryScreen,
) -> std::io::Result<()> {
    match entry {
        EntryScreen::Welcome => launch_screen_graph(out, Start::Welcome),
        EntryScreen::Config => launch_screen_graph(out, Start::Config),
        EntryScreen::Workspace { path } => launch_workspace(out, path),
        EntryScreen::Doctor => {
            let mut runner = BannerScreenRunner::new(out, info);
            application::run(entry, &mut runner)
        }
    }
}

/// The real [`GitRunner`] seam: spawns `git -C <repo> <args>` and captures its
/// output. This is the one piece of real git IO, bound here at the synthesis
/// root (mirroring the daemon's process seams); `usagi-core`'s git operations and
/// `usagi update` stay pure over it.
struct SystemGit;

impl GitRunner for SystemGit {
    fn run(&self, repo: &std::path::Path, args: &[&str]) -> anyhow::Result<GitOutput> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()?;
        Ok(GitOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn dispatch_cli(
    args: Vec<std::ffi::OsString>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
    info: &AppInfo,
) -> std::io::Result<()> {
    match usagi_cli::cli::run(args, info.version, Box::new(SystemGit), out, err)? {
        RunOutcome::Exit(code) => std::process::exit(code),
        RunOutcome::LaunchTui(request) => {
            let entry = match request {
                TuiRequest::Welcome => EntryScreen::Welcome,
                TuiRequest::Workspace { path } => {
                    let path = match path {
                        Some(path) => path,
                        None => std::env::current_dir()?,
                    };
                    let path = resolve_workspace_path(&path)?;
                    EntryScreen::Workspace { path }
                }
                TuiRequest::Config => EntryScreen::Config,
                TuiRequest::Doctor => EntryScreen::Doctor,
            };
            launch_tui(out, info, &entry)
        }
        RunOutcome::DaemonRequest(request) => match daemon_client(ClientPolicy::cli()) {
            Ok(mut client) => match client.request(request) {
                Ok(DaemonReply::Accepted {
                    operation_id,
                    revision,
                }) => {
                    writeln!(
                        out,
                        "accepted operation {operation_id} (revision {revision})"
                    )
                }
                Ok(DaemonReply::Ok(value)) => writeln!(out, "{value}"),
                Err(error) => {
                    writeln!(err, "daemon request failed: {error}")?;
                    Ok(())
                }
            },
            Err(error) => {
                writeln!(err, "daemon unavailable: {error}")?;
                Ok(())
            }
        },
    }
}

/// Connect to the managed daemon, starting it once when no endpoint exists.
/// Any incompatible, unsafe, or unknown-ownership endpoint is returned as a
/// typed error: this boundary never creates a local managed PTY fallback.
fn daemon_client(
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

fn main() -> std::io::Result<()> {
    let info = AppInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    };
    let mut stdout = std::io::stdout();
    // `open` の path は UTF-8 とは限らないため、argv は初めから `OsString` で
    // 1 度だけ収集する。面の選択に必要なコマンド名だけ UTF-8 として照合する。
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    match args.get(1).and_then(|arg| arg.to_str()) {
        Some("daemon") => {
            let command = args.get(2).map(|arg| arg.to_string_lossy());
            let daemon_dir = paths::data_dir()
                .map_err(|err| std::io::Error::other(format!("{err:#}")))?
                .join("daemon");
            if matches!(command.as_deref(), None | Some("serve")) {
                let data_dir = daemon_dir
                    .parent()
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "daemon data path has no parent",
                        )
                    })?
                    .to_path_buf();
                spawn_ipc_server(&data_dir, &info)?;
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
            usagi_daemon::presentation::run(&mut stdout, command.as_deref(), &info, &env)
        }
        Some("mcp") => {
            let stdin = std::io::stdin();
            match daemon_client(ClientPolicy::mcp()) {
                Ok(mut client) => usagi_cli::mcp::serve_with_client(
                    stdin.lock(),
                    &mut stdout,
                    info.version,
                    &mut client,
                ),
                Err(error) => {
                    writeln!(std::io::stderr(), "daemon unavailable: {error}")?;
                    Ok(())
                }
            }
        }
        None if args.get(1).is_none() => launch_tui(&mut stdout, &info, &EntryScreen::Welcome),
        // その他のサブコマンド（UTF-8 でない名前も含む）は入口面の CLI へ。
        // clap がコマンドツリーを解析し、非 UTF-8 のコマンド名は不正値として報告する。
        // ハンドラが終了コードまたは TUI 起動要求を返す。実 stdout / stderr と
        // カレントディレクトリの解決はこの合成ルートで束ねる。
        _ => {
            let mut stderr = std::io::stderr();
            dispatch_cli(args, &mut stdout, &mut stderr, &info)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Start, load_screen_graph_data};
    use usagi_core::infrastructure::store::workspace::Storage;

    #[test]
    fn config_start_degrades_a_broken_workspace_registry() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("workspaces.json"), "{ broken").unwrap();
        let storage = Storage::new(home.path());

        let (workspaces, recent) = load_screen_graph_data(&storage, Start::Config).unwrap();

        assert!(workspaces.is_empty());
        assert!(recent.is_empty());
        assert!(load_screen_graph_data(&storage, Start::Welcome).is_err());
    }
}
