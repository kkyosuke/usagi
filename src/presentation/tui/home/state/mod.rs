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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::domain::history::HistoryEntry;
use crate::domain::issue::Issue;
use crate::domain::resource::ResourceUsage;
use crate::domain::settings::{AgentCli, KeyScheme, SessionActionUi, SessionLabelMaster, Sidebar};
use crate::domain::version::Version;
use crate::domain::workspace_state::{SessionAgent, SessionRecord, WorktreeState};

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

pub use list::{worktree_name, WorkspaceGroup, WorktreeList, ROOT_NAME};
pub use log::{LineKind, LogLine};
pub use modal::{
    CreateInput, DiffView, EnvEditor, ModalSize, NoteEditor, Preview, RemoveEntry, RemoveModal,
    RenameInput, TabMenu, TabMenuItem, TabRenameInput, TextModal,
};
pub use mode::{Mode, PaneExit, ResumeLevel};

use list::{session_row, session_tree_layout};
use modal::{CloseupMenu, CloseupSubmenu, Overlay};

/// The terminal row's inline choices, in display/default order. `open` preserves
/// the existing fast path: add an embedded usagi pane/tab. `new` opens a native
/// terminal application in the same directory.
const TERMINAL_MENU_ACTIONS: [&str; 2] = ["open", "new"];

use crate::presentation::tui::chat::state::Chat;

/// The 集中 (Closeup) menu commands in alphabetical order by name, independent of
/// registry order, so the menu is predictable regardless of how the registry is
/// built.
fn sorted_session_menu_commands(registry: &CommandRegistry) -> Vec<CommandInfo> {
    let mut commands = registry.commands_in_scope(CommandScope::Session);
    commands.sort_by(|a, b| a.name.cmp(b.name));
    commands
}

/// One additional workspace shown below the primary in 統合(unite) mode — its
/// name, root directory, root-row note, and recorded sessions. Unlike the primary
/// workspace (whose sessions live in [`HomeState::sessions`] and are re-synced
/// live), these are seeded by the orchestrator and refreshed when a `session`
/// command targets this workspace. Collapsed into a [`WorkspaceGroup`] on every
/// [`rebuild_list`](HomeState::rebuild_list).
#[derive(Debug, Clone)]
pub struct GroupSource {
    /// The workspace's display name (its sidebar header).
    pub name: String,
    /// The workspace root directory — the `⌂ root` row's working dir, and the
    /// target `session` commands run against when the cursor is in this group.
    pub root_path: PathBuf,
    /// The workspace root's free-form note (the `⌂ root` row's memo).
    pub root_note: Option<String>,
    /// The workspace's recorded sessions (from its `state.json`).
    pub sessions: Vec<SessionRecord>,
    /// The workspace's task issues (from its `.usagi/issues/`), so the `issue`
    /// command lists / shows the issues of whichever group the cursor is in.
    /// Loaded once when the group is built (like [`sessions`](Self::sessions)),
    /// never re-synced.
    pub issues: Vec<Issue>,
}

/// The outcome of submitting the command line: the side effect to act on, plus
/// the command that was recorded in history (so the event loop can persist it).
#[derive(Debug)]
pub struct Submission {
    pub effect: Effect,
    /// The history entry that was run and added to history, or `None` for empty
    /// input. Carries the command, its outcome, and the session it targeted so
    /// the event loop can persist the whole entry (not just the command text).
    pub recorded: Option<HistoryEntry>,
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
    /// The workspace root's note as it stands after this action, when the action
    /// may have changed it (editing the `⌂ root` row's memo). `None` leaves the
    /// in-memory root note untouched; `Some(value)` replaces it, where the inner
    /// `None` means the note was cleared. Only the root-note save sets this — every
    /// session-scoped action leaves it `None`.
    pub root_note: Option<Option<String>>,
}

/// The outcome of a 選択 reorder (`K` / `J`): moving the selected session one
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

/// A transient "working…" indicator shown while a blocking action runs
/// (creating or bulk-removing sessions, launching a terminal / agent). It
/// carries a `label` describing the action and a `frame` tick that advances on
/// each step, so painting it repeatedly animates the chosen loader. Read by the
/// renderer through [`HomeState::loading`].
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

/// A pane being added to the focused session whose tab is loading in the strip.
/// The selected tab moves to this launch immediately — even before the pool has a
/// real pane — and the event loop polls it each frame until its shell paints,
/// then attaches it (没入). While the pending tab is selected the right-pane body
/// shows the same launch rabbits, so the tab bar and the tab contents describe
/// the same in-flight pane. User input no longer cancels the move: adding a tab
/// commits to that tab at dispatch time, and readiness only swaps the loading
/// body for the live terminal.
/// It advances through two phases the event loop drives (see
/// [`HomeState::advance_pending_pane`]):
///
/// - **Resolving** — the launch's per-worktree environment (`op://` secrets) is
///   being resolved on a background thread, so no pool pane exists yet. A
///   placeholder chip (`placeholder`) is shown at the strip's end and animated,
///   the same way the `+ new` chip is a synthetic, pool-less tab.
/// - **Starting** — the environment arrived and the pane was spawned into the
///   pool; `placeholder` is cleared and `tab` tracks the real chip until the
///   shell paints and the loop moves to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPane {
    /// The session (worktree) the pane is being opened in.
    dir: PathBuf,
    /// The animation tick for the loading chip, advanced on each poll.
    frame: usize,
    /// The chip's current tab index, refreshed each poll (a concurrent close can
    /// shift it). `None` until the first poll resolves it — the renderer only
    /// animates a chip once its index is known.
    tab: Option<usize>,
    /// The synthetic chip's label while resolving (before a pool pane exists);
    /// `None` once the pane is spawned (Starting phase reads its label from the
    /// pool tab strip instead).
    placeholder: Option<String>,
}

impl PendingPane {
    /// The session the loading pane belongs to.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The placeholder chip label to draw at the strip's end while the launch's
    /// environment is still resolving (no pool pane yet); `None` once the pane has
    /// spawned and carries its own tab label.
    pub fn placeholder(&self) -> Option<&str> {
        self.placeholder.as_deref()
    }

    /// The loading animation tick, advanced on each poll — drives the launch
    /// rabbits floated over the selected loading tab's body.
    pub fn frame(&self) -> usize {
        self.frame
    }
}

/// Which background lifecycle operation a sidebar skeleton is visualising.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSessionKind {
    /// A session is being created; its row does not exist yet, so the skeleton is
    /// inserted above the workspace's persistent "+ new session" row.
    Create,
    /// A session is being removed; its row still exists until the worker
    /// finishes, so the skeleton replaces that existing row in place.
    Remove,
}

/// A session lifecycle operation currently visualised by an inline sidebar
/// skeleton.
///
/// Creating and removing a session shell out to git (worktree add / submodule
/// init / worktree remove) on a worker thread, so the sidebar keeps the
/// operation visible where the row belongs while the worker runs. Creates insert
/// a placeholder above the target workspace's "+ new session" row; removals
/// replace the existing session row until the refreshed list lands (or the
/// failure clears the placeholder). Tracked per `(kind, root, name)` so
/// concurrent workspace operations stay routed to the row they affect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSession {
    /// The lifecycle operation being visualised.
    kind: PendingSessionKind,
    /// The workspace root the session operation targets.
    root: PathBuf,
    /// The session (branch) name being created or removed — shown inside the
    /// skeleton.
    name: String,
}

impl PendingSession {
    /// The lifecycle operation being visualised.
    pub fn kind(&self) -> PendingSessionKind {
        self.kind
    }

    /// Whether this pending row is a create placeholder.
    pub fn is_create(&self) -> bool {
        self.kind == PendingSessionKind::Create
    }

    /// Whether this pending row is a remove placeholder.
    pub fn is_remove(&self) -> bool {
        self.kind == PendingSessionKind::Remove
    }

    /// The workspace root the pending session lands in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The session name being created.
    pub fn name(&self) -> &str {
        &self.name
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
/// the event loop while previewing a session in 選択 (Overview) / 集中 (Closeup), and
/// the embedded-terminal driver while a session is 没入 (Attached).
///
/// Which party that is is no longer a bare convention: the surface can only be
/// written through a [`SurfaceWriter`] obtained from
/// [`HomeState::surface_writer`], keyed to the [`SurfaceOwner`] that claimed it.
/// [`claim`](Self::claim) drops whatever a *different* owner left behind, so a
/// stale snapshot can never outlive the hand-off regardless of *when* — or
/// whether — the previous owner remembered to clear on yield.
#[derive(Default)]
struct TerminalSurface {
    /// Which party last published the surface, or `None` when it is empty. The
    /// gate [`claim`](Self::claim) checks to decide whether the incoming owner is
    /// taking over from the other party (drop its snapshot) or refreshing its own.
    owner: Option<SurfaceOwner>,
    /// The latest snapshot of the embedded terminal's screen, set while a session
    /// is 没入 (Attached) or previewed in 選択 (Overview) and rendered in the right
    /// pane.
    view: Option<TerminalView>,
    /// The tab strip shown above the embedded terminal: the session's panes and
    /// which one is active. Published alongside the snapshot by whichever party
    /// owns the surface; `None` outside 没入 / a 選択 preview.
    tabs: Option<TabStrip>,
    /// While the active tab is still launching, the tab strip is already
    /// published but the pane has no stable screen to preview yet. This frame
    /// drives the right-pane body loader for that selected loading tab.
    loading_body_frame: Option<usize>,
}

impl TerminalSurface {
    /// Claim the surface for `owner`, discarding any snapshot a *different* owner
    /// left behind so the two never mix. A re-claim by the current owner keeps
    /// what it already published — the event loop re-deriving its own preview, or
    /// the pane driver refreshing its own live screen — so the hot per-frame path
    /// does not churn the snapshot it is about to overwrite anyway.
    fn claim(&mut self, owner: SurfaceOwner) {
        if self.owner != Some(owner) {
            self.view = None;
            self.tabs = None;
            self.loading_body_frame = None;
            self.owner = Some(owner);
        }
    }
}

/// Which party is currently publishing the embedded-terminal surface (the
/// right-pane screen snapshot + tab strip). Exactly one drives it at a time — the
/// home event loop while it previews the highlighted / focused session (選択 /
/// 集中), and the embedded-terminal driver while a session is 没入 (Attached) — and
/// [`HomeState::surface_writer`] is the only way to publish to it. Naming the
/// owner at the write is what makes the single-owner rule enforced rather than
/// merely documented: taking over from the other party drops its leftovers (see
/// [`TerminalSurface::claim`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SurfaceOwner {
    /// The home event loop, previewing the highlighted (選択) / focused (集中)
    /// session's live terminal in the right pane.
    Preview,
    /// The embedded-terminal driver, while a session is 没入 (Attached).
    Attached,
}

/// The sole handle for publishing the embedded-terminal surface, tied to the
/// [`SurfaceOwner`] that claimed it via [`HomeState::surface_writer`]. Holding it
/// is what enforces single-owner publishing: a party cannot set the screen
/// snapshot or the tab strip without first naming itself the owner, and doing so
/// drops a *different* owner's snapshot up front — so no stale frame survives the
/// hand-off between 選択 / 集中 previews and 没入.
pub struct SurfaceWriter<'a> {
    surface: &'a mut TerminalSurface,
}

impl SurfaceWriter<'_> {
    /// Publish the latest embedded-terminal screen snapshot, shown in the right
    /// pane while the session is 没入 (Attached) or previewed in 選択 (Overview) /
    /// 集中 (Closeup).
    pub fn set_view(&mut self, view: TerminalView) {
        self.surface.view = Some(view);
    }

    /// Publish the tab strip shown above the embedded terminal: the session's
    /// pane `labels` and which one is `active`.
    pub fn set_tabs(&mut self, labels: Vec<String>, active: usize) {
        self.surface.tabs = Some(TabStrip { labels, active });
    }

    /// Mark the active tab's body as launching for this frame. The tab strip
    /// still identifies the selected tab; the pane body renders a loader instead
    /// of stale terminal output until the launch is ready to attach.
    pub fn set_loading_body(&mut self, frame: usize) {
        self.surface.loading_body_frame = Some(frame);
    }
}

/// The sole handle for replacing the activity badge snapshot
/// (running / waiting / live / done), tied to the [`SurfaceOwner`] that claimed
/// it via [`HomeState::badge_writer`]. The snapshot itself is already an atomic
/// value (all four sets are read under one monitor lock and replaced together);
/// this handle adds the missing ownership boundary, so the event loop and the
/// embedded-terminal driver cannot both update badge state without explicitly
/// naming who is driving the screen at that moment.
pub struct BadgeWriter<'a> {
    state: &'a mut HomeState,
}

impl BadgeWriter<'_> {
    /// Replace every session activity badge set at once with a fresh reading
    /// from the terminal monitor.
    pub fn apply(&mut self, badges: MonitorSnapshot) {
        self.state.replace_badges(badges);
    }
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
/// hand at every field access. Both the 選択/集中 command line and the `:`
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
            None => self.history.len().saturating_sub(1),
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

/// The session roots whose activity changed between two monitor snapshots: any
/// path that entered or left the running / waiting / done / live set. Used to
/// bump the freshness ("heat") dot of only the sessions that actually did
/// something between frames — the symmetric difference is set-membership only, so
/// a quiet frame yields an empty set and no work.
fn changed_roots(old: &MonitorSnapshot, new: &MonitorSnapshot) -> HashSet<PathBuf> {
    let mut changed = HashSet::new();
    for (a, b) in [
        (&old.running, &new.running),
        (&old.waiting, &new.waiting),
        (&old.done, &new.done),
        (&old.live, &new.live),
    ] {
        changed.extend(a.symmetric_difference(b).cloned());
    }
    changed
}

