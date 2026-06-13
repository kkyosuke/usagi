use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A workspace registered with usagi.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    /// Unique display name of the workspace.
    pub name: String,
    /// Absolute path to the workspace directory.
    pub path: PathBuf,
    /// When the workspace was registered.
    pub created_at: DateTime<Utc>,
    /// When the workspace was last used or modified.
    pub updated_at: DateTime<Utc>,
}

impl Workspace {
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        let now = Utc::now();
        Self {
            name: name.into(),
            path: path.into(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Mark the workspace as used now.
    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }
}
