//! The home screen's transient sub-mode state: the 選択 (Overview) inline
//! create/rename inputs, the 集中 (Closeup) menu cursor, the scrollable text
//! modal, and the session-removal checklist.
//!
//! Each sub-mode is its own type owning its editing/navigation logic and
//! invariants, so [`HomeState`](super::HomeState) only holds the optional state
//! and routes to it — the display- and cursor-level behaviour lives here, not as
//! flat forwarding methods on the screen state.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::LogLine;
use crate::domain::workspace_state::{SessionDecision, SessionTodo};
use crate::presentation::tui::chat::state::Chat;
use crate::presentation::tui::diff::{self, DiffDoc, DiffFile, SplitRow, TreeKind, TreeRow};
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
    /// The inline session-name input open while creating a session from 選択.
    Create(CreateInput),
    /// The inline display-name input open while renaming a session from 選択.
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
    /// The right-pane local-LLM chat surface (集中's `chat`). Like
    /// [`Preview`](Self::Preview) it takes over the right pane and captures the
    /// keyboard while open, but it is interactive: the conversation state lives
    /// here while the event loop drives the (async) model request.
    Chat(Chat),
    /// The right-pane diff view (the `diff` command).
    Diff(DiffView),
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
    /// The mode transitions (entering 選択 / 集中) call this to clear a
    /// half-typed session name without disturbing an unrelated overlay — the
    /// faithful translation of the old per-field `create = None`.
    pub fn clear_create(&mut self) {
        if matches!(self, Overlay::Create(_)) {
            *self = Overlay::None;
        }
    }
}

/// The inline session-name input shown in the left pane while creating a session
/// from 選択 (Overview): the name being typed, the existing branch names it is
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
/// from 選択 (Overview): the session whose sidebar label is being edited
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

/// Which pane of the note scratchpad has the focus (is the editing target).
///
/// The overlay shows all three panes at once, stacked top to bottom: the
/// free-form [`Note`](NotePane::Note) (editable), the
/// [`Todos`](NotePane::Todos) checklist (interactively editable), and the
/// [`Decisions`](NotePane::Decisions) log (read-only — an agent writes it over
/// MCP `session_decision_*`). `Tab` moves the focus forward and `BackTab`
/// (Shift-Tab) backward through the same order; only the focused pane receives
/// the editing keys, and the renderer marks it with the accent frame and the
/// `(編集中)` / `(表示中)` title suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotePane {
    Note,
    Todos,
    Decisions,
}

impl NotePane {
    /// The pane's short label, shown in its box title.
    pub fn label(self) -> &'static str {
        match self {
            NotePane::Note => "note",
            NotePane::Todos => "todos",
            NotePane::Decisions => "decisions",
        }
    }

    /// The three panes in display (stacking) order — also the `Tab` focus
    /// cycle order.
    pub fn all() -> [NotePane; 3] {
        [NotePane::Note, NotePane::Todos, NotePane::Decisions]
    }

    /// The next (`forward`) or previous pane, wrapping around the three.
    fn stepped(self, forward: bool) -> NotePane {
        let panes = NotePane::all();
        let i = panes.iter().position(|t| *t == self).unwrap_or(0);
        let n = panes.len();
        let next = if forward { i + 1 } else { i + n - 1 } % n;
        panes[next]
    }
}

/// The inline single-line input open on the todos pane while adding or editing a
/// todo. `editing` is the index being edited, or `None` when adding a new todo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoInput {
    input: TextInput,
    editing: Option<usize>,
}

impl TodoInput {
    /// The text being typed (for the renderer's caret split).
    pub fn input(&self) -> &TextInput {
        &self.input
    }

    /// Whether this is editing an existing todo (vs adding a new one).
    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }
}

