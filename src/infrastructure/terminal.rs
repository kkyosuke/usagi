//! Launching an interactive shell in a chosen directory.
//!
//! The `terminal` command drops out of the TUI and hands the user a real shell
//! rooted at the active worktree, so manual work (running the AI agent, ad-hoc
//! git, builds) happens without leaving usagi. The shell inherits usagi's
//! stdio; control returns here only once the user exits it.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Shell used when the environment names none.
#[cfg(not(windows))]
const FALLBACK_SHELL: &str = "bash";
#[cfg(windows)]
const FALLBACK_SHELL: &str = "cmd.exe";

/// Open an interactive shell with its working directory set to `dir`, blocking
/// until the user exits it.
///
/// The shell's own exit code is not propagated: leaving the shell with a
/// non-zero status (e.g. after a failed command) is a normal return to usagi,
/// not an error. Only a failure to *launch* the shell is reported.
pub fn open(dir: &Path) -> Result<()> {
    run(&default_shell(), dir)
}

/// The interactive shell to launch, taken from the environment with a
/// platform-appropriate fallback.
#[cfg(not(windows))]
fn default_shell() -> String {
    shell_or_fallback(std::env::var("SHELL").ok())
}

#[cfg(windows)]
fn default_shell() -> String {
    shell_or_fallback(std::env::var("COMSPEC").ok())
}

/// Resolve a configured shell value to a concrete program, falling back when it
/// is unset or empty.
fn shell_or_fallback(configured: Option<String>) -> String {
    configured
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| FALLBACK_SHELL.to_string())
}

/// Run `program` as an interactive shell rooted at `dir`, inheriting stdio.
fn run(program: &str, dir: &Path) -> Result<()> {
    Command::new(program)
        .current_dir(dir)
        .status()
        .with_context(|| format!("failed to launch terminal shell `{program}`"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_or_fallback_prefers_a_configured_value() {
        assert_eq!(shell_or_fallback(Some("/bin/zsh".to_string())), "/bin/zsh");
    }

    #[test]
    fn shell_or_fallback_uses_default_when_unset_or_empty() {
        assert_eq!(shell_or_fallback(None), FALLBACK_SHELL);
        assert_eq!(shell_or_fallback(Some(String::new())), FALLBACK_SHELL);
    }

    #[test]
    fn default_shell_returns_a_non_empty_program() {
        assert!(!default_shell().is_empty());
    }

    #[test]
    fn run_launches_a_program_in_the_directory() {
        // `true` exits 0 immediately and ignores stdin, so it stands in for an
        // interactive shell without blocking the test.
        let dir = tempfile::tempdir().unwrap();
        assert!(run("true", dir.path()).is_ok());
    }

    #[test]
    fn run_ignores_a_non_zero_shell_exit() {
        // `false` exits non-zero; leaving a shell with an error is still a normal
        // return to usagi, so `run` reports success.
        let dir = tempfile::tempdir().unwrap();
        assert!(run("false", dir.path()).is_ok());
    }

    #[test]
    fn run_errors_when_the_shell_cannot_be_launched() {
        let dir = tempfile::tempdir().unwrap();
        let err = run("/no/such/shell-binary", dir.path()).unwrap_err();
        assert!(err.to_string().contains("failed to launch terminal shell"));
    }

    #[cfg(not(windows))]
    #[test]
    fn open_launches_the_shell_from_the_environment() {
        // Point `SHELL` at `true` so the public entry point spawns a program that
        // exits immediately instead of a real interactive shell that would block.
        std::env::set_var("SHELL", "true");
        let dir = tempfile::tempdir().unwrap();
        assert!(open(dir.path()).is_ok());
    }
}
