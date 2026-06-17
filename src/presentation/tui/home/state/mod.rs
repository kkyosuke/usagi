//! Pure, terminal-independent state for the home (workspace) screen.
//!
//! The home screen is a small command shell laid out in three panes: the
//! worktree list (left), the command log (right), and a command input line
//! (bottom). [`HomeState`] holds all of it — the selectable worktree list, the
//! current mode, the input buffer and its history, and the output log — with no
//! terminal IO, so the navigation, editing, and command logic are all directly
//! testable.
//!
//! This module owns [`HomeState`] itself and the [`Submission`] / [`SessionOutcome`]
//! DTOs it exchanges with the event loop. The value types it holds live in
//! sibling modules: the worktree [`list`], the [`mode`] enums, the output
//! [`log`] line model, and the transient [`modal`] state.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::domain::issue::Issue;
use crate::domain::settings::SessionActionUi;
use crate::domain::version::Version;
use crate::domain::workspace_state::{SessionRecord, WorktreeState};

use super::command::{CommandInfo, CommandRegistry, CommandScope, Completion, Effect, Hint};
use super::terminal_view::TerminalView;

mod list;
mod log;
mod modal;
mod mode;

pub use list::{worktree_name, WorktreeList, ROOT_NAME};
pub use log::{LineKind, LogLine};
pub use modal::{RemoveModal, TextModal};
pub use mode::{Mode, PaneExit, ReturnMode};

use list::session_row;
use modal::CreateInput;

/// The outcome of submitting the command line: the side effect to act on, plus
/// the command that was recorded in history (so the event loop can persist it).
#[derive(Debug)]
pub struct Submission {
    pub effect: Effect,
    /// The command that was run and added to history, or `None` for empty input.
    pub recorded: Option<String>,
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
    /// Caret position in `input`, as a byte offset on a `char` boundary. Drives
    /// in-line editing (←/→/Home/End/Del) and where the caret renders.
    cursor: usize,
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
    /// Worktree paths whose agent is actively working a turn (reported the
    /// `running` phase). Refreshed from the terminal monitor each redraw and
    /// rendered with a "running" icon, unless the path is also waiting or done
    /// (which take precedence).
    running: HashSet<PathBuf>,
    /// Worktree paths whose background session is waiting for the user (its
    /// agent paused mid-turn for input/permission, or rang the bell). Refreshed
    /// from the terminal monitor each redraw and rendered as a marker in the
    /// sidebar.
    waiting: HashSet<PathBuf>,
    /// Worktree paths with a live embedded session — an agent/shell is in use,
    /// whether attached or left running in the background. Refreshed from the
    /// terminal monitor each redraw; a live path that is not running, waiting, or
    /// done renders as "ready" (idle, awaiting the first prompt).
    live: HashSet<PathBuf>,
    /// Worktree paths whose agent has finished — a turn completed or the process
    /// exited — shown with a "done" badge. Refreshed from the terminal monitor
    /// each redraw; takes precedence over running and waiting.
    done: HashSet<PathBuf>,
    /// Whether the quit-confirmation modal is open. It is raised when the user
    /// presses `Ctrl-C` while a session is still live, so an accidental close
    /// does not drop a running agent/shell; confirming it quits the app.
    quit_confirm: bool,
    /// The open text modal (a text-dumping command's output, e.g. `man`), when
    /// set. While open it captures the keys (scroll / dismiss).
    text_modal: Option<TextModal>,
    /// Index into `log` where the most recent command's response begins. The
    /// 統括 (Overview) results band renders only `log[response_start..]`, so it
    /// shows the response to the latest command and nothing earlier.
    response_start: usize,
    /// The workspace's task issues, loaded from disk by `mod.rs` and read by the
    /// `issue` command. Empty until injected.
    issues: Vec<Issue>,
    /// The latest released version, set once the background update check finds a
    /// release newer than this build. While `None` (the check is pending, or the
    /// build is up to date) the top-right "update available" notice is hidden.
    update: Option<Version>,
}

