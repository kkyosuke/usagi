//! The home screen's transient modal / inline-input state: the Switch inline
//! create input, the scrollable text modal, and the session-removal checklist.

use std::collections::HashSet;

use super::LogLine;

/// The inline session-name input shown in the left pane while creating a session
/// from 切替 (Switch): the name being typed plus an optional inline validation
/// error (e.g. an empty or duplicate name). Read through [`HomeState`]'s
/// `create_input` / `create_error` accessors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CreateInput {
    pub(super) input: String,
    pub(super) error: Option<String>,
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

/// The open session-removal modal: the workspace's session names with a
/// checklist the user toggles to pick which to delete in one go. A cursor marks
/// the row the keyboard acts on, `selected` holds the checked rows, and `force`
/// carries the `--force` flag from `session remove --force` so the confirmed
/// removal can discard uncommitted changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveModal {
    pub(super) names: Vec<String>,
    pub(super) cursor: usize,
    pub(super) selected: HashSet<usize>,
    pub(super) force: bool,
}

impl RemoveModal {
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
}
