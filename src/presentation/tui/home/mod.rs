mod event;
mod menu;
pub mod ui;

use std::io;

use anyhow::Result;
use console::{Key, Term};

use event::KeyReader;

/// Reads keys from a real terminal.
struct TermKeyReader {
    term: Term,
}

impl KeyReader for TermKeyReader {
    fn read_key(&mut self) -> io::Result<Key> {
        self.term.read_key()
    }
}

/// Displays the startup screen and waits for the user to quit.
///
/// Menu actions other than Quit are placeholders for now and show a
/// "coming soon" notice when selected. This function wires the real terminal
/// to the testable event loop in [`event`].
pub fn run() -> Result<()> {
    let term = Term::stdout();
    let mut reader = TermKeyReader { term: term.clone() };
    event::event_loop(&term, &mut reader)
}
