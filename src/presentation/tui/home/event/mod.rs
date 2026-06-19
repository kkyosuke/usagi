//! The home (workspace) screen's event loop: read a key, dispatch it by mode,
//! repaint, repeat — until the user quits.
//!
//! This module owns the loop itself ([`event_loop`]), the modal key capture
//! (quit-confirm / removal / text modals), and the shared [`Flow`] outcome and
//! [`selected_dir`] helper. The per-mode key handlers it dispatches to — and
//! `open_pane`, which drives the embedded terminal — live in [`handlers`].

use std::path::{Path, PathBuf};

use anyhow::Result;
use console::Key;
use console::Term;

use crate::domain::settings::SessionActionUi;
use crate::presentation::tui::install_task;
use crate::presentation::tui::screen::{animated_read, FramePainter, KeyReader};

use super::state::{HomeState, Mode, PaneExit, SessionOutcome};
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
/// Each command the user runs is handed to `persist` so the caller can append
/// it to the workspace's `history.json`; tests pass a no-op.
///
/// Creating a session (the user typed a name inline in Switch) is delegated to
/// `create_session`; removing one (`session remove <name>`) to `remove_session`
/// (its `bool` is the `--force` flag). Both perform the git / filesystem work and
/// return a [`SessionOutcome`] to apply to the screen, keeping the loop itself
/// free of that IO and directly testable.
///
/// `open_terminal` embeds a live shell in the right pane (没入), rooted at the
/// focused worktree — or at `workspace_root` for the root row. Its first `bool`
/// is `true` for `agent`, `false` for a plain `terminal`; its second (`new_pane`)
/// is `true` to add a fresh pane (在席's `terminal` / `agent`) or `false` to
/// re-attach the session's active pane. It returns a [`PaneExit`]:
/// [`PaneExit::Closed`] (the shell exited → 在席) or [`PaneExit::ToSwitch`]
/// (`Ctrl-O` → 切替). The PTY I/O, rendering, and shell pool live in that
/// injected callback.
///
/// `tab_op` reads (and, given a [`TabNav`], navigates) the highlighted session's
/// tabs from 切替 — the loop reads the strip each frame to draw it, and `←`/`→`
/// move the active tab so re-attaching reveals it.
///
/// `open_config` opens the settings screen, returning `None` when the user quit
/// the application from it (so the loop propagates [`Outcome::Quit`]), or
/// `Some(ui)` with the re-read [`SessionActionUi`] when it returns to home — so a
/// changed Focus surface takes effect without reopening the home screen.
///
/// `preview` snapshots the live terminal of the session rooted at a path, or
/// `None` when it has no running shell/agent — used to decide whether focusing /
/// switching to a session attaches its pane.
#[allow(clippy::too_many_arguments)]
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut state: HomeState,
    workspace_root: &Path,
    monitor: &MonitorHandle,
    update: &UpdateHandle,
    persist: &mut dyn FnMut(&str),
    create_session: &mut dyn FnMut(&str) -> SessionOutcome,
    rename_display: &mut dyn FnMut(&str, &str) -> SessionOutcome,
    remove_session: &mut dyn FnMut(&str, bool) -> SessionOutcome,
    existing_branches: &mut dyn FnMut() -> Vec<String>,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    open_config: &mut dyn FnMut(&Term) -> Result<Option<ConfigReload>>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
    tab_op: &mut TabOp<'_>,
) -> Result<Outcome> {
    let mut painter = FramePainter::new();
    loop {
        // Mark each background session's agent state — running, waiting for
        // input, live (ready), and finished — before painting, reading every
        // badge set together under a single lock.
        let badges = monitor.snapshot();
        state.set_running(badges.running);
        state.set_waiting(badges.waiting);
        state.set_live(badges.live);
        state.set_done(badges.done);
        // Surface the top-right "update available" notice once the background
        // release check has found a newer version than this build.
        state.set_update(update.status().map(|status| status.latest));
        // Drop any stale snapshot / tab strip every frame, then refresh them for
        // the modes that draw the embedded terminal: 没入 (driven directly by
        // `open_pane`) and 切替, where the right pane previews the highlighted
        // session's live terminal — with its tab strip above it, so `←`/`→` has
        // something to act on — so the user sees the actual screen re-attaching
        // reveals.
        state.clear_terminal_view();
        state.clear_terminal_tabs();
        if state.mode() == Mode::Switch {
            let dir = selected_dir(&state, workspace_root);
            if let Some(view) = preview(&dir) {
                state.set_terminal_view(view);
                let (labels, active) = tab_op(&dir, None);
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
        let key = match animated_read(reader, term, &mut painter, &install_task::handle()) {
            Ok(key) => key,
            // An interrupted read (e.g. a delivered signal) means quit.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
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
                Key::ArrowUp | Key::Char('k') => state.remove_modal_move_up(),
                Key::ArrowDown | Key::Char('j') => state.remove_modal_move_down(),
                Key::Char(' ') => state.remove_modal_toggle(),
                Key::Enter => {
                    if let Some((names, force)) = state.submit_remove_modal() {
                        // Bulk removal blocks the loop (each session is git /
                        // filesystem work), so show the loading rabbit in the
                        // top-right and step it per session — repainting before
                        // each removal so it hops along with the progress count.
                        let total = names.len();
                        for (i, name) in names.iter().enumerate() {
                            state.step_loading(format!("削除中… {}/{total}", i + 1));
                            let _ = paint_now(term, &mut painter, &state);
                            let outcome = remove_session(name, force);
                            state.apply_session_outcome(outcome);
                        }
                        state.finish_loading();
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

        let flow = match state.mode() {
            Mode::Overview => overview_key(
                term,
                reader,
                &mut state,
                &mut painter,
                workspace_root,
                key,
                persist,
                create_session,
                remove_session,
                existing_branches,
                open_terminal,
                open_config,
                preview,
            )?,
            Mode::Switch => switch_key(
                term,
                reader,
                &mut state,
                &mut painter,
                workspace_root,
                key,
                create_session,
                rename_display,
                existing_branches,
                open_terminal,
                preview,
                tab_op,
            ),
            // 没入 (Attached) is driven inside `open_pane`, which always leaves it
            // (for 切替 or 在席) before returning — so the loop only ever observes
            // 在席 here. It shares the 在席 handler to keep the match total without a
            // separate, unreachable arm.
            Mode::Focus | Mode::Attached => focus_key(
                term,
                reader,
                &mut state,
                &mut painter,
                workspace_root,
                key,
                remove_session,
                open_terminal,
                preview,
                tab_op,
            ),
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

#[cfg(test)]
mod tests;
