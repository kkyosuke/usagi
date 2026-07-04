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

use crate::domain::settings::{AgentCli, KeyScheme, SessionActionUi};

use super::super::command::Effect;
use super::super::pane_input::{is_double_click, PointerShape, DOUBLE_CLICK};
use super::super::state::{HomeState, ModalSize, PaneExit, ReturnMode, ROOT_NAME};
use super::super::terminal::tabs::TabNav;
use super::super::ui;
use super::{
    paint_now, selected_diff, selected_dir, Flow, Wiring, CTRL_CARET, CTRL_E, CTRL_N, CTRL_O,
    CTRL_P, CTRL_S,
};

/// Minimum time the launch loader stays visible before a fresh pane spawn begins.
///
/// The home loop is about to hand control to the embedded PTY driver, so a fast
/// spawn can otherwise replace the frame before the user perceives it. Four
/// frames at 60ms cover this minimum and reach frame `3`, where the `run 2`
/// loader grows from three to four rabbits (`RUN2_LOADING_GROW = 3` in `ui`).
const LAUNCH_LOADING_MIN_VISIBLE: Duration = Duration::from_millis(180);
/// Frame cadence for the pre-spawn launch loader.
const LAUNCH_LOADING_FRAME_INTERVAL: Duration = Duration::from_millis(60);

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
            if let Some(entry) = submission.recorded.as_ref() {
                (wiring.persist)(entry);
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
                Effect::CreateSession(name) => {
                    let root = state.selected_workspace_root();
                    state.set_op_target(root.clone());
                    let interaction_epoch = wiring.interaction_epoch;
                    (wiring.dispatch_create)(&root, &name, interaction_epoch);
                }
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
                Effect::RemoveSession {
                    workspace,
                    name,
                    force,
                } => {
                    let root = state.workspace_root_for_session(workspace.as_deref(), &name);
                    state.set_op_target(root.clone());
                    (wiring.dispatch_remove)(&root, &name, force, None);
                }
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
                    // Action UI (在席 mode's surface), the pane key scheme, or the
                    // default Agent CLI, so apply the re-read settings — otherwise
                    // Focus / 没入 keep rendering with the old settings and
                    // `agent` / `ai` keep launching the old CLI.
                    Some(reload) => {
                        state.set_session_action_ui(reload.session_action_ui);
                        state.set_key_scheme(reload.key_scheme);
                        state.set_default_agent(reload.agent_cli);
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
                // `unite add <name>`: resolve and load the named workspace, then
                // stack it into the view (refusing a duplicate or an unknown name).
                Effect::UniteAdd(name) => match (wiring.unite_resolve)(&name) {
                    Ok(group) => {
                        if state.add_extra_group(group) {
                            state.log_output(format!("Added \"{name}\" to the unite view 🪟"));
                        } else {
                            state.log_error(format!("\"{name}\" is already in the view"));
                        }
                    }
                    Err(e) => state.log_error(e),
                },
                // `unite remove <name>`: drop the named extra workspace; removing the
                // last one collapses back to the single-workspace view.
                Effect::UniteRemove(name) => {
                    if state.remove_extra_group(&name) {
                        state.log_output(format!("Removed \"{name}\" from the unite view"));
                    } else {
                        state.log_error(format!("\"{name}\" is not in the unite view"));
                    }
                }
                // `env`: open the workspace-env editor as an overlay *over* the
                // palette (the palette stayed open — `OpenEnvEditor` does not
                // close it), so saving / cancelling returns to the Overview. Seed
                // it from the workspace's current bindings.
                Effect::OpenEnvEditor => {
                    let env = crate::usecase::settings::load_local(wiring.workspace_root)
                        .unwrap_or_default()
                        .env;
                    state.open_env_editor(env);
                }
                // `ShowText` already opened its modal inside `submit`; the palette
                // stays open behind it. `None` / `Clear` likewise keep it open.
                //
                // `OpenTerminal` / `OpenExternalTerminal` / `OpenAgent` /
                // `OpenDiff` / `CloseSession` are session-scoped (`terminal` /
                // `agent` / `diff` / `close`): the palette is a workspace surface,
                // so `dispatch_in_scope` refuses them before they reach here — they
                // only fire from the 在席 menu / prompt (see `focus_prompt_key` and
                // `run_focus_command`). Listed so the match stays exhaustive; they
                // are unreachable here.
                Effect::None
                | Effect::Clear
                | Effect::ShowText { .. }
                | Effect::OpenTerminal
                | Effect::OpenExternalTerminal
                | Effect::OpenAgent(_)
                | Effect::OpenAgentPrompt(_)
                | Effect::OpenChat
                | Effect::OpenDiff
                | Effect::CloseSession { .. } => {}
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
                    // It lands in the cursor's group's workspace (統合/unite mode).
                    let root = state.selected_workspace_root();
                    state.set_op_target(root.clone());
                    let interaction_epoch = wiring.interaction_epoch;
                    (wiring.dispatch_create)(&root, &name, interaction_epoch);
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
                    // Filter control chars so a stray chord (e.g. Ctrl-S) that
                    // arrives as `Key::Char('\x13')` can't inject an invisible
                    // byte into the name — the note editor guards the same way.
                    Key::Char(c) if !c.is_control() => create.push_char(c),
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
                    let root = state.selected_workspace_root();
                    state.set_op_target(root.clone());
                    let outcome = (wiring.rename_display)(&root, &target, &label);
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
                    // Filter control chars (see the create-input arm above).
                    Key::Char(c) if !c.is_control() => rename.push_char(c),
                    _ => {}
                }
            }
        }
        return Flow::Continue;
    }

    if state.list().create_row_selected() {
        match key {
            // The visible "+ new session" row behaves like an input affordance:
            // clicking it or pressing Enter starts the inline create editor, while
            // typing a printable character starts the editor and inserts that
            // character as the first byte of the session name.
            Key::Enter => begin_switch_create(state, wiring, None),
            Key::Char(c) if !c.is_control() => begin_switch_create(state, wiring, Some(c)),
            // Keep keyboard escape hatches on the row: arrows still navigate away
            // (the create row carries no session, so it needs no note / tab keys),
            // and Esc backs out of Switch just as it does on real session rows.
            Key::ArrowUp => state.switch_move_up(),
            Key::ArrowDown => state.switch_move_down(),
            Key::Escape => leave_switch(term, state, painter, wiring),
            _ => {}
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
        // `a` launches an agent for the highlighted session (the 切替 analogue of
        // 在席 menu's `agent` action). `Ctrl-A` is decoded by `console` as
        // `Key::Home`, so accept that as an IME-safe alias: with a Japanese IME
        // left on, bare `a` may compose into kana and never reach usagi, but the
        // control chord still does. Inline create / rename inputs consume `Home`
        // earlier and keep its caret meaning there.
        Key::Char('a') | Key::Home => {
            let row = state.list().selected_index();
            state.enter_focus(row);
            launch_agent(term, state, painter, wiring, None);
        }
        // `c` begins inline session creation.
        Key::Char('c') => {
            begin_switch_create(state, wiring, None);
        }
        // `r` begins inline rename of the selected session's sidebar label
        // (a no-op on the root row, which is not a session).
        Key::Char('r') => {
            state.switch_begin_rename();
        }
        // `n` (or `Ctrl-E`, matching 在席 / 没入) opens the selected session's note
        // editor (a no-op on the root row). `console` decodes Ctrl-E as `Key::End`
        // (see 在席's `Ctrl-E`), so accept that too — it doubles as an IME-safe
        // alias for `n` (a control chord reaches usagi even with a Japanese IME
        // left on). Unambiguous here, as 切替 list navigation has no caret to move
        // (the inline create / rename inputs consume `End` earlier and return
        // before this match).
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
        // Esc backs out to where Switch was opened from (inert at the base
        // Switch). The highlighted session's read-only note overlay stays put —
        // it follows the cursor, not a dismissal.
        Key::Escape => leave_switch(term, state, painter, wiring),
        // `Tab` / `Shift-Tab` cycle the selected session's manual status label
        // forward / back through the effective master, ringing through the "unset"
        // slot. A no-op on the root row or when no labels are defined.
        Key::Tab => apply_label_change(state, wiring, state.cycle_selected_label(true)),
        Key::BackTab => apply_label_change(state, wiring, state.cycle_selected_label(false)),
        // `1`–`9` assign the master's Nth label directly (out of range is a no-op),
        // and `0` clears the label. Terminals cannot reliably distinguish `Ctrl`+digit
        // chords, so the bare digits — free while navigating the list — carry this.
        Key::Char(d @ '1'..='9') => {
            let index = d as usize - '1' as usize;
            apply_label_change(state, wiring, state.select_label_index(index));
        }
        Key::Char('0') => apply_label_change(state, wiring, state.clear_selected_label()),
        // Ctrl-^ jumps straight back to the previously focused session.
        Key::Char(CTRL_CARET) => jump_to_previous(term, state, painter, wiring),
        _ => {}
    }
    Flow::Continue
}

