//! Which agent CLIs are actually installed.
//!
//! usagi can drive several agent CLIs ([`AgentCli`]), but a machine usually has
//! only the one (or few) the user installed. Probing the PATH for each tells the
//! config screen which agents to offer as selectable choices and feeds `doctor`'s
//! agent presence report. The probe goes through [`CommandRunner`] so callers can
//! test it without shelling out.

use std::path::{Path, PathBuf};

use crate::domain::agent::{AgentWiring, LaunchMode};
use crate::domain::agent_feature::{self, AgentFeature, Support};
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

/// The agent CLIs whose launch command is available on the PATH, and which
/// support usagi's MCP server integration.
pub fn mcp_capable_clis(runner: &dyn CommandRunner) -> Vec<AgentCli> {
    available_clis(runner)
        .into_iter()
        .filter(|cli| agent_feature::support(*cli, AgentFeature::Mcp) == Support::Yes)
        .collect()
}

/// Build the per-pane launch wiring from a workspace-wide base wiring.
///
/// The domain wiring is data-only, so the caller injects the git-common-dir
/// resolver. A successful resolution is carried as an extra writable root for
/// attended launches, which lets sandboxed Codex sessions update the repository's
/// shared `.git` store without approval prompts. A resolver failure leaves the
/// launch usable with no extra root; adapters still add their own fixed roots
/// (for Codex, usagi's data directory).
pub fn wiring_for_launch(
    base: &AgentWiring,
    model: Option<String>,
    dir: &Path,
    mode: LaunchMode,
    resolve_git_common_dir: impl FnOnce(&Path) -> Option<PathBuf>,
) -> AgentWiring {
    let mut sandbox_writable_roots = base.sandbox_writable_roots.clone();
    if mode == LaunchMode::Interactive {
        sandbox_writable_roots.extend(resolve_git_common_dir(dir));
    }
    AgentWiring {
        model,
        is_root: !crate::usecase::workspace_guard::is_session_worktree(dir),
        sandbox_writable_roots,
        ..base.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        let runner = FakeRunner(vec!["claude", "codex", "codex-fugu", "gemini", "agy"]);
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

    #[test]
    fn mcp_capable_clis_filters_to_installed_and_mcp_supported() {
        let runner = FakeRunner(vec!["claude", "codex", "gemini"]);
        assert_eq!(
            mcp_capable_clis(&runner),
            vec![AgentCli::Claude, AgentCli::Codex, AgentCli::Gemini]
        );
    }

    fn base_wiring() -> AgentWiring {
        AgentWiring {
            usagi_bin: "usagi".to_string(),
            local_llm_model: Some("qwen2.5-coder:7b".to_string()),
            model: None,
            is_root: true,
            sandbox_writable_roots: vec![PathBuf::from("/old/git")],
        }
    }

    #[test]
    fn launch_wiring_carries_model_root_flag_and_git_common_dir() {
        let dir = Path::new("/repo/.usagi/sessions/fix");
        let wiring = wiring_for_launch(
            &base_wiring(),
            Some("gpt-5-codex".to_string()),
            dir,
            LaunchMode::Interactive,
            |_| Some(PathBuf::from("/repo/.git")),
        );

        assert_eq!(wiring.usagi_bin, "usagi");
        assert_eq!(wiring.local_llm_model.as_deref(), Some("qwen2.5-coder:7b"));
        assert_eq!(wiring.model.as_deref(), Some("gpt-5-codex"));
        assert!(!wiring.is_root);
        assert_eq!(
            wiring.sandbox_writable_roots,
            vec![PathBuf::from("/old/git"), PathBuf::from("/repo/.git")]
        );
    }

    #[test]
    fn launch_wiring_falls_back_when_git_common_dir_is_unresolved() {
        let dir = Path::new("/repo/.usagi/sessions/fix");
        let wiring =
            wiring_for_launch(&base_wiring(), None, dir, LaunchMode::Interactive, |_| None);

        assert_eq!(wiring.model, None);
        assert_eq!(
            wiring.sandbox_writable_roots,
            vec![PathBuf::from("/old/git")]
        );
    }

    #[test]
    fn launch_wiring_marks_workspace_root_and_skips_headless_sandbox_roots() {
        use std::cell::Cell;

        let dir = Path::new("/repo");
        let calls = Cell::new(0);
        let resolve_git_common_dir = |path: &Path| {
            calls.set(calls.get() + 1);
            Some(path.join(".git"))
        };
        assert_eq!(
            resolve_git_common_dir(dir),
            Some(PathBuf::from("/repo/.git"))
        );
        let calls_before_launch = calls.get();

        let wiring = wiring_for_launch(
            &base_wiring(),
            None,
            dir,
            LaunchMode::Headless,
            resolve_git_common_dir,
        );

        assert!(wiring.is_root);
        assert_eq!(calls.get(), calls_before_launch);
        assert_eq!(
            wiring.sandbox_writable_roots,
            vec![PathBuf::from("/old/git")]
        );
    }
}
