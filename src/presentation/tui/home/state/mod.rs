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
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::domain::issue::Issue;
use crate::domain::settings::{AgentCli, SessionActionUi, Sidebar};
use crate::domain::version::Version;
use crate::domain::workspace_state::{SessionRecord, WorktreeState};

use super::command::{
    CommandInfo, CommandRegistry, CommandResult, CommandScope, Completion, Effect, Hint,
};
use super::tasks::TaskRow;
use super::terminal::pool::MonitorSnapshot;
use super::terminal::tabs::TabStrip;
use super::terminal::view::TerminalView;
use crate::presentation::tui::widgets::text_input::TextInput;

mod list;
mod log;
mod modal;
mod mode;

pub use list::{worktree_name, WorktreeList, ROOT_NAME};
pub use log::{LineKind, LogLine};
pub use modal::{CreateInput, ModalSize, NoteEditor, Preview, RemoveModal, RenameInput, TextModal};
pub use mode::{Mode, PaneExit, ResumeLevel, ReturnMode};

use list::session_row;
use modal::{FocusMenu, Overlay};

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

/// The outcome of a 切替 reorder (`K` / `J`): moving the selected session one
/// row up or down. Distinct from [`SessionOutcome`] because a successful move is
/// **silent** — reordering is navigation-like and a per-keypress log line would
/// flood the log — and it must **not** re-activate the moved session (the active
/// row is the command target, independent of the cursor). Applied through
/// [`HomeState::apply_reorder`].
#[derive(Debug, Clone)]
pub enum SessionReorder {
    /// The order changed; carries the reloaded sessions to refresh the pane.
    /// [`HomeState::refresh_sessions`] keeps both the cursor and the active row on
    /// their sessions by name, so the cursor follows the moved session to its new
    /// row while the active row stays put.
    Moved(Vec<SessionRecord>),
    /// The selected session was already at the end it was moved toward (or the
    /// root row, which is not reorderable): nothing changed, nothing to apply.
    Stationary,
    /// Persisting the new order failed; carries the error line to log.
    Failed(LogLine),
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

/// The embedded-terminal surface shown in the home screen's right pane: the
/// screen `view` snapshot and the `tabs` strip above it.
///
/// The two are bundled into one owned value so the surface is published and
/// cleared **as a unit**. There is no "clear the tab strip but keep the screen
/// snapshot" path any more — the asymmetry that let a stale view linger after the
/// pane yielded control (it cleared its tabs but left the view for the event
/// loop's next frame to mop up). Exactly one party drives the surface at a time:
/// the event loop while previewing a session in 切替 (Switch), and the
/// embedded-terminal driver while a session is 没入 (Attached). Each clears it via
/// [`HomeState::clear_terminal_surface`] when it yields, so a snapshot never
/// outlives the mode that produced it regardless of *when* control changes hands.
#[derive(Default)]
struct TerminalSurface {
    /// The latest snapshot of the embedded terminal's screen, set while a session
    /// is 没入 (Attached) or previewed in 切替 (Switch) and rendered in the right
    /// pane.
    view: Option<TerminalView>,
    /// The tab strip shown above the embedded terminal: the session's panes and
    /// which one is active. Published alongside the snapshot by whichever party
    /// owns the surface; `None` outside 没入 / a 切替 preview.
    tabs: Option<TabStrip>,
}

/// The most command-history entries kept in memory (and seeded from disk). A
/// long-running session would otherwise grow the recall buffer without bound.
const MAX_COMMAND_HISTORY: usize = 1_000;

/// The most output-log lines kept in memory. The results band shows only the
/// latest command's response, so older lines past this cap are dead weight; the
/// log is otherwise only ever appended to (cleared only by `clear`), so without a
/// cap a long session grows it without bound.
const MAX_LOG_LINES: usize = 5_000;

/// The workspace command line: the editable buffer with its caret, the
/// committed command history, and the recall cursor into that history.
///
/// Extracted from [`HomeState`] so the editing, history-append, and ↑/↓ recall
/// behaviour — together with the invariant that *any* edit cancels an
/// in-progress recall — live on one focused type instead of being maintained by
/// hand at every field access. Both the 切替/在席 command line and the `:`
/// palette drive the same instance.
#[derive(Default)]
struct CommandLine {
    /// The buffer with its caret — drives in-line editing (←/→/Home/End/Del)
    /// and where the caret renders.
    input: TextInput,
    /// Past commands, oldest first; the recall cursor walks this.
    history: Vec<String>,
    /// Index into [`history`](Self::history) while recalling a past command;
    /// `None` when editing a fresh line.
    recall: Option<usize>,
}

impl CommandLine {
    fn new() -> Self {
        Self {
            input: TextInput::new(),
            history: Vec::new(),
            recall: None,
        }
    }

    /// The current buffer contents.
    fn value(&self) -> &str {
        self.input.value()
    }

    /// The caret position as a byte offset into [`value`](Self::value).
    fn cursor(&self) -> usize {
        self.input.cursor()
    }

    /// The committed history, oldest first.
    fn history(&self) -> &[String] {
        &self.history
    }

    /// Replace the history wholesale (e.g. restored from disk), capped to the most
    /// recent [`MAX_COMMAND_HISTORY`] entries so a long-lived on-disk history
    /// never seeds an unbounded in-memory buffer.
    fn set_history(&mut self, mut entries: Vec<String>) {
        let overflow = entries.len().saturating_sub(MAX_COMMAND_HISTORY);
        if overflow > 0 {
            entries.drain(..overflow);
        }
        self.history = entries;
    }

    /// Append a committed command, skipping a consecutive duplicate (standard
    /// shell behaviour) and capping the buffer to [`MAX_COMMAND_HISTORY`] so a
    /// long session cannot grow it without bound. Recall is reset on every submit,
    /// so a front-drain here never strands the recall cursor.
    fn push_history(&mut self, entry: String) {
        if self.history.last() == Some(&entry) {
            return;
        }
        self.history.push(entry);
        let overflow = self.history.len().saturating_sub(MAX_COMMAND_HISTORY);
        if overflow > 0 {
            self.history.drain(..overflow);
        }
    }

    /// Clear the buffer and cancel any in-progress recall.
    fn clear(&mut self) {
        self.input.clear();
        self.recall = None;
    }

    /// Replace the buffer (tab completion); the caller decides whether to also
    /// cancel recall.
    fn set_value(&mut self, value: String) {
        self.input.set_value(value);
    }

    /// Cancel an in-progress recall without touching the buffer.
    fn cancel_recall(&mut self) {
        self.recall = None;
    }

    /// Insert a typed character at the caret, cancelling recall.
    fn push_char(&mut self, c: char) {
        self.input.insert(c);
        self.recall = None;
    }

    /// Delete the character before the caret, cancelling recall.
    fn backspace(&mut self) {
        self.input.backspace();
        self.recall = None;
    }

    /// Delete the character at the caret, cancelling recall.
    fn delete_forward(&mut self) {
        self.input.delete_forward();
        self.recall = None;
    }

    fn cursor_left(&mut self) {
        self.input.move_left();
    }

    fn cursor_right(&mut self) {
        self.input.move_right();
    }

    fn cursor_home(&mut self) {
        self.input.move_home();
    }

    fn cursor_end(&mut self) {
        self.input.move_end();
    }

