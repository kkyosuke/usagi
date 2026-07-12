//! `usagi config` — 設定を編集する（TUI の Config を開く）。

use std::io::{self, Write};

use crate::cli::{Run, RunOutcome, TuiRequest};

/// `usagi config` のハンドラ。
pub struct Config;

impl Run for Config {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::LaunchTui(TuiRequest::Config))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::commands::execute;
    use crate::cli::{Command, RunOutcome, TuiRequest};

    #[test]
    fn requests_config_without_output() {
        let (outcome, output) = execute(Command::Config);
        assert_eq!(outcome, RunOutcome::LaunchTui(TuiRequest::Config));
        assert!(output.is_empty());
    }
}
