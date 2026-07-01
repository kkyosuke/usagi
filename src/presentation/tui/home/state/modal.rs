//! The home screen's transient sub-mode state: the 切替 (Switch) inline
//! create/rename inputs, the 在席 (Focus) menu cursor, the scrollable text
//! modal, and the session-removal checklist.
//!
//! Each sub-mode is its own type owning its editing/navigation logic and
//! invariants, so [`HomeState`](super::HomeState) only holds the optional state
//! and routes to it — the display- and cursor-level behaviour lives here, not as
//! flat forwarding methods on the screen state.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::LogLine;
use crate::presentation::tui::markdown::MarkdownLine;
use crate::presentation::tui::widgets::text_area::TextArea;
use crate::presentation::tui::widgets::text_input::TextInput;

/// The home screen's transient overlay — the single sub-state that captures the
/// keyboard while open and is drawn on top of the normal panes. At most one is
/// open at a time, so they form one enum rather than a struct of independent
/// `Option`s: the type makes "two overlays open at once" unrepresentable, and
/// [`HomeState`](super::HomeState) routes to whichever variant is active.
///
/// The open/close/scroll logic stays on the individual payload types (and on the
/// screen's thin accessor methods that read these). The quit-confirmation modal
/// is *not* here: it can overlay any of these (a `Ctrl-C` raises it without
/// dismissing what is already shown, and cancelling it returns to that), so the
/// screen tracks it as a separate flag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) enum Overlay {
    /// No overlay open — the normal panes have the keyboard.
    #[default]
    None,
    /// The inline session-name input open while creating a session from 切替.
    Create(CreateInput),
    /// The inline display-name input open while renaming a session from 切替.
    Rename(RenameInput),
    /// A context menu opened by right-clicking a live-pane tab chip.
    TabMenu(TabMenu),
    /// The inline label input opened from the tab context menu.
    TabRename(TabRenameInput),
    /// The session-removal checklist modal.
    Remove(RemoveModal),
    /// The scrollable text modal (a text-dumping command's output).
    Text(TextModal),
    /// The right-pane Markdown preview.
    Preview(Preview),
    /// The session-note editor modal.
    Note(NoteEditor),
    /// The workspace-env editor modal (the `env` command), overlaying the palette.
    Env(EnvEditor),
}

/// Which tab-menu row is currently highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabMenuItem {
    MoveLeft,
    MoveRight,
    Rename,
    Close,
}

impl TabMenuItem {
    pub const ALL: [Self; 4] = [Self::MoveLeft, Self::MoveRight, Self::Rename, Self::Close];

    pub fn label(self) -> &'static str {
        match self {
            Self::MoveLeft => "Move left",
            Self::MoveRight => "Move right",
            Self::Rename => "Rename",
            Self::Close => "Close",
        }
    }
}

/// Context menu opened from a live-pane tab chip. It records the screen anchor so
/// the renderer can float the menu beside the clicked chip, and the session/tab
/// target so the event loop can apply the selected operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabMenu {
    dir: PathBuf,
    tab: usize,
    label: String,
    col: u16,
    row: u16,
    cursor: usize,
}

