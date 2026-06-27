//! Pure, terminal-independent state for the project selection screen.
//!
//! Holds the list of registered workspaces and the cursor position. Keeping the
//! navigation logic free of any terminal IO makes it directly testable.

use crate::domain::workspace::Workspace;
use crate::usecase::workspace::WorkspaceOverview;

/// The selectable list of workspaces and the current cursor position.
///
/// Each entry is a [`WorkspaceOverview`] — the workspace plus the session and
/// open-issue counts shown beside it — so the screen can render the figures
/// without re-reading the disk every frame.
#[derive(Debug, Clone)]
pub struct ProjectList {
    overviews: Vec<WorkspaceOverview>,
    selected_index: usize,
}

impl ProjectList {
    /// Builds a list from the given workspace overviews, cursor at the top.
    pub fn new(overviews: Vec<WorkspaceOverview>) -> Self {
        Self {
            overviews,
            selected_index: 0,
        }
    }

    pub fn overviews(&self) -> &[WorkspaceOverview] {
        &self.overviews
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn is_empty(&self) -> bool {
        self.overviews.is_empty()
    }

    /// The workspace under the cursor, or `None` when the list is empty.
    pub fn selected(&self) -> Option<&Workspace> {
        self.overviews
            .get(self.selected_index)
            .map(|o| &o.workspace)
    }

    /// Move the cursor up one row, wrapping to the bottom. No-op when empty.
    pub fn move_up(&mut self) {
        if self.overviews.is_empty() {
            return;
        }
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.overviews.len() - 1);
    }

    /// Move the cursor down one row, wrapping to the top. No-op when empty.
    pub fn move_down(&mut self) {
        if self.overviews.is_empty() {
            return;
        }
        self.selected_index = (self.selected_index + 1) % self.overviews.len();
    }

    /// Remove the workspace under the cursor. The cursor stays on the entry that
    /// shifts up into its place, or clamps to the new last row when the bottom
    /// one was removed. No-op when empty. Used after the user confirms dropping a
    /// stale workspace whose directory no longer exists.
    pub fn remove_selected(&mut self) {
        if self.overviews.is_empty() {
            return;
        }
        self.overviews.remove(self.selected_index);
        if self.selected_index >= self.overviews.len() {
            self.selected_index = self.overviews.len().saturating_sub(1);
        }
    }

    /// Move the selected workspace to the top of the list and keep the cursor on
    /// it. Used after opening a project so the most recently opened one sorts
    /// first, mirroring the persisted `updated_at` order. No-op when empty.
    pub fn promote_selected(&mut self) {
        if self.overviews.is_empty() {
            return;
        }
        let overview = self.overviews.remove(self.selected_index);
        self.overviews.insert(0, overview);
        self.selected_index = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overview(name: &str) -> WorkspaceOverview {
        WorkspaceOverview {
            workspace: Workspace::new(name, format!("/tmp/{name}")),
            session_count: 0,
            open_issue_count: 0,
        }
    }

    fn sample() -> ProjectList {
        ProjectList::new(vec![overview("a"), overview("b"), overview("c")])
    }

    #[test]
    fn new_list_starts_at_the_top() {
        let list = sample();
        assert_eq!(list.selected_index(), 0);
        assert_eq!(list.overviews().len(), 3);
        assert!(!list.is_empty());
        assert_eq!(list.selected().unwrap().name, "a");
    }

    #[test]
    fn empty_list_has_no_selection() {
        let list = ProjectList::new(Vec::new());
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
        assert_eq!(list.selected().unwrap().name, "a");
    }

    #[test]
    fn move_up_wraps_to_the_bottom() {
        let mut list = sample();
        list.move_up();
        assert_eq!(list.selected_index(), 2);
        assert_eq!(list.selected().unwrap().name, "c");
        list.move_up();
        assert_eq!(list.selected_index(), 1);
    }

    #[test]
    fn movement_is_a_noop_on_an_empty_list() {
        let mut list = ProjectList::new(Vec::new());
        list.move_up();
        assert_eq!(list.selected_index(), 0);
        list.move_down();
        assert_eq!(list.selected_index(), 0);
    }

    #[test]
    fn remove_selected_drops_the_entry_and_keeps_the_cursor_in_range() {
        let mut list = sample();
        list.move_down(); // select "b"
        list.remove_selected();
        // "b" is gone; the cursor stays at index 1, now on "c".
        let names: Vec<_> = list
            .overviews()
            .iter()
            .map(|o| o.workspace.name.as_str())
            .collect();
        assert_eq!(names, ["a", "c"]);
        assert_eq!(list.selected_index(), 1);
        assert_eq!(list.selected().unwrap().name, "c");
    }

    #[test]
    fn remove_selected_clamps_the_cursor_when_the_last_row_goes() {
        let mut list = sample();
        list.move_up(); // wraps to the last entry, "c"
        assert_eq!(list.selected_index(), 2);
        list.remove_selected();
        // The bottom row was removed, so the cursor clamps to the new last row.
        let names: Vec<_> = list
            .overviews()
            .iter()
            .map(|o| o.workspace.name.as_str())
            .collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(list.selected_index(), 1);
        assert_eq!(list.selected().unwrap().name, "b");
    }

    #[test]
    fn remove_selected_of_the_last_entry_leaves_an_empty_list() {
        let mut list = ProjectList::new(vec![overview("solo")]);
        list.remove_selected();
        assert!(list.is_empty());
        assert_eq!(list.selected_index(), 0);
        assert!(list.selected().is_none());
    }

    #[test]
    fn remove_selected_is_a_noop_on_an_empty_list() {
        let mut list = ProjectList::new(Vec::new());
        list.remove_selected();
        assert!(list.is_empty());
        assert_eq!(list.selected_index(), 0);
    }

    #[test]
    fn promote_selected_moves_the_selection_to_the_top() {
        let mut list = sample();
        list.move_down();
        list.move_down(); // select "c"
        list.promote_selected();
        // "c" is now first and stays under the cursor; the others keep order.
        let names: Vec<_> = list
            .overviews()
            .iter()
            .map(|o| o.workspace.name.as_str())
            .collect();
        assert_eq!(names, ["c", "a", "b"]);
        assert_eq!(list.selected_index(), 0);
        assert_eq!(list.selected().unwrap().name, "c");
    }

    #[test]
    fn promote_selected_is_a_noop_on_an_empty_list() {
        let mut list = ProjectList::new(Vec::new());
        list.promote_selected();
        assert!(list.is_empty());
        assert_eq!(list.selected_index(), 0);
    }
}
