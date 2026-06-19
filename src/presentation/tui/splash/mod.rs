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

/// Plays the startup splash on the given terminal, returning once it finishes.
/// Drives the testable [`event`] loop with the real clock. Assumes the alternate
/// screen is already active (it is owned by the orchestrator).
pub fn run(term: &Term) -> Result<()> {
    event::event_loop(term, &mut std::thread::sleep)
}
