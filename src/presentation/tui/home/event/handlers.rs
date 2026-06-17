//! The home screen's per-mode key handlers. The event loop in [`super`]
//! dispatches each key to one of the three entry handlers — `overview_key` /
//! `switch_key` / `focus_key` — by mode; those delegate to the helpers here
//! (`activate_named`, `leave_switch`, the focus-surface handlers, …) and to
//! `open_pane`, which drives the embedded terminal (没入). All are pure aside
//! from the injected callbacks.

use std::path::Path;

use anyhow::Result;
use console::Key;
use console::Term;

use crate::presentation::tui::screen::{self, FramePainter, KeyReader};

use crate::domain::settings::SessionActionUi;

use super::super::command::Effect;
use super::super::state::{HomeState, PaneExit, ReturnMode, SessionOutcome};
use super::super::terminal_view::TerminalView;
use super::{selected_dir, Flow, CTRL_O};

/// Handle one key in 統括 (Overview): edit / complete / recall the workspace
/// command line and run it on `Enter`, dispatching the resulting [`Effect`].
#[allow(clippy::too_many_arguments)]
pub(super) fn overview_key(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    key: Key,
    persist: &mut dyn FnMut(&str),
    create_session: &mut dyn FnMut(&str) -> SessionOutcome,
    remove_session: &mut dyn FnMut(&str, bool) -> SessionOutcome,
    existing_branches: &mut dyn FnMut() -> Vec<String>,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    open_config: &mut dyn FnMut(&Term) -> Result<bool>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
) -> Result<Flow> {
    match key {
        Key::Enter => {
            let submission = state.submit();
            if let Some(command) = submission.recorded.as_deref() {
                persist(command);
            }
            match submission.effect {
                Effect::Quit => return Ok(Flow::Quit),
                // `session switch` with no name moves keyboard focus to the left
                // pane to pick a session, returning here on cancel.
                Effect::EnterSwitch => state.enter_switch(ReturnMode::Overview),
                // `session switch <name>` focuses that session: if it resolves,
                // enter 在席 (attaching when it is live); otherwise log an error.
                Effect::Activate(name) => activate_named(
                    term,
                    reader,
                    state,
                    painter,
                    workspace_root,
                    &name,
                    open_terminal,
                    preview,
                ),
                // `session create <name>` creates directly; the screen rebuilds
                // its list and selects the new session.
                Effect::CreateSession(name) => {
                    let outcome = create_session(&name);
                    state.apply_session_outcome(outcome);
                }
                // `session create` with no name moves to 切替 and opens the inline
                // name input there (creation lives in Switch now).
                Effect::OpenSessionModal => {
                    state.enter_switch(ReturnMode::Overview);
                    state.switch_begin_create(existing_branches());
                }
                Effect::ListSessions => state.log_sessions(),
                Effect::RemoveSession { name, force } => {
                    let outcome = remove_session(&name, force);
                    state.apply_session_outcome(outcome);
                }
                Effect::OpenRemoveModal { force } => state.open_remove_modal(force),
                // `terminal` / `agent` are session commands, but the Overview line
                // still dispatches them if typed: focus the active session (the
                // root by default) and attach its pane.
                effect @ (Effect::OpenTerminal | Effect::OpenAgent) => {
                    let row = state.list().active_index();
                    state.enter_focus(row);
                    open_pane(
                        term,
                        reader,
                        state,
                        painter,
                        workspace_root,
                        open_terminal,
                        preview,
                        effect == Effect::OpenAgent,
                    );
                }
                // Hand off to the settings screen; it owns the terminal until
                // dismissed. Quitting there quits the app; otherwise we resume,
                // forcing a full repaint over the screen it drew.
                Effect::OpenConfig => {
                    if open_config(term)? {
                        return Ok(Flow::Quit);
                    }
                    painter.reset();
                }
                // `close` is a session command; the Overview line still
                // dispatches it if typed, closing the active session (the root by
                // default, which is a no-op since the root is not removable).
                Effect::CloseSession => close_focused_session(state, remove_session),
                // `ShowText` already opened its modal inside `submit`; nothing
                // more for the event loop to do.
                Effect::None | Effect::Clear | Effect::ShowText(_) => {}
            }
        }
        Key::Tab => state.complete(),
        Key::Backspace => state.backspace(),
        Key::ArrowUp => state.recall_prev(),
        Key::ArrowDown => state.recall_next(),
        // `Esc` is inert at the top level: the home screen is not left by backing
        // out (that would drop into the project list); the only way out is
        // `Ctrl-C`, handled centrally in the event loop.
        Key::Escape => {}
        // `Ctrl-O` opens 切替 (Switch) to pick a session in the left pane,
        // returning here on cancel.
        Key::Char(CTRL_O) => state.enter_switch(ReturnMode::Overview),
        Key::Char(c) => state.push_char(c),
        _ => {}
    }
    Ok(Flow::Continue)
}