/// The session-note editor modal, opened with `n` in 選択 (Overview) or `Ctrl-E`
/// in 没入 (Attached). It holds the session whose scratchpad is open
/// (`target`, its branch name / identity), the editable note buffer
/// (pre-filled with the existing note), the session's `todos` (editable on the
/// todos pane) and read-only `decisions`, the focused pane
/// ([`focus`](Self::focus)), the todos pane's selection / inline input, whether
/// the todos were changed (`todos_dirty`, so the save persists them only when
/// touched), and `reattach` — whether closing it should re-attach the session's
/// pane (set when opened from 没入). The note buffer's editing and caret
/// movement live on [`TextArea`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteEditor {
    target: String,
    area: TextArea,
    todos: Vec<SessionTodo>,
    decisions: Vec<SessionDecision>,
    focus: NotePane,
    selected: usize,
    input: Option<TodoInput>,
    todos_dirty: bool,
    reattach: bool,
}

impl NoteEditor {
    /// Open the editor for session `target`, pre-filled with `initial` (its
    /// current note) and snapshots of its `todos` / `decisions`. `reattach`
    /// records whether to re-attach the session on close (true when opened from
    /// 没入). Always opens with the focus on the [`Note`](NotePane::Note) pane.
    pub(super) fn new(
        target: impl Into<String>,
        initial: &str,
        todos: Vec<SessionTodo>,
        decisions: Vec<SessionDecision>,
        reattach: bool,
    ) -> Self {
        Self {
            target: target.into(),
            area: TextArea::from_text(initial),
            todos,
            decisions,
            focus: NotePane::Note,
            selected: 0,
            input: None,
            todos_dirty: false,
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

    /// The session's todo checklist (read-only snapshot taken when opened).
    pub fn todos(&self) -> &[SessionTodo] {
        &self.todos
    }

    /// The session's decision log (read-only snapshot taken when opened).
    pub fn decisions(&self) -> &[SessionDecision] {
        &self.decisions
    }

    /// The pane the focus (the editing target) is on.
    pub fn focus(&self) -> NotePane {
        self.focus
    }

    /// Move the focus to the next (`forward`) or previous pane.
    pub(super) fn cycle_focus(&mut self, forward: bool) {
        self.focus = self.focus.stepped(forward);
    }

    /// Whether closing the editor should re-attach the session's pane (it was
    /// opened from 没入).
    pub fn reattach(&self) -> bool {
        self.reattach
    }

    /// The editable note buffer: the event loop routes editing keys straight to
    /// the [`TextArea`]'s own methods (`insert` / `newline` / `backspace` /
    /// `move_*` …) while the [`Note`](NotePane::Note) pane has the focus.
    pub fn area_mut(&mut self) -> &mut TextArea {
        &mut self.area
    }

    /// The index of the highlighted todo on the todos pane (0 when the list is
    /// empty). Read by the renderer to mark the selected row.
    pub fn selected_todo(&self) -> usize {
        self.selected
    }

    /// The inline add / edit input open on the todos pane, if any.
    pub fn todo_input(&self) -> Option<&TodoInput> {
        self.input.as_ref()
    }

    /// Whether the inline todo input is open (adding or editing).
    pub fn is_editing_todo(&self) -> bool {
        self.input.is_some()
    }

    /// Move the todos-pane selection down (`down`) or up, clamped to the list
    /// (no wrap). A no-op while the inline input is open or the list is empty.
    pub(super) fn move_todo(&mut self, down: bool) {
        if self.input.is_some() || self.todos.is_empty() {
            return;
        }
        let last = self.todos.len() - 1;
        self.selected = if down {
            (self.selected + 1).min(last)
        } else {
            self.selected.saturating_sub(1)
        };
    }

    /// Toggle the highlighted todo's done state. A no-op while the input is open
    /// or the list is empty.
    pub(super) fn toggle_selected_todo(&mut self) {
        if self.input.is_some() {
            return;
        }
        if let Some(todo) = self.todos.get_mut(self.selected) {
            todo.done = !todo.done;
            self.todos_dirty = true;
        }
    }

    /// Remove the highlighted todo, clamping the selection to what remains. A
    /// no-op while the input is open or the list is empty.
    pub(super) fn remove_selected_todo(&mut self) {
        if self.input.is_some() || self.selected >= self.todos.len() {
            return;
        }
        self.todos.remove(self.selected);
        self.selected = self.selected.min(self.todos.len().saturating_sub(1));
        self.todos_dirty = true;
    }

    /// Open the inline input to add a new todo (empty). A no-op if one is already
    /// open.
    pub(super) fn begin_add_todo(&mut self) {
        if self.input.is_none() {
            self.input = Some(TodoInput {
                input: TextInput::new(),
                editing: None,
            });
        }
    }

    /// Open the inline input to edit the highlighted todo (pre-filled). A no-op
    /// if the list is empty or an input is already open.
    pub(super) fn begin_edit_todo(&mut self) {
        if self.input.is_some() {
            return;
        }
        if let Some(todo) = self.todos.get(self.selected) {
            self.input = Some(TodoInput {
                input: TextInput::with_value(todo.text.clone()),
                editing: Some(self.selected),
            });
        }
    }

    /// Route a key to the open inline todo input (returns whether it changed).
    /// Callers guard on [`is_editing_todo`](Self::is_editing_todo).
    pub(super) fn todo_input_key(&mut self, key: &console::Key) -> bool {
        self.input
            .as_mut()
            .map(|i| i.input.handle_key(key))
            .unwrap_or(false)
    }

    /// Commit the open inline input: add the typed todo, or replace the edited
    /// one, when the text is non-empty (trimmed); an empty text just closes the
    /// input. Closes the input either way. A no-op when no input is open.
    pub(super) fn commit_todo_input(&mut self) {
        let Some(TodoInput { input, editing }) = self.input.take() else {
            return;
        };
        let text = input.value().trim().to_string();
        if text.is_empty() {
            return;
        }
        match editing {
            // Replace the edited row's text. `get_mut` guards the (unreachable
            // through the UI, since the input blocks other edits) vanished-row case.
            Some(i) => {
                if let Some(todo) = self.todos.get_mut(i) {
                    todo.text = text;
                    self.todos_dirty = true;
                }
            }
            None => {
                self.todos.push(SessionTodo::new(text));
                self.selected = self.todos.len() - 1;
                self.todos_dirty = true;
            }
        }
    }

    /// Close the inline input without applying it.
    pub(super) fn cancel_todo_input(&mut self) {
        self.input = None;
    }

    /// Accept the edit, consuming the editor: the target session, the typed note
    /// text, the todos to persist (`Some` only when they were changed, so an
    /// untouched checklist is not rewritten), and whether to re-attach. The note
    /// is persisted (and trimmed) by the usecase; an empty buffer clears it.
    pub(super) fn confirm(self) -> (String, String, Option<Vec<SessionTodo>>, bool) {
        let todos = self.todos_dirty.then_some(self.todos);
        (self.target, self.area.text(), todos, self.reattach)
    }
}

/// The workspace-env editor modal, opened by the `env` command as an overlay
/// over the command palette. It holds the multi-line buffer of
/// `NAME=op://vault/item/field` bindings, seeded from the workspace's current
/// settings. Editing and caret movement live on [`TextArea`] (the event loop
/// routes keys straight to it, like the note editor); the modal bundles it and
/// parses the valid bindings on confirm. Because it overlays the palette rather
/// than replacing it, closing it (save or cancel) returns to the command palette.
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

/// Which half of the diff view the keyboard drives: the left file explorer
/// (directory tree) or the right diff pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiffFocus {
    /// The left explorer: `↑`/`↓` move the tree cursor, `Enter` expands a
    /// directory or opens a file's diff.
    #[default]
    Tree,
    /// The right diff pane: `↑`/`↓` / `PageUp`/`PageDown` scroll the selected
    /// file's diff.
    Diff,
}

/// One visible row of the explorer tree, flattened for the renderer: its `depth`
/// (indentation), the path `segment` shown at that depth, whether it is a
/// directory (and if so whether `collapsed`), the file's `+added -removed` counts
/// (files only), and whether it is the row under the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffTreeRow {
    pub depth: usize,
    pub segment: String,
    pub is_dir: bool,
    pub collapsed: bool,
    pub added: usize,
    pub removed: usize,
    pub selected: bool,
}

