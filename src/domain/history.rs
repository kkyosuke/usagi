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
}

impl HistoryEntry {
    /// Records `command` as having been run now.
    pub fn now(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            executed_at: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_stamps_the_current_time() {
        let before = Utc::now();
        let entry = HistoryEntry::now("man");
        let after = Utc::now();
        assert_eq!(entry.command, "man");
        assert!(entry.executed_at >= before && entry.executed_at <= after);
    }

    #[test]
    fn round_trips_through_json() {
        let entry = HistoryEntry::now("doctor");
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, entry);
    }
}