/// Focus the session named `name` (from `session switch <name>`): if it resolves
/// in the worktree list, enter 在席 (Focus) on its row and, when the session is
/// live, attach the pane (没入); an unknown name logs an error and stays in
/// Overview.
#[allow(clippy::too_many_arguments)]
fn activate_named(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    name: &str,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
) {
    match resolve_row(state, name) {
        Some(row) => {
            state.enter_focus(row);
            // Attach straight away when the focused session is already live.
            let dir = selected_dir(state, workspace_root);
            if preview(&dir).is_some() {
                open_pane(
                    term,
                    reader,
                    state,
                    painter,
                    workspace_root,
                    open_terminal,
                    preview,
                    false,
                );
            }
        }
        None => {
            state.log_error(format!("no session named \"{name}\""));
        }
    }
}

/// The left-pane row a session `name` maps to (0 is the root row), or `None` when
/// no row matches. Mirrors the worktree list's `activate_by_name` resolution.
fn resolve_row(state: &HomeState, name: &str) -> Option<usize> {
    use super::super::state::{worktree_name, ROOT_NAME};
    if name == ROOT_NAME {
        return Some(0);
    }
    state
        .list()
        .worktrees()
        .iter()
        .position(|w| worktree_name(w) == name)
        .map(|i| i + 1)
}

/// Handle one key in 切替 (Switch): move the left-pane cursor, focus / attach a
/// session, drive the inline create input, or back out one level.
#[allow(clippy::too_many_arguments)]
pub(super) fn switch_key(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    key: Key,
    create_session: &mut dyn FnMut(&str) -> SessionOutcome,
    existing_branches: &mut dyn FnMut() -> Vec<String>,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
) -> Flow {
    // While the inline create input is open it captures every key.
    if state.is_creating() {
        match key {
            Key::Enter => {
                if let Some(name) = state.switch_confirm_create() {
                    let outcome = create_session(&name);
                    state.apply_session_outcome(outcome);
                    // The freshly created session is selected; focus it.
                    let row = state.list().selected_index();
                    state.enter_focus(row);
                }
            }
            Key::Backspace => state.create_backspace(),
            Key::Escape => state.create_cancel(),
            Key::Char(c) => state.create_push_char(c),
            _ => {}
        }
        return Flow::Continue;
    }

    match key {
        Key::ArrowUp | Key::Char('k') => state.switch_move_up(),
        Key::ArrowDown | Key::Char('j') => state.switch_move_down(),
        // Enter / l focuses the selected session: attach when it is live, else
        // just enter 在席.
        Key::Enter | Key::Char('l') => {
            let row = state.list().selected_index();
            let dir = selected_dir(state, workspace_root);
            state.enter_focus(row);
            if preview(&dir).is_some() {
                open_pane(
                    term,
                    reader,
                    state,
                    painter,
                    workspace_root,
                    open_terminal,
                    preview,
                    false,
                );
            }
        }
        // `c` begins inline session creation.
        Key::Char('c') => state.switch_begin_create(existing_branches()),
        // Esc / h backs out to where Switch was opened from.
        Key::Escape | Key::Char('h') => leave_switch(
            term,
            reader,
            state,
            painter,
            workspace_root,
            open_terminal,
            preview,
        ),
        // Ctrl-O zooms one level further out, to 統括.
        Key::Char(CTRL_O) => state.enter_overview(),
        _ => {}
    }
    Flow::Continue
}

