//! The home screen's transient sub-mode state: the 切替 (Switch) inline
//! create/rename inputs, the 在席 (Focus) menu cursor, the scrollable text
//! modal, and the session-removal checklist.
//!
//! Each sub-mode is its own type owning its editing/navigation logic and
//! invariants, so [`HomeState`](super::HomeState) only holds the optional state
//! and routes to it — the display- and cursor-level behaviour lives here, not as
//! flat forwarding methods on the screen state.

use std::collections::HashSet;

use super::LogLine;
use crate::presentation::tui::markdown::MarkdownLine;
use crate::presentation::tui::widgets::text_input::TextInput;

/// The inline session-name input shown in the left pane while creating a session
/// from 切替 (Switch): the name being typed, the existing branch names it is
/// validated against, and an optional inline validation error (e.g. an empty,
/// duplicate, or branch-namespace-clashing name). The name is re-validated on
/// every keystroke so the error appears live.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateInput {
    input: TextInput,
    /// Branch names already taken across the workspace's repositories, captured
    /// when the input opened; the typed name must not duplicate or nest under
    /// any of them.
    taken: Vec<String>,
    error: Option<String>,
}

impl CreateInput {
    /// Open an empty create input validated against the branch names `taken`.
    pub(super) fn new(taken: Vec<String>) -> Self {
        Self {
            input: TextInput::new(),
            taken,
            error: None,
        }
    }

    /// The name typed so far.
    pub fn value(&self) -> &str {
        self.input.value()
    }

    /// The caret position (byte offset) in the typed name.
    pub fn cursor(&self) -> usize {
        self.input.cursor()
    }

    /// The current inline validation error, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Insert a character at the caret, re-validating live.
    pub fn push_char(&mut self, c: char) {
        self.input.insert(c);
        self.revalidate();
    }

    /// Delete the character before the caret, re-validating live.
    pub fn backspace(&mut self) {
        self.input.backspace();
        self.revalidate();
    }

    /// Delete the character at the caret (the `Del` key), re-validating live.
    pub fn delete_forward(&mut self) {
        self.input.delete_forward();
        self.revalidate();
    }

    /// Move the caret one character left.
    pub fn move_left(&mut self) {
        self.input.move_left();
    }

    /// Move the caret one character right.
    pub fn move_right(&mut self) {
        self.input.move_right();
    }

    /// Move the caret to the start of the name.
    pub fn move_home(&mut self) {
        self.input.move_home();
    }

    /// Move the caret to the end of the name.
    pub fn move_end(&mut self) {
        self.input.move_end();
    }

    /// Validate and accept the typed name. On success returns the trimmed name;
    /// on an invalid name (empty, a path separator, a duplicate, or a
    /// branch-namespace clash) the inline error is set and `None` is returned, so
    /// the input stays open with the reason shown.
    pub(super) fn confirm(&mut self) -> Option<String> {
        let name = self.input.value().trim().to_string();
        // Enter on an empty name is the one case live validation stays quiet
        // about (it does not nag while nothing is typed), so guard it here.
        if name.is_empty() {
            self.error = Some("Name must not be empty.".to_string());
            return None;
        }
        if let Some(error) = validate_session_name(self.input.value(), &self.taken) {
            self.error = Some(error);
            return None;
        }
        Some(name)
    }

    fn revalidate(&mut self) {
        self.error = validate_session_name(self.input.value(), &self.taken);
    }
}

/// The inline display-name input shown in the left pane while renaming a session
/// from 切替 (Switch): the session whose sidebar label is being edited
/// (`target`, its branch name / identity, which never changes) and the label
/// being typed (`input`, pre-filled with the current label). An empty input — or
/// one equal to `target` — clears the override on confirm.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenameInput {
    target: String,
    input: String,
}

impl RenameInput {
    /// Open a rename input for session `target`, pre-filled with `label`.
    pub(super) fn new(target: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            input: label.into(),
        }
    }

    /// The name of the session being renamed (its branch / identity).
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The label typed so far.
    pub fn value(&self) -> &str {
        &self.input
    }

    /// Append a character to the label.
    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
    }

    /// Delete the last character of the label.
    pub fn backspace(&mut self) {
        self.input.pop();
    }

    /// Accept the rename, consuming the input: the target session name and the
    /// typed label (trimmed). An empty label means "clear the override", which
    /// the usecase resolves.
    pub(super) fn confirm(self) -> (String, String) {
        (self.target, self.input.trim().to_string())
    }
}

/// An open scrollable text modal: the read-only output of a text-dumping command
/// (`man` / `history` / `session list`). `scroll` is the index of the first
/// visible body line, advanced by the arrow / page keys and clamped on render.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextModal {
    pub title: String,
    pub lines: Vec<LogLine>,
    pub scroll: usize,
}

/// The right-pane Markdown preview, opened by the `preview` command. It takes
/// over the right pane (the third right-pane state alongside the command
/// history/output and the live terminal) and shows a rendered Markdown file:
/// `title` is the file's workspace-relative path, `lines` its rendered Markdown,
/// and `scroll` the index of the first visible content line. Because the preview
/// can be taller than the pane, it scrolls within the pane (the TUI itself never
/// scrolls); the scroll is advanced by the arrow / page keys and clamped on
/// render. While open it captures the keys (scroll / dismiss).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    pub title: String,
    pub lines: Vec<MarkdownLine>,
    pub scroll: usize,
}

