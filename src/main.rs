//! 合成ルート。実 IO（標準出力・プロセス引数）をここで束ね、ロジックはすべて
//! crates/ 配下のライブラリクレート（テスト可能な層）に置く。
//!
//! 配布物は単一バイナリ `usagi` のまま、第 1 引数で面を選ぶ:
//! `usagi daemon` は daemon 面（usagi-daemon）、`usagi mcp` は入口面の MCP、
//! その他のサブコマンドは入口面の CLI（usagi-cli）、引数なしは TUI 面（usagi-tui）。

use usagi_core::domain::AppInfo;

fn main() -> std::io::Result<()> {
    let info = AppInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    };
    let mut stdout = std::io::stdout();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("daemon") => {
            usagi_daemon::presentation::run(&mut stdout, args.get(2).map(String::as_str), &info)
        }
        Some("mcp") => usagi_cli::mcp::write_ready_line(&mut stdout, &info),
        Some(command) => usagi_cli::cli::write_unknown_command(&mut stdout, &info, command),
        None => usagi_tui::presentation::write_banner(&mut stdout, &info),
    }
}
