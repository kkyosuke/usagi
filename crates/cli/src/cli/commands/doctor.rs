//! `usagi doctor` — 必要ツールと daemon / Agent integration を診断・修復する。

use std::io::{self, Write};

use crate::cli::{DoctorInvocation, Run, RunOutcome, TuiRequest};

/// `usagi doctor` のハンドラ。
pub struct Doctor {
    pub fix: bool,
    pub restart_agents: bool,
    pub force: bool,
    pub invocation: DoctorInvocation,
}

impl Run for Doctor {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::LaunchTui(TuiRequest::Doctor {
            fix: self.fix,
            restart_agents: self.restart_agents,
            force: self.force,
            invocation: self.invocation,
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::execute;
    use crate::cli::{Command, DoctorInvocation, RunOutcome, TuiRequest};

    #[test]
    fn requests_doctor_without_output() {
        let (outcome, output) = execute(Command::Doctor {
            fix: false,
            restart_agents: false,
            force: false,
            managed_update_sync: false,
        });
        assert_eq!(
            outcome,
            RunOutcome::LaunchTui(TuiRequest::Doctor {
                fix: false,
                restart_agents: false,
                force: false,
                invocation: DoctorInvocation::User,
            })
        );
        assert!(output.is_empty());
    }
}
