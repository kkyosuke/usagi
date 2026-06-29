use std::path::Path;

use anyhow::Result;
use console::Key;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::presentation::tui::home;
use crate::presentation::tui::install_task;
use crate::presentation::tui::io::screen::{animated_read, FramePainter, KeyReader};

use super::state::{Mode, ProjectList};
use super::ui;

/// What the user chose to do on the project selection screen.
#[derive(Debug)]
pub enum Outcome {
    /// Return to the previous screen without opening a project.
    Back,
    /// The user asked to quit the application entirely.
    Quit,
}

/// Launches the home screen for a chosen workspace and returns the user's
/// choice.
///
/// Taking this as a parameter lets the event loop be tested without a real
/// terminal: production wires it to [`home::run`], tests pass a stub.
///
/// The slice holds every workspace the user chose to open together: one for the
/// ordinary single-workspace home, several for 統合(unite) mode.
pub type OpenHome<'a> = dyn FnMut(&Term, &[Workspace]) -> Result<home::Outcome> + 'a;

/// The workspace-store side effects the selection screen needs when a chosen
/// workspace's directory is gone: deciding whether it still exists on disk, and
/// dropping the stale entry from the registry when the user confirms.
///
/// Taking these as injected closures keeps [`event_loop`] free of real IO so it
/// can be driven in tests: production wires `exists` to [`Path::exists`] and
/// `remove` to the workspace usecase, while tests pass stubs.
pub struct ListActions<'a> {
    /// Whether the workspace at this path still exists on disk.
    pub exists: &'a mut dyn FnMut(&Path) -> bool,
    /// Drop the named workspace from the registry.
    pub remove: &'a mut dyn FnMut(&str) -> Result<()>,
}

