#![feature(coverage_attribute)]

//! 配布バイナリの合成ルート。
//!
//! 実 IO の adapter は `runtime/` に責務別に置く。このファイルは各面の選択だけを担い、
//! CLI / TUI / daemon のライブラリクレート間の依存を作らない。

use std::io::Write;

use usagi_core::domain::AppInfo;
use usagi_core::usecase::client::ClientPolicy;
use usagi_tui::usecase::application::EntryScreen;

mod runtime;
mod tui_input;

#[coverage(off)]
fn main() -> std::io::Result<()> {
    let info = AppInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let mut stdout = std::io::stdout();

    match args.get(1).and_then(|arg| arg.to_str()) {
        Some("daemon") => {
            let command = args.get(2).map(|arg| arg.to_string_lossy());
            runtime::daemon::run(&mut stdout, command.as_deref(), &info)
        }
        Some("mcp") => {
            let stdin = std::io::stdin();
            match runtime::daemon::client(ClientPolicy::mcp()) {
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
        None if args.get(1).is_none() => {
            runtime::tui::launch(&mut stdout, &info, &EntryScreen::Welcome)
        }
        _ => {
            let mut stderr = std::io::stderr();
            runtime::cli::dispatch(args, &mut stdout, &mut stderr, &info)
        }
    }
}
