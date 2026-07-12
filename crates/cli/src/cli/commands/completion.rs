//! `usagi completion <shell>` — 補完スクリプトを標準出力に印字する（Tab 補完を有効化する）。

use std::io::{self, Write};

use super::unimplemented;
use crate::cli::{Run, RunOutcome, Shell};

/// `usagi completion <shell>` のハンドラ。
pub struct Completion {
    pub shell: Shell,
}

impl Run for Completion {
    fn run(&self, out: &mut dyn Write) -> io::Result<RunOutcome> {
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

#[cfg(test)]
mod tests {
    use crate::cli::commands::execute;
    use crate::cli::{Command, RunOutcome, Shell};

    #[test]
    fn maps_every_shell() {
        for (shell, label) in [
            (Shell::Bash, "bash"),
            (Shell::Zsh, "zsh"),
            (Shell::Fish, "fish"),
            (Shell::Powershell, "powershell"),
            (Shell::Elvish, "elvish"),
        ] {
            let (outcome, out) = execute(Command::Completion { shell });
            assert_eq!(outcome, RunOutcome::Exit(0));
            assert!(out.contains(label), "expected {label} in {out}");
        }
    }
}
