//! The home screen's per-mode key handlers. The event loop in [`super`]
//! dispatches each key to one of the three entry handlers — `overview_key` /
//! `switch_key` / `focus_key` — by mode; those delegate to the helpers here
//! (`activate_named`, `leave_switch`, the focus-surface handlers, …) and to
//! `open_pane`, which drives the embedded terminal (没入). All are pure aside
//! from the injected callbacks, which they reach through the shared [`Wiring`]
//! bundle.

use anyhow::Result;
use console::Key;
use console::Term;

use crate::presentation::tui::screen::{self, FramePainter};

use crate::domain::settings::SessionActionUi;

use super::super::command::Effect;
use super::super::state::{HomeState, PaneExit, ReturnMode, ROOT_NAME};
use super::super::terminal_tabs::TabNav;
use super::super::ui;
use super::{paint_now, selected_dir, Flow, Wiring, CTRL_E, CTRL_N, CTRL_O, CTRL_P, CTRL_S};

/// Handle one key in 統括 (Overview): edit / complete / recall the workspace
/// command line and run it on `Enter`, dispatching the resulting [`Effect`].
pub(super) fn overview_key(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    key: Key,
    wiring: &mut Wiring,
) -> Result<Flow> {
    match key {
        Key::Enter => {
            let submission = state.submit();
            if let Some(command) = submission.recorded.as_deref() {
                (wiring.persist)(command);
            }
            match submission.effect {
                Effect::Quit => return Ok(Flow::Quit),
                // `session switch` with no name moves keyboard focus to the left
                // pane to pick a session, returning here on cancel.
                Effect::EnterSwitch => state.enter_switch(ReturnMode::Overview),
                // `session switch <name>` focuses that session: if it resolves,
                // enter 在席 (attaching when it is live); otherwise log an error.
                Effect::Activate(name) => activate_named(term, state, painter, &name, wiring),
                // `session create <name>` dispatches the git work to a background
                // worker and returns at once; the new session appears in the list
                // when the task finishes (tracked in the top-right task panel).
                Effect::CreateSession(name) => (wiring.dispatch_create)(&name),
                // `session create` with no name moves to 切替 and opens the inline
                // name input there (creation lives in Switch now).
                Effect::OpenSessionModal => {
                    state.enter_switch(ReturnMode::Overview);
                    let branches = (wiring.existing_branches)();
                    state.switch_begin_create(branches);
                }
                // `session list`: the state holds the sessions but not their
                // wording — the ui layer formats them into the empty-state line
                // or the scrollable modal, which we then apply.
                Effect::ListSessions => match ui::content::session_list(state.sessions()) {
                    ui::content::SessionList::Empty(line) => state.log_output(line),
                    ui::content::SessionList::Modal(title, lines) => {
                        state.open_text_modal(title, lines)
                    }
                },
                // `session remove <name>` dispatches the removal to a background
                // worker; the session leaves the list when the task finishes.
                Effect::RemoveSession { name, force } => (wiring.dispatch_remove)(&name, force),
                Effect::OpenRemoveModal { force } => state.open_remove_modal(force),
                // `terminal` / `agent` are session commands, but the Overview line
                // still dispatches them if typed: focus the active session (the
                // root by default) and attach a fresh pane.
                effect @ (Effect::OpenTerminal | Effect::OpenAgent) => {
                    let row = state.list().active_index();
                    state.enter_focus(row);
                    launch_pane(term, state, painter, wiring, effect == Effect::OpenAgent);
                }
                // Hand off to the settings screen; it owns the terminal until
                // dismissed. Quitting there quits the app; otherwise we resume,
                // forcing a full repaint over the screen it drew.
                Effect::OpenConfig => match (wiring.open_config)(term)? {
                    // The user quit the app from the settings screen.
                    None => return Ok(Flow::Quit),
                    // Back to home: the config screen may have changed the Session
                    // Action UI (在席 mode's surface) or the local LLM's
                    // availability, so apply the re-read settings — otherwise
                    // Focus keeps rendering the old mode / `ai` visibility.
                    Some(reload) => {
                        state.set_session_action_ui(reload.session_action_ui);
                        state.set_ai_available(reload.ai_available);
                        painter.reset();
                    }
                },
                // `close` is a session command; the Overview line still
                // dispatches it if typed, closing the focused session. On the root
                // row (the default) it is refused, since the root is the workspace
                // itself and not a session.
                Effect::CloseSession => close_focused_session(state, wiring),
                // `preview <path|name>` opens the right-pane Markdown preview:
                // resolve and read the file under the workspace root (the impure
                // step), then render and show it (or log a failure). Reading lives
                // in the infrastructure layer; rendering and storing the result is
                // pure state, so both outcomes are testable.
                Effect::OpenPreview(target) => {
                    state.open_preview_result(crate::infrastructure::markdown_file::read_under(
                        wiring.workspace_root,
                        &target,
                    ))
                }
                // `ShowText` already opened its modal inside `submit`; nothing
                // more for the event loop to do.
                Effect::None | Effect::Clear | Effect::ShowText(_) => {}
            }
        }
        Key::Tab => state.complete(),
        Key::Backspace => state.backspace(),
        Key::Del => state.delete_forward(),
        Key::ArrowUp => state.recall_prev(),
        Key::ArrowDown => state.recall_next(),
        // ←/→/Home/End move the caret within the line so editing works like a
        // normal terminal prompt, not just append/delete at the end.
        Key::ArrowLeft => state.cursor_left(),
        Key::ArrowRight => state.cursor_right(),
        Key::Home => state.cursor_home(),
        Key::End => state.cursor_end(),
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
fn activate_named(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    name: &str,
    wiring: &mut Wiring,
) {
    match resolve_row(state, name) {
        Some(row) => focus_and_attach(term, state, painter, wiring, row),
        None => state.log_error(format!("no session named \"{name}\"")),
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
pub(super) fn switch_key(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    key: Key,
    wiring: &mut Wiring,
) -> Flow {
    // While the inline create input is open it captures every key: Enter / Esc
    // close it (lifecycle on the screen state), everything else edits the input
    // through its own methods.
    if state.is_creating() {
        match key {
            Key::Enter => {
                if let Some(name) = state.switch_confirm_create() {
                    // Dispatch the git work to a background worker and stay in
                    // 切替 so the user keeps navigating; the new session appears in
                    // the list when the task finishes (tracked in the task panel).
                    (wiring.dispatch_create)(&name);
                }
            }
            Key::Escape => state.create_cancel(),
            // Editing keys route to the input's own methods; the guard above
            // guarantees it is open.
            _ => {
                let create = state
                    .create_mut()
                    .expect("create input open while creating");
                match key {
                    Key::Backspace => create.backspace(),
                    Key::Del => create.delete_forward(),
                    // ←/→/Home/End move the caret mid-string.
                    Key::ArrowLeft => create.move_left(),
                    Key::ArrowRight => create.move_right(),
                    Key::Home => create.move_home(),
                    Key::End => create.move_end(),
                    Key::Char(c) => create.push_char(c),
                    _ => {}
                }
            }
        }
        return Flow::Continue;
    }

    // While the inline rename input is open it captures every key, like create.
    if state.is_renaming() {
        match key {
            Key::Enter => {
                if let Some((target, label)) = state.switch_confirm_rename() {
                    let outcome = (wiring.rename_display)(&target, &label);
                    state.apply_session_outcome(outcome);
                }
            }
            Key::Escape => state.rename_cancel(),
            _ => {
                let rename = state
                    .rename_mut()
                    .expect("rename input open while renaming");
                match key {
                    Key::Backspace => rename.backspace(),
                    Key::Char(c) => rename.push_char(c),
                    _ => {}
                }
            }
        }
        return Flow::Continue;
    }

    match key {
        // ↑/↓ (k/j) move between sessions.
        Key::ArrowUp | Key::Char('k') => state.switch_move_up(),
        Key::ArrowDown | Key::Char('j') => state.switch_move_down(),
        // ←/→ (h/l) and Ctrl-P/Ctrl-N move between the highlighted session's tabs,
        // so the preview (and what re-attaching reveals) lands on the chosen pane.
        // A no-op on a session with no panes. The Ctrl chords match what 没入 uses,
        // so the same keys work whether a pane is attached or only previewed here.
        Key::ArrowLeft | Key::Char('h') | Key::Char(CTRL_P) => {
            let dir = selected_dir(state, wiring.workspace_root);
            (wiring.tab_op)(&dir, Some(TabNav::Prev));
        }
        Key::ArrowRight | Key::Char('l') | Key::Char(CTRL_N) => {
            let dir = selected_dir(state, wiring.workspace_root);
            (wiring.tab_op)(&dir, Some(TabNav::Next));
        }
        // Enter focuses the selected session: attach its active pane when live,
        // else just enter 在席.
        Key::Enter => {
            let row = state.list().selected_index();
            focus_and_attach(term, state, painter, wiring, row);
        }
        // `t` opens the session's action surface (在席) — a menu or prompt, per the
        // setting — to add a new pane (`terminal` / `agent`), without attaching the
        // existing one first.
        Key::Char('t') => {
            let row = state.list().selected_index();
            state.enter_focus(row);
        }
        // `x` closes the highlighted session's active tab (pane), killing its
        // shell. The next frame re-reads the session's tabs — landing on the next
        // pane, or previewing its 在席 action menu once the last pane is gone.
        Key::Char('x') => {
            let dir = selected_dir(state, wiring.workspace_root);
            (wiring.close_tab)(state, &dir);
        }
        // `c` begins inline session creation.
        Key::Char('c') => {
            let branches = (wiring.existing_branches)();
            state.switch_begin_create(branches);
        }
        // `r` begins inline rename of the selected session's sidebar label
        // (a no-op on the root row, which is not a session).
        Key::Char('r') => {
            state.switch_begin_rename();
        }
        // `n` (or `Ctrl-E`, matching 在席 / 没入) opens the selected session's note
        // editor (a no-op on the root row). `console` decodes Ctrl-E as `Key::End`
        // (see 在席's `Ctrl-E`), so accept that too — here it is unambiguous, as
        // 切替 list navigation has no caret to move (the inline create / rename
        // inputs consume `End` earlier and return before this match).
        Key::Char('n') | Key::Char(CTRL_E) | Key::End => {
            state.switch_begin_note();
        }
        // Esc first dismisses the highlighted session's read-only note overlay
        // (it auto-shows on selection); with no note showing it backs out to
        // where Switch was opened from.
        Key::Escape => {
            if state.switch_note_visible() {
                state.hide_switch_note();
            } else {
                leave_switch(term, state, painter, wiring);
            }
        }
        // Ctrl-O zooms one level further out, to 統括.
        Key::Char(CTRL_O) => state.enter_overview(),
        _ => {}
    }
    Flow::Continue
}

/// Back out of 切替 on `Esc`: return to the mode it was opened from. From
/// 統括 / 在席 this just restores the mode; from 没入 it re-attaches the focused
/// session's pane when that session is still live, mirroring how `Enter` only
/// attaches a live session (so backing out onto an idle row lands in 在席 rather
/// than spawning a surprise shell).
fn leave_switch(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
) {
    match state.switch_return() {
        ReturnMode::Overview => state.enter_overview(),
        ReturnMode::Focus => {
            let row = state.list().selected_index();
            state.enter_focus(row);
        }
        ReturnMode::Attached => {
            let row = state.list().selected_index();
            // Re-attach only when the focused session is live (it always is when
            // the cursor never left the just-detached session); an idle row stays
            // in 在席.
            focus_and_attach(term, state, painter, wiring, row);
        }
    }
}

/// Handle one key in the session-note editor overlay (opened with `n` in 切替 or
/// `Ctrl-E` in 没入). It captures every key: `Ctrl-S` saves the note (persisted
/// through the wiring), `Esc` cancels, `Enter` inserts a newline, and the usual
/// editing keys edit the multi-line buffer. Closing it — saved or cancelled —
/// re-attaches the session's pane when it was opened from 没入.
pub(super) fn note_editor_key(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    key: Key,
    wiring: &mut Wiring,
) {
    // This handler is only entered while the note editor is open (the event loop
    // guards on `note_editor().is_some()`), so the accessors below always resolve.
    match key {
        // `Ctrl-S` saves: persist the note (clearing it when empty) and close,
        // re-attaching the pane when the editor was opened from 没入.
        Key::Char(CTRL_S) => {
            let (target, text, reattach) = state
                .confirm_note_editor()
                .expect("note editor open while editing");
            let outcome = (wiring.set_note)(&target, &text);
            state.apply_session_outcome(outcome);
            if reattach {
                reattach_focused(term, state, painter, wiring);
            }
        }
        // `Esc` closes without saving, re-attaching the pane if it was 没入.
        Key::Escape => {
            let reattach = state.note_editor_reattaches();
            state.note_editor_cancel();
            if reattach {
                reattach_focused(term, state, painter, wiring);
            }
        }
        // Every other key edits the multi-line buffer in place: Enter splits the
        // line, the editing keys delete / move the caret, and a printable
        // character is inserted at the caret.
        key => {
            let area = state
                .note_editor_mut()
                .expect("note editor open while editing")
                .area_mut();
            match key {
                Key::Enter => area.newline(),
                Key::Backspace => area.backspace(),
                Key::Del => area.delete_forward(),
                Key::ArrowLeft => area.move_left(),
                Key::ArrowRight => area.move_right(),
                Key::ArrowUp => area.move_up(),
                Key::ArrowDown => area.move_down(),
                Key::Home => area.move_home(),
                Key::End => area.move_end(),
                Key::Char(c) if !c.is_control() => area.insert(c),
                _ => {}
            }
        }
    }
}

/// Re-attach the selected session's pane after the note editor closes (没入's
/// `Ctrl-E` flow): focus its row and attach it when live, mirroring how `Enter`
/// in 切替 only attaches a live session.
fn reattach_focused(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
) {
    let row = state.list().selected_index();
    focus_and_attach(term, state, painter, wiring, row);
}

/// Focus the list row `row` and, when its session is already live, attach its
/// active pane (没入); an idle row just lands in 在席. Shared by the three entries
/// that focus an existing session — `session switch <name>`, `Enter` in 切替, and
/// backing out of 切替 onto a just-detached session — so the "enter focus → attach
/// if live" decision lives in one place.
fn focus_and_attach(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
    row: usize,
) {
    state.enter_focus(row);
    let dir = selected_dir(state, wiring.workspace_root);
    // A liveness test only — the snapshot geometry is irrelevant here, so it is
    // sized to the current sidebar state and the result is just tested for `Some`.
    if (wiring.preview)(&dir, state.sidebar()).is_some() {
        open_pane(term, state, painter, wiring, false, false);
    }
}

/// Handle one key in 在席 (Focus): drive the right-pane action surface (a menu
/// of the session's commands or a session-scoped prompt), launching `terminal` /
/// `agent` into 没入, or back out to 統括 / 切替.
pub(super) fn focus_key(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    key: Key,
    wiring: &mut Wiring,
) -> Flow {
    // `Esc` peels back one step: on the "+ new" launch surface opened over live
    // panes (e.g. after `Ctrl-T` from 没入) it discards the surface and steps onto
    // the pane's tab so that pane previews again; everywhere else (a pane tab, or
    // an idle session with no pane behind "+ new") it leaves 在席 for 統括. `Ctrl-O`
    // opens 切替 (return here on cancel); `Ctrl-P` / `Ctrl-N` move the tab selector
    // across the session's live panes and the trailing "+ new" tab. These bind the
    // same whichever tab is selected.
    match key {
        Key::Escape => {
            if !state.focus_discard_new_tab() {
                state.leave_focus();
            }
            return Flow::Continue;
        }
        Key::Char(CTRL_O) => {
            state.enter_switch(ReturnMode::Focus);
            return Flow::Continue;
        }
        // `Ctrl-E` edits the focused session's note (a no-op on the root row).
        // Closing it returns here to 在席 — there is no pane to re-attach, so
        // `reattach` is false (unlike 没入's `Ctrl-E`).
        //
        // `console` decodes Ctrl-E as `Key::End` (its readline-style end-of-line),
        // never as the raw `\x05`, so on a real terminal the chord lands here as
        // `End` — accept it too, or this never fires outside the scripted tests.
        // The one surface where `End` must stay end-of-line is the typed prompt,
        // where it moves the caret; the menu and a pane preview have no caret, so
        // there `End` opens the note.
        Key::Char(CTRL_E) => {
            state.open_focused_note(false);
            return Flow::Continue;
        }
        Key::End
            if !(state.focus_on_new_tab()
                && state.session_action_ui() == SessionActionUi::Prompt) =>
        {
            state.open_focused_note(false);
            return Flow::Continue;
        }
        // `Ctrl-P` / `Ctrl-N` walk the combined tab strip. Landing on a pane tab
        // makes that pane active in the pool (so its preview shows, and re-attach
        // lands on it); landing on the "+ new" tab is a pure state move with no
        // pool tab to activate.
        Key::Char(CTRL_P) => {
            if let Some(index) = state.focus_tab_prev() {
                let dir = selected_dir(state, wiring.workspace_root);
                (wiring.tab_op)(&dir, Some(TabNav::To(index)));
            }
            return Flow::Continue;
        }
        Key::Char(CTRL_N) => {
            if let Some(index) = state.focus_tab_next() {
                let dir = selected_dir(state, wiring.workspace_root);
                (wiring.tab_op)(&dir, Some(TabNav::To(index)));
            }
            return Flow::Continue;
        }
        _ => {}
    }

    // The "+ new" tab drives the action surface (a menu / prompt that launches a
    // pane); a pane tab is a preview, so its only action is `Enter` to re-attach
    // the selected (now-active) pane — every other key is inert there.
    if state.focus_on_new_tab() {
        match state.session_action_ui() {
            SessionActionUi::Menu => focus_menu_key(term, state, painter, key, wiring),
            SessionActionUi::Prompt => focus_prompt_key(term, state, painter, key, wiring),
        }
    } else if key == Key::Enter {
        open_pane(term, state, painter, wiring, false, false);
    }
    Flow::Continue
}

/// Close the focused session forcefully — the `close` command's effect.
/// Dispatches a background removal like `session remove <name> --force`
/// (discarding any uncommitted changes) and, since the user asked to close this
/// session, leaves 在席 for 切替 (Switch) at once so they can pick the next one
/// (`Esc` backs out to 統括); the removal's result is logged and the list
/// refreshed when the background task finishes. The root row is the workspace
/// itself, not a session, so closing it is refused outright and stays in 在席.
fn close_focused_session(state: &mut HomeState, wiring: &mut Wiring) {
    let name = state.focused_session_name();
    // The root row is the workspace itself, not a session, so it cannot be
    // closed. The 在席 menu hides `close` here, but the prompt could still be
    // typed, so refuse it explicitly and stay in 在席.
    if name == ROOT_NAME {
        state.log_error("the root row is the workspace and cannot be closed");
        return;
    }
    (wiring.dispatch_remove)(&name, true);
    state.enter_switch(ReturnMode::Overview);
}

/// 在席 menu surface: `↑`/`↓` move the cursor, `Enter` runs the highlighted
/// command, and `t` / `a` are shortcuts for `terminal` / `agent`. `ai` runs its
/// coming-soon line.
fn focus_menu_key(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    key: Key,
    wiring: &mut Wiring,
) {
    match key {
        Key::ArrowUp | Key::Char('k') => state.focus_menu_move_up(),
        Key::ArrowDown | Key::Char('j') => state.focus_menu_move_down(),
        Key::Enter => {
            if let Some(command) = state.focus_selected_command() {
                run_focus_command(term, state, painter, command.name, wiring);
            }
        }
        Key::Char('t') => run_focus_command(term, state, painter, "terminal", wiring),
        Key::Char('a') => run_focus_command(term, state, painter, "agent", wiring),
        _ => {}
    }
}

/// 在席 prompt surface: edit / complete the session-scoped command line and run
/// it on `Enter`, attaching the pane on `terminal` / `agent`.
fn focus_prompt_key(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    key: Key,
    wiring: &mut Wiring,
) {
    match key {
        Key::Enter => {
            // `terminal` / `agent` attach the pane; `close` removes the session
            // and leaves 在席; `ai` (coming soon) and anything else only log,
            // staying in Focus.
            let effect = state.focus_prompt_submit().effect;
            match effect {
                Effect::OpenTerminal | Effect::OpenAgent => {
                    launch_pane(term, state, painter, wiring, effect == Effect::OpenAgent);
                }
                Effect::CloseSession => close_focused_session(state, wiring),
                _ => {}
            }
        }
        Key::Tab => {
            let _ = state.focus_prompt_complete();
        }
        // Editing keys route straight to the prompt's own TextInput methods.
        Key::Backspace => {
            state.focus_prompt_mut().backspace();
        }
        Key::Del => {
            state.focus_prompt_mut().delete_forward();
        }
        // ←/→/Home/End move the caret so the prompt can be edited mid-string.
        Key::ArrowLeft => state.focus_prompt_mut().move_left(),
        Key::ArrowRight => state.focus_prompt_mut().move_right(),
        Key::Home => state.focus_prompt_mut().move_home(),
        Key::End => state.focus_prompt_mut().move_end(),
        Key::Char(c) => state.focus_prompt_mut().insert(c),
        _ => {}
    }
}

/// Run a named session command (`terminal` / `agent` / `ai`) from the 在席 menu:
/// the two launch commands attach the pane (没入); `ai` logs its coming-soon
/// line.
fn run_focus_command(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    name: &str,
    wiring: &mut Wiring,
) {
    match name {
        "terminal" => launch_pane(term, state, painter, wiring, false),
        "agent" => launch_pane(term, state, painter, wiring, true),
        // `close` removes the focused session forcefully and leaves 在席.
        "close" => close_focused_session(state, wiring),
        // `ai` (and any future coming-soon command) just logs its line.
        _ => state.log_output(format!("\"{name}\" is coming soon 🐰")),
    }
}

/// Add a fresh `terminal` / `agent` pane to the focused session and drive it
/// (没入). `agent` launches the AI agent CLI inside the pane; otherwise a plain
/// shell. Shared by the three surfaces that launch a pane on command — Overview's
/// typed `terminal` / `agent`, the 在席 menu, and the 在席 prompt — each of which
/// has already focused the target row.
fn launch_pane(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
    agent: bool,
) {
    open_pane(term, state, painter, wiring, agent, true);
}

/// Open the embedded terminal pane (没入) for the focused session and run it
/// until the user leaves it, then act on the [`PaneExit`].
///
/// `agent` governs the shell opened here (`agent` launches the AI agent CLI
/// inside it; `terminal` opens a plain shell). `new_pane` chooses whether to add
/// a fresh pane (the 在席 action surface's `terminal` / `agent`, so a session can
/// hold several) or re-attach the session's active pane (`Enter` on a live
/// session in 切替). The pane is driven by the impure `open_terminal` callback,
/// which returns:
///
/// - [`PaneExit::Closed`] — the shell exited: return to 在席 (Focus).
/// - [`PaneExit::ToSwitch`] — `Ctrl-O`: zoom out to 切替 (Switch), remembering to
///   re-attach (`ReturnMode::Attached`) if the user backs out.
/// - [`PaneExit::ToFocus`] — `Ctrl-T`: zoom out to 在席 (Focus), the session's
///   action menu, leaving every pane alive in the pool.
fn open_pane(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
    agent: bool,
    new_pane: bool,
) {
    let (label, fail) = if agent {
        ("Agent", "agent")
    } else {
        ("Terminal", "terminal")
    };
    let dir = selected_dir(state, wiring.workspace_root);
    // Spawning the PTY (and launching the agent CLI inside it) blocks for a beat;
    // flash the loading rabbit in the top-right so the wait reads as deliberate,
    // until the pane itself paints over the screen.
    state.step_loading(if agent {
        "エージェント起動中…"
    } else {
        "ターミナル起動中…"
    });
    let _ = paint_now(term, painter, state);
    state.finish_loading();
    state.show_attached();
    let outcome = (wiring.open_terminal)(state, &dir, agent, new_pane);
    // The pane toggled `crossterm`'s raw mode around itself and ran a full-screen
    // child that may have reset the terminal; re-assert the alternate screen and
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
        Ok(PaneExit::OpenNote) => {
            // `Ctrl-E` opens the focused session's note editor over the (now
            // detached) pane; closing it re-attaches. The root row is the
            // workspace, not a session, so it has no note — fall back to
            // re-attaching straight away.
            if !state.open_focused_note(true) {
                let row = state.list().active_index();
                focus_and_attach(term, state, painter, wiring, row);
            }
        }
        Ok(PaneExit::ToFocus) => {
            // `Ctrl-T` zooms out one level to 在席: the session's action surface,
            // where the user picks the next action (terminal / agent / …). Every
            // pane stays alive in the pool, so re-launching re-attaches them.
            state.leave_attached();
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
