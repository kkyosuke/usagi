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
use super::tasks::TaskRow;
use super::terminal_tabs::TabStrip;
use super::terminal_view::TerminalView;
use crate::presentation::tui::widgets::text_input::TextInput;

mod list;
mod log;
mod modal;
mod mode;

pub use list::{worktree_name, WorktreeList, ROOT_NAME};
pub use log::{LineKind, LogLine};
pub use modal::{Preview, RemoveModal, TextModal};
pub use mode::{Mode, PaneExit, ReturnMode};

use list::session_row;
use modal::{CreateInput, RenameInput};

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

/// A transient "working…" indicator shown in the top-right corner while a
/// blocking action runs (creating or bulk-removing sessions, launching a
/// terminal / agent). It carries the `label` to show beside the loading rabbit
/// and a `frame` tick that advances on each step, so painting it repeatedly
/// animates the rabbit. Read by the renderer through [`HomeState::loading`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadingIndicator {
    label: String,
    frame: usize,
}

impl LoadingIndicator {
    /// The message shown beside the rabbit (e.g. `作成中…`).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The animation tick, advanced on each step of the running action.
    pub fn frame(&self) -> usize {
        self.frame
    }
}

/// The full state of the home screen.
///
/// Not `Clone`/`Debug`: it owns a [`CommandRegistry`] of trait objects, which
/// are neither. Nothing needs to clone or format the whole screen state.
pub struct HomeState {
    list: WorktreeList,
    mode: Mode,
    /// The command line buffer with its caret — drives in-line editing
    /// (←/→/Home/End/Del) and where the caret renders.
    input: TextInput,
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
    /// Whether the `ai` command is offered in the 在席 (Focus) menu: true only
    /// when the local LLM is enabled and its model is pulled. Injected from the
    /// effective settings (and a runtime probe) by `mod.rs`; false by default so
    /// the command stays hidden until the model is actually usable.
    ai_available: bool,
    /// Where a 切替 (Switch) returns to on `Esc` / `h`; only meaningful in
    /// [`Mode::Switch`].
    switch_return: ReturnMode,
    /// The inline session-name input, when creating a session from 切替. While
    /// set it captures the Switch mode's keys.
    create: Option<CreateInput>,
    /// The inline display-name input, when renaming a session's sidebar label
    /// from 切替. While set it captures the Switch mode's keys, like `create`.
    rename: Option<RenameInput>,
    /// The 在席 (Focus) menu cursor: which Session-scope command is highlighted.
    focus_menu_cursor: usize,
    /// The 在席 (Focus) prompt buffer (the session-scoped command line).
    focus_prompt: TextInput,
    /// The session-removal modal, when open (the user ran `session remove`
    /// without a name). While set it captures all keys.
    remove_modal: Option<RemoveModal>,
    /// Sessions recorded for this workspace (from `state.json`), shown by
    /// `session list` and kept current as sessions are created.
    sessions: Vec<SessionRecord>,
    /// The latest snapshot of the embedded terminal's screen, set while a session
    /// is 没入 (Attached) and rendered in the right pane.
    terminal_view: Option<TerminalView>,
    /// The tab strip shown above the embedded terminal while 没入 (Attached): the
    /// session's panes and which one is active. Set alongside the snapshot by the
    /// pane driver; `None` outside 没入 (and cleared each frame, like the view).
    terminal_tabs: Option<TabStrip>,
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
    /// The open right-pane Markdown preview (the `preview` command), when set.
    /// Unlike the centred text modal it takes over only the right pane, leaving
    /// the session list and command line in place; while open it captures the
    /// keys (scroll / dismiss).
    preview: Option<Preview>,
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
    /// The transient "working…" indicator, set while a blocking action runs
    /// (session create / bulk remove / terminal launch). While `Some` the
    /// top-right corner shows the loading rabbit instead of the update notice.
    loading: Option<LoadingIndicator>,
    /// The rows of the background-task panel (session create / remove running off
    /// the event-loop thread), refreshed each frame from the shared task handle.
    /// While non-empty the top-right corner stacks them instead of the update
    /// notice, so the user sees in-flight work without the screen freezing.
    tasks: Vec<TaskRow>,
    /// The workspace root path — the directory the root row (`⌂ root`) operates
    /// in. The list's worktrees carry their own paths, but the root row has
    /// none, so this is stored separately to recognise the root's live embedded
    /// session (keyed by this path in `live` / `running` / …). Injected by
    /// `mod.rs`; empty until set (tests that never preview the root leave it so).
    root_path: PathBuf,
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
            input: TextInput::new(),
            history: Vec::new(),
            recall: None,
            log,
            registry: CommandRegistry::with_builtins(),
            session_action_ui: SessionActionUi::default(),
            ai_available: false,
            switch_return: ReturnMode::Overview,
            create: None,
            rename: None,
            focus_menu_cursor: 0,
            focus_prompt: TextInput::new(),
            remove_modal: None,
            sessions: Vec::new(),
            terminal_view: None,
            terminal_tabs: None,
            running: HashSet::new(),
            waiting: HashSet::new(),
            live: HashSet::new(),
            done: HashSet::new(),
            quit_confirm: false,
            text_modal: None,
            preview: None,
            response_start: 0,
            issues: Vec::new(),
            update: None,
            loading: None,
            tasks: Vec::new(),
            root_path: PathBuf::new(),
        }
    }

    /// Record the workspace root path so the root row (`⌂ root`) can be matched
    /// against the live / running / waiting / done path sets — its embedded
    /// session is keyed by this path, exactly as a worktree row is keyed by its
    /// own. Injected by `mod.rs` at construction.
    pub fn set_root_path(&mut self, root: impl Into<PathBuf>) {
        self.root_path = root.into();
    }

    /// The workspace root path the root row operates in (see [`set_root_path`]).
    ///
    /// [`set_root_path`]: Self::set_root_path
    pub fn root_path(&self) -> &Path {
        &self.root_path
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

    /// Set whether the `ai` command is offered in the 在席 (Focus) menu (injected
    /// from the effective settings and a runtime probe by `mod.rs`).
    pub fn set_ai_available(&mut self, available: bool) {
        self.ai_available = available;
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
        // Carry each session's sidebar label override onto its row so the pane
        // shows the custom display name while commands still key on the branch.
        let labels = self
            .sessions
            .iter()
            .map(|s| s.display_name.clone())
            .collect();
        self.list = WorktreeList::with_labels(name, rows, labels);
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

    /// Open the right-pane Markdown preview from a load attempt: on success, show
    /// the rendered file (titled by its workspace-relative path); on failure, log
    /// the error and open nothing. The impure file read is the caller's (the
    /// event loop reads it through [`crate::infrastructure::markdown_file`]); this
    /// only renders the text and stores the result, so both outcomes are testable.
    pub fn open_preview_result(&mut self, loaded: anyhow::Result<(String, String)>) {
        match loaded {
            Ok((title, content)) => {
                self.preview = Some(Preview {
                    title,
                    lines: crate::presentation::tui::markdown::render(&content),
                    scroll: 0,
                });
            }
            Err(e) => self.log_error(format!("preview failed: {e}")),
        }
    }

    /// The open right-pane preview, if any.
    pub fn preview(&self) -> Option<&Preview> {
        self.preview.as_ref()
    }

    /// Close the right-pane preview (the user dismissed it).
    pub fn close_preview(&mut self) {
        self.preview = None;
    }

    /// Scroll the preview up one line (no-op when closed or at the top).
    pub fn preview_scroll_up(&mut self) {
        if let Some(preview) = self.preview.as_mut() {
            preview.scroll = preview.scroll.saturating_sub(1);
        }
    }

    /// Scroll the preview down one line, clamped so the last line stays in view
    /// (no-op when closed). `visible` is the pane body height the view can show.
    pub fn preview_scroll_down(&mut self, visible: usize) {
        if let Some(preview) = self.preview.as_mut() {
            let max = preview.lines.len().saturating_sub(visible);
            preview.scroll = (preview.scroll + 1).min(max);
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
        self.input.value()
    }

    /// The caret position in [`input`](Self::input) as a byte offset, so the
    /// renderer can split the line and draw the caret where editing happens.
    pub fn cursor(&self) -> usize {
        self.input.cursor()
    }

    /// The advisory input hint for the current command input (matching commands,
    /// or the usage of the command being given arguments). Computed on demand
    /// for rendering; see [`CommandRegistry::suggest`].
    pub fn hint(&self) -> Hint {
        self.registry
            .suggest(self.input.value(), self.command_scope())
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
        self.terminal_tabs = None;
    }

    /// Publish the tab strip shown above the embedded terminal in 没入: the
    /// session's pane `labels` and which one is `active`. Set by the pane driver
    /// before each repaint, alongside [`set_terminal_view`](Self::set_terminal_view).
    pub fn set_terminal_tabs(&mut self, labels: Vec<String>, active: usize) {
        self.terminal_tabs = Some(TabStrip { labels, active });
    }

    /// The tab strip shown above the embedded terminal, when 没入 (Attached).
    pub fn terminal_tabs(&self) -> Option<&TabStrip> {
        self.terminal_tabs.as_ref()
    }

    /// Drop the tab strip (the pane driver left 没入).
    pub fn clear_terminal_tabs(&mut self) {
        self.terminal_tabs = None;
    }

    /// Store the latest embedded-terminal screen snapshot, shown in the right
    /// pane while the session is 没入 (Attached).
    pub fn set_terminal_view(&mut self, view: TerminalView) {
        self.terminal_view = Some(view);
    }

    /// Drop the embedded-terminal snapshot (and its tab strip) without changing
    /// the mode. Used between frames so a stale snapshot never lingers in the
    /// right pane.
    pub fn clear_terminal_view(&mut self) {
        self.terminal_view = None;
        self.terminal_tabs = None;
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

    /// Begin or advance the transient "working…" indicator with `label`, ticking
    /// its animation frame. Call it before each step of a blocking action (and
    /// repaint) so the top-right loading rabbit appears and hops; a multi-step
    /// action (e.g. a bulk removal) steps once per item so the rabbit animates as
    /// it progresses.
    pub fn step_loading(&mut self, label: impl Into<String>) {
        let frame = self.loading.as_ref().map_or(0, |l| l.frame + 1);
        self.loading = Some(LoadingIndicator {
            label: label.into(),
            frame,
        });
    }

    /// Clear the "working…" indicator once the blocking action has finished, so
    /// the top-right corner returns to its resting state (the update notice, or
    /// nothing).
    pub fn finish_loading(&mut self) {
        self.loading = None;
    }

    /// The transient "working…" indicator, when an action is in flight — the
    /// top-right loading rabbit is shown (taking the corner over the update
    /// notice) only while this is `Some`.
    pub fn loading(&self) -> Option<&LoadingIndicator> {
        self.loading.as_ref()
    }

    /// Swap in the current background-task rows (session create / remove running
    /// off the event-loop thread), read from the shared task handle each frame.
    /// While non-empty the top-right corner stacks them.
    pub fn set_tasks(&mut self, tasks: Vec<TaskRow>) {
        self.tasks = tasks;
    }

    /// The background-task panel rows to render in the top-right corner.
    pub fn tasks(&self) -> &[TaskRow] {
        &self.tasks
    }

    /// Apply a finished background task's outcome: append its result line to the
    /// log and, when the action changed the sessions, swap in the refreshed list
    /// **keeping the cursor and active row where they are** (via
    /// [`refresh_sessions`](Self::refresh_sessions)) — a session created or
    /// removed in the background must never yank the user's cursor mid-navigation.
    pub fn apply_task_completion(&mut self, line: LogLine, sessions: Option<Vec<SessionRecord>>) {
        self.log.push(line);
        if let Some(sessions) = sessions {
            self.refresh_sessions(sessions);
        }
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
        self.create.as_ref().map(|c| c.input.value())
    }

    /// The caret position in the inline create name, if open — so the renderer
    /// can draw the caret where editing happens.
    pub fn create_cursor(&self) -> Option<usize> {
        self.create.as_ref().map(|c| c.input.cursor())
    }

    /// The inline create input's current validation error, if any.
    pub fn create_error(&self) -> Option<&str> {
        self.create.as_ref().and_then(|c| c.error.as_deref())
    }

    /// Insert a character at the caret of the inline create name (no-op when not
    /// creating), re-validating live so the error reflects the new name.
    pub fn create_push_char(&mut self, c: char) {
        if let Some(create) = self.create.as_mut() {
            create.input.insert(c);
            create.error = validate_session_name(create.input.value(), &create.taken);
        }
    }

    /// Delete the character before the caret of the inline create name (no-op
    /// when not creating), re-validating live.
    pub fn create_backspace(&mut self) {
        if let Some(create) = self.create.as_mut() {
            create.input.backspace();
            create.error = validate_session_name(create.input.value(), &create.taken);
        }
    }

    /// Delete the character at the caret of the inline create name (the `Del`
    /// key; no-op when not creating), re-validating live.
    pub fn create_delete_forward(&mut self) {
        if let Some(create) = self.create.as_mut() {
            create.input.delete_forward();
            create.error = validate_session_name(create.input.value(), &create.taken);
        }
    }

    /// Move the inline create caret one character left (no-op when not creating).
    pub fn create_cursor_left(&mut self) {
        if let Some(create) = self.create.as_mut() {
            create.input.move_left();
        }
    }

    /// Move the inline create caret one character right (no-op when not creating).
    pub fn create_cursor_right(&mut self) {
        if let Some(create) = self.create.as_mut() {
            create.input.move_right();
        }
    }

    /// Move the inline create caret to the start of the name (no-op when not
    /// creating).
    pub fn create_cursor_home(&mut self) {
        if let Some(create) = self.create.as_mut() {
            create.input.move_home();
        }
    }

    /// Move the inline create caret to the end of the name (no-op when not
    /// creating).
    pub fn create_cursor_end(&mut self) {
        if let Some(create) = self.create.as_mut() {
            create.input.move_end();
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
        let name = create.input.value().trim().to_string();
        // Enter on an empty name is the one case live validation stays quiet
        // about (it does not nag while nothing is typed), so guard it here.
        if name.is_empty() {
            create.error = Some("Name must not be empty.".to_string());
            return None;
        }
        if let Some(error) = validate_session_name(create.input.value(), &create.taken) {
            create.error = Some(error);
            return None;
        }
        self.create = None;
        Some(name)
    }

    /// Begin inline rename of the selected session's sidebar label in 切替: open
    /// an input pre-filled with its current label that captures the mode's keys
    /// until confirmed (Enter) or cancelled (Esc). A no-op on the root row (which
    /// is not a session and has no label to change) and when an input is already
    /// open. Returns whether the input opened.
    pub fn switch_begin_rename(&mut self) -> bool {
        if self.create.is_some() || self.rename.is_some() {
            return false;
        }
        let Some(worktree) = self.list.selected() else {
            return false;
        };
        let target = worktree_name(worktree).to_string();
        // Pre-fill with the label currently shown so the user edits rather than
        // retypes; an unset override pre-fills with the session name.
        let input = self
            .list
            .display_label(self.list.selected_index() - 1)
            .to_string();
        self.rename = Some(RenameInput { target, input });
        true
    }

    /// Whether an inline rename input is open in 切替.
    pub fn is_renaming(&self) -> bool {
        self.rename.is_some()
    }

    /// The label typed so far in the inline rename input, if open.
    pub fn rename_input(&self) -> Option<&str> {
        self.rename.as_ref().map(|r| r.input.as_str())
    }

    /// The name of the session being renamed (its branch / identity), if open.
    pub fn rename_target(&self) -> Option<&str> {
        self.rename.as_ref().map(|r| r.target.as_str())
    }

    /// Append a character to the inline rename label (no-op when not renaming).
    pub fn rename_push_char(&mut self, c: char) {
        if let Some(rename) = self.rename.as_mut() {
            rename.input.push(c);
        }
    }

    /// Delete the last character of the inline rename label (no-op when not
    /// renaming).
    pub fn rename_backspace(&mut self) {
        if let Some(rename) = self.rename.as_mut() {
            rename.input.pop();
        }
    }

    /// Cancel inline renaming, staying in 切替.
    pub fn rename_cancel(&mut self) {
        self.rename = None;
    }

    /// Accept the inline rename: close the input and return the target session
    /// name together with the typed label (trimmed), for the event loop to
    /// persist. The label is returned as typed — an empty one means "clear the
    /// override", which the usecase resolves. A no-op (returning `None`) when not
    /// renaming.
    pub fn switch_confirm_rename(&mut self) -> Option<(String, String)> {
        let rename = self.rename.take()?;
        Some((rename.target, rename.input.trim().to_string()))
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
    /// (`terminal`, `agent`, `ai`). The `ai` command is filtered out unless the
    /// local LLM is usable (enabled and its model pulled), so it only appears
    /// when running it would actually work. `close` is filtered out on the root
    /// row, which belongs to no session and so cannot be closed.
    pub fn focus_menu_commands(&self) -> Vec<CommandInfo> {
        let root = self.list.root_active();
        self.registry
            .commands_in_scope(CommandScope::Session)
            .into_iter()
            .filter(|info| info.name != "ai" || self.ai_available)
            .filter(|info| info.name != "close" || !root)
            .collect()
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
        self.focus_prompt.value()
    }

    /// The caret position in the 在席 prompt, so the renderer can draw the caret
    /// where editing happens.
    pub fn focus_prompt_cursor(&self) -> usize {
        self.focus_prompt.cursor()
    }

    /// Insert a character at the caret of the 在席 prompt.
    pub fn focus_prompt_push_char(&mut self, c: char) {
        self.focus_prompt.insert(c);
    }

    /// Delete the character before the caret of the 在席 prompt.
    pub fn focus_prompt_backspace(&mut self) {
        self.focus_prompt.backspace();
    }

    /// Delete the character at the caret of the 在席 prompt (the `Del` key).
    pub fn focus_prompt_delete_forward(&mut self) {
        self.focus_prompt.delete_forward();
    }

    /// Move the 在席 prompt caret one character left.
    pub fn focus_prompt_cursor_left(&mut self) {
        self.focus_prompt.move_left();
    }

    /// Move the 在席 prompt caret one character right.
    pub fn focus_prompt_cursor_right(&mut self) {
        self.focus_prompt.move_right();
    }

    /// Move the 在席 prompt caret to the start of the line.
    pub fn focus_prompt_cursor_home(&mut self) {
        self.focus_prompt.move_home();
    }

    /// Move the 在席 prompt caret to the end of the line.
    pub fn focus_prompt_cursor_end(&mut self) {
        self.focus_prompt.move_end();
    }

    /// Tab-complete the 在席 prompt's command word against the Session-scope
    /// commands, returning the candidates when ambiguous (so the caller can log
    /// them, mirroring the Overview line's `complete`).
    pub fn focus_prompt_complete(&mut self) -> Completion {
        let completion = self
            .registry
            .complete(self.focus_prompt.value(), CommandScope::Session);
        self.focus_prompt.set_value(completion.input.clone());
        completion
    }

    /// The advisory hint for the 在席 prompt, computed in the Session scope.
    pub fn focus_prompt_hint(&self) -> Hint {
        self.registry
            .suggest(self.focus_prompt.value(), CommandScope::Session)
    }

    /// Run the 在席 prompt as a Session-scope command: dispatch it, append its
    /// produced lines to the log, clear the prompt, and return the resulting
    /// [`Submission`] (so the event loop can act on `OpenTerminal` / `OpenAgent`).
    /// Empty input is a no-op.
    pub fn focus_prompt_submit(&mut self) -> Submission {
        let entry = self.focus_prompt.value().trim().to_string();
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
        self.input.insert(c);
        self.recall = None;
    }

    /// Delete the character before the caret (command mode), moving it back.
    pub fn backspace(&mut self) {
        self.input.backspace();
        self.recall = None;
    }

    /// Delete the character at the caret (the `Del`/forward-delete key), leaving
    /// the caret in place.
    pub fn delete_forward(&mut self) {
        self.input.delete_forward();
        self.recall = None;
    }

    /// Move the caret one character left.
    pub fn cursor_left(&mut self) {
        self.input.move_left();
    }

    /// Move the caret one character right.
    pub fn cursor_right(&mut self) {
        self.input.move_right();
    }

    /// Move the caret to the start of the line.
    pub fn cursor_home(&mut self) {
        self.input.move_home();
    }

    /// Move the caret to the end of the line.
    pub fn cursor_end(&mut self) {
        self.input.move_end();
    }

    /// Tab-complete the command word, listing candidates when ambiguous.
    pub fn complete(&mut self) {
        let completion = self
            .registry
            .complete(self.input.value(), self.command_scope());
        self.input.set_value(completion.input);
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
        self.input.set_value(self.history[index].clone());
    }

    /// Recall the next (newer) command, returning to an empty line past the end.
    pub fn recall_next(&mut self) {
        let index = match self.recall {
            None => return,
            Some(i) => i,
        };
        if index + 1 < self.history.len() {
            self.recall = Some(index + 1);
            self.input.set_value(self.history[index + 1].clone());
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
        let entry = self.input.value().trim().to_string();
        self.input.clear();
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
