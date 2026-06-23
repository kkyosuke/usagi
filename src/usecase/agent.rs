//! Which agent CLIs are actually installed.
//!
//! usagi can drive several agent CLIs ([`AgentCli`]), but a machine usually has
//! only the one (or few) the user installed. Probing the PATH for each tells the
//! config screen which agents to offer as selectable choices and feeds `doctor`'s
//! agent presence report. The probe goes through [`CommandRunner`] so callers can
//! test it without shelling out.

use crate::domain::settings::AgentCli;
use crate::usecase::doctor::CommandRunner;

/// The agent CLIs whose launch command is available on the PATH, in
/// [`AgentCli::ALL`] order. An empty result means none are installed.
pub fn available_clis(runner: &dyn CommandRunner) -> Vec<AgentCli> {
    AgentCli::ALL
        .into_iter()
        .filter(|cli| runner.available(cli.command()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A runner that reports a fixed allowlist of programs as available.
    struct FakeRunner(Vec<&'static str>);

    impl CommandRunner for FakeRunner {
        fn available(&self, program: &str) -> bool {
            self.0.contains(&program)
        }
        fn run(&self, _program: &str, _args: &[&str]) -> std::io::Result<bool> {
            Ok(true)
        }
        fn check(&self, _program: &str, _args: &[&str]) -> bool {
            true
        }
        fn spawn(&self, _program: &str, _args: &[&str]) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn available_clis_filters_to_installed_in_canonical_order() {
        // Only `claude` and `codex-fugu` are on the PATH: the result keeps them in
        // ALL order and drops the rest.
        let runner = FakeRunner(vec!["claude", "codex-fugu"]);
        assert_eq!(
            available_clis(&runner),
            vec![AgentCli::Claude, AgentCli::CodexFugu]
        );
    }

    #[test]
    fn available_clis_is_empty_when_none_installed() {
        assert!(available_clis(&FakeRunner(vec![])).is_empty());
    }

    #[test]
    fn available_clis_returns_all_when_everything_installed() {
        let runner = FakeRunner(vec!["claude", "codex", "codex-fugu", "gemini"]);
        assert_eq!(available_clis(&runner), AgentCli::ALL.to_vec());
    }

    #[test]
    fn fake_runner_non_probe_methods_are_inert() {
        // `available_clis` only calls `available`; the fake's other `CommandRunner`
        // methods exist solely to satisfy the trait. Exercise them so the double
        // is fully covered (mirroring the doctor module's fake-runner tests).
        let runner = FakeRunner(vec![]);
        assert!(runner.run("x", &[]).unwrap());
        assert!(runner.check("x", &[]));
        assert!(runner.spawn("x", &[]).is_ok());
    }
}
