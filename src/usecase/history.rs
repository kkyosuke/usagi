//! Command-history access for the home screen.
//!
//! The history is persisted per workspace by
//! [`HistoryStore`](crate::infrastructure::history_store); this thin usecase
//! exposes the load / append the home screen needs, so the presentation layer
//! goes through the usecase rather than reaching into the store directly.

use std::path::Path;

use anyhow::Result;

use crate::domain::history::HistoryEntry;
use crate::infrastructure::history_store::HistoryStore;

/// Load the workspace's recorded command history (oldest first).
pub fn load(root: &Path) -> Result<Vec<HistoryEntry>> {
    HistoryStore::new(root).load()
}

/// Append an already-built [`HistoryEntry`] to the workspace's command history.
pub fn append(root: &Path, entry: &HistoryEntry) -> Result<()> {
    HistoryStore::new(root).append(entry)
}

/// Render the recorded command history for display, oldest first (chronological),
/// optionally filtered to a single `session` by display name.
///
/// Each entry becomes one line combining its execution time, success marker, the
/// session it ran against (only when no session filter is applied — a filtered
/// view already names the session), and the command. When the history is empty
/// (after filtering) a single explanatory line is returned instead, so the caller
/// always has something to show.
pub fn view(entries: &[HistoryEntry], session: Option<&str>) -> Vec<String> {
    let filtered: Vec<&HistoryEntry> = entries
        .iter()
        .filter(|e| match session {
            Some(name) => e.session.as_deref() == Some(name),
            None => true,
        })
        .collect();

    if filtered.is_empty() {
        return vec![match session {
            Some(name) => format!("No history for session \"{name}\"."),
            None => "No commands in history yet.".to_string(),
        }];
    }

    filtered
        .iter()
        .map(|entry| format_line(entry, session.is_some()))
        .collect()
}

/// Format one history entry as a display line: `<time>  <mark>  [<session>] <command>`.
/// The `[<session>]` tag is omitted when `session_filtered` is set (the view is
/// already scoped to one session) or when the entry has no session (a
/// workspace-wide command).
fn format_line(entry: &HistoryEntry, session_filtered: bool) -> String {
    let mark = if entry.success { '✓' } else { '✗' };
    let time = entry.executed_at.format("%Y-%m-%d %H:%M UTC");
    match (&entry.session, session_filtered) {
        (Some(session), false) => format!("{time}  {mark}  [{session}] {}", entry.command),
        _ => format!("{time}  {mark}  {}", entry.command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_is_empty_without_a_history_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn append_then_load_round_trips_in_order() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &HistoryEntry::now("session list", None, true)).unwrap();
        append(dir.path(), &HistoryEntry::now("issue list", None, true)).unwrap();

        let commands: Vec<String> = load(dir.path())
            .unwrap()
            .into_iter()
            .map(|e| e.command)
            .collect();
        assert_eq!(commands, vec!["session list", "issue list"]);
    }

    /// A fixed-time entry so the formatted lines are deterministic in tests.
    fn entry_at(command: &str, session: Option<&str>, success: bool, secs: u32) -> HistoryEntry {
        use chrono::{TimeZone, Utc};
        HistoryEntry {
            command: command.to_string(),
            executed_at: Utc.with_ymd_and_hms(2026, 6, 14, 1, 2, secs).unwrap(),
            session: session.map(str::to_string),
            success,
        }
    }

    #[test]
    fn view_of_empty_history_explains_it_is_empty() {
        let lines = view(&[], None);
        assert_eq!(lines, vec!["No commands in history yet.".to_string()]);
    }

    #[test]
    fn view_lists_entries_with_time_marker_session_and_command() {
        let entries = vec![
            entry_at("session list", None, true, 3),
            entry_at("terminal", Some("feature-x"), false, 4),
        ];
        let lines = view(&entries, None);
        assert_eq!(
            lines,
            vec![
                "2026-06-14 01:02 UTC  ✓  session list".to_string(),
                "2026-06-14 01:02 UTC  ✗  [feature-x] terminal".to_string(),
            ]
        );
    }

    #[test]
    fn view_filters_to_a_single_session_and_drops_the_tag() {
        let entries = vec![
            entry_at("session list", None, true, 3),
            entry_at("terminal", Some("feature-x"), true, 4),
            entry_at("agent", Some("other"), true, 5),
        ];
        let lines = view(&entries, Some("feature-x"));
        // Only the matching session's entries, with no redundant [session] tag.
        assert_eq!(lines, vec!["2026-06-14 01:02 UTC  ✓  terminal".to_string()]);
    }

    #[test]
    fn view_of_a_session_with_no_history_explains_it() {
        let entries = vec![entry_at("session list", None, true, 3)];
        let lines = view(&entries, Some("missing"));
        assert_eq!(
            lines,
            vec!["No history for session \"missing\".".to_string()]
        );
    }
}
