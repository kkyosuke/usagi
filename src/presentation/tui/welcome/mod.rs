//! Welcome screen (画面 #1, top menu).
//!
//! The entry screen shown by `usagi hop`. Renders the Open / New / Config /
//! Quit menu and reports the chosen action as an [`Outcome`]; the orchestrator
//! in [`crate::presentation::tui::app`] decides what each action does.

mod event;
mod menu;
pub mod ui;

use anyhow::Result;
use console::Term;

use crate::presentation::tui::term_reader::TermKeyReader;

pub use event::Outcome;

/// The row the mascot's first line sits on, for a `height`-row terminal (already
/// normalised by the caller). **The single source of truth for the mascot's
/// vertical position across every top-level screen.**
///
/// The welcome menu, the Open / New / Config screens, and the startup splash all
/// anchor their mascot to this row, so the rabbit never jumps as the user moves
/// between them (no layout shift). The welcome screen *defines* it — the value
/// centres the welcome body over its footer — and the others align to it; built
/// from the screen's own fixed [`menu`] so it depends only on `height`.
pub fn mascot_top_padding(height: usize) -> usize {
    ui::body_top_padding(height, menu::Menu::new().items(), None)
}

/// Runs the welcome menu on the given terminal until the user picks an action.
/// Wires the real terminal key source to the testable event loop in [`event`].
/// Assumes the alternate screen is already active (it is owned by the
/// orchestrator).
///
/// `notice` seeds the notice line, e.g. an error carried back from a failed
/// project creation.
pub fn run(term: &Term, notice: Option<String>) -> Result<Outcome> {
    let mut reader = TermKeyReader::new(term.clone());
    event::event_loop(term, &mut reader, notice)
}
