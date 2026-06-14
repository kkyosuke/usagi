use anyhow::Result;
use console::Key;
use console::Term;

use crate::presentation::tui::screen::KeyReader;

use super::command::Effect;
use super::state::{HomeState, Mode};
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
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut state: HomeState,
    persist: &mut dyn FnMut(&str),
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
                    if let Effect::Quit = submission.effect {
                        return Ok(Outcome::Quit);
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
    use super::*;
    use crate::domain::workspace_state::{BranchStatus, WorktreeState};
    use chrono::Utc;
    use std::collections::VecDeque;
    use std::io;
    use std::path::PathBuf;

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

    fn run(keys: Vec<io::Result<Key>>, state: HomeState) -> Result<Outcome> {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let mut persist = |_: &str| {};
        event_loop(&term, &mut reader, state, &mut persist)
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
        let outcome = event_loop(&term, &mut reader, sample_state(), &mut persist).unwrap();
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
}
