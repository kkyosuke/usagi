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

/// Plays animation `n` (1–5) on `term` until the user presses a key. Validates
/// `n` before touching the screen, then activates the alternate screen (restored
/// on exit, with the farewell suppressed on error) and drives the testable
/// [`event`] loop with the real terminal key source.
pub fn run(term: &Term, n: u8) -> Result<()> {
    let variation = ui::Variation::from_number(n)?;
    let mut guard = AlternateScreenGuard::new(term.clone())?;
    let mut reader = TermKeyReader::new(term.clone());
    let result = event::event_loop(term, variation, &mut reader);
    if result.is_err() {
        guard.dismiss();
    }
    result
}
