//! The home screen's per-mode key handlers. The event loop in [`super`]
//! dispatches each key to one of the entry handlers — `palette_key` (the `:`
//! command palette overlay) / `switch_key` / `focus_key` — by overlay and mode;
//! those delegate to the helpers here (`focus_and_attach`, `leave_switch`, the
//! focus-surface handlers, …) and to `open_pane`, which drives the embedded
//! terminal (没入). All are pure aside from the injected callbacks, which they
//! reach through the shared [`Wiring`] bundle.

use std::time::{Duration, Instant};

use anyhow::Result;
use console::Key;
use console::Term;

use crate::presentation::tui::io::screen::{self, FramePainter};

use crate::domain::settings::{AgentCli, SessionActionUi};

use super::super::command::Effect;
use super::super::state::{HomeState, ModalSize, PaneExit, ReturnMode, ROOT_NAME};
use super::super::terminal::tabs::TabNav;
use super::super::ui;
use super::{
    paint_now, selected_dir, Flow, Wiring, CTRL_CARET, CTRL_E, CTRL_N, CTRL_O, CTRL_P, CTRL_S,
};

/// Handle one key in the workspace command palette overlay (`:`): edit /
/// complete / recall the workspace command line and run it on `Enter`,
/// dispatching the resulting [`Effect`]. A command with a transitioning effect
/// (entering a mode, attaching a pane, opening config / preview, …) closes the
/// palette first so the effect takes over the screen; a non-transitioning one
/// (a logged result, a text dump) keeps it open so the response shows.
pub(super) fn palette_key(
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
            // A transitioning effect closes the palette so it can take over the
            // screen; a non-transitioning one keeps it open so its response
            // shows. That per-command setting lives on the effect itself
            // ([`Effect::closes_palette`]), applied once here rather than arm by
            // arm. `Activate` is the runtime-conditional exception and closes the
            // palette inside its own arm.
            if submission.effect.closes_palette() {
                state.close_command_palette();
            }
            match submission.effect {
                Effect::Quit => return Ok(Flow::Quit),
                // `session switch` with no name moves keyboard focus to the base
                // 切替 to pick a session.
                Effect::EnterSwitch => state.enter_switch(ReturnMode::Base),
                // `session switch <name>` focuses that session: if it resolves,
                // close the palette and enter 在席 (attaching when it is live);
                // an unknown name keeps the palette open so the error shows.
                Effect::Activate(name) => match resolve_row(state, &name) {
                    Some(row) => {
                        state.close_command_palette();
                        focus_and_attach(term, state, painter, wiring, row);
                    }
                    None => state.log_error(format!("no session named \"{name}\"")),
                },
                // `session create <name>` dispatches the git work to a background
                // worker and returns at once; the new session appears in the list
                // when the task finishes (tracked in the top-right task panel).
                Effect::CreateSession(name) => (wiring.dispatch_create)(&name),
                // `session create` with no name moves to 切替 and opens the inline
                // name input there (creation lives in Switch).
                Effect::OpenSessionModal => {
                    state.enter_switch(ReturnMode::Base);
                    let branches = (wiring.existing_branches)();
                    state.switch_begin_create(branches);
                }
                // `session list`: the state holds the sessions but not their
                // wording — the ui layer formats them into the empty-state line
                // or the scrollable modal, which we then apply. The palette stays
                // open behind the modal so the user lands back on it on dismiss.
                Effect::ListSessions => match ui::content::session_list(state.sessions()) {
                    ui::content::SessionList::Empty(line) => state.log_output(line),
                    ui::content::SessionList::Modal(title, lines) => {
                        state.open_text_modal(title, lines, ModalSize::Normal)
                    }
                },
                // `session remove <name>` dispatches the removal to a background
                // worker; the session leaves the list when the task finishes.
                Effect::RemoveSession { name, force } => (wiring.dispatch_remove)(&name, force),
                // `session remove` with no name opens the removal checklist over
                // the palette, so it stays open behind it.
                Effect::OpenRemoveModal { force } => state.open_remove_modal(force),
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
                        state.set_key_scheme(reload.key_scheme);
                        state.set_ai_available(reload.ai_available);
                        painter.reset();
                    }
                },
                // `preview <path|name>` opens the right-pane Markdown preview (the
                // palette has already closed above): resolve and read the file
                // under the workspace root (the impure step) and render / show it
                // (or log a failure). Reading lives in the infrastructure layer;
                // rendering and storing the result is pure state, so both outcomes
                // are testable.
                Effect::OpenPreview(target) => {
                    state.open_preview_result(crate::infrastructure::markdown_file::read_under(
                        wiring.workspace_root,
                        &target,
                    ))
                }
                // `ShowText` already opened its modal inside `submit`; the palette
                // stays open behind it. `None` / `Clear` likewise keep it open.
                //
                // `OpenTerminal` / `OpenAgent` / `CloseSession` are session-scoped
                // (`terminal` / `agent` / `close`): the palette is a workspace
                // surface, so `dispatch_in_scope` refuses them before they reach
                // here — they only fire from the 在席 prompt (see `focus_prompt_key`).
                // Listed so the match stays exhaustive; they are unreachable here.
                Effect::None
                | Effect::Clear
                | Effect::ShowText { .. }
                | Effect::OpenTerminal
                | Effect::OpenAgent(_)
                | Effect::CloseSession => {}
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
        // `Esc` closes the palette, returning to the mode beneath it.
        Key::Escape => state.close_command_palette(),
        Key::Char(c) => state.push_char(c),
        _ => {}
    }
    Ok(Flow::Continue)
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
            // Editing keys route to the input's own methods (the same set as the
            // create input), so a typo can be fixed mid-label rather than only at
            // the end. The guard above guarantees it is open.
            _ => {
                let rename = state
                    .rename_mut()
                    .expect("rename input open while renaming");
                match key {
                    Key::Backspace => rename.backspace(),
                    Key::Del => rename.delete_forward(),
                    // ←/→/Home/End move the caret mid-string.
                    Key::ArrowLeft => rename.move_left(),
                    Key::ArrowRight => rename.move_right(),
                    Key::Home => rename.move_home(),
                    Key::End => rename.move_end(),
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
        // K/J (Shift+k/j) move the *selected session itself* up/down, persisting
        // the new order — capital mirrors the lower-case cursor move. The cursor
        // follows the moved session; a no-op on the root row and at the ends.
        Key::Char('K') => reorder_selected(state, wiring, true),
        Key::Char('J') => reorder_selected(state, wiring, false),
        // `s` toggles lifting the sessions waiting for input (◆) to the top, so the
        // next session to touch sits at the top. A display-only sort: the manual
        // `K`/`J` order is preserved within each group and restored when off.
        Key::Char('s') => state.toggle_sort_waiting(),
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
        // `:` summons the workspace command palette overlay (the `session` /
        // `config` / `doctor` / `man` commands). It is placed after the inline
        // create / rename / note guards above, so `:` is a literal character
        // while typing a name and only opens the palette from the base list.
        Key::Char(':') => state.open_command_palette(),
        // `?` opens the keybinding cheat sheet (a large scrollable text modal),
        // so the reserved keys never have to be memorised. Like `:`, it sits
        // after the inline-input guards above, so `?` types a literal character
        // into a name field and only opens the sheet from the base list.
        Key::Char('?') => state.open_text_modal(
            "Keys",
            ui::content::cheatsheet(state.key_scheme()),
            ModalSize::Large,
        ),
        // Esc first dismisses the highlighted session's read-only note overlay
        // (it auto-shows on selection); with no note showing it backs out to
        // where Switch was opened from (inert at the base Switch).
        Key::Escape => {
            if state.switch_note_visible() {
                state.hide_switch_note();
            } else {
                leave_switch(term, state, painter, wiring);
            }
        }
        // Ctrl-^ jumps straight back to the previously focused session.
        Key::Char(CTRL_CARET) => jump_to_previous(term, state, painter, wiring),
        _ => {}
    }
    Flow::Continue
}

