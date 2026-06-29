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
///
/// Entries can be *checked* for 統合(unite) mode: `Space` toggles the entry under
/// the cursor, and `Enter` opens every checked workspace together (or just the
/// cursor's when none are checked). The checks are tracked in `checked`, aligned
/// 1:1 with `overviews`.
#[derive(Debug, Clone)]
pub struct ProjectList {
    overviews: Vec<WorkspaceOverview>,
    /// Whether each entry is checked for unite mode (`checked[i]` for
    /// `overviews[i]`). Kept the same length as `overviews` through every mutation.
    checked: Vec<bool>,
    selected_index: usize,
}

impl ProjectList {
    /// Builds a list from the given workspace overviews, cursor at the top and
    /// nothing checked.
    pub fn new(overviews: Vec<WorkspaceOverview>) -> Self {
        let checked = vec![false; overviews.len()];
        Self {
            overviews,
            checked,
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

    /// Whether the entry at `index` is checked for unite mode.
    pub fn is_checked(&self, index: usize) -> bool {
        self.checked.get(index).copied().unwrap_or(false)
    }

    /// How many entries are checked for unite mode.
    pub fn checked_count(&self) -> usize {
        self.checked.iter().filter(|&&c| c).count()
    }

    /// Toggle the unite-mode check on the entry under the cursor. No-op when empty.
    pub fn toggle_checked(&mut self) {
        if let Some(checked) = self.checked.get_mut(self.selected_index) {
            *checked = !*checked;
        }
    }

    /// Check the entries whose workspace name is in `names` (restoring a
    /// remembered unite set), moving the cursor to the first checked one so the
    /// restored selection is visible. Names not present are ignored.
    pub fn preselect(&mut self, names: &[String]) {
        for (i, overview) in self.overviews.iter().enumerate() {
            if names.iter().any(|n| n == &overview.workspace.name) {
                self.checked[i] = true;
            }
        }
        if let Some(first) = self.checked.iter().position(|&c| c) {
            self.selected_index = first;
        }
    }

    /// The workspaces to open: every checked one in list order, or — when none
    /// are checked — just the one under the cursor. Empty only when the list is.
    /// `Enter` opens these together (one workspace → single-workspace home, more →
    /// 統合(unite) mode).
    pub fn chosen(&self) -> Vec<Workspace> {
        let checked: Vec<Workspace> = self
            .overviews
            .iter()
            .zip(&self.checked)
            .filter(|(_, &c)| c)
            .map(|(o, _)| o.workspace.clone())
            .collect();
        if !checked.is_empty() {
            return checked;
        }
        self.selected().cloned().into_iter().collect()
    }

    /// Move the cursor to the entry named `name`, returning whether one matched.
    /// Used to land on a chosen-but-missing workspace so the removal prompt acts
    /// on the right entry.
    pub fn focus_name(&mut self, name: &str) -> bool {
        match self.overviews.iter().position(|o| o.workspace.name == name) {
            Some(index) => {
                self.selected_index = index;
                true
            }
            None => false,
        }
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
            .unwrap_or(self.overviews.len().saturating_sub(1));
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
        self.checked.remove(self.selected_index);
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
        let checked = self.checked.remove(self.selected_index);
        self.checked.insert(0, checked);
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
            pr_count: 0,
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

    // --- unite-mode multi-select ---------------------------------------------

    #[test]
    fn nothing_is_checked_by_default_and_enter_opens_the_cursor_row() {
        let list = sample(); // a, b, c
        assert_eq!(list.checked_count(), 0);
        assert!(!list.is_checked(0));
        // With nothing checked, `chosen` is just the cursor's workspace.
        let names: Vec<_> = list.chosen().into_iter().map(|w| w.name).collect();
        assert_eq!(names, ["a"]);
    }

    #[test]
    fn toggling_checks_the_cursor_row_and_chosen_lists_every_checked_one() {
        let mut list = sample(); // a, b, c
        list.toggle_checked(); // check "a"
        list.move_down();
        list.move_down(); // cursor on "c"
        list.toggle_checked(); // check "c"
        assert_eq!(list.checked_count(), 2);
        assert!(list.is_checked(0));
        assert!(!list.is_checked(1));
        assert!(list.is_checked(2));
        // `chosen` lists the checked workspaces in list order, ignoring the cursor.
        let names: Vec<_> = list.chosen().into_iter().map(|w| w.name).collect();
        assert_eq!(names, ["a", "c"]);
        // Toggling again unchecks.
        list.toggle_checked();
        assert!(!list.is_checked(2));
        assert_eq!(list.checked_count(), 1);
    }

    #[test]
    fn toggle_is_a_noop_on_an_empty_list() {
        let mut list = ProjectList::new(Vec::new());
        list.toggle_checked();
        assert_eq!(list.checked_count(), 0);
        assert!(list.chosen().is_empty());
    }

    #[test]
    fn preselect_checks_named_workspaces_and_moves_the_cursor_to_the_first() {
        let mut list = sample(); // a, b, c
        list.preselect(&["b".to_string(), "c".to_string(), "ghost".to_string()]);
        assert!(!list.is_checked(0));
        assert!(list.is_checked(1));
        assert!(list.is_checked(2));
        // Cursor jumps to the first restored entry so the selection is visible.
        assert_eq!(list.selected_index(), 1);
        // The unknown name is ignored.
        assert_eq!(list.checked_count(), 2);
    }

    #[test]
    fn preselect_with_no_matches_leaves_the_cursor_put() {
        let mut list = sample();
        list.move_down(); // cursor on "b"
        list.preselect(&["ghost".to_string()]);
        assert_eq!(list.checked_count(), 0);
        assert_eq!(list.selected_index(), 1);
    }

    #[test]
    fn remove_selected_keeps_the_checks_aligned() {
        let mut list = sample(); // a, b, c
        list.move_down(); // cursor on "b"
        list.toggle_checked(); // check "b"
        list.move_down(); // cursor on "c"
        list.toggle_checked(); // check "c"
        list.move_up(); // back to "b"
        list.remove_selected(); // drop "b" (was checked)
                                // "c" is now at index 1 and its check rides along.
        let names: Vec<_> = list
            .overviews()
            .iter()
            .map(|o| o.workspace.name.as_str())
            .collect();
        assert_eq!(names, ["a", "c"]);
        assert!(!list.is_checked(0)); // a
        assert!(list.is_checked(1)); // c
    }

    #[test]
    fn focus_name_moves_the_cursor_to_the_match_or_reports_missing() {
        let mut list = sample(); // a, b, c
        assert!(list.focus_name("c"));
        assert_eq!(list.selected_index(), 2);
        assert!(!list.focus_name("ghost"));
        // A failed lookup leaves the cursor put.
        assert_eq!(list.selected_index(), 2);
    }

    #[test]
    fn promote_selected_carries_the_check_to_the_top() {
        let mut list = sample(); // a, b, c
        list.move_down();
        list.move_down(); // cursor on "c"
        list.toggle_checked(); // check "c"
        list.promote_selected(); // move "c" to the top
        let names: Vec<_> = list
            .overviews()
            .iter()
            .map(|o| o.workspace.name.as_str())
            .collect();
        assert_eq!(names, ["c", "a", "b"]);
        // The check followed "c" to index 0.
        assert!(list.is_checked(0));
        assert_eq!(list.checked_count(), 1);
    }
}
