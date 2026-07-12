//! Deciding whether an embedded session's exit is worth logging.
//!
//! When a shell embedded by the `terminal` command (the PTY plumbing lives in
//! [`crate::infrastructure::pty`]) ends, usagi only learns that its pane closed
//! — not *why*. An agent CLI that panics or exits non-zero looks, to the rest of
//! the app, exactly like the user typing `exit`. This module turns the child's
//! exit status into the error-log line to record, or `None` when the exit is
//! normal (clean code 0) and should stay out of the log to avoid noise.
//!
//! Keeping that pure decision here — away from the untestable PTY I/O in
//! [`pty`](crate::infrastructure::pty), which is excluded from coverage — makes
//! it directly testable, mirroring how [`terminal`](crate::infrastructure::terminal)
//! holds the shell-choice logic.

use std::path::Path;

use portable_pty::ExitStatus;

/// The error-log line for a session that has just ended, or `None` when it
/// exited cleanly (code 0 with no signal) — a normal close (the user typed
/// `exit`, or the agent finished its work) that should not be logged as a
/// failure.
///
/// `is_agent` distinguishes a pane launched into an agent CLI from a plain
/// terminal, so the recorded line names what actually died.
pub fn exit_log_message(worktree: &Path, is_agent: bool, status: &ExitStatus) -> Option<String> {
    if status.success() {
        return None;
    }
    let what = if is_agent {
        "agent session"
    } else {
        "terminal"
    };
    let where_ = worktree.display();
    let message = match status.signal() {
        Some(signal) => format!("{what} in {where_} terminated by signal {signal}"),
        None => format!(
            "{what} in {where_} exited with status {}",
            status.exit_code()
        ),
    };
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn worktree() -> &'static Path {
        Path::new("/work/issue-70")
    }

    #[test]
    fn clean_exit_is_not_logged() {
        assert_eq!(
            exit_log_message(worktree(), true, &ExitStatus::with_exit_code(0)),
            None
        );
        assert_eq!(
            exit_log_message(worktree(), false, &ExitStatus::with_exit_code(0)),
            None
        );
    }

    #[test]
    fn non_zero_agent_exit_names_the_agent_session_and_code() {
        assert_eq!(
            exit_log_message(worktree(), true, &ExitStatus::with_exit_code(2)),
            Some("agent session in /work/issue-70 exited with status 2".to_string())
        );
    }

    #[test]
    fn non_zero_terminal_exit_names_the_terminal_and_code() {
        assert_eq!(
            exit_log_message(worktree(), false, &ExitStatus::with_exit_code(127)),
            Some("terminal in /work/issue-70 exited with status 127".to_string())
        );
    }

    #[test]
    fn signal_termination_names_the_signal() {
        assert_eq!(
            exit_log_message(worktree(), true, &ExitStatus::with_signal("Killed")),
            Some("agent session in /work/issue-70 terminated by signal Killed".to_string())
        );
        assert_eq!(
            exit_log_message(
                worktree(),
                false,
                &ExitStatus::with_signal("Segmentation fault")
            ),
            Some("terminal in /work/issue-70 terminated by signal Segmentation fault".to_string())
        );
    }
}
