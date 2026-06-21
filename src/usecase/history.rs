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

/// Append `command` to the workspace's command history.
pub fn append(root: &Path, command: &str) -> Result<()> {
    HistoryStore::new(root).append(command)
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
        append(dir.path(), "session list").unwrap();
        append(dir.path(), "issue list").unwrap();

        let commands: Vec<String> = load(dir.path())
            .unwrap()
            .into_iter()
            .map(|e| e.command)
            .collect();
        assert_eq!(commands, vec!["session list", "issue list"]);
    }
}
