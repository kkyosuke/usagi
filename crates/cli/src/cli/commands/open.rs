//! `usagi open [path]` — ディレクトリをプロジェクトとして登録して TUI で開く。

use std::io::{self, Write};
use std::path::PathBuf;

use crate::cli::{Run, RunOutcome, TuiRequest};

/// `usagi open [path]` のハンドラ。`path` 省略時はカレントディレクトリを開く。
pub struct Open {
    pub path: Option<PathBuf>,
}

impl Run for Open {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::LaunchTui(TuiRequest::Workspace {
            path: self.path.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::execute;
    use crate::cli::{Command, RunOutcome, TuiRequest};

    #[test]
    fn requests_workspace_with_optional_path_without_output() {
        let (with_outcome, with_output) = execute(Command::Open {
            path: Some("/tmp/x".into()),
        });
        assert_eq!(
            with_outcome,
            RunOutcome::LaunchTui(TuiRequest::Workspace {
                path: Some("/tmp/x".into()),
            })
        );
        assert!(with_output.is_empty());

        let (without_outcome, without_output) = execute(Command::Open { path: None });
        assert_eq!(
            without_outcome,
            RunOutcome::LaunchTui(TuiRequest::Workspace { path: None })
        );
        assert!(without_output.is_empty());
    }
}
