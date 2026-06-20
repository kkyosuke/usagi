//! The home (workspace) screen's event loop: read a key, dispatch it by mode,
//! repaint, repeat — until the user quits.
//!
//! This module owns the loop itself ([`event_loop`]), the modal key capture
//! (quit-confirm / removal / text modals), and the shared [`Flow`] outcome and
//! [`selected_dir`] helper. The per-mode key handlers it dispatches to — and
//! `open_pane`, which drives the embedded terminal — live in [`handlers`].

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use console::Key;
use console::Term;

use crate::domain::settings::SessionActionUi;
use crate::presentation::tui::install_task;
use crate::presentation::tui::screen::{FramePainter, KeyReader};

use super::state::{HomeState, Mode, PaneExit, SessionOutcome};
use super::tasks::TaskHandle;
use super::terminal_pool::MonitorHandle;
use super::terminal_tabs::TabNav;
use super::terminal_view::TerminalView;
use super::ui;
use super::update::UpdateHandle;

mod handlers;

use handlers::{focus_key, overview_key, switch_key};

/// The byte `console` reports for `Ctrl-O` on the home screen: a bare control
/// character (`0x0f`), since `console` only special-cases a handful of control
/// keys and passes the rest through as [`Key::Char`]. `Ctrl-O` zooms out one
/// engagement level (没入 → 切替 → 統括) everywhere on the screen.
const CTRL_O: char = '\u{000f}';

/// The bare control characters `console` reports for `Ctrl-N` (`0x0e`) and
/// `Ctrl-P` (`0x10`) on the home screen — the same passthrough as [`CTRL_O`].
/// They move between the focused session's tabs (`Ctrl-P` previous / `Ctrl-N`
/// next) in 切替 / 在席, matching the chords 没入 uses for the same move.
const CTRL_N: char = '\u{000e}';
const CTRL_P: char = '\u{0010}';

/// The callback 切替 uses to read (`None`) or navigate (`Some(nav)`) the
/// highlighted session's tabs, returning the strip's labels and active index.
/// Backed by the [`TerminalPool`](super::terminal_pool::TerminalPool) the pane
/// driver shares, so a tab moved here is the one re-attaching reveals.
pub(super) type TabOp<'a> = dyn FnMut(&Path, Option<TabNav>) -> (Vec<String>, usize) + 'a;

