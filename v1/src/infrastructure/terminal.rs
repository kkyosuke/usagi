//! Resolving the interactive shell to launch for the `terminal` command.
//!
//! The `terminal` command embeds a live shell in the workspace screen's right
//! pane (the pseudo-terminal plumbing lives in [`crate::infrastructure::pty`]).
//! This module only decides *which* shell to start, picking it up from the
//! environment with a platform-appropriate fallback. Keeping that pure choice
//! here makes it directly testable, away from the untestable PTY I/O.

/// Shell used when the environment names none.
#[cfg(not(windows))]
const FALLBACK_SHELL: &str = "bash";
#[cfg(windows)]
const FALLBACK_SHELL: &str = "cmd.exe";

/// The interactive shell to launch, taken from the environment with a
/// platform-appropriate fallback (`$SHELL` on Unix, `%COMSPEC%` on Windows).
#[cfg(not(windows))]
pub(crate) fn default_shell() -> String {
    shell_or_fallback(std::env::var("SHELL").ok())
}

#[cfg(windows)]
pub(crate) fn default_shell() -> String {
    shell_or_fallback(std::env::var("COMSPEC").ok())
}

/// Resolve a configured shell value to a concrete program, falling back when it
/// is unset or empty.
fn shell_or_fallback(configured: Option<String>) -> String {
    configured
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| FALLBACK_SHELL.to_string())
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
}
