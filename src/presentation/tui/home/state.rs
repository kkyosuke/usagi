//! Pure, terminal-independent state for the home (workspace) screen.
//!
//! The home screen is a small command shell laid out in three panes: the
//! worktree list (left), the command log (right), and a command input line
//! (bottom). [`HomeState`] holds all of it — the selectable worktree list, the
//! current mode, the input buffer and its history, and the output log — with no
//! terminal IO, so the navigation, editing, and command logic are all directly
//! testable.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::domain::workspace_state::{SessionRecord, WorktreeState};

use super::command::{CommandRegistry, Effect, Hint, WorktreeRef};
use super::terminal_view::TerminalView;

/// The display name of a worktree: its branch, or a placeholder when detached.
pub fn worktree_name(worktree: &WorktreeState) -> &str {
    worktree.branch.as_deref().unwrap_or("(detached)")
}

/// The name of the root row: the workspace itself, belonging to no session.
/// Used as its display label and to target it from `session switch root`.
pub const ROOT_NAME: &str = "root";

/// The opened workspace and the selectable list of its worktrees, preceded by a
/// synthetic *root row*.
///
/// The first row (index 0) is the workspace root, which belongs to no session:
/// activating it and running `terminal`/`agent` there works at the workspace
/// root rather than inside a session's worktree. Indices `1..=worktrees.len()`
/// are the recorded worktrees, so row `i` maps to `worktrees[i - 1]`.
///
/// Two cursors are tracked: `selected_index` is where the keyboard cursor sits
/// while navigating, and `active_index` is the row subsequent commands
/// (`session switch`, and later `terminal`/`ai`) act on. Both default to the
/// root row.
#[derive(Debug, Clone)]
pub struct WorktreeList {
    workspace_name: String,
    worktrees: Vec<WorktreeState>,
    selected_index: usize,
    active_index: usize,
}

impl WorktreeList {
    /// Builds a list for the named workspace, with both the cursor and the
    /// active row on the root (no session selected yet).
    pub fn new(workspace_name: impl Into<String>, worktrees: Vec<WorktreeState>) -> Self {
        Self {
            workspace_name: workspace_name.into(),
            worktrees,
            selected_index: 0,
            active_index: 0,
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

    /// Index of the active row (the one commands act on).
    pub fn active_index(&self) -> usize {
        self.active_index
    }

    /// Whether the workspace has no recorded worktrees (only the root row).
    pub fn is_empty(&self) -> bool {
        self.worktrees.is_empty()
    }

    /// Number of selectable rows: the root row plus every worktree (≥ 1).
    fn selectable_rows(&self) -> usize {
        self.worktrees.len() + 1
    }

    /// Number of sessions listed in the left pane: the root row plus every
    /// worktree (≥ 1). The title bar reports this so the header count matches
    /// the rows the user actually sees.
    pub fn session_count(&self) -> usize {
        self.selectable_rows()
    }

    /// The worktree at a selectable row: row 0 is the root (no worktree), and
    /// row `i` maps to `worktrees[i - 1]`.
    fn worktree_at(&self, row: usize) -> Option<&WorktreeState> {
        row.checked_sub(1).and_then(|i| self.worktrees.get(i))
    }

    /// The worktree under the cursor, or `None` when the cursor is on the root
    /// row (which belongs to no session).
    pub fn selected(&self) -> Option<&WorktreeState> {
        self.worktree_at(self.selected_index)
    }

    /// The active worktree, or `None` when the root row is active.
    pub fn active(&self) -> Option<&WorktreeState> {
        self.worktree_at(self.active_index)
    }

    /// Whether the cursor is on the root row.
    pub fn root_selected(&self) -> bool {
        self.selected_index == 0
    }

    /// Whether the root row is the active one.
    pub fn root_active(&self) -> bool {
        self.active_index == 0
    }

    /// Make the row under the cursor active, returning its display name (the
    /// branch name, or [`ROOT_NAME`] for the root row).
    pub fn activate_selected(&mut self) -> &str {
        self.active_index = self.selected_index;
        self.active_name()
    }

    /// The display name of the active row: its branch, or [`ROOT_NAME`] for the
    /// root row.
    fn active_name(&self) -> &str {
        self.active().map(worktree_name).unwrap_or(ROOT_NAME)
    }

    /// Make the row named `name` active, returning whether one matched. The
    /// root row matches [`ROOT_NAME`]; every other name is matched against the
    /// worktree branches.
    pub fn activate_by_name(&mut self, name: &str) -> bool {
        if name == ROOT_NAME {
            self.active_index = 0;
            return true;
        }
        match self.worktrees.iter().position(|w| worktree_name(w) == name) {
            Some(index) => {
                self.active_index = index + 1;
                true
            }
            None => false,
        }
    }

    /// Move both the cursor and the active row onto the first worktree named
    /// `name` (a freshly created session's branch), returning whether one
    /// matched. Used after creating a session so the new one is selected and
    /// active without the user navigating to it.
    pub fn select_by_name(&mut self, name: &str) -> bool {
        match self.worktrees.iter().position(|w| worktree_name(w) == name) {
            Some(index) => {
                self.selected_index = index + 1;
                self.active_index = index + 1;
                true
            }
            None => false,
        }
    }

    /// The rows as command-facing [`WorktreeRef`]s (name + active flag): the
    /// root row first, then every worktree.
    pub fn refs(&self) -> Vec<WorktreeRef> {
        let mut refs = vec![WorktreeRef {
            name: ROOT_NAME.to_string(),
            active: self.active_index == 0,
        }];
        refs.extend(self.worktrees.iter().enumerate().map(|(i, w)| WorktreeRef {
            name: worktree_name(w).to_string(),
            active: i + 1 == self.active_index,
        }));
        refs
    }

    /// Move the cursor directly to a selectable `row` (0 is the root row, `i`
    /// maps to `worktrees[i - 1]`), clamped to the rows that exist. Used by the
    /// session picker (`Ctrl-O`) to jump straight to the chosen session.
    pub fn focus_index(&mut self, row: usize) {
        self.selected_index = row.min(self.selectable_rows() - 1);
    }

    /// Move the cursor up one row, wrapping from the root row to the bottom.
    pub fn move_up(&mut self) {
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.selectable_rows() - 1);
    }

    /// Move the cursor down one row, wrapping from the bottom to the root row.
    pub fn move_down(&mut self) {
        self.selected_index = (self.selected_index + 1) % self.selectable_rows();
    }
}

/// Which part of the screen currently has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Navigating the worktree list (the default).
    Sidebar,
    /// Typing into the command input line.
    Command,
}

/// What the right pane is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightPane {
    /// The command output log (the default).
    Log,
    /// A live embedded terminal (the `terminal` command is running).
    Terminal,
}

/// Why the embedded terminal pane handed control back to the event loop.
///
/// The pane is driven by the impure terminal loop (`terminal_pane`); this enum
/// is the small, testable vocabulary it returns so the event loop can decide
/// what to do next — keep the shell alive and return to the sidebar, close it,
/// or re-root the pane at the session the picker just focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneExit {
    /// The user detached (`Ctrl-O` then `Ctrl-O`): the shell stays alive in the
    /// pool and the pane returns to the sidebar.
    Detach,
    /// The shell exited on its own (e.g. the user typed `exit`); it is gone.
    Closed,
    /// The session picker (`Ctrl-O`) chose a session — already focused in the
    /// list — so re-root the pane there, keeping it open.
    Switch,
    /// The session picker (`Ctrl-O` then `c`) asked to create a new session: the
    /// event loop opens the name modal, and on success re-roots the pane at the
    /// freshly created session (as a plain shell), keeping it open.
    Create,
}

