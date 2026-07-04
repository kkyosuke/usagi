//! Formatting of command *output content* — the user-facing lines and modals
//! built from the structured data the state layer holds (sessions, cursor
//! context). Keeping the strings here keeps the state layer free of display
//! text, so its logic stays terminal-independent and its tests assert on data
//! rather than wording.

use crate::domain::settings::KeyScheme;
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
///
/// The 没入 (Attached) section reflects the active `scheme`: the `Ctrl-O` prefix
/// (`Ctrl-O` then a key) or single `Alt`-chords. The other modes are unaffected
/// — they are usagi's own surfaces, not the shell, so they keep their direct keys.
pub fn cheatsheet(scheme: KeyScheme) -> Vec<LogLine> {
    let mut lines = vec![
        LogLine::output("Keybindings, by mode. ↑↓ scroll · Esc / q close.".to_string()),
        LogLine::output(String::new()),
        cheatsheet_header("General — usagi surfaces (没入 has its own keys, below)"),
        key_row(":", "Open the command palette (run \"man\" for commands)"),
        key_row("?", "Show this cheat sheet"),
        key_row("Ctrl-B", "Toggle the session sidebar"),
        key_row("Ctrl-C", "Quit (confirms first when a session is live)"),
        key_row("Ctrl-Q", "Quit (always confirms first)"),
        LogLine::output(String::new()),
        cheatsheet_header("切替 / Switch — pick a session"),
        key_row("↑↓ / k j", "Move between sessions"),
        key_row(
            "Click / 2×Click",
            "Select a session row / focus it (like Enter)",
        ),
        key_row("K / J", "Reorder the selected session"),
        key_row(
            "Space",
            "Fold / unfold the workspace (unite mode, on a root row)",
        ),
        key_row("s", "Sort sessions waiting for input (◆) to the top"),
        key_row("←→ / h l", "Move between the session's tabs"),
        key_row("Ctrl-P / Ctrl-N", "Move between the session's tabs"),
        key_row("Enter", "Focus the session (attach when live)"),
        key_row("t", "Open the action surface (add a pane)"),
        key_row("x", "Close the highlighted tab"),
        key_row("c / Ctrl-A", "Create a new session (Ctrl-A is IME-safe)"),
        key_row("r", "Rename the session"),
        key_row("n / Ctrl-E", "Edit the session note (Ctrl-E is IME-safe)"),
        key_row("Ctrl-^", "Jump to the previous session"),
        key_row("Esc", "Back out to where Switch was opened from"),
        LogLine::output(String::new()),
    ];
    lines.extend(focus_keys(scheme));
    lines.push(LogLine::output(String::new()));
    lines.extend(attached_keys(scheme));
    lines.push(LogLine::output(String::new()));
    lines.push(LogLine::output(
        "Type \":man\" for the command reference.".to_string(),
    ));
    lines
}

/// The 没入 (Attached) section of the cheat sheet for `scheme`. The prefix scheme
/// claims only the `Ctrl-O` leader (the action is the next key) and frees every
/// other Ctrl key to the shell; the `Alt` scheme binds one `Alt`-chord per action
/// and claims no bare Ctrl key. The trailing keys (`Ctrl-^`, `Ctrl-C`, scroll) are
/// the same in both — they are direct, low-conflict keys.
fn attached_keys(scheme: KeyScheme) -> Vec<LogLine> {
    let mut lines = match scheme {
        KeyScheme::Prefix => vec![
            cheatsheet_header("没入 / Attached — live terminal (Ctrl-O is the leader)"),
            key_row("Ctrl-O o", "Zoom out to Switch"),
            key_row("Ctrl-O a", "Zoom out to Focus (action menu)"),
            key_row("Ctrl-O n/p", "Next / previous tab (or Ctrl-O →/←)"),
            key_row("Ctrl-O g", "Add an agent tab"),
            key_row("Ctrl-O e", "Edit the session note"),
            key_row("Ctrl-O s", "Toggle the session sidebar"),
            key_row("Ctrl-O x", "Close the active tab"),
            key_row("Ctrl-O q", "Quit usagi"),
            key_row("Ctrl-O Ctrl-O", "Zoom out to Switch (IME-safe second key)"),
        ],
        KeyScheme::Alt => vec![
            cheatsheet_header("没入 / Attached — live terminal (needs Option=Meta on macOS)"),
            key_row("Alt-o", "Zoom out to Switch"),
            key_row("Alt-a", "Zoom out to Focus (action menu)"),
            key_row("Alt-→ / Alt-←", "Next / previous tab"),
            key_row("Alt-g", "Add an agent tab"),
            key_row("Alt-e", "Edit the session note"),
            key_row("Alt-s", "Toggle the session sidebar"),
            key_row("Alt-x", "Close the active tab"),
            key_row("Alt-q", "Quit usagi"),
        ],
    };
    // Direct, low-conflict keys shared by both schemes (other keys go to the shell).
    lines.extend([
        key_row("Ctrl+Shift+N/P", "Move the active tab right / left"),
        key_row("Ctrl-^", "Jump to the previous session"),
        key_row("Ctrl-C", "Copy the selection (when one is active)"),
        key_row("Shift+↑↓", "Scroll the history one line"),
        key_row("Shift+PgUp/Dn", "Scroll the history one page"),
    ]);
    lines
}

