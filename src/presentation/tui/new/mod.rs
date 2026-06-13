//! New Project screen (画面 #3).
//!
//! Collects a Git repository URL — and optionally a directory and branch —
//! the way editor "clone repository" dialogs do, then hands the validated
//! result back to the caller.

pub mod event;
pub mod state;
pub mod ui;

use std::io;

use anyhow::Result;
use console::{Key, Term};

use crate::presentation::tui::screen::KeyReader;

pub use event::Outcome;

/// Reads keys from a real terminal.
struct TermKeyReader {
    term: Term,
}

impl KeyReader for TermKeyReader {
    fn read_key(&mut self) -> io::Result<Key> {
        self.term.read_key()
    }
}

/// Runs the New Project screen on the given terminal until the user submits,
/// goes back, or quits. Wires the real terminal to the testable event loop in
/// [`event`]. Assumes the alternate screen is already active.
pub fn run(term: &Term) -> Result<Outcome> {
    let mut reader = TermKeyReader { term: term.clone() };
    event::event_loop(term, &mut reader)
}