/// The kind of a log line, which decides how it is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A command the user entered (echoed back).
    Command,
    /// Ordinary command output.
    Output,
    /// An error (e.g. an unknown command).
    Error,
    /// A transient notice (e.g. a "coming soon" message).
    Notice,
}

/// A single line in the output log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub kind: LineKind,
    pub text: String,
}

impl LogLine {
    pub fn command(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Command,
            text: text.into(),
        }
    }

    pub fn output(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Output,
            text: text.into(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Error,
            text: text.into(),
        }
    }

    pub fn notice(text: impl Into<String>) -> Self {
        Self {
            kind: LineKind::Notice,
            text: text.into(),
        }
    }
}

/// The outcome of submitting the command line: the side effect to act on, plus
/// the command that was recorded in history (so the event loop can persist it).
#[derive(Debug)]
pub struct Submission {
    pub effect: Effect,
    /// The command that was run and added to history, or `None` for empty input.
    pub recorded: Option<String>,
}

/// The open session-name modal: the name being typed plus an optional inline
/// validation error (e.g. an empty or duplicate name).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionModal {
    input: String,
    error: Option<String>,
}

impl SessionModal {
    /// The name typed so far.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// The current validation error, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
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

/// The result of attempting to create a session, applied back to the screen by
/// [`HomeState::apply_session_outcome`]. The impure work (git / filesystem) is
/// done by the event loop's callback; this carries only what the screen shows.
#[derive(Debug, Clone)]
pub struct SessionOutcome {
    /// A line describing the result (success or failure) to append to the log.
    pub line: LogLine,
    /// The refreshed session list, when the action changed it. The worktree
    /// pane is rebuilt from this (each session contributes its worktrees).
    pub sessions: Option<Vec<SessionRecord>>,
    /// The name of a session to select (and make active) once the pane is
    /// rebuilt — set when creating a session so the new one is selected. `None`
    /// leaves the cursor on the root row (e.g. removals and failures).
    pub select: Option<String>,
}

/// The open session picker: the in-terminal overlay (`Ctrl-O`) that lists every
/// session — the root row plus each worktree — so the user can switch the live
/// pane to another without leaving it. `names` are the rows in display order
/// (index 0 is the root), `cursor` is the highlighted row, and `current` marks
/// the session the pane is presently rooted at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionPicker {
    names: Vec<String>,
    cursor: usize,
    current: usize,
}

impl SessionPicker {
    /// The session names, in display order (index 0 is the root row).
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The row the keyboard cursor sits on.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The row the live pane is currently rooted at (marked as "here").
    pub fn current(&self) -> usize {
        self.current
    }
}

/// The full state of the home screen.
///
/// Not `Clone`/`Debug`: it owns a [`CommandRegistry`] of trait objects, which
/// are neither. Nothing needs to clone or format the whole screen state.
pub struct HomeState {
    list: WorktreeList,
    mode: Mode,
    input: String,
    history: Vec<String>,
    /// Index into `history` while recalling past commands; `None` when editing
    /// a fresh line.
    recall: Option<usize>,
    log: Vec<LogLine>,
    /// The commands available in command mode (the extension point for the
    /// follow-up command features).
    registry: CommandRegistry,
    /// The session-name modal, when open. While set it captures all keys.
    modal: Option<SessionModal>,
    /// The session-removal modal, when open (the user ran `session remove`
    /// without a name). While set it captures all keys, like `modal`.
    remove_modal: Option<RemoveModal>,
    /// Sessions recorded for this workspace (from `state.json`), shown by
    /// `session list` and kept current as sessions are created.
    sessions: Vec<SessionRecord>,
    /// What the right pane shows: the command log, or a live terminal.
    right_pane: RightPane,
    /// The latest snapshot of the embedded terminal's screen, set while the
    /// `terminal` command is running and rendered in place of the log.
    terminal_view: Option<TerminalView>,
    /// Worktree paths whose background session is waiting for the user (its
    /// agent rang the bell). Refreshed from the terminal monitor each redraw and
    /// rendered as a marker in the sidebar.
    waiting: HashSet<PathBuf>,
    /// Worktree paths with a live (running) embedded session — an agent/shell is
    /// in use, whether attached or left running in the background. Refreshed from
    /// the terminal monitor each redraw and rendered with a "running" icon,
    /// unless the path is also waiting (which takes precedence).
    live: HashSet<PathBuf>,
    /// How many lines the command-log pane is scrolled up from its tail; `0`
    /// pins it to the newest line. Bumped by the wheel / `PageUp` over the right
    /// pane and reset to the bottom whenever fresh output arrives.
    right_scroll: usize,
    /// The session picker, when open (the user pressed `Ctrl-O` inside the live
    /// terminal). While set it overlays the pane and captures its keys.
    session_picker: Option<SessionPicker>,
}

impl HomeState {
    /// Builds the screen state for `workspace_name` and its `worktrees`. An
    /// optional `notice` (e.g. a load error) seeds the log below a short hint.
    pub fn new(
        workspace_name: impl Into<String>,
        worktrees: Vec<WorktreeState>,
        notice: Option<String>,
    ) -> Self {
        let mut log = vec![LogLine::output(
            "Type \":\" to enter a command, then \"man\" for help.",
        )];
        if let Some(notice) = notice {
            log.push(LogLine::error(notice));
        }
        Self {
            list: WorktreeList::new(workspace_name, worktrees),
            mode: Mode::Sidebar,
            input: String::new(),
            history: Vec::new(),
            recall: None,
            log,
            registry: CommandRegistry::with_builtins(),
            modal: None,
            remove_modal: None,
            sessions: Vec::new(),
            right_pane: RightPane::Log,
            terminal_view: None,
            waiting: HashSet::new(),
            live: HashSet::new(),
            right_scroll: 0,
            session_picker: None,
        }
    }

    /// Seed the command history with entries restored from disk (oldest first),
    /// so `history` and `↑`/`↓` recall reflect commands run in past sessions.
    pub fn restore_history(&mut self, entries: Vec<String>) {
        self.history = entries;
    }

    /// Seed the recorded sessions (from `state.json`), shown by `session list`,
    /// and rebuild the worktree pane from them.
    pub fn restore_sessions(&mut self, sessions: Vec<SessionRecord>) {
        self.sessions = sessions;
        self.rebuild_list();
    }

    /// Rebuild the worktree pane from the current sessions: every session
    /// contributes its per-repository worktrees, in order.
    fn rebuild_list(&mut self) {
        let name = self.list.workspace_name().to_string();
        let worktrees = self
            .sessions
            .iter()
            .flat_map(|s| s.worktrees.iter().cloned())
            .collect();
        self.list = WorktreeList::new(name, worktrees);
    }

    pub fn sessions(&self) -> &[SessionRecord] {
        &self.sessions
    }

    /// Append the recorded sessions to the log (the `session list` command).
    pub fn log_sessions(&mut self) {
        self.reset_right_scroll();
        if self.sessions.is_empty() {
            self.log.push(LogLine::output(
                "No sessions yet. Run \"session create <name>\" to create one.",
            ));
            return;
        }
        self.log.push(LogLine::output(format!(
            "{} session(s):",
            self.sessions.len()
        )));
        for session in &self.sessions {
            self.log.push(LogLine::output(format!(
                "  {}  ({} worktree(s))",
                session.name,
                session.worktrees.len()
            )));
        }
    }

