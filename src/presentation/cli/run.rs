use anyhow::Result;
use console::Term;

use crate::presentation::tui::gallery;

/// Entry point for `usagi run <N>`: play one of the usagi animations full-screen.
pub fn run(n: u8) -> Result<()> {
    gallery::run(&Term::stdout(), n)
}