impl TabMenu {
    pub(super) fn new(
        dir: PathBuf,
        tab: usize,
        label: impl Into<String>,
        col: u16,
        row: u16,
    ) -> Self {
        Self {
            dir,
            tab,
            label: label.into(),
            col,
            row,
            cursor: 0,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn tab(&self) -> usize {
        self.tab
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn col(&self) -> u16 {
        self.col
    }

    pub fn row(&self) -> u16 {
        self.row
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn item(&self) -> TabMenuItem {
        TabMenuItem::ALL[self.cursor]
    }

    pub fn move_up(&mut self) {
        self.cursor = if self.cursor == 0 {
            TabMenuItem::ALL.len() - 1
        } else {
            self.cursor - 1
        };
    }

    pub fn move_down(&mut self) {
        self.cursor = (self.cursor + 1) % TabMenuItem::ALL.len();
    }
}

/// Inline tab-label editor opened from [`TabMenuItem::Rename`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TabRenameInput {
    dir: PathBuf,
    tab: usize,
    input: TextInput,
}

#[cfg(test)]
mod tab_menu_tests {
    use super::*;

    #[test]
    fn tab_menu_moves_wrap_and_exposes_target() {
        let mut menu = TabMenu::new(PathBuf::from("/repo/wt"), 2, "agent", 10, 4);
        assert_eq!(menu.dir(), Path::new("/repo/wt"));
        assert_eq!(menu.tab(), 2);
        assert_eq!(menu.label(), "agent");
        assert_eq!(menu.col(), 10);
        assert_eq!(menu.row(), 4);
        assert_eq!(menu.cursor(), 0);
        assert_eq!(menu.item(), TabMenuItem::MoveLeft);

        menu.move_up();
        assert_eq!(menu.cursor(), 3);
        assert_eq!(menu.item(), TabMenuItem::Close);
        menu.move_down();
        assert_eq!(menu.item(), TabMenuItem::MoveLeft);
        menu.move_down();
        assert_eq!(menu.item(), TabMenuItem::MoveRight);
        menu.move_up();
        assert_eq!(menu.item(), TabMenuItem::MoveLeft);
        menu.move_down();
        menu.move_down();
        assert_eq!(menu.item(), TabMenuItem::Rename);
    }

    #[test]
    fn tab_rename_input_edits_and_confirms_trimmed_label() {
        let mut input = TabRenameInput::new(PathBuf::from("/repo/wt"), 1, "terminal");
        assert_eq!(input.dir(), Path::new("/repo/wt"));
        assert_eq!(input.tab(), 1);
        assert_eq!(input.value(), "terminal");
        assert_eq!(input.cursor(), "terminal".len());

        input.move_home();
        assert_eq!(input.cursor(), 0);
        input.push_char(' ');
        input.move_end();
        input.push_char('!');
        input.move_left();
        input.backspace();
        input.delete_forward();
        input.move_right();
        input.push_char(' ');

        let (dir, tab, label) = input.confirm();
        assert_eq!(dir, PathBuf::from("/repo/wt"));
        assert_eq!(tab, 1);
        assert_eq!(label, "termina");
    }
}

impl TabRenameInput {
    pub(super) fn new(dir: PathBuf, tab: usize, label: impl Into<String>) -> Self {
        Self {
            dir,
            tab,
            input: TextInput::with_value(label),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn tab(&self) -> usize {
        self.tab
    }

    pub fn value(&self) -> &str {
        self.input.value()
    }

    pub fn cursor(&self) -> usize {
        self.input.cursor()
    }

    pub fn push_char(&mut self, c: char) {
        self.input.insert(c);
    }

    pub fn backspace(&mut self) {
        self.input.backspace();
    }

    pub fn delete_forward(&mut self) {
        self.input.delete_forward();
    }

    pub fn move_left(&mut self) {
        self.input.move_left();
    }

    pub fn move_right(&mut self) {
        self.input.move_right();
    }

    pub fn move_home(&mut self) {
        self.input.move_home();
    }

    pub fn move_end(&mut self) {
        self.input.move_end();
    }

    pub(super) fn confirm(self) -> (PathBuf, usize, String) {
        (self.dir, self.tab, self.input.value().trim().to_string())
    }
}

impl Overlay {
    /// Drop an open inline create input, leaving any other overlay untouched.
    /// The mode transitions (entering 切替 / 在席) call this to clear a
    /// half-typed session name without disturbing an unrelated overlay — the
    /// faithful translation of the old per-field `create = None`.
    pub fn clear_create(&mut self) {
        if matches!(self, Overlay::Create(_)) {
            *self = Overlay::None;
        }
    }
}

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
    input: TextInput,
}

impl RenameInput {
    /// Open a rename input for session `target`, pre-filled with `label` (caret
    /// at the end).
    pub(super) fn new(target: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            input: TextInput::with_value(label),
        }
    }

    /// The name of the session being renamed (its branch / identity).
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The label typed so far.
    pub fn value(&self) -> &str {
        self.input.value()
    }

    /// The caret position (byte offset) in the typed label.
    pub fn cursor(&self) -> usize {
        self.input.cursor()
    }

    /// Insert a character at the caret.
    pub fn push_char(&mut self, c: char) {
        self.input.insert(c);
    }

    /// Delete the character before the caret.
    pub fn backspace(&mut self) {
        self.input.backspace();
    }

    /// Delete the character at the caret (the `Del` key).
    pub fn delete_forward(&mut self) {
        self.input.delete_forward();
    }

    /// Move the caret one character left.
    pub fn move_left(&mut self) {
        self.input.move_left();
    }

    /// Move the caret one character right.
    pub fn move_right(&mut self) {
        self.input.move_right();
    }

    /// Move the caret to the start of the label.
    pub fn move_home(&mut self) {
        self.input.move_home();
    }

    /// Move the caret to the end of the label.
    pub fn move_end(&mut self) {
        self.input.move_end();
    }

    /// Accept the rename, consuming the input: the target session name and the
    /// typed label (trimmed). An empty label means "clear the override", which
    /// the usecase resolves.
    pub(super) fn confirm(self) -> (String, String) {
        (self.target, self.input.value().trim().to_string())
    }
}

/// The session-note editor modal, opened with `n` in 切替 (Switch) or `Ctrl-E`
/// in 没入 (Attached). It holds the session whose note is being edited
/// (`target`, its branch name / identity), the multi-line text buffer
/// (pre-filled with the existing note), and `reattach` — whether closing it
/// should re-attach the session's pane (set when opened from 没入, so the user
/// drops straight back into the live terminal). The buffer's editing and caret
/// movement live on [`TextArea`]; the modal just bundles it with its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteEditor {
    target: String,
    area: TextArea,
    reattach: bool,
}

impl NoteEditor {
    /// Open the editor for session `target`, pre-filled with `initial` (its
    /// current note). `reattach` records whether to re-attach the session on
    /// close (true when opened from 没入).
    pub(super) fn new(target: impl Into<String>, initial: &str, reattach: bool) -> Self {
        Self {
            target: target.into(),
            area: TextArea::from_text(initial),
            reattach,
        }
    }

    /// The session whose note is being edited (its branch / identity).
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The text buffer, for rendering its lines and caret.
    pub fn area(&self) -> &TextArea {
        &self.area
    }

    /// Whether closing the editor should re-attach the session's pane (it was
    /// opened from 没入).
    pub fn reattach(&self) -> bool {
        self.reattach
    }

    /// The editable buffer: the event loop routes its keys straight to the
    /// [`TextArea`]'s own editing methods (`insert` / `newline` / `backspace` /
    /// `move_*` …), so the modal has no per-key forwarders of its own.
    pub fn area_mut(&mut self) -> &mut TextArea {
        &mut self.area
    }

    /// Accept the note, consuming the editor: the target session, the typed text,
    /// and whether to re-attach. The text is persisted (and trimmed) by the
    /// usecase; an empty buffer clears the note.
    pub(super) fn confirm(self) -> (String, String, bool) {
        (self.target, self.area.text(), self.reattach)
    }
}

/// The workspace-env editor modal, opened by the `env` command as an overlay
/// over the command palette (Overview). It holds the multi-line buffer of
/// `NAME=op://vault/item/field` bindings, seeded from the workspace's current
/// settings. Editing and caret movement live on [`TextArea`] (the event loop
/// routes keys straight to it, like the note editor); the modal bundles it and
/// parses the valid bindings on confirm. Because it overlays the palette rather
/// than replacing it, closing it (save or cancel) returns to the Overview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvEditor {
    area: TextArea,
}

impl EnvEditor {
    /// Open the editor seeded from `env` (one `NAME=reference` line per binding,
    /// in sorted order), caret at the end.
    pub(super) fn new(env: &crate::domain::settings::SecretEnv) -> Self {
        Self {
            area: TextArea::from_text(&crate::domain::settings::format_env_bindings(env)),
        }
    }