/// Persist a manual-status label change computed by [`HomeState`] and apply the
/// result inline: `change` is the `(session name, new label id)` to store — or
/// `None` when the keypress was a no-op (root row, no labels, or unchanged), in
/// which case nothing is written. Mirrors the rename / note persistence path.
fn apply_label_change(
    state: &mut HomeState,
    wiring: &mut Wiring,
    change: Option<(String, Option<String>)>,
) {
    let Some((name, id)) = change else {
        return;
    };
    let root = state.selected_workspace_root();
    state.set_op_target(root.clone());
    let outcome = (wiring.set_label)(&root, &name, id.as_deref());
    state.apply_session_outcome(outcome);
}

/// Open 切替's inline create input, seeded with `first` when the visible
/// `+ new session` affordance was typed into directly. The branch-name snapshot is
/// taken exactly when the editor opens so validation sees the current workspace.
fn begin_switch_create(state: &mut HomeState, wiring: &mut Wiring, first: Option<char>) {
    let branches = (wiring.existing_branches)();
    state.switch_begin_create(branches);
    if let Some(c) = first {
        if let Some(create) = state.create_mut() {
            create.push_char(c);
        }
    }
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
            let root = state.selected_workspace_root();
            state.set_op_target(root.clone());
            let outcome = (wiring.set_note)(&root, &target, &text);
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

/// Handle one key in the workspace-env editor overlay (the `env` command), which
/// sits over the command palette. `Ctrl-S` parses the buffer's valid bindings,
/// writes them into this workspace's local settings (preserving the other
/// overrides), and closes back to the Overview; `Esc` cancels; every other key
/// edits the multi-line `NAME=op://…` buffer in place. Saving touches the
/// settings file, so this is a handler rather than inline in the loop.
pub(super) fn env_editor_key(state: &mut HomeState, key: Key, wiring: &mut Wiring) {
    // Only entered while the env editor is open (the loop guards on
    // `env_editor().is_some()`), so the accessors below always resolve.
    match key {
        // `Ctrl-S` saves the bindings and returns to the palette.
        Key::Char(CTRL_S) => {
            let env = state
                .confirm_env_editor()
                .expect("env editor open while editing");
            let root = wiring.workspace_root;
            // Read-modify-write so the workspace's other local overrides survive.
            let mut settings = crate::usecase::settings::load_local(root).unwrap_or_default();
            settings.env = env;
            match crate::usecase::settings::save_local(root, &settings) {
                Ok(()) => state.log_output("Saved workspace env 󰤇".to_string()),
                Err(e) => state.log_error(format!("Failed to save env: {e}")),
            }
        }
        // `Esc` closes without saving, returning to the palette.
        Key::Escape => state.env_editor_cancel(),
        // Every other key edits the multi-line buffer in place.
        key => {
            let area = state
                .env_editor_mut()
                .expect("env editor open while editing")
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
        open_pane(term, state, painter, wiring, false, false, true);
    }
}

/// Handle a left click that landed on the selectable session `row` in 切替
/// (Switch): a single click selects the row (moves the cursor onto it), and a
/// second click on the same row within [`DOUBLE_CLICK`] confirms it — focusing
/// the session and attaching its pane when live, exactly like `Enter`.
///
/// `last_click` carries the previous click's row and time across event-loop
/// iterations so the double click can be detected (via [`is_double_click`], the
/// shared core the 没入 pane reuses); a confirm clears it so a third click starts
/// a fresh single click rather than re-confirming.
pub(super) fn switch_click(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
    row: usize,
    now: Instant,
    last_click: &mut Option<(usize, Instant)>,
) {
    // Always land the cursor on the clicked row first, so a double click confirms
    // the row it lands on and a single click just leaves it selected.
    state.switch_select(row);
    if state.list().create_row_selected() {
        begin_switch_create(state, wiring, None);
        return;
    }
    if is_double_click(last_click, row, now, DOUBLE_CLICK) {
        focus_and_attach(term, state, painter, wiring, row);
    }
}

/// Handle a left click on a session `row` in the left pane while in 在席 (Focus):
/// re-focus onto that session — its right-pane action surface rebuilds for the
/// clicked session (menu cursor and prompt reset) — so the list stays a live
/// session switcher even after a session has been entered. A second click on the
/// same row within [`DOUBLE_CLICK`] attaches its pane when live, exactly like
/// `Enter` / a 切替 double click ([`switch_click`]).
///
/// `last_click` is the same cross-iteration click memory `switch_click` threads
/// through [`is_double_click`], so the double-click grammar is identical in both
/// modes; a confirm clears it so a third click starts a fresh single click.
pub(super) fn focus_click(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
    row: usize,
    now: Instant,
    last_click: &mut Option<(usize, Instant)>,
) {
    if row == state.list().create_row() {
        // The create row lives in 切替: a click on it from 在席 zooms back to the
        // picker and opens the same inline input a 切替 click would.
        state.enter_switch(ReturnMode::Focus);
        state.switch_select(row);
        begin_switch_create(state, wiring, None);
        return;
    }
    if is_double_click(last_click, row, now, DOUBLE_CLICK) {
        // `focus_and_attach` re-enters 在席 on the row and attaches when live, so a
        // double click on a running session drops straight into 没入.
        focus_and_attach(term, state, painter, wiring, row);
    } else {
        // A single click switches the focused session, rebuilding the action
        // surface for it; an idle row just stays in 在席, like `Enter` would.
        state.enter_focus(row);
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
    // A pending `Ctrl-O` leader makes this key the second key of the chord — the
    // same prefix grammar as 没入, so `Ctrl-O o` zooms out to 切替 from 在席 exactly
    // as it does from the live terminal. Unlike 没入 this needs no timeout: 在席 has
    // no shell a forgotten leader could leak a literal `Ctrl-O` into, and the very
    // next key always resolves it.
    // Any deliberate key other than `Esc` cancels the one-shot return-to-pane
    // arming (set when 在席 was reached by zooming out of a live pane with
    // `Ctrl-T` / `Ctrl-O a`), so only an *immediate* `Esc` returns to that pane.
    if !matches!(key, Key::Escape) {
        state.clear_focus_return_attach();
    }

    if state.prefix_pending() {
        state.set_prefix_pending(false);
        return focus_prefix_action(term, state, painter, key, wiring);
    }

    // `Esc` peels back one step. When 在席 was reached by zooming out of a live
    // pane (`Ctrl-T` / `Ctrl-O a`) the first `Esc` returns straight to that pane
    // (没入) — back to the tab the zoom started from; otherwise, on a "+ new"
    // launch surface opened over live panes it discards the surface and steps onto
    // the pane's tab so that pane previews again, and everywhere else (a pane tab,
    // or an idle session with no pane behind "+ new") it leaves 在席 for 切替.
    // `Ctrl-O` is the leader for that prefix grammar (the action is the next key);
    // `Ctrl-P` / `Ctrl-N` also move the tab selector directly across the session's
    // live panes and the trailing "+ new" tab. These bind the same whichever tab
    // is selected.
    match key {
        Key::Escape => {
            // A first `Esc` collapses an open agent or close picker back to the
            // menu; only when none is open does it peel back a step.
            if state.focus_menu_collapse_agent() || state.focus_menu_collapse_close() {
                return Flow::Continue;
            }
            // When 在席 was reached by zooming out of a live pane (`Ctrl-T` /
            // `Ctrl-O a`), the first `Esc` returns to that pane (没入) — back to the
            // tab the zoom started from — rather than peeling back toward 切替.
            if state.take_focus_return_attach() {
                open_pane(term, state, painter, wiring, false, false, true);
                return Flow::Continue;
            }
            // A menu floating over a pane tab (the zoomed-out state once another
            // key cancelled the re-attach): `Esc` dismisses the menu, leaving the
            // pane's preview showing — one step short of leaving 在席.
            if state.close_focus_menu_over_pane() {
                return Flow::Continue;
            }
            if !state.focus_discard_new_tab() {
                state.leave_focus();
            }
            return Flow::Continue;
        }
        Key::Char(CTRL_O) => {
            // Under the prefix scheme `Ctrl-O` is the leader (the action is the
            // next key), matching 没入 — `Esc` stays the one-key exit to 切替. The
            // alt scheme drives 没入 with `Alt`-chords and leaves bare `Ctrl-O` to
            // the shell, so there `Ctrl-O` keeps its direct zoom-out to 切替.
            if state.key_scheme() == KeyScheme::Prefix {
                state.set_prefix_pending(true);
            } else {
                state.enter_switch(ReturnMode::Focus);
            }
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
    // pane), and so does the menu floating over a pane tab after a zoom-out; a
    // bare pane tab is a preview, so its only action is `Enter` to re-attach the
    // selected (now-active) pane — every other key is inert there.
    if state.focus_on_new_tab() || state.focus_menu_over_pane() {
        match state.session_action_ui() {
            SessionActionUi::Menu => focus_menu_key(term, state, painter, key, wiring),
            SessionActionUi::Prompt => focus_prompt_key(term, state, painter, key, wiring),
        }
    } else if key == Key::Enter {
        open_pane(term, state, painter, wiring, false, false, true);
    }
    Flow::Continue
}

/// Dispatch the key *after* the `Ctrl-O` leader in 在席 (Focus), mirroring the
/// 没入 prefix grammar (see [`pane_input::prefix_action`]) so the same chords
/// navigate from either surface:
///
/// - `o` (and a double leader `Ctrl-O Ctrl-O`, a control-char second key that
///   works with a Japanese IME on) zooms out to 切替, returning to 在席 on cancel.
/// - `n`/`→` and `p`/`←` walk the tab strip, like the direct `Ctrl-N`/`Ctrl-P`.
/// - `g` launches an agent — 在席's analogue of 没入's "add an agent tab".
/// - `e` edits the note, `s` toggles the sidebar, `q` raises the quit modal.
/// - `Ctrl-^` jumps to the previous session (a direct key in 没入 too).
///
/// `a` (zoom to 在席) is a no-op — we are already here — and every other key is
/// swallowed, exactly as an unrecognised key is after the leader in 没入.
///
/// [`pane_input::prefix_action`]: super::super::pane_input
fn focus_prefix_action(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    key: Key,
    wiring: &mut Wiring,
) -> Flow {
    match key {
        Key::Char('o') | Key::Char(CTRL_O) => state.enter_switch(ReturnMode::Focus),
        Key::Char('n') | Key::ArrowRight => {
            if let Some(index) = state.focus_tab_next() {
                let dir = selected_dir(state, wiring.workspace_root);
                (wiring.tab_op)(&dir, Some(TabNav::To(index)));
            }
        }
        Key::Char('p') | Key::ArrowLeft => {
            if let Some(index) = state.focus_tab_prev() {
                let dir = selected_dir(state, wiring.workspace_root);
                (wiring.tab_op)(&dir, Some(TabNav::To(index)));
            }
        }
        Key::Char('g') => run_focus_command(term, state, painter, "agent", wiring),
        Key::Char('e') => {
            state.open_focused_note(false);
        }
        Key::Char('s') => state.toggle_sidebar(),
        Key::Char('q') => state.open_quit_confirm(),
        Key::Char(CTRL_CARET) => jump_to_previous(term, state, painter, wiring),
        _ => {}
    }
    Flow::Continue
}

/// Close the focused session — the `close` command's effect. Dispatches a
/// background removal like `session remove <name>`: a clean session is removed,
/// while a dirty one is refused unless `force` is true. This keeps bare `close`
/// safe, while `close --force` and the 在席 menu's `Shift`+`c` deliberately mirror
/// `session remove <name> --force`.
///
/// Either way the user asked to leave this session, so 在席 yields to the base
/// 切替 (Switch) at once (`Esc` is inert there). If removal finishes before any
/// other operation, the task then lands on the neighbouring session instead of
/// root; otherwise the refreshed list preserves the user's current operation.
/// The removal's result — success or the dirty refusal — is logged and the list
/// refreshed when the background task finishes. The root row is the workspace
/// itself, not a session, so closing it is refused outright and stays in 在席.
fn close_focused_session(state: &mut HomeState, wiring: &mut Wiring, force: bool) {
    let name = state.focused_session_name();
    // The root row is the workspace itself, not a session, so it cannot be
    // closed. The 在席 menu hides `close` here, but the prompt could still be
    // typed, so refuse it explicitly and stay in 在席.
    if name == ROOT_NAME {
        state.log_error("the root row is the workspace and cannot be closed");
        return;
    }
    let root = state.workspace_root_for_session(None, &name);
    state.set_op_target(root.clone());
    let focus = state
        .focus_target_after_close()
        .map(|name| super::super::tasks::AutoFocus {
            name,
            interaction_epoch: wiring.interaction_epoch,
        });
    (wiring.dispatch_remove)(&root, &name, force, focus);
    state.enter_switch(ReturnMode::Base);
}

/// 在席 menu surface: `↑`/`↓` move the cursor, `Enter` runs the highlighted
/// command, `t` / `a` are shortcuts for `terminal` / `agent`, and `Shift`+`c`
/// runs the deliberate discard path (`close --force`).
///
/// On the `agent` row, `→` / `Tab` expands the agent picker (案A) when more than
/// one CLI is installed; while it is expanded the keys drive the picker instead —
/// `↑`/`↓` move within it, `Enter` launches the highlighted CLI, and `←` collapses
/// it (as does `Esc`, handled one level up in [`focus_key`]).
///
/// On the `close` row, `→` / `Tab` expands the close picker (plain close vs.
/// close --force); the same `↑`/`↓` / `Enter` / `←` pattern drives it.
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
                } else if let Some(action) = state.focus_menu_selected_terminal_action() {
                    state.focus_menu_collapse_agent();
                    if action == "new" {
                        open_external_terminal(state, wiring);
                    } else {
                        launch_pane(term, state, painter, wiring, false);
                    }
                } else {
                    run_focus_close_picker(term, state, painter, wiring);
                }
            }
            _ => {}
        }
        return;
    }
    match key {
        Key::ArrowUp | Key::Char('k') => state.focus_menu_move_up(),
        Key::ArrowDown | Key::Char('j') => state.focus_menu_move_down(),
        // On the `agent` / `terminal` rows, open their inline pickers.
        Key::ArrowRight | Key::Tab => {
            if state.focus_menu_agent_can_expand() {
                state.focus_menu_expand_agent();
            } else if state.focus_menu_terminal_can_expand() {
                state.focus_menu_expand_terminal();
            } else if state.focus_close_can_expand() {
                state.focus_menu_expand_close();
            }
        }
        Key::Enter => {
            if let Some(command) = state.focus_selected_command() {
                run_focus_command(term, state, painter, command.name, wiring);
            }
        }
        Key::Char('t') => run_focus_command(term, state, painter, "terminal", wiring),
        Key::Char('a') => run_focus_command(term, state, painter, "agent", wiring),
        // `Shift`+`c` is the deliberate discard shortcut: run `close --force`
        // instead of the safe `close`, matching the existing capital-letter
        // convention for shifted actions in 切替 (`K`/`J` reorder).
        Key::Char('C') => run_focus_command(term, state, painter, "close --force", wiring),
        _ => {}
    }
}