/// Runs the project selection screen against the given terminal and key source
/// until the user goes back or quits. Assumes the alternate screen is already
/// active (it is owned by the caller).
///
/// Selecting a project opens the home screen for that workspace; returning from
/// it leaves the user back on this list.
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut list: ProjectList,
    initial_notice: Option<String>,
    open_home: &mut OpenHome,
    actions: &mut ListActions,
) -> Result<Outcome> {
    let mut notice = initial_notice;
    let mut painter = FramePainter::new();
    // When `Some`, the selected workspace's directory is gone and we are showing
    // the "remove it from the list?" prompt for the named workspace instead of
    // the list. The confirmation modal owns the screen until it is answered.
    let mut confirming: Option<String> = None;

    loop {
        let (height, width) = term.size();
        let frame = match &confirming {
            Some(name) => ui::confirm_remove_frame(height as usize, width as usize, name),
            None => ui::render_frame(
                height as usize,
                width as usize,
                &list,
                notice.as_deref(),
                chrono::Utc::now(),
            ),
        };
        painter.paint(term, frame)?;

        let key = match animated_read(reader, term, &mut painter, &install_task::handle()) {
            Ok(key) => key,
            // An interrupted read (e.g. a delivered signal) means quit.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
        };

        // While the stale-workspace prompt is up it captures every key.
        if let Some(name) = confirming.clone() {
            match key {
                Key::Char('y') | Key::Char('Y') | Key::Enter => {
                    notice = Some(match (actions.remove)(&name) {
                        Ok(()) => {
                            list.remove_selected();
                            if list.mode() == Mode::Unite && list.checked_count() == 0 {
                                list.enter_unite();
                            }
                            format!("Removed \"{name}\".")
                        }
                        Err(e) => format!("Failed to remove \"{name}\": {e}"),
                    });
                    confirming = None;
                    painter.reset();
                }
                Key::Char('n') | Key::Char('N') | Key::Escape => {
                    confirming = None;
                    painter.reset();
                }
                // The global quit chords still take effect from the prompt.
                Key::CtrlC | Key::Char('\u{0011}') => return Ok(Outcome::Quit),
                _ => {}
            }
            continue;
        }

        match key {
            Key::ArrowUp | Key::Char('k') => {
                list.move_up();
                notice = None;
            }
            Key::ArrowDown | Key::Char('j') => {
                list.move_down();
                notice = None;
            }
            // In the default single-open picker, `Space` enters the explicit
            // unite multi-select screen and seeds it with the cursor row. Once in
            // unite, `Space` toggles membership. Keeping this as a distinct mode
            // makes `Enter` predictable in the default screen: it is always just
            // the cursor row there, no matter what had been checked before.
            Key::Char(' ') => match list.mode() {
                Mode::Single => list.enter_unite(),
                Mode::Unite => {
                    let would_empty_current_set =
                        list.is_checked(list.selected_index()) && list.checked_count() == 1;
                    if would_empty_current_set {
                        notice = Some("Select at least one workspace for unite.".to_string());
                    } else {
                        list.toggle_checked();
                        notice = None;
                    }
                }
            },
            Key::Char('u') | Key::Char('U') => {
                list.enter_unite();
                notice = None;
            }
            Key::Enter => {
                // The workspaces to open are mode-dependent: single mode opens the
                // cursor row, unite mode opens the checked set. Owned clones, so
                // the list can be mutated (promote / focus) afterwards without a
                // borrow conflict.
                let chosen = list.chosen();
                if let Some(first) = chosen.first() {
                    // A workspace whose directory has since been deleted cannot be
                    // opened; offer to drop the first stale entry instead of
                    // launching on a missing path. Land the cursor on it so the
                    // removal prompt acts on the right row.
                    if let Some(missing) = chosen.iter().find(|w| !(actions.exists)(&w.path)) {
                        list.focus_name(&missing.name);
                        confirming = Some(missing.name.clone());
                        painter.reset();
                        continue;
                    }
                    // Opening is wired by the caller: it hides the list, plays the
                    // mascot animation while loading the workspaces off-thread, then
                    // shows the home screen (切替). See [`super::run`].
                    match open_home(term, &chosen)? {
                        // The home screen drew over the list; force a full repaint of
                        // it on the next pass. The (first) just-opened project moves
                        // to the top so the list reflects most-recently-opened order
                        // without a reload.
                        home::Outcome::Back => {
                            list.focus_name(&first.name);
                            list.promote_selected();
                            list.exit_unite();
                            notice = None;
                            painter.reset();
                        }
                        home::Outcome::Quit => return Ok(Outcome::Quit),
                    }
                }
            }
            Key::Escape if list.mode() == Mode::Unite => {
                list.exit_unite();
                notice = None;
                painter.reset();
            }
            Key::Char('q') | Key::Escape => return Ok(Outcome::Back),
            // `Ctrl-C` / `Ctrl-Q` (the bare `0x11`) quit the app from here too.
            Key::CtrlC | Key::Char('\u{0011}') => return Ok(Outcome::Quit),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace::Workspace;
    use std::collections::VecDeque;
    use std::io;

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

    // Existence / removal stubs for the stale-workspace prompt. Defined once as
    // named functions (rather than per-test closure literals) so each body is
    // covered by whichever test does exercise it — a test that passes one without
    // calling it (e.g. cancel/quit before confirming) no longer leaves a dead
    // closure behind.
    fn exists_true(_path: &Path) -> bool {
        true
    }
    fn exists_false(_path: &Path) -> bool {
        false
    }
    fn remove_ok(_name: &str) -> Result<()> {
        Ok(())
    }
    fn remove_err(_name: &str) -> Result<()> {
        Err(anyhow::anyhow!("disk on fire"))
    }

    // Home-screen launchers used as stubs; each is exercised by a test below.
    fn home_back(_t: &Term, _w: &[Workspace]) -> Result<home::Outcome> {
        Ok(home::Outcome::Back)
    }
    fn home_quit(_t: &Term, _w: &[Workspace]) -> Result<home::Outcome> {
        Ok(home::Outcome::Quit)
    }
    fn home_err(_t: &Term, _w: &[Workspace]) -> Result<home::Outcome> {
        Err(anyhow::anyhow!("home screen blew up"))
    }

    fn sample_list() -> ProjectList {
        use crate::usecase::workspace::WorkspaceOverview;
        let overview = |name: &str, path: &str| WorkspaceOverview {
            workspace: Workspace::new(name, path),
            session_count: 0,
            open_issue_count: 0,
        };
        ProjectList::new(vec![
            overview("alpha", "/p/alpha"),
            overview("beta", "/p/beta"),
        ])
    }

    /// [`ListActions`] reporting every workspace as present with a no-op remove —
    /// the default for tests not exercising the stale-workspace prompt. Reporting
    /// every path as present keeps the open-flow tests opening the home screen
    /// rather than tripping the confirmation.
    fn present_actions<'a>(
        exists: &'a mut fn(&Path) -> bool,
        remove: &'a mut fn(&str) -> Result<()>,
    ) -> ListActions<'a> {
        ListActions { exists, remove }
    }

    fn run(keys: Vec<io::Result<Key>>, list: ProjectList) -> Result<Outcome> {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let mut exists: fn(&Path) -> bool = exists_true;
        let mut remove: fn(&str) -> Result<()> = remove_ok;
        let mut actions = present_actions(&mut exists, &mut remove);
        event_loop(&term, &mut reader, list, None, &mut home_back, &mut actions)
    }

    #[test]
    fn escape_returns_back() {
        assert!(matches!(
            run(vec![Ok(Key::Escape)], sample_list()).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn q_returns_back() {
        assert!(matches!(
            run(vec![Ok(Key::Char('q'))], sample_list()).unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn ctrl_c_returns_quit() {
        assert!(matches!(
            run(vec![Ok(Key::CtrlC)], sample_list()).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn ctrl_q_returns_quit() {
        // `Ctrl-Q` is the global quit chord (the bare `0x11` `console` reports).
        assert!(matches!(
            run(vec![Ok(Key::Char('\u{0011}'))], sample_list()).unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn navigation_keys_move_the_cursor_then_back() {
        // Exercises every navigation arm (arrows + j/k aliases) and the
        // ignored-key arm, then leaves via Escape.
        let keys = vec![
            Ok(Key::ArrowDown),
            Ok(Key::ArrowUp),
            Ok(Key::Char('j')),
            Ok(Key::Char('k')),
            Ok(Key::Home), // ignored (the `_` arm)
            Ok(Key::Escape),
        ];
        assert!(matches!(run(keys, sample_list()).unwrap(), Outcome::Back));
    }

    #[test]
    fn enter_opens_home_then_returns_to_the_list() {
        // Enter opens the home screen (stub returns Back), then Escape leaves.
        let keys = vec![Ok(Key::Enter), Ok(Key::Escape)];
        assert!(matches!(run(keys, sample_list()).unwrap(), Outcome::Back));
    }

    #[test]
    fn space_checks_entries_and_enter_opens_them_together() {
        // Space checks "alpha", move down + Space checks "beta", Enter opens both
        // in one go (统合/unite). The capturing stub records what it was handed.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::Char(' ')),
            Ok(Key::ArrowDown),
            Ok(Key::Char(' ')),
            Ok(Key::Enter),
            Ok(Key::Escape),
        ]);
        let mut opened: Vec<Vec<String>> = Vec::new();
        let mut open = |_t: &Term, ws: &[Workspace]| {
            opened.push(ws.iter().map(|w| w.name.clone()).collect());
            Ok(home::Outcome::Back)
        };
        let mut exists: fn(&Path) -> bool = exists_true;
        let mut remove: fn(&str) -> Result<()> = remove_ok;
        let mut actions = present_actions(&mut exists, &mut remove);
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_list(),
            None,
            &mut open,
            &mut actions,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(opened, vec![vec!["alpha".to_string(), "beta".to_string()]]);
    }

    #[test]
    fn u_enters_unite_and_enter_opens_the_checked_set() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::ArrowDown), // cursor on "beta"
            Ok(Key::Char('u')),
            Ok(Key::Enter),
            Ok(Key::Escape),
        ]);
        let mut opened: Vec<Vec<String>> = Vec::new();
        let mut open = |_t: &Term, ws: &[Workspace]| {
            opened.push(ws.iter().map(|w| w.name.clone()).collect());
            Ok(home::Outcome::Back)
        };
        let mut exists: fn(&Path) -> bool = exists_true;
        let mut remove: fn(&str) -> Result<()> = remove_ok;
        let mut actions = present_actions(&mut exists, &mut remove);
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_list(),
            None,
            &mut open,
            &mut actions,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(opened, vec![vec!["beta".to_string()]]);
    }

    #[test]
    fn escape_in_unite_returns_to_single_before_backing_out() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::Char(' ')), // enter unite, seed "alpha"
            Ok(Key::ArrowDown),
            Ok(Key::Escape), // cancel unite mode, stay on the Open screen
            Ok(Key::Enter),  // single-open "beta", not checked "alpha"
            Ok(Key::Escape),
        ]);
        let mut opened: Vec<Vec<String>> = Vec::new();
        let mut open = |_t: &Term, ws: &[Workspace]| {
            opened.push(ws.iter().map(|w| w.name.clone()).collect());
            Ok(home::Outcome::Back)
        };
        let mut exists: fn(&Path) -> bool = exists_true;
        let mut remove: fn(&str) -> Result<()> = remove_ok;
        let mut actions = present_actions(&mut exists, &mut remove);
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_list(),
            None,
            &mut open,
            &mut actions,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(opened, vec![vec!["beta".to_string()]]);
    }

    #[test]
    fn unite_keeps_at_least_one_workspace_checked() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::Char(' ')), // enter unite, seed "alpha"
            Ok(Key::Char(' ')), // trying to uncheck the only row is ignored
            Ok(Key::Enter),
            Ok(Key::Escape),
        ]);
        let mut opened: Vec<Vec<String>> = Vec::new();
        let mut open = |_t: &Term, ws: &[Workspace]| {
            opened.push(ws.iter().map(|w| w.name.clone()).collect());
            Ok(home::Outcome::Back)
        };
        let mut exists: fn(&Path) -> bool = exists_true;
        let mut remove: fn(&str) -> Result<()> = remove_ok;
        let mut actions = present_actions(&mut exists, &mut remove);
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_list(),
            None,
            &mut open,
            &mut actions,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(opened, vec![vec!["alpha".to_string()]]);
    }

    #[test]
    fn enter_with_nothing_checked_opens_just_the_cursor_workspace() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Enter), Ok(Key::Escape)]);
        let mut opened: Vec<Vec<String>> = Vec::new();
        let mut open = |_t: &Term, ws: &[Workspace]| {
            opened.push(ws.iter().map(|w| w.name.clone()).collect());
            Ok(home::Outcome::Back)
        };
        let mut exists: fn(&Path) -> bool = exists_true;
        let mut remove: fn(&str) -> Result<()> = remove_ok;
        let mut actions = present_actions(&mut exists, &mut remove);
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_list(),
            None,
            &mut open,
            &mut actions,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(opened, vec![vec!["alpha".to_string()]]);
    }

    #[test]
    fn home_quit_propagates_as_quit() {
        // Opening the home screen and quitting from it quits the whole app.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Enter)]);
        let mut exists: fn(&Path) -> bool = exists_true;
        let mut remove: fn(&str) -> Result<()> = remove_ok;
        let mut actions = present_actions(&mut exists, &mut remove);
        let outcome = event_loop(
            &term,
            &mut reader,
            sample_list(),
            None,
            &mut home_quit,
            &mut actions,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Quit));
    }

    #[test]
    fn home_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Enter)]);
        let mut exists: fn(&Path) -> bool = exists_true;
        let mut remove: fn(&str) -> Result<()> = remove_ok;
        let mut actions = present_actions(&mut exists, &mut remove);
        let err = event_loop(
            &term,
            &mut reader,
            sample_list(),
            None,
            &mut home_err,
            &mut actions,
        )
        .unwrap_err();
        assert!(err.to_string().contains("home screen blew up"));
    }

    #[test]
    fn enter_on_empty_list_does_nothing() {
        // With no workspaces there is nothing to select; Enter must not open the
        // home screen (the erroring stub would surface if it were called).
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Enter), Ok(Key::Escape)]);
        let mut exists: fn(&Path) -> bool = exists_true;
        let mut remove: fn(&str) -> Result<()> = remove_ok;
        let mut actions = present_actions(&mut exists, &mut remove);
        let outcome = event_loop(
            &term,
            &mut reader,
            ProjectList::new(Vec::new()),
            None,
            &mut home_err,
            &mut actions,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
    }

    #[test]
    fn initial_notice_is_displayed() {
        // A load-error notice passed in is rendered on the first frame.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Escape)]);
        let mut exists: fn(&Path) -> bool = exists_true;
        let mut remove: fn(&str) -> Result<()> = remove_ok;
        let mut actions = present_actions(&mut exists, &mut remove);
        let outcome = event_loop(
            &term,
            &mut reader,
            ProjectList::new(Vec::new()),
            Some("Failed to load projects: boom".to_string()),
            &mut home_back,
            &mut actions,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
    }

    /// Drives the event loop with the given keys against a list whose workspaces
    /// are all reported as missing, capturing the names passed to `remove`.
    /// Returns the outcome and those names.
    fn run_missing(
        keys: Vec<io::Result<Key>>,
        list: ProjectList,
        remove_result: fn(&str) -> Result<()>,
    ) -> (Outcome, Vec<String>) {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        let mut removed: Vec<String> = Vec::new();
        let mut exists: fn(&Path) -> bool = exists_false;
        let mut remove = |name: &str| {
            removed.push(name.to_string());
            remove_result(name)
        };
        let mut actions = ListActions {
            exists: &mut exists,
            remove: &mut remove,
        };
        let outcome =
            event_loop(&term, &mut reader, list, None, &mut home_err, &mut actions).unwrap();
        (outcome, removed)
    }

    #[test]
    fn selecting_a_missing_workspace_prompts_then_removes_on_confirm() {
        // Enter on a missing workspace opens the prompt; `y` removes it (home_err
        // would surface if the home screen were opened instead), then Escape leaves.
        let (outcome, removed) = run_missing(
            vec![Ok(Key::Enter), Ok(Key::Char('y')), Ok(Key::Escape)],
            sample_list(),
            remove_ok,
        );
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(removed, vec!["alpha".to_string()]);
    }

    #[test]
    fn removing_the_only_checked_missing_workspace_reseeds_unite_selection() {
        // Space enters unite and checks "alpha"; removing that stale row would
        // otherwise leave unite with an empty checked set. The event loop reseeds
        // the next row, then the two Esc presses leave unite and the Open screen.
        let (outcome, removed) = run_missing(
            vec![
                Ok(Key::Char(' ')),
                Ok(Key::Enter),
                Ok(Key::Char('y')),
                Ok(Key::Escape),
                Ok(Key::Escape),
            ],
            sample_list(),
            remove_ok,
        );
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(removed, vec!["alpha".to_string()]);
    }

    #[test]
    fn confirming_a_missing_workspace_with_enter_also_removes_it() {
        // Enter answers the prompt the same as `y`.
        let (_outcome, removed) = run_missing(
            vec![Ok(Key::Enter), Ok(Key::Enter), Ok(Key::Escape)],
            sample_list(),
            remove_ok,
        );
        assert_eq!(removed, vec!["alpha".to_string()]);
    }

    #[test]
    fn cancelling_the_prompt_keeps_the_workspace() {
        // `n` dismisses the prompt without removing; a second Enter re-opens it,
        // and Escape from the prompt also cancels.
        let (outcome, removed) = run_missing(
            vec![
                Ok(Key::Enter),
                Ok(Key::Char('n')),
                Ok(Key::Enter),
                Ok(Key::Escape),
                Ok(Key::Escape),
            ],
            sample_list(),
            remove_ok,
        );
        assert!(matches!(outcome, Outcome::Back));
        assert!(removed.is_empty());
    }

    #[test]
    fn a_failed_removal_is_reported_and_the_prompt_closes() {
        // When the usecase removal fails the entry stays but the prompt closes;
        // the loop keeps running (Escape leaves).
        let (outcome, removed) = run_missing(
            vec![Ok(Key::Enter), Ok(Key::Char('Y')), Ok(Key::Escape)],
            sample_list(),
            remove_err,
        );
        assert!(matches!(outcome, Outcome::Back));
        assert_eq!(removed, vec!["alpha".to_string()]);
    }

    #[test]
    fn the_prompt_ignores_unrelated_keys() {
        // An unmapped key inside the prompt is a no-op (the `_` arm); `N` then
        // cancels and Escape leaves.
        let (outcome, removed) = run_missing(
            vec![
                Ok(Key::Enter),
                Ok(Key::Char('z')),
                Ok(Key::Char('N')),
                Ok(Key::Escape),
            ],
            sample_list(),
            remove_ok,
        );
        assert!(matches!(outcome, Outcome::Back));
        assert!(removed.is_empty());
    }

    #[test]
    fn ctrl_c_quits_from_the_prompt() {
        let (outcome, removed) = run_missing(
            vec![Ok(Key::Enter), Ok(Key::CtrlC)],
            sample_list(),
            remove_ok,
        );
        assert!(matches!(outcome, Outcome::Quit));
        assert!(removed.is_empty());
    }

    #[test]
    fn interrupted_read_returns_quit() {
        let keys = vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))];
        assert!(matches!(run(keys, sample_list()).unwrap(), Outcome::Quit));
    }

    #[test]
    fn unexpected_read_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::other("boom"))]);
        let mut exists: fn(&Path) -> bool = exists_true;
        let mut remove: fn(&str) -> Result<()> = remove_ok;
        let mut actions = present_actions(&mut exists, &mut remove);
        let err = event_loop(
            &term,
            &mut reader,
            sample_list(),
            None,
            &mut home_back,
            &mut actions,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }

    #[test]
    fn the_ctrl_q_chord_quits_from_the_prompt() {
        let (outcome, removed) = run_missing(
            vec![Ok(Key::Enter), Ok(Key::Char('\u{0011}'))],
            sample_list(),
            remove_ok,
        );
        assert!(matches!(outcome, Outcome::Quit));
        assert!(removed.is_empty());
    }
}