/// The right-pane diff view, opened by the `diff` command. Like [`Preview`] it
/// takes over the right pane, but it renders a GitHub pull-request-style split: a
/// **left directory-tree explorer** of the changed files beside the **right
/// syntax-highlighted diff** of the selected one (line-number gutter, per-line
/// add/del backgrounds, word-level emphasis).
///
/// `title` names the diffed branch → base. The changed files (`files`) and their
/// directory tree (`tree`) are derived once from `doc`; `collapsed` holds the
/// directory paths the user has folded, `cursor` is the tree index the explorer
/// highlights, and `file` is the file whose diff the right side shows. `focus`
/// selects which half the keyboard drives, `scroll` is the first visible diff row
/// *within the selected file*, and `split` toggles the unified layout (default)
/// against the side-by-side one. While open it captures the keys (navigate /
/// scroll / toggle layout / dismiss).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffView {
    pub title: String,
    pub doc: DiffDoc,
    files: Vec<DiffFile>,
    split_rows: Vec<Vec<SplitRow>>,
    tree: Vec<TreeRow>,
    collapsed: HashSet<String>,
    cursor: usize,
    file: usize,
    focus: DiffFocus,
    scroll: usize,
    split: bool,
    /// How the explorer and the diff are arranged: `false` = side by side
    /// (explorer left, diff right — the default), `true` = stacked (explorer on
    /// top, diff below). Toggled with `v`; independent of [`split`](Self::split),
    /// which is the diff *content*'s unified vs. old|new columns.
    stacked: bool,
}

