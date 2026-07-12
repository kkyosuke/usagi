//! 合成ルート。実 IO（標準出力・プロセス引数）をここで束ね、ロジックはすべて
//! crates/ 配下のライブラリクレート（テスト可能な層）に置く。
//!
//! 配布物は単一バイナリ `usagi` のまま、第 1 引数で面を選ぶ:
//! `usagi daemon` は daemon 面（usagi-daemon）、`usagi mcp` は入口面の MCP、
//! その他のサブコマンドは入口面の CLI（usagi-cli）、引数なしは TUI 面（usagi-tui）。
//! CLI が TUI 起動を要求した場合は、両クレートに依存できるこの合成ルートだけが
//! CLI の起動要求を TUI の初期画面へ変換する。

use std::io::{IsTerminal, Write};

use chrono::{DateTime, Utc};
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};

use usagi_cli::cli::{RunOutcome, TuiRequest};
use usagi_core::domain::AppInfo;
use usagi_tui::presentation::views::welcome::{self, Welcome};
use usagi_tui::presentation::{self, BannerScreenRunner};
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
                        KeyCode::Esc => Key::Quit,
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

/// welcome 画面を起動する。対話端末（tty）なら raw mode + 代替スクリーンで対話ループを回し、
/// 非対話環境（パイプ・CI など）では 1 フレームを `out` へ出して返す。
fn launch_welcome(out: &mut dyn Write) -> std::io::Result<()> {
    let now = Utc::now();
    let mut welcome = Welcome::empty();
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        run_welcome_interactive(&mut welcome, now)
    } else {
        // サイズ 0 は welcome::render 側で 80x24 にフォールバックされる。
        for line in welcome::render(0, 0, &welcome, now) {
            writeln!(out, "{line}")?;
        }
        Ok(())
    }
}

/// raw mode + 代替スクリーンへ入って welcome の対話ループを回し、終了時（エラー時も）に
/// 端末状態を必ず元へ戻す。
fn run_welcome_interactive(welcome: &mut Welcome, now: DateTime<Utc>) -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut setup = std::io::stdout();
    execute!(setup, EnterAlternateScreen, cursor::Hide)?;

    let mut terminal = CrosstermTerminal {
        out: std::io::stdout(),
    };
    let result = presentation::run_welcome(&mut terminal, welcome, now);

    // 描画の成否によらず端末を復元する。
    let mut teardown = std::io::stdout();
    let _ = execute!(teardown, cursor::Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    result
}

fn launch_tui(
    out: &mut dyn std::io::Write,
    info: &AppInfo,
    entry: &EntryScreen,
) -> std::io::Result<()> {
    if entry == &EntryScreen::Welcome {
        launch_welcome(out)
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
            usagi_daemon::presentation::run(&mut stdout, command.as_deref(), &info)
        }
        Some("mcp") => usagi_cli::mcp::write_ready_line(&mut stdout, &info),
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
