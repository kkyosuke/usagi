//! 合成ルート。実 IO（標準出力・プロセス引数）をここで束ね、ロジックはすべて
//! crates/ 配下のライブラリクレート（テスト可能な層）に置く。
//!
//! 配布物は単一バイナリ `usagi` のまま、第 1 引数で面を選ぶ:
//! `usagi daemon` は daemon 面（usagi-daemon）、`usagi mcp` は入口面の MCP、
//! その他のサブコマンドは入口面の CLI（usagi-cli）、引数なしは TUI 面（usagi-tui）。
//! CLI が TUI 起動を要求した場合は、両クレートに依存できるこの合成ルートだけが
//! CLI の起動要求を TUI の初期画面へ変換する。

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};

use usagi_cli::cli::{RunOutcome, TuiRequest};
use usagi_core::domain::AppInfo;
use usagi_core::domain::workspace::Workspace;
use usagi_core::infrastructure::daemon::{
    DaemonLauncher, DaemonRecordStore, LivenessProbe, RecordFile, ShutdownSignal, Sleeper,
    Terminator,
};
use usagi_core::infrastructure::paths;
use usagi_core::infrastructure::store::workspace::Storage;
use usagi_daemon::presentation::DaemonEnv;
use usagi_tui::presentation::views::welcome::{self, Welcome};
use usagi_tui::presentation::{self, BannerScreenRunner, Exit};
use usagi_tui::usecase::application::{self, EntryScreen, Key, Terminal};

/// crossterm を backing にした実端末。TUI 面の [`Terminal`] ポートを合成ルートで実装し、
/// raw mode の描画（毎フレーム全消去して描き直す）とキー/リサイズイベントの読み取りを
/// [`Key`] 語彙へ翻訳する。ここが唯一の実端末 IO 層である。
struct CrosstermTerminal {
    out: std::io::Stdout,
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
        for line in frame {
            // raw mode では改行だけでは行頭へ戻らないため `\r\n` で送る。
            write!(self.out, "{line}\r\n")?;
        }
        self.out.flush()
    }

    fn read_key(&mut self) -> std::io::Result<Key> {
        loop {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        return Ok(Key::Quit);
                    }
                    return Ok(match key.code {
                        KeyCode::Up => Key::Up,
                        KeyCode::Down => Key::Down,
                        KeyCode::Enter => Key::Enter,
                        KeyCode::Esc => Key::Escape,
                        KeyCode::Char(ch) => Key::Char(ch),
                        _ => Key::Other,
                    });
                }
                // リサイズは次フレームで描き直せばよいので Other として抜ける。
                Event::Resize(_, _) => return Ok(Key::Other),
                // その他のイベント（フォーカス・貼り付け・キーの離上など）は読み飛ばす。
                _ => {}
            }
        }
    }
}

/// 登録済み workspace の一覧を読む（実 IO）。ストアが開けない・壊れている等の失敗時は
/// 空一覧にフォールバックする（一覧が空でも welcome / Open 画面は成立するため）。
fn load_workspaces() -> Vec<Workspace> {
    Storage::open_default()
        .and_then(|storage| storage.load_workspaces())
        .unwrap_or_default()
}

/// welcome 画面を起動する。対話端末（tty）なら raw mode + 代替スクリーンで対話ループを回し、
/// 非対話環境（パイプ・CI など）では welcome の 1 フレームを `out` へ出して返す。
fn launch_welcome(out: &mut dyn Write, info: &AppInfo) -> std::io::Result<()> {
    let now = Utc::now();
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_interactive(out, info, now)
    } else {
        // サイズ 0 は welcome::render 側で 80x24 にフォールバックされる。
        for line in welcome::render(0, 0, &Welcome::empty(), now) {
            writeln!(out, "{line}")?;
        }
        Ok(())
    }
}

/// raw mode + 代替スクリーンへ入って対話ループを回し、終了時（エラー時も）に端末状態を必ず
/// 元へ戻す。ユーザーが Open 画面で workspace を選んだら、後続で workspace 画面へ接続する。
fn run_interactive(out: &mut dyn Write, info: &AppInfo, now: DateTime<Utc>) -> std::io::Result<()> {
    let workspaces = load_workspaces();

    enable_raw_mode()?;
    let mut setup = std::io::stdout();
    execute!(setup, EnterAlternateScreen, cursor::Hide)?;

    let mut terminal = CrosstermTerminal {
        out: std::io::stdout(),
    };
    let result = presentation::run(&mut terminal, workspaces, now);

    // 描画の成否によらず端末を復元する。
    let mut teardown = std::io::stdout();
    let _ = execute!(teardown, cursor::Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();

    match result? {
        Exit::Quit => Ok(()),
        // 選んだ workspace を開く。workspace 画面は現状バナーなので、代替スクリーンを出た
        // あとの通常端末へ接続する（対話的な workspace 画面は今後実装する）。
        Exit::OpenWorkspace(path) => {
            let mut runner = BannerScreenRunner::new(out, info);
            application::run(&EntryScreen::Workspace { path }, &mut runner)
        }
    }
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

fn launch_tui(
    out: &mut dyn std::io::Write,
    info: &AppInfo,
    entry: &EntryScreen,
) -> std::io::Result<()> {
    if entry == &EntryScreen::Welcome {
        launch_welcome(out, info)
    } else {
        let mut runner = BannerScreenRunner::new(out, info);
        application::run(entry, &mut runner)
    }
}

fn dispatch_cli(
    args: Vec<std::ffi::OsString>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
    info: &AppInfo,
) -> std::io::Result<()> {
    match usagi_cli::cli::run(args, info.version, out, err)? {
        RunOutcome::Exit(code) => std::process::exit(code),
        RunOutcome::LaunchTui(request) => {
            let entry = match request {
                TuiRequest::Welcome => EntryScreen::Welcome,
                TuiRequest::Workspace { path } => EntryScreen::Workspace {
                    path: match path {
                        Some(path) => path,
                        None => std::env::current_dir()?,
                    },
                },
                TuiRequest::Config => EntryScreen::Config,
                TuiRequest::Doctor => EntryScreen::Doctor,
            };
            launch_tui(out, info, &entry)
        }
    }
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
            let path = paths::data_dir()
                .map_err(|err| std::io::Error::other(format!("{err:#}")))?
                .join("daemon")
                .join("daemon.json");
            let store = DaemonRecordStore::new(FsRecordFile { path });
            let launcher = ServeLauncher {
                exe: std::env::current_exe()?,
            };
            let env = DaemonEnv {
                store: &store,
                probe: &KillProbe,
                terminator: &SigtermTerminator,
                shutdown: &SignalShutdown,
                launcher: &launcher,
                sleeper: &RealSleeper,
                pid: std::process::id(),
            };
            usagi_daemon::presentation::run(&mut stdout, command.as_deref(), &info, &env)
        }
        Some("mcp") => {
            let stdin = std::io::stdin();
            usagi_cli::mcp::serve(stdin.lock(), &mut stdout, info.version)
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
