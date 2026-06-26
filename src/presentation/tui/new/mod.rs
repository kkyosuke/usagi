//! New Project screen (画面 #3).
//!
//! Collects a Git repository URL — and optionally a directory and branch —
//! the way editor "clone repository" dialogs do, then hands the validated
//! result back to the caller.

pub mod event;
pub mod state;
pub mod ui;

use anyhow::Result;
use console::Term;

use crate::presentation::tui::io::term_reader::TermKeyReader;
use crate::presentation::tui::widgets::dir_picker::FsDirSource;

pub use event::Outcome;

#[cfg(not(test))]
use event::event_loop;
#[cfg(test)]
use tests::mock_event_loop as event_loop;

/// Runs the New Project screen on the given terminal until the user submits,
/// goes back, or quits. Wires the real terminal to the testable event loop in
/// [`event`]. Assumes the alternate screen is already active.
///
/// `default_location` pre-fills the Location field with the base directory new
/// projects are created under.
pub fn run(term: &Term, default_location: &str) -> Result<Outcome> {
    let mut reader = TermKeyReader::new(term.clone());
    event_loop(term, &mut reader, default_location, &FsDirSource)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::tui::io::screen::KeyReader;
    use crate::presentation::tui::widgets::dir_picker::DirSource;
    use anyhow::bail;
    use std::cell::RefCell;

    thread_local! {
        /// The pre-filled location `run` forwarded, and the result the mock returns.
        static MOCK: RefCell<(Option<String>, Result<(), &'static str>)> =
            const { RefCell::new((None, Ok(()))) };
    }

    /// Stands in for the real New Project loop so [`run`]'s wiring is exercised
    /// without blocking on real terminal input.
    pub(super) fn mock_event_loop(
        _term: &Term,
        _reader: &mut dyn KeyReader,
        default_location: &str,
        _dir_source: &dyn DirSource,
    ) -> Result<Outcome> {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.0 = Some(default_location.to_string());
            match m.1 {
                Ok(()) => Ok(Outcome::Back),
                Err(e) => bail!(e),
            }
        })
    }

    #[test]
    fn run_pre_fills_the_location_and_returns_the_outcome() {
        MOCK.with(|m| *m.borrow_mut() = (None, Ok(())));
        let outcome = run(&Term::stdout(), "/tmp/projects").unwrap();
        assert!(matches!(outcome, Outcome::Back));
        // The default location is passed straight through to the form loop.
        MOCK.with(|m| assert_eq!(m.borrow().0.as_deref(), Some("/tmp/projects")));
    }

    #[test]
    fn run_propagates_a_loop_error() {
        MOCK.with(|m| *m.borrow_mut() = (None, Err("read failed")));
        assert_eq!(
            run(&Term::stdout(), "/tmp").unwrap_err().to_string(),
            "read failed"
        );
    }
}