    /// The text buffer, for rendering its lines and caret.
    pub fn area(&self) -> &TextArea {
        &self.area
    }

    /// The editable buffer: the event loop routes its keys straight to the
    /// [`TextArea`]'s own editing methods, so the modal has no per-key forwarders.
    pub fn area_mut(&mut self) -> &mut TextArea {
        &mut self.area
    }

    /// The valid bindings currently in the buffer (see
    /// [`crate::domain::settings::parse_env_bindings`] for the filtering rule).
    pub(super) fn bindings(&self) -> crate::domain::settings::SecretEnv {
        crate::domain::settings::parse_env_bindings(&self.area.text())
    }
}

/// How big a [`TextModal`] is drawn. Most text-dumping commands (`history` /
/// `session list` / `issue …`) use the compact [`Normal`](Self::Normal) floating
/// box; `man` uses [`Large`](Self::Large), which fills most of the terminal so
/// the whole command surface is readable at once with less scrolling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModalSize {
    /// The compact, fixed-size floating box (the historical text-modal size).
    #[default]
    Normal,
    /// A terminal-filling box that scales its width and visible-line count to the
    /// screen (see [`crate::presentation::tui::widgets::large_modal_geometry`]).
    Large,
}

/// An open scrollable text modal: the read-only output of a text-dumping command
/// (`man` / `history` / `session list`). `scroll` is the index of the first
/// visible body line, advanced by the arrow / page keys and clamped on render;
/// `size` selects the compact or terminal-filling box (see [`ModalSize`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextModal {
    pub title: String,
    pub lines: Vec<LogLine>,
    pub scroll: usize,
    pub size: ModalSize,
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

