//! The selectable worktree list (left pane): the workspace's sessions collapsed
//! into one row each, preceded by the synthetic root row, with the cursor /
//! active-row navigation the home screen drives.

use crate::domain::workspace_state::{BranchStatus, DiffStat, SessionRecord, WorktreeState};

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
        diff: DiffStat::aggregate(session.worktrees.iter().map(|w| w.diff)),
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
    /// Sidebar label overrides, aligned 1:1 with `worktrees`: `labels[i]` is the
    /// custom display name for `worktrees[i]`, or `None` to show its branch. The
    /// override is cosmetic only — every name-based lookup keys on the branch (the
    /// session identity), never on the label.
    labels: Vec<Option<String>>,
    selected_index: usize,
    active_index: usize,
    /// The display name of the session that was active *before* the current one,
    /// or `None` until the active row has moved off its initial spot. It is the
    /// target `Ctrl-^` jumps back to (vim's `Ctrl-^` / tmux's `last-window`):
    /// recorded by [`activate_selected`](Self::activate_selected) whenever the
    /// active row changes to a *different* session, and resolved back to a current
    /// row by [`previous_row`](Self::previous_row). Stored as a name, not an index,
    /// so a list rebuild (a background re-sync) keeps it pointing at the same
    /// session — and drops it when that session is gone.
    previous_active: Option<String>,
}

impl WorktreeList {
    /// Builds a list for the named workspace, with both the cursor and the
    /// active row on the root (no session selected yet) and no label overrides.
    pub fn new(workspace_name: impl Into<String>, worktrees: Vec<WorktreeState>) -> Self {
        let labels = vec![None; worktrees.len()];
        Self::with_labels(workspace_name, worktrees, labels)
    }

    /// Builds a list with a sidebar label override per worktree (`labels[i]`
    /// applies to `worktrees[i]`; a shorter/longer `labels` is padded/ignored to
    /// match), with both cursors on the root row.
    pub fn with_labels(
        workspace_name: impl Into<String>,
        worktrees: Vec<WorktreeState>,
        mut labels: Vec<Option<String>>,
    ) -> Self {
        labels.resize(worktrees.len(), None);
        Self {
            workspace_name: workspace_name.into(),
            worktrees,
            labels,
            selected_index: 0,
            active_index: 0,
            previous_active: None,
        }
    }

    /// The sidebar label for the worktree at `index`: its override when set,
    /// otherwise its branch name (the same string [`worktree_name`] returns).
    pub fn display_label(&self, index: usize) -> &str {
        match self.labels.get(index).and_then(Option::as_deref) {
            Some(label) => label,
            None => self.worktrees.get(index).map(worktree_name).unwrap_or(""),
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
    ///
    /// When this lands on a *different* session than the one currently active,
    /// the one being left is remembered as the [`previous_row`](Self::previous_row)
    /// `Ctrl-^` jumps back to; re-activating the same row leaves that memory
    /// untouched (so a no-op focus does not erase where to jump back to).
    pub fn activate_selected(&mut self) -> &str {
        if self.selected_index != self.active_index {
            self.previous_active = Some(self.active_name().to_string());
        }
        self.active_index = self.selected_index;
        self.active_name()
    }

    /// The name of the previously active session, for carrying the `Ctrl-^` jump
    /// target across a list rebuild (a background re-sync drops the list and
    /// builds a fresh one). Paired with [`set_previous_active`](Self::set_previous_active).
    pub fn previous_active_name(&self) -> Option<&str> {
        self.previous_active.as_deref()
    }

    /// Restore the previously active session after a rebuild, so the `Ctrl-^`
    /// jump survives a background re-sync. The name is validated lazily by
    /// [`previous_row`](Self::previous_row), so one that no longer matches simply
    /// yields no jump rather than an error.
    pub fn set_previous_active(&mut self, name: Option<String>) {
        self.previous_active = name;
    }

    /// The row the previously active session now sits at (0 for the root row),
    /// or `None` when no previous session has been recorded yet or it has since
    /// been removed from the list. Resolved by name so a list rebuild keeps it
    /// pointing at the same session — the target `Ctrl-^` focuses.
    pub fn previous_row(&self) -> Option<usize> {
        let name = self.previous_active.as_deref()?;
        if name == ROOT_NAME {
            return Some(0);
        }
        self.worktrees
            .iter()
            .position(|w| worktree_name(w) == name)
            .map(|index| index + 1)
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
