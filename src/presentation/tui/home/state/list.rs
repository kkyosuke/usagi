//! The selectable worktree list (left pane): the workspace's sessions collapsed
//! into one row each, preceded by the synthetic root row, with the cursor /
//! active-row navigation the home screen drives.

use crate::domain::workspace_state::{BranchStatus, SessionRecord, WorktreeState};

use super::super::command::WorktreeRef;

/// The display name of a worktree: its branch, or a placeholder when detached.
pub fn worktree_name(worktree: &WorktreeState) -> &str {
    worktree.branch.as_deref().unwrap_or("(detached)")
}

/// The name of the root row: the workspace itself, belonging to no session.
/// Used as its display label and to target it from `session switch root`.
pub const ROOT_NAME: &str = "root";

/// Collapse a session's per-repository worktrees into the single list row that
/// represents it.
///
/// A session is one branch name checked out into a worktree in every repository
/// under the workspace, so the home list shows one row per *session*, not per
/// repository. The row is keyed on the session tree root (`<workspace>/.usagi/
/// sessions/<name>`) — where the embedded terminal/agent roots and the key for
/// its live/waiting state — and carries:
///
/// - the session name as its branch label,
/// - a status [aggregated](BranchStatus::aggregate) across the repositories (the
///   least-progressed, so `synced` means every repository's branch has landed),
/// - `primary` set when any repository's worktree is the primary checkout,
/// - the first repository's `head` / `upstream` as representative detail.
///
/// For a single-repository workspace the session root *is* that repository's
/// worktree, so the row matches the lone worktree exactly.
pub(super) fn session_row(session: &SessionRecord) -> WorktreeState {
    let status = BranchStatus::aggregate(session.worktrees.iter().map(|w| w.status));
    let primary = session.worktrees.iter().any(|w| w.primary);
    let first = session.worktrees.first();
    WorktreeState {
        branch: Some(session.name.clone()),
        path: session.root.clone(),
        head: first.map(|w| w.head.clone()).unwrap_or_default(),
        primary,
        upstream: first.and_then(|w| w.upstream.clone()),
        status,
        updated_at: session.created_at,
    }
}

/// The opened workspace and the selectable list of its worktrees, preceded by a
/// synthetic *root row*.
///
/// The first row (index 0) is the workspace root, which belongs to no session:
/// activating it and running `terminal`/`agent` there works at the workspace
/// root rather than inside a session's worktree. Indices `1..=worktrees.len()`
/// are the recorded worktrees, so row `i` maps to `worktrees[i - 1]`.
///
/// Two cursors are tracked: `selected_index` is where the keyboard cursor sits
/// while navigating, and `active_index` is the row subsequent commands
/// (`session switch`, and later `terminal`/`ai`) act on. Both default to the
/// root row.
#[derive(Debug, Clone)]
pub struct WorktreeList {
    workspace_name: String,
    worktrees: Vec<WorktreeState>,
    selected_index: usize,
    active_index: usize,
}

impl WorktreeList {
    /// Builds a list for the named workspace, with both the cursor and the
    /// active row on the root (no session selected yet).
    pub fn new(workspace_name: impl Into<String>, worktrees: Vec<WorktreeState>) -> Self {
        Self {
            workspace_name: workspace_name.into(),
            worktrees,
            selected_index: 0,
            active_index: 0,
        }
    }

    pub fn workspace_name(&self) -> &str {
        &self.workspace_name
    }

