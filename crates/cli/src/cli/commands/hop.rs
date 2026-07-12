//! `usagi hop` — Welcome TUI を開く（引数なし起動の互換 alias）。

use std::io::{self, Write};

use crate::cli::{Run, RunOutcome, TuiRequest};

/// `usagi hop` のハンドラ。
pub struct Hop;

impl Run for Hop {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::LaunchTui(TuiRequest::Welcome))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::commands::execute;
    use crate::cli::{Command, RunOutcome, TuiRequest};

    #[test]
    fn requests_welcome_without_output() {
        let (outcome, output) = execute(Command::Hop);
        assert_eq!(outcome, RunOutcome::LaunchTui(TuiRequest::Welcome));
        assert!(output.is_empty());
    }
}
