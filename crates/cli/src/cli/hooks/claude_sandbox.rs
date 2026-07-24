//! `usagi claude-sandbox --mode <session|root> [--writable-root <path>]… -- <program> <args…>`
//! — OS sandbox の中で Claude を fail-closed 起動する内部コマンド。
//!
//! usagi の Claude provisioner が起動 program をこの launcher で包む（`usagi claude-sandbox … --
//! claude …`）。人手で叩くものではない（`--help` 非表示）。ここは解析済み引数を typed な
//! [`RunOutcome::ClaudeSandbox`] にまとめるだけの薄いシムで、platform 判定・backend 探索・
//! `$TMPDIR` / `$HOME` の読み取り・実 exec は合成ルートが束ねる。sandbox 計画の純粋な決定部は
//! [`usagi_core::usecase::claude_sandbox`] にあり、backend 不在・未対応 platform では起動を拒否する
//! （無保護フォールバックしない）。

use std::io::{self, Write};
use std::path::PathBuf;

use usagi_core::usecase::claude_sandbox::SandboxMode;

use crate::cli::{Run, RunOutcome};

/// `usagi claude-sandbox` のハンドラ。実 platform 解決と exec は合成ルートが束ねる
/// （[`RunOutcome::ClaudeSandbox`]）。
pub struct ClaudeSandbox {
    /// 起動モード（session / root）。
    pub mode: SandboxMode,
    /// sandbox が書き込みを許す起動固有 root。
    pub writable_roots: Vec<PathBuf>,
    /// sandbox の中で exec する program と引数。
    pub command: Vec<String>,
}

impl Run for ClaudeSandbox {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::ClaudeSandbox {
            mode: self.mode,
            writable_roots: self.writable_roots.clone(),
            command: self.command.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command, RunOutcome, SandboxModeArg};
    use clap::Parser;

    /// argv を解析し、`Command::into_handler` 経由でハンドラを実行して `RunOutcome` を得る。
    /// `Command` を直接分解せず（未実行 panic 行を作らず）、解析 → mode 変換 → 引数受け渡しの
    /// フロー全体を被覆する。`into_handler` は coverage-off の合成点で、その中の
    /// `SandboxModeArg::into()` は本 crate の [`From`] を実際に呼ぶ。
    fn run_parsed(argv: &[&str]) -> RunOutcome {
        Cli::try_parse_from(argv)
            .unwrap()
            .command
            .unwrap()
            .into_handler("9.9.9")
            .run(&mut Vec::new())
            .unwrap()
    }

    #[test]
    fn session_mode_packages_writable_roots_and_command_after_dashes() {
        let outcome = run_parsed(&[
            "usagi",
            "claude-sandbox",
            "--mode",
            "session",
            "--writable-root",
            "/repo/.usagi/sessions/work",
            "--writable-root",
            "/repo/.git",
            "--",
            "claude",
            "--print",
        ]);
        assert_eq!(
            outcome,
            RunOutcome::ClaudeSandbox {
                mode: SandboxMode::Session,
                writable_roots: vec![
                    PathBuf::from("/repo/.usagi/sessions/work"),
                    PathBuf::from("/repo/.git"),
                ],
                command: vec!["claude".to_owned(), "--print".to_owned()],
            }
        );
        // derive された Clone / Debug も実行する（RunOutcome の新 variant を被覆）。
        assert_eq!(outcome.clone(), outcome);
        assert!(format!("{outcome:?}").contains("ClaudeSandbox"));
    }

    #[test]
    fn root_mode_maps_and_requires_a_command() {
        let outcome = run_parsed(&["usagi", "claude-sandbox", "--mode", "root", "--", "claude"]);
        assert_eq!(
            outcome,
            RunOutcome::ClaudeSandbox {
                mode: SandboxMode::Root,
                writable_roots: vec![],
                command: vec!["claude".to_owned()],
            }
        );
        // command（`-- …`）を省くと required により解析が失敗する。
        assert!(Cli::try_parse_from(["usagi", "claude-sandbox", "--mode", "root"]).is_err());
    }

    #[test]
    fn mode_argument_parses_and_exposes_derives() {
        // clap ValueEnum の解析と、`Command` の Debug derive が要求する SandboxModeArg の
        // Debug / Clone を直接実行する。
        assert!(matches!(
            Cli::try_parse_from([
                "usagi",
                "claude-sandbox",
                "--mode",
                "session",
                "--",
                "claude"
            ])
            .unwrap()
            .command,
            Some(Command::ClaudeSandbox { .. })
        ));
        assert_eq!(format!("{:?}", SandboxModeArg::Root.clone()), "Root");
    }
}
