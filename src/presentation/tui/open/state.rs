//! Pure, terminal-independent state for the project selection screen.
//!
//! Holds the list of registered workspaces and the cursor position. Keeping the
//! navigation logic free of any terminal IO makes it directly testable.
//!
//! The workspaces are kept in **alphabetical order** (case-insensitive by name)
//! and a **filter** narrows the list to the names matching a query the user
//! types into the search bar. Navigation and selection operate over the visible
//! (filtered) subset; the checked set used by 統合(unite) mode stays tied to the
//! underlying workspaces so a filter never silently drops a checked entry.

use crate::domain::workspace::Workspace;
use crate::usecase::workspace::WorkspaceOverview;

/// Which selection screen the user is on.
///
/// The Open screen separates the two ways of opening into distinct modes so the
/// single-open and 統合(unite) flows never blur together: [`Mode::Single`] is a
/// plain picker (no check column, `Enter` opens the cursor row alone), and
/// [`Mode::Unite`] is an explicit multi-select (a check column appears, `Space`
/// toggles membership, `Enter` opens the whole checked set together).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Single-open picker: `Enter` opens the cursor row by itself; no checks.
    Single,
    /// 統合(unite) multi-select: `Space` toggles membership, `Enter` opens the
    /// checked workspaces together.
    Unite,
}

/// One visible (filter-passing) row, handed to the renderer so it never needs to
/// map visible positions back to the underlying workspace list.
#[derive(Debug)]
pub struct Row<'a> {
    /// The workspace overview shown on this row.
    pub overview: &'a WorkspaceOverview,
    /// Whether the cursor is on this row.
    pub selected: bool,
    /// Whether this row is checked for 統合(unite) mode.
    pub checked: bool,
}

/// The selectable list of workspaces and the current cursor position.
///
/// Each entry is a [`WorkspaceOverview`] — the workspace plus the session and
/// open-issue counts shown beside it — so the screen can render the figures
/// without re-reading the disk every frame.
///
/// The screen runs in one of two [`Mode`]s. In [`Mode::Unite`] entries can be
/// *checked*: `Space` toggles the entry under the cursor, and `Enter` opens every
/// checked workspace together. The checks are tracked in `checked`, aligned 1:1
/// with `overviews`, and persist across mode switches so toggling back into unite
/// keeps the in-progress selection.
#[derive(Debug, Clone)]
pub struct ProjectList {
    /// Every registered workspace, sorted alphabetically by name
    /// (case-insensitive). The display order, independent of recency.
    overviews: Vec<WorkspaceOverview>,
    /// Whether each entry is checked for unite mode (`checked[i]` for
    /// `overviews[i]`). Kept the same length as `overviews` through every mutation.
    checked: Vec<bool>,
    /// The current search query. Empty means "show everything". Matched
    /// case-insensitively against each workspace name.
    filter: String,
    /// Indices into `overviews` that pass the current filter, in display order.
    /// Rebuilt whenever the filter or the workspace set changes.
    visible: Vec<usize>,
    /// Cursor position as an index into `visible` (not `overviews`).
    selected: usize,
    /// Which selection screen is active. Starts in [`Mode::Single`].
    mode: Mode,
    /// Names of the workspaces from the last 統合(unite) open, applied as the
    /// initial checks the first time the user enters [`Mode::Unite`] so the same
    /// union is one selection away. Empty when there is nothing to restore.
    remembered: Vec<String>,
    /// Whether the remembered set has already been applied (so re-entering unite
    /// does not re-check what the user has since unchecked).
    unite_initialized: bool,
}

impl ProjectList {
    /// Builds a list from the given workspace overviews, sorting them
    /// alphabetically by name (case-insensitive), cursor at the top and nothing
    /// checked, starting in [`Mode::Single`] with no filter.
    pub fn new(mut overviews: Vec<WorkspaceOverview>) -> Self {
        overviews.sort_by(|a, b| {
            a.workspace
                .name
                .to_lowercase()
                .cmp(&b.workspace.name.to_lowercase())
        });
        let checked = vec![false; overviews.len()];
        let visible = (0..overviews.len()).collect();
        Self {
            overviews,
            checked,
            filter: String::new(),
            visible,
            selected: 0,
            mode: Mode::Single,
            remembered: Vec::new(),
            unite_initialized: false,
        }
    }

