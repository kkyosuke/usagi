//! `usagi update` — 最新版があるか確認する。

use std::io::{self, Write};

use super::unimplemented;
use crate::cli::{Run, RunOutcome};

/// `usagi update` のハンドラ。
pub struct Update;

impl Run for Update {
    fn run(&self, out: &mut dyn Write) -> io::Result<RunOutcome> {
        unimplemented(out, "update")
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::execute;
    use crate::cli::{Command, RunOutcome};

    #[test]
    fn reports_name() {
        let (outcome, output) = execute(Command::Update);
        assert_eq!(outcome, RunOutcome::Exit(0));
        assert!(output.contains("update"));
    }
}
