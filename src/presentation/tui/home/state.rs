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

use crate::domain::settings::SessionActionUi;
use crate::domain::workspace_state::{BranchStatus, SessionRecord, WorktreeState};

use super::command::{
    CommandInfo, CommandRegistry, CommandScope, Completion, Effect, Hint, WorktreeRef,
};
use super::terminal_view::TerminalView;

/// The display name of a worktree: its branch, or a placeholder when detached.
pub fn worktree_name(worktree: &WorktreeState) -> &str {
    worktree.branch.as_deref().unwrap_or("(detached)")
}

/// The name of the root row: the workspace itself, belonging to no session.
/// Used as its display label and to target it from `session switch root`.
pub const ROOT_NAME: &str = "root";

/// Collapse a session's per-repository worktrees into the single list row that
/// represents it.
///
/// A session is one branch name checked out into a worktree in every repository
/// under the workspace, so the home list shows one row per *session*, not per
/// repository. The row is keyed on the session tree root (`<workspace>/.usagi/
/// sessions/<name>`) — where the embedded terminal/agent roots and the key for
/// its live/waiting state — and carries:
///
/// - the session name as its branch label,
/// - a status [aggregated](BranchStatus::aggregate) across the repositories (the
///   least-progressed, so `merged` means every repository's branch has landed),
/// - `primary` set when any repository's worktree is the primary checkout,
/// - the first repository's `head` / `upstream` as representative detail.
///
/// For a single-repository workspace the session root *is* that repository's
/// worktree, so the row matches the lone worktree exactly.
fn session_row(session: &SessionRecord) -> WorktreeState {
    let status = BranchStatus::aggregate(session.worktrees.iter().map(|w| w.status));
    let primary = session.worktrees.iter().any(|w| w.primary);
    let first = session.worktrees.first();
    WorktreeState {
        branch: Some(session.name.clone()),
        path: session.root.clone(),
        head: first.map(|w| w.head.clone()).unwrap_or_default(),
        primary,
        upstream: first.and_then(|w| w.upstream.clone()),
        status,
        updated_at: session.created_at,
    }
}

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

/// The home screen's mode — the "engagement ladder" the design is built around
/// (統括 / 切替 / 在席 / 没入). Each step moves from overseeing the whole
/// workspace toward operating deeper inside one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 統括 (Overview): the workspace-wide command line, the default. The user
    /// types `session` / `config` / `doctor`; results render *below the input*
    /// and the right pane stays blank.
    Overview,
    /// 切替 (Switch): the session picker. The left pane has the keyboard for
    /// choosing a session (Enter), creating one inline (`c`), or backing out
    /// (Esc). Entered from Overview via `session switch`, and from Focus /
    /// Attached via `Ctrl-O`.
    Switch,
    /// 在席 (Focus): a session is selected and operated in the *right pane* —
    /// either a menu of its runnable commands or a session-scoped prompt
    /// (chosen by [`crate::domain::settings::SessionActionUi`]).
    Focus,
    /// 没入 (Attached): an embedded terminal / agent is live in the right pane
    /// and keys flow to it. `Ctrl-O` zooms out to Switch; `Ctrl-O` again to
    /// Overview.
    Attached,
}

/// Where a [`Mode::Switch`] should return to when cancelled (`Esc` / `h`) — the
/// mode it was opened from. `Ctrl-O` while in Switch always zooms out to
/// Overview regardless of this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnMode {
    /// Opened from 統括 via `session switch`.
    Overview,
    /// Opened from 在席 via `Ctrl-O`.
    Focus,
    /// Opened from 没入 via `Ctrl-O`; cancelling re-attaches the session.
    Attached,
}

/// Why the embedded terminal pane handed control back to the event loop.
///
/// The pane is driven by the impure terminal loop (`terminal_pane`); this enum
/// is the small, testable vocabulary it returns so the event loop can decide
/// what to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneExit {
    /// The shell exited on its own (e.g. the user typed `exit`); it is gone, so
    /// the pane returns to 在席 (Focus).
    Closed,
    /// The user pressed `Ctrl-O`: leave the pane to the 切替 (Switch) mode on the
    /// left pane. Re-selecting the same session re-attaches; `Ctrl-O` again zooms
    /// out to 統括 (Overview).
    ToSwitch,
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

