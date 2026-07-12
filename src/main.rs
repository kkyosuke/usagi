//! 合成ルート。実 IO（標準出力・プロセス引数）をここで束ね、ロジックはすべて
//! crates/ 配下のライブラリクレート（テスト可能な層）に置く。
//!
//! 配布物は単一バイナリ `usagi` のまま、第 1 引数で面を選ぶ:
//! `usagi daemon` は daemon 面（usagi-daemon）、`usagi mcp` は入口面の MCP、
//! その他のサブコマンドは入口面の CLI（usagi-cli）、引数なしは TUI 面（usagi-tui）。
//! CLI が TUI 起動を要求した場合は、両クレートに依存できるこの合成ルートだけが
//! CLI の起動要求を TUI の初期画面へ変換する。

use usagi_cli::cli::{RunOutcome, TuiRequest};
use usagi_core::domain::AppInfo;
use usagi_tui::presentation::BannerScreenRunner;
use usagi_tui::usecase::application::{self, EntryScreen};

fn launch_tui(
    out: &mut dyn std::io::Write,
    info: &AppInfo,
    entry: &EntryScreen,
) -> std::io::Result<()> {
    let mut runner = BannerScreenRunner::new(out, info);
    application::run(entry, &mut runner)
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