/// Back out of 切替 on `Esc` / `h`: return to the mode it was opened from. From
/// 統括 / 在席 this just restores the mode; from 没入 it re-attaches the focused
/// session's pane when that session is still live, mirroring how `Enter` only
/// attaches a live session (so backing out onto an idle row lands in 在席 rather
/// than spawning a surprise shell).
#[allow(clippy::too_many_arguments)]
fn leave_switch(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
) {
    match state.switch_return() {
        ReturnMode::Overview => state.enter_overview(),
        ReturnMode::Focus => {
            let row = state.list().selected_index();
            state.enter_focus(row);
        }
        ReturnMode::Attached => {
            let row = state.list().selected_index();
            let dir = selected_dir(state, workspace_root);
            state.enter_focus(row);
            // Re-attach only when the focused session is live (it always is when
            // the cursor never left the just-detached session); an idle row stays
            // in 在席.
            if preview(&dir).is_some() {
                open_pane(
                    term,
                    reader,
                    state,
                    painter,
                    workspace_root,
                    open_terminal,
                    preview,
                    false,
                );
            }
        }
    }
}

/// Handle one key in 在席 (Focus): drive the right-pane action surface (a menu
/// of the session's commands or a session-scoped prompt), launching `terminal` /
/// `agent` into 没入, or back out to 統括 / 切替.
#[allow(clippy::too_many_arguments)]
pub(super) fn focus_key(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    key: Key,
    remove_session: &mut dyn FnMut(&str, bool) -> SessionOutcome,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
) -> Flow {
    // `Esc` returns to 統括; `Ctrl-O` opens 切替 (return here on cancel). These
    // bind the same in both action surfaces.
    match key {
        Key::Escape => {
            state.leave_focus();
            return Flow::Continue;
        }
        Key::Char(CTRL_O) => {
            state.enter_switch(ReturnMode::Focus);
            return Flow::Continue;
        }
        _ => {}
    }

    match state.session_action_ui() {
        SessionActionUi::Menu => focus_menu_key(
            term,
            reader,
            state,
            painter,
            workspace_root,
            key,
            remove_session,
            open_terminal,
            preview,
        ),
        SessionActionUi::Prompt => focus_prompt_key(
            term,
            reader,
            state,
            painter,
            workspace_root,
            key,
            remove_session,
            open_terminal,
            preview,
        ),
    }
    Flow::Continue
}

/// Close the focused session forcefully — the `close` command's effect. Removes
/// the session like `session remove <name> --force` (discarding any uncommitted
/// changes) via the `remove_session` callback; on success the session is gone,
/// so leave 在席 for 統括 (Overview). A failed removal (e.g. the root row, or a
/// git error) only logs and stays in 在席.
fn close_focused_session(
    state: &mut HomeState,
    remove_session: &mut dyn FnMut(&str, bool) -> SessionOutcome,
) {
    let name = state.focused_session_name();
    let outcome = remove_session(&name, true);
    // The callback returns a refreshed list only when it actually removed the
    // session; on an error it leaves the list untouched.
    let removed = outcome.sessions.is_some();
    state.apply_session_outcome(outcome);
    if removed {
        state.leave_focus();
    }
}

/// 在席 menu surface: `↑`/`↓` move the cursor, `Enter` runs the highlighted
/// command, and `t` / `a` are shortcuts for `terminal` / `agent`. `ai` runs its
/// coming-soon line.
#[allow(clippy::too_many_arguments)]
fn focus_menu_key(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    key: Key,
    remove_session: &mut dyn FnMut(&str, bool) -> SessionOutcome,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
) {
    match key {
        Key::ArrowUp | Key::Char('k') => state.focus_menu_move_up(),
        Key::ArrowDown | Key::Char('j') => state.focus_menu_move_down(),
        Key::Enter => {
            let name = state.focus_selected_command().name;
            run_focus_command(
                term,
                reader,
                state,
                painter,
                workspace_root,
                name,
                remove_session,
                open_terminal,
                preview,
            );
        }
        Key::Char('t') => run_focus_command(
            term,
            reader,
            state,
            painter,
            workspace_root,
            "terminal",
            remove_session,
            open_terminal,
            preview,
        ),
        Key::Char('a') => run_focus_command(
            term,
            reader,
            state,
            painter,
            workspace_root,
            "agent",
            remove_session,
            open_terminal,
            preview,
        ),
        _ => {}
    }
}

