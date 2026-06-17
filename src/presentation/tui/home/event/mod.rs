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

use crate::presentation::tui::screen::{FramePainter, KeyReader};

use super::state::{HomeState, Mode, PaneExit, SessionOutcome};
use super::terminal_pool::MonitorHandle;
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
///   via `session switch`, or from Focus / Attached via `Ctrl-O`). `↑`/`↓`
///   move, `Enter` focuses (attaching when the session is live), `c` creates one
///   inline, `Esc` / `h` backs out to where it was opened from, `Ctrl-O` zooms
///   further out to Overview.
/// - **在席 (Focus)** — a session is selected and operated in the right pane,
///   either as a menu of its runnable commands or a session-scoped prompt
///   (chosen by the [`SessionActionUi`] setting). Launching `terminal` / `agent`
///   attaches the pane; `Esc` returns to Overview; `Ctrl-O` opens Switch.
/// - **没入 (Attached)** — the embedded shell / agent is live in the right pane
///   and keys flow to it. `Ctrl-O` is the only reserved key (everything else,
///   including `Esc`, goes to the shell): it zooms out to Switch. The shell
///   exiting returns to Focus.
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
/// focused worktree — or at `workspace_root` for the root row. Its `bool` is
/// `true` for `agent`, `false` for a plain `terminal`. It returns a [`PaneExit`]:
/// [`PaneExit::Closed`] (the shell exited → 在席) or [`PaneExit::ToSwitch`]
/// (`Ctrl-O` → 切替). The PTY I/O, rendering, and shell pool live in that
/// injected callback.
///
/// `open_config` opens the settings screen, returning `true` when the user quit
/// the application from it (so the loop propagates [`Outcome::Quit`]).
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
    remove_session: &mut dyn FnMut(&str, bool) -> SessionOutcome,
    existing_branches: &mut dyn FnMut() -> Vec<String>,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    open_config: &mut dyn FnMut(&Term) -> Result<bool>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
) -> Result<Outcome> {
    let mut painter = FramePainter::new();
    loop {
        // Mark each background session's agent state — running, waiting for
        // input, live (ready), and finished — before painting.
        state.set_running(monitor.running());
        state.set_waiting(monitor.waiting());
        state.set_live(monitor.live());
        state.set_done(monitor.done());
        // Surface the top-right "update available" notice once the background
        // release check has found a newer version than this build.
        state.set_update(update.status().map(|status| status.latest));
        // Drop any stale snapshot every frame, then refresh it for the modes that
        // draw the embedded terminal: 没入 (driven directly by `open_pane`) and
        // 切替, where the right pane previews the highlighted session's live
        // terminal so the user sees the actual screen re-attaching reveals.
        state.clear_terminal_view();
        if state.mode() == Mode::Switch {
            let dir = selected_dir(&state, workspace_root);
            if let Some(view) = preview(&dir) {
                state.set_terminal_view(view);
            }
        }
        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &state);
        painter.paint(term, frame)?;

        // The TUI itself never scrolls, so a wheel turn is read and dropped here
        // (it is swallowed by the reader before it can reach the host terminal's
        // viewport and reveal the pre-launch scrollback). The embedded terminal
        // pane has its own history scroll, handled separately.
        let key = match reader.read_key() {
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
                        for name in names {
                            let outcome = remove_session(&name, force);
                            state.apply_session_outcome(outcome);
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
                existing_branches,
                open_terminal,
                preview,
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
