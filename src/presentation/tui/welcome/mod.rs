//! Welcome screen (画面 #1, top menu).
//!
//! The entry screen shown by `usagi hop`. Renders the Open / New / Config /
//! Quit menu and reports the chosen action as an [`Outcome`]; the orchestrator
//! in [`crate::presentation::tui::app`] decides what each action does.

mod event;
mod menu;
pub mod ui;

use anyhow::Result;
use console::Term;

use crate::presentation::tui::io::term_reader::TermKeyReader;

pub use event::Outcome;

#[cfg(not(test))]
use event::event_loop;
#[cfg(test)]
use tests::mock_event_loop as event_loop;

/// The row the mascot's first line sits on, for a `height`-row terminal (already
/// normalised by the caller). **The single source of truth for the mascot's
/// vertical position across every top-level screen.**
///
/// The welcome menu, the Open / New / Config screens, and the startup splash all
/// anchor their mascot to this row, so the rabbit never jumps as the user moves
/// between them (no layout shift). The welcome screen *defines* it — the value
/// centres the welcome body over its footer — and the others align to it; built
/// from the screen's own fixed [`menu`] so it depends only on `height`.
pub fn mascot_top_padding(height: usize) -> usize {
    ui::body_top_padding(height, menu::Menu::new().items(), None)
}

/// Runs the welcome menu on the given terminal until the user picks an action.
/// Wires the real terminal key source to the testable event loop in [`event`].
/// Assumes the alternate screen is already active (it is owned by the
/// orchestrator).
///
/// `notice` seeds the notice line, e.g. an error carried back from a failed
/// project creation.
pub fn run(term: &Term, notice: Option<String>) -> Result<Outcome> {
    let mut reader = TermKeyReader::new(term.clone());
    event_loop(term, &mut reader, notice)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::tui::io::screen::KeyReader;
    use anyhow::bail;
    use std::cell::RefCell;

    thread_local! {
        /// The notice `run` forwarded, and the result the mock returns next (taken
        /// so the returned `Outcome` need not be `Clone`).
        static MOCK: RefCell<(Option<String>, Result<Outcome, &'static str>)> =
            const { RefCell::new((None, Ok(Outcome::Quit))) };
    }

    /// Stands in for the real welcome loop so [`run`]'s terminal/reader wiring is
    /// exercised without blocking on real terminal input.
    pub(super) fn mock_event_loop(
        _term: &Term,
        _reader: &mut dyn KeyReader,
        notice: Option<String>,
    ) -> Result<Outcome> {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.0 = notice;
            match std::mem::replace(&mut m.1, Ok(Outcome::Quit)) {
                Ok(outcome) => Ok(outcome),
                Err(e) => bail!(e),
            }
        })
    }

    #[test]
    fn run_forwards_the_notice_and_returns_the_chosen_action() {
        MOCK.with(|m| *m.borrow_mut() = (None, Ok(Outcome::NewProject)));
        let outcome = run(&Term::stdout(), Some("welcome back".to_string())).unwrap();
        assert_eq!(outcome, Outcome::NewProject);
        // The seed notice is passed straight through to the loop.
        MOCK.with(|m| assert_eq!(m.borrow().0.as_deref(), Some("welcome back")));
    }

    #[test]
    fn run_propagates_a_loop_error() {
        MOCK.with(|m| *m.borrow_mut() = (None, Err("read failed")));
        assert_eq!(
            run(&Term::stdout(), None).unwrap_err().to_string(),
            "read failed"
        );
    }

    #[test]
    fn mascot_top_padding_matches_the_body_layout() {
        // The shared mascot row is derived from the welcome screen's own menu, so
        // it depends only on the terminal height.
        assert_eq!(
            mascot_top_padding(40),
            ui::body_top_padding(40, menu::Menu::new().items(), None)
        );
    }
}