/// The full state of the home screen.
///
/// Not `Clone`/`Debug`: it owns a [`CommandRegistry`] of trait objects, which
/// are neither. Nothing needs to clone or format the whole screen state.
pub struct HomeState {
    list: WorktreeList,
    mode: Mode,
    /// Whether the current Closeup sub-state is a live embedded pane. This used
    /// to be a third top-level mode; now it only affects Closeup rendering while
    /// the pane driver owns input.
    closeup_attached: bool,
    /// The workspace command line (buffer, history, and recall cursor). See
    /// [`CommandLine`].
    cmdline: CommandLine,
    log: Vec<LogLine>,
    /// The commands available in command mode (the extension point for the
    /// follow-up command features).
    registry: CommandRegistry,
    /// Sorted Session-scope commands used by the 集中 menu. This static part is
    /// derived once from [`registry`](Self::registry); each render then only
    /// applies the dynamic gates (`close`, `agent`) instead of cloning and
    /// sorting the registry metadata again.
    session_menu_commands: Vec<CommandInfo>,
    /// Which right-pane action surface 集中 (Closeup) presents — a pickable menu
    /// or a typed prompt. Injected from the effective settings by `mod.rs`.
    session_action_ui: SessionActionUi,
    /// How the left session sidebar is sized — its full-width list or the
    /// collapsed rail. `Ctrl-B` toggles it; the initial value is injected from
    /// the effective settings by `mod.rs`. Independent of [`mode`](Self::mode),
    /// so zooming between modes never resets it.
    sidebar: Sidebar,
    /// The effective manual-status label master 選択 (Overview) cycles with `Tab` /
    /// the digit keys and the sidebar resolves each session's
    /// [`label_id`](crate::domain::workspace_state::SessionRecord::label_id)
    /// against. Injected from the effective settings by `mod.rs`; defaults to the
    /// generic built-in set (see [`SessionLabelMaster`]). Empty leaves the feature
    /// dormant — `Tab` is a no-op and no label column is drawn.
    label_master: SessionLabelMaster,
    /// How the embedded terminal (没入) reserves its navigation keys — a `Ctrl-O`
    /// prefix or single `Alt`-chords — so the rest reach the shell / agent.
    /// Injected from the effective settings by `mod.rs` and re-read when the
    /// config screen closes; read by the pane input loop ([`super::pane_input`]).
    key_scheme: KeyScheme,
    /// Whether a `Ctrl-O` leader press is awaiting its action key right now
    /// (prefix [`KeyScheme`] only), so the 没入 footer can show it is waiting. Set
    /// by the pane drive loop ([`super::terminal::pane`]) as the leader is pressed
    /// and as it lapses ([`super::pane_input::PREFIX_TIMEOUT`]); read by the
    /// footer ([`super::ui`]). Always `false` outside 没入 / the prefix scheme.
    prefix_pending: bool,
    /// The configured agent CLI launched by `agent` with no explicit choice (the
    /// 集中 menu's `agent` row / `a` shortcut and a bare `agent` prompt). Injected
    /// from the effective settings by `mod.rs`; its display name labels the menu's
    /// `agent` row. Defaults to [`AgentCli::Claude`].
    default_agent: AgentCli,
    /// The configured local-LLM model name (Ollama) the 集中 `chat` overlay talks
    /// to. Injected from the effective settings by `mod.rs`; defaults to
    /// [`DEFAULT_LOCAL_LLM_MODEL`](crate::domain::settings::DEFAULT_LOCAL_LLM_MODEL).
    local_llm_model: String,
    /// Whether the local LLM is usable (enabled and its model pulled) — gates the
    /// `chat` row in the 集中 (Closeup) menu so it only appears when a reply would
    /// actually work. Injected from the effective settings (and a runtime probe)
    /// by `mod.rs`; false by default.
    ai_available: bool,
    /// The agent CLIs installed on this machine (PATH-probed), in canonical order,
    /// offered by the 集中 menu's agent picker. Injected by `mod.rs`; empty by
    /// default (tests that do not set it never expand the picker).
    installed_agents: Vec<AgentCli>,
    /// The agent CLI the next agent launch should use, set by the 集中 menu picker
    /// or the `agent <name>` prompt just before launching and consumed by the
    /// terminal-pool wiring on a fresh agent spawn. `None` means "use
    /// [`default_agent`](Self::default_agent)".
    agent_choice: Option<AgentCli>,
    /// The opening prompt the next configured-agent launch should deliver,
    /// captured from `ai <prompt>` and consumed by the terminal-pool wiring. It is
    /// separate from [`agent_choice`](Self::agent_choice) because `ai` always uses
    /// the configured default CLI rather than an ad-hoc override.
    agent_initial_prompt: Option<String>,
    /// The worktree (by index in [`list`](Self::list)'s worktrees) whose PR hover
    /// popup is pinned open, or `None` when none is. Set by clicking a session's PR
    /// badge (in any mode, on the full sidebar) and held open across pointer moves —
    /// unlike a hover tooltip — so the pointer can travel into the box to click a
    /// `#<number>`; cleared by a click outside it, a keypress, or `Esc`. The
    /// renderer floats the session's `#<number>` list beside its row.
    pr_popup: Option<usize>,
    /// The transient overlay that captures the keyboard while open (the 選択
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
    /// Whether the update-confirmation modal is open. Raised by clicking the
    /// sidebar mascot while it is announcing an available update (see
    /// [`update`](Self::update)); confirming it runs the self-update. Like
    /// [`quit_confirm`](Self::quit_confirm) it is a full-screen modal tracked as a
    /// flag rather than an [`Overlay`], and the two never coexist (the mascot is
    /// not clickable while the quit modal is up).
    update_confirm: bool,
    /// The engagement to persist for restore when the next quit is confirmed,
    /// armed only when the live mode would otherwise be lost. A quit from 没入
    /// (Attached) drops to [`Mode::Closeup`] on its way to the quit modal, so the
    /// pane driver arms [`ResumeLevel::Attached`] here before that downgrade; for
    /// 選択 / 集中 the level is read straight off [`mode`](Self::mode) at save time,
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
    /// from 選択 (Overview) and 集中 (Closeup), it reuses the workspace command-line
    /// state ([`input`](Self::input) / [`recall`](Self::recall) /
    /// [`history`](Self::history) / [`log`](Self::log) /
    /// [`response_start`](Self::response_start)) and floats over the panes while
    /// open. Separate from [`overlay`](Self::overlay) because a text dump (`man`
    /// / `session list`) it runs can layer its modal on top of the palette.
    command_open: bool,
    /// The 集中 (Closeup) menu cursor: which Session-scope command is highlighted.
    closeup_menu: CloseupMenu,
    /// The 集中 (Closeup) menu's live filter (`/`): `None` when the menu lists every
    /// command, `Some(query)` while the user narrows the list by typing. An empty
    /// `Some` is filter mode with nothing typed yet (every command still shows), so
    /// key routing knows to treat letters as filter input rather than shortcuts.
    closeup_menu_filter: Option<String>,
    /// The 集中 (Closeup) prompt buffer (the session-scoped command line).
    closeup_prompt: TextInput,
    /// Whether 集中's tab selector sits on the trailing "+ new" tab (the action
    /// surface that launches a pane) rather than an existing live pane. The
    /// session's live panes (from the published [`TabStrip`]) form the leading
    /// tabs and the "+ new" tab is appended after them; this flag picks between
    /// "an existing pane is selected" (its preview shows) and "the + new tab is
    /// selected" (the menu / prompt shows). It is forced on whenever the session
    /// has no live panes, so an idle session always shows the action surface.
    closeup_new_tab: bool,
    /// Whether the 集中 (Closeup) action surface (Menu or Prompt) floats over the
    /// *selected pane tab* rather than the trailing "+ new" tab — the state after
    /// zooming out of a live pane (`Ctrl-T` / `Ctrl-O a`): the selector stays on
    /// the pane the zoom left, its live preview keeps showing behind the floating
    /// box, and the strip never grows a "+ new" chip for a tab that was never
    /// created. Dropped when the surface is dismissed (`Esc`), when the tab
    /// selector moves (the user is browsing previews), or when the mode changes.
    closeup_action_over_pane: bool,
    /// A one-shot arming bit: 集中 (Closeup) was reached by zooming *out* of a live
    /// pane with `Ctrl-T` / `Ctrl-O a` (`PaneExit::ToFocus`), so the very next
    /// `Esc` re-attaches that pane — returning to the 没入 (Attached) tab the zoom
    /// started from rather than peeling back toward 選択. Armed in that zoom-out
    /// path and cleared the moment any other key is handled (or the mode changes),
    /// so it only ever turns one immediate `Esc` into a return-to-pane.
    closeup_return_attach: bool,
    /// Sessions recorded for this workspace (from `state.json`), shown by
    /// `session list` and kept current as sessions are created.
    sessions: Vec<SessionRecord>,
    /// The embedded-terminal surface drawn in the right pane (screen snapshot +
    /// tab strip), published and cleared as a unit. See [`TerminalSurface`].
    terminal: TerminalSurface,
    /// The session activity badge sets read together from the terminal monitor
    /// before each redraw: the worktree paths whose agent is running / waiting /
    /// live / done. Stored as one [`MonitorSnapshot`] and replaced wholesale by a
    /// [`BadgeWriter`] claimed through [`badge_writer`](Self::badge_writer), so a
    /// frame never mixes one set's fresh reading with another's stale one *and*
    /// the screen driver updating it is explicit. Rendering precedence among
    /// them (done > waiting > running, atop live) lives in the sidebar renderer.
    badges: MonitorSnapshot,
    /// Which screen driver last replaced [`badges`](Self::badges). The value is
    /// not needed to merge the snapshot (replacement is atomic), but recording it
    /// in the same owner vocabulary as [`terminal`](Self::terminal) makes badge
    /// writes go through the same single-owner gate instead of two call sites
    /// mutating screen state by convention.
    badge_owner: Option<SurfaceOwner>,
    /// When set, the left pane lists the sessions whose agent is waiting for
    /// input (◆) first, so the next session to touch is at the top. Toggled with
    /// `s` in 選択. The order is a *display* concern only: `sessions` stays in its
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
    /// The recorded command history (oldest first), seeded from disk by `mod.rs`
    /// and extended as commands run. Read by the `history` command for its
    /// time-stamped, per-session listing; the ↑/↓ recall uses the separate
    /// command-string buffer on [`CommandLine`].
    history_entries: Vec<HistoryEntry>,
    /// The latest released version, set once the background update check finds a
    /// release newer than this build. While `None` (the check is pending, or the
    /// build is up to date) the sidebar mascot's "update available" notice is
    /// hidden.
    update: Option<Version>,
    /// The transient "working…" indicator, set while a blocking action runs
    /// (session create / bulk remove / terminal launch). While `Some` the right
    /// pane shows the launch loader.
    loading: Option<LoadingIndicator>,
    /// A pane launched in the background (集中's `terminal` / `agent` on a session
    /// that already shows tabs) whose tab is loading in the strip. While `Some`
    /// the event loop polls it each frame, animating its chip and — once ready —
    /// moving to it unless the user acted meanwhile. See [`PendingPane`].
    pending_pane: Option<PendingPane>,
    /// Session lifecycle operations currently shown as animated sidebar
    /// skeletons: creates inserted above the target workspace's "+ new session"
    /// row, removals replacing the target session row until the worker reports
    /// success or failure.
    pending_sessions: Vec<PendingSession>,
    /// The rows of the background-task board (session create / remove running
    /// off the event-loop thread), refreshed each frame from the shared task
    /// handle. The renderer may filter rows by kind when a task has its own
    /// inline affordance; the mascot still speaks the leading task label.
    tasks: Vec<TaskRow>,
    /// The workspace root's free-form note (the `⌂ root` row's memo), loaded from
    /// `state.json` at startup and updated in place when the user edits it. The
    /// sidebar reads it for the root row's memo marker; the 選択 preview and the
    /// note editor read it the way they read a session's [`SessionRecord::note`].
    /// Only the user editing it changes it — background re-syncs leave it as is —
    /// so it is carried separately from the re-synced `sessions`.
    root_note: Option<String>,
    /// The *additional* workspace groups shown below the primary one in 統合(unite)
    /// mode — empty in single-workspace mode. The primary workspace (its sessions,
    /// root path, and root note) stays in [`sessions`](Self::sessions) /
    /// [`root_path`](Self::root_path) / [`root_note`](Self::root_note) and is
    /// re-synced live; these extra groups are display snapshots appended after the
    /// primary on every [`rebuild_list`](Self::rebuild_list). Built by the
    /// orchestrator from the other selected workspaces' preloads via
    /// [`set_extra_groups`](Self::set_extra_groups).
    extra_groups: Vec<GroupSource>,
    /// The workspace root the most recently dispatched `session` operation
    /// (create / remove / rename / note) acts on, so its async (or sync) result is
    /// applied back to the right 統合(unite) group rather than always the primary.
    /// `None` (or the primary's root) routes to [`sessions`](Self::sessions);
    /// otherwise it matches an [`extra_groups`](Self::extra_groups) entry. Set by
    /// the handlers from the cursor's group just before dispatching and cleared as
    /// the result lands.
    op_target: Option<PathBuf>,
    /// The workspace names the user has folded shut in 統合(unite) mode, by name so
    /// the folds survive a list rebuild (a background re-sync drops and rebuilds the
    /// groups). Re-applied to the fresh list on every
    /// [`rebuild_list`](Self::rebuild_list); empty in single-workspace mode (the
    /// fold toggle is only exposed with several workspaces). In-memory only — folds
    /// reset when usagi restarts.
    collapsed_workspaces: HashSet<String>,
    /// Whether the sidebar mascot reacts to interaction — injected from the
    /// effective settings by `mod.rs`. While `false` the mascot never blinks and
    /// the Working rabbit never pumps its paw, so it stays a perfectly still
    /// resting image (and [`tick_mascot`](Self::tick_mascot) /
    /// [`kick_mascot_blink`](Self::kick_mascot_blink) become no-ops). On by default.
    mascot_animation_enabled: bool,
    /// When set, the mascot is mid-blink until this instant — the eyes stay shut
    /// while `now` is before it. [`kick_mascot_blink`](Self::kick_mascot_blink)
    /// arms it the moment the user interacts (in 選択 / 集中), and
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
    /// The one-shot reaction the mascot is playing after being clicked, or `None`
    /// while it rests. [`kick_mascot_reaction`](Self::kick_mascot_reaction) sets it
    /// (picking one pseudo-randomly), the renderer reads it to draw the burst, and
    /// [`tick_mascot`](Self::tick_mascot) clears it once
    /// [`mascot_reaction_deadline`](Self::mascot_reaction_deadline) passes.
    mascot_reaction: Option<crate::presentation::tui::widgets::MascotReaction>,
    /// When set, the click reaction plays until this instant, then
    /// [`tick_mascot`](Self::tick_mascot) drops it back to rest — the same
    /// deadline-on-the-clock shape as the blink.
    mascot_reaction_deadline: Option<Instant>,
    /// The value of [`mascot_tick`](Self::mascot_tick) when the current reaction
    /// began, so [`mascot_reaction_phase`](Self::mascot_reaction_phase) yields a
    /// from-zero sub-frame counter for the reaction's animation.
    mascot_reaction_start_tick: usize,
    /// A tiny linear-congruential state advanced on each click to pick the next
    /// reaction. Deterministic (seeded at zero) so it is unit-testable, while still
    /// cycling through the reactions in a varied, shuffled-feeling order.
    mascot_reaction_rng: u32,
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
    /// turn each session's `updated_at` into a relative "Nm ago" label. Kept on the
    /// state (rather than threaded through the pure `render_frame`) so the renderer
    /// stays a `&HomeState`-only function and its many test call sites are
    /// unaffected; tests that pin the label set a fixed value with `set_now`.
    now: DateTime<Utc>,
}

/// How long the mascot holds a blink (eyes shut). A touch longer than the
/// loop's `ANIM_TICK`, so the blink spans a couple of paints — long enough to
/// read as a blink, short enough to feel natural — before the eyes reopen.
const MASCOT_BLINK: Duration = Duration::from_millis(180);