/// The 在席 (Focus) section of the cheat sheet for `scheme`. Under the prefix
/// scheme 在席 shares 没入's `Ctrl-O` leader, so the same `Ctrl-O <key>` chords
/// navigate from either surface (and `Esc` is the one-key exit); the `Alt` scheme
/// keeps `Ctrl-O` a direct zoom-out here, since it drives 没入 with `Alt`-chords
/// and never reads `Ctrl-O` as a leader. The menu / tab keys are the same in both.
fn focus_keys(scheme: KeyScheme) -> Vec<LogLine> {
    let mut lines = vec![
        cheatsheet_header("在席 / Focus — operate a session"),
        key_row("↑↓ / k j", "Move the action menu cursor"),
        key_row("→ / Tab", "Expand the agent picker"),
        key_row("Enter", "Run the highlighted action / open the pane"),
        key_row("t / a", "Launch a terminal / agent"),
        key_row("Shift+c", "Close the focused session with --force"),
        key_row("Ctrl-P / Ctrl-N", "Move between the session's tabs"),
    ];
    match scheme {
        KeyScheme::Prefix => lines.extend([
            key_row("Ctrl-O o", "Zoom out to Switch (or Esc)"),
            key_row("Ctrl-O n/p", "Next / previous tab (or Ctrl-O →/←)"),
            key_row("Ctrl-O g", "Launch an agent"),
            key_row("Ctrl-O e", "Edit the session note (or Ctrl-E)"),
            key_row("Ctrl-O s", "Toggle the session sidebar"),
            key_row("Ctrl-O q", "Quit usagi"),
        ]),
        KeyScheme::Alt => lines.extend([
            key_row("Ctrl-O", "Zoom out to Switch"),
            key_row("Ctrl-E", "Edit the session note"),
        ]),
    }
    lines.extend([
        key_row("Ctrl-^", "Jump to the previous session"),
        key_row("Esc", "Back out to Switch"),
    ]);
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
            pr: Vec::new(),
            updated_at: Utc::now(),
        }
    }

    fn session_record(name: &str, worktrees: usize) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            display_name: None,
            note: None,
            label_id: None,
            root: PathBuf::from(format!("/repo/.usagi/sessions/{name}")),
            worktrees: (0..worktrees).map(|_| worktree(name)).collect(),
            created_at: Utc::now(),
            last_active: None,
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
        let lines = cheatsheet(KeyScheme::Prefix);
        let text: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        // A header per mode (plus the General group) so the reference is grouped.
        assert!(text.iter().any(|t| t.starts_with("General")));
        assert!(text.iter().any(|t| t.contains("切替 / Switch")));
        assert!(text.iter().any(|t| t.contains("在席 / Focus")));
        assert!(text.iter().any(|t| t.contains("没入 / Attached")));
        // The keys the cheat sheet exists to surface are all present.
        let joined = text.join("\n");
        for chord in ["Ctrl-O", "Ctrl-^", "Ctrl-Q", "?", ":"] {
            assert!(joined.contains(chord), "cheat sheet should mention {chord}");
        }
        // Every line is plain output (the modal styles it as a dump).
        assert!(lines.iter().all(|l| l.kind == LineKind::Output));
    }

    #[test]
    fn cheatsheet_attached_section_reflects_the_active_scheme() {
        // Prefix scheme: 没入 navigation is "Ctrl-O" then a key, and the leader can
        // be sent literally — no bare Ctrl-N/Ctrl-T chords are claimed there.
        let prefix = cheatsheet(KeyScheme::Prefix)
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prefix.contains("Ctrl-O is the leader"));
        assert!(prefix.contains("Ctrl-O o"));
        assert!(prefix.contains("Ctrl-O x"));
        assert!(prefix.contains("Ctrl-O Ctrl-O"));
        assert!(!prefix.contains("Alt-"));

        // Alt scheme: one Alt-chord per action, with the macOS Option=Meta note.
        let alt = cheatsheet(KeyScheme::Alt)
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(alt.contains("Option=Meta"));
        assert!(alt.contains("Alt-o"));
        assert!(alt.contains("Alt-x"));
        assert!(alt.contains("Alt-→ / Alt-←"));
        // Both schemes keep the direct, low-conflict keys.
        for sheet in [&prefix, &alt] {
            assert!(sheet.contains("Ctrl-^"));
            assert!(sheet.contains("Shift+↑↓"));
        }
    }

    #[test]
    fn cheatsheet_focus_section_reflects_the_active_scheme() {
        // Prefix scheme: 在席 shares 没入's `Ctrl-O` leader, so the same chords
        // (`Ctrl-O o`, `Ctrl-O g`, …) are listed for it.
        let prefix = cheatsheet(KeyScheme::Prefix)
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prefix.contains("Ctrl-O o"));
        assert!(prefix.contains("Ctrl-O g"));

        // Alt scheme: `Ctrl-O` stays a direct zoom-out in 在席 (no leader), so the
        // Focus section names it plainly alongside the direct `Ctrl-E` note key.
        let alt = cheatsheet(KeyScheme::Alt)
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        // The only `Ctrl-O <key>` rows in the alt sheet are 没入's — none in Focus —
        // so the Focus header is followed by the bare `Ctrl-O` zoom-out row.
        let focus_idx = alt
            .lines()
            .position(|l| l.contains("在席 / Focus"))
            .expect("the Focus header renders");
        let attached_idx = alt
            .lines()
            .position(|l| l.contains("没入 / Attached"))
            .expect("the Attached header renders");
        let focus_section = alt.lines().collect::<Vec<_>>()[focus_idx..attached_idx].to_vec();
        assert!(focus_section
            .iter()
            .any(|l| l.contains("Ctrl-O") && l.contains("Zoom out to Switch")));
        assert!(!focus_section.iter().any(|l| l.contains("Ctrl-O o")));
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
