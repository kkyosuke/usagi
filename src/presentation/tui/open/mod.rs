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

use crate::domain::workspace::Workspace;
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
    let (mut list, notice) = match load_overviews() {
        Ok(overviews) => (ProjectList::new(overviews), None),
        Err(e) => (
            ProjectList::new(Vec::new()),
            Some(format!("Failed to load projects: {e}")),
        ),
    };
    // Pre-check the workspaces from the last 統合(unite) open, so re-opening the
    // same union is one `Enter` away. Names no longer registered are ignored.
    list.preselect(&crate::infrastructure::unite_store::load());
    let mut reader = TermKeyReader::new(term.clone());
    // Whether a workspace's directory still exists, and how to drop a stale entry
    // when the user confirms — injected so the event loop stays testable.
    let mut exists = |path: &std::path::Path| path.exists();
    let mut remove = |name: &str| -> Result<()> {
        let storage = Storage::open_default()?;
        workspace::remove(&storage, name)
    };
    let mut actions = event::ListActions {
        exists: &mut exists,
        remove: &mut remove,
    };
    event::event_loop(
        term,
        &mut reader,
        list,
        notice,
        &mut |t, wss| open_home(t, wss),
        &mut actions,
    )
}

/// Opens the home screen for the chosen workspace(s): marks them just-used,
/// remembers the selection, plays the open→home mascot animation while the
/// primary workspace loads in the background, then runs the home screen.
///
/// The slice holds every workspace the user chose to open together: one for the
/// ordinary single-workspace home, several for 統合(unite) mode. Shared by the
/// project selection screen and the welcome screen's "recent" shortcuts so both
/// open a workspace exactly the same way.
pub fn open_home(term: &Term, wss: &[Workspace]) -> Result<home::Outcome> {
    // The primary workspace the `Preload` belongs to; any further entries are
    // stacked below it in 統合(unite) mode.
    let primary = &wss[0];
    // Mark every opened workspace as just-used so they sort to the top of the
    // list on the next load. A failure to persist must not block opening, so the
    // error is swallowed.
    if let Ok(storage) = Storage::open_default() {
        for ws in wss {
            let _ = workspace::touch(&storage, &ws.name);
        }
    }
    // Remember this selection so the next Open pre-checks the same union.
    let names: Vec<String> = wss.iter().map(|w| w.name.clone()).collect();
    let _ = crate::infrastructure::unite_store::save(&names);
    // Start loading the primary workspace (state.json / issues / settings / agent
    // probe / history) on a background thread, then play the mascot animation on
    // this thread while it runs. By the time the rabbit lands at the bottom-left
    // the load is almost always already done, so joining it is near-instant and
    // the home screen (切替) paints with no perceptible delay. Any extra unite
    // workspaces are loaded inside `home::run`, after the animation, since they
    // only seed display snapshots.
    let loader = {
        let ws = primary.clone();
        std::thread::spawn(move || home::preload(&ws))
    };
    play_open_animation(term)?;
    // Recover by loading synchronously if the loader thread panicked.
    let preload = loader.join().unwrap_or_else(|_| home::preload(primary));
    home::run(term, wss, preload)
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
