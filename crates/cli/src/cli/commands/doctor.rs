//! `usagi doctor` — 必要ツール、settings、既存 daemon を診断する。

use std::io::{self, Write};

use crate::cli::{Run, RunOutcome, TuiRequest};

/// `usagi doctor` のハンドラ。
pub struct Doctor {
    pub fix: bool,
}

impl Run for Doctor {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::LaunchTui(TuiRequest::Doctor { fix: self.fix }))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::execute;
    use crate::cli::{Command, RunOutcome, TuiRequest};

    #[test]
    fn requests_doctor_without_output() {
        let (outcome, output) = execute(Command::Doctor { fix: false });
        assert_eq!(
            outcome,
            RunOutcome::LaunchTui(TuiRequest::Doctor { fix: false })
        );
        assert!(output.is_empty());
    }
}