impl DiffView {
    /// Build the view from a parsed diff: derive the changed files and their
    /// directory tree, and park the cursor on the first file (so its diff shows on
    /// the right straight away). Nothing is collapsed and the unified layout is the
    /// default, mirroring the previous flat view.
    pub(super) fn new(title: String, doc: DiffDoc) -> Self {
        let files = diff::files(&doc);
        let split_rows = files
            .iter()
            .map(|file| diff::split_rows_slice(&doc.rows[file.start..file.end], file.start))
            .collect();
        let tree = diff::tree_rows(&files);
        let cursor = tree
            .iter()
            .position(|r| matches!(r.kind, TreeKind::File { .. }))
            .unwrap_or(0);
        let file = match tree.get(cursor).map(|r| &r.kind) {
            Some(TreeKind::File { index }) => *index,
            _ => 0,
        };
        Self {
            title,
            doc,
            files,
            split_rows,
            tree,
            collapsed: HashSet::new(),
            cursor,
            file,
            focus: DiffFocus::Tree,
            scroll: 0,
            split: false,
            stacked: false,
        }
    }

    /// Whether the diff changed no files (an empty patch), so the view shows a
    /// friendly "no changes" line instead of an explorer.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Which half the keyboard drives.
    pub fn focus(&self) -> DiffFocus {
        self.focus
    }

    /// The first visible diff row within the selected file.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Whether the side-by-side layout is active.
    pub fn split(&self) -> bool {
        self.split
    }

    /// Whether the explorer and diff are stacked (explorer on top, diff below)
    /// rather than side by side (explorer left, diff right — the default).
    pub fn stacked(&self) -> bool {
        self.stacked
    }

    /// The selected file's section, or `None` for an empty patch.
    pub fn selected_file(&self) -> Option<&DiffFile> {
        self.files.get(self.file)
    }

    /// The selected file's precomputed split-layout rows.
    pub fn selected_split_rows(&self) -> Option<&[SplitRow]> {
        self.split_rows.get(self.file).map(Vec::as_slice)
    }

