use anyhow::Result;
use console::Key;
use console::Term;

use crate::presentation::tui::screen::KeyReader;

use super::state::{FormState, NewProject};
use super::ui;

/// What the user chose to do on the New Project screen.
#[derive(Debug)]
pub enum Outcome {
    /// Return to the previous screen without creating a project.
    Back,
    /// The user submitted a valid project.
    Submitted(NewProject),
    /// The user asked to quit the application entirely.
    Quit,
}

/// Runs the New Project screen against the given terminal and key source until
/// the user submits, goes back, or quits. Assumes the alternate screen is
/// already active (it is owned by the caller).
///
/// `default_location` pre-fills the Location field with the base directory new
/// projects are created under; the user can edit it before submitting.
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    default_location: &str,
) -> Result<Outcome> {
    let mut state = FormState::new();
    state.set_location(default_location);
    let mut notice: Option<String> = None;

    loop {
        term.move_cursor_to(0, 0)?;
        term.clear_screen()?;
        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &state, notice.as_deref());
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
            Key::Escape => return Ok(Outcome::Back),
            Key::CtrlC => return Ok(Outcome::Quit),
            Key::Enter => match state.validate() {
                Ok(project) => return Ok(Outcome::Submitted(project)),
                Err(message) => notice = Some(message),
            },
            Key::Tab | Key::ArrowDown => {
                state.focus_next();
                notice = None;
            }
            Key::BackTab | Key::ArrowUp => {
                state.focus_prev();
                notice = None;
            }
            Key::Backspace => {
                state.backspace();
                notice = None;
            }
            Key::Char(c) => {
                state.insert_char(c);
                notice = None;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn type_keys(s: &str) -> Vec<io::Result<Key>> {
        s.chars().map(|c| Ok(Key::Char(c))).collect()
    }

    #[test]
    fn escape_returns_back() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Escape)]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base").unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn ctrl_c_returns_quit() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::CtrlC)]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base").unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn enter_with_valid_url_submits() {
        let term = Term::stdout();
        let mut keys = type_keys("https://github.com/owner/repo.git");
        keys.push(Ok(Key::Enter));
        let mut reader = ScriptedReader::new(keys);
        // The pre-filled location lets validation succeed without editing it.
        let outcome = event_loop(&term, &mut reader, "/base").unwrap();
        assert!(matches!(
            &outcome,
            Outcome::Submitted(project)
                if project.directory == "repo"
                    && project.url.as_str() == "https://github.com/owner/repo.git"
                    && project.location == std::path::Path::new("/base")
        ));
    }

    #[test]
    fn enter_with_invalid_url_shows_notice_then_back() {
        let term = Term::stdout();
        // Enter on an empty form fails validation (notice), then Escape goes back.
        let mut reader = ScriptedReader::new(vec![Ok(Key::Enter), Ok(Key::Escape)]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base").unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn navigation_and_editing_keys_are_handled() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::Tab),       // focus_next
            Ok(Key::ArrowDown), // focus_next
            Ok(Key::BackTab),   // focus_prev
            Ok(Key::ArrowUp),   // focus_prev
            Ok(Key::Char('x')), // insert
            Ok(Key::Backspace), // delete
            Ok(Key::Home),      // ignored (the `_` arm)
            Ok(Key::Escape),    // back
        ]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base").unwrap(),
            Outcome::Back
        ));
    }

    #[test]
    fn interrupted_read_returns_quit() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))]);
        assert!(matches!(
            event_loop(&term, &mut reader, "/base").unwrap(),
            Outcome::Quit
        ));
    }

    #[test]
    fn unexpected_read_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::other("boom"))]);
        let err = event_loop(&term, &mut reader, "/base").unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }
}