/// One row in the open session-removal checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveEntry {
    name: String,
    root_path: PathBuf,
    display: String,
}

impl RemoveEntry {
    /// Create a removal row for a session, remembering the workspace root the
    /// confirmed removal must target and the label the UI should show.
    pub(super) fn new(
        name: impl Into<String>,
        root_path: PathBuf,
        workspace_name: Option<&str>,
    ) -> Self {
        let name = name.into();
        let display = workspace_name
            .map(|workspace_name| format!("{workspace_name}: {name}"))
            .unwrap_or_else(|| name.clone());
        Self {
            name,
            root_path,
            display,
        }
    }

    /// The raw session name passed to the session-removal backend.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The workspace root that owns this session.
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// The UI label for this row. In 統合(unite) mode this includes the workspace
    /// name (`workspace: session`); otherwise it is just the session name.
    pub fn display(&self) -> &str {
        &self.display
    }
}

/// The open session-removal modal: the session entries with a checklist the
/// user toggles to pick which to delete in one go. In 統合(unite) mode entries
/// span every visible workspace and are labelled as `workspace: session`. A
/// cursor marks the row the keyboard acts on, `selected` holds the checked rows,
/// and `force` carries the `--force` flag from `session remove --force` so the
/// confirmed removal can discard uncommitted changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveModal {
    entries: Vec<RemoveEntry>,
    cursor: usize,
    selected: HashSet<usize>,
    force: bool,
}

impl RemoveModal {
    /// Open the modal over `entries`, nothing checked, carrying the `--force`
    /// flag.
    pub(super) fn new(entries: Vec<RemoveEntry>, force: bool) -> Self {
        Self {
            entries,
            cursor: 0,
            selected: HashSet::new(),
            force,
        }
    }

