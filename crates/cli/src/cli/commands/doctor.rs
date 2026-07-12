//! `usagi doctor` — 必要ツールの導入状況を診断する（TUI の Doctor を開く）。

use std::io::{self, Write};

use super::unimplemented;
use crate::cli::Run;

/// `usagi doctor` のハンドラ。
pub struct Doctor;

impl Run for Doctor {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "doctor", "")
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::Command;
    use crate::cli::commands::render;

    #[test]
    fn reports_name() {
        assert!(render(Command::Doctor).contains("doctor"));
    }
}
