//! `usagi open [path]` — ディレクトリをプロジェクトとして登録して TUI で開く。

use std::io::{self, Write};
use std::path::PathBuf;

use super::unimplemented;
use crate::cli::Run;

/// `usagi open [path]` のハンドラ。`path` 省略時はカレントディレクトリを開く。
pub struct Open {
    pub path: Option<PathBuf>,
}

impl Run for Open {
    fn run(&self, out: &mut dyn Write) -> io::Result<()> {
        let detail = match &self.path {
            Some(path) => format!("path={}", path.display()),
            None => String::new(),
        };
        unimplemented(out, "open", &detail)
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::Command;
    use crate::cli::commands::render;

    #[test]
    fn reports_optional_path() {
        let with = render(Command::Open {
            path: Some("/tmp/x".into()),
        });
        assert!(with.contains("open") && with.contains("path=/tmp/x"));
        let without = render(Command::Open { path: None });
        assert!(without.contains("open") && !without.contains('('));
    }
}
