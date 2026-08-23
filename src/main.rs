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

// `main` returns `ExitCode`, not `io::Result<ExitCode>`: the `Result` form makes
// Rust print a failure with `Debug`, which spells a deliberate message as
// `Error: Custom { kind: Other, error: "…" }`. Reporting goes through
// `process_outcome` so every failing CLI path renders as one line of prose.
#[coverage(off)] // Final process argv and stdio composition.
fn main() -> ExitCode {
    let info = AppInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    };
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let result = runtime::cli::dispatch(args, &mut stdout, &mut stderr, &info);
    runtime::cli::process_outcome(result, &mut stderr)
}