    pub fn worktrees(&self) -> &[WorktreeState] {
        &self.worktrees
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Index of the active row (the one commands act on).
    pub fn active_index(&self) -> usize {
        self.active_index
    }

    /// Whether the workspace has no recorded worktrees (only the root row).
    pub fn is_empty(&self) -> bool {
        self.worktrees.is_empty()
    }

    /// Number of selectable rows: the root row plus every worktree (≥ 1).
    fn selectable_rows(&self) -> usize {
        self.worktrees.len() + 1
    }

    /// Number of sessions listed in the left pane: the root row plus every
    /// worktree (≥ 1). The title bar reports this so the header count matches
    /// the rows the user actually sees.
    pub fn session_count(&self) -> usize {
        self.selectable_rows()
    }

    /// The worktree at a selectable row: row 0 is the root (no worktree), and
    /// row `i` maps to `worktrees[i - 1]`.
    fn worktree_at(&self, row: usize) -> Option<&WorktreeState> {
        row.checked_sub(1).and_then(|i| self.worktrees.get(i))
    }

    /// The worktree under the cursor, or `None` when the cursor is on the root
    /// row (which belongs to no session).
    pub fn selected(&self) -> Option<&WorktreeState> {
        self.worktree_at(self.selected_index)
    }

    /// The active worktree, or `None` when the root row is active.
    pub fn active(&self) -> Option<&WorktreeState> {
        self.worktree_at(self.active_index)
    }

    /// Whether the cursor is on the root row.
    pub fn root_selected(&self) -> bool {
        self.selected_index == 0
    }

    /// Whether the root row is the active one.
    pub fn root_active(&self) -> bool {
        self.active_index == 0
    }

    /// Make the row under the cursor active, returning its display name (the
    /// branch name, or [`ROOT_NAME`] for the root row).
    pub fn activate_selected(&mut self) -> &str {
        self.active_index = self.selected_index;
        self.active_name()
    }

    /// The display name of the active row: its branch, or [`ROOT_NAME`] for the
    /// root row.
    pub fn active_name(&self) -> &str {
        self.active().map(worktree_name).unwrap_or(ROOT_NAME)
    }

    /// The display name of the row under the cursor: its branch, or [`ROOT_NAME`]
    /// for the root row.
    pub fn selected_name(&self) -> &str {
        self.selected().map(worktree_name).unwrap_or(ROOT_NAME)
    }

    /// Make the row named `name` active, returning whether one matched. The
    /// root row matches [`ROOT_NAME`]; every other name is matched against the
    /// worktree branches.
    pub fn activate_by_name(&mut self, name: &str) -> bool {
        if name == ROOT_NAME {
            self.active_index = 0;
            return true;
        }
        match self.worktrees.iter().position(|w| worktree_name(w) == name) {
            Some(index) => {
                self.active_index = index + 1;
                true
            }
            None => false,
        }
    }

    /// Move both the cursor and the active row onto the first worktree named
    /// `name` (a freshly created session's branch), returning whether one
    /// matched. Used after creating a session so the new one is selected and
    /// active without the user navigating to it.
    pub fn select_by_name(&mut self, name: &str) -> bool {
        match self.worktrees.iter().position(|w| worktree_name(w) == name) {
            Some(index) => {
                self.selected_index = index + 1;
                self.active_index = index + 1;
                true
            }
            None => false,
        }
    }

    /// The rows as command-facing [`WorktreeRef`]s (name + active flag): the
    /// root row first, then every worktree.
    pub fn refs(&self) -> Vec<WorktreeRef> {
        let mut refs = vec![WorktreeRef {
            name: ROOT_NAME.to_string(),
            active: self.active_index == 0,
        }];
        refs.extend(self.worktrees.iter().enumerate().map(|(i, w)| WorktreeRef {
            name: worktree_name(w).to_string(),
            active: i + 1 == self.active_index,
        }));
        refs
    }

    /// Move the cursor directly to a selectable `row` (0 is the root row, `i`
    /// maps to `worktrees[i - 1]`), clamped to the rows that exist. Used by the
    /// session picker (`Ctrl-O`) to jump straight to the chosen session.
    pub fn focus_index(&mut self, row: usize) {
        self.selected_index = row.min(self.selectable_rows() - 1);
    }

    /// Move the cursor up one row, wrapping from the root row to the bottom.
    pub fn move_up(&mut self) {
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.selectable_rows() - 1);
    }

    /// Move the cursor down one row, wrapping from the bottom to the root row.
    pub fn move_down(&mut self) {
        self.selected_index = (self.selected_index + 1) % self.selectable_rows();
    }
}