/// 在席 prompt surface: edit / complete the session-scoped command line and run
/// it on `Enter`, attaching the pane on `terminal` / `agent`.
#[allow(clippy::too_many_arguments)]
fn focus_prompt_key(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    key: Key,
    remove_session: &mut dyn FnMut(&str, bool) -> SessionOutcome,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
) {
    match key {
        Key::Enter => {
            // `terminal` / `agent` attach the pane; `close` removes the session
            // and leaves 在席; `ai` (coming soon) and anything else only log,
            // staying in Focus.
            let effect = state.focus_prompt_submit().effect;
            match effect {
                Effect::OpenTerminal | Effect::OpenAgent => {
                    let agent = effect == Effect::OpenAgent;
                    open_pane(
                        term,
                        reader,
                        state,
                        painter,
                        workspace_root,
                        open_terminal,
                        preview,
                        agent,
                    );
                }
                Effect::CloseSession => close_focused_session(state, remove_session),
                _ => {}
            }
        }
        Key::Tab => {
            let _ = state.focus_prompt_complete();
        }
        Key::Backspace => state.focus_prompt_backspace(),
        Key::Char(c) => state.focus_prompt_push_char(c),
        _ => {}
    }
}

/// Run a named session command (`terminal` / `agent` / `ai`) from the 在席 menu:
/// the two launch commands attach the pane (没入); `ai` logs its coming-soon
/// line.
#[allow(clippy::too_many_arguments)]
fn run_focus_command(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    name: &str,
    remove_session: &mut dyn FnMut(&str, bool) -> SessionOutcome,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
) {
    match name {
        "terminal" => open_pane(
            term,
            reader,
            state,
            painter,
            workspace_root,
            open_terminal,
            preview,
            false,
        ),
        "agent" => open_pane(
            term,
            reader,
            state,
            painter,
            workspace_root,
            open_terminal,
            preview,
            true,
        ),
        // `close` removes the focused session forcefully and leaves 在席.
        "close" => close_focused_session(state, remove_session),
        // `ai` (and any future coming-soon command) just logs its line.
        _ => state.log_output(format!("\"{name}\" is coming soon 🐰")),
    }
}

/// Open the embedded terminal pane (没入) for the focused session and run it
/// until the user leaves it, then act on the [`PaneExit`] and report whether the
/// application should quit.
///
/// `agent` governs the shell opened here (`agent` launches the AI agent CLI
/// inside it; `terminal` opens a plain shell). The pane is driven by the impure
/// `open_terminal` callback, which returns:
///
/// - [`PaneExit::Closed`] — the shell exited: return to 在席 (Focus).
/// - [`PaneExit::ToSwitch`] — `Ctrl-O`: zoom out to 切替 (Switch), remembering to
///   re-attach (`ReturnMode::Attached`) if the user backs out.
#[allow(clippy::too_many_arguments)]
fn open_pane(
    term: &Term,
    // `reader` / `preview` are threaded through so the helper's signature matches
    // the others; the pane owns its own input, and re-attaching is decided by the
    // caller, so neither is read here.
    _reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    _preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
    agent: bool,
) {
    let (label, fail) = if agent {
        ("Agent", "agent")
    } else {
        ("Terminal", "terminal")
    };
    let dir = selected_dir(state, workspace_root);
    state.show_attached();
    let outcome = open_terminal(state, &dir, agent);
    // The pane toggled `crossterm`'s raw mode around itself; re-assert the
    // wheel-capture modes so the wheel can't scroll the host terminal once we are
    // back on the workspace screen.
    let _ = screen::write_input_modes(term);
    // The embedded terminal drew over the whole screen, so the remembered frame
    // is stale: force a full repaint on the next pass.
    painter.reset();
    match outcome {
        Ok(PaneExit::ToSwitch) => {
            // `Ctrl-O` zooms out: pick a session in the left pane, re-attaching
            // this one if the user backs out.
            state.enter_switch(ReturnMode::Attached);
        }
        Ok(PaneExit::Closed) => {
            // The shell exited: drop back to 在席 on the same session.
            state.leave_attached();
            state.log_output(format!("{label} in {} closed.", dir.display()));
        }
        Err(e) => {
            state.leave_attached();
            state.log_error(format!("{fail} failed: {e}"));
        }
    }
}