    /// The visible (filter-passing) rows, in display order, each tagged with
    /// whether it is under the cursor and whether it is checked for unite mode.
    pub fn rows(&self) -> Vec<Row<'_>> {
        self.visible
            .iter()
            .enumerate()
            .map(|(pos, &idx)| Row {
                overview: &self.overviews[idx],
                selected: pos == self.selected,
                checked: self.checked[idx],
            })
            .collect()
    }

    pub fn overviews(&self) -> &[WorkspaceOverview] {
        &self.overviews
    }

    /// The cursor position as an index into the visible (filtered) rows.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Whether no workspaces are registered at all (independent of the filter).
    pub fn is_empty(&self) -> bool {
        self.overviews.is_empty()
    }

    /// Whether the current filter matches no workspace (the registry may still
    /// hold entries). Distinguished from [`is_empty`](Self::is_empty) so the
    /// screen can show a "no matches" hint rather than the empty-registry one.
    pub fn visible_is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    /// The current search query (empty when nothing is typed).
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Append a character to the filter and re-narrow the list, moving the
    /// cursor back to the first match.
    pub fn push_filter(&mut self, c: char) {
        self.filter.push(c);
        self.selected = 0;
        self.recompute_visible();
    }

    /// Delete the last character of the filter (no-op when empty) and re-narrow
    /// the list, moving the cursor back to the first match.
    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.selected = 0;
        self.recompute_visible();
    }

    /// Clear the filter entirely, showing every workspace again with the cursor
    /// on the first row.
    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.selected = 0;
        self.recompute_visible();
    }

    /// Rebuild `visible` from the current filter, clamping the cursor so it never
    /// points past the last visible row.
    fn recompute_visible(&mut self) {
        let query = self.filter.to_lowercase();
        self.visible = self
            .overviews
            .iter()
            .enumerate()
            .filter(|(_, o)| query.is_empty() || o.workspace.name.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.visible.len() {
            self.selected = self.visible.len().saturating_sub(1);
        }
    }

    /// The index into `overviews` of the row under the cursor, or `None` when no
    /// row is visible.
    fn cursor_overview(&self) -> Option<usize> {
        self.visible.get(self.selected).copied()
    }

    /// The active selection mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Record the workspace names from the last 統合(unite) open so the first
    /// switch into [`Mode::Unite`] restores that union. Stored verbatim; names no
    /// longer registered are ignored when applied.
    pub fn remember(&mut self, names: Vec<String>) {
        self.remembered = names;
    }

    /// Switch into [`Mode::Unite`] without touching the checks. Callers decide
    /// what the initial selection is: `Space` selects the row it was pressed on
    /// ([`ProjectList::select_cursor`]); `u` restores the remembered union
    /// ([`ProjectList::restore_remembered`]).
    pub fn enter_unite(&mut self) {
        self.mode = Mode::Unite;
    }

    /// Check the cursor row, leaving it checked if it already was. Used when
    /// `Space` enters unite from the picker so the workspace the user pressed
    /// `Space` on is the one selected. No-op when the list is empty.
    pub fn select_cursor(&mut self) {
        if let Some(idx) = self.cursor_overview() {
            self.checked[idx] = true;
        }
    }

    /// Restore the remembered union (see [`ProjectList::remember`]) the first
    /// time unite is entered via `u`, moving the cursor to the first restored
    /// entry; thereafter the existing checks are kept. Seeds the cursor row when
    /// there is nothing to restore so the unite set is never empty.
    pub fn restore_remembered(&mut self) {
        if !self.unite_initialized {
            self.unite_initialized = true;
            let remembered = std::mem::take(&mut self.remembered);
            self.preselect(&remembered);
        }
        if self.checked_count() == 0 {
            self.select_cursor();
        }
    }

    /// Leave [`Mode::Unite`] back to the single-open picker. The checks are kept
    /// so switching back into unite resumes the selection. No-op when already in
    /// [`Mode::Single`].
    pub fn exit_unite(&mut self) {
        self.mode = Mode::Single;
    }

    /// Whether the entry at `index` is checked for unite mode.
    ///
    /// `index` is a **visible** position (matching [`selected_index`](Self::selected_index)),
    /// so it lines up with the rows the screen draws.
    pub fn is_checked(&self, index: usize) -> bool {
        self.visible
            .get(index)
            .map(|&i| self.checked[i])
            .unwrap_or(false)
    }

    /// How many entries are checked for unite mode.
    pub fn checked_count(&self) -> usize {
        self.checked.iter().filter(|&&c| c).count()
    }

    /// Toggle the unite-mode check on the entry under the cursor. No-op when empty.
    pub fn toggle_checked(&mut self) {
        if let Some(idx) = self.cursor_overview() {
            self.checked[idx] = !self.checked[idx];
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
        if let Some(pos) = self.visible.iter().position(|&i| self.checked[i]) {
            self.selected = pos;
        }
    }

    /// The workspaces to open. In [`Mode::Single`] this is just the cursor row,
    /// so `Enter` is always predictable. In [`Mode::Unite`] it is every checked
    /// one in list order; if none are checked, nothing is chosen yet.
    /// `Enter` opens these together (one workspace → single-workspace home, more
    /// → 統合(unite) mode).
    pub fn chosen(&self) -> Vec<Workspace> {
        if self.mode == Mode::Single {
            return self.selected().cloned().into_iter().collect();
        }
        self.overviews
            .iter()
            .zip(&self.checked)
            .filter(|(_, &c)| c)
            .map(|(o, _)| o.workspace.clone())
            .collect()
    }

    /// Move the cursor to the entry named `name`, returning whether one matched.
    /// Used to land on a chosen-but-missing workspace so the removal prompt acts
    /// on the right entry. When an active filter hides the match, the filter is
    /// cleared so the entry becomes visible and the cursor can land on it.
    pub fn focus_name(&mut self, name: &str) -> bool {
        let Some(idx) = self.overviews.iter().position(|o| o.workspace.name == name) else {
            return false;
        };
        if !self.visible.contains(&idx) {
            self.filter.clear();
            self.recompute_visible();
        }
        if let Some(pos) = self.visible.iter().position(|&i| i == idx) {
            self.selected = pos;
        }
        true
    }

    /// The workspace under the cursor, or `None` when the list is empty.
    pub fn selected(&self) -> Option<&Workspace> {
        self.cursor_overview().map(|i| &self.overviews[i].workspace)
    }

    /// Move the cursor up one row, wrapping to the bottom. No-op when empty.
    pub fn move_up(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.visible.len().saturating_sub(1));
    }

    /// Move the cursor down one row, wrapping to the top. No-op when empty.
    pub fn move_down(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.visible.len();
    }

    /// Remove the workspace under the cursor. The cursor stays on the entry that
    /// shifts up into its place, or clamps to the new last row when the bottom
    /// one was removed. No-op when empty. Used after the user confirms dropping a
    /// stale workspace whose directory no longer exists.
    pub fn remove_selected(&mut self) {
        let Some(idx) = self.cursor_overview() else {
            return;
        };
        self.overviews.remove(idx);
        self.checked.remove(idx);
        // Rebuild the visible set; the cursor keeps its numeric position so it
        // lands on the row that shifted up, clamped to the new last row.
        self.recompute_visible();
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
        assert_eq!(list.mode(), Mode::Single);
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
    fn new_list_sorts_workspaces_alphabetically_by_name() {
        let list = ProjectList::new(vec![
            overview("charlie"),
            overview("Alpha"),
            overview("beta"),
        ]);
        let names: Vec<_> = list
            .overviews()
            .iter()
            .map(|o| o.workspace.name.as_str())
            .collect();
        assert_eq!(names, ["Alpha", "beta", "charlie"]);
        assert_eq!(list.selected().unwrap().name, "Alpha");
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
        list.enter_unite(); // switches `chosen` to checked rows
        list.select_cursor(); // select "a"
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
    fn single_mode_ignores_checks_when_choosing() {
        let mut list = sample(); // a, b, c
        list.toggle_checked(); // check "a" (kept for unite, ignored by single)
        list.move_down(); // cursor on "b"
        assert_eq!(list.mode(), Mode::Single);
        let names: Vec<_> = list.chosen().into_iter().map(|w| w.name).collect();
        assert_eq!(names, ["b"]);
    }

    #[test]
    fn select_cursor_checks_the_row_space_was_pressed_on() {
        let mut list = sample(); // a, b, c
        list.move_down(); // cursor on "b"
        list.enter_unite();
        list.select_cursor();
        assert_eq!(list.mode(), Mode::Unite);
        assert!(list.is_checked(1));
        // Selecting the same row again keeps it checked (it does not toggle off).
        list.select_cursor();
        assert!(list.is_checked(1));
        let names: Vec<_> = list.chosen().into_iter().map(|w| w.name).collect();
        assert_eq!(names, ["b"]);
    }

    #[test]
    fn select_cursor_is_a_noop_on_an_empty_list() {
        let mut list = ProjectList::new(Vec::new());
        list.enter_unite();
        list.select_cursor();
        assert_eq!(list.checked_count(), 0);
    }

    #[test]
    fn exit_unite_returns_to_single_without_clearing_checks() {
        let mut list = sample(); // a, b, c
        list.enter_unite();
        list.select_cursor(); // check "a"
        assert!(list.is_checked(0));
        list.exit_unite();
        assert_eq!(list.mode(), Mode::Single);
        assert!(list.is_checked(0));
        // Single-open is still just the cursor row even while checks exist.
        list.move_down(); // cursor on "b"
        let names: Vec<_> = list.chosen().into_iter().map(|w| w.name).collect();
        assert_eq!(names, ["b"]);
    }

    #[test]
    fn restore_remembered_applies_the_remembered_set_only_once() {
        let mut list = sample(); // a, b, c
        list.remember(vec!["b".to_string(), "c".to_string()]);
        list.enter_unite();
        list.restore_remembered();
        assert!(list.is_checked(1));
        assert!(list.is_checked(2));
        assert_eq!(list.selected_index(), 1);

        // User edits the set, then leaves and re-enters. The remembered set is
        // not re-applied over the user's in-progress changes.
        list.toggle_checked(); // uncheck "b"
        list.exit_unite();
        list.enter_unite();
        list.restore_remembered();
        assert!(!list.is_checked(1));
        assert!(list.is_checked(2));
    }

    #[test]
    fn restore_remembered_with_no_match_seeds_the_cursor_row() {
        let mut list = sample(); // a, b, c
        list.move_down(); // cursor on "b"
        list.remember(vec!["ghost".to_string()]);
        list.enter_unite();
        list.restore_remembered();
        assert!(list.is_checked(1));
        assert_eq!(list.checked_count(), 1);
    }

    #[test]
    fn restore_remembered_seeds_the_cursor_on_an_empty_list() {
        let mut list = ProjectList::new(Vec::new());
        list.enter_unite();
        list.restore_remembered();
        assert_eq!(list.checked_count(), 0);
    }

    #[test]
    fn unite_mode_with_nothing_checked_has_no_chosen_workspaces() {
        let mut list = sample(); // a, b, c
        list.enter_unite(); // unite, but nothing selected yet
        assert_eq!(list.mode(), Mode::Unite);
        assert_eq!(list.checked_count(), 0);
        // Nothing checked means nothing is chosen yet.
        let names: Vec<_> = list.chosen().into_iter().map(|w| w.name).collect();
        assert!(names.is_empty());
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
    fn filter_matches_names_case_insensitively_and_moves_the_cursor_to_matches() {
        let mut list = ProjectList::new(vec![
            overview("alpha"),
            overview("Beta"),
            overview("alphabet"),
            overview("gamma"),
        ]);
        list.push_filter('A');
        list.push_filter('l');
        let names: Vec<_> = list
            .rows()
            .into_iter()
            .map(|row| row.overview.workspace.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "alphabet"]);
        assert_eq!(list.selected().unwrap().name, "alpha");
        assert!(!list.visible_is_empty());
    }

    #[test]
    fn clearing_filter_restores_all_rows_without_losing_checks() {
        let mut list = sample(); // a, b, c
        list.move_down(); // b
        list.toggle_checked();
        list.push_filter('c');
        assert_eq!(list.rows().len(), 1);
        assert_eq!(list.selected().unwrap().name, "c");
        list.clear_filter();
        assert_eq!(list.rows().len(), 3);
        assert!(list.is_checked(1)); // b is visible at index 1 again
        assert_eq!(list.checked_count(), 1);
    }

    #[test]
    fn focus_name_clears_a_filter_that_hides_the_target() {
        let mut list = sample(); // a, b, c
        list.push_filter('a');
        assert_eq!(list.rows().len(), 1);
        assert!(list.focus_name("c"));
        assert_eq!(list.filter(), "");
        assert_eq!(list.selected().unwrap().name, "c");
    }
}
