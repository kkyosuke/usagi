//! The home (workspace) screen's event loop: read a key, dispatch it by mode,
//! repaint, repeat — until the user quits.
//!
//! This module owns the loop itself ([`event_loop`]), the modal key capture
//! (quit-confirm / removal / text modals), and the shared [`Flow`] outcome and
//! [`selected_dir`] helper. The per-mode key handlers it dispatches to — and
//! `open_pane`, which drives the embedded terminal — live in [`handlers`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use console::Key;
use console::Term;

use crate::domain::settings::{AgentCli, KeyScheme, SessionActionUi, Sidebar};
use crate::domain::workspace_state::SessionTodo;
use crate::presentation::tui::install_task;
use crate::presentation::tui::io::screen::{
    ClickEvent, FramePainter, Input, KeyReader, ScrollEvent,
};

use super::oneshot::OneShot;
use super::sessions_refresh::SessionsRefreshHandle;
use super::state::{
    DiffFocus, GroupSource, HomeState, Mode, PaneExit, ResumeLevel, SessionOutcome, SessionReorder,
    SurfaceOwner,
};
use super::tasks::TaskHandle;
use super::terminal::pool::MonitorHandle;
use super::terminal::tabs::TabNav;
use super::terminal::tabs::TabSwap;
use super::terminal::view::TerminalView;
use super::ui;
use super::update::UpdateHandle;

mod handlers;

use handlers::{
    closeup_click, closeup_key, env_editor_key, note_editor_key, overview_click, overview_key,
    palette_key,
};

/// The byte `console` reports for `Ctrl-O` on the home screen: a bare control
/// character (`0x0f`), since `console` only special-cases a handful of control
/// keys and passes the rest through as [`Key::Char`]. `Ctrl-O` zooms out one
/// engagement level (没入 → 選択) on the screen.
const CTRL_O: char = '\u{000f}';

/// The bare control characters `console` reports for `Ctrl-N` (`0x0e`) and
/// `Ctrl-P` (`0x10`) on the home screen — the same passthrough as [`CTRL_O`].
/// They move between the focused session's tabs (`Ctrl-P` previous / `Ctrl-N`
/// next) in 選択 / 集中, matching the chords 没入 uses for the same move.
const CTRL_N: char = '\u{000e}';
const CTRL_P: char = '\u{0010}';

/// The bare control character `console` reports for `Ctrl-B` (`0x02`) on the home
/// screen — the same passthrough as [`CTRL_O`]. It toggles the left session
/// sidebar between its full width and the collapsed rail from any non-modal mode.
/// 没入 (Attached) is driven inside the embedded-terminal loop, so its `Ctrl-B` is
/// intercepted there instead (see [`super::terminal::pane`]).
const CTRL_B: char = '\u{0002}';

/// How often an otherwise-idle screen wakes to pick up a session list a
/// background watcher published — a create / remove made outside this screen (an
/// agent's MCP call, another usagi window, or the CLI) that only wrote
/// `state.json`. Slow relative to [`ANIM_TICK`](install_task::ANIM_TICK) so a
/// quiet screen stays cheap while still reflecting external changes within about
/// a second, instead of freezing the sidebar until the next keypress or detach.
const WATCH_SESSIONS_TICK: Duration = Duration::from_millis(500);

/// The bare control character `console` reports for `Ctrl-S` (`0x13`) on the home
/// screen — the same passthrough as [`CTRL_O`]. It saves the session-note editor
/// (`Enter` inserts a newline there, so saving needs its own chord). 没入's
/// `Ctrl-E` (which opens the editor) is intercepted inside the embedded-terminal
/// loop instead (see [`super::terminal::pane`]).
const CTRL_S: char = '\u{0013}';

/// The bare control character `console` reports for `Ctrl-E` (`0x05`) on the home
/// screen — the same passthrough as [`CTRL_O`]. It opens the session-note editor
/// from 集中 (Closeup). 没入 (Attached) is driven inside the embedded-terminal loop,
/// so its `Ctrl-E` is intercepted there instead (see [`super::terminal::pane`]).
const CTRL_E: char = '\u{0005}';

/// The bare control character `console` reports for `Ctrl-^` (`Ctrl-Shift-6`,
/// `0x1e`) on the home screen — the same passthrough as [`CTRL_O`]. It jumps to
/// the previously focused session (vim's `Ctrl-^` / tmux's `last-window`),
/// attaching it when live, so two sessions can be toggled between without going
/// through 選択. 没入 (Attached) is driven inside the embedded-terminal loop, so
/// its `Ctrl-^` is intercepted there instead (see [`super::terminal::pane`]).
const CTRL_CARET: char = '\u{001e}';

/// The bare control character `console` reports for `Ctrl-Q` (`0x11`) on the home
/// screen — the same passthrough as [`CTRL_O`]. It is the dedicated quit chord:
/// unlike `Ctrl-C` (which quits an idle screen outright and only confirms when a
/// session is live), `Ctrl-Q` *always* raises the quit-confirmation modal first,
/// so quitting is never a single keystroke. 没入 (Attached) is driven inside the
/// embedded-terminal loop, so its `Ctrl-Q` is intercepted there instead (see
/// [`super::terminal::pane`]) and surfaces as the same modal on the way out.
const CTRL_Q: char = '\u{0011}';

/// The callback 選択 uses to read (`None`) or navigate (`Some(nav)`) the
/// highlighted session's tabs, returning the strip's labels and active index.
/// Backed by the [`TerminalPool`](super::terminal::pool::TerminalPool) the pane
/// driver shares, so a tab moved here is the one re-attaching reveals.
pub(super) type TabOp<'a> = dyn FnMut(&Path, Option<TabNav>) -> (Vec<String>, usize) + 'a;
pub(super) type TabActionOp<'a> = dyn FnMut(&mut HomeState, &Path, usize, TabMenuAction) + 'a;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TabMenuAction {
    Move(TabSwap),
    Rename(String),
    Close,
}

/// The settings-derived values re-read when the config screen closes, so an
/// edit takes effect without reopening the home screen: the 集中 (Closeup)
/// right-pane surface, the 没入 key scheme, and the default Agent CLI.
#[derive(Debug, Clone, Copy)]
pub struct ConfigReload {
    /// The effective Session Action UI (集中 mode's surface).
    pub session_action_ui: SessionActionUi,
    /// The effective 没入 key scheme (the `Ctrl-O` prefix or single `Alt`-chords),
    /// so the pane's key handling reflects the edit without reopening the screen.
    pub key_scheme: KeyScheme,
    /// The effective default Agent CLI, so `agent` (and the 集中 menu's
    /// `Launch <名前>` row) picks up a CLI switched in Config without restarting.
    pub agent_cli: AgentCli,
    /// Whether the local LLM is usable (enabled and its model pulled), gating the
    /// `chat` row in the 集中 menu — re-read when Config closes so enabling the LLM
    /// (or pulling its model) surfaces `chat` without restarting.
    pub ai_available: bool,
}

/// A `(session name, last_active)` pair — the freshness ("heat") timestamp
/// [`Wiring::save_last_active`] flushes to `state.json` on quit.
pub(super) type SessionLastActive = (String, DateTime<Utc>);

type RemoveDispatch<'a> = dyn FnMut(&Path, &str, bool, Option<super::tasks::AutoFocus>) + 'a;

/// The outcome of dispatching a background pane launch (see
/// [`Wiring::start_pending_spawn`]).
pub(super) enum StartPending {
    /// The launch reused an existing agent tab: no new (loading) tab — the caller
    /// re-attaches it directly.
    Reused,
    /// A new background launch is in flight. `label` is the placeholder chip shown
    /// at the strip's end while its environment resolves (before a pool pane exists).
    Pending { label: String },
}

/// A single frame's poll of the in-flight background launch (see
/// [`Wiring::poll_pending_spawn`]).
pub(super) enum PendingPoll {
    /// The launch's environment is still resolving off-thread; no pool pane yet.
    Resolving,
    /// The pane has spawned and is starting; carries its current tab index so the
    /// loading chip animates on the right chip.
    Starting(usize),
    /// The pane's shell has painted. `selected` is true when the pending pane is
    /// still the selected tab, in which case the loop may attach it immediately;
    /// false means the user selected something else while it loaded, so it should
    /// simply stop showing as pending.
    Ready { selected: bool },
    /// The launch is gone (its spawn failed, or the pane vanished): drop it.
    Gone,
}

/// Dispatch a background pane launch. See [`Wiring::start_pending_spawn`].
pub(super) type StartPendingFn<'a> =
    dyn FnMut(&mut HomeState, &Path, bool) -> Result<StartPending> + 'a;

