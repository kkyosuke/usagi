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

    /// Rank by lifecycle progress: `Local` < `Pushed` < `Merged`.
    fn rank(self) -> u8 {
        match self {
            BranchStatus::Local => 0,
            BranchStatus::Pushed => 1,
            BranchStatus::Merged => 2,
        }
    }

    /// Aggregate the per-repository statuses of one session's branch into a
    /// single status: the *least-progressed* of them. So a session reads as
    /// `merged` only when every repository's branch has merged, and `pushed`
    /// only when none is still local — a conservative summary where `merged`
    /// always means "fully landed everywhere". An empty iterator yields `Local`.
    pub fn aggregate(statuses: impl IntoIterator<Item = BranchStatus>) -> BranchStatus {
        statuses
            .into_iter()
            .min_by_key(|s| s.rank())
            .unwrap_or(BranchStatus::Local)
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

/// A session created under `.usagi/sessions/<name>/`: a parallel working tree
/// spanning every repository found under the workspace root (each as a git
/// worktree on the session branch) plus any copied non-git files.
///
/// Sessions are the single unit of state usagi tracks: each carries the git
/// status of its per-repository worktrees, so a workspace is fully described by
/// its sessions — even when the root itself is not a git repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Session name (also the branch name created in every repository).
    pub name: String,
    /// Root of the session tree: `<workspace>/.usagi/sessions/<name>`.
    pub root: PathBuf,
    /// One entry per repository that received a worktree, with its git status.
    pub worktrees: Vec<WorktreeState>,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
}

/// State of a workspace: the sessions created under it.
///
/// There is no workspace-wide default branch — a workspace may span several git
/// repositories with differing defaults (`main`, `master`, …), so each
/// worktree's status is classified against *its own* repository's default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Sessions created under `.usagi/sessions/`, across all repositories in the
    /// workspace tree. Empty (and omitted from older files) when none exist.
    #[serde(default)]
    pub sessions: Vec<SessionRecord>,
    /// When the state was last refreshed from git.
    pub updated_at: DateTime<Utc>,
}

impl WorkspaceState {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            updated_at: Utc::now(),
        }
    }
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::new()
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
    fn aggregate_reports_the_least_progressed_status() {
        use BranchStatus::*;
        // Uniform sets keep their status.
        assert_eq!(BranchStatus::aggregate([Merged, Merged]), Merged);
        assert_eq!(BranchStatus::aggregate([Pushed, Pushed]), Pushed);
        // Mixed sets fall to the least-progressed member, regardless of order.
        assert_eq!(BranchStatus::aggregate([Merged, Local]), Local);
        assert_eq!(BranchStatus::aggregate([Pushed, Merged]), Pushed);
        assert_eq!(BranchStatus::aggregate([Merged, Pushed, Local]), Local);
        // A single repository reports its own status; an empty set is `Local`.
        assert_eq!(BranchStatus::aggregate([Merged]), Merged);
        assert_eq!(BranchStatus::aggregate([]), Local);
    }

    #[test]
    fn branch_status_serializes_to_snake_case() {
        let json = serde_json::to_string(&BranchStatus::Merged).unwrap();
        assert_eq!(json, "\"merged\"");
        let parsed: BranchStatus = serde_json::from_str("\"pushed\"").unwrap();
        assert_eq!(parsed, BranchStatus::Pushed);
    }

    fn sample_worktree() -> WorktreeState {
        WorktreeState {
            branch: Some("feature-x".to_string()),
            path: PathBuf::from("/repo/.usagi/sessions/feature-x/app-a"),
            head: "abc1234".to_string(),
            primary: false,
            upstream: Some("origin/feature-x".to_string()),
            status: BranchStatus::Pushed,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn new_state_starts_with_no_sessions() {
        assert!(WorkspaceState::new().sessions.is_empty());
        // `default()` delegates to `new()`, so it is also empty.
        assert!(WorkspaceState::default().sessions.is_empty());
    }

    #[test]
    fn workspace_state_round_trips_through_json() {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "feature-x".to_string(),
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
        });

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn sessions_default_to_empty_when_absent() {
        // An older file without a `sessions` key still parses (defaults empty).
        let legacy = r#"{"updated_at":"2026-06-13T05:01:18.659149Z"}"#;
        let parsed: WorkspaceState = serde_json::from_str(legacy).unwrap();
        assert!(parsed.sessions.is_empty());
    }
}
