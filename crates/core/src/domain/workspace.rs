//! The `Workspace` entity: a directory registered with usagi, addressed by a
//! unique display name and its absolute path.
//!
//! The struct is a plain value object — it carries no behaviour beyond its
//! [`Workspace::new`] constructor, which stamps the creation and update times.
//! It derives `serde` so the workspace registry (an infrastructure concern) can
//! persist it as JSON without the domain knowing where or how it is stored.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A workspace registered with usagi.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Build a workspace, stamping `created_at` and `updated_at` with the current
    /// time (both equal at creation).
    #[must_use]
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        let now = Utc::now();
        Self {
            name: name.into(),
            path: path.into(),
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Workspace;

    #[test]
    fn new_stamps_equal_created_and_updated_times() {
        let ws = Workspace::new("app", "/home/user/app");
        assert_eq!(ws.name, "app");
        assert_eq!(ws.path.to_str(), Some("/home/user/app"));
        // Both timestamps are taken from a single `Utc::now()`, so they match.
        assert_eq!(ws.created_at, ws.updated_at);
        // Exercise the derived Clone / PartialEq / Debug.
        assert_eq!(ws.clone(), ws);
        assert!(format!("{ws:?}").contains("app"));
    }

    #[test]
    fn workspace_round_trips_through_json() {
        let ws = Workspace::new("app", "/home/user/app");
        let json = serde_json::to_string(&ws).unwrap();
        let back: Workspace = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ws);
    }
}
