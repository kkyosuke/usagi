use std::path::{Path, PathBuf};

use anyhow::Result;
use console::Key;
use console::Term;

use crate::presentation::tui::screen::{self, FramePainter, KeyReader};

use crate::domain::settings::SessionActionUi;

use super::command::Effect;
use super::state::{HomeState, Mode, PaneExit, ReturnMode, SessionOutcome};
use super::terminal_pool::MonitorHandle;
use super::terminal_view::TerminalView;
use super::ui;

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
///   `Esc` is inert here (it does not back out to the project list).
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
    persist: &mut dyn FnMut(&str),
    create_session: &mut dyn FnMut(&str) -> SessionOutcome,
    remove_session: &mut dyn FnMut(&str, bool) -> SessionOutcome,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    open_config: &mut dyn FnMut(&Term) -> Result<bool>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
) -> Result<Outcome> {
    let mut painter = FramePainter::new();
    loop {
        // Mark any background sessions waiting for input, and which have a live
        // (running) agent, before painting.
        state.set_waiting(monitor.waiting());
        state.set_live(monitor.live());
        // The right pane is blank in 統括 / 切替 and the action surface in 在席,
        // so the terminal preview is only ever drawn while 没入 (which `open_pane`
        // drives directly). Drop any stale snapshot every frame here.
        state.clear_terminal_view();
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

/// Handle one key in 統括 (Overview): edit / complete / recall the workspace
/// command line and run it on `Enter`, dispatching the resulting [`Effect`].
#[allow(clippy::too_many_arguments)]
fn overview_key(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    key: Key,
    persist: &mut dyn FnMut(&str),
    create_session: &mut dyn FnMut(&str) -> SessionOutcome,
    remove_session: &mut dyn FnMut(&str, bool) -> SessionOutcome,
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
                    state.switch_begin_create();
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
                Effect::None | Effect::Clear => {}
            }
        }
        Key::Tab => state.complete(),
        Key::Backspace => state.backspace(),
        Key::ArrowUp => state.recall_prev(),
        Key::ArrowDown => state.recall_next(),
        // `Esc` is inert at the top level: the home screen is not left by backing
        // out (that would drop into the project list); the only way out is
        // `Ctrl-C`, handled centrally in the event loop. `Ctrl-O` is likewise
        // inert here (Overview is already the outermost engagement level).
        Key::Escape | Key::Char(CTRL_O) => {}
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
    use super::state::{worktree_name, ROOT_NAME};
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
fn switch_key(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    key: Key,
    create_session: &mut dyn FnMut(&str) -> SessionOutcome,
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
        Key::Char('c') => state.switch_begin_create(),
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
fn focus_key(
    term: &Term,
    reader: &mut dyn KeyReader,
    state: &mut HomeState,
    painter: &mut FramePainter,
    workspace_root: &Path,
    key: Key,
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
            open_terminal,
            preview,
        ),
    }
    Flow::Continue
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
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
) {
    match key {
        Key::Enter => {
            // `terminal` / `agent` attach the pane; `ai` (coming soon) and
            // anything else only log, staying in Focus.
            let effect = state.focus_prompt_submit().effect;
            if matches!(effect, Effect::OpenTerminal | Effect::OpenAgent) {
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

#[cfg(test)]
mod tests {
    use super::super::state::LogLine;
    use super::*;
    use crate::domain::settings::SessionActionUi;
    use crate::domain::workspace_state::{BranchStatus, SessionRecord, WorktreeState};
    use chrono::Utc;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io;
    use std::path::PathBuf;

    fn noop_create(_: &str) -> SessionOutcome {
        SessionOutcome {
            line: LogLine::output("created"),
            sessions: None,
            select: None,
        }
    }

    fn noop_remove(_: &str, _: bool) -> SessionOutcome {
        SessionOutcome {
            line: LogLine::output("removed"),
            sessions: None,
            select: None,
        }
    }

    /// A key source that replays a scripted sequence of results.
    struct ScriptedReader {
        keys: VecDeque<io::Result<Key>>,
    }

    impl ScriptedReader {
        fn new(keys: Vec<io::Result<Key>>) -> Self {
            Self { keys: keys.into() }
        }
    }

    impl KeyReader for ScriptedReader {
        fn read_key(&mut self) -> io::Result<Key> {
            // Default to Ctrl-C so a test can never spin forever: Esc no longer
            // leaves Overview, so Ctrl-C (which quits when no session is live, as
            // in these tests) is the terminator the loop falls back to.
            self.keys.pop_front().unwrap_or(Ok(Key::CtrlC))
        }
    }

    fn worktree(branch: Option<&str>, path: &str) -> WorktreeState {
        WorktreeState {
            branch: branch.map(|b| b.to_string()),
            path: PathBuf::from(path),
            head: "abc1234".to_string(),
            primary: false,
            upstream: None,
            status: BranchStatus::Local,
            updated_at: Utc::now(),
        }
    }

    fn sample_state() -> HomeState {
        HomeState::new(
            "usagi",
            vec![
                worktree(Some("main"), "/r/main"),
                worktree(Some("feat"), "/r/feat"),
            ],
            None,
        )
    }

    fn prompt_state() -> HomeState {
        let mut state = sample_state();
        state.set_session_action_ui(SessionActionUi::Prompt);
        state
    }

    /// A `open_terminal` callback reporting the shell closed (one pane iteration).
    fn noop_open(_: &mut HomeState, _: &Path, _: bool) -> Result<PaneExit> {
        Ok(PaneExit::Closed)
    }

    fn noop_config(_: &Term) -> Result<bool> {
        Ok(false)
    }

    fn noop_preview(_: &Path) -> Option<TerminalView> {
        None
    }

    fn live_preview(_: &Path) -> Option<TerminalView> {
        Some(TerminalView::from_rows(vec!["live".to_string()], None))
    }

    fn noop_persist(_: &str) {}

    /// Run the loop with all-default callbacks (idle preview, no-op pane).
    fn run(keys: Vec<io::Result<Key>>, state: HomeState) -> Result<Outcome> {
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        run_full(
            keys,
            state,
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config,
        )
    }

    /// Run the loop with all-default callbacks but every session live.
    fn run_live(keys: Vec<io::Result<Key>>, state: HomeState) -> Result<Outcome> {
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
        run_full(
            keys,
            state,
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_full(
        keys: Vec<io::Result<Key>>,
        state: HomeState,
        open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
        create_session: &mut dyn FnMut(&str) -> SessionOutcome,
        preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
        open_config: &mut dyn FnMut(&Term) -> Result<bool>,
    ) -> Result<Outcome> {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::detached();
        let mut persist: fn(&str) = noop_persist;
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        event_loop(
            &term,
            &mut reader,
            state,
            Path::new("/ws"),
            &monitor,
            &mut persist,
            create_session,
            &mut remove_session,
            open_terminal,
            open_config,
            preview,
        )
    }

    /// Run the loop with a monitor reporting a live session, so `Ctrl-C` raises
    /// the quit-confirmation modal instead of quitting outright. `persist`
    /// records the commands run, so a test can prove the screen kept running
    /// after the modal was cancelled.
    fn run_with_live_monitor(
        keys: Vec<io::Result<Key>>,
        state: HomeState,
        persist: &mut dyn FnMut(&str),
    ) -> Result<Outcome> {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::with_live(vec![PathBuf::from("/r/main")]);
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut config: fn(&Term) -> Result<bool> = noop_config;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        event_loop(
            &term,
            &mut reader,
            state,
            Path::new("/ws"),
            &monitor,
            persist,
            &mut create,
            &mut remove_session,
            &mut open,
            &mut config,
            &mut preview,
        )
    }

    fn typed(s: &str) -> Vec<io::Result<Key>> {
        s.chars().map(|c| Ok(Key::Char(c))).collect()
    }

    fn state_with_sessions(names: &[&str]) -> HomeState {
        let mut state = sample_state();
        let sessions = names
            .iter()
            .map(|n| SessionRecord {
                name: n.to_string(),
                root: PathBuf::from(format!("/ws/.usagi/worktree/{n}")),
                worktrees: vec![worktree(Some(n), &format!("/ws/{n}"))],
                created_at: Utc::now(),
            })
            .collect();
        state.restore_sessions(sessions);
        state
    }

    // --- 統括 (Overview) ---------------------------------------------------

    #[test]
    fn escape_in_overview_is_inert_and_does_not_leave() {
        // Esc no longer backs out to the project list: it is a no-op in Overview,
        // so the loop runs on and only the fallback Ctrl-C (no live session) quits.
        // A Back-returning Esc would instead resolve to `Outcome::Back` here.
        assert!(matches!(
            run(vec![Ok(Key::Escape)], sample_state()).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn ctrl_c_in_overview_returns_quit() {
        assert!(matches!(
            run(vec![Ok(Key::CtrlC)], sample_state()).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn ctrl_o_in_overview_is_inert() {
        // Ctrl-O and Esc are both inert at the top level; the fallback Ctrl-C quits.
        let keys = vec![Ok(Key::Char(CTRL_O)), Ok(Key::Escape)];
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn overview_edits_completes_and_recalls_then_runs() {
        let mut keys = typed("ma");
        keys.push(Ok(Key::Backspace));
        keys.push(Ok(Key::Tab)); // "m" -> "man"
        keys.push(Ok(Key::Enter)); // run
        keys.push(Ok(Key::ArrowUp)); // recall
        keys.push(Ok(Key::ArrowDown)); // back to empty
        keys.push(Ok(Key::Home)); // ignored
        keys.push(Ok(Key::Escape)); // back
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn quit_command_exits_the_app() {
        let mut keys = typed("quit");
        keys.push(Ok(Key::Enter));
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn submitted_commands_are_handed_to_persist() {
        let mut keys = typed("man");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape));
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::detached();
        let mut recorded = Vec::new();
        let mut persist = |c: &str| recorded.push(c.to_string());
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut config: fn(&Term) -> Result<bool> = noop_config;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_state(),
            Path::new("/ws"),
            &monitor,
            &mut persist,
            &mut create,
            &mut remove,
            &mut open,
            &mut config,
            &mut preview,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Quit));
        assert_eq!(recorded, vec!["man"]);
    }

    #[test]
    fn overview_terminal_and_agent_attach_the_active_session() {
        // Typing `terminal` / `agent` in Overview still dispatches: it focuses the
        // active row (the root) and attaches the pane.
        let opened = RefCell::new(Vec::new());
        let mut open = |_h: &mut HomeState, _d: &Path, a: bool| {
            opened.borrow_mut().push(a);
            Ok(PaneExit::Closed)
        };
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        let mut keys = typed("terminal");
        keys.push(Ok(Key::Enter)); // attach (root, plain shell) -> Closed -> Focus
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.extend(typed("agent"));
        keys.push(Ok(Key::Enter)); // wait — we are back in Overview after Esc
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert_eq!(*opened.borrow(), vec![false, true]);
    }

    #[test]
    fn session_list_logs_the_sessions() {
        let mut keys = typed("session list");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape));
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn session_create_with_a_name_creates_immediately() {
        let mut keys = typed("session create newx");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape));
        let created = RefCell::new(Vec::new());
        let mut create = |name: &str| {
            created.borrow_mut().push(name.to_string());
            noop_create(name)
        };
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert_eq!(*created.borrow(), vec!["newx"]);
    }

    #[test]
    fn bare_session_create_moves_to_switch_and_opens_the_inline_input() {
        // `session create` (no name) enters 切替 and begins inline creation; the
        // name is typed and confirmed there, creating the session.
        let mut keys = typed("session create");
        keys.push(Ok(Key::Enter)); // -> Switch + begin create
        keys.extend(typed("wip"));
        keys.push(Ok(Key::Enter)); // confirm create -> Focus
        keys.push(Ok(Key::Escape)); // Focus Esc -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
        let created = RefCell::new(Vec::new());
        let mut create = |name: &str| {
            created.borrow_mut().push(name.to_string());
            noop_create(name)
        };
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert_eq!(*created.borrow(), vec!["wip"]);
    }

    #[test]
    fn session_remove_with_a_name_and_force_routes_to_remove() {
        let mut keys = typed("session remove old --force");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape));
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::detached();
        let mut persist: fn(&str) = noop_persist;
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut removed = Vec::new();
        let mut remove = |name: &str, force: bool| {
            removed.push((name.to_string(), force));
            noop_remove(name, force)
        };
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut config: fn(&Term) -> Result<bool> = noop_config;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_state(),
            Path::new("/ws"),
            &monitor,
            &mut persist,
            &mut create,
            &mut remove,
            &mut open,
            &mut config,
            &mut preview,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Quit));
        assert_eq!(removed, vec![("old".to_string(), true)]);
    }

    // --- session-removal modal --------------------------------------------

    #[test]
    fn session_remove_without_a_name_opens_the_modal_and_bulk_removes() {
        let mut keys = typed("session remove");
        keys.push(Ok(Key::Enter)); // open the modal
        keys.push(Ok(Key::Char(' '))); // check "alpha"
        keys.push(Ok(Key::ArrowDown));
        keys.push(Ok(Key::Char('j'))); // cursor on "gamma"
        keys.push(Ok(Key::Char(' '))); // check "gamma"
        keys.push(Ok(Key::Char('k')));
        keys.push(Ok(Key::ArrowUp)); // cursor 0
        keys.push(Ok(Key::Home)); // ignored
        keys.push(Ok(Key::Enter)); // confirm
        keys.push(Ok(Key::Escape)); // Overview back
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::detached();
        let mut persist: fn(&str) = noop_persist;
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut removed = Vec::new();
        let mut remove = |name: &str, force: bool| {
            removed.push((name.to_string(), force));
            noop_remove(name, force)
        };
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut config: fn(&Term) -> Result<bool> = noop_config;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        let outcome = event_loop(
            &term,
            &mut reader,
            state_with_sessions(&["alpha", "beta", "gamma"]),
            Path::new("/ws"),
            &monitor,
            &mut persist,
            &mut create,
            &mut remove,
            &mut open,
            &mut config,
            &mut preview,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Quit));
        assert_eq!(
            removed,
            vec![("alpha".to_string(), false), ("gamma".to_string(), false)]
        );
    }

    #[test]
    fn removal_modal_cancels_via_escape_and_keeps_open_on_empty_enter() {
        let mut keys = typed("session remove");
        keys.push(Ok(Key::Enter)); // open
        keys.push(Ok(Key::Enter)); // nothing checked -> stays open
        keys.push(Ok(Key::Char(' '))); // check alpha
        keys.push(Ok(Key::Escape)); // cancel the modal
        keys.push(Ok(Key::Escape)); // Overview back
        assert!(matches!(
            run(keys, state_with_sessions(&["alpha"])).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn ctrl_c_in_the_removal_modal_quits() {
        let mut keys = typed("session remove");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::CtrlC));
        assert!(matches!(
            run(keys, state_with_sessions(&["alpha"])).unwrap(),
            Outcome::Quit
        ));
    }

    // --- quit-confirmation modal (Ctrl-C with a live session) --------------

    #[test]
    fn ctrl_c_quits_outright_when_no_session_is_live() {
        // The default `run` harness has no live session, so Ctrl-C closes the app
        // without asking — the gate only triggers when something is running.
        assert!(matches!(
            run(vec![Ok(Key::CtrlC)], sample_state()).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn ctrl_c_with_a_live_session_quits_only_after_confirming() {
        // 'y' confirms the close.
        let mut persist: fn(&str) = noop_persist;
        assert!(matches!(
            run_with_live_monitor(
                vec![Ok(Key::CtrlC), Ok(Key::Char('y'))],
                sample_state(),
                &mut persist,
            )
            .unwrap(),
            Outcome::Quit
        ));

        // A second Ctrl-C inside the modal confirms too.
        assert!(matches!(
            run_with_live_monitor(
                vec![Ok(Key::CtrlC), Ok(Key::CtrlC)],
                sample_state(),
                &mut persist,
            )
            .unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn confirm_modal_cancel_keeps_the_screen_running() {
        // Ctrl-C raises the modal (a session is live); an ignored key is a no-op
        // in it; 'n' cancels back to Overview, where a command still runs (proving
        // the first Ctrl-C did not quit). Esc also cancels; Enter finally confirms.
        let mut keys = vec![
            Ok(Key::CtrlC),     // raise the modal
            Ok(Key::Home),      // ignored inside the modal
            Ok(Key::Char('n')), // cancel -> Overview
        ];
        keys.extend(typed("man"));
        keys.push(Ok(Key::Enter)); // runs `man` -> persisted
        keys.push(Ok(Key::CtrlC)); // raise again
        keys.push(Ok(Key::Escape)); // cancel via Esc
        keys.push(Ok(Key::CtrlC)); // raise again
        keys.push(Ok(Key::Enter)); // confirm via Enter -> quit

        let mut recorded = Vec::new();
        let mut persist = |c: &str| recorded.push(c.to_string());
        let outcome = run_with_live_monitor(keys, sample_state(), &mut persist).unwrap();
        assert!(matches!(outcome, Outcome::Quit));
        // The command ran between the cancelled closes, so the screen kept going.
        assert_eq!(recorded, vec!["man"]);
    }

    // --- config hand-off ---------------------------------------------------

    fn config_keys() -> Vec<io::Result<Key>> {
        let mut keys = typed("config");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape));
        keys
    }

    #[test]
    fn config_opens_the_settings_screen_and_can_quit() {
        // Returns false -> resume, then back.
        let opened = RefCell::new(false);
        let mut config = |_: &Term| {
            *opened.borrow_mut() = true;
            Ok(false)
        };
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        assert!(matches!(
            run_full(
                config_keys(),
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert!(opened.into_inner());

        // Returns true -> quit.
        let mut config_quit = |_: &Term| Ok(true);
        assert!(matches!(
            run_full(
                config_keys(),
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut config_quit
            )
            .unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn config_failure_is_propagated() {
        let mut keys = typed("config");
        keys.push(Ok(Key::Enter));
        let mut config = |_: &Term| Err(anyhow::anyhow!("settings blew up"));
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        let err = run_full(
            keys,
            sample_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut config,
        )
        .unwrap_err();
        assert!(err.to_string().contains("settings blew up"));
    }

    // --- session switch <name> (Overview -> Focus / Attached) --------------

    #[test]
    fn session_switch_unknown_name_logs_an_error_and_stays_in_overview() {
        let mut keys = typed("session switch nope");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape)); // still in Overview; Esc inert, fallback Ctrl-C quits
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn session_switch_known_idle_name_enters_focus() {
        // "feat" resolves but is idle (no live preview), so it just enters Focus.
        let mut keys = typed("session switch feat");
        keys.push(Ok(Key::Enter)); // -> Focus
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn session_switch_known_live_name_attaches_then_returns_to_focus() {
        // "root" resolves and is live, so it attaches; noop_open closes the pane,
        // returning to Focus, then Esc -> Overview (fallback Ctrl-C quits).
        let opened = RefCell::new(0);
        let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| {
            *opened.borrow_mut() += 1;
            Ok(PaneExit::Closed)
        };
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
        let mut keys = typed("session switch root");
        keys.push(Ok(Key::Enter)); // -> Focus -> attach -> Closed -> Focus
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert_eq!(*opened.borrow(), 1);
    }

    // --- 切替 (Switch) -----------------------------------------------------

    #[test]
    fn switch_navigates_and_backs_out_to_overview() {
        // `session switch` enters Switch; arrows / jk move; Esc returns to Overview
        // (the origin); Esc is then inert, so the fallback Ctrl-C quits.
        let mut keys = typed("session switch");
        keys.push(Ok(Key::Enter)); // -> Switch (origin Overview)
        keys.push(Ok(Key::ArrowDown));
        keys.push(Ok(Key::ArrowUp));
        keys.push(Ok(Key::Char('j')));
        keys.push(Ok(Key::Char('k')));
        keys.push(Ok(Key::Home)); // ignored
        keys.push(Ok(Key::Char('h'))); // back to Overview
        keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn switch_ctrl_o_zooms_out_to_overview() {
        let mut keys = typed("session switch");
        keys.push(Ok(Key::Enter)); // -> Switch
        keys.push(Ok(Key::Char(CTRL_O))); // -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn switch_ctrl_c_quits() {
        let mut keys = typed("session switch");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::CtrlC));
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn switch_enter_on_an_idle_session_just_focuses_it() {
        let mut keys = typed("session switch");
        keys.push(Ok(Key::Enter)); // -> Switch
        keys.push(Ok(Key::ArrowDown)); // cursor on "main"
        keys.push(Ok(Key::Enter)); // focus (idle -> no attach)
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn switch_enter_on_a_live_session_attaches_via_l() {
        let opened = RefCell::new(0);
        let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| {
            *opened.borrow_mut() += 1;
            Ok(PaneExit::Closed)
        };
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
        let mut keys = typed("session switch");
        keys.push(Ok(Key::Enter)); // -> Switch
        keys.push(Ok(Key::Char('l'))); // focus + attach (live)
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert_eq!(*opened.borrow(), 1);
    }

    #[test]
    fn switch_inline_create_makes_and_focuses_the_new_session() {
        let mut keys = typed("session switch");
        keys.push(Ok(Key::Enter)); // -> Switch
        keys.push(Ok(Key::Char('c'))); // begin create
        keys.extend(typed("wip"));
        keys.push(Ok(Key::Backspace)); // "wi"
        keys.push(Ok(Key::Char('p'))); // "wip"
        keys.push(Ok(Key::Home)); // ignored inside create
        keys.push(Ok(Key::Enter)); // confirm -> Focus
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        let created = RefCell::new(Vec::new());
        let mut create = |name: &str| {
            created.borrow_mut().push(name.to_string());
            noop_create(name)
        };
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert_eq!(*created.borrow(), vec!["wip"]);
    }

    #[test]
    fn switch_inline_create_can_be_cancelled_and_ctrl_c_quits() {
        // Cancel path: Esc closes the input, staying in Switch; then Ctrl-O -> Overview (fallback Ctrl-C quits).
        let mut keys = typed("session switch");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Char('c'))); // begin create
        keys.push(Ok(Key::Char('x')));
        keys.push(Ok(Key::Escape)); // cancel create (stay in Switch)
        keys.push(Ok(Key::Char(CTRL_O))); // Switch -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));

        // Ctrl-C inside the create input quits.
        let mut keys = typed("session switch");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Char('c')));
        keys.push(Ok(Key::CtrlC));
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn switch_create_invalid_name_keeps_the_input_open() {
        // An empty confirm keeps the input open; then Ctrl-C ends the run.
        let mut keys = typed("session switch");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Char('c')));
        keys.push(Ok(Key::Enter)); // empty -> error, stays open
        keys.push(Ok(Key::CtrlC));
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    // --- 在席 (Focus) menu surface -----------------------------------------

    #[test]
    fn focus_menu_moves_and_runs_terminal_via_enter() {
        // Switch -> focus "main" (idle, so just Focus). The menu highlights
        // "terminal" by default; move down to "agent" and back up to "terminal",
        // then Enter runs it (attaches).
        let opened = RefCell::new(Vec::new());
        let mut open = |_h: &mut HomeState, d: &Path, a: bool| {
            opened.borrow_mut().push((d.to_path_buf(), a));
            Ok(PaneExit::Closed)
        };
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        let mut keys = typed("session switch");
        keys.push(Ok(Key::Enter)); // Switch
        keys.push(Ok(Key::ArrowDown)); // cursor "main" (/r/main)
        keys.push(Ok(Key::Enter)); // focus main (idle)
        keys.push(Ok(Key::Char('j'))); // terminal -> agent
        keys.push(Ok(Key::ArrowUp)); // agent -> terminal
        keys.push(Ok(Key::Enter)); // run terminal (attach) -> Closed -> Focus
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert_eq!(*opened.borrow(), vec![(PathBuf::from("/r/main"), false)]);
    }

    #[test]
    fn focus_menu_shortcut_keys_launch_terminal_and_agent() {
        let opened = RefCell::new(Vec::new());
        let mut open = |_h: &mut HomeState, _d: &Path, a: bool| {
            opened.borrow_mut().push(a);
            Ok(PaneExit::Closed)
        };
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        let mut keys = typed("session switch feat");
        keys.push(Ok(Key::Enter)); // Focus feat
        keys.push(Ok(Key::Char('t'))); // terminal
        keys.push(Ok(Key::Char('k'))); // a menu move (no-op effect here)
        keys.push(Ok(Key::Char('a'))); // agent
        keys.push(Ok(Key::Escape)); // -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert_eq!(*opened.borrow(), vec![false, true]);
    }

    #[test]
    fn focus_menu_can_run_the_coming_soon_ai_command() {
        // The menu lists terminal (0, default), agent (1), ai (2). ArrowUp from
        // the top wraps to "ai"; Enter on it just logs (no attach).
        let mut keys = typed("session switch feat");
        keys.push(Ok(Key::Enter)); // Focus
        keys.push(Ok(Key::Home)); // ignored in the menu
        keys.push(Ok(Key::ArrowDown)); // terminal -> agent
        keys.push(Ok(Key::ArrowUp)); // back to terminal
        keys.push(Ok(Key::ArrowUp)); // wrap to "ai"
        keys.push(Ok(Key::Enter)); // run ai (coming soon)
        keys.push(Ok(Key::Escape)); // -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn focus_ctrl_o_opens_switch_then_esc_re_focuses() {
        // Focus -> Ctrl-O -> Switch(return=Focus); Esc/h re-enters Focus; Esc ->
        // Overview; Esc inert, fallback Ctrl-C quits.
        let mut keys = typed("session switch feat");
        keys.push(Ok(Key::Enter)); // Focus feat
        keys.push(Ok(Key::Char(CTRL_O))); // -> Switch(return Focus)
        keys.push(Ok(Key::Char('h'))); // back -> Focus
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn focus_ctrl_c_quits() {
        let mut keys = typed("session switch feat");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::CtrlC));
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    // --- 在席 (Focus) prompt surface ---------------------------------------

    #[test]
    fn focus_prompt_edits_completes_and_runs_terminal() {
        let opened = RefCell::new(0);
        let mut open = |_h: &mut HomeState, _d: &Path, a: bool| {
            assert!(!a);
            *opened.borrow_mut() += 1;
            Ok(PaneExit::Closed)
        };
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        let mut keys = typed("session switch feat");
        keys.push(Ok(Key::Enter)); // Focus feat (prompt UI)
        keys.extend(typed("ter"));
        keys.push(Ok(Key::Backspace)); // "te"
        keys.push(Ok(Key::Tab)); // -> "terminal"
        keys.push(Ok(Key::Enter)); // run terminal (attach)
        keys.push(Ok(Key::Escape)); // -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(
            run_full(
                keys,
                prompt_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert_eq!(*opened.borrow(), 1);
    }

    #[test]
    fn focus_prompt_runs_agent_and_coming_soon_and_ignores_empty() {
        let opened = RefCell::new(Vec::new());
        let mut open = |_h: &mut HomeState, _d: &Path, a: bool| {
            opened.borrow_mut().push(a);
            Ok(PaneExit::Closed)
        };
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
        let mut keys = typed("session switch feat");
        keys.push(Ok(Key::Enter)); // Focus (prompt)
        keys.push(Ok(Key::Home)); // ignored in the prompt
        keys.push(Ok(Key::Enter)); // empty prompt -> no-op
        keys.extend(typed("ai go"));
        keys.push(Ok(Key::Enter)); // coming soon -> log, no attach
        keys.extend(typed("agent"));
        keys.push(Ok(Key::Enter)); // attach agent
        keys.push(Ok(Key::Escape)); // -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(
            run_full(
                keys,
                prompt_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert_eq!(*opened.borrow(), vec![true]);
    }

    // --- 没入 (Attached) exits ---------------------------------------------

    #[test]
    fn ctrl_o_in_the_pane_zooms_out_to_switch() {
        // Attaching to a live session; the pane returns ToSwitch (Ctrl-O), so the
        // loop enters Switch with return=Attached. Then Ctrl-O -> Overview (fallback Ctrl-C quits).
        let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| Ok(PaneExit::ToSwitch);
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
        let mut keys = typed("session switch root");
        keys.push(Ok(Key::Enter)); // Focus root -> attach -> ToSwitch -> Switch
        keys.push(Ok(Key::Char(CTRL_O))); // Switch -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn pane_to_switch_then_esc_re_attaches() {
        // ToSwitch -> Switch(return=Attached). In Switch, Esc re-attaches. The pane
        // returns ToSwitch the first time and Closed the second so the run ends.
        let calls = RefCell::new(0);
        let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| {
            let mut n = calls.borrow_mut();
            *n += 1;
            if *n == 1 {
                Ok(PaneExit::ToSwitch)
            } else {
                Ok(PaneExit::Closed)
            }
        };
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
        let mut keys = typed("session switch root");
        keys.push(Ok(Key::Enter)); // attach -> ToSwitch -> Switch(return Attached)
        keys.push(Ok(Key::Escape)); // Switch Esc -> re-attach -> Closed -> Focus
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        assert_eq!(*calls.borrow(), 2);
    }

    #[test]
    fn pane_to_switch_then_esc_onto_an_idle_session_lands_in_focus() {
        // ToSwitch -> Switch(return=Attached). Moving the cursor onto an idle
        // session and pressing Esc lands in 在席 *without* spawning a second pane
        // — only a live session re-attaches, mirroring how Enter behaves.
        let calls = RefCell::new(0);
        let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| {
            *calls.borrow_mut() += 1;
            Ok(PaneExit::ToSwitch)
        };
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        // Only the root (/ws) is live; the worktree rows are idle.
        let mut preview = |p: &Path| {
            if p == Path::new("/ws") {
                Some(TerminalView::from_rows(vec!["live".to_string()], None))
            } else {
                None
            }
        };
        let mut keys = typed("session switch root");
        keys.push(Ok(Key::Enter)); // attach root -> ToSwitch -> Switch(return Attached)
        keys.push(Ok(Key::ArrowDown)); // cursor -> an idle worktree row
        keys.push(Ok(Key::Escape)); // Esc -> idle row stays in Focus (no re-attach)
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
        // The pane opened only once (the initial attach); the Esc did not re-attach.
        assert_eq!(*calls.borrow(), 1);
    }

    #[test]
    fn pane_failure_is_reported_and_returns_to_focus() {
        let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| Err(anyhow::anyhow!("no shell"));
        let mut create: fn(&str) -> SessionOutcome = noop_create;
        let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
        let mut keys = typed("session switch root");
        keys.push(Ok(Key::Enter)); // attach -> Err -> Focus (logged)
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
        assert!(matches!(
            run_full(
                keys,
                sample_state(),
                &mut open,
                &mut create,
                &mut preview,
                &mut noop_config
            )
            .unwrap(),
            Outcome::Quit
        ));
    }

    // --- read errors -------------------------------------------------------

    #[test]
    fn interrupted_read_returns_quit() {
        let keys = vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))];
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn unexpected_read_error_is_propagated() {
        let err = run(vec![Err(io::Error::other("boom"))], sample_state()).unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }

    #[test]
    fn page_keys_are_inert_in_overview() {
        let keys = vec![Ok(Key::PageUp), Ok(Key::PageDown), Ok(Key::Escape)];
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn default_callbacks_run_through_the_harness() {
        // Drive the shared no-op `open_terminal` (via the live harness, which
        // attaches) and `open_config` (via `config`) so both default callbacks
        // execute end to end.
        let mut keys = typed("session switch root");
        keys.push(Ok(Key::Enter)); // live -> attach via noop_open -> Closed -> Focus
        keys.push(Ok(Key::Escape)); // Focus -> Overview
        keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
        assert!(matches!(
            run_live(keys, sample_state()).unwrap(),
            Outcome::Quit
        ));

        // `config` through the default `noop_config` (returns false -> resume).
        let mut keys = typed("config");
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape));
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }
}
