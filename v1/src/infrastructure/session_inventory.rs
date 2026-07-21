//! Filesystem primitives for the production session inventory adapter.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::domain::workspace_state::SessionRecord;
use crate::infrastructure::repo_paths::{SESSIONS_DIR, STATE_DIR};
use crate::infrastructure::workspace_store::WorkspaceStore;

pub fn workspace_root(start: &Path) -> PathBuf {
    let mut prefix = PathBuf::new();
    let mut components = start.components().peekable();
    while let Some(component) = components.next() {
        if component.as_os_str() == OsStr::new(STATE_DIR)
            && components
                .peek()
                .is_some_and(|next| next.as_os_str() == OsStr::new(SESSIONS_DIR))
        {
            return prefix;
        }
        prefix.push(component);
    }
    start.to_path_buf()
}

pub fn sessions(workspace_root: &Path) -> Result<Vec<SessionRecord>> {
    Ok(WorkspaceStore::new(workspace_root)
        .load()?
        .map(|state| state.sessions)
        .unwrap_or_default())
}