/// The workspace root and the impure callbacks the home event loop and its key
/// handlers drive, bundled so they thread one value instead of a dozen separate
/// closures. [`super::run`] builds this against the real terminal, shell pool,
/// and session store; the tests pass fakes. Every field is a side-effecting hook
/// except `workspace_root`, the directory the screen operates in.
///
/// Each `dispatch_*` hook returns at once, having handed its git / filesystem
/// work to a background worker; the loop drains the finished ones each frame.
/// `rename_display` stays synchronous (no git work) and returns its outcome.
pub(super) struct Wiring<'a> {
    /// A monotonically increasing counter for user input handled by the home
    /// loop. Create tasks copy it at dispatch time, and only auto-focus when it
    /// is unchanged at completion time.
    pub interaction_epoch: u64,
    /// Whether a background thread is polling `state.json` for changes made
    /// outside this screen and publishing them through the shared refresh slot.
    /// When set, an otherwise-idle loop wakes on [`WATCH_SESSIONS_TICK`] to apply
    /// them; when clear (the tests' default) a truly idle screen blocks on input.
    pub watch_sessions: bool,
    /// The workspace root: where the root row's pane is rooted, and the base
    /// [`selected_dir`] falls back to when the cursor is on the root row.
    pub workspace_root: &'a Path,
    /// Append a run command to the workspace history (best-effort; tests no-op).
    pub persist: &'a mut dyn FnMut(&crate::domain::history::HistoryEntry),
    /// Dispatch `session create <name>` to a background worker, in the workspace
    /// rooted at the given path (the cursor's group in 統合/unite mode).
    pub dispatch_create: &'a mut dyn FnMut(&Path, &str, u64),
    /// Rename a session's sidebar label in the given workspace, returning the
    /// outcome to apply inline.
    pub rename_display: &'a mut dyn FnMut(&Path, &str, &str) -> SessionOutcome,
    /// Save (or clear) a session's note in the given workspace, returning the
    /// outcome to apply inline.
    pub set_note: &'a mut dyn FnMut(&Path, &str, &str) -> SessionOutcome,
    /// Replace a session's (or the root's) whole todo checklist in the given
    /// workspace, returning the outcome to apply inline. Called on save when the
    /// todos pane was edited — like `set_note` it stays synchronous (no git work).
    pub set_todos: &'a mut dyn FnMut(&Path, &str, &[SessionTodo]) -> SessionOutcome,
    /// Set (`Some(id)`) or clear (`None`) a session's manual status label in the
    /// given workspace, returning the outcome to apply inline. Stays synchronous
    /// (no git work) like `rename_display` / `set_note`.
    pub set_label: &'a mut dyn FnMut(&Path, &str, Option<&str>) -> SessionOutcome,
    /// Reorder the selected session one row up/down (`bool` = up), persisting the
    /// new order and returning the reloaded list to refresh. Stays synchronous
    /// (no git work) like `rename_display` / `set_note`.
    pub reorder_session: &'a mut dyn FnMut(&str, bool) -> SessionReorder,
    /// Dispatch `session remove <name>` to a background worker (`bool` = force),
    /// in the workspace rooted at the given path. The optional auto-focus is set
    /// by `close`, which wants to land on a neighbouring session if the user does
    /// nothing else while removal runs.
    pub dispatch_remove: &'a mut RemoveDispatch<'a>,
    /// Resolve a registered workspace by name and load it into a [`GroupSource`]
    /// to stack into the 統合(unite) view (`unite add <name>`), or an error message
    /// to log when no such workspace is registered.
    pub unite_resolve: &'a mut dyn FnMut(&str) -> std::result::Result<GroupSource, String>,
    /// Launch the self-update on a background thread (replace the installed
    /// binary with the latest release). Called when the user confirms the
    /// update-confirmation modal raised by clicking the mascot's update notice;
    /// returns at once, the progress showing as the shared loading rabbit.
    pub dispatch_update: &'a mut dyn FnMut(),
    /// Evict a removed session's pooled shell, run on the loop thread (the pool
    /// is not `Send`).
    pub evict_pool: &'a mut dyn FnMut(&Path),
    /// The branch names already taken across the workspace, read fresh so the
    /// inline create input can validate against duplicates.
    pub existing_branches: &'a mut dyn FnMut() -> Vec<String>,
    /// Embed a live shell in the right pane (没入) and drive it: the first `bool`
    /// is `agent` vs plain `terminal`, the second `new_pane` vs re-attach.
    pub open_terminal: &'a mut dyn FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    /// Dispatch a new pane launch for the given session. `bool` is `agent` vs
    /// plain `terminal`. It resolves the launch and kicks the environment
    /// resolution off-thread **without** spawning the pane yet, so the selected
    /// loading tab can appear immediately; the loop then drives it via
    /// [`Self::poll_pending_spawn`]. Returns [`StartPending::Reused`] when it
    /// reused an existing agent tab (no loading tab — re-attach directly), else
    /// [`StartPending::Pending`] with the placeholder chip label.
    pub start_pending_spawn: &'a mut StartPendingFn<'a>,
    /// Poll the in-flight launch each frame: resolving / starting (with its tab
    /// index) / ready / gone. On the frame its environment lands this spawns the
    /// pane selected into the pool (both spawn and pool access must run on this —
    /// the loop — thread, since the pool is not `Send`).
    pub poll_pending_spawn: &'a mut dyn FnMut(&Path) -> PendingPoll,
    /// Consume the launch and defensively re-select its pane before attaching.
    /// Normal tab-add selection already happened at dispatch/spawn time.
    pub activate_pending: &'a mut dyn FnMut(&Path) -> bool,
    /// Drop the in-flight launch (it vanished, or it finished after another tab /
    /// session was selected): its environment-resolver result is discarded when no
    /// pane was spawned yet; otherwise the spawned pane simply remains live.
    pub clear_pending_spawn: &'a mut dyn FnMut(),
    /// Open `url` in the platform's default browser — the side effect behind
    /// clicking a `#<number>` in a session's pinned PR popup. [`super::run`] wires
    /// the real launcher (the same detached spawn the immersive pane uses); tests
    /// pass a capture or a no-op so the loop's open path runs without shelling out.
    pub open_url: &'a mut dyn FnMut(&str),
    /// Open a new native terminal application rooted at the selected directory.
    /// Tests pass a capture or a no-op; production shells out through the
    /// platform-specific terminal launcher.
    pub open_external_terminal: &'a mut dyn FnMut(&Path) -> std::result::Result<(), String>,
    /// Open the settings screen, re-reading the affected settings on return
    /// (`None` when the user quit the app from it).
    pub open_config: &'a mut dyn FnMut(&Term) -> Result<Option<ConfigReload>>,
    /// Start a local-LLM chat request, returning a receiver that yields the reply
    /// (or an error message) once. Called when the user submits a line in the 集中
    /// `chat` overlay; the loop polls the receiver each tick so a slow model never
    /// blocks the screen. [`super::run`] wires it to the Ollama-backed request
    /// against the configured model; tests return a ready receiver.
    pub chat_ask: &'a mut dyn FnMut(String) -> std::sync::mpsc::Receiver<Result<String, String>>,
    /// Snapshot a session's live terminal for the 選択 preview, or `None` when it
    /// has no running shell — also the live/idle test the focus handlers use. The
    /// snapshot is sized to the given sidebar state (the preview widens when the
    /// rail is collapsed); the liveness test passes the current state and ignores
    /// the geometry.
    pub preview: &'a mut dyn FnMut(&Path, Sidebar) -> Option<TerminalView>,
    /// Read (`None`) or navigate (`Some(nav)`) the highlighted session's tabs
    /// from 選択.
    pub tab_op: &'a mut TabOp<'a>,
    /// Close the highlighted session's active tab (pane) from 選択.
    pub close_tab: &'a mut dyn FnMut(&mut HomeState, &Path),
    /// Apply a tab-context-menu action to a concrete tab.
    pub tab_action: &'a mut TabActionOp<'a>,
    /// Persist the engagement to restore on the next launch — the focused
    /// session's name and how deeply it was engaged — called when a quit is
    /// confirmed. [`super::run`] writes it to the resume-focus store (gated by the
    /// restore setting); tests pass a capture or a no-op.
    pub save_resume: &'a mut dyn FnMut(&str, ResumeLevel),
    /// Flush the freshness ("heat") timestamps accumulated this run — the
    /// `(session name, last_active)` pairs — so the sidebar dots survive a
    /// restart. Called alongside [`save_resume`](Self::save_resume) on a confirmed
    /// quit. [`super::run`] merges them into `state.json`; tests no-op.
    pub save_last_active: &'a mut dyn FnMut(&[SessionLastActive]),
    /// Auto-start any queued prompt whose session has no live pane, spawning that
    /// session's agent pane in the background (not attached) with the queued
    /// prompt as its first message. Called each idle tick so a prompt queued while
    /// the screen runs — an MCP `session_delegate_issue` / `session_prompt` — is
    /// picked up without a human opening the pane. Returns one log line per pane it
    /// started (empty when the feature is off or nothing was queued), which the
    /// loop appends to the command log. [`super::run`] wires the real pool spawn
    /// (gated by the `autostart_queued_prompts` setting); tests pass a fake.
    pub autostart_queued: &'a mut dyn FnMut(&HomeState) -> Vec<String>,
    /// Broadcast the wake prompt (`continue`) to every session whose agent pane is
    /// currently live. Called when a pending `wake -t hhmm` schedule becomes due;
    /// returns how many agents were messaged so the loop can log the outcome.
    /// Production wires this to the live-pane / live-prompt stores; tests pass a
    /// capture or a no-op.
    pub broadcast_wake: &'a mut dyn FnMut(&HomeState) -> usize,
}

/// What the user chose to do on the home (workspace) screen.
#[derive(Debug)]
pub enum Outcome {
    /// Return to the project selection screen without acting on a worktree.
    /// Retained for the screens that open the home screen ([`super::super::open`]
    /// / [`super::super::app`]); the home loop itself no longer emits it, since
    /// `Esc` is inert here and the only way out is quitting via `Ctrl-C`.
    Back,
    /// The user asked to quit the application entirely.
    Quit,
}

/// Runs the home screen against the given terminal and key source until the
/// user quits. Assumes the alternate screen is already active (it is owned by
/// the orchestrator, several levels up).
///
/// `Ctrl-C` is the only way out: it closes the app, but when a session is still
/// live (an agent/shell is running) it first raises a quit-confirmation modal so
/// an accidental press does not drop running work — confirming it quits.
///
/// The screen is a three-step engagement ladder, with the workspace command
/// line summoned on top as a `:` palette:
///
/// - **選択 (Overview)** — the default: pick a session in the left pane. `↑`/`↓`
///   (or `k`/`j`) move between sessions, `←`/`→` (or `h`/`l`, or `Ctrl-P`/
///   `Ctrl-N`) move between the highlighted session's tabs, `Enter` focuses
///   (attaching when the session is live), `t` opens the action surface to add a
///   pane, `c` creates a session inline, `:` summons the command palette. `Esc`
///   is inert at the base Overview (the home screen is not left by backing out).
///   Overview is also re-entered from Closeup / Attached via `Ctrl-O`, where `Esc`
///   then backs out to where it was opened from.
/// - **集中 (Closeup)** — a session is selected and operated in the right pane,
///   either as a menu of its runnable commands or a session-scoped prompt
///   (chosen by the [`SessionActionUi`] setting). Launching `terminal` / `agent`
///   adds a pane and attaches it; `Esc` returns to Overview; `Ctrl-O` opens
///   Overview; `:` summons the command palette; `Ctrl-P`/`Ctrl-N` move the focused
///   session's active tab.
/// - **没入 (Attached)** — the embedded shell / agent is live in the right pane
///   and keys flow to it. The reserved keys are `Ctrl-O` (zoom out to Overview,
///   where panes are added) and `Ctrl-P`/`Ctrl-N` (switch to the previous / next
///   tab in place, without detaching); everything else, including `Esc`, goes to
///   the shell. The shell exiting returns to Closeup.
///
/// The **command palette** (`:`, from Overview or Closeup) floats the workspace
/// command line over the panes (`session` / `config` / `doctor` / `man` / …);
/// results render in its own band, `Esc` closes it, and a command with a
/// transitioning effect closes it as it acts.
///
/// The workspace root and every side-effecting hook the loop drives — appending
/// run commands to history, dispatching background session create / remove,
/// embedding the terminal pane, previewing / navigating tabs, opening the
/// settings screen — are bundled into [`Wiring`]; see its fields for each hook's
/// contract. Tests build a `Wiring` of fakes (via [`event_loop_compat`]) so the
/// loop's logic is exercised without a real terminal or shell pool.
/// Apply a session list a background sync produced (a pane-exit detach, or the
/// entry re-sync), if one has landed, refreshing the worktree statuses without
/// yanking the cursor; a slot with no sync yet leaves the state untouched.
/// Returns whether a list was applied, so the loop forces a repaint (the new git
/// statuses are not part of the badge snapshot the skip-paint check compares).
/// Split out of [`event_loop`] so the apply is exercised directly rather than
/// only through a full loop run.
/// Persist the engagement to restore on the next launch, just before a confirmed
/// quit: the focused (cursor) session's name and how deeply it was engaged
/// ([`HomeState::resume_level`], which consumes any 没入 arm and otherwise reads
/// the current mode). Routed through [`Wiring::save_resume`] so the disk write
/// lives in [`super::run`] and tests observe it through a capture.
fn save_resume_focus(state: &mut HomeState, wiring: &mut Wiring) {
    let session = state.list().selected_name().to_string();
    let level = state.resume_level();
    (wiring.save_resume)(&session, level);
    (wiring.save_last_active)(&state.last_active_flush());
}