    /// Recall the previous (older) command into the buffer.
    fn recall_prev(&mut self) {
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
    fn recall_next(&mut self) {
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
}

/// The full state of the home screen.
///
/// Not `Clone`/`Debug`: it owns a [`CommandRegistry`] of trait objects, which
/// are neither. Nothing needs to clone or format the whole screen state.
pub struct HomeState {
    list: WorktreeList,
    mode: Mode,
    /// The workspace command line (buffer, history, and recall cursor). See
    /// [`CommandLine`].
    cmdline: CommandLine,
    log: Vec<LogLine>,
    /// The commands available in command mode (the extension point for the
    /// follow-up command features).
    registry: CommandRegistry,
    /// Which right-pane action surface 在席 (Focus) presents — a pickable menu
    /// or a typed prompt. Injected from the effective settings by `mod.rs`.
    session_action_ui: SessionActionUi,
    /// How the left session sidebar is sized — its full-width list or the
    /// collapsed rail. `Ctrl-B` toggles it; the initial value is injected from
    /// the effective settings by `mod.rs`. Independent of [`mode`](Self::mode),
    /// so zooming between modes never resets it.
    sidebar: Sidebar,
    /// Whether the `ai` command is offered in the 在席 (Focus) menu: true only
    /// when the local LLM is enabled and its model is pulled. Injected from the
    /// effective settings (and a runtime probe) by `mod.rs`; false by default so
    /// the command stays hidden until the model is actually usable.
    ai_available: bool,
    /// The configured agent CLI launched by `agent` with no explicit choice (the
    /// 在席 menu's `agent` row / `a` shortcut and a bare `agent` prompt). Injected
    /// from the effective settings by `mod.rs`; its display name labels the menu's
    /// `agent` row. Defaults to [`AgentCli::Claude`].
    default_agent: AgentCli,
    /// The agent CLIs installed on this machine (PATH-probed), in canonical order,
    /// offered by the 在席 menu's agent picker. Injected by `mod.rs`; empty by
    /// default (tests that do not set it never expand the picker).
    installed_agents: Vec<AgentCli>,
    /// The agent CLI the next agent launch should use, set by the 在席 menu picker
    /// or the `agent <name>` prompt just before launching and consumed by the
    /// terminal-pool wiring on a fresh agent spawn. `None` means "use
    /// [`default_agent`](Self::default_agent)".
    agent_choice: Option<AgentCli>,
    /// Where a 切替 (Switch) returns to on `Esc`; only meaningful in
    /// [`Mode::Switch`].
    switch_return: ReturnMode,
    /// Whether the highlighted session's read-only note overlay is dismissed in
    /// 切替 (Switch). The note auto-shows the moment a session is highlighted;
    /// `Esc` hides it (before a second `Esc` backs out of 切替), and moving the
    /// cursor to another row clears the flag so the next session's note shows.
    /// Only meaningful in [`Mode::Switch`]; the note *editor* is independent of
    /// it (it captures the keyboard through [`overlay`](Self::overlay)).
    note_hidden: bool,
    /// The transient overlay that captures the keyboard while open (the 切替
    /// inline create/rename inputs, the text modal, the right-pane preview, the
    /// session-removal checklist, the note editor). One [`Overlay`] rather than a
    /// field per kind, so at most one can be open by construction and the screen
    /// routes to whichever variant is active. The quit confirmation is tracked
    /// separately in [`quit_confirm`](Self::quit_confirm) because it can overlay
    /// any of these.
    overlay: Overlay,
    /// Whether the quit-confirmation modal is open. Separate from
    /// [`overlay`](Self::overlay): a `Ctrl-C` raises it on top of whatever is
    /// already shown, and cancelling it returns to that overlay rather than
    /// closing it, so the two are independent.
    quit_confirm: bool,
    /// The engagement to persist for restore when the next quit is confirmed,
    /// armed only when the live mode would otherwise be lost. A quit from 没入
    /// (Attached) drops to [`Mode::Focus`] on its way to the quit modal, so the
    /// pane driver arms [`ResumeLevel::Attached`] here before that downgrade; for
    /// 切替 / 在席 the level is read straight off [`mode`](Self::mode) at save time,
    /// so it stays `None`. Cleared when the quit modal is cancelled, so a later
    /// quit from a shallower mode is recorded accurately.
    pending_resume: Option<ResumeLevel>,
    /// Whether a restored 没入 (Attached) engagement should auto-attach the focused
    /// session on the event loop's first pass. Set by
    /// [`restore_focus`](Self::restore_focus) when the recorded engagement was
    /// Attached — it focuses the session synchronously (so the cursor is already on
    /// it), but attaching needs the loop's terminal wiring — and taken once there.
    resume_attach: bool,
    /// Whether the workspace command palette overlay is open. Summoned with `:`
    /// from 切替 (Switch) and 在席 (Focus), it reuses the workspace command-line
    /// state ([`input`](Self::input) / [`recall`](Self::recall) /
    /// [`history`](Self::history) / [`log`](Self::log) /
    /// [`response_start`](Self::response_start)) and floats over the panes while
    /// open. Separate from [`overlay`](Self::overlay) because a text dump (`man`
    /// / `session list`) it runs can layer its modal on top of the palette.
    command_open: bool,
    /// The 在席 (Focus) menu cursor: which Session-scope command is highlighted.
    focus_menu: FocusMenu,
    /// The 在席 (Focus) prompt buffer (the session-scoped command line).
    focus_prompt: TextInput,
    /// Whether 在席's tab selector sits on the trailing "+ new" tab (the action
    /// surface that launches a pane) rather than an existing live pane. The
    /// session's live panes (from the published [`TabStrip`]) form the leading
    /// tabs and the "+ new" tab is appended after them; this flag picks between
    /// "an existing pane is selected" (its preview shows) and "the + new tab is
    /// selected" (the menu / prompt shows). It is forced on whenever the session
    /// has no live panes, so an idle session always shows the action surface.
    focus_new_tab: bool,
    /// Sessions recorded for this workspace (from `state.json`), shown by
    /// `session list` and kept current as sessions are created.
    sessions: Vec<SessionRecord>,
    /// The embedded-terminal surface drawn in the right pane (screen snapshot +
    /// tab strip), published and cleared as a unit. See [`TerminalSurface`].
    terminal: TerminalSurface,
    /// The session activity badge sets read together from the terminal monitor
    /// before each redraw: the worktree paths whose agent is running / waiting /
    /// live / done. Stored as one [`MonitorSnapshot`] and replaced wholesale by
    /// [`apply_badges`](Self::apply_badges), so a frame never mixes one set's
    /// fresh reading with another's stale one. Rendering precedence among them
    /// (done > waiting > running, atop live) lives in the sidebar renderer.
    badges: MonitorSnapshot,
    /// When set, the left pane lists the sessions whose agent is waiting for
    /// input (◆) first, so the next session to touch is at the top. Toggled with
    /// `s` in 切替. The order is a *display* concern only: `sessions` stays in its
    /// canonical (manual `K`/`J`) order, and the waiting-first ordering is applied
    /// when the pane is built ([`rebuild_list`](Self::rebuild_list)) — a stable
    /// partition, so within each group the manual order is preserved and a session
    /// returns to its place once its agent stops waiting.
    sort_waiting: bool,
    /// Index into `log` where the most recent command's response begins. The
    /// command palette (`:`) renders only `log[response_start..]`, so it shows
    /// the response to the latest command and nothing earlier.
    response_start: usize,
    /// The workspace's task issues, loaded from disk by `mod.rs` and read by the
    /// `issue` command. Empty until injected.
    issues: Vec<Issue>,
    /// The latest released version, set once the background update check finds a
    /// release newer than this build. While `None` (the check is pending, or the
    /// build is up to date) the sidebar mascot's "update available" notice is
    /// hidden.
    update: Option<Version>,
    /// The transient "working…" indicator, set while a blocking action runs
    /// (session create / bulk remove / terminal launch). While `Some` the
    /// top-right corner shows the loading rabbit.
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
    /// Whether the sidebar mascot reacts to interaction — injected from the
    /// effective settings by `mod.rs`. While `false` the mascot never blinks and
    /// the Working rabbit never pumps its paw, so it stays a perfectly still
    /// resting image (and [`tick_mascot`](Self::tick_mascot) /
    /// [`kick_mascot_blink`](Self::kick_mascot_blink) become no-ops). On by default.
    mascot_animation_enabled: bool,
    /// When set, the mascot is mid-blink until this instant — the eyes stay shut
    /// while `now` is before it. [`kick_mascot_blink`](Self::kick_mascot_blink)
    /// arms it the moment the user interacts (in 切替 / 在席), and
    /// [`tick_mascot`](Self::tick_mascot) clears it once the instant passes, so the
    /// rabbit blinks back without any idle timer — the blink rides paints that
    /// already happen.
    mascot_blink_deadline: Option<Instant>,
    /// Whether the mascot's eyes are shut on the frame being painted, recomputed
    /// from [`mascot_blink_deadline`](Self::mascot_blink_deadline) by
    /// [`tick_mascot`](Self::tick_mascot) just before each paint, so the renderer
    /// (which has no clock) can read a plain bool.
    mascot_blinking: bool,
    /// A slow pose counter for the 没入 (Attached) Working rabbit's pumping paw,
    /// advanced by [`tick_mascot`](Self::tick_mascot) on each live-loop tick. The
    /// open-eyed moods ignore it (they animate off the blink instead), so it only
    /// matters while a session is live and the loop is already ticking.
    mascot_tick: usize,
    /// The single sink that persists operation-failure error lines to the daily
    /// log file. [`log_error`](Self::log_error) and the failure lines applied from
    /// background tasks / session outcomes flow through it, so an on-screen
    /// operation failure (preview / settings / session action) also lands in
    /// `<data dir>/logs/`. Defaults to a no-op so tests (and any uninjected path)
    /// record nothing; `mod.rs` injects the real
    /// [`FileLogger`](crate::infrastructure::error_log::FileLogger) for the running
    /// screen. Input / usage mistakes (unknown command, `usage: …`) are *not*
    /// routed here — they stay command-log notices, so the file log keeps only
    /// real failures rather than the noise of mistyped commands.
    logger: Box<dyn crate::infrastructure::error_log::Logger>,
    /// The wall-clock instant the current frame renders at, refreshed each paint
    /// by the event loop ([`set_now`](Self::set_now)). The left pane reads it to
    /// turn each session's `updated_at` into a relative "N分前" label. Kept on the
    /// state (rather than threaded through the pure `render_frame`) so the renderer
    /// stays a `&HomeState`-only function and its many test call sites are
    /// unaffected; tests that pin the label set a fixed value with `set_now`.
    now: DateTime<Utc>,
}

/// How long the mascot holds a blink (eyes shut). A touch longer than the
/// loop's `ANIM_TICK`, so the blink spans a couple of paints — long enough to
/// read as a blink, short enough to feel natural — before the eyes reopen.
const MASCOT_BLINK: Duration = Duration::from_millis(180);

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
            mode: Mode::Switch,
            cmdline: CommandLine::new(),
            log,
            registry: CommandRegistry::with_builtins(),
            session_action_ui: SessionActionUi::default(),
            sidebar: Sidebar::default(),
            ai_available: false,
            default_agent: AgentCli::default(),
            installed_agents: Vec::new(),
            agent_choice: None,
            switch_return: ReturnMode::Base,
            note_hidden: false,
            overlay: Overlay::default(),
            quit_confirm: false,
            pending_resume: None,
            resume_attach: false,
            command_open: false,
            focus_menu: FocusMenu::default(),
            focus_prompt: TextInput::new(),
            focus_new_tab: true,
            sessions: Vec::new(),
            terminal: TerminalSurface::default(),
            badges: MonitorSnapshot::default(),
            sort_waiting: false,
            response_start: 0,
            issues: Vec::new(),
            update: None,
            loading: None,
            tasks: Vec::new(),
            root_path: PathBuf::new(),
            // The mascot reacts by default; `mod.rs` overrides it from the
            // effective settings, and tests get a lively mascot without setup.
            mascot_animation_enabled: true,
            mascot_blink_deadline: None,
            mascot_blinking: false,
            mascot_tick: 0,
            logger: Box::new(crate::infrastructure::error_log::NoopLogger),
            now: Utc::now(),
        }
    }