/// The settings-derived values re-read when the config screen closes, so an
/// edit takes effect without reopening the home screen: the 在席 (Focus)
/// right-pane surface and whether the `ai` command is offered.
#[derive(Debug, Clone, Copy)]
pub struct ConfigReload {
    /// The effective Session Action UI (在席 mode's surface).
    pub session_action_ui: SessionActionUi,
    /// Whether the local LLM is usable (enabled and its model pulled), gating
    /// the `ai` command in the 在席 menu.
    pub ai_available: bool,
}

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
    /// The workspace root: where the root row's pane is rooted, and the base
    /// [`selected_dir`] falls back to when the cursor is on the root row.
    pub workspace_root: &'a Path,
    /// Append a run command to the workspace history (best-effort; tests no-op).
    pub persist: &'a mut dyn FnMut(&str),
    /// Dispatch `session create <name>` to a background worker.
    pub dispatch_create: &'a mut dyn FnMut(&str),
    /// Rename a session's sidebar label, returning the outcome to apply inline.
    pub rename_display: &'a mut dyn FnMut(&str, &str) -> SessionOutcome,
    /// Dispatch `session remove <name>` to a background worker (`bool` = force).
    pub dispatch_remove: &'a mut dyn FnMut(&str, bool),
    /// Evict a removed session's pooled shell, run on the loop thread (the pool
    /// is not `Send`).
    pub evict_pool: &'a mut dyn FnMut(&Path),
    /// The branch names already taken across the workspace, read fresh so the
    /// inline create input can validate against duplicates.
    pub existing_branches: &'a mut dyn FnMut() -> Vec<String>,
    /// Embed a live shell in the right pane (没入) and drive it: the first `bool`
    /// is `agent` vs plain `terminal`, the second `new_pane` vs re-attach.
    pub open_terminal: &'a mut dyn FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    /// Open the settings screen, re-reading the affected settings on return
    /// (`None` when the user quit the app from it).
    pub open_config: &'a mut dyn FnMut(&Term) -> Result<Option<ConfigReload>>,
    /// Snapshot a session's live terminal for the 切替 preview, or `None` when it
    /// has no running shell — also the live/idle test the focus handlers use.
    pub preview: &'a mut dyn FnMut(&Path) -> Option<TerminalView>,
    /// Read (`None`) or navigate (`Some(nav)`) the highlighted session's tabs
    /// from 切替.
    pub tab_op: &'a mut TabOp<'a>,
    /// Close the highlighted session's active tab (pane) from 切替.
    pub close_tab: &'a mut dyn FnMut(&mut HomeState, &Path),
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
/// The screen is a four-step engagement ladder:
///
/// - **統括 (Overview)** — the default. The bottom command line operates the
///   whole workspace (`session` / `config` / `doctor` / `man` / …); results are
///   appended to the log, rendered below the input. The right pane is blank.
///   `Esc` is inert here (it does not back out to the project list); `Ctrl-O`
///   opens Switch.
/// - **切替 (Switch)** — pick a session in the left pane (entered from Overview
///   via `session switch`, or from Focus / Attached via `Ctrl-O`). `↑`/`↓` (or
///   `k`/`j`) move between sessions, `←`/`→` (or `h`/`l`, or `Ctrl-P`/`Ctrl-N`)
///   move between the highlighted session's tabs, `Enter` focuses (attaching when
///   the session is live), `t` opens the action surface to add a pane, `c`
///   creates a session inline, `Esc` backs out to where it was opened from,
///   `Ctrl-O` zooms further out to Overview.
/// - **在席 (Focus)** — a session is selected and operated in the right pane,
///   either as a menu of its runnable commands or a session-scoped prompt
///   (chosen by the [`SessionActionUi`] setting). Launching `terminal` / `agent`
///   adds a pane and attaches it; `Esc` returns to Overview; `Ctrl-O` opens
///   Switch; `Ctrl-P`/`Ctrl-N` move the focused session's active tab.
/// - **没入 (Attached)** — the embedded shell / agent is live in the right pane
///   and keys flow to it. The reserved keys are `Ctrl-O` (zoom out to Switch,
///   where panes are added) and `Ctrl-P`/`Ctrl-N` (switch to the previous / next
///   tab in place, without detaching); everything else, including `Esc`, goes to
///   the shell. The shell exiting returns to Focus.
///
/// The workspace root and every side-effecting hook the loop drives — appending
/// run commands to history, dispatching background session create / remove,
/// embedding the terminal pane, previewing / navigating tabs, opening the
/// settings screen — are bundled into [`Wiring`]; see its fields for each hook's
/// contract. Tests build a `Wiring` of fakes (via [`event_loop_compat`]) so the
/// loop's logic is exercised without a real terminal or shell pool.
pub(super) fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut state: HomeState,
    monitor: &MonitorHandle,
    update: &UpdateHandle,
    tasks: &TaskHandle,
    wiring: &mut Wiring,
) -> Result<Outcome> {
    let workspace_root = wiring.workspace_root;
    let mut painter = FramePainter::new();
    loop {
        // Mark each background session's agent state — running, waiting for
        // input, live (ready), and finished — before painting, applying every
        // badge set together (read under a single lock) so the frame never mixes
        // one set's fresh reading with another's stale one.
        state.apply_badges(monitor.snapshot());
        // Surface the top-right "update available" notice once the background
        // release check has found a newer version than this build.
        state.set_update(update.status().map(|status| status.latest));
        // Apply any background session task (create / remove) that finished since
        // the last frame: evict the removed session's pooled shell (on this
        // thread — the pool is not `Send`), then log the result and refresh the
        // session list without yanking the cursor. Then refresh the task panel
        // rows so in-flight work shows in the top-right corner.
        for completion in tasks.drain_completed() {
            let super::tasks::Completion {
                line,
                sessions,
                evict,
            } = completion;
            if let Some(path) = evict {
                (wiring.evict_pool)(&path);
            }
            state.apply_task_completion(line, sessions);
        }
        state.set_tasks(tasks.view(Instant::now()));
        // Drop any stale surface every frame, then refresh it for the modes that
        // draw the embedded terminal: 没入 (driven directly by `open_pane`, which
        // clears its own surface on the way out) and 切替, where the right pane
        // previews the highlighted session's live terminal — with its tab strip
        // above it, so `←`/`→` has something to act on — so the user sees the
        // actual screen re-attaching reveals.
        state.clear_terminal_surface();
        if state.mode() == Mode::Switch {
            let dir = selected_dir(&state, workspace_root);
            if let Some(view) = (wiring.preview)(&dir) {
                state.set_terminal_view(view);
                let (labels, active) = (wiring.tab_op)(&dir, None);
                state.set_terminal_tabs(labels, active);
            }
        }
        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &state);
        painter.paint(term, frame)?;

        // The TUI itself never scrolls, so a wheel turn is read and dropped here
        // (it is swallowed by the reader before it can reach the host terminal's
        // viewport and reveal the pre-launch scrollback). The embedded terminal
        // pane has its own history scroll, handled separately.
        //
        // While a background install or a session task is in flight the read
        // wakes every `ANIM_TICK` so the loop re-iterates — re-draining finished
        // work and repainting, which advances the task panel's and install
        // rabbit's time-based animation. With nothing in flight it blocks on the
        // next key, so an idle screen costs nothing.
        let now = Instant::now();
        let animate = install_task::handle().is_active(now) || tasks.is_active(now);
        let key = if animate {
            match reader.read_key_timeout(install_task::ANIM_TICK) {
                Ok(Some(key)) => key,
                // A tick with no key: re-iterate to drain and repaint.
                Ok(None) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
                Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
            }
        } else {
            match reader.read_key() {
                Ok(key) => key,
                // An interrupted read (e.g. a delivered signal) means quit.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
                Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
            }
        };

        // The quit-confirmation modal, when open, captures every key: `y` /
        // `Enter` (or a second `Ctrl-C`) confirms the close, `n` / `Esc` cancels.
        if state.quit_confirm() {
            match key {
                Key::Char('y') | Key::Char('Y') | Key::Enter | Key::CtrlC => {
                    return Ok(Outcome::Quit)
                }
                Key::Char('n') | Key::Char('N') | Key::Escape => state.cancel_quit_confirm(),
                _ => {}
            }
            continue;
        }

        // `Ctrl-C` closes the app from anywhere on the home screen. Quitting
        // would drop any session whose shell / agent is still running, so when
        // one is live we raise the quit-confirmation modal first instead of
        // closing outright; an idle screen quits immediately.
        if let Key::CtrlC = key {
            if state.has_live_sessions() {
                state.open_quit_confirm();
            } else {
                return Ok(Outcome::Quit);
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
                    if let Some(modal) = state.remove_modal_mut() {
                        modal.move_up();
                    }
                }
                Key::ArrowDown | Key::Char('j') => {
                    if let Some(modal) = state.remove_modal_mut() {
                        modal.move_down();
                    }
                }
                Key::Char(' ') => {
                    if let Some(modal) = state.remove_modal_mut() {
                        modal.toggle();
                    }
                }
                Key::Enter => {
                    if let Some((names, force)) = state.submit_remove_modal() {
                        // Each checked session is dispatched to a background
                        // worker, so the loop never blocks on the git work; the
                        // task panel stacks them and the loop drains each as it
                        // finishes.
                        for name in &names {
                            (wiring.dispatch_remove)(name, force);
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
        if state.text_modal().is_some() {
            let page = ui::TEXT_MODAL_VISIBLE;
            match key {
                Key::ArrowUp | Key::Char('k') => state.text_modal_scroll_up(),
                Key::ArrowDown | Key::Char('j') => state.text_modal_scroll_down(page),
                Key::PageUp => {
                    for _ in 0..page {
                        state.text_modal_scroll_up();
                    }
                }
                Key::PageDown => {
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
                Key::PageDown => {
                    for _ in 0..page {
                        state.preview_scroll_down(page);
                    }
                }
                Key::Escape | Key::Enter | Key::Char('q') => state.close_preview(),
                _ => {}
            }
            continue;
        }

        let flow = match state.mode() {
            Mode::Overview => overview_key(term, &mut state, &mut painter, key, wiring)?,
            Mode::Switch => switch_key(term, &mut state, &mut painter, key, wiring),
            // 没入 (Attached) is driven inside `open_pane`, which always leaves it
            // (for 切替 or 在席) before returning — so the loop only ever observes
            // 在席 here. It shares the 在席 handler to keep the match total without a
            // separate, unreachable arm.
            Mode::Focus | Mode::Attached => focus_key(term, &mut state, &mut painter, key, wiring),
        };
        match flow {
            Flow::Continue => {}
            Flow::Quit => return Ok(Outcome::Quit),
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

/// Paint the current frame immediately, outside the loop's top-of-iteration
/// paint. Used to flush a transient [`HomeState`] state — the loading rabbit —
/// to the screen just before a blocking action runs, since the action would
/// otherwise hold the loop until it returned without ever drawing the indicator.
/// Errors are the caller's to ignore: a missed transient frame must not abort
/// the action it was announcing.
pub(super) fn paint_now(term: &Term, painter: &mut FramePainter, state: &HomeState) -> Result<()> {
    let (height, width) = term.size();
    let frame = ui::render_frame(height as usize, width as usize, state);
    painter.paint(term, frame)
}

/// The directory the pane should root at for the focused list row: the selected
/// worktree's path, or `workspace_root` when the cursor is on the root row
/// (which belongs to no session, so `selected()` is `None`).
fn selected_dir(state: &HomeState, workspace_root: &Path) -> PathBuf {
    state
        .list()
        .selected()
        .map(|w| w.path.clone())
        .unwrap_or_else(|| workspace_root.to_path_buf())
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
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn event_loop_compat(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: HomeState,
    workspace_root: &Path,
    monitor: &MonitorHandle,
    update: &UpdateHandle,
    mut persist: impl FnMut(&str),
    mut create_session: impl FnMut(&str) -> SessionOutcome,
    mut rename_display: impl FnMut(&str, &str) -> SessionOutcome,
    mut remove_session: impl FnMut(&str, bool) -> SessionOutcome,
    mut existing_branches: impl FnMut() -> Vec<String>,
    mut open_terminal: impl FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    mut open_config: impl FnMut(&Term) -> Result<Option<ConfigReload>>,
    mut preview: impl FnMut(&Path) -> Option<TerminalView>,
    mut tab_op: impl FnMut(&Path, Option<TabNav>) -> (Vec<String>, usize),
    mut close_tab: impl FnMut(&mut HomeState, &Path),
) -> Result<Outcome> {
    let tasks = TaskHandle::new();
    let mut dispatch_create = |name: &str| {
        let id = tasks.begin(super::tasks::TaskKind::CreateSession, name);
        let outcome = create_session(name);
        tasks.complete(
            id,
            true,
            super::tasks::Completion {
                line: outcome.line,
                sessions: outcome.sessions,
                evict: None,
            },
        );
    };
    let mut dispatch_remove = |name: &str, force: bool| {
        let id = tasks.begin(super::tasks::TaskKind::RemoveSession, name);
        let outcome = remove_session(name, force);
        // Mirror a production removal, which carries the session root to evict, so
        // the loop's eviction path is exercised; the tests' `evict_pool` is a
        // no-op (they have no pool).
        let evict = outcome
            .sessions
            .as_ref()
            .map(|_| std::path::PathBuf::from(name));
        tasks.complete(
            id,
            true,
            super::tasks::Completion {
                line: outcome.line,
                sessions: outcome.sessions,
                evict,
            },
        );
    };
    let mut evict_pool = |_: &Path| {};
    let mut wiring = Wiring {
        workspace_root,
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename_display,
        dispatch_remove: &mut dispatch_remove,
        evict_pool: &mut evict_pool,
        existing_branches: &mut existing_branches,
        open_terminal: &mut open_terminal,
        open_config: &mut open_config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close_tab,
    };
    event_loop(term, reader, state, monitor, update, &tasks, &mut wiring)
}

#[cfg(test)]
mod tests;
