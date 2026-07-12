//! `usagi version` — 配布 version を表示する（入口から注入される）。

use std::io::{self, Write};

use crate::cli::{Run, RunOutcome};

/// `usagi version` のハンドラ。配布 version は合成ルートが `run` に注入し、dispatch が
/// ここへ渡す（cli クレートの 0.0.0 ではなくルートパッケージの version）。
pub struct Version {
    pub version: String,
}

impl Run for Version {
    fn run(&self, out: &mut dyn Write) -> io::Result<RunOutcome> {
        writeln!(out, "usagi {}", self.version)?;
        Ok(RunOutcome::Exit(0))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::execute;
    use crate::cli::{Command, RunOutcome};

    #[test]
    fn prints_injected_value() {
        let (outcome, output) = execute(Command::Version);
        assert_eq!(outcome, RunOutcome::Exit(0));
        assert_eq!(output, "usagi 9.9.9\n");
    }
}
