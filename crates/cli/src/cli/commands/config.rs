//! `usagi config` — 設定を編集する（TUI の Config を開く）。

use std::io::{self, Write};

use super::unimplemented;
use crate::cli::Run;

/// `usagi config` のハンドラ。
pub struct Config;

impl Run for Config {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "config", "")
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::Command;
    use crate::cli::commands::render;

    #[test]
    fn reports_name() {
        assert!(render(Command::Config).contains("config"));
    }
}