    /// Append an ordinary output line to the log (used by the event loop to
    /// report the result of a command's side effect, e.g. `terminal`).
    pub fn log_output(&mut self, text: impl Into<String>) {
        self.reset_right_scroll();
        self.log.push(LogLine::output(text));
    }

    /// Append an error line to the log.
    pub fn log_error(&mut self, text: impl Into<String>) {
        self.reset_right_scroll();
        self.log.push(LogLine::error(text));
    }

    pub fn list(&self) -> &WorktreeList {
        &self.list
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    /// The advisory input hint for the current command input (matching commands,
    /// or the usage of the command being given arguments). Computed on demand
    /// for rendering; see [`CommandRegistry::suggest`].
    pub fn hint(&self) -> Hint {
        self.registry.suggest(&self.input)
    }

    pub fn log(&self) -> &[LogLine] {
        &self.log
    }

    /// What the right pane is currently showing.
    pub fn right_pane(&self) -> RightPane {
        self.right_pane
    }

    /// How many lines the command-log pane is scrolled up from its newest line
    /// (`0` is pinned to the bottom).
    pub fn right_scroll(&self) -> usize {
        self.right_scroll
    }

    /// Scroll the command-log pane up by `lines` toward older output, never past
    /// the oldest line that a `viewport_rows`-tall pane could show.
    pub fn scroll_log_up(&mut self, lines: usize, viewport_rows: usize) {
        let max = self.log.len().saturating_sub(viewport_rows);
        self.right_scroll = (self.right_scroll + lines).min(max);
    }

    /// Scroll the command-log pane down by `lines` toward the newest output,
    /// stopping when it is pinned to the bottom.
    pub fn scroll_log_down(&mut self, lines: usize) {
        self.right_scroll = self.right_scroll.saturating_sub(lines);
    }

    /// Pin the command-log pane back to its newest line, so fresh output is
    /// always in view after it arrives.
    fn reset_right_scroll(&mut self) {
        self.right_scroll = 0;
    }

    /// The current embedded-terminal snapshot, when the terminal is running.
    pub fn terminal_view(&self) -> Option<&TerminalView> {
        self.terminal_view.as_ref()
    }

    /// Switch the right pane to the live terminal (the `terminal` command is
    /// starting). The first snapshot arrives via [`set_terminal_view`].
    ///
    /// [`set_terminal_view`]: Self::set_terminal_view
    pub fn show_terminal(&mut self) {
        self.right_pane = RightPane::Terminal;
    }

    /// Switch the right pane back to the command log (the terminal closed),
    /// dropping the last terminal snapshot.
    pub fn show_log(&mut self) {
        self.right_pane = RightPane::Log;
        self.terminal_view = None;
        self.reset_right_scroll();
    }

    /// Store the latest embedded-terminal screen snapshot, shown in the right
    /// pane while the terminal is running.
    pub fn set_terminal_view(&mut self, view: TerminalView) {
        self.terminal_view = Some(view);
    }

    /// Replace the set of worktree paths whose background session is waiting for
    /// the user, refreshed from the terminal monitor before each redraw.
    pub fn set_waiting(&mut self, waiting: HashSet<PathBuf>) {
        self.waiting = waiting;
    }

    /// Whether the worktree at `path` has a background session waiting for input.
    pub fn is_waiting(&self, path: &Path) -> bool {
        self.waiting.contains(path)
    }

    /// The set of worktree paths whose background session is waiting for input,
    /// for the sidebar renderer.
    pub fn waiting_paths(&self) -> &HashSet<PathBuf> {
        &self.waiting
    }

    /// Replace the set of worktree paths with a live (running) embedded session,
    /// refreshed from the terminal monitor before each redraw.
    pub fn set_live(&mut self, live: HashSet<PathBuf>) {
        self.live = live;
    }

    /// Whether the worktree at `path` has a live (running) embedded session.
    pub fn is_live(&self, path: &Path) -> bool {
        self.live.contains(path)
    }

    /// The set of worktree paths with a live (running) embedded session, for the
    /// sidebar renderer.
    pub fn live_paths(&self) -> &HashSet<PathBuf> {
        &self.live
    }

    /// Move the worktree cursor up (sidebar mode).
    pub fn move_up(&mut self) {
        self.list.move_up();
    }

    /// Move the worktree cursor down (sidebar mode).
    pub fn move_down(&mut self) {
        self.list.move_down();
    }

    /// Make the row under the cursor the active one (the target of subsequent
    /// commands), logging the switch. The root row switches to "root".
    pub fn select_worktree(&mut self) {
        let name = self.list.activate_selected().to_string();
        self.log
            .push(LogLine::notice(format!("Switched to \"{name}\"")));
    }

    /// Focus the session at `row` (0 is the root row, `i` maps to worktree
    /// `i - 1`) in the list, so the embedded terminal re-roots there. Used by
    /// the session picker on confirm.
    pub fn focus_session(&mut self, row: usize) {
        self.list.focus_index(row);
    }

    /// The open session picker, if any.
    pub fn session_picker(&self) -> Option<&SessionPicker> {
        self.session_picker.as_ref()
    }

    /// Open the session picker, listing the root row then every worktree, with
    /// the cursor on — and "here" marking — the session the pane is rooted at.
    pub fn open_session_picker(&mut self) {
        let mut names = vec![ROOT_NAME.to_string()];
        names.extend(
            self.list
                .worktrees()
                .iter()
                .map(worktree_name)
                .map(String::from),
        );
        let current = self.list.selected_index();
        self.session_picker = Some(SessionPicker {
            names,
            cursor: current,
            current,
        });
    }

    /// Move the picker cursor up one row, wrapping to the bottom. No-op when the
    /// picker is closed.
    pub fn session_picker_move_up(&mut self) {
        if let Some(picker) = self.session_picker.as_mut() {
            picker.cursor = picker
                .cursor
                .checked_sub(1)
                .unwrap_or(picker.names.len() - 1);
        }
    }

    /// Move the picker cursor down one row, wrapping to the top. No-op when the
    /// picker is closed.
    pub fn session_picker_move_down(&mut self) {
        if let Some(picker) = self.session_picker.as_mut() {
            picker.cursor = (picker.cursor + 1) % picker.names.len();
        }
    }

    /// Move the picker cursor to the 1-based session `number`, returning whether
    /// it was in range. No-op (returning `false`) when out of range or the
    /// picker is closed.
    pub fn session_picker_select_number(&mut self, number: usize) -> bool {
        let Some(picker) = self.session_picker.as_mut() else {
            return false;
        };
        match number.checked_sub(1) {
            Some(row) if row < picker.names.len() => {
                picker.cursor = row;
                true
            }
            _ => false,
        }
    }

    /// Close the picker without switching.
    pub fn cancel_session_picker(&mut self) {
        self.session_picker = None;
    }

    /// Confirm the picker: close it, focus the highlighted session, and return
    /// its row (so the terminal pane re-roots there). A no-op returning `None`
    /// when the picker is closed.
    pub fn confirm_session_picker(&mut self) -> Option<usize> {
        let picker = self.session_picker.take()?;
        self.focus_session(picker.cursor);
        Some(picker.cursor)
    }

    /// Switch from the sidebar to the command input line.
    pub fn enter_command_mode(&mut self) {
        self.mode = Mode::Command;
    }

    /// Leave the command input line, discarding the half-typed input.
    pub fn leave_command_mode(&mut self) {
        self.mode = Mode::Sidebar;
        self.input.clear();
        self.recall = None;
    }

    /// Append a typed character to the input (command mode).
    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
        self.recall = None;
    }