impl HomeState {
    /// Builds the screen state for `workspace_name` and its `worktrees`. An
    /// optional `notice` (e.g. a load error) seeds the log below a short hint.
    pub fn new(
        workspace_name: impl Into<String>,
        worktrees: Vec<WorktreeState>,
        notice: Option<String>,
    ) -> Self {
        let mut log = vec![LogLine::output("Type \"man\" for help.")];
        if let Some(notice) = notice {
            log.push(LogLine::error(notice));
        }
        Self {
            list: WorktreeList::new(workspace_name, worktrees),
            mode: Mode::Overview,
            input: String::new(),
            cursor: 0,
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
            running: HashSet::new(),
            waiting: HashSet::new(),
            live: HashSet::new(),
            done: HashSet::new(),
            quit_confirm: false,
            text_modal: None,
            response_start: 0,
            issues: Vec::new(),
            update: None,
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

    /// Inject the workspace's task issues (loaded from disk by `mod.rs`), read by
    /// the `issue` command for its list / graph / show views.
    pub fn set_issues(&mut self, issues: Vec<Issue>) {
        self.issues = issues;
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

    /// Swap in a freshly re-synced set of sessions while keeping the cursor and
    /// the active row on the same session names (when they still exist).
    ///
    /// Used after the user works in an embedded terminal / agent — where they may
    /// commit, push, or merge — so the worktree status reflects what they just
    /// did, without yanking the cursor back to the root row the way
    /// [`restore_sessions`](Self::restore_sessions) (which resets it) would.
    pub fn refresh_sessions(&mut self, sessions: Vec<SessionRecord>) {
        let selected = self.list.selected_name().to_string();
        let active = self.list.active_name().to_string();
        self.sessions = sessions;
        self.rebuild_list();
        // Restore the cursor (`select_by_name` moves both cursor and active onto
        // the row; it is a no-op for the root row / a vanished session, leaving
        // the rebuilt default on the root), then correct the active row.
        self.list.select_by_name(&selected);
        self.list.activate_by_name(&active);
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

    /// Show the recorded sessions (the `session list` command). With sessions it
    /// opens a scrollable text modal; with none it reports the empty state in the
    /// results band (a one-liner needs no modal).
    pub fn log_sessions(&mut self) {
        if self.sessions.is_empty() {
            self.log.push(LogLine::output(
                "No sessions yet. Run \"session create <name>\" to create one.",
            ));
            return;
        }
        let mut lines = vec![LogLine::output(format!(
            "{} session(s):",
            self.sessions.len()
        ))];
        for session in &self.sessions {
            lines.push(LogLine::output(format!(
                "  {}  ({} worktree(s))",
                session.name,
                session.worktrees.len()
            )));
        }
        self.open_text_modal("Sessions", lines);
    }

    /// Open a scrollable text modal showing `lines` under `title` (used by the
    /// text-dumping commands). Replaces any modal already open.
    pub fn open_text_modal(&mut self, title: impl Into<String>, lines: Vec<LogLine>) {
        self.text_modal = Some(TextModal {
            title: title.into(),
            lines,
            scroll: 0,
        });
    }

    /// The open text modal, if any.
    pub fn text_modal(&self) -> Option<&TextModal> {
        self.text_modal.as_ref()
    }

    /// Close the text modal (the user dismissed it).
    pub fn close_text_modal(&mut self) {
        self.text_modal = None;
    }

    /// Scroll the text modal up one line (no-op when closed or at the top).
    pub fn text_modal_scroll_up(&mut self) {
        if let Some(modal) = self.text_modal.as_mut() {
            modal.scroll = modal.scroll.saturating_sub(1);
        }
    }

    /// Scroll the text modal down one line, clamped so the last line stays in
    /// view (no-op when closed). `visible` is the body height the view can show.
    pub fn text_modal_scroll_down(&mut self, visible: usize) {
        if let Some(modal) = self.text_modal.as_mut() {
            let max = modal.lines.len().saturating_sub(visible);
            modal.scroll = (modal.scroll + 1).min(max);
        }
    }

    /// The lines of the most recent command's response (what the 統括 results band
    /// shows): everything in the log from `response_start` onward.
    pub fn response_lines(&self) -> &[LogLine] {
        let start = self.response_start.min(self.log.len());
        &self.log[start..]
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

    /// The caret position in [`input`](Self::input) as a byte offset, so the
    /// renderer can split the line and draw the caret where editing happens.
    pub fn cursor(&self) -> usize {
        self.cursor
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

    /// Replace the set of worktree paths whose agent is actively working a turn,
    /// refreshed from the terminal monitor before each redraw.
    pub fn set_running(&mut self, running: HashSet<PathBuf>) {
        self.running = running;
    }

    /// Whether the worktree at `path` has a background session actively working a
    /// turn.
    pub fn is_running(&self, path: &Path) -> bool {
        self.running.contains(path)
    }

    /// The set of worktree paths whose agent is actively working a turn, for the
    /// sidebar renderer.
    pub fn running_paths(&self) -> &HashSet<PathBuf> {
        &self.running
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

    /// Replace the set of worktree paths whose agent has finished, refreshed from
    /// the terminal monitor before each redraw.
    pub fn set_done(&mut self, done: HashSet<PathBuf>) {
        self.done = done;
    }

    /// Whether the worktree at `path` has a background session whose agent has
    /// finished (a turn completed or it exited).
    pub fn is_done(&self, path: &Path) -> bool {
        self.done.contains(path)
    }

    /// The set of worktree paths whose agent has finished, for the sidebar
    /// renderer.
    pub fn done_paths(&self) -> &HashSet<PathBuf> {
        &self.done
    }

    /// Record the latest released version found by the background update check,
    /// or clear it with `None`. Set before each redraw from the update handle.
    pub fn set_update(&mut self, latest: Option<Version>) {
        self.update = latest;
    }

    /// The latest released version, when it is newer than this build — the
    /// top-right "update available" notice is shown only while this is `Some`.
    pub fn update(&self) -> Option<Version> {
        self.update
    }

    /// How many sessions currently have a live (running) embedded shell/agent.
    /// Shown in the quit-confirmation modal so the user sees what is at stake.
    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// Whether any session has a live (running) embedded shell/agent — the
    /// condition that makes `Ctrl-C` ask for confirmation before quitting.
    pub fn has_live_sessions(&self) -> bool {
        !self.live.is_empty()
    }

    /// Whether the quit-confirmation modal is open.
    pub fn quit_confirm(&self) -> bool {
        self.quit_confirm
    }

    /// Open the quit-confirmation modal (a live session is still running).
    pub fn open_quit_confirm(&mut self) {
        self.quit_confirm = true;
    }

    /// Dismiss the quit-confirmation modal without quitting.
    pub fn cancel_quit_confirm(&mut self) {
        self.quit_confirm = false;
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
    ///
    /// `taken` is the set of branch names that already exist across the
    /// workspace's repositories (from
    /// [`crate::usecase::session::existing_branch_names`]); the typed name is
    /// validated against it live so a duplicate or branch-namespace clash is
    /// flagged before Enter.
    pub fn switch_begin_create(&mut self, taken: Vec<String>) {
        self.create = Some(CreateInput {
            taken,
            ..Default::default()
        });
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

    /// Append a character to the inline create name (no-op when not creating),
    /// re-validating live so the error reflects the new name.
    pub fn create_push_char(&mut self, c: char) {
        if let Some(create) = self.create.as_mut() {
            create.input.push(c);
            create.error = validate_session_name(&create.input, &create.taken);
        }
    }

    /// Delete the last character of the inline create name (no-op when not
    /// creating), re-validating live.
    pub fn create_backspace(&mut self) {
        if let Some(create) = self.create.as_mut() {
            create.input.pop();
            create.error = validate_session_name(&create.input, &create.taken);
        }
    }

    /// Cancel inline creation, staying in 切替.
    pub fn create_cancel(&mut self) {
        self.create = None;
    }

    /// Validate and accept the inline create name. On success the input closes
    /// and the trimmed name is returned (for the event loop to create the
    /// session); on an invalid name (empty, a path separator, a duplicate, or a
    /// branch-namespace clash) the input stays open with the same inline error
    /// shown live and `None` is returned. A no-op (returning `None`) when not
    /// creating.
    pub fn switch_confirm_create(&mut self) -> Option<String> {
        let create = self.create.as_mut()?;
        let name = create.input.trim().to_string();
        // Enter on an empty name is the one case live validation stays quiet
        // about (it does not nag while nothing is typed), so guard it here.
        if name.is_empty() {
            create.error = Some("Name must not be empty.".to_string());
            return None;
        }
        if let Some(error) = validate_session_name(&create.input, &create.taken) {
            create.error = Some(error);
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
        let result =
            self.registry
                .dispatch_with(&entry, &self.history, &self.list.refs(), &self.issues);
        self.history.push(entry.clone());
        // A text-dumping utility (`man` / `history`) run from the prompt shows its
        // output in a modal, like in 統括; everything else appends to the log.
        if let Effect::ShowText(title) = result.effect {
            self.open_text_modal(title, result.lines);
        } else {
            self.log.extend(result.lines);
        }
        Submission {
            effect: result.effect,
            recorded: Some(entry),
        }
    }

    /// Insert a typed character at the caret (Overview line), advancing it.
    pub fn push_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.recall = None;
    }

    /// Delete the character before the caret (command mode), moving it back.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_boundary();
        self.input.replace_range(prev..self.cursor, "");
        self.cursor = prev;
        self.recall = None;
    }

    /// Delete the character at the caret (the `Del`/forward-delete key), leaving
    /// the caret in place.
    pub fn delete_forward(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        let next = self.next_boundary();
        self.input.replace_range(self.cursor..next, "");
        self.recall = None;
    }

    /// Move the caret one character left.
    pub fn cursor_left(&mut self) {
        self.cursor = self.prev_boundary();
    }

    /// Move the caret one character right.
    pub fn cursor_right(&mut self) {
        self.cursor = self.next_boundary();
    }

    /// Move the caret to the start of the line.
    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the caret to the end of the line.
    pub fn cursor_end(&mut self) {
        self.cursor = self.input.len();
    }

    /// Byte offset of the `char` boundary just before the caret (or `0`).
    fn prev_boundary(&self) -> usize {
        self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    /// Byte offset of the `char` boundary just after the caret (or the end).
    fn next_boundary(&self) -> usize {
        self.input[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }

    /// Tab-complete the command word, listing candidates when ambiguous.
    pub fn complete(&mut self) {
        let completion = self.registry.complete(&self.input, self.command_scope());
        self.input = completion.input;
        self.cursor = self.input.len();
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
        self.cursor = self.input.len();
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
        self.cursor = self.input.len();
    }

    /// Run the current input as a command: echo it, dispatch it, record it in
    /// history, and apply the resulting log lines and side effect. Returns a
    /// [`Submission`] carrying the side effect (so the event loop can act on
    /// `Quit`) and the recorded command (so it can be persisted). Empty input is
    /// a no-op.
    pub fn submit(&mut self) -> Submission {
        let entry = self.input.trim().to_string();
        self.input.clear();
        self.cursor = 0;
        self.recall = None;
        if entry.is_empty() {
            return Submission {
                effect: Effect::None,
                recorded: None,
            };
        }

        // The results band shows only this command's response: mark where it
        // begins (the command echo), so everything earlier drops out of view.
        self.response_start = self.log.len();
        self.log.push(LogLine::command(entry.clone()));
        let result =
            self.registry
                .dispatch_with(&entry, &self.history, &self.list.refs(), &self.issues);
        self.history.push(entry.clone());

        match result.effect {
            Effect::Clear => {
                self.log.clear();
                self.response_start = 0;
            }
            // `session switch` (→ 切替) and `session switch <name>` (→ 在席) are
            // resolved by the event loop, which owns the mode transitions (and, for
            // a live session, the pane). They append no lines here.
            Effect::EnterSwitch | Effect::Activate(_) => {}
            // Text-dumping commands (`man` / `history`) show their output in a
            // scrollable modal, not the band; leave the band empty for them.
            Effect::ShowText(title) => {
                self.open_text_modal(title, result.lines);
                self.response_start = self.log.len();
            }
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

/// Validate a typed session name against the branch names already taken, used
/// for the live inline-create feedback. Returns the reason the name cannot be
/// used, or `None` when it is usable.
///
/// An empty (or all-whitespace) name returns `None` — the input does not nag
/// while nothing has been typed; the empty case is rejected only on Enter (see
/// [`HomeState::switch_confirm_create`]). The checks mirror what
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
mod tests;
