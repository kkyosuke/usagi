use std::path::Path;

use anyhow::Result;
use console::Key;
use console::Term;

use crate::presentation::tui::screen::{FramePainter, Input, KeyReader, ScrollEvent};

use super::command::Effect;
use super::state::{HomeState, Mode, PaneExit, SessionOutcome};
use super::terminal_pool::MonitorHandle;
use super::ui;

/// What the user chose to do on the home (workspace) screen.
#[derive(Debug)]
pub enum Outcome {
    /// Return to the project selection screen without acting on a worktree.
    Back,
    /// The user asked to quit the application entirely.
    Quit,
}

/// Runs the home screen against the given terminal and key source until the
/// user goes back or quits. Assumes the alternate screen is already active (it
/// is owned by the orchestrator, several levels up).
///
/// The screen has two modes. In sidebar mode the worktree list is navigated and
/// `:` (or `i`) opens the command line; in command mode the user types a
/// command, with Tab completion and `↑`/`↓` history recall. Opening a worktree
/// and most commands are placeholders for now (they log a notice).
///
/// Each command the user runs is handed to `persist` so the caller can append
/// it to the workspace's `history.json`; tests pass a no-op.
///
/// Creating a session (the user ran `session <name>`, or confirmed the name in
/// the modal) is delegated to `create_session`; removing one (`session remove
/// <name>`) to `remove_session` (its `bool` is the `--force` flag). Both perform
/// the git / filesystem work and return a [`SessionOutcome`] to apply to the
/// screen, keeping the loop itself free of that IO and directly testable.
///
/// `terminal` embeds a live shell in the right pane, rooted at the selected
/// worktree — or at `workspace_root` when the cursor is on the root row (the
/// entry that belongs to no session). `agent` does the same but launches the
/// configured AI agent CLI inside that shell. The loop
/// switches the right pane to terminal mode, then runs the embedded session via
/// `open_terminal` (its `bool` is `true` for `agent`, `false` for a bare
/// `terminal`) in a small switch loop: the callback returns a [`PaneExit`], and
/// [`PaneExit::Switch`] re-roots the pane at the session the picker focused
/// without leaving it (the shell behind it stays alive), while `Detach` /
/// `Closed` switch the pane back. The PTY I/O, rendering, the persistent shell
/// pool, and agent-command resolution all live in that injected callback.
///
/// `config` opens the settings screen via `open_config`, which runs it against
/// the real terminal and returns `true` when the user quit the application from
/// it (so the loop propagates [`Outcome::Quit`]) and `false` to return to the
/// workspace screen. The screen wiring lives in that injected callback.
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
) -> Result<Outcome> {
    let mut painter = FramePainter::new();
    loop {
        // Mark any background sessions waiting for input before painting.
        state.set_waiting(monitor.waiting());
        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &state);
        painter.paint(term, frame)?;

        let key = match reader.read_input() {
            Ok(Input::Key(key)) => key,
            // A wheel turn scrolls the command-log pane in place (never the host
            // terminal's viewport) and otherwise changes nothing, so redraw and
            // wait for the next event.
            Ok(Input::Scroll(scroll)) => {
                scroll_log(term, &mut state, scroll);
                continue;
            }
            // An interrupted read (e.g. a delivered signal) means quit.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
        };

        // `PageUp` / `PageDown` scroll the command-log pane by a page, in either
        // mode (they are bound nowhere else). Skipped while a modal is capturing
        // keys; otherwise handled here so neither mode has to thread them through.
        if state.modal().is_none() && state.remove_modal().is_none() {
            match key {
                Key::PageUp => {
                    state.scroll_log_up(log_page(term), log_rows(term));
                    continue;
                }
                Key::PageDown => {
                    state.scroll_log_down(log_page(term));
                    continue;
                }
                _ => {}
            }
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
                Key::CtrlC => return Ok(Outcome::Quit),
                _ => {}
            }
            continue;
        }

        // The session-name modal, when open, captures every key until it is
        // confirmed (Enter) or cancelled (Esc), overlaying both modes.
        if state.modal().is_some() {
            match key {
                Key::Enter => {
                    if let Some(name) = state.submit_modal() {
                        let outcome = create_session(&name);
                        state.apply_session_outcome(outcome);
                    }
                }
                Key::Backspace => state.modal_backspace(),
                Key::Escape => state.cancel_modal(),
                Key::CtrlC => return Ok(Outcome::Quit),
                Key::Char(c) => state.modal_push_char(c),
                _ => {}
            }
            continue;
        }

        match state.mode() {
            Mode::Sidebar => match key {
                Key::ArrowUp | Key::Char('k') => state.move_up(),
                Key::ArrowDown | Key::Char('j') => state.move_down(),
                Key::Enter => state.select_worktree(),
                Key::Char(':') | Key::Char('i') => state.enter_command_mode(),
                Key::Char('q') | Key::Escape => return Ok(Outcome::Back),
                Key::CtrlC => return Ok(Outcome::Quit),
                _ => {}
            },
            Mode::Command => match key {
                Key::Enter => {
                    let submission = state.submit();
                    if let Some(command) = submission.recorded.as_deref() {
                        persist(command);
                    }
                    match submission.effect {
                        Effect::Quit => return Ok(Outcome::Quit),
                        Effect::OpenSessionModal => state.open_session_modal(),
                        Effect::CreateSession(name) => {
                            let outcome = create_session(&name);
                            state.apply_session_outcome(outcome);
                        }
                        Effect::ListSessions => state.log_sessions(),
                        Effect::RemoveSession { name, force } => {
                            let outcome = remove_session(&name, force);
                            state.apply_session_outcome(outcome);
                        }
                        Effect::OpenRemoveModal { force } => state.open_remove_modal(force),
                        effect @ (Effect::OpenTerminal | Effect::OpenAgent) => {
                            // Embed the shell in the right pane, rooted at the
                            // selected worktree — or at the workspace root when
                            // the cursor is on the root row (no session), which
                            // makes `selected()` `None`. `agent` is the same
                            // shell with the AI agent CLI launched inside it. The
                            // pane is switched to terminal mode for the duration
                            // and back afterwards.
                            let agent = effect == Effect::OpenAgent;
                            let (label, fail) = if agent {
                                ("Agent", "agent")
                            } else {
                                ("Terminal", "terminal")
                            };
                            let mut dir = state
                                .list()
                                .selected()
                                .map(|w| w.path.clone())
                                .unwrap_or_else(|| workspace_root.to_path_buf());
                            state.show_terminal();
                            // Stay in the pane across session switches: the picker
                            // (`Ctrl-O`) focuses a session and returns `Switch`, so
                            // we re-root the shell at the now-selected session (the
                            // one just left keeps running), and only leave on a
                            // detach, a close, or an error.
                            let outcome = loop {
                                match open_terminal(&mut state, &dir, agent) {
                                    Ok(PaneExit::Switch) => {
                                        dir = state
                                            .list()
                                            .selected()
                                            .map(|w| w.path.clone())
                                            .unwrap_or_else(|| workspace_root.to_path_buf());
                                    }
                                    other => break other,
                                }
                            };
                            state.show_log();
                            // The embedded terminal drew over the whole screen,
                            // so the remembered frame is stale: force a full
                            // repaint on the next pass.
                            painter.reset();
                            match outcome {
                                Ok(PaneExit::Detach) => {
                                    state.log_output(format!("{label} detached (still running) 🐰"))
                                }
                                Ok(_) => state
                                    .log_output(format!("{label} in {} closed.", dir.display())),
                                Err(e) => state.log_error(format!("{fail} failed: {e}")),
                            }
                        }
                        // Hand off to the settings screen; it owns the terminal
                        // until dismissed. Quitting there quits the app;
                        // otherwise we resume the workspace screen, forcing a
                        // full repaint over the screen it drew.
                        Effect::OpenConfig => {
                            if open_config(term)? {
                                return Ok(Outcome::Quit);
                            }
                            painter.reset();
                        }
                        _ => {}
                    }
                }
                Key::Tab => state.complete(),
                Key::Backspace => state.backspace(),
                Key::ArrowUp => state.recall_prev(),
                Key::ArrowDown => state.recall_next(),
                Key::Escape => state.leave_command_mode(),
                Key::CtrlC => return Ok(Outcome::Quit),
                Key::Char(c) => state.push_char(c),
                _ => {}
            },
        }
    }
}

