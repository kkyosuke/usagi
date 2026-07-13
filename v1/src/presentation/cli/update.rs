//! `usagi update` — download and install the latest released binary.

use anyhow::bail;

use crate::usecase::doctor::{CommandRunner, SystemRunner};
use crate::usecase::self_update;

/// Run the documented installer for the latest GitHub release.
///
/// The installer downloads the platform-specific release archive and replaces
/// `~/.usagi/bin/usagi`. The currently running process keeps its old binary;
/// the replacement is used after restarting usagi.
pub fn run() -> anyhow::Result<()> {
    run_with(&SystemRunner, env!("CARGO_PKG_REPOSITORY"))
}

fn run_with(runner: &dyn CommandRunner, repository: &str) -> anyhow::Result<()> {
    let (ok, message) = self_update::run(runner, repository);
    println!("{message}");
    if ok {
        Ok(())
    } else {
        bail!("usagi update failed")
    }
}

#[cfg(test)]
mod tests {
    use super::run_with;
    use crate::usecase::doctor::CommandRunner;

    struct FakeRunner(bool);

    impl CommandRunner for FakeRunner {
        fn available(&self, _program: &str) -> bool {
            true
        }

        fn run(&self, _program: &str, _args: &[&str]) -> std::io::Result<bool> {
            Ok(true)
        }

        fn run_quiet(&self, _program: &str, _args: &[&str]) -> std::io::Result<bool> {
            Ok(self.0)
        }

        fn check(&self, _program: &str, _args: &[&str]) -> bool {
            true
        }

        fn spawn(&self, _program: &str, _args: &[&str]) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn reports_the_installer_result_as_the_command_exit_status() {
        assert!(run_with(&FakeRunner(true), "https://github.com/KKyosuke/usagi").is_ok());
        assert!(run_with(&FakeRunner(false), "https://github.com/KKyosuke/usagi").is_err());
    }
}
