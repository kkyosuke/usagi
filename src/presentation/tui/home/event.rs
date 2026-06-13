use std::io;

use anyhow::Result;
use console::{Key, Term};

use crate::presentation::tui::screen::AlternateScreenGuard;

use super::menu::{Action, Menu};
use super::ui;

/// Source of key presses driving the startup screen.
///
/// Abstracting the read lets the event loop be exercised without a real
/// terminal: tests supply a scripted sequence of keys.
pub trait KeyReader {
    fn read_key(&mut self) -> io::Result<Key>;
}

/// Runs the startup screen against the given terminal and key source until the
/// user quits (or an unrecoverable read error occurs).
pub fn event_loop(term: &Term, reader: &mut dyn KeyReader) -> Result<()> {
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
            Ok(key) => {
                if menu.handle_key(key) == Action::Quit {
                    return Ok(());
                }
            }
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

    #[test]
    fn loop_quits_when_quit_key_pressed() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Ok(Key::Char('q'))]);
        assert!(event_loop(&term, &mut reader).is_ok());
    }

    #[test]
    fn loop_redraws_across_several_keys_before_quitting() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![
            Ok(Key::ArrowDown),
            Ok(Key::Char('o')), // produces a notice, exercising the notice redraw
            Ok(Key::Enter),
            Ok(Key::ArrowUp),
            Ok(Key::Char('q')),
        ]);
        assert!(event_loop(&term, &mut reader).is_ok());
    }

    #[test]
    fn interrupted_read_is_treated_as_quit() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))]);
        assert!(event_loop(&term, &mut reader).is_ok());
    }

    #[test]
    fn unexpected_read_error_is_propagated() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::other("boom"))]);
        let err = event_loop(&term, &mut reader).unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }
}
