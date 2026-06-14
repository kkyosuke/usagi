//! A reusable, terminal-independent searchable list.
//!
//! [`Picker`] holds a set of entries, a typed search query, and a cursor over
//! the entries that match the query (case-insensitive substring). Keeping it
//! free of any terminal IO makes it directly testable and lets any screen reuse
//! it for a "type to filter, arrow to choose" interaction.

/// A list of entries filtered live by a search query, with a cursor over the
/// matches.
#[derive(Debug, Clone)]
pub struct Picker {
    /// Every entry, in their original order.
    entries: Vec<String>,
    /// The current search query.
    query: String,
    /// Indices into `entries` that match `query`, in order.
    matches: Vec<usize>,
    /// Cursor position within `matches`.
    cursor: usize,
}

impl Picker {
    /// Builds a picker over `entries` with an empty query (so every entry
    /// matches) and the cursor at the top.
    pub fn new(entries: Vec<String>) -> Self {
        let mut picker = Self {
            entries,
            query: String::new(),
            matches: Vec::new(),
            cursor: 0,
        };
        picker.refilter();
        picker
    }

    /// The current search query.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The cursor position within the current matches.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The entries matching the current query, in order.
    pub fn matches(&self) -> Vec<&str> {
        self.matches
            .iter()
            .map(|&i| self.entries[i].as_str())
            .collect()
    }

    /// Whether the query matches no entries.
    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    /// The entry under the cursor, or `None` when nothing matches.
    pub fn selected(&self) -> Option<&str> {
        self.matches
            .get(self.cursor)
            .map(|&i| self.entries[i].as_str())
    }

    /// Append a character to the query and re-filter.
    pub fn insert_char(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    /// Delete the last character of the query and re-filter.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.refilter();
    }

    /// Replace the entries (e.g. after navigating elsewhere), clearing the query
    /// and resetting the cursor.
    pub fn set_entries(&mut self, entries: Vec<String>) {
        self.entries = entries;
        self.query.clear();
        self.refilter();
    }

    /// Move the cursor up one match, wrapping to the bottom. No-op with no
    /// matches.
    pub fn move_up(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.cursor = self.cursor.checked_sub(1).unwrap_or(self.matches.len() - 1);
    }

    /// Move the cursor down one match, wrapping to the top. No-op with no
    /// matches.
    pub fn move_down(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.matches.len();
    }

    /// Recompute the matches for the current query and clamp the cursor to the
    /// top: a case-insensitive substring test against each entry.
    fn refilter(&mut self) {
        let needle = self.query.to_lowercase();
        self.matches = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        self.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Picker {
        Picker::new(vec![
            "src".to_string(),
            "tests".to_string(),
            "docs".to_string(),
        ])
    }

    #[test]
    fn new_picker_matches_everything_with_the_cursor_at_the_top() {
        let picker = sample();
        assert_eq!(picker.query(), "");
        assert_eq!(picker.cursor(), 0);
        assert_eq!(picker.matches(), vec!["src", "tests", "docs"]);
        assert_eq!(picker.selected(), Some("src"));
        assert!(!picker.is_empty());
    }

    #[test]
    fn typing_filters_case_insensitively_and_resets_the_cursor() {
        let mut picker = sample();
        picker.move_down(); // cursor on "tests"
        picker.insert_char('T'); // matches "tests" only (case-insensitive)
        assert_eq!(picker.query(), "T");
        assert_eq!(picker.matches(), vec!["tests"]);
        // Filtering resets the cursor to the top.
        assert_eq!(picker.cursor(), 0);
        assert_eq!(picker.selected(), Some("tests"));
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        let mut picker = sample();
        picker.insert_char('d');
        picker.insert_char('o');
        assert_eq!(picker.matches(), vec!["docs"]);
        picker.backspace();
        picker.backspace();
        assert_eq!(picker.query(), "");
        assert_eq!(picker.matches(), vec!["src", "tests", "docs"]);
    }

    #[test]
    fn a_query_matching_nothing_leaves_no_selection() {
        let mut picker = sample();
        picker.insert_char('z');
        assert!(picker.is_empty());
        assert_eq!(picker.matches(), Vec::<&str>::new());
        assert_eq!(picker.selected(), None);
        // Movement on an empty match set is a no-op.
        picker.move_down();
        picker.move_up();
        assert_eq!(picker.cursor(), 0);
    }

    #[test]
    fn movement_wraps_around_the_matches() {
        let mut picker = sample();
        picker.move_down();
        assert_eq!(picker.selected(), Some("tests"));
        picker.move_down();
        picker.move_down();
        // Wraps from the last match back to the first.
        assert_eq!(picker.selected(), Some("src"));
        picker.move_up();
        // Wraps from the first match to the last.
        assert_eq!(picker.selected(), Some("docs"));
    }

    #[test]
    fn set_entries_replaces_the_list_and_clears_the_query() {
        let mut picker = sample();
        picker.insert_char('s');
        picker.move_down();
        picker.set_entries(vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(picker.query(), "");
        assert_eq!(picker.cursor(), 0);
        assert_eq!(picker.matches(), vec!["alpha", "beta"]);
    }
}