/// How long a click reaction plays before the mascot settles back to rest. Spans
/// several of the loop's `ANIM_TICK`s, so the reaction's little animation has room
/// to cycle a few frames — long enough to read as a playful burst, short enough to
/// stay out of the way.
const MASCOT_REACTION: Duration = Duration::from_millis(660);

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
        let registry = CommandRegistry::with_builtins();
        let session_menu_commands = sorted_session_menu_commands(&registry);
        Self {
            list: WorktreeList::new(workspace_name, worktrees),
            mode: Mode::Switch,
            closeup_attached: false,
            cmdline: CommandLine::new(),
            log,
            registry,
            session_menu_commands,
            session_action_ui: SessionActionUi::default(),
            sidebar: Sidebar::default(),
            label_master: SessionLabelMaster::default(),
            key_scheme: KeyScheme::default(),
            prefix_pending: false,
            default_agent: AgentCli::default(),
            local_llm_model: crate::domain::settings::DEFAULT_LOCAL_LLM_MODEL.to_string(),
            ai_available: false,
            installed_agents: Vec::new(),
            agent_choice: None,
            agent_initial_prompt: None,
            pr_popup: None,
            overlay: Overlay::default(),
            quit_confirm: false,
            update_confirm: false,
            pending_resume: None,
            resume_attach: false,
            command_open: false,
            closeup_menu: CloseupMenu::default(),
            closeup_menu_filter: None,
            closeup_prompt: TextInput::new(),
            closeup_new_tab: true,
            closeup_action_over_pane: false,
            closeup_return_attach: false,
            sessions: Vec::new(),
            terminal: TerminalSurface::default(),
            badges: MonitorSnapshot::default(),
            badge_owner: None,
            sort_waiting: false,
            response_start: 0,
            issues: Vec::new(),
            history_entries: Vec::new(),
            update: None,
            loading: None,
            pending_pane: None,
            pending_sessions: Vec::new(),
            tasks: Vec::new(),
            root_note: None,
            extra_groups: Vec::new(),
            op_target: None,
            collapsed_workspaces: HashSet::new(),
            // The mascot reacts by default; `mod.rs` overrides it from the
            // effective settings, and tests get a lively mascot without setup.
            mascot_animation_enabled: true,
            mascot_blink_deadline: None,
            mascot_blinking: false,
            mascot_tick: 0,
            mascot_reaction: None,
            mascot_reaction_deadline: None,
            mascot_reaction_start_tick: 0,
            mascot_reaction_rng: 0,
            logger: Box::new(crate::infrastructure::error_log::NoopLogger),
            now: Utc::now(),
        }
    }

    /// Record the instant the next frame renders at, so the left pane's relative
    /// "Nm ago" labels track real time. The event loop calls this before each paint;
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
    /// own. Injected by `mod.rs` at construction. The value now lives on the
    /// primary [`WorkspaceGroup`] value type, so this delegates to the list's first
    /// group (and [`rebuild_list`](Self::rebuild_list) carries it across rebuilds).
    pub fn set_root_path(&mut self, root: impl Into<PathBuf>) {
        self.list.set_root_path(root);
    }

    /// The workspace root path the root row operates in (see [`set_root_path`]),
    /// read from the primary [`WorkspaceGroup`] value type.
    ///
    /// [`set_root_path`]: Self::set_root_path
    pub fn root_path(&self) -> &Path {
        self.list.root_path()
    }

    /// Set which right-pane action surface 集中 (Closeup) presents (injected from
    /// the effective settings by `mod.rs` at construction).
    pub fn set_session_action_ui(&mut self, ui: SessionActionUi) {
        self.session_action_ui = ui;
    }

    /// Which right-pane action surface 集中 (Closeup) presents.
    pub fn session_action_ui(&self) -> SessionActionUi {
        self.session_action_ui
    }

    /// Set the sidebar's initial state (injected from the effective settings by
    /// `mod.rs` at construction).
    pub fn set_sidebar(&mut self, sidebar: Sidebar) {
        self.sidebar = sidebar;
    }

    /// Set the manual-status label master (injected from the effective settings by
    /// `mod.rs` at construction, and re-read when the config screen closes).
    pub fn set_label_master(&mut self, master: SessionLabelMaster) {
        self.label_master = master;
    }

    /// The effective manual-status label master — read by the sidebar renderer to
    /// resolve each session's [`label_id`] and by the footer to decide whether the
    /// `Tab` hint applies.
    ///
    /// [`label_id`]: crate::domain::workspace_state::SessionRecord::label_id
    pub fn label_master(&self) -> &SessionLabelMaster {
        &self.label_master
    }

    /// The resolved manual-status label of the worktree at `index` in the first
    /// group's rows (the sidebar's display order), or `None` when that session has
    /// no label set or its stored id no longer resolves in the master. The sidebar
    /// renderer calls this per row to draw the label column.
    pub fn row_label(&self, index: usize) -> Option<&crate::domain::settings::SessionLabelDef> {
        let id = self.list.row_label_id(index)?;
        self.label_master.get(id)
    }

    /// The name of the session under the cursor and the label id to store next
    /// when 選択's `Tab` (`forward`) / `Shift-Tab` cycles its manual status — the
    /// next entry in the master, ringing through the "unset" slot — or `None` when
    /// the cursor is not on a session or no labels are defined. Pure: the caller
    /// persists it (and the reload refreshes the row); the inner `None` clears the
    /// label.
    pub fn cycle_selected_label(&self, forward: bool) -> Option<(String, Option<String>)> {
        if self.label_master.is_empty() {
            return None;
        }
        let name = self.list.selected().map(worktree_name)?.to_string();
        let current = self
            .sessions
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.label_id.as_deref());
        let new_id = self.next_label_in_cycle(current, forward);
        // The ring has ≥ 2 slots (master non-empty), so a cycle always advances to
        // a different slot; the equality guard is belt-and-braces.
        (current.map(str::to_string) != new_id).then_some((name, new_id))
    }

    /// The name of the session under the cursor and the id of the master's
    /// `index`-th label (what digit key `index + 1` selects), or `None` when the
    /// cursor is not on a session, the index is out of range, or that label is
    /// already set (a no-op). Pure, like [`cycle_selected_label`](Self::cycle_selected_label).
    pub fn select_label_index(&self, index: usize) -> Option<(String, Option<String>)> {
        let id = self.label_master.at(index)?.id.clone();
        self.resolve_label_change(Some(id))
    }

    /// The name of the session under the cursor paired with `None` to clear its
    /// manual status (the digit `0` key), or `None` when the cursor is not on a
    /// session or it carries no label already. Pure, like
    /// [`cycle_selected_label`](Self::cycle_selected_label).
    pub fn clear_selected_label(&self) -> Option<(String, Option<String>)> {
        self.resolve_label_change(None)
    }

    /// Pair the selected session's name with `new_id` when it differs from the
    /// session's current label, or `None` when the cursor is not on a session or
    /// the label is unchanged (so a repeat keypress writes nothing).
    fn resolve_label_change(&self, new_id: Option<String>) -> Option<(String, Option<String>)> {
        let name = self.list.selected().map(worktree_name)?.to_string();
        let current = self
            .sessions
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.label_id.clone());
        (current != new_id).then_some((name, new_id))
    }

    /// The label id one step from `current` through the master's ring — the labels
    /// in order preceded by the "unset" slot (index 0). `forward` advances; a
    /// `current` id no longer in the master is treated as the unset slot. Returns
    /// `None` for the unset slot, `Some(id)` otherwise.
    fn next_label_in_cycle(&self, current: Option<&str>, forward: bool) -> Option<String> {
        let n = self.label_master.len();
        let ring = n + 1;
        let cur = match current {
            None => 0,
            Some(id) => self.label_master.position(id).map(|p| p + 1).unwrap_or(0),
        };
        let next = if forward {
            (cur + 1) % ring
        } else {
            (cur + ring - 1) % ring
        };
        (next != 0).then(|| self.label_master.at(next - 1).unwrap().id.clone())
    }

    /// Set how the embedded terminal (没入) reserves its navigation keys
    /// (injected from the effective settings by `mod.rs`, and re-read when the
    /// config screen closes).
    pub fn set_key_scheme(&mut self, scheme: KeyScheme) {
        self.key_scheme = scheme;
    }

    /// How the embedded terminal (没入) reserves its navigation keys — read by the
    /// pane input loop to classify each key (see [`super::pane_input::classify`]).
    pub fn key_scheme(&self) -> KeyScheme {
        self.key_scheme
    }

    /// Record whether a `Ctrl-O` leader is currently awaiting its action key, so
    /// the 没入 footer can show it (set by the pane drive loop as the leader is
    /// pressed and as it lapses; see [`super::pane_input::prefix_alive`]).
    pub fn set_prefix_pending(&mut self, pending: bool) {
        self.prefix_pending = pending;
    }

    /// Whether a `Ctrl-O` leader is awaiting its action key — read by the footer
    /// to hint that the next key completes the chord (prefix scheme, 没入 only).
    pub fn prefix_pending(&self) -> bool {
        self.prefix_pending
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
            self.mascot_reaction = None;
            self.mascot_reaction_deadline = None;
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

    /// The one-shot reaction the mascot is playing after a click, or `None` while
    /// it rests. The renderer reads it (with [`mascot_reaction_phase`](Self::mascot_reaction_phase))
    /// to draw the burst over the resting mascot.
    pub fn mascot_reaction(&self) -> Option<crate::presentation::tui::widgets::MascotReaction> {
        self.mascot_reaction
    }

    /// The current reaction's sub-frame counter, counting up from zero since it
    /// began — the live tick minus the tick the reaction started on. The widget
    /// cycles its frames modulo their length, so this can advance freely.
    pub fn mascot_reaction_phase(&self) -> usize {
        self.mascot_tick
            .wrapping_sub(self.mascot_reaction_start_tick)
    }

    /// Whether a click reaction is in flight, so the event loop keeps ticking (and
    /// repainting) until it finishes — the burst animates on the live tick, exactly
    /// like a blink keeps the loop awake until the eyes reopen.
    pub fn mascot_reacting(&self) -> bool {
        self.mascot_reaction.is_some()
    }

    /// Start a click reaction: pick one of the [`MascotReaction`](crate::presentation::tui::widgets::MascotReaction)s
    /// pseudo-randomly and play it until [`MASCOT_REACTION`] from `now`. The event
    /// loop calls this when the user clicks the sidebar rabbit in 選択 / 集中, so the
    /// usagi does something cute back. A no-op when the mascot animation is
    /// disabled, so a toggled-off mascot stays a still resting image.
    pub fn kick_mascot_reaction(&mut self, now: Instant) {
        if !self.mascot_animation_enabled {
            return;
        }
        // Advance a small LCG and pick from it, so repeated clicks vary rather than
        // replaying the same reaction; the constants are the well-known Numerical
        // Recipes multiplier/increment, and the high bits (the most well-mixed) pick.
        self.mascot_reaction_rng = self
            .mascot_reaction_rng
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        self.mascot_reaction = Some(match (self.mascot_reaction_rng >> 24) % 3 {
            0 => crate::presentation::tui::widgets::MascotReaction::Hop,
            1 => crate::presentation::tui::widgets::MascotReaction::Sparkle,
            _ => crate::presentation::tui::widgets::MascotReaction::Bashful,
        });
        self.mascot_reaction_deadline = Some(now + MASCOT_REACTION);
        self.mascot_reaction_start_tick = self.mascot_tick;
    }

    /// Start a blink: shut the mascot's eyes until [`MASCOT_BLINK`] from `now`. The
    /// event loop calls this the moment the user interacts in 選択 / 集中, so the
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
        // End a click reaction once its window has passed, settling the mascot back
        // to its resting pose — the same deadline-on-the-clock shape as the blink.
        if let Some(deadline) = self.mascot_reaction_deadline {
            if now >= deadline {
                self.mascot_reaction = None;
                self.mascot_reaction_deadline = None;
            }
        }
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

    /// Inject the configured default agent CLI (its display name labels the 集中
    /// menu's `agent` row, and a bare `agent` / the `a` shortcut launch it).
    pub fn set_default_agent(&mut self, cli: AgentCli) {
        self.default_agent = cli;
    }

    /// The configured default agent CLI.
    pub fn default_agent(&self) -> AgentCli {
        self.default_agent
    }

    /// Inject the configured local-LLM model name (Ollama), used when opening the
    /// 集中 `chat` overlay. Injected at startup and re-applied when Config changes
    /// it, so the next chat uses the current model without restarting.
    pub fn set_local_llm_model(&mut self, model: impl Into<String>) {
        self.local_llm_model = model.into();
    }

    /// Set whether the local LLM is usable (enabled and its model pulled), gating
    /// the `chat` row in the 集中 menu. Injected from the effective settings and a
    /// runtime probe by `mod.rs`.
    pub fn set_ai_available(&mut self, available: bool) {
        self.ai_available = available;
    }

    /// The configured local-LLM model name.
    pub fn local_llm_model(&self) -> &str {
        &self.local_llm_model
    }

    /// Inject the installed agent CLIs (PATH-probed, canonical order) the 集中
    /// menu's agent picker offers.
    pub fn set_installed_agents(&mut self, agents: Vec<AgentCli>) {
        self.installed_agents = agents;
    }

    /// The installed agent CLIs offered by the 集中 menu's agent picker.
    pub fn installed_agents(&self) -> &[AgentCli] {
        &self.installed_agents
    }

    /// Record which agent CLI the next agent launch should use (`None` = the
    /// configured default). Set by the 集中 picker / `agent <name>` just before
    /// launching; consumed by [`take_agent_choice`](Self::take_agent_choice).
    pub fn set_agent_choice(&mut self, cli: Option<AgentCli>) {
        self.agent_choice = cli;
    }

    /// Take the pending agent choice, leaving `None` behind. Returns the CLI the
    /// next agent spawn should launch, or `None` to use the configured default.
    pub fn take_agent_choice(&mut self) -> Option<AgentCli> {
        self.agent_choice.take()
    }

    /// Record the opening prompt for the next configured-agent launch, set by
    /// `ai <prompt>` just before launching and consumed by
    /// [`take_agent_initial_prompt`](Self::take_agent_initial_prompt).
    pub fn set_agent_initial_prompt(&mut self, prompt: String) {
        self.agent_initial_prompt = Some(prompt);
    }

    /// Take the pending `ai <prompt>` opening prompt, leaving no prompt behind.
    /// Returns `None` for ordinary `agent` launches.
    pub fn take_agent_initial_prompt(&mut self) -> Option<String> {
        self.agent_initial_prompt.take()
    }

    /// Inject the workspace's task issues (loaded from disk by `mod.rs`), read by
    /// the `issue` command for its list / graph / show views.
    pub fn set_issues(&mut self, issues: Vec<Issue>) {
        self.issues = issues;
    }

    /// Seed the command history with entries restored from disk (oldest first),
    /// so `history` and `↑`/`↓` recall reflect commands run in past sessions.
    pub fn restore_history<E: Into<HistoryEntry>>(&mut self, entries: Vec<E>) {
        let mut entries: Vec<HistoryEntry> = entries.into_iter().map(Into::into).collect();
        // The command line caps its own recall buffer, so hand it the full command
        // list rather than pre-capping here.
        self.cmdline
            .set_history(entries.iter().map(|e| e.command.clone()).collect());
        // Cap the retained entries (read by the `history` command) to the same bound.
        let overflow = entries.len().saturating_sub(MAX_COMMAND_HISTORY);
        if overflow > 0 {
            entries.drain(..overflow);
        }
        self.history_entries = entries;
    }

    /// Seed the recorded sessions (from `state.json`), shown by `session list`,
    /// and rebuild the worktree pane from them.
    pub fn restore_sessions(&mut self, sessions: Vec<SessionRecord>) {
        self.sessions = sessions;
        self.rebuild_list();
    }

    /// Seed the workspace root's note (from `state.json`) at startup, so the `⌂
    /// root` row's memo marker, the 選択 preview, and the note editor all reflect
    /// what is on disk.
    pub fn restore_root_note(&mut self, note: Option<String>) {
        self.root_note = note;
        // Seeded after `restore_sessions` built the list, so refresh the marker in
        // place rather than relying on that earlier (note-less) rebuild.
        self.list.set_root_note_marker(self.root_note.is_some());
    }

    /// The workspace root's note (the `⌂ root` row's memo), or `None` when none
    /// has been written.
    pub fn root_note(&self) -> Option<&str> {
        self.root_note.as_deref()
    }

    /// Add the additional workspace groups shown below the primary one in
    /// 統合(unite) mode (the other workspaces the user selected on the Open
    /// screen), then rebuild the pane so they appear. Empty restores the
    /// single-workspace view. Each group carries its own sessions, sidebar labels,
    /// note markers, and root path; the orchestrator builds them from the other
    /// workspaces' preloads. The primary workspace stays first and is the one the
    /// live re-sync, `session` commands, and root row act on.
    pub fn set_extra_groups(&mut self, groups: Vec<GroupSource>) {
        self.extra_groups = groups;
        self.rebuild_list_keep_cursor();
    }

    /// Whether the home screen is showing more than one workspace (統合/unite mode).
    pub fn is_united(&self) -> bool {
        !self.extra_groups.is_empty()
    }

    /// Stack another workspace into the 統合(unite) view (`unite add`), keeping the
    /// cursor put. A no-op (returning `false`) when that workspace is already shown
    /// — the primary, or an extra group with the same root — so adding twice does
    /// not duplicate it.
    pub fn add_extra_group(&mut self, group: GroupSource) -> bool {
        if group.root_path.as_path() == self.root_path()
            || self
                .extra_groups
                .iter()
                .any(|g| g.root_path == group.root_path)
        {
            return false;
        }
        self.extra_groups.push(group);
        self.rebuild_list_keep_cursor();
        true
    }

    /// Drop the extra (unite) workspace named `name` from the view (`unite
    /// remove`), returning whether one matched. Removing the last extra group
    /// restores the single-workspace view (no headers). The primary workspace
    /// cannot be removed this way.
    pub fn remove_extra_group(&mut self, name: &str) -> bool {
        let Some(i) = self.extra_groups.iter().position(|g| g.name == name) else {
            return false;
        };
        self.extra_groups.remove(i);
        self.rebuild_list_keep_cursor();
        true
    }

    /// The names of every workspace currently shown — the primary first, then each
    /// extra (unite) group — for persisting the active unite set.
    pub fn united_workspace_names(&self) -> Vec<String> {
        std::iter::once(self.list.workspace_name().to_string())
            .chain(self.extra_groups.iter().map(|g| g.name.clone()))
            .collect()
    }

    /// The group index the cursor points at. Each 統合(unite) group now owns its
    /// own `+ new session` row, so the cursor's group is correct whether it rests
    /// on a session, a root, or a create row — no last-group special case needed.
    /// `0` is the primary; `i + 1` indexes [`extra_groups`](Self::extra_groups).
    /// The shared basis for every "cursor's group" accessor below.
    fn cursor_group(&self) -> usize {
        self.list.selected_group()
    }

    /// The workspace root the cursor's group operates in — the primary's root when
    /// the cursor is in the first group, otherwise the matching extra group's root.
    /// `session` commands (create / remove / rename / note) and the `config` (env)
    /// editor run against this so they act on the workspace the user is pointing at.
    /// The root now lives on each group's value type, so resolve it straight from
    /// the cursor's group; `cursor_group()` is always a valid group index.
    pub fn selected_workspace_root(&self) -> PathBuf {
        self.list
            .groups()
            .get(self.cursor_group())
            .map(|g| g.root_path().to_path_buf())
            .unwrap_or_default()
    }

    /// The display name of the cursor's group — the primary workspace's name, or
    /// the matching extra (unite) group's. Shown in the footer / command palette so
    /// it is clear which workspace a scoped command (`c` / `r` / `config` /
    /// `issue`) acts on in 統合(unite) mode.
    pub fn selected_workspace_name(&self) -> &str {
        match self.cursor_group().checked_sub(1) {
            None => self.list.workspace_name(),
            Some(i) => &self.extra_groups[i].name,
        }
    }

    /// The task issues of the cursor's group — the primary workspace's, or the
    /// matching extra (unite) group's — so the `issue` command lists / shows the
    /// issues of the workspace the cursor is pointing at, not always the primary.
    fn selected_group_issues(&self) -> &[Issue] {
        match self.cursor_group().checked_sub(1) {
            None => &self.issues,
            Some(i) => &self.extra_groups[i].issues,
        }
    }

    /// Fold / unfold the workspace the cursor sits in (統合(unite) mode), recording
    /// it by name so the fold survives a background re-sync's list rebuild. A no-op
    /// unless the cursor is on a group's root row and more than one workspace is
    /// shown — folding the sole workspace would hide the whole list.
    pub fn toggle_selected_collapsed(&mut self) {
        if self.list.group_count() < 2 || !self.list.root_selected() {
            return;
        }
        let group = self.list.selected_group();
        let name = self.list.groups()[group].name().to_string();
        if self.list.toggle_collapsed(group) {
            self.collapsed_workspaces.insert(name);
        } else {
            self.collapsed_workspaces.remove(&name);
        }
    }

    /// Unfold the workspace the cursor sits in if it is folded, so a session can be
    /// created into it (a folded group hides its "+ new session" row). Clears the
    /// recorded fold so a background re-sync does not re-fold it. Called just before
    /// opening the inline create input; a no-op when the group is already expanded.
    pub fn expand_selected_group_for_create(&mut self) {
        let group = self.list.selected_group();
        if self.list.is_collapsed(group) {
            let name = self.list.groups()[group].name().to_string();
            self.list.toggle_collapsed(group);
            self.collapsed_workspaces.remove(&name);
        }
    }

    /// The root-row note of the cursor's group (the primary's, or the matching
    /// extra group's), so the note editor opens prefilled with the right text.
    pub fn selected_root_note(&self) -> Option<&str> {
        match self.list.selected_group().checked_sub(1) {
            None => self.root_note.as_deref(),
            Some(i) => self.extra_groups[i].root_note.as_deref(),
        }
    }

    /// The workspace root that owns the session named `name` — searched across the
    /// primary workspace and every extra (unite) group, falling back to the primary
    /// when no group claims it. Used by name-based operations (remove / close) so
    /// they act on the workspace the session actually lives in, not just the cursor's.
    pub fn workspace_root_for_session(&self, workspace: Option<&str>, name: &str) -> PathBuf {
        // A `workspace:` qualifier (統合(unite) mode) targets that workspace's
        // root directly, even when the name is shared across workspaces or absent
        // there — so the removal acts on, and reports against, the named one. An
        // unknown qualifier falls through to name-only resolution.
        if let Some(workspace) = workspace {
            if workspace == self.list.workspace_name() {
                return self.root_path().to_path_buf();
            }
            if let Some(group) = self.extra_groups.iter().find(|g| g.name == workspace) {
                return group.root_path.clone();
            }
        }
        if self.sessions.iter().any(|s| s.name == name) {
            return self.root_path().to_path_buf();
        }
        self.extra_groups
            .iter()
            .find(|g| g.sessions.iter().any(|s| s.name == name))
            .map(|g| g.root_path.clone())
            .unwrap_or_else(|| self.root_path().to_path_buf())
    }

    /// Record the workspace a `session` operation is about to act on (the cursor's
    /// group), so its result is applied back to that group. Cleared when the result
    /// lands ([`apply_task_completion`](Self::apply_task_completion) /
    /// [`apply_session_outcome`](Self::apply_session_outcome)).
    pub fn set_op_target(&mut self, root: PathBuf) {
        self.op_target = Some(root);
    }

    /// The index into [`extra_groups`](Self::extra_groups) the recorded
    /// [`op_target`](Self::op_target) matches, or `None` for the primary workspace
    /// (or no target). Takes (clears) the target.
    fn take_op_target_group(&mut self) -> Option<usize> {
        let target = self.op_target.take()?;
        self.extra_groups.iter().position(|g| g.root_path == target)
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

    /// Swap in a freshly re-synced set of sessions for the workspace rooted at
    /// `root`, routing them to the matching sidebar group: the primary workspace
    /// or one of the 統合(unite) groups. Like [`refresh_sessions`](Self::refresh_sessions)
    /// it keeps the cursor and active row on the same session names.
    ///
    /// Used by the background `state.json` watcher, which republishes each
    /// workspace's recorded sessions keyed by its root (an agent's MCP
    /// `session_create` / `session_delegate_issue`, another window, or the CLI may
    /// have written any of them). A `root` matching no displayed workspace — one
    /// dropped from unite mode after the refresh was queued — is ignored rather
    /// than misfiled onto the primary.
    pub fn refresh_sessions_for(&mut self, root: &Path, sessions: Vec<SessionRecord>) {
        if root == self.root_path() {
            self.refresh_sessions(sessions);
        } else if let Some(i) = self.extra_groups.iter().position(|g| g.root_path == root) {
            self.extra_groups[i].sessions = sessions;
            self.rebuild_list_keep_cursor();
        }
    }

    /// Rebuild the worktree pane while keeping the cursor, active row, and `Ctrl-^`
    /// jump target on the same sessions by name. Used whenever the rows are rebuilt
    /// under the user (a background re-sync, a manual reorder, the waiting-first
    /// sort toggling on/off, or a session entering/leaving the waiting set) so the
    /// rows can be replaced wholesale without yanking the cursor back to the root.
    fn rebuild_list_keep_cursor(&mut self) {
        enum RowAnchor {
            Create,
            Root(usize),
            Session { group: usize, name: String },
        }

        let selected = if self.list.create_row_selected() {
            RowAnchor::Create
        } else if let Some(worktree) = self.list.selected() {
            RowAnchor::Session {
                group: self.list.selected_group(),
                name: worktree_name(worktree).to_string(),
            }
        } else {
            RowAnchor::Root(self.list.selected_group())
        };
        let active = if self.list.active_index() == self.list.create_row() {
            RowAnchor::Create
        } else if let Some(worktree) = self.list.active() {
            RowAnchor::Session {
                group: self.list.active_group(),
                name: worktree_name(worktree).to_string(),
            }
        } else {
            RowAnchor::Root(self.list.active_group())
        };
        // The fresh list drops the `Ctrl-^` jump target, so carry it across the
        // rebuild by its `(root, name)` identity (re-validated lazily, so a session
        // that vanished in this sync simply yields no jump).
        let previous = self.list.previous_active().cloned();
        self.rebuild_list();
        // Restore by the row's identity *within its workspace group*. In 統合
        // (unite) mode every workspace contributes a synthetic root row named
        // `ROOT_NAME`, and several workspaces may also share a branch name; using
        // the old name-only lookup would resolve those ambiguous rows to the first
        // group and snap 選択 back to the top when a background refresh landed.
        let resolve = |list: &WorktreeList, anchor: &RowAnchor| match anchor {
            RowAnchor::Create => list.create_row(),
            RowAnchor::Root(group) => list.group_root_row(*group).unwrap_or(0),
            RowAnchor::Session { group, name } => list
                .row_in_group_of_name(*group, name)
                .or_else(|| list.group_root_row(*group))
                .unwrap_or(0),
        };
        self.list.focus_index(resolve(&self.list, &selected));
        // The active row is command-facing, so keep it on a real selectable row;
        // if a corrupt/old state ever had it on the create affordance, normalize
        // it to the primary root while rebuilding.
        let active_row = match active {
            RowAnchor::Create => 0,
            ref anchor => resolve(&self.list, anchor),
        };
        self.list.activate_index(active_row);
        self.list.set_previous_active(previous);
    }

    /// Rebuild the worktree pane from the current sessions: one row per session
    /// (not per repository). A session spanning several git repositories is
    /// collapsed into a single row by [`session_row`]. The rows follow the session
    /// order from [`display_order`](Self::display_order) — the canonical (manual)
    /// order, or waiting-first when [`sort_waiting`](Self::sort_waiting) is on.
    fn rebuild_list(&mut self) {
        let name = self.list.workspace_name().to_string();
        // The primary workspace root lives on the first group's value type; carry
        // it across the rebuild (which replaces the list wholesale), exactly as the
        // workspace name is carried above.
        let root_path = self.list.root_path().to_path_buf();
        let layout = self.display_layout();
        let rows = layout
            .iter()
            .map(|(i, _)| session_row(&self.sessions[*i]))
            .collect();
        // Carry each session's sidebar label override onto its row so the pane
        // shows the custom display name while commands still key on the branch.
        let labels = layout
            .iter()
            .map(|(i, _)| self.sessions[*i].display_name.clone())
            .collect();
        // Carry each session's note-presence onto its row so the pane can show a
        // memo marker; the note text itself is read on demand (Overview preview /
        // editor), never stored on the row.
        let notes = layout
            .iter()
            .map(|(i, _)| self.sessions[*i].note.is_some())
            .collect();
        // Carry each session's manual-status label id onto its row so the sidebar
        // can resolve it against the master; the id itself is cosmetic and every
        // command still keys on the branch.
        let label_ids = layout
            .iter()
            .map(|(i, _)| self.sessions[*i].label_id.clone())
            .collect();
        // Carry the session lineage depth (derived from `started_from`) so child
        // sessions can be drawn indented under the session that created them.
        let nesting_depths = layout.iter().map(|(_, depth)| *depth).collect();
        let mut list = WorktreeList::with_labels(name, rows, labels);
        list.set_root_path(root_path);
        list.set_notes(notes);
        list.set_label_ids(label_ids);
        list.set_nesting_depths(nesting_depths);
        // The root row's note lives on the workspace state (it belongs to no
        // session), so its marker is carried separately from the per-session notes.
        list.set_root_note_marker(self.root_note.is_some());
        // 統合(unite) mode: stack the other selected workspaces below the primary
        // one, each collapsed from its recorded sessions the same way the primary is.
        for group in &self.extra_groups {
            list.add_group(WorkspaceGroup::from_sessions(
                &group.name,
                group.root_path.clone(),
                &group.sessions,
                group.root_note.is_some(),
            ));
        }
        // Re-apply the user's folds by workspace name so a background re-sync (which
        // rebuilds the list wholesale) does not silently unfold every workspace.
        list.set_collapsed_by_names(&self.collapsed_workspaces);
        self.list = list;
    }

    /// The order the sessions are laid out in the left pane, as indices into
    /// `sessions`. Identity (canonical / manual `K`/`J` order) by default; with
    /// [`sort_waiting`](Self::sort_waiting) on, a *stable* partition that lifts the
    /// sessions whose agent is waiting for input (◆) above the rest while keeping
    /// each group in its canonical order.
    fn base_display_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.sessions.len()).collect();
        if self.sort_waiting {
            // `sort_by_key` is stable, and `false` (waiting) sorts before `true`,
            // so waiting sessions rise to the top without disturbing either group's
            // relative order.
            order.sort_by_key(|&i| !self.badges.waiting.contains(&self.sessions[i].root));
        }
        order
    }

    /// The order and indentation depth the sessions are laid out in the left
    /// pane. It starts from [`base_display_order`](Self::base_display_order), then
    /// folds `started_from` parent links into a visible tree: children are drawn
    /// immediately below the session that created them.
    fn display_layout(&self) -> Vec<(usize, usize)> {
        session_tree_layout(&self.sessions, &self.base_display_order())
    }

    /// Whether the left pane is lifting the waiting-for-input (◆) sessions to the
    /// top — read by the footer to show the toggle's state.
    pub fn sort_waiting(&self) -> bool {
        self.sort_waiting
    }

    /// Toggle the waiting-first ordering of the left pane (`s` in 選択) and rebuild
    /// the rows, keeping the cursor on the same session by name so it follows its
    /// row to the new position.
    pub fn toggle_sort_waiting(&mut self) {
        self.sort_waiting = !self.sort_waiting;
        self.rebuild_list_keep_cursor();
    }

    pub fn sessions(&self) -> &[SessionRecord] {
        &self.sessions
    }

    /// The per-session agent CLI / model override recorded for the session that
    /// owns worktree `dir` — its session root or any of its worktree paths — or
    /// the default (follow the workspace effective settings) when `dir` belongs to
    /// no session (e.g. the `⌂ root` row) or the matched session pinned nothing.
    /// Read at every agent launch site (interactive, pane recovery, queued-prompt
    /// auto-start) so a session started with a chosen CLI / model launches with it.
    pub fn session_agent_for(&self, dir: &Path) -> SessionAgent {
        self.sessions
            .iter()
            .find(|s| s.root == dir || s.worktrees.iter().any(|w| w.path == dir))
            .map(|s| s.agent.clone())
            .unwrap_or_default()
    }

    /// Mark the session rooted at `root` as touched at `now`, driving its sidebar
    /// freshness ("heat") dot back to fresh. Returns whether a session matched (so
    /// the caller can skip a needless list rebuild). The bump lives in `sessions`
    /// (the source [`session_row`] reads), so the dot only reflects it once the
    /// list is rebuilt — callers that want it shown now rebuild after.
    fn bump_last_active(&mut self, root: &Path, now: DateTime<Utc>) -> bool {
        match self.sessions.iter_mut().find(|s| s.root == root) {
            Some(session) => {
                session.last_active = Some(now);
                true
            }
            None => false,
        }
    }

    /// Touch the active session (the one 集中/没入 acts on), refreshing its heat dot
    /// immediately. A no-op when the root row is active (it is no session).
    fn touch_active(&mut self, now: DateTime<Utc>) {
        let Some(root) = self.list.active().map(|w| w.path.clone()) else {
            return;
        };
        if self.bump_last_active(&root, now) {
            self.rebuild_list_keep_cursor();
        }
    }

    /// The accumulated `(session name, last_active)` pairs to flush to `state.json`
    /// on quit, so the freshness dots survive a restart. Only sessions actually
    /// touched this run (those carrying a `last_active`) are included.
    pub fn last_active_flush(&self) -> Vec<(String, DateTime<Utc>)> {
        self.sessions
            .iter()
            .filter_map(|s| s.last_active.map(|t| (s.name.clone(), t)))
            .collect()
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

    /// Open the right-pane diff view from a load attempt: on success, parse and
    /// show the patch (titled by the diffed branch → base) in the scrollable right
    /// pane; on failure — no session highlighted, or the base branch could not be
    /// resolved — log the error and open nothing. The impure git shell-out is the
    /// caller's (the event loop); parsing / highlighting the patch and storing the
    /// result is pure, so both outcomes are testable.
    pub fn open_diff_result(&mut self, loaded: anyhow::Result<(String, String)>) {
        match loaded {
            Ok((title, patch)) => {
                self.overlay = Overlay::Diff(DiffView {
                    title,
                    doc: crate::presentation::tui::diff::render(&patch),
                    scroll: 0,
                    split: false,
                });
            }
            Err(e) => self.log_error(format!("diff failed: {e}")),
        }
    }

    /// The open right-pane diff view, if any.
    pub fn diff_view(&self) -> Option<&DiffView> {
        match &self.overlay {
            Overlay::Diff(diff) => Some(diff),
            _ => None,
        }
    }

    /// Close the diff view (the user dismissed it). Called only while the diff view
    /// is the open overlay, so it clears the overlay outright.
    pub fn close_diff(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Scroll the diff view up one line (no-op when closed or at the top).
    pub fn diff_scroll_up(&mut self) {
        if let Overlay::Diff(diff) = &mut self.overlay {
            diff.scroll = diff.scroll.saturating_sub(1);
        }
    }

    /// Scroll the diff view down one line, clamped so the last row stays in view
    /// (no-op when closed). `visible` is the pane body height the view can show.
    /// The row count is layout-aware: the split view folds paired add/del lines
    /// into one visual row, so it clamps against fewer rows than the unified view.
    pub fn diff_scroll_down(&mut self, visible: usize) {
        if let Overlay::Diff(diff) = &mut self.overlay {
            let total = if diff.split {
                crate::presentation::tui::diff::split_rows(&diff.doc).len()
            } else {
                diff.doc.rows.len()
            };
            let max = total.saturating_sub(visible);
            diff.scroll = (diff.scroll + 1).min(max);
        }
    }

    /// Toggle the diff view between the unified and split (side-by-side) layouts
    /// (no-op when closed), resetting the scroll so the switch lands at the top.
    pub fn diff_toggle_split(&mut self) {
        if let Overlay::Diff(diff) = &mut self.overlay {
            diff.split = !diff.split;
            diff.scroll = 0;
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

    // --- 集中 chat (local LLM) --------------------------------------------

    /// Open the local-LLM chat overlay in the right pane, bound to the configured
    /// local model. Replaces any other open overlay (only one is ever open).
    pub fn open_chat(&mut self) {
        self.overlay = Overlay::Chat(Chat::new(&self.local_llm_model));
    }

    /// The open chat overlay, if any.
    pub fn chat(&self) -> Option<&Chat> {
        match &self.overlay {
            Overlay::Chat(chat) => Some(chat),
            _ => None,
        }
    }

    /// The open chat overlay for mutation (editing / scrolling / submitting), if
    /// any.
    pub fn chat_mut(&mut self) -> Option<&mut Chat> {
        match &mut self.overlay {
            Overlay::Chat(chat) => Some(chat),
            _ => None,
        }
    }

    /// Close the chat overlay (the user pressed `Esc`), returning the right pane
    /// to the 集中 surface beneath it. A no-op unless the chat is the open overlay.
    pub fn close_chat(&mut self) {
        if matches!(self.overlay, Overlay::Chat(_)) {
            self.overlay = Overlay::None;
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

    /// Reflects freshly detected pull-request links in the sidebar `#N` badge of
    /// the session row at `root`, without waiting for the next workspace re-sync.
    ///
    /// The attached pane and background watcher call this when they spot a new
    /// `/pull/<N>` URL in shell output, passing the PR-link store's accumulated
    /// set so the live badge matches what a later re-sync would fold in from
    /// `pr-links/`. Returns whether anything changed, so the caller repaints only
    /// when it did.
    pub fn set_pr_links(
        &mut self,
        root: &Path,
        prs: Vec<crate::domain::workspace_state::PrLink>,
    ) -> bool {
        self.list.set_pr_links(root, prs)
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Which command scope the command palette (`:`) operates in: always the
    /// whole workspace, since the palette is workspace-only (the session-scoped
    /// surface lives in the 集中 right pane instead). Completion, hints, and `man`
    /// grouping follow this. The 集中 prompt calls the registry with
    /// [`CommandScope::Session`] directly via [`Self::closeup_prompt_hint`] etc.
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

    /// Test-only: register an extra command into the registry (re-deriving the
    /// 集中 menu's static command list), so tests can exercise how the menu and
    /// its dispatch treat a Session-scope entry the built-ins do not cover.
    #[cfg(test)]
    pub fn register_command(&mut self, command: Box<dyn super::command::Command>) {
        self.registry.register(command);
        self.session_menu_commands = sorted_session_menu_commands(&self.registry);
    }

    /// The current embedded-terminal snapshot, when a session is 没入 (Attached)
    /// or previewed in 選択 (Overview).
    pub fn terminal_view(&self) -> Option<&TerminalView> {
        self.terminal.view.as_ref()
    }

    /// Enter 没入 (Attached): an embedded terminal / agent is going live in the
    /// right pane. The first snapshot arrives via a
    /// [`SurfaceOwner::Attached`] writer from
    /// [`surface_writer`](Self::surface_writer).
    pub fn show_attached(&mut self) {
        self.mode = Mode::Closeup;
        self.closeup_attached = true;
        // Attaching consumes any action surface still floating over a pane tab, so
        // a later return to 集中 starts fresh rather than over a stale float.
        self.closeup_action_over_pane = false;
    }

    /// Whether Closeup is currently being rendered as a live attached pane.
    pub fn closeup_attached(&self) -> bool {
        self.closeup_attached
    }

    /// Leave 没入 for 集中 (Closeup): the embedded session was closed or detached,
    /// so drop the surface and return to the focused session's action surface.
    /// The tab selector lands on the trailing "+ new" tab — the launch surface.
    /// A deliberate zoom-out (`Ctrl-T` / `Ctrl-O a`) instead keeps the pane's
    /// own tab selected with the action surface floating over its preview — the
    /// caller follows up with [`closeup_action_over_active_pane`] — and arms
    /// [`arm_closeup_return_attach`], so the next `Esc` re-attaches the pane
    /// rather than stepping back onto its preview (see [`closeup_discard_new_tab`]).
    ///
    /// [`closeup_action_over_active_pane`]: Self::closeup_action_over_active_pane
    /// [`arm_closeup_return_attach`]: Self::arm_closeup_return_attach
    /// [`closeup_discard_new_tab`]: Self::closeup_discard_new_tab
    pub fn leave_attached(&mut self) {
        self.mode = Mode::Closeup;
        self.closeup_attached = false;
        self.closeup_new_tab = true;
        self.closeup_action_over_pane = false;
        // Returning to the menu from a pane presents its full listing: a filter
        // typed before the launch does not linger over the surface it left.
        self.closeup_menu_filter = None;
        self.clear_terminal_surface();
        // The 没入 drive loop may have left its `Ctrl-O` leader bit set when it
        // exited on the second key; clear it so 集中 starts without one pending.
        self.prefix_pending = false;
    }

    /// The tab strip shown above the embedded terminal, when the surface is live.
    pub fn terminal_tabs(&self) -> Option<&TabStrip> {
        self.terminal.tabs.as_ref()
    }

    /// Animation frame for the selected loading tab's body, when the current
    /// surface explicitly published one.
    pub fn terminal_loading_body_frame(&self) -> Option<usize> {
        self.terminal.loading_body_frame
    }

    /// Claim the embedded-terminal surface for `owner` and return the only handle
    /// that can publish a view or tab strip to it. Claiming from a different owner
    /// first drops that owner's snapshot as a unit, so the right pane cannot be
    /// composed from a stale view written by one party and tabs written by the
    /// other.
    pub fn surface_writer(&mut self, owner: SurfaceOwner) -> SurfaceWriter<'_> {
        self.terminal.claim(owner);
        SurfaceWriter {
            surface: &mut self.terminal,
        }
    }

    /// Drop the embedded-terminal surface — both the screen snapshot and the tab
    /// strip — without changing the mode. Clearing the two together is the whole
    /// point of bundling them: there is no path that drops one and forgets the
    /// other. It also releases ownership, so the next writer must explicitly claim
    /// the surface before publishing.
    pub fn clear_terminal_surface(&mut self) {
        self.terminal = TerminalSurface::default();
    }

    /// Test-only convenience for publishing a tab strip without making every UI
    /// renderer test spell out the ownership claim. Production code uses
    /// [`surface_writer`](Self::surface_writer), so the single-writer hand-off is
    /// enforced outside tests.
    #[cfg(test)]
    pub fn set_terminal_tabs(&mut self, labels: Vec<String>, active: usize) {
        self.surface_writer(SurfaceOwner::Preview)
            .set_tabs(labels, active);
    }

    /// Test-only convenience for publishing a terminal snapshot without making
    /// every UI renderer test spell out the ownership claim. Production code uses
    /// [`surface_writer`](Self::surface_writer), so the single-writer hand-off is
    /// enforced outside tests.
    #[cfg(test)]
    pub fn set_terminal_view(&mut self, view: TerminalView) {
        self.surface_writer(SurfaceOwner::Preview).set_view(view);
    }

    /// Claim the activity badge snapshot for `owner` and return the only handle
    /// that can replace it. The snapshot itself is replaced as a unit, and the
    /// claim makes the write owner explicit (the event loop between frames, or
    /// the pane driver while a session is 没入) instead of letting both mutate the
    /// home state directly.
    pub fn badge_writer(&mut self, owner: SurfaceOwner) -> BadgeWriter<'_> {
        if self.badge_owner != Some(owner) {
            self.badge_owner = Some(owner);
        }
        BadgeWriter { state: self }
    }

    /// Replace every session activity badge set at once with a fresh reading from
    /// the terminal monitor (running / waiting / live / done). Kept private so
    /// production callers must go through [`badge_writer`](Self::badge_writer) and
    /// declare which screen driver owns this write. Replacing them as a unit keeps
    /// the four sets consistent with one another (all from the same lock).
    fn replace_badges(&mut self, badges: MonitorSnapshot) {
        // With the waiting-first sort on, a session entering or leaving the
        // waiting set changes the row order, so rebuild the pane (keeping the
        // cursor by name). Compared before the move, and only when the *waiting*
        // set actually moved, so the hot per-frame path skips the rebuild whenever
        // nothing relevant changed.
        let resort = self.sort_waiting && self.badges.waiting != badges.waiting;
        // A session whose activity state changed (entered/left running / waiting /
        // done / live) was just doing something — bump its heat dot to fresh. The
        // diff is over set membership only (no I/O), so the per-frame path stays
        // cheap and a quiet frame bumps nothing.
        let touched = changed_roots(&self.badges, &badges);
        let now = Utc::now();
        let mut bumped = false;
        for root in &touched {
            // Every changed root is bumped (not short-circuited), so a frame that
            // moves several sessions freshens them all.
            bumped |= self.bump_last_active(root, now);
        }
        self.badges = badges;
        if resort || bumped {
            self.rebuild_list_keep_cursor();
        }
    }

    /// Test-only convenience for replacing badge sets without making every state
    /// test spell out the ownership claim. Production code uses
    /// [`badge_writer`](Self::badge_writer), so the single-writer ownership is
    /// enforced outside tests.
    #[cfg(test)]
    pub fn apply_badges(&mut self, badges: MonitorSnapshot) {
        self.badge_writer(SurfaceOwner::Preview).apply(badges);
    }

    /// The badge sets the last [`BadgeWriter::apply`] stored, so a render loop can
    /// compare a freshly read snapshot against what it last drew and skip the
    /// repaint when nothing moved — without keeping (and cloning into) its own
    /// copy each frame.
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

    /// The CPU / memory each live session is using, keyed by worktree path, from
    /// the terminal monitor's last resource sample — the sidebar shows a figure
    /// only for the rows that have one (the live sessions).
    pub fn resource_usages(&self) -> &HashMap<PathBuf, ResourceUsage> {
        &self.badges.resources
    }

    /// The workspace total CPU / memory across every live session, shown beside
    /// the resting mascot. Idle (zero) while nothing is live, so the mascot rests
    /// without a number.
    pub fn resource_total(&self) -> ResourceUsage {
        self.badges.resource_total
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
    /// repaint) so the loading indicator appears; a multi-step action (e.g. a
    /// bulk removal) steps once per item so the loader animates as it progresses.
    pub fn step_loading(&mut self, label: impl Into<String>) {
        let frame = self.loading.as_ref().map_or(0, |l| l.frame + 1);
        self.loading = Some(LoadingIndicator {
            label: label.into(),
            frame,
        });
    }

    /// Clear the "working…" indicator once the blocking action has finished, so
    /// the screen returns to its resting state.
    pub fn finish_loading(&mut self) {
        self.loading = None;
    }

    /// The transient "working…" indicator, when an action is in flight — the
    /// launch loader is shown only while this is `Some`.
    pub fn loading(&self) -> Option<&LoadingIndicator> {
        self.loading.as_ref()
    }

    /// Begin tracking a pane launch whose tab loads in the strip. It starts in
    /// the **Resolving** phase — no pool pane exists yet while the launch's
    /// environment resolves — so `placeholder` is the synthetic chip label to draw
    /// at the strip's end. The chip starts un-placed (`tab: None`) and
    /// un-animated (`frame: 0`) until the first poll.
    pub fn begin_pending_pane(&mut self, dir: PathBuf, placeholder: String) {
        self.pending_pane = Some(PendingPane {
            dir,
            frame: 0,
            tab: None,
            placeholder: Some(placeholder),
        });
    }

    /// Advance the pending pane's loading animation one tick and refresh the tab
    /// index its chip sits at. `resolving` distinguishes the phase: while `true`
    /// the chip is the synthetic placeholder at the strip's end (the pane has not
    /// spawned yet); once `false` the pane is in the pool, so its placeholder is
    /// dropped and `tab` tracks the real chip. A no-op when nothing is pending.
    pub fn advance_pending_pane(&mut self, tab: usize, resolving: bool) {
        if let Some(p) = self.pending_pane.as_mut() {
            p.frame += 1;
            p.tab = Some(tab);
            if !resolving {
                p.placeholder = None;
            }
        }
    }

    /// Stop tracking the pending pane — because it became ready (attached when it
    /// was still selected, left as a normal tab otherwise), or its shell vanished.
    /// Returns the dropped tracker so the caller can read what it was.
    pub fn clear_pending_pane(&mut self) -> Option<PendingPane> {
        self.pending_pane.take()
    }

    /// The background pane currently loading in the strip, when one is tracked.
    pub fn pending_pane(&self) -> Option<&PendingPane> {
        self.pending_pane.as_ref()
    }

    /// The `(tab index, animation frame)` of the loading chip, once its index is
    /// resolved — the renderer animates exactly this chip in the tab strip.
    /// `None` when nothing is pending (or its index is not yet known).
    pub fn loading_tab(&self) -> Option<(usize, usize)> {
        self.pending_pane
            .as_ref()
            .and_then(|p| p.tab.map(|tab| (tab, p.frame)))
    }

    fn begin_pending_session_kind(
        &mut self,
        kind: PendingSessionKind,
        root: PathBuf,
        name: String,
    ) {
        if self
            .pending_sessions
            .iter()
            .any(|p| p.kind == kind && p.root == root && p.name == name)
        {
            return;
        }
        self.pending_sessions
            .push(PendingSession { kind, root, name });
    }

    fn clear_pending_session_kind(
        &mut self,
        kind: PendingSessionKind,
        root: &Path,
        name: &str,
    ) -> bool {
        let before = self.pending_sessions.len();
        self.pending_sessions
            .retain(|p| !(p.kind == kind && p.root.as_path() == root && p.name == name));
        self.pending_sessions.len() != before
    }

    /// Begin showing an inline skeleton for a session create targeting `root`.
    /// Duplicate begins for the same `(root, name)` are ignored so repeated
    /// dispatch paths cannot stack identical skeletons.
    pub fn begin_pending_session(&mut self, root: PathBuf, name: String) {
        self.begin_pending_session_kind(PendingSessionKind::Create, root, name);
    }

    /// Begin showing an inline skeleton for a session removal targeting `root`.
    /// Duplicate begins for the same `(root, name)` are ignored so repeated
    /// dispatch paths cannot stack identical skeletons.
    pub fn begin_removing_session(&mut self, root: PathBuf, name: String) {
        self.begin_pending_session_kind(PendingSessionKind::Remove, root, name);
    }

    /// Clear the inline create skeleton for `name` under `root`, returning
    /// whether one was present.
    pub fn clear_pending_session(&mut self, root: &Path, name: &str) -> bool {
        self.clear_pending_session_kind(PendingSessionKind::Create, root, name)
    }

    /// Clear the inline removal skeleton for `name` under `root`, returning
    /// whether one was present.
    pub fn clear_removing_session(&mut self, root: &Path, name: &str) -> bool {
        self.clear_pending_session_kind(PendingSessionKind::Remove, root, name)
    }

    /// Pending session lifecycle skeletons currently shown in the sidebar.
    pub fn pending_sessions(&self) -> &[PendingSession] {
        &self.pending_sessions
    }

    /// Swap in the current background-task rows (session create / remove running
    /// off the event-loop thread), read from the shared task handle each frame.
    /// The UI decides per row kind whether it belongs in the sidebar skeleton or
    /// the top-right status block.
    pub fn set_tasks(&mut self, tasks: Vec<TaskRow>) {
        self.tasks = tasks;
    }

    /// The background-task rows the UI renders by kind.
    pub fn tasks(&self) -> &[TaskRow] {
        &self.tasks
    }

    /// Apply a finished background task's outcome: append its result line to the
    /// log and, when the action changed the sessions, swap in the refreshed list
    /// **keeping the cursor and active row where they are** (via
    /// [`refresh_sessions`](Self::refresh_sessions)) — a session created or
    /// removed in the background must never yank the user's cursor mid-navigation.
    pub fn apply_task_completion(
        &mut self,
        line: LogLine,
        sessions: Option<Vec<SessionRecord>>,
        target_root: Option<&Path>,
    ) {
        self.push_logged_line(line);
        let target_group = match target_root {
            Some(root) => {
                self.op_target = None;
                self.extra_groups.iter().position(|g| g.root_path == root)
            }
            None => None,
        };
        if let Some(sessions) = sessions {
            // Route the reloaded sessions to the workspace the operation targeted:
            // an extra (unite) group when the cursor was in one, else the primary.
            match target_group.or_else(|| self.take_op_target_group()) {
                Some(i) => {
                    self.extra_groups[i].sessions = sessions;
                    self.rebuild_list_keep_cursor();
                }
                None => self.refresh_sessions(sessions),
            }
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

    /// Whether the update-confirmation modal is open.
    pub fn update_confirm(&self) -> bool {
        self.update_confirm
    }

    /// Open the update-confirmation modal — the user clicked the mascot while it
    /// was announcing an available update and is asked to confirm before the
    /// self-update runs.
    pub fn open_update_confirm(&mut self) {
        self.update_confirm = true;
    }

    /// Dismiss the update-confirmation modal (cancelled, or the update was
    /// dispatched), returning to the normal screen.
    pub fn cancel_update_confirm(&mut self) {
        self.update_confirm = false;
    }

    /// React to a click on the resting sidebar mascot: when it is visibly
    /// announcing an available update ([`update`](Self::update) is `Some` and no
    /// loading / background task is using the bubble), raise the update-confirm
    /// modal; otherwise play a one-shot click reaction. The event loop calls this
    /// on a hit so the rabbit either offers the update or just does something
    /// cute back, matching what its bubble currently says.
    pub fn click_mascot(&mut self, now: Instant) {
        if self.update.is_some() && self.loading.is_none() && self.tasks.is_empty() {
            self.open_update_confirm();
        } else {
            self.kick_mascot_reaction(now);
        }
    }

    /// Arm [`ResumeLevel::Attached`] to be persisted when the next quit is
    /// confirmed. Called by the pane driver when `Ctrl-Q` leaves 没入, before the
    /// mode drops to [`Mode::Closeup`] on the way to the quit modal — otherwise the
    /// recorded engagement would lose that the user was attached.
    pub fn arm_resume_attached(&mut self) {
        self.pending_resume = Some(ResumeLevel::Attached);
    }

    /// The engagement to persist for restore, consuming any arm. An armed level
    /// (a 没入 quit) wins; otherwise it is read off the current [`mode`](Self::mode)
    /// — 選択 → [`ResumeLevel::Switch`], 集中 → [`ResumeLevel::Closeup`]. Attached is a Closeup sub-state, so the pane driver arms it
    /// explicitly before returning to the management loop.
    pub fn resume_level(&mut self) -> ResumeLevel {
        self.pending_resume.take().unwrap_or(match self.mode {
            Mode::Switch => ResumeLevel::Switch,
            Mode::Closeup => ResumeLevel::Closeup,
        })
    }

    /// Restore the engagement recorded at the last quit: move the cursor to
    /// `session` (選択), focus it (集中), or focus it and arm an auto-attach (没入).
    /// A no-op when the session no longer exists (it was removed since), so a
    /// stale snapshot never strands the cursor on a missing row. Called at startup
    /// after the panes are restored, so a 没入 target's pane is already live for
    /// the event loop's first-pass attach.
    pub fn restore_focus(&mut self, session: &str, level: ResumeLevel) {
        match level {
            ResumeLevel::Switch => {
                // Move the 選択 cursor onto the session (root stays at the default
                // cursor, which `select_by_name` leaves put by not matching it).
                self.list.select_by_name(session);
            }
            ResumeLevel::Closeup => {
                self.enter_closeup_named(session);
            }
            ResumeLevel::Attached => {
                if self.enter_closeup_named(session) {
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
        self.list
            .focus_index(row.min(self.list.create_row().saturating_sub(1)));
    }

    // --- Overview modal (`:`) ----------------------------------------------

    /// Open the workspace Overview modal (`:`), clearing any half-typed command
    /// line so it starts fresh. The modal reuses the workspace
    /// command-line state ([`input`](Self::input) / [`recall`](Self::recall)),
    /// floating over the current 選択 / 集中 panes while open.
    pub fn open_overview_modal(&mut self) {
        self.command_open = true;
        self.cmdline.clear();
    }

    /// Compatibility wrapper while command handlers still call the old name.
    pub fn open_command_palette(&mut self) {
        self.open_overview_modal();
    }

    /// Close the Overview modal (`Esc`), clearing its command line.
    pub fn close_overview_modal(&mut self) {
        self.command_open = false;
        self.cmdline.clear();
    }

    /// Compatibility wrapper while command handlers still call the old name.
    pub fn close_command_palette(&mut self) {
        self.close_overview_modal();
    }

    /// Whether the workspace Overview modal is open.
    pub fn overview_modal_open(&self) -> bool {
        self.command_open
    }

    /// Compatibility wrapper while command handlers still call the old name.
    pub fn command_palette_open(&self) -> bool {
        self.overview_modal_open()
    }

    // --- Switch --------------------------------------------------------------

    /// Enter Switch: move keyboard focus to the left pane to pick a session.
    pub fn enter_switch(&mut self) {
        self.mode = Mode::Switch;
        self.closeup_attached = false;
        self.overlay.clear_create();
        // Any 集中 `Ctrl-O` leader is abandoned by leaving the surface.
        self.prefix_pending = false;
    }

    /// Open the Focus modal (`Ctrl-O a`) for the selected / focused session.
    ///
    /// From Switch this enters Closeup on the highlighted row, so the session's
    /// action surface appears. From Closeup it makes the action surface visible
    /// again: on a pane preview it floats over that pane, and on the "+ new" tab
    /// it is already the active surface.
    pub fn open_focus_modal(&mut self) {
        match self.mode {
            Mode::Switch => {
                let row = self.list.selected_index();
                self.enter_closeup(row);
            }
            Mode::Closeup if self.closeup_attached => {}
            Mode::Closeup if self.closeup_on_new_tab() => {}
            Mode::Closeup => {
                self.closeup_action_over_active_pane();
            }
        }
    }

    /// Move the Overview cursor up one row, wrapping (delegates to the list).
    pub fn overview_move_up(&mut self) {
        self.list.move_up();
    }

    /// Move the Overview cursor down one row, wrapping (delegates to the list).
    pub fn overview_move_down(&mut self) {
        self.list.move_down();
    }

    /// Move the Overview cursor straight to a selectable `row` (0 is the root row),
    /// clamped to the rows that exist — used when a left click selects a session
    /// row directly.
    pub fn overview_select(&mut self, row: usize) {
        self.list.focus_index(row);
    }

    /// Begin inline session creation in 選択: open an empty name input that
    /// captures the mode's keys until confirmed (Enter) or cancelled (Esc).
    ///
    /// `taken` is the set of branch names that already exist across the
    /// workspace's repositories (from
    /// [`crate::usecase::session::existing_branch_names`]); the typed name is
    /// validated against it live so a duplicate or branch-namespace clash is
    /// flagged before Enter.
    pub fn overview_begin_create(&mut self, taken: Vec<String>) {
        self.overlay = Overlay::Create(CreateInput::new(taken));
    }

    /// Whether an inline create input is open in 選択.
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
    /// 選択 keys to its own methods ([`CreateInput::push_char`] etc.).
    pub fn create_mut(&mut self) -> Option<&mut CreateInput> {
        match &mut self.overlay {
            Overlay::Create(input) => Some(input),
            _ => None,
        }
    }

    /// Cancel inline creation, staying in 選択.
    pub fn create_cancel(&mut self) {
        self.overlay.clear_create();
    }

    /// Validate and accept the inline create name. On success the input closes
    /// and the trimmed name is returned (for the event loop to create the
    /// session); on an invalid name the input stays open with the inline error
    /// shown live and `None` is returned (see [`CreateInput::confirm`]). A no-op
    /// (returning `None`) when not creating.
    pub fn overview_confirm_create(&mut self) -> Option<String> {
        let Overlay::Create(input) = &mut self.overlay else {
            return None;
        };
        // An invalid name keeps the input open (with its live error); only a
        // valid one closes it.
        let name = input.confirm()?;
        self.overlay = Overlay::None;
        Some(name)
    }

    /// Begin inline rename of the selected session's sidebar label in 選択: open
    /// an input pre-filled with its current label that captures the mode's keys
    /// until confirmed (Enter) or cancelled (Esc). A no-op on the root row (which
    /// is not a session and has no label to change) and when an input is already
    /// open. Returns whether the input opened.
    pub fn overview_begin_rename(&mut self) -> bool {
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

    /// Whether an inline rename input is open in 選択.
    pub fn is_renaming(&self) -> bool {
        matches!(self.overlay, Overlay::Rename(_))
    }

    pub fn open_tab_menu(
        &mut self,
        dir: PathBuf,
        tab: usize,
        label: impl Into<String>,
        col: u16,
        row: u16,
    ) {
        self.overlay = Overlay::TabMenu(TabMenu::new(dir, tab, label, col, row));
    }

    pub fn tab_menu(&self) -> Option<&TabMenu> {
        match &self.overlay {
            Overlay::TabMenu(menu) => Some(menu),
            _ => None,
        }
    }

    pub fn tab_menu_mut(&mut self) -> Option<&mut TabMenu> {
        match &mut self.overlay {
            Overlay::TabMenu(menu) => Some(menu),
            _ => None,
        }
    }

    pub fn close_tab_menu(&mut self) {
        if matches!(self.overlay, Overlay::TabMenu(_)) {
            self.overlay = Overlay::None;
        }
    }

    pub fn begin_tab_rename_from_menu(&mut self) -> Option<()> {
        let Overlay::TabMenu(menu) = std::mem::take(&mut self.overlay) else {
            return None;
        };
        self.overlay = Overlay::TabRename(TabRenameInput::new(
            menu.dir().to_path_buf(),
            menu.tab(),
            menu.label().to_string(),
        ));
        Some(())
    }

    pub fn tab_rename(&self) -> Option<&TabRenameInput> {
        match &self.overlay {
            Overlay::TabRename(input) => Some(input),
            _ => None,
        }
    }

    pub fn tab_rename_mut(&mut self) -> Option<&mut TabRenameInput> {
        match &mut self.overlay {
            Overlay::TabRename(input) => Some(input),
            _ => None,
        }
    }

    pub fn cancel_tab_rename(&mut self) {
        if matches!(self.overlay, Overlay::TabRename(_)) {
            self.overlay = Overlay::None;
        }
    }

    pub fn confirm_tab_rename(&mut self) -> Option<(PathBuf, usize, String)> {
        match std::mem::take(&mut self.overlay) {
            Overlay::TabRename(input) => Some(input.confirm()),
            other => {
                self.overlay = other;
                None
            }
        }
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
    /// 選択 keys to its own methods ([`RenameInput::push_char`] etc.).
    pub fn rename_mut(&mut self) -> Option<&mut RenameInput> {
        match &mut self.overlay {
            Overlay::Rename(input) => Some(input),
            _ => None,
        }
    }

    /// Cancel inline renaming, staying in 選択. Called only while the rename input
    /// is the open overlay, so it clears the overlay outright.
    pub fn rename_cancel(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Accept the inline rename: close the input and return the target session
    /// name together with the typed label (trimmed), for the event loop to
    /// persist (see [`RenameInput::confirm`]). A no-op (returning `None`) when
    /// not renaming.
    pub fn overview_confirm_rename(&mut self) -> Option<(String, String)> {
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

    /// The note recorded for the row named `name`, if any: the workspace root's
    /// note for [`ROOT_NAME`], otherwise the session's. Looked up in the recorded
    /// sessions / the root note (the sidebar list carries only the worktree rows),
    /// so the editor opens pre-filled with what is on disk.
    fn session_note(&self, name: &str) -> Option<&str> {
        if name == ROOT_NAME {
            return self.root_note();
        }
        self.sessions
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.note())
    }

    /// The note of the row highlighted in 選択 (the cursor row): the workspace
    /// root's note on the root row, the session's note otherwise — `None` when the
    /// highlighted row carries no note. Read by the right-pane renderer so the
    /// highlighted row's note (its next-time TODO) shows the moment it is selected
    /// — without opening the editor.
    pub fn selected_session_note(&self) -> Option<&str> {
        self.session_note(self.list.selected_name())
    }

    /// The highlighted session's read-only note when its overlay is currently
    /// shown in 選択 (Overview), else `None`: it shows when the cursor is on a
    /// session that has a note and no note *editor* is open (the editor takes
    /// over the overlay). The right-pane renderer draws the note exactly when
    /// this is `Some` — so its absence is a genuine path, not a dead branch
    /// behind a separate predicate.
    pub fn visible_overview_note(&self) -> Option<&str> {
        if self.mode != Mode::Switch || matches!(self.overlay, Overlay::Note(_)) {
            return None;
        }
        self.selected_session_note()
    }

    /// The worktree whose PR popup is pinned open (by index in the list's
    /// worktrees), or `None`. Read by the renderer to float the session's
    /// `#<number>` list beside its row.
    pub fn pr_popup(&self) -> Option<usize> {
        self.pr_popup
    }

    /// Pin the PR popup to worktree `target` (or close it with `None`), returning
    /// whether the target changed — the loop repaints only then, so re-pinning the
    /// same session (or a no-op close) costs no redraw.
    pub fn set_pr_popup(&mut self, target: Option<usize>) -> bool {
        let changed = self.pr_popup != target;
        self.pr_popup = target;
        changed
    }

    /// Open the note editor for `target`, pre-filled with its current note.
    /// `reattach` records whether closing it should re-attach the session's pane
    /// (没入's `Ctrl-E`); `false` for 選択's `n`.
    fn open_note_for(&mut self, target: String, reattach: bool) {
        let initial = self.session_note(&target).unwrap_or_default().to_string();
        self.overlay = Overlay::Note(NoteEditor::new(target, &initial, reattach));
    }

    /// Begin editing the selected row's note in 選択 (Overview): open the note editor
    /// pre-filled with its current note. Works on the `⌂ root` row too (it edits
    /// the workspace root's note), as well as a session row. A no-op only when an
    /// editor is already open. Returns whether the editor opened.
    pub fn overview_begin_note(&mut self) -> bool {
        if matches!(self.overlay, Overlay::Note(_)) {
            return false;
        }
        let target = self.list.selected_name().to_string();
        self.open_note_for(target, false);
        true
    }

    /// Open the note editor for the focused (active) row — the `Ctrl-E` action in
    /// 集中 (Closeup) and 没入 (Attached). `reattach` records whether closing the
    /// editor should re-attach the row's pane: `true` from 没入 (drop back into the
    /// live terminal), `false` from 集中 (return to the action surface). Works on
    /// the `⌂ root` row too (it edits the workspace root's note). A no-op only when
    /// an editor is already open. Returns whether the editor opened.
    pub fn open_focused_note(&mut self, reattach: bool) -> bool {
        if matches!(self.overlay, Overlay::Note(_)) {
            return false;
        }
        let name = self.focused_session_name();
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

    /// Open the workspace-env editor (the `env` command) over the palette, seeded
    /// from the workspace's current bindings. Replaces any open overlay.
    pub fn open_env_editor(&mut self, env: crate::domain::settings::SecretEnv) {
        self.overlay = Overlay::Env(EnvEditor::new(&env));
    }

    /// The open env editor, when any — its buffer and caret are read through it.
    pub fn env_editor(&self) -> Option<&EnvEditor> {
        match &self.overlay {
            Overlay::Env(editor) => Some(editor),
            _ => None,
        }
    }

    /// The open env editor for editing, when any: the event loop routes its keys
    /// to the buffer's own methods (via [`EnvEditor::area_mut`]).
    pub fn env_editor_mut(&mut self) -> Option<&mut EnvEditor> {
        match &mut self.overlay {
            Overlay::Env(editor) => Some(editor),
            _ => None,
        }
    }

    /// Cancel the env editor, discarding the edits and returning to the command palette.
    /// Called only while the env editor is the open overlay, so it clears the
    /// overlay outright (the palette stays open beneath it).
    pub fn env_editor_cancel(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Accept the env edit: close the editor and return the parsed bindings for
    /// the event loop to persist. A no-op (returning `None`) when not editing.
    pub fn confirm_env_editor(&mut self) -> Option<crate::domain::settings::SecretEnv> {
        match std::mem::take(&mut self.overlay) {
            Overlay::Env(editor) => Some(editor.bindings()),
            other => {
                self.overlay = other;
                None
            }
        }
    }

    // --- 集中 (Closeup) ------------------------------------------------------

    /// Enter 集中 (Closeup) on the session at `row` (0 is the root row): make it the
    /// active and selected row, switch to the right-pane action surface, and reset
    /// the menu cursor and prompt buffer.
    pub fn enter_closeup(&mut self, row: usize) {
        self.list.focus_index(row);
        self.list.activate_selected();
        self.touch_active(Utc::now());
        self.enter_closeup_surface();
    }

    /// Switch into 集中 (Closeup) on the already-positioned session: enter the mode
    /// and reset the right-pane action surface (close any inline create input,
    /// reset the menu cursor and prompt, land on the "+ new" tab). The cursor must
    /// already point at the target session; [`enter_closeup`](Self::enter_closeup) and
    /// [`enter_closeup_named`](Self::enter_closeup_named) differ only in how they get
    /// there.
    fn enter_closeup_surface(&mut self) {
        self.mode = Mode::Closeup;
        self.closeup_attached = false;
        self.overlay.clear_create();
        self.closeup_menu.reset();
        self.closeup_menu_filter = None;
        self.closeup_prompt.clear();
        self.closeup_new_tab = true;
        self.closeup_action_over_pane = false;
        // A fresh 集中 entry is not the zoom-out-from-没入 path, so the one-shot
        // return-to-pane arming never carries into it.
        self.closeup_return_attach = false;
        // Enter 集中 with no `Ctrl-O` leader pending, so the first key is read
        // as itself rather than as a stale prefix's second key.
        self.prefix_pending = false;
    }

    /// Enter 集中 (Closeup) on the session named `name`, returning whether one
    /// matched. Like [`enter_closeup`](Self::enter_closeup) but addressing the session
    /// by branch rather than row, so a freshly created session can be focused
    /// against the just-refreshed list without computing its row. A no-op
    /// (returning `false`, leaving the mode untouched) when no session matches.
    pub fn enter_closeup_named(&mut self, name: &str) -> bool {
        if !self.list.select_by_name(name) {
            return false;
        }
        self.touch_active(Utc::now());
        self.enter_closeup_surface();
        true
    }

    /// Enter 集中 (Closeup) on the session named `name`, landing on its **existing**
    /// live pane (whatever tab the pool has active) rather than the trailing
    /// "+ new" action surface — the mirror of [`enter_closeup_named`] used when the
    /// user did not ask for a fresh tab. Falls back to the "+ new" surface for an
    /// idle session (no live pane), where [`closeup_on_new_tab`](Self::closeup_on_new_tab)
    /// is forced on anyway. Returns whether a session matched.
    ///
    /// Used by the auto-focus a finished `close` requests: the neighbouring
    /// session opens in the state it was left in (its running agent/terminal),
    /// not a new-tab prompt.
    pub fn enter_closeup_named_existing(&mut self, name: &str) -> bool {
        if !self.enter_closeup_named(name) {
            return false;
        }
        // `enter_closeup_surface` lands on the "+ new" tab; drop that so the
        // session's existing pane shows (an idle one has no pane, so
        // `closeup_on_new_tab` stays true and the action surface shows regardless).
        self.closeup_new_tab = false;
        true
    }

    /// The row the previously focused session now sits at — the target `Ctrl-^`
    /// jumps to (vim's `Ctrl-^` / tmux's `last-window`) — or `None` when no other
    /// session has been focused yet or the previous one has since been removed.
    /// Delegates to the list, which records it whenever [`enter_closeup`] moves the
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

    /// The session that should receive focus after the currently focused session
    /// is closed: the nearest session visually above it, or (when none exists)
    /// the nearest session below it. Root rows are skipped so a close never lands
    /// on `⌂ root` merely because the closed session was the first row.
    pub fn focus_target_after_close(&self) -> Option<String> {
        let refs = self.list.refs();
        let active = refs.iter().position(|r| r.active && r.name != ROOT_NAME)?;
        refs[..active]
            .iter()
            .rev()
            .chain(refs[active + 1..].iter())
            .find(|r| r.name != ROOT_NAME)
            .map(|r| r.name.clone())
    }

    /// Leave 集中 for the base 選択 (Overview) — the default mode.
    pub fn leave_closeup(&mut self) {
        self.enter_switch();
    }

    /// The Session-scope commands the 集中 menu lists, in alphabetical order
    /// (see [`sorted_session_menu_commands`]). The prompt-taking `ai <prompt>` is kept out of
    /// the pickable menu (it needs typed arguments; use the Prompt UI). `chat` is
    /// filtered out unless the local LLM is usable (enabled and its model pulled),
    /// so it only appears when a reply would actually work. `close` / `diff` are
    /// filtered out on the root row, which belongs to no session. `agent` always
    /// stays: a session can hold one agent pane per CLI.
    ///
    /// Resolved for the **active** row: 集中 acts on the session it focused. When
    /// the menu filter (`/`) is active the list is narrowed by
    /// [`filter_closeup_menu`](Self::filter_closeup_menu) so both the renderer and key
    /// routing operate on the same surviving commands.
    pub fn closeup_menu_commands(&self) -> Vec<CommandInfo> {
        let commands = self.menu_commands_for_root(self.list.root_active());
        self.filter_closeup_menu(commands)
    }

    /// Narrow a Session-scope command list by the live 集中 menu filter (`/`): an
    /// absent or empty filter keeps every command; otherwise only the commands
    /// whose name starts with the typed text (case-insensitive) survive, mirroring
    /// the command registry's prefix completion. The 選択 preview
    /// ([`preview_menu_commands`](Self::preview_menu_commands)) deliberately skips
    /// this — the filter is a 集中 interaction and is cleared on every 集中 entry.
    fn filter_closeup_menu(&self, commands: Vec<CommandInfo>) -> Vec<CommandInfo> {
        let Some(query) = self
            .closeup_menu_filter
            .as_deref()
            .filter(|q| !q.is_empty())
        else {
            return commands;
        };
        let needle = query.to_lowercase();
        commands
            .into_iter()
            .filter(|info| info.name.to_lowercase().starts_with(&needle))
            .collect()
    }

    /// Shared body of [`closeup_menu_commands`]: the
    /// Session-scope commands in alphabetical order (see
    /// [`sorted_session_menu_commands`]): the prompt-taking `ai` is kept out of the menu,
    /// `chat` is gated on local-LLM availability, and the session-only `close` /
    /// `diff` are hidden when `root` (the row belongs to no session).
    fn menu_commands_for_root(&self, root: bool) -> Vec<CommandInfo> {
        self.session_menu_commands
            .iter()
            .copied()
            .filter(|info| info.name != "ai")
            .filter(|info| info.name != "chat" || self.ai_available)
            .filter(|info| !matches!(info.name, "close" | "diff") || !root)
            .collect()
    }

    /// Whether the focused session already has a live `agent` pane — a tab the
    /// session's published [`TabStrip`] labels `agent` (or `agent N` when several
    /// agents run). `ai <prompt>` uses this to skip its installed-CLI gate,
    /// delivering the prompt to that pane (whatever CLI it runs) rather than
    /// freshly spawning the default.
    pub fn agent_tab_open(&self) -> bool {
        self.terminal.tabs.as_ref().is_some_and(|strip| {
            strip
                .labels
                .iter()
                .any(|label| label == "agent" || label.starts_with("agent "))
        })
    }

    /// How many live panes the focused session publishes (the leading 集中 tabs),
    /// from the surface's tab strip — `0` when none are live (an idle session).
    fn closeup_pane_count(&self) -> usize {
        self.terminal.tabs.as_ref().map_or(0, |t| t.labels.len())
    }

    /// The active pane index the focused session's tab strip publishes (`0` when
    /// no panes are live). The pane preview shows this pane, so the tab selector
    /// rides it rather than tracking a duplicate index of its own.
    fn closeup_active_pane(&self) -> usize {
        self.terminal.tabs.as_ref().map_or(0, |t| t.active)
    }

    /// Whether 集中's tab selector is on the trailing "+ new" tab — the action
    /// surface (menu / prompt) that launches a pane — rather than an existing
    /// live pane. Always true when the session has no live panes, since the
    /// "+ new" tab is then the only one.
    pub fn closeup_on_new_tab(&self) -> bool {
        self.closeup_new_tab || self.closeup_pane_count() == 0
    }

    /// Whether the 集中 (Closeup) action surface — the [Menu or the
    /// Prompt](SessionActionUi) — is currently presented as a floating overlay
    /// modal (centred over the right pane) rather than drawn inline in the pane.
    ///
    /// Both surfaces float: the setting only picks *which* surface the box holds
    /// (a command list or a command line), not whether it floats. It holds only
    /// while 集中 actually shows that surface: on the trailing "+ new" tab (which
    /// [`closeup_on_new_tab`] also reports for an idle session with no live panes),
    /// or floating over the selected pane tab after a zoom-out (see
    /// [`closeup_action_over_active_pane`](Self::closeup_action_over_active_pane)).
    ///
    /// It yields to whatever else has claimed the screen so the floating box
    /// never fights another surface for the pane: the momentary loading indicator,
    /// and any open overlay ([`Overlay`] — the note editor, a text modal a menu
    /// command opened, the Markdown preview / diff view, …) or the `:` command
    /// palette, each of which captures the keyboard and draws its own box.
    ///
    /// The renderer floats the surface when this holds and [`closeup_pane`] leaves
    /// the pane behind it clear, so the two read the one predicate and never
    /// disagree on where the surface is drawn.
    ///
    /// [`closeup_on_new_tab`]: Self::closeup_on_new_tab
    /// [`closeup_pane`]: super::ui::panes
    pub fn closeup_action_overlay(&self) -> bool {
        self.mode == Mode::Closeup
            && !self.closeup_attached
            && (self.closeup_on_new_tab() || self.closeup_action_over_pane)
            && self.loading().is_none()
            && matches!(self.overlay, Overlay::None)
            && !self.command_palette_open()
    }

    /// Whether the 集中 action surface currently floats over the selected pane tab
    /// (rather than living on the "+ new" tab) — the zoomed-out-from-没入 state
    /// set by [`closeup_action_over_active_pane`](Self::closeup_action_over_active_pane).
    /// Key routing reads this so the floating surface keeps the keyboard while a
    /// pane tab is selected beneath it.
    pub fn closeup_action_over_pane(&self) -> bool {
        self.closeup_action_over_pane
    }

    /// Keep the 集中 action surface (Menu or Prompt) floating over the pane tab a
    /// zoom-out left (`Ctrl-T` / `Ctrl-O a`): step the selector off the "+ new"
    /// tab [`leave_attached`](Self::leave_attached) landed on, so the pane's own
    /// tab stays selected — its live preview keeps showing behind the floating
    /// box and the strip never grows a "+ new" chip for a tab that was never
    /// created.
    pub fn closeup_action_over_active_pane(&mut self) {
        self.closeup_new_tab = false;
        self.closeup_action_over_pane = true;
    }

    /// Select the currently active pane tab in 集中 without showing the action
    /// surface over it. Used when launching a new tab: the pending tab becomes the
    /// selected tab immediately, and its body is the loading indicator rather than
    /// the `+ new` launch surface.
    pub fn closeup_select_active_pane_tab(&mut self) {
        self.closeup_new_tab = false;
        self.closeup_action_over_pane = false;
    }

    /// Dismiss the action surface floating over a pane tab, returning whether it
    /// was up. The 集中 `Esc` handler consumes this after the one-shot re-attach
    /// bit: a dismissed surface leaves the selected pane's preview showing, one
    /// step short of leaving 集中.
    pub fn close_closeup_action_over_pane(&mut self) -> bool {
        std::mem::take(&mut self.closeup_action_over_pane)
    }

    /// Move 集中's tab selector to the next tab, wrapping through the live panes
    /// and the trailing "+ new" tab (`[pane 0 … pane n-1, + new]`). Returns the
    /// pane index to make active (for the caller to apply to the terminal pool) on
    /// landing on a pane tab, or `None` when it lands on the "+ new" tab (or the
    /// session has no panes, leaving the selector on "+ new").
    pub fn closeup_tab_next(&mut self) -> Option<usize> {
        // Walking the strip is browsing previews: any floating menu steps aside.
        self.closeup_action_over_pane = false;
        let panes = self.closeup_pane_count();
        if panes == 0 {
            self.closeup_new_tab = true;
            return None;
        }
        if self.closeup_on_new_tab() {
            // "+ new" wraps to the first pane.
            self.closeup_new_tab = false;
            Some(0)
        } else if self.closeup_active_pane() + 1 >= panes {
            // The last pane steps onto the "+ new" tab.
            self.closeup_new_tab = true;
            None
        } else {
            Some(self.closeup_active_pane() + 1)
        }
    }

    /// Move 集中's tab selector to the previous tab, wrapping through the live
    /// panes and the trailing "+ new" tab (the mirror of [`closeup_tab_next`]).
    /// Returns the pane index to make active on landing on a pane tab, or `None`
    /// when it lands on the "+ new" tab.
    ///
    /// [`closeup_tab_next`]: Self::closeup_tab_next
    pub fn closeup_tab_prev(&mut self) -> Option<usize> {
        // Walking the strip is browsing previews: any floating menu steps aside.
        self.closeup_action_over_pane = false;
        let panes = self.closeup_pane_count();
        if panes == 0 {
            self.closeup_new_tab = true;
            return None;
        }
        if self.closeup_on_new_tab() {
            // "+ new" wraps back to the last pane.
            self.closeup_new_tab = false;
            Some(panes - 1)
        } else if self.closeup_active_pane() == 0 {
            // The first pane steps back onto the "+ new" tab.
            self.closeup_new_tab = true;
            None
        } else {
            Some(self.closeup_active_pane() - 1)
        }
    }

    /// Select a concrete live-pane tab in 集中 (Closeup), returning the clamped
    /// pane index the terminal pool should activate. Used by right-pane mouse
    /// clicks; keyboard navigation uses [`closeup_tab_next`](Self::closeup_tab_next)
    /// / [`closeup_tab_prev`](Self::closeup_tab_prev).
    pub fn closeup_select_pane_tab(&mut self, index: usize) -> Option<usize> {
        // Clicking a tab is browsing previews: any floating menu steps aside.
        self.closeup_action_over_pane = false;
        let panes = self.closeup_pane_count();
        if panes == 0 {
            self.closeup_new_tab = true;
            return None;
        }
        self.closeup_new_tab = false;
        Some(index.min(panes - 1))
    }

    /// Discard 集中's "+ new" launch surface when it sits over live panes — the
    /// state after zooming out with `Ctrl-T` (or navigating onto "+ new") — by
    /// stepping the selector back onto the active pane's tab, so that pane
    /// previews again. Returns whether it moved: `false` (a no-op) when "+ new"
    /// is the only tab (an idle session, nothing to step back to), leaving the
    /// caller to back out of 集中 instead.
    pub fn closeup_discard_new_tab(&mut self) -> bool {
        if self.closeup_on_new_tab() && self.closeup_pane_count() > 0 {
            self.closeup_new_tab = false;
            true
        } else {
            false
        }
    }

    /// Arm the one-shot "next `Esc` re-attaches" bit, set when 集中 (Closeup) is
    /// entered by zooming *out* of a live pane (`Ctrl-T` / `Ctrl-O a`). The next
    /// `Esc` then returns to that pane (没入) instead of peeling back toward 選択.
    pub fn arm_closeup_return_attach(&mut self) {
        self.closeup_return_attach = true;
    }
    /// Take (read and clear) the one-shot return-to-pane bit. The 集中 `Esc`
    /// handler consumes it to decide whether to re-attach; any other key clears it
    /// first via [`clear_closeup_return_attach`](Self::clear_closeup_return_attach), so
    /// only an immediate `Esc` after the zoom-out re-attaches.
    pub fn take_closeup_return_attach(&mut self) -> bool {
        std::mem::take(&mut self.closeup_return_attach)
    }
    /// Clear the one-shot return-to-pane bit. Called for every non-`Esc` key
    /// handled in 集中 so any deliberate action cancels the pending re-attach.
    pub fn clear_closeup_return_attach(&mut self) {
        self.closeup_return_attach = false;
    }

    /// The 集中 menu cursor (which Session-scope command is highlighted).
    pub fn closeup_menu_cursor(&self) -> usize {
        self.closeup_menu.cursor()
    }

    /// The live 集中 menu filter (`/`) text, or `None` when the menu lists every
    /// command. The renderer draws a filter line (rather than the `Run a command:`
    /// label) while this is `Some`.
    pub fn closeup_menu_filter(&self) -> Option<&str> {
        self.closeup_menu_filter.as_deref()
    }

    /// Whether the 集中 menu is in filter mode (`/`): typed characters narrow the
    /// command list rather than driving the single-key shortcuts (`t` / `a` / `C`).
    pub fn closeup_menu_filtering(&self) -> bool {
        self.closeup_menu_filter.is_some()
    }

    /// Enter 集中 menu filter mode (`/`) from an empty query, homing the cursor on
    /// the first command. A no-op while already filtering, so a stray `/` typed
    /// into an active filter is ignored rather than wiping what was typed.
    pub fn start_closeup_menu_filter(&mut self) {
        if self.closeup_menu_filter.is_none() {
            self.closeup_menu_filter = Some(String::new());
            self.closeup_menu.reset_cursor();
        }
    }

    /// Append a character to the 集中 menu filter and re-home the cursor on the
    /// first surviving match. A no-op when the menu is not filtering.
    pub fn push_closeup_menu_filter(&mut self, c: char) {
        if let Some(query) = self.closeup_menu_filter.as_mut() {
            query.push(c);
            self.closeup_menu.reset_cursor();
        }
    }

    /// Delete the last character of the 集中 menu filter (`Backspace`), re-homing
    /// the cursor. A no-op when not filtering or the query is already empty; filter
    /// mode is left with `Esc` ([`clear_closeup_menu_filter`]), not by backspacing
    /// past the start.
    ///
    /// [`clear_closeup_menu_filter`]: Self::clear_closeup_menu_filter
    pub fn closeup_menu_filter_backspace(&mut self) {
        if let Some(query) = self.closeup_menu_filter.as_mut() {
            query.pop();
            self.closeup_menu.reset_cursor();
        }
    }

    /// Leave 集中 menu filter mode (`Esc`), returning whether it was filtering — so
    /// the `Esc` handler treats the key as consumed only then, peeling the filter
    /// before it steps back out of 集中. The list returns to its full listing.
    pub fn clear_closeup_menu_filter(&mut self) -> bool {
        self.closeup_menu_filter.take().is_some()
    }

    /// Whether any 集中 menu row is expanded into an inline picker (agent /
    /// terminal / close).
    pub fn closeup_menu_expanded(&self) -> bool {
        self.closeup_menu.is_expanded()
    }

    /// The highlighted agent in the 集中 menu's agent picker, or `None` when the
    /// picker is collapsed (or there are no installed agents to pick from).
    pub fn closeup_menu_agent_cursor(&self) -> Option<usize> {
        self.closeup_menu
            .agent_cursor()
            .filter(|_| !self.installed_agents.is_empty())
    }

    /// Whether the 集中 menu's `terminal` row is expanded into the open/new
    /// picker.
    pub fn closeup_menu_terminal_expanded(&self) -> bool {
        self.closeup_menu.terminal_cursor().is_some()
    }

    /// The highlighted terminal action in the 集中 menu's terminal picker, or
    /// `None` when the picker is collapsed.
    pub fn closeup_menu_terminal_cursor(&self) -> Option<usize> {
        self.closeup_menu.terminal_cursor()
    }

    /// Whether the 集中 menu's `agent` row can expand into the picker: the cursor
    /// is on `agent` and more than one CLI is installed (so there is a choice).
    pub fn closeup_menu_agent_can_expand(&self) -> bool {
        self.installed_agents.len() > 1
            && self
                .closeup_selected_command()
                .is_some_and(|info| info.name == "agent")
    }

    /// Whether the 集中 menu's `terminal` row can expand into the open/new
    /// picker. It always has two choices; expansion is gated only by the cursor.
    pub fn closeup_menu_terminal_can_expand(&self) -> bool {
        self.closeup_selected_command()
            .is_some_and(|info| info.name == "terminal")
    }

    /// Expand the 集中 menu's agent picker, highlighting the configured default
    /// agent's position in the installed list (or the top when it is not
    /// installed). No-op unless [`closeup_menu_agent_can_expand`] holds.
    ///
    /// [`closeup_menu_agent_can_expand`]: Self::closeup_menu_agent_can_expand
    pub fn closeup_menu_expand_agent(&mut self) {
        if !self.closeup_menu_agent_can_expand() {
            return;
        }
        let default_index = self
            .installed_agents
            .iter()
            .position(|&cli| cli == self.default_agent)
            .unwrap_or(0);
        self.closeup_menu
            .expand(CloseupSubmenu::Agent, default_index);
    }

    /// Expand the 集中 menu's terminal picker, highlighting `open` (the default
    /// embedded-pane action).
    pub fn closeup_menu_expand_terminal(&mut self) {
        if !self.closeup_menu_terminal_can_expand() {
            return;
        }
        self.closeup_menu.expand(CloseupSubmenu::Terminal, 0);
    }

    /// Collapse the 集中 menu's inline picker, returning whether one was expanded
    /// (so the caller treats `←` / `Esc` as consumed only then).
    pub fn closeup_menu_collapse_agent(&mut self) -> bool {
        self.closeup_menu.collapse()
    }

    /// Whether the 集中 menu's `close` row is expanded into the close picker.
    pub fn closeup_close_expanded(&self) -> bool {
        self.closeup_menu.is_close_expanded()
    }

    /// The highlighted option in the 集中 menu's close picker, or `None` collapsed.
    /// `Some(0)` = plain close, `Some(1)` = close --force.
    pub fn closeup_close_cursor(&self) -> Option<usize> {
        self.closeup_menu.close_cursor()
    }

    /// Whether the 集中 menu's `close` row can expand: the cursor is on `close`.
    pub fn closeup_close_can_expand(&self) -> bool {
        self.closeup_selected_command()
            .is_some_and(|info| info.name == "close")
    }

    /// Expand the 集中 menu's close picker, starting at option 0 (plain close).
    /// No-op unless the cursor is on the `close` row.
    pub fn closeup_menu_expand_close(&mut self) {
        if !self.closeup_close_can_expand() {
            return;
        }
        self.closeup_menu.expand_close();
    }

    /// Collapse the 集中 menu's close picker, returning whether it was expanded.
    pub fn closeup_menu_collapse_close(&mut self) -> bool {
        self.closeup_menu.collapse_close()
    }

    /// Whether the selected close-picker option is `--force`. Call only while
    /// the close picker is expanded ([`closeup_close_expanded`] is true), which
    /// guarantees `close_cursor` is `Some`.
    ///
    /// [`closeup_close_expanded`]: Self::closeup_close_expanded
    pub fn closeup_menu_selected_close_force(&self) -> bool {
        self.closeup_menu.close_selected() == 1
    }

    /// The agent CLI highlighted in the picker, or `None` when collapsed / there
    /// are none installed. Used to launch the chosen CLI on `Enter`.
    pub fn closeup_menu_selected_agent(&self) -> Option<AgentCli> {
        self.closeup_menu.agent_cursor()?;
        self.installed_agents
            .get(
                self.closeup_menu
                    .agent_selected(self.installed_agents.len()),
            )
            .copied()
    }

    /// The terminal action highlighted in the picker (`open` / `new`), or `None`
    /// when the picker is collapsed.
    pub fn closeup_menu_selected_terminal_action(&self) -> Option<&'static str> {
        self.closeup_menu.terminal_cursor()?;
        TERMINAL_MENU_ACTIONS
            .get(
                self.closeup_menu
                    .terminal_selected(TERMINAL_MENU_ACTIONS.len()),
            )
            .copied()
    }

    /// The terminal actions shown below the expanded terminal row.
    pub fn closeup_menu_terminal_actions(&self) -> &'static [&'static str] {
        &TERMINAL_MENU_ACTIONS
    }

    /// Move the 集中 menu cursor up one row, wrapping (delegated to [`CloseupMenu`],
    /// which keeps it underflow-safe). Acts on the active picker while expanded.
    pub fn closeup_menu_move_up(&mut self) {
        let count = self.closeup_menu_nav_count();
        self.closeup_menu.move_up(count);
    }

    /// Move the 集中 menu cursor down one row, wrapping (delegated to [`CloseupMenu`]).
    pub fn closeup_menu_move_down(&mut self) {
        let count = self.closeup_menu_nav_count();
        self.closeup_menu.move_down(count);
    }

    /// The row count the menu cursor wraps against: the installed agents while the
    /// agent picker is expanded, 2 while the close picker is expanded, otherwise
    /// the Session-scope commands.
    fn closeup_menu_nav_count(&self) -> usize {
        if self.closeup_menu.is_expanded() {
            if self.closeup_menu_terminal_expanded() {
                TERMINAL_MENU_ACTIONS.len()
            } else if self.closeup_menu.is_close_expanded() {
                2
            } else {
                self.installed_agents.len()
            }
        } else {
            self.closeup_menu_commands().len()
        }
    }

    /// The 集中 command under the menu cursor, clamped to the available commands,
    /// or `None` when no Session-scope command is available.
    ///
    /// `CloseupMenu::selected` clamps to `len - 1`, which is `0` for an empty list
    /// — so indexing directly would panic if the registry ever yielded no
    /// Session-scope commands. Returning `Option` keeps the caller a no-op in
    /// that case instead of crashing (and unwinding) the TUI.
    pub fn closeup_selected_command(&self) -> Option<CommandInfo> {
        let commands = self.closeup_menu_commands();
        commands
            .get(self.closeup_menu.selected(commands.len()))
            .copied()
    }

    /// The 集中 prompt buffer (the session-scoped command line).
    pub fn closeup_prompt(&self) -> &str {
        self.closeup_prompt.value()
    }

    /// Whether the 集中 Prompt is the surface currently capturing keys: the
    /// action UI is [`SessionActionUi::Prompt`] and its floating command line is
    /// up — on the trailing "+ new" tab or floating over a pane after a zoom-out
    /// (the two states [`closeup_action_overlay`](Self::closeup_action_overlay) draws
    /// the box in). In that state `End` and `?` are literal edits to the command
    /// line rather than their usual note / cheat-sheet bindings, so a
    /// session-scoped command can contain them.
    pub fn closeup_prompt_capturing(&self) -> bool {
        self.session_action_ui == SessionActionUi::Prompt
            && (self.closeup_on_new_tab() || self.closeup_action_over_pane)
    }

    /// The caret position in the 集中 prompt, so the renderer can draw the caret
    /// where editing happens.
    pub fn closeup_prompt_cursor(&self) -> usize {
        self.closeup_prompt.cursor()
    }

    /// The 集中 prompt's editable buffer: the event loop routes its keys straight
    /// to the [`TextInput`]'s own editing methods (`insert` / `backspace` /
    /// `move_left` …), so the prompt has no per-key forwarders of its own.
    pub fn closeup_prompt_mut(&mut self) -> &mut TextInput {
        &mut self.closeup_prompt
    }

    /// Tab-complete the 集中 prompt's command word against the Session-scope
    /// commands, returning the candidates when ambiguous (so the caller can log
    /// them, mirroring the palette line's `complete`).
    pub fn closeup_prompt_complete(&mut self) -> Completion {
        let completion = self
            .registry
            .complete(self.closeup_prompt.value(), CommandScope::Session);
        self.closeup_prompt.set_value(completion.input.clone());
        if !completion.candidates.is_empty() {
            self.log
                .push(LogLine::output(completion.candidates.join("  ")));
        }
        completion
    }

    /// The advisory hint for the 集中 prompt, computed in the Session scope.
    pub fn closeup_prompt_hint(&self) -> Hint {
        self.registry
            .suggest(self.closeup_prompt.value(), CommandScope::Session)
    }

    /// Run the 集中 prompt as a Session-scope command: dispatch it, append its
    /// produced lines to the log, clear the prompt, and return the resulting
    /// [`Submission`] (so the event loop can act on `OpenTerminal` / `OpenAgent`).
    /// Empty input is a no-op.
    pub fn closeup_prompt_submit(&mut self) -> Submission {
        let entry = self.closeup_prompt.value().trim().to_string();
        self.closeup_prompt.clear();
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
        let session = match self.focused_session_name() {
            name if name == ROOT_NAME => None,
            name => Some(name),
        };
        let (result, recorded) = self.dispatch_and_record(&entry, CommandScope::Session, session);
        let effect = self.record_response(result);
        Submission {
            effect,
            recorded: Some(recorded),
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
        // `session remove` completes against qualified `workspace:session` names
        // in 統合(unite) mode (so a shared name can be disambiguated), and plain
        // session names otherwise.
        let removable_owned = self.removable_session_names();
        let removable: Vec<&str> = removable_owned.iter().map(String::as_str).collect();
        let completion = self.registry.complete_with(
            self.cmdline.value(),
            self.command_scope(),
            &session_names,
            &removable,
        );
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
        let (result, recorded) = self.dispatch_and_record(&entry, self.command_scope(), None);
        let effect = self.record_response(result);
        Submission {
            effect,
            recorded: Some(recorded),
        }
    }

    /// Dispatch `entry` as a `scope`-scoped command and record it in command
    /// history, returning the raw result. The shared core of [`submit`](Self::submit)
    /// (palette line, [`CommandScope::Workspace`]) and
    /// [`closeup_prompt_submit`](Self::closeup_prompt_submit) (集中 prompt,
    /// [`CommandScope::Session`]) so both record history identically and refuse
    /// commands outside their surface's scope; folding the result into the log is
    /// [`record_response`](Self::record_response).
    fn dispatch_and_record(
        &mut self,
        entry: &str,
        scope: CommandScope,
        session: Option<String>,
    ) -> (CommandResult, HistoryEntry) {
        let result = self.registry.dispatch_in_scope(
            entry,
            scope,
            &self.history_entries,
            &self.list.refs(),
            self.selected_group_issues(),
        );
        let success = !result.lines.iter().any(|line| line.kind == LineKind::Error);
        self.cmdline.push_history(entry.to_string());
        let recorded = HistoryEntry::now(entry, session, success);
        self.push_history_entry(recorded.clone());
        (result, recorded)
    }

    /// Append one full history entry to the display history, capping it to the
    /// same most-recent window used for command recall.
    fn push_history_entry(&mut self, entry: HistoryEntry) {
        self.history_entries.push(entry);
        let overflow = self
            .history_entries
            .len()
            .saturating_sub(MAX_COMMAND_HISTORY);
        if overflow > 0 {
            self.history_entries.drain(..overflow);
        }
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
        // Route the result to the workspace the operation targeted (an extra unite
        // group when the cursor was in one, else the primary).
        let target_group = self.take_op_target_group();
        // Apply a refreshed root note (set only by the root-note save) before the
        // rebuild, so the `⌂ root` row's memo marker reflects the edit this frame.
        let note_changed = outcome.root_note.is_some();
        if let Some(root_note) = outcome.root_note {
            match target_group {
                Some(i) => self.extra_groups[i].root_note = root_note,
                None => self.root_note = root_note,
            }
        }
        if let Some(sessions) = outcome.sessions {
            // A create / remove changed the rows: rebuild and (for a create) move
            // the cursor onto the new session.
            match target_group {
                Some(i) => self.extra_groups[i].sessions = sessions,
                None => self.sessions = sessions,
            }
            self.rebuild_list();
            if let Some(name) = outcome.select {
                self.list.select_by_name(&name);
            }
        } else if note_changed {
            // A note edit changes no session rows, so refresh the memo markers
            // without yanking the cursor back to the root row.
            self.rebuild_list_keep_cursor();
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

    /// Build the session-removal rows in display order. In single-workspace mode
    /// labels are plain session names; in 統合(unite) mode every visible
    /// workspace contributes rows labelled as `workspace: session` so duplicate
    /// session names stay distinguishable and can be removed from their own root.
    fn remove_entries(&self) -> Vec<RemoveEntry> {
        let united = self.is_united();
        let primary_name = self.list.workspace_name();
        let primary_label = united.then_some(primary_name);

        let mut entries: Vec<RemoveEntry> = self
            .sessions
            .iter()
            .map(|session| {
                RemoveEntry::new(
                    session.name.clone(),
                    self.root_path().to_path_buf(),
                    primary_label,
                )
            })
            .collect();

        for group in &self.extra_groups {
            entries.extend(group.sessions.iter().map(|session| {
                RemoveEntry::new(
                    session.name.clone(),
                    group.root_path.clone(),
                    united.then_some(group.name.as_str()),
                )
            }));
        }

        entries
    }

    /// The `<name>` argument completions for `session remove`, in display order.
    /// In single-workspace mode these are the plain session names; in 統合(unite)
    /// mode every visible workspace contributes its sessions qualified as
    /// `workspace:session`, matching the form `session remove` accepts there.
    fn removable_session_names(&self) -> Vec<String> {
        if !self.is_united() {
            return self.sessions.iter().map(|s| s.name.clone()).collect();
        }
        let primary = self.list.workspace_name();
        let mut names: Vec<String> = self
            .sessions
            .iter()
            .map(|s| format!("{primary}:{}", s.name))
            .collect();
        for group in &self.extra_groups {
            names.extend(
                group
                    .sessions
                    .iter()
                    .map(|s| format!("{}:{}", group.name, s.name)),
            );
        }
        names
    }

    /// Open the session-removal modal, seeded with the current sessions and
    /// nothing selected. `force` is carried from `session remove --force`.
    pub fn open_remove_modal(&mut self, force: bool) {
        self.overlay = Overlay::Remove(RemoveModal::new(self.remove_entries(), force));
    }

    /// Close the removal modal, discarding any selection. Called only while the
    /// removal modal is the open overlay, so it clears the overlay outright.
    pub fn cancel_remove_modal(&mut self) {
        self.overlay = Overlay::None;
    }

    /// Confirm the removal modal: close it and return the checked session
    /// entries (in display order) together with the `--force` flag, for the
    /// event loop to remove each (see [`RemoveModal::confirm`]). Returns `None`
    /// when nothing is checked, leaving the modal open; also `None` when it is
    /// closed.
    pub fn submit_remove_modal(&mut self) -> Option<(Vec<RemoveEntry>, bool)> {
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
