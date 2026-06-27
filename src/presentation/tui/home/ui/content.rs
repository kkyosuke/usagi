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

/// Column the key names are padded to in the cheat sheet, so the descriptions
/// line up regardless of how wide each key glyph renders.
const CHEATSHEET_KEY_COL: usize = 16;

/// One key row of the cheat sheet: the key (or chord) padded to a fixed display
/// column, then its description. Padding is by display width (not byte/char
/// count) so arrow / chord glyphs (`↑↓`, `Ctrl-^`) still align.
fn key_row(key: &str, desc: &str) -> LogLine {
    let pad = CHEATSHEET_KEY_COL.saturating_sub(console::measure_text_width(key));
    LogLine::output(format!("    {key}{}{desc}", " ".repeat(pad)))
}

/// A section header in the cheat sheet (a mode name, unindented).
fn cheatsheet_header(text: &str) -> LogLine {
    LogLine::output(text.to_string())
}

/// The keybinding cheat sheet shown by `?` (a large, scrollable text modal):
/// every reserved key, grouped by the mode it acts in, so "which key does what?"
/// never has to be memorised. The mode keys themselves live in their per-mode
/// handlers ([`super::super::event`]); this is the user-facing reference for
/// them, kept here with the rest of the display text rather than in the state /
/// handler logic.
pub fn cheatsheet() -> Vec<LogLine> {
    let mut lines = vec![
        LogLine::output("Keybindings, by mode. ↑↓ scroll · Esc / q close.".to_string()),
        LogLine::output(String::new()),
        cheatsheet_header("General (any mode)"),
        key_row(":", "Open the command palette (run \"man\" for commands)"),
        key_row("?", "Show this cheat sheet"),
        key_row("Ctrl-B", "Toggle the session sidebar"),
        key_row("Ctrl-C", "Quit (confirms first when a session is live)"),
        key_row("Ctrl-Q", "Quit (always confirms first)"),
        LogLine::output(String::new()),
        cheatsheet_header("切替 / Switch — pick a session"),
        key_row("↑↓ / k j", "Move between sessions"),
        key_row("K / J", "Reorder the selected session"),
        key_row("s", "Sort sessions waiting for input (◆) to the top"),
        key_row("←→ / h l", "Move between the session's tabs"),
        key_row("Ctrl-P / Ctrl-N", "Move between the session's tabs"),
        key_row("Enter", "Focus the session (attach when live)"),
        key_row("t", "Open the action surface (add a pane)"),
        key_row("x", "Close the highlighted tab"),
        key_row("c", "Create a new session"),
        key_row("r", "Rename the session"),
        key_row("n / Ctrl-E", "Edit the session note"),
        key_row("Ctrl-^", "Jump to the previous session"),
        key_row("Esc", "Close the note / back out"),
        LogLine::output(String::new()),
        cheatsheet_header("在席 / Focus — operate a session"),
        key_row("↑↓ / k j", "Move the action menu cursor"),
        key_row("→ / Tab", "Expand the agent picker"),
        key_row("Enter", "Run the highlighted action / open the pane"),
        key_row("t / a", "Launch a terminal / agent"),
        key_row("Ctrl-P / Ctrl-N", "Move between the session's tabs"),
        key_row("Ctrl-O", "Zoom out to Switch"),
        key_row("Ctrl-^", "Jump to the previous session"),
        key_row("Ctrl-E", "Edit the session note"),
        key_row("Esc", "Back out to Switch"),
        LogLine::output(String::new()),
        cheatsheet_header("没入 / Attached — live terminal (other keys go to the shell)"),
        key_row("Ctrl-O", "Zoom out to Switch"),
        key_row("Ctrl-T", "Zoom out to Focus (action menu)"),
        key_row("Ctrl-N / Ctrl-P", "Next / previous tab, in place"),
        key_row("Ctrl-G", "Add an agent tab"),
        key_row("Ctrl-E", "Edit the session note"),
        key_row("Ctrl-B", "Toggle the session sidebar"),
        key_row("Ctrl-^", "Jump to the previous session"),
        key_row("Ctrl-Q", "Quit usagi"),
        key_row("Ctrl-C", "Copy the selection (when one is active)"),
        key_row("Shift+↑↓", "Scroll the history one line"),
        key_row("Shift+PgUp/Dn", "Scroll the history one page"),
    ];
    lines.push(LogLine::output(String::new()));
    lines.push(LogLine::output(
        "Type \":man\" for the command reference.".to_string(),
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::{BranchStatus, WorktreeState};
    use crate::presentation::tui::home::state::LineKind;
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
            diff: None,
            ahead_behind: None,
            updated_at: Utc::now(),
        }
    }

    fn session_record(name: &str, worktrees: usize) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            display_name: None,
            note: None,
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

    #[test]
    fn cheatsheet_lists_keys_grouped_by_every_mode() {
        let lines = cheatsheet();
        let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        // A header per mode (plus the General group) so the reference is grouped.
        assert!(text.iter().any(|t| t.starts_with("General")));
        assert!(text.iter().any(|t| t.contains("切替 / Switch")));
        assert!(text.iter().any(|t| t.contains("在席 / Focus")));
        assert!(text.iter().any(|t| t.contains("没入 / Attached")));
        // The reserved chords the cheat sheet exists to surface are all present.
        let joined = text.join("\n");
        for chord in [
            "Ctrl-O", "Ctrl-T", "Ctrl-N", "Ctrl-G", "Ctrl-Q", "Ctrl-^", "?", ":",
        ] {
            assert!(joined.contains(chord), "cheat sheet should mention {chord}");
        }
        // Every line is plain output (the modal styles it as a dump).
        assert!(lines.iter().all(|l| l.kind == LineKind::Output));
    }

    #[test]
    fn cheatsheet_key_rows_align_descriptions_by_display_width() {
        // A short key, a wide chord, and an arrow glyph (whose display width is
        // not its char count) all pad to the same description column.
        let short = key_row("?", "desc");
        let wide = key_row("Ctrl-P / Ctrl-N", "desc");
        let arrows = key_row("↑↓ / k j", "desc");
        let col =
            |line: &LogLine| console::measure_text_width(line.text.split("desc").next().unwrap());
        assert_eq!(col(&short), col(&wide));
        assert_eq!(col(&short), col(&arrows));
    }
}