/// The inline session-name input shown in the left pane while creating a session
/// from 切替 (Switch): the name being typed plus an optional inline validation
/// error (e.g. an empty or duplicate name). Read through [`HomeState`]'s
/// `create_input` / `create_error` accessors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CreateInput {
    input: String,
    error: Option<String>,
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
    /// Which right-pane action surface 在席 (Focus) presents — a pickable menu
    /// or a typed prompt. Injected from the effective settings by `mod.rs`.
    session_action_ui: SessionActionUi,
    /// Where a 切替 (Switch) returns to on `Esc` / `h`; only meaningful in
    /// [`Mode::Switch`].
    switch_return: ReturnMode,
    /// The inline session-name input, when creating a session from 切替. While
    /// set it captures the Switch mode's keys.
    create: Option<CreateInput>,
    /// The 在席 (Focus) menu cursor: which Session-scope command is highlighted.
    focus_menu_cursor: usize,
    /// The 在席 (Focus) prompt buffer (the session-scoped command line).
    focus_prompt: String,
    /// The session-removal modal, when open (the user ran `session remove`
    /// without a name). While set it captures all keys.
    remove_modal: Option<RemoveModal>,
    /// Sessions recorded for this workspace (from `state.json`), shown by
    /// `session list` and kept current as sessions are created.
    sessions: Vec<SessionRecord>,
    /// The latest snapshot of the embedded terminal's screen, set while a session
    /// is 没入 (Attached) and rendered in the right pane.
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
            mode: Mode::Overview,
            input: String::new(),
            history: Vec::new(),
            recall: None,
            log,
            registry: CommandRegistry::with_builtins(),
            session_action_ui: SessionActionUi::default(),
            switch_return: ReturnMode::Overview,
            create: None,
            focus_menu_cursor: 0,
            focus_prompt: String::new(),
            remove_modal: None,
            sessions: Vec::new(),
            terminal_view: None,
            waiting: HashSet::new(),
            live: HashSet::new(),
        }
    }

    /// Set which right-pane action surface 在席 (Focus) presents (injected from
    /// the effective settings by `mod.rs` at construction).
    pub fn set_session_action_ui(&mut self, ui: SessionActionUi) {
        self.session_action_ui = ui;
    }

    /// Which right-pane action surface 在席 (Focus) presents.
    pub fn session_action_ui(&self) -> SessionActionUi {
        self.session_action_ui
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

    /// Rebuild the worktree pane from the current sessions: one row per session
    /// (not per repository), in order. A session spanning several git
    /// repositories is collapsed into a single row by [`session_row`].
    fn rebuild_list(&mut self) {
        let name = self.list.workspace_name().to_string();
        let rows = self.sessions.iter().map(session_row).collect();
        self.list = WorktreeList::new(name, rows);
    }

    pub fn sessions(&self) -> &[SessionRecord] {
        &self.sessions
    }

    /// Append the recorded sessions to the log (the `session list` command).
    pub fn log_sessions(&mut self) {
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
        self.log.push(LogLine::output(text));
    }

    /// Append an error line to the log.
    pub fn log_error(&mut self, text: impl Into<String>) {
        self.log.push(LogLine::error(text));
    }

    /// Append a hint that the session under the cursor has no live shell/agent,
    /// so navigating to (or selecting) it only moved the cursor — pointing at
    /// the commands that actually start one. Selecting an idle session never
    /// spawns a shell on its own; starting one is always explicit and launches
    /// the agent (with its MCP wiring) by default.
    pub fn hint_no_live_session(&mut self) {
        self.log.push(LogLine::notice(
            "No live session here — run \":agent\" to start one (\":terminal\" for a plain shell).",
        ));
    }

    pub fn list(&self) -> &WorktreeList {
        &self.list
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Which command scope the 統括 (Overview) command line operates in: always
    /// the whole workspace, since Overview is workspace-only (the session-scoped
    /// surface lives in the 在席 right pane instead). Completion, hints, and `man`
    /// grouping follow this. The 在席 prompt calls the registry with
    /// [`CommandScope::Session`] directly via [`Self::focus_prompt_hint`] etc.
    pub fn command_scope(&self) -> CommandScope {
        CommandScope::Workspace
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    /// The advisory input hint for the current command input (matching commands,
    /// or the usage of the command being given arguments). Computed on demand
    /// for rendering; see [`CommandRegistry::suggest`].
    pub fn hint(&self) -> Hint {
        self.registry.suggest(&self.input, self.command_scope())
    }

    pub fn log(&self) -> &[LogLine] {
        &self.log
    }

    /// The current embedded-terminal snapshot, when a session is 没入 (Attached).
    pub fn terminal_view(&self) -> Option<&TerminalView> {
        self.terminal_view.as_ref()
    }

    /// Enter 没入 (Attached): an embedded terminal / agent is going live in the
    /// right pane. The first snapshot arrives via [`set_terminal_view`].
    ///
    /// [`set_terminal_view`]: Self::set_terminal_view
    pub fn show_attached(&mut self) {
        self.mode = Mode::Attached;
    }

    /// Leave 没入 for 在席 (Focus): the embedded session was closed or detached,
    /// so drop the snapshot and return to the focused session's action surface.
    pub fn leave_attached(&mut self) {
        self.mode = Mode::Focus;
        self.terminal_view = None;
    }

    /// Store the latest embedded-terminal screen snapshot, shown in the right
    /// pane while the session is 没入 (Attached).
    pub fn set_terminal_view(&mut self, view: TerminalView) {
        self.terminal_view = Some(view);
    }

    /// Drop the embedded-terminal snapshot without changing the mode. Used
    /// between frames so a stale snapshot never lingers in the right pane.
    pub fn clear_terminal_view(&mut self) {
        self.terminal_view = None;
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

    /// Focus the session at `row` (0 is the root row, `i` maps to worktree
    /// `i - 1`) in the list, so the embedded terminal re-roots there.
    pub fn focus_session(&mut self, row: usize) {
        self.list.focus_index(row);
    }

    // --- 統括 (Overview) ---------------------------------------------------

    /// Return to 統括 (Overview), clearing the transient 切替 / 在席 state.
    pub fn enter_overview(&mut self) {
        self.mode = Mode::Overview;
        self.create = None;
        self.focus_prompt.clear();
        self.focus_menu_cursor = 0;
        self.input.clear();
        self.recall = None;
    }

    // --- 切替 (Switch) -----------------------------------------------------

    /// Enter 切替 (Switch): move keyboard focus to the left pane to pick a
    /// session, remembering where to return on `Esc` / `h`.
    pub fn enter_switch(&mut self, return_to: ReturnMode) {
        self.mode = Mode::Switch;
        self.switch_return = return_to;
        self.create = None;
    }

    /// Where the current 切替 returns to on `Esc` / `h`.
    pub fn switch_return(&self) -> ReturnMode {
        self.switch_return
    }

    /// Move the Switch cursor up one row, wrapping (delegates to the list).
    pub fn switch_move_up(&mut self) {
        self.list.move_up();
    }

    /// Move the Switch cursor down one row, wrapping (delegates to the list).
    pub fn switch_move_down(&mut self) {
        self.list.move_down();
    }

    /// Begin inline session creation in 切替: open an empty name input that
    /// captures the mode's keys until confirmed (Enter) or cancelled (Esc).
    pub fn switch_begin_create(&mut self) {
        self.create = Some(CreateInput::default());
    }

    /// Whether an inline create input is open in 切替.
    pub fn is_creating(&self) -> bool {
        self.create.is_some()
    }

    /// The inline create input's name typed so far, if open.
    pub fn create_input(&self) -> Option<&str> {
        self.create.as_ref().map(|c| c.input.as_str())
    }

    /// The inline create input's current validation error, if any.
    pub fn create_error(&self) -> Option<&str> {
        self.create.as_ref().and_then(|c| c.error.as_deref())
    }

    /// Append a character to the inline create name (no-op when not creating).
    pub fn create_push_char(&mut self, c: char) {
        if let Some(create) = self.create.as_mut() {
            create.input.push(c);
            create.error = None;
        }
    }

    /// Delete the last character of the inline create name (no-op when not
    /// creating).
    pub fn create_backspace(&mut self) {
        if let Some(create) = self.create.as_mut() {
            create.input.pop();
            create.error = None;
        }
    }

    /// Cancel inline creation, staying in 切替.
    pub fn create_cancel(&mut self) {
        self.create = None;
    }

    /// Validate and accept the inline create name. On success the input closes
    /// and the trimmed name is returned (for the event loop to create the
    /// session); on an empty or duplicate name the input stays open with an
    /// inline error and `None` is returned. A no-op (returning `None`) when not
    /// creating.
    pub fn switch_confirm_create(&mut self) -> Option<String> {
        let create = self.create.as_mut()?;
        let name = create.input.trim().to_string();
        if name.is_empty() {
            create.error = Some("Name must not be empty.".to_string());
            return None;
        }
        if self
            .list
            .worktrees()
            .iter()
            .any(|w| w.branch.as_deref() == Some(name.as_str()))
        {
            create.error = Some(format!("\"{name}\" already exists."));
            return None;
        }
        self.create = None;
        Some(name)
    }

    // --- 在席 (Focus) ------------------------------------------------------

    /// Enter 在席 (Focus) on the session at `row` (0 is the root row): make it the
    /// active and selected row, switch to the right-pane action surface, and reset
    /// the menu cursor and prompt buffer.
    pub fn enter_focus(&mut self, row: usize) {
        self.list.focus_index(row);
        self.list.activate_selected();
        self.mode = Mode::Focus;
        self.create = None;
        self.focus_menu_cursor = 0;
        self.focus_prompt.clear();
    }

    /// The display name of the focused (active) session: its branch, or
    /// [`ROOT_NAME`] for the root row.
    pub fn focused_session_name(&self) -> String {
        self.list
            .selected()
            .map(worktree_name)
            .unwrap_or(ROOT_NAME)
            .to_string()
    }

    /// Leave 在席 for 統括 (Overview).
    pub fn leave_focus(&mut self) {
        self.enter_overview();
    }

    /// The Session-scope commands the 在席 menu lists, in registry order
    /// (`terminal`, `agent`, `ai`).
    pub fn focus_menu_commands(&self) -> Vec<CommandInfo> {
        self.registry.commands_in_scope(CommandScope::Session)
    }

    /// The 在席 menu cursor (which Session-scope command is highlighted).
    pub fn focus_menu_cursor(&self) -> usize {
        self.focus_menu_cursor
    }

    /// Move the 在席 menu cursor up one row, wrapping. The Session-scope command
    /// list is always non-empty, so `count` is clamped to at least 1 to stay
    /// underflow-safe.
    pub fn focus_menu_move_up(&mut self) {
        let count = self.focus_menu_commands().len().max(1);
        self.focus_menu_cursor = self.focus_menu_cursor.checked_sub(1).unwrap_or(count - 1);
    }

    /// Move the 在席 menu cursor down one row, wrapping. See [`focus_menu_move_up`]
    /// for the non-empty invariant.
    ///
    /// [`focus_menu_move_up`]: Self::focus_menu_move_up
    pub fn focus_menu_move_down(&mut self) {
        let count = self.focus_menu_commands().len().max(1);
        self.focus_menu_cursor = (self.focus_menu_cursor + 1) % count;
    }

    /// The 在席 command under the menu cursor, clamped to the available commands.
    pub fn focus_selected_command(&self) -> CommandInfo {
        let commands = self.focus_menu_commands();
        let index = self.focus_menu_cursor.min(commands.len().saturating_sub(1));
        commands[index]
    }

    /// The 在席 prompt buffer (the session-scoped command line).
    pub fn focus_prompt(&self) -> &str {
        &self.focus_prompt
    }

    /// Append a character to the 在席 prompt.
    pub fn focus_prompt_push_char(&mut self, c: char) {
        self.focus_prompt.push(c);
    }

    /// Delete the last character of the 在席 prompt.
    pub fn focus_prompt_backspace(&mut self) {
        self.focus_prompt.pop();
    }

    /// Tab-complete the 在席 prompt's command word against the Session-scope
    /// commands, returning the candidates when ambiguous (so the caller can log
    /// them, mirroring the Overview line's `complete`).
    pub fn focus_prompt_complete(&mut self) -> Completion {
        let completion = self
            .registry
            .complete(&self.focus_prompt, CommandScope::Session);
        self.focus_prompt = completion.input.clone();
        completion
    }

    /// The advisory hint for the 在席 prompt, computed in the Session scope.
    pub fn focus_prompt_hint(&self) -> Hint {
        self.registry
            .suggest(&self.focus_prompt, CommandScope::Session)
    }

    /// Run the 在席 prompt as a Session-scope command: dispatch it, append its
    /// produced lines to the log, clear the prompt, and return the resulting
    /// [`Submission`] (so the event loop can act on `OpenTerminal` / `OpenAgent`).
    /// Empty input is a no-op.
    pub fn focus_prompt_submit(&mut self) -> Submission {
        let entry = self.focus_prompt.trim().to_string();
        self.focus_prompt.clear();
        if entry.is_empty() {
            return Submission {
                effect: Effect::None,
                recorded: None,
            };
        }
        let result = self
            .registry
            .dispatch(&entry, &self.history, &self.list.refs());
        self.history.push(entry.clone());
        self.log.extend(result.lines);
        Submission {
            effect: result.effect,
            recorded: Some(entry),
        }
    }

    /// Append a typed character to the input (Overview line).
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
        let completion = self.registry.complete(&self.input, self.command_scope());
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

        self.log.push(LogLine::command(entry.clone()));
        let result = self
            .registry
            .dispatch(&entry, &self.history, &self.list.refs());
        self.history.push(entry.clone());

        match result.effect {
            Effect::Clear => self.log.clear(),
            // `session switch` (→ 切替) and `session switch <name>` (→ 在席) are
            // resolved by the event loop, which owns the mode transitions (and, for
            // a live session, the pane). They append no lines here.
            Effect::EnterSwitch | Effect::Activate(_) => {}
            _ => self.log.extend(result.lines),
        }
        Submission {
            effect: result.effect,
            recorded: Some(entry),
        }
    }

    /// Apply the result of a session-creation attempt: log its line and, when
    /// creation refreshed the worktree list, swap it in.
    pub fn apply_session_outcome(&mut self, outcome: SessionOutcome) {
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
    fn new_state_starts_in_overview_with_a_hint() {
        let state = state();
        assert_eq!(state.mode(), Mode::Overview);
        assert_eq!(state.input(), "");
        assert_eq!(state.list().worktrees().len(), 2);
        // The seed log carries the usage hint.
        assert_eq!(state.log().len(), 1);
        assert!(state.log()[0].text.contains("man"));
        // The default action surface is the menu.
        assert_eq!(state.session_action_ui(), SessionActionUi::Menu);
        // The Overview line is always workspace-scoped.
        assert_eq!(state.command_scope(), CommandScope::Workspace);
    }

    #[test]
    fn a_notice_is_seeded_as_an_error_line() {
        let state = HomeState::new("usagi", Vec::new(), Some("load failed".to_string()));
        assert_eq!(state.log().len(), 2);
        assert_eq!(state.log()[1].kind, LineKind::Error);
        assert_eq!(state.log()[1].text, "load failed");
    }

    #[test]
    fn set_session_action_ui_overrides_the_default() {
        let mut state = state();
        state.set_session_action_ui(SessionActionUi::Prompt);
        assert_eq!(state.session_action_ui(), SessionActionUi::Prompt);
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
        // Empty input matches every workspace command, so Tab lists them.
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
        let echoed = state.log().iter().find(|l| l.kind == LineKind::Command);
        assert_eq!(echoed.unwrap().text, "man");
        assert!(state.log().iter().any(|l| l.text.contains("Available")));
        assert_eq!(state.input(), "");
    }

    #[test]
    fn session_switch_with_no_name_yields_the_enter_switch_effect() {
        // The screen leaves the mode transition to the event loop; submit only
        // surfaces the effect and logs no resolution line.
        let mut state = state();
        for c in "session switch".chars() {
            state.push_char(c);
        }
        let before = state.log().len();
        let submission = state.submit();
        assert_eq!(submission.effect, Effect::EnterSwitch);
        // Only the echoed command line was appended.
        assert_eq!(state.log().len(), before + 1);
    }

    #[test]
    fn session_switch_with_a_name_yields_the_activate_effect() {
        let mut state = state();
        for c in "session switch feature".chars() {
            state.push_char(c);
        }
        let submission = state.submit();
        assert_eq!(submission.effect, Effect::Activate("feature".to_string()));
        // The list is not resolved here (the event loop does it).
        assert_eq!(state.list().active_index(), 0);
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
        state.recall_prev();
        assert_eq!(state.input(), "space");
        state.recall_prev();
        assert_eq!(state.input(), "session");
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
        state.recall_prev();
        assert_eq!(state.input(), "doctor");
        state.recall_prev();
        assert_eq!(state.input(), "man");
        state.recall_prev();
        assert_eq!(state.input(), "man");
        state.recall_next();
        assert_eq!(state.input(), "doctor");
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
        state.recall_next();
        assert_eq!(state.input(), "");
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
        state.push_char('!');
        state.recall_next();
        assert_eq!(state.input(), "man!");
    }

    // --- 切替 (Switch) -----------------------------------------------------

    #[test]
    fn enter_switch_remembers_its_return_mode_and_moves_the_cursor() {
        let mut state = state(); // root, main, feature
        state.enter_switch(ReturnMode::Overview);
        assert_eq!(state.mode(), Mode::Switch);
        assert_eq!(state.switch_return(), ReturnMode::Overview);
        state.switch_move_down();
        assert_eq!(state.list().selected_index(), 1);
        state.switch_move_up();
        assert_eq!(state.list().selected_index(), 0);
        // Up from the root wraps to the bottom (the last worktree row, 2).
        state.switch_move_up();
        assert_eq!(state.list().selected_index(), 2);
    }

    #[test]
    fn switch_return_carries_each_origin() {
        let mut state = state();
        state.enter_switch(ReturnMode::Focus);
        assert_eq!(state.switch_return(), ReturnMode::Focus);
        state.enter_switch(ReturnMode::Attached);
        assert_eq!(state.switch_return(), ReturnMode::Attached);
    }

    #[test]
    fn switch_inline_create_edits_then_confirms_a_fresh_name() {
        let mut state = state();
        state.enter_switch(ReturnMode::Overview);
        assert!(!state.is_creating());
        state.switch_begin_create();
        assert!(state.is_creating());
        assert_eq!(state.create_input(), Some(""));
        for c in "  wip  ".chars() {
            state.create_push_char(c);
        }
        state.create_backspace(); // drop a trailing space
                                  // A fresh, trimmed name is accepted and the input closes.
        assert_eq!(state.switch_confirm_create().as_deref(), Some("wip"));
        assert!(!state.is_creating());
    }

    #[test]
    fn switch_inline_create_rejects_empty_and_duplicate_names() {
        let mut state = state(); // has a "feature" worktree
        state.enter_switch(ReturnMode::Overview);
        state.switch_begin_create();
        // Whitespace only is empty after trimming.
        state.create_push_char(' ');
        assert!(state.switch_confirm_create().is_none());
        assert!(state.create_error().unwrap().contains("must not be empty"));
        // Typing clears the error, then a duplicate name is rejected.
        for c in "feature".chars() {
            state.create_push_char(c);
        }
        assert!(state.create_error().is_none());
        assert!(state.switch_confirm_create().is_none());
        assert!(state.create_error().unwrap().contains("feature"));
        assert!(state.is_creating());
    }

    #[test]
    fn switch_inline_create_can_be_cancelled() {
        let mut state = state();
        state.enter_switch(ReturnMode::Overview);
        state.switch_begin_create();
        state.create_push_char('x');
        state.create_cancel();
        assert!(!state.is_creating());
    }

    #[test]
    fn create_editing_is_a_noop_when_not_creating() {
        let mut state = state();
        // Nothing open: editing keys are harmless and confirm returns None.
        state.create_push_char('a');
        state.create_backspace();
        assert!(!state.is_creating());
        assert!(state.create_input().is_none());
        assert!(state.create_error().is_none());
        assert!(state.switch_confirm_create().is_none());
    }

    // --- 在席 (Focus) ------------------------------------------------------

    #[test]
    fn enter_focus_activates_a_row_and_resets_the_surface() {
        let mut state = state(); // root, main, feature
        state.enter_focus(2); // feature
        assert_eq!(state.mode(), Mode::Focus);
        assert_eq!(state.list().active_index(), 2);
        assert_eq!(state.list().selected_index(), 2);
        assert_eq!(state.focused_session_name(), "feature");
        assert_eq!(state.focus_menu_cursor(), 0);
        assert_eq!(state.focus_prompt(), "");
    }

    #[test]
    fn enter_focus_on_the_root_row_names_root() {
        let mut state = state();
        state.enter_focus(0);
        assert!(state.list().root_active());
        assert_eq!(state.focused_session_name(), ROOT_NAME);
    }

    #[test]
    fn leave_focus_returns_to_overview() {
        let mut state = state();
        state.enter_focus(1);
        state.leave_focus();
        assert_eq!(state.mode(), Mode::Overview);
    }

    #[test]
    fn focus_menu_lists_the_session_commands_in_order() {
        let state = state();
        let names: Vec<&str> = state.focus_menu_commands().iter().map(|i| i.name).collect();
        assert_eq!(names, vec!["terminal", "agent", "ai"]);
    }

    #[test]
    fn focus_menu_cursor_moves_and_wraps_and_selects() {
        let mut state = state();
        state.enter_focus(1);
        // terminal (0, highlighted by default), agent (1), ai (2).
        assert_eq!(state.focus_selected_command().name, "terminal");
        state.focus_menu_move_down();
        assert_eq!(state.focus_selected_command().name, "agent");
        state.focus_menu_move_down();
        state.focus_menu_move_down(); // wraps to the top
        assert_eq!(state.focus_menu_cursor(), 0);
        // Up from the top wraps to the bottom.
        state.focus_menu_move_up();
        assert_eq!(state.focus_selected_command().name, "ai");
    }

    #[test]
    fn focus_prompt_edits_completes_and_hints_in_session_scope() {
        let mut state = state();
        state.enter_focus(1);
        for c in "ter".chars() {
            state.focus_prompt_push_char(c);
        }
        state.focus_prompt_backspace(); // "te"
                                        // "te" uniquely completes to "terminal" (a session command).
        let completion = state.focus_prompt_complete();
        assert_eq!(state.focus_prompt(), "terminal");
        assert!(completion.candidates.is_empty());
        // The hint is computed in the session scope: arguments show usage.
        state.focus_prompt_push_char(' ');
        assert!(matches!(state.focus_prompt_hint(), Hint::Usage { .. }));
    }

    #[test]
    fn focus_prompt_submit_runs_a_session_command() {
        let mut state = state();
        state.enter_focus(1);
        for c in "terminal".chars() {
            state.focus_prompt_push_char(c);
        }
        let submission = state.focus_prompt_submit();
        assert_eq!(submission.effect, Effect::OpenTerminal);
        assert_eq!(submission.recorded.as_deref(), Some("terminal"));
        // The prompt is cleared and the command recorded in history.
        assert_eq!(state.focus_prompt(), "");
        assert_eq!(state.history, vec!["terminal"]);
    }

    #[test]
    fn focus_prompt_submit_on_empty_input_is_a_noop() {
        let mut state = state();
        state.enter_focus(1);
        let submission = state.focus_prompt_submit();
        assert_eq!(submission.effect, Effect::None);
        assert!(submission.recorded.is_none());
        assert!(state.history.is_empty());
    }

    #[test]
    fn focus_prompt_runs_the_coming_soon_ai_command() {
        let mut state = state();
        state.enter_focus(1);
        for c in "ai hi".chars() {
            state.focus_prompt_push_char(c);
        }
        let submission = state.focus_prompt_submit();
        assert_eq!(submission.effect, Effect::None);
        assert!(state.log().last().unwrap().text.contains("coming soon"));
    }

    // --- 没入 (Attached) ---------------------------------------------------

    #[test]
    fn attached_holds_a_terminal_view_and_leaving_drops_it() {
        let mut state = state();
        state.enter_focus(1);
        state.show_attached();
        assert_eq!(state.mode(), Mode::Attached);
        state.set_terminal_view(TerminalView::from_rows(
            vec!["$ ".to_string()],
            Some((0, 2)),
        ));
        assert_eq!(state.terminal_view().unwrap().rows(), ["$ "]);
        // Leaving 没入 returns to 在席 and drops the snapshot.
        state.leave_attached();
        assert_eq!(state.mode(), Mode::Focus);
        assert!(state.terminal_view().is_none());
    }

    #[test]
    fn clear_terminal_view_drops_the_snapshot_without_changing_the_mode() {
        let mut state = state();
        state.enter_focus(1);
        state.show_attached();
        state.set_terminal_view(TerminalView::from_rows(vec!["x".to_string()], None));
        state.clear_terminal_view();
        assert!(state.terminal_view().is_none());
        // The mode is untouched (the per-frame cleanup must not leave 没入).
        assert_eq!(state.mode(), Mode::Attached);
    }

    #[test]
    fn enter_overview_clears_transient_state() {
        let mut state = state();
        state.enter_switch(ReturnMode::Overview);
        state.switch_begin_create();
        state.enter_focus(1);
        state.focus_prompt_push_char('x');
        state.focus_menu_move_down();
        state.enter_overview();
        assert_eq!(state.mode(), Mode::Overview);
        assert!(!state.is_creating());
        assert_eq!(state.focus_prompt(), "");
        assert_eq!(state.focus_menu_cursor(), 0);
        assert_eq!(state.input(), "");
    }

    #[test]
    fn focus_session_jumps_to_a_row_and_clamps_to_the_list() {
        let mut state = state(); // root (0), main (1), feature (2)
        state.focus_session(2);
        assert_eq!(state.list().selected_index(), 2);
        state.focus_session(0);
        assert!(state.list().root_selected());
        state.focus_session(99);
        assert_eq!(state.list().selected_index(), 2);
    }

    fn session_record(name: &str, worktrees: usize) -> SessionRecord {
        SessionRecord {
            name: name.to_string(),
            root: std::path::PathBuf::from(format!("/repo/.usagi/sessions/{name}")),
            worktrees: (0..worktrees).map(|_| worktree(name)).collect(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn apply_session_outcome_logs_and_rebuilds_the_pane_from_sessions() {
        let mut state = state();
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
        assert_eq!(state.list().selected_index(), 2);
        assert_eq!(state.list().active_index(), 2);

        // A failure outcome only logs; the pane is unchanged.
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
    fn multi_repo_session_collapses_to_one_row_with_an_aggregated_status() {
        // A session spanning three repositories: two merged, one still local.
        let mut merged_a = worktree("feature");
        merged_a.path = PathBuf::from("/repo/.usagi/sessions/feature/app-a");
        merged_a.primary = true;
        merged_a.status = BranchStatus::Merged;
        merged_a.upstream = Some("origin/feature".to_string());
        let mut merged_b = worktree("feature");
        merged_b.path = PathBuf::from("/repo/.usagi/sessions/feature/app-b");
        merged_b.status = BranchStatus::Merged;
        let mut local_c = worktree("feature");
        local_c.path = PathBuf::from("/repo/.usagi/sessions/feature/app-c");
        local_c.status = BranchStatus::Local;

        let mut state = state();
        state.restore_sessions(vec![SessionRecord {
            name: "feature".to_string(),
            root: PathBuf::from("/repo/.usagi/sessions/feature"),
            worktrees: vec![merged_a, merged_b, local_c],
            created_at: Utc::now(),
        }]);

        // The three repositories collapse into a single row.
        assert_eq!(state.list().worktrees().len(), 1);
        let row = &state.list().worktrees()[0];
        assert_eq!(row.branch.as_deref(), Some("feature"));
        // Keyed on the session tree root (not any single repository's worktree).
        assert_eq!(row.path, PathBuf::from("/repo/.usagi/sessions/feature"));
        // Least-progressed wins: one local repo keeps the whole session `local`.
        assert_eq!(row.status, BranchStatus::Local);
        // Primary is set because one repository's worktree is primary.
        assert!(row.primary);
        // Representative detail comes from the first repository.
        assert_eq!(row.upstream.as_deref(), Some("origin/feature"));
    }

    #[test]
    fn a_session_with_no_worktrees_still_yields_a_row() {
        let mut state = state();
        state.restore_sessions(vec![SessionRecord {
            name: "empty".to_string(),
            root: PathBuf::from("/repo/.usagi/sessions/empty"),
            worktrees: Vec::new(),
            created_at: Utc::now(),
        }]);
        assert_eq!(state.list().worktrees().len(), 1);
        let row = &state.list().worktrees()[0];
        assert_eq!(row.branch.as_deref(), Some("empty"));
        // No repositories: a conservative `local`, no primary, no upstream, and
        // an empty representative head.
        assert_eq!(row.status, BranchStatus::Local);
        assert!(!row.primary);
        assert!(row.upstream.is_none());
        assert!(row.head.is_empty());
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
        state.remove_modal_move_up();
        state.remove_modal_move_up();
        assert_eq!(state.remove_modal().unwrap().cursor(), 2);
        state.remove_modal_move_down();
        assert_eq!(state.remove_modal().unwrap().cursor(), 0);
    }

    #[test]
    fn remove_modal_toggle_checks_and_unchecks_the_cursor_row() {
        let mut state = state();
        state.restore_sessions(vec![session_record("a", 1), session_record("b", 1)]);
        state.open_remove_modal(false);
        state.remove_modal_toggle();
        state.remove_modal_move_down();
        state.remove_modal_toggle();
        let modal = state.remove_modal().unwrap();
        assert!(modal.is_selected(0));
        assert!(modal.is_selected(1));
        assert_eq!(modal.selected_count(), 2);
        state.remove_modal_toggle();
        assert!(!state.remove_modal().unwrap().is_selected(1));
    }

    #[test]
    fn remove_modal_navigation_is_a_noop_when_empty_or_closed() {
        let mut state = state();
        state.open_remove_modal(false);
        assert!(state.remove_modal().unwrap().is_empty());
        state.remove_modal_move_up();
        state.remove_modal_move_down();
        state.remove_modal_toggle();
        assert_eq!(state.remove_modal().unwrap().cursor(), 0);
        assert_eq!(state.remove_modal().unwrap().selected_count(), 0);

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
        state.open_remove_modal(true);
        state.remove_modal_move_down();
        state.remove_modal_move_down();
        state.remove_modal_toggle(); // "c"
        state.remove_modal_move_up();
        state.remove_modal_move_up();
        state.remove_modal_toggle(); // "a"
        let (names, force) = state.submit_remove_modal().unwrap();
        assert_eq!(names, vec!["a".to_string(), "c".to_string()]);
        assert!(force);
        assert!(state.remove_modal().is_none());
    }

    #[test]
    fn submit_remove_modal_with_nothing_checked_keeps_it_open() {
        let mut state = state();
        state.restore_sessions(vec![session_record("a", 1)]);
        state.open_remove_modal(false);
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
    fn hint_no_live_session_logs_a_notice_pointing_at_the_launch_commands() {
        let mut state = state();
        state.hint_no_live_session();
        let last = state.log().last().unwrap();
        assert_eq!(last.kind, LineKind::Notice);
        assert!(last.text.contains(":agent"));
        assert!(last.text.contains(":terminal"));
    }

    #[test]
    fn waiting_paths_track_sessions_awaiting_input() {
        let mut state = state();
        assert!(!state.is_waiting(Path::new("/repo/feature")));
        assert!(state.waiting_paths().is_empty());
        let mut waiting = HashSet::new();
        waiting.insert(PathBuf::from("/repo/feature"));
        state.set_waiting(waiting);
        assert!(state.is_waiting(Path::new("/repo/feature")));
        assert!(!state.is_waiting(Path::new("/repo/main")));
        state.set_waiting(HashSet::new());
        assert!(!state.is_waiting(Path::new("/repo/feature")));
    }

    #[test]
    fn live_paths_track_sessions_with_a_running_agent() {
        let mut state = state();
        assert!(!state.is_live(Path::new("/repo/feature")));
        assert!(state.live_paths().is_empty());
        let mut live = HashSet::new();
        live.insert(PathBuf::from("/repo/feature"));
        state.set_live(live);
        assert!(state.is_live(Path::new("/repo/feature")));
        assert!(!state.is_live(Path::new("/repo/main")));
        assert_eq!(state.live_paths().len(), 1);
        state.set_live(HashSet::new());
        assert!(!state.is_live(Path::new("/repo/feature")));
    }
}