    /// The removal entries, in display order.
    pub fn entries(&self) -> &[RemoveEntry] {
        &self.entries
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
        self.entries.is_empty()
    }

    /// Move the cursor up one row, wrapping to the bottom. No-op with no sessions.
    pub fn move_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.cursor = self
            .cursor
            .checked_sub(1)
            .unwrap_or(self.entries.len().saturating_sub(1));
    }

    /// Move the cursor down one row, wrapping to the top. No-op with no sessions.
    pub fn move_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.cursor = (self.cursor + 1) % self.entries.len();
    }

    /// Toggle the checked state of the session under the cursor. No-op with no
    /// sessions.
    pub fn toggle(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        if !self.selected.insert(self.cursor) {
            self.selected.remove(&self.cursor);
        }
    }

    /// The checked removal entries (in display order) together with the
    /// `--force` flag, or `None` when nothing is checked (so the modal stays
    /// open).
    pub(super) fn confirm(&self) -> Option<(Vec<RemoveEntry>, bool)> {
        if self.selected.is_empty() {
            return None;
        }
        let entries = self
            .entries
            .iter()
            .enumerate()
            .filter(|(i, _)| self.selected.contains(i))
            .map(|(_, entry)| entry.clone())
            .collect();
        Some((entries, self.force))
    }
}

/// The 在席 (Focus) menu cursor: which Session-scope command is highlighted, and
/// — when the `agent` row is expanded into the agent picker — which installed
/// agent is highlighted. The Session-scope command list is always non-empty, so
/// the navigation methods take the current `count` and keep the cursor
/// underflow-safe and in range.
///
/// The agent picker (案A) lets a session launch a CLI other than the configured
/// default: pressing `→` / `Tab` on the `agent` row expands an inline sub-list of
/// installed agents (only when more than one is installed, so there is a choice);
/// `↑` / `↓` move within it and `Enter` launches the highlighted one. While
/// expanded, `agent_cursor` is `Some(index)` and the move/`selected` methods act
/// on the picker instead of the command list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct FocusMenu {
    cursor: usize,
    /// The agent picker's sub-cursor while the `agent` row is expanded, or `None`
    /// when the menu is in its normal (collapsed) state.
    agent_cursor: Option<usize>,
    /// The close picker's sub-cursor while the `close` row is expanded, or `None`
    /// when collapsed. `0` = plain close, `1` = close --force.
    close_cursor: Option<usize>,
}

impl FocusMenu {
    /// The highlighted command row.
    pub(super) fn cursor(self) -> usize {
        self.cursor
    }

    /// Whether the `agent` row is expanded into the agent picker.
    pub(super) fn is_expanded(self) -> bool {
        self.agent_cursor.is_some()
    }

    /// Whether the `close` row is expanded into the close picker.
    pub(super) fn is_close_expanded(self) -> bool {
        self.close_cursor.is_some()
    }

    /// The agent picker's highlighted index while expanded, or `None` collapsed.
    pub(super) fn agent_cursor(self) -> Option<usize> {
        self.agent_cursor
    }

    /// The close picker's highlighted index while expanded, or `None` collapsed.
    pub(super) fn close_cursor(self) -> Option<usize> {
        self.close_cursor
    }

    /// Reset to the top, collapsed (entering 在席 / leaving for 切替).
    pub(super) fn reset(&mut self) {
        self.cursor = 0;
        self.agent_cursor = None;
        self.close_cursor = None;
    }

    /// Expand the agent picker, highlighting `default_index` (the configured
    /// agent's position in the installed list, clamped by the renderer).
    pub(super) fn expand(&mut self, default_index: usize) {
        self.agent_cursor = Some(default_index);
    }

    /// Collapse the agent picker back to the normal menu. Returns whether it was
    /// expanded (so the caller can treat `←` / `Esc` as "consumed" only then).
    pub(super) fn collapse(&mut self) -> bool {
        self.agent_cursor.take().is_some()
    }

