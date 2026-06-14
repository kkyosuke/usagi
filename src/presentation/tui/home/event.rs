use std::path::{Path, PathBuf};

use anyhow::Result;
use console::Key;
use console::Term;

use crate::domain::session::Session;
use crate::domain::workspace_state::{BranchStatus, WorktreeState};
use crate::presentation::tui::screen::KeyReader;

use super::command::{Effect, SessionRequest};
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

/// Side-effectful operations the home screen's commands delegate to, injected so
/// the loop stays testable: production (in [`super::run`]) wires these to the
/// real session usecase and shell launcher; tests pass stubs.
pub struct HomeHandlers<'a> {
    /// Workspace root, used as the terminal's working directory when no worktree
    /// is selected.
    pub workspace_root: PathBuf,
    /// Create a session with the given name, returning the created session.
    pub create_session: &'a mut dyn FnMut(&str) -> Result<Session>,
    /// List the workspace's sessions.
    pub list_sessions: &'a mut dyn FnMut() -> Result<Vec<Session>>,
    /// Open an interactive shell rooted at the given directory, returning once
    /// the user exits it.
    pub open_terminal: &'a mut dyn FnMut(&Path) -> Result<()>,
}

/// Convert a session's worktrees into sidebar rows so they appear immediately
/// after `session new` (and on reload). Head is left blank (a later `usagi
/// status` fills it in) and the branch is freshly created, so the row is
/// `Local`.
pub(crate) fn session_rows(session: &Session) -> Vec<WorktreeState> {
    session
        .repos
        .iter()
        .map(|repo| WorktreeState {
            branch: Some(repo.branch.clone()),
            path: repo.path.clone(),
            head: String::new(),
            primary: false,
            upstream: None,
            status: BranchStatus::Local,
            updated_at: session.created_at,
        })
        .collect()
}