/// The open session-removal modal: the workspace's session names with a
/// checklist the user toggles to pick which to delete in one go. A cursor marks
/// the row the keyboard acts on, `selected` holds the checked rows, and `force`
/// carries the `--force` flag from `session remove --force` so the confirmed
/// removal can discard uncommitted changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveModal {
    names: Vec<String>,
    cursor: usize,
    selected: HashSet<usize>,
    force: bool,
}

impl RemoveModal {
    /// Open the modal over `names`, nothing checked, carrying the `--force` flag.
    pub(super) fn new(names: Vec<String>, force: bool) -> Self {
        Self {
            names,
            cursor: 0,
            selected: HashSet::new(),
            force,
        }
    }

    /// The session names, in display order.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The row the keyboard cursor sits on.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the row at `index` is checked for removal.
    pub fn is_selected(&self, index: usize) -> bool {
        self.selected.contains(&index)
    }

    /// How many sessions are checked for removal.
    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    /// Whether there are no sessions to remove.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Move the cursor up one row, wrapping to the bottom. No-op with no sessions.
    pub fn move_up(&mut self) {
        if self.names.is_empty() {
            return;
        }
        self.cursor = self.cursor.checked_sub(1).unwrap_or(self.names.len() - 1);
    }

    /// Move the cursor down one row, wrapping to the top. No-op with no sessions.
    pub fn move_down(&mut self) {
        if self.names.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.names.len();
    }

    /// Toggle the checked state of the session under the cursor. No-op with no
    /// sessions.
    pub fn toggle(&mut self) {
        if self.names.is_empty() {
            return;
        }
        if !self.selected.insert(self.cursor) {
            self.selected.remove(&self.cursor);
        }
    }

    /// The checked session names (in display order) together with the `--force`
    /// flag, or `None` when nothing is checked (so the modal stays open).
    pub(super) fn confirm(&self) -> Option<(Vec<String>, bool)> {
        if self.selected.is_empty() {
            return None;
        }
        let names = self
            .names
            .iter()
            .enumerate()
            .filter(|(i, _)| self.selected.contains(i))
            .map(|(_, name)| name.clone())
            .collect();
        Some((names, self.force))
    }
}

/// The 在席 (Focus) menu cursor: which Session-scope command is highlighted. The
/// Session-scope command list is always non-empty, so the navigation methods
/// take the current `count` and keep the cursor underflow-safe and in range.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct FocusMenu {
    cursor: usize,
}

impl FocusMenu {
    /// The highlighted row.
    pub(super) fn cursor(self) -> usize {
        self.cursor
    }

    /// Reset the cursor to the top (entering 在席 / leaving for 統括).
    pub(super) fn reset(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor up one row, wrapping. `count` is clamped to at least 1.
    pub(super) fn move_up(&mut self, count: usize) {
        let count = count.max(1);
        self.cursor = self.cursor.checked_sub(1).unwrap_or(count - 1);
    }

    /// Move the cursor down one row, wrapping. `count` is clamped to at least 1.
    pub(super) fn move_down(&mut self, count: usize) {
        let count = count.max(1);
        self.cursor = (self.cursor + 1) % count;
    }

    /// The selected row, clamped to the available `count`.
    pub(super) fn selected(self, count: usize) -> usize {
        self.cursor.min(count.saturating_sub(1))
    }
}

/// Validate a typed session name against the branch names already taken, used
/// for the live inline-create feedback. Returns the reason the name cannot be
/// used, or `None` when it is usable.
///
/// An empty (or all-whitespace) name returns `None` — the input does not nag
/// while nothing has been typed; the empty case is rejected only on Enter (see
/// [`CreateInput::confirm`]). The checks mirror what
/// [`crate::usecase::session::create`] enforces, so the inline message matches
/// the eventual outcome:
///
/// - a path separator (`/`, `\`, `.`, `..`) — not a legal session name;
/// - an exact duplicate of an existing branch;
/// - a clash with an existing branch nested under `<name>/` (git cannot create
///   the `<name>` branch alongside `<name>/…`).
fn validate_session_name(name: &str, taken: &[String]) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Some("\"/\" cannot be used in a name.".to_string());
    }
    if taken.iter().any(|b| b == name) {
        return Some(format!("\"{name}\" already exists."));
    }
    let prefix = format!("{name}/");
    if let Some(conflict) = taken.iter().find(|b| b.starts_with(&prefix)) {
        return Some(format!("\"{name}\" conflicts with branch \"{conflict}\"."));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_session_name_flags_empty_separators_duplicates_and_nesting() {
        // An empty / whitespace name is quiet (the input does not nag).
        assert_eq!(validate_session_name("", &[]), None);
        assert_eq!(validate_session_name("   ", &[]), None);
        // Path separators are illegal.
        assert!(validate_session_name("a/b", &[])
            .unwrap()
            .contains("cannot be used"));
        assert!(validate_session_name("a\\b", &[])
            .unwrap()
            .contains("cannot be used"));
        assert!(validate_session_name(".", &[])
            .unwrap()
            .contains("cannot be used"));
        // An exact duplicate is reported.
        let taken = vec!["feature".to_string()];
        assert!(validate_session_name("feature", &taken)
            .unwrap()
            .contains("already exists"));
        // A clash with a nested branch is reported.
        let taken = vec!["feature/x".to_string()];
        assert!(validate_session_name("feature", &taken)
            .unwrap()
            .contains("conflicts with branch"));
        // A free name is usable.
        assert_eq!(validate_session_name("wip", &taken), None);
    }
}