    /// Delete the last character of the input (command mode).
    pub fn backspace(&mut self) {
        self.input.pop();
        self.recall = None;
    }

    /// Tab-complete the command word, listing candidates when ambiguous.
    pub fn complete(&mut self) {
        let completion = self.registry.complete(&self.input);
        self.input = completion.input;
        if !completion.candidates.is_empty() {
            self.log
                .push(LogLine::output(completion.candidates.join("  ")));
        }
        self.recall = None;
    }

    /// Recall the previous (older) command into the input.
    pub fn recall_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.recall {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.recall = Some(index);
        self.input = self.history[index].clone();
    }

    /// Recall the next (newer) command, returning to an empty line past the end.
    pub fn recall_next(&mut self) {
        let index = match self.recall {
            None => return,
            Some(i) => i,
        };
        if index + 1 < self.history.len() {
            self.recall = Some(index + 1);
            self.input = self.history[index + 1].clone();
        } else {
            self.recall = None;
            self.input.clear();
        }
    }

    /// Run the current input as a command: echo it, dispatch it, record it in
    /// history, and apply the resulting log lines and side effect. Returns a
    /// [`Submission`] carrying the side effect (so the event loop can act on
    /// `Quit`) and the recorded command (so it can be persisted). Empty input is
    /// a no-op.
    pub fn submit(&mut self) -> Submission {
        let entry = self.input.trim().to_string();
        self.input.clear();
        self.recall = None;
        if entry.is_empty() {
            return Submission {
                effect: Effect::None,
                recorded: None,
            };
        }

        self.reset_right_scroll();
        self.log.push(LogLine::command(entry.clone()));
        let result = self
            .registry
            .dispatch(&entry, &self.history, &self.list.refs());
        self.history.push(entry.clone());

        match result.effect {
            Effect::Clear => self.log.clear(),
            // `session switch <name>`: the screen owns the worktree list, so it resolves
            // the name and reports success or failure here.
            Effect::Activate(ref name) => {
                if self.list.activate_by_name(name) {
                    self.log
                        .push(LogLine::notice(format!("Switched to \"{name}\"")));
                } else {
                    self.log
                        .push(LogLine::error(format!("no worktree named \"{name}\"")));
                }
            }
            _ => self.log.extend(result.lines),
        }
        Submission {
            effect: result.effect,
            recorded: Some(entry),
        }
    }

    /// The open session-name modal, if any.
    pub fn modal(&self) -> Option<&SessionModal> {
        self.modal.as_ref()
    }

    /// Open the session-name modal with an empty input.
    pub fn open_session_modal(&mut self) {
        self.modal = Some(SessionModal::default());
    }

    /// Append a typed character to the modal's name (no-op when closed).
    pub fn modal_push_char(&mut self, c: char) {
        if let Some(modal) = self.modal.as_mut() {
            modal.input.push(c);
            modal.error = None;
        }
    }

    /// Delete the last character of the modal's name (no-op when closed).
    pub fn modal_backspace(&mut self) {
        if let Some(modal) = self.modal.as_mut() {
            modal.input.pop();
            modal.error = None;
        }
    }

    /// Close the modal, discarding the half-typed name.
    pub fn cancel_modal(&mut self) {
        self.modal = None;
    }

    /// Validate and accept the modal's name. On success the modal closes and the
    /// trimmed name is returned (for the event loop to create the session); on an
    /// empty or duplicate name the modal stays open with an inline error and
    /// `None` is returned. A no-op (returning `None`) when the modal is closed.
    pub fn submit_modal(&mut self) -> Option<String> {
        let modal = self.modal.as_mut()?;
        let name = modal.input.trim().to_string();
        if name.is_empty() {
            modal.error = Some("Name must not be empty.".to_string());
            return None;
        }
        if self
            .list
            .worktrees()
            .iter()
            .any(|w| w.branch.as_deref() == Some(name.as_str()))
        {
            modal.error = Some(format!("\"{name}\" already exists."));
            return None;
        }
        self.modal = None;
        Some(name)
    }

    /// Apply the result of a session-creation attempt: log its line and, when
    /// creation refreshed the worktree list, swap it in.
    pub fn apply_session_outcome(&mut self, outcome: SessionOutcome) {
        self.reset_right_scroll();
        self.log.push(outcome.line);
        if let Some(sessions) = outcome.sessions {
            self.sessions = sessions;
            self.rebuild_list();
            if let Some(name) = outcome.select {
                self.list.select_by_name(&name);
            }
        }
    }

    /// The open session-removal modal, if any.
    pub fn remove_modal(&self) -> Option<&RemoveModal> {
        self.remove_modal.as_ref()
    }

    /// Open the session-removal modal, seeded with the current session names and
    /// nothing selected. `force` is carried from `session remove --force`.
    pub fn open_remove_modal(&mut self, force: bool) {
        self.remove_modal = Some(RemoveModal {
            names: self.sessions.iter().map(|s| s.name.clone()).collect(),
            cursor: 0,
            selected: HashSet::new(),
            force,
        });
    }

    /// Move the removal cursor up one row, wrapping to the bottom. No-op when
    /// the modal is closed or has no sessions.
    pub fn remove_modal_move_up(&mut self) {
        if let Some(modal) = self.remove_modal.as_mut() {
            if modal.names.is_empty() {
                return;
            }
            modal.cursor = modal.cursor.checked_sub(1).unwrap_or(modal.names.len() - 1);
        }
    }

    /// Move the removal cursor down one row, wrapping to the top. No-op when the
    /// modal is closed or has no sessions.
    pub fn remove_modal_move_down(&mut self) {
        if let Some(modal) = self.remove_modal.as_mut() {
            if modal.names.is_empty() {
                return;
            }
            modal.cursor = (modal.cursor + 1) % modal.names.len();
        }
    }

    /// Toggle the checked state of the session under the cursor. No-op when the
    /// modal is closed or has no sessions.
    pub fn remove_modal_toggle(&mut self) {
        if let Some(modal) = self.remove_modal.as_mut() {
            if modal.names.is_empty() {
                return;
            }
            if !modal.selected.insert(modal.cursor) {
                modal.selected.remove(&modal.cursor);
            }
        }
    }

    /// Close the removal modal, discarding any selection.
    pub fn cancel_remove_modal(&mut self) {
        self.remove_modal = None;
    }