    /// How many changed files the diff has (for the explorer's `Files (N)` label).
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// The explorer rows currently visible (those with no collapsed ancestor),
    /// flattened for the renderer with per-row display flags. Depth-first order,
    /// directories before files.
    pub fn visible_rows(&self) -> Vec<DiffTreeRow> {
        self.visible_indices()
            .into_iter()
            .map(|i| {
                let row = &self.tree[i];
                let (is_dir, collapsed) = match &row.kind {
                    TreeKind::Dir { path } => (true, self.collapsed.contains(path)),
                    TreeKind::File { .. } => (false, false),
                };
                let (added, removed) = match &row.kind {
                    TreeKind::File { index } => self
                        .files
                        .get(*index)
                        .map(|f| (f.added, f.removed))
                        .unwrap_or((0, 0)),
                    TreeKind::Dir { .. } => (0, 0),
                };
                DiffTreeRow {
                    depth: row.depth,
                    segment: row.name.clone(),
                    is_dir,
                    collapsed,
                    added,
                    removed,
                    selected: i == self.cursor,
                }
            })
            .collect()
    }

    /// How many visual rows the selected file's diff occupies in the current
    /// layout — the unified row count, or the folded side-by-side count. Drives the
    /// scroll clamp and the renderer's window.
    pub fn file_row_count(&self) -> usize {
        let Some(file) = self.selected_file() else {
            return 0;
        };
        if self.split {
            self.split_rows.get(self.file).map_or(0, Vec::len)
        } else {
            file.end - file.start
        }
    }

    /// The tree indices with no collapsed ancestor, in tree (depth-first) order.
    /// Because the tree is depth-first, a collapsed directory hides every deeper
    /// row until the depth returns to its own or shallower.
    fn visible_indices(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut hide_below: Option<usize> = None;
        for (i, row) in self.tree.iter().enumerate() {
            if let Some(depth) = hide_below {
                if row.depth > depth {
                    continue;
                }
                hide_below = None;
            }
            out.push(i);
            if let TreeKind::Dir { path } = &row.kind {
                if self.collapsed.contains(path) {
                    hide_below = Some(row.depth);
                }
            }
        }
        out
    }

    /// Move the explorer cursor to the previous visible row (clamped at the top).
    /// Landing on a file selects it, resetting the diff scroll.
    pub(super) fn move_up(&mut self) {
        let visible = self.visible_indices();
        if let Some(pos) = visible.iter().position(|&i| i == self.cursor) {
            if pos > 0 {
                self.set_cursor(visible[pos - 1]);
            }
        }
    }

    /// Move the explorer cursor to the next visible row (clamped at the bottom).
    /// Landing on a file selects it, resetting the diff scroll.
    pub(super) fn move_down(&mut self) {
        let visible = self.visible_indices();
        if let Some(pos) = visible.iter().position(|&i| i == self.cursor) {
            if pos + 1 < visible.len() {
                self.set_cursor(visible[pos + 1]);
            }
        }
    }

    /// Park the cursor on tree index `i`; when it is a file, show its diff and
    /// reset the scroll to the top.
    fn set_cursor(&mut self, i: usize) {
        self.cursor = i;
        if let Some(TreeKind::File { index }) = self.tree.get(i).map(|r| &r.kind) {
            if *index != self.file {
                self.file = *index;
                self.scroll = 0;
            }
        }
    }

    /// Activate the cursor row: a directory folds/unfolds; a file moves the focus
    /// to the diff pane so the arrows scroll it.
    pub(super) fn activate(&mut self) {
        match self.tree.get(self.cursor).map(|r| &r.kind) {
            Some(TreeKind::Dir { path }) => {
                let path = path.clone();
                if !self.collapsed.remove(&path) {
                    self.collapsed.insert(path);
                }
            }
            Some(TreeKind::File { .. }) => self.focus = DiffFocus::Diff,
            None => {}
        }
    }

    /// Collapse the cursor's directory if it is an expanded one (the explorer's
    /// `←`); a no-op on a file or an already-collapsed directory.
    pub(super) fn collapse_current(&mut self) {
        if let Some(TreeKind::Dir { path }) = self.tree.get(self.cursor).map(|r| &r.kind) {
            self.collapsed.insert(path.clone());
        }
    }

    /// Give the keyboard to the explorer.
    pub(super) fn focus_tree(&mut self) {
        self.focus = DiffFocus::Tree;
    }

