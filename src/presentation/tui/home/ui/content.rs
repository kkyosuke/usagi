//! Formatting of command *output content* — the user-facing lines and modals
//! built from the structured data the state layer holds (sessions, cursor
//! context). Keeping the strings here keeps the state layer free of display
//! text, so its logic stays terminal-independent and its tests assert on data
//! rather than wording.

use crate::domain::workspace_state::SessionRecord;

use super::super::state::LogLine;

/// What the `session list` command renders, given the recorded sessions.
///
/// With sessions it is a scrollable text modal (a long list needs to scroll);
/// with none it is a single output line for the results band (a one-liner needs
/// no modal). The state layer hands over its [`SessionRecord`]s and acts on the
/// variant — opening the modal or logging the line — without owning the wording.
#[derive(Debug, PartialEq, Eq)]
pub enum SessionList {
    /// No sessions yet — one output line for the results band.
    Empty(String),
    /// One or more sessions — a titled, scrollable text modal.
    Modal(&'static str, Vec<LogLine>),
}

/// Build the [`SessionList`] view for `sessions` (see its variants).
pub fn session_list(sessions: &[SessionRecord]) -> SessionList {
    if sessions.is_empty() {
        return SessionList::Empty(
            "No sessions yet. Run \"session create <name>\" to create one.".to_string(),
        );
    }
    let mut lines = vec![LogLine::output(format!("{} session(s):", sessions.len()))];
    for session in sessions {
        lines.push(LogLine::output(format!(
            "  {}  ({} worktree(s))",
            session.name,
            session.worktrees.len()
        )));
    }
    SessionList::Modal("Sessions", lines)
}

/// The notice shown when the session under the cursor has no live shell/agent,
/// pointing at the commands that actually start one.
pub fn no_live_session_hint() -> &'static str {
    "No live session here — run \":agent\" to start one (\":terminal\" for a plain shell)."
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::{BranchStatus, WorktreeState};
    use chrono::Utc;
    use std::path::PathBuf;

    fn worktree(branch: &str) -> WorktreeState {
        WorktreeState {
            branch: Some(branch.to_string()),
            path: PathBuf::from(format!("/repo/{branch}")),
            head: "abc1234".to_string(),
            primary: false,
            upstream: None,
            status: BranchStatus::Local,
            updated_at: Utc::now(),
        }
    }

    fn session_record(name: &str, worktrees: usize) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            display_name: None,
            root: PathBuf::from(format!("/repo/.usagi/sessions/{name}")),
            worktrees: (0..worktrees).map(|_| worktree(name)).collect(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn session_list_with_sessions_builds_a_modal() {
        let sessions = vec![session_record("alpha", 2), session_record("beta", 1)];
        // The modal is titled "Sessions" with a count header and one row per
        // session (its name and worktree count).
        assert_eq!(
            session_list(&sessions),
            SessionList::Modal(
                "Sessions",
                vec![
                    LogLine::output("2 session(s):"),
                    LogLine::output("  alpha  (2 worktree(s))"),
                    LogLine::output("  beta  (1 worktree(s))"),
                ]
            )
        );
    }

    #[test]
    fn session_list_when_empty_is_a_single_line() {
        assert_eq!(
            session_list(&[]),
            SessionList::Empty(
                "No sessions yet. Run \"session create <name>\" to create one.".to_string()
            )
        );
    }

    #[test]
    fn no_live_session_hint_points_at_the_launch_commands() {
        let hint = no_live_session_hint();
        assert!(hint.contains(":agent"));
        assert!(hint.contains(":terminal"));
    }
}
