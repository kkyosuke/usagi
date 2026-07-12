//! `usagi doctor` — 必要ツールの導入状況を診断する（TUI の Doctor を開く）。

use std::io::{self, Write};

use crate::cli::{Run, RunOutcome, TuiRequest};

/// `usagi doctor` のハンドラ。
pub struct Doctor;

impl Run for Doctor {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::LaunchTui(TuiRequest::Doctor))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::execute;
    use crate::cli::{Command, RunOutcome, TuiRequest};

    #[test]
    fn requests_doctor_without_output() {
        let (outcome, output) = execute(Command::Doctor);
        assert_eq!(outcome, RunOutcome::LaunchTui(TuiRequest::Doctor));
        assert!(output.is_empty());
    }
}
