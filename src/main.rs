#![feature(coverage_attribute)]

//! 配布バイナリの合成ルート。
//!
//! 実 IO の adapter は `runtime/` に責務別に置く。このファイルは process argv と
//! stdout/stderr を解析済み dispatch へ束ねるだけで、CLI / TUI / daemon の
//! ライブラリクレート間に依存を作らない。

use std::process::ExitCode;

use usagi_core::domain::AppInfo;

mod runtime;
mod tui_input;

#[coverage(off)]
fn main() -> std::io::Result<ExitCode> {
    let info = AppInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    runtime::cli::dispatch(args, &mut stdout, &mut stderr, &info)
}