    /// Expand the close picker, starting at option 0 (plain close).
    pub(super) fn expand_close(&mut self) {
        self.close_cursor = Some(0);
    }

    /// Collapse the close picker back to the normal menu. Returns whether it was
    /// expanded (so the caller can treat `←` / `Esc` as "consumed" only then).
    pub(super) fn collapse_close(&mut self) -> bool {
        self.close_cursor.take().is_some()
    }

    /// Move up one row, wrapping. Acts on whichever picker is open, or the
    /// command list when none is. `count` is clamped to at least 1.
    pub(super) fn move_up(&mut self, count: usize) {
        let count = count.max(1);
        if let Some(c) = &mut self.agent_cursor {
            *c = c.checked_sub(1).unwrap_or(count - 1);
        } else if let Some(c) = &mut self.close_cursor {
            *c = c.checked_sub(1).unwrap_or(count - 1);
        } else {
            self.cursor = self.cursor.checked_sub(1).unwrap_or(count - 1);
        }
    }

    /// Move down one row, wrapping (the mirror of [`move_up`](Self::move_up)).
    pub(super) fn move_down(&mut self, count: usize) {
        let count = count.max(1);
        if let Some(c) = &mut self.agent_cursor {
            *c = (*c + 1) % count;
        } else if let Some(c) = &mut self.close_cursor {
            *c = (*c + 1) % count;
        } else {
            self.cursor = (self.cursor + 1) % count;
        }
    }

    /// The selected command row, clamped to the available `count`.
    pub(super) fn selected(self, count: usize) -> usize {
        self.cursor.min(count.saturating_sub(1))
    }

    /// The selected agent-picker index, clamped to the available `count`. `0`
    /// when collapsed (no picker open).
    pub(super) fn agent_selected(self, count: usize) -> usize {
        self.agent_cursor.unwrap_or(0).min(count.saturating_sub(1))
    }

    /// The selected close-picker index, clamped to 0 or 1. `0` when collapsed.
    pub(super) fn close_selected(self) -> usize {
        self.close_cursor.unwrap_or(0).min(1)
    }
}

