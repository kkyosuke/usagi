//! `usagi version` — 配布 version を表示する（入口から注入される）。

use std::io::{self, Write};

use crate::cli::Run;

/// `usagi version` のハンドラ。配布 version は合成ルートが `run` に注入し、dispatch が
/// ここへ渡す（cli クレートの 0.0.0 ではなくルートパッケージの version）。
pub struct Version {
    pub version: String,
}

impl Run for Version {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "usagi {}", self.version)
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::Command;
    use crate::cli::commands::render;

    #[test]
    fn prints_injected_value() {
        assert_eq!(render(Command::Version), "usagi 9.9.9\n");
    }
}