/// Move the selected session one row up (`up`) or down in the list (`K` / `J` in
/// 切替), persisting the new order through the wiring and refreshing the pane so
/// the cursor follows it. A no-op on the root row, which is not a reorderable
/// session.
fn reorder_selected(state: &mut HomeState, wiring: &mut Wiring, up: bool) {
    if state.list().root_selected() {
        return;
    }
    let name = state.list().selected_name().to_string();
    let outcome = (wiring.reorder_session)(&name, up);
    state.apply_reorder(outcome);
}

/// Back out of 切替 on `Esc`: return to the mode it was opened from. At the base
/// Switch (the default) `Esc` is inert — the home screen is not left by backing
/// out. From 在席 it restores Focus; from 没入 it re-attaches the focused
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
        // The base Switch is the default mode: `Esc` stays put.
        ReturnMode::Base => {}
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
            // `Shift`+a cursor key extends the selection. `console` cannot decode
            // these chords (it leaks the escape sequence's bytes as stray keys), so
            // `term_reader` reassembles each into one `UnknownEscSeq`, which
            // `shift_select` maps back to the motion here.
            if let Some(motion) = shift_select(&key) {
                match motion {
                    Select::Left => area.select_left(),
                    Select::Right => area.select_right(),
                    Select::Up => area.select_up(),
                    Select::Down => area.select_down(),
                    Select::Home => area.select_home(),
                    Select::End => area.select_end(),
                }
                return;
            }
            match key {
                Key::Enter => area.newline(),
                Key::Backspace => area.backspace(),
                Key::Del => area.delete_forward(),
                // A plain (unshifted) motion collapses any selection and moves.
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

/// A cursor motion that, held with `Shift`, extends the note selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Select {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

/// Decode a reassembled modified-cursor-key sequence — `CSI 1 ; <mod> <letter>`,
/// which [`super::super::super::io::term_reader`] gathers into one
/// [`Key::UnknownEscSeq`] — into the selection motion it represents, but **only
/// when `Shift` is among its modifiers** (the chord that extends the selection).
/// Returns `None` for any other key, modifier, or malformed sequence, so a plain
/// `Ctrl`/`Alt`+arrow (or an unrelated escape) falls through untouched.
fn shift_select(key: &Key) -> Option<Select> {
    let Key::UnknownEscSeq(seq) = key else {
        return None;
    };
    // The sequence is `[ 1 ; <modifier digits> <letter>`.
    let rest = seq.strip_prefix(&['[', '1', ';'])?;
    let (letter, modifier) = rest.split_last()?;
    let modifier: u32 = modifier.iter().collect::<String>().parse().ok()?;
    // xterm encodes the modifier as `1 + bitmask`, with bit 0 = Shift; require it.
    if modifier.checked_sub(1)? & 1 == 0 {
        return None;
    }
    Some(match letter {
        'A' => Select::Up,
        'B' => Select::Down,
        'C' => Select::Right,
        'D' => Select::Left,
        'H' => Select::Home,
        'F' => Select::End,
        _ => return None,
    })
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

/// How close two left clicks on the same session row must fall to count as a
/// double click — the threshold separating a single click (select the row) from
/// a double click (confirm, like `Enter`).
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Handle a left click that landed on the selectable session `row` in 切替
/// (Switch): a single click selects the row (moves the cursor onto it), and a
/// second click on the same row within [`DOUBLE_CLICK`] confirms it — focusing
/// the session and attaching its pane when live, exactly like `Enter`.
///
/// `last_click` carries the previous click's row and time across event-loop
/// iterations so the double click can be detected; a confirm clears it so a third
/// click starts a fresh single click rather than re-confirming.
pub(super) fn switch_click(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
    row: usize,
    now: Instant,
    last_click: &mut Option<(usize, Instant)>,
) {
    let is_double = matches!(
        *last_click,
        Some((prev_row, at)) if prev_row == row && now.duration_since(at) <= DOUBLE_CLICK
    );
    // Always land the cursor on the clicked row first, so a double click confirms
    // the row it lands on and a single click just leaves it selected.
    state.switch_select(row);
    if is_double {
        *last_click = None;
        focus_and_attach(term, state, painter, wiring, row);
    } else {
        *last_click = Some((row, now));
    }
}

/// Re-attach the session a restored 没入 (Attached) engagement recorded, run once
/// on the event loop's first pass. [`HomeState::restore_focus`] focused the
/// session synchronously at startup and armed it here; attaching needs the
/// terminal wiring, so it happens now rather than at startup. A no-op when
/// nothing was armed, the session has since gone, or it has no live pane (it then
/// stays in 在席, exactly as `Enter` on an idle row would).
///
/// [`HomeState::restore_focus`]: super::super::state::HomeState::restore_focus
pub(super) fn resume_attach(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
) {
    if state.take_resume_attach() {
        // `restore_focus` already focused the session, so the cursor is on it;
        // attach the focused row, landing in 在席 when it has no live pane.
        let row = state.list().selected_index();
        focus_and_attach(term, state, painter, wiring, row);
    }
}

/// Jump to the previously focused session — the `Ctrl-^` action (vim's `Ctrl-^`
/// / tmux's `last-window`). Focuses the row [`HomeState::previous_session_row`]
/// resolves to, attaching it when live (so toggling between two running sessions
/// drops straight back into the shell); a no-op when no other session has been
/// focused yet or the previous one has since been removed. Focusing it records
/// the session being left as the new previous, so a second `Ctrl-^` toggles back.
fn jump_to_previous(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
) {
    if let Some(row) = state.previous_session_row() {
        focus_and_attach(term, state, painter, wiring, row);
    }
}

/// Handle one key in 在席 (Focus): drive the right-pane action surface (a menu
/// of the session's commands or a session-scoped prompt), launching `terminal` /
/// `agent` into 没入, or back out to 切替.
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
    // an idle session with no pane behind "+ new") it leaves 在席 for 切替. `Ctrl-O`
    // opens 切替 (return here on cancel); `Ctrl-P` / `Ctrl-N` move the tab selector
    // across the session's live panes and the trailing "+ new" tab. These bind the
    // same whichever tab is selected.
    match key {
        Key::Escape => {
            // A first `Esc` collapses an open agent picker (案A) back to the menu;
            // only when none is open does it peel back a step.
            if state.focus_menu_collapse_agent() {
                return Flow::Continue;
            }
            if !state.focus_discard_new_tab() {
                state.leave_focus();
            }
            return Flow::Continue;
        }
        Key::Char(CTRL_O) => {
            state.enter_switch(ReturnMode::Focus);
            return Flow::Continue;
        }
        // `:` summons the workspace command palette overlay from 在席. Handled
        // before the action-surface dispatch so it fires whether the menu or the
        // prompt is showing (in the prompt `:` would otherwise be typed).
        Key::Char(':') => {
            state.open_command_palette();
            return Flow::Continue;
        }
        // `Ctrl-^` jumps straight back to the previously focused session.
        Key::Char(CTRL_CARET) => {
            jump_to_previous(term, state, painter, wiring);
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
        // `?` opens the keybinding cheat sheet. Guarded like `End` above: on every
        // surface but the typed Prompt it opens the sheet, while in the Prompt's
        // command line `?` stays a literal character (so a session-scoped command
        // can contain it).
        Key::Char('?')
            if !(state.focus_on_new_tab()
                && state.session_action_ui() == SessionActionUi::Prompt) =>
        {
            state.open_text_modal(
                "Keys",
                ui::content::cheatsheet(state.key_scheme()),
                ModalSize::Large,
            );
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

/// Close the focused session — the `close` command's effect. Dispatches a
/// background removal like `session remove <name>` (no `--force`): a clean
/// session is removed, but one with **uncommitted changes is refused** and the
/// task logs how to discard them (`session remove <name> --force`), so a single
/// `close` can never silently throw away unsaved work. This matches the CLI's
/// `session remove` default and the quit-confirm modal's intent to protect
/// running work, rather than the old unconditional `--force`.
///
/// Either way the user asked to leave this session, so 在席 yields to the base
/// 切替 (Switch) at once to pick the next one (`Esc` is inert there); the
/// removal's result — success or the dirty refusal — is logged and the list
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
    // `false`: do not force. A dirty worktree is refused (the task logs the
    // `--force` hint) instead of being discarded without confirmation.
    (wiring.dispatch_remove)(&name, false);
    state.enter_switch(ReturnMode::Base);
}

/// 在席 menu surface: `↑`/`↓` move the cursor, `Enter` runs the highlighted
/// command, and `t` / `a` are shortcuts for `terminal` / `agent`. `ai` runs its
/// coming-soon line.
///
/// On the `agent` row, `→` / `Tab` expands the agent picker (案A) when more than
/// one CLI is installed; while it is expanded the keys drive the picker instead —
/// `↑`/`↓` move within it, `Enter` launches the highlighted CLI, and `←` collapses
/// it (as does `Esc`, handled one level up in [`focus_key`]).
fn focus_menu_key(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    key: Key,
    wiring: &mut Wiring,
) {
    if state.focus_menu_expanded() {
        match key {
            Key::ArrowUp | Key::Char('k') => state.focus_menu_move_up(),
            Key::ArrowDown | Key::Char('j') => state.focus_menu_move_down(),
            Key::ArrowLeft => {
                state.focus_menu_collapse_agent();
            }
            Key::Enter => {
                if let Some(cli) = state.focus_menu_selected_agent() {
                    state.focus_menu_collapse_agent();
                    launch_agent(term, state, painter, wiring, Some(cli));
                }
            }
            _ => {}
        }
        return;
    }
    match key {
        Key::ArrowUp | Key::Char('k') => state.focus_menu_move_up(),
        Key::ArrowDown | Key::Char('j') => state.focus_menu_move_down(),
        // On the `agent` row, open the picker to choose a non-default CLI.
        Key::ArrowRight | Key::Tab => state.focus_menu_expand_agent(),
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
                Effect::OpenTerminal => launch_pane(term, state, painter, wiring, false),
                Effect::OpenAgent(cli) => launch_agent(term, state, painter, wiring, cli),
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
        // The menu's `agent` row / `a` shortcut launch the configured default.
        "agent" => launch_agent(term, state, painter, wiring, None),
        // `close` removes the focused session forcefully and leaves 在席.
        "close" => close_focused_session(state, wiring),
        // `ai` (and any future coming-soon command) just logs its line.
        _ => state.log_output(format!("\"{name}\" is coming soon 🐰")),
    }
}

/// Launch an agent pane, recording which CLI to spawn: `None` uses the
/// workspace's configured default (the fast path, always allowed); `Some(cli)`
/// overrides it for this session. A named CLI that is neither the configured
/// default nor installed is refused with an error line instead of launching a
/// shell that would just fail with "command not found". The choice is stashed on
/// the state and consumed by the terminal-pool wiring on the fresh agent spawn.
fn launch_agent(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
    cli: Option<AgentCli>,
) {
    if let Some(cli) = cli {
        if cli != state.default_agent() && !state.installed_agents().contains(&cli) {
            state.log_error(format!("{} is not installed", cli.display_name()));
            return;
        }
    }
    state.set_agent_choice(cli);
    launch_pane(term, state, painter, wiring, true);
}

/// Add a fresh `terminal` / `agent` pane to the focused session and drive it
/// (没入). `agent` launches the AI agent CLI inside the pane; otherwise a plain
/// shell. Shared by the three surfaces that launch a pane on command — the `:`
/// palette's typed `terminal` / `agent`, the 在席 menu, and the 在席 prompt — each of which
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
/// - [`PaneExit::ToPreviousSession`] — `Ctrl-^`: jump to the previously focused
///   session, re-attaching it when live (or 在席 when none was recorded).
/// - [`PaneExit::Quit`] — `Ctrl-Q`: leave the pane and raise the quit-confirmation
///   modal on the home screen (every pane stays alive in the pool until confirmed).
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
        Ok(PaneExit::ToPreviousSession) => {
            // `Ctrl-^` jumps to the previously focused session, re-attaching it
            // when live (like `Enter` in 切替, via `focus_and_attach`); focusing it
            // records the session being left, so a second `Ctrl-^` toggles back.
            // With no previous session recorded, fall back to 在席 on the current
            // one (like `Ctrl-T`), so the pane never lingers in 没入 with no driver.
            match state.previous_session_row() {
                Some(row) => focus_and_attach(term, state, painter, wiring, row),
                None => state.leave_attached(),
            }
        }
        Ok(PaneExit::Quit) => {
            // `Ctrl-Q` in 没入: leave the pane (every shell / agent stays alive in
            // the pool) and raise the quit-confirmation modal on the home screen.
            // The event loop renders it on the next frame; confirming quits, which
            // then drops the pool — so a live agent is never closed by one keystroke.
            // Arm the 没入 engagement for restore *before* `leave_attached` drops the
            // mode to 在席, so a confirmed quit records that the user was attached.
            state.arm_resume_attached();
            state.leave_attached();
            state.open_quit_confirm();
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
    use super::{shift_select, Select};
    use console::Key;

    /// Build the reassembled key for `CSI 1 ; <modifier> <letter>`.
    fn seq(modifier: &str, letter: char) -> Key {
        let mut chars = vec!['[', '1', ';'];
        chars.extend(modifier.chars());
        chars.push(letter);
        Key::UnknownEscSeq(chars)
    }

    #[test]
    fn shift_modifier_maps_each_cursor_key_to_a_motion() {
        assert_eq!(shift_select(&seq("2", 'A')), Some(Select::Up));
        assert_eq!(shift_select(&seq("2", 'B')), Some(Select::Down));
        assert_eq!(shift_select(&seq("2", 'C')), Some(Select::Right));
        assert_eq!(shift_select(&seq("2", 'D')), Some(Select::Left));
        assert_eq!(shift_select(&seq("2", 'H')), Some(Select::Home));
        assert_eq!(shift_select(&seq("2", 'F')), Some(Select::End));
        // Ctrl+Shift (modifier 6) still counts as Shift held.
        assert_eq!(shift_select(&seq("6", 'D')), Some(Select::Left));
    }

    #[test]
    fn a_modifier_without_shift_is_not_a_selection() {
        // Ctrl alone (5) and Alt alone (3) leave the bit-0 Shift flag clear.
        assert_eq!(shift_select(&seq("5", 'D')), None);
        assert_eq!(shift_select(&seq("3", 'C')), None);
        // Modifier 0 underflows the `1 +` encoding and is rejected, not panicked.
        assert_eq!(shift_select(&seq("0", 'D')), None);
    }

    #[test]
    fn malformed_or_unrelated_sequences_decode_to_none() {
        // Not an escape sequence at all.
        assert_eq!(shift_select(&Key::Char('x')), None);
        // Wrong prefix (a different CSI head).
        assert_eq!(shift_select(&Key::UnknownEscSeq(vec!['[', 'M'])), None);
        // No tail after the `1 ;` head.
        assert_eq!(shift_select(&Key::UnknownEscSeq(vec!['[', '1', ';'])), None);
        // Non-numeric modifier.
        assert_eq!(shift_select(&seq("x", 'D')), None);
        // Shift held but the final byte is not a cursor key.
        assert_eq!(shift_select(&seq("2", 'Z')), None);
    }
}
