//! Pure, terminal-independent state for the project selection screen.
//!
//! Holds the list of registered workspaces and the cursor position. Keeping the
//! navigation logic free of any terminal IO makes it directly testable.

use crate::domain::workspace::Workspace;

/// The selectable list of workspaces and the current cursor position.
#[derive(Debug, Clone)]
pub struct ProjectList {
    workspaces: Vec<Workspace>,
    selected_index: usize,
}

impl ProjectList {
    /// Builds a list from the given workspaces, with the cursor at the top.
    pub fn new(workspaces: Vec<Workspace>) -> Self {
        Self {
            workspaces,
            selected_index: 0,
        }
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn is_empty(&self) -> bool {
        self.workspaces.is_empty()
    }

    /// The workspace under the cursor, or `None` when the list is empty.
    pub fn selected(&self) -> Option<&Workspace> {
        self.workspaces.get(self.selected_index)
    }

    /// Move the cursor up one row, wrapping to the bottom. No-op when empty.
    pub fn move_up(&mut self) {
        if self.workspaces.is_empty() {
            return;
        }
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.workspaces.len() - 1);
    }

    /// Move the cursor down one row, wrapping to the top. No-op when empty.
    pub fn move_down(&mut self) {
        if self.workspaces.is_empty() {
            return;
        }
        self.selected_index = (self.selected_index + 1) % self.workspaces.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(name: &str) -> Workspace {
        Workspace::new(name, format!("/tmp/{name}"))
    }

    fn sample() -> ProjectList {
        ProjectList::new(vec![workspace("a"), workspace("b"), workspace("c")])
    }

    #[test]
    fn new_list_starts_at_the_top() {
        let list = sample();
        assert_eq!(list.selected_index(), 0);
        assert_eq!(list.workspaces().len(), 3);
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
}
