use std::path::Path;

use anyhow::Result;
use console::Key;
use console::Term;

use crate::presentation::tui::screen::KeyReader;

use super::command::Effect;
use super::state::{HomeState, Mode, PaneExit, SessionOutcome};
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
/// worktree (or `workspace_root` when nothing is selected). `agent` does the
/// same but launches the configured AI agent CLI inside that shell. The loop
/// switches the right pane to terminal mode, then runs the embedded session via
/// `open_terminal` (its `bool` is `true` for `agent`, `false` for a bare
/// `terminal`) in a small switch loop: the callback returns a [`PaneExit`], and
/// [`PaneExit::SwitchNext`] / [`PaneExit::SwitchPrev`] re-root the pane at the
/// next / previous worktree without leaving it (the shell behind it stays
/// alive), while `Detach` / `Closed` switch the pane back. The PTY I/O,
/// rendering, the persistent shell pool, and agent-command resolution all live
/// in that injected callback.
#[allow(clippy::too_many_arguments)]
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut state: HomeState,
    workspace_root: &Path,
    persist: &mut dyn FnMut(&str),
    create_session: &mut dyn FnMut(&str) -> SessionOutcome,
    remove_session: &mut dyn FnMut(&str, bool) -> SessionOutcome,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
) -> Result<Outcome> {
    loop {
        term.move_cursor_to(0, 0)?;
        term.clear_screen()?;
        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &state);
        for line in &frame {
            term.write_line(line)?;
        }

        let key = match reader.read_key() {
            Ok(key) => key,
            // An interrupted read (e.g. a delivered signal) means quit.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
        };

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
                        effect @ (Effect::OpenTerminal | Effect::OpenAgent) => {
                            // Embed the shell in the right pane, rooted at the
                            // selected worktree (or the workspace root when
                            // nothing is selected). `agent` is the same shell
                            // with the AI agent CLI launched inside it. The pane
                            // is switched to terminal mode for the duration and
                            // back afterwards.
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
                            // Stay in the pane across session switches: a leader
                            // switch re-roots the shell at the next / previous
                            // worktree (the one just left keeps running), and we
                            // only leave on a detach, a close, or an error.
                            let outcome = loop {
                                match open_terminal(&mut state, &dir, agent) {
                                    Ok(PaneExit::SwitchNext) => match state.focus_next_worktree() {
                                        Some(next) => dir = next,
                                        None => break Ok(PaneExit::Detach),
                                    },
                                    Ok(PaneExit::SwitchPrev) => match state.focus_prev_worktree() {
                                        Some(prev) => dir = prev,
                                        None => break Ok(PaneExit::Detach),
                                    },
                                    other => break other,
                                }
                            };
                            state.show_log();
                            match outcome {
                                Ok(PaneExit::Detach) => {
                                    state.log_output(format!("{label} detached (still running) 🐰"))
                                }
                                Ok(_) => state
                                    .log_output(format!("{label} in {} closed.", dir.display())),
                                Err(e) => state.log_error(format!("{fail} failed: {e}")),
                            }
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

    fn run(keys: Vec<io::Result<Key>>, state: HomeState) -> Result<Outcome> {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let mut persist = |_: &str| {};
        let mut create_session: fn(&str) -> SessionOutcome = noop_create;
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        let mut open_terminal: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        event_loop(
            &term,
            &mut reader,
            state,
            Path::new("/ws"),
            &mut persist,
            &mut create_session,
            &mut remove_session,
            &mut open_terminal,
        )
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
        let mut recorded = Vec::new();
        let mut persist = |command: &str| recorded.push(command.to_string());
        let mut create_session: fn(&str) -> SessionOutcome = noop_create;
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        let mut open_terminal: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_state(),
            Path::new("/ws"),
            &mut persist,
            &mut create_session,
            &mut remove_session,
            &mut open_terminal,
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
        let mut persist = |_: &str| {};
        let mut created = Vec::new();
        let mut create_session = |name: &str| {
            created.push(name.to_string());
            outcome.clone()
        };
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        let mut open_terminal: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let result = event_loop(
            &term,
            &mut reader,
            state,
            Path::new("/ws"),
            &mut persist,
            &mut create_session,
            &mut remove_session,
            &mut open_terminal,
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
        let mut persist = |_: &str| {};
        let mut create_session: fn(&str) -> SessionOutcome = noop_create;
        let mut removed = Vec::new();
        let mut remove_session = |name: &str, force: bool| {
            removed.push((name.to_string(), force));
            ok_outcome()
        };
        let mut open_terminal: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_state(),
            Path::new("/ws"),
            &mut persist,
            &mut create_session,
            &mut remove_session,
            &mut open_terminal,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(removed, vec![("old".to_string(), true)]);
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
    /// and result handling are exercised.
    fn run_pane(
        command: &str,
        state: HomeState,
        open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    ) -> Result<Outcome> {
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed(command));
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar

        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let mut persist = |_: &str| {};
        let mut create_session: fn(&str) -> SessionOutcome = noop_create;
        let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
        event_loop(
            &term,
            &mut reader,
            state,
            Path::new("/ws"),
            &mut persist,
            &mut create_session,
            &mut remove_session,
            open_terminal,
        )
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
        // sample_state's selected worktree path is /repo/wt.
        assert!(matches!(
            run_pane("terminal", sample_state(), &mut open).unwrap(),
            Outcome::Back
        ));
        assert_eq!(opened.into_inner(), Some(PathBuf::from("/repo/wt")));
    }

    #[test]
    fn terminal_falls_back_to_the_workspace_root_without_selection() {
        let opened = RefCell::new(None);
        let mut open = |_home: &mut HomeState, dir: &Path, _agent: bool| {
            *opened.borrow_mut() = Some(dir.to_path_buf());
            Ok(PaneExit::Closed)
        };
        // No worktrees: the loop opens the shell in the workspace root (/ws).
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
        assert!(matches!(
            run_pane("agent", sample_state(), &mut open).unwrap(),
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

    #[test]
    fn leader_switch_next_reroots_the_pane_then_detaches() {
        // The pane opens on the selected worktree, a SwitchNext re-roots it at
        // the next one (the first shell stays alive in the pool), then it
        // detaches.
        let dirs = RefCell::new(Vec::new());
        let exits = RefCell::new(vec![PaneExit::SwitchNext, PaneExit::Detach]);
        let mut open = |_home: &mut HomeState, dir: &Path, _agent: bool| {
            dirs.borrow_mut().push(dir.to_path_buf());
            Ok(exits.borrow_mut().remove(0))
        };
        assert!(matches!(
            run_pane("terminal", two_worktree_state(), &mut open).unwrap(),
            Outcome::Back
        ));
        assert_eq!(
            *dirs.borrow(),
            vec![PathBuf::from("/r/main"), PathBuf::from("/r/feat")]
        );
    }

    #[test]
    fn leader_switch_prev_reroots_the_pane_then_detaches() {
        // SwitchPrev from the top worktree wraps to the bottom one.
        let dirs = RefCell::new(Vec::new());
        let exits = RefCell::new(vec![PaneExit::SwitchPrev, PaneExit::Detach]);
        let mut open = |_home: &mut HomeState, dir: &Path, _agent: bool| {
            dirs.borrow_mut().push(dir.to_path_buf());
            Ok(exits.borrow_mut().remove(0))
        };
        assert!(matches!(
            run_pane("terminal", two_worktree_state(), &mut open).unwrap(),
            Outcome::Back
        ));
        assert_eq!(
            *dirs.borrow(),
            vec![PathBuf::from("/r/main"), PathBuf::from("/r/feat")]
        );
    }

    #[test]
    fn leader_switch_with_no_worktrees_detaches() {
        // With an empty list a switch has nowhere to go, so the pane detaches
        // after the single opening call (both directions).
        for exit in [PaneExit::SwitchNext, PaneExit::SwitchPrev] {
            let calls = RefCell::new(0);
            let mut open = |_home: &mut HomeState, _dir: &Path, _agent: bool| {
                *calls.borrow_mut() += 1;
                Ok(exit)
            };
            let state = HomeState::new("usagi", Vec::new(), None);
            assert!(matches!(
                run_pane("terminal", state, &mut open).unwrap(),
                Outcome::Back
            ));
            assert_eq!(*calls.borrow(), 1);
        }
    }
}