    /// Record the instant the next frame renders at, so the left pane's relative
    /// "N分前" labels track real time. The event loop calls this before each paint;
    /// tests pin it to control the labels.
    pub fn set_now(&mut self, now: DateTime<Utc>) {
        self.now = now;
    }

    /// The instant the current frame renders at (see [`set_now`](Self::set_now)).
    pub fn now(&self) -> DateTime<Utc> {
        self.now
    }

    /// Inject the error sink that persists operation failures to the daily log
    /// file (`mod.rs` passes the real
    /// [`FileLogger`](crate::infrastructure::error_log::FileLogger) at startup).
    /// Without this the screen records nothing — the no-op default — which is what
    /// tests rely on.
    pub fn set_logger(&mut self, logger: Box<dyn crate::infrastructure::error_log::Logger>) {
        self.logger = logger;
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

    /// Set the sidebar's initial state (injected from the effective settings by
    /// `mod.rs` at construction).
    pub fn set_sidebar(&mut self, sidebar: Sidebar) {
        self.sidebar = sidebar;
    }

    /// Enable or disable the sidebar mascot's reactions (injected from the
    /// effective settings by `mod.rs` at construction). Disabling it stops the
    /// blink and the Working paw and clears any blink in flight, so the mascot
    /// immediately settles into a still resting image.
    pub fn set_mascot_animation_enabled(&mut self, enabled: bool) {
        self.mascot_animation_enabled = enabled;
        if !enabled {
            self.mascot_blink_deadline = None;
            self.mascot_blinking = false;
        }
    }

    /// Whether the mascot's eyes are shut on the frame being painted, as last
    /// computed by [`tick_mascot`](Self::tick_mascot). The renderer reads this
    /// rather than a clock.
    pub fn mascot_blinking(&self) -> bool {
        self.mascot_blinking
    }

    /// The mascot's slow pose counter, driving the 没入 Working rabbit's paw.
    pub fn mascot_tick(&self) -> usize {
        self.mascot_tick
    }

    /// Start a blink: shut the mascot's eyes until [`MASCOT_BLINK`] from `now`. The
    /// event loop calls this the moment the user interacts in 切替 / 在席, so the
    /// resting rabbit blinks back. A no-op when the mascot animation is disabled.
    pub fn kick_mascot_blink(&mut self, now: Instant) {
        if self.mascot_animation_enabled {
            self.mascot_blink_deadline = Some(now + MASCOT_BLINK);
        }
    }

    /// Refresh the mascot's animation state for the frame about to be painted:
    /// reopen the eyes once the blink's deadline has passed, and advance the slow
    /// pose counter the Working paw rides. Called once per event-loop iteration
    /// with the loop's `now`. A no-op (and forces the eyes open) when the mascot
    /// animation is disabled, so a toggled-off mascot is perfectly still.
    pub fn tick_mascot(&mut self, now: Instant) {
        if !self.mascot_animation_enabled {
            self.mascot_blinking = false;
            return;
        }
        self.mascot_blinking = match self.mascot_blink_deadline {
            Some(deadline) if now < deadline => true,
            // The blink has run its course (or none is armed): reopen the eyes and
            // drop the spent deadline.
            _ => {
                self.mascot_blink_deadline = None;
                false
            }
        };
        // Bounded so a long-lived session never overflows it; the Working face
        // only reads it modulo a small period.
        self.mascot_tick = self.mascot_tick.wrapping_add(1);
    }

    /// How the left session sidebar is currently sized (full width or the
    /// collapsed rail).
    pub fn sidebar(&self) -> Sidebar {
        self.sidebar
    }

    /// Toggle the left session sidebar between its full width and the collapsed
    /// rail — the `Ctrl-B` action.
    pub fn toggle_sidebar(&mut self) {
        self.sidebar = self.sidebar.toggled();
    }

    /// Set whether the `ai` command is offered in the 在席 (Focus) menu (injected
    /// from the effective settings and a runtime probe by `mod.rs`).
    pub fn set_ai_available(&mut self, available: bool) {
        self.ai_available = available;
    }

    /// Inject the configured default agent CLI (its display name labels the 在席
    /// menu's `agent` row, and a bare `agent` / the `a` shortcut launch it).
    pub fn set_default_agent(&mut self, cli: AgentCli) {
        self.default_agent = cli;
    }

    /// The configured default agent CLI.
    pub fn default_agent(&self) -> AgentCli {
        self.default_agent
    }

    /// Inject the installed agent CLIs (PATH-probed, canonical order) the 在席
    /// menu's agent picker offers.
    pub fn set_installed_agents(&mut self, agents: Vec<AgentCli>) {
        self.installed_agents = agents;
    }

    /// The installed agent CLIs offered by the 在席 menu's agent picker.
    pub fn installed_agents(&self) -> &[AgentCli] {
        &self.installed_agents
    }

    /// Record which agent CLI the next agent launch should use (`None` = the
    /// configured default). Set by the 在席 picker / `agent <name>` just before
    /// launching; consumed by [`take_agent_choice`](Self::take_agent_choice).
    pub fn set_agent_choice(&mut self, cli: Option<AgentCli>) {
        self.agent_choice = cli;
    }

    /// Take the pending agent choice, leaving `None` behind. Returns the CLI the
    /// next agent spawn should launch, or `None` to use the configured default.
    pub fn take_agent_choice(&mut self) -> Option<AgentCli> {
        self.agent_choice.take()
    }

    /// Inject the workspace's task issues (loaded from disk by `mod.rs`), read by
    /// the `issue` command for its list / graph / show views.
    pub fn set_issues(&mut self, issues: Vec<Issue>) {
        self.issues = issues;
    }

    /// Seed the command history with entries restored from disk (oldest first),
    /// so `history` and `↑`/`↓` recall reflect commands run in past sessions.
    pub fn restore_history(&mut self, entries: Vec<String>) {
        self.cmdline.set_history(entries);
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
        self.sessions = sessions;
        self.rebuild_list_keep_cursor();
    }

    /// Rebuild the worktree pane while keeping the cursor, active row, and `Ctrl-^`
    /// jump target on the same sessions by name. Used whenever the rows are rebuilt
    /// under the user (a background re-sync, a manual reorder, the waiting-first
    /// sort toggling on/off, or a session entering/leaving the waiting set) so the
    /// rows can be replaced wholesale without yanking the cursor back to the root.
    fn rebuild_list_keep_cursor(&mut self) {
        let selected = self.list.selected_name().to_string();
        let active = self.list.active_name().to_string();
        // The fresh list drops the `Ctrl-^` jump target, so carry it across the
        // rebuild by name (it is re-validated lazily, so a session that vanished
        // in this sync simply yields no jump).
        let previous = self.list.previous_active_name().map(str::to_string);
        self.rebuild_list();
        // Restore the cursor (`select_by_name` moves both cursor and active onto
        // the row; it is a no-op for the root row / a vanished session, leaving
        // the rebuilt default on the root), then correct the active row.
        self.list.select_by_name(&selected);
        self.list.activate_by_name(&active);
        self.list.set_previous_active(previous);
    }

    /// Rebuild the worktree pane from the current sessions: one row per session
    /// (not per repository). A session spanning several git repositories is
    /// collapsed into a single row by [`session_row`]. The rows follow the session
    /// order from [`display_order`](Self::display_order) — the canonical (manual)
    /// order, or waiting-first when [`sort_waiting`](Self::sort_waiting) is on.
    fn rebuild_list(&mut self) {
        let name = self.list.workspace_name().to_string();
        let order = self.display_order();
        let rows = order
            .iter()
            .map(|&i| session_row(&self.sessions[i]))
            .collect();
        // Carry each session's sidebar label override onto its row so the pane
        // shows the custom display name while commands still key on the branch.
        let labels = order
            .iter()
            .map(|&i| self.sessions[i].display_name.clone())
            .collect();
        // Carry each session's note-presence onto its row so the pane can show a
        // memo marker; the note text itself is read on demand (Switch preview /
        // editor), never stored on the row.
        let notes = order
            .iter()
            .map(|&i| self.sessions[i].note.is_some())
            .collect();
        let mut list = WorktreeList::with_labels(name, rows, labels);
        list.set_notes(notes);
        self.list = list;
    }

    /// The order the sessions are laid out in the left pane, as indices into
    /// `sessions`. Identity (canonical / manual `K`/`J` order) by default; with
    /// [`sort_waiting`](Self::sort_waiting) on, a *stable* partition that lifts the
    /// sessions whose agent is waiting for input (◆) above the rest while keeping
    /// each group in its canonical order.
    fn display_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.sessions.len()).collect();
        if self.sort_waiting {
            // `sort_by_key` is stable, and `false` (waiting) sorts before `true`,
            // so waiting sessions rise to the top without disturbing either group's
            // relative order.
            order.sort_by_key(|&i| !self.badges.waiting.contains(&self.sessions[i].root));
        }
        order
    }

    /// Whether the left pane is lifting the waiting-for-input (◆) sessions to the
    /// top — read by the footer to show the toggle's state.
    pub fn sort_waiting(&self) -> bool {
        self.sort_waiting
    }

    /// Toggle the waiting-first ordering of the left pane (`s` in 切替) and rebuild
    /// the rows, keeping the cursor on the same session by name so it follows its
    /// row to the new position.
    pub fn toggle_sort_waiting(&mut self) {
        self.sort_waiting = !self.sort_waiting;
        self.rebuild_list_keep_cursor();
    }

    pub fn sessions(&self) -> &[SessionRecord] {
        &self.sessions
    }

    /// Open a scrollable text modal showing `lines` under `title` at the given
    /// `size` (used by the text-dumping commands). Replaces any modal already
    /// open.
    pub fn open_text_modal(
        &mut self,
        title: impl Into<String>,
        lines: Vec<LogLine>,
        size: ModalSize,
    ) {
        self.overlay = Overlay::Text(TextModal {
            title: title.into(),
            lines,
            scroll: 0,
            size,
        });
    }

    /// The open text modal, if any.
    pub fn text_modal(&self) -> Option<&TextModal> {
        match &self.overlay {
            Overlay::Text(modal) => Some(modal),
            _ => None,
        }
    }

    /// Close the text modal (the user dismissed it). Called only while the text
    /// modal is the open overlay, so it clears the overlay outright.
    pub fn close_text_modal(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Scroll the text modal up one line (no-op when closed or at the top).
    pub fn text_modal_scroll_up(&mut self) {
        if let Overlay::Text(modal) = &mut self.overlay {
            modal.scroll = modal.scroll.saturating_sub(1);
        }
    }

    /// Scroll the text modal down one line, clamped so the last line stays in
    /// view (no-op when closed). `visible` is the body height the view can show.
    pub fn text_modal_scroll_down(&mut self, visible: usize) {
        if let Overlay::Text(modal) = &mut self.overlay {
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
                self.overlay = Overlay::Preview(Preview {
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
        match &self.overlay {
            Overlay::Preview(preview) => Some(preview),
            _ => None,
        }
    }

    /// Close the right-pane preview (the user dismissed it). Called only while the
    /// preview is the open overlay, so it clears the overlay outright.
    pub fn close_preview(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Scroll the preview up one line (no-op when closed or at the top).
    pub fn preview_scroll_up(&mut self) {
        if let Overlay::Preview(preview) = &mut self.overlay {
            preview.scroll = preview.scroll.saturating_sub(1);
        }
    }

    /// Scroll the preview down one line, clamped so the last line stays in view
    /// (no-op when closed). `visible` is the pane body height the view can show.
    pub fn preview_scroll_down(&mut self, visible: usize) {
        if let Overlay::Preview(preview) = &mut self.overlay {
            let max = preview.lines.len().saturating_sub(visible);
            preview.scroll = (preview.scroll + 1).min(max);
        }
    }

    /// The lines of the most recent command's response (what the command palette
    /// shows): everything in the log from `response_start` onward.
    pub fn response_lines(&self) -> &[LogLine] {
        let start = self.response_start.min(self.log.len());
        &self.log[start..]
    }

    /// Append an ordinary output line to the log (used by the event loop to
    /// report the result of a command's side effect, e.g. `terminal`).
    pub fn log_output(&mut self, text: impl Into<String>) {
        self.log.push(LogLine::output(text));
        self.trim_log();
    }

    /// Drop the oldest log lines once the buffer exceeds [`MAX_LOG_LINES`],
    /// keeping `response_start` pointing at the same response by shifting it down
    /// by however many lines were removed. The cap is far larger than any single
    /// command's response, so the visible results band is never trimmed away.
    fn trim_log(&mut self) {
        let overflow = self.log.len().saturating_sub(MAX_LOG_LINES);
        if overflow > 0 {
            self.log.drain(..overflow);
            self.response_start = self.response_start.saturating_sub(overflow);
        }
    }

    /// Append an error line to the log **and** persist it through the injected
    /// logger — the home screen's single sink for operation failures (preview /
    /// settings save / session actions). The same text shown on screen is written
    /// to the daily log file, so the failure stays inspectable after the screen
    /// closes. Input / usage mistakes (unknown command, `usage: …`) deliberately
    /// do *not* come through here: they are command-result notices appended via
    /// [`record_response`](Self::record_response), so they never reach the file.
    pub fn log_error(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.logger.record(&text);
        self.log.push(LogLine::error(text));
        self.trim_log();
    }

    /// Push `line` to the log, persisting it through the logger when it is an
    /// error. This is the recording path for failures built off the event-loop
    /// thread and applied later: a background task's completion (session create /
    /// remove) and a synchronous session outcome (rename), which construct their
    /// own [`LogLine`] rather than calling [`log_error`](Self::log_error).
    /// Success / output lines are shown only, never recorded.
    fn push_logged_line(&mut self, line: LogLine) {
        if line.kind == LineKind::Error {
            self.logger.record(&line.text);
        }
        self.log.push(line);
        self.trim_log();
    }

    pub fn list(&self) -> &WorktreeList {
        &self.list
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Which command scope the command palette (`:`) operates in: always the
    /// whole workspace, since the palette is workspace-only (the session-scoped
    /// surface lives in the 在席 right pane instead). Completion, hints, and `man`
    /// grouping follow this. The 在席 prompt calls the registry with
    /// [`CommandScope::Session`] directly via [`Self::focus_prompt_hint`] etc.
    pub fn command_scope(&self) -> CommandScope {
        CommandScope::Workspace
    }

    pub fn input(&self) -> &str {
        self.cmdline.value()
    }

    /// The caret position in [`input`](Self::input) as a byte offset, so the
    /// renderer can split the line and draw the caret where editing happens.
    pub fn cursor(&self) -> usize {
        self.cmdline.cursor()
    }

    /// The advisory input hint for the current command input (matching commands,
    /// or the usage of the command being given arguments). Computed on demand
    /// for rendering; see [`CommandRegistry::suggest`].
    pub fn hint(&self) -> Hint {
        self.registry
            .suggest(self.cmdline.value(), self.command_scope())
    }

    pub fn log(&self) -> &[LogLine] {
        &self.log
    }

    /// The current embedded-terminal snapshot, when a session is 没入 (Attached)
    /// or previewed in 切替 (Switch).
    pub fn terminal_view(&self) -> Option<&TerminalView> {
        self.terminal.view.as_ref()
    }

    /// Enter 没入 (Attached): an embedded terminal / agent is going live in the
    /// right pane. The first snapshot arrives via [`set_terminal_view`].
    ///
    /// [`set_terminal_view`]: Self::set_terminal_view
    pub fn show_attached(&mut self) {
        self.mode = Mode::Attached;
    }

    /// Leave 没入 for 在席 (Focus): the embedded session was closed or detached,
    /// so drop the surface and return to the focused session's action surface.
    /// The tab selector lands on the trailing "+ new" tab — the launch surface —
    /// so zooming out with `Ctrl-T` opens the action menu over the live panes
    /// (which still ride the strip), and `Esc` there steps back onto the pane it
    /// was opened over (see [`focus_discard_new_tab`]).
    ///
    /// [`focus_discard_new_tab`]: Self::focus_discard_new_tab
    pub fn leave_attached(&mut self) {
        self.mode = Mode::Focus;
        self.focus_new_tab = true;
        self.clear_terminal_surface();
    }

    /// Publish the tab strip shown above the embedded terminal: the session's
    /// pane `labels` and which one is `active`. Published alongside
    /// [`set_terminal_view`](Self::set_terminal_view) by whichever party owns the
    /// surface (the pane driver in 没入, the event loop's 切替 preview).
    pub fn set_terminal_tabs(&mut self, labels: Vec<String>, active: usize) {
        self.terminal.tabs = Some(TabStrip { labels, active });
    }

    /// The tab strip shown above the embedded terminal, when the surface is live.
    pub fn terminal_tabs(&self) -> Option<&TabStrip> {
        self.terminal.tabs.as_ref()
    }

    /// Store the latest embedded-terminal screen snapshot, shown in the right
    /// pane while the session is 没入 (Attached) or previewed in 切替 (Switch).
    pub fn set_terminal_view(&mut self, view: TerminalView) {
        self.terminal.view = Some(view);
    }

    /// Drop the embedded-terminal surface — both the screen snapshot and the tab
    /// strip — without changing the mode. Whichever party owns the surface calls
    /// this when it yields control (the event loop between frames, the pane driver
    /// when it leaves 没入), so a stale snapshot never lingers in the right pane.
    /// Clearing the two together is the whole point of bundling them: there is no
    /// path that drops one and forgets the other.
    pub fn clear_terminal_surface(&mut self) {
        self.terminal = TerminalSurface::default();
    }

    /// Replace every session activity badge set at once with a fresh reading from
    /// the terminal monitor (running / waiting / live / done). Called before each
    /// redraw by whichever party is driving the screen — the event loop between
    /// frames, the pane driver while a session is 没入. Replacing them as a unit
    /// keeps the four sets consistent with one another (all from the same lock).
    pub fn apply_badges(&mut self, badges: MonitorSnapshot) {
        // With the waiting-first sort on, a session entering or leaving the
        // waiting set changes the row order, so rebuild the pane (keeping the
        // cursor by name). Compared before the move, and only when the *waiting*
        // set actually moved, so the hot per-frame path skips the rebuild whenever
        // nothing relevant changed.
        let resort = self.sort_waiting && self.badges.waiting != badges.waiting;
        self.badges = badges;
        if resort {
            self.rebuild_list_keep_cursor();
        }
    }

    /// The badge sets the last [`apply_badges`](Self::apply_badges) stored, so a
    /// render loop can compare a freshly read snapshot against what it last drew
    /// and skip the repaint when nothing moved — without keeping (and cloning into)
    /// its own copy each frame.
    pub fn badges(&self) -> &MonitorSnapshot {
        &self.badges
    }

    /// Whether the worktree at `path` has a background session actively working a
    /// turn.
    pub fn is_running(&self, path: &Path) -> bool {
        self.badges.running.contains(path)
    }

    /// The set of worktree paths whose agent is actively working a turn, for the
    /// sidebar renderer.
    pub fn running_paths(&self) -> &HashSet<PathBuf> {
        &self.badges.running
    }

    /// Whether the worktree at `path` has a background session waiting for input.
    pub fn is_waiting(&self, path: &Path) -> bool {
        self.badges.waiting.contains(path)
    }

    /// The set of worktree paths whose background session is waiting for input,
    /// for the sidebar renderer.
    pub fn waiting_paths(&self) -> &HashSet<PathBuf> {
        &self.badges.waiting
    }

    /// Whether the worktree at `path` has a live (running) embedded session.
    pub fn is_live(&self, path: &Path) -> bool {
        self.badges.live.contains(path)
    }

    /// The set of worktree paths with a live (running) embedded session, for the
    /// sidebar renderer.
    pub fn live_paths(&self) -> &HashSet<PathBuf> {
        &self.badges.live
    }

    /// Whether the worktree at `path` has a background session whose agent has
    /// finished (a turn completed or it exited).
    pub fn is_done(&self, path: &Path) -> bool {
        self.badges.done.contains(path)
    }

    /// The set of worktree paths whose agent has finished, for the sidebar
    /// renderer.
    pub fn done_paths(&self) -> &HashSet<PathBuf> {
        &self.badges.done
    }

    /// Record the latest released version found by the background update check,
    /// or clear it with `None`. Set before each redraw from the update handle.
    pub fn set_update(&mut self, latest: Option<Version>) {
        self.update = latest;
    }

    /// The latest released version, when it is newer than this build — the
    /// sidebar mascot speaks the "update available" notice only while this is
    /// `Some`.
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
        self.push_logged_line(line);
        if let Some(sessions) = sessions {
            self.refresh_sessions(sessions);
        }
    }

    /// How many sessions currently have a live (running) embedded shell/agent.
    /// Shown in the quit-confirmation modal so the user sees what is at stake.
    pub fn live_count(&self) -> usize {
        self.badges.live.len()
    }

    /// Whether any session has a live (running) embedded shell/agent — the
    /// condition that makes `Ctrl-C` ask for confirmation before quitting.
    pub fn has_live_sessions(&self) -> bool {
        !self.badges.live.is_empty()
    }

    /// Whether the quit-confirmation modal is open.
    pub fn quit_confirm(&self) -> bool {
        self.quit_confirm
    }

    /// Open the quit-confirmation modal (a live session is still running). It
    /// overlays whatever is already shown rather than replacing it.
    pub fn open_quit_confirm(&mut self) {
        self.quit_confirm = true;
    }

    /// Dismiss the quit-confirmation modal without quitting, returning to
    /// whatever overlay it was raised over. Also drops any armed
    /// [`ResumeLevel`] (e.g. the 没入 arm from a cancelled `Ctrl-Q`), so a later
    /// quit from a shallower mode is recorded at its actual depth rather than
    /// inheriting the stale one.
    pub fn cancel_quit_confirm(&mut self) {
        self.quit_confirm = false;
        self.pending_resume = None;
    }

    /// Arm [`ResumeLevel::Attached`] to be persisted when the next quit is
    /// confirmed. Called by the pane driver when `Ctrl-Q` leaves 没入, before the
    /// mode drops to [`Mode::Focus`] on the way to the quit modal — otherwise the
    /// recorded engagement would lose that the user was attached.
    pub fn arm_resume_attached(&mut self) {
        self.pending_resume = Some(ResumeLevel::Attached);
    }

    /// The engagement to persist for restore, consuming any arm. An armed level
    /// (a 没入 quit) wins; otherwise it is read off the current [`mode`](Self::mode)
    /// — 切替 → [`ResumeLevel::Switch`], 在席 → [`ResumeLevel::Focus`]. The live
    /// event loop never observes [`Mode::Attached`] (the pane driver arms instead),
    /// so that arm maps to Focus as a defensive fallback.
    pub fn resume_level(&mut self) -> ResumeLevel {
        self.pending_resume.take().unwrap_or(match self.mode {
            Mode::Switch => ResumeLevel::Switch,
            Mode::Focus | Mode::Attached => ResumeLevel::Focus,
        })
    }

    /// Restore the engagement recorded at the last quit: move the cursor to
    /// `session` (切替), focus it (在席), or focus it and arm an auto-attach (没入).
    /// A no-op when the session no longer exists (it was removed since), so a
    /// stale snapshot never strands the cursor on a missing row. Called at startup
    /// after the panes are restored, so a 没入 target's pane is already live for
    /// the event loop's first-pass attach.
    pub fn restore_focus(&mut self, session: &str, level: ResumeLevel) {
        match level {
            ResumeLevel::Switch => {
                // Move the 切替 cursor onto the session (root stays at the default
                // cursor, which `select_by_name` leaves put by not matching it).
                self.list.select_by_name(session);
            }
            ResumeLevel::Focus => {
                self.enter_focus_named(session);
            }
            ResumeLevel::Attached => {
                if self.enter_focus_named(session) {
                    self.resume_attach = true;
                }
            }
        }
    }

    /// Whether a restored 没入 engagement should auto-attach the focused session,
    /// consuming the flag set by [`restore_focus`](Self::restore_focus). `false`
    /// once consumed (or when the restored engagement was not Attached), so the
    /// event loop attaches at most once on its first pass.
    pub fn take_resume_attach(&mut self) -> bool {
        std::mem::take(&mut self.resume_attach)
    }

    /// Focus the session at `row` (0 is the root row, `i` maps to worktree
    /// `i - 1`) in the list, so the embedded terminal re-roots there.
    pub fn focus_session(&mut self, row: usize) {
        self.list.focus_index(row);
    }

    // --- command palette (`:`) ---------------------------------------------

    /// Open the workspace command palette overlay (`:`), clearing any half-typed
    /// command line so it starts fresh. The palette reuses the workspace
    /// command-line state ([`input`](Self::input) / [`recall`](Self::recall)),
    /// floating over the current 切替 / 在席 panes while open.
    pub fn open_command_palette(&mut self) {
        self.command_open = true;
        self.cmdline.clear();
    }

    /// Close the command palette overlay (`Esc`), clearing its command line.
    pub fn close_command_palette(&mut self) {
        self.command_open = false;
        self.cmdline.clear();
    }

    /// Whether the workspace command palette overlay is open.
    pub fn command_palette_open(&self) -> bool {
        self.command_open
    }

    // --- 切替 (Switch) -----------------------------------------------------

    /// Enter 切替 (Switch): move keyboard focus to the left pane to pick a
    /// session, remembering where to return on `Esc`.
    pub fn enter_switch(&mut self, return_to: ReturnMode) {
        self.mode = Mode::Switch;
        self.switch_return = return_to;
        self.overlay.clear_create();
        // A fresh 切替 shows the highlighted session's note (any prior dismissal
        // belonged to the previous visit).
        self.note_hidden = false;
    }

    /// Where the current 切替 returns to on `Esc`.
    pub fn switch_return(&self) -> ReturnMode {
        self.switch_return
    }

    /// Move the Switch cursor up one row, wrapping (delegates to the list).
    pub fn switch_move_up(&mut self) {
        self.list.move_up();
        // The cursor now sits on a different session, so re-show its note even if
        // the previous row's note was dismissed.
        self.note_hidden = false;
    }

    /// Move the Switch cursor down one row, wrapping (delegates to the list).
    pub fn switch_move_down(&mut self) {
        self.list.move_down();
        self.note_hidden = false;
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
        self.overlay = Overlay::Create(CreateInput::new(taken));
    }

    /// Whether an inline create input is open in 切替.
    pub fn is_creating(&self) -> bool {
        matches!(self.overlay, Overlay::Create(_))
    }

    /// The inline create input, when open — its typed name, caret, and live
    /// validation error are read through it ([`CreateInput`]).
    pub fn create(&self) -> Option<&CreateInput> {
        match &self.overlay {
            Overlay::Create(input) => Some(input),
            _ => None,
        }
    }

    /// The inline create input for editing, when open: the event loop routes the
    /// 切替 keys to its own methods ([`CreateInput::push_char`] etc.).
    pub fn create_mut(&mut self) -> Option<&mut CreateInput> {
        match &mut self.overlay {
            Overlay::Create(input) => Some(input),
            _ => None,
        }
    }

    /// Cancel inline creation, staying in 切替.
    pub fn create_cancel(&mut self) {
        self.overlay.clear_create();
    }

    /// Validate and accept the inline create name. On success the input closes
    /// and the trimmed name is returned (for the event loop to create the
    /// session); on an invalid name the input stays open with the inline error
    /// shown live and `None` is returned (see [`CreateInput::confirm`]). A no-op
    /// (returning `None`) when not creating.
    pub fn switch_confirm_create(&mut self) -> Option<String> {
        let Overlay::Create(input) = &mut self.overlay else {
            return None;
        };
        // An invalid name keeps the input open (with its live error); only a
        // valid one closes it.
        let name = input.confirm()?;
        self.overlay = Overlay::None;
        Some(name)
    }

    /// Begin inline rename of the selected session's sidebar label in 切替: open
    /// an input pre-filled with its current label that captures the mode's keys
    /// until confirmed (Enter) or cancelled (Esc). A no-op on the root row (which
    /// is not a session and has no label to change) and when an input is already
    /// open. Returns whether the input opened.
    pub fn switch_begin_rename(&mut self) -> bool {
        if matches!(self.overlay, Overlay::Create(_) | Overlay::Rename(_)) {
            return false;
        }
        let Some(worktree) = self.list.selected() else {
            return false;
        };
        let target = worktree_name(worktree).to_string();
        // Pre-fill with the label currently shown so the user edits rather than
        // retypes; an unset override pre-fills with the session name.
        let label = self
            .list
            .display_label(self.list.selected_index() - 1)
            .to_string();
        self.overlay = Overlay::Rename(RenameInput::new(target, label));
        true
    }

    /// Whether an inline rename input is open in 切替.
    pub fn is_renaming(&self) -> bool {
        matches!(self.overlay, Overlay::Rename(_))
    }

    /// The inline rename input, when open — its target session and typed label
    /// are read through it ([`RenameInput`]).
    pub fn rename(&self) -> Option<&RenameInput> {
        match &self.overlay {
            Overlay::Rename(input) => Some(input),
            _ => None,
        }
    }

    /// The inline rename input for editing, when open: the event loop routes the
    /// 切替 keys to its own methods ([`RenameInput::push_char`] etc.).
    pub fn rename_mut(&mut self) -> Option<&mut RenameInput> {
        match &mut self.overlay {
            Overlay::Rename(input) => Some(input),
            _ => None,
        }
    }

    /// Cancel inline renaming, staying in 切替. Called only while the rename input
    /// is the open overlay, so it clears the overlay outright.
    pub fn rename_cancel(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Accept the inline rename: close the input and return the target session
    /// name together with the typed label (trimmed), for the event loop to
    /// persist (see [`RenameInput::confirm`]). A no-op (returning `None`) when
    /// not renaming.
    pub fn switch_confirm_rename(&mut self) -> Option<(String, String)> {
        match std::mem::take(&mut self.overlay) {
            Overlay::Rename(input) => Some(input.confirm()),
            // Not renaming: leave whatever was open (if anything) untouched.
            other => {
                self.overlay = other;
                None
            }
        }
    }

    // --- session note editor ----------------------------------------------

    /// The note recorded for the session named `name`, if any. Looked up in the
    /// recorded sessions (the sidebar list carries only the worktree rows), so
    /// the editor opens pre-filled with what is on disk.
    fn session_note(&self, name: &str) -> Option<&str> {
        self.sessions
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.note())
    }

    /// The note of the session highlighted in 切替 (the cursor row), or `None` on
    /// the root row (which is the workspace, not a session) and for a session
    /// with no note. Read by the right-pane renderer so the highlighted session's
    /// note (its next-time TODO) shows the moment it is selected — without opening
    /// the editor.
    pub fn selected_session_note(&self) -> Option<&str> {
        let worktree = self.list.selected()?;
        self.session_note(worktree_name(worktree))
    }

    /// The highlighted session's read-only note when its overlay is currently
    /// shown in 切替 (Switch), else `None`: it shows when the cursor is on a
    /// session that has a note, it has not been dismissed with `Esc`, and no note
    /// *editor* is open (the editor takes over the overlay). The right-pane
    /// renderer draws the note exactly when this is `Some` — so its absence is a
    /// genuine path, not a dead branch behind a separate predicate.
    pub fn visible_switch_note(&self) -> Option<&str> {
        if self.mode != Mode::Switch || self.note_hidden || matches!(self.overlay, Overlay::Note(_))
        {
            return None;
        }
        self.selected_session_note()
    }

    /// Whether the highlighted session's read-only note overlay is shown in 切替
    /// (Switch) — see [`visible_switch_note`](Self::visible_switch_note). Read by
    /// the event loop and the footer to decide whether `Esc` first hides the note
    /// or backs out of 切替.
    pub fn switch_note_visible(&self) -> bool {
        self.visible_switch_note().is_some()
    }

    /// Dismiss the highlighted session's read-only note overlay in 切替 (Switch)
    /// (the first `Esc`). Moving the cursor to another row re-shows it.
    pub fn hide_switch_note(&mut self) {
        self.note_hidden = true;
    }

    /// Open the note editor for `target`, pre-filled with its current note.
    /// `reattach` records whether closing it should re-attach the session's pane
    /// (没入's `Ctrl-E`); `false` for 切替's `n`.
    fn open_note_for(&mut self, target: String, reattach: bool) {
        let initial = self.session_note(&target).unwrap_or_default().to_string();
        self.overlay = Overlay::Note(NoteEditor::new(target, &initial, reattach));
    }

    /// Begin editing the selected session's note in 切替 (Switch): open the note
    /// editor pre-filled with its current note. A no-op on the root row (which is
    /// the workspace, not a session) and when an editor is already open. Returns
    /// whether the editor opened.
    pub fn switch_begin_note(&mut self) -> bool {
        if matches!(self.overlay, Overlay::Note(_)) {
            return false;
        }
        let Some(worktree) = self.list.selected() else {
            return false;
        };
        let target = worktree_name(worktree).to_string();
        self.open_note_for(target, false);
        true
    }

    /// Open the note editor for the focused (active) session — the `Ctrl-E` action
    /// in 在席 (Focus) and 没入 (Attached). `reattach` records whether closing the
    /// editor should re-attach the session's pane: `true` from 没入 (drop back into
    /// the live terminal), `false` from 在席 (return to the action surface). A
    /// no-op on the root row (the workspace, not a session) and when an editor is
    /// already open. Returns whether the editor opened.
    pub fn open_focused_note(&mut self, reattach: bool) -> bool {
        if matches!(self.overlay, Overlay::Note(_)) {
            return false;
        }
        let name = self.focused_session_name();
        if name == ROOT_NAME {
            return false;
        }
        self.open_note_for(name, reattach);
        true
    }

    /// The open note editor, when any — its target, text buffer, and caret are
    /// read through it ([`NoteEditor`]).
    pub fn note_editor(&self) -> Option<&NoteEditor> {
        match &self.overlay {
            Overlay::Note(editor) => Some(editor),
            _ => None,
        }
    }

    /// The open note editor for editing, when any: the event loop routes its keys
    /// to the buffer's own methods (via [`NoteEditor::area_mut`]).
    pub fn note_editor_mut(&mut self) -> Option<&mut NoteEditor> {
        match &mut self.overlay {
            Overlay::Note(editor) => Some(editor),
            _ => None,
        }
    }

    /// Whether closing the open note editor should re-attach the session's pane
    /// (it was opened from 没入). `false` when no editor is open.
    pub fn note_editor_reattaches(&self) -> bool {
        self.note_editor().is_some_and(NoteEditor::reattach)
    }

    /// Cancel the note editor, discarding the edits. Called only while the note
    /// editor is the open overlay, so it clears the overlay outright.
    pub fn note_editor_cancel(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Accept the note edit: close the editor and return the target session, the
    /// typed text, and whether to re-attach, for the event loop to persist (see
    /// [`NoteEditor::confirm`]). A no-op (returning `None`) when not editing.
    pub fn confirm_note_editor(&mut self) -> Option<(String, String, bool)> {
        match std::mem::take(&mut self.overlay) {
            Overlay::Note(editor) => Some(editor.confirm()),
            // Not editing: leave whatever was open (if anything) untouched.
            other => {
                self.overlay = other;
                None
            }
        }
    }

    // --- 在席 (Focus) ------------------------------------------------------

    /// Enter 在席 (Focus) on the session at `row` (0 is the root row): make it the
    /// active and selected row, switch to the right-pane action surface, and reset
    /// the menu cursor and prompt buffer.
    pub fn enter_focus(&mut self, row: usize) {
        self.list.focus_index(row);
        self.list.activate_selected();
        self.enter_focus_surface();
    }

    /// Switch into 在席 (Focus) on the already-positioned session: enter the mode
    /// and reset the right-pane action surface (close any inline create input,
    /// reset the menu cursor and prompt, land on the "+ new" tab). The cursor must
    /// already point at the target session; [`enter_focus`](Self::enter_focus) and
    /// [`enter_focus_named`](Self::enter_focus_named) differ only in how they get
    /// there.
    fn enter_focus_surface(&mut self) {
        self.mode = Mode::Focus;
        self.overlay.clear_create();
        self.focus_menu.reset();
        self.focus_prompt.clear();
        self.focus_new_tab = true;
    }

    /// Enter 在席 (Focus) on the session named `name`, returning whether one
    /// matched. Like [`enter_focus`](Self::enter_focus) but addressing the session
    /// by branch rather than row, so a freshly created session can be focused
    /// against the just-refreshed list without computing its row. A no-op
    /// (returning `false`, leaving the mode untouched) when no session matches.
    pub fn enter_focus_named(&mut self, name: &str) -> bool {
        if !self.list.select_by_name(name) {
            return false;
        }
        self.enter_focus_surface();
        true
    }

    /// The row the previously focused session now sits at — the target `Ctrl-^`
    /// jumps to (vim's `Ctrl-^` / tmux's `last-window`) — or `None` when no other
    /// session has been focused yet or the previous one has since been removed.
    /// Delegates to the list, which records it whenever [`enter_focus`] moves the
    /// active row to a different session.
    pub fn previous_session_row(&self) -> Option<usize> {
        self.list.previous_row()
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

    /// Leave 在席 for the base 切替 (Switch) — the default mode.
    pub fn leave_focus(&mut self) {
        self.enter_switch(ReturnMode::Base);
    }

    /// The Session-scope commands the 在席 menu lists, in registry order
    /// (`terminal`, `agent`, `ai`). The `ai` command is filtered out unless the
    /// local LLM is usable (enabled and its model pulled), so it only appears
    /// when running it would actually work. `close` is filtered out on the root
    /// row, which belongs to no session and so cannot be closed.
    ///
    /// Resolved for the **active** row: 在席 acts on the session it focused.
    pub fn focus_menu_commands(&self) -> Vec<CommandInfo> {
        self.menu_commands_for_root(self.list.root_active())
    }

    /// The same Session-scope command list as [`focus_menu_commands`], but
    /// resolved for the row under the **cursor** rather than the active row. The
    /// 切替 (Switch) preview shows what *selecting* the highlighted row reveals,
    /// so its `close` visibility must follow that row — otherwise a session row
    /// previewed while the root row is active would hide `close` (and vice
    /// versa), showing the active row's menu instead of the highlighted one's.
    pub fn preview_menu_commands(&self) -> Vec<CommandInfo> {
        self.menu_commands_for_root(self.list.root_selected())
    }

    /// Shared body of [`focus_menu_commands`] / [`preview_menu_commands`]: the
    /// Session-scope commands, with `ai` gated on local-LLM availability and
    /// `close` hidden when `root` (the row belongs to no session).
    fn menu_commands_for_root(&self, root: bool) -> Vec<CommandInfo> {
        self.registry
            .commands_in_scope(CommandScope::Session)
            .into_iter()
            .filter(|info| info.name != "ai" || self.ai_available)
            .filter(|info| info.name != "close" || !root)
            .collect()
    }

    /// How many live panes the focused session publishes (the leading 在席 tabs),
    /// from the surface's tab strip — `0` when none are live (an idle session).
    fn focus_pane_count(&self) -> usize {
        self.terminal.tabs.as_ref().map_or(0, |t| t.labels.len())
    }

    /// The active pane index the focused session's tab strip publishes (`0` when
    /// no panes are live). The pane preview shows this pane, so the tab selector
    /// rides it rather than tracking a duplicate index of its own.
    fn focus_active_pane(&self) -> usize {
        self.terminal.tabs.as_ref().map_or(0, |t| t.active)
    }

    /// Whether 在席's tab selector is on the trailing "+ new" tab — the action
    /// surface (menu / prompt) that launches a pane — rather than an existing
    /// live pane. Always true when the session has no live panes, since the
    /// "+ new" tab is then the only one.
    pub fn focus_on_new_tab(&self) -> bool {
        self.focus_new_tab || self.focus_pane_count() == 0
    }

    /// Move 在席's tab selector to the next tab, wrapping through the live panes
    /// and the trailing "+ new" tab (`[pane 0 … pane n-1, + new]`). Returns the
    /// pane index to make active (for the caller to apply to the terminal pool) on
    /// landing on a pane tab, or `None` when it lands on the "+ new" tab (or the
    /// session has no panes, leaving the selector on "+ new").
    pub fn focus_tab_next(&mut self) -> Option<usize> {
        let panes = self.focus_pane_count();
        if panes == 0 {
            self.focus_new_tab = true;
            return None;
        }
        if self.focus_on_new_tab() {
            // "+ new" wraps to the first pane.
            self.focus_new_tab = false;
            Some(0)
        } else if self.focus_active_pane() + 1 >= panes {
            // The last pane steps onto the "+ new" tab.
            self.focus_new_tab = true;
            None
        } else {
            Some(self.focus_active_pane() + 1)
        }
    }

    /// Move 在席's tab selector to the previous tab, wrapping through the live
    /// panes and the trailing "+ new" tab (the mirror of [`focus_tab_next`]).
    /// Returns the pane index to make active on landing on a pane tab, or `None`
    /// when it lands on the "+ new" tab.
    ///
    /// [`focus_tab_next`]: Self::focus_tab_next
    pub fn focus_tab_prev(&mut self) -> Option<usize> {
        let panes = self.focus_pane_count();
        if panes == 0 {
            self.focus_new_tab = true;
            return None;
        }
        if self.focus_on_new_tab() {
            // "+ new" wraps back to the last pane.
            self.focus_new_tab = false;
            Some(panes - 1)
        } else if self.focus_active_pane() == 0 {
            // The first pane steps back onto the "+ new" tab.
            self.focus_new_tab = true;
            None
        } else {
            Some(self.focus_active_pane() - 1)
        }
    }

    /// Discard 在席's "+ new" launch surface when it sits over live panes — the
    /// state after zooming out with `Ctrl-T` (or navigating onto "+ new") — by
    /// stepping the selector back onto the active pane's tab, so that pane
    /// previews again. Returns whether it moved: `false` (a no-op) when "+ new"
    /// is the only tab (an idle session, nothing to step back to), leaving the
    /// caller to back out of 在席 instead.
    pub fn focus_discard_new_tab(&mut self) -> bool {
        if self.focus_on_new_tab() && self.focus_pane_count() > 0 {
            self.focus_new_tab = false;
            true
        } else {
            false
        }
    }

    /// The 在席 menu cursor (which Session-scope command is highlighted).
    pub fn focus_menu_cursor(&self) -> usize {
        self.focus_menu.cursor()
    }

    /// Whether the 在席 menu's `agent` row is expanded into the agent picker.
    pub fn focus_menu_expanded(&self) -> bool {
        self.focus_menu.is_expanded()
    }

    /// The highlighted agent in the 在席 menu's agent picker, or `None` when the
    /// picker is collapsed (or there are no installed agents to pick from).
    pub fn focus_menu_agent_cursor(&self) -> Option<usize> {
        self.focus_menu
            .agent_cursor()
            .filter(|_| !self.installed_agents.is_empty())
    }

    /// Whether the 在席 menu's `agent` row can expand into the picker: the cursor
    /// is on `agent` and more than one CLI is installed (so there is a choice).
    pub fn focus_menu_agent_can_expand(&self) -> bool {
        self.installed_agents.len() > 1
            && self
                .focus_selected_command()
                .is_some_and(|info| info.name == "agent")
    }

    /// Expand the 在席 menu's agent picker, highlighting the configured default
    /// agent's position in the installed list (or the top when it is not
    /// installed). No-op unless [`focus_menu_agent_can_expand`] holds.
    ///
    /// [`focus_menu_agent_can_expand`]: Self::focus_menu_agent_can_expand
    pub fn focus_menu_expand_agent(&mut self) {
        if !self.focus_menu_agent_can_expand() {
            return;
        }
        let default_index = self
            .installed_agents
            .iter()
            .position(|&cli| cli == self.default_agent)
            .unwrap_or(0);
        self.focus_menu.expand(default_index);
    }

    /// Collapse the 在席 menu's agent picker, returning whether it was expanded
    /// (so the caller treats `←` / `Esc` as consumed only then).
    pub fn focus_menu_collapse_agent(&mut self) -> bool {
        self.focus_menu.collapse()
    }

    /// The agent CLI highlighted in the picker, or `None` when collapsed / there
    /// are none installed. Used to launch the chosen CLI on `Enter`.
    pub fn focus_menu_selected_agent(&self) -> Option<AgentCli> {
        self.focus_menu.agent_cursor()?;
        self.installed_agents
            .get(self.focus_menu.agent_selected(self.installed_agents.len()))
            .copied()
    }

    /// Move the 在席 menu cursor up one row, wrapping (delegated to [`FocusMenu`],
    /// which keeps it underflow-safe). Acts on the agent picker while expanded.
    pub fn focus_menu_move_up(&mut self) {
        let count = self.focus_menu_nav_count();
        self.focus_menu.move_up(count);
    }

    /// Move the 在席 menu cursor down one row, wrapping (delegated to [`FocusMenu`]).
    pub fn focus_menu_move_down(&mut self) {
        let count = self.focus_menu_nav_count();
        self.focus_menu.move_down(count);
    }

    /// The row count the menu cursor wraps against: the installed agents while the
    /// picker is expanded, otherwise the Session-scope commands.
    fn focus_menu_nav_count(&self) -> usize {
        if self.focus_menu.is_expanded() {
            self.installed_agents.len()
        } else {
            self.focus_menu_commands().len()
        }
    }

    /// The 在席 command under the menu cursor, clamped to the available commands,
    /// or `None` when no Session-scope command is available.
    ///
    /// `FocusMenu::selected` clamps to `len - 1`, which is `0` for an empty list
    /// — so indexing directly would panic if the registry ever yielded no
    /// Session-scope commands. Returning `Option` keeps the caller a no-op in
    /// that case instead of crashing (and unwinding) the TUI.
    pub fn focus_selected_command(&self) -> Option<CommandInfo> {
        let commands = self.focus_menu_commands();
        commands
            .get(self.focus_menu.selected(commands.len()))
            .copied()
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

    /// The 在席 prompt's editable buffer: the event loop routes its keys straight
    /// to the [`TextInput`]'s own editing methods (`insert` / `backspace` /
    /// `move_left` …), so the prompt has no per-key forwarders of its own.
    pub fn focus_prompt_mut(&mut self) -> &mut TextInput {
        &mut self.focus_prompt
    }

    /// Tab-complete the 在席 prompt's command word against the Session-scope
    /// commands, returning the candidates when ambiguous (so the caller can log
    /// them, mirroring the palette line's `complete`).
    pub fn focus_prompt_complete(&mut self) -> Completion {
        let completion = self
            .registry
            .complete(self.focus_prompt.value(), CommandScope::Session);
        self.focus_prompt.set_value(completion.input.clone());
        if !completion.candidates.is_empty() {
            self.log
                .push(LogLine::output(completion.candidates.join("  ")));
        }
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
        // Mark where this response begins (before any lines it appends), exactly
        // as the command palette line does, so the palette shows only this
        // command's response — the prompt has no echo line, so the response
        // starts at the current log end.
        self.response_start = self.log.len();
        let result = self.dispatch_and_record(&entry, CommandScope::Session);
        let effect = self.record_response(result);
        Submission {
            effect,
            recorded: Some(entry),
        }
    }

    /// Insert a typed character at the caret (command palette line), advancing it.
    pub fn push_char(&mut self, c: char) {
        self.cmdline.push_char(c);
    }

    /// Delete the character before the caret (command mode), moving it back.
    pub fn backspace(&mut self) {
        self.cmdline.backspace();
    }

    /// Delete the character at the caret (the `Del`/forward-delete key), leaving
    /// the caret in place.
    pub fn delete_forward(&mut self) {
        self.cmdline.delete_forward();
    }

    /// Move the caret one character left.
    pub fn cursor_left(&mut self) {
        self.cmdline.cursor_left();
    }

    /// Move the caret one character right.
    pub fn cursor_right(&mut self) {
        self.cmdline.cursor_right();
    }

    /// Move the caret to the start of the line.
    pub fn cursor_home(&mut self) {
        self.cmdline.cursor_home();
    }

    /// Move the caret to the end of the line.
    pub fn cursor_end(&mut self) {
        self.cmdline.cursor_end();
    }

    /// Tab-complete the command word, listing candidates when ambiguous.
    pub fn complete(&mut self) {
        let session_names: Vec<&str> = self.sessions.iter().map(|s| s.name.as_str()).collect();
        let completion =
            self.registry
                .complete_with(self.cmdline.value(), self.command_scope(), &session_names);
        self.cmdline.set_value(completion.input);
        if !completion.candidates.is_empty() {
            self.log
                .push(LogLine::output(completion.candidates.join("  ")));
        }
        self.cmdline.cancel_recall();
    }

    /// Recall the previous (older) command into the input.
    pub fn recall_prev(&mut self) {
        self.cmdline.recall_prev();
    }

    /// Recall the next (newer) command, returning to an empty line past the end.
    pub fn recall_next(&mut self) {
        self.cmdline.recall_next();
    }

    /// Run the current input as a command: echo it, dispatch it, record it in
    /// history, and apply the resulting log lines and side effect. Returns a
    /// [`Submission`] carrying the side effect (so the event loop can act on
    /// `Quit`) and the recorded command (so it can be persisted). Empty input is
    /// a no-op.
    pub fn submit(&mut self) -> Submission {
        let entry = self.cmdline.value().trim().to_string();
        self.cmdline.clear();
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
        self.trim_log();
        let result = self.dispatch_and_record(&entry, self.command_scope());
        let effect = self.record_response(result);
        Submission {
            effect,
            recorded: Some(entry),
        }
    }

    /// Dispatch `entry` as a `scope`-scoped command and record it in command
    /// history, returning the raw result. The shared core of [`submit`](Self::submit)
    /// (palette line, [`CommandScope::Workspace`]) and
    /// [`focus_prompt_submit`](Self::focus_prompt_submit) (在席 prompt,
    /// [`CommandScope::Session`]) so both record history identically and refuse
    /// commands outside their surface's scope; folding the result into the log is
    /// [`record_response`](Self::record_response).
    fn dispatch_and_record(&mut self, entry: &str, scope: CommandScope) -> CommandResult {
        let result = self.registry.dispatch_in_scope(
            entry,
            scope,
            self.cmdline.history(),
            &self.list.refs(),
            &self.issues,
        );
        self.cmdline.push_history(entry.to_string());
        result
    }

    /// Fold a command `result` into the log and advance the results-band start,
    /// returning the side effect for the caller to act on. Shared by both command
    /// surfaces so they reflect a result identically:
    ///
    /// - `Clear` empties the log (and resets the band);
    /// - `EnterSwitch` / `Activate` append nothing (the event loop owns those
    ///   mode transitions);
    /// - a text dump (`man` / `history`) opens a scrollable modal and leaves the
    ///   band empty;
    /// - everything else appends its lines to the log.
    fn record_response(&mut self, result: CommandResult) -> Effect {
        match result.effect {
            Effect::Clear => {
                self.log.clear();
                self.response_start = 0;
            }
            Effect::EnterSwitch | Effect::Activate(_) => {}
            Effect::ShowText { title, size } => {
                self.open_text_modal(title, result.lines, size);
                self.response_start = self.log.len();
            }
            _ => {
                self.log.extend(result.lines);
                self.trim_log();
            }
        }
        result.effect
    }

    /// Apply the result of a session-creation attempt: log its line and, when
    /// creation refreshed the worktree list, swap it in.
    pub fn apply_session_outcome(&mut self, outcome: SessionOutcome) {
        self.push_logged_line(outcome.line);
        if let Some(sessions) = outcome.sessions {
            self.sessions = sessions;
            self.rebuild_list();
            if let Some(name) = outcome.select {
                self.list.select_by_name(&name);
            }
        }
    }

    /// Apply a [`SessionReorder`] from `K` / `J`: refresh the pane from the
    /// reloaded sessions on a move (the cursor follows the moved session to its
    /// new row, the active row stays put — see [`refresh_sessions`](Self::refresh_sessions)),
    /// do nothing at an edge / on the root row, and log a failure. Kept separate
    /// from [`apply_session_outcome`](Self::apply_session_outcome) so a move is
    /// silent and never re-activates the moved session.
    pub fn apply_reorder(&mut self, outcome: SessionReorder) {
        match outcome {
            SessionReorder::Moved(sessions) => self.refresh_sessions(sessions),
            SessionReorder::Stationary => {}
            SessionReorder::Failed(line) => self.push_logged_line(line),
        }
    }

    /// The open session-removal modal, if any — its names, cursor, and checked
    /// rows are read and navigated through it ([`RemoveModal`]).
    pub fn remove_modal(&self) -> Option<&RemoveModal> {
        match &self.overlay {
            Overlay::Remove(modal) => Some(modal),
            _ => None,
        }
    }

    /// The open session-removal modal for navigation, if any: the event loop
    /// routes its keys to the modal's own methods ([`RemoveModal::move_up`] etc.).
    pub fn remove_modal_mut(&mut self) -> Option<&mut RemoveModal> {
        match &mut self.overlay {
            Overlay::Remove(modal) => Some(modal),
            _ => None,
        }
    }

    /// Open the session-removal modal, seeded with the current session names and
    /// nothing selected. `force` is carried from `session remove --force`.
    pub fn open_remove_modal(&mut self, force: bool) {
        let names = self.sessions.iter().map(|s| s.name.clone()).collect();
        self.overlay = Overlay::Remove(RemoveModal::new(names, force));
    }

    /// Close the removal modal, discarding any selection. Called only while the
    /// removal modal is the open overlay, so it clears the overlay outright.
    pub fn cancel_remove_modal(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Confirm the removal modal: close it and return the checked session names
    /// (in display order) together with the `--force` flag, for the event loop
    /// to remove each (see [`RemoveModal::confirm`]). Returns `None` when nothing
    /// is checked, leaving the modal open; also `None` when it is closed.
    pub fn submit_remove_modal(&mut self) -> Option<(Vec<String>, bool)> {
        let Overlay::Remove(modal) = &self.overlay else {
            return None;
        };
        // Nothing checked keeps the modal open; only a non-empty selection closes it.
        let result = modal.confirm()?;
        self.overlay = Overlay::None;
        Some(result)
    }
}

#[cfg(test)]
mod tests;
