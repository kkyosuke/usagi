mod event;
mod menu;
pub mod ui;

use std::io;

use anyhow::Result;
use console::{Key, Term};

use crate::presentation::tui::new;
use crate::presentation::tui::open;
use crate::presentation::tui::screen::KeyReader;

/// Reads keys from a real terminal.
struct TermKeyReader {
    term: Term,
}

impl KeyReader for TermKeyReader {
    fn read_key(&mut self) -> io::Result<Key> {
        self.term.read_key()
    }
}

/// Displays the welcome screen and dispatches the selected menu action.
///
/// `Open` opens the project selection screen and `New` opens the New Project
/// screen; the remaining non-Quit actions are placeholders for now and show a
/// "coming soon" notice when selected. This function wires the real terminal
/// and the real sub-screens to the testable event loop in [`event`].
pub fn run() -> Result<()> {
    let term = Term::stdout();
    let mut reader = TermKeyReader { term: term.clone() };
    event::event_loop(&term, &mut reader, &mut |t| open::run(t), &mut |t| {
        new::run(t)
    })
}
