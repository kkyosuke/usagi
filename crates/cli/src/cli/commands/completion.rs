//! `usagi completion <shell>` — 補完スクリプトを標準出力に印字する（Tab 補完を有効化する）。

use std::io::{self, Write};

use clap::CommandFactory;

use crate::cli::{Cli, Run, RunOutcome, Shell};

/// `usagi completion <shell>` のハンドラ。
pub struct Completion {
    pub shell: Shell,
}

impl Run for Completion {
    fn run(&self, out: &mut dyn Write) -> io::Result<RunOutcome> {
        // clap のコマンドツリー（`Cli`）から対象シェルの補完スクリプトを生成して印字する。
        // 定義が唯一の真実なので、コマンド・フラグ・値候補は CLI の実態と常に一致する。
        let mut command = Cli::command();
        clap_complete::generate(self.shell, &mut command, "usagi", out);
        Ok(RunOutcome::Exit(0))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::execute;
    use crate::cli::{Command, RunOutcome, Shell};

    #[test]
    fn generates_a_script_for_every_shell() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let (outcome, output) = execute(Command::Completion { shell });
            assert_eq!(outcome, RunOutcome::Exit(0));
            // 生成スクリプトには必ずバイナリ名が現れる（生成が実際に走ったことの確認）。
            assert!(
                output.contains("usagi"),
                "{shell} script should mention usagi"
            );
        }
    }
}
