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

use crate::presentation::tui::term_reader::TermKeyReader;

pub use event::Outcome;

/// Runs the New Project screen on the given terminal until the user submits,
/// goes back, or quits. Wires the real terminal to the testable event loop in
/// [`event`]. Assumes the alternate screen is already active.
///
/// `default_location` pre-fills the Location field with the base directory new
/// projects are created under.
pub fn run(term: &Term, default_location: &str) -> Result<Outcome> {
    let mut reader = TermKeyReader::new(term.clone());
    event::event_loop(term, &mut reader, default_location)
}
