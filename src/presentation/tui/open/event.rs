use anyhow::Result;
use console::Key;
use console::Term;

use crate::presentation::tui::screen::KeyReader;

use super::state::ProjectList;
use super::ui;

/// What the user chose to do on the project selection screen.
#[derive(Debug)]
pub enum Outcome {
    /// Return to the previous screen without opening a project.
    Back,
    /// The user asked to quit the application entirely.
    Quit,
}

/// Runs the project selection screen against the given terminal and key source
/// until the user goes back or quits. Assumes the alternate screen is already
/// active (it is owned by the caller).
///
/// Opening a project is a placeholder for now: selecting one shows a "coming
/// soon" notice, since the workspace screen is not implemented yet.
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut list: ProjectList,
    initial_notice: Option<String>,
) -> Result<Outcome> {
    let mut notice = initial_notice;

    loop {
        term.move_cursor_to(0, 0)?;
        term.clear_screen()?;
        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &list, notice.as_deref());
        for line in &frame {
            term.write_line(line)?;
        }

        let key = match reader.read_key() {
            Ok(key) => key,
            // An interrupted read (e.g. a delivered signal) means quit.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Outcome::Quit),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
        };

        match key {
            Key::ArrowUp | Key::Char('k') => {
                list.move_up();
                notice = None;
            }
            Key::ArrowDown | Key::Char('j') => {
                list.move_down();
                notice = None;
            }
            Key::Enter => {
                if let Some(workspace) = list.selected() {
                    notice = Some(format!("Opening \"{}\" is coming soon 🐰", workspace.name));
                }
            }
            Key::Char('q') | Key::Escape => return Ok(Outcome::Back),
            Key::CtrlC => return Ok(Outcome::Quit),
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

    fn sample_list() -> ProjectList {
        ProjectList::new(vec![
            Workspace::new("alpha", "/p/alpha"),
            Workspace::new("beta", "/p/beta"),
        ])
    }

    fn run(keys: Vec<io::Result<Key>>, list: ProjectList) -> Result<Outcome> {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        event_loop(&term, &mut reader, list, None)
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
    fn enter_on_a_project_shows_a_notice_then_back() {
        // Enter selects a project (sets the "coming soon" notice), then Escape.
        let keys = vec![Ok(Key::Enter), Ok(Key::Escape)];
        assert!(matches!(run(keys, sample_list()).unwrap(), Outcome::Back));
    }

    #[test]
    fn enter_on_empty_list_does_nothing() {
        // With no workspaces there is nothing to select; Enter is a no-op.
        let keys = vec![Ok(Key::Enter), Ok(Key::Escape)];
        let list = ProjectList::new(Vec::new());
        assert!(matches!(run(keys, list).unwrap(), Outcome::Back));
    }

    #[test]
    fn initial_notice_is_displayed() {
        // A load-error notice passed in is rendered on the first frame.
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Escape)]);
        let outcome = event_loop(
            &term,
            &mut reader,
            ProjectList::new(Vec::new()),
            Some("Failed to load projects: boom".to_string()),
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Back));
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
        let err = event_loop(&term, &mut reader, sample_list(), None).unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }
}
