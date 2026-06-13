use std::io;

use anyhow::Result;
use console::Term;

use crate::presentation::tui::new;
use crate::presentation::tui::open;
use crate::presentation::tui::screen::{AlternateScreenGuard, KeyReader};

use super::menu::{Action, Menu};
use super::ui;

/// Launches the project selection screen and returns the user's choice.
///
/// Taking this as a parameter lets the event loop be tested without a real
/// terminal: production wires it to [`open::run`], tests pass a stub.
pub type OpenOpen<'a> = dyn FnMut(&Term) -> Result<open::Outcome> + 'a;

/// Launches the New Project screen and returns the user's choice.
///
/// Taking this as a parameter lets the event loop be tested without a real
/// terminal: production wires it to [`new::run`], tests pass a stub.
pub type OpenNew<'a> = dyn FnMut(&Term) -> Result<new::Outcome> + 'a;

/// Runs the startup screen against the given terminal and key source until the
/// user quits (or an unrecoverable read error occurs).
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    open_open: &mut OpenOpen,
    open_new: &mut OpenNew,
) -> Result<()> {
    let mut guard = AlternateScreenGuard::new(term.clone())?;
    let mut menu = Menu::new();

    loop {
        term.move_cursor_to(0, 0)?;
        term.clear_screen()?;
        let (height, width) = term.size();
        let frame = ui::render_frame(
            height as usize,
            width as usize,
            menu.items(),
            menu.selected_index(),
            menu.notice(),
        );
        for line in &frame {
            term.write_line(line)?;
        }

        match reader.read_key() {
            Ok(key) => match menu.handle_key(key) {
                Action::Continue => {}
                Action::Quit => return Ok(()),
                Action::OpenOpen => match open_open(term) {
                    Ok(open::Outcome::Back) => menu.set_notice(None),
                    Ok(open::Outcome::Quit) => return Ok(()),
                    Err(e) => {
                        // Restore the terminal without the farewell on error.
                        guard.dismiss();
                        return Err(e);
                    }
                },
                Action::OpenNew => match open_new(term) {
                    Ok(new::Outcome::Back) => menu.set_notice(None),
                    Ok(new::Outcome::Quit) => return Ok(()),
                    Ok(new::Outcome::Submitted(project)) => {
                        menu.set_notice(Some(format!(
                            "Ready to init \"{}\" from {} 🐰",
                            project.directory,
                            project.url.as_str()
                        )));
                    }
                    Err(e) => {
                        // Restore the terminal without the farewell on error.
                        guard.dismiss();
                        return Err(e);
                    }
                },
            },
            // Treat an interrupted read (e.g. Ctrl+C delivered as a signal) as quit.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(e) => {
                // Restore the terminal without the farewell on an unexpected error.
                guard.dismiss();
                return Err(anyhow::Error::from(e).context("Failed to read key"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::repository::RepoUrl;
    use crate::presentation::tui::new::state::NewProject;
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
            self.keys.pop_front().unwrap_or(Ok(Key::Char('q')))
        }
    }

    // Project-selection (Open) screen launchers used as stubs.
    fn open_screen_back(_t: &Term) -> Result<open::Outcome> {
        Ok(open::Outcome::Back)
    }
    fn open_screen_quit(_t: &Term) -> Result<open::Outcome> {
        Ok(open::Outcome::Quit)
    }
    fn open_screen_err(_t: &Term) -> Result<open::Outcome> {
        Err(anyhow::anyhow!("open screen blew up"))
    }

    // New-screen launchers used as stubs; each is exercised by a test below.
    fn new_back(_t: &Term) -> Result<new::Outcome> {
        Ok(new::Outcome::Back)
    }
    fn new_quit(_t: &Term) -> Result<new::Outcome> {
        Ok(new::Outcome::Quit)
    }
    fn new_submitted(_t: &Term) -> Result<new::Outcome> {
        Ok(new::Outcome::Submitted(NewProject {
            url: RepoUrl::parse("https://github.com/owner/repo.git").unwrap(),
            directory: "repo".to_string(),
            branch: None,
        }))
    }
    fn new_err(_t: &Term) -> Result<new::Outcome> {
        Err(anyhow::anyhow!("new screen blew up"))
    }

    #[test]
    fn loop_quits_when_quit_key_pressed() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('q'))]);
        assert!(event_loop(&term, &mut reader, &mut open_screen_back, &mut new_back).is_ok());
    }

    #[test]
    fn loop_redraws_across_several_keys_before_quitting() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::ArrowDown),
            Ok(Key::Char('c')), // produces a notice, exercising the notice redraw
            Ok(Key::Enter),
            Ok(Key::ArrowUp),
            Ok(Key::Char('q')),
        ]);
        assert!(event_loop(&term, &mut reader, &mut open_screen_back, &mut new_back).is_ok());
    }

    #[test]
    fn interrupted_read_is_treated_as_quit() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))]);
        assert!(event_loop(&term, &mut reader, &mut open_screen_back, &mut new_back).is_ok());
    }

    #[test]
    fn unexpected_read_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::other("boom"))]);
        let err = event_loop(&term, &mut reader, &mut open_screen_back, &mut new_back).unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }

    #[test]
    fn open_screen_back_returns_to_menu() {
        let term = Term::stdout();
        // 'o' opens the project selection screen (stub returns Back), then 'q'.
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('o')), Ok(Key::Char('q'))]);
        assert!(event_loop(&term, &mut reader, &mut open_screen_back, &mut new_back).is_ok());
    }

    #[test]
    fn open_screen_quit_exits_the_app() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('o'))]);
        assert!(event_loop(&term, &mut reader, &mut open_screen_quit, &mut new_back).is_ok());
    }

    #[test]
    fn open_screen_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('o'))]);
        let err = event_loop(&term, &mut reader, &mut open_screen_err, &mut new_back).unwrap_err();
        assert!(err.to_string().contains("open screen blew up"));
    }

    #[test]
    fn new_screen_back_returns_to_menu() {
        let term = Term::stdout();
        // 'e' opens the New screen (stub returns Back), then 'q' quits.
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e')), Ok(Key::Char('q'))]);
        assert!(event_loop(&term, &mut reader, &mut open_screen_back, &mut new_back).is_ok());
    }

    #[test]
    fn new_screen_quit_exits_the_app() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e'))]);
        assert!(event_loop(&term, &mut reader, &mut open_screen_back, &mut new_quit).is_ok());
    }

    #[test]
    fn new_screen_submitted_sets_a_notice_then_quits() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e')), Ok(Key::Char('q'))]);
        assert!(event_loop(
            &term,
            &mut reader,
            &mut open_screen_back,
            &mut new_submitted
        )
        .is_ok());
    }

    #[test]
    fn new_screen_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('e'))]);
        let err = event_loop(&term, &mut reader, &mut open_screen_back, &mut new_err).unwrap_err();
        assert!(err.to_string().contains("new screen blew up"));
    }
}