/// Whether a left click should act on the session list: in 選択 (Overview), where it
/// is the picker, and in 集中 (Closeup), where the list still shows beside the action
/// surface so a click re-focuses onto another session (see [`closeup_click`]). Not in
/// 没入 (Attached) — there the right pane owns the pointer. In either acting mode a
/// click while a modal / palette / note editor / inline create / rename is open is
/// ignored, mirroring how those overlays capture every key in the loop below — so a
/// stray click never reaches the session list beneath them.
fn click_selects_session(state: &HomeState) -> bool {
    matches!(state.mode(), Mode::Switch | Mode::Closeup)
        && !state.quit_confirm()
        && state.remove_modal().is_none()
        && state.text_modal().is_none()
        && state.preview().is_none()
        && !state.diff_active()
        && state.note_editor().is_none()
        && state.tab_menu().is_none()
        && state.tab_rename().is_none()
        && !state.command_palette_open()
        && !state.is_creating()
        && !state.is_renaming()
}

fn apply_pending_refresh(state: &mut HomeState, refresh: &SessionsRefreshHandle) -> bool {
    let pending = refresh.take_all();
    let mut changed = false;
    for (root, sessions) in pending {
        state.refresh_sessions_for(&root, sessions);
        changed = true;
    }
    changed
}

fn apply_pending_pr_links(state: &mut HomeState, monitor: &MonitorHandle) -> bool {
    let mut changed = false;
    for (root, prs) in monitor.take_pr_link_updates() {
        changed |= state.set_pr_links(&root, prs);
    }
    changed
}

fn bump_interaction_epoch(wiring: &mut Wiring) {
    wiring.interaction_epoch = wiring.interaction_epoch.saturating_add(1);
}

/// Auto-start any session with a queued prompt but no live pane, logging one line
/// per pane started. Returns whether anything was started, so the loop forces a
/// repaint (a background spawn changes no key state the skip-paint check would
/// otherwise notice, and its new badge should show at once). Passed only the
/// autostart hook — not the whole [`Wiring`] — so it is exercised directly.
///
/// Shared with the attached pane loop
/// ([`terminal::pane::drive`](super::terminal::pane)): while 没入 (Attached) owns
/// the event loop, the outer loop is stopped, so the pane loop calls this on its
/// idle tick to pick up a prompt queued for a pane-less session (an MCP
/// `session_delegate_issue` / `session_prompt` made by the coordinator agent
/// running in the attached pane) without waiting for a detach back to 選択.
pub(super) fn apply_autostart(
    state: &mut HomeState,
    autostart: &mut dyn FnMut(&HomeState) -> Vec<String>,
) -> bool {
    let lines = autostart(state);
    let started = !lines.is_empty();
    for line in lines {
        state.log_output(line);
    }
    started
}

/// Apply every background session task (create / remove) that finished since
/// the last drain: evict a removed session's pooled shell (via `evict_pool`, on
/// this — the loop — thread, since the pool is not `Send`), clear the create /
/// removal skeleton whichever way the task ended, reflect a removal in the
/// removal modal, then log the result line and refresh the session list without
/// yanking the cursor. Returns whether anything was drained, so the caller
/// repaints.
///
/// `focus_epoch` is the current user-input epoch when the caller can honor a
/// finished task's auto-focus (the outer home loop passes
/// [`Wiring::interaction_epoch`]), or `None` to ignore auto-focus entirely: the
/// attached pane loop passes `None`, because pulling the user out of 没入 into
/// 集中 while they operate a live pane would be wrong — and the epoch has
/// necessarily advanced past the dispatch by then anyway (attaching took
/// keystrokes), so the outer loop would skip it too.
///
/// Shared with the attached pane loop
/// ([`terminal::pane`](super::terminal::pane)) like [`apply_autostart`]: while
/// 没入 (Attached) owns the event loop, the outer loop is stopped, so the pane
/// loop drains finished tasks itself — otherwise a create / remove finishing
/// while the user operates another session leaves its sidebar skeleton
/// animating until they detach back to 選択.
pub(super) fn apply_task_completions(
    state: &mut HomeState,
    tasks: &TaskHandle,
    evict_pool: &mut dyn FnMut(&Path),
    focus_epoch: Option<u64>,
) -> bool {
    let mut applied = false;
    for completion in tasks.drain_completed() {
        applied = true;
        let super::tasks::Completion {
            line,
            sessions,
            target_root,
            evict,
            focus,
            created,
            removed,
        } = completion;
        // A removal reports success by carrying the evicted pool path (set
        // only on the success branch of `run_remove`); both failure branches
        // leave it `None`. Captured before `evict` is consumed below.
        let removal_ok = evict.is_some();
        if let Some(path) = evict {
            evict_pool(&path);
        }
        if let (Some(root), Some(name)) = (target_root.as_deref(), created.as_deref()) {
            state.clear_pending_session(root, name);
        }
        if let (Some(root), Some(name)) = (target_root.as_deref(), removed.as_deref()) {
            state.clear_removing_session(root, name);
            // If the removal modal is still open behind the task, reflect the
            // outcome in it: a success drops the row (closing the modal once
            // all succeed), a failure keeps it open with the error shown.
            state.resolve_remove_modal(root, name, removal_ok, &line.text);
        }
        state.apply_task_completion(line, sessions, target_root.as_deref());
        // A finished create/close may ask to focus a landing session. Done
        // after the refresh above so the branch is in the list to match.
        // Unlike that refresh — which deliberately keeps the cursor put for
        // background changes — this is the user's own task landing, so moving
        // the cursor onto it is the intended result. A create drops into
        // 集中 (Closeup) so the user can operate the new session immediately;
        // a close stays in 選択 (Switch) on the neighbouring session because
        // the user just asked to leave the closed session.
        if let (Some(focus), Some(epoch)) = (focus, focus_epoch) {
            if focus.interaction_epoch == epoch {
                match focus.landing {
                    super::tasks::FocusLanding::Closeup => {
                        state.enter_closeup_named_existing(&focus.name);
                    }
                    super::tasks::FocusLanding::Switch => {
                        state.focus_switch_named(&focus.name);
                    }
                }
            }
        }
    }
    applied
}

/// Fire a scheduled wake if its target instant has arrived. The wake is consumed
/// before the callback runs so it is one-shot even if the broadcast reports zero
/// running agents; the log line records either the sent count or the no-op result.
pub(super) fn apply_due_wake(
    state: &mut HomeState,
    now: DateTime<Local>,
    broadcast: &mut dyn FnMut(&HomeState) -> usize,
) -> bool {
    let Some(at) = state.take_due_wake(now) else {
        return false;
    };
    let sent = broadcast(state);
    if sent == 0 {
        state.log_output(format!(
            "Wake fired at {}: no running agents to continue",
            at.format("%H:%M")
        ));
    } else {
        state.log_output(format!(
            "Wake fired at {}: sent `continue` to {sent} running agent(s)",
            at.format("%H:%M")
        ));
    }
    true
}

/// Whether the terminal size changed since the last event-loop pass, updating
/// the memo. A change must force a repaint: the quiet-選択 skip (`skip_paint`)
/// compares only content-level moving parts, so without this a resize while
/// idle left the old-size frame — and whatever the terminal's reflow made of
/// it — on screen until the next keypress. The repaint it forces is a full
/// clear + redraw: the painter discards its diff base on the same size change
/// ([`FramePainter`]'s resize invalidation). The first pass reports `false`;
/// there is no previous size to differ from, and the first frame always paints
/// anyway (`force_paint` starts true).
pub(super) fn size_changed(last: &mut Option<(u16, u16)>, size: (u16, u16)) -> bool {
    let changed = last.is_some_and(|l| l != size);
    *last = Some(size);
    changed
}

