//! Per-repository workspace state.
//!
//! While [`crate::domain::workspace::Workspace`] is a *global* registry entry
//! (stored under `~/.usagi`), the types here describe the state of a single
//! repository and its worktrees. They are persisted inside the repository
//! itself, under `<repo>/.usagi/state.json`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle status of a branch relative to its working tree, its remote, and
/// the default branch.
///
/// The states form a progression — `New` → (`Dirty`) → `Local` → `Pushed` →
/// `Synced` — but a branch does not march through them in order: it is
/// re-derived from git on every refresh, so editing files reads `Dirty`,
/// committing reads `Local`, pushing reads `Pushed`, and a branch the default
/// has moved past reads `Synced`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchStatus {
    /// Freshly cut and untouched: a clean working tree with no commits of its
    /// own and the default branch has not moved past it (even with the default).
    /// This is the state a session branch starts in, before any work.
    New,
    /// The working tree has uncommitted changes (modified, staged, or untracked
    /// files) — work in progress that has not been committed yet.
    Dirty,
    /// Clean tree with commits of its own that have not been pushed (no upstream
    /// tracking branch).
    Local,
    /// Clean tree with commits of its own and an upstream tracking branch (the
    /// branch has been pushed but is not yet merged).
    Pushed,
    /// The default branch has moved past this branch (it is behind with no
    /// commits of its own ahead): everything the branch carried is now on the
    /// integration branch, so it reads as `synced` — merged / up to date. Older
    /// `state.json` spelled this `"merged"` then `"up_to_date"`; both aliases
    /// keep that data loading (it is now written as `"synced"`).
    #[serde(alias = "merged", alias = "up_to_date")]
    Synced,
}

impl BranchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BranchStatus::New => "new",
            BranchStatus::Dirty => "dirty",
            BranchStatus::Local => "local",
            BranchStatus::Pushed => "pushed",
            BranchStatus::Synced => "synced",
        }
    }

    /// Rank by lifecycle progress: `New` < `Dirty` < `Local` < `Pushed` <
    /// `Synced`. Used to aggregate a session's repositories into its
    /// least-progressed status.
    fn rank(self) -> u8 {
        match self {
            BranchStatus::New => 0,
            BranchStatus::Dirty => 1,
            BranchStatus::Local => 2,
            BranchStatus::Pushed => 3,
            BranchStatus::Synced => 4,
        }
    }

    /// Aggregate the per-repository statuses of one session's branch into a
    /// single status: the *least-progressed* of them. So a session reads as
    /// `synced` only when every repository's branch is up to date, and `pushed`
    /// only when none is still local/dirty/new — a conservative summary where
    /// `synced` always means "no un-merged work anywhere". An empty iterator
    /// yields `New`.
    pub fn aggregate(statuses: impl IntoIterator<Item = BranchStatus>) -> BranchStatus {
        statuses
            .into_iter()
            .min_by_key(|s| s.rank())
            .unwrap_or(BranchStatus::New)
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
    /// Session name (also the branch name created in every repository). This is
    /// the session's identity: commands (`session switch`, removal) target it,
    /// so it never changes once created.
    pub name: String,
    /// An optional sidebar label that overrides [`name`](Self::name) in the home
    /// screen's session list, without touching the branch / identity. `None`
    /// (the default, and omitted from the file) shows the `name` as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// A free-form, multi-line note attached to the session — scratch space for
    /// what it is for, what is left to do, links, and so on. Display / UX only:
    /// it never affects the session's identity or its branches. `None` (the
    /// default, and omitted from the file) means no note has been written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Root of the session tree: `<workspace>/.usagi/sessions/<name>`.
    pub root: PathBuf,
    /// One entry per repository that received a worktree, with its git status.
    pub worktrees: Vec<WorktreeState>,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
}

