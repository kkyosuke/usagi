//! `usagi update` — 最新版があるか確認する。

use std::io::{self, Write};

use super::unimplemented;
use crate::cli::Run;

/// `usagi update` のハンドラ。
pub struct Update;

impl Run for Update {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        unimplemented(out, "update", "")
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::Command;
    use crate::cli::commands::render;

    #[test]
    fn reports_name() {
        assert!(render(Command::Update).contains("update"));
    }
}
