//! Configuration screen (画面 #4).
//!
//! Lists the user-configurable settings and lets each be cycled through its
//! choices. Changes are applied and persisted immediately.

pub mod event;
pub mod state;
pub mod ui;

use anyhow::Result;
use console::Term;

use crate::domain::settings::Settings;
use crate::infrastructure::storage::Storage;
use crate::presentation::tui::term_reader::TermKeyReader;
use crate::usecase::{settings, workspace};

pub use event::Outcome;

use state::Config;

/// Runs the configuration screen on the given terminal until the user goes back
/// or quits. Wires the real terminal and storage-backed settings to the
/// testable event loop in [`event`]. Assumes the alternate screen is already
/// active.
pub fn run(term: &Term) -> Result<Outcome> {
    let storage = Storage::open_default()?;
    let (config, notice) = match load(&storage) {
        Ok((settings, workspaces)) => (Config::new(settings, workspaces), None),
        Err(e) => (
            Config::new(Settings::default(), Vec::new()),
            Some(format!("Failed to load settings: {e}")),
        ),
    };
    let mut reader = TermKeyReader::new(term.clone());
    let mut save = |s: &Settings| settings::save(&storage, s);
    event::event_loop(term, &mut reader, config, &mut save, notice)
}

/// Loads the current settings together with the registered workspace names the
/// default-workspace field can cycle through.
fn load(storage: &Storage) -> Result<(Settings, Vec<String>)> {
    let current = settings::load(storage)?;
    let workspaces = workspace::list(storage)?
        .into_iter()
        .map(|w| w.name)
        .collect();
    Ok((current, workspaces))
}
