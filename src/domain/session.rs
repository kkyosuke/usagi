//! Session entities.
//!
//! A *session* is a unit of work created from a workspace root: usagi walks the
//! root recursively and, under `.usagi/worktree/<name>/`, reproduces its
//! directory structure — each git repository as a `git worktree` on a new
//! branch, everything else copied. The types here describe that result; they
//! are persisted inside the workspace's `state.json` (see
//! [`crate::domain::workspace_state::WorkspaceState`]).

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One git worktree created for a session.
///
/// Plain files and non-git directories copied into the session are not recorded
/// here — only the worktrees, since they are what the workspace screen lists and
/// what `terminal` can be opened in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRepo {
    /// Path of the source repository relative to the workspace root.
    pub relative: PathBuf,
    /// Absolute path to the created worktree under `.usagi/worktree/<name>/`.
    pub path: PathBuf,
    /// The new branch checked out in the worktree.
    pub branch: String,
}

/// A session and the worktrees built for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Unique session name (also the new branch name and worktree directory).
    pub name: String,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// Absolute path to the session root (`.usagi/worktree/<name>`).
    pub root: PathBuf,
    /// The git worktrees created for this session.
    pub repos: Vec<SessionRepo>,
}

impl Session {
    /// Build a session record from its name, root, and created worktrees,
    /// stamped as created now.
    pub fn new(name: impl Into<String>, root: impl Into<PathBuf>, repos: Vec<SessionRepo>) -> Self {
        Self {
            name: name.into(),
            created_at: Utc::now(),
            root: root.into(),
            repos,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_stamps_created_at_and_keeps_fields() {
        let repos = vec![SessionRepo {
            relative: PathBuf::from("app"),
            path: PathBuf::from("/ws/.usagi/worktree/feat/app"),
            branch: "feat".to_string(),
        }];
        let session = Session::new("feat", "/ws/.usagi/worktree/feat", repos.clone());
        assert_eq!(session.name, "feat");
        assert_eq!(session.root, PathBuf::from("/ws/.usagi/worktree/feat"));
        assert_eq!(session.repos, repos);
    }

    #[test]
    fn round_trips_through_json() {
        let session = Session::new(
            "feature-x",
            "/ws/.usagi/worktree/feature-x",
            vec![SessionRepo {
                relative: PathBuf::from("be/be1"),
                path: PathBuf::from("/ws/.usagi/worktree/feature-x/be/be1"),
                branch: "feature-x".to_string(),
            }],
        );
        let json = serde_json::to_string_pretty(&session).unwrap();
        let parsed: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, session);
    }
}
