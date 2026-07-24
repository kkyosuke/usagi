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
    use crate::cli::{Cli, Command, RunOutcome};
    use clap::Parser;

    #[test]
    fn packages_parsed_arguments_into_a_launch_request() {
        let cli = Cli::try_parse_from([
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
        ])
        .unwrap();
        let Some(Command::ClaudeSandbox {
            mode,
            writable_root,
            command,
        }) = cli.command
        else {
            panic!("expected a claude-sandbox command");
        };
        // SandboxModeArg の derive（Debug / Clone）を実行する。
        assert!(format!("{mode:?}").contains("Session"));
        let handler = ClaudeSandbox {
            mode: mode.clone().into(),
            writable_roots: writable_root,
            command,
        };
        let outcome = handler.run(&mut Vec::new()).unwrap();
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
    fn root_mode_parses_and_requires_a_command() {
        let cli =
            Cli::try_parse_from(["usagi", "claude-sandbox", "--mode", "root", "--", "claude"])
                .unwrap();
        let Some(Command::ClaudeSandbox { mode, command, .. }) = cli.command else {
            panic!("expected a claude-sandbox command");
        };
        assert_eq!(SandboxMode::from(mode), SandboxMode::Root);
        assert_eq!(command, ["claude"]);
        // command（`-- …`）を省くと required により解析が失敗する。
        assert!(Cli::try_parse_from(["usagi", "claude-sandbox", "--mode", "root"]).is_err());
    }
}
