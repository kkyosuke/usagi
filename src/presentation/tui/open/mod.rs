//! Project selection screen (画面 #2).
//!
//! Lists the registered workspaces (most recently used first) and lets the
//! user pick one to open. Selecting a project opens the home screen for that
//! workspace; returning from the home screen leaves the user back on this list.

pub mod event;
pub mod state;
pub mod transition;
pub mod ui;

use anyhow::Result;
use console::Term;

use crate::infrastructure::storage::Storage;
use crate::presentation::tui::home;
use crate::presentation::tui::io::screen::FramePainter;
use crate::presentation::tui::io::term_reader::TermKeyReader;
use crate::presentation::tui::{welcome, widgets};
use crate::usecase::workspace::{self, WorkspaceOverview};

pub use event::Outcome;

use state::ProjectList;

/// Runs the project selection screen on the given terminal until the user goes
/// back or quits. Wires the real terminal and storage-backed workspace list to
/// the testable event loop in [`event`]. Assumes the alternate screen is
/// already active.
pub fn run(term: &Term) -> Result<Outcome> {
    let (list, notice) = match load_overviews() {
        Ok(overviews) => (ProjectList::new(overviews), None),
        Err(e) => (
            ProjectList::new(Vec::new()),
            Some(format!("Failed to load projects: {e}")),
        ),
    };
    let mut reader = TermKeyReader::new(term.clone());
    event::event_loop(term, &mut reader, list, notice, &mut |t, ws| {
        // Mark the workspace as just-used so it sorts to the top of the list on
        // the next load. A failure to persist must not block opening, so the
        // error is swallowed.
        if let Ok(storage) = Storage::open_default() {
            let _ = workspace::touch(&storage, &ws.name);
        }
        // Start loading the workspace (state.json / issues / settings / agent
        // probe / history) on a background thread, then play the mascot animation
        // on this thread while it runs. By the time the rabbit lands at the
        // bottom-left the load is almost always already done, so joining it is
        // near-instant and the home screen (切替) paints with no perceptible delay.
        let loader = {
            let ws = ws.clone();
            std::thread::spawn(move || home::preload(&ws))
        };
        play_open_animation(t)?;
        // Recover by loading synchronously if the loader thread panicked.
        let preload = loader.join().unwrap_or_else(|_| home::preload(ws));
        home::run(t, ws, preload)
    })
}

/// Plays the open→home mascot animation: the project list is cleared and the
/// usagi glides from where it was shown down to the bottom-left corner, where the
/// home screen's status line sits. Paced by [`std::thread::sleep`]; a fresh
/// painter clears the list on the first frame (hiding it) and the rabbit lifts
/// off from the shared mascot row ([`welcome::mascot_top_padding`]).
fn play_open_animation(term: &Term) -> Result<()> {
    let mut painter = FramePainter::new();
    let (raw_height, raw_width) = term.size();
    let (height, width) = widgets::normalize_size(raw_height as usize, raw_width as usize);
    let start = (
        welcome::mascot_top_padding(height),
        widgets::centered_padding(width, widgets::rabbit_width()),
    );
    // A blank backdrop: only the gliding rabbit is on screen during the flight.
    let backdrop = vec![String::new(); height];
    transition::play(term, &mut painter, backdrop, start, &mut |d| {
        std::thread::sleep(d)
    })
}

/// Loads the registered workspaces (most recently used first) with their
/// session and open-issue counts.
fn load_overviews() -> Result<Vec<WorkspaceOverview>> {
    let storage = Storage::open_default()?;
    workspace::overviews(&storage)
}