fn run_focus_close_picker(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
) {
    let force = state.focus_menu_selected_close_force();
    state.focus_menu_collapse_close();
    let cmd = if force { "close --force" } else { "close" };
    run_focus_command(term, state, painter, cmd, wiring);
}

/// 在席 prompt surface: edit / complete the session-scoped command line and run
/// it on `Enter`, attaching the pane on `terminal` / `agent`, and launching the
/// configured agent with an opening prompt on `ai <prompt>`.
fn focus_prompt_key(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    key: Key,
    wiring: &mut Wiring,
) {
    match key {
        Key::Enter => {
            // `terminal` / `agent` attach the pane; `ai <prompt>` attaches the
            // configured agent and hands it that prompt; `chat` opens the local-LLM
            // overlay; `diff` opens the right-pane diff view; `close` removes the
            // session and leaves 在席; anything else only logs, staying in Focus. The
            // command is persisted (with its session) so per-session history
            // survives across launches, like the palette line.
            let submission = state.focus_prompt_submit();
            if let Some(entry) = submission.recorded.as_ref() {
                (wiring.persist)(entry);
            }
            let effect = submission.effect;
            match effect {
                Effect::OpenTerminal => launch_pane(term, state, painter, wiring, false),
                Effect::OpenExternalTerminal => open_external_terminal(state, wiring),
                Effect::OpenAgent(cli) => launch_agent(term, state, painter, wiring, cli),
                Effect::OpenAgentPrompt(prompt) => {
                    launch_agent_with_prompt(term, state, painter, wiring, prompt)
                }
                Effect::OpenChat => state.open_chat(),
                Effect::OpenDiff => state.open_diff_result(selected_diff(state)),
                Effect::CloseSession { force } => close_focused_session(state, wiring, force),
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
        // Filter control chars (see the create-input arm in `switch_key`).
        Key::Char(c) if !c.is_control() => state.focus_prompt_mut().insert(c),
        _ => {}
    }
}

/// Run a named session command (`terminal` / `agent` / `diff` / `chat` /
/// `close` / `close --force`) from the 在席 menu: the launch commands attach
/// the pane (没入), `diff` opens the right-pane diff view over 在席, `chat` opens
/// the local-LLM chat overlay, `close` variants remove the session and leave 在席.
/// The prompt-taking `ai <prompt>` is kept out of the menu (typed in the Prompt
/// UI); a command with no arm here logs its coming-soon line.
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
        // `diff` opens the right-pane diff view of the focused session (the same
        // effect the 在席 prompt / palette `diff` produced): resolve its worktree
        // and shell out to git, then render / store the patch (or log a failure).
        "diff" => state.open_diff_result(selected_diff(state)),
        // `chat` opens the local-LLM chat overlay in the right pane.
        "chat" => state.open_chat(),
        // `close` removes the focused session safely and leaves 在席.
        "close" => close_focused_session(state, wiring, false),
        // `close --force` is exposed as `Shift`+`c` on the 在席 menu for the
        // explicit discard path.
        "close --force" => close_focused_session(state, wiring, true),
        // Any future coming-soon command just logs its line.
        _ => state.log_output(format!("\"{name}\" is coming soon 󰤇")),
    }
}

