use std::io;

use anyhow::Result;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::presentation::tui::install_task;
use crate::presentation::tui::io::screen::{animated_read, FramePainter, KeyReader};
use crate::usecase::workspace::WorkspaceOverview;

use super::menu::{Action, Menu};
use super::ui;

/// What the user chose on the welcome menu.
///
/// The welcome screen only reports the chosen action; deciding what each action
/// does (opening a sub-screen, creating a project, …) is the orchestrator's job
/// in [`crate::presentation::tui::app`].
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Open the project selection screen.
    OpenProjects,
    /// Open the New Project screen.
    NewProject,
    /// Open the Config screen.
    Configure,
    /// Open a recent workspace directly from the welcome screen.
    RecentProject(Workspace),
    /// Leave the welcome screen (quit the application).
    Quit,
}

/// Runs the welcome menu against the given terminal and key source until the
/// user picks an action (or an unrecoverable read error occurs). Assumes the
/// alternate screen is already active (it is owned by the orchestrator).
///
/// `initial_notice` seeds the notice line, e.g. an error carried back from a
/// failed project creation; navigating clears it.
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    recent_overviews: Vec<WorkspaceOverview>,
    initial_notice: Option<String>,
) -> Result<Outcome> {
    let mut menu = Menu::new(recent_overviews.clone());
    menu.set_notice(initial_notice);
    let mut painter = FramePainter::new();

    loop {
        let (height, width) = term.size();
        let frame = ui::render_frame(
            height as usize,
            width as usize,
            menu.items(),
            menu.selected_index(),
            menu.recent_items(),
            menu.notice(),
        );
        painter.paint(term, frame)?;

        match animated_read(reader, term, &mut painter, &install_task::handle()) {
            Ok(key) => match menu.handle_key(key) {
                Action::Continue => {}
                Action::OpenOpen => return Ok(Outcome::OpenProjects),
                Action::OpenNew => return Ok(Outcome::NewProject),
                Action::OpenConfig => return Ok(Outcome::Configure),
                Action::OpenRecent(index) => {
                    return Ok(Outcome::RecentProject(
                        recent_overviews[index].workspace.clone(),
                    ));
                }
                Action::Quit => return Ok(Outcome::Quit),
            },
            // Treat an interrupted read (e.g. Ctrl+C delivered as a signal) as quit.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use console::Key;
    use std::collections::VecDeque;

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
            // Default to 'q' so a test can never spin forever.
            self.keys.pop_front().unwrap_or(Ok(Key::Char('q')))
        }
    }

    fn run(keys: Vec<io::Result<Key>>) -> Result<Outcome> {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        event_loop(&term, &mut reader, Vec::new(), None)
    }

    fn overview(name: &str) -> WorkspaceOverview {
        WorkspaceOverview {
            workspace: Workspace::new(name, format!("/tmp/{name}")),
            session_count: 0,
            open_issue_count: 0,
            pr_count: 0,
        }
    }

    #[test]
    fn quit_key_returns_quit() {
        assert_eq!(run(vec![Ok(Key::Char('q'))]).unwrap(), Outcome::Quit);
    }

    #[test]
    fn navigation_keys_continue_then_select() {
        // Exercises the Continue arm (arrows + redraw) before selecting "New".
        let outcome = run(vec![
            Ok(Key::ArrowDown),
            Ok(Key::ArrowUp),
            Ok(Key::Char('e')),
        ])
        .unwrap();
        assert_eq!(outcome, Outcome::NewProject);
    }

    #[test]
    fn open_shortcut_returns_open_projects() {
        assert_eq!(
            run(vec![Ok(Key::Char('o'))]).unwrap(),
            Outcome::OpenProjects
        );
    }

    #[test]
    fn new_shortcut_returns_new_project() {
        assert_eq!(run(vec![Ok(Key::Char('e'))]).unwrap(), Outcome::NewProject);
    }

    #[test]
    fn config_shortcut_returns_configure() {
        assert_eq!(run(vec![Ok(Key::Char('c'))]).unwrap(), Outcome::Configure);
    }

    #[test]
    fn interrupted_read_returns_quit() {
        let keys = vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))];
        assert_eq!(run(keys).unwrap(), Outcome::Quit);
    }

    #[test]
    fn unexpected_read_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::other("boom"))]);
        let err = event_loop(&term, &mut reader, Vec::new(), None).unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }

    #[test]
    fn initial_notice_is_displayed_then_selection_returns() {
        // A notice carried back from a failed creation is rendered on the first
        // frame; selecting an action still returns its outcome.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('o'))]);
        let outcome = event_loop(
            &term,
            &mut reader,
            Vec::new(),
            Some("Could not create project: boom".to_string()),
        )
        .unwrap();
        assert_eq!(outcome, Outcome::OpenProjects);
    }

    #[test]
    fn recent_number_key_returns_that_workspace() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('2'))]);
        let outcome = event_loop(
            &term,
            &mut reader,
            vec![overview("alpha"), overview("beta")],
            None,
        )
        .unwrap();
        assert!(matches!(
            &outcome,
            Outcome::RecentProject(workspace) if workspace.name == "beta"
        ));
    }
}