/// Validate a typed session name against the branch names already taken, used
/// for the live inline-create feedback. Returns the reason the name cannot be
/// used, or `None` when it is usable.
///
/// An empty (or all-whitespace) name returns `None` — the input does not nag
/// while nothing has been typed; the empty case is rejected only on Enter (see
/// [`CreateInput::confirm`]). The name-format rules (path separators, a leading
/// `-`) are delegated to [`crate::usecase::session::name_format_error`] so the
/// inline message is exactly the one `create` would raise; the duplicate /
/// namespace checks here work against the pre-fetched branch list rather than
/// touching git:
///
/// - an exact duplicate of the branch `usagi/<name>` it would cut;
/// - a clash with an existing branch nested under `usagi/<name>/` (git cannot
///   create the `usagi/<name>` branch alongside `usagi/<name>/…`).
///
/// The name is compared against the *branch* it would create — `usagi/<name>`,
/// per [`crate::usecase::session::branch_name`] — not the bare name, so a
/// hand-made branch sharing the bare name (e.g. `<name>` or `feat/<name>`) is no
/// longer a false conflict: every session branch is namespaced under `usagi/`.
fn validate_session_name(name: &str, taken: &[String]) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if let Some(error) = crate::usecase::session::name_format_error(name) {
        return Some(error);
    }
    let branch = crate::usecase::session::branch_name(name);
    if taken.contains(&branch) {
        return Some(format!("\"{name}\" already exists."));
    }
    let prefix = format!("{branch}/");
    if let Some(conflict) = taken.iter().find(|b| b.starts_with(&prefix)) {
        return Some(format!("\"{name}\" conflicts with branch \"{conflict}\"."));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_menu_moves_and_selects_the_command_cursor_when_collapsed() {
        let mut menu = FocusMenu::default();
        assert_eq!(menu.cursor(), 0);
        assert!(!menu.is_expanded());
        // Down wraps within the command count; up wraps back.
        menu.move_down(3);
        assert_eq!(menu.cursor(), 1);
        menu.move_up(3);
        assert_eq!(menu.cursor(), 0);
        menu.move_up(3);
        assert_eq!(menu.cursor(), 2);
        // `selected` clamps to the available count.
        assert_eq!(menu.selected(2), 1);
        // A zero count is clamped to 1 so navigation never divides by zero.
        let mut empty = FocusMenu::default();
        empty.move_down(0);
        assert_eq!(empty.cursor(), 0);
    }

    #[test]
    fn focus_menu_expand_collapse_drives_the_agent_picker() {
        let mut menu = FocusMenu::default();
        // Expanding highlights the given default index and routes navigation to
        // the picker, leaving the command cursor untouched.
        menu.expand(2);
        assert!(menu.is_expanded());
        assert_eq!(menu.agent_cursor(), Some(2));
        assert_eq!(menu.cursor(), 0);
        // Moving now wraps within the agent count, not the command count.
        menu.move_down(4);
        assert_eq!(menu.agent_cursor(), Some(3));
        menu.move_down(4);
        assert_eq!(menu.agent_cursor(), Some(0));
        menu.move_up(4);
        assert_eq!(menu.agent_cursor(), Some(3));
        assert_eq!(menu.agent_selected(4), 3);
        // Collapsing reports it was open and clears the picker cursor.
        assert!(menu.collapse());
        assert!(!menu.is_expanded());
        assert_eq!(menu.agent_cursor(), None);
        // Collapsing again is a no-op that reports "was not open".
        assert!(!menu.collapse());
        // `agent_selected` is 0 when collapsed (no picker open).
        assert_eq!(menu.agent_selected(4), 0);
    }

    #[test]
    fn focus_menu_reset_clears_both_cursors() {
        let mut menu = FocusMenu::default();
        menu.move_down(3);
        menu.expand(1);
        menu.reset();
        assert_eq!(menu.cursor(), 0);
        assert!(!menu.is_expanded());
    }

    #[test]
    fn validate_session_name_flags_empty_separators_duplicates_and_nesting() {
        // An empty / whitespace name is quiet (the input does not nag).
        assert_eq!(validate_session_name("", &[]), None);
        assert_eq!(validate_session_name("   ", &[]), None);
        // Path separators are illegal (message delegated to the usecase).
        assert!(validate_session_name("a/b", &[])
            .unwrap()
            .contains("path separators"));
        assert!(validate_session_name("a\\b", &[])
            .unwrap()
            .contains("path separators"));
        assert!(validate_session_name(".", &[])
            .unwrap()
            .contains("path separators"));
        // A leading "-" is illegal too (git would read it as an option). This is
        // the rule the old hand-rolled validator was missing.
        assert!(validate_session_name("-x", &[])
            .unwrap()
            .contains("must not start with"));
        // The name is matched against the `usagi/<name>` branch it would cut, so
        // an existing `usagi/feature` is an exact duplicate...
        let taken = vec!["usagi/feature".to_string()];
        assert!(validate_session_name("feature", &taken)
            .unwrap()
            .contains("already exists"));
        // ...while a hand-made branch sharing the bare name is not (sessions live
        // under `usagi/`, so they never collide with `feature` itself).
        assert_eq!(
            validate_session_name("feature", &["feature".to_string()]),
            None
        );
        // A clash with a branch nested under `usagi/<name>/` is reported.
        let taken = vec!["usagi/feature/x".to_string()];
        assert!(validate_session_name("feature", &taken)
            .unwrap()
            .contains("conflicts with branch"));
        // A free name is usable.
        assert_eq!(validate_session_name("wip", &taken), None);
    }
}
