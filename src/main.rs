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
    match std::env::args().nth(1).as_deref() {
        Some("daemon") => usagi_daemon::write_ready_line(&mut stdout, &info),
        Some("mcp") => usagi_cli::mcp::write_ready_line(&mut stdout, &info),
        None | Some("hop") => usagi_tui::write_banner(&mut stdout, &info),
        // その他のサブコマンドは入口面の CLI へ。clap がコマンドツリーを解析し、
        // ハンドラが実行され、プロセス終了コードが返る。実 stdout / stderr をここで束ねる。
        Some(_) => {
            let mut stderr = std::io::stderr();
            let args = std::env::args_os().collect();
            let code = usagi_cli::cli::run(args, info.version, &mut stdout, &mut stderr)?;
            std::process::exit(code);
        }
    }
}
