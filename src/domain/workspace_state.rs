//! Per-repository workspace state.
//!
//! While [`crate::domain::workspace::Workspace`] is a *global* registry entry
//! (stored under `~/.usagi`), the types here describe the state of a single
//! repository and its worktrees. They are persisted inside the repository
//! itself, under `<repo>/.usagi/state.json`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle status of a branch relative to its remote and the default branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchStatus {
    /// The branch only exists locally; it has no upstream tracking branch.
    Local,
    /// The branch is tracked by an upstream (it has been pushed).
    Pushed,
    /// The branch has been merged into the default branch.
    Merged,
}

impl BranchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BranchStatus::Local => "local",
            BranchStatus::Pushed => "pushed",
            BranchStatus::Merged => "merged",
        }
    }
}

impl std::fmt::Display for BranchStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// State of a single worktree (a branch checked out into a directory).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeState {
    /// Branch checked out in this worktree. `None` for a detached HEAD.
    pub branch: Option<String>,
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// Short commit hash currently checked out.
    pub head: String,
    /// `true` for the repository's primary (main) worktree.
    #[serde(default)]
    pub primary: bool,
    /// Upstream tracking branch (e.g. `origin/feature`), if any.
    #[serde(default)]
    pub upstream: Option<String>,
    /// Lifecycle status of the checked-out branch.
    pub status: BranchStatus,
    /// When this worktree's state was last refreshed.
    pub updated_at: DateTime<Utc>,
}

/// A session created under `.usagi/worktree/<name>/`: a parallel working tree
/// spanning every repository found under the workspace root (each as a git
/// worktree on the session branch) plus any copied non-git files.
///
/// Recorded so the workspace knows its sessions even when the root is not a git
/// repository — `worktrees` (synced from `git worktree list`) only covers a
/// single repo, whereas `sessions` aggregates across a multi-repo tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Session name (also the branch name created in every repository).
    pub name: String,
    /// Root of the session tree: `<workspace>/.usagi/worktree/<name>`.
    pub root: PathBuf,
    /// The mirrored path of every repository that received a worktree.
    pub worktrees: Vec<PathBuf>,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
}

/// State of an entire repository and all of its worktrees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// The repository's default branch (e.g. `main`).
    pub default_branch: String,
    /// State of every worktree, primary first.
    pub worktrees: Vec<WorktreeState>,
    /// Sessions created under `.usagi/worktree/`, across all repositories in the
    /// workspace tree. Empty (and omitted from older files) when none exist.
    #[serde(default)]
    pub sessions: Vec<SessionRecord>,
    /// When the state was last synced from git.
    pub updated_at: DateTime<Utc>,
}

impl WorkspaceState {
    pub fn new(default_branch: impl Into<String>, worktrees: Vec<WorktreeState>) -> Self {
        Self {
            default_branch: default_branch.into(),
            worktrees,
            sessions: Vec::new(),
            updated_at: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_status_as_str_and_display_match() {
        for (status, text) in [
            (BranchStatus::Local, "local"),
            (BranchStatus::Pushed, "pushed"),
            (BranchStatus::Merged, "merged"),
        ] {
            assert_eq!(status.as_str(), text);
            assert_eq!(format!("{status}"), text);
        }
    }

    #[test]
    fn branch_status_serializes_to_snake_case() {
        let json = serde_json::to_string(&BranchStatus::Merged).unwrap();
        assert_eq!(json, "\"merged\"");
        let parsed: BranchStatus = serde_json::from_str("\"pushed\"").unwrap();
        assert_eq!(parsed, BranchStatus::Pushed);
    }

    #[test]
    fn workspace_state_round_trips_through_json() {
        let state = WorkspaceState::new(
            "main",
            vec![WorktreeState {
                branch: Some("main".to_string()),
                path: PathBuf::from("/repo"),
                head: "abc1234".to_string(),
                primary: true,
                upstream: Some("origin/main".to_string()),
                status: BranchStatus::Pushed,
                updated_at: Utc::now(),
            }],
        );

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn new_state_starts_with_no_sessions() {
        let state = WorkspaceState::new("main", Vec::new());
        assert!(state.sessions.is_empty());
    }

    #[test]
    fn sessions_round_trip_and_default_to_empty_when_absent() {
        let mut state = WorkspaceState::new("main", Vec::new());
        state.sessions.push(SessionRecord {
            name: "feature-x".to_string(),
            root: PathBuf::from("/repo/.usagi/worktree/feature-x"),
            worktrees: vec![PathBuf::from("/repo/.usagi/worktree/feature-x/app-a")],
            created_at: Utc::now(),
        });

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);

        // An older file without a `sessions` key still parses (defaults empty).
        let legacy = r#"{"default_branch":"main","worktrees":[],"updated_at":"2026-06-13T05:01:18.659149Z"}"#;
        let parsed: WorkspaceState = serde_json::from_str(legacy).unwrap();
        assert!(parsed.sessions.is_empty());
    }
}
