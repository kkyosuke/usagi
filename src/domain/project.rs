use serde::{Deserialize, Serialize};

/// State of a managed project.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectState {
    pub initialized: bool,
    pub worktrees: Vec<Worktree>,
    pub current_worktree: Option<String>,
}

/// A Git worktree managed by usagi.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worktree {
    pub branch: String,
    pub directory: String,
    pub default: bool,
    pub status: SessionStatus,
}

/// Status of a work session on a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Todo,
    InProgress,
    Done,
}