/// Carry out a command's side-effect request, appending its result to the log
/// and updating the worktree list. `None`, `Clear`, and `Quit` are handled by
/// the loop itself and ignored here.
fn handle_effect(state: &mut HomeState, effect: Effect, handlers: &mut HomeHandlers) {
    match effect {
        Effect::Session(SessionRequest::New(name)) => match (handlers.create_session)(&name) {
            Ok(session) => {
                state.log_output(format!(
                    "Created session \"{}\" at {}",
                    session.name,
                    session.root.display()
                ));
                state.add_worktrees(session_rows(&session));
            }
            Err(e) => state.log_error(format!("session new failed: {e}")),
        },
        Effect::Session(SessionRequest::List) => match (handlers.list_sessions)() {
            Ok(sessions) if sessions.is_empty() => state.log_output("No sessions yet."),
            Ok(sessions) => {
                for session in sessions {
                    state.log_output(format!(
                        "  {}  ({} worktree(s))",
                        session.name,
                        session.repos.len()
                    ));
                }
            }
            Err(e) => state.log_error(format!("session list failed: {e}")),
        },
        Effect::OpenTerminal => {
            let dir = state
                .selected_worktree_path()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| handlers.workspace_root.clone());
            match (handlers.open_terminal)(&dir) {
                Ok(()) => state.log_output(format!("Terminal in {} closed.", dir.display())),
                Err(e) => state.log_error(format!("terminal failed: {e}")),
            }
        }
        Effect::None | Effect::Clear | Effect::Quit => {}
    }
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
/// it to the workspace's `history.json`; tests pass a no-op. Commands with
/// filesystem / process side effects (`session`, `terminal`) are carried out
/// through `handlers`.
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut state: HomeState,
    persist: &mut dyn FnMut(&str),
    handlers: &mut HomeHandlers,
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
                    handle_effect(&mut state, submission.effect, handlers);
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
    use crate::domain::session::SessionRepo;
    use crate::domain::workspace_state::{BranchStatus, WorktreeState};
    use crate::presentation::tui::home::state::LineKind;
    use chrono::Utc;
    use std::cell::RefCell;
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

    /// A session with a single worktree, for stub handlers.
    fn sample_session() -> Session {
        Session::new(
            "feature-x",
            "/ws/.usagi/worktree/feature-x",
            vec![SessionRepo {
                relative: PathBuf::new(),
                path: PathBuf::from("/ws/.usagi/worktree/feature-x"),
                branch: "feature-x".to_string(),
            }],
        )
    }

    fn run(keys: Vec<io::Result<Key>>, state: HomeState) -> Result<Outcome> {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let mut persist = |_: &str| {};
        let mut create = ok_create;
        let mut list = ok_list;
        let mut open = ok_open;
        let mut handlers = HomeHandlers {
            workspace_root: PathBuf::from("/ws"),
            create_session: &mut create,
            list_sessions: &mut list,
            open_terminal: &mut open,
        };
        event_loop(&term, &mut reader, state, &mut persist, &mut handlers)
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
        let mut create = ok_create;
        let mut list = ok_list;
        let mut open = ok_open;
        let mut handlers = HomeHandlers {
            workspace_root: PathBuf::from("/ws"),
            create_session: &mut create,
            list_sessions: &mut list,
            open_terminal: &mut open,
        };
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_state(),
            &mut persist,
            &mut handlers,
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

    // --- effect handling ---------------------------------------------------

    /// Run `effect` against `state` with the given handlers. Slots irrelevant to
    /// the effect under test are filled with the no-op `ok_*` stubs.
    fn apply(
        state: &mut HomeState,
        effect: Effect,
        create: &mut dyn FnMut(&str) -> Result<Session>,
        list: &mut dyn FnMut() -> Result<Vec<Session>>,
        open: &mut dyn FnMut(&Path) -> Result<()>,
    ) {
        let mut handlers = HomeHandlers {
            workspace_root: PathBuf::from("/ws"),
            create_session: create,
            list_sessions: list,
            open_terminal: open,
        };
        handle_effect(state, effect, &mut handlers);
    }

    /// Shared no-op handlers, used both as `run`'s defaults and as fillers for
    /// the slots a given effect does not exercise.
    fn ok_create(_: &str) -> Result<Session> {
        Ok(sample_session())
    }
    fn ok_list() -> Result<Vec<Session>> {
        Ok(Vec::new())
    }
    fn ok_open(_: &Path) -> Result<()> {
        Ok(())
    }

    #[test]
    fn session_rows_converts_repos_to_local_worktree_rows() {
        let rows = session_rows(&sample_session());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].branch.as_deref(), Some("feature-x"));
        assert_eq!(rows[0].status, BranchStatus::Local);
        assert!(!rows[0].primary);
        assert!(rows[0].head.is_empty());
    }

    #[test]
    fn handle_session_new_logs_and_adds_worktrees() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        let mut create = |name: &str| -> Result<Session> {
            assert_eq!(name, "feature-x");
            Ok(sample_session())
        };
        apply(
            &mut state,
            Effect::Session(SessionRequest::New("feature-x".to_string())),
            &mut create,
            &mut ok_list,
            &mut ok_open,
        );
        assert!(state.log().last().unwrap().text.contains("Created session"));
        assert!(state
            .list()
            .worktrees()
            .iter()
            .any(|w| w.path == Path::new("/ws/.usagi/worktree/feature-x")));
    }

    #[test]
    fn handle_session_new_reports_failure() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        let mut create = |_: &str| -> Result<Session> { Err(anyhow::anyhow!("boom")) };
        apply(
            &mut state,
            Effect::Session(SessionRequest::New("x".to_string())),
            &mut create,
            &mut ok_list,
            &mut ok_open,
        );
        let last = state.log().last().unwrap();
        assert_eq!(last.kind, LineKind::Error);
        assert!(last.text.contains("session new failed"));
    }

    #[test]
    fn handle_session_list_reports_empty_and_populated() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        // `ok_list` yields no sessions, exercising the empty branch.
        apply(
            &mut state,
            Effect::Session(SessionRequest::List),
            &mut ok_create,
            &mut ok_list,
            &mut ok_open,
        );
        assert!(state.log().last().unwrap().text.contains("No sessions yet"));

        let mut populated = || -> Result<Vec<Session>> { Ok(vec![sample_session()]) };
        apply(
            &mut state,
            Effect::Session(SessionRequest::List),
            &mut ok_create,
            &mut populated,
            &mut ok_open,
        );
        assert!(state.log().last().unwrap().text.contains("feature-x"));
    }

    #[test]
    fn handle_session_list_reports_failure() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        let mut failing = || -> Result<Vec<Session>> { Err(anyhow::anyhow!("nope")) };
        apply(
            &mut state,
            Effect::Session(SessionRequest::List),
            &mut ok_create,
            &mut failing,
            &mut ok_open,
        );
        assert!(state
            .log()
            .last()
            .unwrap()
            .text
            .contains("session list failed"));
    }

    #[test]
    fn handle_terminal_uses_the_selected_worktree() {
        let mut state = sample_state(); // selected worktree path is /repo/wt
        let opened = RefCell::new(None);
        let mut open = |dir: &Path| -> Result<()> {
            *opened.borrow_mut() = Some(dir.to_path_buf());
            Ok(())
        };
        apply(
            &mut state,
            Effect::OpenTerminal,
            &mut ok_create,
            &mut ok_list,
            &mut open,
        );
        assert_eq!(opened.into_inner(), Some(PathBuf::from("/repo/wt")));
        assert!(state.log().last().unwrap().text.contains("closed"));
    }

    #[test]
    fn handle_terminal_falls_back_to_the_workspace_root() {
        let mut state = HomeState::new("usagi", Vec::new(), None); // no selection
        let opened = RefCell::new(None);
        let mut open = |dir: &Path| -> Result<()> {
            *opened.borrow_mut() = Some(dir.to_path_buf());
            Ok(())
        };
        apply(
            &mut state,
            Effect::OpenTerminal,
            &mut ok_create,
            &mut ok_list,
            &mut open,
        );
        assert_eq!(opened.into_inner(), Some(PathBuf::from("/ws")));
    }

    #[test]
    fn handle_terminal_reports_failure() {
        let mut state = sample_state();
        let mut open = |_: &Path| -> Result<()> { Err(anyhow::anyhow!("no shell")) };
        apply(
            &mut state,
            Effect::OpenTerminal,
            &mut ok_create,
            &mut ok_list,
            &mut open,
        );
        assert!(state.log().last().unwrap().text.contains("terminal failed"));
    }

    #[test]
    fn handle_effect_ignores_loop_handled_effects() {
        let mut state = HomeState::new("usagi", Vec::new(), None);
        let before = state.log().len();
        apply(
            &mut state,
            Effect::None,
            &mut ok_create,
            &mut ok_list,
            &mut ok_open,
        );
        assert_eq!(state.log().len(), before);
    }

    /// Drives the command-mode Enter path so the loop's `handle_effect` routing
    /// is exercised for each effect (and `run`'s default handlers are invoked).
    fn run_command(command: &str) -> Outcome {
        let mut keys = vec![Ok(Key::Char(':'))];
        keys.extend(typed(command));
        keys.push(Ok(Key::Enter));
        keys.push(Ok(Key::Escape)); // cancel command mode
        keys.push(Ok(Key::Escape)); // leave sidebar
        run(keys, sample_state()).unwrap()
    }

    #[test]
    fn loop_routes_session_new() {
        assert!(matches!(run_command("session new x"), Outcome::Back));
    }

    #[test]
    fn loop_routes_session_list() {
        assert!(matches!(run_command("session list"), Outcome::Back));
    }

    #[test]
    fn loop_routes_terminal() {
        assert!(matches!(run_command("terminal"), Outcome::Back));
    }
}
