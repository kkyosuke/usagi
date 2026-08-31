//! `usagi doctor` — 必要ツールと daemon / Agent integration を診断・修復する。

use std::io::{self, Write};

use crate::cli::{Run, RunOutcome, TuiRequest};

/// `usagi doctor` のハンドラ。
pub struct Doctor {
    pub fix: bool,
    pub restart_agents: bool,
    pub force: bool,
}

impl Run for Doctor {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::LaunchTui(TuiRequest::Doctor {
            fix: self.fix,
            restart_agents: self.restart_agents,
            force: self.force,
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::execute;
    use crate::cli::{Command, RunOutcome, TuiRequest};

    #[test]
    fn requests_doctor_without_output() {
        let (outcome, output) = execute(Command::Doctor {
            fix: false,
            restart_agents: false,
            force: false,
        });
        assert_eq!(
            outcome,
            RunOutcome::LaunchTui(TuiRequest::Doctor {
                fix: false,
                restart_agents: false,
                force: false,
            })
        );
        assert!(output.is_empty());
    }
}