    /// Toggle the keyboard between the explorer and the diff pane.
    pub(super) fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            DiffFocus::Tree => DiffFocus::Diff,
            DiffFocus::Diff => DiffFocus::Tree,
        };
    }

    /// Scroll the selected file's diff up one row (no-op at the top).
    pub(super) fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    /// Scroll the selected file's diff down one row, clamped so the last row of the
    /// file stays in view. `visible` is the diff pane's body height.
    pub(super) fn scroll_down(&mut self, visible: usize) {
        let max = self.file_row_count().saturating_sub(visible);
        self.scroll = (self.scroll + 1).min(max);
    }

    /// Toggle the diff pane between the unified and side-by-side layouts, resetting
    /// the scroll so the switch lands at the top of the file.
    pub(super) fn toggle_split(&mut self) {
        self.split = !self.split;
        self.scroll = 0;
    }

    /// Toggle the explorer/diff arrangement between side-by-side (explorer left,
    /// diff right) and stacked (explorer on top, diff below).
    pub(super) fn toggle_layout(&mut self) {
        self.stacked = !self.stacked;
    }
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
///
/// Confirming does not close the modal outright: the checked removals run in the
/// background, so the modal stays open in a *removing* state — `pending` holds
/// the `(root, name)` of every removal still in flight, keyed by the same pair
/// the finished task reports back. As each lands, the screen calls
/// [`resolve`](Self::resolve): a success drops that session's row, a failure
/// records its message in `error` and keeps the row. The modal closes only once
/// every dispatched removal has succeeded (empty `pending`, no `error`); a
/// failure leaves it open so the error is read and the remaining rows retried.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveModal {
    entries: Vec<RemoveEntry>,
    cursor: usize,
    selected: HashSet<usize>,
    force: bool,
    /// The `(root, name)` of the removals dispatched from this modal and not yet
    /// finished. Empty until a confirm dispatches work, and again once every
    /// dispatched removal has resolved.
    pending: HashSet<(PathBuf, String)>,
    /// The last removal failure's message, shown in the modal so the user sees
    /// why a session was not removed. Cleared on the next confirm.
    error: Option<String>,
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
            pending: HashSet::new(),
            error: None,
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

    /// Whether removals dispatched from this modal are still in flight — while
    /// true the modal shows a *removing* state and refuses a fresh confirm.
    pub fn is_removing(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether the row at `index` is a removal still in flight (so the renderer
    /// can mark it as removing rather than a plain checkbox).
    pub fn is_pending(&self, index: usize) -> bool {
        self.entries
            .get(index)
            .is_some_and(|entry| self.pending.contains(&entry_key(entry)))
    }

    /// The last removal failure's message, shown under the checklist.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Confirm the checked removals: record them as in-flight, clear the checks
    /// and any previous error, and return the entries (in display order) with the
    /// `--force` flag for the caller to dispatch. Returns `None` — leaving the
    /// modal untouched — when nothing is checked or removals are already running,
    /// so neither an empty confirm nor a double-submit dispatches work.
    pub(super) fn begin_removal(&mut self) -> Option<(Vec<RemoveEntry>, bool)> {
        if self.is_removing() {
            return None;
        }
        let (entries, force) = self.confirm()?;
        self.pending = entries.iter().map(entry_key).collect();
        self.selected.clear();
        self.error = None;
        Some((entries, force))
    }

    /// Apply a finished background removal of `(root, name)`: on success drop that
    /// session's row, on failure record `error` and keep the row. Returns `true`
    /// when the modal should now close — every dispatched removal has succeeded.
    /// A removal this modal did not dispatch is ignored (returns `false`), so a
    /// stray completion never closes it.
    pub(super) fn resolve(&mut self, root: &Path, name: &str, ok: bool, error: &str) -> bool {
        let key = (root.to_path_buf(), name.to_string());
        if !self.pending.remove(&key) {
            return false;
        }
        if ok {
            // Drop the removed row; the surviving indices shift, so reset the
            // cursor and any stale checks rather than leave them dangling.
            self.entries.retain(|entry| entry_key(entry) != key);
            self.selected.clear();
            self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
        } else {
            self.error = Some(error.to_string());
        }
        self.pending.is_empty() && self.error.is_none()
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

/// The `(root, name)` key identifying a removal — the pair a finished background
/// task reports back — so a modal row is matched to its completion even in
/// 統合(unite) mode, where two workspaces can hold the same session name.
fn entry_key(entry: &RemoveEntry) -> (PathBuf, String) {
    (entry.root_path().to_path_buf(), entry.name().to_string())
}

/// Which inline sub-picker is expanded under a 集中 (Closeup) menu row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseupSubmenu {
    /// The `agent` row's installed-CLI picker.
    Agent,
    /// The `terminal` row's `open` / `new` picker.
    Terminal,
    /// The `close` row's plain / `--force` picker.
    Close,
}

/// The 集中 (Closeup) menu cursor: which Session-scope command is highlighted, and
/// — when the `agent` or `terminal` row is expanded into a picker — which
/// sub-action is highlighted. The Session-scope command list is always non-empty,
/// so the navigation methods take the current `count` and keep the cursor
/// underflow-safe and in range.
///
/// The agent picker (案A) lets a session launch a CLI other than the configured
/// default: pressing `→` / `Tab` on the `agent` row expands an inline sub-list of
/// installed agents (only when more than one is installed, so there is a choice);
/// `↑` / `↓` move within it and `Enter` launches the highlighted one. While
/// expanded, `agent_cursor` is `Some(index)` and the move/`selected` methods act
/// on the picker instead of the command list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CloseupMenu {
    cursor: usize,
    /// The expanded inline picker and its sub-cursor, or `None` when the menu is
    /// in its normal (collapsed) state.
    expanded: Option<(CloseupSubmenu, usize)>,
}

impl CloseupMenu {
    /// The highlighted command row.
    pub(super) fn cursor(self) -> usize {
        self.cursor
    }

    /// Whether any row is expanded into an inline picker.
    pub(super) fn is_expanded(self) -> bool {
        self.expanded.is_some()
    }

    /// Whether the `close` row is expanded into the close picker.
    pub(super) fn is_close_expanded(self) -> bool {
        self.close_cursor().is_some()
    }

    /// The agent picker's highlighted index while expanded, or `None` collapsed.
    pub(super) fn agent_cursor(self) -> Option<usize> {
        match self.expanded {
            Some((CloseupSubmenu::Agent, cursor)) => Some(cursor),
            _ => None,
        }
    }

    /// The terminal picker's highlighted index while expanded, or `None`
    /// collapsed.
    pub(super) fn terminal_cursor(self) -> Option<usize> {
        match self.expanded {
            Some((CloseupSubmenu::Terminal, cursor)) => Some(cursor),
            _ => None,
        }
    }

    /// The close picker's highlighted index while expanded, or `None` collapsed.
    pub(super) fn close_cursor(self) -> Option<usize> {
        match self.expanded {
            Some((CloseupSubmenu::Close, cursor)) => Some(cursor),
            _ => None,
        }
    }

    /// Reset to the top, collapsed (entering 集中 / leaving for 選択).
    pub(super) fn reset(&mut self) {
        self.cursor = 0;
        self.expanded = None;
    }

    /// Re-home the cursor on the first row without touching any open picker — used
    /// when the 集中 menu filter (`/`) changes and the match list shifts under it,
    /// so the highlight lands on the first surviving command rather than a now
    /// out-of-range row.
    pub(super) fn reset_cursor(&mut self) {
        self.cursor = 0;
    }

    /// Expand an inline picker, highlighting `default_index` (clamped by the
    /// renderer).
    pub(super) fn expand(&mut self, submenu: CloseupSubmenu, default_index: usize) {
        self.expanded = Some((submenu, default_index));
    }

    /// Collapse an inline picker back to the normal menu. Returns whether it was
    /// expanded (so the caller can treat `←` / `Esc` as "consumed" only then).
    pub(super) fn collapse(&mut self) -> bool {
        self.expanded.take().is_some()
    }

    /// Expand the close picker, starting at option 0 (plain close).
    pub(super) fn expand_close(&mut self) {
        self.expanded = Some((CloseupSubmenu::Close, 0));
    }

    /// Collapse the close picker back to the normal menu. Returns whether it was
    /// expanded (so the caller can treat `←` / `Esc` as "consumed" only then).
    pub(super) fn collapse_close(&mut self) -> bool {
        if self.is_close_expanded() {
            self.expanded = None;
            true
        } else {
            false
        }
    }

    /// Move up one row, wrapping. Acts on whichever picker is open, or the
    /// command list when none is. `count` is clamped to at least 1.
    pub(super) fn move_up(&mut self, count: usize) {
        let count = count.max(1);
        match &mut self.expanded {
            Some((_, c)) => *c = c.checked_sub(1).unwrap_or(count - 1),
            None => self.cursor = self.cursor.checked_sub(1).unwrap_or(count - 1),
        }
    }

    /// Move down one row, wrapping (the mirror of [`move_up`](Self::move_up)).
    pub(super) fn move_down(&mut self, count: usize) {
        let count = count.max(1);
        match &mut self.expanded {
            Some((_, c)) => *c = (*c + 1) % count,
            None => self.cursor = (self.cursor + 1) % count,
        }
    }

    /// The selected command row, clamped to the available `count`.
    pub(super) fn selected(self, count: usize) -> usize {
        self.cursor.min(count.saturating_sub(1))
    }

    /// The selected agent-picker index, clamped to the available `count`. `0`
    /// when collapsed (no picker open).
    pub(super) fn agent_selected(self, count: usize) -> usize {
        self.agent_cursor()
            .unwrap_or(0)
            .min(count.saturating_sub(1))
    }

    /// The selected terminal-picker index, clamped to the available `count`. `0`
    /// when collapsed (no picker open).
    pub(super) fn terminal_selected(self, count: usize) -> usize {
        self.terminal_cursor()
            .unwrap_or(0)
            .min(count.saturating_sub(1))
    }

    /// The selected close-picker index, clamped to the available `count`. `0`
    /// when collapsed (no picker open).
    pub(super) fn close_selected(self, count: usize) -> usize {
        self.close_cursor()
            .unwrap_or(0)
            .min(count.saturating_sub(1))
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
    fn closeup_menu_moves_and_selects_the_command_cursor_when_collapsed() {
        let mut menu = CloseupMenu::default();
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
        let mut empty = CloseupMenu::default();
        empty.move_down(0);
        assert_eq!(empty.cursor(), 0);
    }

    #[test]
    fn closeup_menu_expand_collapse_drives_the_agent_picker() {
        let mut menu = CloseupMenu::default();
        // Expanding highlights the given default index and routes navigation to
        // the picker, leaving the command cursor untouched.
        menu.expand(CloseupSubmenu::Agent, 2);
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
    fn closeup_menu_reset_clears_both_cursors() {
        let mut menu = CloseupMenu::default();
        menu.move_down(3);
        menu.expand(CloseupSubmenu::Agent, 1);
        menu.reset();
        assert_eq!(menu.cursor(), 0);
        assert!(!menu.is_expanded());
    }

    #[test]
    fn closeup_menu_reset_cursor_rehomes_without_touching_the_picker() {
        let mut menu = CloseupMenu::default();
        menu.move_down(3);
        assert_eq!(menu.cursor(), 1);
        menu.reset_cursor();
        assert_eq!(menu.cursor(), 0);
    }

    #[test]
    fn closeup_menu_expand_can_drive_the_terminal_picker() {
        let mut menu = CloseupMenu::default();
        menu.expand(CloseupSubmenu::Terminal, 0);
        assert!(menu.is_expanded());
        assert_eq!(menu.agent_cursor(), None);
        assert_eq!(menu.terminal_cursor(), Some(0));
        menu.move_down(2);
        assert_eq!(menu.terminal_cursor(), Some(1));
        assert_eq!(menu.terminal_selected(2), 1);
        assert!(menu.collapse());
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
