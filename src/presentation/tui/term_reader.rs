use std::io;

use console::{Key, Term};

use crate::presentation::tui::screen::KeyReader;

/// Reads keys from a real terminal for the interactive screens.
///
/// This is a thin wrapper over `console::Term` whose behaviour can only be
/// exercised against a live terminal, so it is excluded from coverage; the
/// event loops are tested with scripted [`KeyReader`] stubs instead.
pub struct TermKeyReader {
    term: Term,
}

impl TermKeyReader {
    pub fn new(term: Term) -> Self {
        Self { term }
    }
}

impl KeyReader for TermKeyReader {
    fn read_key(&mut self) -> io::Result<Key> {
        // `read_key_raw` surfaces Ctrl+C as `Key::CtrlC` instead of raising
        // SIGINT, so the event loop can quit gracefully and the alternate
        // screen guard restores the terminal on the way out.
        self.term.read_key_raw()
    }
}
