//! 人間向け CLI サブコマンドのハンドラ置き場。1 コマンド = 1 ハンドラ型とし、
//! それぞれが `Run` を実装する。`cli/mod.rs` の dispatch（`Command::into_handler`）が
//! 解釈済みコマンドを対応ハンドラに変換し、実行は `Run::run` の一様な呼び出しになる。
//!
//! 各ハンドラは presentation に徹する — 解析済みのオプションを保持し、store 系は
//! usagi-core の usecase を直接呼び、session 系は usagi-core の IPC クライアント経由で
//! daemon に委譲し、結果を整形して返す（独自のビジネスロジックは持たない）。
//!
//! 現状は **コマンド面の枠だけ** で、`version` 以外のハンドラは未実装を報告するスタブ。
//! v2 では必要になった時点で中身を実装する。

use std::io::{self, Write};
use std::path::PathBuf;

use super::{Run, Shell};

/// 未実装のサブコマンドを表す共通のスタブ出力を `out` に書く。
///
/// `detail` が空でなければ括弧付きで解析済みオプションを併記し、コマンド面の枠が
/// オプションまで通っていることを示す。
fn unimplemented(out: &mut dyn Write, command: &str, detail: &str) -> io::Result<()> {
    if detail.is_empty() {
        writeln!(out, "usagi {command}: not yet implemented")
    } else {
        writeln!(out, "usagi {command}: not yet implemented ({detail})")
    }
}

/// `usagi open [path]` — ディレクトリを登録して TUI で開く。
pub struct Open {
    pub path: Option<PathBuf>,
}

impl Run for Open {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        let detail = match &self.path {
            Some(path) => format!("path={}", path.display()),
            None => String::new(),
        };
        unimplemented(out, "open", &detail)
    }
}

/// `usagi config` — TUI の Config を開く。
pub struct Config;

impl Run for Config {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "config", "")
    }
}

/// `usagi doctor` — TUI の Doctor を開く（診断）。
pub struct Doctor;

impl Run for Doctor {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "doctor", "")
    }
}

/// `usagi update` — 最新版があるか確認する。
pub struct Update;

impl Run for Update {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "update", "")
    }
}

/// `usagi completion <shell>` — 補完スクリプトを印字する。
pub struct Completion {
    pub shell: Shell,
}

impl Run for Completion {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        let shell = match self.shell {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::Powershell => "powershell",
            Shell::Elvish => "elvish",
        };
        unimplemented(out, "completion", &format!("shell={shell}"))
    }
}

/// `usagi version` — 配布 version を表示する（入口から注入される）。
pub struct Version {
    pub version: String,
}

impl Run for Version {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "usagi {}", self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Command, Shell};

    /// コマンドを dispatch してハンドラを実行し、出力文字列を得るヘルパ。
    fn render(command: Command) -> String {
        let mut out = Vec::new();
        command.into_handler("9.9.9").run(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn open_reports_optional_path() {
        let with = render(Command::Open {
            path: Some("/tmp/x".into()),
        });
        assert!(with.contains("open") && with.contains("path=/tmp/x"));
        let without = render(Command::Open { path: None });
        assert!(without.contains("open") && !without.contains('('));
    }

    #[test]
    fn simple_handlers_report_their_names() {
        assert!(render(Command::Config).contains("config"));
        assert!(render(Command::Doctor).contains("doctor"));
        assert!(render(Command::Update).contains("update"));
    }

    #[test]
    fn completion_maps_every_shell() {
        for (shell, label) in [
            (Shell::Bash, "bash"),
            (Shell::Zsh, "zsh"),
            (Shell::Fish, "fish"),
            (Shell::Powershell, "powershell"),
            (Shell::Elvish, "elvish"),
        ] {
            let out = render(Command::Completion { shell });
            assert!(out.contains(label), "expected {label} in {out}");
        }
    }

    #[test]
    fn version_prints_injected_value() {
        assert_eq!(render(Command::Version), "usagi 9.9.9\n");
    }
}
