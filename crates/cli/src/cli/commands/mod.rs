//! 人間向け CLI サブコマンドのハンドラ置き場。**1 コマンド = 1 ファイル**とし、
//! 各ファイルのハンドラ型が `Run` を実装する。`cli/mod.rs` の dispatch
//! （`Command::into_handler`）が解釈済みコマンドを対応ハンドラに変換し、実行は
//! `Run::run` の一様な呼び出しになる。
//!
//! 各ハンドラは presentation に徹する — 解析済みのオプションを保持し、TUI/daemon 面への
//! 委譲や core usecase 呼び出し・結果整形を行う（独自のビジネスロジックは持たない）。
//!
//! 現状は **コマンド面の枠だけ** で、`version` 以外のハンドラは未実装を報告するスタブ。
//! v2 では必要になった時点で中身を実装する。

pub mod completion;
pub mod config;
pub mod doctor;
pub mod open;
pub mod update;
pub mod version;

pub use completion::Completion;
pub use config::Config;
pub use doctor::Doctor;
pub use open::Open;
pub use update::Update;
pub use version::Version;

use std::io::{self, Write};

/// 未実装のサブコマンドを表す共通のスタブ出力を `out` に書く。
///
/// `detail` が空でなければ括弧付きで解析済みオプションを併記し、コマンド面の枠が
/// オプションまで通っていることを示す。各コマンドのハンドラ（子モジュール）から使う。
fn unimplemented(out: &mut dyn Write, command: &str, detail: &str) -> io::Result<()> {
    if detail.is_empty() {
        writeln!(out, "usagi {command}: not yet implemented")
    } else {
        writeln!(out, "usagi {command}: not yet implemented ({detail})")
    }
}

/// コマンドを dispatch してハンドラを実行し、出力文字列を得るテストヘルパ。
/// 各コマンドファイルのテストから使い、`Command::into_handler` の各アームも被覆する。
#[cfg(test)]
pub(crate) fn render(command: crate::cli::Command) -> String {
    let mut out = Vec::new();
    command.into_handler("9.9.9").run(&mut out).unwrap();
    String::from_utf8(out).unwrap()
}
