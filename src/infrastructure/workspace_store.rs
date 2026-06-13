//! Persistence for a single repository's [`WorkspaceState`].
//!
//! The state lives inside the repository under `<repo>/.usagi/state.json`, next
//! to the code it describes. Writes go through a temp file + rename so a crash
//! never leaves a half-written `state.json` behind.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::domain::workspace_state::WorkspaceState;

/// Directory created inside the repository to hold usagi's per-repo data.
const STATE_DIR_NAME: &str = ".usagi";
const STATE_FILE: &str = "state.json";

const FILE_FORMAT_VERSION: u32 = 1;

/// On-disk shape of `state.json`.
#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    #[serde(flatten)]
    state: WorkspaceState,
}

/// File-based persistence rooted at a repository's `.usagi/` directory.
pub struct WorkspaceStore {
    dir: PathBuf,
}

impl WorkspaceStore {
    /// Open the store for the repository whose primary worktree is `repo_root`.
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            dir: repo_root.as_ref().join(STATE_DIR_NAME),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.join(STATE_FILE)
    }

    /// Load the saved state, or `None` if it has never been written.
    pub fn load(&self) -> Result<Option<WorkspaceState>> {
        let path = self.state_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
        };
        let file: StateFile = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(file.state))
    }

    /// Persist `state` to `<repo>/.usagi/state.json`.
    pub fn save(&self, state: &WorkspaceState) -> Result<()> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create {}", self.dir.display()))?;

        let file = StateFile {
            version: FILE_FORMAT_VERSION,
            state: state.clone(),
        };
        let mut text = serde_json::to_string_pretty(&file)?;
        text.push('\n');

        let path = self.state_path();
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).with_context(|| format!("failed to write {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::{BranchStatus, WorktreeState};
    use chrono::Utc;

    fn sample_state() -> WorkspaceState {
        WorkspaceState::new(
            "main",
            vec![WorktreeState {
                branch: Some("feature".to_string()),
                path: PathBuf::from("/repo/feature"),
                head: "deadbee".to_string(),
                primary: false,
                upstream: Some("origin/feature".to_string()),
                status: BranchStatus::Pushed,
                updated_at: Utc::now(),
            }],
        )
    }

    #[test]
    fn load_returns_none_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        assert_eq!(store.load().unwrap(), None);
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        let state = sample_state();

        store.save(&state).unwrap();
        assert!(store.state_path().exists());
        assert_eq!(store.load().unwrap(), Some(state));
    }

    #[test]
    fn saved_file_records_the_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        store.save(&sample_state()).unwrap();

        let text = std::fs::read_to_string(store.state_path()).unwrap();
        assert!(text.contains("\"version\": 1"));
    }
}