/// Open a native terminal application at the focused row's directory. Unlike
/// [`launch_pane`], this does not enter 没入: the OS owns the new terminal, and
/// usagi stays in 在席 so the user can continue navigating.
fn open_external_terminal(state: &mut HomeState, wiring: &mut Wiring) {
    let dir = selected_dir(state, wiring.workspace_root);
    match (wiring.open_external_terminal)(&dir) {
        Ok(()) => state.log_output(format!("Opened a new terminal in {}.", dir.display())),
        Err(e) => state.log_error(e),
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

/// Launch the configured agent with an opening prompt from `ai <prompt>`.
///
/// The prompt belongs to the worktree currently focused in 在席. The terminal
/// pool consumes it only for a fresh agent-pane spawn; when the session already
/// has a live agent pane, it is sent directly to that pane as interactive input —
/// whatever CLI that pane runs, so the installed-CLI gate is skipped (nothing is
/// launched). Only a launch that would freshly spawn the configured default is
/// refused when the PATH probe has landed and excludes it, with a Config-oriented
/// hint rather than opening a terminal that immediately fails with
/// `command not found`. Before the probe lands (or when it found no CLI at all)
/// the launch proceeds, mirroring `agent`'s permissiveness for the default.
fn launch_agent_with_prompt(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
    prompt: String,
) {
    let cli = state.default_agent();
    if !state.agent_tab_open()
        && !state.installed_agents().is_empty()
        && !state.installed_agents().contains(&cli)
    {
        state.log_error(format!(
            "Agent CLI is not configured or installed: {} (open config and choose an installed Agent CLI)",
            cli.display_name()
        ));
        return;
    }
    state.set_agent_initial_prompt(prompt);
    state.set_agent_choice(None);
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
    open_pane(term, state, painter, wiring, agent, true, false);
}

/// Number of paint calls required to keep a transient loader visible for at
/// least `min_visible`. The first frame is immediate; each additional frame is
/// separated by `interval`, so `180ms / 60ms` needs four paints at
/// 0/60/120/180ms.
fn launch_loading_frame_count(min_visible: Duration, interval: Duration) -> usize {
    if interval.is_zero() {
        return 1;
    }
    let intervals = min_visible
        .as_nanos()
        .saturating_add(interval.as_nanos().saturating_sub(1))
        / interval.as_nanos();
    usize::try_from(intervals)
        .unwrap_or(usize::MAX)
        .saturating_add(1)
        .max(1)
}

fn wait_launch_loading_frame() {
    std::thread::sleep(LAUNCH_LOADING_FRAME_INTERVAL);
}

/// Paint the launch loader for a short minimum window before entering a fresh
/// pane. This guarantees the indicator is perceptible even when the PTY starts
/// quickly, while the already-painted final frame remains on screen during any
/// subsequent blocking spawn work.
fn paint_launch_loading(
    term: &Term,
    painter: &mut FramePainter,
    state: &mut HomeState,
    label: &str,
) {
    let frames =
        launch_loading_frame_count(LAUNCH_LOADING_MIN_VISIBLE, LAUNCH_LOADING_FRAME_INTERVAL);
    for frame in 0..frames {
        state.step_loading(label);
        let _ = paint_now(term, painter, state);
        if frame + 1 < frames {
            wait_launch_loading_frame();
        }
    }
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
/// - [`PaneExit::ToSession`] — a double click on a selectable sidebar row: switch
///   to that focus row (re-attaching it when live) or open inline creation for
///   the create row.
/// - [`PaneExit::Quit`] — `Ctrl-Q`: leave the pane and raise the quit-confirmation
///   modal on the home screen (every pane stays alive in the pool until confirmed).
///
/// `known_live` means the caller already proved that the focused session has a
/// live pane (typically via a preview snapshot). Re-attaching that pane should
/// avoid both the loading frame and a second preview snapshot.
fn open_pane(
    term: &Term,
    state: &mut HomeState,
    painter: &mut FramePainter,
    wiring: &mut Wiring,
    agent: bool,
    new_pane: bool,
    known_live: bool,
) {
    let (label, fail) = if agent {
        ("Agent", "agent")
    } else {
        ("Terminal", "terminal")
    };
    let dir = selected_dir(state, wiring.workspace_root);
    // Re-attaching a pane that is already live in the pool is instant — the grid
    // is already buffered, so the pane paints over the screen in the same beat.
    // Only a *fresh spawn* (a brand-new pane, or the session's first pane when
    // none is live yet) blocks while the PTY and agent CLI start up. Flashing the
    // loading indicator means painting the whole home frame for one tick; doing
    // that on a re-attach is the visible flicker when switching sessions (the
    // home screen blinks between the old pane and the new one), so restrict it
    // to the spawning case where the wait is real.
    let will_spawn = new_pane || (!known_live && (wiring.preview)(&dir, state.sidebar()).is_none());
    if will_spawn {
        // Spawning the PTY (and launching the agent CLI inside it) blocks for a
        // beat; keep the right-pane-centred loading indicator visible for a
        // short minimum window so the wait reads as deliberate, until the pane
        // itself paints over the screen.
        let label = if agent {
            "エージェント起動中…"
        } else {
            "ターミナル起動中…"
        };
        paint_launch_loading(term, painter, state, label);
        state.finish_loading();
    }
    state.show_attached();
    let outcome = (wiring.open_terminal)(state, &dir, agent, new_pane);
    // The pane toggled `crossterm`'s raw mode around itself and ran a full-screen
    // child that may have reset the terminal; re-assert the alternate screen and
    // wheel-capture modes so the wheel can't scroll the host terminal once we are
    // back on the workspace screen.
    let _ = screen::write_input_modes(term);
    // Leaving the embedded pane returns the pointer to management chrome. The
    // pane itself keeps the last OSC 22 pointer shape across in-pane tab hops so
    // a text caret does not flicker back to an arrow during `Ctrl-O ←/→`, so the
    // boundary between 没入 and the home UI owns the reset instead. Emit this
    // after re-entering the alternate screen so terminals that scope pointer
    // shapes per screen also reset the visible management screen.
    let _ = term.write_str(PointerShape::Default.osc22());
    let _ = term.flush();
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
            // `Ctrl-E` opens the focused row's note editor over the (now detached)
            // pane; closing it re-attaches. Works on the root row too (it edits the
            // workspace root's note). Coming from a live pane there is never a note
            // overlay already open, so this always opens.
            state.open_focused_note(true);
        }
        Ok(PaneExit::ToFocus) => {
            // `Ctrl-T` zooms out one level to 在席: the session's action surface,
            // where the user picks the next action (terminal / agent / …). Every
            // pane stays alive in the pool, so re-launching re-attaches them. The
            // selector stays on the tab the zoom left — its live preview keeps
            // showing behind the floating action menu — instead of jumping to a
            // "+ new" chip for a tab that was never created. Arm the one-shot
            // return-to-pane bit so an immediate `Esc` bounces back to the pane
            // this zoom started from (没入) rather than peeling back to 切替.
            state.leave_attached();
            state.focus_menu_over_active_pane();
            state.arm_focus_return_attach();
        }
        Ok(PaneExit::ToPreviousSession) => {
            // `Ctrl-^` jumps to the previously focused session, re-attaching it
            // when live (like `Enter` in 切替, via `focus_and_attach`); focusing it
            // records the session being left, so a second `Ctrl-^` toggles back.
            // With no previous session recorded, fall back to 在席 on the current
            // one (like `Ctrl-T`), so the pane never lingers in 没入 with no driver.
            match state.previous_session_row() {
                Some(row) => focus_and_attach(term, state, painter, wiring, row),
                None => {
                    state.leave_attached();
                    state.focus_menu_over_active_pane();
                }
            }
        }
        Ok(PaneExit::ToSession(row)) => {
            if row == state.list().create_row() {
                // A double click on the sidebar create row in 没入: leave the pane
                // to the picker and open the same inline create editor that 切替 /
                // 在席 expose. ReturnMode::Attached preserves the usual `Esc`
                // path: if the user cancels creation and backs out of 切替, the
                // live pane re-attaches.
                state.enter_switch(ReturnMode::Attached);
                state.switch_select(row);
                begin_switch_create(state, wiring, None);
            } else {
                // A double click on a sidebar session row in 没入: switch to that
                // focus row, re-attaching it when live (like `Enter` in 切替, via
                // `focus_and_attach`) — focusing records the session being left,
                // so `Ctrl-^` can toggle back.
                focus_and_attach(term, state, painter, wiring, row);
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
    use std::time::Duration;

    use super::{launch_loading_frame_count, shift_select, Select};
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

    #[test]
    fn launch_loading_frame_count_covers_the_minimum_visible_window() {
        assert_eq!(
            launch_loading_frame_count(Duration::from_millis(180), Duration::from_millis(60)),
            4,
            "frames paint at 0/60/120/180ms"
        );
        assert_eq!(
            launch_loading_frame_count(Duration::from_millis(181), Duration::from_millis(60)),
            5,
            "round up so the elapsed window is never shorter than requested"
        );
        assert_eq!(
            launch_loading_frame_count(Duration::ZERO, Duration::from_millis(60)),
            1,
            "even a zero-duration flash still paints once"
        );
    }

    #[test]
    fn launch_loading_frame_count_is_safe_with_a_zero_interval() {
        assert_eq!(
            launch_loading_frame_count(Duration::from_millis(180), Duration::ZERO),
            1
        );
    }
}
