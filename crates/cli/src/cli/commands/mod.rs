//! 人間向け CLI サブコマンドのハンドラ置き場。**1 コマンド = 1 ファイル**とし、
//! 各ファイルのハンドラ型が `Run` を実装する。`cli/mod.rs` の dispatch
//! （`Command::into_handler`）が解釈済みコマンドを対応ハンドラに変換し、実行は
//! `Run::run` の一様な呼び出しになる。
//!
//! 各ハンドラは presentation に徹する — 解析済みのオプションを保持し、TUI/daemon 面への
//! 委譲や core usecase 呼び出し・結果整形を行う（独自のビジネスロジックは持たない）。
//!
//! TUI を開くハンドラは起動要求を返し、それ以外の未実装ハンドラは案内を出して終了する。

pub mod completion;
pub mod config;
pub mod doctor;
pub mod hop;
pub mod open;
pub mod update;
pub mod version;

pub use completion::Completion;
pub use config::Config;
pub use doctor::Doctor;
pub use hop::Hop;
pub use open::Open;
pub use update::Update;
pub use version::Version;

use std::io::{self, Write};

use crate::cli::RunOutcome;

/// 未実装のサブコマンドを表す共通のスタブ出力を `out` に書く。
///
/// `detail` が空でなければ括弧付きで解析済みオプションを併記し、コマンド面の枠が
/// オプションまで通っていることを示す。各コマンドのハンドラ（子モジュール）から使う。
fn unimplemented(out: &mut dyn Write, command: &str, detail: &str) -> io::Result<RunOutcome> {
    if detail.is_empty() {
        writeln!(out, "usagi {command}: not yet implemented")?;
    } else {
        writeln!(out, "usagi {command}: not yet implemented ({detail})")?;
    }
    Ok(RunOutcome::Exit(0))
}

/// コマンドを dispatch してハンドラを実行し、結果と出力文字列を得るテストヘルパ。
/// 各コマンドファイルのテストから使い、`Command::into_handler` の各アームも被覆する。
#[cfg(test)]
pub(crate) fn execute(command: crate::cli::Command) -> (RunOutcome, String) {
    let mut out = Vec::new();
    let outcome = command.into_handler("9.9.9").run(&mut out).unwrap();
    (outcome, String::from_utf8(out).unwrap())
}
