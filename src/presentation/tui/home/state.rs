//! Pure, terminal-independent state for the home (workspace) screen.
//!
//! Holds the opened workspace's name and the list of its worktrees, plus the
//! cursor position. Keeping the navigation logic free of any terminal IO makes
//! it directly testable.

use crate::domain::workspace_state::WorktreeState;

/// The opened workspace and the selectable list of its worktrees.
#[derive(Debug, Clone)]
pub struct WorktreeList {
    workspace_name: String,
    worktrees: Vec<WorktreeState>,
    selected_index: usize,
}

impl WorktreeList {
    /// Builds a list for the named workspace, with the cursor at the top.
    pub fn new(workspace_name: impl Into<String>, worktrees: Vec<WorktreeState>) -> Self {
        Self {
            workspace_name: workspace_name.into(),
            worktrees,
            selected_index: 0,
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

    pub fn is_empty(&self) -> bool {
        self.worktrees.is_empty()
    }

    /// The worktree under the cursor, or `None` when the list is empty.
    pub fn selected(&self) -> Option<&WorktreeState> {
        self.worktrees.get(self.selected_index)
    }

    /// Move the cursor up one row, wrapping to the bottom. No-op when empty.
    pub fn move_up(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.worktrees.len() - 1);
    }

    /// Move the cursor down one row, wrapping to the top. No-op when empty.
    pub fn move_down(&mut self) {
        if self.worktrees.is_empty() {
            return;
        }
        self.selected_index = (self.selected_index + 1) % self.worktrees.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::BranchStatus;
    use chrono::Utc;
    use std::path::PathBuf;

    fn worktree(branch: &str) -> WorktreeState {
        WorktreeState {
            branch: Some(branch.to_string()),
            path: PathBuf::from(format!("/repo/{branch}")),
            head: "abc1234".to_string(),
            primary: false,
            upstream: None,
            status: BranchStatus::Local,
            updated_at: Utc::now(),
        }
    }

    fn sample() -> WorktreeList {
        WorktreeList::new(
            "usagi",
            vec![worktree("main"), worktree("feature"), worktree("fix")],
        )
    }

    #[test]
    fn new_list_starts_at_the_top() {
        let list = sample();
        assert_eq!(list.workspace_name(), "usagi");
        assert_eq!(list.selected_index(), 0);
        assert_eq!(list.worktrees().len(), 3);
        assert!(!list.is_empty());
        assert_eq!(list.selected().unwrap().branch.as_deref(), Some("main"));
    }

    #[test]
    fn empty_list_has_no_selection() {
        let list = WorktreeList::new("usagi", Vec::new());
        assert!(list.is_empty());
        assert!(list.selected().is_none());
    }

    #[test]
    fn move_down_advances_and_wraps() {
        let mut list = sample();
        list.move_down();
        assert_eq!(list.selected_index(), 1);
        list.move_down();
        list.move_down();
        // Wraps from the last item back to the first.
        assert_eq!(list.selected_index(), 0);
        assert_eq!(list.selected().unwrap().branch.as_deref(), Some("main"));
    }

    #[test]
    fn move_up_wraps_to_the_bottom() {
        let mut list = sample();
        list.move_up();
        assert_eq!(list.selected_index(), 2);
        assert_eq!(list.selected().unwrap().branch.as_deref(), Some("fix"));
        list.move_up();
        assert_eq!(list.selected_index(), 1);
    }

    #[test]
    fn movement_is_a_noop_on_an_empty_list() {
        let mut list = WorktreeList::new("usagi", Vec::new());
        list.move_up();
        assert_eq!(list.selected_index(), 0);
        list.move_down();
        assert_eq!(list.selected_index(), 0);
    }
}
