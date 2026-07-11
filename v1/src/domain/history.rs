//! Per-repository command history.
//!
//! Every command run in the workspace screen's command mode is recorded as a
//! [`HistoryEntry`]. The entries are persisted inside the repository, under
//! `<repo>/.usagi/history.json`, so past commands survive across sessions and
//! can be recalled. The type is a plain entity with no IO; persistence lives in
//! [`crate::infrastructure::history_store`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A single command execution recorded in the workspace's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// The command line exactly as the user submitted it (already trimmed).
    pub command: String,
    /// When the command was run.
    pub executed_at: DateTime<Utc>,
    /// The session (worktree) the command was run against, by display name, or
    /// `None` when it operated on the whole workspace (the `:` command palette /
    /// the `⌂ root` row). Recorded so the `history` command can filter by
    /// session. Defaults to `None` for entries written before the field existed.
    #[serde(default)]
    pub session: Option<String>,
    /// Whether the command succeeded (it produced no error line). Recorded so the
    /// `history` command can flag failures. Defaults to `true` for entries written
    /// before the field existed (a past command with no recorded outcome is shown
    /// as a success rather than a spurious failure).
    #[serde(default = "default_success")]
    pub success: bool,
}

/// The serde default for [`HistoryEntry::success`]: an entry without a recorded
/// outcome (an older history file) is treated as a success, not a failure.
fn default_success() -> bool {
    true
}

impl HistoryEntry {
    /// Records `command`, run now against `session` (or the whole workspace when
    /// `None`), with the given `success` outcome.
    pub fn now(command: impl Into<String>, session: Option<String>, success: bool) -> Self {
        Self {
            command: command.into(),
            executed_at: Utc::now(),
            session,
            success,
        }
    }
}

impl From<String> for HistoryEntry {
    fn from(command: String) -> Self {
        Self::now(command, None, true)
    }
}

impl From<&str> for HistoryEntry {
    fn from(command: &str) -> Self {
        Self::now(command, None, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_stamps_the_current_time() {
        let before = Utc::now();
        let entry = HistoryEntry::now("man", None, true);
        let after = Utc::now();
        assert_eq!(entry.command, "man");
        assert_eq!(entry.session, None);
        assert!(entry.success);
        assert!(entry.executed_at >= before && entry.executed_at <= after);
    }

    #[test]
    fn round_trips_through_json() {
        let entry = HistoryEntry::now("doctor", Some("feature-x".to_string()), false);
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn missing_session_and_success_default_when_deserialized() {
        // An entry written before `session` / `success` existed still loads: the
        // session defaults to `None` and the outcome to a success.
        let json = r#"{"command":"man","executed_at":"2026-06-14T01:02:03.456789Z"}"#;
        let parsed: HistoryEntry = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.command, "man");
        assert_eq!(parsed.session, None);
        assert!(parsed.success);
    }
}
