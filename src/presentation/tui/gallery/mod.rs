//! Rabbit animation gallery for `usagi run <N>`.
//!
//! Plays one of the usagi animations full-screen so the art can be previewed in
//! isolation: 1 走り回る / 2 増えていく / 3 読み込み（ホップ）/ 4 読み込み（表情）/
//! 5 マスコット. The animations themselves live in the shared
//! [`widgets`](crate::presentation::tui::widgets); this module only selects one,
//! owns the alternate screen, and loops it until a key is pressed.

mod event;
pub mod ui;

use anyhow::Result;
use console::Term;

use crate::presentation::tui::screen::AlternateScreenGuard;
use crate::presentation::tui::term_reader::TermKeyReader;

#[cfg(not(test))]
use event::event_loop;
#[cfg(test)]
use tests::mock_event_loop as event_loop;

/// Plays animation `n` (1–5) on `term` until the user presses a key. Validates
/// `n` before touching the screen, then activates the alternate screen (restored
/// on exit, with the farewell suppressed on error) and drives the testable
/// [`event`] loop with the real terminal key source.
pub fn run(term: &Term, n: u8) -> Result<()> {
    let variation = ui::Variation::from_number(n)?;
    let mut guard = AlternateScreenGuard::new(term.clone())?;
    let mut reader = TermKeyReader::new(term.clone());
    let result = event_loop(term, variation, &mut reader);
    if result.is_err() {
        guard.dismiss();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::tui::screen::KeyReader;
    use anyhow::bail;
    use std::cell::RefCell;
    use ui::Variation;

    thread_local! {
        /// The variation `run` selected, and the result the mock returns.
        static MOCK: RefCell<(Option<Variation>, Result<(), &'static str>)> =
            const { RefCell::new((None, Ok(()))) };
    }

    /// Stands in for the real gallery loop so [`run`]'s validation and
    /// alternate-screen wiring are exercised without playing the animation.
    pub(super) fn mock_event_loop(
        _term: &Term,
        variation: Variation,
        _reader: &mut dyn KeyReader,
    ) -> Result<()> {
        MOCK.with(|m| {
            let mut m = m.borrow_mut();
            m.0 = Some(variation);
            match m.1 {
                Ok(()) => Ok(()),
                Err(e) => bail!(e),
            }
        })
    }

    #[test]
    fn run_rejects_an_out_of_range_number_before_touching_the_screen() {
        // Validation happens first, so an invalid `n` errors without entering the
        // alternate screen or the loop.
        assert!(run(&Term::stdout(), 0).is_err());
        MOCK.with(|m| assert_eq!(m.borrow().0, None));
    }

    #[test]
    fn run_plays_the_selected_animation_and_returns_ok() {
        MOCK.with(|m| *m.borrow_mut() = (None, Ok(())));
        assert!(run(&Term::stdout(), 2).is_ok());
        // The number maps to its variation before the loop runs.
        MOCK.with(|m| assert_eq!(m.borrow().0, Some(Variation::Multiplying)));
    }

    #[test]
    fn run_dismisses_the_farewell_on_a_loop_error() {
        // A loop error suppresses the farewell (via `guard.dismiss`) and propagates.
        MOCK.with(|m| *m.borrow_mut() = (None, Err("loop failed")));
        assert_eq!(
            run(&Term::stdout(), 1).unwrap_err().to_string(),
            "loop failed"
        );
    }
}
