//! Startup splash (画面 #0).
//!
//! Shown for a moment when `usagi hop` starts: the usagi mascot runs back and
//! forth across the screen above the `USAGI` title, then the [`welcome`] menu
//! takes over. It is purely decorative and self-timed (a couple of seconds); it
//! reads no input, so a key pressed during it is buffered straight through to
//! the menu. The orchestrator in [`crate::presentation::tui::app`] plays it once
//! before entering the screen graph.
//!
//! [`welcome`]: crate::presentation::tui::welcome

mod event;
pub mod ui;

use anyhow::Result;
use console::Term;

#[cfg(not(test))]
use event::event_loop;
#[cfg(test)]
use tests::mock_event_loop as event_loop;

/// Plays the startup splash on the given terminal, returning once it finishes.
/// Drives the testable [`event`] loop with the real clock. Assumes the alternate
/// screen is already active (it is owned by the orchestrator).
pub fn run(term: &Term) -> Result<()> {
    event_loop(term, &mut std::thread::sleep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use std::cell::RefCell;
    use std::time::Duration;

    thread_local! {
        static MOCK_RESULT: RefCell<Result<(), &'static str>> = const { RefCell::new(Ok(())) };
    }

    /// Stands in for the real splash loop so [`run`]'s terminal wiring is exercised
    /// without playing the (real-time) animation.
    pub(super) fn mock_event_loop(_term: &Term, _sleep: &mut dyn FnMut(Duration)) -> Result<()> {
        MOCK_RESULT.with(|res| match *res.borrow() {
            Ok(()) => Ok(()),
            Err(e) => bail!(e),
        })
    }

    #[test]
    fn run_returns_ok_when_the_splash_finishes() {
        MOCK_RESULT.with(|res| *res.borrow_mut() = Ok(()));
        assert!(run(&Term::stdout()).is_ok());
    }

    #[test]
    fn run_propagates_a_paint_error() {
        MOCK_RESULT.with(|res| *res.borrow_mut() = Err("paint failed"));
        assert_eq!(
            run(&Term::stdout()).unwrap_err().to_string(),
            "paint failed"
        );
    }
}