    /// Confirm the removal modal: close it and return the checked session names
    /// (in display order) together with the `--force` flag, for the event loop
    /// to remove each. Returns `None` when nothing is checked, leaving the modal
    /// open so the user can pick something or cancel. A no-op (returning `None`)
    /// when the modal is closed.
    pub fn submit_remove_modal(&mut self) -> Option<(Vec<String>, bool)> {
        let modal = self.remove_modal.as_ref()?;
        if modal.selected.is_empty() {
            return None;
        }
        let names = modal
            .names
            .iter()
            .enumerate()
            .filter(|(i, _)| modal.selected.contains(i))
            .map(|(_, name)| name.clone())
            .collect();
        let force = modal.force;
        self.remove_modal = None;
        Some((names, force))
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
    fn new_list_starts_on_the_root_row() {
        let list = sample();
        assert_eq!(list.workspace_name(), "usagi");
        // The cursor starts on the root row, which belongs to no session.
        assert_eq!(list.selected_index(), 0);
        assert!(list.root_selected());
        assert!(list.selected().is_none());
        assert_eq!(list.worktrees().len(), 3);
        assert!(!list.is_empty());
    }

    #[test]
    fn empty_list_still_has_the_root_row() {
        let list = WorktreeList::new("usagi", Vec::new());
        assert!(list.is_empty());
        assert!(list.root_selected());
        // The root row has no worktree behind it.
        assert!(list.selected().is_none());
    }

    #[test]
    fn move_down_advances_past_the_root_row_and_wraps() {
        let mut list = sample(); // root, main, feature, fix
        list.move_down();
        assert_eq!(list.selected_index(), 1);
        assert_eq!(list.selected().unwrap().branch.as_deref(), Some("main"));
        list.move_down();
        list.move_down();
        assert_eq!(list.selected_index(), 3);
        assert_eq!(list.selected().unwrap().branch.as_deref(), Some("fix"));
        // Wraps from the last worktree back to the root row.
        list.move_down();
        assert_eq!(list.selected_index(), 0);
        assert!(list.root_selected());
    }

    #[test]
    fn move_up_wraps_from_the_root_row_to_the_bottom() {
        let mut list = sample(); // root, main, feature, fix
        list.move_up();
        assert_eq!(list.selected_index(), 3);
        assert_eq!(list.selected().unwrap().branch.as_deref(), Some("fix"));
        list.move_up();
        assert_eq!(list.selected_index(), 2);
        assert_eq!(list.selected().unwrap().branch.as_deref(), Some("feature"));
    }

    #[test]
    fn movement_wraps_around_the_lone_root_row_when_empty() {
        let mut list = WorktreeList::new("usagi", Vec::new());
        // Only the root row exists, so movement keeps the cursor on it.
        list.move_up();
        assert_eq!(list.selected_index(), 0);
        list.move_down();
        assert_eq!(list.selected_index(), 0);
    }

    #[test]
    fn the_root_row_is_active_by_default() {
        let list = sample();
        assert_eq!(list.active_index(), 0);
        assert!(list.root_active());
        assert!(list.active().is_none());
    }

    #[test]
    fn activate_selected_follows_the_cursor() {
        let mut list = sample(); // root, main, feature, fix
        list.move_down();
        list.move_down(); // cursor on "feature"
        assert_eq!(list.activate_selected(), "feature");
        assert_eq!(list.active_index(), 2);
        assert!(!list.root_active());
        // The cursor and the active row are independent afterwards.
        list.move_down(); // cursor on "fix"
        assert_eq!(list.active_index(), 2);
        assert_eq!(list.selected_index(), 3);
    }

    #[test]
    fn activate_selected_can_return_to_the_root_row() {
        let mut list = sample();
        list.move_down(); // cursor on "main"
        list.activate_selected();
        assert!(!list.root_active());
        // Moving back to the root row and activating it returns to "root".
        list.move_up(); // cursor on the root row
        assert_eq!(list.activate_selected(), ROOT_NAME);
        assert!(list.root_active());
    }

    #[test]
    fn activate_selected_on_an_empty_list_picks_the_root_row() {
        let mut list = WorktreeList::new("usagi", Vec::new());
        assert_eq!(list.activate_selected(), ROOT_NAME);
        assert!(list.root_active());
        assert!(list.active().is_none());
    }

    #[test]
    fn activate_by_name_matches_worktrees_the_root_or_reports_missing() {
        let mut list = sample(); // root, main, feature, fix
        assert!(list.activate_by_name("fix"));
        assert_eq!(list.active_index(), 3);
        // The root row is reachable by name too.
        assert!(list.activate_by_name(ROOT_NAME));
        assert_eq!(list.active_index(), 0);
        assert!(list.root_active());
        assert!(!list.activate_by_name("nope"));
        // A failed lookup leaves the active row unchanged.
        assert_eq!(list.active_index(), 0);
    }

    #[test]
    fn select_by_name_moves_the_cursor_and_active_row_to_the_match() {
        let mut list = sample(); // root, main, feature, fix
        assert!(list.select_by_name("feature"));
        // Both the cursor and the active row land on the matched worktree.
        assert_eq!(list.selected_index(), 2);
        assert_eq!(list.active_index(), 2);
        assert_eq!(list.selected().unwrap().branch.as_deref(), Some("feature"));
        // An unknown name leaves both cursors unchanged.
        assert!(!list.select_by_name("nope"));
        assert_eq!(list.selected_index(), 2);
        assert_eq!(list.active_index(), 2);
    }

    #[test]
    fn refs_expose_the_root_row_then_worktrees_with_the_active_flag() {
        let mut list = sample();
        list.activate_by_name("feature");
        let refs = list.refs();
        assert_eq!(refs.len(), 4);
        assert_eq!(refs[0].name, ROOT_NAME);
        assert!(!refs[0].active);
        assert_eq!(refs[1].name, "main");
        assert!(!refs[1].active);
        assert_eq!(refs[2].name, "feature");
        assert!(refs[2].active);
    }

    #[test]
    fn refs_mark_the_root_row_active_by_default() {
        let refs = sample().refs();
        assert_eq!(refs[0].name, ROOT_NAME);
        assert!(refs[0].active);
    }

    #[test]
    fn worktree_name_falls_back_to_detached() {
        let mut detached = worktree("main");
        detached.branch = None;
        assert_eq!(worktree_name(&detached), "(detached)");
    }

    // --- HomeState ---------------------------------------------------------

    fn state() -> HomeState {
        HomeState::new("usagi", vec![worktree("main"), worktree("feature")], None)
    }

    #[test]
    fn new_state_starts_in_sidebar_with_a_hint() {
        let state = state();
        assert_eq!(state.mode(), Mode::Sidebar);
        assert_eq!(state.input(), "");
        assert_eq!(state.list().worktrees().len(), 2);
        // The seed log carries the usage hint.
        assert_eq!(state.log().len(), 1);
        assert!(state.log()[0].text.contains("man"));
    }

    #[test]
    fn a_notice_is_seeded_as_an_error_line() {
        let state = HomeState::new("usagi", Vec::new(), Some("load failed".to_string()));
        assert_eq!(state.log().len(), 2);
        assert_eq!(state.log()[1].kind, LineKind::Error);
        assert_eq!(state.log()[1].text, "load failed");
    }

    #[test]
    fn worktree_navigation_delegates_to_the_list() {
        let mut state = state();
        state.move_down();
        assert_eq!(state.list().selected_index(), 1);
        state.move_up();
        assert_eq!(state.list().selected_index(), 0);
    }

    #[test]
    fn selecting_a_worktree_activates_it() {
        let mut state = state(); // root, main, feature
        state.move_down();
        state.move_down(); // cursor on "feature"
        state.select_worktree();
        assert_eq!(state.list().active_index(), 2);
        let last = state.log().last().unwrap();
        assert_eq!(last.kind, LineKind::Notice);
        assert!(last.text.contains("feature"));
        assert!(last.text.contains("Switched"));
    }

    #[test]
    fn selecting_the_root_row_switches_to_root() {
        // The cursor starts on the root row; activating it switches to "root".
        let mut state = HomeState::new("usagi", Vec::new(), None);
        state.select_worktree();
        assert!(state.list().root_active());
        let last = state.log().last().unwrap();
        assert_eq!(last.kind, LineKind::Notice);
        assert!(last.text.contains("root"));
        assert!(last.text.contains("Switched"));
    }

    #[test]
    fn mode_switching_clears_half_typed_input() {
        let mut state = state();
        state.enter_command_mode();
        assert_eq!(state.mode(), Mode::Command);
        state.push_char('a');
        state.push_char('b');
        assert_eq!(state.input(), "ab");
        state.leave_command_mode();
        assert_eq!(state.mode(), Mode::Sidebar);
        assert_eq!(state.input(), "");
    }

    #[test]
    fn backspace_removes_the_last_character() {
        let mut state = state();
        state.push_char('m');
        state.push_char('a');
        state.backspace();
        assert_eq!(state.input(), "m");
        state.backspace();
        state.backspace(); // popping past empty is harmless
        assert_eq!(state.input(), "");
    }

    #[test]
    fn tab_completes_a_unique_command() {
        let mut state = state();
        state.push_char('d');
        state.push_char('o');
        state.push_char('c');
        state.complete();
        assert_eq!(state.input(), "doctor");
        // A unique completion adds nothing to the log.
        assert_eq!(state.log().len(), 1);
    }

    #[test]
    fn tab_lists_candidates_when_ambiguous() {
        let mut state = state();
        // Empty input matches every command, so Tab lists them all as candidates.
        state.complete();
        assert_eq!(state.input(), "");
        let last = state.log().last().unwrap();
        assert!(last.text.contains("session"));
        assert!(last.text.contains("man"));
    }

    #[test]
    fn submitting_an_empty_line_is_a_noop() {
        let mut state = state();
        let before = state.log().len();
        let submission = state.submit();
        assert_eq!(submission.effect, Effect::None);
        assert!(submission.recorded.is_none());
        assert_eq!(state.log().len(), before);
    }

    #[test]
    fn submitting_a_command_echoes_and_runs_it() {
        let mut state = state();
        for c in "man".chars() {
            state.push_char(c);
        }
        let submission = state.submit();
        assert_eq!(submission.effect, Effect::None);
        assert_eq!(submission.recorded.as_deref(), Some("man"));
        // The echoed command line is followed by the man output.
        let echoed = state.log().iter().find(|l| l.kind == LineKind::Command);
        assert_eq!(echoed.unwrap().text, "man");
        assert!(state.log().iter().any(|l| l.text.contains("Available")));
        assert_eq!(state.input(), "");
    }

    #[test]
    fn session_switch_changes_the_active_worktree() {
        let mut state = state(); // root (active), main, feature
        for c in "session switch feature".chars() {
            state.push_char(c);
        }
        let submission = state.submit();
        assert!(matches!(submission.effect, Effect::Activate(_)));
        assert_eq!(state.list().active_index(), 2);
        let last = state.log().last().unwrap();
        assert_eq!(last.kind, LineKind::Notice);
        assert!(last.text.contains("feature"));
    }

    #[test]
    fn session_switch_root_returns_to_the_root_row() {
        let mut state = state(); // root (active), main, feature
        state.list.activate_by_name("feature");
        assert!(!state.list().root_active());
        for c in "session switch root".chars() {
            state.push_char(c);
        }
        state.submit();
        assert!(state.list().root_active());
        let last = state.log().last().unwrap();
        assert_eq!(last.kind, LineKind::Notice);
        assert!(last.text.contains("root"));
    }

    #[test]
    fn session_switch_with_an_unknown_name_errors() {
        let mut state = state();
        for c in "session switch nope".chars() {
            state.push_char(c);
        }
        state.submit();
        // The active worktree is unchanged and an error is logged.
        assert_eq!(state.list().active_index(), 0);
        let last = state.log().last().unwrap();
        assert_eq!(last.kind, LineKind::Error);
        assert!(last.text.contains("no worktree named"));
    }

    #[test]
    fn clear_command_empties_the_log() {
        let mut state = state();
        for c in "clear".chars() {
            state.push_char(c);
        }
        assert_eq!(state.submit().effect, Effect::Clear);
        assert!(state.log().is_empty());
    }

    #[test]
    fn quit_command_returns_the_quit_effect() {
        let mut state = state();
        for c in "quit".chars() {
            state.push_char(c);
        }
        assert_eq!(state.submit().effect, Effect::Quit);
    }

    #[test]
    fn submitted_commands_are_recorded_in_history() {
        let mut state = state();
        for c in "man".chars() {
            state.push_char(c);
        }
        state.submit();
        for c in "doctor".chars() {
            state.push_char(c);
        }
        state.submit();
        assert_eq!(state.history, vec!["man", "doctor"]);
    }

    #[test]
    fn restored_history_feeds_recall_and_new_commands_append_to_it() {
        let mut state = state();
        state.restore_history(vec!["session".to_string(), "space".to_string()]);
        // Recall walks the restored entries.
        state.enter_command_mode();
        state.recall_prev();
        assert_eq!(state.input(), "space");
        state.recall_prev();
        assert_eq!(state.input(), "session");
        // A freshly run command is appended after the restored ones.
        state.input = "man".to_string();
        state.submit();
        assert_eq!(state.history, vec!["session", "space", "man"]);
    }

    #[test]
    fn history_recall_walks_backwards_and_forwards() {
        let mut state = state();
        for entry in ["man", "doctor"] {
            for c in entry.chars() {
                state.push_char(c);
            }
            state.submit();
        }

        // Recalling with no prior recall jumps to the most recent entry.
        state.recall_prev();
        assert_eq!(state.input(), "doctor");
        // Older.
        state.recall_prev();
        assert_eq!(state.input(), "man");
        // Already at the oldest — stays put.
        state.recall_prev();
        assert_eq!(state.input(), "man");
        // Forward again towards the newest.
        state.recall_next();
        assert_eq!(state.input(), "doctor");
        // Past the newest returns to an empty line.
        state.recall_next();
        assert_eq!(state.input(), "");
    }

    #[test]
    fn recall_prev_is_a_noop_without_history() {
        let mut state = state();
        state.recall_prev();
        assert_eq!(state.input(), "");
    }

    #[test]
    fn recall_next_without_active_recall_is_a_noop() {
        let mut state = state();
        for c in "man".chars() {
            state.push_char(c);
        }
        state.submit();
        // No recall in progress: recall_next does nothing.
        state.recall_next();
        assert_eq!(state.input(), "");
    }

    #[test]
    fn session_command_without_a_name_opens_the_modal() {
        let mut state = state();
        for c in "session new".chars() {
            state.push_char(c);
        }
        let submission = state.submit();
        assert_eq!(submission.effect, Effect::OpenSessionModal);
        // The event loop turns the effect into an open modal.
        assert!(state.modal().is_none());
        state.open_session_modal();
        assert_eq!(state.modal().unwrap().input(), "");
        assert!(state.modal().unwrap().error().is_none());
    }

    #[test]
    fn modal_editing_appends_and_deletes_characters() {
        let mut state = state();
        state.open_session_modal();
        state.modal_push_char('a');
        state.modal_push_char('b');
        assert_eq!(state.modal().unwrap().input(), "ab");
        state.modal_backspace();
        assert_eq!(state.modal().unwrap().input(), "a");
    }

    #[test]
    fn modal_editing_is_a_noop_when_closed() {
        let mut state = state();
        // No modal open: editing keys are harmless.
        state.modal_push_char('a');
        state.modal_backspace();
        assert!(state.modal().is_none());
        assert!(state.submit_modal().is_none());
    }

    #[test]
    fn cancel_modal_closes_it() {
        let mut state = state();
        state.open_session_modal();
        state.modal_push_char('x');
        state.cancel_modal();
        assert!(state.modal().is_none());
    }

    #[test]
    fn submit_modal_rejects_an_empty_name() {
        let mut state = state();
        state.open_session_modal();
        // Whitespace only is empty after trimming.
        state.modal_push_char(' ');
        assert!(state.submit_modal().is_none());
        let modal = state.modal().unwrap();
        assert!(modal.error().unwrap().contains("must not be empty"));
    }

    #[test]
    fn submit_modal_rejects_a_duplicate_name() {
        // The sample state has a "feature" worktree; reusing it is rejected.
        let mut state = state();
        state.open_session_modal();
        for c in "feature".chars() {
            state.modal_push_char(c);
        }
        assert!(state.submit_modal().is_none());
        assert!(state.modal().unwrap().error().unwrap().contains("feature"));
    }

    #[test]
    fn submit_modal_accepts_a_fresh_name_and_closes() {
        let mut state = state();
        state.open_session_modal();
        for c in "  fresh  ".chars() {
            state.modal_push_char(c);
        }
        // The name is trimmed, the modal closes, and the name is returned.
        assert_eq!(state.submit_modal().as_deref(), Some("fresh"));
        assert!(state.modal().is_none());
    }

    fn session_record(name: &str, worktrees: usize) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            root: std::path::PathBuf::from(format!("/repo/.usagi/worktree/{name}")),
            // Each repository's worktree is on the session branch `name`.
            worktrees: (0..worktrees).map(|_| worktree(name)).collect(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn apply_session_outcome_logs_and_rebuilds_the_pane_from_sessions() {
        let mut state = state();
        // A success outcome with refreshed sessions rebuilds the worktree pane
        // from them (one worktree per session here).
        state.apply_session_outcome(SessionOutcome {
            line: LogLine::output("Created session \"x\""),
            sessions: Some(vec![session_record("main", 1), session_record("x", 1)]),
            select: Some("x".to_string()),
        });
        assert!(state.log().last().unwrap().text.contains("Created session"));
        assert_eq!(state.sessions().len(), 2);
        assert_eq!(state.list().worktrees().len(), 2);
        assert_eq!(state.list().workspace_name(), "usagi");
        assert!(state
            .list()
            .worktrees()
            .iter()
            .any(|w| w.branch.as_deref() == Some("x")));
        // The freshly created session is selected and active (row 2: root, main, x).
        assert_eq!(state.list().selected_index(), 2);
        assert_eq!(state.list().active_index(), 2);
        assert_eq!(
            state.list().selected().unwrap().branch.as_deref(),
            Some("x")
        );

        // A failure outcome (no sessions) only logs; the pane is unchanged.
        state.apply_session_outcome(SessionOutcome {
            line: LogLine::error("session failed"),
            sessions: None,
            select: None,
        });
        assert_eq!(state.log().last().unwrap().kind, LineKind::Error);
        assert_eq!(state.list().worktrees().len(), 2);
        assert_eq!(state.sessions().len(), 2);
    }

    #[test]
    fn open_remove_modal_lists_the_session_names() {
        let mut state = state();
        state.restore_sessions(vec![session_record("alpha", 1), session_record("beta", 1)]);
        assert!(state.remove_modal().is_none());
        state.open_remove_modal(false);
        let modal = state.remove_modal().unwrap();
        assert_eq!(modal.names(), ["alpha", "beta"]);
        assert_eq!(modal.cursor(), 0);
        assert_eq!(modal.selected_count(), 0);
        assert!(!modal.is_empty());
        assert!(!modal.is_selected(0));
    }

    #[test]
    fn remove_modal_cursor_wraps_in_both_directions() {
        let mut state = state();
        state.restore_sessions(vec![
            session_record("a", 1),
            session_record("b", 1),
            session_record("c", 1),
        ]);
        state.open_remove_modal(false);
        state.remove_modal_move_down();
        assert_eq!(state.remove_modal().unwrap().cursor(), 1);
        // Up from the top wraps to the bottom.
        state.remove_modal_move_up();
        state.remove_modal_move_up();
        assert_eq!(state.remove_modal().unwrap().cursor(), 2);
        // Down from the bottom wraps to the top.
        state.remove_modal_move_down();
        assert_eq!(state.remove_modal().unwrap().cursor(), 0);
    }

    #[test]
    fn remove_modal_toggle_checks_and_unchecks_the_cursor_row() {
        let mut state = state();
        state.restore_sessions(vec![session_record("a", 1), session_record("b", 1)]);
        state.open_remove_modal(false);
        state.remove_modal_toggle(); // check "a"
        state.remove_modal_move_down();
        state.remove_modal_toggle(); // check "b"
        let modal = state.remove_modal().unwrap();
        assert!(modal.is_selected(0));
        assert!(modal.is_selected(1));
        assert_eq!(modal.selected_count(), 2);
        // Toggling again unchecks it.
        state.remove_modal_toggle();
        assert!(!state.remove_modal().unwrap().is_selected(1));
    }

    #[test]
    fn remove_modal_navigation_is_a_noop_when_empty_or_closed() {
        let mut state = state();
        // No sessions: opening yields an empty modal that ignores movement.
        state.open_remove_modal(false);
        assert!(state.remove_modal().unwrap().is_empty());
        state.remove_modal_move_up();
        state.remove_modal_move_down();
        state.remove_modal_toggle();
        assert_eq!(state.remove_modal().unwrap().cursor(), 0);
        assert_eq!(state.remove_modal().unwrap().selected_count(), 0);

        // Closed: every removal-modal action is harmless.
        state.cancel_remove_modal();
        state.remove_modal_move_up();
        state.remove_modal_move_down();
        state.remove_modal_toggle();
        assert!(state.remove_modal().is_none());
        assert!(state.submit_remove_modal().is_none());
    }

    #[test]
    fn submit_remove_modal_returns_checked_names_in_order_and_closes() {
        let mut state = state();
        state.restore_sessions(vec![
            session_record("a", 1),
            session_record("b", 1),
            session_record("c", 1),
        ]);
        // Open carrying the force flag; check "c" then "a".
        state.open_remove_modal(true);
        state.remove_modal_move_down();
        state.remove_modal_move_down();
        state.remove_modal_toggle(); // "c"
        state.remove_modal_move_up();
        state.remove_modal_move_up();
        state.remove_modal_toggle(); // "a"
        let (names, force) = state.submit_remove_modal().unwrap();
        // Names come back in display order regardless of the toggle order.
        assert_eq!(names, vec!["a".to_string(), "c".to_string()]);
        assert!(force);
        // Confirming closes the modal.
        assert!(state.remove_modal().is_none());
    }

    #[test]
    fn submit_remove_modal_with_nothing_checked_keeps_it_open() {
        let mut state = state();
        state.restore_sessions(vec![session_record("a", 1)]);
        state.open_remove_modal(false);
        // Nothing checked: there is nothing to remove, so the modal stays open.
        assert!(state.submit_remove_modal().is_none());
        assert!(state.remove_modal().is_some());
    }

    #[test]
    fn log_sessions_lists_recorded_sessions() {
        let mut state = state();
        state.restore_sessions(vec![session_record("alpha", 2), session_record("beta", 1)]);
        state.log_sessions();
        let text = state
            .log()
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("2 session(s)"));
        assert!(text.contains("alpha"));
        assert!(text.contains("beta"));
    }

    #[test]
    fn log_sessions_reports_when_empty() {
        let mut state = state();
        state.log_sessions();
        assert!(state.log().last().unwrap().text.contains("No sessions yet"));
    }

    #[test]
    fn log_output_and_error_append_lines() {
        let mut state = state();
        state.log_output("did a thing");
        state.log_error("it broke");
        let last_two: Vec<_> = state.log().iter().rev().take(2).collect();
        assert_eq!(last_two[0].kind, LineKind::Error);
        assert_eq!(last_two[0].text, "it broke");
        assert_eq!(last_two[1].kind, LineKind::Output);
        assert_eq!(last_two[1].text, "did a thing");
    }

    #[test]
    fn log_scroll_moves_clamps_and_sticks_to_the_bottom() {
        let mut state = state();
        for i in 0..10 {
            state.log_output(format!("line {i}"));
        }
        let len = state.log().len();
        // It starts pinned to the newest line.
        assert_eq!(state.right_scroll(), 0);

        // Scrolling up moves the window earlier, up to the oldest line a
        // 4-row pane could show (len - 4).
        state.scroll_log_up(3, 4);
        assert_eq!(state.right_scroll(), 3);
        state.scroll_log_up(100, 4);
        assert_eq!(state.right_scroll(), len - 4);

        // Scrolling down walks back toward the bottom and stops there.
        state.scroll_log_down(2);
        assert_eq!(state.right_scroll(), len - 4 - 2);
        state.scroll_log_down(1000);
        assert_eq!(state.right_scroll(), 0);

        // Fresh output snaps the pane back to the bottom.
        state.scroll_log_up(2, 4);
        assert_ne!(state.right_scroll(), 0);
        state.log_output("new output");
        assert_eq!(state.right_scroll(), 0);
    }

    #[test]
    fn right_pane_starts_on_the_log_and_toggles_with_the_terminal() {
        let mut state = state();
        // The log is shown by default, with no terminal snapshot.
        assert_eq!(state.right_pane(), RightPane::Log);
        assert!(state.terminal_view().is_none());

        // Starting the terminal switches the pane; a snapshot then arrives.
        state.show_terminal();
        assert_eq!(state.right_pane(), RightPane::Terminal);
        state.set_terminal_view(TerminalView::from_rows(
            vec!["$ ".to_string()],
            Some((0, 2)),
        ));
        assert_eq!(state.terminal_view().unwrap().rows(), ["$ "]);

        // Closing the terminal returns to the log and drops the snapshot.
        state.show_log();
        assert_eq!(state.right_pane(), RightPane::Log);
        assert!(state.terminal_view().is_none());
    }

    #[test]
    fn focus_session_jumps_to_a_row_and_clamps_to_the_list() {
        // root (row 0), main (row 1), feature (row 2).
        let mut state = state();
        state.focus_session(2);
        assert_eq!(state.list().selected_index(), 2);
        assert_eq!(
            state.list().selected().unwrap().branch.as_deref(),
            Some("feature")
        );
        // The root row is reachable too.
        state.focus_session(0);
        assert_eq!(state.list().selected_index(), 0);
        assert!(state.list().root_selected());
        // A row past the end clamps to the last worktree.
        state.focus_session(99);
        assert_eq!(state.list().selected_index(), 2);
    }

    #[test]
    fn session_picker_opens_listing_the_root_then_each_worktree() {
        let mut state = state();
        // Open it from the second worktree, so the cursor and "here" land there.
        state.focus_session(2);
        assert!(state.session_picker().is_none());
        state.open_session_picker();
        let picker = state.session_picker().unwrap();
        assert_eq!(picker.names(), [ROOT_NAME, "main", "feature"]);
        assert_eq!(picker.cursor(), 2);
        assert_eq!(picker.current(), 2);
    }

    #[test]
    fn session_picker_cursor_moves_and_wraps() {
        let mut state = state(); // root + 2 worktrees → 3 rows
        state.open_session_picker(); // cursor on the root row (0)
                                     // Up from the top wraps to the bottom.
        state.session_picker_move_up();
        assert_eq!(state.session_picker().unwrap().cursor(), 2);
        // Down from the bottom wraps to the top.
        state.session_picker_move_down();
        assert_eq!(state.session_picker().unwrap().cursor(), 0);
        state.session_picker_move_down();
        assert_eq!(state.session_picker().unwrap().cursor(), 1);
    }

    #[test]
    fn session_picker_select_number_jumps_in_range_only() {
        let mut state = state();
        state.open_session_picker();
        // Session 3 is the second worktree (row 2).
        assert!(state.session_picker_select_number(3));
        assert_eq!(state.session_picker().unwrap().cursor(), 2);
        // 0 and out-of-range numbers are ignored, leaving the cursor put.
        assert!(!state.session_picker_select_number(0));
        assert!(!state.session_picker_select_number(4));
        assert_eq!(state.session_picker().unwrap().cursor(), 2);
    }

    #[test]
    fn session_picker_confirm_focuses_the_cursor_and_closes() {
        let mut state = state();
        state.open_session_picker();
        state.session_picker_move_down(); // cursor → row 1 (main)
        assert_eq!(state.confirm_session_picker(), Some(1));
        assert!(state.session_picker().is_none());
        assert_eq!(state.list().selected_index(), 1);
    }

    #[test]
    fn session_picker_cancel_closes_without_switching() {
        let mut state = state();
        state.open_session_picker();
        state.session_picker_move_down();
        state.cancel_session_picker();
        assert!(state.session_picker().is_none());
        // The list cursor never moved off the root row.
        assert_eq!(state.list().selected_index(), 0);
    }

    #[test]
    fn session_picker_methods_are_noops_while_closed() {
        let mut state = state();
        // Nothing is open, so every mutator is inert and confirm returns None.
        state.session_picker_move_up();
        state.session_picker_move_down();
        assert!(!state.session_picker_select_number(1));
        assert_eq!(state.confirm_session_picker(), None);
        assert!(state.session_picker().is_none());
    }

    #[test]
    fn waiting_paths_track_sessions_awaiting_input() {
        let mut state = state();
        // Nothing is waiting by default.
        assert!(!state.is_waiting(Path::new("/repo/feature")));
        assert!(state.waiting_paths().is_empty());

        // The monitor's snapshot is swapped in wholesale before each redraw.
        let mut waiting = HashSet::new();
        waiting.insert(PathBuf::from("/repo/feature"));
        state.set_waiting(waiting);
        assert!(state.is_waiting(Path::new("/repo/feature")));
        assert!(!state.is_waiting(Path::new("/repo/main")));

        // A later (empty) snapshot clears it.
        state.set_waiting(HashSet::new());
        assert!(!state.is_waiting(Path::new("/repo/feature")));
    }

    #[test]
    fn live_paths_track_sessions_with_a_running_agent() {
        let mut state = state();
        // Nothing is live by default.
        assert!(!state.is_live(Path::new("/repo/feature")));
        assert!(state.live_paths().is_empty());

        // The monitor's snapshot of live sessions is swapped in wholesale.
        let mut live = HashSet::new();
        live.insert(PathBuf::from("/repo/feature"));
        state.set_live(live);
        assert!(state.is_live(Path::new("/repo/feature")));
        assert!(!state.is_live(Path::new("/repo/main")));
        assert_eq!(state.live_paths().len(), 1);

        // A later (empty) snapshot clears it.
        state.set_live(HashSet::new());
        assert!(!state.is_live(Path::new("/repo/feature")));
    }

    #[test]
    fn typing_or_completing_cancels_an_active_recall() {
        let mut state = state();
        for c in "man".chars() {
            state.push_char(c);
        }
        state.submit();
        state.recall_prev();
        assert_eq!(state.input(), "man");
        // Editing resets the recall cursor, so recall_next no longer advances.
        state.push_char('!');
        state.recall_next();
        assert_eq!(state.input(), "man!");
    }
}