/// The right pane's window height (the rows the command log scrolls within) for
/// the current terminal size.
fn log_rows(term: &Term) -> usize {
    let (height, width) = term.size();
    ui::log_pane_rows(height as usize, width as usize)
}

/// One page of log scrolling: the visible window minus a row of overlap, and at
/// least one line so a tiny pane still moves.
fn log_page(term: &Term) -> usize {
    log_rows(term).saturating_sub(1).max(1)
}

/// Apply a wheel turn to the command-log pane. It scrolls only when the turn
/// happened over the right pane (not the worktree list) and no modal is open,
/// so the wheel never disturbs the rest of the screen.
fn scroll_log(term: &Term, state: &mut HomeState, scroll: ScrollEvent) {
    if state.modal().is_some() || state.remove_modal().is_some() {
        return;
    }
    let (height, width) = term.size();
    if (scroll.col as usize) < ui::right_pane_col_start(width as usize) {
        return;
    }
    let rows = ui::log_pane_rows(height as usize, width as usize);
    if scroll.lines < 0 {
        state.scroll_log_up(scroll.lines.unsigned_abs() as usize, rows);
    } else {
        state.scroll_log_down(scroll.lines as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::LogLine;
    use super::*;
    use crate::domain::workspace_state::{BranchStatus, WorktreeState};
    use chrono::Utc;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io;
    use std::path::PathBuf;

    /// A `create_session` callback that does nothing but report success — shared
    /// by tests that exercise the event loop without inspecting creation.
    fn noop_create(_: &str) -> SessionOutcome {
        SessionOutcome {
            line: LogLine::output("created"),
            sessions: None,
        }
    }

    /// A `remove_session` callback that reports success without touching disk.
    fn noop_remove(_: &str, _: bool) -> SessionOutcome {
        SessionOutcome {
            line: LogLine::output("removed"),
            sessions: None,
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
            // Default to Escape so a test can never spin forever.
            self.keys.pop_front().unwrap_or(Ok(Key::Escape))
        }
    }

    /// A reader that replays a scripted sequence of full [`Input`]s, so a test
    /// can feed wheel scrolls (not just keys) into the loop.
    struct InputReader {
        inputs: VecDeque<io::Result<Input>>,
    }

    impl KeyReader for InputReader {
        fn read_key(&mut self) -> io::Result<Key> {
            loop {
                match self.inputs.pop_front() {
                    Some(Ok(Input::Key(key))) => return Ok(key),
                    Some(Ok(Input::Scroll(_))) => {}
                    Some(Err(e)) => return Err(e),
                    // Default to Escape so a test can never spin forever.
                    None => return Ok(Key::Escape),
                }
            }
        }

        fn read_input(&mut self) -> io::Result<Input> {
            self.inputs
                .pop_front()
                .unwrap_or(Ok(Input::Key(Key::Escape)))
        }
    }

    fn worktree(branch: Option<&str>) -> WorktreeState {
        WorktreeState {
            branch: branch.map(|b| b.to_string()),
            path: PathBuf::from("/repo/wt"),
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
            vec![worktree(Some("main")), worktree(Some("feature"))],
            None,
        )
    }

    /// A `open_terminal` callback that reports the shell closed without spawning
    /// one (so the switch loop runs a single iteration and leaves the pane).
    fn noop_open(_: &mut HomeState, _: &Path, _: bool) -> Result<PaneExit> {
        Ok(PaneExit::Closed)
    }

    /// An `open_config` callback that returns to the workspace screen (does not
    /// quit) without running the real settings screen.
    fn noop_config(_: &Term) -> Result<bool> {
        Ok(false)
    }

    fn run(keys: Vec<io::Result<Key>>, state: HomeState) -> Result<Outcome> {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::detached();
        let mut persist = |_: &str| {};
        let mut create_session: fn(&str) -> SessionOutcome = noop_create;
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        let mut open_terminal: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut open_config: fn(&Term) -> Result<bool> = noop_config;
        event_loop(
            &term,
            &mut reader,
            state,
            Path::new("/ws"),
            &monitor,
            &mut persist,
            &mut create_session,
            &mut remove_session,
            &mut open_terminal,
            &mut open_config,
        )
    }

    /// Drive the loop with a scripted sequence of full inputs (keys and scrolls).
    fn run_inputs(inputs: Vec<io::Result<Input>>, state: HomeState) -> Result<Outcome> {
        let term = Term::stdout();
        let mut reader = InputReader {
            inputs: inputs.into(),
        };
        let monitor = MonitorHandle::detached();
        let mut persist = |_: &str| {};
        let mut create_session: fn(&str) -> SessionOutcome = noop_create;
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        let mut open_terminal: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut open_config: fn(&Term) -> Result<bool> = noop_config;
        event_loop(
            &term,
            &mut reader,
            state,
            Path::new("/ws"),
            &monitor,
            &mut persist,
            &mut create_session,
            &mut remove_session,
            &mut open_terminal,
            &mut open_config,
        )
    }

    #[test]
    fn input_reader_read_key_skips_scrolls_errors_and_defaults_to_escape() {
        // A scroll is skipped and the following key returned.
        let mut reader = InputReader {
            inputs: VecDeque::from(vec![
                Ok(Input::Scroll(scroll_right(-3))),
                Ok(Input::Key(Key::Char('z'))),
            ]),
        };
        assert_eq!(reader.read_key().unwrap(), Key::Char('z'));

        // A read error propagates.
        let mut reader = InputReader {
            inputs: VecDeque::from(vec![Err(io::Error::other("boom"))]),
        };
        assert!(reader.read_key().is_err());

        // Drained input defaults to Escape so a test never spins forever.
        let mut reader = InputReader {
            inputs: VecDeque::new(),
        };
        assert_eq!(reader.read_key().unwrap(), Key::Escape);
    }

    #[test]
    fn a_wheel_scroll_is_consumed_and_the_loop_continues() {
        // A scroll over the right pane is handled in place; the loop then keeps
        // running normally — a command runs (exercising the persist hook) and
        // the trailing Escapes leave the screen.
        let mut inputs = vec![
            Ok(Input::Scroll(scroll_right(-3))),
            Ok(Input::Key(Key::Char(':'))),
        ];
        inputs.extend("man".chars().map(|c| Ok(Input::Key(Key::Char(c)))));
        inputs.extend([
            Ok(Input::Key(Key::Enter)),  // run "man" (persisted)
            Ok(Input::Key(Key::Escape)), // cancel command mode
            Ok(Input::Key(Key::Escape)), // leave sidebar
        ]);
        assert!(matches!(
            run_inputs(inputs, sample_state()).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn page_up_and_page_down_scroll_the_log_then_the_loop_continues() {
        // Both page keys are handled before the mode dispatch; the trailing
        // Escape leaves the screen.
        let keys = vec![
            Ok(Key::PageUp),
            Ok(Key::PageDown),
            Ok(Key::PageUp),
            Ok(Key::Escape),
        ];
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Back));
    }

    #[test]
    fn scroll_log_moves_the_pane_only_over_the_right_pane() {
        let term = Term::stdout();
        let mut state = sample_state();
        for i in 0..200 {
            state.log_output(format!("line {i}"));
        }

        // A wheel turn over the left pane (column 0) is ignored.
        scroll_log(
            &term,
            &mut state,
            ScrollEvent {
                lines: -3,
                col: 0,
                row: 1,
            },
        );
        assert_eq!(state.right_scroll(), 0);

        // Over the right pane it scrolls up, then back down toward the bottom.
        scroll_log(&term, &mut state, scroll_right(-3));
        assert_eq!(state.right_scroll(), 3);
        scroll_log(&term, &mut state, scroll_right(2));
        assert_eq!(state.right_scroll(), 1);
    }

    #[test]
    fn scroll_log_is_ignored_while_a_modal_is_open() {
        let term = Term::stdout();
        let mut state = sample_state();
        for i in 0..200 {
            state.log_output(format!("line {i}"));
        }
        state.open_session_modal();
        scroll_log(&term, &mut state, scroll_right(-3));
        assert_eq!(state.right_scroll(), 0);
    }

    /// The `ScrollEvent` of a wheel turn over the right pane (a high column).
    fn scroll_right(lines: i32) -> ScrollEvent {
        ScrollEvent {
            lines,
            col: 200,
            row: 1,
        }
    }

    /// Types each character of `s` as a `Char` key.
    fn typed(s: &str) -> Vec<io::Result<Key>> {
        s.chars().map(|c| Ok(Key::Char(c))).collect()
    }

    #[test]
    fn escape_returns_back() {
        assert!(matches!(
            run(vec![Ok(Key::Escape)], sample_state()).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn q_returns_back() {
        assert!(matches!(
            run(vec![Ok(Key::Char('q'))], sample_state()).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn ctrl_c_in_sidebar_returns_quit() {
        assert!(matches!(
            run(vec![Ok(Key::CtrlC)], sample_state()).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn sidebar_navigation_and_select_then_back() {
        // Every sidebar navigation arm (arrows + j/k), Enter to select, and an
        // ignored key, then Escape.
        let keys = vec![
            Ok(Key::ArrowDown),
            Ok(Key::ArrowUp),
            Ok(Key::Char('j')),
            Ok(Key::Char('k')),
            Ok(Key::Enter),
            Ok(Key::Home), // ignored (the `_` arm)
            Ok(Key::Escape),
        ];
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Back));
    }

    #[test]
    fn entering_command_mode_and_cancelling_returns_to_sidebar() {
        // ':' enters command mode, Escape cancels back to sidebar, then 'q'
        // leaves the screen. (Reaching the final Back proves the mode switched.)
        let keys = vec![Ok(Key::Char(':')), Ok(Key::Escape), Ok(Key::Char('q'))];
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Back));
    }

    #[test]
    fn i_also_enters_command_mode() {
        // 'i' enters command mode; in command mode 'q' is just typed, so the
        // screen does not exit — the scripted Escape default cancels, and the
        // trailing Escape leaves the sidebar.
        let mut keys = vec![Ok(Key::Char('i'))];
        keys.extend(typed("q"));
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Back));
    }

    #[test]
    fn command_mode_edits_completes_and_recalls_before_running() {
        // Type "ma", Backspace to "m", complete to "man", run it; then recall
        // it with the arrows and run again; finally cancel and leave.
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("ma"));
        keys.push(Ok(Key::Backspace));
        keys.push(Ok(Key::Tab)); // "m" -> "man" (unique)
        keys.push(Ok(Key::Enter)); // run "man"
        keys.push(Ok(Key::ArrowUp)); // recall "man"
        keys.push(Ok(Key::ArrowDown)); // back to empty
        keys.push(Ok(Key::Tab)); // Tab on empty: ambiguous, lists candidates
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Back));
    }

    #[test]
    fn submitted_commands_are_handed_to_persist() {
        // Run "man", then leave: the persist callback receives "man".
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("man"));
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar

        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::detached();
        let mut recorded = Vec::new();
        let mut persist = |command: &str| recorded.push(command.to_string());
        let mut create_session: fn(&str) -> SessionOutcome = noop_create;
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        let mut open_terminal: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut open_config: fn(&Term) -> Result<bool> = noop_config;
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_state(),
            Path::new("/ws"),
            &monitor,
            &mut persist,
            &mut create_session,
            &mut remove_session,
            &mut open_terminal,
            &mut open_config,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(recorded, vec!["man"]);
    }

    #[test]
    fn quit_command_exits_the_app() {
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("quit"));
        keys.push(Ok(Key::Enter));
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn ctrl_c_in_command_mode_returns_quit() {
        let keys = vec![Ok(Key::Char(':')), Ok(Key::CtrlC)];
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn ignored_key_in_command_mode_is_a_noop() {
        // Home has no binding in command mode; it falls through the `_` arm.
        let keys = vec![
            Ok(Key::Char(':')),
            Ok(Key::Home),
            Ok(Key::Escape),
            Ok(Key::Escape),
        ];
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Back));
    }

    /// Run with a `create_session` callback that records the names it is asked
    /// to create and returns a fixed outcome.
    fn run_with_create(
        keys: Vec<io::Result<Key>>,
        state: HomeState,
        outcome: SessionOutcome,
    ) -> (Result<Outcome>, Vec<String>) {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::detached();
        let mut persist = |_: &str| {};
        let mut created = Vec::new();
        let mut create_session = |name: &str| {
            created.push(name.to_string());
            outcome.clone()
        };
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        let mut open_terminal: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut open_config: fn(&Term) -> Result<bool> = noop_config;
        let result = event_loop(
            &term,
            &mut reader,
            state,
            Path::new("/ws"),
            &monitor,
            &mut persist,
            &mut create_session,
            &mut remove_session,
            &mut open_terminal,
            &mut open_config,
        );
        (result, created)
    }

    fn ok_outcome() -> SessionOutcome {
        SessionOutcome {
            line: LogLine::output("created"),
            sessions: None,
        }
    }

    #[test]
    fn creating_a_session_via_the_default_callback_succeeds() {
        // Drives `:session new x` through the shared no-op `create_session`, then
        // leaves — exercising the create branch with the default callback.
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session new x"));
        keys.push(Ok(Key::Enter)); // create via noop_create
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Back));
    }

    #[test]
    fn removing_a_session_via_the_default_callback_succeeds() {
        // Drives `:session remove x` through the shared no-op `remove_session`.
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session remove x"));
        keys.push(Ok(Key::Enter)); // remove via noop_remove
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Back));
    }

    #[test]
    fn session_remove_invokes_the_remove_callback_with_force() {
        // ":session remove old --force" routes to remove_session("old", true).
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session remove old --force"));
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar

        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::detached();
        let mut persist = |_: &str| {};
        let mut create_session: fn(&str) -> SessionOutcome = noop_create;
        let mut removed = Vec::new();
        let mut remove_session = |name: &str, force: bool| {
            removed.push((name.to_string(), force));
            ok_outcome()
        };
        let mut open_terminal: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut open_config: fn(&Term) -> Result<bool> = noop_config;
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_state(),
            Path::new("/ws"),
            &monitor,
            &mut persist,
            &mut create_session,
            &mut remove_session,
            &mut open_terminal,
            &mut open_config,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(removed, vec![("old".to_string(), true)]);
    }

    /// A state seeded with `names` as recorded sessions, so the removal modal
    /// has something to list (the worktree pane is rebuilt from them).
    fn state_with_sessions(names: &[&str]) -> HomeState {
        use crate::domain::workspace_state::SessionRecord;
        let mut state = sample_state();
        let sessions = names
            .iter()
            .map(|n| SessionRecord {
                name: n.to_string(),
                root: PathBuf::from(format!("/ws/.usagi/worktree/{n}")),
                worktrees: vec![worktree(Some(n))],
                created_at: Utc::now(),
            })
            .collect();
        state.restore_sessions(sessions);
        state
    }

    #[test]
    fn session_remove_without_a_name_opens_the_picker_and_bulk_removes() {
        // ":session remove" opens the picker; check the first and third sessions
        // (toggling with Space, moving with arrows and j/k), then Enter removes
        // both via remove_session, in display order.
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session remove"));
        keys.push(Ok(Key::Enter)); // open the modal
        keys.push(Ok(Key::Char(' '))); // check "alpha" (cursor 0)
        keys.push(Ok(Key::ArrowDown)); // cursor 1 ("beta")
        keys.push(Ok(Key::Char('j'))); // cursor 2 ("gamma")
        keys.push(Ok(Key::Char(' '))); // check "gamma"
        keys.push(Ok(Key::Char('k'))); // cursor 1
        keys.push(Ok(Key::ArrowUp)); // cursor 0
        keys.push(Ok(Key::Home)); // ignored key inside the modal
        keys.push(Ok(Key::Enter)); // confirm -> remove alpha & gamma
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar

        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::detached();
        let mut persist = |_: &str| {};
        let mut create_session: fn(&str) -> SessionOutcome = noop_create;
        let mut removed = Vec::new();
        let mut remove_session = |name: &str, force: bool| {
            removed.push((name.to_string(), force));
            ok_outcome()
        };
        let mut open_terminal: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let mut open_config: fn(&Term) -> Result<bool> = noop_config;
        let outcome = event_loop(
            &term,
            &mut reader,
            state_with_sessions(&["alpha", "beta", "gamma"]),
            Path::new("/ws"),
            &monitor,
            &mut persist,
            &mut create_session,
            &mut remove_session,
            &mut open_terminal,
            &mut open_config,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(
            removed,
            vec![("alpha".to_string(), false), ("gamma".to_string(), false)]
        );
    }

    #[test]
    fn session_remove_picker_cancels_via_escape() {
        // Open the picker, check a session, then Esc cancels the modal (the
        // shared no-op `remove_session` would have recorded a removal, but the
        // Escape arm closes without confirming).
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session remove"));
        keys.push(Ok(Key::Enter)); // open the modal
        keys.push(Ok(Key::Char(' '))); // check "alpha"
        keys.push(Ok(Key::Escape)); // cancel the modal
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar
        assert!(matches!(
            run(keys, state_with_sessions(&["alpha"])).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn session_remove_picker_via_the_default_callback_succeeds() {
        // ":session remove", check one session, confirm — routed through the
        // shared no-op `remove_session`, with an empty-selection Enter first to
        // exercise the "stays open" branch.
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session remove"));
        keys.push(Ok(Key::Enter)); // open the modal
        keys.push(Ok(Key::Enter)); // nothing checked -> modal stays open
        keys.push(Ok(Key::Char(' '))); // check "alpha"
        keys.push(Ok(Key::Enter)); // confirm -> remove via noop_remove
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar
        assert!(matches!(
            run(keys, state_with_sessions(&["alpha"])).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn ctrl_c_in_the_removal_modal_returns_quit() {
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session remove"));
        keys.push(Ok(Key::Enter)); // open the modal
        keys.push(Ok(Key::CtrlC)); // quit from within the modal
        assert!(matches!(
            run(keys, state_with_sessions(&["alpha"])).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn session_list_logs_the_sessions() {
        // `:session list` triggers the list effect, which logs the sessions.
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session list"));
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar
                                    // Empty session list still drives the ListSessions arm without panicking.
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Back));
    }

    #[test]
    fn session_command_with_a_name_creates_immediately() {
        // ":session new feature-x" then Enter creates without opening the modal.
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session new feature-x"));
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar

        let (result, created) = run_with_create(keys, sample_state(), ok_outcome());
        assert!(matches!(result.unwrap(), Outcome::Back));
        assert_eq!(created, vec!["feature-x"]);
    }

    #[test]
    fn session_new_opens_the_modal_then_confirms_to_create() {
        // ":session new" + Enter opens the modal; type a fresh name; Enter creates.
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session new"));
        keys.push(Ok(Key::Enter)); // open modal
        keys.extend(typed("wip"));
        keys.push(Ok(Key::Backspace)); // edit: "wi"
        keys.push(Ok(Key::Char('p'))); // "wip"
        keys.push(Ok(Key::Home)); // ignored key inside the modal
        keys.push(Ok(Key::Enter)); // confirm -> create
        keys.push(Ok(Key::Escape)); // back to sidebar (modal closed) -> cancel cmd
        keys.push(Ok(Key::Escape)); // leave sidebar

        let (result, created) = run_with_create(keys, sample_state(), ok_outcome());
        assert!(matches!(result.unwrap(), Outcome::Back));
        assert_eq!(created, vec!["wip"]);
    }

    #[test]
    fn modal_escape_cancels_without_creating() {
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session new"));
        keys.push(Ok(Key::Enter)); // open modal
        keys.extend(typed("x"));
        keys.push(Ok(Key::Escape)); // cancel modal
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar

        let (result, created) = run_with_create(keys, sample_state(), ok_outcome());
        assert!(matches!(result.unwrap(), Outcome::Back));
        assert!(created.is_empty());
    }

    #[test]
    fn modal_invalid_name_keeps_it_open() {
        // Confirming an empty name does not create and keeps the modal open, so
        // the trailing CtrlC (handled by the modal) is what ends the run.
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("session new"));
        keys.push(Ok(Key::Enter)); // open modal
        keys.push(Ok(Key::Enter)); // empty name -> error, stays open
        keys.push(Ok(Key::CtrlC)); // quit from within the modal

        let (result, created) = run_with_create(keys, sample_state(), ok_outcome());
        assert!(matches!(result.unwrap(), Outcome::Quit));
        assert!(created.is_empty());
    }

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

    /// Drive a pane-opening command (`terminal` or `agent`) through the loop with
    /// a custom `open_terminal`, so the effect's directory resolution, agent flag,
    /// and result handling are exercised. `nav` is replayed in sidebar mode first
    /// (e.g. an `ArrowDown` to move the cursor off the root row onto a worktree).
    fn run_pane_with_nav(
        nav: Vec<io::Result<Key>>,
        command: &str,
        state: HomeState,
        open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    ) -> Result<Outcome> {
        let mut keys = nav;
        keys.push(Ok(Key::Char(':')));
        keys.extend(typed(command));
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar

        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::detached();
        let mut persist = |_: &str| {};
        let mut create_session: fn(&str) -> SessionOutcome = noop_create;
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        let mut open_config: fn(&Term) -> Result<bool> = noop_config;
        event_loop(
            &term,
            &mut reader,
            state,
            Path::new("/ws"),
            &monitor,
            &mut persist,
            &mut create_session,
            &mut remove_session,
            open_terminal,
            &mut open_config,
        )
    }

    /// Drive a pane-opening command from the default (root-row) cursor.
    fn run_pane(
        command: &str,
        state: HomeState,
        open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    ) -> Result<Outcome> {
        run_pane_with_nav(Vec::new(), command, state, open_terminal)
    }

    #[test]
    fn terminal_via_the_default_callback_succeeds() {
        // Drives `:terminal` through the shared no-op `open_terminal`.
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("terminal"));
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar
        assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Back));
    }

    #[test]
    fn terminal_opens_in_the_selected_worktree() {
        let opened = RefCell::new(None);
        let mut open = |home: &mut HomeState, dir: &Path, agent: bool| {
            // The pane is in terminal mode while the embedded session runs, and
            // `:terminal` is a bare shell (not the agent).
            assert_eq!(
                home.right_pane(),
                crate::presentation::tui::home::state::RightPane::Terminal
            );
            assert!(!agent);
            *opened.borrow_mut() = Some(dir.to_path_buf());
            Ok(PaneExit::Closed)
        };
        // Move the cursor off the root row onto a worktree (path /repo/wt).
        assert!(matches!(
            run_pane_with_nav(
                vec![Ok(Key::ArrowDown)],
                "terminal",
                sample_state(),
                &mut open
            )
            .unwrap(),
            Outcome::Back
        ));
        assert_eq!(opened.into_inner(), Some(PathBuf::from("/repo/wt")));
    }

    #[test]
    fn terminal_on_the_root_row_opens_in_the_workspace_root() {
        let opened = RefCell::new(None);
        let mut open = |_home: &mut HomeState, dir: &Path, _agent: bool| {
            *opened.borrow_mut() = Some(dir.to_path_buf());
            Ok(PaneExit::Closed)
        };
        // The cursor starts on the root row even with worktrees present, so the
        // shell opens in the workspace root (/ws), not in a worktree.
        assert!(matches!(
            run_pane("terminal", sample_state(), &mut open).unwrap(),
            Outcome::Back
        ));
        assert_eq!(opened.into_inner(), Some(PathBuf::from("/ws")));
    }

    #[test]
    fn terminal_falls_back_to_the_workspace_root_without_worktrees() {
        let opened = RefCell::new(None);
        let mut open = |_home: &mut HomeState, dir: &Path, _agent: bool| {
            *opened.borrow_mut() = Some(dir.to_path_buf());
            Ok(PaneExit::Closed)
        };
        // No worktrees: only the root row, so the shell opens in /ws.
        let state = HomeState::new("usagi", Vec::new(), None);
        assert!(matches!(
            run_pane("terminal", state, &mut open).unwrap(),
            Outcome::Back
        ));
        assert_eq!(opened.into_inner(), Some(PathBuf::from("/ws")));
    }

    #[test]
    fn terminal_failure_is_reported() {
        let mut open = |_: &mut HomeState, _: &Path, _: bool| Err(anyhow::anyhow!("no shell"));
        // A launch failure logs an error but the screen continues to Back.
        assert!(matches!(
            run_pane("terminal", sample_state(), &mut open).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn agent_opens_a_terminal_with_the_agent_flag_set() {
        let saw_agent = RefCell::new(None);
        let mut open = |home: &mut HomeState, dir: &Path, agent: bool| {
            // `:agent` is `:terminal` with the agent CLI launched inside it.
            assert_eq!(
                home.right_pane(),
                crate::presentation::tui::home::state::RightPane::Terminal
            );
            *saw_agent.borrow_mut() = Some((agent, dir.to_path_buf()));
            Ok(PaneExit::Closed)
        };
        // Move the cursor onto a worktree (path /repo/wt) before opening.
        assert!(matches!(
            run_pane_with_nav(vec![Ok(Key::ArrowDown)], "agent", sample_state(), &mut open)
                .unwrap(),
            Outcome::Back
        ));
        assert_eq!(
            saw_agent.into_inner(),
            Some((true, PathBuf::from("/repo/wt")))
        );
    }

    #[test]
    fn agent_via_the_default_callback_succeeds() {
        // Drives `:agent` through the shared no-op `open_terminal`.
        let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        assert!(matches!(
            run_pane("agent", sample_state(), &mut open).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn agent_failure_is_reported() {
        let mut open = |_: &mut HomeState, _: &Path, _: bool| Err(anyhow::anyhow!("no agent"));
        // A launch failure logs an error but the screen continues to Back.
        assert!(matches!(
            run_pane("agent", sample_state(), &mut open).unwrap(),
            Outcome::Back
        ));
    }

    /// A worktree at a specific path, so the switch loop's re-rooting is
    /// observable (the shared `worktree` helper pins every path to /repo/wt).
    fn worktree_at(branch: &str, path: &str) -> WorktreeState {
        WorktreeState {
            branch: Some(branch.to_string()),
            path: PathBuf::from(path),
            head: "abc1234".to_string(),
            primary: false,
            upstream: None,
            status: BranchStatus::Local,
            updated_at: Utc::now(),
        }
    }

    fn two_worktree_state() -> HomeState {
        HomeState::new(
            "usagi",
            vec![
                worktree_at("main", "/r/main"),
                worktree_at("feat", "/r/feat"),
            ],
            None,
        )
    }

    /// Drive `:config` through the loop with a custom `open_config`, so the
    /// effect's hand-off and quit propagation are exercised.
    fn run_config(
        state: HomeState,
        open_config: &mut dyn FnMut(&Term) -> Result<bool>,
    ) -> Result<Outcome> {
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed("config"));
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar

        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let monitor = MonitorHandle::detached();
        let mut persist = |_: &str| {};
        let mut create_session: fn(&str) -> SessionOutcome = noop_create;
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        let mut open_terminal: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        event_loop(
            &term,
            &mut reader,
            state,
            Path::new("/ws"),
            &monitor,
            &mut persist,
            &mut create_session,
            &mut remove_session,
            &mut open_terminal,
            open_config,
        )
    }

    #[test]
    fn picker_switch_reroots_the_pane_at_the_focused_session_then_detaches() {
        // The pane opens on the selected worktree; the picker focuses another
        // session and returns Switch, so the loop re-roots there (the first
        // shell stays alive in the pool), then it detaches. An ArrowDown first
        // moves the cursor off the root row onto the first worktree.
        let dirs = RefCell::new(Vec::new());
        let calls = RefCell::new(0);
        let mut open = |home: &mut HomeState, dir: &Path, _agent: bool| {
            dirs.borrow_mut().push(dir.to_path_buf());
            let mut n = calls.borrow_mut();
            *n += 1;
            if *n == 1 {
                // The picker focuses the second worktree (row 2) before switching.
                home.focus_session(2);
                Ok(PaneExit::Switch)
            } else {
                Ok(PaneExit::Detach)
            }
        };
        assert!(matches!(
            run_pane_with_nav(
                vec![Ok(Key::ArrowDown)],
                "terminal",
                two_worktree_state(),
                &mut open
            )
            .unwrap(),
            Outcome::Back
        ));
        assert_eq!(
            *dirs.borrow(),
            vec![PathBuf::from("/r/main"), PathBuf::from("/r/feat")]
        );
    }

    #[test]
    fn picker_switch_to_the_root_row_reroots_at_the_workspace_root() {
        // Switching to the root row (which belongs to no session) re-roots the
        // pane at the workspace root rather than a worktree path.
        let dirs = RefCell::new(Vec::new());
        let calls = RefCell::new(0);
        let mut open = |home: &mut HomeState, dir: &Path, _agent: bool| {
            dirs.borrow_mut().push(dir.to_path_buf());
            let mut n = calls.borrow_mut();
            *n += 1;
            if *n == 1 {
                home.focus_session(0); // the root row
                Ok(PaneExit::Switch)
            } else {
                Ok(PaneExit::Detach)
            }
        };
        assert!(matches!(
            run_pane_with_nav(
                vec![Ok(Key::ArrowDown)],
                "terminal",
                two_worktree_state(),
                &mut open
            )
            .unwrap(),
            Outcome::Back
        ));
        assert_eq!(
            *dirs.borrow(),
            vec![PathBuf::from("/r/main"), PathBuf::from("/ws")]
        );
    }

    #[test]
    fn config_via_the_default_callback_returns_to_the_workspace_screen() {
        // Drives `:config` through the shared no-op `open_config` (returns false),
        // so the loop resumes the workspace screen and then leaves with Back.
        let mut open: fn(&Term) -> Result<bool> = noop_config;
        assert!(matches!(
            run_config(sample_state(), &mut open).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn config_opens_the_settings_screen() {
        let opened = RefCell::new(false);
        let mut open = |_: &Term| {
            *opened.borrow_mut() = true;
            Ok(false)
        };
        assert!(matches!(
            run_config(sample_state(), &mut open).unwrap(),
            Outcome::Back
        ));
        assert!(opened.into_inner());
    }

    #[test]
    fn quitting_from_config_quits_the_app() {
        // `open_config` returning true means the user quit the settings screen,
        // so the workspace screen propagates Quit immediately.
        let mut open = |_: &Term| Ok(true);
        assert!(matches!(
            run_config(sample_state(), &mut open).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn config_failure_is_propagated() {
        let mut open = |_: &Term| Err(anyhow::anyhow!("settings blew up"));
        let err = run_config(sample_state(), &mut open).unwrap_err();
        assert!(err.to_string().contains("settings blew up"));
    }
}