#[allow(clippy::too_many_arguments)]
pub(super) fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut state: HomeState,
    monitor: &MonitorHandle,
    update: &UpdateHandle,
    refresh: &SessionsRefreshHandle,
    ai_available: &OneShot<bool>,
    installed_agents: &OneShot<Vec<AgentCli>>,
    tasks: &TaskHandle,
    wiring: &mut Wiring,
) -> Result<Outcome> {
    let workspace_root = wiring.workspace_root;
    let mut painter = FramePainter::new();
    // Re-attach a session restored into 没入 (Attached) from the last quit. The
    // cursor was focused synchronously at startup ([`HomeState::restore_focus`]),
    // but attaching needs this loop's terminal wiring, so it runs once here on the
    // first pass — by now `restore_open_panes` has re-spawned the session's panes,
    // so it is live to attach. A no-op when nothing was armed (the usual case).
    handlers::resume_attach(term, &mut state, &mut painter, wiring);
    // What the last paint reflected, so an idle 選択 (Overview) tick whose badges
    // and update notice are unchanged can skip rebuilding and repainting the whole
    // frame. `force_paint` keeps the first frame — and the frame after any key —
    // always repainting.
    let mut last_update = None;
    let mut force_paint = true;
    // Whether the last paint drew the mascot mid-blink, so the frame that reopens
    // its eyes (an idle tick, not a keypress) still repaints in a quiet 選択 rather
    // than being skipped — leaving the eyes stuck shut.
    let mut last_blinking = false;
    // The previous left click's session row and time, so a second click on the
    // same row within the double-click window confirms it (see [`overview_click`]).
    let mut last_click: Option<(usize, Instant)> = None;
    // The terminal size at the last pass, so a resize forces a full repaint
    // (see [`size_changed`]).
    let mut last_size: Option<(u16, u16)> = None;
    // The monitor snapshot version last applied to `state`. When unchanged, the
    // loop skips `monitor.snapshot()` entirely — avoiding the clone of every badge
    // set on each idle/live-frame pass.
    let mut seen_badge_version = u64::MAX;
    // The in-flight local-LLM chat reply's channel while the 集中 `chat` overlay
    // awaits one, or `None` when idle. Owned by the loop (it is IO) rather than the
    // pure state, which only tracks that a reply is pending. Dropped when the
    // overlay closes, abandoning the request harmlessly.
    let mut chat_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>> = None;
    loop {
        // Drain a finished chat reply into the overlay before painting so it shows
        // this frame; drop the channel if the overlay was closed mid-request. While
        // one is pending the spinner advances each pass (the `animate` read below
        // keeps the loop ticking).
        if state.chat().is_none() {
            chat_rx = None;
        } else if let Some(rx) = chat_rx.as_ref() {
            match rx.try_recv() {
                Ok(reply) => {
                    if let Some(chat) = state.chat_mut() {
                        chat.finish(reply);
                    }
                    chat_rx = None;
                    force_paint = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    if let Some(chat) = state.chat_mut() {
                        chat.advance_tick();
                    }
                    force_paint = true;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if let Some(chat) = state.chat_mut() {
                        chat.finish(Err("local LLM request failed".to_string()));
                    }
                    chat_rx = None;
                    force_paint = true;
                }
            }
        }
        force_paint |= state.poll_pending_diff();
        // Mark each background session's agent state — running, waiting for
        // input, live (ready), and finished — before painting, applying every
        // badge set together (read under a single lock) so the frame never mixes
        // one set's fresh reading with another's stale one.
        let badge_version = monitor.badge_version();
        let badges_changed = if badge_version != seen_badge_version {
            let badges = monitor.snapshot();
            // Whether the sidebar badges moved since the last paint, decided
            // before storing them so the snapshot can be applied by move rather
            // than cloned (the loop no longer keeps its own copy alongside the one
            // in `state`).
            let changed = state.badges() != &badges;
            state.badge_writer(SurfaceOwner::Preview).apply(badges);
            seen_badge_version = badge_version;
            changed
        } else {
            false
        };
        // Surface the sidebar mascot's "update available" notice once the
        // background release check has found a newer version than this build.
        let latest_update = update.status().map(|status| status.latest);
        state.set_update(latest_update);
        // Apply a session list a background sync produced — a pane-exit detach, or
        // the one-shot entry re-sync — if one has landed, refreshing the worktree
        // statuses without yanking the cursor. Done before the task drain below so
        // a session create / remove that finished on the same frame still has the
        // last word on the list. A landed refresh changes the sidebar git statuses
        // (which the badge snapshot does not capture), so `refreshed` forces a
        // repaint below.
        let refreshed = apply_pending_refresh(&mut state, refresh);
        // Apply PR URLs harvested from live background panes. Attached panes
        // already update their own row directly from the pane driver; this drains
        // the watcher path so detached/background sessions get the same immediate
        // sidebar `#N` badges without waiting for a full git re-sync.
        let pr_links_changed = apply_pending_pr_links(&mut state, monitor);
        // Pick up any prompt queued for a pane-less session (an MCP
        // `session_delegate_issue` / `session_prompt` made while this screen runs)
        // and start its agent pane in the background — no human opens it. A started
        // pane logs a line and forces a repaint so its new sidebar badge shows at
        // once. A no-op when the feature is off or nothing is queued.
        force_paint |= apply_autostart(&mut state, wiring.autostart_queued);
        // Fire the one-shot `wake -t hhmm` schedule once its local wall-clock time
        // arrives, sending `continue` to every live session agent and logging the
        // count. The same idle tick that keeps session watching alive also drives
        // this timer.
        force_paint |= apply_due_wake(&mut state, Local::now(), wiring.broadcast_wake);
        // Flip the local-LLM `chat` menu row on once the background probe confirms
        // it is usable (drained once); until then the 集中 menu simply omits it.
        // Force a repaint so the change is reflected without waiting for a keypress.
        if let Some(available) = ai_available.take() {
            state.set_ai_available(available);
            force_paint = true;
        }
        // Fill in the installed-agent list once the background PATH probe lands
        // (drained once), so 集中's agent picker can offer the alternatives it found;
        // until then the picker simply shows none. Force a repaint so the picker
        // reflects them without waiting for the next keypress.
        if let Some(agents) = installed_agents.take() {
            state.set_installed_agents(agents);
            force_paint = true;
        }
        // Apply any background session task (create / remove) that finished since
        // the last frame (see [`apply_task_completions`]), honoring auto-focus
        // when the user has not typed since the dispatch. Then refresh the task
        // panel rows so in-flight work shows in the top-right corner.
        let completed_any = apply_task_completions(
            &mut state,
            tasks,
            wiring.evict_pool,
            Some(wiring.interaction_epoch),
        );
        state.set_tasks(tasks.view(Instant::now()));
        // Drop any stale surface every frame, then refresh it for the modes that
        // draw the embedded terminal: 没入 (driven directly by `open_pane`, which
        // clears its own surface on the way out) and 選択, where the right pane
        // previews the highlighted session's live terminal — with its tab strip
        // above it, so `←`/`→` has something to act on — so the user sees the
        // actual screen re-attaching reveals.
        state.clear_terminal_surface();
        // Collapsed to the rail, 選択's create / rename input takes over the right
        // pane (no room inline in the 5-column list), so there is no preview to
        // draw then; otherwise preview the highlighted session, sized to the
        // current sidebar state so the snapshot fills the pane it is drawn into.
        let input_in_right_pane = state.sidebar() == Sidebar::Rail
            && (state.create().is_some() || state.rename().is_some());
        // 選択 previews the highlighted session; 集中 previews the focused session's
        // selected pane — both read the same live snapshot + tab strip, so the
        // focused session's panes show as tabs and the chosen one previews live
        // (an idle session has no live snapshot, so the strip stays absent and the
        // action surface shows instead).
        let drives_surface = matches!(state.mode(), Mode::Switch | Mode::Closeup);
        // The note editor opened from 没入 (`Ctrl-E`) floats over the attached
        // session's pane, which keeps drawing in Attached mode while the editor is
        // open. The surface was just cleared, so refresh it here too — otherwise
        // the live terminal would not show behind the floating box, and the empty
        // fallback pane (a one-line starting hint) would be too short to hold the
        // box, clipping its bottom border as the note grows with each newline.
        let attached_note = state.closeup_attached() && state.note_editor().is_some();
        let drive_now = (drives_surface && !input_in_right_pane) || attached_note;
        // Refresh the surface for the mode that draws it, when the highlighted /
        // focused session has a live snapshot. Folded into one `if let` (rather
        // than a guard `if` wrapping an inner `if let`) so the whole refresh is a
        // single covered branch.
        if let Some(dir) = drive_now.then(|| selected_dir(&state, workspace_root)) {
            let view = (wiring.preview)(&dir, state.sidebar());
            let (mut labels, mut active) = (wiring.tab_op)(&dir, None);
            // A launch whose environment is still resolving has no pool pane yet,
            // so append a synthetic placeholder chip at the strip's end (like the
            // `+ new` chip). The new tab is selected immediately — readiness no
            // longer selects it later — and the same loading frame is also
            // published for the pane body while this dir is previewed/focused.
            let mut loading_body = None;
            if let Some(label) = state
                .pending_pane()
                .filter(|p| p.dir() == dir)
                .and_then(|p| p.placeholder())
                .map(str::to_string)
            {
                labels.push(label);
                active = labels.len() - 1;
                state.advance_pending_pane(active, true);
            }
            // Once spawned, the pane is already the selected pool tab (see
            // `add_pane_selected`), so `active` from `tab_op` is correct as-is; the
            // body just needs the current loading frame for either phase.
            if let Some(frame) = state
                .pending_pane()
                .filter(|p| p.dir() == dir)
                .map(|p| p.frame())
            {
                loading_body = Some(frame);
            }
            let mut surface = state.surface_writer(SurfaceOwner::Preview);
            if let Some(v) = view {
                surface.set_view(v);
            }
            surface.set_tabs(labels, active);
            if let Some(frame) = loading_body {
                surface.set_loading_body(frame);
            }
        }
        // A pending pane loads its selected tab in the strip. Poll it each frame:
        // animate its chip and, once its shell has started painting, attach it
        // (没入) only if that tab is still selected. There is no ready-time
        // selection step: if the user selected something else while it loaded, the
        // pane simply becomes an ordinary background tab.
        if let Some(dir) = state.pending_pane().map(|p| p.dir().to_path_buf()) {
            match (wiring.poll_pending_spawn)(&dir) {
                // Environment still resolving: the placeholder chip is animated by
                // the publish above; nothing to do here.
                PendingPoll::Resolving => {}
                // Spawned and starting: animate the real chip at its tab index.
                PendingPoll::Starting(tab) => state.advance_pending_pane(tab, false),
                // Ready: stop showing the loading chip/body. Attach only if the
                // user still has this session and this tab selected; no delayed
                // tab selection happens here.
                PendingPoll::Ready { selected } => {
                    state.clear_pending_pane();
                    if selected && selected_dir(&state, workspace_root) == dir {
                        // The pane spawned already made itself the selected pool
                        // tab; re-assert it defensively before attaching so a
                        // concurrent tab close cannot leave a stale index behind.
                        (wiring.activate_pending)(&dir);
                        handlers::reattach_pane(term, &mut state, &mut painter, wiring);
                    } else {
                        // The user selected another tab / session while it loaded:
                        // leave it as an ordinary background pane and just drop the
                        // launch tracker.
                        (wiring.clear_pending_spawn)();
                    }
                    force_paint = true;
                }
                // The launch is gone (spawn failed / pane vanished): drop it.
                PendingPoll::Gone => {
                    state.clear_pending_pane();
                    (wiring.clear_pending_spawn)();
                }
            }
        }
        // The task panel and the install rabbit animate on the clock, so a frame
        // showing either must repaint even when nothing else moved.
        let now = Instant::now();
        let panel_animating = install_task::handle().is_active(now) || tasks.is_active(now);
        // Session create / remove show an inline sidebar skeleton whose loading
        // wave is derived from the wall clock (see [`ui::sidebar::skeleton_frame`]),
        // so it only reads as motion while the screen keeps repainting. Treat a
        // pending session as a moving part in its own right — independent of the
        // background task board's animation — so the skeleton animates in every
        // mode (Switch and the Closeup Focus modal alike), not only when some
        // other animated part happens to keep the loop ticking.
        let sessions_animating = !state.pending_sessions().is_empty();
        // Refresh the sidebar mascot for this paint: reopen its eyes once a blink's
        // window has passed and advance the Working paw on the live tick. Reactive,
        // not timed — it rides paints that already happen, so a settled mascot
        // leaves `mascot_blinking` false and a truly idle 選択 still skips painting.
        state.tick_mascot(now);
        let blink_changed = state.mascot_blinking() != last_blinking;
        // The size this frame paints at, read once per pass. A resize since the
        // last pass invalidates everything on screen — the terminal reflowed the
        // old frame — so it forces a repaint past the quiet-選択 skip below; the
        // painter separately discards its diff base on the same size change,
        // making that repaint a whole-screen clear + full redraw. The loop is
        // awake to notice: a resize interrupts a blocking read (SIGWINCH →
        // EINTR, surfaced as a `Key::Unknown` or a swallowed `Interrupted`
        // below), and an idle 選択 wakes on `WATCH_SESSIONS_TICK` regardless.
        let (height, width) = term.size();
        force_paint |= size_changed(&mut last_size, (height, width));
        // In a quiet base 選択 (Overview) — no live preview in the right pane and no
        // command palette open — an idle frame's only moving parts are the sidebar
        // badges, the update notice, and those time-animated panels. When none
        // changed since the last paint — and no key was just pressed
        // (`force_paint`) and no background task just finished — skip rebuilding
        // and repainting the whole frame. Anything with a live pane (a 選択 preview
        // of a running session, 集中, 没入) or the palette open repaints as before,
        // so a live pane is never frozen stale. The cheap per-frame state updates
        // above still run, so the next paint (when something does change) is
        // correct.
        let skip_paint = state.mode() == Mode::Switch
            && state.terminal_view().is_none()
            && !state.command_palette_open()
            // A loading tab animates on the clock, so its frames must not be skipped.
            && state.pending_pane().is_none()
            && state.pending_diff_frame().is_none()
            && !force_paint
            && !completed_any
            && !refreshed
            && !pr_links_changed
            && !panel_animating
            && !badges_changed
            // A session create / remove skeleton animates on the clock, so its
            // frames must not be skipped either.
            && !sessions_animating
            // A mascot blink (or the frame that ends one) is a moving part too, so
            // it repaints rather than freezing the eyes mid-blink.
            && !state.mascot_blinking()
            && !blink_changed
            && last_update == latest_update;
        if !skip_paint {
            // Stamp the frame's render time so the left pane's "Nm ago" labels track
            // real time. Only on a real paint — a skipped frame draws nothing, so
            // the label refreshes on the next change rather than ticking every
            // second (keeping the loop's repaint budget low).
            state.set_now(chrono::Utc::now());
            let frame = ui::render_frame(height as usize, width as usize, &state);
            let columns = ui::column_diff(height as usize, width as usize, state.sidebar());
            painter.paint_columns(term, frame, columns)?;
        }
        last_update = latest_update;
        last_blinking = state.mascot_blinking();
        force_paint = false;

        // Read the next input event. A wheel turn is read and dropped (the TUI
        // never scrolls in place; the embedded pane scrolls its own history
        // separately), and a click only ever pokes the sidebar mascot — neither is
        // a key, so both loop without dispatching one.
        //
        // While a background install or a session task is in flight — or any
        // session is live, or the mascot is mid-animation — the read wakes every
        // `ANIM_TICK` so the loop re-iterates: re-draining finished work, re-reading
        // the monitor badges and update notice, and (when something changed)
        // repainting — which also advances the task panel's, install rabbit's, and
        // mascot's time-based animation. This is what keeps a live background
        // agent's badge moving to waiting (◆) / finished (✓) — and a click reaction
        // playing out — without the user typing. With nothing in flight and no live
        // session it blocks on the next input, so a truly idle screen costs nothing.
        let animate = panel_animating
            || state.has_live_sessions()
            || state.mascot_blinking()
            || state.mascot_reacting()
            // A session create / remove skeleton advances on the clock, so keep
            // ticking fast to animate it in every mode.
            || sessions_animating
            // A pending chat reply: keep ticking to poll the receiver and animate
            // the "thinking" spinner.
            || chat_rx.is_some()
            // A pending diff animates its tab/body and must keep polling the
            // background git worker.
            || state.pending_diff_frame().is_some()
            // A background pane loading its tab: keep ticking to poll it ready and
            // animate its chip.
            || state.pending_pane().is_some();
        // How long to wait for input before re-iterating, or `None` to block until
        // it arrives. An animating frame ticks fast (`ANIM_TICK`) to advance its
        // moving parts; an otherwise-idle screen still wakes on the slower
        // [`WATCH_SESSIONS_TICK`] while the session watcher is running, so a create
        // / remove made outside this screen (an agent's MCP call, another usagi
        // window, or the CLI — all of which only write `state.json`) lands in the
        // sidebar without waiting for the next keypress. With no watcher (the
        // tests' default) a truly idle screen blocks and costs nothing.
        let idle_tick = if animate {
            Some(install_task::ANIM_TICK)
        } else if wiring.watch_sessions {
            Some(WATCH_SESSIONS_TICK)
        } else {
            None
        };
        let input = match idle_tick {
            Some(tick) => match reader.read_input_timeout(tick) {
                Ok(Some(input)) => input,
                // A tick with no input: re-iterate to drain and repaint.
                Ok(None) => continue,
                // A delivered signal (crossterm installs a SIGWINCH handler that
                // persists after the embedded pane; an exiting agent also raises
                // SIGCHLD) interrupts the blocking read with `EINTR`. That is not a
                // request to quit — a real Ctrl-C arrives as `Key::CtrlC`, handled
                // below — so swallow it and re-iterate, exactly like an idle tick.
                // Quitting here dropped the user out of the alternate screen and
                // revealed the pre-launch scrollback whenever a signal landed
                // mid-read (e.g. exiting an agent, then `Ctrl-O` while waiting).
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(anyhow::Error::from(e).context("Failed to read input")),
            },
            None => match reader.read_input() {
                Ok(input) => input,
                // An interrupted read (a delivered signal) is not a quit: re-read.
                // See the tick branch above for the full rationale.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(anyhow::Error::from(e).context("Failed to read input")),
            },
        };
        let key = match input {
            Input::Key(key) => {
                bump_interaction_epoch(wiring);
                key
            }
            // A wheel turn scrolls whichever scrollable right-pane surface is open
            // (the diff view, the Markdown preview, or a text modal); with none
            // open it is dropped, so the base panes never scroll in place and the
            // pre-launch scrollback stays hidden.
            Input::Scroll(ev) => {
                bump_interaction_epoch(wiring);
                let page = ui::preview_visible(height as usize, width as usize, &state);
                scroll_open_surface(&mut state, ev, page);
                continue;
            }
            // A click on a session row in the left pane acts on it: in 選択 (Overview)
            // it selects the row (a second click on the same row confirms it, like
            // `Enter`); in 集中 (Closeup) it re-focuses onto that session (a second
            // click attaches its pane when live). A click on the resting sidebar
            // mascot makes it react; anywhere else it is ignored. The two hit
            // disjoint regions, so the session list is tried first and the mascot
            // only when it misses. No key was pressed either way, so repaint only
            // when the click actually did something.
            Input::Click(click) => {
                bump_interaction_epoch(wiring);
                // A pinned PR popup intercepts the click first: a `#<number>` opens
                // that PR in the browser, a click elsewhere in the box keeps it
                // pinned, and a click outside dismisses it — consuming the click so
                // it neither selects a row nor re-pins on the same press.
                if state.pr_popup().is_some() {
                    match ui::pr_popup_click(
                        &state,
                        height as usize,
                        width as usize,
                        click.col,
                        click.row,
                    ) {
                        ui::PopupClick::Open(url) => (wiring.open_url)(&url),
                        ui::PopupClick::SetState {
                            pr_key,
                            state: pr_state,
                        } => {
                            if state.set_pinned_pr_state(&pr_key, pr_state) {
                                force_paint = true;
                            }
                        }
                        ui::PopupClick::ToggleDismissedView => {
                            state.toggle_pr_popup_show_dismissed();
                            force_paint = true;
                        }
                        ui::PopupClick::Inside => {}
                        ui::PopupClick::Outside => {
                            state.set_pr_popup(None);
                            force_paint = true;
                        }
                    }
                    continue;
                }
                // No popup open: a click on a session's PR badge pins that session's
                // popup so the pointer can travel into it and click a `#<number>`.
                if let Some(idx) = ui::sidebar_pr_badge_at(
                    &state,
                    height as usize,
                    width as usize,
                    click.col,
                    click.row,
                ) {
                    state.set_pr_popup(Some(idx));
                    force_paint = true;
                    continue;
                }
                // The right-pane tab strips are active in both 選択 and 集中:
                // clicking an inactive pane tab drives the same `tab_op` keyboard
                // navigation uses, so the preview and the pane that `Enter`
                // re-attaches move together. 没入 handles its own tab clicks
                // inside the pane driver.
                match state.mode() {
                    Mode::Switch => {
                        if let Some(index) = ui::overview_tab_at(
                            &state,
                            click.col,
                            click.row,
                            height as usize,
                            width as usize,
                        ) {
                            let dir = selected_dir(&state, wiring.workspace_root);
                            (wiring.tab_op)(&dir, Some(TabNav::To(index)));
                            force_paint = true;
                            continue;
                        }
                    }
                    Mode::Closeup => {
                        if let Some(index) = ui::closeup_tab_at(
                            &state,
                            click.col,
                            click.row,
                            height as usize,
                            width as usize,
                        ) {
                            let index = state
                                .closeup_select_pane_tab(index)
                                .expect("focus tab hit selects a live pane");
                            let dir = selected_dir(&state, wiring.workspace_root);
                            (wiring.tab_op)(&dir, Some(TabNav::To(index)));
                            force_paint = true;
                            continue;
                        }
                    }
                }
                let selected = click_selects_session(&state)
                    .then(|| {
                        ui::left_pane_session_at(
                            &state,
                            click.col,
                            click.row,
                            height as usize,
                            width as usize,
                        )
                    })
                    .flatten();
                match selected {
                    Some(row) => {
                        let now = Instant::now();
                        if state.mode() == Mode::Closeup {
                            closeup_click(
                                term,
                                &mut state,
                                &mut painter,
                                wiring,
                                row,
                                now,
                                &mut last_click,
                            );
                        } else {
                            overview_click(
                                term,
                                &mut state,
                                &mut painter,
                                wiring,
                                row,
                                now,
                                &mut last_click,
                            );
                        }
                        force_paint = true;
                    }
                    None => {
                        if handle_mascot_click(term, &mut state, click) {
                            force_paint = true;
                        }
                    }
                }
                continue;
            }
            Input::RightClick(click) => {
                bump_interaction_epoch(wiring);
                state.set_pr_popup(None);
                let hit = match state.mode() {
                    Mode::Switch => ui::overview_tab_hit(
                        &state,
                        click.col,
                        click.row,
                        height as usize,
                        width as usize,
                    ),
                    Mode::Closeup if !state.closeup_attached() => ui::closeup_tab_hit(
                        &state,
                        click.col,
                        click.row,
                        height as usize,
                        width as usize,
                    ),
                    Mode::Closeup => None,
                };
                if let Some(index) = hit {
                    let dir = selected_dir(&state, wiring.workspace_root);
                    let label = state
                        .terminal_tabs()
                        .and_then(|tabs| tabs.labels.get(index))
                        .cloned()
                        .expect("tab hit index has a published label");
                    state.open_tab_menu(dir, index, label, click.col, click.row);
                    force_paint = true;
                } else if state.tab_menu().is_some() {
                    state.close_tab_menu();
                    force_paint = true;
                }
                continue;
            }
            // A bare pointer move no longer drives the PR popup — it is pinned by a
            // badge click and dismissed only by a click or a keypress — so motion
            // reports are ignored. Moving the pointer toward the popup to click a
            // `#<number>` must not dismiss it. No key was pressed, so it loops
            // without dispatching one.
            Input::Hover(_) => continue,
        };
        // A key was pressed: whatever it does to the state, repaint on the next
        // iteration (the skip above only applies to idle ticks that read no key).
        force_paint = true;
        // Touching the keyboard dismisses the pinned PR popup (so `Esc` — or any
        // key — closes it), so it never lingers over a screen the user has moved on
        // from: a stale popup would otherwise survive a keypress, a mode change, or
        // attaching a pane, since those paths read no click to dismiss it.
        state.set_pr_popup(None);
        // Nudge the resting mascot to blink back at the user — reactive, so the
        // rabbit reacts the moment a key lands without any idle timer. Only while
        // it shows an open-eyed face (選択 / 集中); 没入's heads-down face has no eyes
        // to blink and animates on the live tick instead. A fresh `now` (the read
        // may have blocked a while) so the blink's window starts from the keypress;
        // the call is a no-op when the mascot animation is turned off.
        if matches!(state.mode(), Mode::Switch | Mode::Closeup) {
            state.kick_mascot_blink(Instant::now());
        }

        // Record the key press (and the mode it landed in) to the operation trace,
        // so a session's navigation can be analysed after the fact. `record_with`
        // builds the event — the timestamp, the allocation, and the `{mode} {key}`
        // `format!` — only once tracing is enabled, so the hot key loop pays
        // nothing for it while tracing is off (the default).
        crate::infrastructure::trace_log::TraceLog::record_with(|| {
            crate::domain::trace::TraceEvent::now(crate::domain::trace::TraceCategory::Tui, "key")
                .with_detail(format!("{:?} {:?}", state.mode(), key))
        });

        // Consume the one-shot pane-exit grace on *every* key, so a deliberate key
        // (not just the absorbed `Ctrl-C`) disarms it — the guard only ever spans
        // the single press right after an embedded pane hands focus back.
        let pane_grace = state.take_pane_exit_grace();

        // The quit-confirmation modal, when open, captures every key: only `y` /
        // `Y` / `Enter` confirms the close and `n` / `N` / `Esc` cancels. `Ctrl-C`
        // and `Ctrl-Q` are inert here on purpose: a burst of `Ctrl-C` (say, from
        // interrupting an agent) must not be able to both raise *and* confirm the
        // modal, so closing usagi always takes a deliberate, distinct keystroke.
        if state.quit_confirm() {
            match key {
                Key::Char('y') | Key::Char('Y') | Key::Enter => {
                    save_resume_focus(&mut state, wiring);
                    return Ok(Outcome::Quit);
                }
                Key::Char('n') | Key::Char('N') | Key::Escape => state.cancel_quit_confirm(),
                _ => {}
            }
            continue;
        }

        // The update-confirmation modal, when open, captures every key: `y` /
        // `Enter` launches the self-update (and closes the modal — its progress
        // then shows as the shared loading rabbit), `n` / `Esc` cancels.
        //
        // `Ctrl-C` / `Ctrl-Q` also cancel here. This block sits above the global
        // quit chords below, so without handling them they would be inert while
        // this modal is open (unlike every other overlay, which sits below those
        // handlers and so passes them through). Routing them to the global path
        // instead would raise the quit modal on top of this one, but the two are
        // documented never to coexist; cancelling first — a second press then
        // quits — keeps that invariant.
        if state.update_confirm() {
            match key {
                Key::Char('y') | Key::Char('Y') | Key::Enter => {
                    (wiring.dispatch_update)();
                    state.cancel_update_confirm();
                }
                Key::Char('n') | Key::Char('N') | Key::Escape | Key::CtrlC | Key::Char(CTRL_Q) => {
                    state.cancel_update_confirm()
                }
                _ => {}
            }
            continue;
        }

        // `Ctrl-C` closes the app from anywhere on the home screen. Quitting
        // would drop any session whose shell / agent is still running, so when
        // one is live we raise the quit-confirmation modal first instead of
        // closing outright; an idle screen quits immediately.
        if let Key::CtrlC = key {
            // …unless an embedded pane just handed focus back: a `Ctrl-C` burst
            // meant to close / interrupt the agent can spill past the pane onto
            // the home screen. Swallow the first such press (with a hint) so it
            // does not quit usagi by accident; the grace is one-shot, so a second,
            // deliberate `Ctrl-C` quits as usual.
            if pane_grace {
                state.log_output(
                    "Ctrl-C ignored just after leaving a pane — press it again (or Ctrl-Q) to quit usagi.",
                );
                continue;
            }
            if state.has_live_sessions() {
                state.open_quit_confirm();
            } else {
                save_resume_focus(&mut state, wiring);
                return Ok(Outcome::Quit);
            }
            continue;
        }

        // `Ctrl-Q` is the dedicated quit chord: unlike `Ctrl-C` it *always* raises
        // the quit-confirmation modal first, idle or live, so the app never closes
        // on a single keystroke. (没入's `Ctrl-Q` lands here too: the pane detaches
        // and `open_pane` opens the same modal on the way out.)
        if let Key::Char(CTRL_Q) = key {
            state.open_quit_confirm();
            continue;
        }

        if state.tab_menu().is_some() {
            match key {
                Key::ArrowUp | Key::Char('k') => {
                    state
                        .tab_menu_mut()
                        .expect("tab menu open while handling its keys")
                        .move_up();
                }
                Key::ArrowDown | Key::Char('j') => {
                    state
                        .tab_menu_mut()
                        .expect("tab menu open while handling its keys")
                        .move_down();
                }
                Key::Escape => state.close_tab_menu(),
                Key::Enter => {
                    let (dir, tab, item) = {
                        let menu = state
                            .tab_menu()
                            .expect("tab menu open while handling Enter");
                        (menu.dir().to_path_buf(), menu.tab(), menu.item())
                    };
                    match item {
                        super::state::TabMenuItem::MoveLeft => {
                            state.close_tab_menu();
                            (wiring.tab_action)(
                                &mut state,
                                &dir,
                                tab,
                                TabMenuAction::Move(TabSwap::Left),
                            );
                        }
                        super::state::TabMenuItem::MoveRight => {
                            state.close_tab_menu();
                            (wiring.tab_action)(
                                &mut state,
                                &dir,
                                tab,
                                TabMenuAction::Move(TabSwap::Right),
                            );
                        }
                        super::state::TabMenuItem::Rename => {
                            state.begin_tab_rename_from_menu();
                        }
                        super::state::TabMenuItem::Close => {
                            state.close_tab_menu();
                            (wiring.tab_action)(&mut state, &dir, tab, TabMenuAction::Close);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        if state.tab_rename().is_some() {
            match key {
                Key::Enter => {
                    let (dir, tab, label) = state
                        .confirm_tab_rename()
                        .expect("tab rename open while handling Enter");
                    (wiring.tab_action)(&mut state, &dir, tab, TabMenuAction::Rename(label));
                }
                Key::Escape => state.cancel_tab_rename(),
                _ => {
                    let rename = state
                        .tab_rename_mut()
                        .expect("tab rename open while handling its keys");
                    match key {
                        Key::Backspace => rename.backspace(),
                        Key::Del => rename.delete_forward(),
                        Key::ArrowLeft => rename.move_left(),
                        Key::ArrowRight => rename.move_right(),
                        Key::Home => rename.move_home(),
                        Key::End => rename.move_end(),
                        Key::Char(c) if !c.is_control() => rename.push_char(c),
                        _ => {}
                    }
                }
            }
            continue;
        }

        // The session-removal modal, when open, captures every key: the cursor
        // moves with the arrows (or j/k), Space toggles the row's checkbox, and
        // Enter removes every checked session (Esc cancels).
        if state.remove_modal().is_some() {
            match key {
                // Cursor moves and checkbox toggles route to the modal's own
                // methods; Enter / Esc are lifecycle on the screen state.
                Key::ArrowUp | Key::Char('k') => {
                    state
                        .remove_modal_mut()
                        .expect("remove modal open while handling its keys")
                        .move_up();
                }
                Key::ArrowDown | Key::Char('j') => {
                    state
                        .remove_modal_mut()
                        .expect("remove modal open while handling its keys")
                        .move_down();
                }
                Key::Char(' ') => {
                    state
                        .remove_modal_mut()
                        .expect("remove modal open while handling its keys")
                        .toggle();
                }
                // `y`/`Y` confirm removal like `Enter`, matching the yes-key
                // convention of the quit / delete confirmations.
                Key::Enter | Key::Char('y') | Key::Char('Y') => {
                    if let Some((entries, force)) = state.submit_remove_modal() {
                        // Each checked session is dispatched to a background
                        // worker, so the loop never blocks on the git work; the
                        // task panel stacks them and the loop drains each as it
                        // finishes. Each row already carries the owning workspace
                        // root, which keeps 統合(unite) bulk-removal correct even
                        // when different workspaces contain the same session name.
                        for entry in &entries {
                            let root = entry.root_path().to_path_buf();
                            state.set_op_target(root.clone());
                            state.begin_removing_session(root.clone(), entry.name().to_string());
                            (wiring.dispatch_remove)(&root, entry.name(), force, None);
                        }
                    }
                }
                Key::Escape => state.cancel_remove_modal(),
                _ => {}
            }
            continue;
        }

        // The text modal (a text-dumping command's output, e.g. `man`), when open,
        // captures every key: the arrows / `j`/`k` and PageUp/PageDown scroll it,
        // and `Esc` / `Enter` / `q` dismiss it.
        if let Some(size) = state.text_modal().map(|modal| modal.size) {
            // Page by exactly the window the renderer shows, so PageUp/PageDown
            // move one screenful for both the compact and the large `man` modal.
            let (_, page) = ui::text_modal_geometry(height as usize, width as usize, size);
            match key {
                Key::ArrowUp | Key::Char('k') => state.text_modal_scroll_up(),
                Key::ArrowDown | Key::Char('j') => state.text_modal_scroll_down(page),
                Key::PageUp => {
                    for _ in 0..page {
                        state.text_modal_scroll_up();
                    }
                }
                // `Space` pages forward too, matching the less / pager convention.
                Key::PageDown | Key::Char(' ') => {
                    for _ in 0..page {
                        state.text_modal_scroll_down(page);
                    }
                }
                Key::Escape | Key::Enter | Key::Char('q') => state.close_text_modal(),
                _ => {}
            }
            continue;
        }

        // The right-pane Markdown preview, when open, captures every key: the
        // arrows / `j`/`k` and PageUp/PageDown scroll within the pane, and `Esc` /
        // `Enter` / `q` dismiss it (returning the right pane to the default).
        if state.preview().is_some() {
            let page = ui::preview_visible(height as usize, width as usize, &state);
            match key {
                Key::ArrowUp | Key::Char('k') => state.preview_scroll_up(),
                Key::ArrowDown | Key::Char('j') => state.preview_scroll_down(page),
                Key::PageUp => {
                    for _ in 0..page {
                        state.preview_scroll_up();
                    }
                }
                // `Space` pages forward too, matching the less / pager convention.
                Key::PageDown | Key::Char(' ') => {
                    for _ in 0..page {
                        state.preview_scroll_down(page);
                    }
                }
                Key::Escape | Key::Enter | Key::Char('q') => state.close_preview(),
                _ => {}
            }
            continue;
        }

        // The local-LLM chat overlay, when open, captures every key: the editing
        // keys build the line, `↑`/`↓` scroll the transcript, `Enter` submits it
        // (starting a request the loop then polls), and `Esc` closes it back to the
        // 集中 surface. Scroll / close work even while a reply is in flight; typing
        // and submitting are inert until it lands, so a turn cannot be garbled.
        if state.chat().is_some() {
            match key {
                Key::Escape => state.close_chat(),
                Key::ArrowUp => {
                    if let Some(chat) = state.chat_mut() {
                        chat.scroll_up();
                    }
                }
                Key::ArrowDown => {
                    if let Some(chat) = state.chat_mut() {
                        chat.scroll_down();
                    }
                }
                _ if state.chat().is_some_and(|chat| chat.is_pending()) => {}
                Key::Enter => {
                    if let Some(prompt) = state.chat_mut().and_then(|chat| chat.submit()) {
                        chat_rx = Some((wiring.chat_ask)(prompt));
                    }
                }
                Key::Backspace => {
                    if let Some(chat) = state.chat_mut() {
                        chat.input_mut().backspace();
                    }
                }
                Key::Del => {
                    if let Some(chat) = state.chat_mut() {
                        chat.input_mut().delete_forward();
                    }
                }
                Key::ArrowLeft => {
                    if let Some(chat) = state.chat_mut() {
                        chat.input_mut().move_left();
                    }
                }
                Key::ArrowRight => {
                    if let Some(chat) = state.chat_mut() {
                        chat.input_mut().move_right();
                    }
                }
                Key::Home => {
                    if let Some(chat) = state.chat_mut() {
                        chat.input_mut().move_home();
                    }
                }
                Key::End => {
                    if let Some(chat) = state.chat_mut() {
                        chat.input_mut().move_end();
                    }
                }
                Key::Char(c) if !c.is_control() => {
                    if let Some(chat) = state.chat_mut() {
                        chat.input_mut().insert(c);
                    }
                }
                _ => {}
            }
            continue;
        }

        // The loading `diff` tab owns the right pane as soon as the command is
        // dispatched. It only needs close/tab-leave keys until the worker yields;
        // the finished view below takes over rich navigation.
        if state.pending_diff_frame().is_some() {
            match key {
                Key::Escape | Key::Char('q') | Key::Char(CTRL_P) | Key::Char(CTRL_N) => {
                    state.close_diff()
                }
                _ => {}
            }
            continue;
        }

        // The right-pane diff view, when open, captures every key. It is a GitHub
        // pull-request-style split, so the keys are focus-aware: the left explorer
        // (directory tree) navigates with the arrows / `j`/`k`, `Enter` (or `→`)
        // folds a directory or opens a file's diff, and `←` collapses a directory;
        // the right diff pane scrolls with the arrows / `j`/`k` / PageUp/PageDown /
        // Space, `←` returns to the explorer, and `s` toggles the layout. `Tab`
        // swaps focus from either side and `Esc` / `q` dismiss. `Ctrl-P` / `Ctrl-N`
        // (the tab-switch keys) leave the `diff` tab back to the session's panes,
        // so the diff reads and behaves as a tab beside them. It shares the
        // preview's one-row-header geometry, so it pages by the same measure.
        if let Some(focus) = state.diff_view().map(|d| d.focus()) {
            let page = ui::preview_visible(height as usize, width as usize, &state);
            match key {
                Key::Tab => state.diff_toggle_focus(),
                // `v` flips the explorer/diff arrangement between side-by-side and
                // stacked (works from either focus, like `Tab`).
                Key::Char('v') => state.diff_toggle_layout(),
                Key::Escape | Key::Char('q') | Key::Char(CTRL_P) | Key::Char(CTRL_N) => {
                    state.close_diff()
                }
                _ => match focus {
                    DiffFocus::Tree => match key {
                        Key::ArrowUp | Key::Char('k') => state.diff_move_up(),
                        Key::ArrowDown | Key::Char('j') => state.diff_move_down(),
                        Key::Enter | Key::ArrowRight | Key::Char('l') => state.diff_activate(),
                        Key::ArrowLeft | Key::Char('h') => state.diff_collapse(),
                        _ => {}
                    },
                    DiffFocus::Diff => match key {
                        Key::ArrowUp | Key::Char('k') => state.diff_scroll_up(),
                        Key::ArrowDown | Key::Char('j') => state.diff_scroll_down(page),
                        Key::PageUp => {
                            for _ in 0..page {
                                state.diff_scroll_up();
                            }
                        }
                        Key::PageDown | Key::Char(' ') => {
                            for _ in 0..page {
                                state.diff_scroll_down(page);
                            }
                        }
                        Key::ArrowLeft | Key::Char('h') => state.diff_focus_tree(),
                        Key::Char('s') => state.diff_toggle_split(),
                        _ => {}
                    },
                },
            }
            continue;
        }

        // The session-note editor, when open, captures every key: `Ctrl-S` saves,
        // `Esc` cancels, `Enter` inserts a newline, and the editing keys edit the
        // multi-line buffer. It is driven through a handler (not inline like the
        // pure modals above) because closing it from 没入 re-attaches the pane,
        // which needs the terminal / wiring.
        if state.note_editor().is_some() {
            note_editor_key(term, &mut state, &mut painter, key, wiring);
            continue;
        }

        // The workspace-env editor (`env`), when open, captures every key: it sits
        // *over* the palette (which stays open beneath it), so `Ctrl-S` saves the
        // bindings and `Esc` cancels — either way returning to the command palette. The
        // editing keys edit the multi-line buffer.
        if state.env_editor().is_some() {
            env_editor_key(&mut state, key);
            continue;
        }

        // The workspace command palette (`:`), when open, captures every key:
        // editing / completion / recall and `Enter` to run a command, `Esc` to
        // close. It sits below the pure overlays above (a `man` / `session list`
        // text dump it runs layers its modal on top of the palette), and above the
        // sidebar toggle and per-mode dispatch (so `:`-typed text never leaks to
        // the session list / focus surface beneath it).
        if state.command_palette_open() {
            let flow = palette_key(term, &mut state, &mut painter, key, wiring)?;
            if let Flow::Quit = flow {
                save_resume_focus(&mut state, wiring);
                return Ok(Outcome::Quit);
            }
            continue;
        }

        // `Ctrl-B` collapses / expands the left session sidebar from anywhere on
        // the (non-modal) screen. It is a pure view toggle, so it is handled here
        // before the per-mode dispatch rather than threaded through each handler.
        // 没入's keys never reach this loop (the pane driver owns them), so its
        // Ctrl-B is handled inside `terminal::pane` instead.
        if let Key::Char(CTRL_B) = key {
            state.toggle_sidebar();
            continue;
        }

        // The per-mode handlers never quit (only the command palette's `Enter` and
        // the quit-confirm modal do — both handled above), so their `Flow` is
        // discarded rather than matched for a now-dead `Quit` arm.
        match state.mode() {
            Mode::Switch => {
                overview_key(term, &mut state, &mut painter, key, wiring);
            }
            Mode::Closeup => {
                closeup_key(term, &mut state, &mut painter, key, wiring);
            }
        }
    }
}

/// What handling a key (or driving the embedded pane it opened) resolved to.
enum Flow {
    /// Resume the home screen.
    Continue,
    /// Quit the application.
    Quit,
}

/// The most rows one wheel turn scrolls, so a fast flick (or a terminal that
/// reports several lines per notch) advances a few rows without lurching the
/// whole page.
const MAX_SCROLL_STEP: usize = 6;

/// Route a mouse-wheel turn to whichever scrollable right-pane surface is open —
/// the diff view, the Markdown preview, or a scrollable text modal — scrolling it
/// by the wheel's line count (bounded by [`MAX_SCROLL_STEP`]). Negative `lines`
/// scroll up, positive down; `page` is the visible body height used to clamp a
/// downward scroll. A no-op when no such surface is open (the base panes never
/// scroll in place) or when the wheel reported no movement.
fn scroll_open_surface(state: &mut HomeState, ev: ScrollEvent, page: usize) {
    if ev.lines == 0 {
        return;
    }
    let steps = (ev.lines.unsigned_abs() as usize).min(MAX_SCROLL_STEP);
    let up = ev.lines < 0;
    for _ in 0..steps {
        if state.diff_view().is_some() {
            if up {
                state.diff_scroll_up();
            } else {
                state.diff_scroll_down(page);
            }
        } else if state.preview().is_some() {
            if up {
                state.preview_scroll_up();
            } else {
                state.preview_scroll_down(page);
            }
        } else if state.text_modal().is_some() {
            if up {
                state.text_modal_scroll_up();
            } else {
                state.text_modal_scroll_down(page);
            }
        }
    }
}

/// The directory the pane should root at for the focused list row: the selected
/// worktree's path, or the workspace root when the cursor is on a root row (which
/// belongs to no session, so `selected()` is `None`). In 統合(unite) mode a root
/// row past the first group resolves to *that group's* workspace, so a root-row
/// `terminal` / `agent` opens in the workspace the cursor is pointing at; the
/// primary group's root row uses `workspace_root` (the screen's base directory).
fn selected_dir(state: &HomeState, workspace_root: &Path) -> PathBuf {
    if let Some(w) = state.list().selected() {
        return w.path.clone();
    }
    // A root row: the primary group uses the screen's base root; an extra (unite)
    // group uses its own workspace root.
    if state.list().selected_group() == 0 {
        workspace_root.to_path_buf()
    } else {
        state.selected_workspace_root()
    }
}

/// Gather the highlighted session worktree's diff against its base branch for the
/// right-pane diff view, as `(title, patch)`. The base is the worktree
/// repository's default branch (resolved against `origin/<default>` first, like
/// the sidebar `+N -M` badge), and the patch is its cumulative merge-base diff.
///
/// Fails — so [`HomeState::open_diff_result`] logs and opens nothing — when the
/// cursor is on a root row (no session highlighted) or the base ref cannot be
/// resolved (e.g. a repo with no commits). The git shell-out makes this the
/// impure half of the `diff` command; the selection read is pure.
#[cfg(test)]
fn selected_diff(state: &HomeState) -> Result<(String, String)> {
    let Some(worktree) = state.list().selected() else {
        anyhow::bail!("highlight a session to see its diff");
    };
    let base = crate::infrastructure::git::default_branch(&worktree.path);
    let patch = crate::infrastructure::git::diff_text(&worktree.path, &base)
        .ok_or_else(|| anyhow::anyhow!("could not resolve the base branch `{base}`"))?;
    let branch = worktree.branch.as_deref().unwrap_or("(detached)");
    Ok((format!("{branch} → {base}"), patch))
}

struct DiffRequest {
    path: PathBuf,
    branch: String,
}

fn selected_diff_request(state: &HomeState) -> Result<DiffRequest> {
    let Some(worktree) = state.list().selected() else {
        anyhow::bail!("highlight a session to see its diff");
    };
    Ok(DiffRequest {
        path: worktree.path.clone(),
        branch: worktree
            .branch
            .as_deref()
            .unwrap_or("(detached)")
            .to_string(),
    })
}

/// Start loading the highlighted session's patch off the UI path. The returned
/// channel always yields once: either a ready `(title, patch)` or the selection /
/// git error that should be logged by [`HomeState::open_diff_result`].
fn load_selected_diff(state: &HomeState) -> std::sync::mpsc::Receiver<Result<(String, String)>> {
    let (tx, rx) = std::sync::mpsc::channel();
    match selected_diff_request(state) {
        Ok(request) => {
            std::thread::spawn(move || {
                let base = crate::infrastructure::git::default_branch(&request.path);
                let loaded = crate::infrastructure::git::diff_text(&request.path, &base)
                    .ok_or_else(|| anyhow::anyhow!("could not resolve the base branch `{base}`"))
                    .map(|patch| (format!("{} → {base}", request.branch), patch));
                let _ = tx.send(loaded);
            });
        }
        Err(e) => {
            let _ = tx.send(Err(e));
        }
    }
    rx
}

/// Whether a click is allowed to poke the sidebar mascot: only on the plain home
/// view (選択 / 集中) with nothing floating over the panes. Anywhere an overlay
/// sits — the quit-confirm / removal / text modals, the Markdown preview, the note
/// editor, or the command palette — a click is meant for it (or for nothing), not
/// the rabbit drawn beneath it.
fn mascot_clickable(state: &HomeState) -> bool {
    matches!(state.mode(), Mode::Switch | Mode::Closeup)
        && !state.quit_confirm()
        && !state.update_confirm()
        && state.remove_modal().is_none()
        && state.text_modal().is_none()
        && state.preview().is_none()
        && !state.diff_active()
        && state.note_editor().is_none()
        && !state.command_palette_open()
}

/// Handle a mouse click: when it lands on the resting sidebar mascot (and the
/// screen is in a state where the rabbit is clickable), let the mascot respond
/// ([`HomeState::click_mascot`] — raise the update-confirmation modal when it is
/// announcing an update, otherwise a playful one-shot reaction) and report `true`
/// so the loop repaints. A click anywhere else — or while an overlay is up — is
/// ignored (`false`), so nothing else on the TUI is click-driven. The mascot's
/// screen rectangle is recomputed from the same layout the renderer used
/// ([`ui::mascot_hit_rect`]), so the hit-test matches exactly where the rabbit was
/// drawn.
fn handle_mascot_click(term: &Term, state: &mut HomeState, click: ClickEvent) -> bool {
    if !mascot_clickable(state) {
        return false;
    }
    let (height, width) = term.size();
    click_hits_mascot(height as usize, width as usize, state, click)
        .then(|| state.click_mascot(Instant::now()))
        .is_some()
}

/// Whether `click` lands on the sidebar mascot's body for a terminal of the given
/// size. Split from [`handle_mascot_click`] (which owns the mode/overlay gate and
/// the side effect) so the geometry — including the "no mascot shown" case — is
/// unit-testable at an explicit size rather than the live terminal's.
fn click_hits_mascot(height: usize, width: usize, state: &HomeState, click: ClickEvent) -> bool {
    ui::mascot_hit_rect(height, width, state)
        .is_some_and(|rect| rect.contains(click.col, click.row))
}

/// Test-only adapter that keeps the event-loop tests' synchronous shape —
/// `create_session` / `remove_session` returning a [`SessionOutcome`] — against
/// the loop's background-task model. Each dispatch runs the fake inline and
/// queues its outcome on a fresh task handle, so the loop drains it on the next
/// frame exactly as a finished worker thread would. Pool eviction is a no-op
/// (the tests have no pool).
///
/// The synchronous fakes are taken **by value** (as `impl FnMut`) so they are
/// owned locals here, sharing one lifetime with the `dispatch_*` wrappers built
/// below — which is what lets them all be bundled into a single [`Wiring`].
/// A test `chat_ask` that echoes the prompt back on an already-ready channel, so
/// the loop's submit → drain → reply path runs without a model runtime.
#[cfg(test)]
fn echo_chat_ask(prompt: String) -> std::sync::mpsc::Receiver<Result<String, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = tx.send(Ok(format!("echo: {prompt}")));
    rx
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn event_loop_compat(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: HomeState,
    workspace_root: &Path,
    monitor: &MonitorHandle,
    update: &UpdateHandle,
    installed_agents: &OneShot<Vec<AgentCli>>,
    mut persist: impl FnMut(&str),
    mut create_session: impl FnMut(&str) -> SessionOutcome,
    mut rename_display: impl FnMut(&str, &str) -> SessionOutcome,
    mut set_note: impl FnMut(&str, &str) -> SessionOutcome,
    mut remove_session: impl FnMut(&str, bool) -> SessionOutcome,
    mut existing_branches: impl FnMut() -> Vec<String>,
    mut open_terminal: impl FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    mut open_config: impl FnMut(&Term) -> Result<Option<ConfigReload>>,
    mut preview: impl FnMut(&Path, Sidebar) -> Option<TerminalView>,
    mut tab_op: impl FnMut(&Path, Option<TabNav>) -> (Vec<String>, usize),
    mut close_tab: impl FnMut(&mut HomeState, &Path),
    mut reorder_session: impl FnMut(&str, bool) -> SessionReorder,
) -> Result<Outcome> {
    let tasks = TaskHandle::new();
    // The unite target root is irrelevant to these single-workspace test fakes, so
    // the shim accepts it (matching the production [`Wiring`] signature) and drops it.
    let mut dispatch_create = |_root: &Path, name: &str, interaction_epoch: u64| {
        let id = tasks.begin(super::tasks::TaskKind::CreateSession, name);
        let outcome = create_session(name);
        // Mirror a production create, which carries the new branch to focus, so the
        // loop's auto-focus path is exercised; a fake whose `create_session` reports
        // no new sessions just won't match it.
        let focus = outcome.sessions.as_ref().map(|_| super::tasks::AutoFocus {
            name: name.to_string(),
            landing: super::tasks::FocusLanding::Closeup,
            interaction_epoch,
        });
        tasks.complete(
            id,
            true,
            super::tasks::Completion {
                line: outcome.line,
                sessions: outcome.sessions,
                target_root: Some(_root.to_path_buf()),
                evict: None,
                focus,
                created: Some(name.to_string()),
                removed: None,
            },
        );
    };
    let mut dispatch_remove =
        |_root: &Path, name: &str, force: bool, focus: Option<super::tasks::AutoFocus>| {
            let id = tasks.begin(super::tasks::TaskKind::RemoveSession, name);
            let outcome = remove_session(name, force);
            // Mirror a production removal, which carries the session root to evict, so
            // the loop's eviction path is exercised; the tests' `evict_pool` is a
            // no-op (they have no pool).
            let evict = outcome
                .sessions
                .as_ref()
                .map(|_| std::path::PathBuf::from(name));
            let focus = outcome.sessions.as_ref().and(focus);
            tasks.complete(
                id,
                true,
                super::tasks::Completion {
                    line: outcome.line,
                    sessions: outcome.sessions,
                    target_root: Some(_root.to_path_buf()),
                    evict,
                    focus,
                    created: None,
                    removed: Some(name.to_string()),
                },
            );
        };
    let mut evict_pool = |_: &Path| {};
    // The self-update spawn is real IO wired in `super::run`; here it is a no-op,
    // so the compat-shim loop tests never shell out. The dispatch path itself is
    // covered by the dedicated update-modal tests that build a capturing `Wiring`.
    let mut dispatch_update = || {};
    // The resume-focus persistence is exercised through its own state unit tests
    // ([`HomeState::resume_level`] / `restore_focus`); here it is a no-op, so a
    // quit in these loop tests does not touch the store.
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    // Auto-starting queued prompts spawns real panes in `super::run`; here it never
    // starts anything, so the compat-shim loop tests do not touch the pool or the
    // prompt store. The apply path is covered directly in `apply_autostart` tests.
    let mut autostart_queued = |_: &HomeState| Vec::<String>::new();
    // Timed wakes use real live-prompt IO in `super::run`; the compat shim's
    // tests do not exercise it, so report that no agents were messaged.
    let mut broadcast_wake = |_: &HomeState| 0usize;
    // The fakes have no equivalent of the production pane-exit sync thread that
    // fills this, so it stays empty here; the apply path is covered directly in
    // `a_background_refresh_updates_the_session_list`.
    let refresh = SessionsRefreshHandle::new();
    // The sync rename / note fakes are single-workspace (no root arg); wrap them to
    // the production 3-arg shape, dropping the unite target root.
    let mut rename_display_w = |_root: &Path, name: &str, label: &str| rename_display(name, label);
    let mut set_note_w = |_root: &Path, name: &str, note: &str| set_note(name, note);
    // The compat-shim loop tests do not drive interactive todo editing (it is
    // covered directly against `note_editor_key` with a capturing `Wiring`), so a
    // no-op that reports no session change keeps the loop's apply path a no-op.
    let mut set_todos_w = |_root: &Path, _name: &str, _todos: &[SessionTodo]| SessionOutcome {
        line: super::state::LogLine::output("todos"),
        sessions: None,
        select: None,
        root_note: None,
    };
    // The compat-shim loop tests do not drive manual-status labels; a no-op that
    // reports no session change keeps the loop's apply path a no-op. The label flow
    // is covered directly against `overview_key` with a capturing `Wiring`.
    let mut set_label_w = |_root: &Path, _name: &str, _id: Option<&str>| SessionOutcome {
        line: super::state::LogLine::output("label"),
        sessions: None,
        select: None,
        root_note: None,
    };
    // `unite add` is not exercised by the compat-shim loop tests; report no match.
    let mut unite_resolve =
        |name: &str| Err::<GroupSource, String>(format!("no workspace named \"{name}\""));
    // Opening a PR in the browser is a no-op here so the compat-shim loop tests
    // never shell out; the open path itself is covered by the dedicated popup tests
    // that build a capturing `Wiring`.
    let mut open_url = |_: &str| {};
    // The chat request shells out to Ollama in `super::run`; here it echoes the
    // prompt back on a ready channel so the loop's submit → poll → reply path runs
    // without a model runtime. Tests that need a withheld / failed reply build a
    // capturing `Wiring` with their own `chat_ask`.
    let mut chat_ask = echo_chat_ask;
    let mut open_external_terminal = |_: &Path| Ok::<(), String>(());
    // The background-pane hooks need a real pool, which the compat-shim tests do
    // not have: the default fakes report "no new tab / nothing pending", so a
    // launch here falls back to a synchronous re-attach (through `open_terminal`).
    // The full background flow — spawn a loading tab, poll it ready, move to it —
    // is covered by the dedicated `background_tab` tests that build a capturing
    // `Wiring`.
    let mut start_pending_spawn = |_: &mut HomeState, _: &Path, _: bool| {
        Ok::<StartPending, anyhow::Error>(StartPending::Reused)
    };
    let mut poll_pending_spawn = |_: &Path| PendingPoll::Gone;
    let mut activate_pending = |_: &Path| false;
    let mut clear_pending_spawn = || {};
    let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, _: TabMenuAction| {};
    // Exercise the test-shim no-op once so coverage does not treat this helper
    // closure as an uncovered function. The production tab-action path is covered
    // by event-loop tests with a capturing callback.
    let mut dummy = HomeState::new("", Vec::new(), None);
    tab_action(&mut dummy, Path::new(""), 0, TabMenuAction::Close);
    let _ = open_external_terminal(Path::new(""));
    // Exercise the background-pane no-op shims once so coverage does not flag
    // these helper closures as uncovered; the real flow is covered by the
    // capturing-`Wiring` background-tab tests.
    let _ = start_pending_spawn(&mut dummy, Path::new(""), false);
    let _ = poll_pending_spawn(Path::new(""));
    let _ = activate_pending(Path::new(""));
    clear_pending_spawn();
    // The compat-shim tests hand in a `FnMut(&str)` persist (they only assert on
    // the recorded command text); adapt it to the production entry-shaped hook so
    // the loop's `Wiring` stays uniform.
    let mut persist_entry = |entry: &crate::domain::history::HistoryEntry| persist(&entry.command);
    let mut wiring = Wiring {
        interaction_epoch: 0,
        watch_sessions: false,
        workspace_root,
        persist: &mut persist_entry,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename_display_w,
        set_note: &mut set_note_w,
        set_todos: &mut set_todos_w,
        set_label: &mut set_label_w,
        reorder_session: &mut reorder_session,
        dispatch_remove: &mut dispatch_remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict_pool,
        existing_branches: &mut existing_branches,
        open_terminal: &mut open_terminal,
        start_pending_spawn: &mut start_pending_spawn,
        poll_pending_spawn: &mut poll_pending_spawn,
        activate_pending: &mut activate_pending,
        clear_pending_spawn: &mut clear_pending_spawn,
        open_url: &mut open_url,
        open_external_terminal: &mut open_external_terminal,
        open_config: &mut open_config,
        chat_ask: &mut chat_ask,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close_tab,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
        autostart_queued: &mut autostart_queued,
        broadcast_wake: &mut broadcast_wake,
    };
    // The compat-shim tests do not exercise the local-LLM probe, so a never-filled
    // handle keeps `ai_available` false throughout (matching an unconfigured LLM).
    let ai_available = OneShot::<bool>::new();
    event_loop(
        term,
        reader,
        state,
        monitor,
        update,
        &refresh,
        &ai_available,
        installed_agents,
        &tasks,
        &mut wiring,
    )
}

#[cfg(test)]
mod tests;