impl SessionRecord {
    /// The label shown in the sidebar: the custom [`display_name`](Self::display_name)
    /// when set, otherwise the session [`name`](Self::name).
    pub fn display_label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }

    /// The session's note, or `None` when none has been written.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }
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
            (BranchStatus::New, "new"),
            (BranchStatus::Dirty, "dirty"),
            (BranchStatus::Local, "local"),
            (BranchStatus::Pushed, "pushed"),
            (BranchStatus::Synced, "synced"),
        ] {
            assert_eq!(status.as_str(), text);
            assert_eq!(format!("{status}"), text);
        }
    }

    #[test]
    fn aggregate_reports_the_least_progressed_status() {
        use BranchStatus::*;
        // Uniform sets keep their status.
        assert_eq!(BranchStatus::aggregate([Synced, Synced]), Synced);
        assert_eq!(BranchStatus::aggregate([Pushed, Pushed]), Pushed);
        // Mixed sets fall to the least-progressed member, regardless of order.
        assert_eq!(BranchStatus::aggregate([Synced, Local]), Local);
        assert_eq!(BranchStatus::aggregate([Pushed, Synced]), Pushed);
        assert_eq!(BranchStatus::aggregate([Synced, Pushed, Local]), Local);
        // Dirty and New outrank a committed branch as "least progressed".
        assert_eq!(BranchStatus::aggregate([Pushed, Dirty]), Dirty);
        assert_eq!(BranchStatus::aggregate([Synced, New]), New);
        assert_eq!(BranchStatus::aggregate([Dirty, New]), New);
        // A single repository reports its own status; an empty set is `New`.
        assert_eq!(BranchStatus::aggregate([Synced]), Synced);
        assert_eq!(BranchStatus::aggregate([]), New);
    }

    #[test]
    fn branch_status_serializes_to_snake_case_and_reads_legacy_aliases() {
        let json = serde_json::to_string(&BranchStatus::Synced).unwrap();
        assert_eq!(json, "\"synced\"");
        let parsed: BranchStatus = serde_json::from_str("\"pushed\"").unwrap();
        assert_eq!(parsed, BranchStatus::Pushed);
        assert_eq!(
            serde_json::from_str::<BranchStatus>("\"new\"").unwrap(),
            BranchStatus::New
        );
        assert_eq!(
            serde_json::from_str::<BranchStatus>("\"dirty\"").unwrap(),
            BranchStatus::Dirty
        );
        // Older state.json spelled the synced status "merged", then "up_to_date";
        // both aliases keep that data loading.
        assert_eq!(
            serde_json::from_str::<BranchStatus>("\"merged\"").unwrap(),
            BranchStatus::Synced
        );
        assert_eq!(
            serde_json::from_str::<BranchStatus>("\"up_to_date\"").unwrap(),
            BranchStatus::Synced
        );
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
            display_name: None,
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
        });

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn display_label_falls_back_to_name_then_prefers_display_name() {
        let mut session = SessionRecord {
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
        };
        // No override → the session name is the label.
        assert_eq!(session.display_label(), "feature-x");
        session.display_name = Some("My Feature".to_string());
        assert_eq!(session.display_label(), "My Feature");
    }

    #[test]
    fn display_name_is_omitted_from_json_when_absent_and_round_trips_when_set() {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "feature-x".to_string(),
            display_name: Some("Nice name".to_string()),
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
        });
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"display_name\":\"Nice name\""));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );

        // Cleared again → the key is dropped, and an older file without it parses.
        state.sessions[0].display_name = None;
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("display_name"));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );
    }

    #[test]
    fn note_is_omitted_when_absent_round_trips_when_set_and_reads_legacy_files() {
        let mut state = WorkspaceState::new();
        state.sessions.push(SessionRecord {
            name: "feature-x".to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from("/repo/.usagi/sessions/feature-x"),
            worktrees: vec![sample_worktree()],
            created_at: Utc::now(),
        });
        // No note → the accessor is `None` and the key is dropped from the file.
        assert_eq!(state.sessions[0].note(), None);
        let json = serde_json::to_string(&state).unwrap();
        assert!(!json.contains("note"));

        // A multi-line note is stored, exposed, and round-trips through JSON.
        state.sessions[0].note = Some("line 1\nline 2".to_string());
        assert_eq!(state.sessions[0].note(), Some("line 1\nline 2"));
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"note\":\"line 1\\nline 2\""));
        assert_eq!(
            serde_json::from_str::<WorkspaceState>(&json).unwrap(),
            state
        );

        // An older file without a `note` key still parses (defaults to `None`).
        let legacy = r#"{"sessions":[{"name":"x","root":"/r","worktrees":[],"created_at":"2026-06-13T05:01:18.659149Z"}],"updated_at":"2026-06-13T05:01:18.659149Z"}"#;
        let parsed: WorkspaceState = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.sessions[0].note(), None);
    }

    #[test]
    fn sessions_default_to_empty_when_absent() {
        // An older file without a `sessions` key still parses (defaults empty).
        let legacy = r#"{"updated_at":"2026-06-13T05:01:18.659149Z"}"#;
        let parsed: WorkspaceState = serde_json::from_str(legacy).unwrap();
        assert!(parsed.sessions.is_empty());
    }
}
